//! Allowlista endpointów. `HttpRequest` można zbudować wyłącznie z
//! [`ValidatedEndpoint`], więc nie da się wysłać audio pod dowolny URL.

use std::fmt;

use url::Url;
use victor_integrations::IntegrationError;

const MAX_URL_LEN: usize = 2_048;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AllowedEndpoint {
    host: String,
    path_prefix: String,
}

impl AllowedEndpoint {
    pub fn new(host: impl Into<String>, path_prefix: impl Into<String>) -> Self {
        Self {
            host: host.into().to_ascii_lowercase(),
            path_prefix: path_prefix.into(),
        }
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn path_prefix(&self) -> &str {
        &self.path_prefix
    }
}

#[derive(Debug, Clone, Default)]
pub struct EndpointAllowlist {
    entries: Vec<AllowedEndpoint>,
    allow_http: bool,
}

impl EndpointAllowlist {
    pub fn new(entries: Vec<AllowedEndpoint>) -> Self {
        Self {
            entries,
            allow_http: false,
        }
    }

    pub fn https(host: impl Into<String>, path_prefix: impl Into<String>) -> Self {
        Self::new(vec![AllowedEndpoint::new(host, path_prefix)])
    }

    /// Domyślnie wyłączone. Włączać tylko dla świadomie skonfigurowanego
    /// lokalnego gatewaya; publiczne providery zawsze przez HTTPS.
    pub fn allow_http(mut self, allow: bool) -> Self {
        self.allow_http = allow;
        self
    }

    pub fn validate(&self, raw: &str) -> Result<ValidatedEndpoint, IntegrationError> {
        if raw.is_empty() || raw.len() > MAX_URL_LEN {
            return Err(IntegrationError::Denied(
                "URL endpointu jest pusty lub zbyt długi".into(),
            ));
        }

        let url = Url::parse(raw)
            .map_err(|_| IntegrationError::Denied("nieprawidłowy URL endpointu".into()))?;

        match url.scheme() {
            "https" => {}
            "http" if self.allow_http => {}
            "http" => return Err(IntegrationError::Denied("chmurowy STT wymaga HTTPS".into())),
            _ => {
                return Err(IntegrationError::Denied(
                    "dozwolone są tylko endpointy HTTP(S)".into(),
                ))
            }
        }

        if !url.username().is_empty() || url.password().is_some() {
            return Err(IntegrationError::Denied(
                "URL endpointu nie może zawierać danych logowania".into(),
            ));
        }
        if url.fragment().is_some() {
            return Err(IntegrationError::Denied(
                "URL endpointu nie może zawierać fragmentu".into(),
            ));
        }
        if url.port().is_some() {
            return Err(IntegrationError::Denied(
                "endpoint nie może używać niestandardowego portu".into(),
            ));
        }

        let host = url
            .host_str()
            .ok_or_else(|| IntegrationError::Denied("URL endpointu nie ma hosta".into()))?
            .to_ascii_lowercase();
        let path = url.path();

        // Ścieżka jest częścią allowlisty. Odrzucamy kodowane separatory i
        // segmenty kropkowe, żeby serwer nie znormalizował URL-a poza prefiks.
        if path.contains('%') || path.split('/').any(|segment| matches!(segment, "." | "..")) {
            return Err(IntegrationError::Denied(
                "ścieżka endpointu zawiera niedozwolone segmenty".into(),
            ));
        }

        let allowed = self
            .entries
            .iter()
            .any(|entry| entry.host == host && path_matches(path, &entry.path_prefix));
        if !allowed {
            return Err(IntegrationError::Denied(format!(
                "endpoint `{host}{path}` nie jest na allowliście"
            )));
        }

        Ok(ValidatedEndpoint(url))
    }
}

fn path_matches(path: &str, prefix: &str) -> bool {
    if prefix == "/" {
        return true;
    }
    let prefix = prefix.trim_end_matches('/');
    path == prefix || path.starts_with(&format!("{prefix}/"))
}

/// URL, który przeszedł allowlistę. Pola są prywatne, a konstruktor niedostępny
/// publicznie — jedyna droga to [`EndpointAllowlist::validate`].
#[derive(Clone, PartialEq, Eq)]
pub struct ValidatedEndpoint(Url);

impl ValidatedEndpoint {
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    #[cfg(any(feature = "openai", feature = "deepgram"))]
    pub(crate) fn as_url(&self) -> &Url {
        &self.0
    }
}

impl fmt::Debug for ValidatedEndpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn openai() -> EndpointAllowlist {
        EndpointAllowlist::https("api.openai.com", "/v1/audio/transcriptions")
    }

    #[test]
    fn accepts_exact_allowed_endpoint() {
        let validated = openai()
            .validate("https://api.openai.com/v1/audio/transcriptions")
            .unwrap();
        assert_eq!(
            validated.as_str(),
            "https://api.openai.com/v1/audio/transcriptions"
        );
    }

    #[test]
    fn rejects_http_unknown_host_wrong_path_and_custom_port() {
        assert!(openai()
            .validate("http://api.openai.com/v1/audio/transcriptions")
            .is_err());
        assert!(openai()
            .validate("https://evil.example/v1/audio/transcriptions")
            .is_err());
        assert!(openai()
            .validate("https://api.openai.com/v1/responses")
            .is_err());
        assert!(openai()
            .validate("https://api.openai.com:8443/v1/audio/transcriptions")
            .is_err());
    }

    #[test]
    fn path_prefix_matches_segment_boundary_not_substring() {
        let allowlist = EndpointAllowlist::https("api.example", "/v1");
        assert!(allowlist.validate("https://api.example/v1").is_ok());
        assert!(allowlist.validate("https://api.example/v1/listen").is_ok());
        assert!(allowlist.validate("https://api.example/v11").is_err());
        assert!(allowlist.validate("https://api.example.evil/v1").is_err());
    }

    #[test]
    fn rejects_credentials_and_fragments() {
        assert!(openai()
            .validate("https://user:pass@api.openai.com/v1/audio/transcriptions")
            .is_err());
        assert!(openai()
            .validate("https://api.openai.com/v1/audio/transcriptions#frag")
            .is_err());
    }

    #[test]
    fn rejects_encoded_path_traversal() {
        let allowlist = EndpointAllowlist::https("api.example", "/v1");
        assert!(allowlist
            .validate("https://api.example/v1/%2e%2e/admin")
            .is_err());
        assert!(allowlist
            .validate("https://api.example/v1/%2fadmin")
            .is_err());
    }

    #[test]
    fn http_requires_explicit_opt_in() {
        let allowlist = EndpointAllowlist::https("gateway.local", "/v1").allow_http(true);
        assert!(allowlist.validate("http://gateway.local/v1/stt").is_ok());
    }
}
