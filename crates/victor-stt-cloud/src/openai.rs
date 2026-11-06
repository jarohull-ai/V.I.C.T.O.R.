//! OpenAI Speech-to-Text (`POST /v1/audio/transcriptions`).
//!
//! Feature flag: `openai`. Bez tej flagi moduł nie istnieje, więc lokalna
//! ścieżka whisper.cpp nie ma nawet jak zaimportować klienta OpenAI.

use async_trait::async_trait;
use serde::Deserialize;
use victor_integrations::{AudioChunk, IntegrationError, Transcript, TranscriptionLocation};

use crate::authorized::{AuthorizedCloudTranscriptionRequest, CloudSpeechToText};
use crate::endpoint::EndpointAllowlist;
use crate::retry::execute_with_retry;
use crate::secret::ApiKey;
use crate::transport::{HttpRequest, HttpSettings, HttpTransport, ReqwestTransport};

const PROVIDER: &str = "openai";
const DEFAULT_ENDPOINT: &str = "https://api.openai.com/v1/audio/transcriptions";
const ALLOWED_HOST: &str = "api.openai.com";
const ALLOWED_PATH: &str = "/v1/audio/transcriptions";

#[derive(Debug, Clone)]
pub struct OpenAiConfig {
    pub http: HttpSettings,
    pub api_key: ApiKey,
    pub model: String,
    pub language: Option<String>,
}

impl OpenAiConfig {
    pub fn new(api_key: ApiKey) -> Self {
        Self {
            http: HttpSettings::new(
                DEFAULT_ENDPOINT,
                EndpointAllowlist::https(ALLOWED_HOST, ALLOWED_PATH),
            ),
            api_key,
            model: "whisper-1".into(),
            language: None,
        }
    }

    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }

    pub fn with_language(mut self, language: impl Into<String>) -> Self {
        self.language = Some(language.into());
        self
    }

    /// Pozwala podmienić endpoint tylko wtedy, gdy nowy host i ścieżka są
    /// jawnie dodane do allowlisty. Służy to lokalnym gatewayom, nie
    /// „dowolnemu URL-owi z ustawień".
    pub fn with_endpoint(
        mut self,
        endpoint: impl Into<String>,
        allowlist: EndpointAllowlist,
    ) -> Self {
        self.http.endpoint = endpoint.into();
        self.http.allowlist = allowlist;
        self
    }

    pub fn validate(&self) -> Result<(), IntegrationError> {
        self.http.validate()?;
        crate::validate_model(&self.model)?;
        if let Some(language) = &self.language {
            crate::validate_language(language)?;
        }
        Ok(())
    }
}

pub struct OpenAiSpeechToText<T: HttpTransport = ReqwestTransport> {
    config: OpenAiConfig,
    transport: T,
}

impl OpenAiSpeechToText<ReqwestTransport> {
    pub fn new(config: OpenAiConfig) -> Result<Self, IntegrationError> {
        config.validate()?;
        Ok(Self {
            config,
            transport: ReqwestTransport::new()?,
        })
    }
}

impl<T: HttpTransport> OpenAiSpeechToText<T> {
    pub fn with_transport(config: OpenAiConfig, transport: T) -> Result<Self, IntegrationError> {
        config.validate()?;
        Ok(Self { config, transport })
    }

    pub fn config(&self) -> &OpenAiConfig {
        &self.config
    }
}

#[async_trait]
impl<T: HttpTransport> CloudSpeechToText for OpenAiSpeechToText<T> {
    fn provider(&self) -> &'static str {
        PROVIDER
    }

    async fn transcribe(
        &self,
        request: AuthorizedCloudTranscriptionRequest,
    ) -> Result<Transcript, IntegrationError> {
        let audio = request.audio();
        if audio.bytes.len() as u64 > self.config.http.max_audio_bytes {
            return Err(IntegrationError::LimitExceeded(
                "nagranie przekracza limit audio".into(),
            ));
        }

        let endpoint = self
            .config
            .http
            .allowlist
            .validate(&self.config.http.endpoint)?;
        let boundary = multipart_boundary(audio);
        let body = multipart_body(
            audio,
            &self.config.model,
            self.config.language.as_deref(),
            &boundary,
        );
        let authorization = format!("Bearer {}", self.config.api_key.expose());
        let content_type = format!("multipart/form-data; boundary={boundary}");

        let response = execute_with_retry(
            &self.transport,
            request.permit(),
            request.plan(),
            audio,
            PROVIDER,
            self.config.http.retry,
            || {
                Ok(HttpRequest::post(
                    endpoint.clone(),
                    self.config.http.timeout,
                    self.config.http.max_response_bytes,
                )
                .header("Authorization", authorization.clone())?
                .header("Content-Type", content_type.clone())?
                .body(body.clone()))
            },
        )
        .await?;

        if !(200..300).contains(&response.status) {
            return Err(IntegrationError::provider(
                PROVIDER,
                format!("HTTP {}", response.status),
            ));
        }
        parse_openai_response(&response.body)
    }
}

#[derive(Debug, Deserialize)]
struct OpenAiResponse {
    text: String,
    #[serde(default)]
    language: Option<String>,
}

fn parse_openai_response(body: &[u8]) -> Result<Transcript, IntegrationError> {
    let parsed: OpenAiResponse = serde_json::from_slice(body)
        .map_err(|_| IntegrationError::provider(PROVIDER, "nieprawidłowa odpowiedź JSON"))?;
    let text = parsed.text.trim().to_string();
    if text.is_empty() {
        return Err(IntegrationError::provider(
            PROVIDER,
            "pusta transkrypcja w odpowiedzi",
        ));
    }
    Ok(Transcript {
        text,
        language: parsed
            .language
            .map(|language| language.trim().to_string())
            .filter(|language| !language.is_empty()),
        confidence: None,
        is_final: true,
        provider: PROVIDER.to_string(),
        location: TranscriptionLocation::Cloud,
    })
}

fn multipart_boundary(audio: &AudioChunk) -> String {
    // 32 znaki hex + prefiks = 39 znaków, poniżej limitu 70 znaków RFC 2046.
    format!("victor-{}", &audio.sha256()[..32])
}

fn multipart_body(
    audio: &AudioChunk,
    model: &str,
    language: Option<&str>,
    boundary: &str,
) -> Vec<u8> {
    let mut body = Vec::new();
    append_file_part(&mut body, boundary, audio);
    append_text_part(&mut body, boundary, "model", model);
    append_text_part(&mut body, boundary, "response_format", "verbose_json");
    if let Some(language) = language {
        append_text_part(&mut body, boundary, "language", language);
    }
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    body
}

fn append_file_part(body: &mut Vec<u8>, boundary: &str, audio: &AudioChunk) {
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    body.extend_from_slice(
        format!(
            "Content-Disposition: form-data; name=\"file\"; filename=\"audio.{}\"\r\n",
            audio.format.as_str()
        )
        .as_bytes(),
    );
    body.extend_from_slice(
        format!("Content-Type: {}\r\n\r\n", audio.format.mime_type()).as_bytes(),
    );
    body.extend_from_slice(&audio.bytes);
    body.extend_from_slice(b"\r\n");
}

fn append_text_part(body: &mut Vec<u8>, boundary: &str, name: &str, value: &str) {
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    body.extend_from_slice(
        format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n").as_bytes(),
    );
    body.extend_from_slice(value.as_bytes());
    body.extend_from_slice(b"\r\n");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::endpoint::EndpointAllowlist;
    use crate::test_support;
    use crate::testing::{ScriptedResponse, ScriptedTransport};
    use std::time::Duration;

    fn config() -> OpenAiConfig {
        OpenAiConfig::new(ApiKey::new("sk-test").unwrap()).with_language("pl")
    }

    fn adapter(responses: Vec<ScriptedResponse>) -> OpenAiSpeechToText<ScriptedTransport> {
        OpenAiSpeechToText::with_transport(config(), ScriptedTransport::new(responses)).unwrap()
    }

    fn authorized() -> AuthorizedCloudTranscriptionRequest {
        let plan = test_support::plan();
        let audio = test_support::wav();
        let permit = test_support::permit_for(&plan, &audio);
        AuthorizedCloudTranscriptionRequest::new(&plan, audio, permit).unwrap()
    }

    #[tokio::test]
    async fn parses_transcript_and_marks_cloud_provenance() {
        let responses = vec![ScriptedResponse::ok(
            r#"{"text":"  cześć  ","language":"pl"}"#.as_bytes().to_vec(),
        )];
        let transcript = adapter(responses).transcribe(authorized()).await.unwrap();

        assert_eq!(transcript.text, "cześć");
        assert_eq!(transcript.language.as_deref(), Some("pl"));
        assert_eq!(transcript.provider, "openai");
        assert_eq!(transcript.location, TranscriptionLocation::Cloud);
        assert!(transcript.is_final);
    }

    #[tokio::test]
    async fn sends_key_in_header_never_in_url_or_body() {
        let transport =
            ScriptedTransport::new(vec![ScriptedResponse::ok(br#"{"text":"ok"}"#.to_vec())]);
        let adapter = OpenAiSpeechToText::with_transport(config(), transport).unwrap();
        adapter.transcribe(authorized()).await.unwrap();

        let transport = &adapter.transport;
        transport
            .last_request(|request| {
                assert_eq!(
                    request.header_value("Authorization"),
                    Some("Bearer sk-test")
                );
                assert!(!request.url().as_str().contains("sk-test"));
                let body = String::from_utf8_lossy(request.body_bytes());
                assert!(body.contains("name=\"model\""));
                assert!(body.contains("whisper-1"));
                assert!(body.contains("name=\"file\""));
                assert!(body.contains("name=\"language\""));
                assert!(!body.contains("sk-test"));
                let rendered = format!("{request:?}");
                assert!(!rendered.contains("sk-test"));
            })
            .expect("request został zapisany");
    }

    #[tokio::test]
    async fn http_error_body_is_not_leaked_and_is_not_retried() {
        let responses = vec![ScriptedResponse::status_with_body(
            400,
            b"SECRET-AUDIO-AND-KEY".to_vec(),
        )];
        let transport = ScriptedTransport::new(responses);
        let adapter = OpenAiSpeechToText::with_transport(config(), transport).unwrap();

        let error = adapter.transcribe(authorized()).await.unwrap_err();
        let rendered = format!("{error:?} {error}");
        assert!(rendered.contains("400"));
        assert!(!rendered.contains("SECRET-AUDIO-AND-KEY"));
        assert_eq!(adapter.transport.request_count(), 1);
    }

    #[tokio::test]
    async fn empty_text_is_an_error() {
        let responses = vec![ScriptedResponse::ok(br#"{"text":"   "}"#.to_vec())];
        assert!(adapter(responses).transcribe(authorized()).await.is_err());
    }

    #[tokio::test]
    async fn oversized_audio_is_rejected_before_any_request() {
        let mut config = config();
        config.http.max_audio_bytes = 1;
        let transport = ScriptedTransport::new(vec![ScriptedResponse::ok(b"{}".to_vec())]);
        let adapter = OpenAiSpeechToText::with_transport(config, transport).unwrap();

        assert!(matches!(
            adapter.transcribe(authorized()).await,
            Err(IntegrationError::LimitExceeded(_))
        ));
        assert_eq!(adapter.transport.request_count(), 0);
    }

    #[test]
    fn rejects_endpoint_outside_allowlist() {
        let config = config().with_endpoint(
            "https://evil.example/v1/audio/transcriptions",
            EndpointAllowlist::https("api.openai.com", "/v1/audio/transcriptions"),
        );
        assert!(OpenAiSpeechToText::with_transport(config, ScriptedTransport::default()).is_err());
    }

    #[test]
    fn retry_policy_and_timeout_are_validated() {
        let mut config = config();
        config.http.timeout = Duration::ZERO;
        assert!(OpenAiSpeechToText::with_transport(config, ScriptedTransport::default()).is_err());
    }
}
