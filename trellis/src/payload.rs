//! Payloads in tests: sample payloads from a schema, payload files, schema
//! conformance and matching.
//!
//! Payload files are **plain JSON shaped by the schema**: `{"text": "hi",
//! "n": 3}` for a schema with a STRING `text` and an I32 `n`. The schema
//! gives each value its type: DATETIME from an RFC 3339 string, BYTES from a
//! UTF-8 string or an array of 0–255, packed arrays from number arrays, null
//! for a field without a value. Files named `*.pb.json` are proto3 JSON of a
//! `PluginPayload` instead (exact control over the encoding).

use flwrs_plugin::schema::payload::{
    ArrayValue, Field, FieldDefinition, FieldValue, MapValue, PackedF32, PackedF64, PackedI32,
    PackedI64, PackedU32, PackedU64, PluginPayload, SchemaDefinition,
    field_type::Enum as FieldType, field_value::Value,
};
use serde_json::{Map, Value as Json};
use std::path::Path;

/// The deepest nesting accepted (as in the protocol).
const MAX_DEPTH: usize = 32;

/// A payload with a sample value for every field of `schema`.
pub fn sample_payload(schema: &SchemaDefinition) -> PluginPayload {
    PluginPayload {
        fields: schema
            .fields
            .iter()
            .map(|def| Field {
                key: def.key.clone(),
                value: Some(sample_value(def, 0)),
            })
            .collect(),
    }
}

fn sample_value(def: &FieldDefinition, depth: usize) -> FieldValue {
    let value = match def.r#type() {
        FieldType::String if def.key == "url" => {
            Some(Value::String("http://127.0.0.1:9/conformance".into()))
        }
        FieldType::String if def.key == "method" => Some(Value::String("POST".into())),
        FieldType::String => Some(Value::String("conformance".into())),
        FieldType::I32 => Some(Value::I32(-1)),
        FieldType::I64 => Some(Value::I64(-1)),
        FieldType::U32 => Some(Value::U32(1)),
        FieldType::U64 => Some(Value::U64(1)),
        FieldType::F32 => Some(Value::F32(0.5)),
        FieldType::F64 => Some(Value::F64(0.5)),
        FieldType::Bool => Some(Value::Bool(true)),
        FieldType::Datetime => Some(Value::DateTime(Default::default())),
        FieldType::Bytes => Some(Value::Bytes(b"conformance".to_vec())),
        FieldType::Array => def
            .nested_type_definition
            .as_deref()
            .filter(|_| depth < 8)
            .map(|nested| {
                Value::Array(ArrayValue {
                    value: vec![sample_value(nested, depth + 1)],
                })
            }),
        FieldType::Map => def
            .nested_type_definition
            .as_deref()
            .filter(|_| depth < 8)
            .map(|nested| {
                Value::Map(MapValue {
                    value: [("key".to_string(), sample_value(nested, depth + 1))].into(),
                })
            }),
        FieldType::Object => Some(Value::Object(MapValue {
            value: def
                .object_fields
                .iter()
                .filter(|_| depth < 8)
                .map(|f| (f.key.clone(), sample_value(f, depth + 1)))
                .collect(),
        })),
        FieldType::PackedF32 => Some(Value::PackedF32(PackedF32 {
            values: vec![0.5, 1.5],
        })),
        FieldType::PackedF64 => Some(Value::PackedF64(PackedF64 {
            values: vec![0.5, 1.5],
        })),
        FieldType::PackedI32 => Some(Value::PackedI32(PackedI32 {
            values: vec![-1, 1],
        })),
        FieldType::PackedI64 => Some(Value::PackedI64(PackedI64 {
            values: vec![-1, 1],
        })),
        FieldType::PackedU32 => Some(Value::PackedU32(PackedU32 { values: vec![1, 2] })),
        FieldType::PackedU64 => Some(Value::PackedU64(PackedU64 { values: vec![1, 2] })),
        FieldType::Undefined => None,
    };
    FieldValue { value }
}

/// Reads a payload file: plain JSON shaped by `schema`, or proto3 JSON for `*.pb.json`.
pub fn load(path: &Path, schema: &SchemaDefinition) -> Result<PluginPayload, String> {
    load_with(path, schema, str::to_string)
}

/// [`load`], with the file's text passed through `prepare` first (variables).
pub fn load_with(
    path: &Path,
    schema: &SchemaDefinition,
    prepare: impl Fn(&str) -> String,
) -> Result<PluginPayload, String> {
    let text =
        prepare(&std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?);
    let result = if path.to_string_lossy().ends_with(".pb.json") {
        serde_json::from_str::<PluginPayload>(&text).map_err(|e| e.to_string())
    } else {
        serde_json::from_str::<Json>(&text)
            .map_err(|e| e.to_string())
            .and_then(|json| from_plain(&json, schema))
    };
    result.map_err(|e| format!("{}: {e}", path.display()))
}

/// Plain JSON (an object) → a payload, typed by `schema`. Top-level fields
/// follow the schema's order; keys the schema doesn't declare are an error.
pub fn from_plain(json: &Json, schema: &SchemaDefinition) -> Result<PluginPayload, String> {
    let Json::Object(map) = json else {
        return Err("a payload must be a JSON object".into());
    };
    let mut errors = vec![];
    for key in map.keys() {
        if !schema.fields.iter().any(|d| &d.key == key) {
            errors.push(format!("{key}: not in the schema"));
        }
    }
    let mut fields = vec![];
    for def in &schema.fields {
        if let Some(value) = map.get(&def.key) {
            match value_from(value, def, &def.key, 0) {
                Ok(value) => fields.push(Field {
                    key: def.key.clone(),
                    value: Some(value),
                }),
                Err(e) => errors.push(e),
            }
        }
    }
    if errors.is_empty() {
        Ok(PluginPayload { fields })
    } else {
        Err(errors.join("; "))
    }
}

fn value_from(
    json: &Json,
    def: &FieldDefinition,
    at: &str,
    depth: usize,
) -> Result<FieldValue, String> {
    if depth > MAX_DEPTH {
        return Err(format!("{at}: nested deeper than {MAX_DEPTH} levels"));
    }
    if json.is_null() {
        return Ok(FieldValue { value: None });
    }
    let wrong = || format!("{at}: {json} isn't a {}", def.r#type().as_str_name());
    let nested = || {
        def.nested_type_definition
            .as_deref()
            .ok_or_else(|| format!("{at}: the schema has no nested type"))
    };
    let int = |min: i128, max: i128| -> Result<i128, String> {
        let n = json
            .as_i64()
            .map(i128::from)
            .or_else(|| json.as_u64().map(i128::from))
            .ok_or_else(wrong)?;
        if n < min || n > max {
            return Err(format!("{at}: {n} is out of range"));
        }
        Ok(n)
    };
    let numbers = || -> Result<Vec<&Json>, String> {
        json.as_array()
            .map(|a| a.iter().collect())
            .ok_or_else(wrong)
    };
    let value = match def.r#type() {
        FieldType::String => Value::String(json.as_str().ok_or_else(wrong)?.to_string()),
        FieldType::I32 => Value::I32(int(i32::MIN.into(), i32::MAX.into())? as i32),
        FieldType::I64 => Value::I64(int(i64::MIN.into(), i64::MAX.into())? as i64),
        FieldType::U32 => Value::U32(int(0, u32::MAX.into())? as u32),
        FieldType::U64 => Value::U64(int(0, u64::MAX.into())? as u64),
        FieldType::F32 => Value::F32(json.as_f64().ok_or_else(wrong)? as f32),
        FieldType::F64 => Value::F64(json.as_f64().ok_or_else(wrong)?),
        FieldType::Bool => Value::Bool(json.as_bool().ok_or_else(wrong)?),
        FieldType::Datetime => Value::DateTime(
            serde_json::from_value(json.clone())
                .map_err(|e| format!("{at}: not an RFC 3339 datetime: {e}"))?,
        ),
        FieldType::Bytes => match json {
            Json::String(s) => Value::Bytes(s.as_bytes().to_vec()),
            Json::Array(items) => Value::Bytes(
                items
                    .iter()
                    .map(|b| b.as_u64().filter(|b| *b <= 255).map(|b| b as u8))
                    .collect::<Option<Vec<u8>>>()
                    .ok_or_else(|| format!("{at}: bytes must be 0–255"))?,
            ),
            _ => return Err(wrong()),
        },
        FieldType::Array => {
            let def = nested()?;
            let items = json.as_array().ok_or_else(wrong)?;
            Value::Array(ArrayValue {
                value: items
                    .iter()
                    .enumerate()
                    .map(|(i, v)| value_from(v, def, &format!("{at}[{i}]"), depth + 1))
                    .collect::<Result<_, _>>()?,
            })
        }
        FieldType::Map => {
            let def = nested()?;
            let map = json.as_object().ok_or_else(wrong)?;
            Value::Map(MapValue {
                value: map
                    .iter()
                    .map(|(k, v)| {
                        Ok((
                            k.clone(),
                            value_from(v, def, &format!("{at}.{k}"), depth + 1)?,
                        ))
                    })
                    .collect::<Result<_, String>>()?,
            })
        }
        FieldType::Object => {
            let map = json.as_object().ok_or_else(wrong)?;
            let mut value = std::collections::HashMap::new();
            for (k, v) in map {
                let field = def
                    .object_fields
                    .iter()
                    .find(|f| &f.key == k)
                    .ok_or_else(|| format!("{at}.{k}: not in the schema"))?;
                value.insert(
                    k.clone(),
                    value_from(v, field, &format!("{at}.{k}"), depth + 1)?,
                );
            }
            Value::Object(MapValue { value })
        }
        FieldType::PackedF32 => Value::PackedF32(PackedF32 {
            values: numbers()?
                .iter()
                .map(|n| n.as_f64().map(|f| f as f32))
                .collect::<Option<_>>()
                .ok_or_else(wrong)?,
        }),
        FieldType::PackedF64 => Value::PackedF64(PackedF64 {
            values: numbers()?
                .iter()
                .map(|n| n.as_f64())
                .collect::<Option<_>>()
                .ok_or_else(wrong)?,
        }),
        FieldType::PackedI32 => Value::PackedI32(PackedI32 {
            values: numbers()?
                .iter()
                .map(|n| n.as_i64().and_then(|n| i32::try_from(n).ok()))
                .collect::<Option<_>>()
                .ok_or_else(wrong)?,
        }),
        FieldType::PackedI64 => Value::PackedI64(PackedI64 {
            values: numbers()?
                .iter()
                .map(|n| n.as_i64())
                .collect::<Option<_>>()
                .ok_or_else(wrong)?,
        }),
        FieldType::PackedU32 => Value::PackedU32(PackedU32 {
            values: numbers()?
                .iter()
                .map(|n| n.as_u64().and_then(|n| u32::try_from(n).ok()))
                .collect::<Option<_>>()
                .ok_or_else(wrong)?,
        }),
        FieldType::PackedU64 => Value::PackedU64(PackedU64 {
            values: numbers()?
                .iter()
                .map(|n| n.as_u64())
                .collect::<Option<_>>()
                .ok_or_else(wrong)?,
        }),
        FieldType::Undefined => return Err(format!("{at}: the schema's type is UNDEFINED")),
    };
    Ok(FieldValue { value: Some(value) })
}

/// A payload → plain JSON (the format of payload files). BYTES become a
/// string when they're valid UTF-8, else an array of numbers.
pub fn to_plain(payload: &PluginPayload) -> Json {
    Json::Object(
        payload
            .fields
            .iter()
            .map(|f| {
                (
                    f.key.clone(),
                    f.value.as_ref().map(plain).unwrap_or(Json::Null),
                )
            })
            .collect(),
    )
}

fn plain(value: &FieldValue) -> Json {
    let Some(value) = &value.value else {
        return Json::Null;
    };
    let map = |m: &MapValue| -> Json {
        let mut keys: Vec<&String> = m.value.keys().collect();
        keys.sort();
        Json::Object(
            keys.into_iter()
                .map(|k| (k.clone(), plain(&m.value[k])))
                .collect::<Map<_, _>>(),
        )
    };
    match value {
        Value::String(s) => Json::from(s.clone()),
        Value::I32(n) => Json::from(*n),
        Value::I64(n) => Json::from(*n),
        Value::U32(n) => Json::from(*n),
        Value::U64(n) => Json::from(*n),
        Value::F32(n) => Json::from(f64::from(*n)),
        Value::F64(n) => Json::from(*n),
        Value::Bool(b) => Json::from(*b),
        Value::DateTime(ts) => serde_json::to_value(ts).unwrap_or(Json::Null),
        Value::Bytes(b) => match std::str::from_utf8(b) {
            Ok(s) => Json::from(s),
            Err(_) => Json::from(b.clone()),
        },
        Value::Array(a) => Json::Array(a.value.iter().map(plain).collect()),
        Value::Map(m) | Value::Object(m) => map(m),
        Value::PackedF32(p) => {
            Json::from(p.values.iter().map(|v| f64::from(*v)).collect::<Vec<_>>())
        }
        Value::PackedF64(p) => Json::from(p.values.clone()),
        Value::PackedI32(p) => Json::from(p.values.clone()),
        Value::PackedI64(p) => Json::from(p.values.clone()),
        Value::PackedU32(p) => Json::from(p.values.clone()),
        Value::PackedU64(p) => Json::from(p.values.clone()),
    }
}

/// Problems with `payload` against `schema`: undeclared or repeated fields,
/// values of the wrong type. Missing fields and nulls are allowed.
pub fn conforms(payload: &PluginPayload, schema: &SchemaDefinition) -> Vec<String> {
    let mut problems = vec![];
    let mut seen = std::collections::HashSet::new();
    for field in &payload.fields {
        if !seen.insert(&field.key) {
            problems.push(format!("{}: repeated", field.key));
        }
        match schema.fields.iter().find(|d| d.key == field.key) {
            None => problems.push(format!("{}: not in the schema", field.key)),
            Some(def) => {
                if let Some(value) = &field.value {
                    check_value(value, def, &field.key, &mut problems);
                }
            }
        }
    }
    problems
}

fn check_value(value: &FieldValue, def: &FieldDefinition, at: &str, problems: &mut Vec<String>) {
    let Some(v) = &value.value else { return };
    let expected = def.r#type();
    let actual = match v {
        Value::String(_) => FieldType::String,
        Value::I32(_) => FieldType::I32,
        Value::I64(_) => FieldType::I64,
        Value::U32(_) => FieldType::U32,
        Value::U64(_) => FieldType::U64,
        Value::F32(_) => FieldType::F32,
        Value::F64(_) => FieldType::F64,
        Value::Bool(_) => FieldType::Bool,
        Value::DateTime(_) => FieldType::Datetime,
        Value::Bytes(_) => FieldType::Bytes,
        Value::Array(_) => FieldType::Array,
        Value::Map(_) => FieldType::Map,
        Value::Object(_) => FieldType::Object,
        Value::PackedF32(_) => FieldType::PackedF32,
        Value::PackedF64(_) => FieldType::PackedF64,
        Value::PackedI32(_) => FieldType::PackedI32,
        Value::PackedI64(_) => FieldType::PackedI64,
        Value::PackedU32(_) => FieldType::PackedU32,
        Value::PackedU64(_) => FieldType::PackedU64,
    };
    if actual != expected {
        problems.push(format!(
            "{at}: a {} value, the schema says {}",
            actual.as_str_name(),
            expected.as_str_name()
        ));
        return;
    }
    match v {
        Value::Array(a) => {
            if let Some(nested) = def.nested_type_definition.as_deref() {
                for (i, item) in a.value.iter().enumerate() {
                    check_value(item, nested, &format!("{at}[{i}]"), problems);
                }
            }
        }
        Value::Map(m) => {
            if let Some(nested) = def.nested_type_definition.as_deref() {
                for (k, item) in &m.value {
                    check_value(item, nested, &format!("{at}.{k}"), problems);
                }
            }
        }
        Value::Object(m) => {
            for (k, item) in &m.value {
                match def.object_fields.iter().find(|f| &f.key == k) {
                    Some(field) => check_value(item, field, &format!("{at}.{k}"), problems),
                    None => problems.push(format!("{at}.{k}: not in the schema")),
                }
            }
        }
        _ => {}
    }
}

/// Exact match: the same fields (in any order) with equal values.
pub fn matches_exact(expected: &PluginPayload, actual: &PluginPayload) -> Result<(), String> {
    let mut problems = vec![];
    for f in &expected.fields {
        match actual.fields.iter().find(|a| a.key == f.key) {
            None => problems.push(format!("{}: missing", f.key)),
            Some(a) if a.value != f.value => problems.push(format!(
                "{}: {}, expected {}",
                f.key,
                a.value.as_ref().map(plain).unwrap_or(Json::Null),
                f.value.as_ref().map(plain).unwrap_or(Json::Null)
            )),
            Some(_) => {}
        }
    }
    for a in &actual.fields {
        if !expected.fields.iter().any(|f| f.key == a.key) {
            problems.push(format!("{}: not expected", a.key));
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems.join("; "))
    }
}

/// Subset match: every expected field is there with an equal value; objects
/// and maps compare their expected keys only (recursively), the rest exactly.
pub fn matches_subset(expected: &PluginPayload, actual: &PluginPayload) -> Result<(), String> {
    let mut problems = vec![];
    for f in &expected.fields {
        match actual.fields.iter().find(|a| a.key == f.key) {
            None => problems.push(format!("{}: missing", f.key)),
            Some(a) => subset_value(f.value.as_ref(), a.value.as_ref(), &f.key, &mut problems),
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems.join("; "))
    }
}

fn subset_value(
    expected: Option<&FieldValue>,
    actual: Option<&FieldValue>,
    at: &str,
    problems: &mut Vec<String>,
) {
    let inner = |v: Option<&FieldValue>| v.and_then(|v| v.value.as_ref()).cloned();
    match (inner(expected), inner(actual)) {
        (Some(Value::Object(e)), Some(Value::Object(a)))
        | (Some(Value::Map(e)), Some(Value::Map(a))) => {
            for (k, ev) in &e.value {
                match a.value.get(k) {
                    None => problems.push(format!("{at}.{k}: missing")),
                    Some(av) => subset_value(Some(ev), Some(av), &format!("{at}.{k}"), problems),
                }
            }
        }
        (e, a) if e == a => {}
        (e, a) => problems.push(format!(
            "{at}: {}, expected {}",
            a.map(|v| plain(&FieldValue { value: Some(v) }))
                .unwrap_or(Json::Null),
            e.map(|v| plain(&FieldValue { value: Some(v) }))
                .unwrap_or(Json::Null)
        )),
    }
}

/// The value at a dot path in plain JSON (`a.b`, `a[0]`, `a.b[2].c`).
pub fn at_path<'a>(json: &'a Json, path: &str) -> Option<&'a Json> {
    let mut current = json;
    for step in path_steps(path)? {
        current = match step {
            Step::Key(key) => current.get(key)?,
            Step::Index(n) => current.get(n)?,
        };
    }
    Some(current)
}

enum Step {
    Key(String),
    Index(usize),
}

/// `a.b`, `a[0]`, `a["any key"]` (quoted keys may hold `.`, `/`, `{`, …;
/// `\"` and `\\` inside them): the dot paths flwrs mappings use.
fn path_steps(path: &str) -> Option<Vec<Step>> {
    let mut steps = vec![];
    let mut chars = path.chars().peekable();
    let mut key = String::new();
    while let Some(c) = chars.next() {
        match c {
            '.' => {
                if !key.is_empty() {
                    steps.push(Step::Key(std::mem::take(&mut key)));
                }
            }
            '[' => {
                if !key.is_empty() {
                    steps.push(Step::Key(std::mem::take(&mut key)));
                }
                if chars.peek() == Some(&'"') {
                    chars.next();
                    let mut quoted = String::new();
                    loop {
                        match chars.next()? {
                            '\\' => quoted.push(chars.next()?),
                            '"' => break,
                            c => quoted.push(c),
                        }
                    }
                    if chars.next()? != ']' {
                        return None;
                    }
                    steps.push(Step::Key(quoted));
                } else {
                    let mut digits = String::new();
                    loop {
                        match chars.next()? {
                            ']' => break,
                            c => digits.push(c),
                        }
                    }
                    steps.push(Step::Index(digits.parse().ok()?));
                }
            }
            c => key.push(c),
        }
    }
    if !key.is_empty() {
        steps.push(Step::Key(key));
    }
    Some(steps)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_with_quoted_keys() {
        let json = serde_json::json!({"paths": {"/devices/{id}": {"post": {"body": {"temp": 21.5}}}}, "a": [{"b": 1}], "x.y": 2});
        assert_eq!(
            at_path(&json, r#"paths["/devices/{id}"].post.body.temp"#),
            Some(&serde_json::json!(21.5))
        );
        assert_eq!(at_path(&json, "a[0].b"), Some(&serde_json::json!(1)));
        assert_eq!(at_path(&json, r#"["x.y"]"#), Some(&serde_json::json!(2)));
        assert_eq!(at_path(&json, "a[x]"), None);
        assert_eq!(at_path(&json, r#"paths["/devices"#), None);
    }
    use flwrs_plugin::schema::rules::parse_json;
    use serde_json::json;

    fn schema() -> SchemaDefinition {
        parse_json(
            r#"{"fields": [
                {"key": "text", "type": "STRING"},
                {"key": "n", "type": "I32"},
                {"key": "big", "type": "U64"},
                {"key": "when", "type": "DATETIME"},
                {"key": "raw", "type": "BYTES"},
                {"key": "tags", "type": "ARRAY", "nested_type_definition": {"type": "STRING"}},
                {"key": "attrs", "type": "MAP", "nested_type_definition": {"type": "F64"}},
                {"key": "pos", "type": "OBJECT", "object_fields": [
                    {"key": "x", "type": "F32"}, {"key": "y", "type": "F32"}]},
                {"key": "vec", "type": "PACKED_F32"}
            ]}"#,
        )
        .unwrap()
    }

    #[test]
    fn plain_json_round_trips_through_the_schema() {
        let json = json!({
            "text": "hi", "n": -3, "big": 18446744073709551615u64,
            "when": "2026-11-14T10:00:00+00:00", "raw": [0, 159, 255],
            "tags": ["a", "b"], "attrs": {"t": 1.5}, "pos": {"x": 1.0, "y": 2.0},
            "vec": [0.5, 1.5]
        });
        let payload = from_plain(&json, &schema()).unwrap();
        assert!(conforms(&payload, &schema()).is_empty());
        assert_eq!(to_plain(&payload), json);
        assert_eq!(from_plain(&to_plain(&payload), &schema()).unwrap(), payload);
    }

    #[test]
    fn plain_json_errors_are_reported() {
        let err = from_plain(&json!({"n": 1.5, "nope": 1, "big": -1}), &schema()).unwrap_err();
        for part in [
            "nope: not in the schema",
            "n: 1.5 isn't a I32",
            "big: -1 is out of range",
        ] {
            assert!(err.contains(part), "{part}: {err}");
        }
    }

    #[test]
    fn matching_and_paths() {
        let s = schema();
        let actual = from_plain(
            &json!({"text": "hi", "n": 3, "pos": {"x": 1.0, "y": 2.0}}),
            &s,
        )
        .unwrap();
        let subset = from_plain(&json!({"pos": {"x": 1.0}}), &s).unwrap();
        assert!(matches_subset(&subset, &actual).is_ok());
        assert!(matches_exact(&subset, &actual).is_err());
        let wrong = from_plain(&json!({"pos": {"x": 9.0}}), &s).unwrap();
        assert!(
            matches_subset(&wrong, &actual)
                .unwrap_err()
                .contains("pos.x")
        );
        let plain = json!({"a": {"b": [1, {"c": 2}]}});
        assert_eq!(at_path(&plain, "a.b[1].c"), Some(&json!(2)));
        assert_eq!(at_path(&plain, "a.x"), None);
    }

    #[test]
    fn conformance_finds_wrong_types() {
        let payload = PluginPayload {
            fields: vec![
                Field {
                    key: "text".into(),
                    value: Some(FieldValue {
                        value: Some(Value::I32(1)),
                    }),
                },
                Field {
                    key: "other".into(),
                    value: None,
                },
            ],
        };
        let problems = conforms(&payload, &schema());
        assert!(
            problems
                .iter()
                .any(|p| p.contains("text: a I32 value, the schema says STRING"))
        );
        assert!(
            problems
                .iter()
                .any(|p| p.contains("other: not in the schema"))
        );
    }
}
