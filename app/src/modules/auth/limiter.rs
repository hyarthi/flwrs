//! Failed logins per client address: after `MAX_FAILURES` within `WINDOW`,
//! that address waits for the window to end. Keys are at least 16 bytes, so
//! this isn't what stops guessing; it keeps the log and the CPU quiet.
//! Behind a reverse proxy every client shares the proxy's address.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

const MAX_FAILURES: u32 = 10;
const WINDOW: Duration = Duration::from_secs(60);
/// Addresses tracked at most; beyond it (after dropping expired entries) new
/// addresses aren't counted.
const MAX_TRACKED: usize = 10_000;

pub(crate) struct LoginLimiter {
    max_failures: u32,
    window: Duration,
    /// Address → (window start, failures in it).
    failures: Mutex<HashMap<IpAddr, (Instant, u32)>>,
}

impl LoginLimiter {
    pub(crate) fn new() -> Self {
        Self::with(MAX_FAILURES, WINDOW)
    }

    pub(crate) fn with(max_failures: u32, window: Duration) -> Self {
        Self {
            max_failures,
            window,
            failures: Mutex::new(HashMap::new()),
        }
    }

    /// `Err(wait)` while the address has used up its failures.
    pub(crate) fn check(&self, ip: IpAddr) -> Result<(), Duration> {
        let ip = ip.to_canonical();
        let now = Instant::now();
        let failures = self.failures.lock().unwrap_or_else(PoisonError::into_inner);
        match failures.get(&ip) {
            Some(&(start, count)) if count >= self.max_failures => {
                let left = self.window.saturating_sub(now.duration_since(start));
                if left.is_zero() { Ok(()) } else { Err(left) }
            }
            _ => Ok(()),
        }
    }

    pub(crate) fn failed(&self, ip: IpAddr) {
        let ip = ip.to_canonical();
        let now = Instant::now();
        let mut failures = self.failures.lock().unwrap_or_else(PoisonError::into_inner);
        if !failures.contains_key(&ip) && failures.len() >= MAX_TRACKED {
            let window = self.window;
            failures.retain(|_, (start, _)| now.duration_since(*start) < window);
            if failures.len() >= MAX_TRACKED {
                return;
            }
        }
        let entry = failures.entry(ip).or_insert((now, 0));
        if now.duration_since(entry.0) >= self.window {
            *entry = (now, 0);
        }
        entry.1 += 1;
    }

    pub(crate) fn succeeded(&self, ip: IpAddr) {
        self.failures
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&ip.to_canonical());
    }
}
