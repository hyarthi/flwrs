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

use super::launch::{ConfigContent, LaunchSpec, Via, check_file_name, validate_launch_args};
use super::manifest::Manifest;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// The contract version this module speaks.
pub const CONTRACT: u32 = 1;
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
    let config_dir = dir.path().join(super::launch::CONFIG_DIR);
    std::fs::create_dir(&config_dir).map_err(|e| vec![format!("{}: {e}", config_dir.display())])?;

    // config/ and the environment, as the plugin would get them.
    let plan = inv.spec.plan(&base, &config_dir)?;
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

    let executable = match inv.executable {
        Some(program) => program.to_path_buf(),
        None => inv.package_dir.join(&configurator.executable),
    };
    // It runs in the temporary folder: a relative path (e.g. a server whose
    // plugins folder is configured relative to its working directory) would
    // be looked up there.
    let executable = std::path::absolute(&executable).unwrap_or(executable);
    let mut command = Command::new(&executable);
    command
        .args(["--request", REQUEST_FILE, "--response", RESPONSE_FILE])
        .current_dir(dir.path())
        .env_clear()
        .envs(plan.env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .env("TMPDIR", dir.path())
        .env("TEMP", dir.path())
        .env("TMP", dir.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    for name in ["PATH", "SystemRoot", "SYSTEMROOT", "windir"] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    let mut child = command.spawn().map_err(|e| {
        vec![format!(
            "starting the configurator {}: {e}",
            executable.display()
        )]
    })?;
    // Read stderr meanwhile (a full pipe would block the child); keep its end.
    let mut stderr = child.stderr.take().expect("piped");
    let reader = std::thread::spawn(move || {
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
        String::from_utf8_lossy(&tail).trim().to_string()
    });
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() >= inv.limits.timeout => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(vec![format!(
                    "the configurator didn't finish within {} s",
                    inv.limits.timeout.as_secs_f64()
                )]);
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(e) => return Err(vec![format!("waiting for the configurator: {e}")]),
        }
    };
    let stderr = reader.join().unwrap_or_default();
    let response = read_response(dir.path(), inv.limits);
    let response = match (status.success(), response) {
        (true, Ok(r)) => r,
        (_, Ok(r)) if !r.errors.is_empty() => return Err(r.errors),
        (true, Err(e)) => return Err(vec![e]),
        (false, _) => {
            let detail = if stderr.is_empty() {
                String::new()
            } else {
                format!(": {stderr}")
            };
            return Err(vec![format!("the configurator failed ({status}){detail}")]);
        }
    };
    if !response.errors.is_empty() {
        return Err(response.errors);
    }
    check(inv, dir, &fields, base, response)
}

fn read_response(dir: &Path, limits: &Limits) -> Result<Response, String> {
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
    let text = std::fs::read(&path).map_err(|e| format!("{RESPONSE_FILE}: {e}"))?;
    serde_json::from_slice(&text).map_err(|e| format!("{RESPONSE_FILE}: {e}"))
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
