//! Limits on connections still in their handshake (`[hub]
//! max_pending_connections` and `max_pending_per_ip`).
//!
//! A connection past the handshake belongs to an expected node, and each node
//! has at most one; until then, a connection is a stranger that costs a task,
//! a file descriptor and a handshake buffer. These limits bound how many
//! strangers there are at once, so a flood of connections that never
//! authenticate can't use up the server's file descriptors or memory, and
//! (per address) can't take every slot from a real plugin reconnecting.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex, PoisonError};

pub(super) struct PendingLimits {
    /// `0` = no limit.
    max_total: usize,
    /// `0` = no limit. Loopback addresses aren't counted: every local plugin
    /// connects from one, and a scene start may launch many at once.
    max_per_ip: usize,
    counts: Mutex<Counts>,
}

#[derive(Default)]
struct Counts {
    total: usize,
    per_ip: HashMap<IpAddr, usize>,
}

/// A connection's place among the pending ones; released when dropped (at
/// the end of the handshake, whatever its outcome).
pub(super) struct PendingSlot {
    limits: Arc<PendingLimits>,
    /// The address it's counted under, if any.
    ip: Option<IpAddr>,
}

impl PendingLimits {
    pub(super) fn new(max_total: usize, max_per_ip: usize) -> Arc<Self> {
        Arc::new(Self {
            max_total,
            max_per_ip,
            counts: Mutex::default(),
        })
    }

    /// A slot for a new connection from `peer`, or why there's none.
    pub(super) fn admit(self: &Arc<Self>, peer: IpAddr) -> Result<PendingSlot, String> {
        // An IPv4 client on a dual-stack socket shows up as ::ffff:a.b.c.d.
        let peer = peer.to_canonical();
        let per_ip = (self.max_per_ip > 0 && !peer.is_loopback()).then_some(peer);
        let mut counts = self.counts.lock().unwrap_or_else(PoisonError::into_inner);
        if self.max_total > 0 && counts.total >= self.max_total {
            return Err(format!(
                "too many connections in their handshake ({}, max_pending_connections)",
                self.max_total
            ));
        }
        if let Some(ip) = per_ip {
            let from_ip = counts.per_ip.entry(ip).or_default();
            if *from_ip >= self.max_per_ip {
                return Err(format!(
                    "too many connections in their handshake from this address ({}, max_pending_per_ip)",
                    self.max_per_ip
                ));
            }
            *from_ip += 1;
        }
        counts.total += 1;
        Ok(PendingSlot {
            limits: self.clone(),
            ip: per_ip,
        })
    }

    /// Connections in their handshake now.
    pub(super) fn pending(&self) -> usize {
        self.counts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .total
    }
}

impl Drop for PendingSlot {
    fn drop(&mut self) {
        let mut counts = self
            .limits
            .counts
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        counts.total -= 1;
        if let Some(ip) = self.ip
            && let Some(from_ip) = counts.per_ip.get_mut(&ip)
        {
            *from_ip -= 1;
            if *from_ip == 0 {
                counts.per_ip.remove(&ip);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn the_total_limit_counts_every_address_and_frees_on_drop() {
        let limits = PendingLimits::new(2, 0);
        let a = limits.admit(ip("10.0.0.1")).unwrap();
        let _b = limits.admit(ip("127.0.0.1")).unwrap();
        assert!(limits.admit(ip("10.0.0.2")).is_err());
        assert!(limits.admit(ip("127.0.0.1")).is_err());
        // The end of a's handshake frees its slot.
        drop(a);
        let _c = limits.admit(ip("10.0.0.2")).unwrap();
        assert_eq!(limits.pending(), 2);
    }

    #[test]
    fn the_per_address_limit_spares_other_addresses_and_loopback() {
        let limits = PendingLimits::new(0, 2);
        let a = limits.admit(ip("10.0.0.1")).unwrap();
        // The same client over IPv4-mapped IPv6 counts as the same address.
        let _b = limits.admit(ip("::ffff:10.0.0.1")).unwrap();
        assert!(limits.admit(ip("10.0.0.1")).is_err());
        let _other = limits.admit(ip("10.0.0.2")).unwrap();
        // Local plugins all connect from loopback: not limited per address.
        let local: Vec<_> = (0..5)
            .map(|_| limits.admit(ip("127.0.0.1")).unwrap())
            .chain((0..5).map(|_| limits.admit(ip("::1")).unwrap()))
            .collect();
        assert_eq!(local.len(), 10);
        drop(a);
        let _c = limits.admit(ip("10.0.0.1")).unwrap();
        // A refused connection takes no slot.
        assert!(limits.admit(ip("10.0.0.1")).is_err());
        assert_eq!(limits.pending(), 13);
    }

    #[test]
    fn zero_means_no_limit() {
        let limits = PendingLimits::new(0, 0);
        let slots: Vec<_> = (0..1000)
            .map(|_| limits.admit(ip("10.0.0.1")).unwrap())
            .collect();
        assert_eq!(limits.pending(), 1000);
        drop(slots);
        assert_eq!(limits.pending(), 0);
    }
}
