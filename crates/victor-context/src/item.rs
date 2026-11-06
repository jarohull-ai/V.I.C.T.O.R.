use serde::{Deserialize, Serialize};
use victor_domain::UnixMs;

use crate::error::ContextError;
use crate::scope::ContextScope;

pub const MAX_ITEM_ID_LEN: usize = 128;
pub const MAX_INLINE_BYTES: u64 = 256 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ContextItemId(String);

impl ContextItemId {
    pub fn parse(raw: &str) -> Result<Self, ContextError> {
        let valid = !raw.is_empty()
            && raw.len() <= MAX_ITEM_ID_LEN
            && raw
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':'));
        if valid {
            Ok(Self(raw.to_string()))
        } else {
            Err(ContextError::InvalidItemId(raw.to_string()))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ContextItemId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Treść albo referencja. Duże rzeczy (pliki, zrzuty, dokumenty) nie wchodzą
/// do pamięci jako bajty — zostaje URI, rozmiar i opcjonalny hash.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ContextContent {
    Inline {
        text: String,
    },
    Reference {
        uri: String,
        bytes: u64,
        sha256: Option<String>,
    },
}

impl ContextContent {
    pub fn inline(text: impl Into<String>) -> Self {
        Self::Inline { text: text.into() }
    }

    pub fn reference(uri: impl Into<String>, bytes: u64) -> Self {
        Self::Reference {
            uri: uri.into(),
            bytes,
            sha256: None,
        }
    }

    pub fn size_bytes(&self) -> u64 {
        match self {
            ContextContent::Inline { text } => text.len() as u64,
            ContextContent::Reference { bytes, .. } => *bytes,
        }
    }

    pub fn render(&self) -> String {
        match self {
            ContextContent::Inline { text } => text.clone(),
            ContextContent::Reference { uri, bytes, sha256 } => match sha256 {
                Some(hash) => format!("ref: {uri} ({bytes} B, sha256: {hash})"),
                None => format!("ref: {uri} ({bytes} B)"),
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    User,
    Assistant,
    Tool,
    Model,
    Imported,
}

impl Origin {
    pub fn as_str(&self) -> &'static str {
        match self {
            Origin::User => "user",
            Origin::Assistant => "assistant",
            Origin::Tool => "tool",
            Origin::Model => "model",
            Origin::Imported => "imported",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextSource {
    pub origin: Origin,
    /// Konkretne źródło, np. `aionui-stt`, `files.scan`, `manual`.
    pub label: String,
}

impl ContextSource {
    pub fn new(origin: Origin, label: impl Into<String>) -> Self {
        Self {
            origin,
            label: label.into(),
        }
    }

    /// Transkrypcja powstała na komputerze. `provider` to np. `whisper.cpp`.
    pub fn local_stt(provider: impl AsRef<str>) -> Self {
        Self::new(Origin::Tool, format!("stt:local:{}", provider.as_ref()))
    }

    /// Transkrypcja wymagała wysłania audio do usługi. `provider` to np. `openai`.
    pub fn cloud_stt(provider: impl AsRef<str>) -> Self {
        Self::new(Origin::Tool, format!("stt:cloud:{}", provider.as_ref()))
    }
}

/// Wrażliwość jest częścią modelu, bo od niej zależy, czy element w ogóle
/// może trafić do promptu i do logów.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Sensitivity {
    Public,
    Internal,
    Sensitive,
    Secret,
}

impl Sensitivity {
    pub fn as_str(&self) -> &'static str {
        match self {
            Sensitivity::Public => "public",
            Sensitivity::Internal => "internal",
            Sensitivity::Sensitive => "sensitive",
            Sensitivity::Secret => "secret",
        }
    }

    /// Sekrety nigdy nie trafiają do widoku bez jawnej zgody.
    pub fn is_secret(&self) -> bool {
        matches!(self, Sensitivity::Secret)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextItemKind {
    Message,
    Note,
    Decision,
    Preference,
    Observation,
    Evidence,
    FileReference,
    ToolResult,
}

impl ContextItemKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            ContextItemKind::Message => "message",
            ContextItemKind::Note => "note",
            ContextItemKind::Decision => "decision",
            ContextItemKind::Preference => "preference",
            ContextItemKind::Observation => "observation",
            ContextItemKind::Evidence => "evidence",
            ContextItemKind::FileReference => "file_reference",
            ContextItemKind::ToolResult => "tool_result",
        }
    }
}

/// Trzy poziomy pamięci, o których mówi plan: ulotna, trwała projektowa i dowody.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryTier {
    Working,
    Durable,
    Evidence,
}

impl MemoryTier {
    pub fn as_str(&self) -> &'static str {
        match self {
            MemoryTier::Working => "working",
            MemoryTier::Durable => "durable",
            MemoryTier::Evidence => "evidence",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Retention {
    KeepForever,
    /// Bezwzględny moment wygaśnięcia.
    Until(UnixMs),
    /// Czas życia liczony od `created_at_unix_ms`.
    TtlFromCreation(u64),
}

impl Retention {
    pub fn expires_at(&self, created_at: UnixMs) -> Option<UnixMs> {
        match self {
            Retention::KeepForever => None,
            Retention::Until(at) => Some(*at),
            Retention::TtlFromCreation(ttl) => created_at.checked_add(*ttl),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextItem {
    pub id: ContextItemId,
    pub scope: ContextScope,
    pub kind: ContextItemKind,
    pub tier: MemoryTier,
    pub content: ContextContent,
    pub source: ContextSource,
    pub sensitivity: Sensitivity,
    pub created_at_unix_ms: UnixMs,
    pub updated_at_unix_ms: UnixMs,
    pub retention: Retention,
}

impl ContextItem {
    pub fn new(
        id: ContextItemId,
        scope: ContextScope,
        kind: ContextItemKind,
        tier: MemoryTier,
        content: ContextContent,
        source: ContextSource,
        created_at_unix_ms: UnixMs,
    ) -> Result<Self, ContextError> {
        if let ContextContent::Reference { uri, .. } = &content {
            if uri.trim().is_empty() {
                return Err(ContextError::InvalidItem(
                    id.to_string(),
                    "pusta referencja URI".into(),
                ));
            }
        }
        if let ContextContent::Inline { text } = &content {
            if text.len() as u64 > MAX_INLINE_BYTES {
                return Err(ContextError::InvalidItem(
                    id.to_string(),
                    format!("treść inline przekracza {MAX_INLINE_BYTES} B — użyj referencji"),
                ));
            }
        }

        Ok(Self {
            id,
            scope,
            kind,
            tier,
            content,
            source,
            sensitivity: Sensitivity::Internal,
            created_at_unix_ms,
            updated_at_unix_ms: created_at_unix_ms,
            retention: Retention::KeepForever,
        })
    }

    pub fn with_sensitivity(mut self, sensitivity: Sensitivity) -> Self {
        self.sensitivity = sensitivity;
        self
    }

    pub fn with_retention(mut self, retention: Retention) -> Result<Self, ContextError> {
        match retention {
            Retention::Until(at) if at <= self.created_at_unix_ms => {
                Err(ContextError::InvalidItem(
                    self.id.to_string(),
                    "retencja wygasa przed utworzeniem".into(),
                ))
            }
            Retention::TtlFromCreation(0) => Err(ContextError::InvalidItem(
                self.id.to_string(),
                "TTL retencji nie może być zerowe".into(),
            )),
            other => {
                self.retention = other;
                Ok(self)
            }
        }
    }

    pub fn expires_at(&self) -> Option<UnixMs> {
        self.retention.expires_at(self.created_at_unix_ms)
    }

    pub fn is_expired(&self, now: UnixMs) -> bool {
        self.expires_at().is_some_and(|at| now >= at)
    }

    pub fn size_bytes(&self) -> u64 {
        self.content.size_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope() -> ContextScope {
        ContextScope::project("victor").unwrap()
    }

    fn item(id: &str) -> ContextItem {
        ContextItem::new(
            ContextItemId::parse(id).unwrap(),
            scope(),
            ContextItemKind::Note,
            MemoryTier::Durable,
            ContextContent::inline("treść"),
            ContextSource::new(Origin::User, "manual"),
            1_000,
        )
        .unwrap()
    }

    #[test]
    fn rejects_invalid_item_ids() {
        for raw in ["", "a b", "../x", "ząb"] {
            assert!(ContextItemId::parse(raw).is_err(), "{raw}");
        }
    }

    #[test]
    fn inline_and_reference_report_sizes() {
        assert_eq!(ContextContent::inline("abcd").size_bytes(), 4);
        assert_eq!(ContextContent::reference("file:///x", 99).size_bytes(), 99);
    }

    #[test]
    fn transcription_source_records_local_or_cloud() {
        let local = ContextSource::local_stt("whisper.cpp");
        assert_eq!(local.origin, Origin::Tool);
        assert_eq!(local.label, "stt:local:whisper.cpp");

        let cloud = ContextSource::cloud_stt("openai");
        assert_eq!(cloud.origin, Origin::Tool);
        assert_eq!(cloud.label, "stt:cloud:openai");
        assert_ne!(local.label, cloud.label);
    }

    #[test]
    fn rejects_empty_reference() {
        let error = ContextItem::new(
            ContextItemId::parse("i1").unwrap(),
            scope(),
            ContextItemKind::FileReference,
            MemoryTier::Evidence,
            ContextContent::reference("   ", 1),
            ContextSource::new(Origin::Tool, "files.scan"),
            0,
        )
        .unwrap_err();
        assert!(matches!(error, ContextError::InvalidItem(_, _)));
    }

    #[test]
    fn rejects_oversized_inline_content() {
        let big = "x".repeat(MAX_INLINE_BYTES as usize + 1);
        let error = ContextItem::new(
            ContextItemId::parse("i1").unwrap(),
            scope(),
            ContextItemKind::Note,
            MemoryTier::Working,
            ContextContent::inline(big),
            ContextSource::new(Origin::User, "manual"),
            0,
        )
        .unwrap_err();
        assert!(matches!(error, ContextError::InvalidItem(_, _)));
    }

    #[test]
    fn retention_is_validated() {
        let error = item("a")
            .with_retention(Retention::Until(1_000))
            .unwrap_err();
        assert!(matches!(error, ContextError::InvalidItem(_, _)));

        let error = item("b")
            .with_retention(Retention::TtlFromCreation(0))
            .unwrap_err();
        assert!(matches!(error, ContextError::InvalidItem(_, _)));
    }

    #[test]
    fn expiry_uses_creation_time() {
        let ttl = item("a")
            .with_retention(Retention::TtlFromCreation(50))
            .unwrap();
        assert_eq!(ttl.expires_at(), Some(1_050));
        assert!(!ttl.is_expired(1_049));
        assert!(ttl.is_expired(1_050));
    }

    #[test]
    fn keep_forever_never_expires() {
        let forever = item("a");
        assert_eq!(forever.expires_at(), None);
        assert!(!forever.is_expired(u64::MAX));
    }
}
