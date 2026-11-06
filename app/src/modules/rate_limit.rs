//! Keeps repeated log messages from flooding the log.

use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

/// Allows one event per interval (e.g. one WARN per minute for the same
/// problem; the rest go to DEBUG).
pub(crate) struct RateLimit {
    interval: Duration,
    last: Mutex<Option<Instant>>,
}

impl RateLimit {
    pub(crate) fn new(interval: Duration) -> Self {
        Self {
            interval,
            last: Mutex::new(None),
        }
    }

    pub(crate) fn allow(&self) -> bool {
        let mut last = self.last.lock().unwrap_or_else(PoisonError::into_inner);
        let now = Instant::now();
        if last.is_some_and(|t| now.duration_since(t) < self.interval) {
            return false;
        }
        *last = Some(now);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_one_per_interval() {
        let limit = RateLimit::new(Duration::from_secs(60));
        assert!(limit.allow());
        assert!(!limit.allow());
    }
}
