use std::collections::BTreeMap;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

use sha2::{Digest, Sha256};
use tempfile::tempdir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::oneshot;
use victor_context::{ContextContent, ContextScope, ContextStore};
use victor_daemon::{
    Daemon, DaemonConfig, TrustedLocalModelDefinition, TrustedLocalModelSpec,
    TrustedLocalSttDefinition, TrustedPluginDefinition,
};
use victor_domain::{RiskLevel, ToolDescriptor, ToolId};
use victor_plugin_host::{PluginLaunchRequest, SandboxRequirement};
use victor_storage::SqliteContextStore;
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
    let fixture_model = std::path::PathBuf::from(env!("CARGO_BIN_EXE_daemon_model_fixture"));
    let fixture_stt = std::path::PathBuf::from(env!("CARGO_BIN_EXE_daemon_stt_fixture"));
    let fixture_plugin = std::path::PathBuf::from(env!("CARGO_BIN_EXE_daemon_plugin_fixture"));
    let model_bin = root.path().join("model-fixture");
    let stt_bin = root.path().join("stt-fixture");
    let plugin_bin = root.path().join("plugin-fixture");
    for (source, target) in [
        (&fixture_model, &model_bin),
        (&fixture_stt, &stt_bin),
        (&fixture_plugin, &plugin_bin),
    ] {
        std::fs::copy(source, target).unwrap();
        std::fs::set_permissions(target, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    for binary in [&model_bin, &plugin_bin, &stt_bin] {
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
    let stt_spec = victor_daemon::TrustedLocalSttSpec {
        executable: stt_bin.display().to_string(),
        model: stt_bin.display().to_string(),
        language: Some("pl".into()),
        threads: Some(1),
        timeout_ms: 1000,
        max_wav_bytes: 1024 * 1024,
        working_dir: root.path().display().to_string(),
        environment: BTreeMap::new(),
        sandbox_required: false,
        sandbox_executable: None,
        executable_sha256: hash(&std::fs::read(&stt_bin).unwrap()),
        model_sha256: hash(&std::fs::read(&stt_bin).unwrap()),
    };
    let stt = TrustedLocalSttDefinition {
        config_sha256: hash(&serde_json::to_vec(&stt_spec).unwrap()),
        spec: stt_spec,
    };
    private_file(
        &defs.join("local-stt.json"),
        &serde_json::to_vec(&stt).unwrap(),
    );
    let token_path = root.path().join("token");
    private_file(&token_path, b"0123456789abcdef");
    let sqlite_path = root.path().join("state.sqlite");
    let config = DaemonConfig {
        sqlite_path: sqlite_path.clone(),
        socket_path: runtime.join("victor.sock"),
        runtime_dir: runtime,
        trusted_definitions_dir: defs,
        allowed_roots: vec![root.path().to_path_buf()],
        token_file: token_path,
        scope: "local".into(),
        local_model_definition: root.path().join("defs/model.conf"),
        local_stt_definition: root.path().join("defs/local-stt.json"),
    };
    let daemon = Daemon::prepare(config).await.unwrap();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let task = tokio::spawn(daemon.run_with_shutdown(shutdown_rx));
    let socket = root.path().join("run/victor.sock");
    let mut connected = None;
    for _ in 0..400 {
        if let Ok(stream) = UnixStream::connect(&socket).await {
            connected = Some(stream);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    let mut stream = match connected {
        Some(stream) => stream,
        None => {
            let _ = shutdown_tx.send(());
            panic!("daemon nie otworzył prywatnego socketu: {:?}", task.await);
        }
    };
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
    let mut wav = vec![0_u8; 64];
    wav[0..4].copy_from_slice(b"RIFF");
    wav[8..12].copy_from_slice(b"WAVE");
    let request = serde_json::json!({"token":"0123456789abcdef","scope":"local","idempotency_key":"voice-1","operation":{"operation":"voice","conversation_id":"voice-c1","audio":wav,"at_unix_ms":3}});
    reader
        .get_mut()
        .write_all(format!("{}\n", request).as_bytes())
        .await
        .unwrap();
    line.clear();
    reader.read_line(&mut line).await.unwrap();
    let response: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(response["ok"], true, "{response}");
    assert_eq!(response["result"]["provider"], "whisper.cpp");
    assert_eq!(response["result"]["location"], "local");
    assert_eq!(response["result"]["transcript"], "pokaż status systemu");
    let _ = shutdown_tx.send(());
    assert!(task.await.unwrap().is_ok());
    let store = SqliteContextStore::open(&sqlite_path).unwrap();
    assert!(store.audit_count().unwrap() >= 3);
    let scope = ContextScope::conversation("local.voice-c1").unwrap();
    let items = store.list(&scope).unwrap();
    assert!(items.iter().all(|item| match &item.content {
        ContextContent::Inline { text } => !text.contains("RIFF"),
        ContextContent::Reference { .. } => true,
    }));
}
