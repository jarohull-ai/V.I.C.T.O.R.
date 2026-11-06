//! Rejestr i kontrakty wykonywania narzędzi V.I.C.T.O.R.
//!
//! Ten crate nie wykonuje polityki. Przechowuje deklaracje narzędzi, waliduje
//! ich manifesty i argumenty oraz dostarcza typowany punkt wykonania. Decyzja
//! „czy wolno” pozostaje w victor-policy.

use std::collections::BTreeMap;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use victor_context::ContextItem;
use victor_domain::{ToolDescriptor, ToolId, UnixMs};

#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("nieprawidłowy manifest narzędzia: {reason}")]
    InvalidManifest { tool: String, reason: String },
    #[error("narzędzie nie jest zarejestrowane: {0}")]
    NotRegistered(String),
    #[error("argumenty narzędzia są niezgodne ze schematem: {reason}")]
    InvalidInput { tool: String, reason: String },
    #[error("wykonanie narzędzia nie powiodło się: {reason}")]
    Execution { tool: String, reason: String },
    #[error("błąd trwałego storage: {0}")]
    Storage(String),
}

pub const MANIFEST_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResultTier {
    Working,
    Evidence,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolManifest {
    pub manifest_version: u32,
    pub tool_version: String,
    pub result_tier: ResultTier,
    pub descriptor: ToolDescriptor,
}

impl ToolManifest {
    pub fn new(tool_version: impl Into<String>, descriptor: ToolDescriptor) -> Self {
        Self {
            manifest_version: MANIFEST_VERSION,
            tool_version: tool_version.into(),
            result_tier: ResultTier::Working,
            descriptor,
        }
    }

    pub fn with_result_tier(mut self, result_tier: ResultTier) -> Self {
        self.result_tier = result_tier;
        self
    }

    pub fn validate(&self) -> Result<(), ToolError> {
        let tool = self.descriptor.id.as_str().to_string();
        if self.manifest_version != MANIFEST_VERSION {
            return Err(ToolError::InvalidManifest {
                tool,
                reason: format!("nieobsługiwana wersja manifestu {}", self.manifest_version),
            });
        }
        if self.tool_version.trim().is_empty() || self.tool_version.len() > 32 {
            return Err(ToolError::InvalidManifest {
                tool: self.descriptor.id.as_str().into(),
                reason: "tool_version nie może być pusta ani dłuższa niż 32 znaki".into(),
            });
        }
        for (schema, label) in [
            (&self.descriptor.input_schema, "input_schema"),
            (&self.descriptor.output_schema, "output_schema"),
        ] {
            validate_json_schema(schema, label).map_err(|reason| ToolError::InvalidManifest {
                tool: self.descriptor.id.as_str().into(),
                reason,
            })?;
        }
        if self.descriptor.default_timeout_ms == 0 {
            return Err(ToolError::InvalidManifest {
                tool: self.descriptor.id.as_str().into(),
                reason: "default_timeout_ms nie może być zerowy".into(),
            });
        }
        Ok(())
    }
}

#[derive(Debug, Default)]
pub struct ToolRegistry {
    manifests: BTreeMap<String, ToolManifest>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, manifest: ToolManifest) -> Result<(), ToolError> {
        manifest.validate()?;
        let id = manifest.descriptor.id.as_str().to_string();
        if self.manifests.contains_key(&id) {
            return Err(ToolError::InvalidManifest {
                tool: id,
                reason: "narzędzie jest już zarejestrowane".into(),
            });
        }
        self.manifests.insert(id, manifest);
        Ok(())
    }

    pub fn get(&self, id: &ToolId) -> Option<&ToolManifest> {
        self.manifests.get(id.as_str())
    }

    pub fn descriptors(&self) -> Vec<ToolDescriptor> {
        self.manifests
            .values()
            .map(|m| m.descriptor.clone())
            .collect()
    }

    pub fn validate_input(&self, id: &ToolId, args: &Value) -> Result<(), ToolError> {
        let manifest = self
            .get(id)
            .ok_or_else(|| ToolError::NotRegistered(id.to_string()))?;
        validate_value(&manifest.descriptor.input_schema, args).map_err(|reason| {
            ToolError::InvalidInput {
                tool: id.to_string(),
                reason,
            }
        })
    }

    pub fn validate_output(&self, id: &ToolId, output: &Value) -> Result<(), ToolError> {
        let manifest = self
            .get(id)
            .ok_or_else(|| ToolError::NotRegistered(id.to_string()))?;
        validate_value(&manifest.descriptor.output_schema, output).map_err(|reason| {
            ToolError::InvalidInput {
                tool: id.to_string(),
                reason: format!("wynik: {reason}"),
            }
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolExecution {
    pub output: Value,
    pub summary: String,
    pub finished_at_unix_ms: UnixMs,
}

#[async_trait]
pub trait ToolExecutor: Send + Sync {
    fn tool_id(&self) -> &ToolId;
    async fn execute(
        &self,
        args: Value,
        started_at_unix_ms: UnixMs,
    ) -> Result<ToolExecution, ToolError>;
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolResult {
    pub tool: ToolId,
    pub tool_version: String,
    pub output: Value,
    pub summary: String,
    pub started_at_unix_ms: UnixMs,
    pub finished_at_unix_ms: UnixMs,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolAuditEvent {
    pub task_id: String,
    pub tool: ToolId,
    pub tool_version: String,
    pub plan_hash: String,
    pub started_at_unix_ms: UnixMs,
    pub finished_at_unix_ms: UnixMs,
    pub outcome: AuditOutcome,
    pub summary: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditOutcome {
    Succeeded,
    Denied,
    Failed,
}

pub trait AuditSink: Send + Sync {
    fn record(&self, event: ToolAuditEvent) -> Result<(), ToolError>;
}

/// Opcjonalny atomowy backend wyniku narzędzia. Backend może zapisać context i
/// audyt w jednej transakcji; prostsze implementacje używają osobnych sinków.
pub trait ToolResultStore: Send + Sync {
    fn commit_tool_result(&self, item: ContextItem, event: ToolAuditEvent)
        -> Result<(), ToolError>;
}

fn validate_schema(schema: &Value, label: &str) -> Result<(), String> {
    validate_schema_node(schema, label)
}

pub fn validate_json_schema(schema: &Value, label: &str) -> Result<(), String> {
    validate_schema(schema, label)
}

fn validate_schema_node(schema: &Value, label: &str) -> Result<(), String> {
    let object = schema
        .as_object()
        .ok_or_else(|| format!("{label} musi być obiektem JSON"))?;
    const SUPPORTED: &[&str] = &["type", "required", "properties", "items"];
    if let Some(unknown) = object.keys().find(|key| !SUPPORTED.contains(&key.as_str())) {
        return Err(format!(
            "{label} zawiera nieobsługiwane słowo kluczowe {unknown}"
        ));
    }
    match object.get("type") {
        Some(Value::String(kind))
            if matches!(
                kind.as_str(),
                "object" | "array" | "string" | "number" | "integer" | "boolean" | "null"
            ) =>
        {
            Ok(())
        }
        Some(_) => Err(format!("{label}.type musi być obsługiwanym stringiem")),
        None => Err(format!("{label}.type jest wymagane")),
    }?;
    if let Some(required) = object.get("required") {
        let values = required
            .as_array()
            .ok_or_else(|| format!("{label}.required musi być tablicą"))?;
        if values.iter().any(|value| value.as_str().is_none()) {
            return Err(format!("{label}.required musi zawierać wyłącznie stringi"));
        }
    }
    if let Some(properties) = object.get("properties") {
        let properties = properties
            .as_object()
            .ok_or_else(|| format!("{label}.properties musi być obiektem"))?;
        for (name, property) in properties {
            validate_schema_node(property, &format!("{label}.properties.{name}"))?;
        }
    }
    if let Some(items) = object.get("items") {
        validate_schema_node(items, &format!("{label}.items"))?;
    }
    Ok(())
}

fn validate_value(schema: &Value, value: &Value) -> Result<(), String> {
    let object = schema
        .as_object()
        .ok_or_else(|| "schema nie jest obiektem".to_string())?;
    if let Some(Value::String(kind)) = object.get("type") {
        let valid = match kind.as_str() {
            "object" => value.is_object(),
            "array" => value.is_array(),
            "string" => value.is_string(),
            "number" => value.is_number(),
            "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
            "boolean" => value.is_boolean(),
            "null" => value.is_null(),
            _ => false,
        };
        if !valid {
            return Err(format!("oczekiwano typu {kind}"));
        }
    }
    if let (Some(required), Some(map)) = (object.get("required"), value.as_object()) {
        for name in required
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            if !map.contains_key(name) {
                return Err(format!("brak wymaganego pola {name}"));
            }
        }
    }
    if let (Some(properties), Some(map)) = (
        object.get("properties").and_then(Value::as_object),
        value.as_object(),
    ) {
        for (name, property_schema) in properties {
            if let Some(property) = map.get(name) {
                validate_value(property_schema, property)
                    .map_err(|reason| format!("{name}: {reason}"))?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn manifest() -> ToolManifest {
        ToolManifest::new(
            "1.0.0",
            ToolDescriptor {
                id: ToolId::parse("files.scan").unwrap(),
                schema_version: 1,
                description: "Skan".into(),
                input_schema: json!({
                    "type": "object",
                    "required": ["path"],
                    "properties": {"path": {"type": "string"}}
                }),
                output_schema: json!({"type": "object"}),
                capabilities: vec![],
                risk: victor_domain::RiskLevel::R0,
                default_timeout_ms: 1000,
                undo_support: false,
                path_arguments: vec!["path".into()],
            },
        )
    }

    #[test]
    fn registry_rejects_duplicate_and_validates_input() {
        let mut registry = ToolRegistry::new();
        registry.register(manifest()).unwrap();
        assert!(registry.register(manifest()).is_err());
        let id = ToolId::parse("files.scan").unwrap();
        assert!(registry
            .validate_input(&id, &json!({"path": "/tmp"}))
            .is_ok());
        assert!(registry.validate_input(&id, &json!({})).is_err());
    }

    #[test]
    fn manifest_rejects_zero_timeout() {
        let mut value = manifest();
        value.descriptor.default_timeout_ms = 0;
        assert!(value.validate().is_err());
    }

    #[test]
    fn manifest_rejects_unsupported_schema_keywords_in_nested_schema() {
        let mut value = manifest();
        value.descriptor.input_schema = json!({
            "type": "object",
            "properties": {"query": {"type": "string", "minLength": 1}}
        });
        let error = value.validate().unwrap_err().to_string();
        assert!(error.contains("minLength"));
    }
}
