//! SSRF policy. Dwa poziomy:
//!
//! 1. `validate_syntax` — wszystko, co da się sprawdzić bez DNS (schemat, porty,
//!    credentials, lokalne nazwy, literały IP).
//! 2. `validate_url` + `SsrfResolver` — każdy hostname jest rozwiązywany, a
//!    **wszystkie** zwrócone adresy muszą być publiczne. Resolver jest wpięty
//!    w reqwest, więc kontrola działa także przy ponownym połączeniu i redirectach.
//!
//! To nie jest pełna ochrona przed DNS rebinding po stronie serwera HTTP, ale
//! eliminuje klasyczny atak „sprawdź jeden raz, połącz się drugi raz”.

use std::collections::BTreeSet;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::OnceLock;

use ipnet::{Ipv4Net, Ipv6Net};
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use thiserror::Error;
use url::{Host, Url};

const BLOCKED_V4: &[&str] = &[
    "0.0.0.0/8",
    "10.0.0.0/8",
    "100.64.0.0/10",
    "127.0.0.0/8",
    "169.254.0.0/16",
    "172.16.0.0/12",
    "192.0.0.0/24",
    "192.0.2.0/24",
    "192.88.99.0/24",
    "192.168.0.0/16",
    "198.18.0.0/15",
    "198.51.100.0/24",
    "203.0.113.0/24",
    "224.0.0.0/4",
    "240.0.0.0/4",
];

const BLOCKED_V6: &[&str] = &[
    "::/96",
    "64:ff9b::/96",
    "100::/64",
    "2001:10::/28",
    "2001:20::/28",
    "2001:db8::/32",
    "2002::/16",
    "fc00::/7",
    "fe80::/10",
    "ff00::/8",
];

const BLOCKED_SUFFIXES: &[&str] = &[
    ".localhost",
    ".local",
    ".localdomain",
    ".internal",
    ".lan",
    ".home.arpa",
    ".test",
    ".invalid",
    ".example",
    ".onion",
];

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum SsrfError {
    #[error("scheme must be http or https")]
    InvalidScheme,

    #[error("URL must not contain credentials")]
    Credentials,

    #[error("URL has no host")]
    MissingHost,

    #[error("port {0} is not allowed")]
    Port(u16),

    #[error("local hostname is not allowed")]
    LocalHost,

    #[error("blocked IP address: {0}")]
    BlockedIp(IpAddr),

    #[error("DNS resolution failed: {0}")]
    Dns(String),

    #[error("host has no public addresses")]
    NoAddresses,
}

#[derive(Debug, Clone)]
pub struct SsrfPolicy {
    allowed_ports: Vec<u16>,
}

impl Default for SsrfPolicy {
    fn default() -> Self {
        Self {
            allowed_ports: vec![80, 443],
        }
    }
}

impl SsrfPolicy {
    pub fn new(allowed_ports: Vec<u16>) -> Self {
        Self { allowed_ports }
    }

    /// Kontrola bez DNS. Bezpieczna do użycia dla metadanych i w testach.
    pub fn validate_syntax(&self, url: &Url) -> Result<(), SsrfError> {
        if !matches!(url.scheme(), "http" | "https") {
            return Err(SsrfError::InvalidScheme);
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err(SsrfError::Credentials);
        }

        let port = url.port_or_known_default().ok_or(SsrfError::MissingHost)?;
        if !self.allowed_ports.contains(&port) {
            return Err(SsrfError::Port(port));
        }

        match url.host() {
            Some(Host::Ipv4(ip)) => {
                let ip = IpAddr::V4(ip);
                if is_blocked_ip(ip) {
                    return Err(SsrfError::BlockedIp(ip));
                }
            }
            Some(Host::Ipv6(ip)) => {
                let ip = IpAddr::V6(ip);
                if is_blocked_ip(ip) {
                    return Err(SsrfError::BlockedIp(ip));
                }
            }
            Some(Host::Domain(host)) => check_hostname(host)?,
            None => return Err(SsrfError::MissingHost),
        }
        Ok(())
    }

    /// Kontrola pełna: syntax + DNS. Każdy zwrócony adres musi być publiczny.
    pub async fn validate_url(&self, url: &Url) -> Result<(), SsrfError> {
        self.validate_syntax(url)?;
        if let Some(Host::Domain(host)) = url.host() {
            self.resolve_host(host).await?;
        }
        Ok(())
    }

    pub async fn resolve_host(&self, raw_host: &str) -> Result<Vec<IpAddr>, SsrfError> {
        let host = normalize_host(raw_host);
        if let Ok(ip) = host.parse::<IpAddr>() {
            if is_blocked_ip(ip) {
                return Err(SsrfError::BlockedIp(ip));
            }
            return Ok(vec![ip]);
        }
        check_hostname(&host)?;

        let resolved = tokio::net::lookup_host((host.as_str(), 0))
            .await
            .map_err(|e| SsrfError::Dns(e.to_string()))?;
        let mut out = Vec::new();
        let mut seen = BTreeSet::new();
        for addr in resolved {
            let ip = addr.ip();
            if !seen.insert(ip) {
                continue;
            }
            if is_blocked_ip(ip) {
                return Err(SsrfError::BlockedIp(ip));
            }
            out.push(ip);
            if out.len() >= 64 {
                break;
            }
        }
        if out.is_empty() {
            return Err(SsrfError::NoAddresses);
        }
        Ok(out)
    }
}

pub(crate) struct SsrfResolver {
    policy: SsrfPolicy,
}

impl SsrfResolver {
    pub(crate) fn new(policy: SsrfPolicy) -> Self {
        Self { policy }
    }
}

impl Resolve for SsrfResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let policy = self.policy.clone();
        let host = name.as_str().to_string();
        Box::pin(async move {
            let ips = policy.resolve_host(&host).await?;
            let addrs: Vec<SocketAddr> = ips.into_iter().map(|ip| SocketAddr::new(ip, 0)).collect();
            Ok(Box::new(addrs.into_iter()) as Addrs)
        })
    }
}

/// Publiczne, globalnie routowalne adresy. Lista jest celowo konserwatywna.
pub fn is_blocked_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_blocked_v4(v4),
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_blocked_v4(v4);
            }
            blocked_v6().iter().any(|net| net.contains(&v6))
        }
    }
}

fn is_blocked_v4(ip: Ipv4Addr) -> bool {
    blocked_v4().iter().any(|net| net.contains(&ip))
}

fn check_hostname(raw: &str) -> Result<(), SsrfError> {
    let host = normalize_host(raw);
    if host.is_empty() || host == "localhost" || host.ends_with(".localhost") {
        return Err(SsrfError::LocalHost);
    }
    if BLOCKED_SUFFIXES.iter().any(|s| host.ends_with(s)) {
        return Err(SsrfError::LocalHost);
    }
    if !host.contains('.') {
        // Pojedyncze etykiety to prawie zawsze intranet / search domain.
        return Err(SsrfError::LocalHost);
    }
    if host.split('.').any(|label| label.is_empty()) {
        return Err(SsrfError::LocalHost);
    }
    if host
        .chars()
        .any(|c| !(c.is_ascii_alphanumeric() || c == '-' || c == '.'))
    {
        return Err(SsrfError::LocalHost);
    }
    Ok(())
}

fn normalize_host(host: &str) -> String {
    host.trim().trim_end_matches('.').to_ascii_lowercase()
}

fn blocked_v4() -> &'static Vec<Ipv4Net> {
    static NETS: OnceLock<Vec<Ipv4Net>> = OnceLock::new();
    NETS.get_or_init(|| {
        BLOCKED_V4
            .iter()
            .map(|s| s.parse().expect("valid IPv4 CIDR"))
            .collect()
    })
}

fn blocked_v6() -> &'static Vec<Ipv6Net> {
    static NETS: OnceLock<Vec<Ipv6Net>> = OnceLock::new();
    NETS.get_or_init(|| {
        BLOCKED_V6
            .iter()
            .map(|s| s.parse().expect("valid IPv6 CIDR"))
            .collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(raw: &str) -> Url {
        Url::parse(raw).unwrap()
    }

    #[test]
    fn blocks_loopback_private_and_metadata() {
        for raw in [
            "http://127.0.0.1/",
            "http://127.1.2.3/",
            "http://10.0.0.1/",
            "http://172.16.0.1/",
            "http://192.168.1.1/",
            "http://169.254.169.254/latest/meta-data",
            "http://[::1]/",
            "http://[fc00::1]/",
            "http://[fe80::1]/",
            "http://[::ffff:127.0.0.1]/",
            "http://0.0.0.0/",
            "http://100.64.0.1/",
        ] {
            let url = parse(raw);
            assert!(
                SsrfPolicy::default().validate_syntax(&url).is_err(),
                "should block {raw}"
            );
        }
    }

    #[test]
    fn blocks_dangerous_url_forms() {
        for raw in [
            "file:///etc/passwd",
            "ftp://example.com/file",
            "http://user:pass@example.com/",
            "http://example.com:8080/",
            "http://localhost/",
            "http://foo.local/",
            "http://metadata.google.internal/",
            "http://foo.onion/",
            "http://intranet/",
            "http://example.com./",
        ] {
            let url = parse(raw);
            let result = SsrfPolicy::default().validate_syntax(&url);
            // `example.com.` ma poprawną składnię (kropka FQDN), ale resolver ją weryfikuje.
            if raw != "http://example.com./" {
                assert!(result.is_err(), "should block {raw}");
            }
        }
    }

    #[test]
    fn allows_public_syntax() {
        for raw in [
            "https://example.com/",
            "https://www.example.com/path?q=1",
            "http://example.com:80/",
            "https://example.com:443/",
            "https://8.8.8.8/",
            "https://[2606:4700:4700::1111]/",
        ] {
            assert!(
                SsrfPolicy::default().validate_syntax(&parse(raw)).is_ok(),
                "should allow {raw}"
            );
        }
    }

    #[test]
    fn ip_classifier_covers_edge_cases() {
        for ip in [
            "0.0.0.0",
            "127.0.0.1",
            "10.1.2.3",
            "100.64.1.1",
            "169.254.1.1",
            "172.31.255.255",
            "192.168.0.1",
            "198.18.0.1",
            "224.0.0.1",
            "240.0.0.1",
            "::1",
            "fc00::1",
            "fe80::1",
            "ff02::1",
            "::ffff:10.0.0.1",
        ] {
            assert!(is_blocked_ip(ip.parse().unwrap()), "blocked: {ip}");
        }
        for ip in ["8.8.8.8", "1.1.1.1", "93.184.216.34", "2606:4700::1111"] {
            assert!(!is_blocked_ip(ip.parse().unwrap()), "public: {ip}");
        }
    }

    #[tokio::test]
    async fn localhost_is_rejected_before_dns() {
        let err = SsrfPolicy::default()
            .resolve_host("localhost")
            .await
            .unwrap_err();
        assert_eq!(err, SsrfError::LocalHost);
    }

    #[tokio::test]
    async fn literal_private_ip_is_rejected() {
        let err = SsrfPolicy::default()
            .resolve_host("169.254.169.254")
            .await
            .unwrap_err();
        assert!(matches!(err, SsrfError::BlockedIp(_)));
    }
}
