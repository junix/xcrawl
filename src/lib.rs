//! Bounded web crawling with page understanding delegated to `readabilities-rs`.
//!
//! `xcrawl` owns traversal and acquisition. It fetches each URL exactly once,
//! then passes the immutable response snapshot to `readabilities-rs` for
//! decoding, full-page link discovery, metadata, and article extraction.

mod analyzer;
mod budget;
mod config;
mod crawler;
mod error;
mod fetch;
mod frontier;
mod model;
mod robots;
mod throttle;

pub use analyzer::{PageAnalyzer, PageInput, ReadabilitiesAnalyzer};
pub use config::{
    CrawlConfig, CrawlStrategy, NetworkPolicy, OutputPolicy, PathMatchMode, PortPolicy,
    RedirectPolicy, ResourceLimits, RetryPolicy, RobotsPolicy, ScopeBoundary, ScopePolicy,
    TraversalPolicy,
};
pub use crawler::Crawler;
pub use error::{CrawlError, Result};
pub use frontier::{EnqueueResult, Frontier, FrontierEntry, InMemoryFrontier};
pub use model::{
    AnalysisError, AnalysisWarning, AnalyzedArticle, AnalyzedLink, ArticleMetadata,
    ArticleProvenance, ArticleSignals, CrawlEvent, CrawlFailure, CrawlFailureDetail, CrawlOutcome,
    CrawlPage, CrawlRecord, CrawlReport, CrawlSink, CrawlSinkError, CrawlStats, CrawlSummary,
    FailureKind, NullCrawlSink, PageAnalysis, PageRobots, RequestKind, RobotsDecision,
};
pub use robots::{RobotsRules, RobotsState};

/// Library version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
