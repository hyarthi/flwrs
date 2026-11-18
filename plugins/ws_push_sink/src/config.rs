//! The command line (see `schema/launch_args.json`): the bootstrap
//! arguments, the server's and the broadcast core's (from the kit), and the
//! WebSocket push sink's own; their checks report every problem.

use clap::{Parser, ValueEnum};
use flwrs_plugin::cli::{Bootstrap, read_json};
use flwrs_plugin_kit::broadcast::{BroadcastArgs, BroadcastSettings};
use flwrs_plugin_kit::server::{ServerArgs, ServerSettings};
use std::path::PathBuf;
use std::time::Duration;

/// flwrs sink plugin: a WebSocket server broadcasting events to connected clients
#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
pub struct Args {
    #[command(flatten)]
    pub bootstrap: Bootstrap,

    #[command(flatten)]
    pub server: ServerArgs,

    #[command(flatten)]
    pub broadcast: BroadcastArgs,

    /// A prefix for the route (/ws/{channel})
    #[arg(long, default_value = "")]
    pub base_path: String,

    /// How an event becomes a message: auto (text if UTF-8, else binary), text, binary, or envelope (JSON text)
    #[arg(long, value_enum, default_value = "auto")]
    pub frame: Frame,

    /// JSON file: the subprotocols the server speaks, in order of preference
    #[arg(long)]
    pub subprotocols: Option<PathBuf>,

    /// JSON file: the Origin headers browsers may connect from (empty: any)
    #[arg(long)]
    pub allowed_origins: Option<PathBuf>,

    /// Seconds between pings; a ping unanswered by the next one closes the connection
    #[arg(long, default_value_t = 30)]
    pub ping_interval_s: u64,

    /// A message not written within this long closes the connection (a stuck client)
    #[arg(long, default_value_t = 10_000)]
    pub write_timeout_ms: u64,

    /// Largest message a client may send (they're ignored; pings and closes need room)
    #[arg(long, default_value_t = 4096)]
    pub max_client_message_bytes: usize,

    /// Events handled at once (delivered: waiting for subscribers at once)
    #[arg(long, default_value_t = 64)]
    pub max_in_flight: usize,

    /// Per connection: the read buffer, allocated up front (clients only send pings and closes)
    #[arg(long, default_value_t = 4096)]
    pub read_buffer_bytes: usize,

    /// Per connection: how much is gathered before writing (it grows to fit a larger message)
    #[arg(long, default_value_t = 16 * 1024)]
    pub write_buffer_bytes: usize,
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum Frame {
    #[value(name = "auto")]
    Auto,
    #[value(name = "text")]
    Text,
    #[value(name = "binary")]
    Binary,
    #[value(name = "envelope")]
    Envelope,
}

#[derive(Debug, Clone)]
pub struct Settings {
    pub server: ServerSettings,
    pub broadcast: BroadcastSettings,
    pub base_path: String,
    pub frame: Frame,
    pub subprotocols: Vec<String>,
    pub allowed_origins: Vec<String>,
    pub ping_interval: Duration,
    pub write_timeout: Duration,
    pub max_client_message_bytes: usize,
    pub max_in_flight: usize,
    pub read_buffer_bytes: usize,
    pub write_buffer_bytes: usize,
}

impl Args {
    pub fn settings(&self) -> Result<Settings, Vec<String>> {
        let mut problems = vec![];
        let server = self.server.settings().map_err(|e| problems.extend(e)).ok();
        let broadcast = self
            .broadcast
            .settings()
            .map_err(|e| problems.extend(e))
            .ok();
        let mut list = |flag: &str, p: &Option<PathBuf>| -> Vec<String> {
            p.as_deref()
                .and_then(|p| {
                    read_json(p)
                        .map_err(|e| problems.push(format!("--{flag} {e}")))
                        .ok()
                })
                .unwrap_or_default()
        };
        let subprotocols = list("subprotocols", &self.subprotocols);
        let allowed_origins = list("allowed-origins", &self.allowed_origins);
        let base_path = self.base_path.trim_end_matches('/').to_string();
        if !base_path.is_empty()
            && (!base_path.starts_with('/') || base_path.contains(['{', '}', '?', '#', '*']))
        {
            problems.push(format!(
                "base_path {:?} must be a plain path starting with /",
                self.base_path
            ));
        }
        for (name, v) in [
            ("ping_interval_s", self.ping_interval_s),
            ("write_timeout_ms", self.write_timeout_ms),
            ("max_in_flight", self.max_in_flight as u64),
        ] {
            if v == 0 {
                problems.push(format!("{name} must be at least 1"));
            }
        }
        if self.read_buffer_bytes < 256 {
            problems.push("read_buffer_bytes must be at least 256".into());
        }
        if self.max_client_message_bytes < 128 {
            problems.push("max_client_message_bytes must be at least 128".into());
        }
        match (server, broadcast) {
            (Some(server), Some(broadcast)) if problems.is_empty() => Ok(Settings {
                server,
                broadcast,
                base_path,
                frame: self.frame,
                subprotocols,
                allowed_origins,
                ping_interval: Duration::from_secs(self.ping_interval_s),
                write_timeout: Duration::from_millis(self.write_timeout_ms),
                max_client_message_bytes: self.max_client_message_bytes,
                max_in_flight: self.max_in_flight,
                read_buffer_bytes: self.read_buffer_bytes,
                write_buffer_bytes: self.write_buffer_bytes,
            }),
            _ => Err(problems),
        }
    }
}
