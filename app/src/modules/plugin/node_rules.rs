//! What a plugin says about its nodes' configuration, from its registry
//! entry: the launch-args JSON Schema and launch rules (`LaunchSpec`), its
//! secrets, its code fields, and where each node's payload schemas come from
//! (the package, or the node's `launch_args` with manifest version 2).

use super::manifest::{LaunchSpec, Manifest, validate_launch_args};
use super::service::Plugin;
use flwrs_plugin::schema::payload::SchemaDefinition;
use flwrs_plugin::schema::rules;
use serde_json::{Map, Value};
use std::collections::BTreeMap;

/// A plugin's rules for its nodes.
pub(crate) struct NodeRules {
    validator: Option<jsonschema::Validator>,
    pub spec: LaunchSpec,
    in_schema: Option<SchemaDefinition>,
    out_schema: Option<SchemaDefinition>,
    /// A remote plugin definition (manifest `launch: "remote"`): its nodes
    /// must be remote.
    pub remote_only: bool,
    plugin_id: String,
}

/// A payload direction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Side {
    In,
    Out,
}

impl NodeRules {
    /// Reads the rules from a registered plugin. Errors mean the stored
    /// package data is unreadable (it was checked at upload).
    pub(crate) fn of(plugin: &Plugin) -> Result<Self, String> {
        let manifest = plugin
            .parsed_manifest()
            .ok_or_else(|| format!("plugin {} has an unreadable manifest", plugin.id))?;
        Self::from_parts(
            manifest,
            &plugin.launch_args_schema,
            &plugin.in_schema,
            &plugin.out_schema,
        )
    }

    /// The rules of a package: its manifest and the JSON of its launch-args
    /// and payload schemas (as stored).
    pub(crate) fn from_parts(
        manifest: Manifest,
        launch_args_schema: &Option<String>,
        in_schema: &Option<String>,
        out_schema: &Option<String>,
    ) -> Result<Self, String> {
        let plugin = &manifest;
        let schema =
            match launch_args_schema {
                None => None,
                Some(text) => Some(serde_json::from_str::<Value>(text).map_err(|e| {
                    format!("plugin {} launch_args_schema isn't JSON: {e}", plugin.id)
                })?),
            };
        let validator = match &schema {
            None => None,
            Some(value) => Some(
                jsonschema::validator_for(value)
                    .map_err(|e| format!("plugin {} launch_args_schema: {e}", plugin.id))?,
            ),
        };
        let (spec, _) = LaunchSpec::new(&manifest, schema.as_ref());
        let parse = |text: &Option<String>, side: &str| -> Result<_, String> {
            text.as_deref()
                .map(rules::parse_json)
                .transpose()
                .map_err(|e| format!("plugin {} {side} schema: {e}", plugin.id))
        };
        Ok(Self {
            remote_only: manifest.is_remote_only(),
            plugin_id: manifest.id.clone(),
            in_schema: parse(in_schema, "in")?,
            out_schema: parse(out_schema, "out")?,
            validator,
            spec,
        })
    }

    /// JSON Schema, conversion rules and limits of a local node's `launch_args`.
    pub(crate) fn validate_launch_args(&self, args: &Value) -> Vec<String> {
        validate_launch_args(self.validator.as_ref(), &self.spec, args)
    }

    /// A node launched as `launch` can't use this plugin: a remote plugin
    /// definition has nothing to run here.
    pub(crate) fn launch_problem(
        &self,
        launch: crate::modules::node_config::LaunchMode,
    ) -> Option<String> {
        (self.remote_only && launch == crate::modules::node_config::LaunchMode::Local).then(|| {
            format!(
                "config.launch: plugin {} is a remote plugin definition (nothing runs on this server); its nodes must be remote (launch = \"remote\")",
                self.plugin_id
            )
        })
    }

    /// For `NodeConfig::validate`.
    pub(crate) fn plugin_rules(&self) -> crate::modules::node_config::PluginRules<'_> {
        crate::modules::node_config::PluginRules {
            launch_args_schema: self.validator.as_ref(),
            launch_spec: &self.spec,
            node: Some(self),
        }
    }

    /// Whether a node's payload schemas come from its `launch_args`.
    pub(crate) fn has_node_schemas(&self) -> bool {
        self.spec.schema_pointers() != (None, None)
    }

    /// A node's payload schema for `side`: from its `launch_args` (when the
    /// plugin says so, falling back to the package's default), else the
    /// package's. An error describes a schema the node got wrong.
    pub(crate) fn node_schema(
        &self,
        side: Side,
        args: &Value,
    ) -> Result<Option<SchemaDefinition>, String> {
        let (pointer, package) = match side {
            Side::In => (self.spec.schema_pointers().0, &self.in_schema),
            Side::Out => (self.spec.schema_pointers().1, &self.out_schema),
        };
        let Some(pointer) = pointer else {
            return Ok(package.clone());
        };
        match args.pointer(pointer).filter(|v| !v.is_null()) {
            Some(value) => {
                let schema = rules::parse_json(&value.to_string())
                    .map_err(|e| format!("launch_args{pointer}: not a payload schema: {e}"))?;
                let problems = rules::check_well_formed(&schema);
                if problems.is_empty() {
                    Ok(Some(schema))
                } else {
                    Err(format!("launch_args{pointer}: {}", problems.join("; ")))
                }
            }
            None if package.is_some() => Ok(package.clone()),
            None => Err(format!(
                "launch_args{pointer}: the node's payload schema is required"
            )),
        }
    }

    /// Both of a node's schemas' problems (empty when they're fine).
    pub(crate) fn node_schema_problems(&self, args: &Value) -> Vec<String> {
        [Side::In, Side::Out]
            .into_iter()
            .filter_map(|side| self.node_schema(side, args).err())
            .collect()
    }

    /// Syntax errors in the node's code fields. `None` when this server
    /// can't check them (built without Lua).
    pub(crate) fn code_problems(&self, args: &Value) -> Option<Vec<String>> {
        let fields = self.spec.code_fields();
        if fields.is_empty() {
            return Some(vec![]);
        }
        #[cfg(feature = "mapping-lua")]
        {
            let mut problems = vec![];
            for field in fields {
                let Some(code) = args.pointer(&field.pointer).and_then(Value::as_str) else {
                    continue;
                };
                let label = format!("launch_args{}", field.pointer);
                if let Err(e) =
                    crate::modules::mapping::lua::check_syntax(&label, code, &field.params)
                {
                    problems.push(format!("{label}: {e}"));
                }
            }
            Some(problems)
        }
        #[cfg(not(feature = "mapping-lua"))]
        {
            let _ = args;
            None
        }
    }

    /// The uploaded files a node's `launch_args` name: (field, upload name,
    /// whether the field is a secret); a list field gives one per name.
    pub(crate) fn uploads_referenced(&self, args: &Value) -> Vec<(String, String, bool)> {
        self.spec
            .fields()
            .filter(|f| f.upload)
            .flat_map(|f| {
                args.get(&f.name)
                    .map(|v| f.upload_names(v))
                    .unwrap_or_default()
                    .into_iter()
                    .map(|name| (f.name.clone(), name.to_string(), f.secret))
            })
            .collect()
    }

    /// Every secret the plugin declares (JSON pointer) and whether `args` has it.
    pub(crate) fn secrets_set(&self, args: &Value) -> BTreeMap<String, bool> {
        self.spec
            .secrets()
            .into_iter()
            .map(|p| {
                let set = args.pointer(&p).is_some_and(|v| !v.is_null());
                (p, set)
            })
            .collect()
    }

    /// `args` without its secrets' values.
    pub(crate) fn without_secrets(&self, args: &Value) -> Value {
        strip(args, &self.spec.secrets())
    }

    /// An update's `launch_args`: secrets it leaves out are kept from
    /// `existing`; an explicit `null` clears one (and is removed).
    pub(crate) fn merge_secrets(&self, update: &Value, existing: &Value) -> Value {
        let secrets = self.spec.secrets();
        if secrets.is_empty() {
            return update.clone();
        }
        let mut merged = match update {
            Value::Object(map) => map.clone(),
            Value::Null => Map::new(),
            other => return other.clone(), // not an object: validation reports it
        };
        for pointer in &secrets {
            let key = pointer_key(pointer);
            match merged.get(&key) {
                Some(Value::Null) => {
                    merged.remove(&key);
                }
                Some(_) => {}
                None => {
                    if let Some(old) = existing.pointer(pointer).filter(|v| !v.is_null()) {
                        merged.insert(key, old.clone());
                    }
                }
            }
        }
        if merged.is_empty() && update.is_null() {
            Value::Null
        } else {
            Value::Object(merged)
        }
    }
}

/// Removes top-level keys named by `pointers` (secrets are top-level).
pub(crate) fn strip(args: &Value, pointers: &[String]) -> Value {
    match args {
        Value::Object(map) => {
            let mut copy = map.clone();
            for p in pointers {
                copy.remove(&pointer_key(p));
            }
            Value::Object(copy)
        }
        other => other.clone(),
    }
}

/// The key of a top-level JSON pointer (`/a~1b` → `a/b`).
fn pointer_key(pointer: &str) -> String {
    pointer
        .trim_start_matches('/')
        .replace("~1", "/")
        .replace("~0", "~")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn plugin(manifest: Value, schema: Value, out: Option<&str>) -> Plugin {
        Plugin {
            id: "script".into(),
            name: "Script".into(),
            version: "0.1.0".into(),
            plugin_type: "TRANSFORM".into(),
            description: None,
            manifest: manifest.to_string(),
            launch_args_schema: Some(schema.to_string()),
            in_schema: None,
            out_schema: out.map(String::from),
            install_path: "/x".into(),
            archive_sha256: String::new(),
            create_time: Default::default(),
            update_time: Default::default(),
        }
    }

    fn rules() -> NodeRules {
        let manifest = json!({"manifest_version": 2, "id": "script", "name": "Script",
            "version": "0.1.0", "type": "TRANSFORM", "executable": "bin/run",
            "launch_args_schema": "schema/launch_args.json",
            "payload_schemas": {"in": {"from_launch_args": "/schemas/in"},
                                "out": {"from_launch_args": "/schemas/out", "default": "schema/out.json"}}});
        let schema = json!({"type": "object", "properties": {
            "schemas": {"type": "object", "properties": {"in": {"type": "object"}, "out": {"type": "object"}}},
            "script": {"type": "string", "x-flwrs-launch": {"via": "file"},
                       "x-flwrs-code": {"language": "lua", "params": ["payload", "meta"]}},
            "api_key": {"type": "string", "writeOnly": true, "x-flwrs-launch": {"via": "env"}},
            "pem": {"type": "string", "writeOnly": true, "x-flwrs-launch": {"via": "file"}},
            "model": {"type": "string", "x-flwrs-launch": {"via": "file", "source": "upload"}},
            "labels": {"type": "array", "items": {"type": "string"},
                       "x-flwrs-launch": {"via": "file", "source": "upload"}}
        }});
        let out = r#"{"fields": [{"key": "x", "type": "STRING"}]}"#;
        NodeRules::of(&plugin(manifest, schema, Some(out))).unwrap()
    }

    #[test]
    fn node_schemas_come_from_the_launch_args() {
        let r = rules();
        assert!(r.has_node_schemas());
        let args = json!({"schemas": {"in": {"fields": [{"key": "a", "type": "U64"}]}}});
        let input = r.node_schema(Side::In, &args).unwrap().unwrap();
        assert_eq!(input.fields[0].key, "a");
        // out falls back to the package's default
        let output = r.node_schema(Side::Out, &args).unwrap().unwrap();
        assert_eq!(output.fields[0].key, "x");
        // in has no default: required
        let e = r.node_schema(Side::In, &json!({})).unwrap_err();
        assert!(
            e.contains("launch_args/schemas/in: the node's payload schema is required"),
            "{e}"
        );
        let bad = json!({"schemas": {"in": {"fields": [{"key": "a"}]}}});
        assert!(!r.node_schema_problems(&bad).is_empty());
        let bad = json!({"schemas": {"in": {"fields": "no"}}});
        assert!(
            r.node_schema(Side::In, &bad)
                .unwrap_err()
                .contains("not a payload schema")
        );
    }

    #[cfg(feature = "mapping-lua")]
    #[test]
    fn code_fields_are_syntax_checked() {
        let r = rules();
        assert!(
            r.code_problems(&json!({"script": "return payload"}))
                .unwrap()
                .is_empty()
        );
        let problems = r
            .code_problems(&json!({"script": "local x = \nreturn x +"}))
            .unwrap();
        assert!(
            problems[0].starts_with("launch_args/script: launch_args/script:2:"),
            "{problems:?}"
        );
        // `...` isn't valid inside the function the code becomes
        assert!(
            !r.code_problems(&json!({"script": "return ..."}))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn secrets_are_listed_hidden_and_kept() {
        let r = rules();
        let stored = json!({"api_key": "k", "pem": "p", "script": "x"});
        assert_eq!(
            r.secrets_set(&stored),
            BTreeMap::from([("/api_key".into(), true), ("/pem".into(), true)])
        );
        assert_eq!(r.without_secrets(&stored), json!({"script": "x"}));
        // left out: kept; null: cleared; a value: replaced
        let merged = r.merge_secrets(
            &json!({"script": "y", "pem": null, "api_key": "new"}),
            &stored,
        );
        assert_eq!(merged, json!({"script": "y", "api_key": "new"}));
        let merged = r.merge_secrets(&json!({"script": "y"}), &stored);
        assert_eq!(merged, json!({"script": "y", "api_key": "k", "pem": "p"}));
        assert_eq!(
            r.merge_secrets(&Value::Null, &stored),
            json!({"api_key": "k", "pem": "p"})
        );
        assert!(!r.secrets_set(&Value::Null)["/api_key"]);
    }

    #[test]
    fn uploads_are_listed_one_per_name() {
        let r = rules();
        let args = json!({"model": "m.onnx", "labels": ["a.txt", "b.txt"], "script": "x"});
        let named: Vec<_> = r
            .uploads_referenced(&args)
            .into_iter()
            .map(|(field, name, _)| format!("{field}:{name}"))
            .collect();
        assert_eq!(named, ["labels:a.txt", "labels:b.txt", "model:m.onnx"]);
        assert!(r.uploads_referenced(&json!({"labels": []})).is_empty());
    }
}
