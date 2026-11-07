//! Wersjonowany, ograniczony kontrakt pluginów V.I.C.T.O.R.
//!
//! Transport jest celowo prosty: jedna wiadomość JSON na linię przez stdin/stdout.
//! To framing inspirowany JFP, ale nie kopiuje jego protokołu. Plugin opisuje tylko
//! interfejs narzędzia; capability i risk pochodzą wyłącznie z lokalnego manifestu.

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const PROTOCOL_VERSION: u32 = 1;
pub const MAX_FRAME_BYTES: usize = 256 * 1024;
pub const DEFAULT_REQUEST_TIMEOUT_MS: u64 = 30_000;
pub const DEFAULT_HEARTBEAT_TIMEOUT_MS: u64 = 5_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageKind {
    Hello,
    HelloAck,
    Request,
    Response,
    Heartbeat,
    HeartbeatAck,
    Error,
    Shutdown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginToolBinding {
    pub id: String,
    pub schema_version: u32,
    pub input_schema: Value,
    pub output_schema: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginManifest {
    pub protocol_version: u32,
    pub plugin_id: String,
    pub plugin_version: String,
    pub tools: Vec<PluginToolBinding>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub manifest: PluginManifest,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    pub request_id: String,
    pub tool_id: String,
    pub args: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Response {
    pub request_id: String,
    pub output: Value,
    pub summary: String,
    pub finished_at_unix_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorMessage {
    pub request_id: Option<String>,
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "payload", rename_all = "snake_case")]
pub enum WireMessage {
    Hello(Hello),
    HelloAck { protocol_version: u32 },
    Request(Request),
    Response(Response),
    Heartbeat { nonce: String },
    HeartbeatAck { nonce: String },
    Error(ErrorMessage),
    Shutdown,
}

impl WireMessage {
    pub fn kind(&self) -> MessageKind {
        match self {
            Self::Hello(_) => MessageKind::Hello,
            Self::HelloAck { .. } => MessageKind::HelloAck,
            Self::Request(_) => MessageKind::Request,
            Self::Response(_) => MessageKind::Response,
            Self::Heartbeat { .. } => MessageKind::Heartbeat,
            Self::HeartbeatAck { .. } => MessageKind::HeartbeatAck,
            Self::Error(_) => MessageKind::Error,
            Self::Shutdown => MessageKind::Shutdown,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ContractError {
    #[error("ramka pluginu przekracza limit {limit} bajtów")]
    FrameTooLarge { limit: usize },
    #[error("nieprawidłowy JSON ramki: {0}")]
    InvalidJson(String),
    #[error("nieobsługiwana wersja protokołu {0}")]
    UnsupportedProtocol(u32),
    #[error("ramka nie kończy się znakiem nowej linii")]
    MissingFrameDelimiter,
}

pub fn encode(message: &WireMessage) -> Result<Vec<u8>, ContractError> {
    encode_with_limit(message, MAX_FRAME_BYTES)
}

pub fn encode_with_limit(
    message: &WireMessage,
    max_bytes: usize,
) -> Result<Vec<u8>, ContractError> {
    let mut bytes =
        serde_json::to_vec(message).map_err(|e| ContractError::InvalidJson(e.to_string()))?;
    if bytes.len() + 1 > max_bytes {
        return Err(ContractError::FrameTooLarge { limit: max_bytes });
    }
    bytes.push(b'\n');
    Ok(bytes)
}

pub fn decode(bytes: &[u8]) -> Result<WireMessage, ContractError> {
    decode_with_limit(bytes, MAX_FRAME_BYTES)
}

pub fn decode_with_limit(bytes: &[u8], max_bytes: usize) -> Result<WireMessage, ContractError> {
    if bytes.len() > max_bytes {
        return Err(ContractError::FrameTooLarge { limit: max_bytes });
    }
    serde_json::from_slice(bytes).map_err(|e| ContractError::InvalidJson(e.to_string()))
}

/// Dekoduje kompletną ramkę transportową. Delimiter jest częścią kontraktu;
/// wywołujący nie może przekazać niekompletnego fragmentu JSON.
pub fn decode_frame(bytes: &[u8]) -> Result<WireMessage, ContractError> {
    decode_frame_with_limit(bytes, MAX_FRAME_BYTES)
}

pub fn decode_frame_with_limit(
    bytes: &[u8],
    max_bytes: usize,
) -> Result<WireMessage, ContractError> {
    if bytes.last() != Some(&b'\n') {
        return Err(ContractError::MissingFrameDelimiter);
    }
    decode_with_limit(&bytes[..bytes.len() - 1], max_bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn request_round_trips() {
        let message = WireMessage::Request(Request {
            request_id: "r1".into(),
            tool_id: "plugin_sysinfo".into(),
            args: json!({}),
        });
        assert_eq!(decode(&encode(&message).unwrap()).unwrap(), message);
    }

    #[test]
    fn oversized_frame_is_rejected() {
        let message = WireMessage::Error(ErrorMessage {
            request_id: None,
            code: "x".into(),
            message: "x".repeat(MAX_FRAME_BYTES),
        });
        assert!(matches!(
            encode(&message),
            Err(ContractError::FrameTooLarge { .. })
        ));
    }

    #[test]
    fn frame_requires_newline_delimiter() {
        let message = serde_json::to_vec(&WireMessage::Shutdown).unwrap();
        assert!(matches!(
            decode_frame(&message),
            Err(ContractError::MissingFrameDelimiter)
        ));
    }
}
