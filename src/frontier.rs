use std::collections::{HashSet, VecDeque};
use std::sync::Mutex;

use async_trait::async_trait;
use url::Url;

use crate::{CrawlError, CrawlStrategy, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrontierEntry {
    pub url: Url,
    pub depth: usize,
}

#[async_trait]
pub trait Frontier: Send + Sync {
    async fn push(&self, entry: FrontierEntry) -> Result<()>;
    async fn pop_batch(&self, limit: usize) -> Result<Vec<FrontierEntry>>;
    async fn mark_seen(&self, key: &str) -> Result<bool>;
    async fn is_empty(&self) -> Result<bool>;
}

#[derive(Debug)]
pub struct InMemoryFrontier {
    strategy: CrawlStrategy,
    state: Mutex<FrontierState>,
}

#[derive(Debug, Default)]
struct FrontierState {
    queue: VecDeque<FrontierEntry>,
    seen: HashSet<String>,
}

impl InMemoryFrontier {
    pub fn new(strategy: CrawlStrategy) -> Self {
        Self {
            strategy,
            state: Mutex::new(FrontierState::default()),
        }
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, FrontierState>> {
        self.state
            .lock()
            .map_err(|error| CrawlError::Frontier(error.to_string()))
    }
}

#[async_trait]
impl Frontier for InMemoryFrontier {
    async fn push(&self, entry: FrontierEntry) -> Result<()> {
        self.lock()?.queue.push_back(entry);
        Ok(())
    }

    async fn pop_batch(&self, limit: usize) -> Result<Vec<FrontierEntry>> {
        let mut state = self.lock()?;
        let mut entries = Vec::with_capacity(limit.min(state.queue.len()));
        for _ in 0..limit {
            let entry = match self.strategy {
                CrawlStrategy::BreadthFirst => state.queue.pop_front(),
                CrawlStrategy::DepthFirst => state.queue.pop_back(),
            };
            match entry {
                Some(entry) => entries.push(entry),
                None => break,
            }
        }
        Ok(entries)
    }

    async fn mark_seen(&self, key: &str) -> Result<bool> {
        Ok(self.lock()?.seen.insert(key.to_string()))
    }

    async fn is_empty(&self) -> Result<bool> {
        Ok(self.lock()?.queue.is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn frontier_order_and_seen_state_are_explicit() {
        let frontier = InMemoryFrontier::new(CrawlStrategy::BreadthFirst);
        assert!(frontier.mark_seen("a").await.unwrap());
        assert!(!frontier.mark_seen("a").await.unwrap());
        for raw in ["https://example.test/a", "https://example.test/b"] {
            frontier
                .push(FrontierEntry {
                    url: Url::parse(raw).unwrap(),
                    depth: 0,
                })
                .await
                .unwrap();
        }
        let batch = frontier.pop_batch(2).await.unwrap();
        assert_eq!(batch[0].url.path(), "/a");
        assert_eq!(batch[1].url.path(), "/b");
        assert!(frontier.is_empty().await.unwrap());
    }
}
