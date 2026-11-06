//! Adapter BALD bse-fetch dla read-only narzędzia web.fetch.
//!
//! Ten crate jest jedynym miejscem, w którym V.I.C.T.O.R. zna BALD. Bazowy
//! victor-integrations pozostaje bez klienta HTTP.

use std::collections::BTreeSet;
use std::time::Duration;

use async_trait::async_trait;
use bse_fetch::{FetchError, FetchOptions, FetchedDocument, Fetcher};
use serde_json::{json, Value};
use url::Url;
use victor_domain::{Capability, RiskLevel, ToolDescriptor, ToolId};
use victor_tools::{ResultTier, ToolError, ToolExecution, ToolExecutor, ToolManifest};

pub const WEB_FETCH_TOOL_ID: &str = "web.fetch";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaldFetchConfig {
    pub allowed_hosts: BTreeSet<String>,
    pub max_bytes: usize,
    pub max_redirects: usize,
    pub timeout: Duration,
    pub respect_robots: bool,
}

impl BaldFetchConfig {
    pub fn new(
        allowed_hosts: impl IntoIterator<Item = String>,
        max_bytes: usize,
        max_redirects: usize,
        timeout: Duration,
    ) -> Result<Self, ToolError> {
        let hosts: BTreeSet<String> = allowed_hosts
            .into_iter()
            .map(|host| host.trim_end_matches('.').to_ascii_lowercase())
            .filter(|host| !host.is_empty())
            .collect();
        if hosts.is_empty() {
            return Err(ToolError::InvalidManifest {
                tool: WEB_FETCH_TOOL_ID.into(),
                reason: "web.fetch wymaga niepustej allowlisty hostów".into(),
            });
        }
        if max_bytes == 0 || max_redirects == 0 || timeout.is_zero() {
            return Err(ToolError::InvalidManifest {
                tool: WEB_FETCH_TOOL_ID.into(),
                reason: "limity web.fetch muszą być większe od zera".into(),
            });
        }
        Ok(Self {
            allowed_hosts: hosts,
            max_bytes,
            max_redirects,
            timeout,
            respect_robots: true,
        })
    }

    pub fn without_robots(mut self) -> Self {
        self.respect_robots = false;
        self
    }

    fn validate_url(&self, raw_url: &str) -> Result<Url, ToolError> {
        let url = Url::parse(raw_url.trim()).map_err(|error| ToolError::Execution {
            tool: WEB_FETCH_TOOL_ID.into(),
            reason: format!("nieprawidłowy URL: {error}"),
        })?;
        if url.scheme() != "https" {
            return Err(ToolError::Execution {
                tool: WEB_FETCH_TOOL_ID.into(),
                reason: "web.fetch wymaga HTTPS".into(),
            });
        }
        let host = url.host_str().ok_or_else(|| ToolError::Execution {
            tool: WEB_FETCH_TOOL_ID.into(),
            reason: "URL nie zawiera hosta".into(),
        })?;
        if !self.allowed_hosts.contains(&host.to_ascii_lowercase()) {
            return Err(ToolError::Execution {
                tool: WEB_FETCH_TOOL_ID.into(),
                reason: format!("host nie jest na allowliście: {host}"),
            });
        }
        Ok(url)
    }
}

pub fn web_fetch_manifest(config: &BaldFetchConfig) -> Result<ToolManifest, ToolError> {
    let descriptor = ToolDescriptor {
        id: ToolId::parse(WEB_FETCH_TOOL_ID).expect("stały identyfikator web.fetch"),
        schema_version: 1,
        description:
            "Pobiera tekstową stronę WWW przez BALD, bez zapisu i bez wykonywania JavaScript".into(),
        input_schema: json!({
            "type": "object",
            "required": ["url"],
            "properties": {"url": {"type": "string"}}
        }),
        output_schema: json!({
            "type": "object",
            "required": ["requested_url", "final_url", "content_hash", "fetched_at", "text"],
            "properties": {
                "requested_url": {"type": "string"},
                "final_url": {"type": "string"},
                "content_hash": {"type": "string"},
                "fetched_at": {"type": "string"},
                "text": {"type": "string"},
                "status": {"type": "integer"}
            }
        }),
        capabilities: vec![Capability::Network {
            provider: "bald".into(),
        }],
        risk: RiskLevel::R0,
        default_timeout_ms: config.timeout.as_millis().min(u64::MAX as u128) as u64,
        undo_support: false,
        path_arguments: vec![],
    };
    Ok(ToolManifest::new("1.0.0", descriptor).with_result_tier(ResultTier::Evidence))
}

pub struct BaldFetchExecutor {
    id: ToolId,
    config: BaldFetchConfig,
    fetcher: Fetcher,
}

impl BaldFetchExecutor {
    pub fn new(config: BaldFetchConfig) -> Result<Self, ToolError> {
        let options = FetchOptions {
            max_bytes: config.max_bytes,
            max_redirects: config.max_redirects,
            timeout: config.timeout,
            respect_robots: config.respect_robots,
            allowed_hosts: Some(config.allowed_hosts.iter().cloned().collect()),
            ..FetchOptions::default()
        };
        let fetcher = Fetcher::new(options).map_err(|error| ToolError::Execution {
            tool: WEB_FETCH_TOOL_ID.into(),
            reason: format!("nie można uruchomić BALD: {}", error.kind()),
        })?;
        Ok(Self {
            id: ToolId::parse(WEB_FETCH_TOOL_ID).expect("stały identyfikator web.fetch"),
            config,
            fetcher,
        })
    }

    pub fn config(&self) -> &BaldFetchConfig {
        &self.config
    }

    fn output(document: FetchedDocument) -> Value {
        json!({
            "requested_url": document.requested_url,
            "final_url": document.final_url,
            "canonical_url": document.canonical_url,
            "status": document.status,
            "content_type": document.content_type,
            "title": document.title,
            "description": document.description,
            "text": document.text,
            "lang": document.lang,
            "published": document.published,
            "content_hash": document.content_hash,
            "html_hash": document.html_hash,
            "fetched_at": document.fetched_at,
            "noindex": document.noindex,
            "cache_hit": document.cache_hit
        })
    }
}

#[async_trait]
impl ToolExecutor for BaldFetchExecutor {
    fn tool_id(&self) -> &ToolId {
        &self.id
    }

    async fn execute(
        &self,
        args: Value,
        started_at_unix_ms: u64,
    ) -> Result<ToolExecution, ToolError> {
        let raw_url =
            args.get("url")
                .and_then(Value::as_str)
                .ok_or_else(|| ToolError::InvalidInput {
                    tool: WEB_FETCH_TOOL_ID.into(),
                    reason: "pole url musi być stringiem".into(),
                })?;
        let url = self.config.validate_url(raw_url)?;
        let document = self
            .fetcher
            .fetch(url.as_str())
            .await
            .map_err(map_fetch_error)?;
        Ok(ToolExecution {
            output: Self::output(document),
            summary: "pobrano dokument przez BALD".into(),
            finished_at_unix_ms: started_at_unix_ms.saturating_add(1),
        })
    }
}

fn map_fetch_error(error: FetchError) -> ToolError {
    ToolError::Execution {
        tool: WEB_FETCH_TOOL_ID.into(),
        reason: format!("BALD {}: {}", error.kind(), safe_error_message(&error)),
    }
}

fn safe_error_message(error: &FetchError) -> String {
    match error {
        FetchError::InvalidUrl(_) => "invalid_url".into(),
        FetchError::Blocked(_) => "ssrf_blocked".into(),
        FetchError::Http(_) => "http_error".into(),
        FetchError::Status(status) => format!("status_{status}"),
        FetchError::TooLarge { limit } => format!("too_large_{limit}"),
        FetchError::UnsupportedContentType(_) => "unsupported_content_type".into(),
        FetchError::TooManyRedirects(limit) => format!("too_many_redirects_{limit}"),
        FetchError::RedirectWithoutLocation => "redirect_without_location".into(),
        FetchError::Extract(_) => "extract_error".into(),
        FetchError::RobotsDisallowed => "robots_disallowed".into(),
        FetchError::RobotsUnavailable(_) => "robots_unavailable".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> BaldFetchConfig {
        BaldFetchConfig::new(["example.com".to_string()], 1024, 3, Duration::from_secs(5)).unwrap()
    }

    #[test]
    fn manifest_is_read_only_and_stores_evidence() {
        let manifest = web_fetch_manifest(&config()).unwrap();
        assert_eq!(manifest.descriptor.risk, RiskLevel::R0);
        assert_eq!(manifest.result_tier, ResultTier::Evidence);
        assert!(manifest.descriptor.has_capability("network"));
    }

    #[test]
    fn rejects_local_private_and_unlisted_hosts_before_fetch() {
        let config = config();
        for url in [
            "https://localhost/",
            "https://127.0.0.1/",
            "https://192.168.1.1/",
            "https://not-allowed.example/",
        ] {
            assert!(config.validate_url(url).is_err(), "{url}");
        }
    }

    #[test]
    fn rejects_http_and_invalid_limits() {
        assert!(config().validate_url("http://example.com/").is_err());
        assert!(
            BaldFetchConfig::new(["example.com".to_string()], 0, 1, Duration::from_secs(1))
                .is_err()
        );
    }

    #[test]
    fn preserves_bald_redirect_and_size_denials_without_response_body() {
        let too_large = map_fetch_error(FetchError::TooLarge { limit: 1024 }).to_string();
        let redirects = map_fetch_error(FetchError::TooManyRedirects(3)).to_string();
        assert!(too_large.contains("too_large_1024"));
        assert!(redirects.contains("too_many_redirects_3"));
        assert!(!too_large.contains("body"));
        assert!(!redirects.contains("body"));
    }
}
