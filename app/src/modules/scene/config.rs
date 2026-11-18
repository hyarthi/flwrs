//! `[scenes]` config section: limits of the scene API.

use flwrs_core::config::{self, main_config};
use serde::Deserialize;

/// `[scenes]` config section. Every field is optional.
#[derive(Deserialize, Clone, Debug)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct ScenesConfig {
    /// Largest file uploaded to a node (`PUT …/nodes/by-id/{id}/uploads/{name}`).
    pub max_upload_bytes: u64,
    /// Largest scene import (`POST /api/scenes/import`): the archive, and
    /// separately what it unpacks to.
    pub max_import_bytes: u64,
    /// Most entries in an imported archive.
    pub max_import_entries: usize,
    /// How often the live list (`GET /api/scenes/stream`) sends the scenes'
    /// status. At least 100.
    pub stream_interval_ms: u64,
}

impl Default for ScenesConfig {
    fn default() -> Self {
        Self {
            max_upload_bytes: 1024 * 1024 * 1024,
            max_import_bytes: 1024 * 1024 * 1024,
            max_import_entries: 10_000,
            stream_interval_ms: 1_000,
        }
    }
}

impl ScenesConfig {
    pub(crate) fn read() -> Self {
        // A missing or invalid section falls back to the defaults (errors are
        // printed by the config reader).
        config::read_struct(main_config(), &["scenes".to_string()]).unwrap_or_default()
    }

    /// `stream_interval_ms`, at least 100 ms.
    pub(crate) fn stream_interval(&self) -> std::time::Duration {
        std::time::Duration::from_millis(self.stream_interval_ms.max(100))
    }
}
