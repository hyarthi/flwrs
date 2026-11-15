//! The plugin under check: its executable, its manifest and how to start it.

use flwrs_plugin::package::{
    self, LaunchArgsStyle, Limits, MANIFEST_FILE, Manifest, current_platform, launch_args_to_argv,
    validate_launch_args,
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
    /// Working directory: the package's folder, as on the server.
    pub cwd: PathBuf,
    pub manifest: Manifest,
    pub plugin_type: PluginType,
    pub version: String,
    /// From the manifest: what the plugin must declare.
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
    /// Keeps an unpacked package's temporary folder alive.
    unpacked: Option<Arc<tempfile::TempDir>>,
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
            if field != "executable" && !dir.join(path).is_file() {
                problems.push(format!("{field} {path:?} is missing"));
            }
        }
        if !problems.is_empty() {
            return Err(format!("{MANIFEST_FILE}: {}", problems.join("; ")));
        }
        let schema = |path: &Option<String>| -> Result<Option<SchemaDefinition>, String> {
            match path {
                None => Ok(None),
                Some(file) => rules::parse_json(&read(file)?)
                    .map(Some)
                    .map_err(|e| format!("{file}: {e}")),
            }
        };
        Ok(Self {
            program: dir.join(&manifest.executable),
            cwd: dir.to_path_buf(),
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
            unpacked: None,
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

    /// Sets the node's `launch_args`, checked like the server checks them
    /// (the manifest's JSON Schema and `launch_args_style`).
    pub fn with_launch_args(mut self, args: Value) -> Result<Self, String> {
        let schema = match &self.manifest.launch_args_schema {
            None => None,
            Some(file) => {
                let text = std::fs::read_to_string(self.cwd.join(file))
                    .map_err(|e| format!("{file}: {e}"))?;
                let value: Value =
                    serde_json::from_str(&text).map_err(|e| format!("{file}: {e}"))?;
                Some(
                    jsonschema::validator_for(&value)
                        .map_err(|e| format!("{file}: not a valid JSON Schema: {e}"))?,
                )
            }
        };
        let style: LaunchArgsStyle = self.manifest.launch_args_style;
        let problems = validate_launch_args(schema.as_ref(), style, &args);
        if !problems.is_empty() {
            return Err(problems.join("; "));
        }
        self.launch_argv = launch_args_to_argv(style, &args).map_err(|e| e.join("; "))?;
        self.launch_args = args;
        Ok(self)
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

fn protocol_type(t: package::PluginType) -> PluginType {
    match t {
        package::PluginType::Source => PluginType::Source,
        package::PluginType::Sink => PluginType::Sink,
        package::PluginType::Transform => PluginType::Transform,
    }
}
