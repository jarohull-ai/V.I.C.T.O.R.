//! Deepgram Speech-to-Text (`POST /v1/listen`).
//!
//! Feature flag: `deepgram`. To osobny typ i osobny endpoint — nigdy nie jest
//! wywoływany jako fallback po błędzie OpenAI.

use async_trait::async_trait;
use serde::Deserialize;
use victor_integrations::{IntegrationError, Transcript, TranscriptionLocation};

use crate::authorized::{AuthorizedCloudTranscriptionRequest, CloudSpeechToText};
use crate::endpoint::EndpointAllowlist;
use crate::retry::execute_with_retry;
use crate::secret::ApiKey;
use crate::transport::{HttpRequest, HttpSettings, HttpTransport, ReqwestTransport};

const PROVIDER: &str = "deepgram";
const DEFAULT_ENDPOINT: &str = "https://api.deepgram.com/v1/listen";
const ALLOWED_HOST: &str = "api.deepgram.com";
const ALLOWED_PATH: &str = "/v1/listen";

#[derive(Debug, Clone)]
pub struct DeepgramConfig {
    pub http: HttpSettings,
    pub api_key: ApiKey,
    pub model: String,
    pub language: Option<String>,
    pub smart_format: bool,
    pub punctuate: bool,
}

impl DeepgramConfig {
    pub fn new(api_key: ApiKey) -> Self {
        Self {
            http: HttpSettings::new(
                DEFAULT_ENDPOINT,
                EndpointAllowlist::https(ALLOWED_HOST, ALLOWED_PATH),
            ),
            api_key,
            model: "nova-2".into(),
            language: None,
            smart_format: true,
            punctuate: true,
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

    /// Nowy endpoint działa tylko z jawnie podaną allowlistą.
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

pub struct DeepgramSpeechToText<T: HttpTransport = ReqwestTransport> {
    config: DeepgramConfig,
    transport: T,
}

impl DeepgramSpeechToText<ReqwestTransport> {
    pub fn new(config: DeepgramConfig) -> Result<Self, IntegrationError> {
        config.validate()?;
        Ok(Self {
            config,
            transport: ReqwestTransport::new()?,
        })
    }
}

impl<T: HttpTransport> DeepgramSpeechToText<T> {
    pub fn with_transport(config: DeepgramConfig, transport: T) -> Result<Self, IntegrationError> {
        config.validate()?;
        Ok(Self { config, transport })
    }

    pub fn config(&self) -> &DeepgramConfig {
        &self.config
    }
}

#[async_trait]
impl<T: HttpTransport> CloudSpeechToText for DeepgramSpeechToText<T> {
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

        let url = self.request_url();
        let endpoint = self.config.http.allowlist.validate(&url)?;
        let authorization = format!("Token {}", self.config.api_key.expose());
        let content_type = audio.format.mime_type().to_string();

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
                .body(audio.bytes.clone()))
            },
        )
        .await?;

        if !(200..300).contains(&response.status) {
            return Err(IntegrationError::provider(
                PROVIDER,
                format!("HTTP {}", response.status),
            ));
        }
        parse_deepgram_response(&response.body)
    }
}

impl<T: HttpTransport> DeepgramSpeechToText<T> {
    fn request_url(&self) -> String {
        let mut serializer = url::form_urlencoded::Serializer::new(String::new());
        serializer.append_pair("model", &self.config.model);
        serializer.append_pair("smart_format", bool_str(self.config.smart_format));
        serializer.append_pair("punctuate", bool_str(self.config.punctuate));
        match self.config.language.as_deref() {
            Some(language) => {
                serializer.append_pair("language", language);
            }
            None => {
                serializer.append_pair("detect_language", "true");
            }
        }
        let query = serializer.finish();
        let separator = if self.config.http.endpoint.contains('?') {
            '&'
        } else {
            '?'
        };
        format!("{}{separator}{query}", self.config.http.endpoint)
    }
}

fn bool_str(value: bool) -> &'static str {
    if value {
        "true"
    } else {
        "false"
    }
}

#[derive(Debug, Deserialize)]
struct DeepgramResponse {
    results: DeepgramResults,
}

#[derive(Debug, Deserialize)]
struct DeepgramResults {
    #[serde(default)]
    channels: Vec<DeepgramChannel>,
}

#[derive(Debug, Deserialize)]
struct DeepgramChannel {
    #[serde(default)]
    alternatives: Vec<DeepgramAlternative>,
    #[serde(default)]
    detected_language: Option<String>,
    #[serde(default)]
    language: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DeepgramAlternative {
    transcript: String,
    #[serde(default)]
    confidence: Option<f32>,
}

fn parse_deepgram_response(body: &[u8]) -> Result<Transcript, IntegrationError> {
    let parsed: DeepgramResponse = serde_json::from_slice(body)
        .map_err(|_| IntegrationError::provider(PROVIDER, "nieprawidłowa odpowiedź JSON"))?;
    let channel = parsed
        .results
        .channels
        .into_iter()
        .next()
        .ok_or_else(|| IntegrationError::provider(PROVIDER, "brak kanałów w odpowiedzi"))?;
    let alternative = channel
        .alternatives
        .into_iter()
        .next()
        .ok_or_else(|| IntegrationError::provider(PROVIDER, "brak alternatyw w odpowiedzi"))?;

    let text = alternative.transcript.trim().to_string();
    if text.is_empty() {
        return Err(IntegrationError::provider(
            PROVIDER,
            "pusta transkrypcja w odpowiedzi",
        ));
    }
    let language = channel
        .detected_language
        .or(channel.language)
        .map(|language| language.trim().to_string())
        .filter(|language| !language.is_empty());

    Ok(Transcript {
        text,
        language,
        confidence: alternative.confidence,
        is_final: true,
        provider: PROVIDER.to_string(),
        location: TranscriptionLocation::Cloud,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::endpoint::EndpointAllowlist;
    use crate::test_support;
    use crate::testing::{ScriptedResponse, ScriptedTransport};

    fn config() -> DeepgramConfig {
        DeepgramConfig::new(ApiKey::new("dg-test").unwrap()).with_language("pl")
    }

    fn adapter(responses: Vec<ScriptedResponse>) -> DeepgramSpeechToText<ScriptedTransport> {
        DeepgramSpeechToText::with_transport(config(), ScriptedTransport::new(responses)).unwrap()
    }

    fn authorized() -> AuthorizedCloudTranscriptionRequest {
        let plan = test_support::plan();
        let audio = test_support::wav();
        let permit = test_support::permit_for(&plan, &audio);
        AuthorizedCloudTranscriptionRequest::new(&plan, audio, permit).unwrap()
    }

    #[tokio::test]
    async fn parses_transcript_confidence_and_language() {
        let body = r#"{
            "results": {
                "channels": [{
                    "alternatives": [{"transcript": "  dzień dobry  ", "confidence": 0.97}],
                    "detected_language": "pl"
                }]
            }
        }"#;
        let transcript = adapter(vec![ScriptedResponse::ok(body.as_bytes().to_vec())])
            .transcribe(authorized())
            .await
            .unwrap();

        assert_eq!(transcript.text, "dzień dobry");
        assert_eq!(transcript.language.as_deref(), Some("pl"));
        assert_eq!(transcript.confidence, Some(0.97));
        assert_eq!(transcript.provider, "deepgram");
        assert_eq!(transcript.location, TranscriptionLocation::Cloud);
    }

    #[tokio::test]
    async fn sends_model_and_language_as_query_and_key_as_header() {
        let transport = ScriptedTransport::new(vec![ScriptedResponse::ok(
            br#"{"results":{"channels":[]}}"#.to_vec(),
        )]);
        let adapter = DeepgramSpeechToText::with_transport(config(), transport).unwrap();
        // Pusta odpowiedź jest błędem, ale request i tak został wysłany.
        assert!(adapter.transcribe(authorized()).await.is_err());

        adapter
            .transport
            .last_request(|request| {
                let url = request.url().as_str();
                assert!(url.starts_with("https://api.deepgram.com/v1/listen?"));
                assert!(url.contains("model=nova-2"));
                assert!(url.contains("language=pl"));
                assert!(url.contains("smart_format=true"));
                assert!(!url.contains("dg-test"));
                assert_eq!(request.header_value("Authorization"), Some("Token dg-test"));
                assert_eq!(request.body_bytes(), test_support::wav().bytes.as_slice());
                let rendered = format!("{request:?}");
                assert!(!rendered.contains("dg-test"));
            })
            .expect("request został zapisany");
    }

    #[tokio::test]
    async fn http_error_body_is_not_leaked() {
        let responses = vec![ScriptedResponse::status_with_body(
            401,
            b"SECRET-DEEPGRAM-BODY".to_vec(),
        )];
        let transport = ScriptedTransport::new(responses);
        let adapter = DeepgramSpeechToText::with_transport(config(), transport).unwrap();

        let error = adapter.transcribe(authorized()).await.unwrap_err();
        let rendered = format!("{error:?} {error}");
        assert!(rendered.contains("401"));
        assert!(!rendered.contains("SECRET-DEEPGRAM-BODY"));
        assert_eq!(adapter.transport.request_count(), 1);
    }

    #[tokio::test]
    async fn empty_channels_are_an_error() {
        assert!(adapter(vec![ScriptedResponse::ok(
            br#"{"results":{"channels":[]}}"#.to_vec()
        )])
        .transcribe(authorized())
        .await
        .is_err());
    }

    #[test]
    fn rejects_endpoint_outside_allowlist() {
        let config = config().with_endpoint(
            "https://evil.example/v1/listen",
            EndpointAllowlist::https("api.deepgram.com", "/v1/listen"),
        );
        assert!(
            DeepgramSpeechToText::with_transport(config, ScriptedTransport::default()).is_err()
        );
    }
}
