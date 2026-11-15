//! Stimuli: what makes a source emit when it waits for the outside world (an
//! HTTP request, an MQTT publish). Used by `audit`, `test` and `bench`.
//!
//! In Rust, pass any async closure:
//!
//! ```rust,ignore
//! let audit = Audit::new(target).stimulus(|ctx: StimulusCtx| async move {
//!     post(ctx.url("/in"), "{}").await.map_err(|e| e.to_string())?;
//!     Ok(None) // or Some(id) when the request carries a correlation id
//! });
//! ```
//!
//! From the CLI and config files, a command ([`CommandStimulus`]).

use flwrs_plugin::schema::payload::PluginPayload;
use serde_json::Value;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;

/// An id a stimulus put into its request, found again in the emitted event by
/// a [`Correlator`]. Lets `test` tie a request to its event and `bench`
/// measure per-event latency.
pub type Correlation = String;

/// Finds the correlation id in an emitted event's payload.
pub type Correlator = Arc<dyn Fn(&PluginPayload) -> Option<Correlation> + Send + Sync>;

/// What a stimulus knows about the run.
#[derive(Debug, Clone)]
pub struct StimulusCtx {
    /// 0 for the first call; `bench` counts up.
    pub iteration: u64,
    /// When this call was meant to happen (`bench`'s open loop measures from here).
    pub intended: Instant,
    /// The node's `launch_args`.
    pub launch_args: Value,
    /// The run's free port (`${port}` in config files and launch args).
    pub port: u16,
}

impl StimulusCtx {
    /// `http://127.0.0.1:<port><path>`.
    pub fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }
}

/// The future a stimulus returns.
pub type StimulusFuture = Pin<Box<dyn Future<Output = Result<Option<Correlation>, String>> + Send>>;

/// Makes the plugin emit. `Ok(Some(id))` when the stimulus can be correlated.
pub trait Stimulus: Send {
    fn fire(&mut self, ctx: StimulusCtx) -> StimulusFuture;
}

impl<F, Fut> Stimulus for F
where
    F: FnMut(StimulusCtx) -> Fut + Send,
    Fut: Future<Output = Result<Option<Correlation>, String>> + Send + 'static,
{
    fn fire(&mut self, ctx: StimulusCtx) -> StimulusFuture {
        Box::pin(self(ctx))
    }
}

/// A command as a stimulus: success is exit status 0. `${port}` (and
/// `${name}` for `vars`) in its arguments is replaced, and `TRELLIS_PORT` is set.
#[derive(Debug, Clone)]
pub struct CommandStimulus {
    pub argv: Vec<String>,
    pub cwd: PathBuf,
    pub vars: Vars,
}

impl Stimulus for CommandStimulus {
    fn fire(&mut self, ctx: StimulusCtx) -> StimulusFuture {
        let argv: Vec<String> = self
            .argv
            .iter()
            .map(|a| substitute_vars(a, ctx.port, &self.vars))
            .collect();
        let cwd = self.cwd.clone();
        Box::pin(async move { run_command(&argv, &cwd, ctx.port).await.map(|_| None) })
    }
}

/// Runs a command to completion; the error carries the end of its output.
pub(crate) async fn run_command(argv: &[String], cwd: &PathBuf, port: u16) -> Result<(), String> {
    let (program, args) = argv.split_first().ok_or("empty command")?;
    let output = tokio::process::Command::new(program)
        .args(args)
        .current_dir(cwd)
        .env("TRELLIS_PORT", port.to_string())
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .map_err(|e| format!("couldn't run {program}: {e}"))?;
    if output.status.success() {
        return Ok(());
    }
    let mut text = String::from_utf8_lossy(&output.stderr).to_string();
    if text.trim().is_empty() {
        text = String::from_utf8_lossy(&output.stdout).to_string();
    }
    let tail: String = text
        .trim()
        .chars()
        .rev()
        .take(500)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    Err(format!(
        "{} failed ({}): {tail}",
        argv.join(" "),
        output.status
    ))
}

/// A free TCP port on loopback (free when checked; good enough for tests).
pub fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .map(|a| a.port())
        .unwrap_or(0)
}

/// Named values for `${name}` in payload files, launch args and commands.
pub type Vars = std::collections::BTreeMap<String, String>;

/// `${port}` → the run's port.
pub(crate) fn substitute(text: &str, port: u16) -> String {
    substitute_vars(text, port, &Vars::new())
}

/// `${port}` and `${name}` for each of `vars`.
pub(crate) fn substitute_vars(text: &str, port: u16, vars: &Vars) -> String {
    let mut text = text.replace("${port}", &port.to_string());
    for (name, value) in vars {
        text = text.replace(&format!("${{{name}}}"), value);
    }
    text
}

/// [`substitute`] in every string of a JSON value.
pub(crate) fn substitute_json(value: &Value, port: u16) -> Value {
    substitute_json_vars(value, port, &Vars::new())
}

/// [`substitute_vars`] in every string of a JSON value.
pub(crate) fn substitute_json_vars(value: &Value, port: u16, vars: &Vars) -> Value {
    match value {
        Value::String(s) => Value::String(substitute_vars(s, port, vars)),
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|v| substitute_json_vars(v, port, vars))
                .collect(),
        ),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), substitute_json_vars(v, port, vars)))
                .collect(),
        ),
        other => other.clone(),
    }
}
