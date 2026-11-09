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

mod durable;
mod spill;

pub(crate) use durable::{DurableSettings, delete_files, purge_files};
use durable::{DurableCore, Push};

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
    /// `drop_oldest`: the most queued events one push may evict to fit the
    /// new one. If it would take more, nothing is evicted and the new event
    /// is dropped (`Full`). `None` = no limit.
    pub max_evictions: Option<usize>,
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
    /// Nothing (left behind when an evicted event's permit moves on).
    None,
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
    /// Durable queues: the event's row in the spool file (deleted on ack).
    row: Option<i64>,
    /// The queue's epoch when it was queued. A purge starts a new epoch: an
    /// event from an earlier one that's still in flight was purged with the
    /// rest (a requeue drops it, an ack changes nothing).
    epoch: u64,
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
    /// Durable: every event is on disk until the plugin acks it.
    pub durable: bool,
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
    /// Delivered to the plugin, waiting for its ACK (capability ACKS).
    pub in_flight: u64,
    /// Delivered again: after a disconnect, a crash or a NACK with retry.
    pub redelivered: u64,
    /// Rejected by the plugin (NACK).
    pub nacked: u64,
    /// Given up on: out of attempts, or a NACK without retry.
    pub dead_lettered: u64,
}

/// A queue's totals since it started: one set per queue, whatever its kind.
/// A durable queue's core and a spilling queue's task add to the same set
/// as `NodeQueue`, so every counter lives here.
#[derive(Default)]
pub(super) struct Counters {
    /// Accepted by `push` (all kinds).
    pub(super) queued: AtomicU64,
    /// Handed to the plugin and finished with (all kinds).
    pub(super) delivered: AtomicU64,
    /// Over the hub-wide hard limit (all kinds).
    pub(super) dropped_oversized: AtomicU64,
    /// The new event refused because the queue is full (all kinds): any
    /// policy but `block`, including `drop_oldest` when nothing in memory
    /// can be evicted. "Full" is the memory limits for a memory queue and
    /// the disk limits (or the pending-write cap) for spill and durable.
    pub(super) dropped_full: AtomicU64,
    /// `block` waited `block_timeout` without room (all kinds).
    pub(super) dropped_timeout: AtomicU64,
    /// The hub-wide byte budget was exhausted (memory queues).
    pub(super) dropped_budget: AtomicU64,
    /// `drop_oldest` removed the oldest undelivered event to make room (all
    /// kinds). It's always taken from memory, which holds the oldest
    /// events; a durable queue deletes its row too.
    pub(super) evicted: AtomicU64,
    /// Written to the spool file: overflow (spill) or every event (durable).
    pub(super) spilled: AtomicU64,
    /// Loaded back from the spool file into memory (spill, durable).
    pub(super) refilled: AtomicU64,
    pub(super) disk_bytes_written: AtomicU64,
    /// Lost because a spool write failed (spill, durable).
    pub(super) dropped_disk_error: AtomicU64,
    /// Time `block` pushes waited for room (all kinds).
    pub(super) blocked_us: AtomicU64,
    /// Delivery bookkeeping (all kinds).
    pub(super) in_flight: AtomicU64,
    pub(super) redelivered: AtomicU64,
    pub(super) nacked: AtomicU64,
    pub(super) dead_lettered: AtomicU64,
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
    /// The spill task should delete the file (a purge).
    purge_disk: Option<tokio::sync::oneshot::Sender<u64>>,
    /// Bumped by every purge (see [`Queued`]).
    epoch: u64,
}

/// What `drop_oldest` has to evict for a new event to fit: the oldest
/// `events`, `bytes` in all.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct Evictions {
    pub events: usize,
    pub bytes: u64,
}

/// Plans `drop_oldest`: the fewest of the oldest queued events (none
/// included) for which `fits(events, bytes)` holds once they're gone. `None`
/// if it would take more than `max` (`None` = no limit) or more than memory
/// holds: then nothing is evicted, and the new event is dropped instead.
pub(super) fn evictions_needed(
    items: &VecDeque<Queued>,
    max: Option<usize>,
    fits: impl Fn(u64, u64) -> bool,
) -> Option<Evictions> {
    let max = max.unwrap_or(usize::MAX).min(items.len());
    let mut plan = Evictions::default();
    loop {
        if fits(plan.events as u64, plan.bytes) {
            return Some(plan);
        }
        if plan.events == max {
            return None;
        }
        plan.bytes += items[plan.events].size as u64;
        plan.events += 1;
    }
}

/// Carries out a plan: takes the events out of `items` in one go and counts
/// them. The caller takes `plan.bytes` off its own byte total, and drops what
/// this returns after releasing its lock (dropping gives back each event's
/// share of the memory budget, which may wake producers).
pub(super) fn evict_oldest(
    items: &mut VecDeque<Queued>,
    plan: Evictions,
    counter: &AtomicU64,
) -> Vec<Queued> {
    counter.fetch_add(plan.events as u64, Ordering::Relaxed);
    items.drain(..plan.events).collect()
}

pub(crate) struct NodeQueue {
    /// Changeable in place (a reload of a node keeps its queue).
    settings: Mutex<QueueSettings>,
    budget: Budget,
    spill: Option<SpillSettings>,
    state: Mutex<State>,
    /// Something was pushed or refilled (or the queue closed).
    pushed: Notify,
    /// Something was taken out (or the queue closed): room may have appeared.
    popped: Notify,
    /// Wakes the spill task.
    work: Notify,
    /// Shared with the durable core, if any.
    counters: Arc<Counters>,
    task: Mutex<Option<JoinHandle<()>>>,
    /// Durable queues: the file-backed core every operation goes to.
    durable: Option<Arc<DurableCore>>,
}

/// Where a queue keeps its events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum QueueKind {
    Memory,
    /// Memory, overflowing to disk.
    Spill,
    /// Disk, until acked; memory is a cache.
    Durable,
}

impl NodeQueue {
    /// A durable queue (see [`durable`]); starts its task, which opens (or
    /// adopts) the node's spool file.
    pub(crate) fn new_durable(
        settings: QueueSettings,
        budget: Budget,
        durable: DurableSettings,
    ) -> Arc<Self> {
        let counters = Arc::new(Counters::default());
        let core = DurableCore::new(durable, counters.clone());
        core.start();
        Arc::new(Self {
            settings: Mutex::new(settings),
            budget,
            spill: None,
            state: Mutex::default(),
            pushed: Notify::new(),
            popped: Notify::new(),
            work: Notify::new(),
            counters,
            task: Mutex::new(None),
            durable: Some(core),
        })
    }

    pub(crate) fn kind(&self) -> QueueKind {
        match (&self.durable, &self.spill) {
            (Some(_), _) => QueueKind::Durable,
            (None, Some(_)) => QueueKind::Spill,
            (None, None) => QueueKind::Memory,
        }
    }

    /// A queue; with `spill`, also starts its spill task (which creates the
    /// spool file on the first spill, and deletes it in [`NodeQueue::finish`]).
    pub(crate) fn new(
        settings: QueueSettings,
        budget: Budget,
        spill: Option<SpillSettings>,
    ) -> Arc<Self> {
        let spilling = spill.is_some();
        let queue = Arc::new(Self {
            settings: Mutex::new(settings),
            budget,
            spill,
            state: Mutex::default(),
            pushed: Notify::new(),
            popped: Notify::new(),
            work: Notify::new(),
            counters: Arc::default(),
            task: Mutex::new(None),
            durable: None,
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

    fn settings(&self) -> QueueSettings {
        self.settings
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// New limits and policy, from now on (events already queued stay).
    pub(crate) fn reconfigure(&self, settings: QueueSettings) {
        *self.settings.lock().unwrap_or_else(PoisonError::into_inner) = settings;
        // Waiting producers re-check against the new limits.
        self.popped.notify_waiters();
    }

    /// An event larger than the queue's byte limit still fits an empty queue:
    /// node limits shape normal traffic, and only the hub-wide limit
    /// (`[hub] max_inflight_bytes`) decides what's too large.
    fn wake(&self, wake: Wake) {
        match wake {
            Wake::Nobody => {}
            Wake::Consumer => self.pushed.notify_waiters(),
            Wake::Task => self.work.notify_one(),
        }
    }

    fn has_room(&self, state: &State, size: usize) -> bool {
        memory_fits(&self.settings(), state.items.len(), state.bytes, size)
    }

    /// Adds an event, applying the queue's `when_full` policy. With `block`,
    /// waits (up to `block_timeout`) for room.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) async fn push(&self, event: DataEvent, size: usize) -> Pushed {
        self.push_tracked(event, size).await.0
    }

    /// [`NodeQueue::push`]; for a durable queue, also the ticket to wait on
    /// ([`NodeQueue::committed`]) before the event counts as stored.
    pub(crate) async fn push_tracked(&self, event: DataEvent, size: usize) -> (Pushed, Option<u64>) {
        let started = Instant::now();
        let settings = self.settings();
        let when_full = settings.when_full;
        let mut ticket = None;
        let result = match (&self.durable, &self.spill) {
            (Some(core), _) => {
                if size > self.budget.total {
                    Pushed::Dropped(DropReason::Oversized)
                } else {
                    match core
                        .push(
                            event,
                            size,
                            when_full,
                            settings.block_timeout,
                            settings.max_evictions,
                        )
                        .await
                    {
                        Push::Queued(t) => {
                            ticket = Some(t);
                            Pushed::Queued
                        }
                        Push::Full => Pushed::Dropped(DropReason::Full),
                        Push::Timeout => Pushed::Dropped(DropReason::Timeout),
                        Push::Closed => Pushed::Dropped(DropReason::Closed),
                    }
                }
            }
            (None, None) => self.push_memory(event, size).await,
            (None, Some(spill)) => self.push_spill(spill, event, size).await,
        };
        if when_full == WhenFull::Block {
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
            Pushed::Dropped(DropReason::Closed) => return (result, None),
        };
        counter.fetch_add(1, Ordering::Relaxed);
        (result, ticket)
    }

    /// Waits until a durable push's event is committed to disk. `false` if
    /// writing it failed (or the queue closed first).
    pub(crate) async fn committed(&self, ticket: u64) -> bool {
        match &self.durable {
            Some(core) => core.committed(ticket).await,
            None => true,
        }
    }

    async fn push_memory(&self, event: DataEvent, size: usize) -> Pushed {
        if size > self.budget.total {
            return Pushed::Dropped(DropReason::Oversized);
        }
        let Ok(permits) = u32::try_from(size) else {
            return Pushed::Dropped(DropReason::Oversized);
        };
        let settings = self.settings();
        if settings.when_full == WhenFull::DropOldest {
            return self.push_memory_evicting(event, size, &settings);
        }
        let deadline = Instant::now() + settings.block_timeout;
        let semaphore = self.budget.semaphore.clone();
        let permit = if settings.when_full == WhenFull::Block {
            match tokio::time::timeout_at(deadline, semaphore.acquire_many_owned(permits)).await {
                Ok(Ok(permit)) => permit,
                Ok(Err(_)) => return Pushed::Dropped(DropReason::Closed),
                Err(_) => return Pushed::Dropped(DropReason::Timeout),
            }
        } else {
            match semaphore.try_acquire_many_owned(permits) {
                Ok(permit) => permit,
                Err(_) => return Pushed::Dropped(DropReason::Budget),
            }
        };
        let mut item = Some(Queued {
            event,
            size,
            row: None,
            epoch: 0,
            _hold: Hold::Budget(permit),
        });

        loop {
            let popped = self.popped.notified();
            tokio::pin!(popped);
            popped.as_mut().enable();
            // Decide under the lock (`None`: wait for room); act after releasing it.
            let decided = 'locked: {
                let mut state = self.state();
                if state.closed {
                    break 'locked Some((Pushed::Dropped(DropReason::Closed), Wake::Nobody));
                }
                if self.has_room(&state, size) {
                    let mut item = item.take().expect("pushed once");
                    state.bytes += size;
                    item.epoch = state.epoch;
                    state.items.push_back(item);
                    break 'locked Some((Pushed::Queued, Wake::Consumer));
                }
                if settings.when_full != WhenFull::Block {
                    break 'locked Some((Pushed::Dropped(DropReason::Full), Wake::Nobody));
                }
                None
            };
            if let Some((result, wake)) = decided {
                self.wake(wake);
                return result;
            }
            if tokio::time::timeout_at(deadline, popped).await.is_err() {
                return Pushed::Dropped(DropReason::Timeout);
            }
        }
    }

    /// `drop_oldest` in a memory queue: evicts this queue's oldest events
    /// when either its own limits or the hub-wide budget has no room. The
    /// evicted events' budget permits pass straight to the new event, under
    /// the queue's lock, so no other queue can take the freed room in
    /// between. Nothing is evicted unless the event then fits.
    fn push_memory_evicting(&self, event: DataEvent, size: usize, settings: &QueueSettings) -> Pushed {
        let semaphore = &self.budget.semaphore;
        {
            // Declared before the lock, so they're dropped after it: the evicted
            // events (their permits already taken) and the permits left over,
            // which go back to the budget and may wake other producers.
            let mut evicted: Vec<Queued>;
            let _spare: OwnedSemaphorePermit;
            let mut state = self.state();
            if state.closed {
                return Pushed::Dropped(DropReason::Closed);
            }
            let (len, bytes) = (state.items.len() as u64, state.bytes as u64);
            let own_limits =
                |n: u64, freed: u64| memory_fits(settings, (len - n) as usize, (bytes - freed) as usize, size);
            // A snapshot: another queue may take budget meanwhile, which the
            // reservation below catches.
            let available = semaphore.available_permits() as u64;
            let evict = evictions_needed(&state.items, settings.max_evictions, |n, freed| {
                own_limits(n, freed) && available + freed >= size as u64
            });
            let Some(plan) = evict else {
                // Name the limit that refused it.
                let own = evictions_needed(&state.items, settings.max_evictions, own_limits);
                return Pushed::Dropped(match own {
                    Some(_) => DropReason::Budget,
                    None => DropReason::Full,
                });
            };
            // What the evictions don't free comes from the budget, reserved
            // before anything is evicted.
            let shortfall = (size as u64).saturating_sub(plan.bytes) as u32;
            let Ok(mut permit) = semaphore.clone().try_acquire_many_owned(shortfall) else {
                return Pushed::Dropped(DropReason::Budget);
            };
            evicted = evict_oldest(&mut state.items, plan, &self.counters.evicted);
            state.bytes -= plan.bytes as usize;
            for old in &mut evicted {
                if let Hold::Budget(p) = std::mem::replace(&mut old._hold, Hold::None) {
                    permit.merge(p);
                }
            }
            let mine = permit.split(size).expect("the shortfall and evictions cover the event");
            _spare = permit;
            state.bytes += size;
            let epoch = state.epoch;
            state.items.push_back(Queued {
                event,
                size,
                row: None,
                epoch,
                _hold: Hold::Budget(mine),
            });
        }
        self.wake(Wake::Consumer);
        Pushed::Queued
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
        let settings = self.settings();
        let deadline = Instant::now() + settings.block_timeout;
        let mut event = Some(event);
        loop {
            let popped = self.popped.notified();
            tokio::pin!(popped);
            popped.as_mut().enable();
            // Decide under the lock (`None`: wait for room); act after releasing it.
            let decided = 'locked: {
                // Declared before the lock, so it's dropped after it.
                let mut _evicted = Vec::new();
                let mut state = self.state();
                if state.closed {
                    break 'locked Some((Pushed::Dropped(DropReason::Closed), Wake::Nobody));
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
                    let epoch = state.epoch;
                    state.items.push_back(Queued {
                        event: event.take().expect("pushed once"),
                        size,
                        row: None,
                        epoch,
                        _hold: hold,
                    });
                    break 'locked Some((Pushed::Queued, Wake::Consumer));
                }
                // Disk from now on, until it's empty again (keeps the order).
                state.spilling = true;
                let mut room = self.disk_room(spill, &state, size);
                let mut plan = Evictions::default();
                // The oldest events are in memory, so evicting frees memory,
                // not disk: the hub-wide disk limit must take the event as it
                // is. What's freed in memory counts against this queue's disk
                // limits (the next refill moves that much off the disk), so
                // the disk may hold up to the memory limit more than its own.
                if !room
                    && settings.when_full == WhenFull::DropOldest
                    && self.writable(spill, &state)
                    && spill.shared.disk.used() + bytes <= spill.shared.disk.limit
                {
                    let events = state.disk_len + (state.writing + state.pending.len()) as u64;
                    let on_disk =
                        state.disk_bytes + (state.writing_bytes + state.pending_bytes) as u64;
                    let fits = |n: u64, freed: u64| {
                        events < spec.disk_max_events + n
                            && on_disk + bytes <= spec.disk_max_bytes + freed
                    };
                    if let Some(needed) =
                        evictions_needed(&state.items, settings.max_evictions, fits)
                    {
                        plan = needed;
                        room = true;
                    }
                }
                if room && spill.shared.disk.try_add(bytes) {
                    _evicted = evict_oldest(&mut state.items, plan, &self.counters.evicted);
                    state.bytes -= plan.bytes as usize;
                    state.pending_bytes += size;
                    state.pending.push_back(Pending {
                        event: event.take().expect("pushed once"),
                        size,
                    });
                    break 'locked Some((Pushed::Queued, Wake::Task));
                }
                if settings.when_full != WhenFull::Block {
                    break 'locked Some((Pushed::Dropped(DropReason::Full), Wake::Nobody));
                }
                None
            };
            if let Some((result, wake)) = decided {
                self.wake(wake);
                return result;
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
        if let Some(core) = &self.durable {
            return core.pop().await;
        }
        loop {
            let pushed = self.pushed.notified();
            tokio::pin!(pushed);
            pushed.as_mut().enable();
            // Decide under the lock; act after releasing it.
            let (next, refill) = {
                let mut state = self.state();
                if state.closed {
                    (Next::Closed, false)
                } else if let Some(item) = state.items.pop_front() {
                    state.bytes -= item.size;
                    let refill = self.needs_refill(&state);
                    (Next::Item(item), refill)
                } else {
                    // Memory is empty; if the rest is on disk, ask for a refill.
                    (Next::Wait, state.spilling)
                }
            };
            if refill {
                self.work.notify_one();
            }
            match next {
                Next::Closed => return None,
                Next::Item(item) => {
                    // Room appeared: wake blocked producers.
                    self.popped.notify_waiters();
                    return Some(item);
                }
                Next::Wait => pushed.await,
            }
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
        if let Some(core) = &self.durable {
            return core.requeue(item);
        }
        {
            let mut state = self.state();
            if state.closed || item.epoch != state.epoch {
                // Closed, or purged while it was in flight: it's not coming back.
                return;
            }
            state.bytes += item.size;
            state.items.push_front(item);
        }
        self.pushed.notify_waiters();
    }

    /// The plugin has the event (acked it, or it was sent to a plugin
    /// without ACKS). Dropping `item` releases what it holds.
    pub(crate) fn delivered(&self, item: Queued) {
        if let Some(core) = &self.durable {
            core.remove(item);
        }
        self.counters.delivered.fetch_add(1, Ordering::Relaxed);
    }

    /// Delivery bookkeeping, kept by the queue's deliverer.
    pub(crate) fn set_in_flight(&self, n: usize) {
        self.counters.in_flight.store(n as u64, Ordering::Relaxed);
    }

    pub(crate) fn count_redelivered(&self) {
        self.counters.redelivered.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn count_nacked(&self) {
        self.counters.nacked.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn dead_letter(&self, item: Queued) {
        if let Some(core) = &self.durable {
            core.remove(item);
        }
        self.counters.dead_lettered.fetch_add(1, Ordering::Relaxed);
    }

    /// Starts over: drops every queued event (memory and disk; the spool file
    /// itself is deleted, and a fresh one starts on the next spill), and the
    /// events in flight with the plugin, which won't come back (see
    /// [`Queued`]). Returns how many went.
    pub(crate) async fn purge(&self) -> u64 {
        if let Some(core) = &self.durable {
            return core.purge().await;
        }
        let (purged, disk) = {
            let mut state = self.state();
            let purged = (state.items.len() + state.pending.len()) as u64;
            state.epoch += 1;
            state.items.clear();
            state.bytes = 0;
            if let Some(spill) = &self.spill {
                spill.shared.disk.sub(state.pending_bytes as u64);
            }
            state.pending.clear();
            state.pending_bytes = 0;
            let disk = self.spill.is_some().then(|| {
                let (tx, rx) = tokio::sync::oneshot::channel();
                state.purge_disk = Some(tx);
                rx
            });
            (purged, disk)
        };
        self.popped.notify_waiters();
        self.work.notify_one();
        let on_disk = match disk {
            Some(rx) => rx.await.unwrap_or(0),
            None => 0,
        };
        purged + on_disk
    }

    /// Drops everything queued, and wakes and refuses every waiter. The spill
    /// task (if any) then deletes the spool file.
    pub(crate) fn close(&self) {
        {
            // Declared before the lock, so they're dropped after it: freeing
            // the events gives back their share of the memory limits, which
            // may wake producers.
            let _items: VecDeque<Queued>;
            let _pending: VecDeque<Pending>;
            let mut state = self.state();
            state.closed = true;
            _items = std::mem::take(&mut state.items);
            state.bytes = 0;
            if let Some(spill) = &self.spill {
                spill.shared.disk.sub(state.pending_bytes as u64);
            }
            _pending = std::mem::take(&mut state.pending);
            state.pending_bytes = 0;
        }
        self.pushed.notify_waiters();
        self.popped.notify_waiters();
        self.work.notify_one();
    }

    /// [`NodeQueue::close`], then waits for the spill task to delete the
    /// file. A durable queue keeps its file (for the next start) unless
    /// `remove` (its node is gone).
    pub(crate) async fn finish(&self, remove: bool) {
        if let Some(core) = &self.durable {
            return core.finish(remove).await;
        }
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
        let mut stats = QueueStats {
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
            in_flight: c.in_flight.load(Ordering::Relaxed),
            redelivered: c.redelivered.load(Ordering::Relaxed),
            nacked: c.nacked.load(Ordering::Relaxed),
            dead_lettered: c.dead_lettered.load(Ordering::Relaxed),
            durable: false,
        };
        if let Some(core) = &self.durable {
            // Its state is the core's; the counters are already shared.
            let d = core.stats();
            stats.durable = true;
            stats.len = d.len;
            stats.bytes = d.bytes;
            stats.disk_len = d.disk_len;
            stats.disk_bytes = d.disk_bytes;
            stats.disk_blocked = d.disk_blocked;
        }
        stats
    }
}

/// Who a push wakes once it has released the queue's lock.
#[derive(Clone, Copy)]
enum Wake {
    Nobody,
    /// The consumer (an event is in memory).
    Consumer,
    /// The spill task (an event is waiting to be written).
    Task,
}

/// What `pop` does once it has looked at the state.
enum Next {
    Closed,
    Item(Queued),
    Wait,
}

/// Room in a memory queue holding `len` events of `bytes` for one more of
/// `size`. An event over the byte limit is fine on its own.
fn memory_fits(settings: &QueueSettings, len: usize, bytes: usize, size: usize) -> bool {
    len < settings.capacity && (bytes + size <= settings.max_bytes || len == 0)
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
            body: n.to_string().into(),
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
                max_evictions: None,
            },
            budget.clone(),
            None,
        )
    }

    async fn ids(q: &NodeQueue) -> Vec<String> {
        let mut out = vec![];
        while q.stats().len > 0 {
            out.push(q.pop().await.unwrap().event.label());
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

    /// A memory queue of 40 bytes holding four 10-byte events, `drop_oldest`.
    async fn full_queue(max_evictions: Option<usize>, budget: &Budget) -> Arc<NodeQueue> {
        let q = NodeQueue::new(
            QueueSettings {
                capacity: 100,
                max_bytes: 40,
                when_full: WhenFull::DropOldest,
                block_timeout: Duration::from_millis(200),
                max_evictions,
            },
            budget.clone(),
            None,
        );
        for n in 0..4 {
            assert_eq!(q.push(event(n), 10).await, Pushed::Queued);
        }
        q
    }

    #[tokio::test]
    async fn drop_oldest_evicts_as_many_as_a_large_event_needs() {
        let budget = Budget::new(1000);
        // 25 bytes need 30 freed: three events.
        for max in [None, Some(3)] {
            let q = full_queue(max, &budget).await;
            assert_eq!(q.push(event(9), 25).await, Pushed::Queued, "{max:?}");
            assert_eq!(q.stats().evicted, 3);
            assert_eq!(ids(&q).await, ["3", "9"]);
        }
        assert_eq!(budget.available(), 1000);
    }

    #[tokio::test]
    async fn drop_oldest_over_the_eviction_limit_evicts_nothing() {
        let budget = Budget::new(1000);
        let q = full_queue(Some(2), &budget).await;
        assert_eq!(
            q.push(event(9), 25).await,
            Pushed::Dropped(DropReason::Full)
        );
        // A queue-sized event would otherwise have emptied the queue.
        assert_eq!(
            q.push(event(8), 40).await,
            Pushed::Dropped(DropReason::Full)
        );
        let stats = q.stats();
        assert_eq!((stats.evicted, stats.dropped_full), (0, 2));
        assert_eq!(ids(&q).await, ["0", "1", "2", "3"]);
        // Without a limit, it does: the event is accepted on its own.
        let q = full_queue(None, &budget).await;
        assert_eq!(q.push(event(8), 50).await, Pushed::Queued);
        assert_eq!(q.stats().evicted, 4);
        assert_eq!(ids(&q).await, ["8"]);
    }

    #[tokio::test]
    async fn drop_oldest_evicts_its_own_events_when_the_hub_wide_budget_is_full() {
        // 50 bytes for every queue: 40 in this one, 10 in another.
        let budget = Budget::new(50);
        let settings = |max_evictions| QueueSettings {
            capacity: 100,
            max_bytes: 1000,
            when_full: WhenFull::DropOldest,
            block_timeout: Duration::from_millis(200),
            max_evictions,
        };
        let q = NodeQueue::new(settings(Some(1)), budget.clone(), None);
        for n in 0..4 {
            assert_eq!(q.push(event(n), 10).await, Pushed::Queued);
        }
        let other = queue(WhenFull::DropNewest, 100, 1000, &budget);
        assert_eq!(other.push(event(100), 10).await, Pushed::Queued);
        assert_eq!(budget.available(), 0);

        // 15 bytes take two evictions, over the limit of one: nothing goes.
        assert_eq!(
            q.push(event(9), 15).await,
            Pushed::Dropped(DropReason::Budget)
        );
        assert_eq!((q.stats().evicted, q.stats().bytes), (0, 40));

        // Without a limit: two go, and the 5 bytes left over return to the budget.
        q.reconfigure(settings(None));
        assert_eq!(q.push(event(9), 15).await, Pushed::Queued);
        assert_eq!(budget.available(), 5);
        // 12 bytes: one eviction frees 10, the other 2 come from the budget.
        assert_eq!(q.push(event(8), 12).await, Pushed::Queued);
        assert_eq!(budget.available(), 3);
        assert_eq!((q.stats().evicted, q.stats().bytes), (3, 37));
        // The other queue's event was never touched.
        assert_eq!(ids(&other).await, ["100"]);
        assert_eq!(ids(&q).await, ["3", "9", "8"]);
        assert_eq!(budget.available(), 50);
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
                q.pop().await.map(|i| i.event.label())
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

    #[tokio::test]
    async fn a_purge_drops_what_is_in_flight_too() {
        let budget = Budget::new(1000);
        let q = queue(WhenFull::Block, 10, 1000, &budget);
        for n in 0..3 {
            q.push(event(n), 10).await;
        }
        let in_flight = q.pop().await.unwrap();
        assert_eq!(q.purge().await, 2);
        // A NACK or reconnect after the purge doesn't bring it back.
        q.requeue(in_flight);
        assert_eq!(q.stats().len, 0);
        // What comes after the purge is queued as usual.
        q.push(event(9), 10).await;
        let item = q.pop().await.unwrap();
        q.requeue(item);
        assert_eq!(ids(&q).await, ["9"]);
        assert_eq!(budget.available(), 1000);
    }
}
