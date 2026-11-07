use std::sync::{Arc, Mutex};

use serde_json::json;
use tempfile::tempdir;
use victor_context::{ContextItemId, ContextScope, ContextStore, InMemoryContextStore};
use victor_domain::{Plan, PlanStep, RiskLevel, ToolDescriptor, ToolId};
use victor_plugin_host::{
    HostError, PersistentPluginHost, PluginLaunchPolicy, PluginToolExecutor, SandboxRequirement,
};
use victor_policy::{PolicyConfig, PolicyEngine};
use victor_runtime::{ToolExecutionJob, ToolRuntime};
use victor_storage::SqliteContextStore;
use victor_tools::{AuditSink, ToolAuditEvent, ToolExecutor, ToolManifest, ToolRegistry};

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
        PluginToolExecutor::spawn(fixture("oversized-no-newline", None), manifest()).await,
        Err(HostError::FrameTooLarge)
    ));
    assert!(matches!(
        PluginToolExecutor::spawn(fixture("malformed", None), manifest()).await,
        Err(HostError::Handshake(_))
    ));
    assert!(matches!(
        PluginToolExecutor::spawn(fixture("forbidden-capability", None), manifest()).await,
        Err(HostError::Handshake(_))
    ));
    assert!(matches!(
        PluginToolExecutor::spawn(fixture("forbidden-risk", None), manifest()).await,
        Err(HostError::Handshake(_))
    ));
}

#[tokio::test]
async fn times_out_and_restarts_once_after_crash() {
    let mut short = manifest();
    short.descriptor.default_timeout_ms = 25;
    let timeout_marker = tempdir().unwrap();
    let timeout_pid_file = timeout_marker.path().join("pids");
    let timeout_executor =
        PluginToolExecutor::spawn(fixture("timeout", Some(&timeout_pid_file)), short)
            .await
            .unwrap();
    let timeout_error = victor_tools::ToolExecutor::execute(&timeout_executor, json!({}), 4)
        .await
        .unwrap_err();
    assert!(timeout_error.to_string().contains("timeout pluginu"));
    assert_no_recorded_pids_alive(&timeout_pid_file);
    let marker = tempdir().unwrap();
    let audits = Arc::new(Audits::default());
    let executor = PluginToolExecutor::spawn_with_policy(
        fixture("crash-once", Some(&marker.path().join("marker"))),
        manifest(),
        PluginLaunchPolicy::default(),
        Some(audits.clone()),
    )
    .await
    .unwrap();
    let result = victor_tools::ToolExecutor::execute(&executor, json!({}), 5)
        .await
        .unwrap();
    assert_eq!(result.output["cpu_percent"], 0.0);
    assert_eq!(audits.0.lock().unwrap().len(), 1);
    drop(executor);
    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    assert_no_recorded_pids_alive(&marker.path().join("marker"));
}

#[tokio::test]
async fn host_clears_secret_environment_and_requires_available_sandbox() {
    std::env::set_var("VICTOR_TEST_SECRET", "must-not-leak");
    let executor = PluginToolExecutor::spawn(fixture("env", None), manifest())
        .await
        .unwrap();
    let result = victor_tools::ToolExecutor::execute(&executor, json!({}), 9)
        .await
        .unwrap();
    std::env::remove_var("VICTOR_TEST_SECRET");
    assert_eq!(result.output["secret_present"], false);

    let policy = PluginLaunchPolicy {
        sandbox: SandboxRequirement::Required,
        sandbox_executable: Some("/definitely/missing/bwrap".into()),
        ..PluginLaunchPolicy::default()
    };
    assert!(matches!(
        PluginToolExecutor::spawn_with_policy(fixture("normal", None), manifest(), policy, None)
            .await,
        Err(HostError::SandboxUnavailable)
    ));
}

#[tokio::test]
async fn persistent_host_restores_sysinfo_and_rejects_changed_manifest() {
    let file = tempfile::NamedTempFile::new().unwrap();
    let store = Arc::new(SqliteContextStore::open(file.path()).unwrap());
    let mut host = PersistentPluginHost::open(store.clone(), None)
        .await
        .unwrap();
    let mut registry = ToolRegistry::new();
    let executor = host
        .register_into_registry(
            fixture("normal", None),
            manifest(),
            PluginLaunchPolicy::default(),
            &mut registry,
        )
        .await
        .unwrap();
    assert_eq!(registry.descriptors().len(), 1);
    assert_eq!(store.plugin_registrations().unwrap()[0].status, "active");
    drop(host);

    let restarted_host = PersistentPluginHost::open(store.clone(), None)
        .await
        .unwrap();
    let restored = restarted_host
        .get(&ToolId::parse("plugin_sysinfo").unwrap())
        .unwrap();
    assert_eq!(
        restored.tool_id(),
        &ToolId::parse("plugin_sysinfo").unwrap()
    );
    drop(restarted_host);
    drop(executor);

    let mut records = store.plugin_registrations().unwrap();
    records[0].manifest_json = "{}".into();
    store.save_plugin_registration(&records[0]).unwrap();
    assert!(matches!(
        PersistentPluginHost::open(store, None).await,
        Err(HostError::ManifestMismatch(_))
    ));
}

#[tokio::test]
async fn persistent_plugin_uses_runtime_policy_and_sqlite_result_transaction() {
    let file = tempfile::NamedTempFile::new().unwrap();
    let store = Arc::new(SqliteContextStore::open(file.path()).unwrap());
    let mut host = PersistentPluginHost::open(store.clone(), None)
        .await
        .unwrap();
    let mut registry = ToolRegistry::new();
    let executor = host
        .register_into_registry(
            fixture("normal", None),
            manifest(),
            PluginLaunchPolicy::default(),
            &mut registry,
        )
        .await
        .unwrap();
    let registry = Arc::new(registry);
    let policy = Arc::new(
        PolicyEngine::new(
            PolicyConfig::new([tempdir().unwrap().path().to_path_buf()])
                .with_enabled_tools([ToolId::parse("plugin_sysinfo").unwrap()]),
        )
        .unwrap(),
    );
    let audit = Arc::new(Audits::default());
    let runtime = ToolRuntime::new(registry, policy, store.clone(), audit.clone())
        .with_transactional_store(store.clone());
    let plan = Plan::new(
        "persistent-plugin",
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
    runtime
        .execute(
            &plan,
            None,
            ToolExecutionJob {
                result_item_id: ContextItemId::parse("sqlite-result").unwrap(),
                scope: ContextScope::project("victor").unwrap(),
                started_at_unix_ms: 10,
            },
            executor.as_ref(),
        )
        .await
        .unwrap();
    assert_eq!(store.audit_count().unwrap(), 1);
    assert!(store
        .get(
            &ContextItemId::parse("sqlite-result").unwrap(),
            &ContextScope::project("victor").unwrap(),
        )
        .unwrap()
        .is_some());
}

#[tokio::test]
async fn policy_denies_plugin_before_executor_for_unsafe_local_manifest() {
    let mut unsafe_manifest = manifest();
    unsafe_manifest.descriptor.risk = RiskLevel::R1;
    let executor = PluginToolExecutor::spawn(fixture("normal", None), unsafe_manifest.clone())
        .await
        .unwrap();
    let mut registry = ToolRegistry::new();
    registry.register(unsafe_manifest).unwrap();
    let policy = Arc::new(
        PolicyEngine::new(
            PolicyConfig::new([tempdir().unwrap().path().to_path_buf()])
                .with_enabled_tools([ToolId::parse("plugin_sysinfo").unwrap()]),
        )
        .unwrap(),
    );
    let context = Arc::new(InMemoryContextStore::new());
    let audit = Arc::new(Audits::default());
    let runtime = ToolRuntime::new(Arc::new(registry), policy, context.clone(), audit.clone());
    let plan = Plan::new(
        "denied-plugin",
        1,
        "unsafe sysinfo",
        vec![PlanStep::new(
            ToolId::parse("plugin_sysinfo").unwrap(),
            "unsafe",
            json!({}),
            RiskLevel::R1,
        )],
    )
    .unwrap();
    let result = runtime
        .execute(
            &plan,
            None,
            ToolExecutionJob {
                result_item_id: ContextItemId::parse("denied-result").unwrap(),
                scope: ContextScope::project("victor").unwrap(),
                started_at_unix_ms: 20,
            },
            &executor,
        )
        .await;
    assert!(matches!(
        result,
        Err(victor_runtime::ToolRuntimeError::PolicyDenied(_))
    ));
    assert_eq!(context.len(), 0);
    assert_eq!(
        audit.0.lock().unwrap()[0].outcome,
        victor_tools::AuditOutcome::Denied
    );
}

fn assert_no_recorded_pids_alive(path: &std::path::Path) {
    let content = std::fs::read_to_string(path).unwrap();
    for pid in content.lines().filter_map(|line| line.parse::<u32>().ok()) {
        let status = std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(!status.success(), "plugin PID {pid} nadal działa");
    }
}
