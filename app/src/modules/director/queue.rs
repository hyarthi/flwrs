//! A node's bounded inbound queue.
//!
//! **Without spill:** a memory FIFO limited by event count and bytes, plus a
//! hub-wide byte budget shared by every queue.
//!
//! **With spill** (`runtime.queue.spill.enabled`): the memory FIFO has a soft
//! limit (`spill.memory_max_*`, plus a hub-wide soft limit); beyond it, new
//! events go to a per-node SQLite spool file, which has the hard limits
//! (`spill.disk_max_*`, a hub-wide disk limit, and a free-space guard). Once
//! anything is on disk, every new event goes to disk too (strict FIFO); memory
//! is refilled from disk when it drops below the refill watermark, and the
//! queue goes back to memory-only when the disk is empty. A single background
//! task does all disk I/O (writes and refills), so they can't race. See
//! [`spill`].

mod spill;

use crate::modules::hub::wire::DataEvent;
use crate::modules::node_config::WhenFull;
use crate::modules::supervisor::SpillSpec;
use serde::Serialize;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinHandle;
use tokio::time::Instant;

/// Bytes of queued events across the whole hub (queues without spill). Each
/// queued event holds one permit per byte until it has been handed to the
/// plugin's connection.
#[derive(Clone)]
pub(crate) struct Budget {
    semaphore: Arc<Semaphore>,
    total: usize,
}

impl Budget {
    pub(crate) fn new(bytes: usize) -> Self {
        let total = bytes.min(Semaphore::MAX_PERMITS);
        Self {
            semaphore: Arc::new(Semaphore::new(total)),
            total,
        }
    }

    pub(crate) fn available(&self) -> usize {
        self.semaphore.available_permits()
    }

    pub(crate) fn total(&self) -> usize {
        self.total
    }
}

/// A shared byte count with a limit.
pub(crate) struct Gauge {
    used: AtomicU64,
    limit: u64,
}

impl Gauge {
    fn new(limit: u64) -> Self {
        Self {
            used: AtomicU64::new(0),
            limit,
        }
    }

    /// Adds `n` if that stays within the limit.
    fn try_add(&self, n: u64) -> bool {
        let mut used = self.used.load(Ordering::Acquire);
        loop {
            let Some(total) = used.checked_add(n).filter(|total| *total <= self.limit) else {
                return false;
            };
            match self
                .used
                .compare_exchange_weak(used, total, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return true,
                Err(actual) => used = actual,
            }
        }
    }

    /// Adds `n` even past the limit (a soft limit, e.g. when refilling).
    fn add(&self, n: u64) {
        self.used.fetch_add(n, Ordering::AcqRel);
    }

    fn sub(&self, n: u64) {
        let mut used = self.used.load(Ordering::Acquire);
        loop {
            match self.used.compare_exchange_weak(
                used,
                used.saturating_sub(n),
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return,
                Err(actual) => used = actual,
            }
        }
    }

    pub(crate) fn used(&self) -> u64 {
        self.used.load(Ordering::Acquire)
    }
}

/// What all spilling queues share: where files go, and the hub-wide limits.
#[derive(Clone)]
pub(crate) struct SpillShared {
    pub spool_dir: PathBuf,
    /// For nodes that don't set `spill.enabled`.
    pub default_enabled: bool,
    /// `[director] memory_soft_inflight_bytes`.
    memory: Arc<Gauge>,
    /// `[director] spill_max_bytes`.
    disk: Arc<Gauge>,
    /// `[director] spill_min_free_bytes`.
    min_free_bytes: u64,
}

impl SpillShared {
    pub(crate) fn new(
        spool_dir: PathBuf,
        default_enabled: bool,
        memory_soft_bytes: u64,
        disk_max_bytes: u64,
        min_free_bytes: u64,
    ) -> Self {
        Self {
            spool_dir,
            default_enabled,
            memory: Arc::new(Gauge::new(memory_soft_bytes)),
            disk: Arc::new(Gauge::new(disk_max_bytes)),
            min_free_bytes,
        }
    }

    /// `(used, limit)` of the hub-wide memory soft limit.
    pub(crate) fn memory_usage(&self) -> (u64, u64) {
        (self.memory.used(), self.memory.limit)
    }

    /// `(used, limit)` of the hub-wide disk limit.
    pub(crate) fn disk_usage(&self) -> (u64, u64) {
        (self.disk.used(), self.disk.limit)
    }

    /// `<spool_dir>/<scene_id>`.
    pub(crate) fn scene_dir(&self, scene_id: &str) -> PathBuf {
        self.spool_dir.join(scene_id)
    }
}

/// A queue's spill configuration.
#[derive(Clone)]
pub(crate) struct SpillSettings {
    pub spec: SpillSpec,
    /// `<spool_dir>/<scene-id>/<node-id>.<ulid>.db`; created on the first spill.
    pub file: PathBuf,
    pub shared: SpillShared,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct QueueSettings {
    /// Without spill: the hard limits.
    pub capacity: usize,
    pub max_bytes: usize,
    pub when_full: WhenFull,
    pub block_timeout: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DropReason {
    /// Larger than the queue (or the whole budget) could ever hold.
    Oversized,
    /// The queue was full (`drop_newest`).
    Full,
    /// `block` waited `block_timeout` for room.
    Timeout,
    /// The hub-wide budget was used up (`drop_*`).
    Budget,
    /// The queue was closed (scene stopping).
    Closed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Pushed {
    Queued,
    Dropped(DropReason),
}

/// What a queued event holds until it's delivered (or dropped): held only
/// for its `Drop`, which gives the bytes back.
#[allow(dead_code)]
enum Hold {
    /// Bytes of the hub-wide budget (queues without spill).
    Budget(OwnedSemaphorePermit),
    /// Bytes of the hub-wide memory soft limit (spilling queues).
    Soft(SoftShare),
}

struct SoftShare {
    gauge: Arc<Gauge>,
    bytes: u64,
}

impl Drop for SoftShare {
    fn drop(&mut self) {
        self.gauge.sub(self.bytes);
    }
}

/// An event waiting in memory; dropping it releases what it holds.
pub(crate) struct Queued {
    pub event: DataEvent,
    pub size: usize,
    _hold: Hold,
}

/// An event waiting to be written to the spool file.
struct Pending {
    event: DataEvent,
    size: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, utoipa::ToSchema)]
pub(crate) struct QueueStats {
    /// Events and bytes in memory.
    pub len: usize,
    pub bytes: usize,
    pub queued: u64,
    pub delivered: u64,
    pub dropped_oversized: u64,
    pub dropped_full: u64,
    pub dropped_timeout: u64,
    pub dropped_budget: u64,
    /// Evicted to make room (`drop_oldest`).
    pub evicted: u64,
    /// Spill on for this queue.
    pub spill: bool,
    /// Events are going to disk (until the disk is empty again).
    pub spilling: bool,
    /// Events on disk (including those about to be written).
    pub disk_len: u64,
    pub disk_bytes: u64,
    /// Totals: written to disk, loaded back, lost to a disk error.
    pub spilled: u64,
    pub refilled: u64,
    pub disk_bytes_written: u64,
    pub dropped_disk_error: u64,
    /// The free-space guard is holding writes back.
    pub disk_blocked: bool,
    /// Total time producers waited for room (`block`), in milliseconds.
    pub blocked_ms: u64,
}

#[derive(Default)]
struct Counters {
    queued: AtomicU64,
    delivered: AtomicU64,
    dropped_oversized: AtomicU64,
    dropped_full: AtomicU64,
    dropped_timeout: AtomicU64,
    dropped_budget: AtomicU64,
    evicted: AtomicU64,
    spilled: AtomicU64,
    refilled: AtomicU64,
    disk_bytes_written: AtomicU64,
    dropped_disk_error: AtomicU64,
    blocked_us: AtomicU64,
}

#[derive(Default)]
struct State {
    /// The memory FIFO: the oldest events.
    items: VecDeque<Queued>,
    bytes: usize,
    closed: bool,
    // --- spill only. Logical order: items, then disk, then writing, then pending.
    /// New events go to disk (set when memory is full; cleared when the disk empties).
    spilling: bool,
    /// Newest events, waiting for the spill task to write them.
    pending: VecDeque<Pending>,
    pending_bytes: usize,
    /// Taken from `pending`, being written right now.
    writing: usize,
    writing_bytes: usize,
    /// In the spool file.
    disk_len: u64,
    disk_bytes: u64,
    /// The free-space guard tripped; writes wait.
    disk_blocked: bool,
}

pub(crate) struct NodeQueue {
    settings: QueueSettings,
    budget: Budget,
    spill: Option<SpillSettings>,
    state: Mutex<State>,
    /// Something was pushed or refilled (or the queue closed).
    pushed: Notify,
    /// Something was taken out (or the queue closed): room may have appeared.
    popped: Notify,
    /// Wakes the spill task.
    work: Notify,
    counters: Counters,
    task: Mutex<Option<JoinHandle<()>>>,
}

impl NodeQueue {
    /// A queue; with `spill`, also starts its spill task (which creates the
    /// spool file on the first spill, and deletes it in [`NodeQueue::finish`]).
    pub(crate) fn new(
        settings: QueueSettings,
        budget: Budget,
        spill: Option<SpillSettings>,
    ) -> Arc<Self> {
        let spilling = spill.is_some();
        let queue = Arc::new(Self {
            settings,
            budget,
            spill,
            state: Mutex::default(),
            pushed: Notify::new(),
            popped: Notify::new(),
            work: Notify::new(),
            counters: Counters::default(),
            task: Mutex::new(None),
        });
        if spilling {
            let task = tokio::spawn(spill::run(queue.clone()));
            *queue.task.lock().unwrap_or_else(PoisonError::into_inner) = Some(task);
        }
        queue
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// An event larger than the queue's byte limit still fits an empty queue:
    /// node limits shape normal traffic, and only the hub-wide limit
    /// (`[hub] max_inflight_bytes`) decides what's too large.
    fn has_room(&self, state: &State, size: usize) -> bool {
        state.items.len() < self.settings.capacity
            && (state.bytes + size <= self.settings.max_bytes || state.items.is_empty())
    }

    /// Adds an event, applying the queue's `when_full` policy. With `block`,
    /// waits (up to `block_timeout`) for room.
    pub(crate) async fn push(&self, event: DataEvent, size: usize) -> Pushed {
        let started = Instant::now();
        let result = match &self.spill {
            None => self.push_memory(event, size).await,
            Some(spill) => self.push_spill(spill, event, size).await,
        };
        if self.settings.when_full == WhenFull::Block {
            let waited = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
            self.counters
                .blocked_us
                .fetch_add(waited, Ordering::Relaxed);
        }
        let counter = match result {
            Pushed::Queued => &self.counters.queued,
            Pushed::Dropped(DropReason::Oversized) => &self.counters.dropped_oversized,
            Pushed::Dropped(DropReason::Full) => &self.counters.dropped_full,
            Pushed::Dropped(DropReason::Timeout) => &self.counters.dropped_timeout,
            Pushed::Dropped(DropReason::Budget) => &self.counters.dropped_budget,
            Pushed::Dropped(DropReason::Closed) => return result,
        };
        counter.fetch_add(1, Ordering::Relaxed);
        result
    }

    async fn push_memory(&self, event: DataEvent, size: usize) -> Pushed {
        if size > self.budget.total {
            return Pushed::Dropped(DropReason::Oversized);
        }
        let Ok(permits) = u32::try_from(size) else {
            return Pushed::Dropped(DropReason::Oversized);
        };
        let deadline = Instant::now() + self.settings.block_timeout;
        let semaphore = self.budget.semaphore.clone();
        let permit = match self.settings.when_full {
            WhenFull::Block => {
                match tokio::time::timeout_at(deadline, semaphore.acquire_many_owned(permits)).await
                {
                    Ok(Ok(permit)) => permit,
                    Ok(Err(_)) => return Pushed::Dropped(DropReason::Closed),
                    Err(_) => return Pushed::Dropped(DropReason::Timeout),
                }
            }
            WhenFull::DropNewest | WhenFull::DropOldest => {
                match semaphore.try_acquire_many_owned(permits) {
                    Ok(permit) => permit,
                    Err(_) => return Pushed::Dropped(DropReason::Budget),
                }
            }
        };
        let item = Queued {
            event,
            size,
            _hold: Hold::Budget(permit),
        };

        loop {
            let popped = self.popped.notified();
            tokio::pin!(popped);
            popped.as_mut().enable();
            {
                let mut state = self.state();
                if state.closed {
                    return Pushed::Dropped(DropReason::Closed);
                }
                let room = self.has_room(&state, size);
                if !room && self.settings.when_full == WhenFull::DropOldest {
                    while !self.has_room(&state, size) {
                        let Some(old) = state.items.pop_front() else {
                            break;
                        };
                        state.bytes -= old.size;
                        self.counters.evicted.fetch_add(1, Ordering::Relaxed);
                    }
                }
                if self.has_room(&state, size) {
                    state.bytes += size;
                    state.items.push_back(item);
                    drop(state);
                    self.pushed.notify_waiters();
                    return Pushed::Queued;
                }
                if self.settings.when_full == WhenFull::DropNewest {
                    return Pushed::Dropped(DropReason::Full);
                }
            }
            if tokio::time::timeout_at(deadline, popped).await.is_err() {
                return Pushed::Dropped(DropReason::Timeout);
            }
        }
    }

    async fn push_spill(&self, spill: &SpillSettings, event: DataEvent, size: usize) -> Pushed {
        let spec = &spill.spec;
        let bytes = size as u64;
        // Only the hub-wide hard memory limit decides what's too large. An
        // event over this node's limits is accepted alone: into empty memory,
        // or onto an empty disk, and comes back from disk on its own once
        // memory is empty.
        if size > self.budget.total {
            return Pushed::Dropped(DropReason::Oversized);
        }
        let deadline = Instant::now() + self.settings.block_timeout;
        let mut event = Some(event);
        loop {
            let popped = self.popped.notified();
            tokio::pin!(popped);
            popped.as_mut().enable();
            {
                let mut state = self.state();
                if state.closed {
                    return Pushed::Dropped(DropReason::Closed);
                }
                // Memory while nothing is on disk and there's room (both soft limits).
                if !state.spilling
                    && state.items.len() < spec.memory_max_events
                    && (state.bytes + size <= spec.memory_max_bytes || state.items.is_empty())
                    && spill.shared.memory.try_add(bytes)
                {
                    let hold = Hold::Soft(SoftShare {
                        gauge: spill.shared.memory.clone(),
                        bytes,
                    });
                    state.bytes += size;
                    state.items.push_back(Queued {
                        event: event.take().expect("pushed once"),
                        size,
                        _hold: hold,
                    });
                    drop(state);
                    self.pushed.notify_waiters();
                    return Pushed::Queued;
                }
                // Disk from now on, until it's empty again (keeps the order).
                state.spilling = true;
                let room = self.disk_room(spill, &state, size);
                let evict = !room
                    && self.settings.when_full == WhenFull::DropOldest
                    && !state.items.is_empty()
                    && self.writable(spill, &state);
                if (room || evict) && spill.shared.disk.try_add(bytes) {
                    if evict {
                        // The oldest event is in memory. The disk may hold up
                        // to the memory limit more than its own until the
                        // next refill.
                        let old = state.items.pop_front().expect("checked above");
                        state.bytes -= old.size;
                        self.counters.evicted.fetch_add(1, Ordering::Relaxed);
                    }
                    state.pending_bytes += size;
                    state.pending.push_back(Pending {
                        event: event.take().expect("pushed once"),
                        size,
                    });
                    drop(state);
                    self.work.notify_one();
                    return Pushed::Queued;
                }
                if self.settings.when_full != WhenFull::Block {
                    return Pushed::Dropped(DropReason::Full);
                }
            }
            if tokio::time::timeout_at(deadline, popped).await.is_err() {
                return Pushed::Dropped(DropReason::Timeout);
            }
        }
    }

    /// Whether a write could happen now at all (no guard, no write backlog).
    fn writable(&self, spill: &SpillSettings, state: &State) -> bool {
        !state.disk_blocked && state.pending.len() < pending_cap(&spill.spec)
    }

    /// Room on disk for one more event of `size` (this queue's and the hub's limits).
    fn disk_room(&self, spill: &SpillSettings, state: &State, size: usize) -> bool {
        let spec = &spill.spec;
        let events = state.disk_len + (state.writing + state.pending.len()) as u64;
        let bytes = state.disk_bytes + (state.writing_bytes + state.pending_bytes + size) as u64;
        // Over this node's disk byte limit is fine for an event on its own.
        let empty = events == 0;
        self.writable(spill, state)
            && events < spec.disk_max_events
            && (bytes <= spec.disk_max_bytes || empty)
            && spill.shared.disk.used() + size as u64 <= spill.shared.disk.limit
    }

    /// The oldest event; waits while the queue is empty. `None` once closed.
    pub(crate) async fn pop(&self) -> Option<Queued> {
        loop {
            let pushed = self.pushed.notified();
            tokio::pin!(pushed);
            pushed.as_mut().enable();
            {
                let mut state = self.state();
                if state.closed {
                    return None;
                }
                if let Some(item) = state.items.pop_front() {
                    state.bytes -= item.size;
                    let refill = self.needs_refill(&state);
                    drop(state);
                    if refill {
                        self.work.notify_one();
                    }
                    self.popped.notify_waiters();
                    return Some(item);
                }
                if state.spilling {
                    // Memory is empty, the rest is on disk: ask for a refill.
                    self.work.notify_one();
                }
            }
            pushed.await;
        }
    }

    /// Spilling, and memory is below the refill watermark (by count or bytes).
    fn needs_refill(&self, state: &State) -> bool {
        let Some(spill) = &self.spill else {
            return false;
        };
        let spec = &spill.spec;
        state.spilling
            && (state.items.len() as f64 <= spec.memory_max_events as f64 * spec.refill_watermark
                || state.bytes as f64 <= spec.memory_max_bytes as f64 * spec.refill_watermark)
    }

    /// Puts an event back at the front (it couldn't be delivered), ignoring
    /// the limits: it was already counted against them.
    pub(crate) fn requeue(&self, item: Queued) {
        let mut state = self.state();
        if state.closed {
            return;
        }
        state.bytes += item.size;
        state.items.push_front(item);
        drop(state);
        self.pushed.notify_waiters();
    }

    pub(crate) fn delivered(&self) {
        self.counters.delivered.fetch_add(1, Ordering::Relaxed);
    }

    /// Drops everything queued, and wakes and refuses every waiter. The spill
    /// task (if any) then deletes the spool file.
    pub(crate) fn close(&self) {
        let mut state = self.state();
        state.closed = true;
        state.items.clear();
        state.bytes = 0;
        if let Some(spill) = &self.spill {
            spill.shared.disk.sub(state.pending_bytes as u64);
        }
        state.pending.clear();
        state.pending_bytes = 0;
        drop(state);
        self.pushed.notify_waiters();
        self.popped.notify_waiters();
        self.work.notify_one();
    }

    /// [`NodeQueue::close`], then waits for the spill task to delete the file.
    pub(crate) async fn finish(&self) {
        self.close();
        let task = self
            .task
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(task) = task {
            let _ = task.await;
        }
    }

    pub(crate) fn stats(&self) -> QueueStats {
        let state = self.state();
        let c = &self.counters;
        QueueStats {
            len: state.items.len(),
            bytes: state.bytes,
            queued: c.queued.load(Ordering::Relaxed),
            delivered: c.delivered.load(Ordering::Relaxed),
            dropped_oversized: c.dropped_oversized.load(Ordering::Relaxed),
            dropped_full: c.dropped_full.load(Ordering::Relaxed),
            dropped_timeout: c.dropped_timeout.load(Ordering::Relaxed),
            dropped_budget: c.dropped_budget.load(Ordering::Relaxed),
            evicted: c.evicted.load(Ordering::Relaxed),
            spill: self.spill.is_some(),
            spilling: state.spilling,
            disk_len: state.disk_len + (state.writing + state.pending.len()) as u64,
            disk_bytes: state.disk_bytes + (state.writing_bytes + state.pending_bytes) as u64,
            spilled: c.spilled.load(Ordering::Relaxed),
            refilled: c.refilled.load(Ordering::Relaxed),
            disk_bytes_written: c.disk_bytes_written.load(Ordering::Relaxed),
            dropped_disk_error: c.dropped_disk_error.load(Ordering::Relaxed),
            disk_blocked: state.disk_blocked,
            blocked_ms: c.blocked_us.load(Ordering::Relaxed) / 1_000,
        }
    }
}

/// Events waiting to be written are in memory too: cap them, so a slow disk
/// slows producers down instead of filling memory.
fn pending_cap(spec: &SpillSpec) -> usize {
    spec.write_batch.max(1) * 2
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(n: u32) -> DataEvent {
        DataEvent {
            instance_id: n.to_string(),
            ..Default::default()
        }
    }

    fn queue(
        when_full: WhenFull,
        capacity: usize,
        max_bytes: usize,
        budget: &Budget,
    ) -> Arc<NodeQueue> {
        NodeQueue::new(
            QueueSettings {
                capacity,
                max_bytes,
                when_full,
                block_timeout: Duration::from_millis(200),
            },
            budget.clone(),
            None,
        )
    }

    async fn ids(q: &NodeQueue) -> Vec<String> {
        let mut out = vec![];
        while q.stats().len > 0 {
            out.push(q.pop().await.unwrap().event.instance_id);
        }
        out
    }

    #[tokio::test]
    async fn drop_newest_keeps_the_first_events() {
        let budget = Budget::new(1000);
        let q = queue(WhenFull::DropNewest, 2, 1000, &budget);
        for n in 0..4 {
            q.push(event(n), 10).await;
        }
        let stats = q.stats();
        assert_eq!((stats.queued, stats.dropped_full), (2, 2));
        assert_eq!(ids(&q).await, ["0", "1"]);
    }

    #[tokio::test]
    async fn drop_oldest_keeps_the_latest_events() {
        let budget = Budget::new(1000);
        let q = queue(WhenFull::DropOldest, 2, 1000, &budget);
        for n in 0..4 {
            assert_eq!(q.push(event(n), 10).await, Pushed::Queued);
        }
        assert_eq!(q.stats().evicted, 2);
        assert_eq!(ids(&q).await, ["2", "3"]);
        // Evicted events gave their bytes back.
        assert_eq!(budget.available(), 1000);
    }

    #[tokio::test]
    async fn byte_limit_counts_like_the_event_limit() {
        let budget = Budget::new(1000);
        let q = queue(WhenFull::DropNewest, 100, 25, &budget);
        assert_eq!(q.push(event(0), 10).await, Pushed::Queued);
        assert_eq!(q.push(event(1), 10).await, Pushed::Queued);
        assert_eq!(
            q.push(event(2), 10).await,
            Pushed::Dropped(DropReason::Full)
        );
        // Over the queue's byte limit: not alongside others...
        assert_eq!(
            q.push(event(3), 30).await,
            Pushed::Dropped(DropReason::Full)
        );
        assert_eq!(q.stats().bytes, 20);
        assert_eq!(ids(&q).await, ["0", "1"]);
        // ...but alone in an empty queue, since the hub-wide budget allows it.
        assert_eq!(q.push(event(4), 30).await, Pushed::Queued);
        assert_eq!(ids(&q).await, ["4"]);
        // Over the hub-wide hard limit: dropped.
        assert_eq!(
            q.push(event(5), 1001).await,
            Pushed::Dropped(DropReason::Oversized)
        );
    }

    #[tokio::test]
    async fn block_waits_for_room_then_times_out() {
        let budget = Budget::new(1000);
        let q = queue(WhenFull::Block, 1, 1000, &budget);
        assert_eq!(q.push(event(0), 10).await, Pushed::Queued);

        // Room appears while waiting: queued.
        let popper = {
            let q = q.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(50)).await;
                q.pop().await.map(|i| i.event.instance_id)
            })
        };
        let started = std::time::Instant::now();
        assert_eq!(q.push(event(1), 10).await, Pushed::Queued);
        assert!(started.elapsed() >= Duration::from_millis(40));
        assert_eq!(popper.await.unwrap().as_deref(), Some("0"));

        // No room within block_timeout: dropped.
        let started = std::time::Instant::now();
        assert_eq!(
            q.push(event(2), 10).await,
            Pushed::Dropped(DropReason::Timeout)
        );
        assert!(started.elapsed() >= Duration::from_millis(190));
        assert_eq!(q.stats().dropped_timeout, 1);
        // Both waits (~50 ms, then the 200 ms timeout) count as time blocked.
        assert!(q.stats().blocked_ms >= 240, "{:?}", q.stats());
    }

    #[tokio::test]
    async fn the_budget_is_shared_between_queues() {
        let budget = Budget::new(25);
        let a = queue(WhenFull::DropNewest, 100, 1000, &budget);
        let b = queue(WhenFull::Block, 100, 1000, &budget);
        assert_eq!(a.push(event(0), 20).await, Pushed::Queued);
        // b blocks on the budget until a's event is delivered.
        let release = {
            let a = a.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(50)).await;
                drop(a.pop().await);
            })
        };
        assert_eq!(b.push(event(1), 20).await, Pushed::Queued);
        release.await.unwrap();
        // a can't get budget while b holds it.
        assert_eq!(
            a.push(event(2), 20).await,
            Pushed::Dropped(DropReason::Budget)
        );
        assert_eq!(
            a.push(event(3), 30).await,
            Pushed::Dropped(DropReason::Oversized)
        );
    }

    #[tokio::test]
    async fn requeue_puts_back_at_the_front_and_close_wakes_everyone() {
        let budget = Budget::new(1000);
        let q = queue(WhenFull::Block, 1, 1000, &budget);
        q.push(event(0), 10).await;
        let item = q.pop().await.unwrap();
        q.requeue(item);
        assert_eq!(q.stats().len, 1);

        let blocked = {
            let q = q.clone();
            tokio::spawn(async move { q.push(event(1), 10).await })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        q.close();
        assert_eq!(blocked.await.unwrap(), Pushed::Dropped(DropReason::Closed));
        assert!(q.pop().await.is_none());
        assert_eq!(budget.available(), 1000);
    }
}
