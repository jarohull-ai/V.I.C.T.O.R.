//! Deterministyczne adaptery do testów runtime. Nie udają prawdziwych usług:
//! liczą wywołania i pozwalają sprawdzić, że ścieżka lokalna nie dotyka chmury.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use victor_integrations::{
    IntegrationError, SpeechToText, Transcript, TranscriptionLocation, TranscriptionRequest,
};
use victor_stt_cloud::{AuthorizedCloudTranscriptionRequest, CloudSpeechToText};

#[derive(Debug, Clone)]
pub struct FakeCloudSpeechToText {
    pub text: String,
    pub language: Option<String>,
    pub confidence: Option<f32>,
    fail: Option<String>,
    calls: Arc<AtomicUsize>,
}

impl FakeCloudSpeechToText {
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            language: Some("pl".into()),
            confidence: Some(0.99),
            fail: None,
            calls: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub fn failing(message: impl Into<String>) -> Self {
        Self {
            fail: Some(message.into()),
            ..Self::new("")
        }
    }

    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl CloudSpeechToText for FakeCloudSpeechToText {
    fn provider(&self) -> &'static str {
        "fake-cloud"
    }

    async fn transcribe(
        &self,
        request: AuthorizedCloudTranscriptionRequest,
    ) -> Result<Transcript, IntegrationError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        request.audio().validate()?;
        if let Some(message) = &self.fail {
            return Err(IntegrationError::provider("fake-cloud", message.clone()));
        }
        Ok(Transcript {
            text: self.text.clone(),
            language: self.language.clone(),
            confidence: self.confidence,
            is_final: true,
            provider: self.provider().to_string(),
            location: TranscriptionLocation::Cloud,
        })
    }
}

/// Lokalny adapter, który zawsze zawodzi. Służy do dowodu, że błąd lokalny nie
/// uruchamia chmury.
#[derive(Debug, Clone)]
pub struct FailingSpeechToText {
    message: String,
    calls: Arc<AtomicUsize>,
}

impl FailingSpeechToText {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            calls: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl SpeechToText for FailingSpeechToText {
    fn provider(&self) -> &'static str {
        "failing-local"
    }

    async fn transcribe(
        &self,
        _request: TranscriptionRequest,
    ) -> Result<Transcript, IntegrationError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Err(IntegrationError::provider(
            self.provider(),
            self.message.clone(),
        ))
    }
}
