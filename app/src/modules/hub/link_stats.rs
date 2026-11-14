//! A node's link diagnostics, across its connections: heartbeat round trips
//! and how its connections end. None of it changes how the hub treats a
//! connection; it's there to spot slow links and setup problems.
//!
//! - Heartbeats: how long the hub's `PING`s take to come back as `PONG`s,
//!   and which never do or come back with a nonce the hub didn't send.
//!   Liveness counts any frame (`docs/protocol.md`, section 9), so these
//!   don't close anything. A slow or growing round trip points at a slow link
//!   or a busy plugin; missed pings at a plugin that stopped reading (or
//!   applies backpressure for a long time); unexpected nonces at a plugin
//!   that doesn't echo them as it should.
//! - Connections: accepted, rejected (declarations that don't match), replaced
//!   by a newer one (e.g. two plugins started with the same id), and lost, by
//!   cause. Closes the hub asks for (stop, token renewal) aren't counted.

use serde::Serialize;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};
use utoipa::ToSchema;

/// Unanswered pings kept per connection; an older one beyond that counts as
/// missed. Pings are missed after three intervals, so normally there are at
/// most three.
const MAX_OUTSTANDING: usize = 16;
/// `last_rtt` before the first answer.
const NO_RTT: u64 = u64::MAX;

/// A node's link numbers, across its connections. They start over when the
/// node does, like its other counters.
#[derive(Default)]
pub(crate) struct LinkStats {
    heartbeats: Heartbeats,
    accepted: AtomicU64,
    rejected: AtomicU64,
    replaced: AtomicU64,
    closed_by_plugin: AtomicU64,
    timed_out: AtomicU64,
    io_errors: AtomicU64,
    violations: AtomicU64,
}

/// How a connection was lost (closes the hub asked for aren't).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Lost {
    /// The plugin closed it without sending `Exit` first.
    ClosedByPlugin,
    /// Nothing from the plugin for three heartbeat intervals.
    TimedOut,
    /// Reading or writing failed (e.g. a connection reset).
    IoError,
    ProtocolViolation,
}

/// A node's link numbers, as reported.
#[derive(Debug, Clone, Default, PartialEq, Serialize, ToSchema)]
pub(crate) struct LinkReport {
    pub heartbeats: HeartbeatStats,
    pub connections: ConnectionStats,
}

/// How the node's connections went.
#[derive(Debug, Clone, Default, PartialEq, Serialize, ToSchema)]
pub(crate) struct ConnectionStats {
    /// Connections that passed the handshake.
    pub accepted: u64,
    /// Handshakes refused because the plugin's declarations (type, version,
    /// schemas, protocol) don't match the node. Wrong or missing credentials
    /// can't be told apart from strangers, so they're only logged.
    pub rejected: u64,
    /// Connections closed because a newer one for the node was accepted.
    pub replaced: u64,
    /// Lost connections, by cause: closed by the plugin without `Exit`, …
    pub closed_by_plugin: u64,
    /// … silent for three heartbeat intervals, …
    pub timed_out: u64,
    /// … a read or write error (e.g. a reset), …
    pub io_errors: u64,
    /// … or a protocol violation.
    pub protocol_violations: u64,
}

impl LinkStats {
    pub(crate) fn report(&self) -> LinkReport {
        let n = |c: &AtomicU64| c.load(Ordering::Relaxed);
        LinkReport {
            heartbeats: self.heartbeats.stats(),
            connections: ConnectionStats {
                accepted: n(&self.accepted),
                rejected: n(&self.rejected),
                replaced: n(&self.replaced),
                closed_by_plugin: n(&self.closed_by_plugin),
                timed_out: n(&self.timed_out),
                io_errors: n(&self.io_errors),
                protocol_violations: n(&self.violations),
            },
        }
    }

    pub(super) fn accepted(&self, replaced_one: bool) {
        self.accepted.fetch_add(1, Ordering::Relaxed);
        if replaced_one {
            self.replaced.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub(super) fn rejected(&self) {
        self.rejected.fetch_add(1, Ordering::Relaxed);
    }

    pub(super) fn lost(&self, cause: Lost) {
        let counter = match cause {
            Lost::ClosedByPlugin => &self.closed_by_plugin,
            Lost::TimedOut => &self.timed_out,
            Lost::IoError => &self.io_errors,
            Lost::ProtocolViolation => &self.violations,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }
}

/// The hub's connections right now, and its refusals so far (connections
/// not tied to a node).
#[derive(Debug, Clone, Default, PartialEq, Serialize, ToSchema)]
pub(crate) struct HubConnections {
    /// Connections past the handshake (at most one per node).
    pub connected: u64,
    /// Connections still in their handshake.
    pub pending: u64,
    /// Connections closed at once: too many in their handshake
    /// (`[hub] max_pending_connections` / `max_pending_per_ip`).
    pub refused_busy: u64,
    /// Handshakes that failed before reaching a node: no or a bad
    /// `Initialize` in time, a failed TLS handshake, an unknown instance id or
    /// wrong credentials.
    pub failed_handshakes: u64,
}

/// Heartbeat numbers.
struct Heartbeats {
    answered: AtomicU64,
    missed: AtomicU64,
    unexpected: AtomicU64,
    /// The latest round trip, in microseconds (`NO_RTT`: none yet).
    last_rtt: AtomicU64,
}

impl Default for Heartbeats {
    fn default() -> Self {
        Self {
            answered: AtomicU64::new(0),
            missed: AtomicU64::new(0),
            unexpected: AtomicU64::new(0),
            last_rtt: AtomicU64::new(NO_RTT),
        }
    }
}

/// A node's heartbeat numbers, as reported.
#[derive(Debug, Clone, Default, PartialEq, Serialize, ToSchema)]
pub(crate) struct HeartbeatStats {
    /// The latest `PING` → `PONG` round trip, in milliseconds. `None` before
    /// the first answer (or with heartbeats off).
    pub last_rtt_ms: Option<f64>,
    /// The hub's pings answered with their nonce.
    pub answered: u64,
    /// The hub's pings not answered within three heartbeat intervals (pings
    /// still waiting when a connection closes don't count).
    pub missed: u64,
    /// `PONG`s whose nonce the hub wasn't waiting for: never sent, answered
    /// twice, or answered after counting as missed.
    pub unexpected: u64,
}

impl Heartbeats {
    fn stats(&self) -> HeartbeatStats {
        let rtt = self.last_rtt.load(Ordering::Relaxed);
        HeartbeatStats {
            last_rtt_ms: (rtt != NO_RTT).then(|| rtt as f64 / 1000.0),
            answered: self.answered.load(Ordering::Relaxed),
            missed: self.missed.load(Ordering::Relaxed),
            unexpected: self.unexpected.load(Ordering::Relaxed),
        }
    }
}

/// One connection's pings waiting for their `PONG`.
pub(super) struct Pings {
    /// `(nonce, sent)`, oldest first.
    outstanding: Mutex<VecDeque<(u64, Instant)>>,
    node: Arc<LinkStats>,
}

impl Pings {
    pub(super) fn new(node: Arc<LinkStats>) -> Self {
        Self {
            outstanding: Mutex::default(),
            node,
        }
    }

    /// A `PING` with `nonce` is going out now. Pings older than `horizon`
    /// (three heartbeat intervals) without an answer count as missed.
    pub(super) fn sent(&self, nonce: u64, now: Instant, horizon: Duration) {
        let mut outstanding = self.lock();
        let mut missed = 0;
        while let Some(&(_, sent)) = outstanding.front()
            && (now.saturating_duration_since(sent) >= horizon
                || outstanding.len() >= MAX_OUTSTANDING)
        {
            outstanding.pop_front();
            missed += 1;
        }
        outstanding.push_back((nonce, now));
        if missed > 0 {
            self.node.heartbeats.missed.fetch_add(missed, Ordering::Relaxed);
        }
    }

    /// A `PONG` with `nonce` arrived now.
    pub(super) fn answered(&self, nonce: u64, now: Instant) {
        let sent = {
            let mut outstanding = self.lock();
            outstanding
                .iter()
                .position(|&(n, _)| n == nonce)
                .and_then(|i| outstanding.remove(i))
                .map(|(_, sent)| sent)
        };
        match sent {
            Some(sent) => {
                let rtt = now.saturating_duration_since(sent);
                let micros = u64::try_from(rtt.as_micros()).unwrap_or(NO_RTT - 1);
                self.node.heartbeats.last_rtt.store(micros, Ordering::Relaxed);
                self.node.heartbeats.answered.fetch_add(1, Ordering::Relaxed);
            }
            None => {
                self.node.heartbeats.unexpected.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// The node's numbers this connection adds to.
    pub(super) fn stats(&self) -> &LinkStats {
        &self.node
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, VecDeque<(u64, Instant)>> {
        self.outstanding
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HORIZON: Duration = Duration::from_secs(30);

    fn pings() -> (Pings, Arc<LinkStats>) {
        let node = Arc::new(LinkStats::default());
        (Pings::new(node.clone()), node)
    }

    #[test]
    fn an_answer_records_the_round_trip() {
        let (pings, node) = pings();
        assert_eq!(node.heartbeats.stats(), HeartbeatStats::default());
        let start = Instant::now();
        pings.sent(1, start, HORIZON);
        pings.sent(2, start + Duration::from_secs(10), HORIZON);
        // Answers may come in any order.
        pings.answered(2, start + Duration::from_millis(10_002));
        assert_eq!(node.heartbeats.stats().last_rtt_ms, Some(2.0));
        pings.answered(1, start + Duration::from_millis(10_500));
        let stats = node.heartbeats.stats();
        assert_eq!(stats.last_rtt_ms, Some(10_500.0));
        assert_eq!((stats.answered, stats.missed, stats.unexpected), (2, 0, 0));
    }

    #[test]
    fn unknown_and_repeated_nonces_are_unexpected() {
        let (pings, node) = pings();
        let start = Instant::now();
        pings.answered(5, start); // never sent
        pings.sent(1, start, HORIZON);
        pings.answered(1, start);
        pings.answered(1, start); // twice
        let stats = node.heartbeats.stats();
        assert_eq!((stats.answered, stats.missed, stats.unexpected), (1, 0, 2));
    }

    #[test]
    fn pings_unanswered_past_the_horizon_are_missed() {
        let (pings, node) = pings();
        let start = Instant::now();
        let at = |s| start + Duration::from_secs(s);
        pings.sent(1, at(0), HORIZON);
        pings.sent(2, at(10), HORIZON);
        pings.sent(3, at(20), HORIZON);
        assert_eq!(node.heartbeats.stats().missed, 0);
        // Nonce 1 is 30 s old when nonce 4 goes out.
        pings.sent(4, at(30), HORIZON);
        assert_eq!(node.heartbeats.stats().missed, 1);
        // A late answer to it no longer matches.
        pings.answered(1, at(31));
        pings.answered(2, at(31));
        let stats = node.heartbeats.stats();
        assert_eq!((stats.answered, stats.missed, stats.unexpected), (1, 1, 1));
        assert_eq!(stats.last_rtt_ms, Some(21_000.0));
    }

    #[test]
    fn at_most_a_bounded_number_wait() {
        let (pings, node) = pings();
        let start = Instant::now();
        // A very long horizon: only the bound applies.
        for nonce in 1..=(MAX_OUTSTANDING as u64 + 4) {
            pings.sent(nonce, start, Duration::MAX);
        }
        assert_eq!(node.heartbeats.stats().missed, 4);
        assert_eq!(pings.lock().len(), MAX_OUTSTANDING);
    }
}
