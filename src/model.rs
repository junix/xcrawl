use serde::{Deserialize, Serialize};
use url::Url;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnalyzedLink {
    pub url: Url,
    pub text: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rel: Vec<String>,
    pub nofollow: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PageRobots {
    pub noindex: bool,
    pub nofollow: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ArticleMetadata {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub published: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modified: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub site: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub canonical_url: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keywords: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArticleSignals {
    pub words: usize,
    pub text_chars: usize,
    pub paragraphs: usize,
    pub headings: usize,
    pub links: usize,
    pub code_blocks: usize,
    pub tables: usize,
    pub score: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArticleProvenance {
    pub engine: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub site_extractor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub site_config: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_url: Option<String>,
    pub degraded: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnalysisWarning {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnalyzedArticle {
    pub content: String,
    pub metadata: ArticleMetadata,
    pub word_count: usize,
    pub quality: String,
    pub signals: ArticleSignals,
    pub provenance: ArticleProvenance,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<AnalysisWarning>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnalysisError {
    pub kind: String,
    pub stage: String,
    pub message: String,
    pub retry: String,
}

#[derive(Debug, Clone)]
pub struct PageAnalysis {
    pub article: Option<AnalyzedArticle>,
    pub article_error: Option<AnalysisError>,
    pub links: Vec<AnalyzedLink>,
    pub canonical_url: Option<Url>,
    pub robots: PageRobots,
    pub detected_encoding: String,
    pub decode_errors: bool,
    pub links_discovered: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CrawlPage {
    pub requested_url: Url,
    pub final_url: Url,
    pub redirect_chain: Vec<Url>,
    pub depth: usize,
    pub status: u16,
    pub content_type: Option<String>,
    pub body_bytes: usize,
    pub detected_encoding: String,
    pub decode_errors: bool,
    pub canonical_url: Option<Url>,
    pub robots: PageRobots,
    pub links_discovered: usize,
    pub links_truncated: bool,
    pub links: Vec<AnalyzedLink>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub article: Option<AnalyzedArticle>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub article_error: Option<AnalysisError>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureKind {
    InvalidUrl,
    NetworkDenied,
    Network,
    HttpStatus,
    Timeout,
    RobotsDenied,
    RedirectDenied,
    RedirectBudget,
    ResourceBudget,
    Deadline,
    Analysis,
    Frontier,
    Output,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CrawlFailureDetail {
    pub kind: FailureKind,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    pub attempts: usize,
    pub redirect_chain: Vec<Url>,
    pub retryable: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CrawlFailure {
    pub url: Url,
    pub depth: usize,
    pub error: CrawlFailureDetail,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestKind {
    Page,
    Robots,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RobotsDecision {
    Rules,
    UnavailableAllow,
    UnreachableDisallow,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CrawlStats {
    pub pages_crawled: usize,
    pub pages_failed: usize,
    pub urls_discovered: usize,
    pub urls_filtered: usize,
    pub http_requests: usize,
    pub downloaded_bytes: usize,
    pub unique_origins: usize,
    pub elapsed_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum CrawlEvent {
    Request {
        kind: RequestKind,
        url: Url,
        attempt: u8,
        status: Option<u16>,
        bytes: usize,
        elapsed_ms: u64,
    },
    Robots {
        origin: String,
        decision: RobotsDecision,
        status: Option<u16>,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
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
        error: CrawlFailureDetail,
    },
    Complete {
        outcome: CrawlOutcome,
        stats: CrawlStats,
    },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CrawlOutcome {
    #[default]
    Complete,
    Partial,
    SeedFailed,
    DeadlineExceeded,
    Cancelled,
    Fatal,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CrawlSummary {
    pub outcome: CrawlOutcome,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub termination_reason: Option<String>,
    pub stats: CrawlStats,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CrawlReport {
    pub outcome: CrawlOutcome,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub termination_reason: Option<String>,
    pub pages: Vec<CrawlPage>,
    pub failures: Vec<CrawlFailure>,
    pub events: Vec<CrawlEvent>,
    pub stats: CrawlStats,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "record", rename_all = "snake_case")]
pub enum CrawlRecord {
    Event { value: CrawlEvent },
    Page { value: Box<CrawlPage> },
    Failure { value: CrawlFailure },
    Summary { value: CrawlSummary },
}

pub trait CrawlSink: Send + Sync {
    fn emit(&self, record: &CrawlRecord) -> std::result::Result<(), CrawlSinkError>;
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum CrawlSinkError {
    #[error("output consumer closed the stream")]
    BrokenPipe,
    #[error("{0}")]
    Other(String),
}

#[derive(Debug, Default)]
pub struct NullCrawlSink;

impl CrawlSink for NullCrawlSink {
    fn emit(&self, _record: &CrawlRecord) -> std::result::Result<(), CrawlSinkError> {
        Ok(())
    }
}
