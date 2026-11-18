//! Credit for sending events (capability `CREDITS`), handed out in order.
//!
//! Senders that find no credit wait in a queue; a grant from the hub goes to
//! the longest-waiting ones first, one credit each, reserved for them, so a
//! newcomer can't take it from a sender that was woken for it (only when
//! nobody waits does a sender take free credit at once). A reserved credit
//! that isn't used (the sender gave up, or its write failed before the
//! event went out) passes to the next waiter, or back to the pool.
//!
//! The state is per connection: a new connection starts from zero, and the
//! senders still waiting are woken to look again.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use tokio::sync::oneshot;

#[derive(Default)]
struct State {
    /// The connection the credit belongs to (0: none).
    conn: u64,
    /// Whether the hub enabled `CREDITS` on it.
    enabled: bool,
    available: u64,
    waiting: VecDeque<oneshot::Sender<Grant>>,
}

#[derive(Clone, Default)]
pub(crate) struct Credits(Arc<Mutex<State>>);

/// The right to send one event on connection `conn`. Dropped unused, its
/// credit goes to the next waiter (or back).
pub(crate) struct Grant {
    credits: Option<Credits>,
    pub(crate) conn: u64,
}

impl Grant {
    /// The event went out: the credit is spent.
    pub(crate) fn spend(mut self) {
        self.credits = None;
    }
}

impl Drop for Grant {
    fn drop(&mut self) {
        if let Some(credits) = self.credits.take() {
            let mut state = credits.lock();
            if state.conn == self.conn && state.enabled {
                state.available += 1;
                credits.hand_out(&mut state);
            }
        }
    }
}

/// What [`Credits::try_acquire`] found.
pub(crate) enum Acquire {
    Granted(Grant),
    /// Wait for this (an error: the connection changed; look again).
    Wait(oneshot::Receiver<Grant>),
}

impl Credits {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Passes available credit to waiters, longest-waiting first.
    fn hand_out(&self, state: &mut State) {
        while state.available > 0 {
            let Some(tx) = state.waiting.pop_front() else {
                return;
            };
            state.available -= 1;
            let grant = Grant {
                credits: Some(self.clone()),
                conn: state.conn,
            };
            if let Err(mut unwanted) = tx.send(grant) {
                // That waiter is gone: the credit stays here (no recursion).
                unwanted.credits = None;
                state.available += 1;
            }
        }
    }

    /// A connection's start (`enabled` once the hub has answered), or none
    /// (`conn` 0). Waiting senders are woken to look again.
    pub(crate) fn reset(&self, conn: u64, enabled: bool) {
        let mut state = self.lock();
        *state = State {
            conn,
            enabled,
            ..State::default()
        };
    }

    /// Credit from the hub for connection `conn`.
    pub(crate) fn add(&self, conn: u64, events: u32) {
        let mut state = self.lock();
        if state.conn == conn && state.enabled {
            state.available = state.available.saturating_add(u64::from(events));
            self.hand_out(&mut state);
        }
    }

    /// Credit for connection `conn` now, or a place in the queue.
    pub(crate) fn try_acquire(&self, conn: u64) -> Acquire {
        let mut state = self.lock();
        if state.conn != conn || !state.enabled {
            // No flow control (or a stale connection: the caller checks it).
            return Acquire::Granted(Grant {
                credits: None,
                conn,
            });
        }
        if state.available > 0 && state.waiting.is_empty() {
            state.available -= 1;
            return Acquire::Granted(Grant {
                credits: Some(self.clone()),
                conn,
            });
        }
        let (tx, rx) = oneshot::channel();
        state.waiting.push_back(tx);
        Acquire::Wait(rx)
    }

    /// Whether a send would go out now without waiting.
    pub(crate) fn would_send(&self, conn: u64) -> bool {
        let state = self.lock();
        state.conn != conn || !state.enabled || (state.available > 0 && state.waiting.is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn granted(a: Acquire) -> Grant {
        match a {
            Acquire::Granted(g) => g,
            Acquire::Wait(_) => panic!("expected credit"),
        }
    }

    fn waiting(a: Acquire) -> oneshot::Receiver<Grant> {
        match a {
            Acquire::Wait(rx) => rx,
            Acquire::Granted(_) => panic!("expected to wait"),
        }
    }

    #[test]
    fn grants_go_to_the_longest_waiting_first() {
        let c = Credits::default();
        c.reset(1, true);
        let mut first = waiting(c.try_acquire(1));
        let mut second = waiting(c.try_acquire(1));
        let mut third = waiting(c.try_acquire(1));
        c.add(1, 2);
        first.try_recv().unwrap().spend();
        second.try_recv().unwrap().spend();
        assert!(third.try_recv().is_err(), "no credit left for the third");
        // A newcomer can't jump the queue, even with credit available to the queue.
        c.add(1, 1);
        assert!(matches!(c.try_acquire(1), Acquire::Wait(_)));
        third.try_recv().unwrap().spend();
    }

    #[test]
    fn unused_credit_passes_on_and_gone_waiters_are_skipped() {
        let c = Credits::default();
        c.reset(1, true);
        let gone = waiting(c.try_acquire(1));
        let mut next = waiting(c.try_acquire(1));
        drop(gone);
        c.add(1, 1);
        // The gone waiter is skipped; the next one gets the credit.
        let grant = next.try_recv().unwrap();
        let mut after = waiting(c.try_acquire(1));
        // Dropped unused (e.g. a timeout after it was woken): to the next one.
        drop(grant);
        let grant = after.try_recv().unwrap();
        drop(grant);
        // Nobody waiting: back to the pool, and taken at once.
        assert!(c.would_send(1));
        granted(c.try_acquire(1)).spend();
        assert!(!c.would_send(1));
    }

    #[test]
    fn a_new_connection_starts_over() {
        let c = Credits::default();
        c.reset(1, true);
        c.add(1, 5);
        let mut rx = {
            let _ = granted(c.try_acquire(1));
            c.reset(2, true);
            waiting(c.try_acquire(2))
        };
        // Credit for the old connection is ignored.
        c.add(1, 3);
        assert!(rx.try_recv().is_err());
        // Waiters of the old connection see their channel close.
        c.reset(3, false);
        assert!(rx.try_recv().is_err());
        // Without CREDITS, everything goes at once.
        granted(c.try_acquire(3)).spend();
    }
}
