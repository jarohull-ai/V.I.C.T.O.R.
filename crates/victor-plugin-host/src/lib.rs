//! Host bezpośredniego dostępu do capabilities.
//!
//! Proces pluginu ma tylko kontrakt stdio. Policy, audit, context i approval są
//! nadal własnością `victor-runtime`; ten crate dostarcza wyłącznie executor.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;
use tokio::time::timeout;
use victor_domain::{ToolId, UnixMs};
use victor_plugin_contract::{
    decode_frame, encode, Hello, PluginManifest, Request, Response, WireMessage,
    DEFAULT_HEARTBEAT_TIMEOUT_MS, DEFAULT_REQUEST_TIMEOUT_MS, MAX_FRAME_BYTES, PROTOCOL_VERSION,
};
use victor_tools::{
    AuditOutcome, AuditSink, ToolAuditEvent, ToolError, ToolExecution, ToolExecutor, ToolManifest,
};

pub const DEFAULT_MAX_RESTART_ATTEMPTS: u32 = 1;

#[derive(Debug, thiserror::Error)]
pub enum HostError {
    #[error("nie można uruchomić pluginu: {0}")]
    Spawn(String),
    #[error("plugin nie przeszedł handshake: {0}")]
    Handshake(String),
    #[error("plugin rozłączył się")]
    Disconnected,
    #[error("plugin przekroczył limit ramki")]
    FrameTooLarge,
    #[error("timeout pluginu po {0} ms")]
    Timeout(u64),
    #[error("błąd I/O pluginu: {0}")]
    Io(String),
    #[error("plugin zwrócił błąd {code}: {message}")]
    Plugin { code: String, message: String },
    #[error("manifest pluginu nie pasuje do lokalnego ToolManifest: {0}")]
    ManifestMismatch(String),
    #[error("nieprawidłowe framing pluginu: {0}")]
    InvalidFraming(String),
    #[error("plugin wymaga sandboxa, ale bubblewrap nie jest dostępny")]
    SandboxUnavailable,
    #[error("nieprawidłowa polityka uruchomienia pluginu: {0}")]
    InvalidLaunchPolicy(String),
}

struct ProcessConnection {
    child: Child,
    stdin: BufWriter<ChildStdin>,
    stdout: BufReader<ChildStdout>,
}

impl ProcessConnection {
    async fn terminate(&mut self) {
        let _ = self.stdin.shutdown().await;
        let _ = self.child.start_kill();
        let _ = timeout(Duration::from_secs(2), self.child.wait()).await;
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxRequirement {
    NotRequired,
    Required,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginLaunchPolicy {
    pub sandbox: SandboxRequirement,
    pub sandbox_executable: Option<PathBuf>,
    pub working_dir: Option<PathBuf>,
    pub environment: BTreeMap<String, String>,
}

impl Default for PluginLaunchPolicy {
    fn default() -> Self {
        Self {
            sandbox: SandboxRequirement::NotRequired,
            sandbox_executable: None,
            working_dir: None,
            environment: BTreeMap::new(),
        }
    }
}

pub trait SandboxRunner: Send + Sync {
    fn wrap_command(
        &self,
        command: &mut Command,
        executable: &str,
        args: &[String],
        policy: &PluginLaunchPolicy,
    ) -> Result<(), HostError>;
}

#[derive(Debug, Clone)]
pub struct BubblewrapSandboxRunner {
    executable: PathBuf,
}

impl BubblewrapSandboxRunner {
    pub fn detect() -> Option<Self> {
        std::env::var_os("PATH").and_then(|path| {
            std::env::split_paths(&path)
                .map(|dir| dir.join("bwrap"))
                .find(|candidate| candidate.is_file())
                .map(|executable| Self { executable })
        })
    }

    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self {
            executable: path.into(),
        }
    }
}

impl SandboxRunner for BubblewrapSandboxRunner {
    fn wrap_command(
        &self,
        command: &mut Command,
        executable: &str,
        args: &[String],
        policy: &PluginLaunchPolicy,
    ) -> Result<(), HostError> {
        if !self.executable.is_file() {
            return Err(HostError::SandboxUnavailable);
        }
        let mut wrapped = vec![
            "--die-with-parent".to_string(),
            "--new-session".to_string(),
            "--clearenv".to_string(),
            "--unshare-net".to_string(),
            "--ro-bind".to_string(),
            "/".to_string(),
            "/".to_string(),
            "--tmpfs".to_string(),
            "/tmp".to_string(),
        ];
        for (key, value) in &policy.environment {
            wrapped.extend(["--setenv".to_string(), key.clone(), value.clone()]);
        }
        wrapped.extend(["--".to_string(), executable.to_string()]);
        wrapped.extend(args.iter().cloned());
        *command = Command::new(&self.executable);
        command.args(wrapped);
        if let Some(dir) = &policy.working_dir {
            command.current_dir(dir);
        }
        Ok(())
    }
}

impl ProcessConnection {
    async fn send(&mut self, message: &WireMessage) -> Result<(), HostError> {
        let frame = encode(message).map_err(|_| HostError::FrameTooLarge)?;
        self.stdin
            .write_all(&frame)
            .await
            .map_err(|e| HostError::Io(e.to_string()))?;
        self.stdin
            .flush()
            .await
            .map_err(|e| HostError::Io(e.to_string()))?;
        Ok(())
    }

    async fn receive(&mut self, timeout_ms: u64) -> Result<WireMessage, HostError> {
        let line = timeout(
            Duration::from_millis(timeout_ms),
            read_limited_frame(&mut self.stdout, MAX_FRAME_BYTES),
        )
        .await
        .map_err(|_| HostError::Timeout(timeout_ms))?
        .map_err(|error| match error {
            FrameReadError::TooLarge => HostError::FrameTooLarge,
            FrameReadError::Unterminated => {
                HostError::InvalidFraming("EOF przed końcem ramki".into())
            }
            FrameReadError::Io(error) => HostError::Io(error.to_string()),
        })?;
        let Some(line) = line else {
            return Err(HostError::Disconnected);
        };
        if line.len() > MAX_FRAME_BYTES {
            return Err(HostError::FrameTooLarge);
        }
        decode_frame(&line).map_err(|e| match e {
            victor_plugin_contract::ContractError::FrameTooLarge { .. } => HostError::FrameTooLarge,
            victor_plugin_contract::ContractError::MissingFrameDelimiter => {
                HostError::InvalidFraming("brak delimitera ramki".into())
            }
            other => HostError::Handshake(other.to_string()),
        })
    }
}

enum FrameReadError {
    TooLarge,
    Unterminated,
    Io(std::io::Error),
}

async fn read_limited_frame<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    max_bytes: usize,
) -> Result<Option<Vec<u8>>, FrameReadError> {
    let mut frame = Vec::new();
    loop {
        let available = reader.fill_buf().await.map_err(FrameReadError::Io)?;
        if available.is_empty() {
            return if frame.is_empty() {
                Ok(None)
            } else {
                Err(FrameReadError::Unterminated)
            };
        }
        if let Some(position) = available.iter().position(|byte| *byte == b'\n') {
            if frame.len().saturating_add(position + 1) > max_bytes {
                return Err(FrameReadError::TooLarge);
            }
            frame.extend_from_slice(&available[..=position]);
            reader.consume(position + 1);
            return Ok(Some(frame));
        }
        if frame.len().saturating_add(available.len()) > max_bytes {
            return Err(FrameReadError::TooLarge);
        }
        frame.extend_from_slice(available);
        let consumed = available.len();
        reader.consume(consumed);
    }
}

pub struct PluginToolExecutor {
    tool_id: ToolId,
    local_manifest: Arc<ToolManifest>,
    command: Arc<Vec<String>>,
    connection: Arc<Mutex<ProcessConnection>>,
    request_timeout_ms: u64,
    restart_attempts: Arc<Mutex<u32>>,
    max_restart_attempts: u32,
    audit: Option<Arc<dyn AuditSink>>,
    launch_policy: PluginLaunchPolicy,
}

/// Rejestr hosta. Rejestruje wyłącznie lokalnie zaufane manifesty narzędzi;
/// manifest otrzymany z procesu służy tylko do sprawdzenia zgodności schematu.
#[derive(Default)]
pub struct PluginHost {
    tools: BTreeMap<String, Arc<PluginToolExecutor>>,
}

impl PluginHost {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn register(
        &mut self,
        command: Vec<String>,
        manifest: ToolManifest,
    ) -> Result<Arc<PluginToolExecutor>, HostError> {
        let id = manifest.descriptor.id.as_str().to_string();
        if self.tools.contains_key(&id) {
            return Err(HostError::ManifestMismatch(format!(
                "narzędzie {id} jest już zarejestrowane"
            )));
        }
        let executor = Arc::new(PluginToolExecutor::spawn(command, manifest).await?);
        self.tools.insert(id, executor.clone());
        Ok(executor)
    }

    pub fn get(&self, id: &ToolId) -> Option<Arc<PluginToolExecutor>> {
        self.tools.get(id.as_str()).cloned()
    }

    pub fn tool_ids(&self) -> Vec<ToolId> {
        self.tools
            .keys()
            .filter_map(|id| ToolId::parse(id).ok())
            .collect()
    }
}

impl PluginToolExecutor {
    pub async fn spawn(
        command: Vec<String>,
        local_manifest: ToolManifest,
    ) -> Result<Self, HostError> {
        Self::spawn_with_policy(command, local_manifest, PluginLaunchPolicy::default(), None).await
    }

    pub async fn spawn_with_policy(
        command: Vec<String>,
        local_manifest: ToolManifest,
        launch_policy: PluginLaunchPolicy,
        audit: Option<Arc<dyn AuditSink>>,
    ) -> Result<Self, HostError> {
        if command.is_empty() {
            return Err(HostError::Spawn("pusta komenda pluginu".into()));
        }
        local_manifest
            .validate()
            .map_err(|e| HostError::ManifestMismatch(e.to_string()))?;
        validate_launch_policy(&launch_policy)?;
        let connection = spawn_and_handshake(&command, &local_manifest, &launch_policy).await?;
        let tool_id = local_manifest.descriptor.id.clone();
        let request_timeout_ms = local_manifest
            .descriptor
            .default_timeout_ms
            .min(DEFAULT_REQUEST_TIMEOUT_MS);
        Ok(Self {
            tool_id,
            local_manifest: Arc::new(local_manifest),
            command: Arc::new(command),
            connection: Arc::new(Mutex::new(connection)),
            request_timeout_ms,
            restart_attempts: Arc::new(Mutex::new(0)),
            max_restart_attempts: DEFAULT_MAX_RESTART_ATTEMPTS,
            audit,
            launch_policy,
        })
    }

    async fn restart(&self) -> Result<(), HostError> {
        let mut old = self.connection.lock().await;
        old.terminate().await;
        let connection =
            spawn_and_handshake(&self.command, &self.local_manifest, &self.launch_policy).await?;
        *old = connection;
        Ok(())
    }

    /// Wysyła heartbeat bez uruchamiania narzędzia. Host może użyć go do
    /// sprawdzenia procesu przed pokazaniem go jako online.
    pub async fn heartbeat(&self, nonce: impl Into<String>) -> Result<(), HostError> {
        let nonce = nonce.into();
        let mut connection = self.connection.lock().await;
        let result = async {
            connection
                .send(&WireMessage::Heartbeat {
                    nonce: nonce.clone(),
                })
                .await?;
            match connection.receive(DEFAULT_HEARTBEAT_TIMEOUT_MS).await? {
                WireMessage::HeartbeatAck { nonce: received } if received == nonce => Ok(()),
                other => Err(HostError::Handshake(format!(
                    "heartbeat zwrócił {:?}",
                    other.kind()
                ))),
            }
        }
        .await;
        if result.is_err() {
            connection.terminate().await;
        }
        result
    }

    fn record_restart(&self, attempt: u32, reason: &str) -> Result<(), ToolError> {
        let Some(audit) = &self.audit else {
            return Ok(());
        };
        audit.record(ToolAuditEvent {
            task_id: format!("plugin-host:{}", self.tool_id),
            tool: self.tool_id.clone(),
            tool_version: self.local_manifest.tool_version.clone(),
            plan_hash: "plugin-host-restart".into(),
            started_at_unix_ms: 0,
            finished_at_unix_ms: 0,
            outcome: AuditOutcome::Failed,
            summary: format!("restart attempt {attempt}: {reason}"),
        })
    }
}

async fn spawn_and_handshake(
    command: &[String],
    local: &ToolManifest,
    launch_policy: &PluginLaunchPolicy,
) -> Result<ProcessConnection, HostError> {
    let mut process = Command::new(&command[0]);
    process.args(&command[1..]);
    process
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    process.env_clear();
    process.envs(&launch_policy.environment);
    if let Some(dir) = &launch_policy.working_dir {
        process.current_dir(dir);
    }
    if matches!(launch_policy.sandbox, SandboxRequirement::Required) {
        let runner = launch_policy
            .sandbox_executable
            .as_ref()
            .map(|path| BubblewrapSandboxRunner::at(path.clone()))
            .or_else(BubblewrapSandboxRunner::detect)
            .ok_or(HostError::SandboxUnavailable)?;
        runner.wrap_command(&mut process, &command[0], &command[1..], launch_policy)?;
        process
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        process.env_clear();
        process.envs(&launch_policy.environment);
    }
    let mut child = process
        .spawn()
        .map_err(|e| HostError::Spawn(e.to_string()))?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| HostError::Spawn("brak stdin pluginu".into()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| HostError::Spawn("brak stdout pluginu".into()))?;
    let mut connection = ProcessConnection {
        child,
        stdin: BufWriter::new(stdin),
        stdout: BufReader::new(stdout),
    };
    let hello = match connection.receive(DEFAULT_HEARTBEAT_TIMEOUT_MS).await {
        Ok(message) => message,
        Err(error) => {
            connection.terminate().await;
            return Err(error);
        }
    };
    let WireMessage::Hello(Hello { manifest }) = hello else {
        connection.terminate().await;
        return Err(HostError::Handshake(
            "pierwsza wiadomość nie jest Hello".into(),
        ));
    };
    if let Err(error) = validate_manifest(&manifest, local) {
        connection.terminate().await;
        return Err(error);
    }
    if let Err(error) = connection
        .send(&WireMessage::HelloAck {
            protocol_version: PROTOCOL_VERSION,
        })
        .await
    {
        connection.terminate().await;
        return Err(error);
    }
    Ok(connection)
}

fn validate_launch_policy(policy: &PluginLaunchPolicy) -> Result<(), HostError> {
    if policy.environment.keys().any(|key| {
        key.is_empty() || key.contains('=') || key.starts_with("LD_") || key.contains("PROXY")
    }) {
        return Err(HostError::InvalidLaunchPolicy(
            "środowisko pluginu zawiera niedozwoloną nazwę".into(),
        ));
    }
    if let Some(dir) = &policy.working_dir {
        if !dir.is_dir() {
            return Err(HostError::InvalidLaunchPolicy(
                "katalog roboczy pluginu nie istnieje".into(),
            ));
        }
    }
    Ok(())
}

fn validate_manifest(remote: &PluginManifest, local: &ToolManifest) -> Result<(), HostError> {
    if remote.protocol_version != PROTOCOL_VERSION {
        return Err(HostError::Handshake(format!(
            "wersja {} zamiast {}",
            remote.protocol_version, PROTOCOL_VERSION
        )));
    }
    let id = local.descriptor.id.as_str();
    let Some(binding) = remote.tools.iter().find(|tool| tool.id == id) else {
        return Err(HostError::ManifestMismatch(format!(
            "plugin nie deklaruje narzędzia {id}"
        )));
    };
    if binding.schema_version != local.descriptor.schema_version
        || binding.input_schema != local.descriptor.input_schema
        || binding.output_schema != local.descriptor.output_schema
    {
        return Err(HostError::ManifestMismatch(
            "schemat pluginu różni się od lokalnego".into(),
        ));
    }
    Ok(())
}

#[async_trait]
impl ToolExecutor for PluginToolExecutor {
    fn tool_id(&self) -> &ToolId {
        &self.tool_id
    }

    async fn execute(
        &self,
        args: Value,
        started_at_unix_ms: UnixMs,
    ) -> Result<ToolExecution, ToolError> {
        let mut restart_attempts = self.restart_attempts.lock().await;
        for attempt in 0..=self.max_restart_attempts {
            let request_id = format!("plugin-{started_at_unix_ms}-{attempt}");
            let result = async {
                let mut connection = self.connection.lock().await;
                connection
                    .send(&WireMessage::Request(Request {
                        request_id: request_id.clone(),
                        tool_id: self.tool_id.to_string(),
                        args: args.clone(),
                    }))
                    .await
                    .map_err(|e| e.to_string())?;
                match connection
                    .receive(self.request_timeout_ms)
                    .await
                    .map_err(|e| e.to_string())?
                {
                    WireMessage::Response(Response {
                        request_id: response_id,
                        output,
                        summary,
                        finished_at_unix_ms,
                    }) if response_id == request_id => Ok(ToolExecution {
                        output,
                        summary,
                        finished_at_unix_ms,
                    }),
                    WireMessage::Error(error) => Err(format!("{}: {}", error.code, error.message)),
                    other => Err(format!("nieoczekiwana odpowiedź {:?}", other.kind())),
                }
            }
            .await;
            match result {
                Ok(value) => {
                    *restart_attempts = 0;
                    return Ok(value);
                }
                Err(error) if attempt < self.max_restart_attempts => {
                    *restart_attempts = restart_attempts.saturating_add(1);
                    let restart_number = *restart_attempts;
                    self.record_restart(restart_number, &error)?;
                    drop(restart_attempts);
                    self.restart().await.map_err(|e| ToolError::Execution {
                        tool: self.tool_id.to_string(),
                        reason: e.to_string(),
                    })?;
                    restart_attempts = self.restart_attempts.lock().await;
                }
                Err(error) => {
                    self.connection.lock().await.terminate().await;
                    return Err(ToolError::Execution {
                        tool: self.tool_id.to_string(),
                        reason: error,
                    });
                }
            }
        }
        unreachable!()
    }
}

impl Drop for PluginToolExecutor {
    fn drop(&mut self) {
        if let Ok(mut connection) = self.connection.try_lock() {
            let _ = connection.child.start_kill();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use victor_domain::{Capability, PathScope, RiskLevel, ToolDescriptor, ToolId};

    fn manifest() -> ToolManifest {
        ToolManifest::new(
            "1.0.0",
            ToolDescriptor {
                id: ToolId::parse("plugin_sysinfo").unwrap(),
                schema_version: 1,
                description: "CPU RAM".into(),
                input_schema: json!({"type":"object"}),
                output_schema: json!({"type":"object"}),
                capabilities: vec![],
                risk: RiskLevel::R0,
                default_timeout_ms: 2_000,
                undo_support: false,
                path_arguments: vec![],
            },
        )
    }

    #[tokio::test]
    async fn rejects_remote_capability_claim_by_schema_mismatch() {
        let mut local = manifest();
        local.descriptor.capabilities = vec![Capability::FilesystemRead {
            scope: PathScope::new("/tmp"),
        }];
        let remote = PluginManifest {
            protocol_version: PROTOCOL_VERSION,
            plugin_id: "plugin_sysinfo".into(),
            plugin_version: "1".into(),
            tools: vec![victor_plugin_contract::PluginToolBinding {
                id: "plugin_sysinfo".into(),
                schema_version: 1,
                input_schema: json!({"type":"object"}),
                output_schema: json!({"type":"object"}),
            }],
        };
        assert!(validate_manifest(&remote, &local).is_ok());
        assert_eq!(local.descriptor.risk, RiskLevel::R0);
    }
}
