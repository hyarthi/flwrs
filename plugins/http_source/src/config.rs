//! The command line (see `schema/launch_args.json`): the bootstrap
//! arguments, the server's (`flwrs_plugin_kit::server::ServerArgs`), and the
//! HTTP source's own; their checks report every problem.

use clap::{Parser, ValueEnum};
use flwrs_plugin::cli::{Bootstrap, read_json, read_text};
use flwrs_plugin_kit::server::{ServerArgs, ServerSettings};
use serde::Deserialize;
use std::path::PathBuf;
use std::time::Duration;

/// flwrs source plugin: an HTTP server that turns requests into events
#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
pub struct Args {
    #[command(flatten)]
    pub bootstrap: Bootstrap,

    #[command(flatten)]
    pub server: ServerArgs,

    /// File: the OpenAPI 3.1 document (JSON or YAML); without it, only health and readiness
    #[arg(long)]
    pub openapi: Option<PathBuf>,

    /// JSON file: the node's out schema (set by the configurator from the document)
    #[arg(long)]
    pub out_schema: Option<PathBuf>,

    /// A prefix for every route (e.g. /v1)
    #[arg(long, default_value = "")]
    pub base_path: String,

    /// When a request is answered: once the hub has the event, or once it's sent
    #[arg(long, value_enum, default_value = "confirmed")]
    pub ack_mode: AckMode,

    /// How long a request waits for the hub to take its event (then 503)
    #[arg(long, default_value_t = 5000)]
    pub enqueue_timeout_ms: u64,

    /// Answer 400s without the validation details
    #[arg(long)]
    pub hide_error_details: bool,

    /// JSON file: Swagger UI for the document, {"enabled", "path", "public"}
    #[arg(long)]
    pub swagger_ui: Option<PathBuf>,
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum AckMode {
    /// 2xx once the hub has the event (committed, for a durable queue).
    #[value(name = "confirmed")]
    Confirmed,
    /// 2xx once the event is sent (faster; a crash can lose it).
    #[value(name = "accepted")]
    Accepted,
}

#[derive(Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields, default)]
pub struct SwaggerUi {
    pub enabled: bool,
    pub path: String,
    /// Without the node's auth.
    pub public: bool,
}

impl Default for SwaggerUi {
    fn default() -> Self {
        Self {
            enabled: false,
            path: "/docs".into(),
            public: false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Settings {
    pub server: ServerSettings,
    /// The document's text (JSON or YAML).
    pub openapi: Option<String>,
    /// The configured out schema (JSON), to check against the document.
    pub out_schema: Option<serde_json::Value>,
    pub base_path: String,
    pub ack_mode: AckMode,
    pub enqueue_timeout: Duration,
    pub hide_error_details: bool,
    pub swagger_ui: SwaggerUi,
}

impl Args {
    pub fn settings(&self) -> Result<Settings, Vec<String>> {
        let mut problems = vec![];
        let server = self.server.settings().map_err(|e| problems.extend(e)).ok();
        let openapi = self.openapi.as_deref().and_then(|p| {
            read_text(p)
                .map_err(|e| problems.push(format!("--openapi {e}")))
                .ok()
        });
        let out_schema = self.out_schema.as_deref().and_then(|p| {
            read_json::<serde_json::Value>(p)
                .map_err(|e| problems.push(format!("--out-schema {e}")))
                .ok()
        });
        let base_path = self.base_path.trim_end_matches('/').to_string();
        if !base_path.is_empty()
            && (!base_path.starts_with('/') || base_path.contains(['{', '}', '?', '#', '*']))
        {
            problems.push(format!(
                "base_path {:?} must be a plain path starting with /",
                self.base_path
            ));
        }
        if self.enqueue_timeout_ms == 0 {
            problems.push("enqueue_timeout_ms must be at least 1".into());
        }
        let swagger_ui: SwaggerUi = self
            .swagger_ui
            .as_deref()
            .and_then(|p| {
                read_json(p)
                    .map_err(|e| problems.push(format!("--swagger-ui {e}")))
                    .ok()
            })
            .unwrap_or_default();
        if swagger_ui.enabled {
            if !cfg!(feature = "swagger-ui") {
                problems.push(
                    "swagger_ui: this build of the plugin has no Swagger UI (feature swagger-ui)"
                        .into(),
                );
            }
            if openapi.is_none() {
                problems.push("swagger_ui needs an openapi document to show".into());
            }
            let path = swagger_ui.path.trim_end_matches('/');
            if !path.starts_with('/') || path.len() < 2 || path.contains(['{', '}', '?', '#', '*'])
            {
                problems.push(format!(
                    "swagger_ui.path {:?} must be a plain path like /docs",
                    swagger_ui.path
                ));
            }
        }
        match server {
            Some(server) if problems.is_empty() => Ok(Settings {
                server,
                openapi,
                out_schema,
                base_path,
                ack_mode: self.ack_mode,
                enqueue_timeout: Duration::from_millis(self.enqueue_timeout_ms),
                hide_error_details: self.hide_error_details,
                swagger_ui: SwaggerUi {
                    path: swagger_ui.path.trim_end_matches('/').to_string(),
                    ..swagger_ui
                },
            }),
            _ => Err(problems),
        }
    }
}
