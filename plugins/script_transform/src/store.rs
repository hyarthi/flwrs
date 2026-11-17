//! The node's state: a SQLite file in its folder (`state.db`), and the
//! **outbox** that makes state changes happen exactly once per input event.
//!
//! The hub delivers an unacknowledged event again (after a crash or a lost
//! connection) with the same `event_id`. Running the script again would apply
//! its `store`s twice. So each input's state changes, its outputs and its
//! event id are committed in one transaction; a redelivered input is answered
//! from the outbox without running the script. State changes are exactly
//! once; outputs at least once (the redelivered outputs are the stored ones).
//!
//! **Space.** The outbox is partitioned into tables of the same file
//! (`outbox_1`, `outbox_2`, …; one file, so a commit stays atomic): old
//! partitions are retired whole (`DROP TABLE`) once they're outside
//! `applied_window`, and also when the file (with its WAL) reaches 90 % of
//! `max_file_bytes`. The file uses incremental auto-vacuum, so the space goes
//! back to the OS. The node's own keys are never dropped: when they alone
//! fill the file, commits that would add data fail.

use flwrs_plugin::schema::payload::{FieldValue, PluginPayload};
use flwrs_types::value::{self, Value};
use prost::Message;
use rusqlite::{Connection, OptionalExtension, params};
use serde::Deserialize;
use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};

/// The longest key.
pub const MAX_KEY_BYTES: usize = 256;
/// How often expired keys are swept (in commits).
const SWEEP_EVERY: u64 = 1_000;
/// Outbox partitions per `applied_window` (each holds a share of it).
const PARTITIONS_PER_WINDOW: u64 = 8;
/// A partition is also closed at this share of `max_file_bytes`, so the size
/// limit always has old partitions to drop.
const PARTITIONS_PER_FILE: u64 = 16;
/// Old outbox partitions start going at this share of `max_file_bytes`.
const RECLAIM_AT_PERCENT: u64 = 90;

/// `state` settings of a node.
#[derive(Deserialize, Debug, Clone, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct StateSettings {
    /// Most keys at once; a `store` past it fails the event.
    pub max_keys: u64,
    /// Largest stored value (encoded).
    pub max_value_bytes: usize,
    /// Largest total of stored values.
    pub max_bytes: u64,
    /// `normal` (fast; a power loss can lose the last commits) or `full`
    /// (every commit on disk before the input is acknowledged).
    pub sync: Sync,
    /// How many recent inputs' outputs are kept for redeliveries.
    pub applied_window: u64,
    /// Largest size on disk of the state file (with its WAL). At 90 % the
    /// oldest outbox records go (the redelivery window shrinks); the node's
    /// keys are never dropped, so when they alone fill it, commits that add
    /// data fail.
    pub max_file_bytes: u64,
}

impl Default for StateSettings {
    fn default() -> Self {
        Self {
            max_keys: 100_000,
            max_value_bytes: 64 * 1024,
            max_bytes: 256 * 1024 * 1024,
            sync: Sync::Normal,
            applied_window: 10_000,
            max_file_bytes: 1024 * 1024 * 1024,
        }
    }
}

#[derive(Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum Sync {
    #[default]
    Normal,
    Full,
}

/// One input's changes, not committed yet. Reads see them.
#[derive(Debug, Default)]
pub struct Writes {
    /// `None` = deleted.
    changes: BTreeMap<String, Option<Stored>>,
}

#[derive(Debug, Clone)]
struct Stored {
    bytes: Vec<u8>,
    /// Epoch ms; `None` = kept until deleted.
    expire_time: Option<i64>,
}

impl Writes {
    pub fn clear(&mut self) {
        self.changes.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }

    /// `Some(None)`: deleted in this input; `None`: not touched.
    fn get(&self, key: &str) -> Option<Option<&Stored>> {
        self.changes.get(key).map(Option::as_ref)
    }
}

#[derive(Debug, Clone, Copy)]
struct Partition {
    number: u64,
    rows: u64,
    /// Bytes of its stored outputs.
    bytes: u64,
}

/// The node's state store.
pub struct Store {
    conn: Connection,
    /// The file (`None` in memory), to measure its size.
    path: Option<PathBuf>,
    settings: StateSettings,
    /// Outbox partitions, oldest first.
    partitions: VecDeque<Partition>,
    /// The file is at its limit and nothing more can be reclaimed.
    full: bool,
    /// Live keys and their bytes (expired ones until swept), kept in step
    /// with the file so the limits don't need a full count per commit.
    keys: u64,
    bytes: u64,
    commits: u64,
}

fn sql(e: rusqlite::Error) -> String {
    format!("state store: {e}")
}

impl Store {
    /// Opens (or creates) the store at `path`.
    pub fn open(path: &Path, settings: StateSettings) -> Result<Self, String> {
        let conn = Connection::open(path).map_err(sql)?;
        Self::with_connection(conn, Some(path.to_path_buf()), settings)
    }

    /// An in-memory store (tests).
    pub fn memory(settings: StateSettings) -> Result<Self, String> {
        Self::with_connection(Connection::open_in_memory().map_err(sql)?, None, settings)
    }

    fn with_connection(
        conn: Connection,
        path: Option<PathBuf>,
        settings: StateSettings,
    ) -> Result<Self, String> {
        let sync = match settings.sync {
            Sync::Normal => "NORMAL",
            Sync::Full => "FULL",
        };
        // auto_vacuum only takes effect before the first table is created.
        conn.execute_batch(&format!(
            "PRAGMA auto_vacuum = INCREMENTAL;
             PRAGMA journal_mode = WAL;
             PRAGMA synchronous = {sync};
             CREATE TABLE IF NOT EXISTS kv (
                 key TEXT PRIMARY KEY,
                 value BLOB NOT NULL,
                 update_time INTEGER NOT NULL,
                 expire_time INTEGER
             );"
        ))
        .map_err(sql)?;
        let mut numbers: Vec<u64> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table' AND name LIKE 'outbox_%'")
            .and_then(|mut stmt| {
                stmt.query_map([], |r| r.get::<_, String>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()
            })
            .map_err(sql)?
            .iter()
            .filter_map(|name| name.strip_prefix("outbox_")?.parse().ok())
            .collect();
        numbers.sort_unstable();
        let mut partitions = VecDeque::new();
        for number in numbers {
            let (rows, bytes): (i64, Option<i64>) = conn
                .query_row(
                    &format!("SELECT COUNT(*), SUM(length(outputs)) FROM outbox_{number}"),
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .map_err(sql)?;
            partitions.push_back(Partition {
                number,
                rows: rows as u64,
                bytes: bytes.unwrap_or(0) as u64,
            });
        }
        if partitions.is_empty() {
            conn.execute_batch(&create_partition(1)).map_err(sql)?;
            partitions.push_back(Partition {
                number: 1,
                rows: 0,
                bytes: 0,
            });
        }
        let mut store = Self {
            conn,
            path,
            settings,
            partitions,
            full: false,
            keys: 0,
            bytes: 0,
            commits: 0,
        };
        store.sweep()?;
        store.reclaim()?;
        Ok(store)
    }

    /// Rows per outbox partition.
    fn partition_rows(&self) -> u64 {
        (self.settings.applied_window / PARTITIONS_PER_WINDOW).max(1)
    }

    /// The file's size on disk, with its WAL.
    pub fn file_bytes(&self) -> u64 {
        match &self.path {
            Some(path) => {
                let size = |p: &Path| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
                let mut wal = path.as_os_str().to_owned();
                wal.push("-wal");
                size(path) + size(Path::new(&wal))
            }
            None => self
                .conn
                .query_row(
                    "SELECT page_count * page_size FROM pragma_page_count(), pragma_page_size()",
                    [],
                    |r| r.get::<_, i64>(0),
                )
                .map(|n| n as u64)
                .unwrap_or(0),
        }
    }

    /// At 90 % of `max_file_bytes`: drops the oldest outbox partitions (all
    /// but the current one) until it's below, and gives the space back.
    /// Marks the store full if that isn't enough.
    fn reclaim(&mut self) -> Result<(), String> {
        let limit = self.settings.max_file_bytes;
        let threshold = limit / 100 * RECLAIM_AT_PERCENT;
        if self.file_bytes() < threshold {
            self.full = false;
            return Ok(());
        }
        while self.partitions.len() > 1 && self.file_bytes() >= threshold {
            let Partition { number, rows, .. } =
                self.partitions.pop_front().expect("more than one");
            self.conn
                .execute_batch(&format!("DROP TABLE outbox_{number}"))
                .map_err(sql)?;
            self.shrink()?;
            log::info!(
                "state file at {} of {limit} bytes: dropped {rows} old outbox records (the redelivery window shrinks)",
                self.file_bytes()
            );
        }
        if self.file_bytes() >= threshold {
            self.shrink()?;
        }
        self.full = self.file_bytes() >= limit;
        if self.full {
            log::warn!(
                "the state file is at its limit ({limit} bytes, state.max_file_bytes) with the node's own keys: commits that add data fail until keys are deleted"
            );
        }
        Ok(())
    }

    /// Gives free pages back to the OS and folds the WAL in.
    fn shrink(&self) -> Result<(), String> {
        self.conn
            .execute_batch("PRAGMA incremental_vacuum; PRAGMA wal_checkpoint(TRUNCATE);")
            .map_err(sql)
    }

    /// Removes expired keys and recounts.
    fn sweep(&mut self) -> Result<(), String> {
        self.conn
            .execute(
                "DELETE FROM kv WHERE expire_time IS NOT NULL AND expire_time <= ?1",
                params![now_ms()],
            )
            .map_err(sql)?;
        let (keys, bytes): (i64, Option<i64>) = self
            .conn
            .query_row("SELECT COUNT(*), SUM(length(value)) FROM kv", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .map_err(sql)?;
        self.keys = keys as u64;
        self.bytes = bytes.unwrap_or(0) as u64;
        Ok(())
    }

    /// A key's value: this input's change if it made one, else the stored one
    /// (unless expired).
    pub fn get(&self, writes: &Writes, key: &str) -> Result<Option<Value>, String> {
        let bytes = match writes.get(key) {
            Some(Some(stored)) => Some(stored.bytes.clone()),
            Some(None) => None,
            None => self
                .conn
                .query_row(
                    "SELECT value FROM kv WHERE key = ?1 AND (expire_time IS NULL OR expire_time > ?2)",
                    params![key, now_ms()],
                    |r| r.get::<_, Vec<u8>>(0),
                )
                .optional()
                .map_err(sql)?,
        };
        bytes.map(|b| decode_value(&b)).transpose()
    }

    /// Committed keys in `[from, to)` (not expired), in key order, at most
    /// `limit`: e.g. timers indexed by a zero-padded time.
    pub fn scan(&self, from: &str, to: &str, limit: usize) -> Result<Vec<(String, Value)>, String> {
        let mut stmt = self
            .conn
            .prepare_cached(
                "SELECT key, value FROM kv WHERE key >= ?1 AND key < ?2
                 AND (expire_time IS NULL OR expire_time > ?3) ORDER BY key LIMIT ?4",
            )
            .map_err(sql)?;
        let rows = stmt
            .query_map(params![from, to, now_ms(), limit as i64], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?))
            })
            .map_err(sql)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(sql)?;
        rows.into_iter()
            .map(|(k, v)| Ok((k, decode_value(&v)?)))
            .collect()
    }

    /// Records a `store` in this input's changes. Checks the key and the
    /// value's size now (so the script's error names the call).
    pub fn put(
        &self,
        writes: &mut Writes,
        key: &str,
        value: Value,
        ttl_ms: Option<i64>,
    ) -> Result<(), String> {
        check_key(key)?;
        let bytes = value::into_field_value(value).encode_to_vec();
        if bytes.len() > self.settings.max_value_bytes {
            return Err(format!(
                "the value for {key:?} is {} bytes; the limit is {} (state.max_value_bytes)",
                bytes.len(),
                self.settings.max_value_bytes
            ));
        }
        let expire_time = ttl_ms.map(|ttl| now_ms() + ttl);
        writes
            .changes
            .insert(key.to_string(), Some(Stored { bytes, expire_time }));
        Ok(())
    }

    /// Records a `delete` in this input's changes.
    pub fn remove(&self, writes: &mut Writes, key: &str) -> Result<(), String> {
        check_key(key)?;
        writes.changes.insert(key.to_string(), None);
        Ok(())
    }

    /// The outputs an earlier delivery of `event_id` produced, if it was
    /// committed.
    pub fn applied(&self, event_id: &str) -> Result<Option<Vec<PluginPayload>>, String> {
        for p in self.partitions.iter().rev() {
            let blob: Option<Vec<u8>> = self
                .conn
                .query_row(
                    &format!(
                        "SELECT outputs FROM outbox_{} WHERE event_id = ?1",
                        p.number
                    ),
                    params![event_id],
                    |r| r.get(0),
                )
                .optional()
                .map_err(sql)?;
            if let Some(blob) = blob {
                return decode_outputs(&blob).map(Some);
            }
        }
        Ok(None)
    }

    /// Commits one input: its changes, and (with an event id) its outputs.
    /// Nothing is written if a limit would be broken.
    pub fn commit(
        &mut self,
        writes: &Writes,
        event_id: Option<&str>,
        outputs: &[PluginPayload],
    ) -> Result<(), String> {
        if writes.is_empty() && event_id.is_none() {
            return Ok(());
        }
        let (mut keys, mut bytes) = (self.keys, self.bytes);
        let now = now_ms();
        let partition_rows = self.partition_rows();
        let partition_bytes = (self.settings.max_file_bytes / PARTITIONS_PER_FILE).max(1);
        let tx = self.conn.transaction().map_err(sql)?;
        for (key, change) in &writes.changes {
            let old: Option<i64> = tx
                .query_row(
                    "SELECT length(value) FROM kv WHERE key = ?1",
                    params![key],
                    |r| r.get(0),
                )
                .optional()
                .map_err(sql)?;
            if let Some(old) = old {
                keys -= 1;
                bytes -= old as u64;
            }
            match change {
                Some(stored) => {
                    tx.execute(
                        "INSERT INTO kv (key, value, update_time, expire_time) VALUES (?1, ?2, ?3, ?4)
                         ON CONFLICT (key) DO UPDATE SET value = excluded.value,
                             update_time = excluded.update_time, expire_time = excluded.expire_time",
                        params![key, stored.bytes, now, stored.expire_time],
                    )
                    .map_err(sql)?;
                    keys += 1;
                    bytes += stored.bytes.len() as u64;
                }
                None => {
                    tx.execute("DELETE FROM kv WHERE key = ?1", params![key])
                        .map_err(sql)?;
                }
            }
        }
        if keys > self.settings.max_keys {
            return Err(format!(
                "the state would hold {keys} keys; the limit is {} (state.max_keys)",
                self.settings.max_keys
            ));
        }
        if bytes > self.settings.max_bytes {
            return Err(format!(
                "the state would hold {bytes} bytes; the limit is {} (state.max_bytes)",
                self.settings.max_bytes
            ));
        }
        if self.full && bytes > self.bytes {
            return Err(format!(
                "the state file is at its limit of {} bytes (state.max_file_bytes); delete keys to make room",
                self.settings.max_file_bytes
            ));
        }
        let mut partitions = self.partitions.clone();
        if let Some(event_id) = event_id {
            let current = *partitions.back().expect("there's always one");
            if current.rows >= partition_rows || current.bytes >= partition_bytes {
                tx.execute_batch(&create_partition(current.number + 1))
                    .map_err(sql)?;
                partitions.push_back(Partition {
                    number: current.number + 1,
                    rows: 0,
                    bytes: 0,
                });
            }
            let current = partitions.back_mut().expect("there's always one");
            let blob = encode_outputs(outputs);
            let inserted = tx
                .execute(
                    &format!(
                        "INSERT OR IGNORE INTO outbox_{} (event_id, outputs, create_time) VALUES (?1, ?2, ?3)",
                        current.number
                    ),
                    params![event_id, blob, now],
                )
                .map_err(sql)?;
            if inserted > 0 {
                current.rows += 1;
                current.bytes += blob.len() as u64;
            }
            // Partitions wholly outside the window go.
            while partitions.len() > 1 {
                let total: u64 = partitions.iter().map(|p| p.rows).sum();
                let oldest = partitions.front().expect("more than one");
                if total - oldest.rows < self.settings.applied_window {
                    break;
                }
                let number = oldest.number;
                partitions.pop_front();
                tx.execute_batch(&format!("DROP TABLE outbox_{number}"))
                    .map_err(sql)?;
            }
        }
        tx.commit().map_err(sql)?;
        (self.keys, self.bytes) = (keys, bytes);
        self.partitions = partitions;
        self.commits += 1;
        if self.commits.is_multiple_of(SWEEP_EVERY) {
            self.sweep()?;
        }
        self.reclaim()?;
        Ok(())
    }
}

fn create_partition(n: u64) -> String {
    format!(
        "CREATE TABLE IF NOT EXISTS outbox_{n} (
             event_id TEXT PRIMARY KEY,
             outputs BLOB NOT NULL,
             create_time INTEGER NOT NULL
         );"
    )
}

fn check_key(key: &str) -> Result<(), String> {
    if key.is_empty() || key.len() > MAX_KEY_BYTES {
        return Err(format!("a key must be 1 to {MAX_KEY_BYTES} bytes"));
    }
    Ok(())
}

fn decode_value(bytes: &[u8]) -> Result<Value, String> {
    let field =
        FieldValue::decode(bytes).map_err(|e| format!("state store: a corrupt value: {e}"))?;
    Ok(value::from_field_value(Some(&field)))
}

/// Outputs as `u32 LE length | PluginPayload` repeated.
fn encode_outputs(outputs: &[PluginPayload]) -> Vec<u8> {
    let mut out = vec![];
    for payload in outputs {
        let bytes = payload.encode_to_vec();
        out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        out.extend_from_slice(&bytes);
    }
    out
}

fn decode_outputs(mut bytes: &[u8]) -> Result<Vec<PluginPayload>, String> {
    let corrupt = || "state store: corrupt outputs in the outbox".to_string();
    let mut outputs = vec![];
    while !bytes.is_empty() {
        let (len, rest) = bytes.split_first_chunk::<4>().ok_or_else(corrupt)?;
        let len = u32::from_le_bytes(*len) as usize;
        if rest.len() < len {
            return Err(corrupt());
        }
        let (payload, rest) = rest.split_at(len);
        outputs.push(PluginPayload::decode(payload).map_err(|_| corrupt())?);
        bytes = rest;
    }
    Ok(outputs)
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// `"10m"`, `"1h30m"`, `"500ms"`: whole numbers with `ms`, `s`, `m`, `h`, `d`.
pub fn parse_duration_ms(text: &str) -> Result<i64, String> {
    let invalid = || format!("{text:?} isn't a duration (e.g. 500ms, 10s, 5m, 1h30m, 1d)");
    let mut rest = text.trim();
    if rest.is_empty() {
        return Err(invalid());
    }
    let mut total: i64 = 0;
    while !rest.is_empty() {
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        let n: i64 = rest[..digits].parse().map_err(|_| invalid())?;
        rest = &rest[digits..];
        let unit = rest.bytes().take_while(u8::is_ascii_alphabetic).count();
        let ms = match &rest[..unit] {
            "ms" => 1,
            "s" => 1_000,
            "m" => 60_000,
            "h" => 3_600_000,
            "d" => 86_400_000,
            _ => return Err(invalid()),
        };
        rest = &rest[unit..];
        total = n
            .checked_mul(ms)
            .and_then(|v| total.checked_add(v))
            .ok_or_else(invalid)?;
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(settings: StateSettings) -> Store {
        Store::memory(settings).unwrap()
    }

    #[test]
    fn reads_see_this_inputs_writes_until_committed() {
        let mut s = store(StateSettings::default());
        let mut w = Writes::default();
        s.put(&mut w, "n", Value::I64(1), None).unwrap();
        assert_eq!(s.get(&w, "n").unwrap(), Some(Value::I64(1)));
        assert_eq!(s.get(&Writes::default(), "n").unwrap(), None);
        s.commit(&w, None, &[]).unwrap();
        assert_eq!(s.get(&Writes::default(), "n").unwrap(), Some(Value::I64(1)));
        let mut w = Writes::default();
        s.remove(&mut w, "n").unwrap();
        assert_eq!(s.get(&w, "n").unwrap(), None);
        s.commit(&w, None, &[]).unwrap();
        assert_eq!(s.get(&Writes::default(), "n").unwrap(), None);
    }

    #[test]
    fn keeps_types_and_expires() {
        let mut s = store(StateSettings::default());
        let mut w = Writes::default();
        // (nested objects come back with sorted keys, like payloads)
        let value = Value::Object(vec![
            ("big".into(), Value::U64(u64::MAX)),
            ("raw".into(), Value::Bytes(vec![0, 255])),
        ]);
        s.put(&mut w, "v", value.clone(), None).unwrap();
        s.put(&mut w, "gone", Value::Bool(true), Some(-1)).unwrap();
        s.commit(&w, None, &[]).unwrap();
        let none = Writes::default();
        assert_eq!(s.get(&none, "v").unwrap(), Some(value));
        assert_eq!(s.get(&none, "gone").unwrap(), None, "expired");
    }

    #[test]
    fn the_outbox_answers_redeliveries() {
        let mut s = store(StateSettings {
            applied_window: 2,
            ..Default::default()
        });
        let out = |n: i64| PluginPayload {
            fields: vec![flwrs_plugin::schema::payload::Field {
                key: "n".into(),
                value: Some(value::into_field_value(Value::I64(n))),
            }],
        };
        for (id, n) in [("e1", 1), ("e2", 2), ("e3", 3)] {
            s.commit(&Writes::default(), Some(id), &[out(n), out(n + 10)])
                .unwrap();
        }
        assert_eq!(s.applied("e3").unwrap(), Some(vec![out(3), out(13)]));
        assert_eq!(s.applied("e2").unwrap(), Some(vec![out(2), out(12)]));
        assert_eq!(s.applied("e1").unwrap(), None, "outside the window");
        assert_eq!(s.applied("e9").unwrap(), None);
    }

    #[test]
    fn limits_refuse_the_whole_input() {
        let mut s = store(StateSettings {
            max_keys: 2,
            max_value_bytes: 16,
            max_bytes: 1_000,
            ..Default::default()
        });
        let mut w = Writes::default();
        assert!(
            s.put(&mut w, "big", Value::String("x".repeat(64)), None)
                .unwrap_err()
                .contains("max_value_bytes")
        );
        assert!(s.put(&mut w, "", Value::Null, None).is_err());
        for k in ["a", "b", "c"] {
            s.put(&mut w, k, Value::I64(1), None).unwrap();
        }
        let e = s.commit(&w, Some("e1"), &[]).unwrap_err();
        assert!(e.contains("max_keys"), "{e}");
        // nothing of it was written
        assert_eq!(s.get(&Writes::default(), "a").unwrap(), None);
        assert_eq!(s.applied("e1").unwrap(), None);
        // replacing a key doesn't count twice
        let mut w = Writes::default();
        s.put(&mut w, "a", Value::I64(1), None).unwrap();
        s.commit(&w, None, &[]).unwrap();
        s.put(&mut w, "a", Value::I64(2), None).unwrap();
        s.put(&mut w, "b", Value::I64(2), None).unwrap();
        s.commit(&w, None, &[]).unwrap();
    }

    #[test]
    fn reopens_with_its_counts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let mut s = Store::open(&path, StateSettings::default()).unwrap();
        let mut w = Writes::default();
        s.put(&mut w, "a", Value::I64(1), None).unwrap();
        s.commit(&w, Some("e1"), &[]).unwrap();
        drop(s);
        let s = Store::open(&path, StateSettings::default()).unwrap();
        assert_eq!(s.keys, 1);
        assert_eq!(s.get(&Writes::default(), "a").unwrap(), Some(Value::I64(1)));
        assert_eq!(s.applied("e1").unwrap(), Some(vec![]));
    }

    #[test]
    fn survives_a_process_that_never_closed_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let mut s = Store::open(&path, StateSettings::default()).unwrap();
        let mut w = Writes::default();
        s.put(&mut w, "a", Value::I64(7), None).unwrap();
        s.commit(&w, Some("e1"), &[]).unwrap();
        std::mem::forget(s); // like a kill: no close, no checkpoint
        let s = Store::open(&path, StateSettings::default()).unwrap();
        assert_eq!(s.get(&Writes::default(), "a").unwrap(), Some(Value::I64(7)));
    }

    fn big_output(n: u64) -> PluginPayload {
        PluginPayload {
            fields: vec![flwrs_plugin::schema::payload::Field {
                key: "blob".into(),
                value: Some(value::into_field_value(Value::Bytes(vec![n as u8; 2048]))),
            }],
        }
    }

    #[test]
    fn old_outbox_records_go_when_the_file_fills() {
        let dir = tempfile::tempdir().unwrap();
        let limit = 512 * 1024;
        let mut s = Store::open(
            &dir.path().join("state.db"),
            StateSettings {
                applied_window: 100_000, // only the size limit retires records
                max_file_bytes: limit,
                ..Default::default()
            },
        )
        .unwrap();
        for i in 0..2_000u64 {
            let mut w = Writes::default();
            s.put(&mut w, "count", Value::U64(i), None).unwrap();
            s.commit(&w, Some(&format!("e{i}")), &[big_output(i)])
                .unwrap();
            assert!(
                s.file_bytes() <= limit,
                "{} > {limit} after {i}",
                s.file_bytes()
            );
        }
        // the newest records are there, the oldest went; the keys stayed
        assert!(s.applied("e1999").unwrap().is_some());
        assert!(s.applied("e0").unwrap().is_none());
        assert_eq!(
            s.get(&Writes::default(), "count").unwrap(),
            Some(Value::U64(1999))
        );
        assert!(!s.full);
    }

    #[test]
    fn the_nodes_own_keys_are_never_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Store::open(
            &dir.path().join("state.db"),
            StateSettings {
                max_file_bytes: 256 * 1024,
                ..Default::default()
            },
        )
        .unwrap();
        let mut refused = None;
        for i in 0..200 {
            let mut w = Writes::default();
            s.put(
                &mut w,
                &format!("k{i}"),
                Value::Bytes(vec![1; 8 * 1024]),
                None,
            )
            .unwrap();
            if let Err(e) = s.commit(&w, None, &[]) {
                refused = Some((i, e));
                break;
            }
        }
        let (i, e) = refused.expect("the limit is reached");
        assert!(e.contains("state.max_file_bytes"), "{e}");
        assert!(
            s.get(&Writes::default(), "k0").unwrap().is_some(),
            "keys are kept"
        );
        // deleting keys still works, and makes room again
        for k in 0..i {
            let mut w = Writes::default();
            s.remove(&mut w, &format!("k{k}")).unwrap();
            s.commit(&w, None, &[]).unwrap();
        }
        let mut w = Writes::default();
        s.put(&mut w, "again", Value::Bytes(vec![1; 8 * 1024]), None)
            .unwrap();
        s.commit(&w, None, &[]).unwrap();
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration_ms("10m").unwrap(), 600_000);
        assert_eq!(parse_duration_ms("1h30m").unwrap(), 5_400_000);
        assert!(parse_duration_ms("soon").is_err());
        assert!(parse_duration_ms("").is_err());
    }
}
