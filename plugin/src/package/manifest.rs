//! Plugin manifest (`manifest.json` at the root of a plugin package) and the
//! rules for a node's `launch_args`, which the manifest defines.
//!
//! The format is specified in `docs/plugin-spec.md` in the repository. The
//! server and `flwrs-trellis` share this code, so a package that passes
//! `flwrs-trellis package` passes the server's upload checks too.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};
use std::path::{Component, Path};

pub const MANIFEST_FILE: &str = "manifest.json";
/// The newest manifest version; every version from 1 up to it is read.
pub const MANIFEST_VERSION: u32 = 2;
/// A package can't have a top-level entry with this name: it's the node
/// folder's `config/` (the files the server provides to a node).
pub const RESERVED_ENTRY: &str = "config";

/// Hub bootstrap arguments; a node's `launch_args` can't override them.
pub const RESERVED_ARGS: [&str; 5] = crate::plugin::core::BOOTSTRAP_ARGS;

#[derive(Deserialize, Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "UPPERCASE")]
pub enum PluginType {
    Source,
    Sink,
    Transform,
}

impl PluginType {
    pub fn as_str(&self) -> &'static str {
        match self {
            PluginType::Source => "SOURCE",
            PluginType::Sink => "SINK",
            PluginType::Transform => "TRANSFORM",
        }
    }
}

/// How a node's `launch_args` JSON becomes command-line arguments.
#[derive(Deserialize, Serialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LaunchArgsStyle {
    /// `{"http_timeout_seconds": 60, "verbose": true}` → `--http-timeout-seconds 60 --verbose`.
    #[default]
    Flags,
    /// `["a", "b"]` → `a b`, verbatim.
    Positional,
}

/// Where a payload schema comes from.
#[derive(Deserialize, Serialize, Clone, Debug, PartialEq, Eq)]
#[serde(untagged)]
pub enum SchemaSource {
    /// A file in the package.
    Path(String),
    /// The node's config (manifest version 2).
    FromLaunchArgs(FromLaunchArgs),
}

/// `{"from_launch_args": "/schemas/out", "default": "schema/out.json"}`: the
/// schema is the `SchemaDefinition` at a JSON pointer into the node's
/// `launch_args`; `default` (a file in the package) applies when it's absent.
#[derive(Deserialize, Serialize, Clone, Debug, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FromLaunchArgs {
    pub from_launch_args: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
}

impl SchemaSource {
    /// The package file: the path, or the default of a per-node schema.
    pub fn file(&self) -> Option<&str> {
        match self {
            SchemaSource::Path(p) => Some(p),
            SchemaSource::FromLaunchArgs(f) => f.default.as_deref(),
        }
    }

    /// The JSON pointer of a per-node schema.
    pub fn pointer(&self) -> Option<&str> {
        match self {
            SchemaSource::Path(_) => None,
            SchemaSource::FromLaunchArgs(f) => Some(&f.from_launch_args),
        }
    }
}

#[derive(Deserialize, Serialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PayloadSchemas {
    #[serde(rename = "in", default, skip_serializing_if = "Option::is_none")]
    pub input: Option<SchemaSource>,
    #[serde(rename = "out", default, skip_serializing_if = "Option::is_none")]
    pub output: Option<SchemaSource>,
}

/// How the plugin is presented in a UI (manifest version 2).
#[derive(Deserialize, Serialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Ui {
    /// SVG or PNG file in the package.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    /// Free tags for grouping and filtering, e.g. `["Messaging", "IoT"]`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub categories: Vec<String>,
    /// `#RRGGBB`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    /// Markdown file in the package: the help text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub docs: Option<String>,
}

/// `manifest.json`. Unknown fields are refused, except top-level `x-*` ones
/// (kept in `extensions`; see [`Manifest::warnings`]).
#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct Manifest {
    pub manifest_version: u32,
    pub id: String,
    pub name: String,
    pub version: String,
    #[serde(rename = "type")]
    pub plugin_type: PluginType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Path of the executable, relative to the package root.
    pub executable: String,
    /// `<os>-<arch>` targets the executable runs on; `None` = any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platforms: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_args_schema: Option<String>,
    #[serde(default)]
    pub launch_args_style: LaunchArgsStyle,
    #[serde(default)]
    pub payload_schemas: PayloadSchemas,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_defaults: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ui: Option<Ui>,
    /// Every other field. Only `x-*` ones are allowed (a warning); the rest
    /// are errors in [`Manifest::validate`].
    #[serde(flatten)]
    pub extensions: BTreeMap<String, Value>,
}

/// The platform this hub runs on, in manifest notation (`macos-aarch64`, `linux-x86_64`, …).
pub fn current_platform() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

impl Manifest {
    /// Checks the manifest's own fields. File references are checked against
    /// the package contents separately. Returns every problem found.
    ///
    /// `runtime_defaults` is only checked to be an object here: its keys are
    /// the server's runtime settings, which the server checks itself.
    pub fn validate(&self, platform: &str) -> Vec<String> {
        let mut errors = vec![];
        if !(1..=MANIFEST_VERSION).contains(&self.manifest_version) {
            errors.push(format!(
                "manifest_version {} is not supported (1 to {MANIFEST_VERSION})",
                self.manifest_version
            ));
        }
        for key in self.extensions.keys().filter(|k| !k.starts_with("x-")) {
            errors.push(format!("unknown field {key:?}"));
        }
        if !is_valid_id(&self.id) {
            errors.push(format!(
                "id {:?} must match ^[a-z0-9][a-z0-9_-]{{0,63}}$",
                self.id
            ));
        }
        if self.name.trim().is_empty() {
            errors.push("name is empty".into());
        }
        if !is_semver(&self.version) {
            errors.push(format!(
                "version {:?} is not a semantic version (e.g. 1.2.3)",
                self.version
            ));
        }
        for (field, path) in self.file_references() {
            if let Err(e) = check_relative_path(path) {
                errors.push(format!("{field} {path:?}: {e}"));
            }
        }
        if let Some(platforms) = &self.platforms {
            if platforms.is_empty() {
                errors.push("platforms is empty (omit it for packages that run anywhere)".into());
            } else if !platforms.iter().any(|p| p == platform) {
                errors.push(format!(
                    "package is built for {} but this hub runs on {platform}",
                    platforms.join(", ")
                ));
            }
        }
        let (needs_in, needs_out) = match self.plugin_type {
            PluginType::Source => (false, true),
            PluginType::Sink => (true, false),
            PluginType::Transform => (true, true),
        };
        let schemas = &self.payload_schemas;
        let v2 = self.manifest_version >= 2;
        for (name, source) in [("in", &schemas.input), ("out", &schemas.output)] {
            if let Some(SchemaSource::FromLaunchArgs(f)) = source {
                if !v2 {
                    errors.push(format!(
                        "payload_schemas.{name}.from_launch_args needs manifest_version 2"
                    ));
                }
                if self.launch_args_schema.is_none() {
                    errors.push(format!(
                        "payload_schemas.{name}.from_launch_args needs a launch_args_schema"
                    ));
                }
                if let Err(e) = parse_pointer(&f.from_launch_args) {
                    errors.push(format!(
                        "payload_schemas.{name}.from_launch_args {:?}: {e}",
                        f.from_launch_args
                    ));
                }
            }
        }
        if let Some(ui) = &self.ui {
            if !v2 {
                errors.push("ui needs manifest_version 2".into());
            }
            errors.extend(ui.validate());
        }
        for (name, needed, present) in [
            ("in", needs_in, schemas.input.is_some()),
            ("out", needs_out, schemas.output.is_some()),
        ] {
            if needed && !present {
                errors.push(format!(
                    "payload_schemas.{name} is required for a {}",
                    self.plugin_type.as_str()
                ));
            }
            if !needed && present {
                errors.push(format!(
                    "payload_schemas.{name} isn't used by a {}",
                    self.plugin_type.as_str()
                ));
            }
        }
        if let Some(defaults) = &self.runtime_defaults
            && !defaults.is_object()
        {
            // The keys and their rules are the server's (a node's `config.runtime`);
            // it checks them on top of this, see `archive::read_package`'s `extra`.
            errors.push("runtime_defaults must be a JSON object".into());
        }
        errors
    }

    /// Problems that don't make the manifest invalid: `x-*` fields, which
    /// this server doesn't know and ignores.
    pub fn warnings(&self) -> Vec<String> {
        self.extensions
            .keys()
            .filter(|k| k.starts_with("x-"))
            .map(|k| format!("field {k:?} isn't known to this server and is ignored"))
            .collect()
    }

    /// Every path the manifest points to, with the field it comes from.
    pub fn file_references(&self) -> Vec<(&'static str, &str)> {
        let mut refs = vec![("executable", self.executable.as_str())];
        if let Some(p) = &self.launch_args_schema {
            refs.push(("launch_args_schema", p));
        }
        if let Some(p) = self.payload_schemas.input.as_ref().and_then(|s| s.file()) {
            refs.push(("payload_schemas.in", p));
        }
        if let Some(p) = self.payload_schemas.output.as_ref().and_then(|s| s.file()) {
            refs.push(("payload_schemas.out", p));
        }
        if let Some(ui) = &self.ui {
            if let Some(p) = &ui.icon {
                refs.push(("ui.icon", p));
            }
            if let Some(p) = &ui.docs {
                refs.push(("ui.docs", p));
            }
        }
        refs
    }
}

impl Ui {
    fn validate(&self) -> Vec<String> {
        let mut errors = vec![];
        let mut seen = HashSet::new();
        for c in &self.categories {
            if c.trim().is_empty() || c.chars().count() > 64 {
                errors.push(format!(
                    "ui.categories: {c:?} must be 1 to 64 characters, not blank"
                ));
            } else if !seen.insert(c.as_str()) {
                errors.push(format!("ui.categories: {c:?} is listed twice"));
            }
        }
        if let Some(color) = &self.color {
            let hex = color.strip_prefix('#').unwrap_or_default();
            if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
                errors.push(format!("ui.color {color:?} must be #RRGGBB"));
            }
        }
        if let Some(icon) = &self.icon {
            let lower = icon.to_ascii_lowercase();
            if !(lower.ends_with(".svg") || lower.ends_with(".png")) {
                errors.push(format!("ui.icon {icon:?} must be an .svg or .png file"));
            }
        }
        errors
    }
}

/// Parses a JSON pointer (RFC 6901) into its reference tokens. The root
/// pointer `""` isn't allowed: a schema must be a field of the config.
pub fn parse_pointer(pointer: &str) -> Result<Vec<String>, &'static str> {
    let Some(rest) = pointer.strip_prefix('/') else {
        return Err("must be a JSON pointer starting with '/'");
    };
    let mut tokens = vec![];
    for raw in rest.split('/') {
        if raw.is_empty() {
            return Err("has an empty segment");
        }
        let mut token = String::with_capacity(raw.len());
        let mut chars = raw.chars();
        while let Some(c) = chars.next() {
            match c {
                '~' => match chars.next() {
                    Some('0') => token.push('~'),
                    Some('1') => token.push('/'),
                    _ => return Err("has '~' not followed by 0 or 1"),
                },
                c => token.push(c),
            }
        }
        tokens.push(token);
    }
    Ok(tokens)
}

pub fn is_valid_id(id: &str) -> bool {
    let bytes = id.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 64
        && (bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit())
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'_' || *b == b'-')
}

/// `MAJOR.MINOR.PATCH`, optionally followed by `-prerelease` and/or `+build`.
pub fn is_semver(version: &str) -> bool {
    let core = version.split(['-', '+']).next().unwrap_or_default();
    let parts: Vec<&str> = core.split('.').collect();
    parts.len() == 3
        && parts.iter().all(|p| {
            !p.is_empty()
                && p.bytes().all(|b| b.is_ascii_digit())
                && (p.len() == 1 || !p.starts_with('0'))
        })
        && !version.ends_with(['-', '+'])
}

/// A safe path inside the package: relative, no `..`, not empty.
pub fn check_relative_path(path: &str) -> Result<(), &'static str> {
    if path.is_empty() {
        return Err("is empty");
    }
    if path.contains('\\') {
        return Err("must use '/' as separator");
    }
    let p = Path::new(path);
    for component in p.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            Component::ParentDir => return Err("must not contain '..'"),
            Component::RootDir | Component::Prefix(_) => return Err("must be relative"),
        }
    }
    Ok(())
}

/// Converts a node's `launch_args` into command-line arguments for `style`.
/// Returns every problem found (e.g. reserved or nested keys) as errors.
pub fn launch_args_to_argv(
    style: LaunchArgsStyle,
    args: &Value,
) -> Result<Vec<String>, Vec<String>> {
    if args.is_null() {
        return Ok(vec![]);
    }
    let mut argv = vec![];
    let mut errors = vec![];
    match style {
        LaunchArgsStyle::Flags => {
            let Some(map) = args.as_object() else {
                return Err(vec![
                    "launch_args must be a JSON object for launch_args_style \"flags\"".into(),
                ]);
            };
            for (key, value) in map {
                let name = key.replace('_', "-");
                if name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                {
                    errors.push(format!(
                        "launch_args.{key}: key may only contain letters, digits, '_' and '-'"
                    ));
                    continue;
                }
                if RESERVED_ARGS.contains(&name.as_str()) {
                    errors.push(format!(
                        "launch_args.{key}: --{name} is set by the hub and can't be overridden"
                    ));
                    continue;
                }
                let flag = format!("--{name}");
                match value {
                    Value::Null | Value::Bool(false) => {}
                    Value::Bool(true) => argv.push(flag),
                    Value::Number(n) => argv.extend([flag, n.to_string()]),
                    Value::String(s) => argv.extend([flag, s.clone()]),
                    Value::Array(items) => {
                        for item in items {
                            match item {
                                Value::Number(n) => argv.extend([flag.clone(), n.to_string()]),
                                Value::String(s) => argv.extend([flag.clone(), s.clone()]),
                                _ => errors.push(format!(
                                    "launch_args.{key}: array items must be strings or numbers"
                                )),
                            }
                        }
                    }
                    Value::Object(_) => errors.push(format!(
                        "launch_args.{key}: nested objects aren't supported by launch_args_style \"flags\""
                    )),
                }
            }
        }
        LaunchArgsStyle::Positional => {
            let Some(items) = args.as_array() else {
                return Err(vec!["launch_args must be a JSON array of strings for launch_args_style \"positional\"".into()]);
            };
            for (i, item) in items.iter().enumerate() {
                let Some(s) = item.as_str() else {
                    errors.push(format!("launch_args[{i}]: must be a string"));
                    continue;
                };
                let flag = s
                    .trim_start_matches('-')
                    .split('=')
                    .next()
                    .unwrap_or_default();
                if s.starts_with("--") && RESERVED_ARGS.contains(&flag) {
                    errors.push(format!(
                        "launch_args[{i}]: --{flag} is set by the hub and can't be overridden"
                    ));
                    continue;
                }
                argv.push(s.to_string());
            }
        }
    }
    if errors.is_empty() {
        Ok(argv)
    } else {
        Err(errors)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn manifest(plugin_type: PluginType) -> Manifest {
        Manifest {
            manifest_version: 1,
            id: "http-sink".into(),
            name: "HTTP sink".into(),
            version: "0.1.0".into(),
            plugin_type,
            description: None,
            executable: "bin/plugin".into(),
            platforms: None,
            launch_args_schema: None,
            launch_args_style: LaunchArgsStyle::Flags,
            payload_schemas: PayloadSchemas {
                input: Some(path("schema/in.json")),
                output: None,
            },
            runtime_defaults: None,
            ui: None,
            extensions: Default::default(),
        }
    }

    fn path(p: &str) -> SchemaSource {
        SchemaSource::Path(p.into())
    }

    #[test]
    fn valid_manifest_passes() {
        assert!(
            manifest(PluginType::Sink)
                .validate("linux-x86_64")
                .is_empty()
        );
    }

    #[test]
    fn reports_invalid_fields() {
        let mut m = manifest(PluginType::Sink);
        m.manifest_version = 3;
        m.id = "Bad Id".into();
        m.name = " ".into();
        m.version = "1.0".into();
        m.executable = "../escape".into();
        m.platforms = Some(vec!["windows-x86_64".into()]);
        m.runtime_defaults = Some(json!([1]));
        let errors = m.validate("linux-x86_64");
        for fragment in [
            "manifest_version 3",
            "id \"Bad Id\"",
            "name is empty",
            "version \"1.0\"",
            "executable \"../escape\": must not contain '..'",
            "built for windows-x86_64 but this hub runs on linux-x86_64",
            "runtime_defaults must be a JSON object",
        ] {
            assert!(
                errors.iter().any(|e| e.contains(fragment)),
                "missing {fragment:?} in {errors:?}"
            );
        }
    }

    #[test]
    fn payload_schemas_follow_the_type() {
        let mut source = manifest(PluginType::Source);
        let errors = source.validate("x");
        assert!(
            errors
                .iter()
                .any(|e| e.contains("payload_schemas.out is required for a SOURCE"))
        );
        assert!(
            errors
                .iter()
                .any(|e| e.contains("payload_schemas.in isn't used by a SOURCE"))
        );
        source.payload_schemas = PayloadSchemas {
            input: None,
            output: Some(path("o.json")),
        };
        assert!(source.validate("x").is_empty());

        let mut transform = manifest(PluginType::Transform);
        assert!(
            transform
                .validate("x")
                .iter()
                .any(|e| e.contains("payload_schemas.out is required"))
        );
        transform.payload_schemas.output = Some(path("o.json"));
        assert!(transform.validate("x").is_empty());
    }

    #[test]
    fn manifest_json_is_strict() {
        let ok = json!({"manifest_version": 1, "id": "a", "name": "A", "version": "1.0.0",
                        "type": "SINK", "executable": "a", "payload_schemas": {"in": "i.json"}});
        assert!(serde_json::from_value::<Manifest>(ok).is_ok());
        let typo = json!({"manifest_version": 1, "id": "a", "name": "A", "version": "1.0.0",
                          "type": "SINK", "executabel": "a"});
        assert!(serde_json::from_value::<Manifest>(typo).is_err());
        let bad_type = json!({"manifest_version": 1, "id": "a", "name": "A", "version": "1.0.0",
                              "type": "sink", "executable": "a"});
        assert!(serde_json::from_value::<Manifest>(bad_type).is_err());
        // unknown top-level fields: x-* is a warning, anything else an error
        let extra = json!({"manifest_version": 2, "id": "a", "name": "A", "version": "1.0.0",
                           "type": "SINK", "executable": "a", "payload_schemas": {"in": "i.json"},
                           "x-vendor": {"a": 1}, "executabel": "a"});
        let m: Manifest = serde_json::from_value(extra).unwrap();
        let errors = m.validate("x");
        assert_eq!(errors, ["unknown field \"executabel\""]);
        assert!(m.warnings()[0].contains("x-vendor"), "{:?}", m.warnings());
        // nested objects stay strict
        let nested = json!({"manifest_version": 2, "id": "a", "name": "A", "version": "1.0.0",
                            "type": "SINK", "executable": "a", "payload_schemas": {"in": "i.json"},
                            "ui": {"icons": "a.svg"}});
        assert!(serde_json::from_value::<Manifest>(nested).is_err());
    }

    #[test]
    fn version_2_fields() {
        let v2 = json!({"manifest_version": 2, "id": "a", "name": "A", "version": "1.0.0",
                        "type": "TRANSFORM", "executable": "bin/a",
                        "launch_args_schema": "schema/launch_args.json",
                        "payload_schemas": {
                            "in": {"from_launch_args": "/schemas/in"},
                            "out": {"from_launch_args": "/schemas/out", "default": "schema/out.json"}
                        },
                        "ui": {"icon": "ui/a.svg", "categories": ["Transform", "Scripting"],
                               "color": "#4f7cff", "docs": "README.md"}});
        let m: Manifest = serde_json::from_value(v2).unwrap();
        assert!(m.validate("x").is_empty(), "{:?}", m.validate("x"));
        let refs: Vec<_> = m.file_references().into_iter().map(|(_, p)| p).collect();
        assert_eq!(
            refs,
            [
                "bin/a",
                "schema/launch_args.json",
                "schema/out.json",
                "ui/a.svg",
                "README.md"
            ]
        );
        assert_eq!(
            m.payload_schemas.input.as_ref().unwrap().pointer(),
            Some("/schemas/in")
        );

        let mut v1 = m.clone();
        v1.manifest_version = 1;
        let errors = v1.validate("x");
        assert!(errors.iter().any(|e| e.contains("payload_schemas.in.from_launch_args needs manifest_version 2")));
        assert!(
            errors.iter().any(|e| e == "ui needs manifest_version 2"),
            "{errors:?}"
        );

        let mut bad = m.clone();
        bad.launch_args_schema = None;
        bad.payload_schemas.input = Some(SchemaSource::FromLaunchArgs(FromLaunchArgs {
            from_launch_args: "schemas".into(),
            default: None,
        }));
        bad.ui = Some(Ui {
            icon: Some("a.gif".into()),
            categories: vec!["A".into(), "A".into(), " ".into()],
            color: Some("blue".into()),
            docs: None,
        });
        let errors = bad.validate("x");
        for fragment in [
            "needs a launch_args_schema",
            "must be a JSON pointer",
            "ui.icon \"a.gif\"",
            "\"A\" is listed twice",
            "\" \" must be 1 to 64",
            "ui.color \"blue\"",
        ] {
            assert!(
                errors.iter().any(|e| e.contains(fragment)),
                "missing {fragment:?} in {errors:?}"
            );
        }
    }

    #[test]
    fn json_pointers() {
        assert_eq!(parse_pointer("/a/b").unwrap(), ["a", "b"]);
        assert_eq!(parse_pointer("/a~1b/c~0d").unwrap(), ["a/b", "c~d"]);
        for bad in ["", "a", "/", "/a//b", "/a~2"] {
            assert!(parse_pointer(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn ids_and_versions() {
        for ok in ["a", "http-sink", "x_1", "0abc"] {
            assert!(is_valid_id(ok), "{ok}");
        }
        for bad in ["", "-a", "_a", "A", "a.b", "a/b", &"a".repeat(65)] {
            assert!(!is_valid_id(bad), "{bad}");
        }
        for ok in ["0.0.1", "1.2.3", "10.20.30", "1.0.0-rc.1", "1.0.0+build.5"] {
            assert!(is_semver(ok), "{ok}");
        }
        for bad in ["1.0", "1.0.0.0", "01.0.0", "1.x.0", "1.0.0-", ""] {
            assert!(!is_semver(bad), "{bad}");
        }
    }

    #[test]
    fn flags_conversion() {
        let argv = launch_args_to_argv(
            LaunchArgsStyle::Flags,
            &json!({"http_timeout_seconds": 60, "verbose": true, "quiet": false,
                    "name": "x y", "tag": ["a", 1], "unset": null}),
        )
        .unwrap();
        // serde_json orders object keys alphabetically
        assert_eq!(
            argv,
            [
                "--http-timeout-seconds",
                "60",
                "--name",
                "x y",
                "--tag",
                "a",
                "--tag",
                "1",
                "--verbose"
            ]
        );
        let errors = launch_args_to_argv(
            LaunchArgsStyle::Flags,
            &json!({"port": 1, "log_level": "debug", "nested": {"a": 1}, "bad key": 1, "list": [true]}),
        )
        .unwrap_err();
        assert_eq!(errors.len(), 5, "{errors:?}");
        assert!(launch_args_to_argv(LaunchArgsStyle::Flags, &json!([1])).is_err());
    }

    #[test]
    fn positional_conversion() {
        assert_eq!(
            launch_args_to_argv(LaunchArgsStyle::Positional, &json!(["run", "--fast"])).unwrap(),
            ["run", "--fast"]
        );
        let errors = launch_args_to_argv(
            LaunchArgsStyle::Positional,
            &json!(["--host=evil", "--id", 3]),
        )
        .unwrap_err();
        assert_eq!(errors.len(), 3, "{errors:?}");
    }
}
