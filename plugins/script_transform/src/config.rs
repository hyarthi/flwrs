//! The command line: the bootstrap arguments and the node's settings, which
//! the server delivers as files in the node's `config/` folder (see
//! `schema/launch_args.json`), and their checks.

use crate::store::StateSettings;
use clap::{Parser, ValueEnum};
use flwrs_plugin::cli::{Bootstrap, read_json, read_text};
use flwrs_plugin::schema::payload::SchemaDefinition;
use flwrs_plugin::schema::rules;
use flwrs_types::lua::Limits;
use serde::Deserialize;
use std::path::PathBuf;
use std::time::Duration;

/// flwrs transform plugin that runs a Lua script on each event, with keyed state
#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
pub struct Args {
    #[command(flatten)]
    pub bootstrap: Bootstrap,

    /// JSON file: the node's payload schemas, {"in": …, "out": …}
    #[arg(long)]
    pub schemas: PathBuf,

    /// Lua file: the script, run as function(payload, meta) for each event
    #[arg(long)]
    pub script: PathBuf,

    /// JSON file: limits per event (max_instructions, timeout_ms, max_memory_bytes)
    #[arg(long)]
    pub limits: Option<PathBuf>,

    /// JSON file: state limits and durability (max_keys, max_value_bytes,
    /// max_bytes, sync, applied_window)
    #[arg(long)]
    pub state: Option<PathBuf>,

    /// What happens to an event the script fails on
    #[arg(long, value_enum, default_value = "nack")]
    pub on_error: OnError,
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnError {
    /// Reject it; the hub doesn't deliver it again (dead letter).
    #[value(name = "nack")]
    Nack,
    /// Reject it; the hub delivers it again later.
    #[value(name = "nack_retry")]
    NackRetry,
    /// Acknowledge it with no outputs.
    #[value(name = "drop")]
    Drop,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SchemasFile {
    #[serde(rename = "in")]
    input: serde_json::Value,
    out: serde_json::Value,
}

/// `limits`.
#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct LimitsFile {
    max_instructions: Option<u64>,
    timeout_ms: Option<u64>,
    max_memory_bytes: Option<u64>,
}

pub const DEFAULT_MAX_INSTRUCTIONS: u64 = 1_000_000;
pub const DEFAULT_TIMEOUT_MS: u64 = 100;
pub const DEFAULT_MAX_MEMORY_BYTES: u64 = 16 * 1024 * 1024;

/// Everything the plugin runs with.
#[derive(Debug)]
pub struct Settings {
    pub input: SchemaDefinition,
    pub output: SchemaDefinition,
    pub script: String,
    pub limits: Limits,
    pub state: StateSettings,
    pub on_error: OnError,
}

/// The node's payload schemas from a `schemas` settings file.
pub fn read_schemas(
    path: &std::path::Path,
    problems: &mut Vec<String>,
) -> (Option<SchemaDefinition>, Option<SchemaDefinition>) {
    let schemas: Option<SchemasFile> = read_json(path)
        .map_err(|e| problems.push(format!("--schemas {e}")))
        .ok();
    let mut schema = |name: &str, json: Option<&serde_json::Value>| {
        let json = json?;
        match rules::parse_json(&json.to_string()) {
            Ok(s) => {
                let errors = rules::check_well_formed(&s);
                if errors.is_empty() {
                    Some(s)
                } else {
                    problems.push(format!("schemas.{name}: {}", errors.join("; ")));
                    None
                }
            }
            Err(e) => {
                problems.push(format!("schemas.{name}: {e}"));
                None
            }
        }
    };
    let input = schema("in", schemas.as_ref().map(|s| &s.input));
    let output = schema("out", schemas.as_ref().map(|s| &s.out));
    (input, output)
}

/// Lua limits from a `limits` settings file (or the defaults).
pub fn read_limits(path: Option<&std::path::Path>, problems: &mut Vec<String>) -> Limits {
    let limits = match path {
        None => LimitsFile::default(),
        Some(path) => read_json(path)
            .map_err(|e| problems.push(format!("--limits {e}")))
            .unwrap_or_default(),
    };
    let mut range = |name: &str, value: Option<u64>, min: u64, max: u64, default: u64| {
        let v = value.unwrap_or(default);
        if !(min..=max).contains(&v) {
            problems.push(format!("limits.{name} must be between {min} and {max}"));
        }
        v
    };
    Limits {
        max_instructions: range(
            "max_instructions",
            limits.max_instructions,
            1,
            1_000_000_000,
            DEFAULT_MAX_INSTRUCTIONS,
        ),
        timeout: Duration::from_millis(range(
            "timeout_ms",
            limits.timeout_ms,
            1,
            10_000,
            DEFAULT_TIMEOUT_MS,
        )),
        max_memory_bytes: range(
            "max_memory_bytes",
            limits.max_memory_bytes,
            256 * 1024,
            1024 * 1024 * 1024,
            DEFAULT_MAX_MEMORY_BYTES,
        ),
    }
}

/// State settings from a `state` settings file (or the defaults).
pub fn read_state(path: Option<&std::path::Path>, problems: &mut Vec<String>) -> StateSettings {
    match path {
        None => StateSettings::default(),
        Some(path) => read_json(path)
            .map_err(|e| problems.push(format!("--state {e}")))
            .unwrap_or_default(),
    }
}

impl Args {
    /// Reads and checks the settings files. Every problem, not just the first.
    pub fn settings(&self) -> Result<Settings, Vec<String>> {
        let mut problems = vec![];
        let (input, output) = read_schemas(&self.schemas, &mut problems);
        let script = read_text(&self.script)
            .map_err(|e| problems.push(format!("--script {e}")))
            .ok();
        let limits = read_limits(self.limits.as_deref(), &mut problems);
        let state = read_state(self.state.as_deref(), &mut problems);
        match (input, output, script) {
            (Some(input), Some(output), Some(script)) if problems.is_empty() => Ok(Settings {
                input,
                output,
                script,
                limits,
                state,
                on_error: self.on_error,
            }),
            _ => Err(problems),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(schemas: &str, limits: Option<&str>) -> (tempfile::TempDir, Args) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("schemas.json"), schemas).unwrap();
        std::fs::write(dir.path().join("script.lua"), "return payload").unwrap();
        let mut argv = vec![
            "script".to_string(),
            "--id".into(),
            "n".into(),
            "--port".into(),
            "1".into(),
            "--schemas".into(),
            dir.path()
                .join("schemas.json")
                .to_string_lossy()
                .into_owned(),
            "--script".into(),
            dir.path().join("script.lua").to_string_lossy().into_owned(),
        ];
        if let Some(limits) = limits {
            std::fs::write(dir.path().join("limits.json"), limits).unwrap();
            argv.push("--limits".into());
            argv.push(
                dir.path()
                    .join("limits.json")
                    .to_string_lossy()
                    .into_owned(),
            );
        }
        (dir, Args::try_parse_from(argv).unwrap())
    }

    const SCHEMAS: &str = r#"{"in": {"fields": [{"key": "x", "type": "I64"}]}, "out": {"fields": [{"key": "y", "type": "STRING"}]}}"#;

    #[test]
    fn reads_the_settings_files() {
        let (_d, args) = files(SCHEMAS, Some(r#"{"timeout_ms": 250}"#));
        let s = args.settings().unwrap();
        assert_eq!(s.input.fields[0].key, "x");
        assert_eq!(s.output.fields[0].key, "y");
        assert_eq!(s.limits.timeout, Duration::from_millis(250));
        assert_eq!(s.limits.max_instructions, DEFAULT_MAX_INSTRUCTIONS);
        assert_eq!(s.script, "return payload");
        assert_eq!(s.on_error, OnError::Nack);
    }

    #[test]
    fn reports_every_problem() {
        let (_d, args) = files(
            r#"{"in": {"fields": [{"key": "x"}]}, "out": {"fields": "no"}}"#,
            Some(r#"{"timeout_ms": 0, "max_memory_bytes": 1}"#),
        );
        let problems = args.settings().unwrap_err();
        assert_eq!(problems.len(), 4, "{problems:?}");
        let (_d, args) = files(SCHEMAS, Some(r#"{"color": 1}"#));
        assert!(args.settings().unwrap_err()[0].contains("--limits"));
    }
}
