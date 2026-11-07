//! `bse-fetch` — jedyne miejsce, które wykonuje żądania HTTP do dowolnych stron.
//!
//! Zasady:
//! - tylko `http`/`https`, tylko porty 80/443, bez danych w URL;
//! - DNS jest filtrowany **w resolverze** (ochrona przed rebindingiem),
//!   a każdy redirect jest ponownie walidowany;
//! - `robots.txt` jest sprawdzany per origin; 5xx/awaria = fail-closed;
//! - twardy limit rozmiaru i typu treści;
//! - brak proxy środowiskowego (świadomy, bezpośredni fetch);
//! - ekstrakcja tekstu jest heurystyczna i nie wykonuje JavaScriptu.

mod cache;
mod extract;
mod ssrf;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{SecondsFormat, Utc};
use encoding_rs::{Encoding, UTF_8};
use reqwest::header::CONTENT_TYPE;
use reqwest::redirect::Policy;
use reqwest::Url;
use robotstxt::DefaultMatcher;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

pub use cache::FetchCache;
pub use extract::{extract_html, extract_plain, ExtractedDocument};
pub use ssrf::{SsrfError, SsrfPolicy};

use ssrf::SsrfResolver;

const DEFAULT_MAX_BYTES: usize = 2 * 1024 * 1024;
const DEFAULT_MAX_REDIRECTS: usize = 5;
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FetchedDocument {
    pub requested_url: String,
    pub final_url: String,
    /// Canonical z `<link rel="canonical">`, tylko gdy prowadzi do tej samej domeny.
    pub canonical_url: String,
    pub status: u16,
    pub content_type: String,
    pub title: String,
    pub description: String,
    pub text: String,
    pub lang: Option<String>,
    pub published: Option<String>,
    pub content_hash: String,
    pub html_hash: String,
    pub fetched_at: String,
    /// Strona oznaczona w meta robots jako `noindex` — nie trafia do lokalnego indeksu.
    pub noindex: bool,
    /// `true`, gdy odpowiedź pochodzi z lokalnego cache.
    pub cache_hit: bool,
}

#[derive(Debug, Clone)]
pub struct FetchOptions {
    pub max_bytes: usize,
    pub max_redirects: usize,
    pub timeout: Duration,
    pub user_agent: String,
    /// Jeśli `true`, przed pobraniem sprawdzany jest `/robots.txt` danej origin.
    pub respect_robots: bool,
    /// Jeśli podano, każdy URL początkowy i każdy redirect musi wskazywać
    /// dokładnie host z tej listy.
    pub allowed_hosts: Option<Vec<String>>,
}

impl Default for FetchOptions {
    fn default() -> Self {
        Self {
            max_bytes: DEFAULT_MAX_BYTES,
            max_redirects: DEFAULT_MAX_REDIRECTS,
            timeout: DEFAULT_TIMEOUT,
            user_agent: "BALD/0.1 (research; +https://github.com/jarohull-ai/bald)".to_string(),
            respect_robots: true,
            allowed_hosts: None,
        }
    }
}

#[derive(Debug, Error)]
pub enum FetchError {
    #[error("invalid URL: {0}")]
    InvalidUrl(String),

    #[error("SSRF policy blocked request: {0}")]
    Blocked(#[from] SsrfError),

    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("unexpected HTTP status {0}")]
    Status(u16),

    #[error("response exceeds {limit} bytes")]
    TooLarge { limit: usize },

    #[error("unsupported content type: {0}")]
    UnsupportedContentType(String),

    #[error("too many redirects ({0})")]
    TooManyRedirects(usize),

    #[error("redirect without Location header")]
    RedirectWithoutLocation,

    #[error("cannot decode or extract document: {0}")]
    Extract(String),

    #[error("robots.txt disallows this URL for BALD")]
    RobotsDisallowed,

    #[error("robots.txt could not be evaluated: {0}")]
    RobotsUnavailable(String),
}

impl FetchError {
    /// Stabilny kod do audytu/UI — bez surowych adresów ani treści.
    pub fn kind(&self) -> &'static str {
        match self {
            FetchError::InvalidUrl(_) => "invalid_url",
            FetchError::Blocked(_) => "ssrf_blocked",
            FetchError::Http(_) => "http",
            FetchError::Status(_) => "status",
            FetchError::TooLarge { .. } => "too_large",
            FetchError::UnsupportedContentType(_) => "unsupported_content_type",
            FetchError::TooManyRedirects(_) => "too_many_redirects",
            FetchError::RedirectWithoutLocation => "redirect_without_location",
            FetchError::Extract(_) => "extract",
            FetchError::RobotsDisallowed => "robots_disallowed",
            FetchError::RobotsUnavailable(_) => "robots_unavailable",
        }
    }
}

pub struct Fetcher {
    client: reqwest::Client,
    policy: SsrfPolicy,
    options: FetchOptions,
    cache: Option<FetchCache>,
    /// origin → treść robots.txt; `None` = brak/4xx (dostęp dozwolony).
    robots: Mutex<HashMap<String, Option<String>>>,
}

impl Fetcher {
    pub fn new(options: FetchOptions) -> Result<Self, FetchError> {
        let policy = SsrfPolicy::default();
        let resolver = Arc::new(SsrfResolver::new(policy.clone()));
        let client = reqwest::Client::builder()
            .user_agent(options.user_agent.clone())
            .timeout(options.timeout)
            // Redirecty obsługujemy ręcznie, żeby każdy hop przechodził przez politykę.
            .redirect(Policy::none())
            // Proxy z env mogłoby ominąć resolver i politykę SSRF.
            .no_proxy()
            .dns_resolver(resolver)
            .build()?;
        Ok(Self {
            client,
            policy,
            options,
            cache: None,
            robots: Mutex::new(HashMap::new()),
        })
    }

    pub fn with_cache(mut self, cache: FetchCache) -> Self {
        self.cache = Some(cache);
        self
    }

    pub async fn fetch(&self, raw_url: &str) -> Result<FetchedDocument, FetchError> {
        let mut url =
            Url::parse(raw_url.trim()).map_err(|e| FetchError::InvalidUrl(e.to_string()))?;
        url.set_fragment(None);
        self.policy.validate_syntax(&url)?;

        // Cache działa też offline: pełny DNS jest potrzebny dopiero przed siecią.
        if let Some(cache) = &self.cache {
            if let Some(mut doc) = cache.get(&url) {
                doc.cache_hit = true;
                return Ok(doc);
            }
        }

        let (final_url, status, content_type, bytes) = self.fetch_bytes(url.clone(), true).await?;
        let decoded = decode_text(&bytes, &content_type);

        let final_host = final_url.host_str().unwrap_or_default().to_string();
        let extracted = if is_html(&content_type) {
            extract_html(&decoded, &final_url)
        } else {
            extract_plain(&decoded, &final_url)
        };

        let canonical_url = extracted
            .canonical
            .as_deref()
            .and_then(|raw| Url::parse(raw).ok())
            .map(|mut candidate| {
                candidate.set_fragment(None);
                candidate
            })
            .filter(|candidate| {
                candidate.host_str() == Some(final_host.as_str())
                    && self.policy.validate_syntax(candidate).is_ok()
            })
            .map(|u| u.to_string())
            .unwrap_or_else(|| final_url.to_string());

        let html_hash = sha256_prefixed(&decoded);
        let content_hash = sha256_prefixed(&extracted.text);
        let doc = FetchedDocument {
            requested_url: url.to_string(),
            final_url: final_url.to_string(),
            canonical_url,
            status,
            content_type,
            title: extracted.title,
            description: extracted.description,
            text: extracted.text,
            lang: extracted.lang,
            published: extracted.published,
            content_hash,
            html_hash,
            fetched_at: Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
            noindex: extracted.noindex,
            cache_hit: false,
        };

        if let Some(cache) = &self.cache {
            // Cache jest best effort: jego błąd nie może zepsuć udanego fetchu.
            let _ = cache.put(&url, &doc);
        }
        Ok(doc)
    }

    async fn fetch_bytes(
        &self,
        mut url: Url,
        check_robots: bool,
    ) -> Result<(Url, u16, String, Vec<u8>), FetchError> {
        let mut redirects = 0usize;

        loop {
            validate_allowed_host(&url, self.options.allowed_hosts.as_deref())?;
            self.policy.validate_url(&url).await?;
            if check_robots {
                self.ensure_robots_allowed(&url).await?;
            }
            let response = self.client.get(url.clone()).send().await?;
            let status = response.status();

            if status.is_redirection() {
                if redirects >= self.options.max_redirects {
                    return Err(FetchError::TooManyRedirects(self.options.max_redirects));
                }
                let location = response
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .ok_or(FetchError::RedirectWithoutLocation)?;
                let next = url
                    .join(location)
                    .map_err(|e| FetchError::InvalidUrl(e.to_string()))?;
                url = next;
                url.set_fragment(None);
                redirects += 1;
                continue;
            }

            if !status.is_success() {
                return Err(FetchError::Status(status.as_u16()));
            }

            let content_type = response
                .headers()
                .get(CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("text/html")
                .to_string();

            if !is_supported(&content_type) {
                return Err(FetchError::UnsupportedContentType(content_type));
            }

            if let Some(length) = response.content_length() {
                if length > self.options.max_bytes as u64 {
                    return Err(FetchError::TooLarge {
                        limit: self.options.max_bytes,
                    });
                }
            }

            let mut response = response;
            let mut body = Vec::new();
            while let Some(chunk) = response.chunk().await? {
                if body.len() + chunk.len() > self.options.max_bytes {
                    return Err(FetchError::TooLarge {
                        limit: self.options.max_bytes,
                    });
                }
                body.extend_from_slice(&chunk);
            }

            return Ok((url, status.as_u16(), content_type, body));
        }
    }

    async fn ensure_robots_allowed(&self, url: &Url) -> Result<(), FetchError> {
        if !self.options.respect_robots {
            return Ok(());
        }

        let origin = origin_key(url);
        let cached = {
            let guard = self
                .robots
                .lock()
                .map_err(|_| FetchError::RobotsUnavailable("cache lock poisoned".to_string()))?;
            guard.get(&origin).cloned()
        };

        let body = match cached {
            Some(body) => body,
            None => {
                let body = self.fetch_robots_body(url).await?;
                let mut guard = self.robots.lock().map_err(|_| {
                    FetchError::RobotsUnavailable("cache lock poisoned".to_string())
                })?;
                guard.insert(origin, body.clone());
                body
            }
        };

        if let Some(body) = body {
            if !robots_allows(&body, url) {
                return Err(FetchError::RobotsDisallowed);
            }
        }
        Ok(())
    }

    async fn fetch_robots_body(&self, url: &Url) -> Result<Option<String>, FetchError> {
        let mut robots = url.clone();
        robots.set_path("/robots.txt");
        robots.set_query(None);
        robots.set_fragment(None);

        match Box::pin(self.fetch_bytes(robots, false)).await {
            Ok((_, _, content_type, bytes)) => Ok(Some(decode_text(&bytes, &content_type))),
            // RFC 9309: 4xx oznacza „brak reguł”, czyli brak ograniczeń.
            Err(FetchError::Status(code)) if (400..500).contains(&code) => Ok(None),
            Err(FetchError::UnsupportedContentType(_)) => Ok(None),
            Err(FetchError::TooLarge { .. }) => Err(FetchError::RobotsUnavailable(
                "robots.txt too large".to_string(),
            )),
            Err(error) => Err(FetchError::RobotsUnavailable(format!(
                "{}: {error}",
                error.kind()
            ))),
        }
    }
}

fn validate_allowed_host(url: &Url, allowed_hosts: Option<&[String]>) -> Result<(), FetchError> {
    let Some(allowed_hosts) = allowed_hosts else {
        return Ok(());
    };
    let host = url
        .host_str()
        .ok_or_else(|| FetchError::InvalidUrl("URL has no host".into()))?;
    if allowed_hosts
        .iter()
        .any(|candidate| candidate.eq_ignore_ascii_case(host))
    {
        Ok(())
    } else {
        Err(FetchError::InvalidUrl(format!(
            "host is not on the allowlist: {host}"
        )))
    }
}

fn robots_allows(body: &str, url: &Url) -> bool {
    let mut matcher = DefaultMatcher::default();
    matcher.one_agent_allowed_by_robots(body, "BALD", url.as_str())
}

fn origin_key(url: &Url) -> String {
    let mut origin = url.clone();
    origin.set_path("/");
    origin.set_query(None);
    origin.set_fragment(None);
    origin.to_string()
}

fn decode_text(bytes: &[u8], content_type: &str) -> String {
    let encoding = charset_from_content_type(content_type)
        .or_else(|| {
            if is_html(content_type) {
                sniff_meta_charset(bytes)
            } else {
                None
            }
        })
        .unwrap_or(UTF_8);
    let (body, _, _) = encoding.decode(bytes);
    body.into_owned()
}

fn charset_from_content_type(content_type: &str) -> Option<&'static Encoding> {
    content_type.split(';').find_map(|part| {
        let part = part.trim();
        let (key, value) = part.split_once('=')?;
        if key.trim().eq_ignore_ascii_case("charset") {
            let label = value.trim().trim_matches('"').as_bytes();
            Encoding::for_label(label)
        } else {
            None
        }
    })
}

/// Stare strony PL/EN często deklarują charset wyłącznie w `<meta>`.
fn sniff_meta_charset(bytes: &[u8]) -> Option<&'static Encoding> {
    let head = &bytes[..bytes.len().min(4096)];
    let text = String::from_utf8_lossy(head).to_ascii_lowercase();
    let start = text.find("charset=")? + "charset=".len();
    let label: String = text[start..]
        .trim_start_matches(['"', '\'', ' '])
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
        .collect();
    if label.is_empty() {
        None
    } else {
        Encoding::for_label(label.as_bytes())
    }
}

fn is_html(content_type: &str) -> bool {
    let mime = content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    matches!(mime.as_str(), "text/html" | "application/xhtml+xml")
}

fn is_supported(content_type: &str) -> bool {
    let mime = content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    matches!(
        mime.as_str(),
        "text/html" | "application/xhtml+xml" | "text/plain" | "text/markdown"
    )
}

fn sha256_prefixed(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    format!("sha256:{}", hex::encode(hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_charset() {
        assert_eq!(
            charset_from_content_type("text/html; charset=iso-8859-2"),
            Some(encoding_rs::ISO_8859_2)
        );
        assert_eq!(charset_from_content_type("text/plain"), None);
        assert_eq!(
            sniff_meta_charset(br#"<meta charset="windows-1250">"#),
            Some(encoding_rs::WINDOWS_1250)
        );
        assert_eq!(
            sniff_meta_charset(
                br#"<meta http-equiv="content-type" content="text/html; charset=iso-8859-2">"#
            ),
            Some(encoding_rs::ISO_8859_2)
        );
    }

    #[test]
    fn content_type_policy() {
        assert!(is_supported("text/html; charset=utf-8"));
        assert!(is_supported("text/plain"));
        assert!(!is_supported("application/pdf"));
        assert!(!is_supported("image/png"));
    }

    #[test]
    fn robots_disallow_and_allow_rules() {
        let body = "User-agent: *\nDisallow: /private\nAllow: /private/public\n";
        assert!(robots_allows(
            body,
            &Url::parse("https://example.com/index").unwrap()
        ));
        assert!(!robots_allows(
            body,
            &Url::parse("https://example.com/private/secret").unwrap()
        ));
        assert!(robots_allows(
            body,
            &Url::parse("https://example.com/private/public/page").unwrap()
        ));
    }

    #[test]
    fn robots_specific_agent_wins_over_wildcard() {
        let body = "User-agent: *\nAllow: /\n\nUser-agent: BALD\nDisallow: /\n";
        assert!(!robots_allows(
            body,
            &Url::parse("https://example.com/").unwrap()
        ));
    }

    #[test]
    fn empty_robots_allows_everything() {
        assert!(robots_allows(
            "",
            &Url::parse("https://example.com/anything").unwrap()
        ));
        assert!(FetchOptions::default().respect_robots);
    }

    #[test]
    fn allowlist_rejects_redirect_target_before_network_validation() {
        let allowed = vec!["example.com".to_string()];
        assert!(validate_allowed_host(
            &Url::parse("https://example.com/a").unwrap(),
            Some(&allowed)
        )
        .is_ok());
        assert!(matches!(
            validate_allowed_host(
                &Url::parse("https://other.example/a").unwrap(),
                Some(&allowed)
            ),
            Err(FetchError::InvalidUrl(_))
        ));
    }
}
