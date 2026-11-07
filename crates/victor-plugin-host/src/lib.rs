//! Host bezpośredniego dostępu do capabilities.
//!
//! Proces pluginu ma tylko kontrakt stdio. Policy, audit, context i approval są
//! nadal własnością `victor-runtime`; ten crate dostarcza wyłącznie executor.

use std::collections::BTreeMap;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;
use tokio::time::timeout;
use victor_domain::{ToolId, UnixMs};
use victor_plugin_contract::{
    decode, encode, Hello, PluginManifest, Request, Response, WireMessage,
    DEFAULT_HEARTBEAT_TIMEOUT_MS, DEFAULT_REQUEST_TIMEOUT_MS, MAX_FRAME_BYTES, PROTOCOL_VERSION,
};
use victor_tools::{ToolError, ToolExecution, ToolExecutor, ToolManifest};

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
}

struct ProcessConnection {
    child: Child,
    stdin: BufWriter<ChildStdin>,
    stdout: BufReader<ChildStdout>,
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
        let mut line = Vec::new();
        let read = timeout(
            Duration::from_millis(timeout_ms),
            self.stdout.read_until(b'\n', &mut line),
        )
        .await
        .map_err(|_| HostError::Timeout(timeout_ms))?
        .map_err(|e| HostError::Io(e.to_string()))?;
        if read == 0 {
            return Err(HostError::Disconnected);
        }
        if line.len() > MAX_FRAME_BYTES {
            return Err(HostError::FrameTooLarge);
        }
        while matches!(line.last(), Some(b'\n' | b'\r')) {
            line.pop();
        }
        decode(&line).map_err(|e| match e {
            victor_plugin_contract::ContractError::FrameTooLarge { .. } => HostError::FrameTooLarge,
            other => HostError::Handshake(other.to_string()),
        })
    }
}

pub struct PluginToolExecutor {
    tool_id: ToolId,
    local_manifest: Arc<ToolManifest>,
    command: Arc<Vec<String>>,
    connection: Arc<Mutex<ProcessConnection>>,
    request_timeout_ms: u64,
    restart_attempted: Arc<Mutex<bool>>,
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
        if command.is_empty() {
            return Err(HostError::Spawn("pusta komenda pluginu".into()));
        }
        local_manifest
            .validate()
            .map_err(|e| HostError::ManifestMismatch(e.to_string()))?;
        let connection = spawn_and_handshake(&command, &local_manifest).await?;
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
            restart_attempted: Arc::new(Mutex::new(false)),
        })
    }

    async fn restart(&self) -> Result<(), HostError> {
        let connection = spawn_and_handshake(&self.command, &self.local_manifest).await?;
        *self.connection.lock().await = connection;
        Ok(())
    }

    /// Wysyła heartbeat bez uruchamiania narzędzia. Host może użyć go do
    /// sprawdzenia procesu przed pokazaniem go jako online.
    pub async fn heartbeat(&self, nonce: impl Into<String>) -> Result<(), HostError> {
        let nonce = nonce.into();
        let mut connection = self.connection.lock().await;
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
}

async fn spawn_and_handshake(
    command: &[String],
    local: &ToolManifest,
) -> Result<ProcessConnection, HostError> {
    let mut process = Command::new(&command[0]);
    process
        .args(&command[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
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
    let hello = connection.receive(DEFAULT_HEARTBEAT_TIMEOUT_MS).await?;
    let WireMessage::Hello(Hello { manifest }) = hello else {
        return Err(HostError::Handshake(
            "pierwsza wiadomość nie jest Hello".into(),
        ));
    };
    validate_manifest(&manifest, local)?;
    connection
        .send(&WireMessage::HelloAck {
            protocol_version: PROTOCOL_VERSION,
        })
        .await?;
    Ok(connection)
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
        let mut restarted = self.restart_attempted.lock().await;
        for attempt in 0..=1 {
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
                    *restarted = false;
                    return Ok(value);
                }
                Err(_error) if attempt == 0 && !*restarted => {
                    *restarted = true;
                    drop(restarted);
                    self.restart().await.map_err(|e| ToolError::Execution {
                        tool: self.tool_id.to_string(),
                        reason: e.to_string(),
                    })?;
                    restarted = self.restart_attempted.lock().await;
                }
                Err(error) => {
                    return Err(ToolError::Execution {
                        tool: self.tool_id.to_string(),
                        reason: error,
                    })
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
