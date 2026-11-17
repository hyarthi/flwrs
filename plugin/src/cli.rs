//! (feature `cli`) The bootstrap arguments the server passes to every local
//! plugin, as a [`clap`] struct to flatten into the plugin's own arguments:
//!
//! ```rust,ignore
//! #[derive(clap::Parser)]
//! struct Args {
//!     #[command(flatten)]
//!     bootstrap: flwrs_plugin::cli::Bootstrap,
//!     /// The plugin's own flags (its launch args) follow.
//!     #[arg(long, default_value = "1s")]
//!     interval: String,
//! }
//!
//! let args = Args::parse();
//! let config = SourceRunnerConfig {
//!     plugin_id: args.bootstrap.id.clone(),
//!     log_level: args.bootstrap.log_level(),
//!     hub_connection: args.bootstrap.connection(),
//! };
//! ```
//!
//! The arguments are specified in `docs/plugin-spec.md` (*Launching*); they
//! have no short forms, so they don't take letters a plugin may want for its
//! own flags.
//!
//! A node's own settings arrive as the plugin's flags, environment variables
//! or files in the node's `config/` folder, as its manifest declares. A
//! structured (`"via": "file"`) setting is a path flag; [`read_json`] and
//! [`read_text`] read it:
//!
//! ```rust,ignore
//! #[arg(long)]
//! schemas: Option<std::path::PathBuf>,
//! // …
//! let schemas: Schemas = flwrs_plugin::cli::read_json(&path)?;
//! ```

use crate::plugin::core::{BOOTSTRAP_ARGS, ConnectionConfig};
use crate::schema::protocol::log_level::Enum as LogLevel;
use serde::de::DeserializeOwned;
use std::path::Path;

/// Reads a JSON file the server delivered (a `"via": "file"` setting), with
/// the path in the error.
pub fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T, String> {
    let text = read_text(path)?;
    serde_json::from_str(&text)
        .map_err(|e| format!("{}: not the expected JSON: {e}", path.display()))
}

/// Reads a text file the server delivered (code, a spec, a PEM file), with
/// the path in the error.
pub fn read_text(path: &Path) -> Result<String, String> {
    std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))
}

/// `--id --host --port --log-level --token`.
#[derive(clap::Args, Debug, Clone, PartialEq, Eq)]
pub struct Bootstrap {
    /// Instance id: the node id, or a remote node's remote_id
    #[arg(long)]
    pub id: String,

    /// Hostname or IP address of the hub
    #[arg(long, default_value = "127.0.0.1")]
    pub host: String,

    /// Port of the hub
    #[arg(long)]
    pub port: u16,

    /// Log level: trace, debug, info, warn or error (anything else: info)
    #[arg(long, default_value = "info")]
    pub log_level: String,

    /// Connection token. Defaults to the FLWRS_TOKEN environment variable,
    /// which is preferred: command-line arguments are visible in `ps`.
    #[arg(long)]
    pub token: Option<String>,
}

impl Bootstrap {
    /// The long names of these arguments (without `--`): what a plugin's
    /// launch-args schema must not contain.
    pub const NAMES: [&'static str; 5] = BOOTSTRAP_ARGS;

    /// `--log-level` as the protocol's level; unknown values are `Info`.
    pub fn log_level(&self) -> LogLevel {
        match self.log_level.to_ascii_lowercase().as_str() {
            "trace" => LogLevel::Trace,
            "debug" => LogLevel::Debug,
            "warn" | "warning" => LogLevel::Warn,
            "error" => LogLevel::Error,
            _ => LogLevel::Info,
        }
    }

    /// How to reach the hub: host, port and `--token` (the runner falls back
    /// to `FLWRS_TOKEN`; TLS comes from the `FLWRS_TLS_*` variables).
    pub fn connection(&self) -> ConnectionConfig {
        ConnectionConfig::new(self.host.clone(), self.port).with_token(self.token.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::{CommandFactory, Parser};

    #[derive(Parser, Debug)]
    struct Args {
        #[command(flatten)]
        bootstrap: Bootstrap,
        #[arg(long)]
        extra: Option<String>,
    }

    #[test]
    fn parses_what_the_server_passes() {
        let args = Args::try_parse_from([
            "plugin",
            "--id",
            "node-1",
            "--host",
            "10.0.0.1",
            "--port",
            "7400",
            "--log-level",
            "debug",
            "--extra",
            "x",
        ])
        .unwrap();
        assert_eq!(args.bootstrap.id, "node-1");
        assert_eq!(args.bootstrap.log_level(), LogLevel::Debug);
        assert_eq!(args.bootstrap.token, None);
        let connection = args.bootstrap.connection();
        assert_eq!(
            (connection.host.as_str(), connection.port),
            ("10.0.0.1", 7400)
        );
        assert_eq!(args.extra.as_deref(), Some("x"));
    }

    #[test]
    fn defaults_and_required() {
        let args = Args::try_parse_from(["plugin", "--id", "n", "--port", "1"]).unwrap();
        assert_eq!(args.bootstrap.host, "127.0.0.1");
        assert_eq!(args.bootstrap.log_level(), LogLevel::Info);
        assert!(Args::try_parse_from(["plugin", "--port", "1"]).is_err());
        assert!(Args::try_parse_from(["plugin", "--id", "n"]).is_err());
    }

    #[test]
    fn reads_delivered_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("schemas.json");
        std::fs::write(&path, r#"{"in": {"fields": []}}"#).unwrap();
        let v: serde_json::Value = read_json(&path).unwrap();
        assert_eq!(v["in"]["fields"], serde_json::json!([]));
        let e = read_json::<Vec<u8>>(&path).unwrap_err();
        assert!(e.contains("schemas.json: not the expected JSON"), "{e}");
        let e = read_text(&dir.path().join("missing.lua")).unwrap_err();
        assert!(e.contains("missing.lua"), "{e}");
    }

    #[test]
    fn names_are_the_flags() {
        let flags: Vec<String> = Args::command()
            .get_arguments()
            .filter_map(|a| a.get_long().map(str::to_string))
            .filter(|l| l != "extra")
            .collect();
        let mut expected: Vec<&str> = Bootstrap::NAMES.to_vec();
        expected.sort();
        let mut flags: Vec<&str> = flags.iter().map(String::as_str).collect();
        flags.sort();
        assert_eq!(flags, expected);
    }
}
