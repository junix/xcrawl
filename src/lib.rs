//! Bounded web crawling with page understanding delegated to `readabilities-rs`.
//!
//! `xcrawl` owns traversal and acquisition. It deduplicates URLs by minimum depth,
//! then passes the immutable response snapshot to `readabilities-rs` for
//! decoding, full-page link discovery, metadata, and article extraction.

mod analyzer;
mod budget;
mod config;
mod crawler;
mod durable_frontier;
mod error;
mod fetch;
mod frontier;
mod model;
mod resource;
mod robots;
mod throttle;

pub use analyzer::{PageAnalyzer, PageInput, ReadabilitiesAnalyzer};
pub use config::{
    CrawlConfig, CrawlStrategy, NetworkPolicy, OutputPolicy, PathMatchMode, PortPolicy,
    RedirectPolicy, ResourceLimits, RetryPolicy, RobotsPolicy, ScopeBoundary, ScopePolicy,
    TraversalPolicy,
};
pub use crawler::Crawler;
pub use durable_frontier::DurableFrontier;
pub use error::{CrawlError, Result};
pub use fetch::safe_url;
pub use frontier::{EnqueueResult, Frontier, FrontierEntry, InMemoryFrontier};
pub use model::{
    AnalysisError, AnalysisWarning, AnalyzedArticle, AnalyzedLink, ArticleMetadata,
    ArticleProvenance, ArticleSignals, CrawlEvent, CrawlFailure, CrawlFailureDetail, CrawlOutcome,
    CrawlPage, CrawlRecord, CrawlReport, CrawlSink, CrawlSinkError, CrawlStats, CrawlSummary,
    FailureKind, NullCrawlSink, PageAnalysis, PageRobots, RequestKind, RobotsDecision,
};
pub use resource::ResourceCandidate;
pub use robots::{RobotsRules, RobotsState};

/// Library version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
