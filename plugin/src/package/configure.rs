//! The configurator contract (manifest `configurator`, contract version 1):
//! a program in the package that fills some of a node's launch args from its
//! other settings, run by the server (and by trellis) before the plugin.
//!
//! The run, as `docs/plugin-spec.md` specifies it:
//!
//! - A fresh temporary folder is its working directory, deleted afterwards.
//!   It holds `config/`, laid out as the node's folder would have it (the
//!   launch plan's files and uploads, as copies), and `request.json`:
//!   `{"contract": 1, "plugin": {"id", "version"}, "node_id",
//!   "launch_args" (without secrets and configured fields), "fields": [...]}`.
//! - Arguments: `--request request.json --response response.json`. The
//!   environment: the launch plan's variables (secrets delivered by env), the
//!   `PATH` and, on Windows, `SystemRoot`; `TMPDIR` / `TEMP` / `TMP` point
//!   into the temporary folder.
//! - It writes `response.json`: `{"values": {field: value}, "files": {field:
//!   "path in the folder"}, "warnings": [...], "errors": [...]}` and exits 0.
//!   A file becomes a node upload; its field is set to the file's name. A
//!   list upload field takes a list of paths and is set to their names.
//!
//! The result is checked: only requested fields, files only for upload
//! fields, and the merged launch args against the launch-args rules.
//!
//! **Pre-flight** (manifest `configurator.preflight`, contract version 2) is
//! a second, read-only mode of the same program: "can this node run on this
//! machine right now?". It runs the same way, with `--mode preflight` before
//! the other arguments, the node's whole launch args (configured fields
//! included, secrets left out) and `"mode": "preflight"` in the request. It
//! answers `{"problems": [{"message", "retryable"}], "warnings": [...]}`; any
//! values or files in the answer are refused. A program that can't start,
//! crashes or answers garbage is a permanent problem carrying its stderr; a
//! timeout is a retryable one. `releasing` in the request lists the running
//! nodes of the same plugin and scene that the change stops first: a resource
//! one of them holds will be free, so it isn't reported as busy.
//!
//! **Describe** (manifest `configurator.describe`, contract version 3) is a
//! third, read-only mode, for editors: what the configured fields would say
//! about the node's uploaded files. It runs with `--mode describe`, `"mode":
//! "describe"` and only the node's upload settings in `launch_args`, since
//! the rest may still be incomplete. It answers `{"values": {field: value}}`
//! with requested fields only, or `{"errors": [...]}`; nothing is stored.

use super::launch::{
    ConfigContent, LaunchPlan, LaunchSpec, Via, check_file_name, validate_launch_args,
};
use super::manifest::Manifest;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// The contract version this module speaks.
pub const CONTRACT: u32 = 1;
/// The contract version of a pre-flight run.
pub const PREFLIGHT_CONTRACT: u32 = 2;
/// The value of `--mode` (and the request's `mode`) for a pre-flight run.
pub const PREFLIGHT_MODE: &str = "preflight";
/// The contract version of a describe run.
pub const DESCRIBE_CONTRACT: u32 = 3;
/// The value of `--mode` (and the request's `mode`) for a describe run.
pub const DESCRIBE_MODE: &str = "describe";
pub const REQUEST_FILE: &str = "request.json";
pub const RESPONSE_FILE: &str = "response.json";

/// Bounds on one run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limits {
    pub timeout: Duration,
    pub max_response_bytes: u64,
    /// Each returned file.
    pub max_file_bytes: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(30),
            max_response_bytes: 1024 * 1024,
            max_file_bytes: 1024 * 1024 * 1024,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub contract: u32,
    pub plugin: PluginRef,
    pub node_id: String,
    pub launch_args: Value,
    pub fields: Vec<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PluginRef {
    pub id: String,
    pub version: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
#[serde(deny_unknown_fields, default)]
pub struct Response {
    pub values: Map<String, Value>,
    /// Field → the path of its file in the folder (a list for a list
    /// upload field).
    pub files: BTreeMap<String, OneOrMany<String>>,
    pub warnings: Vec<String>,
    pub errors: Vec<String>,
}

/// One value, or a list (a list upload field's files).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(untagged)]
pub enum OneOrMany<T> {
    One(T),
    Many(Vec<T>),
}

impl<T> OneOrMany<T> {
    fn into_parts(self) -> (bool, Vec<T>) {
        match self {
            OneOrMany::One(v) => (false, vec![v]),
            OneOrMany::Many(vs) => (true, vs),
        }
    }
}

/// What to run, and on what.
pub struct Invocation<'a> {
    /// The installed (or unpacked) package: the configurator's path is relative to it.
    pub package_dir: &'a Path,
    /// Runs this program instead of the manifest's (e.g. one cargo built for a test).
    pub executable: Option<&'a Path>,
    pub manifest: &'a Manifest,
    pub spec: &'a LaunchSpec,
    /// The compiled launch-args schema, to check the merged result.
    pub schema: Option<&'a jsonschema::Validator>,
    /// The node's launch args, secrets included; configured fields are ignored.
    pub launch_args: &'a Value,
    /// An upload's stored file, by its name on the node.
    pub uploads: &'a dyn Fn(&str) -> Option<PathBuf>,
    pub node_id: &'a str,
    pub limits: &'a Limits,
}

/// A file the configurator produced, to become a node upload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfiguredFile {
    pub field: String,
    /// The upload's name (the file's name).
    pub name: String,
    /// Where it is now (inside the temporary folder: move or copy it before
    /// dropping [`Configured`]).
    pub path: PathBuf,
}

/// A successful run.
#[derive(Debug)]
pub struct Configured {
    /// The configured fields' values (files' fields hold the upload name).
    pub values: Map<String, Value>,
    pub files: Vec<ConfiguredFile>,
    pub warnings: Vec<String>,
    /// The node's launch args with the configured values in place.
    pub launch_args: Value,
    _dir: tempfile::TempDir,
}

/// The remote form of the contract (a remote node's configurator endpoint,
/// `docs/plugin-spec.md` §4.1): the request with the files `config/` would
/// hold inline (name → base64), POSTed as JSON.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RemoteRequest {
    pub contract: u32,
    pub plugin: PluginRef,
    pub node_id: String,
    pub launch_args: Value,
    pub fields: Vec<String>,
    /// `config/` as the plugin would have it: file name → content (base64).
    pub files: BTreeMap<String, String>,
}

/// A file a remote configurator returns: where the hub downloads it from.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RemoteFile {
    /// Its name (the upload's name on the node).
    pub name: String,
    /// Where to download it (with the configurator endpoint's credentials).
    pub url: String,
}

/// The remote form of [`Response`]: returned files by URL.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
#[serde(deny_unknown_fields, default)]
pub struct RemoteResponse {
    pub values: Map<String, Value>,
    pub files: BTreeMap<String, OneOrMany<RemoteFile>>,
    pub warnings: Vec<String>,
    pub errors: Vec<String>,
}

/// The request for a remote configurator: secrets only with `send_secrets`.
pub fn remote_request(inv: &Invocation, send_secrets: bool) -> Result<RemoteRequest, Vec<String>> {
    use base64::Engine;
    let fields = configured_fields(inv)?;
    let base = without_configured(inv.spec, inv.launch_args);
    let plan = inv.spec.plan(&base, Path::new(super::launch::CONFIG_DIR))?;
    let mut files = BTreeMap::new();
    for file in &plan.files {
        if !send_secrets
            && inv
                .spec
                .fields()
                .any(|f| f.secret && f.file_name.as_deref() == Some(file.name.as_str()))
        {
            continue;
        }
        let bytes = match &file.content {
            ConfigContent::Data(bytes) => bytes.clone(),
            ConfigContent::Upload(name) => {
                let secret = inv.spec.fields().any(|f| {
                    f.secret
                        && base
                            .get(&f.name)
                            .is_some_and(|v| f.upload_names(v).contains(&name.as_str()))
                });
                if secret && !send_secrets {
                    continue;
                }
                let stored = (inv.uploads)(name)
                    .ok_or_else(|| vec![format!("the uploaded file {name:?} is missing")])?;
                std::fs::read(&stored).map_err(|e| vec![format!("{name}: {e}")])?
            }
        };
        files.insert(
            file.name.clone(),
            base64::engine::general_purpose::STANDARD.encode(bytes),
        );
    }
    Ok(RemoteRequest {
        contract: CONTRACT,
        plugin: PluginRef {
            id: inv.manifest.id.clone(),
            version: inv.manifest.version.clone(),
        },
        node_id: inv.node_id.to_string(),
        launch_args: if send_secrets {
            base
        } else {
            without_secrets(inv.spec, &base)
        },
        fields,
        files,
    })
}

/// Downloads a remote configurator's file: `(url, destination, max bytes)`.
pub type Download<'a> = &'a dyn Fn(&str, &Path, u64) -> Result<(), String>;

/// Checks a remote configurator's answer like a local one's: its files are
/// downloaded into a temporary folder first (each under its field).
pub fn remote_result(
    inv: &Invocation,
    response: RemoteResponse,
    download: Download,
) -> Result<Configured, Vec<String>> {
    if !response.errors.is_empty() {
        return Err(response.errors);
    }
    let fields = configured_fields(inv)?;
    let dir = tempfile::Builder::new()
        .prefix("flwrs-configure-")
        .tempdir()
        .map_err(|e| vec![format!("a temporary folder: {e}")])?;
    let mut files = BTreeMap::new();
    let mut problems = vec![];
    let mut folders = 0;
    for (field, returned) in response.files {
        let (list, returned) = returned.into_parts();
        let mut paths = vec![];
        for file in returned {
            if let Err(e) = check_file_name(&file.name) {
                problems.push(format!("files.{field}: the name {:?}: {e}", file.name));
                continue;
            }
            // One folder per file, so two files may share a name (the check
            // then says which).
            let relative = format!("f{folders}/{}", file.name);
            let folder = dir.path().join(format!("f{folders}"));
            folders += 1;
            if let Err(e) = std::fs::create_dir(&folder) {
                problems.push(format!("files.{field}: {e}"));
                continue;
            }
            if let Err(e) = download(
                &file.url,
                &folder.join(&file.name),
                inv.limits.max_file_bytes,
            ) {
                problems.push(format!("files.{field}: downloading {}: {e}", file.url));
                continue;
            }
            paths.push(relative);
        }
        let paths = if list {
            OneOrMany::Many(paths)
        } else {
            match paths.pop() {
                Some(p) => OneOrMany::One(p),
                None => continue,
            }
        };
        files.insert(field, paths);
    }
    if !problems.is_empty() {
        return Err(problems);
    }
    let base = without_configured(inv.spec, inv.launch_args);
    let response = Response {
        values: response.values,
        files,
        warnings: response.warnings,
        errors: vec![],
    };
    check(inv, dir, &fields, base, response)
}

fn configured_fields(inv: &Invocation) -> Result<Vec<String>, Vec<String>> {
    inv.manifest
        .configurator
        .as_ref()
        .map(|c| c.fields.clone())
        .ok_or_else(|| vec!["the plugin has no configurator".to_string()])
}

/// The launch args without the configured fields (what the user owns).
pub fn without_configured(spec: &LaunchSpec, args: &Value) -> Value {
    match args {
        Value::Object(map) => Value::Object(
            map.iter()
                .filter(|(k, _)| !spec.is_configured(k))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// Runs the configurator. Errors: everything that went wrong, for the user
/// (the configurator's own errors first).
pub fn configure(inv: &Invocation) -> Result<Configured, Vec<String>> {
    let configurator = inv
        .manifest
        .configurator
        .as_ref()
        .ok_or_else(|| vec!["the plugin has no configurator".to_string()])?;
    let fields: Vec<String> = configurator.fields.clone();
    let base = without_configured(inv.spec, inv.launch_args);
    let dir = tempfile::Builder::new()
        .prefix("flwrs-configure-")
        .tempdir()
        .map_err(|e| vec![format!("a temporary folder: {e}")])?;
    let plan = lay_out(inv, &base, dir.path())?;
    let request = Request {
        contract: CONTRACT,
        plugin: PluginRef {
            id: inv.manifest.id.clone(),
            version: inv.manifest.version.clone(),
        },
        node_id: inv.node_id.to_string(),
        launch_args: without_secrets(inv.spec, &base),
        fields: fields.clone(),
    };
    let request_text = serde_json::to_vec_pretty(&request).expect("a request serializes");
    std::fs::write(dir.path().join(REQUEST_FILE), request_text)
        .map_err(|e| vec![format!("{REQUEST_FILE}: {e}")])?;

    let args = ["--request", REQUEST_FILE, "--response", RESPONSE_FILE];
    let (status, stderr) = run_program(
        inv,
        &configurator.executable,
        dir.path(),
        &plan,
        &args,
        None,
    )
    .map_err(|failure| vec![failure.message()])?;
    let response = read_response(dir.path(), inv.limits);
    let response = match (status.success(), response) {
        (true, Ok(r)) => r,
        (_, Ok(r)) if !r.errors.is_empty() => return Err(r.errors),
        (true, Err(e)) => return Err(vec![e]),
        (false, _) => {
            return Err(vec![format!(
                "the configurator failed ({status}){}",
                stderr_detail(&stderr)
            )]);
        }
    };
    if !response.errors.is_empty() {
        return Err(response.errors);
    }
    check(inv, dir, &fields, base, response)
}

/// Writes `config/` into the run's folder, as the plugin would get it, and
/// returns the launch plan (its environment).
fn lay_out(inv: &Invocation, args: &Value, dir: &Path) -> Result<LaunchPlan, Vec<String>> {
    let config_dir = dir.join(super::launch::CONFIG_DIR);
    std::fs::create_dir(&config_dir).map_err(|e| vec![format!("{}: {e}", config_dir.display())])?;
    let plan = inv.spec.plan(args, &config_dir)?;
    for file in &plan.files {
        let target = config_dir.join(&file.name);
        let written = match &file.content {
            ConfigContent::Data(bytes) => std::fs::write(&target, bytes),
            ConfigContent::Upload(name) => match (inv.uploads)(name) {
                Some(stored) => std::fs::copy(&stored, &target).map(|_| ()),
                None => return Err(vec![format!("the uploaded file {name:?} is missing")]),
            },
        };
        written.map_err(|e| vec![format!("config/{}: {e}", file.name)])?;
    }
    Ok(plan)
}

/// Why a run of the program didn't get to an exit status.
enum RunFailure {
    Start(String),
    Timeout(Duration),
    Wait(String),
    Cancelled,
}

impl RunFailure {
    fn message(&self) -> String {
        match self {
            RunFailure::Start(e) => e.clone(),
            RunFailure::Timeout(limit) => format!(
                "the configurator didn't finish within {} s",
                limit.as_secs_f64()
            ),
            RunFailure::Wait(e) => format!("waiting for the configurator: {e}"),
            RunFailure::Cancelled => "the run was cancelled".to_string(),
        }
    }
}

/// How long the end of stderr is waited for once the program has exited: a
/// process it left behind may keep the pipe open.
const STDERR_GRACE: Duration = Duration::from_millis(500);

/// Runs the configurator in `dir` with a clean environment, until it exits,
/// its time is up or `cancel` is set (then it's killed). Returns its status
/// and the end of its stderr.
fn run_program(
    inv: &Invocation,
    executable: &str,
    dir: &Path,
    plan: &LaunchPlan,
    args: &[&str],
    cancel: Option<&AtomicBool>,
) -> Result<(ExitStatus, String), RunFailure> {
    let executable = match inv.executable {
        Some(program) => program.to_path_buf(),
        None => inv.package_dir.join(executable),
    };
    // It runs in the temporary folder: a relative path (e.g. a server whose
    // plugins folder is configured relative to its working directory) would
    // be looked up there.
    let executable = std::path::absolute(&executable).unwrap_or(executable);
    let mut command = Command::new(&executable);
    command
        .args(args)
        .current_dir(dir)
        .env_clear()
        .envs(plan.env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .env("TMPDIR", dir)
        .env("TEMP", dir)
        .env("TMP", dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    for name in ["PATH", "SystemRoot", "SYSTEMROOT", "windir"] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    let mut child = command.spawn().map_err(|e| {
        RunFailure::Start(format!(
            "starting the configurator {}: {e}",
            executable.display()
        ))
    })?;
    // Read stderr meanwhile (a full pipe would block the child); keep its end.
    let mut stderr = child.stderr.take().expect("piped");
    let (tail_tx, tail_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut tail = Vec::new();
        let mut buf = [0u8; 4096];
        while let Ok(n) = stderr.read(&mut buf) {
            if n == 0 {
                break;
            }
            tail.extend_from_slice(&buf[..n]);
            if tail.len() > 8192 {
                tail.drain(..tail.len() - 4096);
            }
        }
        let _ = tail_tx.send(String::from_utf8_lossy(&tail).trim().to_string());
    });
    let started = Instant::now();
    let status = loop {
        if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(RunFailure::Cancelled);
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() >= inv.limits.timeout => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(RunFailure::Timeout(inv.limits.timeout));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(e) => return Err(RunFailure::Wait(e.to_string())),
        }
    };
    Ok((
        status,
        tail_rx.recv_timeout(STDERR_GRACE).unwrap_or_default(),
    ))
}

fn stderr_detail(stderr: &str) -> String {
    if stderr.is_empty() {
        String::new()
    } else {
        format!(": {stderr}")
    }
}

/// The request of a pre-flight run (`request.json`).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PreflightRequest {
    /// [`PREFLIGHT_CONTRACT`].
    pub contract: u32,
    /// [`PREFLIGHT_MODE`].
    pub mode: String,
    pub plugin: PluginRef,
    pub node_id: String,
    /// The node's launch args, configured fields included, without secrets.
    pub launch_args: Value,
    pub fields: Vec<String>,
    /// Nodes of the same plugin and scene, running now, that this change
    /// stops before the new version starts: what they hold will be free.
    #[serde(default)]
    pub releasing: Vec<ReleasingNode>,
}

/// A running node that will have stopped by the time the checked node starts.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ReleasingNode {
    pub node_id: String,
    /// Its current launch args. A run sends only the fields the plugin
    /// declares as non-secret.
    pub launch_args: Value,
}

/// Something that keeps the node from running on this machine.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PreflightProblem {
    pub message: String,
    /// It may go away by itself (a device still starting up): worth trying
    /// again. Otherwise it's permanent until someone changes something.
    #[serde(default)]
    pub retryable: bool,
}

impl PreflightProblem {
    fn permanent(message: String) -> Self {
        Self {
            message,
            retryable: false,
        }
    }
}

/// A pre-flight run's answer. No problems: the node can run.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
#[serde(deny_unknown_fields, default)]
pub struct PreflightReport {
    pub problems: Vec<PreflightProblem>,
    pub warnings: Vec<String>,
}

impl PreflightReport {
    fn failed(problem: PreflightProblem) -> Self {
        Self {
            problems: vec![problem],
            warnings: vec![],
        }
    }

    /// Whether any problem is permanent.
    pub fn is_permanent(&self) -> bool {
        self.problems.iter().any(|p| !p.retryable)
    }
}

/// Whether the plugin answers pre-flight checks.
pub fn has_preflight(manifest: &Manifest) -> bool {
    manifest
        .configurator
        .as_ref()
        .is_some_and(|c| c.preflight && !c.executable.is_empty())
}

/// Runs the pre-flight check. A plugin that doesn't declare it isn't run,
/// and reports nothing.
pub fn preflight(inv: &Invocation) -> PreflightReport {
    preflight_cancellable(inv, &AtomicBool::new(false))
}

/// [`preflight`], killing the program once `cancel` is set (the report then
/// holds a retryable problem saying so).
pub fn preflight_cancellable(inv: &Invocation, cancel: &AtomicBool) -> PreflightReport {
    preflight_with(inv, &[], cancel)
}

/// Only what the plugin declares as a non-secret setting: a running node may
/// come from another version, whose secrets this one's schema doesn't name.
fn releasing_args(spec: &LaunchSpec, args: &Value) -> Value {
    match args {
        Value::Object(map) => Value::Object(
            map.iter()
                .filter(|(k, _)| spec.field(k).is_some_and(|f| !f.secret))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        ),
        _ => Value::Object(Map::new()),
    }
}

/// [`preflight_cancellable`], telling the program which running nodes will
/// have stopped by the time this one starts (`releasing`).
pub fn preflight_with(
    inv: &Invocation,
    releasing: &[ReleasingNode],
    cancel: &AtomicBool,
) -> PreflightReport {
    let Some(configurator) = inv
        .manifest
        .configurator
        .as_ref()
        .filter(|_| has_preflight(inv.manifest))
    else {
        return PreflightReport::default();
    };
    // Trouble with the server's own temporary folder says nothing about the
    // node: worth another try.
    let transient = |message: String| {
        PreflightReport::failed(PreflightProblem {
            message,
            retryable: true,
        })
    };
    let dir = match tempfile::Builder::new()
        .prefix("flwrs-preflight-")
        .tempdir()
    {
        Ok(dir) => dir,
        Err(e) => return transient(format!("a temporary folder: {e}")),
    };
    let plan = match lay_out(inv, inv.launch_args, dir.path()) {
        Ok(plan) => plan,
        Err(errors) => {
            return PreflightReport {
                problems: errors
                    .into_iter()
                    .map(PreflightProblem::permanent)
                    .collect(),
                warnings: vec![],
            };
        }
    };
    let request = PreflightRequest {
        contract: PREFLIGHT_CONTRACT,
        mode: PREFLIGHT_MODE.to_string(),
        plugin: PluginRef {
            id: inv.manifest.id.clone(),
            version: inv.manifest.version.clone(),
        },
        node_id: inv.node_id.to_string(),
        launch_args: without_secrets(inv.spec, inv.launch_args),
        fields: configurator.fields.clone(),
        releasing: releasing
            .iter()
            .map(|r| ReleasingNode {
                node_id: r.node_id.clone(),
                launch_args: releasing_args(inv.spec, &r.launch_args),
            })
            .collect(),
    };
    let text = serde_json::to_vec_pretty(&request).expect("a request serializes");
    if let Err(e) = std::fs::write(dir.path().join(REQUEST_FILE), text) {
        return transient(format!("{REQUEST_FILE}: {e}"));
    }
    let args = [
        "--mode",
        PREFLIGHT_MODE,
        "--request",
        REQUEST_FILE,
        "--response",
        RESPONSE_FILE,
    ];
    let (status, stderr) = match run_program(
        inv,
        &configurator.executable,
        dir.path(),
        &plan,
        &args,
        Some(cancel),
    ) {
        Ok(ran) => ran,
        Err(failure @ (RunFailure::Timeout(_) | RunFailure::Cancelled)) => {
            return transient(failure.message());
        }
        Err(failure) => {
            return PreflightReport::failed(PreflightProblem::permanent(failure.message()));
        }
    };
    let response = read_response_bytes(dir.path(), inv.limits).and_then(|b| parse_preflight(&b));
    match (status.success(), response) {
        // A check that found something may well exit non-zero.
        (_, Ok(report)) if !report.problems.is_empty() => report,
        (true, Ok(report)) => report,
        (true, Err(e)) => PreflightReport::failed(PreflightProblem::permanent(format!(
            "the configurator's pre-flight answer: {e}{}",
            stderr_detail(&stderr)
        ))),
        (false, _) => PreflightReport::failed(PreflightProblem::permanent(format!(
            "the configurator failed ({status}){}",
            stderr_detail(&stderr)
        ))),
    }
}

/// Whether the plugin answers describe requests.
pub fn has_describe(manifest: &Manifest) -> bool {
    manifest
        .configurator
        .as_ref()
        .is_some_and(|c| c.describe && !c.executable.is_empty())
}

/// The node's upload settings: all a describe run may depend on.
pub fn upload_args(spec: &LaunchSpec, args: &Value) -> Value {
    match args {
        Value::Object(map) => Value::Object(
            map.iter()
                .filter(|(k, _)| spec.field(k).is_some_and(|f| f.upload && !f.secret))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        ),
        _ => Value::Object(Map::new()),
    }
}

/// Runs the describe mode on the node's uploads. `Ok(None)`: the plugin
/// doesn't declare it.
pub fn describe(inv: &Invocation) -> Result<Option<Map<String, Value>>, Vec<String>> {
    let Some(configurator) = inv
        .manifest
        .configurator
        .as_ref()
        .filter(|_| has_describe(inv.manifest))
    else {
        return Ok(None);
    };
    let dir = tempfile::Builder::new()
        .prefix("flwrs-describe-")
        .tempdir()
        .map_err(|e| vec![format!("a temporary folder: {e}")])?;
    let launch_args = upload_args(inv.spec, inv.launch_args);
    let plan = lay_out(inv, &launch_args, dir.path())?;
    let request = serde_json::json!({
        "contract": DESCRIBE_CONTRACT,
        "mode": DESCRIBE_MODE,
        "plugin": PluginRef {
            id: inv.manifest.id.clone(),
            version: inv.manifest.version.clone(),
        },
        "node_id": inv.node_id,
        "launch_args": launch_args,
        "fields": configurator.fields,
    });
    let text = serde_json::to_vec_pretty(&request).expect("a request serializes");
    std::fs::write(dir.path().join(REQUEST_FILE), text)
        .map_err(|e| vec![format!("{REQUEST_FILE}: {e}")])?;
    let args = [
        "--mode",
        DESCRIBE_MODE,
        "--request",
        REQUEST_FILE,
        "--response",
        RESPONSE_FILE,
    ];
    let (status, stderr) = run_program(
        inv,
        &configurator.executable,
        dir.path(),
        &plan,
        &args,
        None,
    )
    .map_err(|f| vec![f.message()])?;
    let response = read_response(dir.path(), inv.limits);
    match response {
        Ok(r) if !r.errors.is_empty() => Err(r.errors),
        Ok(r) if status.success() => {
            if !r.files.is_empty() {
                return Err(vec![
                    "a describe run is read-only: it can't return files".into(),
                ]);
            }
            if let Some(key) = r.values.keys().find(|k| !configurator.fields.contains(k)) {
                return Err(vec![format!(
                    "the describe answer sets {key:?}, which the configurator doesn't declare"
                )]);
            }
            Ok(Some(r.values))
        }
        Ok(_) => Err(vec![format!(
            "the configurator failed ({status}){}",
            stderr_detail(&stderr)
        )]),
        Err(e) => Err(vec![format!(
            "the describe answer: {e}{}",
            stderr_detail(&stderr)
        )]),
    }
}

/// A pre-flight answer. It's read-only: values or files are refused.
fn parse_preflight(bytes: &[u8]) -> Result<PreflightReport, String> {
    let mut value: Value =
        serde_json::from_slice(bytes).map_err(|e| format!("{RESPONSE_FILE}: {e}"))?;
    let map = value
        .as_object_mut()
        .ok_or_else(|| format!("{RESPONSE_FILE} isn't a JSON object"))?;
    for key in ["values", "files"] {
        let empty = match map.remove(key) {
            None | Some(Value::Null) => true,
            Some(Value::Object(o)) => o.is_empty(),
            Some(_) => false,
        };
        if !empty {
            return Err(format!(
                "a pre-flight run is read-only: its response can't carry {key:?}"
            ));
        }
    }
    let report: PreflightReport =
        serde_json::from_value(value).map_err(|e| format!("{RESPONSE_FILE}: {e}"))?;
    if report.problems.iter().any(|p| p.message.trim().is_empty()) {
        return Err(format!("{RESPONSE_FILE}: a problem has an empty message"));
    }
    Ok(report)
}

fn read_response(dir: &Path, limits: &Limits) -> Result<Response, String> {
    let text = read_response_bytes(dir, limits)?;
    serde_json::from_slice(&text).map_err(|e| format!("{RESPONSE_FILE}: {e}"))
}

fn read_response_bytes(dir: &Path, limits: &Limits) -> Result<Vec<u8>, String> {
    let path = dir.join(RESPONSE_FILE);
    let size = std::fs::metadata(&path)
        .map_err(|_| format!("the configurator wrote no {RESPONSE_FILE}"))?
        .len();
    if size > limits.max_response_bytes {
        return Err(format!(
            "{RESPONSE_FILE} is {size} bytes, more than the limit of {}",
            limits.max_response_bytes
        ));
    }
    std::fs::read(&path).map_err(|e| format!("{RESPONSE_FILE}: {e}"))
}

fn without_secrets(spec: &LaunchSpec, args: &Value) -> Value {
    match args {
        Value::Object(map) => Value::Object(
            map.iter()
                .filter(|(k, _)| !spec.field(k).is_some_and(|f| f.secret))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// A returned file's path: relative, inside the folder, a regular file.
fn returned_file(dir: &Path, path: &str, limits: &Limits) -> Result<PathBuf, String> {
    let relative = Path::new(path);
    if relative.is_absolute()
        || relative
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err(format!(
            "{path:?} must be a relative path inside the working folder"
        ));
    }
    let full = dir.join(relative);
    let meta = std::fs::symlink_metadata(&full).map_err(|_| format!("{path:?} doesn't exist"))?;
    if !meta.is_file() {
        return Err(format!("{path:?} isn't a regular file"));
    }
    if meta.len() > limits.max_file_bytes {
        return Err(format!(
            "{path:?} is larger than {} bytes",
            limits.max_file_bytes
        ));
    }
    Ok(full)
}

fn check(
    inv: &Invocation,
    dir: tempfile::TempDir,
    fields: &[String],
    base: Value,
    response: Response,
) -> Result<Configured, Vec<String>> {
    let mut problems = vec![];
    let mut values = Map::new();
    for (field, value) in response.values {
        if !fields.contains(&field) {
            problems.push(format!(
                "the configurator set {field:?}, which isn't one of its fields"
            ));
        } else if inv.spec.field(&field).is_some_and(|f| f.upload) {
            problems.push(format!(
                "{field:?} is a file: the configurator must return it under \"files\""
            ));
        } else {
            values.insert(field, value);
        }
    }
    let mut files = vec![];
    for (field, paths) in response.files {
        let spec_field = inv.spec.field(&field);
        let upload = spec_field.is_some_and(|f| f.upload && f.via == Via::File);
        let list_field = spec_field.is_some_and(|f| f.upload_list);
        if !fields.contains(&field) {
            problems.push(format!(
                "the configurator returned a file for {field:?}, which isn't one of its fields"
            ));
            continue;
        }
        if !upload {
            problems.push(format!(
                "{field:?} isn't an uploaded-file field: return its value under \"values\""
            ));
            continue;
        }
        if values.contains_key(&field) {
            problems.push(format!("{field:?} has both a value and a file"));
            continue;
        }
        let (list, paths) = paths.into_parts();
        if list != list_field {
            problems.push(if list_field {
                format!("{field:?} is a list of files: return a list of paths")
            } else {
                format!("{field:?} is one file: return one path, not a list")
            });
            continue;
        }
        let mut names = vec![];
        for path in paths {
            match returned_file(dir.path(), &path, inv.limits) {
                Err(e) => problems.push(format!("files.{field}: {e}")),
                Ok(full) => {
                    let name = full
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    if let Err(e) = check_file_name(&name) {
                        problems.push(format!("files.{field}: the name {name:?}: {e}"));
                        continue;
                    }
                    if names.contains(&Value::String(name.clone())) {
                        problems.push(format!("files.{field}: two files named {name:?}"));
                        continue;
                    }
                    names.push(Value::String(name.clone()));
                    files.push(ConfiguredFile {
                        field: field.clone(),
                        name,
                        path: full,
                    });
                }
            }
        }
        let value = if list {
            Value::Array(names)
        } else {
            names.pop().unwrap_or(Value::Null)
        };
        if !value.is_null() {
            values.insert(field, value);
        }
    }
    let mut launch_args = match base {
        Value::Object(map) => map,
        _ => Map::new(),
    };
    for (k, v) in &values {
        launch_args.insert(k.clone(), v.clone());
    }
    let launch_args = Value::Object(launch_args);
    if problems.is_empty() {
        problems.extend(
            validate_launch_args(inv.schema, inv.spec, &launch_args)
                .into_iter()
                .map(|e| format!("the configured launch args: {e}")),
        );
    }
    if !problems.is_empty() {
        return Err(problems);
    }
    Ok(Configured {
        values,
        files,
        warnings: response.warnings,
        launch_args,
        _dir: dir,
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::package::LaunchSpec;
    use serde_json::json;
    use std::os::unix::fs::PermissionsExt;

    /// A package folder with a shell-script configurator and a manifest.
    struct Fixture {
        dir: tempfile::TempDir,
        manifest: Manifest,
        schema: Value,
    }

    impl Fixture {
        fn new(script: &str) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let bin = dir.path().join("bin");
            std::fs::create_dir(&bin).unwrap();
            let path = bin.join("configure");
            std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            let manifest: Manifest = serde_json::from_value(json!({
                "manifest_version": 2, "id": "p", "name": "P", "version": "1.0.0", "type": "SOURCE",
                "executable": "bin/run", "launch_args_schema": "schema/launch_args.json",
                "payload_schemas": {"out": {"from_launch_args": "/out_schema"}},
                "configurator": {"executable": "bin/configure", "fields": ["out_schema", "descriptor", "protos"]}
            }))
            .unwrap();
            let schema = json!({
                "type": "object", "additionalProperties": false,
                "properties": {
                    "spec": {"type": "string"},
                    "token": {"type": "string", "writeOnly": true, "x-flwrs-launch": {"via": "env", "alias": "TOKEN"}},
                    "doc": {"type": "string", "x-flwrs-launch": {"via": "file", "source": "upload"}},
                    "out_schema": {"type": "object"},
                    "descriptor": {"type": "string", "x-flwrs-launch": {"via": "file", "source": "upload"}},
                    "protos": {"type": "array", "items": {"type": "string"},
                               "x-flwrs-launch": {"via": "file", "source": "upload"}}
                }
            });
            Self {
                dir,
                manifest,
                schema,
            }
        }

        fn run(&self, args: Value) -> Result<Configured, Vec<String>> {
            let (spec, report) = LaunchSpec::new(&self.manifest, Some(&self.schema));
            assert!(report.errors.is_empty(), "{:?}", report.errors);
            let validator = jsonschema::validator_for(&self.schema).unwrap();
            let upload = self.dir.path().join("stored-doc");
            std::fs::write(&upload, "openapi: 3.1.0").unwrap();
            let uploads = move |name: &str| (name == "doc.yaml").then(|| upload.clone());
            let limits = Limits {
                timeout: Duration::from_secs(5),
                ..Limits::default()
            };
            configure(&Invocation {
                package_dir: self.dir.path(),
                executable: None,
                manifest: &self.manifest,
                spec: &spec,
                schema: Some(&validator),
                launch_args: &args,
                uploads: &uploads,
                node_id: "n1",
                limits: &limits,
            })
        }
    }

    #[test]
    fn a_configurator_sees_the_settings_and_sets_its_fields() {
        // Echoes what it saw into the out schema's description, and writes a file.
        let f = Fixture::new(
            r#"[ "$1" = --request ] && [ "$3" = --response ] || exit 9
               doc=$(cat config/doc.yaml)
               fields=$(grep -c '"out_schema"' request.json)
               secret=$(grep -c 'hunter2' request.json)
               printf 'bin' > descriptor.pb
               printf '{"values": {"out_schema": {"fields": [], "description": "%s|%s|%s|%s"}}, "files": {"descriptor": "descriptor.pb"}, "warnings": ["w"]}' \
                 "$doc" "$TOKEN" "$fields" "$secret" > response.json"#,
        );
        let c = f
            .run(json!({"spec": "x", "token": "hunter2", "doc": "doc.yaml", "out_schema": {"stale": true}}))
            .unwrap();
        // The upload in config/, the secret from the environment (not in
        // request.json), the requested fields listed.
        assert_eq!(
            c.values["out_schema"]["description"],
            "openapi: 3.1.0|hunter2|1|0"
        );
        assert_eq!(c.values["descriptor"], "descriptor.pb");
        assert_eq!(c.files.len(), 1);
        assert_eq!(std::fs::read(&c.files[0].path).unwrap(), b"bin");
        assert_eq!(c.warnings, ["w"]);
        assert_eq!(c.launch_args["spec"], "x");
        assert!(c.launch_args["out_schema"].get("stale").is_none());
    }

    #[test]
    fn a_list_field_gets_a_list_of_files() {
        let f = Fixture::new(
            r#"mkdir -p a b && printf 1 > a/one.proto && printf 2 > b/two.proto
               echo '{"files": {"protos": ["a/one.proto", "b/two.proto"]}}' > response.json"#,
        );
        let c = f.run(json!({"spec": "x"})).unwrap();
        assert_eq!(c.values["protos"], json!(["one.proto", "two.proto"]));
        let names: Vec<_> = c
            .files
            .iter()
            .map(|f| (f.field.as_str(), f.name.as_str()))
            .collect();
        assert_eq!(names, [("protos", "one.proto"), ("protos", "two.proto")]);
        assert_eq!(c.launch_args["protos"], json!(["one.proto", "two.proto"]));
        // The field's shape decides: a list for a list field, one path otherwise;
        // names are unique.
        let errors = |script: &str| Fixture::new(script).run(json!({"spec": "x"})).unwrap_err();
        let e = errors(r#"printf 1 > a; echo '{"files": {"protos": "a"}}' > response.json"#);
        assert!(e[0].contains("a list of files"), "{e:?}");
        let e = errors(r#"printf 1 > a; echo '{"files": {"descriptor": ["a"]}}' > response.json"#);
        assert!(e[0].contains("one file"), "{e:?}");
        let e = errors(
            r#"mkdir -p a b && printf 1 > a/x && printf 2 > b/x
               echo '{"files": {"protos": ["a/x", "b/x"]}}' > response.json"#,
        );
        assert!(e[0].contains("two files named"), "{e:?}");
    }

    #[test]
    fn failures_and_wrong_answers_are_refused() {
        let errors = |script: &str| Fixture::new(script).run(json!({"spec": "x"})).unwrap_err();
        let e = errors(r#"echo '{"errors": ["the spec has no paths"]}' > response.json; exit 1"#);
        assert_eq!(e, ["the spec has no paths"]);
        let e = errors("echo boom >&2; exit 3");
        assert!(e[0].contains("failed") && e[0].contains("boom"), "{e:?}");
        let e = errors("exit 0");
        assert!(e[0].contains("no response.json"), "{e:?}");
        let e = errors(r#"echo '{"values": {"spec": "y"}}' > response.json"#);
        assert!(e[0].contains("isn't one of its fields"), "{e:?}");
        let e = errors(r#"echo '{"values": {"out_schema": 5}}' > response.json"#);
        assert!(e[0].contains("configured launch args"), "{e:?}");
        let e = errors(r#"echo '{"files": {"descriptor": "../x"}}' > response.json"#);
        assert!(e[0].contains("inside the working folder"), "{e:?}");
        let e = errors(r#"echo '{"values": {"descriptor": "a"}}' > response.json"#);
        assert!(e[0].contains("must return it under"), "{e:?}");
        let e = errors(r#"echo '{"bogus": 1}' > response.json"#);
        assert!(e[0].contains("unknown field"), "{e:?}");
    }

    #[test]
    fn the_remote_form_inlines_files_and_keeps_secrets_out() {
        use base64::Engine;
        let f = Fixture::new("exit 0");
        let (spec, _) = LaunchSpec::new(&f.manifest, Some(&f.schema));
        let validator = jsonschema::validator_for(&f.schema).unwrap();
        let upload = f.dir.path().join("stored-doc");
        std::fs::write(&upload, "openapi: 3.1.0").unwrap();
        let uploads = move |name: &str| (name == "doc.yaml").then(|| upload.clone());
        let limits = Limits::default();
        let args =
            json!({"spec": "x", "token": "hunter2", "doc": "doc.yaml", "out_schema": {"stale": 1}});
        let inv = Invocation {
            package_dir: f.dir.path(),
            executable: None,
            manifest: &f.manifest,
            spec: &spec,
            schema: Some(&validator),
            launch_args: &args,
            uploads: &uploads,
            node_id: "n1",
            limits: &limits,
        };
        let request = remote_request(&inv, false).unwrap();
        assert_eq!(request.launch_args, json!({"spec": "x", "doc": "doc.yaml"}));
        let doc = base64::engine::general_purpose::STANDARD
            .decode(&request.files["doc.yaml"])
            .unwrap();
        assert_eq!(doc, b"openapi: 3.1.0");
        assert_eq!(request.fields, ["out_schema", "descriptor", "protos"]);
        // Opted in: the secret goes too.
        assert_eq!(
            remote_request(&inv, true).unwrap().launch_args["token"],
            "hunter2"
        );

        let response: RemoteResponse = serde_json::from_value(json!({
            "values": {"out_schema": {"fields": []}},
            "files": {"descriptor": {"name": "api.pb", "url": "https://configure.example/files/api.pb"}},
            "warnings": ["w"]
        }))
        .unwrap();
        let download = |url: &str, dest: &Path, max: u64| -> Result<(), String> {
            assert_eq!(max, limits.max_file_bytes);
            match url {
                "https://configure.example/files/api.pb" => {
                    std::fs::write(dest, b"bin").map_err(|e| e.to_string())
                }
                _ => Err("404 Not Found".into()),
            }
        };
        let c = remote_result(&inv, response, &download).unwrap();
        assert_eq!(c.values["descriptor"], "api.pb");
        assert_eq!(std::fs::read(&c.files[0].path).unwrap(), b"bin");
        assert_eq!(c.warnings, ["w"]);
        let missing: RemoteResponse = serde_json::from_value(
            json!({"files": {"descriptor": {"name": "a.pb", "url": "https://elsewhere/a.pb"}}}),
        )
        .unwrap();
        assert!(remote_result(&inv, missing, &download).unwrap_err()[0].contains("404"));
        let bad: RemoteResponse =
            serde_json::from_value(json!({"files": {"descriptor": {"name": "../x", "url": "u"}}}))
                .unwrap();
        assert!(remote_result(&inv, bad, &download).unwrap_err()[0].contains("the name"));
        let failed: RemoteResponse = serde_json::from_value(json!({"errors": ["no"]})).unwrap();
        assert_eq!(remote_result(&inv, failed, &download).unwrap_err(), ["no"]);
    }

    #[test]
    fn a_slow_configurator_is_stopped() {
        let f = Fixture::new("sleep 30");
        let (spec, _) = LaunchSpec::new(&f.manifest, Some(&f.schema));
        let limits = Limits {
            timeout: Duration::from_millis(300),
            ..Limits::default()
        };
        let started = Instant::now();
        let e = configure(&Invocation {
            package_dir: f.dir.path(),
            executable: None,
            manifest: &f.manifest,
            spec: &spec,
            schema: None,
            launch_args: &json!({}),
            uploads: &|_| None,
            node_id: "n",
            limits: &limits,
        })
        .unwrap_err();
        assert!(e[0].contains("didn't finish"), "{e:?}");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    /// Runs the fixture's script in pre-flight mode (declared in its manifest).
    fn preflight_of(script: &str, args: Value, timeout: Duration) -> PreflightReport {
        let mut f = Fixture::new(script);
        f.manifest.configurator.as_mut().unwrap().preflight = true;
        let (spec, _) = LaunchSpec::new(&f.manifest, Some(&f.schema));
        let limits = Limits {
            timeout,
            ..Limits::default()
        };
        preflight(&Invocation {
            package_dir: f.dir.path(),
            executable: None,
            manifest: &f.manifest,
            spec: &spec,
            schema: None,
            launch_args: &args,
            uploads: &|_| None,
            node_id: "n1",
            limits: &limits,
        })
    }

    const T: Duration = Duration::from_secs(5);

    #[test]
    fn preflight_gets_its_mode_and_the_whole_settings() {
        let report = preflight_of(
            r#"[ "$1" = --mode ] && [ "$2" = preflight ] && [ "$3" = --request ] || exit 9
               grep -q '"contract": 2' request.json || exit 8
               grep -q '"mode": "preflight"' request.json || exit 7
               grep -q '"out_schema"' request.json || exit 6
               grep -q hunter2 request.json && exit 5
               [ "$TOKEN" = hunter2 ] || exit 4
               echo '{"problems": [{"message": "no camera", "retryable": true}, {"message": "no GPU"}], "warnings": ["slow"]}' > response.json"#,
            json!({"spec": "x", "token": "hunter2", "out_schema": {"fields": []}}),
            T,
        );
        assert_eq!(report.warnings, ["slow"]);
        assert_eq!(report.problems.len(), 2, "{report:?}");
        assert!(report.problems[0].retryable);
        assert!(!report.problems[1].retryable);
        assert!(report.is_permanent());
    }

    fn describe_of(
        script: &str,
        declared: bool,
        args: Value,
    ) -> Result<Option<Map<String, Value>>, Vec<String>> {
        let mut f = Fixture::new(script);
        f.manifest.configurator.as_mut().unwrap().describe = declared;
        let (spec, _) = LaunchSpec::new(&f.manifest, Some(&f.schema));
        let stored = f.dir.path().join("stored-doc");
        std::fs::write(&stored, "x").unwrap();
        let uploads = move |name: &str| (name == "doc.yaml").then(|| stored.clone());
        let limits = Limits {
            timeout: T,
            ..Limits::default()
        };
        describe(&Invocation {
            package_dir: f.dir.path(),
            executable: None,
            manifest: &f.manifest,
            spec: &spec,
            schema: None,
            launch_args: &args,
            uploads: &uploads,
            node_id: "n1",
            limits: &limits,
        })
    }

    #[test]
    fn describe_sees_only_uploads_and_returns_declared_fields() {
        let values = describe_of(
            r#"[ "$1" = --mode ] && [ "$2" = describe ] || exit 9
               grep -q '"contract": 3' request.json || exit 8
               grep -q '"doc": "doc.yaml"' request.json || exit 7
               grep -q incomplete request.json && exit 6
               [ -f config/doc.yaml ] || exit 5
               echo '{"values": {"out_schema": {"fields": []}}}' > response.json"#,
            true,
            json!({"doc": "doc.yaml", "spec": "incomplete"}),
        )
        .unwrap()
        .unwrap();
        assert_eq!(values["out_schema"], json!({"fields": []}));
        // Not declared: not run.
        assert_eq!(describe_of("exit 1", false, json!({})).unwrap(), None);
        // What it may not answer.
        for (script, expected) in [
            (
                r#"echo '{"values": {"other": 1}}' > response.json"#,
                "doesn't declare",
            ),
            (
                r#"echo '{"files": {"doc": "x"}}' > response.json"#,
                "read-only",
            ),
            (
                r#"echo '{"errors": ["bad file"]}' > response.json"#,
                "bad file",
            ),
            ("exit 3", "wrote no"),
        ] {
            let errors = describe_of(script, true, json!({})).unwrap_err();
            assert!(
                errors.iter().any(|e| e.contains(expected)),
                "{expected}: {errors:?}"
            );
        }
    }

    #[test]
    fn preflight_failures_are_problems() {
        let one = |script: &str| {
            let r = preflight_of(script, json!({"spec": "x"}), T);
            assert_eq!(r.problems.len(), 1, "{r:?}");
            r.problems.into_iter().next().unwrap()
        };
        assert!(
            preflight_of("echo '{}' > response.json", json!({}), T)
                .problems
                .is_empty()
        );
        // A check may exit non-zero with its findings.
        let p = one(
            r#"echo '{"problems": [{"message": "busy", "retryable": true}]}' > response.json; exit 1"#,
        );
        assert_eq!((p.message.as_str(), p.retryable), ("busy", true));
        // A crash carries its stderr.
        let p = one("echo 'GLIBC_2.38 not found' >&2; exit 127");
        assert!(p.message.contains("GLIBC_2.38") && !p.retryable, "{p:?}");
        let p = one("exit 0");
        assert!(
            p.message.contains("no response.json") && !p.retryable,
            "{p:?}"
        );
        let p = one("echo nonsense > response.json");
        assert!(!p.retryable, "{p:?}");
        // Read-only: values and files are refused.
        let p = one(r#"echo '{"values": {"out_schema": {}}}' > response.json"#);
        assert!(p.message.contains("read-only") && !p.retryable, "{p:?}");
        let p = one(r#"echo '{"files": {"descriptor": "a"}}' > response.json"#);
        assert!(p.message.contains("read-only"), "{p:?}");
        // A timeout is the one failure worth retrying.
        let r = preflight_of("exec sleep 30", json!({}), Duration::from_millis(200));
        assert!(r.problems[0].retryable, "{r:?}");
        assert!(r.problems[0].message.contains("didn't finish"), "{r:?}");
    }

    #[test]
    fn a_program_that_cant_start_is_a_permanent_problem() {
        let mut f = Fixture::new("exit 0");
        let c = f.manifest.configurator.as_mut().unwrap();
        c.preflight = true;
        c.executable = "bin/missing".into();
        let (spec, _) = LaunchSpec::new(&f.manifest, Some(&f.schema));
        let r = preflight(&Invocation {
            package_dir: f.dir.path(),
            executable: None,
            manifest: &f.manifest,
            spec: &spec,
            schema: None,
            launch_args: &json!({}),
            uploads: &|_| None,
            node_id: "n",
            limits: &Limits::default(),
        });
        assert!(r.is_permanent(), "{r:?}");
        assert!(
            r.problems[0].message.contains("starting the configurator"),
            "{r:?}"
        );
    }

    #[test]
    fn preflight_only_runs_when_declared_and_can_be_cancelled() {
        // Not declared: not run at all.
        let f = Fixture::new("touch ran; exit 1");
        let (spec, _) = LaunchSpec::new(&f.manifest, Some(&f.schema));
        let inv = Invocation {
            package_dir: f.dir.path(),
            executable: None,
            manifest: &f.manifest,
            spec: &spec,
            schema: None,
            launch_args: &json!({}),
            uploads: &|_| None,
            node_id: "n",
            limits: &Limits::default(),
        };
        assert_eq!(preflight(&inv), PreflightReport::default());

        let mut f = Fixture::new("exec sleep 30");
        f.manifest.configurator.as_mut().unwrap().preflight = true;
        let (spec, _) = LaunchSpec::new(&f.manifest, Some(&f.schema));
        let cancel = AtomicBool::new(true);
        let started = Instant::now();
        let r = preflight_cancellable(
            &Invocation {
                package_dir: f.dir.path(),
                executable: None,
                manifest: &f.manifest,
                spec: &spec,
                schema: None,
                launch_args: &json!({}),
                uploads: &|_| None,
                node_id: "n",
                limits: &Limits::default(),
            },
            &cancel,
        );
        assert!(r.problems[0].retryable, "{r:?}");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn the_manifest_and_schema_rules() {
        let f = Fixture::new("exit 0");
        let mut m = f.manifest.clone();
        m.configurator.as_mut().unwrap().fields.push("token".into());
        m.configurator.as_mut().unwrap().fields.push("nope".into());
        let (_, report) = LaunchSpec::new(&m, Some(&f.schema));
        assert!(
            report
                .errors
                .iter()
                .any(|e| e.contains("\"token\" is a secret")),
            "{:?}",
            report.errors
        );
        assert!(
            report
                .errors
                .iter()
                .any(|e| e.contains("\"nope\" isn't a top-level property"))
        );
        let mut m = f.manifest.clone();
        m.manifest_version = 1;
        assert!(
            m.validate(&crate::package::current_platform())
                .iter()
                .any(|e| e.contains("configurator needs manifest_version 2"))
        );
        assert!(
            f.manifest
                .file_references()
                .contains(&("configurator.executable", "bin/configure"))
        );
    }
}
