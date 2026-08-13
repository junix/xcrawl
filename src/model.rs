use readabilities_rs::{Article, DiscoveredLink, MetaRobots, ReadError};
use serde::{Deserialize, Serialize};
use url::Url;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CrawlPage {
    pub requested_url: Url,
    pub final_url: Url,
    pub depth: usize,
    pub status: u16,
    pub content_type: Option<String>,
    pub body_bytes: usize,
    pub detected_encoding: String,
    pub decode_errors: bool,
    pub canonical_url: Option<Url>,
    pub robots: MetaRobots,
    pub links: Vec<DiscoveredLink>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub article: Option<Article>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub article_error: Option<ReadError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CrawlFailure {
    pub url: Url,
    pub depth: usize,
    pub error: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CrawlStats {
    pub pages_crawled: usize,
    pub pages_failed: usize,
    pub urls_discovered: usize,
    pub urls_filtered: usize,
    pub elapsed_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum CrawlEvent {
    Discovered {
        url: Url,
        depth: usize,
    },
    Page {
        url: Url,
        depth: usize,
        status: u16,
    },
    Failed {
        url: Url,
        depth: usize,
        error: String,
    },
    Complete {
        stats: CrawlStats,
    },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CrawlReport {
    pub pages: Vec<CrawlPage>,
    pub failures: Vec<CrawlFailure>,
    pub events: Vec<CrawlEvent>,
    pub stats: CrawlStats,
}
