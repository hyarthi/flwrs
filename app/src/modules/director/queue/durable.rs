//! Durable queues: every event is committed to the node's spool file before
//! the producer is told it's taken, and stays there until the consumer acks
//! it; on a hub restart the file is replayed. Memory holds the oldest
//! undelivered events as a cache.
//!
//! One background task does all disk I/O (writes, deletes, refills, purges),
//! so they can't race. Rows are deleted only when the plugin has acked the
//! event (or it was dead-lettered, evicted or purged), so anything the hub
//! handed out but never got an ack for is delivered again after a crash:
//! at-least-once.
//!
//! Files: `<spool_dir>/<scene-id>/<node-id>.<ulid>.db`, the same table as a
//! spilling queue (`events(seq, enqueue_time, size, header, event)`). A
//! durable queue started fresh adopts every file of its node (oldest first;
//! the others are merged into it). WAL with `synchronous = FULL`: a commit
//! survives power loss.
//!
//! A purge is the "start over" lever (corruption, a full disk, a file that
//! keeps growing): it drops everything the queue holds, including the events
//! in flight with the plugin, deletes the file whatever state it's in, and
//! opens a fresh one. It's also how a queue whose file wouldn't open (it
//! refuses every event meanwhile) gets going again.

use super::{Counters, Evictions, Hold, Next, Queued, SoftShare, SpillShared, evict_oldest};
use crate::modules::hub::wire::DataEvent;
use crate::modules::node_config::WhenFull;
use crate::modules::rate_limit::RateLimit;
use crate::modules::supervisor::SpillSpec;
use flwrs_plugin::schema::protocol::FrameHeader;
use prost::Message;
use sqlx::sqlite::{
    SqliteAutoVacuum, SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous,
};
use sqlx::{Row, SqlitePool};
use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering::Relaxed;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::{Notify, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;

/// How long to wait for more events before writing a partial batch.
const BATCH_WINDOW: Duration = Duration::from_millis(5);
/// How often to recheck free space while the guard holds writes back.
const BLOCKED_RECHECK: Duration = Duration::from_secs(1);

/// Where a durable queue keeps its file, and its limits.
#[derive(Clone)]
pub(crate) struct DurableSettings {
    pub spec: SpillSpec,
    /// `<spool_dir>/<scene-id>`.
    pub dir: PathBuf,
    pub node_id: String,
    pub shared: SpillShared,
    /// Take over the node's existing files (a fresh start). A queue created
    /// on a reload starts empty: the node's previous queue (if any) still
    /// owns its file.
    pub adopt: bool,
}

/// An event waiting to be written.
struct Pending {
    event: DataEvent,
    size: usize,
    ticket: u64,
}

#[derive(Default)]
struct State {
    /// Memory cache: the oldest undelivered events (each with its row).
    items: VecDeque<Queued>,
    bytes: usize,
    /// Newest events, waiting for the task to write them.
    pending: VecDeque<Pending>,
    pending_bytes: usize,
    writing: usize,
    writing_bytes: usize,
    /// Every row in the file (cached, on disk only, or in flight).
    rows: u64,
    rows_bytes: u64,
    /// Rows after the cache that aren't in memory yet.
    not_loaded: u64,
    /// The highest row loaded into memory (or handed out) so far.
    last_loaded: i64,
    /// Rows that were in the file when it was opened: they may have been
    /// delivered before the restart.
    replay_until: i64,
    /// Rows to delete (acked, dead-lettered, evicted), with their sizes.
    deletes: Vec<(i64, usize)>,
    /// Bumped by every purge (see `Queued`): events from an earlier epoch
    /// were in a file that's gone.
    epoch: u64,
    /// Tickets whose write failed.
    failed: HashSet<u64>,
    next_ticket: u64,
    /// The file is open and counted.
    ready: bool,
    closed: bool,
    delete_on_close: bool,
    purge: Option<tokio::sync::oneshot::Sender<u64>>,
    disk_blocked: bool,
    /// Files this queue couldn't open (or create): a purge deletes them.
    unopened: Vec<PathBuf>,
}

pub(super) struct DurableCore {
    settings: DurableSettings,
    state: Mutex<State>,
    /// An event arrived in memory (or the queue closed).
    pushed: Notify,
    /// Room appeared (or the queue closed).
    room: Notify,
    /// Wakes the task.
    work: Notify,
    /// The highest ticket whose write has finished (committed or failed).
    committed: watch::Sender<u64>,
    task: Mutex<Option<JoinHandle<()>>>,
    /// The owning `NodeQueue`'s counters.
    counters: Arc<Counters>,
}

/// What a durable push did.
pub(super) enum Push {
    /// Waiting to be written; wait for this ticket.
    Queued(u64),
    Full,
    Timeout,
    Closed,
}

impl DurableCore {
    pub(super) fn new(settings: DurableSettings, counters: Arc<Counters>) -> Arc<Self> {
        Arc::new(Self {
            settings,
            state: Mutex::new(State {
                next_ticket: 1,
                ..Default::default()
            }),
            pushed: Notify::new(),
            room: Notify::new(),
            work: Notify::new(),
            committed: watch::Sender::new(0),
            task: Mutex::new(None),
            counters,
        })
    }

    /// Starts the task that owns the file.
    pub(super) fn start(self: &Arc<Self>) {
        let task = tokio::spawn(run(self.clone()));
        *lock(&self.task) = Some(task);
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        lock(&self.state)
    }

    fn spec(&self) -> &SpillSpec {
        &self.settings.spec
    }

    /// Room on disk for one more event of `size`.
    fn has_room(&self, state: &State, size: usize) -> bool {
        let spec = self.spec();
        let events = state.rows + (state.writing + state.pending.len()) as u64;
        let bytes = state.rows_bytes + (state.writing_bytes + state.pending_bytes + size) as u64;
        // An event over this node's limits is fine on its own.
        let empty = events == 0;
        !state.disk_blocked
            && state.pending.len() < pending_cap(spec)
            && events < spec.disk_max_events
            && (bytes <= spec.disk_max_bytes || empty)
            && self.settings.shared.disk.used() + size as u64 <= self.settings.shared.disk.limit
    }

    /// How many cached events `drop_oldest` must evict for one more of
    /// `size` to fit this queue's disk limits and the hub-wide one. Rows
    /// already waiting to be deleted (acked or evicted) count as gone, so
    /// pushes right after an eviction don't evict again for the same room.
    fn evictions_for(&self, state: &State, size: usize, max: Option<usize>) -> Option<Evictions> {
        let spec = self.spec();
        let disk = &self.settings.shared.disk;
        let gone = state.deletes.len() as u64;
        let gone_bytes: u64 = state.deletes.iter().map(|(_, size)| *size as u64).sum();
        let events = state.rows + (state.writing + state.pending.len()) as u64;
        let bytes = state.rows_bytes + (state.writing_bytes + state.pending_bytes) as u64;
        let size = size as u64;
        super::evictions_needed(&state.items, max, |n, freed| {
            events < spec.disk_max_events + gone + n
                && bytes + size <= spec.disk_max_bytes + gone_bytes + freed
                && disk.used() + size <= disk.limit + gone_bytes + freed
        })
    }

    pub(super) async fn push(
        &self,
        event: DataEvent,
        size: usize,
        when_full: WhenFull,
        block_timeout: Duration,
        max_evictions: Option<usize>,
    ) -> Push {
        let deadline = Instant::now() + block_timeout;
        let mut event = Some(event);
        loop {
            let room = self.room.notified();
            tokio::pin!(room);
            room.as_mut().enable();
            // Decide under the lock (`None`: wait for room); act after releasing it.
            let decided = 'locked: {
                // Declared before the lock, so it's dropped after it.
                let evicted: Vec<Queued>;
                let mut state = self.state();
                if state.closed {
                    break 'locked Some(Push::Closed);
                }
                let disk = &self.settings.shared.disk;
                let mut reserved = self.has_room(&state, size) && disk.try_add(size as u64);
                if !reserved
                    && when_full == WhenFull::DropOldest
                    && !state.disk_blocked
                    && state.pending.len() < pending_cap(self.spec())
                    && let Some(plan) = self.evictions_for(&state, size, max_evictions)
                {
                    // The oldest undelivered events are in memory (each with
                    // its row): evict them. Their rows, and their bytes in the
                    // hub-wide gauge, go when the task deletes them; the plan
                    // already counted them as gone, so take the bytes without
                    // the limit check (the gauge is over its limit until then).
                    evicted = evict_oldest(&mut state.items, plan, &self.counters.evicted);
                    state.bytes -= plan.bytes as usize;
                    let rows = evicted
                        .iter()
                        .map(|q| (q.row.expect("cached events have a row"), q.size));
                    state.deletes.extend(rows);
                    disk.add(size as u64);
                    reserved = true;
                }
                if reserved {
                    let ticket = state.next_ticket;
                    state.next_ticket += 1;
                    state.pending_bytes += size;
                    state.pending.push_back(Pending {
                        event: event.take().expect("pushed once"),
                        size,
                        ticket,
                    });
                    break 'locked Some(Push::Queued(ticket));
                }
                if when_full != WhenFull::Block {
                    break 'locked Some(Push::Full);
                }
                None
            };
            if let Some(result) = decided {
                if matches!(result, Push::Queued(_)) {
                    // The task writes it (and deletes what was evicted).
                    self.work.notify_one();
                }
                return result;
            }
            if tokio::time::timeout_at(deadline, room).await.is_err() {
                return Push::Timeout;
            }
        }
    }

    /// Waits until `ticket`'s event is committed. `false` if its write failed
    /// or the queue closed first.
    pub(super) async fn committed(&self, ticket: u64) -> bool {
        let mut rx = self.committed.subscribe();
        loop {
            if *rx.borrow_and_update() >= ticket {
                return !self.state().failed.remove(&ticket);
            }
            if self.state().closed {
                return false;
            }
            let closed = self.room.notified();
            tokio::select! {
                changed = rx.changed() => if changed.is_err() { return false; },
                _ = closed => {}
            }
        }
    }

    pub(super) async fn pop(&self) -> Option<Queued> {
        loop {
            let pushed = self.pushed.notified();
            tokio::pin!(pushed);
            pushed.as_mut().enable();
            // Decide under the lock; act after releasing it.
            let (next, wake_task) = {
                let mut state = self.state();
                if state.closed {
                    (Next::Closed, false)
                } else if let Some(item) = state.items.pop_front() {
                    state.bytes -= item.size;
                    // Memory below the watermark: the task loads more.
                    let refill = self.needs_refill(&state);
                    (Next::Item(item), refill)
                } else {
                    // Empty: the task loads rows still on disk, if any.
                    (Next::Wait, state.not_loaded > 0)
                }
            };
            // The lock is released, so the task doesn't wake up just to wait for it.
            if wake_task {
                self.work.notify_one();
            }
            match next {
                Next::Closed => return None,
                Next::Item(item) => return Some(item),
                Next::Wait => pushed.await,
            }
        }
    }

    fn needs_refill(&self, state: &State) -> bool {
        let spec = self.spec();
        state.not_loaded > 0
            && (state.items.len() as f64 <= spec.memory_max_events as f64 * spec.refill_watermark
                || state.bytes as f64 <= spec.memory_max_bytes as f64 * spec.refill_watermark)
    }

    pub(super) fn requeue(&self, item: Queued) {
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

    /// The event is done with (acked or dead-lettered): its row goes.
    pub(super) fn remove(&self, item: Queued) {
        let Some(row) = item.row else { return };
        {
            let mut state = self.state();
            if item.epoch != state.epoch {
                // Its file was purged: nothing to delete.
                return;
            }
            state.deletes.push((row, item.size));
        }
        self.work.notify_one();
    }

    pub(super) fn stats(&self) -> DurableStats {
        let state = self.state();
        DurableStats {
            len: state.items.len(),
            bytes: state.bytes,
            disk_len: state.rows + (state.writing + state.pending.len()) as u64,
            disk_bytes: state.rows_bytes + (state.writing_bytes + state.pending_bytes) as u64,
            disk_blocked: state.disk_blocked,
        }
    }

    /// Starts over (see the module docs): drops every event, in flight too,
    /// and replaces the file with a fresh one. Returns how many were dropped.
    pub(super) async fn purge(&self) -> u64 {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.state().purge = Some(tx);
        self.work.notify_one();
        rx.await.unwrap_or(0)
    }

    /// Stops the task; with `delete`, the file goes too (the node was
    /// removed). Otherwise it's kept for the next start.
    pub(super) async fn finish(&self, delete: bool) {
        {
            let mut state = self.state();
            state.closed = true;
            state.delete_on_close = delete;
        }
        self.pushed.notify_waiters();
        self.room.notify_waiters();
        self.work.notify_one();
        let task = lock(&self.task).take();
        if let Some(task) = task {
            let _ = task.await;
        }
    }
}

pub(super) struct DurableStats {
    pub len: usize,
    pub bytes: usize,
    pub disk_len: u64,
    pub disk_bytes: u64,
    pub disk_blocked: bool,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn pending_cap(spec: &SpillSpec) -> usize {
    spec.write_batch.max(1) * 2
}

async fn run(core: Arc<DurableCore>) {
    let s = &core.settings;
    // A fresh start takes over the node's files (the oldest one is kept, the
    // others are merged into it); otherwise a new file.
    let existing = if s.adopt { node_files(&s.dir, &s.node_id) } else { vec![] };
    let file = existing.first().cloned().unwrap_or_else(|| new_file(&core));
    let merge = existing.get(1..).unwrap_or_default();
    let mut store = match Store::open(&s.dir, &file, merge).await {
        Ok(store) => {
            {
                let mut state = core.state();
                state.rows = store.rows;
                state.rows_bytes = store.rows_bytes;
                state.not_loaded = store.rows;
                state.replay_until = store.max_seq;
                state.ready = true;
            }
            s.shared.disk.add(store.rows_bytes);
            if store.rows > 0 {
                log::info!(
                    "Spool {}: replaying {} events kept for node {}",
                    store.file.display(),
                    store.rows,
                    s.node_id
                );
            }
            Some(store)
        }
        Err(err) => {
            log::error!(
                "Spool {}: couldn't open the durable queue of node {}: {err}; \
                 every event is refused until the queue is purged",
                file.display(),
                s.node_id
            );
            // A purge deletes what it couldn't open, too.
            let mut broken = existing;
            if broken.is_empty() {
                broken.push(file);
            }
            for f in &broken {
                core.state().unopened.push(f.clone());
            }
            None
        }
    };
    let guard_log = RateLimit::new(Duration::from_secs(60));
    loop {
        if core.state().closed {
            break;
        }
        let purge_requested = core.state().purge.take();
        if let Some(done) = purge_requested {
            store = purge(&core, store.take(), done).await;
            continue;
        }
        let Some(store) = store.as_ref() else {
            // No file: refuse every write until a purge starts a fresh one.
            refuse_pending(&core);
            core.work.notified().await;
            continue;
        };
        let mut progressed = false;
        progressed |= delete(&core, store).await;
        progressed |= refill(&core, store).await;
        let partial = {
            let state = core.state();
            !state.pending.is_empty()
                && !state.disk_blocked
                && state.pending.len() < s.spec.write_batch
        };
        if partial {
            tokio::time::sleep(BATCH_WINDOW).await;
        }
        progressed |= write(&core, store, &guard_log).await;
        if progressed {
            continue;
        }
        if core.state().disk_blocked {
            let _ = tokio::time::timeout(BLOCKED_RECHECK, core.work.notified()).await;
        } else {
            core.work.notified().await;
        }
    }
    // Acks that came in until the end still count.
    if let Some(store) = &store {
        delete(&core, store).await;
    }
    let (delete_file, rows_bytes, pending_bytes) = {
        let mut state = core.state();
        // Unwritten events are lost; their producers get no ack (at-least-once).
        // Mark them failed before `committed` jumps to the end below, or a
        // producer asking later would read "committed".
        let pending_bytes = state.pending_bytes;
        let pending: Vec<Pending> = state.pending.drain(..).collect();
        for p in &pending {
            state.failed.insert(p.ticket);
        }
        state.pending_bytes = 0;
        (state.delete_on_close, state.rows_bytes, pending_bytes)
    };
    s.shared.disk.sub(rows_bytes + pending_bytes as u64);
    core.committed.send_replace(u64::MAX);
    if let Some(store) = store {
        store.close(delete_file).await;
    }
}

/// A new spool file name for this node.
fn new_file(core: &DurableCore) -> PathBuf {
    let s = &core.settings;
    s.dir.join(format!("{}.{}.db", s.node_id, ulid::Ulid::generate()))
}

/// Without a file: every event waiting to be written is refused (its
/// producer is told it wasn't stored).
fn refuse_pending(core: &DurableCore) {
    {
        let mut state = core.state();
        let pending: Vec<Pending> = state.pending.drain(..).collect();
        state.pending_bytes = 0;
        for p in &pending {
            state.failed.insert(p.ticket);
            core.settings.shared.disk.sub(p.size as u64);
        }
        if let Some(last) = pending.last() {
            core.committed.send_replace(last.ticket);
        }
    }
    core.room.notify_waiters();
}

/// Writes pending events in batches (free space permitting).
async fn write(core: &DurableCore, store: &Store, guard_log: &RateLimit) -> bool {
    let s = &core.settings;
    let counts = &core.counters;
    let mut progressed = false;
    loop {
        let batch: Vec<Pending> = {
            let mut state = core.state();
            if state.pending.is_empty() {
                return progressed;
            }
            let n = state.pending.len().min(s.spec.write_batch.max(1));
            let batch: Vec<Pending> = state.pending.drain(..n).collect();
            let bytes: usize = batch.iter().map(|p| p.size).sum();
            state.pending_bytes -= bytes;
            state.writing = batch.len();
            state.writing_bytes = bytes;
            batch
        };
        let bytes: usize = batch.iter().map(|p| p.size).sum();
        let enough = match available_space(&s.dir).await {
            Ok(free) => free >= s.shared.min_free_bytes.saturating_add(bytes as u64),
            Err(err) => {
                log::error!("Spool {}: can't check free space: {err}", s.dir.display());
                false
            }
        };
        if !enough {
            if guard_log.allow() {
                log::warn!(
                    "Spool {}: less than {} bytes would be left free; durable events wait",
                    s.dir.display(),
                    s.shared.min_free_bytes
                );
            }
            let mut state = core.state();
            for p in batch.into_iter().rev() {
                state.pending_bytes += p.size;
                state.pending.push_front(p);
            }
            state.writing = 0;
            state.writing_bytes = 0;
            state.disk_blocked = true;
            return progressed;
        }
        progressed = true;
        let last_ticket = batch.last().map_or(0, |p| p.ticket);
        let written = store.append(&batch).await;
        {
            let mut state = core.state();
            state.disk_blocked = false;
            state.writing = 0;
            state.writing_bytes = 0;
            match written {
                Ok(rows) => {
                    state.rows += rows.len() as u64;
                    state.rows_bytes += bytes as u64;
                    counts.spilled.fetch_add(rows.len() as u64, Relaxed);
                    counts.disk_bytes_written.fetch_add(bytes as u64, Relaxed);
                    // Straight into memory while nothing older is on disk only
                    // and there's room; otherwise a refill brings them in.
                    for (p, row) in batch.into_iter().zip(rows) {
                        if state.not_loaded == 0 && fits_memory(core, &state, p.size) {
                            push_memory(core, &mut state, p.event, p.size, row);
                        } else {
                            state.not_loaded += 1;
                        }
                    }
                }
                Err(err) => {
                    log::error!(
                        "Spool {}: couldn't write {} durable events (their producers are told): {err}",
                        s.dir.display(),
                        batch.len()
                    );
                    s.shared.disk.sub(bytes as u64);
                    counts.dropped_disk_error.fetch_add(batch.len() as u64, Relaxed);
                    for p in &batch {
                        state.failed.insert(p.ticket);
                    }
                }
            }
        }
        core.committed.send_replace(last_ticket);
        core.pushed.notify_waiters();
        core.room.notify_waiters();
    }
}

fn fits_memory(core: &DurableCore, state: &State, size: usize) -> bool {
    let spec = core.spec();
    state.items.len() < spec.memory_max_events
        && (state.bytes + size <= spec.memory_max_bytes || state.items.is_empty())
}

fn push_memory(core: &DurableCore, state: &mut State, event: DataEvent, size: usize, row: i64) {
    let gauge = core.settings.shared.memory.clone();
    gauge.add(size as u64);
    state.bytes += size;
    state.last_loaded = state.last_loaded.max(row);
    let epoch = state.epoch;
    state.items.push_back(Queued {
        event,
        size,
        row: Some(row),
        epoch,
        _hold: Hold::Soft(SoftShare {
            gauge,
            bytes: size as u64,
        }),
    });
}

/// Loads the next rows into memory while it's below the watermark.
async fn refill(core: &DurableCore, store: &Store) -> bool {
    let counts = &core.counters;
    let (after, room_events, room_bytes, at_least_one, replay_until) = {
        let state = core.state();
        if state.closed || !core.needs_refill(&state) {
            return false;
        }
        let spec = core.spec();
        (
            state.last_loaded,
            spec.memory_max_events.saturating_sub(state.items.len()),
            spec.memory_max_bytes.saturating_sub(state.bytes),
            state.items.is_empty(),
            state.replay_until,
        )
    };
    if room_events == 0 && !at_least_one {
        return false;
    }
    let rows = match store.load(after, room_events.max(1), room_bytes, at_least_one).await {
        Ok(rows) => rows,
        Err(err) => {
            log::error!("Spool {}: couldn't read: {err}", core.settings.dir.display());
            return false;
        }
    };
    if rows.is_empty() {
        return false;
    }
    {
        let mut state = core.state();
        for (row, size, event) in rows {
            state.not_loaded = state.not_loaded.saturating_sub(1);
            state.last_loaded = state.last_loaded.max(row);
            match event {
                Some(mut event) => {
                    if row <= replay_until {
                        // Kept from before a restart: it may have been delivered.
                        event.attempt = event.attempt.max(1) + 1;
                    }
                    push_memory(core, &mut state, event, size, row);
                    counts.refilled.fetch_add(1, Relaxed);
                }
                None => {
                    log::error!("Spool: skipped an unreadable event");
                    state.deletes.push((row, size));
                }
            }
        }
    }
    core.pushed.notify_waiters();
    true
}

/// Deletes the rows of events that are done with.
async fn delete(core: &DurableCore, store: &Store) -> bool {
    let deletes = std::mem::take(&mut core.state().deletes);
    if deletes.is_empty() {
        return false;
    }
    let rows: Vec<i64> = deletes.iter().map(|(row, _)| *row).collect();
    let bytes: usize = deletes.iter().map(|(_, size)| size).sum();
    match store.delete(&rows).await {
        Ok(()) => {
            let emptied = {
                let mut state = core.state();
                state.rows = state.rows.saturating_sub(rows.len() as u64);
                state.rows_bytes = state.rows_bytes.saturating_sub(bytes as u64);
                state.rows == 0
            };
            core.settings.shared.disk.sub(bytes as u64);
            if emptied {
                store.vacuum().await;
            }
            core.room.notify_waiters();
        }
        Err(err) => {
            log::error!(
                "Spool {}: couldn't delete delivered events (they may be delivered again): {err}",
                core.settings.dir.display()
            );
        }
    }
    true
}

/// Drops what's queued: memory, pending writes, and the rows not handed out.
/// Starts over: everything the queue holds is dropped (cached, on disk only,
/// waiting to be written, and in flight with the plugin), the file is
/// deleted whatever state it's in, and a fresh one is opened. Answers `done`
/// with how many events were dropped. `None` if no fresh file could be
/// opened: then events are refused until the next purge.
async fn purge(
    core: &DurableCore,
    old: Option<Store>,
    done: tokio::sync::oneshot::Sender<u64>,
) -> Option<Store> {
    let s = &core.settings;
    let (purged, held_bytes, unopened) = {
        // Declared before the lock, so it's dropped after it: freeing the
        // cached events gives back their share of the memory limit.
        let _cached: VecDeque<Queued>;
        let mut state = core.state();
        let pending: Vec<Pending> = state.pending.drain(..).collect();
        for p in &pending {
            // Their producers are told the write failed.
            state.failed.insert(p.ticket);
        }
        if let Some(last) = pending.last() {
            core.committed.send_replace(last.ticket);
        }
        // Every row counts: cached, on disk only, and in flight.
        let purged = state.rows + pending.len() as u64;
        let held_bytes = state.rows_bytes + state.pending_bytes as u64;
        _cached = std::mem::take(&mut state.items);
        state.bytes = 0;
        state.pending_bytes = 0;
        state.rows = 0;
        state.rows_bytes = 0;
        state.not_loaded = 0;
        state.deletes.clear();
        // The old file's rows are gone, in flight or not: a new epoch, so a
        // late ack or requeue of one is ignored (the fresh file numbers its
        // rows from 1 again).
        state.epoch += 1;
        state.last_loaded = 0;
        state.replay_until = 0;
        (purged, held_bytes, std::mem::take(&mut state.unopened))
    };
    s.shared.disk.sub(held_bytes);
    // The old file goes whatever state it's in (WAL and shared-memory files too).
    if let Some(old) = old {
        old.close(true).await;
    }
    for file in &unopened {
        delete_files(file).await;
    }
    let file = new_file(core);
    let fresh = match Store::open(&s.dir, &file, &[]).await {
        Ok(store) => {
            log::warn!(
                "Spool {}: purged node {}'s queue ({purged} events dropped); started a fresh file",
                file.display(),
                s.node_id
            );
            Some(store)
        }
        Err(err) => {
            log::error!(
                "Spool {}: purged node {}'s queue, but couldn't start a fresh file: {err}; \
                 every event is refused until the next purge",
                file.display(),
                s.node_id
            );
            core.state().unopened.push(file);
            None
        }
    };
    let _ = done.send(purged);
    core.pushed.notify_waiters();
    core.room.notify_waiters();
    fresh
}

async fn available_space(dir: &Path) -> std::io::Result<u64> {
    tokio::fs::create_dir_all(dir).await?;
    let dir = dir.to_path_buf();
    tokio::task::spawn_blocking(move || fs4::available_space(dir))
        .await
        .map_err(std::io::Error::other)?
}

/// The node's files in `dir`: `<node-id>.<ulid>.db`, oldest first.
fn node_files(dir: &Path, node_id: &str) -> Vec<PathBuf> {
    let prefix = format!("{node_id}.");
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(&prefix) && n.ends_with(".db"))
        })
        .collect();
    files.sort();
    files
}

/// Deletes a spool file and its WAL companions.
pub(crate) async fn delete_files(file: &Path) {
    for suffix in ["", "-wal", "-shm"] {
        let mut path = file.as_os_str().to_owned();
        path.push(suffix);
        let _ = tokio::fs::remove_file(PathBuf::from(path)).await;
    }
}

struct Store {
    pool: SqlitePool,
    file: PathBuf,
    rows: u64,
    rows_bytes: u64,
    max_seq: i64,
}

impl Store {
    /// Opens (or creates) `file` and merges `merge` into it.
    async fn open(dir: &Path, file: &Path, merge: &[PathBuf]) -> Result<Self, sqlx::Error> {
        tokio::fs::create_dir_all(dir)
            .await
            .map_err(sqlx::Error::Io)?;
        let file = file.to_path_buf();
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(&file)
                    .create_if_missing(true)
                    .journal_mode(SqliteJournalMode::Wal)
                    .synchronous(SqliteSynchronous::Full)
                    .auto_vacuum(SqliteAutoVacuum::Incremental),
            )
            .await?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS events (
                seq          INTEGER PRIMARY KEY AUTOINCREMENT,
                enqueue_time INTEGER NOT NULL,
                size         INTEGER NOT NULL,
                header       BLOB    NOT NULL,
                event        BLOB    NOT NULL
            )",
        )
        .execute(&pool)
        .await?;
        // Newer files of the same node (e.g. a queue replaced by a reload
        // before a crash) are merged in after the oldest one's rows.
        for other in merge {
            let merged = async {
                let mut conn = pool.acquire().await?;
                sqlx::query("ATTACH DATABASE $1 AS other")
                    .bind(other.to_string_lossy().into_owned())
                    .execute(&mut *conn)
                    .await?;
                let copied = sqlx::query(
                    "INSERT INTO events (enqueue_time, size, header, event)
                     SELECT enqueue_time, size, header, event FROM other.events ORDER BY seq",
                )
                .execute(&mut *conn)
                .await;
                sqlx::query("DETACH DATABASE other").execute(&mut *conn).await?;
                copied.map(|_| ())
            };
            match merged.await {
                Ok(()) => delete_files(other).await,
                Err(err) => log::error!(
                    "Spool {}: couldn't merge it into {}: {err}",
                    other.display(),
                    file.display()
                ),
            }
        }
        let row = sqlx::query(
            "SELECT count(*) AS n, coalesce(sum(size), 0) AS bytes, coalesce(max(seq), 0) AS max_seq FROM events",
        )
        .fetch_one(&pool)
        .await?;
        Ok(Self {
            pool,
            file,
            rows: u64::try_from(row.get::<i64, _>("n")).unwrap_or(0),
            rows_bytes: u64::try_from(row.get::<i64, _>("bytes")).unwrap_or(0),
            max_seq: row.get("max_seq"),
        })
    }

    /// One transaction for the whole batch; returns the new rows' seqs.
    async fn append(&self, batch: &[Pending]) -> Result<Vec<i64>, sqlx::Error> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX));
        let mut tx = self.pool.begin().await?;
        let mut rows = Vec::with_capacity(batch.len());
        for p in batch {
            let header = FrameHeader {
                metadata: p.event.metadata.clone(),
                attempt: p.event.attempt,
                ..Default::default()
            }
            .encode_to_vec();
            let row = sqlx::query(
                "INSERT INTO events (enqueue_time, size, header, event) VALUES ($1, $2, $3, $4) RETURNING seq",
            )
            .bind(now)
            .bind(i64::try_from(p.size).unwrap_or(i64::MAX))
            .bind(header)
            .bind(p.event.body.to_vec())
            .fetch_one(&mut *tx)
            .await?;
            rows.push(row.get::<i64, _>("seq"));
        }
        tx.commit().await?;
        Ok(rows)
    }

    /// The rows after `after`, oldest first, that fit (at least one with
    /// `at_least_one`). They stay in the file.
    async fn load(
        &self,
        after: i64,
        max_events: usize,
        max_bytes: usize,
        at_least_one: bool,
    ) -> Result<Vec<(i64, usize, Option<DataEvent>)>, sqlx::Error> {
        let limit = i64::try_from(max_events.max(1)).unwrap_or(i64::MAX);
        let rows = sqlx::query(
            "SELECT seq, size, header, event FROM events WHERE seq > $1 ORDER BY seq LIMIT $2",
        )
        .bind(after)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        let mut out = vec![];
        let mut bytes = 0usize;
        for row in rows {
            let size = usize::try_from(row.get::<i64, _>("size")).unwrap_or(usize::MAX);
            if !(out.is_empty() && at_least_one) && bytes + size > max_bytes {
                break;
            }
            bytes += size;
            let header: Vec<u8> = row.get("header");
            let body: Vec<u8> = row.get("event");
            let event = FrameHeader::decode(header.as_slice()).ok().map(|h| DataEvent {
                body: body.into(),
                metadata: h.metadata,
                attempt: h.attempt,
            });
            out.push((row.get("seq"), size, event));
        }
        Ok(out)
    }

    async fn delete(&self, rows: &[i64]) -> Result<(), sqlx::Error> {
        if rows.is_empty() {
            return Ok(());
        }
        let mut tx = self.pool.begin().await?;
        for row in rows {
            sqlx::query("DELETE FROM events WHERE seq = $1")
                .bind(row)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await
    }

    async fn vacuum(&self) {
        let _ = sqlx::query("PRAGMA incremental_vacuum")
            .execute(&self.pool)
            .await;
    }

    async fn close(self, delete: bool) {
        self.pool.close().await;
        if delete {
            delete_files(&self.file).await;
        }
    }
}

/// Events in the spool files of a node (or every file in `dir` with `None`),
/// for a purge of a scene that isn't running. Deletes the files.
pub(crate) async fn purge_files(dir: &Path, node_id: Option<&str>) -> u64 {
    let files: Vec<PathBuf> = match node_id {
        Some(node) => node_files(dir, node),
        None => std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "db"))
            .collect(),
    };
    let mut purged = 0;
    for file in files {
        let count = async {
            let pool = SqlitePoolOptions::new()
                .max_connections(1)
                .connect_with(SqliteConnectOptions::new().filename(&file))
                .await?;
            let n: i64 = sqlx::query("SELECT count(*) AS n FROM events")
                .fetch_one(&pool)
                .await?
                .get("n");
            pool.close().await;
            Ok::<_, sqlx::Error>(u64::try_from(n).unwrap_or(0))
        };
        purged += count.await.unwrap_or(0);
        delete_files(&file).await;
    }
    purged
}

#[cfg(test)]
mod tests {
    use super::super::{Budget, NodeQueue, Pushed, QueueSettings, SpillShared};
    use super::*;
    use crate::modules::supervisor::SpillSpec;

    const SIZE: usize = 10;
    const WAIT: Duration = Duration::from_secs(5);

    fn event(n: usize) -> DataEvent {
        DataEvent {
            body: n.to_string().into(),
            attempt: 1,
            ..Default::default()
        }
    }

    fn shared(dir: &Path) -> SpillShared {
        SpillShared::new(dir.join("spool"), false, 1 << 30, 1 << 30, 0)
    }

    fn queue(shared: &SpillShared, memory_events: usize, adopt: bool) -> Arc<NodeQueue> {
        NodeQueue::new_durable(
            QueueSettings {
                capacity: 0, // unused for durable queues
                max_bytes: 0,
                when_full: WhenFull::DropNewest,
                block_timeout: Duration::from_millis(200),
                max_evictions: None,
            },
            Budget::new(1 << 30),
            DurableSettings {
                spec: SpillSpec {
                    memory_max_events: memory_events,
                    memory_max_bytes: 1 << 20,
                    disk_max_events: 100,
                    disk_max_bytes: 1 << 20,
                    refill_watermark: 0.25,
                    write_batch: 4,
                },
                dir: shared.scene_dir("scene"),
                node_id: "node".into(),
                shared: shared.clone(),
                adopt,
            },
        )
    }

    async fn push(q: &NodeQueue, n: usize) {
        let (pushed, ticket) = q.push_tracked(event(n), SIZE).await;
        assert_eq!(pushed, Pushed::Queued);
        let committed = tokio::time::timeout(WAIT, q.committed(ticket.expect("durable")));
        assert!(committed.await.expect("timed out committing"), "stored");
    }

    async fn pop(q: &NodeQueue) -> Queued {
        tokio::time::timeout(WAIT, q.pop())
            .await
            .expect("timed out popping")
            .expect("queue closed")
    }

    async fn wait_rows(q: &NodeQueue, rows: u64) {
        let deadline = std::time::Instant::now() + WAIT;
        while q.stats().disk_len != rows {
            assert!(
                std::time::Instant::now() < deadline,
                "expected {rows} rows, have {:?}",
                q.stats()
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn events_stay_on_disk_until_delivered_and_survive_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let shared = shared(dir.path());
        let q = queue(&shared, 2, true);
        for n in 0..5 {
            push(&q, n).await;
        }
        assert!(q.stats().durable);
        wait_rows(&q, 5).await;

        // Two handed out: 0 acked, 1 still in flight when the hub "crashes".
        let first = pop(&q).await;
        assert_eq!((first.event.label(), first.event.attempt), ("0".into(), 1));
        q.delivered(first);
        let in_flight = pop(&q).await;
        assert_eq!(in_flight.event.label(), "1");
        wait_rows(&q, 4).await;
        drop(in_flight); // never acked
        q.finish(false).await;

        // The next start replays everything not acked, in order; it may have
        // been delivered before, so the attempt goes up.
        let q = queue(&shared, 2, true);
        let mut seen = vec![];
        for _ in 0..4 {
            let item = pop(&q).await;
            assert_eq!(item.event.attempt, 2, "{}", item.event.label());
            seen.push(item.event.label());
            q.delivered(item);
        }
        assert_eq!(seen, ["1", "2", "3", "4"]);
        wait_rows(&q, 0).await;
        q.finish(true).await;
        assert_eq!(
            std::fs::read_dir(shared.scene_dir("scene")).unwrap().count(),
            0,
            "a removed node's file goes"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn requeued_events_come_back_first() {
        let dir = tempfile::tempdir().unwrap();
        let shared = shared(dir.path());
        let q = queue(&shared, 8, true);
        for n in 0..4 {
            push(&q, n).await;
        }
        let zero = pop(&q).await;
        let one = pop(&q).await;
        // A disconnect: both go back, in order.
        q.requeue(one);
        q.requeue(zero);
        assert_eq!(labels(&q, 4).await, ["0", "1", "2", "3"]);
        q.finish(true).await;
    }

    fn files(shared: &SpillShared) -> Vec<PathBuf> {
        node_files(&shared.scene_dir("scene"), "node")
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_purge_starts_over_with_a_fresh_file() {
        let dir = tempfile::tempdir().unwrap();
        let shared = shared(dir.path());
        let q = queue(&shared, 8, true);
        for n in 0..4 {
            push(&q, n).await;
        }
        let old_file = files(&shared);
        assert_eq!(old_file.len(), 1);
        // "0" and "1" are in flight with the plugin, "2" and "3" queued:
        // all four go.
        let zero = pop(&q).await;
        let one = pop(&q).await;
        assert_eq!(q.purge().await, 4);
        assert_eq!((q.stats().len, q.stats().disk_len), (0, 0));
        assert_eq!(shared.disk.used(), 0);
        let new_file = files(&shared);
        assert_eq!(new_file.len(), 1, "{new_file:?}");
        assert_ne!(new_file, old_file, "a fresh file, the old one deleted");

        // The fresh file numbers from 1 again; a late ack of a purged event
        // (its row was 2) doesn't touch it, and a requeue doesn't bring one back.
        push(&q, 9).await;
        q.delivered(one);
        q.requeue(zero);
        wait_rows(&q, 1).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(q.stats().disk_len, 1, "the new event's row is still there");
        q.finish(false).await;

        // After a restart, only what came after the purge is replayed.
        let q = queue(&shared, 8, true);
        let item = pop(&q).await;
        assert_eq!(item.event.label(), "9");
        q.delivered(item);
        wait_rows(&q, 0).await;
        q.finish(true).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_purge_replaces_a_file_that_wont_open() {
        let dir = tempfile::tempdir().unwrap();
        let shared = shared(dir.path());
        let scene = shared.scene_dir("scene");
        std::fs::create_dir_all(&scene).unwrap();
        let broken = scene.join(format!("node.{}.db", ulid::Ulid::generate()));
        std::fs::write(&broken, vec![0xAB; 8192]).unwrap(); // not a database
        let q = queue(&shared, 8, true);
        // Nothing can be stored meanwhile.
        let (pushed, ticket) = q.push_tracked(event(0), SIZE).await;
        assert_eq!(pushed, Pushed::Queued);
        let stored = tokio::time::timeout(WAIT, q.committed(ticket.unwrap()));
        assert!(!stored.await.expect("timed out"), "refused without a file");

        q.purge().await;
        assert!(!broken.exists(), "the broken file is gone");
        push(&q, 1).await;
        assert_eq!(labels(&q, 1).await, ["1"]);
        q.finish(true).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_fresh_start_merges_every_file_of_the_node_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let shared = shared(dir.path());
        // An older queue's file, then a newer one (e.g. a reload before a crash).
        let old = queue(&shared, 8, false);
        push(&old, 1).await;
        push(&old, 2).await;
        old.finish(false).await;
        tokio::time::sleep(Duration::from_millis(2)).await; // a later ULID
        let new = queue(&shared, 8, false);
        push(&new, 3).await;
        new.finish(false).await;
        assert!(std::fs::read_dir(shared.scene_dir("scene")).unwrap().count() >= 2);

        let q = queue(&shared, 8, true);
        let labels: Vec<String> = [pop(&q).await, pop(&q).await, pop(&q).await]
            .iter()
            .map(|i| i.event.label())
            .collect();
        assert_eq!(labels, ["1", "2", "3"]);
        q.finish(false).await;
        let files: Vec<_> = std::fs::read_dir(shared.scene_dir("scene"))
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".db"))
            .collect();
        assert_eq!(files.len(), 1, "merged into one");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn memory_is_a_cache_refilled_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let shared = shared(dir.path());
        let q = queue(&shared, 2, true);
        for n in 0..20 {
            push(&q, n).await;
        }
        assert!(q.stats().len <= 2, "{:?}", q.stats());
        for n in 0..20 {
            let item = pop(&q).await;
            assert_eq!(item.event.label(), n.to_string());
            q.delivered(item);
        }
        wait_rows(&q, 0).await;
        assert_eq!(q.stats().refilled + 2, 20, "{:?}", q.stats());
        q.finish(true).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_full_disk_applies_the_policy() {
        let dir = tempfile::tempdir().unwrap();
        let shared = shared(dir.path());
        let q = NodeQueue::new_durable(
            QueueSettings {
                capacity: 0,
                max_bytes: 0,
                when_full: WhenFull::DropNewest,
                block_timeout: Duration::from_millis(100),
                max_evictions: None,
            },
            Budget::new(1 << 30),
            DurableSettings {
                spec: SpillSpec {
                    memory_max_events: 8,
                    memory_max_bytes: 1 << 20,
                    disk_max_events: 2,
                    disk_max_bytes: 1 << 20,
                    refill_watermark: 0.25,
                    write_batch: 4,
                },
                dir: shared.scene_dir("scene"),
                node_id: "node".into(),
                shared: shared.clone(),
                adopt: true,
            },
        );
        push(&q, 1).await;
        push(&q, 2).await;
        let (pushed, ticket) = q.push_tracked(event(3), SIZE).await;
        assert_eq!(pushed, Pushed::Dropped(super::super::DropReason::Full));
        assert!(ticket.is_none());
        // Delivering one frees a row.
        let item = pop(&q).await;
        q.delivered(item);
        wait_rows(&q, 1).await;
        push(&q, 3).await;
        q.finish(true).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_event_dropped_unwritten_at_finish_is_never_reported_committed() {
        let dir = tempfile::tempdir().unwrap();
        // The free-space guard always trips, so the event stays unwritten.
        let shared = SpillShared::new(dir.path().join("spool"), false, 1 << 30, 1 << 30, u64::MAX);
        let q = queue(&shared, 8, true);
        let (pushed, ticket) = q.push_tracked(event(0), SIZE).await;
        assert_eq!(pushed, Pushed::Queued);
        let deadline = std::time::Instant::now() + WAIT;
        while !q.stats().disk_blocked {
            assert!(std::time::Instant::now() < deadline, "guard never tripped");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        q.finish(false).await;
        // The producer asks only now, after the task dropped the event: it
        // must hear "not stored", or it would ACK an event that was lost.
        let committed = tokio::time::timeout(WAIT, q.committed(ticket.expect("durable")));
        assert!(
            !committed.await.expect("timed out"),
            "dropped, not committed"
        );
    }

    /// A `drop_oldest` durable queue holding four committed 10-byte events,
    /// all cached in memory.
    async fn full_durable(
        shared: &SpillShared,
        disk_events: u64,
        disk_bytes: u64,
        max_evictions: Option<usize>,
    ) -> Arc<NodeQueue> {
        let q = NodeQueue::new_durable(
            QueueSettings {
                capacity: 0,
                max_bytes: 0,
                when_full: WhenFull::DropOldest,
                block_timeout: Duration::from_millis(100),
                max_evictions,
            },
            Budget::new(1 << 30),
            DurableSettings {
                spec: SpillSpec {
                    memory_max_events: 8,
                    memory_max_bytes: 1 << 20,
                    disk_max_events: disk_events,
                    disk_max_bytes: disk_bytes,
                    refill_watermark: 0.25,
                    write_batch: 4,
                },
                dir: shared.scene_dir("scene"),
                node_id: "node".into(),
                shared: shared.clone(),
                adopt: false,
            },
        );
        for n in 0..4 {
            push(&q, n).await;
        }
        assert_eq!(q.stats().len, 4, "all cached");
        q
    }

    async fn labels(q: &NodeQueue, n: usize) -> Vec<String> {
        let mut out = vec![];
        for _ in 0..n {
            let item = pop(q).await;
            out.push(item.event.label());
            q.delivered(item);
        }
        out
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn drop_oldest_evicts_as_many_as_a_large_event_needs() {
        let dir = tempfile::tempdir().unwrap();
        let shared = shared(dir.path());
        // 25 bytes on a full 40-byte disk need three events gone.
        let q = full_durable(&shared, 100, 40, Some(3)).await;
        let (pushed, ticket) = q.push_tracked(event(9), 25).await;
        assert_eq!(pushed, Pushed::Queued);
        assert!(q.committed(ticket.unwrap()).await);
        assert_eq!(q.stats().evicted, 3);
        wait_rows(&q, 2).await;
        assert_eq!(labels(&q, 2).await, ["3", "9"]);
        q.finish(true).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn drop_oldest_over_the_eviction_limit_evicts_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let shared = shared(dir.path());
        let q = full_durable(&shared, 100, 40, Some(2)).await;
        let (pushed, ticket) = q.push_tracked(event(9), 25).await;
        assert_eq!(pushed, Pushed::Dropped(super::super::DropReason::Full));
        assert!(ticket.is_none());
        assert_eq!(q.stats().evicted, 0);
        assert_eq!(labels(&q, 4).await, ["0", "1", "2", "3"]);
        q.finish(true).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn drop_oldest_never_evicts_and_then_refuses_at_the_hub_wide_limit() {
        let dir = tempfile::tempdir().unwrap();
        // 40 bytes of spool for the whole hub; this queue's own limits are wide.
        let shared = SpillShared::new(dir.path().join("spool"), false, 1 << 30, 40, 0);
        let q = full_durable(&shared, 100, 1 << 20, None).await;
        // 50 bytes can't fit even with everything evicted: nothing goes.
        let (pushed, _) = q.push_tracked(event(9), 50).await;
        assert_eq!(pushed, Pushed::Dropped(super::super::DropReason::Full));
        assert_eq!(q.stats().evicted, 0);
        // 25 bytes fit once three are evicted (their rows free hub-wide room).
        let (pushed, ticket) = q.push_tracked(event(8), 25).await;
        assert_eq!(pushed, Pushed::Queued);
        assert!(q.committed(ticket.unwrap()).await);
        assert_eq!(q.stats().evicted, 3);
        wait_rows(&q, 2).await;
        assert_eq!(shared.disk.used(), 35);
        assert_eq!(labels(&q, 2).await, ["3", "8"]);
        q.finish(true).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn back_to_back_pushes_evict_one_each() {
        let dir = tempfile::tempdir().unwrap();
        let shared = shared(dir.path());
        // Room for four events: each new one needs one gone, even while the
        // previous eviction's row isn't deleted yet.
        let q = full_durable(&shared, 4, 1 << 20, None).await;
        for n in [8, 9] {
            let (pushed, ticket) = q.push_tracked(event(n), SIZE).await;
            assert_eq!(pushed, Pushed::Queued);
            assert!(q.committed(ticket.unwrap()).await);
        }
        assert_eq!(q.stats().evicted, 2);
        wait_rows(&q, 4).await;
        assert_eq!(labels(&q, 4).await, ["2", "3", "8", "9"]);
        q.finish(true).await;
    }
}
