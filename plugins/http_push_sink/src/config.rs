//! The command line (see `schema/launch_args.json`): the bootstrap
//! arguments, the server's and the broadcast core's (from the kit), and the
//! push sink's own; their checks report every problem.

use clap::{Parser, ValueEnum};
use flwrs_plugin::cli::{Bootstrap, read_json};
use flwrs_plugin_kit::broadcast::{BroadcastArgs, BroadcastSettings};
use flwrs_plugin_kit::server::{ServerArgs, ServerSettings};
use std::path::PathBuf;
use std::time::Duration;

/// flwrs sink plugin: an HTTP server broadcasting events over SSE and long polling
#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
pub struct Args {
    #[command(flatten)]
    pub bootstrap: Bootstrap,

    #[command(flatten)]
    pub server: ServerArgs,

    #[command(flatten)]
    pub broadcast: BroadcastArgs,

    /// A prefix for the routes (/subscribe/{channel}, /poll/{channel})
    #[arg(long, default_value = "")]
    pub base_path: String,

    /// SSE data: raw (the bytes as text lines; non-UTF-8 as base64) or json (an envelope)
    #[arg(long, value_enum, default_value = "raw")]
    pub sse_format: SseFormat,

    /// Seconds between SSE keep-alive comments
    #[arg(long, default_value_t = 15)]
    pub heartbeat_s: u64,

    /// The SSE retry hint for reconnecting clients, in ms
    #[arg(long, default_value_t = 3000)]
    pub retry_ms: u64,

    /// Most events in one long-poll answer
    #[arg(long, default_value_t = 100)]
    pub poll_max_events: usize,

    /// Longest a long poll waits for an event, in seconds
    #[arg(long, default_value_t = 25)]
    pub poll_max_wait_s: u64,

    /// JSON file: the Origins browsers may subscribe from (CORS); ["*"]: any
    #[arg(long)]
    pub allowed_origins: Option<PathBuf>,

    /// Events handled at once (delivered: waiting for subscribers at once)
    #[arg(long, default_value_t = 64)]
    pub max_in_flight: usize,
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum SseFormat {
    #[value(name = "raw")]
    Raw,
    #[value(name = "json")]
    Json,
}

#[derive(Debug, Clone)]
pub struct Settings {
    pub server: ServerSettings,
    pub broadcast: BroadcastSettings,
    pub base_path: String,
    pub sse_format: SseFormat,
    pub heartbeat: Duration,
    pub retry_ms: u64,
    pub poll_max_events: usize,
    pub poll_max_wait: Duration,
    pub allowed_origins: Vec<String>,
    pub max_in_flight: usize,
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
        let allowed_origins: Vec<String> = self
            .allowed_origins
            .as_deref()
            .and_then(|p| {
                read_json(p)
                    .map_err(|e| problems.push(format!("--allowed-origins {e}")))
                    .ok()
            })
            .unwrap_or_default();
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
            ("heartbeat_s", self.heartbeat_s),
            ("poll_max_events", self.poll_max_events as u64),
            ("poll_max_wait_s", self.poll_max_wait_s),
            ("max_in_flight", self.max_in_flight as u64),
        ] {
            if v == 0 {
                problems.push(format!("{name} must be at least 1"));
            }
        }
        if let Some(server) = &server
            && Duration::from_secs(self.poll_max_wait_s) >= server.request_timeout()
        {
            problems.push("poll_max_wait_s must be shorter than limits.request_timeout_ms".into());
        }
        match (server, broadcast) {
            (Some(server), Some(broadcast)) if problems.is_empty() => Ok(Settings {
                server,
                broadcast,
                base_path,
                sse_format: self.sse_format,
                heartbeat: Duration::from_secs(self.heartbeat_s),
                retry_ms: self.retry_ms,
                poll_max_events: self.poll_max_events,
                poll_max_wait: Duration::from_secs(self.poll_max_wait_s),
                allowed_origins,
                max_in_flight: self.max_in_flight,
            }),
            _ => Err(problems),
        }
    }
}
