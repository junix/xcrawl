use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use tokio::time::Instant;
use url::Url;

use crate::{CrawlConfig, CrawlError, Result};

#[derive(Debug, Clone, Copy)]
pub(crate) struct BudgetSnapshot {
    pub requests: usize,
    pub bytes: usize,
    pub origins: usize,
}

#[derive(Debug)]
pub(crate) struct CrawlBudget {
    config: Arc<CrawlConfig>,
    deadline: Instant,
    state: Mutex<BudgetState>,
}

#[derive(Debug, Default)]
struct BudgetState {
    requests: usize,
    bytes: usize,
    report_bytes: usize,
    origins: HashSet<String>,
}

impl CrawlBudget {
    pub(crate) fn new(config: Arc<CrawlConfig>) -> Result<Self> {
        let deadline = Instant::now()
            .checked_add(config.limits.max_crawl_duration)
            .ok_or_else(|| {
                CrawlError::InvalidConfig("crawl deadline is outside Instant range".to_string())
            })?;
        Ok(Self {
            deadline,
            config,
            state: Mutex::new(BudgetState::default()),
        })
    }

    pub(crate) fn check_deadline(&self) -> Result<()> {
        if Instant::now() >= self.deadline {
            Err(CrawlError::DeadlineExceeded)
        } else {
            Ok(())
        }
    }

    pub(crate) fn remaining_duration(&self) -> Result<std::time::Duration> {
        self.check_deadline()?;
        Ok(self.deadline.saturating_duration_since(Instant::now()))
    }

    pub(crate) fn reserve_request(&self, url: &Url, origin: &str) -> Result<()> {
        self.check_deadline()?;
        if url.as_str().len() > self.config.limits.max_url_length {
            return Err(CrawlError::ResourceBudget {
                resource: "url_length",
                limit: self.config.limits.max_url_length,
            });
        }
        let mut state = self.state.lock().expect("crawl budget lock poisoned");
        if state.requests >= self.config.limits.max_http_requests {
            return Err(CrawlError::ResourceBudget {
                resource: "http_requests",
                limit: self.config.limits.max_http_requests,
            });
        }
        if !state.origins.contains(origin)
            && state.origins.len() >= self.config.limits.max_unique_origins
        {
            return Err(CrawlError::ResourceBudget {
                resource: "unique_origins",
                limit: self.config.limits.max_unique_origins,
            });
        }
        state.requests += 1;
        state.origins.insert(origin.to_string());
        Ok(())
    }

    pub(crate) fn reserve_bytes(&self, bytes: usize) -> Result<()> {
        let mut state = self.state.lock().expect("crawl budget lock poisoned");
        let Some(total) = state.bytes.checked_add(bytes) else {
            return Err(CrawlError::ResourceBudget {
                resource: "download_bytes",
                limit: self.config.limits.max_total_download_bytes,
            });
        };
        if total > self.config.limits.max_total_download_bytes {
            state.bytes = self.config.limits.max_total_download_bytes;
            return Err(CrawlError::ResourceBudget {
                resource: "download_bytes",
                limit: self.config.limits.max_total_download_bytes,
            });
        }
        state.bytes = total;
        Ok(())
    }

    pub(crate) fn reserve_report_bytes(&self, bytes: usize) -> Result<()> {
        let mut state = self.state.lock().expect("crawl budget lock poisoned");
        let total = state.report_bytes.saturating_add(bytes);
        if total > self.config.limits.max_report_bytes {
            return Err(CrawlError::ResourceBudget {
                resource: "report_bytes",
                limit: self.config.limits.max_report_bytes,
            });
        }
        state.report_bytes = total;
        Ok(())
    }

    pub(crate) fn snapshot(&self) -> BudgetSnapshot {
        let state = self.state.lock().expect("crawl budget lock poisoned");
        BudgetSnapshot {
            requests: state.requests,
            bytes: state.bytes,
            origins: state.origins.len(),
        }
    }
}
