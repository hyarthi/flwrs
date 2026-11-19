//! The plugin under check: its executable, its manifest and how to start it.
//!
//! It runs as on the server: in a node folder (a temporary one here) with a
//! link to each top-level entry of the package, its `launch_args` delivered as
//! arguments, environment variables and files in `config/`, and files
//! uploaded to the node linked into `config/` too.
//!
//! A **remote node** (a remote plugin definition, manifest `"launch":
//! "remote"`, or any plugin given a [`RemoteLaunch`]) runs elsewhere: its
//! launch args aren't delivered (the server never delivers a remote node's),
//! they only set the node's own payload schemas, and its configurator is its
//! endpoint. With a [`RemoteLaunch`], trellis runs its commands to make the
//! plugin start or connect; a definition can also be checked against a local
//! stand-in ([`Target::with_executable`]: the bootstrap arguments, in an
//! empty folder).

use crate::remote::RemoteLaunch;
use flwrs_plugin::package::configure::{self, Configured, Invocation};
use flwrs_plugin::package::launch::check_file_name;
use flwrs_plugin::package::{
    self, CONFIG_DIR, ConfigContent, LaunchSpec, Limits, MANIFEST_FILE, Manifest, SchemaSource,
    current_platform, validate_launch_args,
};
use flwrs_plugin::schema::payload::SchemaDefinition;
use flwrs_plugin::schema::protocol::plugin_type::Enum as PluginType;
use flwrs_plugin::schema::rules;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// A plugin to run against the fake hub.
#[derive(Debug, Clone)]
pub struct Target {
    /// The executable.
    pub program: PathBuf,
    /// The package's folder.
    pub cwd: PathBuf,
    pub manifest: Manifest,
    pub plugin_type: PluginType,
    pub version: String,
    /// What the plugin must declare: the manifest's schemas, or the node's
    /// own (from its `launch_args`, manifest version 2).
    pub in_schema: Option<SchemaDefinition>,
    pub out_schema: Option<SchemaDefinition>,
    /// The node's `launch_args` (as on the server) and the arguments they become.
    pub launch_args: Value,
    launch_argv: Vec<String>,
    /// Extra arguments after the launch args.
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    /// Show the plugin's stdout and stderr instead of discarding them.
    pub show_output: bool,
    /// Files uploaded to the node: name → file.
    uploads: Vec<(String, PathBuf)>,
    /// Keeps an unpacked package's temporary folder alive.
    unpacked: Option<Arc<tempfile::TempDir>>,
    /// Runs this instead of the manifest's configurator (e.g. cargo's build).
    configurator_program: Option<PathBuf>,
    /// The configurator's last run (its files are the node's uploads).
    configured: Option<Arc<Configured>>,
    /// The node's folder: the plugin's working directory.
    node: Arc<tempfile::TempDir>,
    /// A plugin that runs elsewhere: how it's made to connect.
    pub remote: Option<Arc<RemoteLaunch>>,
}

impl Target {
    /// A plugin package folder (unpacked, with `manifest.json` at its root),
    /// or a package ZIP (unpacked into a temporary folder after the server's
    /// upload checks).
    pub fn open(path: &Path) -> Result<Self, String> {
        if path.is_file() {
            Self::from_zip(path)
        } else {
            Self::from_package(path)
        }
    }

    /// An unpacked package folder.
    pub fn from_package(dir: &Path) -> Result<Self, String> {
        let read = |file: &str| {
            std::fs::read_to_string(dir.join(file))
                .map_err(|e| format!("{}: {e}", dir.join(file).display()))
        };
        let manifest: Manifest = serde_json::from_str(&read(MANIFEST_FILE)?)
            .map_err(|e| format!("{MANIFEST_FILE}: {e}"))?;
        let mut problems = manifest.validate(&current_platform());
        for (field, path) in manifest.file_references() {
            // Executables may come from cargo (`with_executable`, `with_configurator_executable`).
            if !matches!(field, "executable" | "configurator.executable")
                && !dir.join(path).is_file()
            {
                problems.push(format!("{field} {path:?} is missing"));
            }
        }
        if !problems.is_empty() {
            return Err(format!("{MANIFEST_FILE}: {}", problems.join("; ")));
        }
        // A per-node schema's default stands in until the launch args set one.
        let schema = |source: &Option<SchemaSource>| -> Result<Option<SchemaDefinition>, String> {
            match source.as_ref().and_then(|s| s.file()) {
                None => Ok(None),
                Some(file) => rules::parse_json(&read(file)?)
                    .map(Some)
                    .map_err(|e| format!("{file}: {e}")),
            }
        };
        let cwd = std::path::absolute(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let remote = manifest.is_remote_only();
        Ok(Self {
            // A remote plugin's comes from `with_executable`.
            program: if remote {
                PathBuf::new()
            } else {
                cwd.join(&manifest.executable)
            },
            node: Arc::new(if remote {
                tempfile::tempdir().map_err(|e| format!("temporary folder: {e}"))?
            } else {
                node_folder(&cwd)?
            }),
            cwd,
            plugin_type: protocol_type(manifest.plugin_type),
            version: manifest.version.clone(),
            in_schema: schema(&manifest.payload_schemas.input)?,
            out_schema: schema(&manifest.payload_schemas.output)?,
            manifest,
            launch_args: Value::Null,
            launch_argv: vec![],
            args: vec![],
            env: vec![],
            show_output: false,
            uploads: vec![],
            unpacked: None,
            configurator_program: None,
            configured: None,
            remote: None,
        })
    }

    /// A package ZIP: checked like an upload to the server, then unpacked
    /// into a temporary folder that lives as long as this target (and its clones).
    pub fn from_zip(zip: &Path) -> Result<Self, String> {
        let temp = tempfile::tempdir().map_err(|e| format!("temporary folder: {e}"))?;
        let dir = temp.path().join("package");
        package::extract_and_validate(
            zip,
            &dir,
            &Limits::default(),
            &current_platform(),
            &|_| vec![],
        )
        .map_err(|e| format!("{}: {e}", zip.display()))?;
        let mut target = Self::from_package(&dir)?;
        target.unpacked = Some(Arc::new(temp));
        Ok(target)
    }

    /// Runs `program` instead of the manifest's executable, e.g. the binary
    /// cargo built for an integration test (`env!("CARGO_BIN_EXE_<name>")`)
    /// with the plugin's source folder as the package folder.
    pub fn with_executable(mut self, program: impl Into<PathBuf>) -> Self {
        self.program = program.into();
        self
    }

    /// A remote plugin definition (manifest `"launch": "remote"`).
    pub fn is_remote(&self) -> bool {
        self.manifest.is_remote_only()
    }

    /// Checked as a remote node: a definition, or a plugin with a [`RemoteLaunch`].
    pub fn remote_node(&self) -> bool {
        self.is_remote() || self.remote.is_some()
    }

    /// Runs the plugin elsewhere: `remote`'s commands make it connect, and
    /// its `configurator` endpoint runs the configurator.
    pub fn with_remote(mut self, remote: RemoteLaunch) -> Self {
        self.remote = Some(Arc::new(remote));
        self
    }

    /// Started by [`RemoteLaunch::start`] commands (not a process of ours).
    pub fn launched_remotely(&self) -> bool {
        self.remote.as_ref().is_some_and(|r| !r.start.is_empty())
    }

    /// The instance id the plugin must declare.
    pub fn instance_id(&self) -> &str {
        self.remote
            .as_ref()
            .and_then(|r| r.id.as_deref())
            .unwrap_or(crate::hub::INSTANCE_ID)
    }

    fn endpoint(&self) -> Option<&crate::remote::RemoteConfigurator> {
        self.remote.as_ref().and_then(|r| r.configurator.as_ref())
    }

    /// Runs `program` as the plugin's configurator (manifest `configurator`)
    /// instead of the package's, e.g. `env!("CARGO_BIN_EXE_<name>-config")`.
    pub fn with_configurator_executable(mut self, program: impl Into<PathBuf>) -> Self {
        self.configurator_program = Some(program.into());
        self
    }

    /// The configured fields' values from the configurator's last run (in
    /// [`Target::with_launch_args`]).
    pub fn configured_values(&self) -> Option<&serde_json::Map<String, Value>> {
        self.configured.as_ref().map(|c| &c.values)
    }

    /// Runs the plugin's configurator on `args` the way the server does
    /// (`docs/plugin-spec.md`), with this target's uploads, and returns its
    /// result. Values `args` gives for configured fields are refused, as the
    /// server refuses them for a new local node.
    pub fn configure(&self, args: &Value) -> Result<Configured, Vec<String>> {
        let (schema, validator) = self.launch_schema().map_err(|e| vec![e])?;
        let (spec, report) = LaunchSpec::new(&self.manifest, schema.as_ref());
        if !report.errors.is_empty() {
            return Err(report.errors);
        }
        let given: Vec<String> = spec
            .configured_fields()
            .iter()
            .filter(|f| args.get(f.as_str()).is_some())
            .map(|f| format!("launch_args/{f} is set by the plugin's configurator; leave it out"))
            .collect();
        if !given.is_empty() {
            return Err(given);
        }
        let uploads = |name: &str| {
            self.uploads
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, p)| p.clone())
        };
        let inv = Invocation {
            package_dir: &self.cwd,
            executable: self.configurator_program.as_deref(),
            manifest: &self.manifest,
            spec: &spec,
            schema: validator.as_ref(),
            launch_args: args,
            uploads: &uploads,
            node_id: self.instance_id(),
            limits: &configure::Limits::default(),
        };
        // A remote node's configurator is its endpoint (unless a program is
        // given to run in its place).
        match self.endpoint() {
            Some(endpoint) if self.configurator_program.is_none() => {
                crate::remote::configure_remote(&inv, endpoint)
            }
            _ if self.remote_node()
                && self.configurator_program.is_none()
                && self.manifest.configurator.as_ref().is_none_or(|c| c.executable.is_empty()) =>
            {
                Err(vec![
                    "a remote node's configurator runs at its endpoint: give it ([remote.configurator] in trellis.toml, or Target::with_remote)".into(),
                ])
            }
            _ => configure::configure(&inv),
        }
    }

    /// The launch-args schema and its validator.
    fn launch_schema(&self) -> Result<(Option<Value>, Option<jsonschema::Validator>), String> {
        let schema = match &self.manifest.launch_args_schema {
            None => None,
            Some(file) => {
                let text = std::fs::read_to_string(self.cwd.join(file))
                    .map_err(|e| format!("{file}: {e}"))?;
                Some(serde_json::from_str::<Value>(&text).map_err(|e| format!("{file}: {e}"))?)
            }
        };
        let validator = match &schema {
            None => None,
            Some(value) => Some(
                jsonschema::validator_for(value)
                    .map_err(|e| format!("launch_args_schema: not a valid JSON Schema: {e}"))?,
            ),
        };
        Ok((schema, validator))
    }

    /// A file uploaded to the node as `name` (what a `"source": "upload"`
    /// launch arg names). Add uploads before [`Target::with_launch_args`].
    pub fn with_upload(mut self, name: &str, file: impl Into<PathBuf>) -> Result<Self, String> {
        check_file_name(name).map_err(|e| format!("upload name {name:?}: {e}"))?;
        let file = file.into();
        if !file.is_file() {
            return Err(format!("upload {name:?}: {} isn't a file", file.display()));
        }
        let file = std::path::absolute(&file).map_err(|e| e.to_string())?;
        self.uploads.retain(|(n, _)| n != name);
        self.uploads.push((name.to_string(), file));
        Ok(self)
    }

    /// Sets the node's `launch_args`, checked like the server checks them
    /// (the manifest's JSON Schema and launch rules), and delivered the same
    /// way: arguments, environment variables, and files in a fresh node
    /// folder's `config/`. With manifest version 2, payload schemas the
    /// plugin takes from its launch args are taken from `args`.
    pub fn with_launch_args(mut self, args: Value) -> Result<Self, String> {
        // A configurator sets its fields first, as on the server (a remote
        // definition's runs at its nodes' endpoints: only with one given here).
        let runs_configurator = self.manifest.configurator.as_ref().is_some_and(|c| {
            if self.remote_node() {
                self.endpoint().is_some() || self.configurator_program.is_some()
            } else {
                !c.executable.is_empty() || self.configurator_program.is_some()
            }
        });
        let args = if runs_configurator {
            let configured = self
                .configure(&args)
                .map_err(|e| format!("configurator: {}", e.join("; ")))?;
            for file in &configured.files {
                self.uploads.retain(|(n, _)| *n != file.name);
                self.uploads.push((file.name.clone(), file.path.clone()));
            }
            let args = configured.launch_args.clone();
            self.configured = Some(Arc::new(configured));
            args
        } else {
            args
        };
        let (schema, validator) = self.launch_schema()?;
        let (spec, report) = LaunchSpec::new(&self.manifest, schema.as_ref());
        if !report.errors.is_empty() {
            return Err(report.errors.join("; "));
        }
        // A remote node's launch args are checked only for its payload
        // schemas, as on the server.
        let remote = self.remote_node();
        if !remote {
            let problems = validate_launch_args(validator.as_ref(), &spec, &args);
            if !problems.is_empty() {
                return Err(problems.join("; "));
            }
        }
        // The node's own payload schemas.
        let (input, output) = spec.schema_pointers();
        for (pointer, slot) in [(input, &mut self.in_schema), (output, &mut self.out_schema)] {
            let Some(pointer) = pointer else { continue };
            match args.pointer(pointer).filter(|v| !v.is_null()) {
                Some(value) => {
                    let parsed = rules::parse_json(&value.to_string())
                        .map_err(|e| format!("launch_args{pointer}: not a payload schema: {e}"))?;
                    let problems = rules::check_well_formed(&parsed);
                    if !problems.is_empty() {
                        return Err(format!("launch_args{pointer}: {}", problems.join("; ")));
                    }
                    *slot = Some(parsed);
                }
                None if slot.is_some() => {} // the package's default
                None => {
                    return Err(format!(
                        "launch_args{pointer}: the node's payload schema is required"
                    ));
                }
            }
        }

        if remote {
            self.launch_args = args;
            return Ok(self);
        }
        let node = node_folder(&self.cwd)?;
        let config = node.path().join(CONFIG_DIR);
        let plan = spec.plan(&args, &config).map_err(|e| e.join("; "))?;
        if !plan.files.is_empty() {
            std::fs::create_dir(&config).map_err(|e| format!("{}: {e}", config.display()))?;
        }
        for file in &plan.files {
            let path = config.join(&file.name);
            match &file.content {
                ConfigContent::Data(bytes) => std::fs::write(&path, bytes)
                    .map_err(|e| format!("config/{}: {e}", file.name))?,
                ConfigContent::Upload(name) => {
                    let (_, source) = self.uploads.iter().find(|(n, _)| n == name).ok_or_else(|| {
                        format!(
                            "launch_args name the uploaded file {name:?}; add it first (Target::with_upload, `[uploads]` in trellis.toml, `uploads` in a test file, or --upload)"
                        )
                    })?;
                    link(source, &path, false).map_err(|e| format!("config/{}: {e}", file.name))?;
                }
            }
        }
        self.env
            .retain(|(k, _)| !plan.env.iter().any(|(n, _)| n == k));
        self.env.extend(plan.env);
        self.launch_argv = plan.argv;
        self.launch_args = args;
        self.node = Arc::new(node);
        Ok(self)
    }

    /// The plugin's working directory: the node's folder.
    pub fn workdir(&self) -> &Path {
        self.node.path()
    }

    /// Everything after the bootstrap arguments.
    pub(crate) fn argv(&self) -> impl Iterator<Item = &String> {
        self.launch_argv.iter().chain(&self.args)
    }

    pub(crate) fn consumes(&self) -> bool {
        self.plugin_type != PluginType::Source
    }

    pub(crate) fn produces(&self) -> bool {
        self.plugin_type != PluginType::Sink
    }
}

/// A fresh node folder: a link to each top-level entry of `package`.
fn node_folder(package: &Path) -> Result<tempfile::TempDir, String> {
    let node = tempfile::tempdir().map_err(|e| format!("temporary folder: {e}"))?;
    let entries = std::fs::read_dir(package).map_err(|e| format!("{}: {e}", package.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| e.to_string())?;
        let name = entry.file_name();
        if name == CONFIG_DIR {
            continue;
        }
        let is_dir = entry.file_type().map_err(|e| e.to_string())?.is_dir();
        link(&entry.path(), &node.path().join(&name), is_dir)
            .map_err(|e| format!("linking {}: {e}", entry.path().display()))?;
    }
    Ok(node)
}

#[cfg(unix)]
fn link(target: &Path, path: &Path, _is_dir: bool) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, path)
}

/// Elsewhere: symbolic links if allowed, else hard links (files) or copies.
#[cfg(not(unix))]
fn link(target: &Path, path: &Path, is_dir: bool) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        let linked = if is_dir {
            std::os::windows::fs::symlink_dir(target, path)
        } else {
            std::os::windows::fs::symlink_file(target, path)
        };
        if linked.is_ok() {
            return Ok(());
        }
    }
    if is_dir {
        copy_dir(target, path)
    } else {
        std::fs::hard_link(target, path).or_else(|_| std::fs::copy(target, path).map(|_| ()))
    }
}

#[cfg(not(unix))]
fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let dest = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &dest)?;
        } else {
            std::fs::copy(entry.path(), dest)?;
        }
    }
    Ok(())
}

fn protocol_type(t: package::PluginType) -> PluginType {
    match t {
        package::PluginType::Source => PluginType::Source,
        package::PluginType::Sink => PluginType::Sink,
        package::PluginType::Transform => PluginType::Transform,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A manifest v2 transform package folder.
    fn package() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let write = |path: &str, text: &str| {
            let p = dir.path().join(path);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, text).unwrap();
        };
        write(
            "manifest.json",
            r#"{"manifest_version": 2, "id": "script", "name": "Script", "version": "0.1.0",
                "type": "TRANSFORM", "executable": "bin/run",
                "launch_args_schema": "schema/launch_args.json",
                "payload_schemas": {"in": {"from_launch_args": "/schemas/in"},
                                    "out": {"from_launch_args": "/schemas/out", "default": "schema/out.json"}}}"#,
        );
        write(
            "schema/launch_args.json",
            r#"{"type": "object", "properties": {
                "schemas": {"type": "object", "properties": {"in": {"type": "object"}, "out": {"type": "object"}}},
                "script": {"type": "string", "x-flwrs-launch": {"via": "file"}},
                "model": {"type": "string", "x-flwrs-launch": {"via": "file", "source": "upload"}},
                "key": {"type": "string", "writeOnly": true, "x-flwrs-launch": {"via": "env", "alias": "SCRIPT_KEY"}}}}"#,
        );
        write(
            "schema/out.json",
            r#"{"fields": [{"key": "y", "type": "STRING"}]}"#,
        );
        write("bin/run", "#!/bin/sh\n");
        dir
    }

    #[test]
    fn runs_like_the_server() {
        let dir = package();
        let model = dir.path().join("weights.bin");
        std::fs::write(&model, "weights").unwrap();
        let target = Target::from_package(dir.path())
            .unwrap()
            .with_upload("model.onnx", &model)
            .unwrap()
            .with_launch_args(json!({
                "schemas": {"in": {"fields": [{"key": "x", "type": "U64"}]}},
                "script": "return payload",
                "model": "model.onnx",
                "key": "secret"
            }))
            .unwrap();
        // the node's own in schema; the package's default out schema
        assert_eq!(target.in_schema.as_ref().unwrap().fields[0].key, "x");
        assert_eq!(target.out_schema.as_ref().unwrap().fields[0].key, "y");
        // a node folder with the package linked in and config/
        let work = target.workdir();
        assert!(work.join("bin/run").is_file());
        assert_eq!(
            std::fs::read_to_string(work.join("config/script")).unwrap(),
            "return payload"
        );
        assert_eq!(
            std::fs::read_to_string(work.join("config/model.onnx")).unwrap(),
            "weights"
        );
        assert!(target.argv().any(|a| a == "--model"));
        assert!(target.env.contains(&("SCRIPT_KEY".into(), "secret".into())));
    }

    #[test]
    fn refuses_what_the_server_refuses() {
        let dir = package();
        let target = Target::from_package(dir.path()).unwrap();
        let e = target
            .clone()
            .with_launch_args(json!({"script": "x"}))
            .unwrap_err();
        assert!(
            e.contains("launch_args/schemas/in: the node's payload schema is required"),
            "{e}"
        );
        let e = target
            .clone()
            .with_launch_args(json!({"schemas": {"in": {"fields": []}}, "model": "m.onnx"}))
            .unwrap_err();
        assert!(e.contains("add it first"), "{e}");
        assert!(
            target
                .with_upload(".x", dir.path().join("bin/run"))
                .is_err()
        );
    }
}
