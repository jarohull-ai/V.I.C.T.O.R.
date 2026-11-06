//! Typ żądania, którego nie da się zbudować bez zgody.
//!
//! To jest celowa różnica względem lokalnego `TranscriptionRequest`: warstwa
//! HTTP nie przyjmuje „request z opcjonalnym permitem", tylko typ, w którym
//! permit i plan są nieusuwalną częścią konstrukcji.

use std::fmt;

use async_trait::async_trait;
use victor_domain::Plan;
use victor_integrations::{AudioChunk, CloudTranscriptionPermit, IntegrationError, Transcript};

#[derive(Clone)]
pub struct AuthorizedCloudTranscriptionRequest {
    plan: Plan,
    audio: AudioChunk,
    permit: CloudTranscriptionPermit,
}

impl AuthorizedCloudTranscriptionRequest {
    /// Jedyny konstruktor. Sprawdza, że permit dotyczy dokładnie tego planu
    /// i tego nagrania — nie tylko „jakiegoś" krytycznie zatwierdzonego planu.
    pub fn new(
        plan: &Plan,
        audio: AudioChunk,
        permit: CloudTranscriptionPermit,
    ) -> Result<Self, IntegrationError> {
        permit.verify(plan, &audio)?;
        Ok(Self {
            plan: plan.clone(),
            audio,
            permit,
        })
    }

    pub fn audio(&self) -> &AudioChunk {
        &self.audio
    }

    pub fn plan(&self) -> &Plan {
        &self.plan
    }

    pub fn permit(&self) -> &CloudTranscriptionPermit {
        &self.permit
    }
}

/// `Debug` nie może pokazywać ani bajtów audio, ani argumentów planu
/// (mogą zawierać prompt lub ścieżki). Zostają wyłącznie hashe i długość.
impl fmt::Debug for AuthorizedCloudTranscriptionRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AuthorizedCloudTranscriptionRequest")
            .field("plan_hash", &self.plan.hash())
            .field("audio_sha256", &self.permit.audio_sha256())
            .field("audio_len", &self.audio.bytes.len())
            .finish()
    }
}

/// Osobny trait od lokalnego `SpeechToText`: jego sygnatura nie pozwala
/// przekazać nagrania bez `AuthorizedCloudTranscriptionRequest`.
#[async_trait]
pub trait CloudSpeechToText: Send + Sync {
    fn provider(&self) -> &'static str;

    async fn transcribe(
        &self,
        request: AuthorizedCloudTranscriptionRequest,
    ) -> Result<Transcript, IntegrationError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support;

    #[test]
    fn cannot_be_built_without_a_permit_for_the_same_audio() {
        let plan = test_support::plan();
        let permitted = test_support::wav();
        let permit = test_support::permit_for(&plan, &permitted);

        assert!(
            AuthorizedCloudTranscriptionRequest::new(&plan, permitted.clone(), permit.clone())
                .is_ok()
        );

        let mut other = permitted;
        other.bytes.push(1);
        assert!(matches!(
            AuthorizedCloudTranscriptionRequest::new(&plan, other, permit),
            Err(IntegrationError::Denied(_))
        ));
    }

    #[test]
    fn cannot_be_built_with_a_permit_for_another_plan() {
        let plan = test_support::plan();
        let audio = test_support::wav();
        let permit = test_support::permit_for(&plan, &audio);

        let mut other_plan = plan.clone();
        other_plan.intent = "inny cel".into();

        assert!(matches!(
            AuthorizedCloudTranscriptionRequest::new(&other_plan, audio, permit),
            Err(IntegrationError::Denied(_))
        ));
    }

    #[test]
    fn expired_approval_cannot_create_a_permit() {
        let plan = test_support::plan();
        let audio = test_support::wav();
        let approval = plan.approve_critical_with_ttl(1_000, 100).unwrap();
        let error =
            CloudTranscriptionPermit::authorize(&plan, &approval, &audio, 1_100).unwrap_err();
        assert!(matches!(error, IntegrationError::Denied(_)));
    }

    #[test]
    fn debug_does_not_show_audio_or_plan_arguments() {
        let plan = test_support::plan();
        let audio = test_support::wav();
        let permit = test_support::permit_for(&plan, &audio);
        let request = AuthorizedCloudTranscriptionRequest::new(&plan, audio, permit).unwrap();

        let rendered = format!("{request:?}");
        assert!(rendered.contains("plan_hash"));
        assert!(rendered.contains("audio_sha256"));
        assert!(rendered.contains("audio_len"));
        assert!(!rendered.contains("Wyślij audio do usługi STT"));
    }
}
