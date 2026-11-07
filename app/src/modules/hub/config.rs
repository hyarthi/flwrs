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
    /// The hub pings every connection this often (announced in
    /// `InitializeResponse`); a connection silent for three intervals while
    /// the hub is reading is closed. `0` = no heartbeats.
    pub heartbeat_interval_ms: u32,
    /// Credit window for producers with `CREDITS`: events they may have sent
    /// that the hub hasn't finished routing.
    pub credit_window: u32,
    /// Upper bound for a consumer's `max_unacked`.
    pub max_unacked: u32,
    /// The window for a consumer that sends `max_unacked = 0`.
    pub default_max_unacked: u32,
    /// `[hub.tls]`: a second listener, with TLS, for remote plugins (feature
    /// `remote`). Local plugins keep using the plain one.
    pub tls: Option<HubTlsConfig>,
}

/// `[hub.tls]`: the TLS listener for remote plugins.
#[derive(Deserialize, Clone, Debug)]
#[cfg_attr(
    not(feature = "remote"),
    allow(dead_code, reason = "read by the TLS listener (feature remote)")
)]
#[serde(deny_unknown_fields)]
pub(crate) struct HubTlsConfig {
    /// Address to listen on (default `0.0.0.0`: remote plugins need to reach it).
    #[serde(default = "any_host")]
    pub host: String,
    /// `0` picks a free port (tests); remote plugins need a fixed one.
    pub port: u16,
    /// The hub's certificate chain and private key (PEM files).
    pub cert: std::path::PathBuf,
    pub key: std::path::PathBuf,
    /// CA certificate(s) (PEM) that client certificates of `mtls` nodes
    /// chain to. Without it, `mtls` nodes need a pinned fingerprint.
    #[serde(default)]
    pub client_ca: Option<std::path::PathBuf>,
}

fn any_host() -> String {
    "0.0.0.0".into()
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
            heartbeat_interval_ms: 10_000,
            credit_window: 64,
            max_unacked: 1024,
            default_max_unacked: 64,
            tls: None,
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
