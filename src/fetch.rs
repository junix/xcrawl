use std::collections::BTreeMap;
use std::error::Error as StdError;
use std::fmt;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use futures_util::StreamExt;
use ipnet::IpNet;
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use reqwest::redirect::Policy;
use url::{Host, Url};

use crate::budget::CrawlBudget;
use crate::config::NetworkPolicy;
use crate::{CrawlConfig, CrawlError, Result};

const DEFAULT_DENIED_CIDRS: &[&str] = &[
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
    "::/128",
    "::1/128",
    "64:ff9b:1::/48",
    "100::/64",
    "100:0:0:1::/64",
    "2001::/23",
    "2002::/16",
    "3fff::/20",
    "5f00::/16",
    "fc00::/7",
    "fe80::/10",
    "ff00::/8",
];

const DEFAULT_ALLOWED_EXCEPTIONS: &[&str] = &[
    "192.0.0.9/32",
    "192.0.0.10/32",
    "2001:1::1/128",
    "2001:1::2/128",
    "2001:1::3/128",
    "2001:3::/32",
    "2001:4:112::/48",
    "2001:20::/28",
    "2001:30::/28",
];

#[derive(Debug, Clone)]
pub(crate) struct ResponseHeaders {
    pub(crate) values: BTreeMap<String, String>,
    pub(crate) content_type: Option<String>,
    pub(crate) retry_after: Option<Duration>,
}

#[derive(Debug, Clone)]
pub(crate) struct HopResponse {
    pub(crate) url: Url,
    pub(crate) status: u16,
    pub(crate) headers: ResponseHeaders,
    pub(crate) body: Vec<u8>,
}

#[derive(Debug, Clone)]
pub(crate) enum HopOutcome {
    Response(HopResponse),
    Redirect {
        status: u16,
        location: String,
        headers: ResponseHeaders,
    },
}

#[derive(Debug, Clone)]
pub(crate) struct OneHopTransport {
    client: reqwest::Client,
    network: Arc<NetworkGuard>,
}

impl OneHopTransport {
    pub(crate) fn new(config: &Arc<CrawlConfig>) -> Result<Self> {
        let network = Arc::new(NetworkGuard::new(config.network.clone()));
        let resolver = ValidatingResolver {
            network: Arc::clone(&network),
        };
        let client = reqwest::Client::builder()
            .redirect(Policy::none())
            .no_proxy()
            .timeout(config.limits.max_attempt_duration)
            .connect_timeout(config.limits.max_attempt_duration)
            .dns_resolver(Arc::new(resolver))
            .user_agent(&config.network.user_agent)
            .build()
            .map_err(|error| CrawlError::Network(safe_error_message(&error)))?;
        Ok(Self { client, network })
    }

    pub(crate) async fn send_one_hop(
        &self,
        url: &Url,
        max_body_bytes: usize,
        budget: &CrawlBudget,
    ) -> Result<HopOutcome> {
        validate_url(url, &self.network.policy)?;
        if let Some(ip) = literal_ip(url) {
            self.network.validate_ip(ip)?;
        }
        let response = self
            .client
            .get(url.clone())
            .send()
            .await
            .map_err(|error| map_reqwest_error(&error))?;
        let status = response.status().as_u16();
        let headers = response_headers(response.headers());
        if is_followed_redirect(status) {
            if let Some(location) = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|value| value.to_str().ok())
            {
                return Ok(HopOutcome::Redirect {
                    status,
                    location: location.to_string(),
                    headers,
                });
            }
        }
        if !(200..300).contains(&status) {
            return Ok(HopOutcome::Response(HopResponse {
                url: url.clone(),
                status,
                headers,
                body: Vec::new(),
            }));
        }
        if response
            .content_length()
            .is_some_and(|length| length > max_body_bytes as u64)
        {
            return Err(CrawlError::ResourceBudget {
                resource: "response_bytes",
                limit: max_body_bytes,
            });
        }
        let mut body = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|error| map_reqwest_error(&error))?;
            if body.len().saturating_add(chunk.len()) > max_body_bytes {
                return Err(CrawlError::ResourceBudget {
                    resource: "response_bytes",
                    limit: max_body_bytes,
                });
            }
            budget.reserve_bytes(chunk.len())?;
            body.extend_from_slice(&chunk);
        }
        Ok(HopOutcome::Response(HopResponse {
            url: url.clone(),
            status,
            headers,
            body,
        }))
    }
}

#[derive(Debug)]
struct NetworkGuard {
    policy: NetworkPolicy,
    denied: Vec<IpNet>,
    allowed_exceptions: Vec<IpNet>,
}

impl NetworkGuard {
    fn new(policy: NetworkPolicy) -> Self {
        Self {
            policy,
            denied: parse_cidrs(DEFAULT_DENIED_CIDRS),
            allowed_exceptions: parse_cidrs(DEFAULT_ALLOWED_EXCEPTIONS),
        }
    }

    fn validate_ip(&self, original: IpAddr) -> Result<()> {
        let canonical = canonicalize_ip(original);
        if self
            .policy
            .allowed_cidrs
            .iter()
            .any(|network| network.contains(&original) || network.contains(&canonical))
        {
            return Ok(());
        }
        let explicitly_denied = self
            .policy
            .denied_cidrs
            .iter()
            .any(|network| network.contains(&original) || network.contains(&canonical));
        if explicitly_denied {
            return Err(CrawlError::NetworkDenied(format!(
                "address {original} is denied by the CIDR policy"
            )));
        }
        if self
            .allowed_exceptions
            .iter()
            .any(|network| network.contains(&original) || network.contains(&canonical))
        {
            return Ok(());
        }
        let non_global = self.policy.deny_non_global
            && self
                .denied
                .iter()
                .any(|network| network.contains(&original) || network.contains(&canonical));
        if non_global {
            return Err(CrawlError::NetworkDenied(format!(
                "address {original} is denied by the CIDR policy"
            )));
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
struct ValidatingResolver {
    network: Arc<NetworkGuard>,
}

impl Resolve for ValidatingResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_string();
        let network = Arc::clone(&self.network);
        Box::pin(async move {
            let resolved = tokio::time::timeout(
                network.policy.dns_timeout,
                tokio::net::lookup_host((host.as_str(), 0)),
            )
            .await
            .map_err(|_| Box::new(ResolveFailure::Timeout) as Box<dyn StdError + Send + Sync>)?
            .map_err(|error| {
                Box::new(ResolveFailure::Lookup(error.to_string()))
                    as Box<dyn StdError + Send + Sync>
            })?
            .collect::<Vec<_>>();
            if resolved.is_empty() {
                return Err(Box::new(ResolveFailure::Empty) as Box<dyn StdError + Send + Sync>);
            }
            for address in &resolved {
                network.validate_ip(address.ip()).map_err(|error| {
                    Box::new(ResolveFailure::Denied(error.to_string()))
                        as Box<dyn StdError + Send + Sync>
                })?;
            }
            Ok(Box::new(resolved.into_iter()) as Addrs)
        })
    }
}

#[derive(Debug)]
enum ResolveFailure {
    Timeout,
    Empty,
    Lookup(String),
    Denied(String),
}

impl fmt::Display for ResolveFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Timeout => formatter.write_str("DNS resolution timed out"),
            Self::Empty => formatter.write_str("DNS returned no addresses"),
            Self::Lookup(message) => write!(formatter, "DNS lookup failed: {message}"),
            Self::Denied(message) => write!(formatter, "{message}"),
        }
    }
}

impl StdError for ResolveFailure {}

pub(crate) fn validate_url(url: &Url, policy: &NetworkPolicy) -> Result<()> {
    if !matches!(url.scheme(), "http" | "https") {
        return Err(CrawlError::InvalidUrl(
            "only http and https URLs are supported".to_string(),
        ));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(CrawlError::InvalidUrl(
            "embedded URL credentials are not allowed".to_string(),
        ));
    }
    if url.host().is_none() {
        return Err(CrawlError::InvalidUrl("URL is missing a host".to_string()));
    }
    let port = url
        .port_or_known_default()
        .ok_or_else(|| CrawlError::InvalidUrl("URL is missing a known port".to_string()))?;
    if !policy.allowed_ports.allows(port) {
        return Err(CrawlError::NetworkDenied(format!(
            "TCP port {port} is denied by policy"
        )));
    }
    Ok(())
}

pub(crate) fn safe_url(url: &Url) -> String {
    let mut safe = url.clone();
    let _ = safe.set_username("");
    let _ = safe.set_password(None);
    if safe.query().is_some() {
        let pairs = safe
            .query_pairs()
            .map(|(key, _)| (key.into_owned(), "REDACTED".to_string()))
            .collect::<Vec<_>>();
        safe.query_pairs_mut().clear().extend_pairs(pairs);
    }
    safe.to_string()
}

pub(crate) fn origin_key(url: &Url) -> String {
    format!(
        "{}://{}:{}",
        url.scheme(),
        url.host_str().unwrap_or_default().to_ascii_lowercase(),
        url.port_or_known_default().unwrap_or_default()
    )
}

fn response_headers(headers: &reqwest::header::HeaderMap) -> ResponseHeaders {
    let values = ["x-robots-tag"]
        .into_iter()
        .filter_map(|name| {
            headers
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(|value| (name.to_string(), value.to_string()))
        })
        .collect();
    let content_type = headers
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(ToOwned::to_owned);
    let retry_after = headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(parse_retry_after);
    ResponseHeaders {
        values,
        content_type,
        retry_after,
    }
}

fn parse_retry_after(value: &str) -> Option<Duration> {
    if let Ok(seconds) = value.trim().parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    let deadline = httpdate::parse_http_date(value).ok()?;
    Some(
        deadline
            .duration_since(SystemTime::now())
            .unwrap_or(Duration::ZERO),
    )
}

fn is_followed_redirect(status: u16) -> bool {
    matches!(status, 301 | 302 | 303 | 307 | 308)
}

fn literal_ip(url: &Url) -> Option<IpAddr> {
    match url.host()? {
        Host::Ipv4(ip) => Some(IpAddr::V4(ip)),
        Host::Ipv6(ip) => Some(IpAddr::V6(ip)),
        Host::Domain(_) => None,
    }
}

fn canonicalize_ip(ip: IpAddr) -> IpAddr {
    let IpAddr::V6(ipv6) = ip else {
        return ip;
    };
    if let Some(ipv4) = ipv6.to_ipv4() {
        return IpAddr::V4(ipv4);
    }
    let segments = ipv6.segments();
    if segments[0] == 0x0064 && segments[1] == 0xff9b && segments[2..6] == [0, 0, 0, 0] {
        let octets = ipv6.octets();
        return IpAddr::V4(Ipv4Addr::new(
            octets[12], octets[13], octets[14], octets[15],
        ));
    }
    ip
}

fn parse_cidrs(values: &[&str]) -> Vec<IpNet> {
    values
        .iter()
        .map(|value| value.parse().expect("static CIDR must be valid"))
        .collect()
}

fn map_reqwest_error(error: &reqwest::Error) -> CrawlError {
    let mut source: Option<&(dyn StdError + 'static)> = Some(error);
    while let Some(current) = source {
        if let Some(failure) = current.downcast_ref::<ResolveFailure>() {
            return match failure {
                ResolveFailure::Denied(message) => CrawlError::NetworkDenied(message.clone()),
                ResolveFailure::Timeout => CrawlError::Network("DNS resolution timed out".into()),
                ResolveFailure::Empty => CrawlError::Network("DNS returned no addresses".into()),
                ResolveFailure::Lookup(message) => CrawlError::Network(message.clone()),
            };
        }
        source = current.source();
    }
    if error.is_timeout() {
        CrawlError::AttemptTimeout
    } else {
        CrawlError::Network(safe_error_message(error))
    }
}

fn safe_error_message(error: &(dyn StdError + 'static)) -> String {
    let mut messages = Vec::new();
    let mut source = error.source();
    while let Some(current) = source {
        let message = current.to_string();
        if !message.is_empty() && !messages.contains(&message) {
            messages.push(message);
        }
        source = current.source();
    }
    let message = if messages.is_empty() {
        "request failed".to_string()
    } else {
        messages.join(": ")
    };
    let lower = message.to_ascii_lowercase();
    if ["authorization", "cookie", "token", "api_key", "api-key"]
        .iter()
        .any(|marker| lower.contains(marker))
    {
        "network error contained credential-like data".to_string()
    } else {
        message.chars().take(500).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn guard() -> NetworkGuard {
        NetworkGuard::new(NetworkPolicy::default())
    }

    #[test]
    fn iana_non_global_ranges_and_embedded_ipv4_are_denied() {
        for raw in [
            "100.64.0.1",
            "198.18.0.1",
            "::ffff:127.0.0.1",
            "64:ff9b::7f00:1",
            "64:ff9b:1::1",
            "100::1",
            "3fff::1",
            "5f00::1",
        ] {
            assert!(guard().validate_ip(raw.parse().unwrap()).is_err(), "{raw}");
        }
        assert!(
            guard()
                .validate_ip("2606:4700:4700::1111".parse().unwrap())
                .is_ok()
        );
    }

    #[test]
    fn diagnostics_redact_query_values() {
        let url = Url::parse("https://example.test/path?secret=value&x=1").unwrap();
        let rendered = safe_url(&url);
        assert!(!rendered.contains("value"));
        assert!(!rendered.contains("x=1"));
    }

    #[test]
    fn only_actual_redirect_statuses_are_followed() {
        assert!(is_followed_redirect(301));
        assert!(!is_followed_redirect(304));
    }
}
