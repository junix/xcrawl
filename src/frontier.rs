use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

use async_trait::async_trait;
use url::Url;

use crate::{CrawlError, CrawlStrategy, Result};

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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
    async fn bind_identity(&self, _identity: &str) -> Result<()> {
        Ok(())
    }
    /// Atomically reserve deduplication keys and enqueue the accepted entries.
    async fn enqueue_if_new(&self, entries: Vec<FrontierEntry>) -> Result<EnqueueResult>;
    async fn pop(&self) -> Result<Option<FrontierEntry>>;
    async fn is_empty(&self) -> Result<bool>;
    async fn pending_count(&self) -> Result<usize> {
        Ok(usize::from(!self.is_empty().await?))
    }
    /// Publish discovered work and acknowledge the parent in one transaction.
    async fn complete(
        &self,
        _entry: &FrontierEntry,
        children: Vec<FrontierEntry>,
    ) -> Result<EnqueueResult> {
        self.enqueue_if_new(children).await
    }
    /// Release this run's outstanding claims on graceful termination.
    async fn release_claims(&self) -> Result<()> {
        Ok(())
    }
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
    seen: HashMap<String, usize>,
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
            if state
                .seen
                .get(&key)
                .is_some_and(|depth| *depth <= entry.depth)
            {
                result.duplicates += 1;
                continue;
            }
            let is_new = !state.seen.contains_key(&key);
            if is_new && state.accepted_total >= self.max_entries {
                result.rejected_capacity += 1;
                continue;
            }
            state.seen.insert(key, entry.depth);
            state.accepted_total += usize::from(is_new);
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
    async fn pending_count(&self) -> Result<usize> {
        Ok(self.lock()?.queue.len())
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
#[path = "frontier_tests.rs"]
mod tests;
