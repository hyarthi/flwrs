use flwrs_core::config::{self, main_config};
use serde::Deserialize;

/// `[hub]` config section: where plugins connect. Every field is optional.
#[derive(Deserialize, Clone, Debug)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct HubConfig {
    /// Address the hub listens on. Loopback by default; set it to a reachable
    /// address (or `0.0.0.0`) for remote plugins.
    pub host: String,
    /// `0` picks a free port. Local plugins are told the actual port; remote
    /// plugins need a fixed one.
    pub port: u16,
    /// Largest data event accepted from a plugin (encoded payload). Nodes can
    /// lower it with `runtime.queue.max_event_bytes`.
    pub max_event_bytes: usize,
    /// Hub-wide budget for event bytes waiting in node queues, across all
    /// scenes. When it's used up, `block` producers wait and `drop_*` queues drop.
    pub max_inflight_bytes: usize,
    /// How long a new connection may take to send `Initialize`.
    pub handshake_timeout_ms: u64,
    /// Largest `Initialize` frame accepted from a connection that hasn't been
    /// authenticated yet.
    pub max_handshake_bytes: usize,
}

impl Default for HubConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".into(),
            port: 0,
            max_event_bytes: 16 * 1024 * 1024,
            max_inflight_bytes: 512 * 1024 * 1024,
            handshake_timeout_ms: 10_000,
            max_handshake_bytes: 1024 * 1024,
        }
    }
}

impl HubConfig {
    pub(crate) fn read() -> Self {
        // A missing or invalid section falls back to the defaults (errors are
        // printed by the config reader).
        config::read_struct(main_config(), &["hub".to_string()]).unwrap_or_default()
    }
}
