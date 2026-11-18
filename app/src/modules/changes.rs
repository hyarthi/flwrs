//! Change counters for live views: a service bumps its counter after every
//! change, and a stream (SSE) waits for the next bump to send fresh data.
//! Bumps carry no detail: readers re-read what they show.

use tokio::sync::{Mutex, MutexGuard, watch};

/// A service's write lock whose release counts as a change, so no write path
/// can forget to tell the live views. Paths that hold the lock only to read
/// a stable state use [`ChangeLock::lock_read`].
pub(crate) struct ChangeLock {
    mutex: Mutex<()>,
    changes: watch::Sender<u64>,
}

impl ChangeLock {
    pub(crate) fn new() -> Self {
        Self {
            mutex: Mutex::new(()),
            changes: watch::Sender::new(0),
        }
    }

    /// The lock for a change: bumps the counter when released (after the
    /// lock is, so readers woken by it never wait for the writer).
    pub(crate) async fn lock(&self) -> ChangeGuard<'_> {
        ChangeGuard {
            guard: Some(self.mutex.lock().await),
            changes: &self.changes,
        }
    }

    /// The lock, without counting a change.
    pub(crate) async fn lock_read(&self) -> MutexGuard<'_, ()> {
        self.mutex.lock().await
    }

    /// Counts a change made without the lock.
    pub(crate) fn bump(&self) {
        bump(&self.changes);
    }

    /// A receiver that sees every later bump (`changed().await`).
    pub(crate) fn subscribe(&self) -> watch::Receiver<u64> {
        self.changes.subscribe()
    }
}

pub(crate) struct ChangeGuard<'a> {
    guard: Option<MutexGuard<'a, ()>>,
    changes: &'a watch::Sender<u64>,
}

impl Drop for ChangeGuard<'_> {
    fn drop(&mut self) {
        drop(self.guard.take());
        bump(self.changes);
    }
}

fn bump(changes: &watch::Sender<u64>) {
    // Works without receivers too.
    changes.send_modify(|n| *n = n.wrapping_add(1));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_released_write_lock_is_a_change_a_read_lock_is_not() {
        let lock = ChangeLock::new();
        let mut rx = lock.subscribe();
        drop(lock.lock_read().await);
        assert!(!rx.has_changed().unwrap());
        {
            let _guard = lock.lock().await;
            assert!(!rx.has_changed().unwrap(), "only once released");
        }
        assert!(rx.has_changed().unwrap());
        rx.mark_unchanged();
        lock.bump();
        assert!(rx.has_changed().unwrap());
        // The lock is free again after a change.
        drop(lock.lock().await);
    }
}
