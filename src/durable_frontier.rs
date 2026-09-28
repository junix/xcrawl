use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use fs2::FileExt;
use rustix::fs::{AtFlags, Mode, OFlags, open, openat, renameat, unlinkat};
use serde::{Deserialize, Serialize};

use crate::frontier::normalize_url;
use crate::{CrawlError, EnqueueResult, Frontier, FrontierEntry, Result};

/// Single-owner durable frontier. The lock excludes concurrent writers; claims
/// survive crashes and are recovered when the next owner opens the directory.
pub struct DurableFrontier {
    directory: File,
    _lock: File,
    capacity: usize,
    state: Mutex<State>,
    failed: std::sync::atomic::AtomicBool,
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct State {
    #[serde(default)]
    identity: Option<String>,
    items: HashMap<String, Item>,
    queue: VecDeque<String>,
}

#[derive(Clone, Serialize, Deserialize)]
struct Item {
    entry: FrontierEntry,
    attempts: usize,
    lease_until_ms: Option<u64>,
    next_attempt_ms: u64,
    done: bool,
}

impl DurableFrontier {
    pub fn open(directory: &Path, capacity: usize) -> Result<Self> {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        if capacity == 0 {
            return Err(failure("frontier capacity must be positive"));
        }
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(directory)
            .map_err(failure)?;
        let metadata = std::fs::symlink_metadata(directory).map_err(failure)?;
        if !metadata.is_dir() || metadata.permissions().mode() & 0o077 != 0 {
            return Err(failure(
                "durable frontier requires a private 0700 directory",
            ));
        }
        let directory = File::from(
            open(
                directory,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW,
                Mode::empty(),
            )
            .map_err(failure)?,
        );
        let lock = File::from(
            openat(
                &directory,
                "owner.lock",
                OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::NONBLOCK,
                Mode::from_bits_truncate(0o600),
            )
            .map_err(failure)?,
        );
        if !lock.metadata().map_err(failure)?.is_file() {
            return Err(failure("frontier lock is not regular"));
        }
        lock.try_lock_exclusive()
            .map_err(|_| failure("frontier is already in use"))?;
        let checkpoint = openat(
            &directory,
            "frontier.json",
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK,
            Mode::empty(),
        );
        let state: State = if let Ok(descriptor) = checkpoint {
            let file = File::from(descriptor);
            if !file.metadata().map_err(failure)?.is_file() {
                return Err(failure("frontier checkpoint is not regular"));
            }
            let mut data = Vec::new();
            file.take(64 * 1024 * 1024 + 1)
                .read_to_end(&mut data)
                .map_err(failure)?;
            if data.len() > 64 * 1024 * 1024 {
                return Err(failure("frontier checkpoint exceeds 64 MiB"));
            }
            serde_json::from_slice(&data).map_err(failure)?
        } else if matches!(checkpoint, Err(rustix::io::Errno::NOENT)) {
            State::default()
        } else {
            return Err(failure(
                "cannot open frontier checkpoint without following links",
            ));
        };
        if state.items.len() > capacity
            || state.queue.iter().any(|key| !state.items.contains_key(key))
            || state
                .items
                .iter()
                .any(|(key, item)| *key != normalize_url(&item.entry.url))
        {
            return Err(failure("invalid or over-capacity frontier checkpoint"));
        }
        let frontier = Self {
            directory,
            _lock: lock,
            capacity,
            state: Mutex::new(state),
            failed: std::sync::atomic::AtomicBool::new(false),
        };
        frontier.transaction(|state| {
            recover(state, true);
            Ok(())
        })?;
        Ok(frontier)
    }

    fn transaction<T>(&self, update: impl FnOnce(&mut State) -> Result<T>) -> Result<T> {
        let mut current = self.state.lock().map_err(failure)?;
        if self.failed.load(std::sync::atomic::Ordering::Acquire) {
            return Err(failure("frontier persistence fault; reopen required"));
        }
        let mut next = current.clone();
        let result = update(&mut next)?;
        // A leftover temporary file is never accepted as committed state.
        let temporary = "frontier.next";
        if let Err(error) = unlinkat(&self.directory, temporary, AtFlags::empty())
            && error != rustix::io::Errno::NOENT
        {
            return Err(failure(error));
        }
        let mut file = File::from(
            openat(
                &self.directory,
                temporary,
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW,
                Mode::from_bits_truncate(0o600),
            )
            .map_err(failure)?,
        );
        file.write_all(&serde_json::to_vec(&next).map_err(failure)?)
            .map_err(failure)?;
        file.sync_all().map_err(failure)?;
        // Fail stop before the first ambiguous commit operation. An error
        // after rename must never leave an older in-memory snapshot writable.
        self.failed
            .store(true, std::sync::atomic::Ordering::Release);
        renameat(&self.directory, temporary, &self.directory, "frontier.json").map_err(failure)?;
        self.directory.sync_all().map_err(failure)?;
        *current = next;
        self.failed
            .store(false, std::sync::atomic::Ordering::Release);
        Ok(result)
    }

    /// Persist a retry deadline and release the claim without losing attempts.
    pub fn retry_at(&self, entry: &FrontierEntry, delay: Duration) -> Result<()> {
        self.transaction(|state| {
            let key = normalize_url(&entry.url);
            let item = state
                .items
                .get_mut(&key)
                .ok_or_else(|| failure("unknown claim"))?;
            item.lease_until_ms = None;
            item.done = false;
            item.next_attempt_ms =
                now_ms().saturating_add(u64::try_from(delay.as_millis()).unwrap_or(u64::MAX));
            if !state.queue.contains(&key) {
                state.queue.push_back(key);
            }
            Ok(())
        })
    }
}

fn enqueue(state: &mut State, entries: Vec<FrontierEntry>, capacity: usize) -> EnqueueResult {
    let mut result = EnqueueResult::default();
    for entry in entries {
        let key = normalize_url(&entry.url);
        if let Some(item) = state.items.get_mut(&key) {
            if item.entry.depth <= entry.depth {
                result.duplicates += 1;
                continue;
            }
            item.entry = entry.clone();
            item.done = false;
        } else {
            if state.items.len() >= capacity {
                result.rejected_capacity += 1;
                continue;
            }
            state.items.insert(
                key.clone(),
                Item {
                    entry: entry.clone(),
                    attempts: 0,
                    lease_until_ms: None,
                    next_attempt_ms: 0,
                    done: false,
                },
            );
        }
        if !state.queue.contains(&key) {
            state.queue.push_back(key);
        }
        result.enqueued.push(entry);
    }
    result
}

fn recover(state: &mut State, all: bool) {
    for (key, item) in &mut state.items {
        if item
            .lease_until_ms
            .is_some_and(|until| all || until <= now_ms())
        {
            item.lease_until_ms = None;
            if !item.done && !state.queue.contains(key) {
                state.queue.push_back(key.clone());
            }
        }
    }
}

#[async_trait]
impl Frontier for DurableFrontier {
    async fn bind_identity(&self, identity: &str) -> Result<()> {
        self.transaction(|state| {
            if state
                .identity
                .as_deref()
                .is_some_and(|saved| saved != identity)
            {
                return Err(failure(
                    "frontier belongs to a different seed or policy; use a new directory",
                ));
            }
            state.identity = Some(identity.to_string());
            Ok(())
        })
    }
    async fn enqueue_if_new(&self, entries: Vec<FrontierEntry>) -> Result<EnqueueResult> {
        self.transaction(|state| Ok(enqueue(state, entries, self.capacity)))
    }
    async fn pop(&self) -> Result<Option<FrontierEntry>> {
        loop {
            let (entry, wait) = self.transaction(|state| {
                recover(state, false);
                let now = now_ms();
                let index = state.queue.iter().position(|key| {
                    state.items[key].next_attempt_ms <= now
                        && state.items[key].lease_until_ms.is_none()
                });
                if let Some(index) = index {
                    let key = state.queue.remove(index).expect("queue index");
                    let item = state.items.get_mut(&key).expect("queued item");
                    item.attempts = item.attempts.saturating_add(1);
                    // Longer than the maximum admitted 24-hour crawl duration.
                    item.lease_until_ms = Some(now.saturating_add(25 * 60 * 60 * 1000));
                    return Ok((Some(item.entry.clone()), None));
                }
                let wait = state
                    .queue
                    .iter()
                    .filter_map(|key| {
                        let item = &state.items[key];
                        item.lease_until_ms
                            .is_none()
                            .then_some(item.next_attempt_ms.saturating_sub(now))
                    })
                    .min();
                Ok((None, wait))
            })?;
            if let Some(wait) = wait {
                tokio::time::sleep(Duration::from_millis(wait.max(1))).await;
            } else {
                return Ok(entry);
            }
        }
    }
    async fn is_empty(&self) -> Result<bool> {
        Ok(self.state.lock().map_err(failure)?.queue.is_empty())
    }
    async fn pending_count(&self) -> Result<usize> {
        Ok(self.state.lock().map_err(failure)?.queue.len())
    }
    async fn complete(
        &self,
        entry: &FrontierEntry,
        children: Vec<FrontierEntry>,
    ) -> Result<EnqueueResult> {
        self.transaction(|state| {
            let key = normalize_url(&entry.url);
            if let Some(item) = state.items.get_mut(&key) {
                item.lease_until_ms = None;
                // A deeper result cannot acknowledge a later shallower visit.
                item.done = item.entry.depth == entry.depth;
            }
            Ok(enqueue(state, children, self.capacity))
        })
    }
    async fn release_claims(&self) -> Result<()> {
        self.transaction(|state| {
            recover(state, true);
            Ok(())
        })
    }
}

fn now_ms() -> u64 {
    u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or(u64::MAX)
}
fn failure(error: impl std::fmt::Display) -> CrawlError {
    CrawlError::Frontier(error.to_string())
}

#[cfg(test)]
#[path = "durable_frontier_tests.rs"]
mod tests;
