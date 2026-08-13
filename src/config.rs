use std::time::Duration;

use ipnet::IpNet;
use serde::{Deserialize, Serialize};

use crate::{CrawlError, Result};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CrawlStrategy {
    #[default]
    BreadthFirst,
    DepthFirst,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RedirectPolicy {
    /// Follow redirects only when the target passes the ordinary crawl scope.
    #[default]
    WithinCrawlScope,
    /// Follow redirects only when scheme, host, and effective port are unchanged.
    SameOrigin,
    /// Follow any redirect that passes the network and resource policies.
    Any,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PathMatchMode {
    /// `/docs` matches `/docs` and `/docs/...`, but not `/docs-old`.
    #[default]
    SegmentPrefix,
    /// Match the percent-encoded URL path using a literal string prefix.
    RawPrefix,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScopeBoundary {
    /// Scheme, host, and effective port must match the seed.
    #[default]
    Origin,
    /// Host must match the seed (or a permitted subdomain).
    Domain,
    /// Any origin may be crawled, subject to the network policy.
    Any,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum PortPolicy {
    /// Permit only the conventional web ports, 80 and 443.
    #[default]
    WebOnly,
    /// Permit any TCP port.
    Any,
    /// Permit only the listed TCP ports.
    Explicit(Vec<u16>),
}

impl PortPolicy {
    pub(crate) fn allows(&self, port: u16) -> bool {
        match self {
            Self::WebOnly => matches!(port, 80 | 443),
            Self::Any => true,
            Self::Explicit(ports) => ports.contains(&port),
        }
    }
}

#[derive(Debug, Clone)]
pub struct TraversalPolicy {
    pub max_depth: usize,
    pub concurrency: usize,
    pub max_origin_in_flight: usize,
    pub max_links_to_analyze: usize,
    pub max_links_to_enqueue: usize,
    pub max_links_to_report: usize,
    pub strategy: CrawlStrategy,
    pub follow_nofollow: bool,
    /// Minimum interval between the starts of requests to one origin.
    pub default_delay: Duration,
}

impl Default for TraversalPolicy {
    fn default() -> Self {
        Self {
            max_depth: 2,
            concurrency: 8,
            max_origin_in_flight: 1,
            max_links_to_analyze: 2_000,
            max_links_to_enqueue: 1_000,
            max_links_to_report: 1_000,
            strategy: CrawlStrategy::BreadthFirst,
            follow_nofollow: false,
            default_delay: Duration::from_millis(250),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ScopePolicy {
    pub boundary: ScopeBoundary,
    pub allow_subdomains: bool,
    pub include_path_prefixes: Vec<String>,
    pub exclude_path_prefixes: Vec<String>,
    pub path_match_mode: PathMatchMode,
    pub redirect_policy: RedirectPolicy,
    pub allow_https_downgrade: bool,
    pub max_redirects: u8,
}

impl Default for ScopePolicy {
    fn default() -> Self {
        Self {
            boundary: ScopeBoundary::Origin,
            allow_subdomains: false,
            include_path_prefixes: Vec::new(),
            exclude_path_prefixes: Vec::new(),
            path_match_mode: PathMatchMode::SegmentPrefix,
            redirect_policy: RedirectPolicy::WithinCrawlScope,
            allow_https_downgrade: false,
            max_redirects: 5,
        }
    }
}

#[derive(Debug, Clone)]
pub struct NetworkPolicy {
    pub deny_non_global: bool,
    pub denied_cidrs: Vec<IpNet>,
    pub allowed_cidrs: Vec<IpNet>,
    pub allowed_ports: PortPolicy,
    pub dns_timeout: Duration,
    pub user_agent: String,
}

impl Default for NetworkPolicy {
    fn default() -> Self {
        Self {
            deny_non_global: true,
            denied_cidrs: Vec::new(),
            allowed_cidrs: Vec::new(),
            allowed_ports: PortPolicy::WebOnly,
            dns_timeout: Duration::from_secs(5),
            user_agent: format!("xcrawl/{}", crate::VERSION),
        }
    }
}

#[derive(Debug, Clone)]
pub struct RobotsPolicy {
    pub respect: bool,
    /// Maximum accepted `Crawl-delay` or derived request-rate interval.
    pub max_delay: Duration,
    /// Redirects followed while resolving `/robots.txt`.
    pub max_redirects: u8,
}

impl Default for RobotsPolicy {
    fn default() -> Self {
        Self {
            respect: true,
            max_delay: Duration::from_secs(60),
            max_redirects: 5,
        }
    }
}

#[derive(Debug, Clone)]
pub struct RetryPolicy {
    /// Total attempts for one hop, including the initial attempt.
    pub max_attempts: u8,
    pub base_delay: Duration,
    pub max_delay: Duration,
    pub honor_retry_after: bool,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            base_delay: Duration::from_millis(100),
            max_delay: Duration::from_secs(5),
            honor_retry_after: true,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ResourceLimits {
    pub max_pages: usize,
    pub max_http_requests: usize,
    pub max_total_download_bytes: usize,
    pub max_unique_origins: usize,
    pub max_frontier_entries: usize,
    pub max_url_length: usize,
    pub max_crawl_duration: Duration,
    pub max_attempt_duration: Duration,
    pub max_response_bytes: usize,
    pub max_robots_bytes: usize,
    pub max_report_bytes: usize,
}

impl Default for ResourceLimits {
    fn default() -> Self {
        Self {
            max_pages: 100,
            max_http_requests: 1_000,
            max_total_download_bytes: 128 * 1024 * 1024,
            max_unique_origins: 32,
            max_frontier_entries: 10_000,
            max_url_length: 8 * 1024,
            max_crawl_duration: Duration::from_secs(10 * 60),
            max_attempt_duration: Duration::from_secs(30),
            max_response_bytes: 8 * 1024 * 1024,
            // RFC 9309 section 2.5 requires support for at least 500 KiB.
            max_robots_bytes: 512 * 1024,
            max_report_bytes: 64 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Clone)]
pub struct OutputPolicy {
    /// Retain the event stream in a collected report. Streaming sinks receive
    /// events regardless of this setting.
    pub collect_events: bool,
    /// Remove query values from URLs written to reports and event sinks.
    pub redact_query_values: bool,
}

impl Default for OutputPolicy {
    fn default() -> Self {
        Self {
            collect_events: true,
            redact_query_values: true,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct CrawlConfig {
    pub traversal: TraversalPolicy,
    pub scope: ScopePolicy,
    pub network: NetworkPolicy,
    pub robots: RobotsPolicy,
    pub retry: RetryPolicy,
    pub limits: ResourceLimits,
    pub output: OutputPolicy,
}

impl CrawlConfig {
    pub fn validate(&self) -> Result<()> {
        let traversal = &self.traversal;
        let limits = &self.limits;
        if limits.max_pages == 0
            || traversal.concurrency == 0
            || traversal.max_origin_in_flight == 0
            || traversal.max_links_to_analyze == 0
            || traversal.max_links_to_enqueue == 0
            || traversal.max_links_to_report == 0
            || limits.max_http_requests == 0
            || limits.max_total_download_bytes == 0
            || limits.max_unique_origins == 0
            || limits.max_frontier_entries == 0
            || limits.max_url_length == 0
            || limits.max_response_bytes == 0
            || limits.max_report_bytes == 0
        {
            return Err(CrawlError::InvalidConfig(
                "page, concurrency, link, request, byte, origin, frontier, URL, and report limits must be positive"
                    .to_string(),
            ));
        }
        if self.robots.respect && limits.max_robots_bytes < 500 * 1024 {
            return Err(CrawlError::InvalidConfig(
                "max_robots_bytes must be at least 500 KiB when robots enforcement is enabled"
                    .to_string(),
            ));
        }
        if limits.max_crawl_duration.is_zero()
            || limits.max_attempt_duration.is_zero()
            || self.network.dns_timeout.is_zero()
            || self.retry.max_delay.is_zero()
        {
            return Err(CrawlError::InvalidConfig(
                "crawl, attempt, DNS, and retry time limits must be positive".to_string(),
            ));
        }
        if self.robots.max_delay > Duration::from_secs(60) {
            return Err(CrawlError::InvalidConfig(
                "robots max_delay must not exceed 60 seconds".to_string(),
            ));
        }
        if limits.max_crawl_duration > Duration::from_secs(24 * 60 * 60) {
            return Err(CrawlError::InvalidConfig(
                "max_crawl_duration must not exceed 24 hours".to_string(),
            ));
        }
        if limits.max_attempt_duration > limits.max_crawl_duration
            || self.network.dns_timeout > limits.max_attempt_duration
            || traversal.default_delay > limits.max_crawl_duration
            || self.retry.max_delay > limits.max_crawl_duration
        {
            return Err(CrawlError::InvalidConfig(
                "attempt, DNS, throttle, and retry durations must fit inside the crawl deadline"
                    .to_string(),
            ));
        }
        if self.retry.max_attempts == 0 || self.retry.max_attempts > 10 {
            return Err(CrawlError::InvalidConfig(
                "retry max_attempts must be between 1 and 10".to_string(),
            ));
        }
        if self.retry.base_delay > self.retry.max_delay {
            return Err(CrawlError::InvalidConfig(
                "retry base_delay must not exceed retry max_delay".to_string(),
            ));
        }
        if self.network.user_agent.trim().is_empty() {
            return Err(CrawlError::InvalidConfig(
                "user_agent must not be empty".to_string(),
            ));
        }
        if self
            .scope
            .include_path_prefixes
            .iter()
            .chain(&self.scope.exclude_path_prefixes)
            .any(|prefix| !prefix.starts_with('/'))
        {
            return Err(CrawlError::InvalidConfig(
                "include and exclude path prefixes must begin with '/'".to_string(),
            ));
        }
        if self.robots.respect && self.robots.max_redirects < 5 {
            return Err(CrawlError::InvalidConfig(
                "robots max_redirects must be at least 5 when enforcement is enabled".to_string(),
            ));
        }
        if matches!(self.network.allowed_ports, PortPolicy::Explicit(ref ports) if ports.is_empty())
        {
            return Err(CrawlError::InvalidConfig(
                "explicit port policy must contain at least one port".to_string(),
            ));
        }
        Ok(())
    }
}
