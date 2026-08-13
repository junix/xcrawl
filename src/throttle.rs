use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const MAX_BACKOFF: Duration = Duration::from_secs(60);

#[derive(Debug)]
pub(crate) struct PerDomainThrottle {
    default_delay: Duration,
    state: Mutex<HashMap<String, DomainState>>,
}

#[derive(Debug, Clone)]
struct DomainState {
    next_request: Instant,
    adaptive_delay: Option<Duration>,
    robots_delay: Option<Duration>,
    consecutive_successes: u8,
}

impl PerDomainThrottle {
    pub(crate) fn new(default_delay: Duration) -> Self {
        Self {
            default_delay,
            state: Mutex::new(HashMap::new()),
        }
    }

    pub(crate) async fn acquire(&self, domain: &str) {
        let sleep = {
            let mut state = self.state.lock().expect("throttle lock poisoned");
            let now = Instant::now();
            let domain = state.entry(domain.to_string()).or_insert(DomainState {
                next_request: now,
                adaptive_delay: None,
                robots_delay: None,
                consecutive_successes: 0,
            });
            let delay = domain
                .adaptive_delay
                .into_iter()
                .chain(domain.robots_delay)
                .max()
                .unwrap_or(self.default_delay);
            let sleep = domain.next_request.saturating_duration_since(now);
            domain.next_request = now + sleep + delay;
            sleep
        };
        if !sleep.is_zero() {
            tokio::time::sleep(sleep).await;
        }
    }

    pub(crate) fn set_robots_delay(&self, domain: &str, delay: Duration) {
        let mut state = self.state.lock().expect("throttle lock poisoned");
        let now = Instant::now();
        state
            .entry(domain.to_string())
            .or_insert(DomainState {
                next_request: now,
                adaptive_delay: None,
                robots_delay: None,
                consecutive_successes: 0,
            })
            .robots_delay = Some(delay);
    }

    pub(crate) fn record_response(&self, domain: &str, status: u16) {
        let mut state = self.state.lock().expect("throttle lock poisoned");
        let Some(domain) = state.get_mut(domain) else {
            return;
        };
        if status == 429 {
            domain.consecutive_successes = 0;
            let current = domain.adaptive_delay.unwrap_or(self.default_delay);
            domain.adaptive_delay = Some((current * 2).min(MAX_BACKOFF));
        } else if status < 400 {
            domain.consecutive_successes = domain.consecutive_successes.saturating_add(1);
            if domain.consecutive_successes >= 5 {
                domain.adaptive_delay = domain.adaptive_delay.and_then(|delay| {
                    let floor = domain.robots_delay.unwrap_or(self.default_delay);
                    (delay / 2 > floor).then_some(delay / 2)
                });
                domain.consecutive_successes = 0;
            }
        }
    }
}
