use std::sync::{Arc, Mutex};

use serde_json::json;
use tempfile::tempdir;
use victor_context::{ContextItemId, ContextScope, InMemoryContextStore};
use victor_domain::{Capability, PathScope, Plan, PlanStep, RiskLevel, ToolDescriptor, ToolId};
use victor_plugin_host::{PluginLaunchPolicy, PluginToolExecutor};
use victor_policy::{PolicyConfig, PolicyEngine};
use victor_runtime::{ToolExecutionJob, ToolRuntime};
use victor_tools::{AuditSink, ToolAuditEvent, ToolError, ToolManifest, ToolRegistry};

#[derive(Default)]
struct Audit(Mutex<Vec<ToolAuditEvent>>);
impl AuditSink for Audit {
    fn record(&self, event: ToolAuditEvent) -> Result<(), ToolError> {
        self.0.lock().unwrap().push(event);
        Ok(())
    }
}

fn command(root: &std::path::Path) -> Vec<String> {
    let executable = std::path::Path::new(env!("CARGO_BIN_EXE_files_inspect"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(executable).unwrap().permissions();
        permissions.set_mode(permissions.mode() & !0o022);
        std::fs::set_permissions(executable, permissions).unwrap();
    }
    vec![
        executable.display().to_string(),
        "--root".into(),
        root.display().to_string(),
    ]
}
fn list_manifest(root: &std::path::Path) -> ToolManifest {
    ToolManifest::new(
        "1.0.0",
        ToolDescriptor {
            id: ToolId::parse("files.list").unwrap(),
            schema_version: 1,
            description: "Lista elementów w zaufanym root".into(),
            input_schema: json!({"type":"object","properties":{"path":{"type":"string"}}}),
            output_schema: json!({"type":"object"}),
            capabilities: vec![Capability::FilesystemRead {
                scope: PathScope::new(root),
            }],
            risk: RiskLevel::R0,
            default_timeout_ms: 2_000,
            undo_support: false,
            path_arguments: vec!["path".into()],
        },
    )
}

fn inspect_manifest(root: &std::path::Path) -> ToolManifest {
    ToolManifest::new(
        "1.0.0",
        ToolDescriptor {
            id: ToolId::parse("files.inspect").unwrap(),
            schema_version: 1,
            description: "Inspekcja metadanych w zaufanym root".into(),
            input_schema: json!({"type":"object","required":["path"],"properties":{"path":{"type":"string"}}}),
            output_schema: json!({"type":"object"}),
            capabilities: vec![Capability::FilesystemRead {
                scope: PathScope::new(root),
            }],
            risk: RiskLevel::R0,
            default_timeout_ms: 2_000,
            undo_support: false,
            path_arguments: vec!["path".into()],
        },
    )
}
fn read_manifest(root: &std::path::Path) -> ToolManifest {
    ToolManifest::new(
        "1.0.0",
        ToolDescriptor {
            id: ToolId::parse("files.read").unwrap(),
            schema_version: 1,
            description: "Czyta ograniczony tekst w zaufanym root".into(),
            input_schema: json!({"type":"object","required":["path"],"properties":{"path":{"type":"string"}}}),
            output_schema: json!({"type":"object"}),
            capabilities: vec![Capability::FilesystemRead {
                scope: PathScope::new(root),
            }],
            risk: RiskLevel::R0,
            default_timeout_ms: 2_000,
            undo_support: false,
            path_arguments: vec!["path".into()],
        },
    )
}

async fn setup(
    root: &std::path::Path,
    manifest: ToolManifest,
) -> (
    ToolRuntime,
    Arc<PluginToolExecutor>,
    Arc<InMemoryContextStore>,
    Arc<Audit>,
) {
    let executor = Arc::new(
        PluginToolExecutor::spawn_with_policy(
            command(root),
            manifest.clone(),
            PluginLaunchPolicy {
                working_dir: Some(root.to_path_buf()),
                ..PluginLaunchPolicy::default()
            },
            None,
        )
        .await
        .unwrap(),
    );
    let mut registry = ToolRegistry::new();
    registry.register(manifest.clone()).unwrap();
    let registry = Arc::new(registry);
    let policy = Arc::new(
        PolicyEngine::new(
            PolicyConfig::new([root.to_path_buf()])
                .with_enabled_tools([manifest.descriptor.id.clone()]),
        )
        .unwrap(),
    );
    let context = Arc::new(InMemoryContextStore::new());
    let audit = Arc::new(Audit::default());
    (
        ToolRuntime::new(registry, policy, context.clone(), audit.clone()),
        executor,
        context,
        audit,
    )
}

#[tokio::test]
async fn list_and_read_are_r0_and_record_provenance() {
    let directory = tempdir().unwrap();
    let root = directory.path();
    std::fs::write(root.join("hello.txt"), "Witaj VICTOR").unwrap();
    let (runtime, executor, context, audit) = setup(root, read_manifest(root)).await;
    let plan = Plan::new(
        "files",
        1,
        "read",
        vec![PlanStep::new(
            ToolId::parse("files.read").unwrap(),
            "read",
            json!({"path": root.join("hello.txt").display().to_string()}),
            RiskLevel::R0,
        )],
    )
    .unwrap();
    let outcome = runtime
        .execute(
            &plan,
            None,
            ToolExecutionJob {
                result_item_id: ContextItemId::parse("result-read").unwrap(),
                scope: ContextScope::project("files").unwrap(),
                started_at_unix_ms: 1,
            },
            executor.as_ref(),
        )
        .await
        .unwrap();
    assert_eq!(outcome.result.output["provenance"], "files.inspect.local");
    assert_eq!(outcome.result.output["content"], "Witaj VICTOR");
    assert_eq!(context.len(), 1);
    assert_eq!(audit.0.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn policy_and_plugin_reject_escape_secret_binary_and_limits() {
    let directory = tempdir().unwrap();
    let root = directory.path();
    let outside = tempdir().unwrap();
    std::fs::write(root.join(".ssh"), "secret").unwrap();
    std::fs::write(root.join("binary"), [0, 159, 146, 150]).unwrap();
    std::fs::write(root.join("large"), vec![b'x'; 1024 * 1024 + 1]).unwrap();
    std::os::unix::fs::symlink(outside.path(), root.join("escape")).unwrap();
    let (runtime, executor, context, audit) = setup(root, read_manifest(root)).await;
    for path in [
        "../outside",
        "/etc/passwd",
        ".ssh",
        "escape",
        "binary",
        "large",
    ] {
        let plan = Plan::new("files-denied", 1, "read", vec![PlanStep::new(ToolId::parse("files.read").unwrap(), "read", json!({"path": if path.starts_with('/') { path.to_string() } else { root.join(path).display().to_string() }}), RiskLevel::R0)]).unwrap();
        let result = runtime
            .execute(
                &plan,
                None,
                ToolExecutionJob {
                    result_item_id: ContextItemId::parse(&format!(
                        "result-{}",
                        audit.0.lock().unwrap().len() + 1
                    ))
                    .unwrap(),
                    scope: ContextScope::project("denied").unwrap(),
                    started_at_unix_ms: 2,
                },
                executor.as_ref(),
            )
            .await;
        assert!(result.is_err(), "{path} powinno być odrzucone");
    }
    assert!(context.is_empty());
    assert_eq!(audit.0.lock().unwrap().len(), 6);
    assert!(audit.0.lock().unwrap().iter().all(|event| matches!(
        event.outcome,
        victor_tools::AuditOutcome::Denied | victor_tools::AuditOutcome::Failed
    )));
}

#[tokio::test]
async fn list_has_bounded_result_without_approval() {
    let directory = tempdir().unwrap();
    let root = directory.path();
    for index in 0..20 {
        std::fs::write(root.join(format!("file-{index}.txt")), "ok").unwrap();
    }
    let (runtime, executor, _context, _audit) = setup(root, list_manifest(root)).await;
    let plan = Plan::new(
        "files-list",
        1,
        "list",
        vec![PlanStep::new(
            ToolId::parse("files.list").unwrap(),
            "list",
            json!({"path": root.display().to_string()}),
            RiskLevel::R0,
        )],
    )
    .unwrap();
    let result = runtime
        .execute(
            &plan,
            None,
            ToolExecutionJob {
                result_item_id: ContextItemId::parse("result-list").unwrap(),
                scope: ContextScope::project("list").unwrap(),
                started_at_unix_ms: 3,
            },
            executor.as_ref(),
        )
        .await
        .unwrap();
    assert_eq!(result.result.output["provenance"], "files.inspect.local");
    assert_eq!(result.result.output["count"], 20);
}

#[tokio::test]
async fn inspect_is_read_only_and_hashes_file_metadata() {
    let directory = tempdir().unwrap();
    let root = directory.path();
    let file = root.join("hello.txt");
    std::fs::write(&file, "hello").unwrap();
    let (runtime, executor, _context, _audit) = setup(root, inspect_manifest(root)).await;
    let plan = Plan::new(
        "files-inspect",
        1,
        "inspect",
        vec![PlanStep::new(
            ToolId::parse("files.inspect").unwrap(),
            "inspect",
            json!({"path": file.display().to_string()}),
            RiskLevel::R0,
        )],
    )
    .unwrap();
    let result = runtime
        .execute(
            &plan,
            None,
            ToolExecutionJob {
                result_item_id: ContextItemId::parse("result-inspect").unwrap(),
                scope: ContextScope::project("inspect").unwrap(),
                started_at_unix_ms: 4,
            },
            executor.as_ref(),
        )
        .await
        .unwrap();
    assert_eq!(result.result.output["kind"], "file");
    assert_eq!(result.result.output["provenance"], "files.inspect.local");
    assert_eq!(result.result.output["sha256"].as_str().unwrap().len(), 64);
}
