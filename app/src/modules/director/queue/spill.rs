//! The spill task: the only code that touches a queue's spool file. It
//! refills memory from disk (oldest first) and writes pending events to disk
//! in batches, one after the other, so the queue's order holds across
//! memory → disk → memory.
//!
//! The spool file is `<spool_dir>/<scene-id>/<node-id>.<ulid>.db`, created on the
//! first spill (the scene's folder is wiped when the scene launches, so it's
//! always new) and deleted when the queue finishes. Table: `events(seq, enqueue_time, size, event)`, where
//! `event` is the encoded protobuf event with its metadata. WAL with
//! `synchronous = NORMAL`: delivery is at-most-once, so an fsync per batch
//! isn't worth it.

use super::{Hold, NodeQueue, Pending, Queued, SoftShare, SpillSettings};
use crate::modules::hub::wire::DataEvent;
use crate::modules::rate_limit::RateLimit;
use flwrs_plugin::schema::transform::TransformEvent;
use prost::Message;
use sqlx::sqlite::{
    SqliteAutoVacuum, SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous,
};
use sqlx::{Row, SqlitePool};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// How long to wait for more events before writing a partial batch.
const BATCH_WINDOW: Duration = Duration::from_millis(5);
/// How often to recheck free space while the guard holds writes back.
const BLOCKED_RECHECK: Duration = Duration::from_secs(1);

pub(super) async fn run(queue: Arc<NodeQueue>) {
    let spill = queue
        .spill
        .clone()
        .expect("spill task without spill settings");
    let mut store: Option<Store> = None;
    let guard_log = RateLimit::new(Duration::from_secs(60));
    loop {
        if queue.state().closed {
            break;
        }
        let refilled = refill(&queue, &spill, &mut store).await;
        // Give a burst a moment to fill the batch.
        let partial = {
            let state = queue.state();
            !state.pending.is_empty()
                && !state.disk_blocked
                && state.pending.len() < spill.spec.write_batch
        };
        if partial {
            tokio::time::sleep(BATCH_WINDOW).await;
        }
        let wrote = flush(&queue, &spill, &mut store, &guard_log).await;
        if refilled || wrote {
            continue;
        }
        // Nothing could move (e.g. memory is full, or the next event on disk
        // doesn't fit beside what's in memory yet): sleep until a push, a pop
        // or close wakes us. A wake-up sent meanwhile is kept by `Notify`.
        if queue.state().disk_blocked {
            let _ = tokio::time::timeout(BLOCKED_RECHECK, queue.work.notified()).await;
        } else {
            queue.work.notified().await;
        }
    }
    remove(&queue, &spill, store).await;
}

/// Moves the oldest events back into memory while it's below the watermark:
/// from disk, or straight from `pending` once the disk is empty. Switches the
/// queue back to memory-only when nothing is left outside memory.
///
/// When memory is empty, the oldest event comes back even if it's larger than
/// the memory soft limit (that's what the limit being soft means), so a big
/// event can't block the queue. Returns whether any event moved.
async fn refill(queue: &NodeQueue, spill: &SpillSettings, store: &mut Option<Store>) -> bool {
    let mut moved_any = false;
    loop {
        let (room_events, room_bytes, at_least_one) = {
            let mut state = queue.state();
            if state.closed || !queue.needs_refill(&state) {
                return moved_any;
            }
            let room_events = spill
                .spec
                .memory_max_events
                .saturating_sub(state.items.len());
            let room_bytes = spill.spec.memory_max_bytes.saturating_sub(state.bytes);
            let at_least_one = state.items.is_empty();
            if room_events == 0 && !at_least_one {
                return moved_any; // full by count; the byte watermark alone doesn't help
            }
            if state.disk_len == 0 {
                // Nothing on disk: pending events are next in line.
                if state.writing == 0 {
                    let moved = move_pending(
                        queue,
                        spill,
                        &mut state,
                        room_events,
                        room_bytes,
                        at_least_one,
                    );
                    moved_any |= moved > 0;
                }
                return moved_any;
            }
            (room_events, room_bytes, at_least_one)
        };
        let Some(open) = store.as_ref() else {
            return moved_any; // disk_len > 0 means it's open
        };
        let rows = match open.take(room_events, room_bytes, at_least_one).await {
            Ok(rows) => rows,
            Err(err) => {
                log::error!("Spool {}: couldn't read: {err}", spill.file.display());
                return moved_any;
            }
        };
        if rows.is_empty() {
            // The next event doesn't fit beside what's in memory yet.
            return moved_any;
        }
        moved_any = true;
        let emptied = {
            let mut state = queue.state();
            if state.closed {
                return moved_any;
            }
            for (event, size) in rows {
                if let Some(event) = event {
                    push_memory(spill, &mut state, event, size);
                    queue.counters.refilled.fetch_add(1, Ordering::Relaxed);
                }
                state.disk_len -= 1;
                state.disk_bytes -= size as u64;
                spill.shared.disk.sub(size as u64);
            }
            let emptied = state.disk_len == 0;
            if emptied && state.pending.is_empty() && state.writing == 0 {
                state.spilling = false;
            }
            emptied
        };
        queue.pushed.notify_waiters();
        queue.popped.notify_waiters();
        if emptied && let Some(open) = store.as_ref() {
            open.vacuum().await;
        }
    }
}

fn move_pending(
    queue: &NodeQueue,
    spill: &SpillSettings,
    state: &mut super::State,
    room_events: usize,
    room_bytes: usize,
    at_least_one: bool,
) -> usize {
    let (mut moved, mut moved_bytes) = (0usize, 0usize);
    while let Some(next) = state.pending.front() {
        let fits = moved < room_events && moved_bytes + next.size <= room_bytes;
        if !fits && !(at_least_one && moved == 0) {
            break;
        }
        let Pending { event, size } = state.pending.pop_front().expect("peeked");
        state.pending_bytes -= size;
        spill.shared.disk.sub(size as u64);
        push_memory(spill, state, event, size);
        moved += 1;
        moved_bytes += size;
    }
    if state.pending.is_empty() {
        state.spilling = false;
    }
    if moved > 0 {
        queue.pushed.notify_waiters();
        queue.popped.notify_waiters();
    }
    moved
}

/// Into memory, counted against the hub-wide soft limit (which a refill may
/// exceed: it's soft).
fn push_memory(spill: &SpillSettings, state: &mut super::State, event: DataEvent, size: usize) {
    spill.shared.memory.add(size as u64);
    state.bytes += size;
    state.items.push_back(Queued {
        event,
        size,
        _hold: Hold::Soft(SoftShare {
            gauge: spill.shared.memory.clone(),
            bytes: size as u64,
        }),
    });
}

/// Writes pending events in batches, unless the free-space guard says no.
/// Returns whether anything was written (or dropped by a disk error).
async fn flush(
    queue: &NodeQueue,
    spill: &SpillSettings,
    store: &mut Option<Store>,
    guard_log: &RateLimit,
) -> bool {
    let mut progressed = false;
    loop {
        let batch: Vec<Pending> = {
            let mut state = queue.state();
            if state.closed || state.pending.is_empty() {
                return progressed;
            }
            let n = state.pending.len().min(spill.spec.write_batch.max(1));
            let batch: Vec<Pending> = state.pending.drain(..n).collect();
            let bytes: usize = batch.iter().map(|p| p.size).sum();
            state.pending_bytes -= bytes;
            state.writing = batch.len();
            state.writing_bytes = bytes;
            batch
        };
        let bytes: usize = batch.iter().map(|p| p.size).sum();

        // Free-space guard: never let a burst fill the device.
        let dir = spill.file.parent().unwrap_or(Path::new(".")).to_path_buf();
        let space = match tokio::fs::create_dir_all(&dir).await {
            Ok(()) => available_space(&dir).await,
            Err(err) => Err(err),
        };
        let enough = match space {
            Ok(free) => free >= spill.shared.min_free_bytes.saturating_add(bytes as u64),
            Err(err) => {
                log::error!("Spool {}: can't check free space: {err}", dir.display());
                false
            }
        };
        if !enough {
            if guard_log.allow() {
                log::warn!(
                    "Spool {}: less than {} bytes would be left free; holding spilled events in memory",
                    dir.display(),
                    spill.shared.min_free_bytes
                );
            }
            let mut state = queue.state();
            for p in batch.into_iter().rev() {
                state.pending_bytes += p.size;
                state.pending.push_front(p);
            }
            state.writing = 0;
            state.writing_bytes = 0;
            state.disk_blocked = true;
            return progressed;
        }

        if store.is_none() {
            match Store::open(&spill.file).await {
                Ok(opened) => *store = Some(opened),
                Err(err) => {
                    log::error!("Spool {}: couldn't open: {err}", spill.file.display());
                }
            }
        }
        let written = match store.as_ref() {
            Some(open) => open.append(&batch).await,
            None => Err(sqlx::Error::PoolClosed),
        };
        progressed = true;
        let mut state = queue.state();
        state.disk_blocked = false;
        state.writing = 0;
        state.writing_bytes = 0;
        match written {
            Ok(()) => {
                state.disk_len += batch.len() as u64;
                state.disk_bytes += bytes as u64;
                let c = &queue.counters;
                c.spilled.fetch_add(batch.len() as u64, Ordering::Relaxed);
                c.disk_bytes_written
                    .fetch_add(bytes as u64, Ordering::Relaxed);
                drop(state);
                // The pending list has room again: wake blocked producers.
                queue.popped.notify_waiters();
            }
            Err(err) => {
                log::error!(
                    "Spool {}: couldn't write {} events, dropped: {err}",
                    spill.file.display(),
                    batch.len()
                );
                spill.shared.disk.sub(bytes as u64);
                queue
                    .counters
                    .dropped_disk_error
                    .fetch_add(batch.len() as u64, Ordering::Relaxed);
                drop(state);
                queue.popped.notify_waiters(); // room appeared
            }
        }
    }
}

/// Closes and deletes the spool file; events still on disk are dropped.
async fn remove(queue: &NodeQueue, spill: &SpillSettings, store: Option<Store>) {
    let (left, left_bytes) = {
        let mut state = queue.state();
        let left = (state.disk_len, state.disk_bytes);
        state.disk_len = 0;
        state.disk_bytes = 0;
        left
    };
    spill.shared.disk.sub(left_bytes);
    if let Some(open) = store {
        open.pool.close().await;
        delete_files(&spill.file).await;
        if left > 0 {
            log::info!(
                "Spool {}: dropped {left} spilled events (at-most-once delivery)",
                spill.file.display()
            );
        }
    }
}

async fn delete_files(file: &Path) {
    for suffix in ["", "-wal", "-shm"] {
        let mut path = file.as_os_str().to_owned();
        path.push(suffix);
        let _ = tokio::fs::remove_file(PathBuf::from(path)).await;
    }
}

async fn available_space(dir: &Path) -> std::io::Result<u64> {
    let dir = dir.to_path_buf();
    tokio::task::spawn_blocking(move || fs4::available_space(dir))
        .await
        .map_err(std::io::Error::other)?
}

struct Store {
    pool: SqlitePool,
}

impl Store {
    /// Creates the spool file. The scene's spool folder is wiped when the
    /// scene launches, so there's no leftover to deal with here.
    async fn open(file: &Path) -> Result<Self, sqlx::Error> {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(file)
                    .create_if_missing(true)
                    .journal_mode(SqliteJournalMode::Wal)
                    .synchronous(SqliteSynchronous::Normal)
                    .auto_vacuum(SqliteAutoVacuum::Incremental),
            )
            .await?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS events (
                seq          INTEGER PRIMARY KEY AUTOINCREMENT,
                enqueue_time INTEGER NOT NULL,
                size         INTEGER NOT NULL,
                event        BLOB    NOT NULL
            )",
        )
        .execute(&pool)
        .await?;
        Ok(Self { pool })
    }

    /// One transaction for the whole batch.
    async fn append(&self, batch: &[Pending]) -> Result<(), sqlx::Error> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX));
        let mut tx = self.pool.begin().await?;
        for p in batch {
            sqlx::query("INSERT INTO events (enqueue_time, size, event) VALUES ($1, $2, $3)")
                .bind(now)
                .bind(i64::try_from(p.size).unwrap_or(i64::MAX))
                .bind(encode(&p.event))
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await
    }

    /// The oldest events that fit (at least one with `at_least_one`), removed
    /// from the file.
    async fn take(
        &self,
        max_events: usize,
        max_bytes: usize,
        at_least_one: bool,
    ) -> Result<Vec<(Option<DataEvent>, usize)>, sqlx::Error> {
        let limit = i64::try_from(max_events.max(1)).unwrap_or(i64::MAX);
        let rows = sqlx::query("SELECT seq, size, event FROM events ORDER BY seq LIMIT $1")
            .bind(limit)
            .fetch_all(&self.pool)
            .await?;
        let mut out = vec![];
        let (mut bytes, mut last_seq) = (0usize, None);
        for row in rows {
            let size = usize::try_from(row.get::<i64, _>("size")).unwrap_or(usize::MAX);
            if !(out.is_empty() && at_least_one) && bytes + size > max_bytes {
                break;
            }
            let blob: Vec<u8> = row.get("event");
            bytes += size;
            last_seq = Some(row.get::<i64, _>("seq"));
            let event = decode(&blob);
            if event.is_none() {
                // Can't happen for files we wrote; skip rather than stall.
                log::error!("Spool: skipped an unreadable event");
            }
            out.push((event, size));
        }
        if let Some(last) = last_seq {
            sqlx::query("DELETE FROM events WHERE seq <= $1")
                .bind(last)
                .execute(&self.pool)
                .await?;
        }
        Ok(out)
    }

    async fn vacuum(&self) {
        let _ = sqlx::query("PRAGMA incremental_vacuum")
            .execute(&self.pool)
            .await;
    }
}

/// Events are stored as protobuf (`TransformEvent` has exactly the fields of
/// every data event), metadata included, so they're delivered unchanged.
fn encode(event: &DataEvent) -> Vec<u8> {
    TransformEvent {
        plugin_id: event.instance_id.clone(),
        plugin_version: event.plugin_version.clone(),
        payload: event.payload.clone(),
        metadata: event.metadata.clone(),
    }
    .encode_to_vec()
}

fn decode(bytes: &[u8]) -> Option<DataEvent> {
    let event = TransformEvent::decode(bytes).ok()?;
    Some(DataEvent {
        instance_id: event.plugin_id,
        plugin_version: event.plugin_version,
        payload: event.payload,
        metadata: event.metadata,
    })
}

#[cfg(test)]
mod tests {
    use super::super::{DropReason, NodeQueue, Pushed, QueueSettings, SpillShared};
    use super::*;
    use crate::modules::node_config::WhenFull;
    use crate::modules::supervisor::SpillSpec;

    const SIZE: usize = 10;
    const WAIT: Duration = Duration::from_secs(5);

    fn event(n: usize) -> DataEvent {
        DataEvent {
            instance_id: n.to_string(),
            ..Default::default()
        }
    }

    fn shared(dir: &Path, memory_soft: u64, min_free: u64) -> SpillShared {
        SpillShared::new(dir.join("spool"), true, memory_soft, 1 << 30, min_free)
    }

    struct Limits {
        memory_events: usize,
        memory_bytes: usize,
        disk_events: u64,
        disk_bytes: u64,
        write_batch: usize,
        when_full: WhenFull,
    }

    impl Default for Limits {
        fn default() -> Self {
            Self {
                memory_events: 4,
                memory_bytes: 1 << 20,
                disk_events: 1_000,
                disk_bytes: 1 << 20,
                write_batch: 3,
                when_full: WhenFull::Block,
            }
        }
    }

    fn queue(shared: &SpillShared, node: &str, limits: Limits) -> Arc<NodeQueue> {
        NodeQueue::new(
            QueueSettings {
                capacity: 0, // unused with spill
                max_bytes: 0,
                when_full: limits.when_full,
                block_timeout: Duration::from_millis(200),
            },
            Budget::new(1 << 30), // the hub-wide hard limit decides "too large"
            Some(SpillSettings {
                spec: SpillSpec {
                    memory_max_events: limits.memory_events,
                    memory_max_bytes: limits.memory_bytes,
                    disk_max_events: limits.disk_events,
                    disk_max_bytes: limits.disk_bytes,
                    refill_watermark: 0.25,
                    write_batch: limits.write_batch,
                },
                file: shared.scene_dir("scene").join(format!("{node}.db")),
                shared: shared.clone(),
            }),
        )
    }

    use super::super::Budget;

    async fn pop(q: &NodeQueue) -> String {
        tokio::time::timeout(WAIT, q.pop())
            .await
            .expect("timed out popping")
            .expect("queue closed")
            .event
            .instance_id
    }

    async fn wait_until(q: &NodeQueue, what: &str, f: impl Fn(&super::super::QueueStats) -> bool) {
        let deadline = std::time::Instant::now() + WAIT;
        while !f(&q.stats()) {
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for {what}: {:?}",
                q.stats()
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn order_is_kept_across_memory_disk_and_back() {
        let dir = tempfile::tempdir().unwrap();
        let shared = shared(dir.path(), 1 << 30, 0);
        // A small deterministic random sequence (no rand dependency).
        let mut seed: u64 = 0x9e37_79b9_7f4a_7c15;
        let mut next = move || {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            seed >> 33
        };
        for round in 0..4 {
            let q = queue(&shared, &format!("n{round}"), Limits::default());
            let (mut pushed, mut popped) = (0usize, 0usize);
            for _ in 0..400 {
                let backlog = pushed - popped;
                if backlog == 0 || (backlog < 40 && next() % 3 != 0) {
                    assert_eq!(q.push(event(pushed), SIZE).await, Pushed::Queued);
                    pushed += 1;
                } else {
                    assert_eq!(pop(&q).await, popped.to_string(), "round {round}");
                    popped += 1;
                }
            }
            while popped < pushed {
                assert_eq!(pop(&q).await, popped.to_string());
                popped += 1;
            }
            let stats = q.stats();
            assert!(stats.spilled > 0 && stats.refilled > 0, "{stats:?}");
            q.finish().await;
        }
        assert_eq!(shared.disk.used(), 0, "disk gauge released");
        assert_eq!(shared.memory.used(), 0, "memory gauge released");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn once_on_disk_new_events_follow_until_it_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let shared = shared(dir.path(), 1 << 30, 0);
        let q = queue(
            &shared,
            "n",
            Limits {
                memory_events: 2,
                ..Limits::default()
            },
        );
        let file = shared.scene_dir("scene").join("n.db");
        for n in 0..2 {
            q.push(event(n), SIZE).await;
        }
        assert!(!q.stats().spilling);
        assert!(!file.exists(), "nothing spilled, no file");

        for n in 2..6 {
            q.push(event(n), SIZE).await;
        }
        assert!(q.stats().spilling);
        wait_until(&q, "a disk write", |s| s.spilled > 0).await;
        assert!(file.exists(), "created on the first spill");
        // Memory has room after a pop, but the next event still queues behind the disk.
        assert_eq!(pop(&q).await, "0");
        q.push(event(6), SIZE).await;
        for n in 1..=6 {
            assert_eq!(pop(&q).await, n.to_string());
        }
        wait_until(&q, "memory mode", |s| !s.spilling && s.disk_len == 0).await;
        q.push(event(7), SIZE).await;
        assert_eq!(
            (q.stats().len, q.stats().disk_len),
            (1, 0),
            "back to memory"
        );
        q.finish().await;
        assert!(!file.exists(), "deleted when the queue finishes");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_full_disk_applies_the_when_full_policy() {
        let dir = tempfile::tempdir().unwrap();
        let shared = shared(dir.path(), 1 << 30, 0);
        for when_full in [WhenFull::DropNewest, WhenFull::Block, WhenFull::DropOldest] {
            let name = format!("{when_full:?}");
            let q = queue(
                &shared,
                &name,
                Limits {
                    memory_events: 1,
                    disk_events: 2,
                    when_full,
                    ..Limits::default()
                },
            );
            for n in 0..3 {
                assert_eq!(q.push(event(n), SIZE).await, Pushed::Queued, "{name}");
            }
            let started = std::time::Instant::now();
            let fourth = q.push(event(3), SIZE).await;
            let expected: &[&str] = match when_full {
                WhenFull::DropNewest => {
                    assert_eq!(fourth, Pushed::Dropped(DropReason::Full));
                    &["0", "1", "2"]
                }
                WhenFull::Block => {
                    assert_eq!(fourth, Pushed::Dropped(DropReason::Timeout));
                    assert!(started.elapsed() >= Duration::from_millis(190));
                    &["0", "1", "2"]
                }
                WhenFull::DropOldest => {
                    assert_eq!(fourth, Pushed::Queued);
                    assert_eq!(q.stats().evicted, 1);
                    &["1", "2", "3"]
                }
            };
            for n in expected {
                assert_eq!(pop(&q).await, *n, "{name}");
            }
            q.finish().await;
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_free_space_guard_holds_writes_back() {
        let dir = tempfile::tempdir().unwrap();
        // Demand more free space than any disk has.
        let shared = shared(dir.path(), 1 << 30, u64::MAX);
        let q = queue(
            &shared,
            "n",
            Limits {
                memory_events: 1,
                write_batch: 1,
                when_full: WhenFull::DropNewest,
                ..Limits::default()
            },
        );
        q.push(event(0), SIZE).await; // memory
        q.push(event(1), SIZE).await; // pending; the write is refused
        wait_until(&q, "the guard", |s| s.disk_blocked).await;
        assert_eq!(
            q.push(event(2), SIZE).await,
            Pushed::Dropped(DropReason::Full)
        );
        assert!(
            !shared.scene_dir("scene").join("n.db").exists(),
            "nothing written"
        );
        // What was held in memory is still delivered, in order.
        assert_eq!(pop(&q).await, "0");
        assert_eq!(pop(&q).await, "1");
        q.finish().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_hub_wide_memory_soft_limit_sends_events_to_disk() {
        let dir = tempfile::tempdir().unwrap();
        let shared = shared(dir.path(), 25, 0);
        let first = queue(&shared, "first", Limits::default());
        let second = queue(&shared, "second", Limits::default());
        first.push(event(0), SIZE).await;
        first.push(event(1), SIZE).await;
        assert!(!first.stats().spilling);
        second.push(event(2), SIZE).await; // 30 bytes > 25 hub-wide
        assert!(second.stats().spilling, "{:?}", second.stats());
        assert_eq!(pop(&second).await, "2");
        // Oversized: bigger than the hub-wide hard memory limit.
        assert_eq!(
            second.push(event(9), (1 << 30) + 1).await,
            Pushed::Dropped(DropReason::Oversized)
        );
        first.finish().await;
        second.finish().await;
        assert_eq!(shared.memory.used(), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_event_over_the_memory_limit_goes_through_disk_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let shared = shared(dir.path(), 1 << 30, 0);
        // Memory holds 25 bytes; one event is 100.
        let q = queue(
            &shared,
            "n",
            Limits {
                memory_bytes: 25,
                ..Limits::default()
            },
        );
        let sizes = [10, 10, 100, 10, 10];
        for (n, size) in sizes.iter().enumerate() {
            assert_eq!(q.push(event(n), *size).await, Pushed::Queued, "event {n}");
        }
        assert!(
            q.stats().spilling,
            "the big event (and those after it) went to disk"
        );
        for n in 0..sizes.len() {
            assert_eq!(pop(&q).await, n.to_string());
        }
        wait_until(&q, "memory mode", |s| !s.spilling && s.disk_len == 0).await;
        q.finish().await;
        assert_eq!((shared.disk.used(), shared.memory.used()), (0, 0));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn over_the_node_limits_is_accepted_alone_within_the_hub_limit() {
        let dir = tempfile::tempdir().unwrap();
        let shared = shared(dir.path(), 1 << 30, 0);
        let q = queue(
            &shared,
            "n",
            Limits {
                memory_bytes: 25,
                disk_bytes: 50,
                when_full: WhenFull::DropNewest,
                ..Limits::default()
            },
        );
        assert_eq!(q.push(event(0), 10).await, Pushed::Queued); // memory
        // 100 bytes: over both node limits, but the disk is empty.
        assert_eq!(q.push(event(1), 100).await, Pushed::Queued);
        // With that on disk, the node's disk limit applies again.
        assert_eq!(
            q.push(event(2), 10).await,
            Pushed::Dropped(DropReason::Full)
        );
        assert_eq!(pop(&q).await, "0");
        assert_eq!(pop(&q).await, "1");
        wait_until(&q, "memory mode", |s| !s.spilling).await;
        assert_eq!(q.push(event(3), 10).await, Pushed::Queued);
        assert_eq!(pop(&q).await, "3");
        q.finish().await;
    }
}
