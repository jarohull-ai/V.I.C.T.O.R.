use serde::{Deserialize, Serialize};

use crate::error::ContextError;

pub const MAX_SCOPE_ID_LEN: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScopeKind {
    User,
    Project,
    Conversation,
    Task,
}

impl ScopeKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            ScopeKind::User => "user",
            ScopeKind::Project => "project",
            ScopeKind::Conversation => "conversation",
            ScopeKind::Task => "task",
        }
    }
}

/// Twarda granica kontekstu. Nie ma tu hierarchii „projekt zawiera rozmowę" —
/// `ContextViewRequest` jawnie podaje, które scope'y wolno odczytać. Dzięki
/// temu o tym, czy zadanie widzi kontekst projektu, decyduje runtime i polityka,
/// a nie ukryta logika magazynu.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ContextScope {
    User { id: String },
    Project { id: String },
    Conversation { id: String },
    Task { id: String },
}

impl ContextScope {
    pub fn new(kind: ScopeKind, id: impl Into<String>) -> Result<Self, ContextError> {
        let id = id.into();
        if !is_valid_scope_id(&id) {
            return Err(ContextError::InvalidScope(id));
        }
        Ok(match kind {
            ScopeKind::User => ContextScope::User { id },
            ScopeKind::Project => ContextScope::Project { id },
            ScopeKind::Conversation => ContextScope::Conversation { id },
            ScopeKind::Task => ContextScope::Task { id },
        })
    }

    pub fn user(id: impl Into<String>) -> Result<Self, ContextError> {
        Self::new(ScopeKind::User, id)
    }

    pub fn project(id: impl Into<String>) -> Result<Self, ContextError> {
        Self::new(ScopeKind::Project, id)
    }

    pub fn conversation(id: impl Into<String>) -> Result<Self, ContextError> {
        Self::new(ScopeKind::Conversation, id)
    }

    pub fn task(id: impl Into<String>) -> Result<Self, ContextError> {
        Self::new(ScopeKind::Task, id)
    }

    pub fn id(&self) -> &str {
        match self {
            ContextScope::User { id }
            | ContextScope::Project { id }
            | ContextScope::Conversation { id }
            | ContextScope::Task { id } => id,
        }
    }

    pub fn kind(&self) -> ScopeKind {
        match self {
            ContextScope::User { .. } => ScopeKind::User,
            ContextScope::Project { .. } => ScopeKind::Project,
            ContextScope::Conversation { .. } => ScopeKind::Conversation,
            ContextScope::Task { .. } => ScopeKind::Task,
        }
    }

    pub fn label(&self) -> String {
        format!("{}:{}", self.kind().as_str(), self.id())
    }
}

fn is_valid_scope_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_SCOPE_ID_LEN
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_typical_ids() {
        assert!(ContextScope::project("XcratesX").is_ok());
        assert!(ContextScope::task("task-2026-11-06.1").is_ok());
    }

    #[test]
    fn rejects_empty_or_unsafe_ids() {
        for id in ["", "a b", "../etc", "projekt/1", "ząb"] {
            assert!(ContextScope::project(id).is_err(), "{id}");
        }
    }

    #[test]
    fn label_is_stable() {
        let scope = ContextScope::conversation("conv-1").unwrap();
        assert_eq!(scope.label(), "conversation:conv-1");
        assert_eq!(scope.kind(), ScopeKind::Conversation);
        assert_eq!(scope.id(), "conv-1");
    }
}
