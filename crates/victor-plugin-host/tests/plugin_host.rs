use std::sync::{Arc, Mutex};

use serde_json::json;
use tempfile::tempdir;
use victor_context::{ContextItemId, ContextScope, InMemoryContextStore};
use victor_domain::{Plan, PlanStep, RiskLevel, ToolDescriptor, ToolId};
use victor_plugin_host::{HostError, PluginToolExecutor};
use victor_policy::{PolicyConfig, PolicyEngine};
use victor_runtime::{ToolExecutionJob, ToolRuntime};
use victor_tools::{AuditSink, ToolAuditEvent, ToolManifest, ToolRegistry};

#[derive(Default)]
struct Audits(Mutex<Vec<ToolAuditEvent>>);
impl AuditSink for Audits {
    fn record(&self, event: ToolAuditEvent) -> Result<(), victor_tools::ToolError> {
        self.0.lock().unwrap().push(event);
        Ok(())
    }
}

fn manifest() -> ToolManifest {
    ToolManifest::new(
        "1.0.0",
        ToolDescriptor {
            id: ToolId::parse("plugin_sysinfo").unwrap(),
            schema_version: 1,
            description: "CPU/RAM read-only".into(),
            input_schema: json!({"type":"object"}),
            output_schema: json!({"type":"object"}),
            capabilities: vec![],
            risk: RiskLevel::R0,
            default_timeout_ms: 1_000,
            undo_support: false,
            path_arguments: vec![],
        },
    )
}

fn fixture(mode: &str, extra: Option<&std::path::Path>) -> Vec<String> {
    let mut command = vec![
        env!("CARGO_BIN_EXE_plugin_fixture").to_string(),
        mode.into(),
    ];
    if let Some(path) = extra {
        command.push(path.display().to_string());
    }
    command
}

#[tokio::test]
async fn sysinfo_goes_through_runtime_policy_context_and_audit() {
    let executor = PluginToolExecutor::spawn(fixture("normal", None), manifest())
        .await
        .unwrap();
    executor.heartbeat("h1").await.unwrap();
    let mut registry = ToolRegistry::new();
    registry.register(manifest()).unwrap();
    let registry = Arc::new(registry);
    let policy = Arc::new(
        PolicyEngine::new(
            PolicyConfig::new([tempdir().unwrap().path().to_path_buf()])
                .with_enabled_tools([ToolId::parse("plugin_sysinfo").unwrap()]),
        )
        .unwrap(),
    );
    let context = Arc::new(InMemoryContextStore::new());
    let audit = Arc::new(Audits::default());
    let runtime = ToolRuntime::new(registry, policy, context.clone(), audit.clone());
    let plan = Plan::new(
        "task-1",
        1,
        "sysinfo",
        vec![PlanStep::new(
            ToolId::parse("plugin_sysinfo").unwrap(),
            "CPU/RAM",
            json!({}),
            RiskLevel::R0,
        )],
    )
    .unwrap();
    let outcome = runtime
        .execute(
            &plan,
            None,
            ToolExecutionJob {
                result_item_id: ContextItemId::parse("result-1").unwrap(),
                scope: ContextScope::project("victor").unwrap(),
                started_at_unix_ms: 1,
            },
            &executor,
        )
        .await
        .unwrap();
    assert_eq!(outcome.result.output["memory_bytes"], 0);
    assert_eq!(context.len(), 1);
    assert_eq!(audit.0.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn rejects_bad_handshake_and_oversized_frame() {
    assert!(matches!(
        PluginToolExecutor::spawn(fixture("bad-handshake", None), manifest()).await,
        Err(HostError::Handshake(_))
    ));
    assert!(matches!(
        PluginToolExecutor::spawn(fixture("oversized", None), manifest()).await,
        Err(HostError::FrameTooLarge)
    ));
    assert!(matches!(
        PluginToolExecutor::spawn(fixture("forbidden-capability", None), manifest()).await,
        Err(HostError::Handshake(_))
    ));
}

#[tokio::test]
async fn times_out_and_restarts_once_after_crash() {
    let mut short = manifest();
    short.descriptor.default_timeout_ms = 25;
    let timeout_executor = PluginToolExecutor::spawn(fixture("timeout", None), short)
        .await
        .unwrap();
    let timeout_error = victor_tools::ToolExecutor::execute(&timeout_executor, json!({}), 4)
        .await
        .unwrap_err();
    assert!(timeout_error.to_string().contains("timeout pluginu"));
    let marker = tempdir().unwrap();
    let executor = PluginToolExecutor::spawn(
        fixture("crash-once", Some(&marker.path().join("marker"))),
        manifest(),
    )
    .await
    .unwrap();
    let result = victor_tools::ToolExecutor::execute(&executor, json!({}), 5)
        .await
        .unwrap();
    assert_eq!(result.output["cpu_percent"], 0.0);
}
