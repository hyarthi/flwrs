//! The command line (see `schema/launch_args.json`): the bootstrap
//! arguments, the server's (`flwrs_plugin_kit::server::ServerArgs`), and the
//! WebSocket source's own; their checks report every problem.

use clap::{Parser, ValueEnum};
use flwrs_plugin::cli::{Bootstrap, read_json, read_text};
use flwrs_plugin_kit::server::{ServerArgs, ServerSettings};
use std::path::PathBuf;
use std::time::Duration;

/// flwrs source plugin: a WebSocket server that turns messages into events
#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
pub struct Args {
    #[command(flatten)]
    pub bootstrap: Bootstrap,

    #[command(flatten)]
    pub server: ServerArgs,

    /// File: the AsyncAPI 3.x document (JSON or YAML); its receive channels are the WebSocket paths
    #[arg(long)]
    pub asyncapi: Option<PathBuf>,

    /// JSON file: the node's out schema (set by the configurator from the document)
    #[arg(long)]
    pub out_schema: Option<PathBuf>,

    /// JSON file: the WebSocket paths without a document (default ["/ws"])
    #[arg(long)]
    pub paths: Option<PathBuf>,

    /// JSON file: the subprotocols the server speaks, in order of preference
    #[arg(long)]
    pub subprotocols: Option<PathBuf>,

    /// JSON file: the Origin headers browsers may connect from (empty: any)
    #[arg(long)]
    pub allowed_origins: Option<PathBuf>,

    /// Largest message (and frame), in bytes
    #[arg(long, default_value_t = 1024 * 1024)]
    pub max_message_bytes: u32,

    /// Seconds between pings; a ping unanswered by the next one closes the connection
    #[arg(long, default_value_t = 30)]
    pub ping_interval_s: u32,

    /// How long a message waits for the workflow to take it; then the connection closes (1013)
    #[arg(long, default_value_t = 5000)]
    pub enqueue_timeout_ms: u64,

    /// A message the document doesn't describe: dropped, or the connection closed (1008)
    #[arg(long, value_enum, default_value = "drop")]
    pub invalid_messages: InvalidMessages,
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvalidMessages {
    #[value(name = "drop")]
    Drop,
    #[value(name = "close")]
    Close,
}

#[derive(Debug, Clone)]
pub struct Settings {
    pub server: ServerSettings,
    pub asyncapi: Option<String>,
    pub out_schema: Option<serde_json::Value>,
    pub paths: Vec<String>,
    pub subprotocols: Vec<String>,
    pub allowed_origins: Vec<String>,
    pub max_message_bytes: usize,
    pub ping_interval: Duration,
    pub enqueue_timeout: Duration,
    pub invalid_messages: InvalidMessages,
}

impl Args {
    pub fn settings(&self) -> Result<Settings, Vec<String>> {
        let mut problems = vec![];
        let server = self.server.settings().map_err(|e| problems.extend(e)).ok();
        let asyncapi = self.asyncapi.as_deref().and_then(|p| {
            read_text(p)
                .map_err(|e| problems.push(format!("--asyncapi {e}")))
                .ok()
        });
        let out_schema = self.out_schema.as_deref().and_then(|p| {
            read_json::<serde_json::Value>(p)
                .map_err(|e| problems.push(format!("--out-schema {e}")))
                .ok()
        });
        let mut list = |flag: &str, p: &Option<PathBuf>| -> Option<Vec<String>> {
            p.as_deref().and_then(|p| {
                read_json(p)
                    .map_err(|e| problems.push(format!("--{flag} {e}")))
                    .ok()
            })
        };
        let paths = list("paths", &self.paths);
        let subprotocols = list("subprotocols", &self.subprotocols).unwrap_or_default();
        let allowed_origins = list("allowed-origins", &self.allowed_origins).unwrap_or_default();
        if paths.is_some() && asyncapi.is_some() {
            problems.push(
                "paths are for nodes without an asyncapi document (its channels are the paths)"
                    .into(),
            );
        }
        let paths = paths.unwrap_or_else(|| vec!["/ws".into()]);
        for p in &paths {
            if !p.starts_with('/') || p.contains(['{', '}', '?', '#', '*']) {
                problems.push(format!("paths: {p:?} must be a plain path starting with /"));
            }
        }
        if self.max_message_bytes < 16 {
            problems.push("max_message_bytes must be at least 16".into());
        }
        if self.ping_interval_s == 0 || self.enqueue_timeout_ms == 0 {
            problems.push("ping_interval_s and enqueue_timeout_ms must be at least 1".into());
        }
        match server {
            Some(server) if problems.is_empty() => Ok(Settings {
                server,
                asyncapi,
                out_schema,
                paths,
                subprotocols,
                allowed_origins,
                max_message_bytes: self.max_message_bytes as usize,
                ping_interval: Duration::from_secs(self.ping_interval_s.into()),
                enqueue_timeout: Duration::from_millis(self.enqueue_timeout_ms),
                invalid_messages: self.invalid_messages,
            }),
            _ => Err(problems),
        }
    }
}
