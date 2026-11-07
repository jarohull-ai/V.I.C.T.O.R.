use std::collections::BTreeMap;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

use sha2::{Digest, Sha256};
use tempfile::tempdir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::oneshot;
use victor_daemon::{
    Daemon, DaemonConfig, TrustedLocalModelDefinition, TrustedLocalModelSpec,
    TrustedPluginDefinition,
};
use victor_domain::{RiskLevel, ToolDescriptor, ToolId};
use victor_plugin_host::{PluginLaunchRequest, SandboxRequirement};
use victor_tools::ToolManifest;

fn hash(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    format!("sha256:{digest:x}")
}

fn private_file(path: &Path, bytes: &[u8]) {
    std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(path)
        .unwrap()
        .write_all(bytes)
        .unwrap();
}

#[tokio::test]
async fn chat_routes_local_model_policy_plugin_context_and_audit() {
    let root = tempdir().unwrap();
    let runtime = root.path().join("run");
    let defs = root.path().join("defs");
    std::fs::create_dir(&runtime).unwrap();
    std::fs::create_dir(&defs).unwrap();
    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::set_permissions(&defs, std::fs::Permissions::from_mode(0o700)).unwrap();
    let model_bin = std::path::PathBuf::from(env!("CARGO_BIN_EXE_daemon_model_fixture"));
    let plugin_bin = std::path::PathBuf::from(env!("CARGO_BIN_EXE_daemon_plugin_fixture"));
    for binary in [&model_bin, &plugin_bin] {
        std::fs::set_permissions(binary, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let manifest = ToolManifest::new(
        "1.0.0",
        ToolDescriptor {
            id: ToolId::parse("plugin_sysinfo").unwrap(),
            schema_version: 1,
            description: "sysinfo".into(),
            input_schema: serde_json::json!({"type":"object"}),
            output_schema: serde_json::json!({"type":"object"}),
            capabilities: vec![],
            risk: RiskLevel::R0,
            default_timeout_ms: 1000,
            undo_support: false,
            path_arguments: vec![],
        },
    );
    let plugin = TrustedPluginDefinition {
        id: ToolId::parse("plugin_sysinfo").unwrap(),
        manifest,
        launch: PluginLaunchRequest {
            command: vec![plugin_bin.display().to_string()],
            working_dir: root.path().display().to_string(),
            environment: BTreeMap::new(),
            sandbox: SandboxRequirement::NotRequired,
            sandbox_executable: None,
            request_timeout_ms: 1000,
            max_frame_bytes: 64 * 1024,
            max_restart_attempts: 1,
        },
    };
    private_file(
        &defs.join("plugin.json"),
        &serde_json::to_vec(&plugin).unwrap(),
    );
    let mut files_manifest = plugin.manifest.clone();
    files_manifest.descriptor.id = ToolId::parse("files.list").unwrap();
    let files_plugin = TrustedPluginDefinition {
        id: ToolId::parse("files.list").unwrap(),
        manifest: files_manifest,
        launch: PluginLaunchRequest {
            command: vec![plugin_bin.display().to_string(), "files".into()],
            ..plugin.launch.clone()
        },
    };
    private_file(
        &defs.join("files.json"),
        &serde_json::to_vec(&files_plugin).unwrap(),
    );
    let spec = TrustedLocalModelSpec {
        command: vec![model_bin.display().to_string()],
        working_dir: root.path().display().to_string(),
        environment: BTreeMap::new(),
        timeout_ms: 1000,
        max_frame_bytes: 64 * 1024,
        max_stderr_bytes: 4096,
        max_restart_attempts: 1,
        sandbox_required: false,
        sandbox_executable: None,
        executable_sha256: hash(&std::fs::read(&model_bin).unwrap()),
    };
    let model = TrustedLocalModelDefinition {
        config_sha256: hash(&serde_json::to_vec(&spec).unwrap()),
        spec,
    };
    private_file(
        &defs.join("model.conf"),
        &serde_json::to_vec(&model).unwrap(),
    );
    let token_path = root.path().join("token");
    private_file(&token_path, b"0123456789abcdef");
    let config = DaemonConfig {
        sqlite_path: root.path().join("state.sqlite"),
        socket_path: runtime.join("victor.sock"),
        runtime_dir: runtime,
        trusted_definitions_dir: defs,
        allowed_roots: vec![root.path().to_path_buf()],
        token_file: token_path,
        scope: "local".into(),
        local_model_definition: root.path().join("defs/model.conf"),
    };
    let daemon = Daemon::prepare(config).await.unwrap();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let task = tokio::spawn(daemon.run_with_shutdown(shutdown_rx));
    let socket = root.path().join("run/victor.sock");
    for _ in 0..50 {
        if socket.exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    let mut stream = UnixStream::connect(&socket).await.unwrap();
    let request = serde_json::json!({"token":"0123456789abcdef","scope":"local","idempotency_key":"chat-1","operation":{"operation":"chat","conversation_id":"c1","input":"pokaż status systemu","at_unix_ms":1,"approval":null}});
    stream
        .write_all(format!("{}\n", request).as_bytes())
        .await
        .unwrap();
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    let response: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(response["ok"], true, "{response}");
    assert_eq!(response["result"]["status"], "executed");
    assert_eq!(response["result"]["tool"], "plugin_sysinfo");
    let request = serde_json::json!({"token":"0123456789abcdef","scope":"local","idempotency_key":"chat-2","operation":{"operation":"chat","conversation_id":"c1","input":"pokaż pliki","at_unix_ms":2,"approval":null}});
    reader
        .get_mut()
        .write_all(format!("{}\n", request).as_bytes())
        .await
        .unwrap();
    line.clear();
    reader.read_line(&mut line).await.unwrap();
    let response: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(response["ok"], true, "{response}");
    assert_eq!(response["result"]["tool"], "files.list");
    let _ = shutdown_tx.send(());
    assert!(task.await.unwrap().is_ok());
}
