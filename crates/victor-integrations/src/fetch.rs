use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::IntegrationError;

pub const DEFAULT_MAX_RESPONSE_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_URL_LEN: usize = 2048;

/// Polityka jedynej dozwolonej ścieżki sieciowej V.I.C.T.O.R.
///
/// Ten walidator jest *kształtem* żądania, a nie pełną ochroną SSRF.
/// Ostateczna blokada adresów prywatnych, DNS rebinding i redirectów należy
/// do backendu (docelowo BALD `bse-fetch`). Tutaj jest pierwsza, tania bariera
/// i wspólny kontrakt, żeby żaden adapter nie mógł jej pominąć.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchPolicy {
    pub max_response_bytes: usize,
    pub timeout_ms: u64,
    pub max_redirects: u8,
    /// `None` = każdy publiczny host; `Some` = wyłącznie te hosty.
    pub allowed_hosts: Option<Vec<String>>,
    /// HTTP jest domyślnie zabronione — tylko HTTPS.
    pub allow_http: bool,
    /// Best-effort blokada localhost / sieci prywatnych.
    pub deny_private_hosts: bool,
}

impl Default for FetchPolicy {
    fn default() -> Self {
        Self {
            max_response_bytes: DEFAULT_MAX_RESPONSE_BYTES,
            timeout_ms: 15_000,
            max_redirects: 3,
            allowed_hosts: None,
            allow_http: false,
            deny_private_hosts: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchRequest {
    pub url: String,
}

impl FetchRequest {
    pub fn new(url: impl Into<String>) -> Self {
        Self { url: url.into() }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FetchProvenance {
    pub requested_url: String,
    pub final_url: String,
    pub status: u16,
    pub content_type: Option<String>,
    pub fetched_at_unix_ms: u64,
    pub sha256: String,
    pub bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchResponse {
    pub body: Vec<u8>,
    pub provenance: FetchProvenance,
    /// `true`, gdy backend uciął treść do `max_response_bytes`.
    pub truncated: bool,
}

/// Jedyne wyjście sieciowe agenta. Docelowy adapter: BALD `bse-fetch`.
#[async_trait]
pub trait WebFetcher: Send + Sync {
    fn backend(&self) -> &'static str;

    async fn fetch(
        &self,
        request: FetchRequest,
        policy: &FetchPolicy,
    ) -> Result<FetchResponse, IntegrationError>;
}

impl FetchPolicy {
    /// Walidacja przed wykonaniem żądania. Backend musi ją wywołać — kontrakt
    /// tego wymaga, a testy pilnują kształtu.
    pub fn validate(&self, request: &FetchRequest) -> Result<(), IntegrationError> {
        let url = request.url.as_str();

        if url.len() > MAX_URL_LEN {
            return Err(IntegrationError::Denied(format!(
                "URL dłuższy niż {MAX_URL_LEN} znaków"
            )));
        }
        if url.chars().any(|c| c.is_control() || c.is_whitespace()) {
            return Err(IntegrationError::Denied(
                "URL zawiera biały znak lub znak kontrolny".into(),
            ));
        }

        let https = url.strip_prefix("https://");
        let http = url.strip_prefix("http://");
        let rest = match (https, http) {
            (Some(rest), _) => rest,
            (None, Some(rest)) if self.allow_http => rest,
            (None, Some(_)) => {
                return Err(IntegrationError::Denied(
                    "dozwolony jest wyłącznie https://".into(),
                ))
            }
            (None, None) => {
                return Err(IntegrationError::Denied(
                    "URL musi zaczynać się od https://".into(),
                ))
            }
        };

        let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        let authority = &rest[..authority_end];

        if authority.is_empty() {
            return Err(IntegrationError::Denied("URL nie zawiera hosta".into()));
        }
        if authority.contains('@') {
            return Err(IntegrationError::Denied(
                "dane logowania w URL są zabronione".into(),
            ));
        }

        let host = if let Some(stripped) = authority.strip_prefix('[') {
            let end = stripped
                .find(']')
                .ok_or_else(|| IntegrationError::Denied("niepoprawny adres IPv6".into()))?;
            &stripped[..end]
        } else {
            authority.split(':').next().unwrap_or(authority)
        };

        if host.is_empty() {
            return Err(IntegrationError::Denied("URL nie zawiera hosta".into()));
        }

        if self.deny_private_hosts && is_blocked_host(host) {
            return Err(IntegrationError::Denied(format!(
                "host `{host}` jest lokalny lub prywatny"
            )));
        }

        if let Some(allowed) = &self.allowed_hosts {
            let normalized = host.to_ascii_lowercase();
            let permitted = allowed
                .iter()
                .any(|entry| entry.eq_ignore_ascii_case(&normalized));
            if !permitted {
                return Err(IntegrationError::Denied(format!(
                    "host `{host}` nie jest na liście dozwolonych"
                )));
            }
        }

        Ok(())
    }
}

/// Best-effort. Nie zastępuje blokady SSRF w backendzie.
fn is_blocked_host(host: &str) -> bool {
    let host = host.trim_matches(['[', ']']).to_ascii_lowercase();

    if host == "localhost"
        || host.ends_with(".localhost")
        || host == "metadata.google.internal"
        || host == "metadata"
    {
        return true;
    }

    if let Ok(ip) = host.parse::<std::net::Ipv4Addr>() {
        return ip.is_private()
            || ip.is_loopback()
            || ip.is_link_local()
            || ip.is_broadcast()
            || ip.is_unspecified();
    }

    if let Ok(ip) = host.parse::<std::net::Ipv6Addr>() {
        let segments = ip.segments();
        return ip.is_loopback()
            || ip.is_unspecified()
            || (segments[0] & 0xfe00) == 0xfc00
            || (segments[0] & 0xffc0) == 0xfe80;
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::FakeWebFetcher;

    fn ok(policy: &FetchPolicy, url: &str) -> bool {
        policy.validate(&FetchRequest::new(url)).is_ok()
    }

    #[test]
    fn accepts_public_https() {
        assert!(ok(&FetchPolicy::default(), "https://example.com/a?b=1"));
    }

    #[test]
    fn rejects_plain_http_by_default() {
        assert!(!ok(&FetchPolicy::default(), "http://example.com"));
    }

    #[test]
    fn allows_http_only_when_explicitly_enabled() {
        let policy = FetchPolicy {
            allow_http: true,
            ..FetchPolicy::default()
        };
        assert!(ok(&policy, "http://example.com"));
    }

    #[test]
    fn rejects_non_http_schemes() {
        for url in [
            "file:///etc/passwd",
            "ftp://example.com",
            "javascript:alert(1)",
        ] {
            assert!(!ok(&FetchPolicy::default(), url), "{url}");
        }
    }

    #[test]
    fn rejects_credentials_in_url() {
        assert!(!ok(
            &FetchPolicy::default(),
            "https://user:pass@example.com/x"
        ));
    }

    #[test]
    fn rejects_local_and_private_hosts() {
        for url in [
            "https://localhost/",
            "https://127.0.0.1/",
            "https://10.0.0.1/",
            "https://192.168.1.1/",
            "https://169.254.169.254/latest/meta-data",
            "https://[::1]/",
            "https://[fc00::1]/",
            "https://[fe80::1]/",
            "https://0.0.0.0/",
        ] {
            assert!(
                !ok(&FetchPolicy::default(), url),
                "{url} powinien być zablokowany"
            );
        }
    }

    #[test]
    fn host_allowlist_is_enforced() {
        let policy = FetchPolicy {
            allowed_hosts: Some(vec!["docs.rs".into()]),
            ..FetchPolicy::default()
        };
        assert!(ok(&policy, "https://docs.rs/serde"));
        assert!(!ok(&policy, "https://example.com"));
    }

    #[test]
    fn rejects_whitespace_and_overlong_urls() {
        assert!(!ok(&FetchPolicy::default(), "https://example.com/a b"));
        let long = format!("https://example.com/{}", "a".repeat(MAX_URL_LEN));
        assert!(!ok(&FetchPolicy::default(), &long));
    }

    #[test]
    fn rejects_url_without_host() {
        assert!(!ok(&FetchPolicy::default(), "https:///path"));
    }

    #[tokio::test]
    async fn fake_fetcher_returns_provenance_and_truncates() {
        let body = b"abcdefghij".to_vec();
        let fetcher = FakeWebFetcher::new(200, "text/plain", body.clone(), 42);
        let policy = FetchPolicy {
            max_response_bytes: 4,
            ..FetchPolicy::default()
        };

        let response = fetcher
            .fetch(FetchRequest::new("https://example.com/x"), &policy)
            .await
            .unwrap();

        assert_eq!(response.body, b"abcd".to_vec());
        assert!(response.truncated);
        assert_eq!(response.provenance.sha256, crate::sha256_hex(b"abcd"));
        assert_eq!(response.provenance.fetched_at_unix_ms, 42);
        assert_eq!(response.provenance.status, 200);
    }

    #[tokio::test]
    async fn fake_fetcher_refuses_policy_violation() {
        let fetcher = FakeWebFetcher::default();
        let error = fetcher
            .fetch(
                FetchRequest::new("http://example.com"),
                &FetchPolicy::default(),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, IntegrationError::Denied(_)));
    }
}
