//! Bezpieczny adapter lokalnego modelu przez ograniczony JSON-over-stdio.
//! Model zwraca wyłącznie `Reply` albo `PlanRequest`; wykonanie planu pozostaje
//! własnością `victor-conversation` i `victor-runtime`.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;
use tokio::time::timeout;
use victor_context::ContextView;
use victor_conversation::{ConversationModel, ConversationTurn, ModelResult};
use victor_domain::ToolId;

pub const PROTOCOL_VERSION: u32 = 1;
pub const MAX_FRAME_BYTES: usize = 128 * 1024;
pub const DEFAULT_TIMEOUT_MS: u64 = 15_000;
pub const DEFAULT_RESTART_ATTEMPTS: u32 = 1;
pub const DEFAULT_MAX_STDERR_BYTES: usize = 32 * 1024;
pub const MAX_TEXT_BYTES: usize = 32 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum LocalModelError {
    #[error("nie można uruchomić lokalnego modelu: {0}")]
    Spawn(String),
    #[error("lokalny model zakończył proces")]
    Disconnected,
    #[error("lokalny model przekroczył limit ramki")]
    FrameTooLarge,
    #[error("lokalny model zwrócił uszkodzony JSON: {0}")]
    InvalidJson(String),
    #[error("nieprawidłowy handshake lokalnego modelu: {0}")]
    Handshake(String),
    #[error("timeout lokalnego modelu po {0} ms")]
    Timeout(u64),
    #[error("błąd I/O lokalnego modelu: {0}")]
    Io(String),
    #[error("lokalny model zwrócił nieprawidłowy tool id: {0}")]
    InvalidTool(String),
    #[error("lokalny model wypisał zbyt dużo na stderr")]
    StderrTooLarge,
    #[error("lokalny model wymaga sandboxa, ale bubblewrap nie jest dostępny")]
    SandboxUnavailable,
}

#[derive(Debug, Clone)]
pub struct LocalModelConfig {
    pub command: Vec<String>,
    pub working_dir: PathBuf,
    pub environment: BTreeMap<String, String>,
    pub timeout: Duration,
    pub max_frame_bytes: usize,
    pub max_stderr_bytes: usize,
    pub max_restart_attempts: u32,
    pub sandbox_required: bool,
    pub sandbox_executable: Option<PathBuf>,
}

impl LocalModelConfig {
    pub fn new(command: Vec<String>) -> Result<Self, LocalModelError> {
        if command.is_empty() || command[0].trim().is_empty() {
            return Err(LocalModelError::Spawn("pusta komenda".into()));
        }
        Ok(Self {
            command,
            working_dir: std::env::current_dir().map_err(|e| LocalModelError::Io(e.to_string()))?,
            environment: BTreeMap::new(),
            timeout: Duration::from_millis(DEFAULT_TIMEOUT_MS),
            max_frame_bytes: MAX_FRAME_BYTES,
            max_stderr_bytes: DEFAULT_MAX_STDERR_BYTES,
            max_restart_attempts: DEFAULT_RESTART_ATTEMPTS,
            sandbox_required: false,
            sandbox_executable: None,
        })
    }

    fn validate(&self) -> Result<(), LocalModelError> {
        if self.command.is_empty()
            || self.timeout.is_zero()
            || self.max_frame_bytes == 0
            || self.max_frame_bytes > MAX_FRAME_BYTES
            || self.max_stderr_bytes == 0
            || !self.working_dir.is_absolute()
            || !self.working_dir.is_dir()
        {
            return Err(LocalModelError::Spawn(
                "nieprawidłowa konfiguracja modelu".into(),
            ));
        }
        if self.sandbox_required {
            let sandbox = self.sandbox_executable.clone().or_else(|| find_in_path("bwrap"));
            if sandbox.as_ref().is_none_or(|path| !path.is_file()) {
                return Err(LocalModelError::SandboxUnavailable);
            }
        }
        Ok(())
    }
}

#[derive(Debug, Serialize)]
#[serde(deny_unknown_fields)]
struct ModelRequest<'a> {
    protocol_version: u32,
    input: &'a str,
    history: &'a [ConversationTurn],
    context: ContextPayload,
}

#[derive(Debug, Serialize)]
struct ContextPayload { rendered: String, total_bytes: u64, dropped_items: usize }

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Hello {
    kind: String,
    protocol_version: u32,
    model_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum ModelResponse {
    Reply {
        text: String,
    },
    PlanRequest {
        tool: String,
        description: String,
        args: Value,
    },
}

struct ModelProcess {
    child: Child,
    stdin: BufWriter<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    max_frame_bytes: usize,
    stderr_exceeded: Arc<AtomicBool>,
}

impl ModelProcess {
    async fn spawn(config: &LocalModelConfig) -> Result<Self, LocalModelError> {
        config.validate()?;
        let mut command = Command::new(&config.command[0]);
        command.args(&config.command[1..]).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
        command.env_clear().envs(&config.environment).current_dir(&config.working_dir);
        let mut child = command
            .spawn()
            .map_err(|e| LocalModelError::Spawn(e.to_string()))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| LocalModelError::Spawn("brak stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| LocalModelError::Spawn("brak stdout".into()))?;
        let stderr = child.stderr.take().ok_or_else(|| LocalModelError::Spawn("brak stderr".into()))?;
        let stderr_exceeded = Arc::new(AtomicBool::new(false));
        let exceeded = stderr_exceeded.clone();
        let limit = config.max_stderr_bytes;
        tokio::spawn(async move {
            let mut reader = stderr.take((limit + 1) as u64);
            let mut bytes = Vec::new();
            if reader.read_to_end(&mut bytes).await.is_ok() && bytes.len() > limit {
                exceeded.store(true, Ordering::Release);
            }
        });
        let mut process = Self {
            child,
            stdin: BufWriter::new(stdin),
            stdout: BufReader::new(stdout),
            max_frame_bytes: config.max_frame_bytes,
            stderr_exceeded,
        };
        let hello: Hello = serde_json::from_value(process.receive(config.timeout).await?)
            .map_err(|e| LocalModelError::Handshake(e.to_string()))?;
        if hello.kind != "hello"
            || hello.protocol_version != PROTOCOL_VERSION
            || hello.model_id.is_empty()
        {
            return Err(LocalModelError::Handshake(
                "niezgodna wersja lub tożsamość modelu".into(),
            ));
        }
        process
            .send(
                r#"{"kind":"hello_ack","protocol_version":1}"#,
                config.timeout,
            )
            .await?;
        Ok(process)
    }

    async fn send(&mut self, body: &str, wait: Duration) -> Result<(), LocalModelError> {
        if body.len() + 1 > self.max_frame_bytes {
            return Err(LocalModelError::FrameTooLarge);
        }
        self.stdin
            .write_all(body.as_bytes())
            .await
            .map_err(|e| LocalModelError::Io(e.to_string()))?;
        self.stdin
            .write_all(b"\n")
            .await
            .map_err(|e| LocalModelError::Io(e.to_string()))?;
        timeout(wait, self.stdin.flush())
            .await
            .map_err(|_| LocalModelError::Timeout(wait.as_millis() as u64))?
            .map_err(|e| LocalModelError::Io(e.to_string()))
    }

    async fn receive(&mut self, wait: Duration) -> Result<Value, LocalModelError> {
        let frame = timeout(
            wait,
            read_limited_frame(&mut self.stdout, self.max_frame_bytes),
        )
        .await
        .map_err(|_| LocalModelError::Timeout(wait.as_millis() as u64))??;
        let Some(frame) = frame else {
            return Err(LocalModelError::Disconnected);
        };
        serde_json::from_slice(&frame).map_err(|e| LocalModelError::InvalidJson(e.to_string()))
    }

    async fn terminate(&mut self) {
        let _ = self.stdin.shutdown().await;
        let _ = self.child.start_kill();
        let _ = timeout(Duration::from_secs(2), self.child.wait()).await;
    }
}

enum FrameError {
    TooLarge,
    Unterminated,
    Io(std::io::Error),
}

async fn read_limited_frame<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    max: usize,
) -> Result<Option<Vec<u8>>, FrameError> {
    let mut frame = Vec::new();
    loop {
        let available = reader.fill_buf().await.map_err(FrameError::Io)?;
        if available.is_empty() {
            return if frame.is_empty() {
                Ok(None)
            } else {
                Err(FrameError::Unterminated)
            };
        }
        if let Some(pos) = available.iter().position(|byte| *byte == b'\n') {
            if frame.len().saturating_add(pos + 1) > max {
                return Err(FrameError::TooLarge);
            }
            frame.extend_from_slice(&available[..=pos]);
            reader.consume(pos + 1);
            return Ok(Some(frame));
        }
        if frame.len().saturating_add(available.len()) > max {
            return Err(FrameError::TooLarge);
        }
        frame.extend_from_slice(available);
        let consumed = available.len();
        reader.consume(consumed);
    }
}

impl From<FrameError> for LocalModelError {
    fn from(error: FrameError) -> Self {
        match error {
            FrameError::TooLarge => Self::FrameTooLarge,
            FrameError::Unterminated => Self::InvalidJson("EOF przed końcem ramki".into()),
            FrameError::Io(e) => Self::Io(e.to_string()),
        }
    }
}

pub struct LocalModel {
    config: LocalModelConfig,
    process: Arc<Mutex<Option<ModelProcess>>>,
}

impl LocalModel {
    pub fn new(config: LocalModelConfig) -> Result<Self, LocalModelError> {
        config.validate()?;
        Ok(Self {
            config,
            process: Arc::new(Mutex::new(None)),
        })
    }

    async fn respond_once(
        &self,
        history: &[ConversationTurn],
        input: &str,
    ) -> Result<ModelResult, LocalModelError> {
        let mut guard = self.process.lock().await;
        if guard.is_none() {
            *guard = Some(ModelProcess::spawn(&self.config).await?);
        }
        let process = guard.as_mut().expect("proces właśnie utworzony");
        let request = serde_json::to_string(&ModelRequest {
            protocol_version: PROTOCOL_VERSION,
            input,
            history,
        })
        .map_err(|e| LocalModelError::InvalidJson(e.to_string()))?;
        process.send(&request, self.config.timeout).await?;
        match serde_json::from_value(process.receive(self.config.timeout).await?)
            .map_err(|e| LocalModelError::InvalidJson(e.to_string()))?
        {
            ModelResponse::Reply { text } => Ok(ModelResult::Reply(text)),
            ModelResponse::PlanRequest {
                tool,
                description,
                args,
            } => Ok(ModelResult::PlanRequest {
                tool: ToolId::parse(&tool)
                    .map_err(|e| LocalModelError::InvalidTool(e.to_string()))?,
                description,
                args,
            }),
        }
    }
}

#[async_trait]
impl ConversationModel for LocalModel {
    async fn respond(
        &self,
        history: &[ConversationTurn],
        input: &str,
    ) -> Result<ModelResult, String> {
        let mut attempts = 0;
        loop {
            match self.respond_once(history, input).await {
                Ok(result) => return Ok(result),
                Err(_error) if attempts < self.config.max_restart_attempts => {
                    attempts += 1;
                    let mut guard = self.process.lock().await;
                    if let Some(process) = guard.as_mut() {
                        process.terminate().await;
                    }
                    *guard = None;
                }
                Err(error) => {
                    let mut guard = self.process.lock().await;
                    if let Some(process) = guard.as_mut() {
                        process.terminate().await;
                    }
                    *guard = None;
                    return Err(error.to_string());
                }
            }
        }
    }
}
