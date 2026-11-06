use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use victor_domain::{Approval, Plan, PlanHash, UnixMs};

use crate::error::IntegrationError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioFormat {
    Wav,
    Opus,
    Mp3,
    Flac,
}

impl AudioFormat {
    pub fn as_str(&self) -> &'static str {
        match self {
            AudioFormat::Wav => "wav",
            AudioFormat::Opus => "opus",
            AudioFormat::Mp3 => "mp3",
            AudioFormat::Flac => "flac",
        }
    }

    pub fn mime_type(&self) -> &'static str {
        match self {
            AudioFormat::Wav => "audio/wav",
            AudioFormat::Opus => "audio/opus",
            AudioFormat::Mp3 => "audio/mpeg",
            AudioFormat::Flac => "audio/flac",
        }
    }
}

/// Kawałek dźwięku wejściowego. Metadane są wymagane, bo backend STT
/// (whisper.cpp / Deepgram / OpenAI) nie zgaduje formatu z bajtów.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioChunk {
    pub bytes: Vec<u8>,
    pub format: AudioFormat,
    pub sample_rate_hz: u32,
    pub channels: u8,
}

impl AudioChunk {
    /// Hash całej operacji, nie tylko bajtów: format, próbkowanie i liczba
    /// kanałów też są częścią zgody. Dzięki temu podmiana metadanych unieważnia
    /// permit tak samo jak podmiana nagrania.
    pub fn sha256(&self) -> String {
        let material = format!(
            "{}\u{1f}{}\u{1f}{}\u{1f}{}",
            self.format.as_str(),
            self.sample_rate_hz,
            self.channels,
            crate::sha256_hex(&self.bytes)
        );
        crate::sha256_hex(material.as_bytes())
    }

    pub fn validate(&self) -> Result<(), IntegrationError> {
        if self.bytes.is_empty() {
            return Err(IntegrationError::Denied("puste nagranie".into()));
        }
        if self.sample_rate_hz == 0 || self.sample_rate_hz > 384_000 {
            return Err(IntegrationError::Denied(format!(
                "nieprawidłowe próbkowanie: {} Hz",
                self.sample_rate_hz
            )));
        }
        if self.channels == 0 || self.channels > 2 {
            return Err(IntegrationError::Denied(format!(
                "nieprawidłowa liczba kanałów: {}",
                self.channels
            )));
        }
        Ok(())
    }
}

/// Czy transkrypcja powstała na komputerze, czy po wysłaniu audio do usługi.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TranscriptionLocation {
    Local,
    Cloud,
}

impl TranscriptionLocation {
    pub fn as_str(&self) -> &'static str {
        match self {
            TranscriptionLocation::Local => "local",
            TranscriptionLocation::Cloud => "cloud",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Transcript {
    pub text: String,
    pub language: Option<String>,
    /// 0.0–1.0; `None`, gdy dostawca nie podaje pewności.
    pub confidence: Option<f32>,
    /// Czy to wynik częściowy (streaming) czy końcowy.
    pub is_final: bool,
    /// Dostawca, np. `whisper.cpp` albo `openai`. Trafia do `ContextItem`.
    pub provider: String,
    /// Lokalne albo chmurowe. `ContextItem` musi to zapisać.
    pub location: TranscriptionLocation,
}

/// Zgoda na wysłanie audio poza komputer.
///
/// Powstaje wyłącznie z `ApprovalKind::Critical` powiązanej z hashem planu
/// **oraz** z hashem konkretnego nagrania. Adapter chmurowy nie może jej
/// zastąpić własnym „tak" — dostaje gotowy permit i jeszcze raz sprawdza oba
/// hashe. Standardowa zgoda z widoku planu nie wystarcza.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloudTranscriptionPermit {
    plan_hash: PlanHash,
    audio_sha256: String,
}

impl CloudTranscriptionPermit {
    pub fn authorize(
        plan: &Plan,
        approval: &Approval,
        audio: &AudioChunk,
        now_unix_ms: UnixMs,
    ) -> Result<Self, IntegrationError> {
        if !approval.is_critical() {
            return Err(IntegrationError::Denied(
                "wysyłka audio do chmury wymaga zgody krytycznej, nie zwykłej".into(),
            ));
        }
        approval.verify(plan, now_unix_ms).map_err(|error| {
            IntegrationError::Denied(format!("zgoda nie obejmuje tego planu: {error}"))
        })?;
        audio.validate()?;
        Ok(Self {
            plan_hash: plan.hash(),
            audio_sha256: audio.sha256(),
        })
    }

    pub fn plan_hash(&self) -> &PlanHash {
        &self.plan_hash
    }

    pub fn audio_sha256(&self) -> &str {
        &self.audio_sha256
    }

    /// Ponowne sprawdzenie po stronie adaptera chmurowego — permit mógł
    /// powstać dla innego planu albo innego nagrania.
    pub fn verify(&self, plan: &Plan, audio: &AudioChunk) -> Result<(), IntegrationError> {
        if self.plan_hash != plan.hash() {
            return Err(IntegrationError::Denied(
                "zgoda dotyczy innego planu niż wykonywany".into(),
            ));
        }
        self.verify_audio(audio)
    }

    /// Wystarczające dla adaptera chmurowego: sam permit powstał już z
    /// krytycznej zgody powiązanej z planem, a tutaj blokujemy podmianę nagrania.
    pub fn verify_audio(&self, audio: &AudioChunk) -> Result<(), IntegrationError> {
        if self.audio_sha256 != audio.sha256() {
            return Err(IntegrationError::Denied(
                "zgoda dotyczy innego nagrania niż wysyłane".into(),
            ));
        }
        Ok(())
    }
}

/// Żądanie transkrypcji. Lokalny adapter ignoruje `authorization` — nie ma
/// czego autoryzować, dopóki nic nie opuszcza komputera.
#[derive(Debug, Clone, PartialEq)]
pub struct TranscriptionRequest {
    pub audio: AudioChunk,
    pub authorization: Option<CloudTranscriptionPermit>,
}

impl TranscriptionRequest {
    pub fn local(audio: AudioChunk) -> Self {
        Self {
            audio,
            authorization: None,
        }
    }

    pub fn cloud(audio: AudioChunk, permit: CloudTranscriptionPermit) -> Self {
        Self {
            audio,
            authorization: Some(permit),
        }
    }
}

/// Wejście głosowe. Implementacje lokalne nie mają prawa wykonywać żadnych
/// żądań sieciowych; implementacje chmurowe wymagają `CloudTranscriptionPermit`.
#[async_trait]
pub trait SpeechToText: Send + Sync {
    fn provider(&self) -> &'static str;

    async fn transcribe(
        &self,
        request: TranscriptionRequest,
    ) -> Result<Transcript, IntegrationError>;
}

#[derive(Debug, Clone, PartialEq)]
pub struct SpeechRequest {
    pub text: String,
    /// `None` = głos domyślny dostawcy.
    pub voice: Option<String>,
    pub language: String,
    pub speed: f32,
}

impl SpeechRequest {
    pub fn new(text: impl Into<String>, language: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            voice: None,
            language: language.into(),
            speed: 1.0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpeechAudio {
    pub bytes: Vec<u8>,
    pub format: AudioFormat,
    pub sample_rate_hz: u32,
    /// Deterministyczny klucz cache — ten sam tekst/głos/język/prędkość
    /// zawsze daje ten sam klucz, niezależnie od dostawcy w obrębie `provider`.
    pub cache_key: String,
}

/// Wyjście głosowe. Adapter do Audioticon `SpeechSynthesizer`.
#[async_trait]
pub trait SpeechOutput: Send + Sync {
    fn provider(&self) -> &'static str;

    async fn synthesize(&self, request: SpeechRequest) -> Result<SpeechAudio, IntegrationError>;
}

/// Klucz cache TTS. Pola łączymy separatorem, którego nie ma w normalnym
/// tekście, żeby `("ab","c")` i `("a","bc")` nie kolidowały.
pub fn speech_cache_key(request: &SpeechRequest, provider: &str) -> String {
    let voice = request.voice.as_deref().unwrap_or("default");
    let material = format!(
        "{provider}\u{1f}{}\u{1f}{voice}\u{1f}{}\u{1f}{:.3}",
        request.language, request.text, request.speed
    );
    crate::sha256_hex(material.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{FakeSpeechOutput, FakeSpeechToText};
    use serde_json::json;
    use victor_domain::{PlanStep, RiskLevel, ToolId};

    fn wav() -> AudioChunk {
        AudioChunk {
            bytes: vec![1, 2, 3, 4],
            format: AudioFormat::Wav,
            sample_rate_hz: 16_000,
            channels: 1,
        }
    }

    fn critical_plan() -> Plan {
        Plan::new(
            "task-stt",
            1,
            "transcribe_audio",
            vec![PlanStep::new(
                ToolId::parse("stt.cloud").unwrap(),
                "Wyślij nagranie do usługi STT",
                json!({"provider": "openai"}),
                RiskLevel::R3,
            )],
        )
        .unwrap()
    }

    #[test]
    fn cache_key_is_deterministic() {
        let request = SpeechRequest::new("Dzień dobry", "pl");
        assert_eq!(
            speech_cache_key(&request, "piper"),
            speech_cache_key(&request, "piper")
        );
    }

    #[test]
    fn cache_key_depends_on_every_field() {
        let base = SpeechRequest::new("Dzień dobry", "pl");
        let base_key = speech_cache_key(&base, "piper");

        let mut other_text = base.clone();
        other_text.text = "Dobry wieczór".into();
        assert_ne!(base_key, speech_cache_key(&other_text, "piper"));

        let mut other_voice = base.clone();
        other_voice.voice = Some("Ewa".into());
        assert_ne!(base_key, speech_cache_key(&other_voice, "piper"));

        let mut other_language = base.clone();
        other_language.language = "en".into();
        assert_ne!(base_key, speech_cache_key(&other_language, "piper"));

        let mut other_speed = base.clone();
        other_speed.speed = 1.25;
        assert_ne!(base_key, speech_cache_key(&other_speed, "piper"));

        assert_ne!(base_key, speech_cache_key(&base, "edge"));
    }

    #[tokio::test]
    async fn fake_stt_returns_final_local_transcript() {
        let stt = FakeSpeechToText::new("otwórz projekt");
        let transcript = stt
            .transcribe(TranscriptionRequest::local(wav()))
            .await
            .unwrap();
        assert_eq!(transcript.text, "otwórz projekt");
        assert!(transcript.is_final);
        assert_eq!(transcript.location, TranscriptionLocation::Local);
        assert_eq!(transcript.provider, "fake-stt");
    }

    #[tokio::test]
    async fn fake_tts_uses_deterministic_cache_key() {
        let tts = FakeSpeechOutput::default();
        let request = SpeechRequest::new("test", "pl");
        let audio = tts.synthesize(request.clone()).await.unwrap();
        assert_eq!(audio.cache_key, speech_cache_key(&request, "fake-tts"));
    }

    #[test]
    fn audio_hash_covers_metadata_not_only_bytes() {
        let base = wav();
        let mut other_rate = base.clone();
        other_rate.sample_rate_hz = 48_000;
        let mut other_bytes = base.clone();
        other_bytes.bytes.push(5);

        assert_ne!(base.sha256(), other_rate.sha256());
        assert_ne!(base.sha256(), other_bytes.sha256());
    }

    #[test]
    fn audio_validation_rejects_empty_and_bad_metadata() {
        let mut audio = wav();
        audio.bytes.clear();
        assert!(audio.validate().is_err());

        let mut audio = wav();
        audio.sample_rate_hz = 0;
        assert!(audio.validate().is_err());

        let mut audio = wav();
        audio.channels = 3;
        assert!(audio.validate().is_err());
    }

    #[test]
    fn cloud_permit_requires_critical_approval() {
        let plan = critical_plan();
        let audio = wav();
        let standard = plan.approve(1_000).unwrap();
        let error =
            CloudTranscriptionPermit::authorize(&plan, &standard, &audio, 1_100).unwrap_err();
        assert!(matches!(error, IntegrationError::Denied(_)));
    }

    #[test]
    fn cloud_permit_requires_unexpired_approval() {
        let plan = critical_plan();
        let audio = wav();
        let approval = plan.approve_critical_with_ttl(1_000, 100).unwrap();
        let error =
            CloudTranscriptionPermit::authorize(&plan, &approval, &audio, 1_100).unwrap_err();
        assert!(matches!(error, IntegrationError::Denied(_)));
    }

    #[test]
    fn cloud_permit_is_bound_to_plan_and_audio() {
        let plan = critical_plan();
        let audio = wav();
        let approval = plan.approve_critical(1_000).unwrap();
        let permit = CloudTranscriptionPermit::authorize(&plan, &approval, &audio, 1_100).unwrap();

        assert!(permit.verify(&plan, &audio).is_ok());
        assert!(permit.verify_audio(&audio).is_ok());

        let mut other_audio = audio.clone();
        other_audio.bytes.push(9);
        assert!(permit.verify(&plan, &other_audio).is_err());
        assert!(permit.verify_audio(&other_audio).is_err());

        let mut other_plan = plan.clone();
        other_plan.intent = "inny cel".into();
        assert!(permit.verify(&other_plan, &audio).is_err());
    }
}
