//! The JSON codec: values to JSON text and back. Used by edge mappings
//! (`"encode": "json"` / `"decode": "json"` on copy rules) and by Lua's
//! `json` table.
//!
//! **Encoding:** numbers stay exact (64-bit integers are JSON numbers, not
//! strings; a reader in JavaScript loses precision above 2^53), datetimes
//! are RFC 3339 in UTC, bytes are standard base64, packed arrays are arrays,
//! maps and objects are objects. NaN and infinities can't be JSON.
//!
//! **Decoding:** shaped by the target field when it's known (the same rules
//! as mapping constants, except that a string for a BYTES field is base64, and
//! object keys the target doesn't declare are dropped),
//! else plain: numbers as I64, U64 or F64, arrays, objects. At most
//! [`MAX_DEPTH`] levels.

use crate::value::{self, Value};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use flwrs_plugin::schema::payload::FieldDefinition;
use serde_json::Value as J;

/// Nesting limit of decoded and encoded JSON, as for payloads.
pub const MAX_DEPTH: usize = 32;

/// A value as JSON.
pub fn to_json(value: &Value) -> Result<J, String> {
    to_json_at(value, 0)
}

fn to_json_at(value: &Value, depth: usize) -> Result<J, String> {
    if depth > MAX_DEPTH {
        return Err(format!("nested deeper than {MAX_DEPTH} levels"));
    }
    let float = |f: f64| {
        serde_json::Number::from_f64(f)
            .map(J::Number)
            .ok_or_else(|| format!("{f} can't be JSON"))
    };
    Ok(match value {
        Value::Null => J::Null,
        Value::String(s) => J::String(s.clone()),
        Value::Bool(b) => J::Bool(*b),
        Value::I32(n) => J::from(*n),
        Value::I64(n) => J::from(*n),
        Value::U32(n) => J::from(*n),
        Value::U64(n) => J::from(*n),
        Value::F32(n) => float(f64::from(*n))?,
        Value::F64(n) => float(*n)?,
        Value::DateTime(t) => {
            let time = chrono::DateTime::from_timestamp(t.seconds, t.nanos.max(0) as u32)
                .ok_or_else(|| format!("datetime {}s is out of range", t.seconds))?;
            J::String(time.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true))
        }
        Value::Bytes(b) => J::String(BASE64.encode(b)),
        Value::Array(items) => J::Array(
            items
                .iter()
                .map(|v| to_json_at(v, depth + 1))
                .collect::<Result<_, _>>()?,
        ),
        Value::Packed(p) => J::Array(
            p.to_values()
                .iter()
                .map(|v| to_json_at(v, depth + 1))
                .collect::<Result<_, _>>()?,
        ),
        Value::Map(map) => J::Object(
            map.iter()
                .map(|(k, v)| Ok((k.clone(), to_json_at(v, depth + 1)?)))
                .collect::<Result<_, String>>()?,
        ),
        Value::Object(fields) => J::Object(
            fields
                .iter()
                .map(|(k, v)| Ok((k.clone(), to_json_at(v, depth + 1)?)))
                .collect::<Result<_, String>>()?,
        ),
    })
}

/// A value as JSON text.
pub fn encode(value: &Value) -> Result<String, String> {
    serde_json::to_string(&to_json(value)?).map_err(|e| e.to_string())
}

/// JSON text as a value, shaped by `def` when it's known.
pub fn decode(text: &[u8], def: Option<&FieldDefinition>) -> Result<Value, String> {
    let json: J = serde_json::from_slice(text).map_err(|e| format!("not JSON: {e}"))?;
    if depth(&json) > MAX_DEPTH {
        return Err(format!("JSON nested deeper than {MAX_DEPTH} levels"));
    }
    value::from_json_base64(&json, def)
}

fn depth(json: &J) -> usize {
    match json {
        J::Array(items) => 1 + items.iter().map(depth).max().unwrap_or(0),
        J::Object(map) => 1 + map.values().map(depth).max().unwrap_or(0),
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flwrs_plugin::schema::rules::parse_json;
    use pbjson_types::Timestamp;
    use serde_json::json;
    use std::collections::BTreeMap;

    #[test]
    fn encodes_every_type() {
        let value = Value::Object(vec![
            ("s".into(), Value::String("hi".into())),
            ("big".into(), Value::U64(u64::MAX)),
            ("neg".into(), Value::I64(-5)),
            ("f".into(), Value::F64(1.5)),
            (
                "when".into(),
                Value::DateTime(Timestamp {
                    seconds: 1_767_225_600,
                    nanos: 500_000_000,
                }),
            ),
            ("raw".into(), Value::Bytes(vec![0, 255, 1])),
            ("none".into(), Value::Null),
            ("list".into(), Value::Array(vec![Value::Bool(true)])),
            (
                "tags".into(),
                Value::Map(BTreeMap::from([("a".into(), Value::I32(1))])),
            ),
        ]);
        let json: J = serde_json::from_str(&encode(&value).unwrap()).unwrap();
        assert_eq!(
            json,
            json!({"s": "hi", "big": 18446744073709551615u64, "neg": -5, "f": 1.5,
                   "when": "2026-01-01T00:00:00.500Z", "raw": "AP8B", "none": null,
                   "list": [true], "tags": {"a": 1}})
        );
        assert!(encode(&Value::F64(f64::NAN)).is_err());
    }

    #[test]
    fn decodes_shaped_by_the_target() {
        let schema = parse_json(
            r#"{"fields": [{"key": "doc", "type": "OBJECT", "object_fields": [
                {"key": "n", "type": "U64"}, {"key": "raw", "type": "BYTES"},
                {"key": "when", "type": "DATETIME"}, {"key": "xs", "type": "ARRAY",
                 "nested_type_definition": {"type": "I32"}}]}]}"#,
        )
        .unwrap();
        let def = &schema.fields[0];
        let v = decode(
            br#"{"n": 18446744073709551615, "raw": "AP8B", "when": "2026-01-01T00:00:00Z", "xs": [1, 2]}"#,
            Some(def),
        )
        .unwrap();
        let Value::Object(fields) = v else { panic!() };
        assert_eq!(fields[0], ("n".into(), Value::U64(u64::MAX)));
        assert_eq!(fields[1], ("raw".into(), Value::Bytes(vec![0, 255, 1])));
        assert!(matches!(fields[2].1, Value::DateTime(_)));
        assert_eq!(
            fields[3].1,
            Value::Array(vec![Value::I32(1), Value::I32(2)])
        );
        // a round trip through encode keeps everything
        assert_eq!(
            decode(
                encode(&Value::Object(fields.clone())).unwrap().as_bytes(),
                Some(def)
            )
            .unwrap(),
            Value::Object(fields)
        );
        // plain, without a target
        assert_eq!(
            decode(b"[1, -1, 1.5, \"x\"]", None).unwrap(),
            Value::Array(vec![
                Value::I64(1),
                Value::I64(-1),
                Value::F64(1.5),
                Value::String("x".into())
            ])
        );
        assert!(decode(b"{oops", None).unwrap_err().starts_with("not JSON"));
        assert!(decode(br#"{"n": "x", "raw": "AP8B"}"#, Some(def)).is_err());
        assert!(decode(br#"{"raw": "not base64!"}"#, Some(def)).is_err());
        let deep = "[".repeat(MAX_DEPTH + 1) + &"]".repeat(MAX_DEPTH + 1);
        assert!(
            decode(deep.as_bytes(), None)
                .unwrap_err()
                .contains("deeper")
        );
    }
}
