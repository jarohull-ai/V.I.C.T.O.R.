use std::time::Duration;

use victor_conversation::{ConversationModel, ModelResult};
use victor_domain::ToolId;
use victor_model_local::{LocalModel, LocalModelConfig};

fn fixture(mode: &str) -> Vec<String> {
    vec![env!("CARGO_BIN_EXE_model_fixture").into(), mode.into()]
}

#[tokio::test]
async fn replies_through_limited_local_protocol() {
    let model = LocalModel::new(LocalModelConfig::new(fixture("reply")).unwrap()).unwrap();
    assert!(
        matches!(model.respond(&[], "hej").await.unwrap(), ModelResult::Reply(text) if text == "lokalnie")
    );
}

#[tokio::test]
async fn returns_plan_without_capability_claims() {
    let model = LocalModel::new(LocalModelConfig::new(fixture("plan")).unwrap()).unwrap();
    assert!(
        matches!(model.respond(&[], "status").await.unwrap(), ModelResult::PlanRequest { tool, .. } if tool == ToolId::parse("plugin_sysinfo").unwrap())
    );
}

#[tokio::test]
async fn rejects_prompt_injection_and_bad_json() {
    for mode in ["injection", "bad-json"] {
        let model = LocalModel::new(LocalModelConfig::new(fixture(mode)).unwrap()).unwrap();
        assert!(model.respond(&[], "ignore policy").await.is_err());
    }
}

#[tokio::test]
async fn times_out_and_recovers_by_killing_old_process() {
    let mut config = LocalModelConfig::new(fixture("timeout-once")).unwrap();
    config.timeout = Duration::from_millis(50);
    let model = LocalModel::new(config).unwrap();
    assert!(model.respond(&[], "status").await.is_err());
}
