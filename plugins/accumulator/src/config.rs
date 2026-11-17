//! The command line: the bootstrap arguments and the node's settings (code and
//! objects as files in the node's `config/` folder; see
//! `schema/launch_args.json`), and their checks.

use crate::engine::LateClose;
use crate::window::Window;
use clap::{Parser, ValueEnum};
use flwrs_plugin::cli::{Bootstrap, read_json, read_text};
use flwrs_plugin::schema::payload::SchemaDefinition;
use flwrs_script_transform::config::{OnError, read_limits, read_schemas, read_state};
use flwrs_script_transform::store::StateSettings;
use flwrs_types::lua::Limits;
use flwrs_types::path::Path;
use std::path::PathBuf;

/// flwrs transform plugin that aggregates events per key, over windows
#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
pub struct Args {
    #[command(flatten)]
    pub bootstrap: Bootstrap,

    /// JSON file: the node's payload schemas, {"in": …, "out": …}
    #[arg(long)]
    pub schemas: PathBuf,

    /// Dot path of the grouping key in the input (e.g. device or tags["site"]);
    /// without it, everything is one group
    #[arg(long)]
    pub key: Option<String>,

    /// Lua file: function(key) returning a group's initial state
    #[arg(long)]
    pub init: Option<PathBuf>,

    /// Lua file: function(state, payload, meta, key) returning the new state
    /// (nil forgets it) and an optional output
    #[arg(long)]
    pub step: PathBuf,

    /// Lua file: function(state, key, window) returning the output of a window
    /// that closes (or nil)
    #[arg(long)]
    pub finish: Option<PathBuf>,

    /// JSON file: the windows: {"type": "none" | "tumbling" | "hopping" |
    /// "session", "size", "hop", "gap", "idle_ttl"}
    #[arg(long)]
    pub window: Option<PathBuf>,

    /// Windows that ended while the plugin wasn't running: run finish late, or drop them
    #[arg(long, value_enum, default_value = "run")]
    pub late_close: LateCloseArg,

    /// What happens to an event the step fails on
    #[arg(long, value_enum, default_value = "nack")]
    pub on_error: OnError,

    /// JSON file: limits per call (max_instructions, timeout_ms, max_memory_bytes)
    #[arg(long)]
    pub limits: Option<PathBuf>,

    /// JSON file: state limits and durability
    #[arg(long)]
    pub state: Option<PathBuf>,
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum LateCloseArg {
    #[value(name = "run")]
    Run,
    #[value(name = "drop")]
    Drop,
}

#[derive(Debug)]
pub struct Settings {
    pub input: SchemaDefinition,
    pub output: SchemaDefinition,
    pub key: Option<Path>,
    pub init: Option<String>,
    pub step: String,
    pub finish: Option<String>,
    pub window: Window,
    pub late_close: LateClose,
    pub on_error: OnError,
    pub limits: Limits,
    pub state: StateSettings,
}

impl Args {
    /// Reads and checks the settings. Every problem, not just the first.
    pub fn settings(&self) -> Result<Settings, Vec<String>> {
        let mut problems = vec![];
        let (input, output) = read_schemas(&self.schemas, &mut problems);
        let key = self.key.as_deref().filter(|k| !k.is_empty()).and_then(|k| {
            Path::parse(k)
                .map_err(|e| problems.push(format!("--key {e}")))
                .ok()
        });
        let mut code = |flag: &str, path: Option<&std::path::Path>| {
            path.and_then(|p| {
                read_text(p)
                    .map_err(|e| problems.push(format!("--{flag} {e}")))
                    .ok()
            })
        };
        let init = code("init", self.init.as_deref());
        let finish = code("finish", self.finish.as_deref());
        let step = code("step", Some(&self.step));
        let window_json: Option<serde_json::Value> = self.window.as_deref().and_then(|p| {
            read_json(p)
                .map_err(|e| problems.push(format!("--window {e}")))
                .ok()
        });
        let window = Window::parse(window_json.as_ref())
            .map_err(|e| problems.extend(e))
            .ok();
        if let Some(window) = window
            && !matches!(window, Window::None { .. })
            && self.finish.is_none()
        {
            problems.push("windows need a finish (what a closing window outputs)".into());
        }
        let limits = read_limits(self.limits.as_deref(), &mut problems);
        let state = read_state(self.state.as_deref(), &mut problems);
        match (input, output, step, window) {
            (Some(input), Some(output), Some(step), Some(window)) if problems.is_empty() => {
                Ok(Settings {
                    input,
                    output,
                    key,
                    init,
                    step,
                    finish,
                    window,
                    late_close: match self.late_close {
                        LateCloseArg::Run => LateClose::Run,
                        LateCloseArg::Drop => LateClose::Drop,
                    },
                    on_error: self.on_error,
                    limits,
                    state,
                })
            }
            _ => Err(problems),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(window: Option<&str>, finish: bool) -> (tempfile::TempDir, Args) {
        let dir = tempfile::tempdir().unwrap();
        let file = |name: &str, text: &str| {
            let path = dir.path().join(name);
            std::fs::write(&path, text).unwrap();
            path.to_string_lossy().into_owned()
        };
        let mut argv = vec![
            "acc".to_string(),
            "--id".into(),
            "n".into(),
            "--port".into(),
            "1".into(),
            "--schemas".into(),
            file(
                "schemas.json",
                r#"{"in": {"fields": [{"key": "d", "type": "STRING"}]}, "out": {"fields": [{"key": "n", "type": "I64"}]}}"#,
            ),
            "--step".into(),
            file("step.lua", "return (state or 0) + 1"),
            "--key".into(),
            "d".into(),
        ];
        if let Some(w) = window {
            argv.push("--window".into());
            argv.push(file("window.json", w));
        }
        if finish {
            argv.push("--finish".into());
            argv.push(file("finish.lua", "return { n = state }"));
        }
        (dir, Args::try_parse_from(argv).unwrap())
    }

    #[test]
    fn reads_the_settings() {
        let (_d, a) = args(Some(r#"{"type": "tumbling", "size": "1m"}"#), true);
        let s = a.settings().unwrap();
        assert_eq!(s.window, Window::Tumbling { size: 60_000 });
        assert_eq!(s.key.unwrap().to_string(), "d");
        let (_d, a) = args(None, false);
        assert_eq!(
            a.settings().unwrap().window,
            Window::None { idle_ttl: None }
        );
    }

    #[test]
    fn windows_need_a_finish() {
        let (_d, a) = args(Some(r#"{"type": "session", "gap": "5m"}"#), false);
        assert!(a.settings().unwrap_err()[0].contains("need a finish"));
        let (_d, a) = args(Some(r#"{"type": "session"}"#), true);
        assert!(a.settings().unwrap_err()[0].contains("window.gap"));
    }
}
