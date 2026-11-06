use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};
use victor_context::{
    ContextContent, ContextItem, ContextItemId, ContextItemKind, ContextScope, ContextSource,
    ContextStore, MemoryTier, Retention, Sensitivity,
};
use victor_domain::{Capability, Plan, PlanStep, RiskLevel, ToolDescriptor, ToolId};
use victor_fetch_bald::{web_fetch_manifest, BaldFetchConfig};
use victor_policy::{PolicyConfig, PolicyEngine};
use victor_runtime::{ToolExecutionJob, ToolRuntime, ToolRuntimeError};
use victor_skills::{research_summary_skill, SkillError, SkillLifecycle, SkillRegistry};
use victor_storage::SqliteContextStore;
use victor_tools::{
    AuditSink, ToolAuditEvent, ToolError, ToolExecution, ToolExecutor, ToolManifest, ToolRegistry,
};

const NOW: u64 = 1_700_000_000_000;

#[derive(Default)]
struct NoopAudit;

impl AuditSink for NoopAudit {
    fn record(&self, _event: ToolAuditEvent) -> Result<(), ToolError> {
        Ok(())
    }
}

struct FakeWebFetch {
    calls: Arc<Mutex<usize>>,
}

#[async_trait]
impl ToolExecutor for FakeWebFetch {
    fn tool_id(&self) -> &ToolId {
        static ID: std::sync::OnceLock<ToolId> = std::sync::OnceLock::new();
        ID.get_or_init(|| ToolId::parse("web.fetch").unwrap())
    }

    async fn execute(
        &self,
        args: Value,
        started_at_unix_ms: u64,
    ) -> Result<ToolExecution, ToolError> {
        *self.calls.lock().unwrap() += 1;
        let url = args.get("url").and_then(Value::as_str).unwrap_or_default();
        if url != "https://allowed.example/article" {
            return Err(ToolError::Execution {
                tool: "web.fetch".into(),
                reason: "host poza allowlistą web.fetch".into(),
            });
        }
        Ok(ToolExecution {
            output: json!({
                "requested_url": url,
                "final_url": url,
                "content_hash": "sha256:article",
                "fetched_at": "2026-11-06T00:00:00Z",
                "text": "article body",
                "status": 200
            }),
            summary: "pobrano artykuł przez BALD".into(),
            finished_at_unix_ms: started_at_unix_ms + 1,
        })
    }
}

struct StaticExecutor {
    id: ToolId,
    output: Value,
    calls: Arc<Mutex<usize>>,
}

#[async_trait]
impl ToolExecutor for StaticExecutor {
    fn tool_id(&self) -> &ToolId {
        &self.id
    }

    async fn execute(
        &self,
        _args: Value,
        started_at_unix_ms: u64,
    ) -> Result<ToolExecution, ToolError> {
        *self.calls.lock().unwrap() += 1;
        Ok(ToolExecution {
            output: self.output.clone(),
            summary: format!("wykonano {}", self.id),
            finished_at_unix_ms: started_at_unix_ms + 1,
        })
    }
}

fn auxiliary_manifest(id: &str, input: Value, output: Value) -> ToolManifest {
    ToolManifest::new(
        "1.0.0",
        ToolDescriptor {
            id: ToolId::parse(id).unwrap(),
            schema_version: 1,
            description: "testowe narzędzie skill".into(),
            input_schema: input,
            output_schema: output,
            capabilities: vec![Capability::Network {
                provider: "local-test".into(),
            }],
            risk: RiskLevel::R0,
            default_timeout_ms: 1_000,
            undo_support: false,
            path_arguments: vec![],
        },
    )
}

fn setup_registry() -> ToolRegistry {
    let config = BaldFetchConfig::new(
        ["allowed.example".to_string()],
        1_024 * 1024,
        3,
        Duration::from_secs(5),
    )
    .unwrap();
    let mut registry = ToolRegistry::new();
    registry
        .register(web_fetch_manifest(&config).unwrap())
        .unwrap();
    registry
        .register(auxiliary_manifest(
            "context.read",
            json!({
                "type": "object",
                "required": ["source_step"],
                "properties": {"source_step": {"type": "string"}}
            }),
            json!({"type": "object", "required": ["context"], "properties": {"context": {"type": "string"}}}),
        ))
        .unwrap();
    registry
        .register(auxiliary_manifest(
            "summary.create",
            json!({
                "type": "object",
                "required": ["source_step"],
                "properties": {"source_step": {"type": "string"}}
            }),
            json!({"type": "object", "required": ["summary"], "properties": {"summary": {"type": "string"}}}),
        ))
        .unwrap();
    registry
}

fn plan_for(step: &victor_skills::SkillStep, index: usize) -> Plan {
    Plan::new(
        "skill-research-summary",
        index as u32 + 1,
        format!("wykonaj skill step {}", step.id),
        vec![PlanStep::new(
            step.tool.clone(),
            step.id.clone(),
            step.args.clone(),
            RiskLevel::R0,
        )],
    )
    .unwrap()
}

fn context_item(id: &str, scope: &ContextScope, snapshot: String) -> ContextItem {
    ContextItem::new(
        ContextItemId::parse(id).unwrap(),
        scope.clone(),
        ContextItemKind::Note,
        MemoryTier::Durable,
        ContextContent::inline(snapshot),
        ContextSource::new(victor_context::Origin::Tool, "victor-skills"),
        NOW,
    )
    .unwrap()
    .with_sensitivity(Sensitivity::Internal)
    .with_retention(Retention::KeepForever)
    .unwrap()
}

#[tokio::test]
async fn production_shaped_declarative_skill_survives_sqlite_restart() {
    let mut skills = SkillRegistry::new();
    let skill = research_summary_skill().unwrap();
    let skill_id = skill.skill_id.clone();
    skills.install(skill.clone()).unwrap();
    assert!(matches!(
        skills.active_manifest(&skill_id),
        Err(SkillError::InvalidTransition { .. })
    ));

    let tools = setup_registry();
    let root = tempfile::tempdir().unwrap();
    let policy = Arc::new(
        PolicyEngine::new(
            PolicyConfig::new([root.path().to_path_buf()]).with_enabled_tools(
                tools
                    .descriptors()
                    .into_iter()
                    .map(|descriptor| descriptor.id),
            ),
        )
        .unwrap(),
    );
    skills
        .validate_with_policy(&skill_id, &tools, &policy)
        .unwrap();
    skills.approve(&skill_id).unwrap();
    skills.activate(&skill_id).unwrap();
    let active_hash = skills.active_manifest(&skill_id).unwrap().hash.clone();

    let file = tempfile::NamedTempFile::new().unwrap();
    let store = Arc::new(SqliteContextStore::open(file.path()).unwrap());
    let runtime = ToolRuntime::new(
        Arc::new(tools),
        policy.clone(),
        store.clone(),
        Arc::new(NoopAudit),
    )
    .with_transactional_store(store.clone());
    let fetch_calls = Arc::new(Mutex::new(0));
    let context_calls = Arc::new(Mutex::new(0));
    let summary_calls = Arc::new(Mutex::new(0));
    let context_scope = ContextScope::project("skill-project").unwrap();
    let active = skills.active_manifest(&skill_id).unwrap().clone();

    for (index, step) in active.workflow.iter().enumerate() {
        let executor: Box<dyn ToolExecutor> = match step.tool.as_str() {
            "web.fetch" => Box::new(FakeWebFetch {
                calls: fetch_calls.clone(),
            }),
            "context.read" => Box::new(StaticExecutor {
                id: step.tool.clone(),
                output: json!({"context": "article body"}),
                calls: context_calls.clone(),
            }),
            "summary.create" => Box::new(StaticExecutor {
                id: step.tool.clone(),
                output: json!({"summary": "article summary"}),
                calls: summary_calls.clone(),
            }),
            other => panic!("nieoczekiwane narzędzie {other}"),
        };
        runtime
            .execute(
                &plan_for(step, index),
                None,
                ToolExecutionJob {
                    result_item_id: ContextItemId::parse(&format!("skill-{index}")).unwrap(),
                    scope: context_scope.clone(),
                    started_at_unix_ms: NOW + index as u64,
                },
                executor.as_ref(),
            )
            .await
            .unwrap();
    }
    assert_eq!(*fetch_calls.lock().unwrap(), 1);
    assert_eq!(*context_calls.lock().unwrap(), 1);
    assert_eq!(*summary_calls.lock().unwrap(), 1);
    assert_eq!(store.audit_count().unwrap(), 3);
    assert_eq!(store.list(&context_scope).unwrap().len(), 3);
    assert!(store
        .list(&ContextScope::project("outside-scope").unwrap())
        .unwrap()
        .is_empty());

    skills.disable(&skill_id).unwrap();
    assert!(matches!(
        skills.active_manifest(&skill_id),
        Err(SkillError::InvalidTransition { .. })
    ));
    let mut changed = skill.clone();
    changed.workflow[0].args["url"] = json!("https://not-allowed.example/article");
    changed.hash = changed.computed_hash().unwrap();
    skills.update(changed).unwrap();
    skills.rollback(&skill_id).unwrap();
    assert_eq!(skills.manifest(&skill_id).unwrap().hash, active_hash);
    assert_eq!(skills.state(&skill_id).unwrap(), SkillLifecycle::Draft);
    skills
        .validate_with_policy(&skill_id, &setup_registry(), &policy)
        .unwrap();
    skills.approve(&skill_id).unwrap();
    skills.activate(&skill_id).unwrap();

    let snapshot = skills.snapshot_json().unwrap();
    let registry_scope = ContextScope::project("skill-registry").unwrap();
    store
        .put(context_item("skills.snapshot", &registry_scope, snapshot))
        .unwrap();
    drop(runtime);
    drop(store);

    let restarted = SqliteContextStore::open(file.path()).unwrap();
    let restored_item = restarted
        .get(
            &ContextItemId::parse("skills.snapshot").unwrap(),
            &registry_scope,
        )
        .unwrap()
        .unwrap();
    let restored_snapshot = match restored_item.content {
        ContextContent::Inline { text } => text,
        ContextContent::Reference { .. } => panic!("snapshot nie może być referencją"),
    };
    let restored = SkillRegistry::from_snapshot_json(&restored_snapshot).unwrap();
    assert_eq!(restored.state(&skill_id).unwrap(), SkillLifecycle::Active);
    assert_eq!(
        restored.active_manifest(&skill_id).unwrap().hash,
        active_hash
    );
    assert_eq!(restarted.audit_count().unwrap(), 3);
}

#[tokio::test]
async fn skill_denial_stops_workflow_and_allowlist_cannot_be_bypassed() {
    let mut skill = research_summary_skill().unwrap();
    skill.workflow[0].args["url"] = json!("https://not-allowed.example/article");
    skill.hash = skill.computed_hash().unwrap();
    let mut skills = SkillRegistry::new();
    skills.install(skill.clone()).unwrap();
    let tools = setup_registry();
    let root = tempfile::tempdir().unwrap();
    let policy = Arc::new(
        PolicyEngine::new(
            PolicyConfig::new([root.path().to_path_buf()]).with_enabled_tools(
                tools
                    .descriptors()
                    .into_iter()
                    .map(|descriptor| descriptor.id),
            ),
        )
        .unwrap(),
    );
    skills
        .validate_with_policy(&skill.skill_id, &tools, &policy)
        .unwrap();
    skills.approve(&skill.skill_id).unwrap();
    skills.activate(&skill.skill_id).unwrap();

    let store = Arc::new(SqliteContextStore::in_memory().unwrap());
    let runtime = ToolRuntime::new(Arc::new(tools), policy, store.clone(), Arc::new(NoopAudit))
        .with_transactional_store(store.clone());
    let context_calls = Arc::new(Mutex::new(0));
    let fetch_calls = Arc::new(Mutex::new(0));
    let mut executed_steps = 0;
    for (index, step) in skill.workflow.iter().enumerate() {
        let executor: Box<dyn ToolExecutor> = if index == 0 {
            Box::new(FakeWebFetch {
                calls: fetch_calls.clone(),
            })
        } else {
            Box::new(StaticExecutor {
                id: step.tool.clone(),
                output: json!({"context": "must not run"}),
                calls: context_calls.clone(),
            })
        };
        let result = runtime
            .execute(
                &plan_for(step, index),
                None,
                ToolExecutionJob {
                    result_item_id: ContextItemId::parse(&format!("bad-step-{index}")).unwrap(),
                    scope: ContextScope::project("skill-project").unwrap(),
                    started_at_unix_ms: NOW + index as u64,
                },
                executor.as_ref(),
            )
            .await;
        match result {
            Ok(_) => executed_steps += 1,
            Err(ToolRuntimeError::Tool(ToolError::Execution { .. })) => break,
            Err(other) => panic!("nieoczekiwany błąd: {other}"),
        }
    }
    assert_eq!(executed_steps, 0);
    assert_eq!(*fetch_calls.lock().unwrap(), 1);
    assert_eq!(*context_calls.lock().unwrap(), 0);
}
