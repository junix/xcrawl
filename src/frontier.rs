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

#[derive(Debug, Clone, Default)]
pub struct EnqueueResult {
    pub enqueued: Vec<FrontierEntry>,
    pub duplicates: usize,
    pub rejected_capacity: usize,
}

#[async_trait]
pub trait Frontier: Send + Sync {
    /// Atomically reserve deduplication keys and enqueue the accepted entries.
    async fn enqueue_if_new(&self, entries: Vec<FrontierEntry>) -> Result<EnqueueResult>;
    async fn pop(&self) -> Result<Option<FrontierEntry>>;
    async fn is_empty(&self) -> Result<bool>;
}

#[derive(Debug)]
pub struct InMemoryFrontier {
    strategy: CrawlStrategy,
    max_entries: usize,
    state: Mutex<FrontierState>,
}

#[derive(Debug, Default)]
struct FrontierState {
    queue: VecDeque<FrontierEntry>,
    seen: HashSet<String>,
    accepted_total: usize,
}

impl InMemoryFrontier {
    pub fn new(strategy: CrawlStrategy, max_entries: usize) -> Self {
        Self {
            strategy,
            max_entries,
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
    async fn enqueue_if_new(&self, entries: Vec<FrontierEntry>) -> Result<EnqueueResult> {
        let mut state = self.lock()?;
        let mut result = EnqueueResult::default();
        for entry in entries {
            let key = normalize_url(&entry.url);
            if state.seen.contains(&key) {
                result.duplicates += 1;
                continue;
            }
            if state.accepted_total >= self.max_entries {
                result.rejected_capacity += 1;
                continue;
            }
            state.seen.insert(key);
            state.accepted_total += 1;
            state.queue.push_back(entry.clone());
            result.enqueued.push(entry);
        }
        Ok(result)
    }

    async fn pop(&self) -> Result<Option<FrontierEntry>> {
        let mut state = self.lock()?;
        Ok(match self.strategy {
            CrawlStrategy::BreadthFirst => state.queue.pop_front(),
            CrawlStrategy::DepthFirst => state.queue.pop_back(),
        })
    }

    async fn is_empty(&self) -> Result<bool> {
        Ok(self.lock()?.queue.is_empty())
    }
}

pub(crate) fn normalize_url(url: &Url) -> String {
    let mut url = url.clone();
    url.set_fragment(None);
    // `url` already canonicalizes scheme/host case and default syntax. Keep
    // query ordering because changing it can change resource identity.
    url.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn enqueue_and_seen_reservation_are_atomic() {
        let frontier = InMemoryFrontier::new(CrawlStrategy::BreadthFirst, 2);
        let entry = FrontierEntry {
            url: Url::parse("https://example.test/a#fragment").unwrap(),
            depth: 0,
        };
        let first = frontier.enqueue_if_new(vec![entry.clone()]).await.unwrap();
        assert_eq!(first.enqueued.len(), 1);
        let duplicate = frontier.enqueue_if_new(vec![entry]).await.unwrap();
        assert_eq!(duplicate.duplicates, 1);
        assert_eq!(frontier.pop().await.unwrap().unwrap().url.path(), "/a");
        assert!(frontier.is_empty().await.unwrap());
    }

    #[tokio::test]
    async fn capacity_bounds_seen_and_queue_state() {
        let frontier = InMemoryFrontier::new(CrawlStrategy::BreadthFirst, 1);
        let entries = ["a", "b"]
            .into_iter()
            .map(|path| FrontierEntry {
                url: Url::parse(&format!("https://example.test/{path}")).unwrap(),
                depth: 0,
            })
            .collect();
        let result = frontier.enqueue_if_new(entries).await.unwrap();
        assert_eq!(result.enqueued.len(), 1);
        assert_eq!(result.rejected_capacity, 1);
    }
}
