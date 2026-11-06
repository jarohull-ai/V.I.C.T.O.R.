//! Test end-to-end z atrapami obu providerów. Sprawdza cztery rzeczy naraz:
//! lokalna ścieżka działa bez zgody, chmura bez `CriticalApproval` kończy się
//! `CloudDenied`, błąd lokalny nie uruchamia chmury, a proweniencja trafia do
//! `victor-context`.

use std::path::Path;
use std::sync::Arc;

use serde_json::json;
use victor_context::{
    ContextContent, ContextItemId, ContextScope, ContextSource, ContextStore, InMemoryContextStore,
};
use victor_domain::{Plan, PlanStep, RiskLevel, ToolId};
use victor_integrations::{AudioChunk, AudioFormat, IntegrationError};
use victor_policy::{CriticalActionPolicy, PolicyConfig, PolicyEngine};
use victor_runtime::testing::{FailingSpeechToText, FakeCloudSpeechToText};
use victor_runtime::{
    CloudEgressPolicy, RuntimeError, SttJob, SttRoute, SttRuntime, SttRuntimeConfig,
    CLOUD_STT_TOOL_ID,
};

use victor_integrations::testing::FakeSpeechToText;

fn scope() -> ContextScope {
    ContextScope::project("victor").unwrap()
}

fn audio() -> AudioChunk {
    AudioChunk {
        bytes: vec![1, 2, 3, 4, 5, 6],
        format: AudioFormat::Wav,
        sample_rate_hz: 16_000,
        channels: 1,
    }
}

fn job(id: &str) -> SttJob {
    SttJob {
        item_id: ContextItemId::parse(id).unwrap(),
        scope: scope(),
        audio: audio(),
        now_unix_ms: 1_100,
    }
}

fn cloud_plan() -> Plan {
    Plan::new(
        "task-voice",
        1,
        "transcribe_audio",
        vec![PlanStep::new(
            ToolId::parse(CLOUD_STT_TOOL_ID).unwrap(),
            "Wyślij nagranie do skonfigurowanej usługi STT",
            json!({"provider": "fake-cloud"}),
            RiskLevel::R3,
        )],
    )
    .unwrap()
}

fn policy(root: &Path, critical: bool) -> Arc<PolicyEngine> {
    let config = PolicyConfig::new([root.to_path_buf()]).with_critical_actions(if critical {
        CriticalActionPolicy::RequireStrongConfirmation
    } else {
        CriticalActionPolicy::Blocked
    });
    Arc::new(PolicyEngine::new(config).unwrap())
}

struct Harness<L: victor_integrations::SpeechToText> {
    runtime: SttRuntime<L>,
    store: Arc<InMemoryContextStore>,
    cloud: FakeCloudSpeechToText,
    _dir: tempfile::TempDir,
}

fn harness_with_local<L: victor_integrations::SpeechToText>(
    local: L,
    cloud: FakeCloudSpeechToText,
    config: SttRuntimeConfig,
    critical_policy: bool,
) -> Harness<L> {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(InMemoryContextStore::new());
    let runtime = SttRuntime::new(
        local,
        policy(dir.path(), critical_policy),
        store.clone(),
        config,
    )
    .with_cloud(Arc::new(cloud.clone()));
    Harness {
        runtime,
        store,
        cloud,
        _dir: dir,
    }
}

fn configured_runtime<L: victor_integrations::SpeechToText>(
    local: L,
    cloud: FakeCloudSpeechToText,
    critical_policy: bool,
) -> Harness<L> {
    harness_with_local(
        local,
        cloud,
        SttRuntimeConfig {
            cloud_egress: CloudEgressPolicy::CriticalApprovalOnly,
            ..SttRuntimeConfig::default()
        },
        critical_policy,
    )
}

#[tokio::test]
async fn local_transcription_succeeds_without_any_consent_and_never_calls_cloud() {
    let harness = configured_runtime(
        FakeSpeechToText::new("otwórz projekt"),
        FakeCloudSpeechToText::new("nie powinno się wydarzyć"),
        true,
    );

    let outcome = harness.runtime.transcribe(job("utt-local")).await.unwrap();

    assert_eq!(outcome.route, SttRoute::Local);
    assert_eq!(outcome.transcript.text, "otwórz projekt");
    assert_eq!(
        outcome.context_item.source,
        ContextSource::local_stt("fake-stt")
    );
    assert_eq!(
        outcome.context_item.content,
        ContextContent::inline("otwórz projekt")
    );
    assert_eq!(harness.cloud.calls(), 0);

    let stored = harness
        .store
        .get(&ContextItemId::parse("utt-local").unwrap(), &scope())
        .unwrap()
        .expect("wpis powinien być w kontekście");
    assert_eq!(stored.source.label, "stt:local:fake-stt");
}

#[tokio::test]
async fn cloud_without_critical_approval_is_denied_before_any_request() {
    let harness = configured_runtime(
        FakeSpeechToText::new("lokalnie"),
        FakeCloudSpeechToText::new("chmura"),
        true,
    );
    let plan = cloud_plan();
    let standard = plan.approve(1_000).unwrap();

    let error = harness
        .runtime
        .execute_approved_cloud_step(&plan, &standard, job("utt-cloud-denied"))
        .await
        .unwrap_err();

    assert!(matches!(error, RuntimeError::CloudDenied(_)), "{error:?}");
    assert_eq!(harness.cloud.calls(), 0);
    assert!(harness
        .store
        .get(&ContextItemId::parse("utt-cloud-denied").unwrap(), &scope())
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn cloud_with_critical_approval_records_cloud_provenance_in_context() {
    let harness = configured_runtime(
        FakeSpeechToText::new("lokalnie"),
        FakeCloudSpeechToText::new("transkrypcja z chmury"),
        true,
    );
    let plan = cloud_plan();
    let critical = plan.approve_critical(1_000).unwrap();

    let outcome = harness
        .runtime
        .execute_approved_cloud_step(&plan, &critical, job("utt-cloud-ok"))
        .await
        .unwrap();

    assert_eq!(outcome.route, SttRoute::Cloud);
    assert_eq!(outcome.transcript.provider, "fake-cloud");
    assert_eq!(
        outcome.context_item.source,
        ContextSource::cloud_stt("fake-cloud")
    );
    assert_eq!(
        outcome.context_item.content,
        ContextContent::inline("transkrypcja z chmury")
    );
    assert_eq!(harness.cloud.calls(), 1);

    let stored = harness
        .store
        .get(&ContextItemId::parse("utt-cloud-ok").unwrap(), &scope())
        .unwrap()
        .expect("wpis chmurowy powinien być w kontekście");
    assert_eq!(stored.source.label, "stt:cloud:fake-cloud");
}

#[tokio::test]
async fn local_backend_error_does_not_trigger_cloud_fallback() {
    let local = FailingSpeechToText::new("model whisper nie wczytał się");
    let harness = configured_runtime(
        local.clone(),
        FakeCloudSpeechToText::new("chmura nie powinna ruszyć"),
        true,
    );

    let error = harness
        .runtime
        .transcribe(job("utt-local-fail"))
        .await
        .unwrap_err();

    match error {
        RuntimeError::Integration(IntegrationError::Provider { provider, .. }) => {
            assert_eq!(provider, "failing-local");
        }
        other => panic!("oczekiwano błędu lokalnego providera, jest {other:?}"),
    }
    assert_eq!(local.calls(), 1);
    assert_eq!(harness.cloud.calls(), 0);
    assert!(harness.store.is_empty());
}

#[tokio::test]
async fn critical_approval_does_not_bypass_disabled_cloud_policy() {
    let harness = harness_with_local(
        FakeSpeechToText::new("lokalnie"),
        FakeCloudSpeechToText::new("chmura"),
        SttRuntimeConfig::default(),
        true,
    );
    let plan = cloud_plan();
    let critical = plan.approve_critical(1_000).unwrap();

    let error = harness
        .runtime
        .execute_approved_cloud_step(&plan, &critical, job("utt-cloud-off"))
        .await
        .unwrap_err();

    assert!(matches!(error, RuntimeError::CloudDenied(_)), "{error:?}");
    assert_eq!(harness.cloud.calls(), 0);
    assert!(harness.store.is_empty());
}

#[tokio::test]
async fn cloud_adapter_must_match_the_provider_recorded_in_the_plan() {
    let harness = configured_runtime(
        FakeSpeechToText::new("lokalnie"),
        FakeCloudSpeechToText::new("chmura"),
        true,
    );
    let plan = Plan::new(
        "task-voice",
        1,
        "transcribe_audio",
        vec![PlanStep::new(
            ToolId::parse(CLOUD_STT_TOOL_ID).unwrap(),
            "Wyślij nagranie",
            json!({"provider": "inny-provider"}),
            RiskLevel::R3,
        )],
    )
    .unwrap();
    let critical = plan.approve_critical(1_000).unwrap();

    let error = harness
        .runtime
        .execute_approved_cloud_step(&plan, &critical, job("utt-wrong-provider"))
        .await
        .unwrap_err();

    assert!(matches!(error, RuntimeError::CloudDenied(_)), "{error:?}");
    assert_eq!(harness.cloud.calls(), 0);
}
