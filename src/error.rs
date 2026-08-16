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
    #[error("request attempt timed out")]
    AttemptTimeout,
    #[error("robots policy denied {0}")]
    RobotsDenied(String),
    #[error("redirect denied: {0}")]
    RedirectDenied(String),
    #[error("exceeded the maximum of {limit} redirects")]
    RedirectBudget { limit: u8 },
    #[error("resource budget exhausted: {resource} limit {limit}")]
    ResourceBudget {
        resource: &'static str,
        limit: usize,
    },
    #[error("crawl deadline exceeded")]
    DeadlineExceeded,
    #[error("crawl was cancelled")]
    Cancelled,
    #[error("frontier state is unavailable: {0}")]
    Frontier(String),
    #[error("page analysis failed: {0}")]
    Analysis(String),
    #[error("output sink failed: {0}")]
    Output(String),
}

impl CrawlError {
    pub(crate) fn retryable(&self) -> bool {
        matches!(
            self,
            Self::Network(_)
                | Self::AttemptTimeout
                | Self::HttpStatus {
                    status: 408 | 425 | 429 | 500..=599,
                    ..
                }
        )
    }

    pub(crate) fn status(&self) -> Option<u16> {
        match self {
            Self::HttpStatus { status, .. } => Some(*status),
            _ => None,
        }
    }
}
