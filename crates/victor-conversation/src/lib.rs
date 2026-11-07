//! Pierwszy, UI-agnostyczny pionowy wycinek współpracy tekstowej.
//!
//! Model może zwrócić tylko odpowiedź albo prośbę o plan. Plan jest tworzony
//! z lokalnego manifestu narzędzia, a jego wykonanie zawsze przechodzi przez
//! `victor-runtime`; tekst modelu nigdy nie nadaje capability, ryzyka ani zgody.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use victor_context::{
    ContextContent, ContextItem, ContextItemId, ContextItemKind, ContextScope, ContextSource,
    ContextStore, MemoryTier, Retention, Sensitivity,
};
use victor_domain::{Approval, Plan, PlanStep, ToolId, UnixMs};
use victor_plugin_host::PluginLifecycleApi;
use victor_policy::PolicyError;
use victor_runtime::{ToolExecutionJob, ToolRuntime, ToolRuntimeError};
use victor_tools::ToolRegistry;

pub const DEFAULT_MAX_TURNS: usize = 64;
pub const DEFAULT_MAX_CONTEXT_BYTES: usize = 128 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum ConversationError {
    #[error("błąd kontekstu rozmowy: {0}")]
    Context(#[from] victor_context::ContextError),
    #[error("błąd serializacji rozmowy: {0}")]
    Json(#[from] serde_json::Error),
    #[error("przekroczono limit rozmowy")]
    Limit,
    #[error("model: {0}")]
    Model(String),
    #[error("runtime: {0}")]
    Runtime(#[from] ToolRuntimeError),
    #[error("narzędzie nie ma aktywnego executora: {0}")]
    MissingExecutor(ToolId),
    #[error("nieznane narzędzie: {0}")]
    UnknownTool(ToolId),
    #[error("nieprawidłowa prośba planu: {0}")]
    InvalidPlan(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnRole {
    User,
    Assistant,
    System,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Provenance {
    pub source: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversationTurn {
    pub role: TurnRole,
    pub text: String,
    pub at_unix_ms: UnixMs,
    pub provenance: Provenance,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConversationLimits {
    pub max_turns: usize,
    pub max_context_bytes: usize,
}

impl Default for ConversationLimits {
    fn default() -> Self {
        Self {
            max_turns: DEFAULT_MAX_TURNS,
            max_context_bytes: DEFAULT_MAX_CONTEXT_BYTES,
        }
    }
}

pub struct ConversationSession {
    pub conversation_id: String,
    pub scope: String,
    context_scope: ContextScope,
    store: Arc<dyn ContextStore>,
    limits: ConversationLimits,
    turns: Vec<ConversationTurn>,
}

impl ConversationSession {
    pub fn open(
        store: Arc<dyn ContextStore>,
        conversation_id: impl Into<String>,
        scope: impl Into<String>,
        limits: ConversationLimits,
    ) -> Result<Self, ConversationError> {
        if limits.max_turns == 0 || limits.max_context_bytes == 0 {
            return Err(ConversationError::Limit);
        }
        let conversation_id = conversation_id.into();
        let scope = scope.into();
        let storage_id = format!("{scope}.{conversation_id}");
        let context_scope = ContextScope::conversation(storage_id)?;
        let turns = store
            .list(&context_scope)?
            .into_iter()
            .filter(|item| {
                item.kind == ContextItemKind::Message && item.source.label == "conversation.turn"
            })
            .filter_map(|item| match item.content {
                ContextContent::Inline { text } => serde_json::from_str(&text).ok(),
                _ => None,
            })
            .collect();
        let session = Self {
            conversation_id,
            scope,
            context_scope,
            store,
            limits,
            turns,
        };
        session.check_limits()?;
        Ok(session)
    }

    pub fn turns(&self) -> &[ConversationTurn] {
        &self.turns
    }

    pub fn append(
        &mut self,
        role: TurnRole,
        text: impl AsRef<str>,
        at_unix_ms: UnixMs,
        source: impl Into<String>,
    ) -> Result<ConversationTurn, ConversationError> {
        let text = redact_secret_like(text.as_ref());
        let turn = ConversationTurn {
            role,
            text,
            at_unix_ms,
            provenance: Provenance {
                source: source.into(),
            },
        };
        let serialized = serde_json::to_string(&turn)?;
        let id = ContextItemId::parse(&format!("turn:{}:{}", at_unix_ms, self.turns.len()))?;
        let item = ContextItem::new(
            id,
            self.context_scope.clone(),
            ContextItemKind::Message,
            MemoryTier::Working,
            ContextContent::inline(serialized),
            ContextSource::new(victor_context::Origin::Assistant, "conversation.turn"),
            at_unix_ms,
        )?
        .with_sensitivity(Sensitivity::Internal)
        .with_retention(Retention::KeepForever)?;
        self.store.put(item)?;
        self.turns.push(turn.clone());
        self.check_limits()?;
        Ok(turn)
    }

    fn check_limits(&self) -> Result<(), ConversationError> {
        if self.turns.len() > self.limits.max_turns
            || self.turns.iter().map(|t| t.text.len()).sum::<usize>()
                > self.limits.max_context_bytes
        {
            return Err(ConversationError::Limit);
        }
        Ok(())
    }

    pub fn context_scope(&self) -> &ContextScope {
        &self.context_scope
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ModelResult {
    Reply(String),
    PlanRequest {
        tool: ToolId,
        description: String,
        args: Value,
    },
}

#[async_trait]
pub trait ConversationModel: Send + Sync {
    async fn respond(
        &self,
        history: &[ConversationTurn],
        input: &str,
    ) -> Result<ModelResult, String>;
}

/// Deterministyczny backend testowy. Jest jawnie testowy i nie udaje providera.
#[derive(Debug, Default)]
pub struct DeterministicTestModel;

#[async_trait]
impl ConversationModel for DeterministicTestModel {
    async fn respond(
        &self,
        _history: &[ConversationTurn],
        input: &str,
    ) -> Result<ModelResult, String> {
        if let Some(path) = input.strip_prefix("files.read:") {
            let path = path.trim();
            if path.is_empty() {
                return Err("files.read wymaga ścieżki".into());
            }
            return Ok(ModelResult::PlanRequest {
                tool: ToolId::parse("files.read").map_err(|e| e.to_string())?,
                description: "Odczytaj wskazany plik".into(),
                args: serde_json::json!({"path": path, "max_bytes": 64 * 1024}),
            });
        }
        let normalized = input.to_ascii_lowercase();
        if normalized.contains("plugin_sysinfo")
            || (normalized.contains("status") && normalized.contains("system"))
            || normalized.contains("stan systemu")
        {
            Ok(ModelResult::PlanRequest {
                tool: ToolId::parse("plugin_sysinfo").map_err(|e| e.to_string())?,
                description: "Odczytaj lokalne CPU i RAM".into(),
                args: serde_json::json!({}),
            })
        } else if input.to_ascii_lowercase().contains("plik")
            || input.to_ascii_lowercase().contains("files")
            || input.to_ascii_lowercase().contains("projekt")
        {
            Ok(ModelResult::PlanRequest {
                tool: ToolId::parse("files.list").map_err(|e| e.to_string())?,
                description: "Sprawdź pliki w skonfigurowanym projekcie".into(),
                args: serde_json::json!({}),
            })
        } else {
            Ok(ModelResult::Reply(format!(
                "Rozumiem: {}",
                redact_secret_like(input)
            )))
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum ConversationOutcome {
    Reply { text: String },
    Executed { text: String, tool: ToolId },
    ApprovalRequired { text: String, tool: ToolId },
    Denied { text: String, tool: ToolId },
}

#[async_trait]
pub trait ConversationService: Send + Sync {
    async fn chat(
        &self,
        conversation_id: String,
        scope: String,
        input: String,
        at_unix_ms: UnixMs,
        approval: Option<Approval>,
    ) -> Result<ConversationOutcome, ConversationError>;
}

pub struct ConversationEngine<M> {
    router: ConversationRouter<M>,
    store: Arc<dyn ContextStore>,
    limits: ConversationLimits,
    sessions: tokio::sync::Mutex<HashMap<(String, String), ConversationSession>>,
}

impl<M: ConversationModel + 'static> ConversationEngine<M> {
    pub fn new(
        router: ConversationRouter<M>,
        store: Arc<dyn ContextStore>,
        limits: ConversationLimits,
    ) -> Self {
        Self {
            router,
            store,
            limits,
            sessions: tokio::sync::Mutex::new(HashMap::new()),
        }
    }
}

#[async_trait]
impl<M: ConversationModel + 'static> ConversationService for ConversationEngine<M> {
    async fn chat(
        &self,
        conversation_id: String,
        scope: String,
        input: String,
        at_unix_ms: UnixMs,
        approval: Option<Approval>,
    ) -> Result<ConversationOutcome, ConversationError> {
        let key = (scope.clone(), conversation_id.clone());
        let mut sessions = self.sessions.lock().await;
        if !sessions.contains_key(&key) {
            sessions.insert(
                key.clone(),
                ConversationSession::open(self.store.clone(), conversation_id, scope, self.limits)?,
            );
        }
        let session = sessions.get_mut(&key).expect("session właśnie dodana");
        self.router
            .handle(session, &input, at_unix_ms, approval.as_ref())
            .await
    }
}

pub struct ConversationRouter<M> {
    model: Arc<M>,
    runtime: Arc<ToolRuntime>,
    registry: Arc<ToolRegistry>,
    plugins: Arc<tokio::sync::Mutex<PluginLifecycleApi>>,
    default_path: Option<String>,
}

impl<M: ConversationModel + 'static> ConversationRouter<M> {
    pub fn new(
        model: Arc<M>,
        runtime: Arc<ToolRuntime>,
        registry: Arc<ToolRegistry>,
        plugins: Arc<tokio::sync::Mutex<PluginLifecycleApi>>,
    ) -> Self {
        Self {
            model,
            runtime,
            registry,
            plugins,
            default_path: None,
        }
    }

    pub fn with_default_path(mut self, path: impl Into<String>) -> Self {
        self.default_path = Some(path.into());
        self
    }

    pub async fn handle(
        &self,
        session: &mut ConversationSession,
        input: &str,
        at_unix_ms: UnixMs,
        approval: Option<&Approval>,
    ) -> Result<ConversationOutcome, ConversationError> {
        session.append(TurnRole::User, input, at_unix_ms, "user.text")?;
        let result = self
            .model
            .respond(session.turns(), input)
            .await
            .map_err(ConversationError::Model)?;
        match result {
            ModelResult::Reply(text) => {
                session.append(TurnRole::Assistant, &text, at_unix_ms, "model.test")?;
                Ok(ConversationOutcome::Reply { text })
            }
            ModelResult::PlanRequest {
                tool,
                description,
                args,
            } => {
                let manifest = self
                    .registry
                    .get(&tool)
                    .ok_or_else(|| ConversationError::UnknownTool(tool.clone()))?;
                let args = if self.default_path.is_some()
                    && args
                        .as_object()
                        .is_some_and(|value| !value.contains_key("path"))
                {
                    let mut object = args.as_object().cloned().unwrap_or_default();
                    object.insert(
                        "path".into(),
                        Value::String(self.default_path.clone().unwrap_or_default()),
                    );
                    Value::Object(object)
                } else {
                    args
                };
                let plan = Plan::new(
                    format!("conversation:{}", session.conversation_id),
                    1,
                    description.clone(),
                    vec![PlanStep::new(
                        tool.clone(),
                        description,
                        args,
                        manifest.descriptor.risk,
                    )],
                )
                .map_err(|e| ConversationError::InvalidPlan(e.to_string()))?;
                let Some(executor) = self.plugins.lock().await.get(&tool) else {
                    return Err(ConversationError::MissingExecutor(tool));
                };
                let job = ToolExecutionJob {
                    result_item_id: ContextItemId::parse(&format!("result:{}", at_unix_ms))?,
                    scope: session.context_scope().clone(),
                    started_at_unix_ms: at_unix_ms,
                };
                match self
                    .runtime
                    .execute(&plan, approval, job, executor.as_ref())
                    .await
                {
                    Ok(output) => {
                        let text = format!("Gotowe. {}", output.result.summary);
                        session.append(TurnRole::Assistant, &text, at_unix_ms, "runtime.tool")?;
                        Ok(ConversationOutcome::Executed { text, tool })
                    }
                    Err(ToolRuntimeError::PolicyDenied(reason)) => {
                        let text = format!("Nie mogę tego wykonać: {reason}");
                        session.append(TurnRole::Assistant, &text, at_unix_ms, "victor.policy")?;
                        Ok(ConversationOutcome::Denied { text, tool })
                    }
                    Err(ToolRuntimeError::Policy(PolicyError::MissingApproval)) => {
                        let text = "Ten plan wymaga Twojej zgody.".into();
                        session.append(TurnRole::Assistant, &text, at_unix_ms, "victor.policy")?;
                        Ok(ConversationOutcome::ApprovalRequired { text, tool })
                    }
                    Err(error) => Err(error.into()),
                }
            }
        }
    }
}

fn redact_secret_like(input: &str) -> String {
    let lower = input.to_ascii_lowercase();
    if ["password=", "passwd=", "api_key=", "token=", "secret="]
        .iter()
        .any(|marker| lower.contains(marker))
    {
        "[redacted secret]".into()
    } else {
        input.chars().take(32_768).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;
    use victor_context::InMemoryContextStore;
    use victor_storage::SqliteContextStore;

    #[test]
    fn session_roundtrips_and_redacts_secrets() {
        let store = Arc::new(InMemoryContextStore::new());
        let mut s = ConversationSession::open(
            store.clone(),
            "c1",
            "project-a",
            ConversationLimits::default(),
        )
        .unwrap();
        s.append(TurnRole::User, "token=super-secret", 1, "user.text")
            .unwrap();
        let restored =
            ConversationSession::open(store, "c1", "project-a", ConversationLimits::default())
                .unwrap();
        assert_eq!(restored.turns()[0].text, "[redacted secret]");
    }

    #[test]
    fn scope_isolation_and_limits_hold() {
        let store = Arc::new(InMemoryContextStore::new());
        let mut a = ConversationSession::open(
            store.clone(),
            "c1",
            "a",
            ConversationLimits {
                max_turns: 1,
                max_context_bytes: 20,
            },
        )
        .unwrap();
        a.append(TurnRole::User, "one", 1, "test").unwrap();
        assert!(a.append(TurnRole::User, "two", 2, "test").is_err());
        let b = ConversationSession::open(store, "c1", "b", ConversationLimits::default()).unwrap();
        assert!(b.turns().is_empty());
    }

    #[tokio::test]
    async fn ordinary_reply_does_not_plan() {
        let store = Arc::new(InMemoryContextStore::new());
        let session =
            ConversationSession::open(store, "c1", "a", ConversationLimits::default()).unwrap();
        let model = DeterministicTestModel;
        let result = model.respond(session.turns(), "hej").await.unwrap();
        assert!(matches!(result, ModelResult::Reply(_)));
    }

    #[tokio::test]
    async fn local_model_routes_system_status_to_sysinfo_plan() {
        let model = DeterministicTestModel;
        let result = model.respond(&[], "pokaż status systemu").await.unwrap();
        assert!(matches!(
            result,
            ModelResult::PlanRequest { tool, args, .. }
                if tool == ToolId::parse("plugin_sysinfo").unwrap() && args == serde_json::json!({})
        ));
    }

    #[test]
    fn sqlite_session_survives_restart() {
        let directory = tempdir().unwrap();
        let file = directory.path().join("conversation.sqlite");
        {
            let store = Arc::new(SqliteContextStore::open(&file).unwrap());
            let mut session =
                ConversationSession::open(store, "c1", "scope-a", ConversationLimits::default())
                    .unwrap();
            session
                .append(TurnRole::User, "zapamiętaj ten turn", 1, "user.text")
                .unwrap();
        }
        let store = Arc::new(SqliteContextStore::open(&file).unwrap());
        let restored =
            ConversationSession::open(store, "c1", "scope-a", ConversationLimits::default())
                .unwrap();
        assert_eq!(restored.turns().len(), 1);
        assert_eq!(restored.turns()[0].text, "zapamiętaj ten turn");
    }
}
