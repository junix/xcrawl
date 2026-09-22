use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::Instant;

const MAX_BACKOFF: Duration = Duration::from_secs(60);
const ZERO_FLOOR_BACKOFF: Duration = Duration::from_millis(100);

#[derive(Debug)]
pub(crate) struct OriginScheduler {
    default_delay: Duration,
    max_in_flight: usize,
    state: Mutex<HashMap<String, OriginState>>,
}

#[derive(Debug, Clone)]
struct OriginState {
    next_request: Instant,
    last_reserved_start: Option<Instant>,
    adaptive_delay: Duration,
    robots_delay: Duration,
    retry_after_deadline: Option<Instant>,
    consecutive_successes: u8,
    semaphore: Arc<Semaphore>,
}

#[derive(Debug)]
pub(crate) struct OriginPermit {
    _permit: OwnedSemaphorePermit,
}

impl OriginScheduler {
    pub(crate) fn new(default_delay: Duration, max_in_flight: usize) -> Self {
        Self {
            default_delay,
            max_in_flight,
            state: Mutex::new(HashMap::new()),
        }
    }

    pub(crate) async fn acquire(&self, origin: &str) -> OriginPermit {
        let semaphore = {
            let mut state = self.state.lock().expect("origin scheduler lock poisoned");
            Self::state_for(&mut state, origin, self.max_in_flight)
                .semaphore
                .clone()
        };
        let permit = semaphore
            .acquire_owned()
            .await
            .expect("origin semaphore is never closed");
        let sleep = {
            let mut state = self.state.lock().expect("origin scheduler lock poisoned");
            let now = Instant::now();
            let origin = Self::state_for(&mut state, origin, self.max_in_flight);
            let retry_after = origin.retry_after_deadline.unwrap_or(now);
            let start = origin.next_request.max(retry_after).max(now);
            let delay = self
                .default_delay
                .max(origin.robots_delay)
                .max(origin.adaptive_delay);
            origin.last_reserved_start = Some(start);
            origin.next_request = start.checked_add(delay).unwrap_or(start);
            start.saturating_duration_since(now)
        };
        if !sleep.is_zero() {
            tokio::time::sleep(sleep).await;
        }
        OriginPermit { _permit: permit }
    }

    pub(crate) fn set_robots_delay(&self, origin_key: &str, delay: Duration) {
        let mut state = self.state.lock().expect("origin scheduler lock poisoned");
        let origin = Self::state_for(&mut state, origin_key, self.max_in_flight);
        origin.robots_delay = delay;
        if let Some(last_start) = origin.last_reserved_start {
            let effective = self
                .default_delay
                .max(origin.robots_delay)
                .max(origin.adaptive_delay);
            if let Some(required_next) = last_start.checked_add(effective) {
                origin.next_request = origin.next_request.max(required_next);
            }
        }
    }

    pub(crate) fn record_response(
        &self,
        origin_key: &str,
        status: u16,
        retry_after: Option<Duration>,
    ) {
        let mut state = self.state.lock().expect("origin scheduler lock poisoned");
        let origin = Self::state_for(&mut state, origin_key, self.max_in_flight);
        if let Some(delay) = retry_after {
            if let Some(deadline) = Instant::now().checked_add(delay.min(MAX_BACKOFF)) {
                origin.retry_after_deadline = Some(
                    origin
                        .retry_after_deadline
                        .map_or(deadline, |current| current.max(deadline)),
                );
            }
        }
        if status == 429 {
            origin.consecutive_successes = 0;
            let current = origin
                .adaptive_delay
                .max(self.default_delay)
                .max(ZERO_FLOOR_BACKOFF);
            origin.adaptive_delay = current.saturating_mul(2).min(MAX_BACKOFF);
            if let Some(last_start) = origin.last_reserved_start {
                let effective = self
                    .default_delay
                    .max(origin.robots_delay)
                    .max(origin.adaptive_delay);
                if let Some(required_next) = last_start.checked_add(effective) {
                    origin.next_request = origin.next_request.max(required_next);
                }
            }
        } else if status < 400 {
            origin.consecutive_successes = origin.consecutive_successes.saturating_add(1);
            if origin.consecutive_successes >= 5 {
                let floor = self.default_delay.max(origin.robots_delay);
                let reduced = origin.adaptive_delay / 2;
                origin.adaptive_delay = if reduced > floor {
                    reduced
                } else {
                    Duration::ZERO
                };
                origin.consecutive_successes = 0;
            }
            if origin
                .retry_after_deadline
                .is_some_and(|deadline| deadline <= Instant::now())
            {
                origin.retry_after_deadline = None;
            }
        }
    }

    fn state_for<'a>(
        state: &'a mut HashMap<String, OriginState>,
        origin: &str,
        max_in_flight: usize,
    ) -> &'a mut OriginState {
        state.entry(origin.to_string()).or_insert_with(|| {
            let now = Instant::now();
            OriginState {
                next_request: now,
                last_reserved_start: None,
                adaptive_delay: Duration::ZERO,
                robots_delay: Duration::ZERO,
                retry_after_deadline: None,
                consecutive_successes: 0,
                semaphore: Arc::new(Semaphore::new(max_in_flight)),
            }
        })
    }
}

#[cfg(test)]
#[path = "throttle_tests.rs"]
mod tests;

