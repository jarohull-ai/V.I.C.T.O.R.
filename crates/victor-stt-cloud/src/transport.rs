//! Transport HTTP. Produkcyjny klient jest za feature flagami providerów;
//! wspólne typy i allowlista działają także bez żadnego klienta sieciowego.

use std::fmt;
use std::time::Duration;

use async_trait::async_trait;
use victor_integrations::IntegrationError;

use crate::endpoint::ValidatedEndpoint;

pub const MAX_TIMEOUT: Duration = Duration::from_secs(300);
pub const MAX_RESPONSE_BYTES: u64 = 8 * 1024 * 1024;
pub const MAX_AUDIO_BYTES: u64 = 100 * 1024 * 1024;

/// Wspólne ustawienia HTTP dla providera. Nie zawiera klucza API — klucz
/// trzyma osobny typ providera, żeby `Debug` tego structa nigdy go nie pokazał.
#[derive(Debug, Clone)]
pub struct HttpSettings {
    pub endpoint: String,
    pub allowlist: crate::endpoint::EndpointAllowlist,
    pub timeout: Duration,
    pub max_response_bytes: u64,
    pub max_audio_bytes: u64,
    pub retry: crate::retry::RetryPolicy,
}

impl HttpSettings {
    pub fn new(endpoint: impl Into<String>, allowlist: crate::endpoint::EndpointAllowlist) -> Self {
        Self {
            endpoint: endpoint.into(),
            allowlist,
            timeout: crate::DEFAULT_TIMEOUT,
            max_response_bytes: crate::DEFAULT_MAX_RESPONSE_BYTES,
            max_audio_bytes: crate::DEFAULT_MAX_AUDIO_BYTES,
            retry: crate::retry::RetryPolicy::default(),
        }
    }

    /// Sprawdza limity i allowlistę. Zwraca bazowy, zwalidowany endpoint;
    /// providery mogą potem dołożyć query i zwalidować finalny URL jeszcze raz.
    pub fn validate(&self) -> Result<ValidatedEndpoint, IntegrationError> {
        if self.timeout.is_zero() || self.timeout > MAX_TIMEOUT {
            return Err(IntegrationError::NotConfigured(format!(
                "timeout HTTP musi mieścić się w (0, {}s]",
                MAX_TIMEOUT.as_secs()
            )));
        }
        if self.max_response_bytes == 0 || self.max_response_bytes > MAX_RESPONSE_BYTES {
            return Err(IntegrationError::NotConfigured(format!(
                "limit odpowiedzi musi mieścić się w (0, {MAX_RESPONSE_BYTES}] B"
            )));
        }
        if self.max_audio_bytes == 0 || self.max_audio_bytes > MAX_AUDIO_BYTES {
            return Err(IntegrationError::NotConfigured(format!(
                "limit audio musi mieścić się w (0, {MAX_AUDIO_BYTES}] B"
            )));
        }
        self.retry.validate()?;
        self.allowlist.validate(&self.endpoint)
    }
}

/// Żądanie HTTP, które może powstać wyłącznie z URL-a przepuszczonego przez
/// allowlistę. Pola są prywatne; `Debug` redaguje wartości nagłówków i body.
#[derive(Clone)]
pub struct HttpRequest {
    url: ValidatedEndpoint,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    timeout: Duration,
    max_response_bytes: u64,
}

impl HttpRequest {
    pub fn post(url: ValidatedEndpoint, timeout: Duration, max_response_bytes: u64) -> Self {
        Self {
            url,
            headers: Vec::new(),
            body: Vec::new(),
            timeout,
            max_response_bytes,
        }
    }

    pub fn header(
        mut self,
        name: impl Into<String>,
        value: impl Into<String>,
    ) -> Result<Self, IntegrationError> {
        let name = name.into();
        let value = value.into();
        validate_header(&name, &value)?;
        self.headers.push((name, value));
        Ok(self)
    }

    pub fn body(mut self, body: impl Into<Vec<u8>>) -> Self {
        self.body = body.into();
        self
    }

    pub fn max_response_bytes(&self) -> u64 {
        self.max_response_bytes
    }

    pub fn url(&self) -> &ValidatedEndpoint {
        &self.url
    }

    #[cfg(all(test, any(feature = "openai", feature = "deepgram")))]
    pub(crate) fn header_value(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(header, _)| header.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    #[cfg(all(test, any(feature = "openai", feature = "deepgram")))]
    pub(crate) fn body_bytes(&self) -> &[u8] {
        &self.body
    }

    #[cfg(any(feature = "openai", feature = "deepgram"))]
    pub(crate) fn into_parts(
        self,
    ) -> (
        ValidatedEndpoint,
        Vec<(String, String)>,
        Vec<u8>,
        Duration,
        u64,
    ) {
        (
            self.url,
            self.headers,
            self.body,
            self.timeout,
            self.max_response_bytes,
        )
    }
}

impl fmt::Debug for HttpRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let header_names: Vec<&str> = self.headers.iter().map(|(name, _)| name.as_str()).collect();
        f.debug_struct("HttpRequest")
            .field("url", &self.url)
            .field("header_names", &header_names)
            .field("body_len", &self.body.len())
            .field("timeout", &self.timeout)
            .field("max_response_bytes", &self.max_response_bytes)
            .finish()
    }
}

fn validate_header(name: &str, value: &str) -> Result<(), IntegrationError> {
    let valid_name = !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'));
    if !valid_name {
        return Err(IntegrationError::Denied(
            "nieprawidłowa nazwa nagłówka HTTP".into(),
        ));
    }
    if value.bytes().any(|byte| !(0x20..=0x7e).contains(&byte)) {
        return Err(IntegrationError::Denied(
            "niedozwolony znak w wartości nagłówka HTTP".into(),
        ));
    }
    Ok(())
}

#[derive(Clone, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    pub body: Vec<u8>,
    pub content_type: Option<String>,
}

impl fmt::Debug for HttpResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpResponse")
            .field("status", &self.status)
            .field("body_len", &self.body.len())
            .field("content_type", &self.content_type)
            .finish()
    }
}

#[async_trait]
pub trait HttpTransport: Send + Sync {
    async fn send(&self, request: HttpRequest) -> Result<HttpResponse, IntegrationError>;
}

#[cfg(any(feature = "openai", feature = "deepgram"))]
pub struct ReqwestTransport {
    client: reqwest::Client,
}

#[cfg(any(feature = "openai", feature = "deepgram"))]
impl ReqwestTransport {
    pub fn new() -> Result<Self, IntegrationError> {
        let client = reqwest::Client::builder()
            // Bez redirectów: allowlista obejmuje każdy faktyczny adres.
            .redirect(reqwest::redirect::Policy::none())
            // Bez proxy z env: proxy mogłoby ominąć allowlistę hostów.
            .no_proxy()
            .user_agent("victor-stt-cloud/0.1")
            .build()
            .map_err(|_| {
                IntegrationError::NotConfigured("nie można zbudować klienta HTTP".into())
            })?;
        Ok(Self { client })
    }
}

#[cfg(any(feature = "openai", feature = "deepgram"))]
#[async_trait]
impl HttpTransport for ReqwestTransport {
    async fn send(&self, request: HttpRequest) -> Result<HttpResponse, IntegrationError> {
        use futures_util::StreamExt;

        let (url, headers, body, timeout, max_response_bytes) = request.into_parts();
        let mut builder = self
            .client
            .post(url.as_url().clone())
            .timeout(timeout)
            .body(body);
        for (name, value) in headers {
            let name = reqwest::header::HeaderName::from_bytes(name.as_bytes()).map_err(|_| {
                IntegrationError::Denied("nieprawidłowa nazwa nagłówka HTTP".into())
            })?;
            let value = reqwest::header::HeaderValue::from_str(&value).map_err(|_| {
                IntegrationError::Denied("nieprawidłowa wartość nagłówka HTTP".into())
            })?;
            builder = builder.header(name, value);
        }

        let response = builder.send().await.map_err(map_reqwest_error)?;
        let status = response.status().as_u16();
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);

        if response
            .content_length()
            .is_some_and(|len| len > max_response_bytes)
        {
            return Err(IntegrationError::LimitExceeded(
                "odpowiedź HTTP przekracza limit".into(),
            ));
        }

        let mut stream = response.bytes_stream();
        let mut buffer = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(map_reqwest_error)?;
            if buffer.len().saturating_add(chunk.len()) as u64 > max_response_bytes {
                return Err(IntegrationError::LimitExceeded(
                    "odpowiedź HTTP przekracza limit".into(),
                ));
            }
            buffer.extend_from_slice(&chunk);
        }

        Ok(HttpResponse {
            status,
            body: buffer,
            content_type,
        })
    }
}

#[cfg(any(feature = "openai", feature = "deepgram"))]
fn map_reqwest_error(error: reqwest::Error) -> IntegrationError {
    if error.is_timeout() {
        IntegrationError::Timeout("żądanie HTTP przekroczyło limit czasu".into())
    } else if error.is_connect() {
        IntegrationError::Transport("nie można połączyć się z endpointem".into())
    } else {
        IntegrationError::Transport("błąd transportu HTTP".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::endpoint::EndpointAllowlist;

    fn endpoint() -> ValidatedEndpoint {
        EndpointAllowlist::https("api.example", "/v1")
            .validate("https://api.example/v1/stt")
            .unwrap()
    }

    #[test]
    fn debug_redacts_header_values_and_body() {
        let request = HttpRequest::post(endpoint(), Duration::from_secs(5), 1024)
            .header("Authorization", "Bearer sk-super-secret")
            .unwrap()
            .header("Content-Type", "audio/wav")
            .unwrap()
            .body(b"audio-bytes".to_vec());

        let rendered = format!("{request:?}");
        assert!(rendered.contains("Authorization"));
        assert!(rendered.contains("Content-Type"));
        assert!(!rendered.contains("sk-super-secret"));
        assert!(!rendered.contains("audio-bytes"));
        assert!(rendered.contains("body_len"));
    }

    #[test]
    fn header_injection_is_rejected() {
        let request = HttpRequest::post(endpoint(), Duration::from_secs(5), 1024);
        assert!(request
            .clone()
            .header("X-Test", "ok\r\nInjected: 1")
            .is_err());
        assert!(request.clone().header("X-Test", "zażółć").is_err());
        assert!(request.header("Bad Name", "ok").is_err());
    }

    #[test]
    fn settings_reject_unsafe_limits() {
        let mut settings = HttpSettings::new(
            "https://api.example/v1/stt",
            EndpointAllowlist::https("api.example", "/v1"),
        );
        assert!(settings.validate().is_ok());

        settings.timeout = Duration::ZERO;
        assert!(settings.validate().is_err());

        let mut settings = HttpSettings::new(
            "https://api.example/v1/stt",
            EndpointAllowlist::https("api.example", "/v1"),
        );
        settings.max_response_bytes = 0;
        assert!(settings.validate().is_err());

        let mut settings = HttpSettings::new(
            "https://api.example/v1/stt",
            EndpointAllowlist::https("api.example", "/v1"),
        );
        settings.max_audio_bytes = 0;
        assert!(settings.validate().is_err());
    }

    #[test]
    fn debug_redacts_response_body() {
        let response = HttpResponse {
            status: 200,
            body: b"transcript-secret".to_vec(),
            content_type: Some("application/json".into()),
        };
        let rendered = format!("{response:?}");
        assert!(!rendered.contains("transcript-secret"));
        assert!(rendered.contains("body_len"));
    }
}
