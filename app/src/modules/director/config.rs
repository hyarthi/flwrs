use flwrs_core::config::{self, main_config};
use serde::Deserialize;
use std::path::PathBuf;

/// `[director]` config section. Every field is optional.
#[derive(Deserialize, Clone, Debug)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct DirectorConfig {
    /// An event is dropped once it has been handed over this many times
    /// (stops runaway loops in cyclic scenes).
    pub max_hops: u32,
    /// Where spill-to-disk queue files go: `<spool_dir>/<scene-id>/<node-id>.db`.
    /// Wiped when the director starts (leftovers of an earlier run are
    /// dropped: delivery is at-most-once).
    pub spool_dir: PathBuf,
    /// Spill to disk for nodes that don't say (`runtime.queue.spill.enabled`).
    pub spill_default: bool,
    /// Memory soft limit across all spill-enabled queues: once reached, their
    /// new events go to disk.
    pub memory_soft_inflight_bytes: u64,
    /// Disk limit across all spool files.
    pub spill_max_bytes: u64,
    /// Never spill if the spool filesystem would have less free space than this.
    pub spill_min_free_bytes: u64,
}

impl Default for DirectorConfig {
    fn default() -> Self {
        Self {
            max_hops: 32,
            spool_dir: PathBuf::from("./scenes"),
            spill_default: false,
            memory_soft_inflight_bytes: 128 * 1024 * 1024,
            spill_max_bytes: 10 * 1024 * 1024 * 1024,
            spill_min_free_bytes: 1024 * 1024 * 1024,
        }
    }
}

impl DirectorConfig {
    pub(crate) fn read() -> Self {
        // A missing or invalid section falls back to the defaults (errors are
        // printed by the config reader).
        config::read_struct(main_config(), &["director".to_string()]).unwrap_or_default()
    }
}
