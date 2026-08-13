use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::{CrawlError, Result};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CrawlStrategy {
    #[default]
    BreadthFirst,
    DepthFirst,
}

#[derive(Debug, Clone)]
pub struct CrawlConfig {
    pub max_depth: usize,
    pub max_pages: usize,
    pub concurrency: usize,
    pub max_links_per_page: usize,
    pub strategy: CrawlStrategy,
    pub stay_on_domain: bool,
    pub allow_subdomains: bool,
    pub follow_nofollow: bool,
    pub respect_robots: bool,
    pub include_path_prefixes: Vec<String>,
    pub exclude_path_prefixes: Vec<String>,
    pub default_delay: Duration,
    pub request_timeout: Duration,
    pub max_download_bytes: usize,
    pub max_redirects: u8,
    pub max_retries: u8,
    pub allow_cross_origin_redirects: bool,
    pub allow_private_networks: bool,
    pub user_agent: String,
}

impl Default for CrawlConfig {
    fn default() -> Self {
        Self {
            max_depth: 2,
            max_pages: 100,
            concurrency: 8,
            max_links_per_page: 1_000,
            strategy: CrawlStrategy::BreadthFirst,
            stay_on_domain: true,
            allow_subdomains: false,
            follow_nofollow: false,
            respect_robots: true,
            include_path_prefixes: Vec::new(),
            exclude_path_prefixes: Vec::new(),
            default_delay: Duration::from_millis(250),
            request_timeout: Duration::from_secs(30),
            max_download_bytes: 8 * 1024 * 1024,
            max_redirects: 5,
            max_retries: 2,
            allow_cross_origin_redirects: true,
            allow_private_networks: false,
            user_agent: format!("xcrawl/{}", crate::VERSION),
        }
    }
}

impl CrawlConfig {
    pub fn validate(&self) -> Result<()> {
        if self.max_pages == 0
            || self.concurrency == 0
            || self.max_links_per_page == 0
            || self.max_download_bytes == 0
            || self.request_timeout.is_zero()
        {
            return Err(CrawlError::InvalidConfig(
                "page, concurrency, link, byte, and timeout limits must be positive".to_string(),
            ));
        }
        if self.user_agent.trim().is_empty() {
            return Err(CrawlError::InvalidConfig(
                "user_agent must not be empty".to_string(),
            ));
        }
        Ok(())
    }
}
