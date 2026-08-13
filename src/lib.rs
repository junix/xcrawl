//! Bounded web crawling with page understanding delegated to `readabilities-rs`.
//!
//! `xcrawl` owns traversal and acquisition. It fetches each URL exactly once,
//! then passes the immutable response snapshot to `readabilities-rs` for
//! decoding, full-page link discovery, metadata, and article extraction.

mod config;
mod crawler;
mod error;
mod fetch;
mod frontier;
mod model;
mod robots;
mod throttle;

pub use config::{CrawlConfig, CrawlStrategy};
pub use crawler::Crawler;
pub use error::{CrawlError, Result};
pub use frontier::{Frontier, FrontierEntry, InMemoryFrontier};
pub use model::{CrawlEvent, CrawlFailure, CrawlPage, CrawlReport, CrawlStats};
pub use robots::RobotsRules;

/// Library version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
