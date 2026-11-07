//! Ograniczony, provider-neutral adapter lokalnego TTS.
//!
//! Proces TTS ma wyłącznie kontrakt JSON-over-stdio. Nie ma tu HTTP, wyboru
//! chmury ani prawa do wykonywania narzędzi. Wynik jest walidowany przed
//! zwróceniem jako `SpeechAudio`.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::time::timeout;
use victor_integrations::{
    speech_cache_key, AudioFormat, IntegrationError, SpeechAudio, SpeechOutput, SpeechRequest,
};

pub const PROTOCOL_VERSION: u32 = 1;
pub const LOCAL_TTS_PROVIDER: &str = "local-tts";

#[derive(Debug, thiserror::Error)]
pub enum LocalTtsError {
    #[error("nieprawidłowa konfiguracja lokalnego TTS: {0}")]
    Config(String),
    #[error("proces lokalnego TTS: {0}")]
    Process(String),
    #[error("timeout lokalnego TTS")]
    Timeout,
    #[error("nieprawidłowa odpowiedź lokalnego TTS: {0}")]
    Protocol(String),
    #[error("błąd I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("błąd JSON: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, Clone, PartialEq)]
pub struct LocalTtsConfig {
    pub command: Vec<String>,
    pub voice: String,
    pub model: PathBuf,
    pub language: String,
    pub output_format: AudioFormat,
    pub min_speed: f32,
    pub max_speed: f32,
    pub working_dir: PathBuf,
    pub environment: BTreeMap<String, String>,
    pub timeout: Duration,
    pub max_frame_bytes: usize,
    pub max_audio_bytes: usize,
    pub max_stderr_bytes: usize,
    pub max_restart_attempts: u32,
    pub sandbox_required: bool,
    pub sandbox_executable: Option<PathBuf>,
}

impl LocalTtsConfig {
    pub fn validate(&self) -> Result<(), LocalTtsError> {
        if self.command.is_empty() || self.command[0].trim().is_empty() {
            return Err(LocalTtsError::Config("brak executable".into()));
        }
        if self.voice.trim().is_empty() || self.language.trim().is_empty() {
            return Err(LocalTtsError::Config(
                "voice i language nie mogą być puste".into(),
            ));
        }
        if !self.min_speed.is_finite()
            || !self.max_speed.is_finite()
            || self.min_speed <= 0.0
            || self.min_speed > self.max_speed
        {
            return Err(LocalTtsError::Config(
                "nieprawidłowy zakres prędkości".into(),
            ));
        }
        if self.timeout.is_zero() || self.max_frame_bytes < 128 || self.max_audio_bytes == 0 {
            return Err(LocalTtsError::Config("nieprawidłowe limity TTS".into()));
        }
        if self.sandbox_required && self.sandbox_executable.is_none() {
            return Err(LocalTtsError::Config("brak executable sandboxa".into()));
        }
        Ok(())
    }
}

impl Default for LocalTtsConfig {
    fn default() -> Self {
        Self {
            command: Vec::new(),
            voice: "default".into(),
            model: PathBuf::new(),
            language: "pl".into(),
            output_format: AudioFormat::Wav,
            min_speed: 0.5,
            max_speed: 2.0,
            working_dir: PathBuf::new(),
            environment: BTreeMap::new(),
            timeout: Duration::from_secs(30),
            max_frame_bytes: 1024 * 1024,
            max_audio_bytes: 4 * 1024 * 1024,
            max_stderr_bytes: 16 * 1024,
            max_restart_attempts: 1,
            sandbox_required: false,
            sandbox_executable: None,
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(deny_unknown_fields)]
struct Request<'a> {
    kind: &'static str,
    protocol_version: u32,
    text: &'a str,
    voice: &'a str,
    language: &'a str,
    speed: f32,
    model: &'a str,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Hello {
    kind: String,
    protocol_version: u32,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Response {
    kind: String,
    format: AudioFormat,
    sample_rate_hz: u32,
    bytes: Vec<u8>,
}

pub struct LocalTts {
    config: LocalTtsConfig,
}

impl LocalTts {
    pub fn new(config: LocalTtsConfig) -> Result<Self, LocalTtsError> {
        config.validate()?;
        Ok(Self { config })
    }

    pub fn config(&self) -> &LocalTtsConfig {
        &self.config
    }

    async fn run_once(&self, request: &SpeechRequest) -> Result<SpeechAudio, LocalTtsError> {
        let mut command = if self.config.sandbox_required {
            let sandbox = self
                .config
                .sandbox_executable
                .clone()
                .ok_or_else(|| LocalTtsError::Config("brak sandboxa".into()))?;
            let mut wrapped = Command::new(sandbox);
            wrapped
                .args([
                    "--die-with-parent",
                    "--new-session",
                    "--clearenv",
                    "--unshare-net",
                    "--ro-bind",
                    "/",
                    "/",
                    "--tmpfs",
                    "/tmp",
                    "--",
                ])
                .arg(&self.config.command[0])
                .args(&self.config.command[1..]);
            wrapped
        } else {
            let mut plain = Command::new(&self.config.command[0]);
            plain.args(&self.config.command[1..]);
            plain
        };
        command
            .env_clear()
            .envs(&self.config.environment)
            .current_dir(&self.config.working_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command
            .spawn()
            .map_err(|e| LocalTtsError::Process(e.to_string()))?;
        let result = self.run_protocol(&mut child, request).await;
        if result.is_err() {
            terminate_child(&mut child).await;
        }
        result
    }

    async fn run_protocol(
        &self,
        child: &mut Child,
        request: &SpeechRequest,
    ) -> Result<SpeechAudio, LocalTtsError> {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| LocalTtsError::Process("brak stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| LocalTtsError::Process("brak stdout".into()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| LocalTtsError::Process("brak stderr".into()))?;
        let stderr_limit = self.config.max_stderr_bytes;
        let stderr_task = tokio::spawn(async move {
            let mut reader = stderr.take((stderr_limit + 1) as u64);
            let mut bytes = Vec::new();
            let _ = reader.read_to_end(&mut bytes).await;
            bytes.len() <= stderr_limit
        });
        let mut reader = BufReader::new(stdout);
        let hello_line = timeout(
            self.config.timeout,
            read_frame(&mut reader, self.config.max_frame_bytes),
        )
        .await
        .map_err(|_| LocalTtsError::Timeout)??;
        let hello: Hello = serde_json::from_slice(&hello_line)?;
        if hello.kind != "hello" || hello.protocol_version != PROTOCOL_VERSION {
            return Err(LocalTtsError::Protocol("nieprawidłowy handshake".into()));
        }
        let hello = serde_json::json!({"kind":"hello_ack","protocol_version":PROTOCOL_VERSION});
        let request_bytes = serde_json::to_vec(&Request {
            kind: "synthesize",
            protocol_version: PROTOCOL_VERSION,
            text: &request.text,
            voice: request.voice.as_deref().unwrap_or(&self.config.voice),
            language: &request.language,
            speed: request.speed,
            model: self.config.model.to_string_lossy().as_ref(),
        })?;
        let mut first = serde_json::to_vec(&hello)?;
        first.push(b'\n');
        stdin.write_all(&first).await?;
        let mut request_line = request_bytes;
        request_line.push(b'\n');
        stdin.write_all(&request_line).await?;
        stdin.flush().await?;
        let line = timeout(
            self.config.timeout,
            read_frame(&mut reader, self.config.max_frame_bytes),
        )
        .await
        .map_err(|_| LocalTtsError::Timeout)??;
        let response: Response = serde_json::from_slice(&line)?;
        let status = timeout(self.config.timeout, child.wait())
            .await
            .map_err(|_| LocalTtsError::Timeout)??;
        let stderr_ok = stderr_task.await.unwrap_or(false);
        if !status.success() {
            return Err(LocalTtsError::Process("proces zakończył się błędem".into()));
        }
        if !stderr_ok {
            return Err(LocalTtsError::Protocol("stderr przekroczył limit".into()));
        }
        if response.kind != "audio"
            || response.format != self.config.output_format
            || response.bytes.is_empty()
            || response.bytes.len() > self.config.max_audio_bytes
            || response.sample_rate_hz == 0
        {
            return Err(LocalTtsError::Protocol(
                "format lub rozmiar audio odrzucony".into(),
            ));
        }
        Ok(SpeechAudio {
            bytes: response.bytes,
            format: response.format,
            sample_rate_hz: response.sample_rate_hz,
            cache_key: speech_cache_key(request, self.provider()),
        })
    }
}

async fn terminate_child(child: &mut Child) {
    let _ = child.kill().await;
    let _ = child.wait().await;
}

async fn read_frame<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    max_bytes: usize,
) -> Result<Vec<u8>, LocalTtsError> {
    let mut frame = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Err(LocalTtsError::Protocol(
                "proces zamknął strumień przed końcem ramki".into(),
            ));
        }
        if let Some(position) = available.iter().position(|byte| *byte == b'\n') {
            if frame.len().saturating_add(position + 1) > max_bytes {
                return Err(LocalTtsError::Protocol("ramka przekracza limit".into()));
            }
            frame.extend_from_slice(&available[..=position]);
            reader.consume(position + 1);
            return Ok(frame);
        }
        if frame.len().saturating_add(available.len()) > max_bytes {
            return Err(LocalTtsError::Protocol("ramka przekracza limit".into()));
        }
        frame.extend_from_slice(available);
        let consumed = available.len();
        reader.consume(consumed);
    }
}

#[async_trait]
impl SpeechOutput for LocalTts {
    fn provider(&self) -> &'static str {
        LOCAL_TTS_PROVIDER
    }

    async fn synthesize(&self, request: SpeechRequest) -> Result<SpeechAudio, IntegrationError> {
        if request.speed < self.config.min_speed || request.speed > self.config.max_speed {
            return Err(IntegrationError::Denied(
                "prędkość TTS poza trusted limitem".into(),
            ));
        }
        let attempts = self.config.max_restart_attempts.saturating_add(1);
        let mut last = None;
        for _ in 0..attempts {
            match self.run_once(&request).await {
                Ok(audio) => return Ok(audio),
                Err(error) => last = Some(error),
            }
        }
        Err(IntegrationError::provider(
            self.provider(),
            last.map_or_else(|| "brak wyniku".into(), |e| e.to_string()),
        ))
    }
}
