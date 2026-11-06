use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::capability::{Capability, RiskLevel};
use crate::error::DomainError;
use crate::UnixMs;

/// Stabilny identyfikator narzędzia, np. `files.scan`.
///
/// Świadomie restrykcyjny: tylko małe litery ASCII, cyfry, `.`, `_`, `-`,
/// pierwszy znak musi być literą. Dzięki temu `id` można bezpiecznie użyć
/// jako klucza w konfiguracji, logach i nazwie wpisu audytu.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ToolId(String);

impl ToolId {
    pub const MAX_LEN: usize = 64;

    pub fn parse(raw: &str) -> Result<Self, DomainError> {
        let valid = !raw.is_empty()
            && raw.len() <= Self::MAX_LEN
            && raw.chars().next().is_some_and(|c| c.is_ascii_lowercase())
            && raw.chars().all(|c| {
                c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-')
            });
        if valid {
            Ok(Self(raw.to_string()))
        } else {
            Err(DomainError::InvalidToolId(raw.to_string()))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ToolId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Pełna deklaracja narzędzia (§13).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDescriptor {
    pub id: ToolId,
    pub schema_version: u32,
    pub description: String,
    /// JSON Schema wejścia — walidowana deterministycznie przez backend.
    pub input_schema: Value,
    /// JSON Schema wyjścia.
    pub output_schema: Value,
    pub capabilities: Vec<Capability>,
    pub risk: RiskLevel,
    pub default_timeout_ms: u64,
    pub undo_support: bool,
    /// Nazwy pól w `args`, które zawierają ścieżki. Policy engine sprawdza
    /// wyłącznie te pola — brak tej listy oznacza, że narzędzie nie dotyka
    /// systemu plików albo deklaracja jest niekompletna.
    #[serde(default)]
    pub path_arguments: Vec<String>,
}

impl ToolDescriptor {
    pub fn has_capability(&self, kind: &str) -> bool {
        self.capabilities.iter().any(|cap| cap.kind() == kind)
    }

    /// Zakresy plikowe zadeklarowane przez narzędzie — niezależnie od tego,
    /// czy to odczyt, czy zapis.
    pub fn filesystem_scopes(&self) -> impl Iterator<Item = &crate::capability::PathScope> {
        self.capabilities
            .iter()
            .filter_map(|cap| cap.filesystem_scope())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PreviewAction {
    Read,
    Inspect,
    MoveToStaging,
    Write,
    Delete,
    NetworkUpload,
}

impl PreviewAction {
    /// Czy akcja zmienia cokolwiek na dysku lub na zewnątrz.
    pub fn is_mutating(&self) -> bool {
        !matches!(self, PreviewAction::Read | PreviewAction::Inspect)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PreviewItem {
    pub path: PathBuf,
    pub action: PreviewAction,
    pub reason: String,
    pub bytes: u64,
}

/// Konkretny podgląd skutków — manifest lub diff, nigdy sam tekst modelu (§13).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Preview {
    pub tool: ToolId,
    pub risk: RiskLevel,
    pub summary: String,
    pub items: Vec<PreviewItem>,
    pub estimated_reclaimed_bytes: u64,
    pub undo_possible: bool,
}

impl Preview {
    pub fn mutates_anything(&self) -> bool {
        self.items.iter().any(|item| item.action.is_mutating())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolOutput {
    pub summary: String,
    pub changed_paths: Vec<PathBuf>,
    pub reclaimed_bytes: u64,
    pub undo: Option<UndoRecord>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UndoState {
    Pending,
    Applied,
    Expired,
    Failed,
}

/// Wpis dziennika cofania. `payload` zawiera wyłącznie dane potrzebne do
/// odwrócenia operacji (np. para ścieżek źródło → staging).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UndoRecord {
    pub id: String,
    pub task_id: String,
    pub operation: String,
    pub payload: Value,
    pub expires_at_unix_ms: Option<UnixMs>,
    pub state: UndoState,
}

impl UndoRecord {
    pub fn is_expired(&self, now: UnixMs) -> bool {
        self.expires_at_unix_ms.is_some_and(|at| now >= at)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn accepts_typical_tool_ids() {
        for raw in ["files.scan", "files.move_to_staging", "sys-info", "audio2"] {
            assert!(ToolId::parse(raw).is_ok(), "{raw} powinno być poprawne");
        }
    }

    #[test]
    fn rejects_unsafe_tool_ids() {
        for raw in [
            "",
            "Files.Scan",
            "1files",
            "../etc/passwd",
            "files scan",
            "files::scan",
            "zażółć",
        ] {
            assert!(ToolId::parse(raw).is_err(), "{raw} powinno być odrzucone");
        }
    }

    #[test]
    fn rejects_overlong_tool_id() {
        let raw = "a".repeat(ToolId::MAX_LEN + 1);
        assert!(ToolId::parse(&raw).is_err());
    }

    #[test]
    fn descriptor_roundtrips_through_json() {
        let descriptor = ToolDescriptor {
            id: ToolId::parse("files.scan").unwrap(),
            schema_version: 1,
            description: "Skan katalogu bez zapisu".into(),
            input_schema: json!({"type": "object", "required": ["path"]}),
            output_schema: json!({"type": "object"}),
            capabilities: vec![Capability::FilesystemRead {
                scope: crate::capability::PathScope::new("/home/jaro"),
            }],
            risk: RiskLevel::R0,
            default_timeout_ms: 30_000,
            undo_support: false,
            path_arguments: vec!["path".into()],
        };
        let encoded = serde_json::to_string(&descriptor).unwrap();
        let decoded: ToolDescriptor = serde_json::from_str(&encoded).unwrap();
        assert_eq!(descriptor, decoded);
    }

    #[test]
    fn preview_reports_mutation_only_for_changing_actions() {
        let preview = Preview {
            tool: ToolId::parse("files.scan").unwrap(),
            risk: RiskLevel::R0,
            summary: "3 pliki".into(),
            items: vec![PreviewItem {
                path: PathBuf::from("/x/a.txt"),
                action: PreviewAction::Read,
                reason: "starszy niż 90 dni".into(),
                bytes: 10,
            }],
            estimated_reclaimed_bytes: 0,
            undo_possible: false,
        };
        assert!(!preview.mutates_anything());

        let mut mutating = preview;
        mutating.items[0].action = PreviewAction::MoveToStaging;
        assert!(mutating.mutates_anything());
    }

    #[test]
    fn undo_record_expires() {
        let record = UndoRecord {
            id: "u1".into(),
            task_id: "t1".into(),
            operation: "move_to_staging".into(),
            payload: json!({"from": "/a", "to": "/staging/a"}),
            expires_at_unix_ms: Some(100),
            state: UndoState::Pending,
        };
        assert!(record.is_expired(100));
        assert!(!record.is_expired(99));
    }
}
