//! Deterministyczne atrapy kontraktów. Nie udają dostawców — istnieją po to,
//! żeby runtime i UI dało się testować bez sieci, kluczy i modeli.

use async_trait::async_trait;

use crate::error::IntegrationError;
use crate::fetch::{FetchPolicy, FetchProvenance, FetchRequest, FetchResponse, WebFetcher};
use crate::voice::{
    speech_cache_key, AudioFormat, SpeechAudio, SpeechOutput, SpeechRequest, SpeechToText,
    Transcript, TranscriptionLocation, TranscriptionRequest,
};

#[derive(Debug, Clone)]
pub struct FakeSpeechToText {
    pub text: String,
    pub language: Option<String>,
    pub confidence: Option<f32>,
    pub is_final: bool,
    pub location: TranscriptionLocation,
}

impl FakeSpeechToText {
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            language: Some("pl".into()),
            confidence: Some(1.0),
            is_final: true,
            location: TranscriptionLocation::Local,
        }
    }

    pub fn cloud(mut self) -> Self {
        self.location = TranscriptionLocation::Cloud;
        self
    }
}

#[async_trait]
impl SpeechToText for FakeSpeechToText {
    fn provider(&self) -> &'static str {
        "fake-stt"
    }

    async fn transcribe(
        &self,
        request: TranscriptionRequest,
    ) -> Result<Transcript, IntegrationError> {
        if request.audio.bytes.is_empty() {
            return Err(IntegrationError::Denied("puste nagranie".into()));
        }
        Ok(Transcript {
            text: self.text.clone(),
            language: self.language.clone(),
            confidence: self.confidence,
            is_final: self.is_final,
            provider: self.provider().to_string(),
            location: self.location,
        })
    }
}

#[derive(Debug, Clone)]
pub struct FakeSpeechOutput {
    pub format: AudioFormat,
    pub sample_rate_hz: u32,
    pub audio: Vec<u8>,
}

impl Default for FakeSpeechOutput {
    fn default() -> Self {
        Self {
            format: AudioFormat::Wav,
            sample_rate_hz: 22_050,
            audio: vec![0; 4],
        }
    }
}

#[async_trait]
impl SpeechOutput for FakeSpeechOutput {
    fn provider(&self) -> &'static str {
        "fake-tts"
    }

    async fn synthesize(&self, request: SpeechRequest) -> Result<SpeechAudio, IntegrationError> {
        if request.text.trim().is_empty() {
            return Err(IntegrationError::Denied("pusty tekst do syntezy".into()));
        }
        Ok(SpeechAudio {
            bytes: self.audio.clone(),
            format: self.format,
            sample_rate_hz: self.sample_rate_hz,
            cache_key: speech_cache_key(&request, self.provider()),
        })
    }
}

#[derive(Debug, Clone)]
pub struct FakeWebFetcher {
    pub status: u16,
    pub content_type: String,
    pub body: Vec<u8>,
    pub now_unix_ms: u64,
}

impl Default for FakeWebFetcher {
    fn default() -> Self {
        Self {
            status: 200,
            content_type: "text/html".into(),
            body: Vec::new(),
            now_unix_ms: 0,
        }
    }
}

impl FakeWebFetcher {
    pub fn new(
        status: u16,
        content_type: impl Into<String>,
        body: Vec<u8>,
        now_unix_ms: u64,
    ) -> Self {
        Self {
            status,
            content_type: content_type.into(),
            body,
            now_unix_ms,
        }
    }
}

#[async_trait]
impl WebFetcher for FakeWebFetcher {
    fn backend(&self) -> &'static str {
        "fake-fetch"
    }

    async fn fetch(
        &self,
        request: FetchRequest,
        policy: &FetchPolicy,
    ) -> Result<FetchResponse, IntegrationError> {
        policy.validate(&request)?;

        let (body, truncated) = if self.body.len() > policy.max_response_bytes {
            (self.body[..policy.max_response_bytes].to_vec(), true)
        } else {
            (self.body.clone(), false)
        };

        Ok(FetchResponse {
            provenance: FetchProvenance {
                requested_url: request.url.clone(),
                final_url: request.url,
                status: self.status,
                content_type: Some(self.content_type.clone()),
                fetched_at_unix_ms: self.now_unix_ms,
                sha256: crate::sha256_hex(&body),
                bytes: body.len(),
            },
            body,
            truncated,
        })
    }
}
