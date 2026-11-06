//! Lokalny adapter whisper.cpp.
//!
//! Ten moduł **nie zawiera żadnego klienta HTTP**. Nie ma tu `reqwest`, `hyper`
//! ani `ureq`; cała komunikacja ze światem zewnętrznym to uruchomienie lokalnej
//! binarki whisper.cpp na lokalnym pliku WAV. Test
//! `integrations_crate_links_no_http_client` pilnuje, że crate pozostaje
//! wolny od klienta sieciowego — adapter chmurowy musi powstać osobno.
//!
//! Zasady:
//! - tylko WAV 16 kHz mono; brak cichej konwersji przez ffmpeg;
//! - brak modelu albo błędu binarki = błąd, nigdy fallback do chmury;
//! - wynik jest oznaczony jako `TranscriptionLocation::Local`.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use tokio::io::AsyncReadExt;

use crate::error::IntegrationError;
use crate::voice::{
    AudioChunk, AudioFormat, SpeechToText, Transcript, TranscriptionLocation, TranscriptionRequest,
};

pub const WHISPER_PROVIDER: &str = "whisper.cpp";
pub const WHISPER_SAMPLE_RATE_HZ: u32 = 16_000;
pub const WHISPER_CHANNELS: u8 = 1;
pub const DEFAULT_WHISPER_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WhisperCppConfig {
    pub binary: PathBuf,
    pub model: PathBuf,
    /// Kod języka, np. `pl`; `None` = `auto`.
    pub language: Option<String>,
    pub threads: Option<u16>,
    pub timeout: Duration,
}

impl WhisperCppConfig {
    pub fn new(binary: impl Into<PathBuf>, model: impl Into<PathBuf>) -> Self {
        Self {
            binary: binary.into(),
            model: model.into(),
            language: None,
            threads: None,
            timeout: DEFAULT_WHISPER_TIMEOUT,
        }
    }

    pub fn with_language(mut self, language: impl Into<String>) -> Self {
        self.language = Some(language.into());
        self
    }

    pub fn with_threads(mut self, threads: u16) -> Self {
        self.threads = Some(threads);
        self
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Fail-closed: brak binarki albo modelu oznacza, że nie ma czym
    /// transkrybować. Nie próbujemy wtedy żadnego innego dostawcy.
    pub fn validate(&self) -> Result<(), IntegrationError> {
        validate_file("binarka whisper.cpp", &self.binary)?;
        validate_file("model whisper.cpp", &self.model)?;
        if self.timeout.is_zero() {
            return Err(IntegrationError::NotConfigured(
                "timeout whisper.cpp nie może być zerowy".into(),
            ));
        }
        if matches!(self.threads, Some(0)) {
            return Err(IntegrationError::NotConfigured(
                "liczba wątków whisper.cpp nie może być zerowa".into(),
            ));
        }
        if let Some(language) = &self.language {
            let valid = language.len() <= 16
                && language
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_alphabetic())
                && language
                    .chars()
                    .all(|c| c.is_ascii_alphabetic() || c == '-');
            if !valid {
                return Err(IntegrationError::NotConfigured(format!(
                    "nieprawidłowy kod języka whisper.cpp: `{language}`"
                )));
            }
        }
        Ok(())
    }
}

fn validate_file(label: &str, path: &std::path::Path) -> Result<(), IntegrationError> {
    let metadata = std::fs::metadata(path).map_err(|error| {
        IntegrationError::NotConfigured(format!("{label} `{}`: {error}", path.display()))
    })?;
    if !metadata.is_file() {
        return Err(IntegrationError::NotConfigured(format!(
            "{label} `{}` nie jest plikiem",
            path.display()
        )));
    }
    Ok(())
}

/// Dokładnie to, co dostaje runner. Nie ma tu URL-a, klucza API ani nazwy
/// dostawcy chmurowego — lokalna ścieżka nie ma czego wysyłać.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WhisperInvocation {
    pub program: PathBuf,
    pub model: PathBuf,
    pub audio_wav: Vec<u8>,
    pub language: Option<String>,
    pub threads: Option<u16>,
    pub timeout: Duration,
}

/// Jedyny sposób, w jaki adapter dotyka świata zewnętrznego. Podmiana runnera
/// w testach pozwala udowodnić, że nie ma żadnej innej ścieżki I/O.
#[async_trait]
pub trait WhisperRunner: Send + Sync {
    /// Zwraca surowy JSON wygenerowany przez whisper.cpp (`-oj`).
    async fn run(&self, invocation: &WhisperInvocation) -> Result<String, IntegrationError>;
}

#[derive(Debug, Default, Clone, Copy)]
pub struct ProcessWhisperRunner;

#[async_trait]
impl WhisperRunner for ProcessWhisperRunner {
    async fn run(&self, invocation: &WhisperInvocation) -> Result<String, IntegrationError> {
        if invocation.audio_wav.is_empty() {
            return Err(IntegrationError::Denied("puste nagranie".into()));
        }
        if invocation.timeout.is_zero() {
            return Err(IntegrationError::NotConfigured(
                "timeout whisper.cpp nie może być zerowy".into(),
            ));
        }

        let working_dir = tempfile::tempdir()?;
        let audio_path = working_dir.path().join("audio.wav");
        tokio::fs::write(&audio_path, &invocation.audio_wav).await?;
        let output_prefix = working_dir.path().join("transcript");

        let mut command = tokio::process::Command::new(&invocation.program);
        command
            .arg("-m")
            .arg(&invocation.model)
            .arg("-f")
            .arg(&audio_path)
            .arg("-oj")
            .arg("-of")
            .arg(&output_prefix)
            .arg("-np")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        if let Some(language) = invocation.language.as_deref() {
            command.arg("-l").arg(language);
        } else {
            command.arg("-l").arg("auto");
        }
        if let Some(threads) = invocation.threads {
            command.arg("-t").arg(threads.to_string());
        }

        let mut child = command.spawn().map_err(|error| {
            IntegrationError::NotConfigured(format!(
                "nie można uruchomić `{}`: {error}",
                invocation.program.display()
            ))
        })?;
        let mut stderr_pipe = child
            .stderr
            .take()
            .expect("stderr jest przechwytywane przez Stdio::piped()");
        let stderr_task = tokio::spawn(async move {
            let mut buffer = String::new();
            let _ = stderr_pipe.read_to_string(&mut buffer).await;
            buffer
        });

        let status = match tokio::time::timeout(invocation.timeout, child.wait()).await {
            Ok(result) => result?,
            Err(_) => {
                let _ = child.kill().await;
                let _ = stderr_task.await;
                return Err(IntegrationError::Timeout(format!(
                    "whisper.cpp przekroczył limit {:?}",
                    invocation.timeout
                )));
            }
        };
        let stderr = stderr_task.await.unwrap_or_default();

        if !status.success() {
            return Err(IntegrationError::provider(
                WHISPER_PROVIDER,
                format!(
                    "proces zakończył się kodem {:?}: {}",
                    status.code(),
                    truncate(&stderr, 2_000)
                ),
            ));
        }

        let json_path = output_prefix.with_extension("json");
        tokio::fs::read_to_string(&json_path)
            .await
            .map_err(|error| {
                IntegrationError::provider(
                    WHISPER_PROVIDER,
                    format!("brak pliku JSON `{}`: {error}", json_path.display()),
                )
            })
    }
}

pub struct WhisperCppSpeechToText<R: WhisperRunner = ProcessWhisperRunner> {
    config: WhisperCppConfig,
    runner: R,
}

impl WhisperCppSpeechToText<ProcessWhisperRunner> {
    pub fn new(config: WhisperCppConfig) -> Result<Self, IntegrationError> {
        Self::with_runner(config, ProcessWhisperRunner)
    }
}

impl<R: WhisperRunner> WhisperCppSpeechToText<R> {
    pub fn with_runner(config: WhisperCppConfig, runner: R) -> Result<Self, IntegrationError> {
        config.validate()?;
        Ok(Self { config, runner })
    }

    pub fn config(&self) -> &WhisperCppConfig {
        &self.config
    }
}

#[async_trait]
impl<R: WhisperRunner> SpeechToText for WhisperCppSpeechToText<R> {
    fn provider(&self) -> &'static str {
        WHISPER_PROVIDER
    }

    async fn transcribe(
        &self,
        request: TranscriptionRequest,
    ) -> Result<Transcript, IntegrationError> {
        let audio = request.audio;
        // `request.authorization` jest świadomie ignorowane: to ścieżka lokalna,
        // nic nie opuszcza komputera, więc zgoda chmurowa nie jest potrzebna.
        validate_local_audio(&audio)?;

        let raw = self
            .runner
            .run(&WhisperInvocation {
                program: self.config.binary.clone(),
                model: self.config.model.clone(),
                audio_wav: audio.bytes,
                language: self.config.language.clone(),
                threads: self.config.threads,
                timeout: self.config.timeout,
            })
            .await?;

        parse_whisper_json(&raw)
    }
}

fn validate_local_audio(audio: &AudioChunk) -> Result<(), IntegrationError> {
    audio.validate()?;
    if audio.format != AudioFormat::Wav {
        return Err(IntegrationError::Denied(format!(
            "whisper.cpp przyjmuje tylko WAV, otrzymano {}; konwersja nie jest wykonywana po cichu",
            audio.format.as_str()
        )));
    }
    if audio.sample_rate_hz != WHISPER_SAMPLE_RATE_HZ || audio.channels != WHISPER_CHANNELS {
        return Err(IntegrationError::Denied(format!(
            "whisper.cpp wymaga {} Hz mono, otrzymano {} Hz / {} kanał(y); konwersja nie jest wykonywana po cichu",
            WHISPER_SAMPLE_RATE_HZ, audio.sample_rate_hz, audio.channels
        )));
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
struct WhisperJson {
    #[serde(default)]
    transcription: Vec<WhisperSegment>,
    #[serde(default)]
    result: Option<WhisperResult>,
}

#[derive(Debug, Deserialize)]
struct WhisperSegment {
    text: String,
}

#[derive(Debug, Deserialize)]
struct WhisperResult {
    language: Option<String>,
}

fn parse_whisper_json(raw: &str) -> Result<Transcript, IntegrationError> {
    let parsed: WhisperJson = serde_json::from_str(raw).map_err(|error| {
        IntegrationError::provider(
            WHISPER_PROVIDER,
            format!("nieprawidłowy JSON od whisper.cpp: {error}"),
        )
    })?;

    let text = parsed
        .transcription
        .iter()
        .map(|segment| segment.text.as_str())
        .collect::<String>()
        .trim()
        .to_string();
    if text.is_empty() {
        return Err(IntegrationError::provider(
            WHISPER_PROVIDER,
            "whisper.cpp zwrócił pustą transkrypcję",
        ));
    }

    Ok(Transcript {
        text,
        language: parsed
            .result
            .and_then(|result| result.language)
            .map(|language| language.trim().to_string())
            .filter(|language| !language.is_empty()),
        confidence: None,
        is_final: true,
        provider: WHISPER_PROVIDER.to_string(),
        location: TranscriptionLocation::Local,
    })
}

fn truncate(text: &str, max_chars: usize) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= max_chars {
        return trimmed.to_string();
    }
    let mut result: String = trimmed.chars().take(max_chars).collect();
    result.push('…');
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    struct RecordingRunner {
        response: Result<String, String>,
        calls: Arc<Mutex<Vec<WhisperInvocation>>>,
    }

    #[async_trait]
    impl WhisperRunner for RecordingRunner {
        async fn run(&self, invocation: &WhisperInvocation) -> Result<String, IntegrationError> {
            self.calls.lock().unwrap().push(invocation.clone());
            match &self.response {
                Ok(raw) => Ok(raw.clone()),
                Err(message) => Err(IntegrationError::provider(
                    WHISPER_PROVIDER,
                    message.clone(),
                )),
            }
        }
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        adapter: WhisperCppSpeechToText<RecordingRunner>,
        calls: Arc<Mutex<Vec<WhisperInvocation>>>,
    }

    fn fixture(response: Result<String, String>) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let binary = dir.path().join("whisper-cli");
        std::fs::write(&binary, b"#!/bin/sh\nexit 0\n").unwrap();
        let model = dir.path().join("ggml-base.bin");
        std::fs::write(&model, b"model").unwrap();

        let calls = Arc::new(Mutex::new(Vec::new()));
        let runner = RecordingRunner {
            response,
            calls: Arc::clone(&calls),
        };
        let adapter =
            WhisperCppSpeechToText::with_runner(WhisperCppConfig::new(binary, model), runner)
                .unwrap();
        Fixture {
            _dir: dir,
            adapter,
            calls,
        }
    }

    fn wav() -> AudioChunk {
        AudioChunk {
            bytes: vec![0; 64],
            format: AudioFormat::Wav,
            sample_rate_hz: 16_000,
            channels: 1,
        }
    }

    fn valid_json() -> String {
        r#"{
            "transcription": [
                {"text": " otwórz "},
                {"text": "projekt "}
            ],
            "result": {"language": "pl"}
        }"#
        .to_string()
    }

    #[tokio::test]
    async fn transcribes_locally_without_any_network_path() {
        let fixture = fixture(Ok(valid_json()));
        let transcript = fixture
            .adapter
            .transcribe(TranscriptionRequest::local(wav()))
            .await
            .unwrap();

        assert_eq!(transcript.text, "otwórz projekt");
        assert_eq!(transcript.language.as_deref(), Some("pl"));
        assert!(transcript.is_final);
        assert_eq!(transcript.provider, WHISPER_PROVIDER);
        assert_eq!(transcript.location, TranscriptionLocation::Local);

        let calls = fixture.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].audio_wav, wav().bytes);
        assert!(calls[0].program.starts_with(fixture._dir.path()));
        assert!(calls[0].model.starts_with(fixture._dir.path()));
    }

    #[tokio::test]
    async fn local_failure_does_not_fall_back_to_cloud() {
        let fixture = fixture(Err("model nie wczytał się".into()));
        let error = fixture
            .adapter
            .transcribe(TranscriptionRequest::local(wav()))
            .await
            .unwrap_err();

        match error {
            IntegrationError::Provider { provider, message } => {
                assert_eq!(provider, WHISPER_PROVIDER);
                assert!(message.contains("model"));
            }
            other => panic!("oczekiwano błędu lokalnego dostawcy, jest {other:?}"),
        }
        assert_eq!(fixture.calls.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn refuses_non_wav_before_starting_the_process() {
        let fixture = fixture(Ok(valid_json()));
        let mut audio = wav();
        audio.format = AudioFormat::Mp3;

        let error = fixture
            .adapter
            .transcribe(TranscriptionRequest::local(audio))
            .await
            .unwrap_err();
        assert!(matches!(error, IntegrationError::Denied(_)));
        assert!(fixture.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn refuses_wrong_sample_rate_without_hidden_conversion() {
        let fixture = fixture(Ok(valid_json()));
        let mut audio = wav();
        audio.sample_rate_hz = 48_000;

        let error = fixture
            .adapter
            .transcribe(TranscriptionRequest::local(audio))
            .await
            .unwrap_err();
        assert!(matches!(error, IntegrationError::Denied(_)));
        assert!(fixture.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn passes_language_and_threads_to_the_local_binary() {
        let dir = tempfile::tempdir().unwrap();
        let binary = dir.path().join("whisper-cli");
        std::fs::write(&binary, b"#!/bin/sh\n").unwrap();
        let model = dir.path().join("ggml-base.bin");
        std::fs::write(&model, b"model").unwrap();

        let calls = Arc::new(Mutex::new(Vec::new()));
        let runner = RecordingRunner {
            response: Ok(valid_json()),
            calls: Arc::clone(&calls),
        };
        let adapter = WhisperCppSpeechToText::with_runner(
            WhisperCppConfig::new(binary, model)
                .with_language("pl")
                .with_threads(4),
            runner,
        )
        .unwrap();

        adapter
            .transcribe(TranscriptionRequest::local(wav()))
            .await
            .unwrap();

        let calls = calls.lock().unwrap();
        assert_eq!(calls[0].language.as_deref(), Some("pl"));
        assert_eq!(calls[0].threads, Some(4));
    }

    #[test]
    fn missing_binary_or_model_is_a_hard_error() {
        let dir = tempfile::tempdir().unwrap();
        let existing = dir.path().join("exists");
        std::fs::write(&existing, b"x").unwrap();

        let config = WhisperCppConfig::new(dir.path().join("nie-ma"), &existing);
        assert!(matches!(
            WhisperCppSpeechToText::new(config),
            Err(IntegrationError::NotConfigured(_))
        ));

        let config = WhisperCppConfig::new(&existing, dir.path().join("nie-ma"));
        assert!(matches!(
            WhisperCppSpeechToText::new(config),
            Err(IntegrationError::NotConfigured(_))
        ));
    }

    #[test]
    fn rejects_unsafe_language_or_zero_limits() {
        let dir = tempfile::tempdir().unwrap();
        let binary = dir.path().join("bin");
        std::fs::write(&binary, b"x").unwrap();
        let model = dir.path().join("model");
        std::fs::write(&model, b"x").unwrap();

        let mut config = WhisperCppConfig::new(&binary, &model);
        config.language = Some("-oj".into());
        assert!(config.validate().is_err());

        let mut config = WhisperCppConfig::new(&binary, &model);
        config.timeout = Duration::ZERO;
        assert!(config.validate().is_err());

        let mut config = WhisperCppConfig::new(&binary, &model);
        config.threads = Some(0);
        assert!(config.validate().is_err());
    }

    #[tokio::test]
    async fn invalid_json_and_empty_transcript_are_errors() {
        let invalid = fixture(Ok("nie-json".into()));
        assert!(matches!(
            invalid
                .adapter
                .transcribe(TranscriptionRequest::local(wav()))
                .await,
            Err(IntegrationError::Provider { .. })
        ));

        let empty = fixture(Ok(
            r#"{"transcription":[],"result":{"language":"pl"}}"#.into()
        ));
        assert!(matches!(
            empty
                .adapter
                .transcribe(TranscriptionRequest::local(wav()))
                .await,
            Err(IntegrationError::Provider { .. })
        ));
    }

    #[test]
    fn integrations_crate_links_no_http_client() {
        let manifest = include_str!("../Cargo.toml");
        for forbidden in ["reqwest", "hyper", "ureq", "isahc"] {
            assert!(
                !manifest.contains(forbidden),
                "`{forbidden}` nie może być zależnością lokalnego crate; adapter chmurowy ma powstać osobno"
            );
        }
    }
}
