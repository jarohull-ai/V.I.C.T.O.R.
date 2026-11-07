use std::collections::BTreeMap;
use std::io::Write;
use std::sync::{Arc, Mutex};

use serde_json::json;
use tempfile::tempdir;
use victor_context::{ContextItemId, ContextScope, ContextStore, InMemoryContextStore};
use victor_domain::{Plan, PlanStep, RiskLevel, ToolDescriptor, ToolId};
use victor_plugin_host::{
    HostError, PersistentPluginHost, PluginLaunchPolicy, PluginLaunchRequest, PluginLifecycleApi,
    PluginToolExecutor, SandboxRequirement,
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
    let executable = std::path::Path::new(env!("CARGO_BIN_EXE_plugin_fixture"));
    let metadata = std::fs::metadata(executable).unwrap();
    let mut permissions = metadata.permissions();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        permissions.set_mode(permissions.mode() & !0o022);
        std::fs::set_permissions(executable, permissions).unwrap();
    }
    let mut command = vec![executable.display().to_string(), mode.into()];
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
async fn files_read_goes_through_policy_plugin_context_and_audit() {
    let root = tempdir().unwrap();
    let file = root.path().join("note.txt");
    std::fs::write(&file, "bezpieczna tresc").unwrap();
    let manifest = ToolManifest::new(
        "1.0.0",
        ToolDescriptor {
            id: ToolId::parse("files.read").unwrap(),
            schema_version: 1,
            description: "read-only file".into(),
            input_schema: json!({"type":"object","required":["path"],"properties":{"path":{"type":"string"}}}),
            output_schema: json!({"type":"object"}),
            capabilities: vec![victor_domain::Capability::FilesystemRead {
                scope: victor_domain::PathScope::new(root.path()),
            }],
            risk: RiskLevel::R0,
            default_timeout_ms: 2_000,
            undo_support: false,
            path_arguments: vec!["path".into()],
        },
    );
    let executable = std::path::Path::new(env!("CARGO_BIN_EXE_files_inspect"));
    let mut executable_permissions = std::fs::metadata(executable).unwrap().permissions();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        executable_permissions.set_mode(executable_permissions.mode() & !0o022);
        std::fs::set_permissions(executable, executable_permissions).unwrap();
    }
    let command = vec![
        executable.display().to_string(),
        "--root".into(),
        root.path().display().to_string(),
    ];
    let policy = Arc::new(
        PolicyEngine::new(
            PolicyConfig::new([root.path().to_path_buf()])
                .with_enabled_tools([ToolId::parse("files.read").unwrap()]),
        )
        .unwrap(),
    );
    let store = Arc::new(SqliteContextStore::in_memory().unwrap());
    let mut registry = ToolRegistry::new();
    registry.register(manifest.clone()).unwrap();
    let runtime = ToolRuntime::new(Arc::new(registry), policy, store.clone(), store.clone())
        .with_transactional_store(store.clone());
    let executor = PluginToolExecutor::spawn_with_policy(
        command,
        manifest,
        PluginLaunchPolicy {
            working_dir: Some(root.path().to_path_buf()),
            ..PluginLaunchPolicy::default()
        },
        Some(store.clone()),
    )
    .await
    .unwrap();
    let tool = ToolId::parse("files.read").unwrap();
    let plan = Plan::new(
        "conversation-files",
        1,
        "read note",
        vec![PlanStep::new(
            tool.clone(),
            "read note",
            json!({"path": file, "max_bytes": 1024}),
            RiskLevel::R0,
        )],
    )
    .unwrap();
    let output = runtime
        .execute(
            &plan,
            None,
            victor_runtime::ToolExecutionJob {
                result_item_id: ContextItemId::parse("files-result").unwrap(),
                scope: ContextScope::conversation("conversation-files").unwrap(),
                started_at_unix_ms: 1,
            },
            &executor,
        )
        .await
        .unwrap();
    assert_eq!(output.result.output["content"], "bezpieczna tresc");
    assert!(store
        .get(&output.context_item.id, &output.context_item.scope)
        .unwrap()
        .is_some());
    assert_eq!(store.audit_count().unwrap(), 1);
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

#[tokio::test]
async fn lifecycle_persists_full_launch_spec_and_requires_explicit_unregister() {
    let database = tempfile::NamedTempFile::new().unwrap();
    let working_dir = tempdir().unwrap();
    let store = Arc::new(SqliteContextStore::open(database.path()).unwrap());
    let lifecycle_audit = Arc::new(Audits::default());
    std::env::set_var("VICTOR_TEST_SECRET", "must-not-leak");
    let mut policy = PluginLaunchPolicy {
        working_dir: Some(working_dir.path().to_path_buf()),
        environment: BTreeMap::from([(String::from("VICTOR_ALLOWED"), String::from("yes"))]),
        ..PluginLaunchPolicy::default()
    };
    let mut host = PersistentPluginHost::open(store.clone(), Some(lifecycle_audit.clone()))
        .await
        .unwrap();
    let mut registry = ToolRegistry::new();
    let command = fixture("cwd-env", None);
    host.register_into_registry(command, manifest(), policy.clone(), &mut registry)
        .await
        .unwrap();
    let executor = host.get(&ToolId::parse("plugin_sysinfo").unwrap()).unwrap();
    let result = executor.execute(json!({}), 30).await.unwrap();
    assert_eq!(result.output["allowed"], "yes");
    assert_eq!(result.output["secret_present"], false);
    assert_eq!(
        result.output["cwd"],
        working_dir.path().display().to_string()
    );
    assert!(host
        .unregister(&ToolId::parse("plugin_sysinfo").unwrap(), false)
        .is_err());
    host.disable(&ToolId::parse("plugin_sysinfo").unwrap())
        .unwrap();
    assert!(host
        .get(&ToolId::parse("plugin_sysinfo").unwrap())
        .is_none());
    host.activate(&ToolId::parse("plugin_sysinfo").unwrap(), &mut registry)
        .await
        .unwrap();
    host.restart(&ToolId::parse("plugin_sysinfo").unwrap())
        .await
        .unwrap();
    host.unregister(&ToolId::parse("plugin_sysinfo").unwrap(), true)
        .unwrap();
    assert!(store.plugin_registrations().unwrap().is_empty());
    assert!(lifecycle_audit.0.lock().unwrap().len() >= 5);
    std::env::remove_var("VICTOR_TEST_SECRET");
    policy.environment.insert("API_TOKEN".into(), "x".into());
    assert!(matches!(
        PluginToolExecutor::spawn_with_policy(fixture("normal", None), manifest(), policy, None)
            .await,
        Err(HostError::InvalidLaunchPolicy(_))
    ));
}

#[tokio::test]
async fn registration_is_idempotent_but_manifest_conflicts_are_rejected() {
    let database = tempfile::NamedTempFile::new().unwrap();
    let store = Arc::new(SqliteContextStore::open(database.path()).unwrap());
    let mut host = PersistentPluginHost::open(store, None).await.unwrap();
    let command = fixture("normal", None);
    let first = host
        .register(command.clone(), manifest(), PluginLaunchPolicy::default())
        .await
        .unwrap();
    let second = host
        .register(command, manifest(), PluginLaunchPolicy::default())
        .await
        .unwrap();
    assert!(Arc::ptr_eq(&first, &second));
    let mut changed = manifest();
    changed.tool_version = "2.0.0".into();
    assert!(matches!(
        host.register(
            fixture("normal", None),
            changed,
            PluginLaunchPolicy::default()
        )
        .await,
        Err(HostError::RegistrationConflict(_))
    ));
}

#[tokio::test]
async fn lifecycle_api_uses_trusted_definition_and_hides_launch_details() {
    let database = Arc::new(SqliteContextStore::in_memory().unwrap());
    let root = tempdir().unwrap();
    let id = ToolId::parse("plugin_sysinfo").unwrap();
    let policy = Arc::new(
        PolicyEngine::new(
            PolicyConfig::new([root.path().to_path_buf()]).with_enabled_tools([id.clone()]),
        )
        .unwrap(),
    );
    let api = PluginLifecycleApi::open(database, policy, None)
        .await
        .unwrap()
        .with_plugin_definition(
            manifest(),
            fixture("normal", None),
            PluginLaunchPolicy::default(),
        )
        .unwrap();
    let mut api = api;
    let launch = PluginLaunchRequest {
        command: fixture("normal", None),
        working_dir: std::env::current_dir().unwrap().display().to_string(),
        environment: BTreeMap::new(),
        sandbox: SandboxRequirement::NotRequired,
        sandbox_executable: None,
        request_timeout_ms: 30_000,
        max_frame_bytes: 256 * 1024,
        max_restart_attempts: 1,
    };
    let summary = api
        .register(
            manifest(),
            launch,
            None,
            "test".into(),
            "register".into(),
            1,
        )
        .await
        .unwrap();
    assert_eq!(summary.status, "active");
    let review = api.review(&id).unwrap();
    assert!(review.launch_spec_verified);
    assert!(review.environment_keys.is_empty());
    assert_eq!(review.argument_count, 1);
    api.disable(&id, "test".into(), "disable".into(), 2)
        .unwrap();
    api.activate(&id, None, "test".into(), "activate".into(), 3)
        .await
        .unwrap();
    api.restart(&id, None, "test".into(), "restart".into(), 4)
        .await
        .unwrap();
    assert_eq!(api.audit_history(Some(&id)).unwrap().len(), 4);
    api.unregister(&id, true, None, "test".into(), "remove".into(), 5)
        .unwrap();
    assert!(api.list().unwrap().is_empty());
}

#[tokio::test]
async fn restore_rejects_changed_binary_symlink_and_missing_working_dir() {
    let executable_source = std::path::Path::new(env!("CARGO_BIN_EXE_plugin_fixture"));
    let directory = tempdir().unwrap();
    let executable = directory.path().join("plugin-fixture");
    std::fs::copy(executable_source, &executable).unwrap();
    let mut permissions = std::fs::metadata(&executable).unwrap().permissions();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        permissions.set_mode(0o755);
        std::fs::set_permissions(&executable, permissions).unwrap();
    }
    let database = tempfile::NamedTempFile::new().unwrap();
    let store = Arc::new(SqliteContextStore::open(database.path()).unwrap());
    let mut host = PersistentPluginHost::open(store.clone(), None)
        .await
        .unwrap();
    host.register(
        vec![executable.display().to_string(), "normal".into()],
        manifest(),
        PluginLaunchPolicy::default(),
    )
    .await
    .unwrap();
    drop(host);
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    std::fs::OpenOptions::new()
        .append(true)
        .open(&executable)
        .unwrap()
        .write_all(b"changed")
        .unwrap();
    assert!(matches!(
        PersistentPluginHost::open(store.clone(), None).await,
        Err(HostError::StaleLaunchSpec(_))
    ));

    let symlink = directory.path().join("plugin-link");
    std::os::unix::fs::symlink(executable_source, &symlink).unwrap();
    assert!(matches!(
        PluginToolExecutor::spawn(
            vec![symlink.display().to_string(), "normal".into()],
            manifest()
        )
        .await,
        Err(HostError::UnsafeExecutable(_))
    ));
    let missing = directory.path().join("missing");
    let policy = PluginLaunchPolicy {
        working_dir: Some(missing),
        ..PluginLaunchPolicy::default()
    };
    assert!(matches!(
        PluginToolExecutor::spawn_with_policy(fixture("normal", None), manifest(), policy, None)
            .await,
        Err(HostError::InvalidLaunchPolicy(_))
    ));
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
