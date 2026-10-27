//! 上游限速 (spec §4.5): per-host clone + fetch slots and 冷却. In memory only; callers pass
//! `now`, and anything that must happen when a cooldown ends is theirs to schedule.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rand::RngExt;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::config::ConfigHandle;
use crate::git::GitFailure;

/// Every upstream host's 上游限速, sized and backed off from the live `[upstream]` config.
pub struct UpstreamLimits {
    config: Arc<ConfigHandle>,
    hosts: Mutex<HashMap<String, Host>>,
}

struct Host {
    /// Clone + fetch concurrency for this host, sized `slots_size` (0 = not sized yet).
    slots: Arc<Semaphore>,
    slots_size: usize,
    /// 冷却 end, if one was ever set.
    cool_until: Option<Instant>,
    /// Consecutive rate limits, for the exponential backoff; reset by any success.
    strikes: u32,
}

impl UpstreamLimits {
    pub fn new(config: Arc<ConfigHandle>) -> UpstreamLimits {
        UpstreamLimits {
            config,
            hosts: Mutex::default(),
        }
    }

    /// Waits for one of `host`'s clone + fetch slots.
    pub async fn slot(&self, host: &str) -> OwnedSemaphorePermit {
        let size = self.config.current().upstream.host_concurrency.max(1);
        let slots = self.host(host, |h| {
            // A reloaded `host_concurrency` takes a fresh semaphore.
            // shortcut: permits held on the old one don't count against it, so the host can briefly
            // see old + new slots; drain the old one first if that ever matters.
            if h.slots_size != size {
                (h.slots, h.slots_size) = (Arc::new(Semaphore::new(size)), size);
            }
            h.slots.clone()
        });
        slots.acquire_owned().await.expect("never closed")
    }

    /// Feeds a clone or fetch result into `host`'s 冷却 state: a success resets the backoff; a
    /// rate limit starts (or extends, never shortens) a cooldown for `Retry-After`, else
    /// exponential backoff with jitter. Returns the cooldown's end when this result was a rate limit.
    pub fn record(&self, host: &str, res: Result<(), GitFailure>, now: Instant) -> Option<Instant> {
        let retry_after = match res {
            Ok(()) => {
                self.host(host, |h| h.strikes = 0);
                return None;
            }
            Err(GitFailure::RateLimited { retry_after }) => retry_after,
            Err(_) => return None,
        };
        let limits = self.config.current().upstream.clone();
        self.host(host, |h| {
            h.strikes += 1;
            let backoff = limits
                .cooldown_initial
                .saturating_mul(1 << (h.strikes - 1).min(16))
                .min(limits.cooldown_max)
                .mul_f64(rand::rng().random_range(0.8..=1.0));
            h.cool_until = h.cool_until.max(Some(now + retry_after.unwrap_or(backoff)));
            h.cool_until
        })
    }

    /// When `host`'s 冷却 ends, if it is cooling at `now`.
    pub fn cool_until(&self, host: &str, now: Instant) -> Option<Instant> {
        let hosts = self.hosts.lock().unwrap();
        hosts.get(host)?.cool_until.filter(|t| *t > now)
    }

    pub fn cooling(&self, host: &str, now: Instant) -> bool {
        self.cool_until(host, now).is_some()
    }

    /// Hosts cooling at `now` and the time left, by host (for the admin port).
    pub fn snapshot(&self, now: Instant) -> Vec<(String, Duration)> {
        let hosts = self.hosts.lock().unwrap();
        let mut v: Vec<_> = hosts
            .iter()
            .filter_map(|(h, s)| Some((h.clone(), s.cool_until?.checked_duration_since(now)?)))
            .filter(|(_, left)| !left.is_zero())
            .collect();
        v.sort();
        v
    }

    fn host<T>(&self, host: &str, f: impl FnOnce(&mut Host) -> T) -> T {
        let mut hosts = self.hosts.lock().unwrap();
        f(hosts.entry(host.to_owned()).or_insert_with(|| Host {
            slots: Arc::new(Semaphore::new(0)),
            slots_size: 0,
            cool_until: None,
            strikes: 0,
        }))
    }
}
