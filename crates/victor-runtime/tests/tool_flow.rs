use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::json;
use victor_context::{
    ContextContent, ContextItemId, ContextScope, ContextStore, InMemoryContextStore,
};
use victor_domain::{Capability, Plan, PlanStep, RiskLevel, ToolDescriptor, ToolId};
use victor_policy::{PolicyConfig, PolicyEngine};
use victor_runtime::{ToolExecutionJob, ToolRuntime, ToolRuntimeError};
use victor_tools::{
    AuditSink, ToolAuditEvent, ToolError, ToolExecution, ToolExecutor, ToolManifest, ToolRegistry,
};

fn manifest(id: &str, risk: RiskLevel, undo_support: bool) -> ToolManifest {
    ToolManifest::new(
        "1.0.0",
        ToolDescriptor {
            id: ToolId::parse(id).unwrap(),
            schema_version: 1,
            description: "testowe narzędzie".into(),
            input_schema: json!({
                "type": "object",
                "required": ["query"],
                "properties": {"query": {"type": "string"}}
            }),
            output_schema: json!({
                "type": "object",
                "required": ["items"],
                "properties": {"items": {"type": "integer"}}
            }),
            capabilities: vec![Capability::Network {
                provider: "test".into(),
            }],
            risk,
            default_timeout_ms: 1_000,
            undo_support,
            path_arguments: vec![],
        },
    )
}

#[derive(Clone)]
struct FakeExecutor {
    id: ToolId,
    calls: Arc<Mutex<usize>>,
}

#[async_trait]
impl ToolExecutor for FakeExecutor {
    fn tool_id(&self) -> &ToolId {
        &self.id
    }

    async fn execute(
        &self,
        _args: serde_json::Value,
        started_at_unix_ms: u64,
    ) -> Result<ToolExecution, ToolError> {
        *self.calls.lock().unwrap() += 1;
        Ok(ToolExecution {
            output: json!({"items": 3}),
            summary: "znaleziono 3 elementy".into(),
            finished_at_unix_ms: started_at_unix_ms + 5,
        })
    }
}

#[derive(Default)]
struct MemoryAudit {
    events: Mutex<Vec<ToolAuditEvent>>,
}

impl AuditSink for MemoryAudit {
    fn record(&self, event: ToolAuditEvent) -> Result<(), ToolError> {
        self.events.lock().unwrap().push(event);
        Ok(())
    }
}

fn setup(
    manifest: ToolManifest,
    risk: RiskLevel,
    undo_support: bool,
) -> (
    ToolRuntime,
    Arc<InMemoryContextStore>,
    Arc<MemoryAudit>,
    FakeExecutor,
    Plan,
) {
    let id = manifest.descriptor.id.clone();
    let mut registry = ToolRegistry::new();
    registry.register(manifest).unwrap();
    let root = tempfile::tempdir().unwrap();
    let policy = Arc::new(
        PolicyEngine::new(
            PolicyConfig::new([root.path().to_path_buf()])
                .with_enabled_tools([id.clone()])
                .allow_filesystem_write(false),
        )
        .unwrap(),
    );
    let context = Arc::new(InMemoryContextStore::new());
    let audit = Arc::new(MemoryAudit::default());
    let runtime = ToolRuntime::new(Arc::new(registry), policy, context.clone(), audit.clone());
    let calls = Arc::new(Mutex::new(0));
    let executor = FakeExecutor {
        id: id.clone(),
        calls,
    };
    let plan = Plan::new(
        "task-tools",
        1,
        "uruchom narzędzie",
        vec![PlanStep::new(id, "wykonaj", json!({"query": "demo"}), risk)],
    )
    .unwrap();
    let _ = undo_support;
    (runtime, context, audit, executor, plan)
}

#[tokio::test]
async fn allowed_tool_result_is_written_to_context_and_audit() {
    let (runtime, context, audit, executor, plan) = setup(
        manifest("demo.read", RiskLevel::R0, false),
        RiskLevel::R0,
        false,
    );
    let scope = ContextScope::project("victor").unwrap();
    let outcome = runtime
        .execute(
            &plan,
            None,
            ToolExecutionJob {
                result_item_id: ContextItemId::parse("tool-1").unwrap(),
                scope: scope.clone(),
                started_at_unix_ms: 100,
            },
            &executor,
        )
        .await
        .unwrap();

    assert_eq!(*executor.calls.lock().unwrap(), 1);
    assert_eq!(outcome.result.output, json!({"items": 3}));
    let stored = context
        .get(&ContextItemId::parse("tool-1").unwrap(), &scope)
        .unwrap()
        .unwrap();
    assert!(matches!(stored.content, ContextContent::Inline { .. }));
    assert_eq!(audit.events.lock().unwrap().len(), 1);
    assert_eq!(
        audit.events.lock().unwrap()[0].outcome,
        victor_tools::AuditOutcome::Succeeded
    );
}

#[tokio::test]
async fn approval_is_required_before_r1_executor_runs() {
    let (runtime, context, audit, executor, plan) = setup(
        manifest("demo.write", RiskLevel::R1, true),
        RiskLevel::R1,
        true,
    );
    let scope = ContextScope::project("victor").unwrap();
    let error = runtime
        .execute(
            &plan,
            None,
            ToolExecutionJob {
                result_item_id: ContextItemId::parse("tool-denied").unwrap(),
                scope,
                started_at_unix_ms: 100,
            },
            &executor,
        )
        .await
        .unwrap_err();

    assert!(matches!(error, ToolRuntimeError::Policy(_)));
    assert_eq!(*executor.calls.lock().unwrap(), 0);
    assert!(context.is_empty());
    assert!(audit.events.lock().unwrap().is_empty());
}
