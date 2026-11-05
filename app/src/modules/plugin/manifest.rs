//! Plugin manifest (`manifest.json` at the root of a plugin package) and the
//! rules for a node's `launch_args`, which the manifest defines.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Component, Path};

pub(crate) const MANIFEST_FILE: &str = "manifest.json";
pub(crate) const MANIFEST_VERSION: u32 = 1;

/// Hub bootstrap arguments; a node's `launch_args` can't override them.
pub(crate) const RESERVED_ARGS: [&str; 5] = ["id", "host", "port", "log-level", "token"];

#[derive(Deserialize, Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "UPPERCASE")]
pub(crate) enum PluginType {
    Source,
    Sink,
    Transform,
}

impl PluginType {
    pub(crate) fn as_str(&self) -> &'static str {
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
pub(crate) enum LaunchArgsStyle {
    /// `{"http_timeout_seconds": 60, "verbose": true}` → `--http-timeout-seconds 60 --verbose`.
    #[default]
    Flags,
    /// `["a", "b"]` → `a b`, verbatim.
    Positional,
    /// Any JSON → `--launch-args '<json>'`.
    Json,
}

#[derive(Deserialize, Serialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct PayloadSchemaPaths {
    #[serde(rename = "in", default, skip_serializing_if = "Option::is_none")]
    pub input: Option<String>,
    #[serde(rename = "out", default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
}

#[derive(Deserialize, Serialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub(crate) struct Manifest {
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
    pub payload_schemas: PayloadSchemaPaths,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_defaults: Option<Value>,
}

/// The platform this hub runs on, in manifest notation (`macos-aarch64`, `linux-x86_64`, …).
pub(crate) fn current_platform() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

impl Manifest {
    /// Checks the manifest's own fields. File references are checked against
    /// the package contents separately. Returns every problem found.
    pub(crate) fn validate(&self, platform: &str) -> Vec<String> {
        let mut errors = vec![];
        if self.manifest_version != MANIFEST_VERSION {
            errors.push(format!(
                "manifest_version {} is not supported (expected {MANIFEST_VERSION})",
                self.manifest_version
            ));
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
        if let Some(defaults) = &self.runtime_defaults {
            // Same keys and rules as a node's `config.runtime`.
            match crate::modules::node_config::RuntimeConfig::parse(defaults) {
                Ok(runtime) => errors.extend(runtime.validate("runtime_defaults")),
                Err(e) => errors.push(format!("runtime_defaults: {e}")),
            }
        }
        errors
    }

    /// Every path the manifest points to, with the field it comes from.
    pub(crate) fn file_references(&self) -> Vec<(&'static str, &str)> {
        let mut refs = vec![("executable", self.executable.as_str())];
        if let Some(p) = &self.launch_args_schema {
            refs.push(("launch_args_schema", p));
        }
        if let Some(p) = &self.payload_schemas.input {
            refs.push(("payload_schemas.in", p));
        }
        if let Some(p) = &self.payload_schemas.output {
            refs.push(("payload_schemas.out", p));
        }
        refs
    }
}

pub(crate) fn is_valid_id(id: &str) -> bool {
    let bytes = id.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 64
        && (bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit())
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'_' || *b == b'-')
}

/// `MAJOR.MINOR.PATCH`, optionally followed by `-prerelease` and/or `+build`.
fn is_semver(version: &str) -> bool {
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
pub(crate) fn check_relative_path(path: &str) -> Result<(), &'static str> {
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
pub(crate) fn launch_args_to_argv(
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
        LaunchArgsStyle::Json => argv.extend(["--launch-args".to_string(), args.to_string()]),
    }
    if errors.is_empty() {
        Ok(argv)
    } else {
        Err(errors)
    }
}

/// Validates a node's `launch_args` against the plugin's JSON Schema (if it
/// has one) and the conversion rules of its `launch_args_style`.
pub(crate) fn validate_launch_args(
    schema: Option<&jsonschema::Validator>,
    style: LaunchArgsStyle,
    args: &Value,
) -> Vec<String> {
    // Omitted launch_args means "no extra arguments": validate the style's
    // empty value, so schemas with required keys still catch the omission.
    // The json style passes nothing at all when omitted, so there's nothing to check.
    let empty_object = Value::Object(Default::default());
    let empty_array = Value::Array(vec![]);
    let effective = match (args, style) {
        (Value::Null, LaunchArgsStyle::Flags) => Some(&empty_object),
        (Value::Null, LaunchArgsStyle::Positional) => Some(&empty_array),
        (Value::Null, LaunchArgsStyle::Json) => None,
        _ => Some(args),
    };
    let mut errors: Vec<String> = match (schema, effective) {
        (Some(validator), Some(args)) => validator
            .iter_errors(args)
            .map(|e| {
                let at = e.instance_path().to_string();
                if at.is_empty() {
                    format!("launch_args: {e}")
                } else {
                    format!("launch_args{at}: {e}")
                }
            })
            .collect(),
        _ => vec![],
    };
    if let Err(conversion) = launch_args_to_argv(style, args) {
        errors.extend(conversion);
    }
    errors
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
            payload_schemas: PayloadSchemaPaths {
                input: Some("schema/in.json".into()),
                output: None,
            },
            runtime_defaults: None,
        }
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
        m.manifest_version = 2;
        m.id = "Bad Id".into();
        m.name = " ".into();
        m.version = "1.0".into();
        m.executable = "../escape".into();
        m.platforms = Some(vec!["windows-x86_64".into()]);
        m.runtime_defaults = Some(json!({"init_timeout_ms": 0, "typo_ms": 1}));
        let errors = m.validate("linux-x86_64");
        for fragment in [
            "manifest_version 2",
            "id \"Bad Id\"",
            "name is empty",
            "version \"1.0\"",
            "executable \"../escape\": must not contain '..'",
            "built for windows-x86_64 but this hub runs on linux-x86_64",
            "runtime_defaults: unknown field `typo_ms`",
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
        source.payload_schemas = PayloadSchemaPaths {
            input: None,
            output: Some("o.json".into()),
        };
        assert!(source.validate("x").is_empty());

        let mut transform = manifest(PluginType::Transform);
        assert!(
            transform
                .validate("x")
                .iter()
                .any(|e| e.contains("payload_schemas.out is required"))
        );
        transform.payload_schemas.output = Some("o.json".into());
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
    fn positional_and_json_conversion() {
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
        assert_eq!(
            launch_args_to_argv(LaunchArgsStyle::Json, &json!({"a": [1, 2]})).unwrap(),
            ["--launch-args", r#"{"a":[1,2]}"#]
        );
        assert!(
            launch_args_to_argv(LaunchArgsStyle::Json, &Value::Null)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn launch_args_schema_validation() {
        let schema = json!({
            "type": "object",
            "properties": {"timeout": {"type": "integer", "minimum": 1}},
            "additionalProperties": false
        });
        let validator = jsonschema::validator_for(&schema).unwrap();
        assert!(
            validate_launch_args(
                Some(&validator),
                LaunchArgsStyle::Flags,
                &json!({"timeout": 5})
            )
            .is_empty()
        );
        let errors = validate_launch_args(
            Some(&validator),
            LaunchArgsStyle::Flags,
            &json!({"timeout": 0, "x": 1}),
        );
        assert!(
            errors.iter().any(|e| e.starts_with("launch_args/timeout:")),
            "{errors:?}"
        );
        assert!(errors.iter().any(|e| e.contains("x")), "{errors:?}");
        // omitted launch_args validate as the style's empty value
        assert!(
            validate_launch_args(Some(&validator), LaunchArgsStyle::Flags, &Value::Null).is_empty()
        );
        let required =
            jsonschema::validator_for(&json!({"type": "object", "required": ["timeout"]})).unwrap();
        assert!(
            !validate_launch_args(Some(&required), LaunchArgsStyle::Flags, &Value::Null).is_empty()
        );
        assert!(
            validate_launch_args(Some(&required), LaunchArgsStyle::Json, &Value::Null).is_empty()
        );
        // conversion rules apply even without a schema
        assert!(
            !validate_launch_args(None, LaunchArgsStyle::Flags, &json!({"token": "t"})).is_empty()
        );
    }
}
