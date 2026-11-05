use flwrs_core::config::{self, main_config};
use serde::Deserialize;
use std::path::PathBuf;

/// `[plugins]` config section. Every field is optional.
#[derive(Deserialize, Clone, Debug)]
#[serde(default)]
pub(crate) struct PluginsConfig {
    /// Where plugin packages are installed (`<dir>/<plugin-id>/`).
    pub dir: PathBuf,
    /// Whether the mutating plugin endpoints (upload, upgrade, delete) are enabled.
    /// Uploading a plugin uploads code the server will run, so this is off by default.
    pub allow_upload: bool,
    /// If set, mutating endpoints require `Authorization: Bearer <admin_token>`.
    /// If not set, they only accept clients connecting from a loopback address.
    pub admin_token: Option<String>,
    /// Maximum size of an uploaded ZIP.
    pub max_upload_bytes: u64,
    /// Maximum total size of a package once unpacked.
    pub max_unpacked_bytes: u64,
    /// Maximum number of entries in a package.
    pub max_entries: usize,
    /// Maximum compression ratio of a single large entry (zip-bomb guard).
    pub max_compression_ratio: u64,
}

impl Default for PluginsConfig {
    fn default() -> Self {
        Self {
            dir: PathBuf::from("./plugins"),
            allow_upload: false,
            admin_token: None,
            max_upload_bytes: 256 * 1024 * 1024,
            max_unpacked_bytes: 1024 * 1024 * 1024,
            max_entries: 10_000,
            max_compression_ratio: 200,
        }
    }
}

impl PluginsConfig {
    pub(crate) fn read() -> Self {
        config::read_struct(main_config(), &["plugins".to_string()]).unwrap_or_default()
    }
}
