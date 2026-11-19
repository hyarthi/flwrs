//! What each node's plugin said lately, for the UI's node logs: the last
//! [`KEEP`] records per node (the plugin's `LogEvent`s and `ErrorEvent`s,
//! its stdout / stderr lines, and the supervisor's notes about it: started,
//! exited, relaunching, failed), in memory, beside the server log (which
//! keeps getting everything). Readers take a snapshot and then follow new
//! records ([`subscribe`]).
//!
//! Process-wide and keyed by node id (ids are unique across scenes). Bounded:
//! [`KEEP`] records per node, [`MAX_NODES`] nodes (the longest quiet is
//! forgotten first), messages cut at [`MAX_MESSAGE`] bytes.

use serde::Serialize;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::{LazyLock, Mutex};
use tokio::sync::broadcast;
use utoipa::ToSchema;

/// Records kept per node.
pub(crate) const KEEP: usize = 500;
/// Nodes with records kept.
const MAX_NODES: usize = 4096;
/// Longest message kept (bytes; cut at a character boundary).
const MAX_MESSAGE: usize = 8 * 1024;

/// Where a record came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Source {
    /// A `LogEvent` from the plugin.
    Plugin,
    /// An `ErrorEvent` from the plugin.
    Error,
    /// A line on the plugin's stdout (local plugins).
    Stdout,
    /// A line on its stderr.
    Stderr,
    /// The server about the plugin: started, exited, relaunching, failed.
    Supervisor,
}

#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub(crate) struct Record {
    /// Counts this node's records since the server started (gaps = records
    /// a slow reader missed).
    pub seq: u64,
    /// Unix milliseconds.
    pub time: i64,
    /// `trace`, `debug`, `info`, `warn` or `error`.
    pub level: &'static str,
    pub source: Source,
    pub message: String,
    /// Structured key-values the plugin sent.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub details: BTreeMap<String, String>,
}

struct NodeLog {
    records: VecDeque<Record>,
    next_seq: u64,
    last_push: std::time::Instant,
    tx: broadcast::Sender<Record>,
}

static BOOK: LazyLock<Mutex<HashMap<String, NodeLog>>> = LazyLock::new(Default::default);

fn level_name(level: log::Level) -> &'static str {
    match level {
        log::Level::Trace => "trace",
        log::Level::Debug => "debug",
        log::Level::Info => "info",
        log::Level::Warn => "warn",
        log::Level::Error => "error",
    }
}

fn cut(mut text: String) -> String {
    if text.len() > MAX_MESSAGE {
        let mut at = MAX_MESSAGE;
        while !text.is_char_boundary(at) {
            at -= 1;
        }
        text.truncate(at);
        text.push('…');
    }
    text
}

/// Notes a record for `node_id`.
pub(crate) fn push(
    node_id: &str,
    level: log::Level,
    source: Source,
    message: impl Into<String>,
    details: BTreeMap<String, String>,
) {
    let mut book = BOOK.lock().unwrap_or_else(|e| e.into_inner());
    if !book.contains_key(node_id) && book.len() >= MAX_NODES {
        // Forget the node that's been quiet longest.
        if let Some(oldest) = book
            .iter()
            .min_by_key(|(_, n)| n.last_push)
            .map(|(id, _)| id.clone())
        {
            book.remove(&oldest);
        }
    }
    let node = book.entry(node_id.to_string()).or_insert_with(|| NodeLog {
        records: VecDeque::with_capacity(16),
        next_seq: 1,
        last_push: std::time::Instant::now(),
        tx: broadcast::channel(256).0,
    });
    let record = Record {
        seq: node.next_seq,
        time: chrono::Utc::now().timestamp_millis(),
        level: level_name(level),
        source,
        message: cut(message.into()),
        details,
    };
    node.next_seq += 1;
    node.last_push = std::time::Instant::now();
    if node.records.len() == KEEP {
        node.records.pop_front();
    }
    node.records.push_back(record.clone());
    // No readers is fine.
    let _ = node.tx.send(record);
}

/// The records kept for `node_id`, oldest first, and a receiver for the
/// ones that follow (taken together, so none falls between them).
pub(crate) fn subscribe(node_id: &str) -> (Vec<Record>, broadcast::Receiver<Record>) {
    let mut book = BOOK.lock().unwrap_or_else(|e| e.into_inner());
    let node = book.entry(node_id.to_string()).or_insert_with(|| NodeLog {
        records: VecDeque::new(),
        next_seq: 1,
        last_push: std::time::Instant::now(),
        tx: broadcast::channel(256).0,
    });
    (node.records.iter().cloned().collect(), node.tx.subscribe())
}

/// The records kept for `node_id`, oldest first.
pub(crate) fn snapshot(node_id: &str) -> Vec<Record> {
    let book = BOOK.lock().unwrap_or_else(|e| e.into_inner());
    book.get(node_id)
        .map(|n| n.records.iter().cloned().collect())
        .unwrap_or_default()
}

/// Forgets a node's records (it was deleted).
pub(crate) fn forget(node_id: &str) {
    BOOK.lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(node_id);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_the_last_records_and_hands_new_ones_to_readers() {
        let node = format!("logs-test-{}", ulid::Ulid::generate());
        for i in 0..KEEP + 5 {
            push(
                &node,
                log::Level::Info,
                Source::Stdout,
                format!("line {i}"),
                BTreeMap::new(),
            );
        }
        let kept = snapshot(&node);
        assert_eq!(kept.len(), KEEP);
        assert_eq!(kept[0].message, "line 5");
        assert_eq!(kept.last().unwrap().seq, (KEEP + 5) as u64);

        let (before, mut rx) = subscribe(&node);
        assert_eq!(before.len(), KEEP);
        push(
            &node,
            log::Level::Warn,
            Source::Plugin,
            "x".repeat(MAX_MESSAGE + 10),
            BTreeMap::from([("k".into(), "v".into())]),
        );
        let next = rx.try_recv().unwrap();
        assert_eq!(next.level, "warn");
        assert!(next.message.len() <= MAX_MESSAGE + '…'.len_utf8());
        assert_eq!(next.details["k"], "v");

        forget(&node);
        assert!(snapshot(&node).is_empty());
    }
}
