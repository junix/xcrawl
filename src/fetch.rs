use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use reqwest::redirect::Policy;
use url::Url;

use crate::{CrawlConfig, CrawlError, Result};

#[derive(Debug, Clone)]
pub(crate) struct FetchedPage {
    pub(crate) final_url: Url,
    pub(crate) status: u16,
    pub(crate) content_type: Option<String>,
    pub(crate) body: Vec<u8>,
    pub(crate) response_headers: BTreeMap<String, String>,
}

#[derive(Debug, Clone)]
pub(crate) struct Fetcher {
    config: Arc<CrawlConfig>,
}

impl Fetcher {
    pub(crate) fn new(config: Arc<CrawlConfig>) -> Self {
        Self { config }
    }

    pub(crate) async fn fetch(&self, url: &Url) -> Result<FetchedPage> {
        let mut attempt = 0_u8;
        loop {
            match self.fetch_once(url).await {
                Ok(page) => return Ok(page),
                Err(error) if error.retryable() && attempt < self.config.max_retries => {
                    let delay = Duration::from_millis(100 * 2_u64.pow(u32::from(attempt)));
                    tokio::time::sleep(delay).await;
                    attempt += 1;
                }
                Err(error) => return Err(error),
            }
        }
    }

    async fn fetch_once(&self, initial_url: &Url) -> Result<FetchedPage> {
        validate_url_shape(initial_url)?;
        let initial_origin = origin(initial_url);
        let mut current = initial_url.clone();
        let mut redirects = 0_u8;

        loop {
            let client = self.client_for(&current).await?;
            let response =
                client.get(current.clone()).send().await.map_err(|error| {
                    CrawlError::Network(redact_network_error(&error.to_string()))
                })?;
            let status = response.status();
            if status.is_redirection() {
                if redirects >= self.config.max_redirects {
                    return Err(CrawlError::RedirectBudget);
                }
                let location = response
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|value| value.to_str().ok())
                    .ok_or_else(|| CrawlError::Network("redirect omitted Location".to_string()))?;
                let next = current
                    .join(location)
                    .map_err(|error| CrawlError::InvalidUrl(error.to_string()))?;
                validate_url_shape(&next)?;
                if !self.config.allow_cross_origin_redirects && origin(&next) != initial_origin {
                    return Err(CrawlError::NetworkDenied(
                        "cross-origin redirect denied by policy".to_string(),
                    ));
                }
                current = next;
                redirects += 1;
                continue;
            }
            if !status.is_success() {
                return Err(CrawlError::HttpStatus {
                    status: status.as_u16(),
                    url: current.to_string(),
                });
            }

            let content_type = response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .map(ToOwned::to_owned);
            let response_headers = ["x-robots-tag"]
                .into_iter()
                .filter_map(|name| {
                    response
                        .headers()
                        .get(name)
                        .and_then(|value| value.to_str().ok())
                        .map(|value| (name.to_string(), value.to_string()))
                })
                .collect();
            if response
                .content_length()
                .is_some_and(|length| length > self.config.max_download_bytes as u64)
            {
                return Err(CrawlError::ByteBudget {
                    limit: self.config.max_download_bytes,
                });
            }
            let mut body = Vec::new();
            let mut stream = response.bytes_stream();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.map_err(|error| {
                    CrawlError::Network(redact_network_error(&error.to_string()))
                })?;
                if body.len().saturating_add(chunk.len()) > self.config.max_download_bytes {
                    return Err(CrawlError::ByteBudget {
                        limit: self.config.max_download_bytes,
                    });
                }
                body.extend_from_slice(&chunk);
            }
            return Ok(FetchedPage {
                final_url: current,
                status: status.as_u16(),
                content_type,
                body,
                response_headers,
            });
        }
    }

    async fn client_for(&self, url: &Url) -> Result<reqwest::Client> {
        let host = url
            .host()
            .ok_or_else(|| CrawlError::InvalidUrl("URL is missing a host".to_string()))?;
        let port = url
            .port_or_known_default()
            .ok_or_else(|| CrawlError::InvalidUrl("URL is missing a known port".to_string()))?;
        let (host_name, addresses) = match host {
            url::Host::Ipv4(ip) => (ip.to_string(), vec![SocketAddr::new(IpAddr::V4(ip), port)]),
            url::Host::Ipv6(ip) => (ip.to_string(), vec![SocketAddr::new(IpAddr::V6(ip), port)]),
            url::Host::Domain(host) => {
                let addresses = tokio::net::lookup_host((host, port))
                    .await
                    .map_err(|error| CrawlError::Network(error.to_string()))?
                    .collect::<Vec<_>>();
                (host.to_string(), addresses)
            }
        };
        if addresses.is_empty() {
            return Err(CrawlError::Network("DNS returned no addresses".to_string()));
        }
        if !self.config.allow_private_networks
            && addresses.iter().any(|address| is_non_public(address.ip()))
        {
            return Err(CrawlError::NetworkDenied(
                "private, loopback, link-local, multicast, and documentation networks are denied"
                    .to_string(),
            ));
        }

        reqwest::Client::builder()
            .redirect(Policy::none())
            .no_proxy()
            .timeout(self.config.request_timeout)
            .resolve_to_addrs(&host_name, &addresses)
            .user_agent(&self.config.user_agent)
            .build()
            .map_err(|error| CrawlError::Network(error.to_string()))
    }
}

fn validate_url_shape(url: &Url) -> Result<()> {
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
    Ok(())
}

fn origin(url: &Url) -> (String, Option<String>, Option<u16>) {
    (
        url.scheme().to_string(),
        url.host_str().map(str::to_ascii_lowercase),
        url.port_or_known_default(),
    )
}

fn is_non_public(ip: IpAddr) -> bool {
    match canonicalize_ip(ip) {
        IpAddr::V4(ip) => {
            ip.is_private()
                || ip.is_loopback()
                || ip.is_link_local()
                || ip.is_unspecified()
                || ip.is_multicast()
                || ip.is_broadcast()
                || ip.is_documentation()
                || ip.octets()[0] == 0
        }
        IpAddr::V6(ip) => {
            let segments = ip.segments();
            ip.is_loopback()
                || ip.is_unspecified()
                || ip.is_multicast()
                || (segments[0] & 0xfe00) == 0xfc00
                || (segments[0] & 0xffc0) == 0xfe80
                || (segments[0] == 0x2001 && segments[1] == 0x0db8)
        }
    }
}

fn canonicalize_ip(ip: IpAddr) -> IpAddr {
    let IpAddr::V6(ipv6) = ip else {
        return ip;
    };
    if let Some(ipv4) = ipv6.to_ipv4_mapped() {
        return IpAddr::V4(ipv4);
    }
    let segments = ipv6.segments();
    if segments[0] == 0x0064 && segments[1] == 0xff9b && segments[2..6] == [0, 0, 0, 0] {
        let octets = ipv6.octets();
        return IpAddr::V4(std::net::Ipv4Addr::new(
            octets[12], octets[13], octets[14], octets[15],
        ));
    }
    ip
}

fn redact_network_error(message: &str) -> String {
    let lower = message.to_ascii_lowercase();
    if ["authorization", "cookie", "token", "api_key"]
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

    #[test]
    fn embedded_private_ipv4_is_denied() {
        for raw in ["::ffff:127.0.0.1", "64:ff9b::7f00:1"] {
            assert!(is_non_public(raw.parse().unwrap()));
        }
        assert!(!is_non_public("2606:4700:4700::1111".parse().unwrap()));
    }
}
