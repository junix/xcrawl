use thiserror::Error;

pub type Result<T> = std::result::Result<T, CrawlError>;

#[derive(Debug, Clone, Error)]
pub enum CrawlError {
    #[error("invalid crawl configuration: {0}")]
    InvalidConfig(String),
    #[error("invalid URL: {0}")]
    InvalidUrl(String),
    #[error("network target denied: {0}")]
    NetworkDenied(String),
    #[error("network error: {0}")]
    Network(String),
    #[error("HTTP {status} returned for {url}")]
    HttpStatus { status: u16, url: String },
    #[error("response exceeded byte budget {limit}")]
    ByteBudget { limit: usize },
    #[error("redirect budget exhausted")]
    RedirectBudget,
    #[error("frontier state is unavailable: {0}")]
    Frontier(String),
}

impl CrawlError {
    pub(crate) fn retryable(&self) -> bool {
        matches!(
            self,
            Self::Network(_)
                | Self::HttpStatus {
                    status: 408 | 429 | 500..=599,
                    ..
                }
        )
    }
}
