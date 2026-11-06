//! Chmurowy STT V.I.C.T.O.R. — OpenAI i Deepgram za osobnymi feature flagami.
//!
//! Ten crate jest **jedynym** miejscem, w którym V.I.C.T.O.R. może wysłać audio
//! poza komputer. Dlatego:
//!
//! - API nie przyjmuje zwykłego `TranscriptionRequest`; wymaga
//!   [`AuthorizedCloudTranscriptionRequest`], którego nie da się zbudować bez
//!   [`CloudTranscriptionPermit`](victor_integrations::CloudTranscriptionPermit)
//!   pasującego do konkretnego planu i nagrania;
//! - permit jest sprawdzany **przed każdą próbą**, także przed retry;
//! - endpoint musi przejść allowlistę (scheme + host + prefiks ścieżki), a
//!   `HttpRequest` da się zbudować tylko z [`ValidatedEndpoint`];
//! - nie ma fallbacku między providerami: OpenAI i Deepgram to osobne typy,
//!   wybierane feature flagą, nie łańcuch „spróbuj kolejnego";
//! - ten crate nie loguje niczego i nie ma **bezpośredniej** zależności
//!   `log`/`tracing`; `Debug` requestu pokazuje tylko nazwy nagłówków i długość
//!   body. Aplikacja hostująca nie może włączać trace-logów `reqwest`.
//!
//! Feature flags:
//! - `openai`
//! - `deepgram`
//!
//! Domyślnie oba są wyłączone — sama zależność od tego crate'a nie dodaje
//! klienta HTTP do lokalnej ścieżki whisper.cpp.

pub mod authorized;
pub mod endpoint;
pub mod retry;
pub mod secret;
pub mod testing;
pub mod transport;

#[cfg(feature = "deepgram")]
pub mod deepgram;
#[cfg(feature = "openai")]
pub mod openai;

pub use authorized::{AuthorizedCloudTranscriptionRequest, CloudSpeechToText};
pub use endpoint::{AllowedEndpoint, EndpointAllowlist, ValidatedEndpoint};
pub use retry::RetryPolicy;
pub use secret::ApiKey;
pub use transport::{HttpRequest, HttpResponse, HttpSettings, HttpTransport};

#[cfg(any(feature = "openai", feature = "deepgram"))]
pub use transport::ReqwestTransport;

#[cfg(feature = "deepgram")]
pub use deepgram::{DeepgramConfig, DeepgramSpeechToText};
#[cfg(feature = "openai")]
pub use openai::{OpenAiConfig, OpenAiSpeechToText};

use std::time::Duration;

#[cfg(any(feature = "openai", feature = "deepgram"))]
use victor_integrations::IntegrationError;

pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);
pub const DEFAULT_MAX_RESPONSE_BYTES: u64 = 1024 * 1024;
pub const DEFAULT_MAX_AUDIO_BYTES: u64 = 25 * 1024 * 1024;

#[cfg(any(feature = "openai", feature = "deepgram"))]
pub(crate) fn validate_model(model: &str) -> Result<(), IntegrationError> {
    let valid = !model.is_empty()
        && model.len() <= 64
        && model
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | ':'));
    if valid {
        Ok(())
    } else {
        Err(IntegrationError::NotConfigured(
            "nieprawidłowa nazwa modelu STT".into(),
        ))
    }
}

#[cfg(any(feature = "openai", feature = "deepgram"))]
pub(crate) fn validate_language(language: &str) -> Result<(), IntegrationError> {
    let valid = !language.is_empty()
        && language.len() <= 16
        && language
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic())
        && language
            .chars()
            .all(|c| c.is_ascii_alphabetic() || c == '-');
    if valid {
        Ok(())
    } else {
        Err(IntegrationError::NotConfigured(
            "nieprawidłowy kod języka STT".into(),
        ))
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use serde_json::json;
    use victor_domain::{Plan, PlanStep, RiskLevel, ToolId};
    use victor_integrations::{AudioChunk, AudioFormat, CloudTranscriptionPermit};

    pub fn wav() -> AudioChunk {
        AudioChunk {
            bytes: vec![1, 2, 3, 4, 5, 6],
            format: AudioFormat::Wav,
            sample_rate_hz: 16_000,
            channels: 1,
        }
    }

    pub fn plan() -> Plan {
        Plan::new(
            "task-cloud",
            1,
            "transcribe_audio",
            vec![PlanStep::new(
                ToolId::parse("stt.cloud").unwrap(),
                "Wyślij audio do usługi STT",
                json!({"provider": "test"}),
                RiskLevel::R3,
            )],
        )
        .unwrap()
    }

    pub fn permit_for(plan: &Plan, audio: &AudioChunk) -> CloudTranscriptionPermit {
        let approval = plan.approve_critical(1_000).unwrap();
        CloudTranscriptionPermit::authorize(plan, &approval, audio, 1_100).unwrap()
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn features_are_opt_in() {
        let manifest = include_str!("../Cargo.toml");
        assert!(
            manifest.contains("default = []"),
            "domyślnie żaden provider chmurowy nie może być włączony"
        );
        assert!(manifest.contains("openai = ["));
        assert!(manifest.contains("deepgram = ["));
    }

    #[test]
    fn http_client_is_optional() {
        let manifest = include_str!("../Cargo.toml");
        assert!(
            manifest.contains("reqwest = { version = \"0.12\", optional = true"),
            "klient HTTP musi być opcjonalny, inaczej lokalna ścieżka go linkuje"
        );
        assert!(
            manifest.contains("futures-util = { version = \"0.3\", optional = true"),
            "futures-util musi być opcjonalny razem z reqwest"
        );
    }

    #[test]
    fn crate_has_no_direct_logging_dependency() {
        let manifest = include_str!("../Cargo.toml");
        for forbidden in ["tracing", "log =", "env_logger", "simplelog"] {
            assert!(
                !manifest.contains(forbidden),
                "`{forbidden}` nie może być zależnością: audio, klucze i prompty nie mogą trafiać do logów"
            );
        }
    }
}
