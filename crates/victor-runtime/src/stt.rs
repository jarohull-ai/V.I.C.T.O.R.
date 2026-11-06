//! Pętla STT: `AudioChunk -> provider -> Transcript -> ContextItem`.
//!
//! Runtime, nie UI, decyduje o providerze:
//!
//! - normalne wejście głosowe zawsze idzie do lokalnego adaptera
//!   ([`SttRuntime::transcribe`]); żądanie nie ma pola „provider";
//! - chmurę można uruchomić wyłącznie przez [`SttRuntime::execute_approved_cloud_step`],
//!   czyli w imieniu zatwierdzonego planu R3. Provider chmurowy jest jednym,
//!   skonfigurowanym na poziomie runtime adapterem — nie listą do wyboru w UI;
//! - błąd lokalnego backendu wraca do użytkownika. Nie ma tu żadnej gałęzi,
//!   która po błędzie lokalnym sięga po chmurę.

use std::sync::Arc;

use victor_context::{ContextItem, ContextItemId, ContextScope, ContextStore};
use victor_domain::{Approval, Plan, UnixMs};
use victor_integrations::{
    AudioChunk, CloudTranscriptionPermit, SpeechToText, Transcript, TranscriptionLocation,
    TranscriptionRequest,
};
use victor_policy::PolicyEngine;
use victor_stt_cloud::{AuthorizedCloudTranscriptionRequest, CloudSpeechToText};

use crate::context_bridge::{transcript_to_context_item, TranscriptContextPolicy};
use crate::error::RuntimeError;

/// Identyfikator kroku planu, który wolno wykonać jako chmurowy STT.
pub const CLOUD_STT_TOOL_ID: &str = "stt.cloud";

/// Czy runtime może w ogóle korzystać z chmury. Domyślnie nie.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CloudEgressPolicy {
    #[default]
    Disabled,
    /// Tylko krok planu R3 z ważną zgodą `ApprovalKind::Critical`.
    CriticalApprovalOnly,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SttRuntimeConfig {
    pub cloud_egress: CloudEgressPolicy,
    pub transcript_context: TranscriptContextPolicy,
}

impl Default for SttRuntimeConfig {
    fn default() -> Self {
        Self {
            cloud_egress: CloudEgressPolicy::Disabled,
            transcript_context: TranscriptContextPolicy::default(),
        }
    }
}

/// Jedno zadanie transkrypcji. Nie ma tu providera ani trybu chmurowego —
/// o trasie decyduje metoda runtime, nie pole sterowane przez UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SttJob {
    pub item_id: ContextItemId,
    pub scope: ContextScope,
    pub audio: AudioChunk,
    pub now_unix_ms: UnixMs,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SttRoute {
    Local,
    Cloud,
}

impl SttRoute {
    pub fn as_str(&self) -> &'static str {
        match self {
            SttRoute::Local => "local",
            SttRoute::Cloud => "cloud",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TranscriptionOutcome {
    pub transcript: Transcript,
    /// Wpis już zapisany w `ContextStore`, zwrócony też po to, by UI mogło
    /// pokazać proweniencję bez ponownego odczytu.
    pub context_item: ContextItem,
    pub route: SttRoute,
}

pub struct SttRuntime<L: SpeechToText> {
    local: L,
    cloud: Option<Arc<dyn CloudSpeechToText>>,
    policy: Arc<PolicyEngine>,
    context: Arc<dyn ContextStore>,
    config: SttRuntimeConfig,
}

impl<L: SpeechToText> SttRuntime<L> {
    pub fn new(
        local: L,
        policy: Arc<PolicyEngine>,
        context: Arc<dyn ContextStore>,
        config: SttRuntimeConfig,
    ) -> Self {
        Self {
            local,
            cloud: None,
            policy,
            context,
            config,
        }
    }

    /// Rejestruje jeden konkretny adapter chmurowy. To konfiguracja hosta,
    /// nie wybór z UI; runtime nigdy nie podmienia go na innego po błędzie.
    pub fn with_cloud(mut self, cloud: Arc<dyn CloudSpeechToText>) -> Self {
        self.cloud = Some(cloud);
        self
    }

    pub fn config(&self) -> &SttRuntimeConfig {
        &self.config
    }

    /// Domyślna ścieżka: lokalny provider. Nie wymaga żadnej zgody, bo nic nie
    /// opuszcza komputera. Błąd lokalnego backendu jest błędem końcowym.
    pub async fn transcribe(&self, job: SttJob) -> Result<TranscriptionOutcome, RuntimeError> {
        let SttJob {
            item_id,
            scope,
            audio,
            now_unix_ms,
        } = job;

        audio.validate()?;
        let provider = self.local.provider();
        let transcript = self
            .local
            .transcribe(TranscriptionRequest::local(audio))
            .await?;

        validate_transcript(
            &transcript,
            TranscriptionLocation::Local,
            provider,
            "lokalny",
        )?;

        self.persist(item_id, scope, transcript, SttRoute::Local, now_unix_ms)
    }

    /// Ścieżka planu: chmura jest dozwolona tylko wtedy, gdy runtime ma ją
    /// włączoną, plan jest dokładnie krokiem `stt.cloud` R3, a zgoda jest
    /// krytyczna i aktualna. Każdy brak = `CloudDenied`, nigdy fallback.
    pub async fn execute_approved_cloud_step(
        &self,
        plan: &Plan,
        approval: &Approval,
        job: SttJob,
    ) -> Result<TranscriptionOutcome, RuntimeError> {
        let cloud = self.cloud_adapter()?;
        validate_cloud_plan(plan, cloud.provider())?;
        let SttJob {
            item_id,
            scope,
            audio,
            now_unix_ms,
        } = job;

        self.policy
            .require_approval(plan, Some(approval), now_unix_ms)
            .map_err(|error| {
                RuntimeError::CloudDenied(format!("polityka odrzuciła krok chmurowy: {error}"))
            })?;

        audio.validate()?;
        let permit = CloudTranscriptionPermit::authorize(plan, approval, &audio, now_unix_ms)
            .map_err(|error| {
                RuntimeError::CloudDenied(format!("brak ważnej zgody na audio: {error}"))
            })?;

        let request = AuthorizedCloudTranscriptionRequest::new(plan, audio, permit)?;
        let transcript = cloud.transcribe(request).await?;

        validate_transcript(
            &transcript,
            TranscriptionLocation::Cloud,
            cloud.provider(),
            "chmurowy",
        )?;

        self.persist(item_id, scope, transcript, SttRoute::Cloud, now_unix_ms)
    }

    fn cloud_adapter(&self) -> Result<&Arc<dyn CloudSpeechToText>, RuntimeError> {
        if self.config.cloud_egress != CloudEgressPolicy::CriticalApprovalOnly {
            return Err(RuntimeError::CloudDenied(
                "chmura jest wyłączona w polityce runtime".into(),
            ));
        }
        self.cloud.as_ref().ok_or_else(|| {
            RuntimeError::CloudDenied(
                "brak skonfigurowanego dostawcy chmurowego; runtime nie wybiera go po cichu".into(),
            )
        })
    }

    fn persist(
        &self,
        item_id: ContextItemId,
        scope: ContextScope,
        transcript: Transcript,
        route: SttRoute,
        now_unix_ms: UnixMs,
    ) -> Result<TranscriptionOutcome, RuntimeError> {
        let expected_route = match transcript.location {
            TranscriptionLocation::Local => SttRoute::Local,
            TranscriptionLocation::Cloud => SttRoute::Cloud,
        };
        if expected_route != route {
            return Err(RuntimeError::ProviderContract(format!(
                "trasa `{}` otrzymała transkrypcję oznaczoną jako `{}`",
                route.as_str(),
                transcript.location.as_str()
            )));
        }

        let context_item = transcript_to_context_item(
            item_id,
            scope,
            &transcript,
            self.config.transcript_context,
            now_unix_ms,
        )?;
        self.context.put(context_item.clone())?;

        Ok(TranscriptionOutcome {
            transcript,
            context_item,
            route,
        })
    }
}

fn validate_cloud_plan(plan: &Plan, provider: &str) -> Result<(), RuntimeError> {
    if plan.steps.len() != 1 {
        return Err(RuntimeError::CloudDenied(
            "plan chmurowy musi zawierać dokładnie jeden krok STT".into(),
        ));
    }
    let step = &plan.steps[0];
    if step.tool.as_str() != CLOUD_STT_TOOL_ID {
        return Err(RuntimeError::CloudDenied(format!(
            "plan nie jest krokiem `{CLOUD_STT_TOOL_ID}`"
        )));
    }
    if !step.risk.is_critical() || !plan.risk.is_critical() {
        return Err(RuntimeError::CloudDenied(
            "wysyłka audio wymaga planu R3".into(),
        ));
    }

    // Pole `provider` w planie jest tylko do audytu. Jeśli istnieje, musi
    // wskazywać dokładnie ten adapter, który skonfigurował runtime.
    if let Some(value) = step.args.get("provider") {
        let matches = value.as_str().is_some_and(|name| name == provider);
        if !matches {
            return Err(RuntimeError::CloudDenied(format!(
                "plan wskazuje dostawcę innego niż skonfigurowany `{provider}`"
            )));
        }
    }

    Ok(())
}

fn validate_transcript(
    transcript: &Transcript,
    location: TranscriptionLocation,
    provider: &str,
    route_label: &str,
) -> Result<(), RuntimeError> {
    if transcript.location != location {
        return Err(RuntimeError::ProviderContract(format!(
            "{route_label} adapter zwrócił `{}` zamiast `{}`",
            transcript.location.as_str(),
            location.as_str()
        )));
    }
    if transcript.provider != provider {
        return Err(RuntimeError::ProviderContract(format!(
            "{route_label} adapter zwrócił proweniencję `{}` zamiast `{provider}`",
            transcript.provider
        )));
    }
    if !transcript.is_final {
        return Err(RuntimeError::ProviderContract(format!(
            "{route_label} adapter zwrócił transkrypcję częściową"
        )));
    }
    if transcript.text.trim().is_empty() {
        return Err(RuntimeError::ProviderContract(format!(
            "{route_label} adapter zwrócił pustą transkrypcję"
        )));
    }
    Ok(())
}
