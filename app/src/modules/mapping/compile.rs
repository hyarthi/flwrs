//! Compiling a [`MappingSpec`] once (in the scene service to validate, in the
//! director when a scene loads) and applying it per event.

use super::path::Path;
use super::value::{self, Value, convertible};
use super::{Codec, MappingMode, MappingSpec, ROOT};
use flwrs_plugin::schema::payload::{
    FieldDefinition, PluginPayload, SchemaDefinition, field_type::Enum as FieldType,
};
use flwrs_plugin::schema::protocol::EventMetadata;
use flwrs_types::json;
use std::sync::Arc;

/// A compiled mapping: dot-notation rules, plus Lua rules,
/// `script` and `filter` with the `mapping-lua` feature.
pub(crate) trait Mapping: Send + Sync + std::fmt::Debug {
    /// The payload to send along the edge; `None` = filtered out.
    fn apply(
        &self,
        input: &PluginPayload,
        meta: Option<&EventMetadata>,
    ) -> Result<Option<PluginPayload>, String>;
}

/// The payload schemas around an edge, when they're known (a plugin may not
/// be installed, e.g. right after an import).
#[derive(Clone, Copy, Default)]
pub(crate) struct Schemas<'a> {
    /// The source node's `out` schema.
    pub source: Option<&'a SchemaDefinition>,
    /// The target node's `in` schema.
    pub target: Option<&'a SchemaDefinition>,
}

/// Compiles `spec`. Paths are checked against the schemas that are known,
/// constants are converted to their target field's type, and copies must be
/// between convertible types. Returns every problem.
pub(crate) fn compile(
    spec: &MappingSpec,
    schemas: Schemas,
) -> Result<Arc<dyn Mapping>, Vec<String>> {
    let mut errors = vec![];
    let lua = LuaParts::compile(spec, schemas)?;
    let mut rules = vec![];
    for (i, rule) in spec.rules.iter().flatten().enumerate() {
        let label = format!("mapping.rules[{i}]");
        if let (Some(codec), Some(source)) = (rule.decode, &rule.source) {
            match compile_decode(codec, source, &rule.target, schemas) {
                Ok(rule) => rules.push(rule),
                Err(e) => errors.push(format!("{label}: {e}")),
            }
            continue;
        }
        if let (Some(codec), Some(source)) = (rule.encode, &rule.source) {
            match compile_encode(codec, source, &rule.target, schemas) {
                Ok(rule) => rules.push(rule),
                Err(e) => errors.push(format!("{label}: {e}")),
            }
            continue;
        }
        let target = match Path::parse(&rule.target) {
            Ok(path) => path,
            Err(e) => {
                errors.push(format!("{label}.target: {e}"));
                continue;
            }
        };
        let target_def = match schemas.target.map(|s| target.resolve(s)).transpose() {
            Ok(def) => def,
            Err(e) => {
                errors.push(format!("{label}.target: {e} (target's input schema)"));
                continue;
            }
        };
        if rule.lua.is_some() {
            match lua.rule_chunk(i) {
                Some(chunk) => rules.push(Rule::Lua {
                    target,
                    target_def: target_def.cloned(),
                    chunk,
                }),
                None => errors.push(format!("{label}.lua: not compiled")),
            }
        } else if rule.delete == Some(true) {
            rules.push(Rule::Delete { target });
        } else if let Some(json) = &rule.value {
            match value::from_json(json, target_def) {
                Ok(value) => rules.push(Rule::Set { target, value }),
                Err(e) => errors.push(format!("{label}.value: {e}")),
            }
        } else if let Some(source) = &rule.source {
            let source = match Path::parse(source) {
                Ok(path) => path,
                Err(e) => {
                    errors.push(format!("{label}.source: {e}"));
                    continue;
                }
            };
            match schemas.source.map(|s| source.resolve(s)).transpose() {
                Err(e) => errors.push(format!("{label}.source: {e} (source's output schema)")),
                Ok(source_def) => {
                    if let (Some(from), Some(to)) = (source_def, target_def)
                        && !convertible(from.r#type(), to.r#type())
                    {
                        errors.push(format!(
                            "{label}: {source} is a {}, which can't become {target} ({})",
                            from.r#type().as_str_name(),
                            to.r#type().as_str_name()
                        ));
                    } else {
                        rules.push(Rule::Copy { source, target });
                    }
                }
            }
        }
    }
    if !errors.is_empty() {
        return Err(errors);
    }
    Ok(Arc::new(DotMapping {
        mode: spec.mode.unwrap_or_default(),
        rules,
        target_schema: schemas.target.cloned(),
        lua,
    }))
}

/// Where a decoded value goes.
#[derive(Debug)]
enum Output {
    /// The whole output (target `"."`).
    Root,
    Path(Path),
}

/// What an encode rule encodes.
#[derive(Debug)]
enum Input {
    /// The whole input (source `"."`).
    Root,
    Path(Path),
}

/// A decode rule: text at `source` becomes a value shaped by `def`.
fn compile_decode(
    codec: Codec,
    source: &str,
    target: &str,
    schemas: Schemas,
) -> Result<Rule, String> {
    let Codec::Json = codec;
    let source = Path::parse(source).map_err(|e| format!("source: {e}"))?;
    if let Some(def) = schemas
        .source
        .map(|s| source.resolve(s))
        .transpose()
        .map_err(|e| format!("source: {e} (source's output schema)"))?
        && !matches!(def.r#type(), FieldType::String | FieldType::Bytes)
    {
        return Err(format!(
            "{source} is a {}; only BYTES or STRING can be decoded",
            def.r#type().as_str_name()
        ));
    }
    let (target, def) = if target == ROOT {
        let def = schemas.target.map(|s| FieldDefinition {
            r#type: FieldType::Object as i32,
            object_fields: s.fields.clone(),
            ..Default::default()
        });
        (Output::Root, def)
    } else {
        let path = Path::parse(target).map_err(|e| format!("target: {e}"))?;
        let def = schemas
            .target
            .map(|s| path.resolve(s))
            .transpose()
            .map_err(|e| format!("target: {e} (target's input schema)"))?
            .cloned();
        (Output::Path(path), def)
    };
    Ok(Rule::Decode {
        source,
        target,
        def,
    })
}

/// An encode rule: the value at `source` becomes text at `target`.
fn compile_encode(
    codec: Codec,
    source: &str,
    target: &str,
    schemas: Schemas,
) -> Result<Rule, String> {
    let Codec::Json = codec;
    let source = if source == ROOT {
        Input::Root
    } else {
        let path = Path::parse(source).map_err(|e| format!("source: {e}"))?;
        if let Some(schema) = schemas.source {
            path.resolve(schema)
                .map_err(|e| format!("source: {e} (source's output schema)"))?;
        }
        Input::Path(path)
    };
    let path = Path::parse(target).map_err(|e| format!("target: {e}"))?;
    let def = schemas
        .target
        .map(|s| path.resolve(s))
        .transpose()
        .map_err(|e| format!("target: {e} (target's input schema)"))?;
    let as_string = match def.map(|d| d.r#type()) {
        None | Some(FieldType::Bytes) => false,
        Some(FieldType::String) => true,
        Some(other) => {
            return Err(format!(
                "{path} is a {}; encoded text goes into BYTES or STRING",
                other.as_str_name()
            ));
        }
    };
    Ok(Rule::Encode {
        source,
        target: path,
        as_string,
    })
}

#[derive(Debug)]
enum Rule {
    Copy {
        source: Path,
        target: Path,
    },
    /// JSON text at `source` → a value (shaped by `def`) at `target`.
    Decode {
        source: Path,
        target: Output,
        def: Option<FieldDefinition>,
    },
    /// The value at `source` → JSON text at `target` (STRING, else BYTES).
    Encode {
        source: Input,
        target: Path,
        as_string: bool,
    },
    Set {
        target: Path,
        value: Value,
    },
    Delete {
        target: Path,
    },
    /// The chunk's result goes to `target`; `nil` removes it.
    #[cfg_attr(not(feature = "mapping-lua"), allow(dead_code))]
    Lua {
        target: Path,
        target_def: Option<flwrs_plugin::schema::payload::FieldDefinition>,
        chunk: LuaChunkRef,
    },
}

#[derive(Debug)]
struct DotMapping {
    mode: MappingMode,
    rules: Vec<Rule>,
    /// The output is shaped to it (lossless conversions; undeclared fields dropped).
    target_schema: Option<SchemaDefinition>,
    lua: LuaParts,
}

impl Mapping for DotMapping {
    fn apply(
        &self,
        input: &PluginPayload,
        meta: Option<&EventMetadata>,
    ) -> Result<Option<PluginPayload>, String> {
        let input = Value::from_payload(input);
        if !self.lua.filter(&input, meta)? {
            return Ok(None);
        }
        if let Some(scripted) = self.lua.script(&input, meta, self.target_schema.as_ref()) {
            let Some(out) = scripted? else {
                return Ok(None); // `return nil` drops the event on this edge
            };
            let out = match &self.target_schema {
                Some(schema) => value::coerce_payload(out, schema)?,
                None => out,
            };
            return out.into_payload().map(Some);
        }
        let mut out = match self.mode {
            MappingMode::Merge => input.clone(),
            MappingMode::Replace => Value::Object(vec![]),
        };
        for rule in &self.rules {
            match rule {
                // Sources read the input, not the output so far. A source
                // that isn't there leaves the target alone.
                Rule::Copy { source, target } => {
                    if let Some(found) = source.get(&input) {
                        target.set(&mut out, found.clone())?;
                    }
                }
                Rule::Decode {
                    source,
                    target,
                    def,
                } => {
                    let text = match source.get(&input) {
                        None | Some(Value::Null) => continue,
                        Some(Value::String(s)) => s.as_bytes(),
                        Some(Value::Bytes(b)) => b.as_slice(),
                        Some(other) => {
                            return Err(format!(
                                "{source}: a {} isn't JSON text",
                                other.type_name()
                            ));
                        }
                    };
                    let decoded =
                        json::decode(text, def.as_ref()).map_err(|e| format!("{source}: {e}"))?;
                    match target {
                        Output::Root => match decoded {
                            Value::Object(_) => out = decoded,
                            other => {
                                return Err(format!(
                                    "{source}: the JSON is a {}, but the whole output must be an object",
                                    other.type_name()
                                ));
                            }
                        },
                        Output::Path(path) => path.set(&mut out, decoded)?,
                    }
                }
                Rule::Encode {
                    source,
                    target,
                    as_string,
                } => {
                    let found = match source {
                        Input::Root => &input,
                        Input::Path(path) => match path.get(&input) {
                            Some(found) => found,
                            None => continue,
                        },
                    };
                    let text = json::encode(found).map_err(|e| format!("{target}: {e}"))?;
                    let text = if *as_string {
                        Value::String(text)
                    } else {
                        Value::Bytes(text.into_bytes())
                    };
                    target.set(&mut out, text)?;
                }
                Rule::Set { target, value } => target.set(&mut out, value.clone())?,
                Rule::Delete { target } => {
                    target.delete(&mut out);
                }
                Rule::Lua {
                    target,
                    target_def,
                    chunk,
                } => {
                    let result = self
                        .lua
                        .rule(chunk, &input, meta, target_def.as_ref())
                        .map_err(|e| format!("{target}: {e}"))?;
                    if result == Value::Null {
                        // nil removes the field, like `delete`.
                        target.delete(&mut out);
                    } else {
                        target.set(&mut out, result)?;
                    }
                }
            }
        }
        let out = match &self.target_schema {
            Some(schema) => value::coerce_payload(out, schema)?,
            None => out,
        };
        out.into_payload().map(Some)
    }
}

// --- the Lua parts of a mapping, or their absence ---

#[cfg(feature = "mapping-lua")]
type LuaChunkRef = super::lua::Chunk;
#[cfg(not(feature = "mapping-lua"))]
type LuaChunkRef = ();

/// A mapping's Lua state and chunks (empty when it has no Lua).
#[derive(Debug, Default)]
struct LuaParts {
    #[cfg(feature = "mapping-lua")]
    runtime: Option<super::lua::Runtime>,
    #[cfg(feature = "mapping-lua")]
    filter: Option<super::lua::Chunk>,
    #[cfg(feature = "mapping-lua")]
    script: Option<super::lua::Chunk>,
    #[cfg(feature = "mapping-lua")]
    rules: Vec<Option<super::lua::Chunk>>,
}

#[cfg(not(feature = "mapping-lua"))]
impl LuaParts {
    fn compile(spec: &MappingSpec, _schemas: Schemas) -> Result<Self, Vec<String>> {
        if spec.uses_lua() {
            return Err(vec![
                "mapping uses Lua, but this server was built without Lua support (the mapping-lua feature)".into(),
            ]);
        }
        Ok(Self::default())
    }

    fn rule_chunk(&self, _rule: usize) -> Option<LuaChunkRef> {
        None
    }

    fn filter(&self, _input: &Value, _meta: Option<&EventMetadata>) -> Result<bool, String> {
        Ok(true)
    }

    fn script(
        &self,
        _input: &Value,
        _meta: Option<&EventMetadata>,
        _schema: Option<&SchemaDefinition>,
    ) -> Option<Result<Option<Value>, String>> {
        None
    }

    fn rule(
        &self,
        _chunk: &LuaChunkRef,
        _input: &Value,
        _meta: Option<&EventMetadata>,
        _def: Option<&flwrs_plugin::schema::payload::FieldDefinition>,
    ) -> Result<Value, String> {
        Err("no Lua support".into())
    }
}

#[cfg(feature = "mapping-lua")]
impl LuaParts {
    /// Creates the edge's Lua state (only if the mapping has Lua) and
    /// compiles every chunk; returns every syntax error.
    fn compile(spec: &MappingSpec, _schemas: Schemas) -> Result<Self, Vec<String>> {
        use super::lua::{Limits, Runtime};
        if !spec.uses_lua() {
            return Ok(Self::default());
        }
        let defaults = Limits::default();
        let limits = spec.lua.as_ref().map_or(defaults, |l| Limits {
            max_instructions: l.max_instructions.unwrap_or(defaults.max_instructions),
            timeout: l
                .timeout_ms
                .map_or(defaults.timeout, std::time::Duration::from_millis),
            max_memory_bytes: l.max_memory_bytes.unwrap_or(defaults.max_memory_bytes),
        });
        let runtime = Runtime::new(limits).map_err(|e| vec![e])?;
        let mut errors = vec![];
        let mut compile = |label: &str, code: &str| match runtime.chunk(label, code) {
            Ok(chunk) => Some(chunk),
            Err(e) => {
                errors.push(format!("{label}.lua: {e}"));
                None
            }
        };
        let filter = spec
            .filter
            .as_ref()
            .and_then(|f| compile("mapping.filter", &f.lua));
        let script = spec
            .script
            .as_ref()
            .and_then(|s| compile("mapping.script", &s.lua));
        let rules = spec
            .rules
            .iter()
            .flatten()
            .enumerate()
            .map(|(i, r)| {
                r.lua
                    .as_ref()
                    .and_then(|code| compile(&format!("mapping.rules[{i}]"), code))
            })
            .collect();
        if !errors.is_empty() {
            return Err(errors);
        }
        Ok(Self {
            runtime: Some(runtime),
            filter,
            script,
            rules,
        })
    }

    fn rule_chunk(&self, rule: usize) -> Option<LuaChunkRef> {
        self.rules.get(rule).cloned().flatten()
    }

    fn runtime(&self) -> &super::lua::Runtime {
        self.runtime.as_ref().expect("chunks imply a runtime")
    }

    /// Whether the event goes along the edge (no filter: yes).
    fn filter(&self, input: &Value, meta: Option<&EventMetadata>) -> Result<bool, String> {
        match &self.filter {
            None => Ok(true),
            Some(chunk) => self
                .runtime()
                .call(chunk, input, meta)
                .map(|v| super::lua::truthy(&v))
                .map_err(|e| format!("filter: {e}")),
        }
    }

    /// The script's output, if the mapping is a script; `Ok(None)` = dropped.
    fn script(
        &self,
        input: &Value,
        meta: Option<&EventMetadata>,
        schema: Option<&SchemaDefinition>,
    ) -> Option<Result<Option<Value>, String>> {
        let chunk = self.script.as_ref()?;
        Some(
            self.runtime()
                .call(chunk, input, meta)
                .and_then(|result| {
                    if result.is_nil() {
                        Ok(None)
                    } else {
                        super::lua::payload_from_lua(result, schema).map(Some)
                    }
                })
                .map_err(|e| format!("script: {e}")),
        )
    }

    fn rule(
        &self,
        chunk: &LuaChunkRef,
        input: &Value,
        meta: Option<&EventMetadata>,
        def: Option<&flwrs_plugin::schema::payload::FieldDefinition>,
    ) -> Result<Value, String> {
        let result = self.runtime().call(chunk, input, meta)?;
        super::lua::from_lua(result, def)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flwrs_plugin::schema::payload::{Field, FieldValue, field_value::Value as Pb};
    use flwrs_plugin::schema::rules::parse_json;
    use serde_json::json;

    fn spec(json: serde_json::Value) -> MappingSpec {
        MappingSpec::parse(&json).unwrap().unwrap()
    }

    fn source_schema() -> SchemaDefinition {
        parse_json(
            r#"{"fields": [
                {"key": "endpoint", "type": "STRING"},
                {"key": "count", "type": "I32"},
                {"key": "readings", "type": "ARRAY", "nested_type_definition": {"type": "OBJECT", "object_fields": [{"key": "value", "type": "F64"}]}},
                {"key": "debug", "type": "STRING"}
            ]}"#,
        )
        .unwrap()
    }

    fn target_schema() -> SchemaDefinition {
        parse_json(
            r#"{"fields": [
                {"key": "url", "type": "STRING"},
                {"key": "method", "type": "STRING"},
                {"key": "total", "type": "I64"},
                {"key": "first", "type": "F64"},
                {"key": "headers", "type": "MAP", "nested_type_definition": {"type": "ARRAY", "nested_type_definition": {"type": "STRING"}}},
                {"key": "debug", "type": "STRING"}
            ]}"#,
        )
        .unwrap()
    }

    fn field(key: &str, value: Pb) -> Field {
        Field {
            key: key.into(),
            value: Some(FieldValue { value: Some(value) }),
        }
    }

    fn input() -> PluginPayload {
        let reading = |v: f64| FieldValue {
            value: Some(Pb::Object(flwrs_plugin::schema::payload::MapValue {
                value: [(
                    "value".to_string(),
                    FieldValue {
                        value: Some(Pb::F64(v)),
                    },
                )]
                .into(),
            })),
        };
        PluginPayload {
            fields: vec![
                field("endpoint", Pb::String("http://x".into())),
                field("count", Pb::I32(3)),
                field(
                    "readings",
                    Pb::Array(flwrs_plugin::schema::payload::ArrayValue {
                        value: vec![reading(1.5), reading(2.5)],
                    }),
                ),
                field("debug", Pb::String("noisy".into())),
            ],
        }
    }

    fn get(payload: &PluginPayload, key: &str) -> Option<Value> {
        Path::parse(key)
            .unwrap()
            .get(&Value::from_payload(payload))
            .cloned()
    }

    #[test]
    fn the_plan_example_maps_and_shapes_the_output() {
        let (s, t) = (source_schema(), target_schema());
        let mapping = compile(
            &spec(json!({"mode": "merge", "rules": [
                {"target": "url", "source": "endpoint"},
                {"target": "method", "value": "POST"},
                {"target": "headers.content-type", "value": ["application/json"]},
                {"target": "total", "source": "count"},
                {"target": "first", "source": "readings[0].value"},
                {"target": "debug", "delete": true}
            ]})),
            Schemas {
                source: Some(&s),
                target: Some(&t),
            },
        )
        .unwrap();
        let out = mapping.apply(&input(), None).unwrap().unwrap();
        assert_eq!(get(&out, "url"), Some(Value::String("http://x".into())));
        assert_eq!(get(&out, "method"), Some(Value::String("POST".into())));
        assert_eq!(
            get(&out, "total"),
            Some(Value::I64(3)),
            "I32 coerced to the target's I64"
        );
        assert_eq!(get(&out, "first"), Some(Value::F64(1.5)));
        assert_eq!(
            get(&out, r#"headers["content-type"][0]"#),
            Some(Value::String("application/json".into()))
        );
        assert_eq!(get(&out, "debug"), None, "deleted");
        assert_eq!(
            get(&out, "endpoint"),
            None,
            "merged, but not in the target schema: dropped"
        );
    }

    #[test]
    fn replace_starts_empty_and_missing_sources_are_skipped() {
        let mapping = compile(
            &spec(json!({"mode": "replace", "rules": [
                {"target": "a", "source": "endpoint"},
                {"target": "b", "source": "nope"}
            ]})),
            Schemas::default(),
        )
        .unwrap();
        let out = mapping.apply(&input(), None).unwrap().unwrap();
        assert_eq!(out.fields.len(), 1);
        assert_eq!(get(&out, "a"), Some(Value::String("http://x".into())));
    }

    #[test]
    fn config_time_checks_against_the_schemas() {
        let (s, t) = (source_schema(), target_schema());
        let errors = compile(
            &spec(json!({"rules": [
                {"target": "nope", "value": 1},
                {"target": "url", "source": "missing"},
                {"target": "url", "source": "count"},
                {"target": "total", "value": "not a number"},
                {"target": "url.x", "value": "y"}
            ]})),
            Schemas {
                source: Some(&s),
                target: Some(&t),
            },
        )
        .unwrap_err();
        assert_eq!(errors.len(), 5, "{errors:?}");
        assert!(
            errors[0].contains("no field \"nope\"") && errors[0].contains("target's input schema")
        );
        assert!(
            errors[1].contains("no field \"missing\"")
                && errors[1].contains("source's output schema")
        );
        assert!(errors[2].contains("count is a I32, which can't become url (STRING)"));
        assert!(errors[3].contains("can't be a I64"));
        assert!(errors[4].contains("url is a STRING"));
        // Without schemas only the syntax is checked.
        assert!(
            compile(
                &spec(json!({"rules": [{"target": "nope", "source": "missing"}]})),
                Schemas::default()
            )
            .is_ok()
        );
    }

    #[test]
    fn run_time_errors_skip_the_event() {
        let t = target_schema();
        // An I64 above I32's range... into a STRING target can't be coerced.
        let mapping = compile(
            &spec(json!({"rules": [{"target": "url", "source": "count"}]})),
            Schemas {
                source: None,
                target: Some(&t),
            },
        )
        .unwrap();
        let err = mapping.apply(&input(), None).unwrap_err();
        assert!(err.contains("url: expected STRING, got I32"), "{err}");
    }

    #[cfg(not(feature = "mapping-lua"))]
    #[test]
    fn a_build_without_lua_refuses_lua_mappings() {
        for lua in [
            json!({"rules": [{"target": "a", "lua": "return 1"}]}),
            json!({"script": {"lua": "return payload"}}),
            json!({"filter": {"lua": "return true"}}),
        ] {
            let errors = compile(&spec(lua), Schemas::default()).unwrap_err();
            assert!(
                errors[0].contains("built without Lua support"),
                "{errors:?}"
            );
        }
    }

    fn body_schema() -> SchemaDefinition {
        parse_json(
            r#"{"fields": [
                {"key": "body", "type": "BYTES"},
                {"key": "topic", "type": "STRING"}
            ]}"#,
        )
        .unwrap()
    }

    fn reading_schema() -> SchemaDefinition {
        parse_json(
            r#"{"fields": [
                {"key": "device", "type": "STRING"},
                {"key": "temp", "type": "F64"},
                {"key": "count", "type": "U64"}
            ]}"#,
        )
        .unwrap()
    }

    fn body(text: &str) -> PluginPayload {
        Value::Object(vec![
            ("body".into(), Value::Bytes(text.as_bytes().to_vec())),
            ("topic".into(), Value::String("t".into())),
        ])
        .into_payload()
        .unwrap()
    }

    fn apply(mapping: &Arc<dyn Mapping>, input: &PluginPayload) -> Result<Value, String> {
        mapping
            .apply(input, None)
            .map(|p| Value::from_payload(&p.expect("not filtered")))
    }

    #[test]
    fn decodes_json_bodies() {
        let (from, to) = (body_schema(), reading_schema());
        let schemas = Schemas {
            source: Some(&from),
            target: Some(&to),
        };
        // the whole body becomes the output
        let whole = compile(
            &spec(json!({"mode": "replace", "rules": [{"target": ".", "source": "body", "decode": "json"}]})),
            schemas,
        )
        .unwrap();
        let out = apply(
            &whole,
            &body(r#"{"device": "d1", "temp": 21.5, "count": 3, "extra": 1}"#),
        );
        assert_eq!(
            out,
            Ok(Value::Object(vec![
                ("device".into(), Value::String("d1".into())),
                ("temp".into(), Value::F64(21.5)),
                ("count".into(), Value::U64(3)),
            ]))
        );
        // bad JSON, or the wrong shape: a mapping error
        assert!(
            apply(&whole, &body("{nope"))
                .unwrap_err()
                .contains("body: not JSON")
        );
        let e = apply(&whole, &body("[1]")).unwrap_err();
        assert!(e.contains("can't be a OBJECT"), "{e}");
        assert!(apply(&whole, &body(r#"{"temp": "warm"}"#)).is_err());
        // into one field
        let one = compile(
            &spec(json!({"mode": "replace", "rules": [
                {"target": "device", "source": "topic"},
                {"target": "temp", "source": "body", "decode": "json"}
            ]})),
            schemas,
        )
        .unwrap();
        let out = apply(&one, &body("19")).unwrap();
        assert_eq!(
            out,
            Value::Object(vec![
                ("device".into(), Value::String("t".into())),
                ("temp".into(), Value::F64(19.0))
            ])
        );
    }

    #[test]
    fn encodes_values_as_json() {
        let (from, to) = (reading_schema(), body_schema());
        let schemas = Schemas {
            source: Some(&from),
            target: Some(&to),
        };
        let mapping = compile(
            &spec(json!({"mode": "replace", "rules": [
                {"target": "body", "source": ".", "encode": "json"},
                {"target": "topic", "source": "device", "encode": "json"}
            ]})),
            schemas,
        )
        .unwrap();
        let input = Value::Object(vec![
            ("device".into(), Value::String("d1".into())),
            ("temp".into(), Value::F64(21.5)),
            ("count".into(), Value::U64(u64::MAX)),
        ])
        .into_payload()
        .unwrap();
        let Value::Object(out) = apply(&mapping, &input).unwrap() else {
            panic!()
        };
        let Value::Bytes(text) = &out[0].1 else {
            panic!("{out:?}")
        };
        let json: serde_json::Value = serde_json::from_slice(text).unwrap();
        assert_eq!(
            json,
            json!({"device": "d1", "temp": 21.5, "count": 18446744073709551615u64})
        );
        assert_eq!(out[1].1, Value::String("\"d1\"".into()));
    }

    #[test]
    fn codec_rules_are_checked() {
        let (bodies, readings) = (body_schema(), reading_schema());
        let check = |rules: serde_json::Value, mode: Option<&str>| {
            let mut m = json!({"rules": rules});
            if let Some(mode) = mode {
                m["mode"] = json!(mode);
            }
            MappingSpec::parse(&m)
        };
        for (rules, mode, expected) in [
            (
                json!([{"target": ".", "source": "body"}]),
                Some("replace"),
                "only for a decode rule",
            ),
            (
                json!([{"target": ".", "source": "body", "decode": "json"}]),
                None,
                "needs \"mode\": \"replace\"",
            ),
            (
                json!([{"target": "x", "source": ".", "decode": "json"}]),
                None,
                "only for an encode rule",
            ),
            (
                json!([{"target": "x", "value": 1, "decode": "json"}]),
                None,
                "go with a source",
            ),
            (
                json!([{"target": "x", "source": "a", "decode": "json", "encode": "json"}]),
                None,
                "not both",
            ),
            (
                json!([{"target": "x", "source": "a", "decode": "yaml"}]),
                None,
                "unknown variant",
            ),
        ] {
            let errors = check(rules, mode).unwrap_err();
            assert!(
                errors.iter().any(|e| e.contains(expected)),
                "{expected:?} not in {errors:?}"
            );
        }
        // against the schemas: decode only from text, encode only into text
        let errors = compile(
            &spec(json!({"rules": [{"target": "device", "source": "temp", "decode": "json"}]})),
            Schemas {
                source: Some(&readings),
                target: Some(&readings),
            },
        )
        .unwrap_err();
        assert!(
            errors[0].contains("only BYTES or STRING can be decoded"),
            "{errors:?}"
        );
        let errors = compile(
            &spec(json!({"rules": [{"target": "temp", "source": "device", "encode": "json"}]})),
            Schemas {
                source: Some(&readings),
                target: Some(&readings),
            },
        )
        .unwrap_err();
        assert!(
            errors[0].contains("goes into BYTES or STRING"),
            "{errors:?}"
        );
        // no schemas known: accepted
        assert!(
            compile(
                &spec(json!({"rules": [{"target": "x", "source": "body", "decode": "json"}]})),
                Schemas {
                    source: Some(&bodies),
                    target: None
                },
            )
            .is_ok()
        );
    }
}
