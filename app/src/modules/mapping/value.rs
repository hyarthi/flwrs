//! The typed value tree mappings work on. It mirrors `FieldValue` (so bytes,
//! datetimes and the difference between u64 and i64 survive, unlike JSON) and
//! converts both ways with `PluginPayload`.

use flwrs_plugin::schema::payload::{
    ArrayValue, Field, FieldDefinition, FieldValue, MapValue, PackedF32, PackedF64, PackedI32,
    PackedI64, PackedU32, PackedU64, PluginPayload, SchemaDefinition,
    field_type::Enum as FieldType, field_value::Value as Pb,
};
use pbjson_types::Timestamp;
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Value {
    Null,
    String(String),
    Bool(bool),
    I32(i32),
    I64(i64),
    U32(u32),
    U64(u64),
    F32(f32),
    F64(f64),
    DateTime(Timestamp),
    Bytes(Vec<u8>),
    Array(Vec<Value>),
    /// Homogeneous map (`MAP`).
    Map(BTreeMap<String, Value>),
    /// Named fields (`OBJECT`, and the payload itself), in order.
    Object(Vec<(String, Value)>),
    /// A packed numeric array (`PACKED_*`).
    Packed(Packed),
}

/// The packed numeric arrays (`PACKED_F32` …).
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Packed {
    F32(Vec<f32>),
    F64(Vec<f64>),
    I32(Vec<i32>),
    I64(Vec<i64>),
    U32(Vec<u32>),
    U64(Vec<u64>),
}

impl Packed {
    pub(crate) fn field_type(&self) -> FieldType {
        match self {
            Packed::F32(_) => FieldType::PackedF32,
            Packed::F64(_) => FieldType::PackedF64,
            Packed::I32(_) => FieldType::PackedI32,
            Packed::I64(_) => FieldType::PackedI64,
            Packed::U32(_) => FieldType::PackedU32,
            Packed::U64(_) => FieldType::PackedU64,
        }
    }

    pub(crate) fn len(&self) -> usize {
        match self {
            Packed::F32(v) => v.len(),
            Packed::F64(v) => v.len(),
            Packed::I32(v) => v.len(),
            Packed::I64(v) => v.len(),
            Packed::U32(v) => v.len(),
            Packed::U64(v) => v.len(),
        }
    }

    /// Element `i` as a scalar value.
    pub(crate) fn get(&self, i: usize) -> Option<Value> {
        Some(match self {
            Packed::F32(v) => Value::F32(*v.get(i)?),
            Packed::F64(v) => Value::F64(*v.get(i)?),
            Packed::I32(v) => Value::I32(*v.get(i)?),
            Packed::I64(v) => Value::I64(*v.get(i)?),
            Packed::U32(v) => Value::U32(*v.get(i)?),
            Packed::U64(v) => Value::U64(*v.get(i)?),
        })
    }

    /// The elements as scalar values.
    pub(crate) fn to_values(&self) -> Vec<Value> {
        (0..self.len()).filter_map(|i| self.get(i)).collect()
    }

    /// Builds a packed array of `packed` type from scalar values, converting
    /// each losslessly to the element type.
    pub(crate) fn from_values(packed: FieldType, values: Vec<Value>) -> Result<Packed, String> {
        let element = element_type(packed).ok_or("not a packed type")?;
        let def = FieldDefinition {
            r#type: element as i32,
            ..Default::default()
        };
        let mut items = Vec::with_capacity(values.len());
        for (i, v) in values.into_iter().enumerate() {
            items.push(coerce(v, &def).map_err(|e| format!("[{i}]: {e}"))?);
        }
        macro_rules! collect {
            ($variant:ident) => {
                Packed::$variant(
                    items
                        .into_iter()
                        .map(|v| match v {
                            Value::$variant(n) => Ok(n),
                            other => Err(format!("unexpected {}", other.type_name())),
                        })
                        .collect::<Result<_, String>>()?,
                )
            };
        }
        Ok(match element {
            FieldType::F32 => collect!(F32),
            FieldType::F64 => collect!(F64),
            FieldType::I32 => collect!(I32),
            FieldType::I64 => collect!(I64),
            FieldType::U32 => collect!(U32),
            _ => collect!(U64),
        })
    }
}

/// The element type of a packed type (`PACKED_F32` → `F32`).
pub(crate) fn element_type(packed: FieldType) -> Option<FieldType> {
    Some(match packed {
        FieldType::PackedF32 => FieldType::F32,
        FieldType::PackedF64 => FieldType::F64,
        FieldType::PackedI32 => FieldType::I32,
        FieldType::PackedI64 => FieldType::I64,
        FieldType::PackedU32 => FieldType::U32,
        FieldType::PackedU64 => FieldType::U64,
        _ => return None,
    })
}

impl Value {
    pub(crate) fn type_name(&self) -> &'static str {
        match self {
            Value::Null => "null",
            Value::String(_) => "STRING",
            Value::Bool(_) => "BOOL",
            Value::I32(_) => "I32",
            Value::I64(_) => "I64",
            Value::U32(_) => "U32",
            Value::U64(_) => "U64",
            Value::F32(_) => "F32",
            Value::F64(_) => "F64",
            Value::DateTime(_) => "DATETIME",
            Value::Bytes(_) => "BYTES",
            Value::Array(_) => "ARRAY",
            Value::Map(_) => "MAP",
            Value::Object(_) => "OBJECT",
            Value::Packed(p) => p.field_type().as_str_name(),
        }
    }

    /// The payload as an object of its fields.
    pub(crate) fn from_payload(payload: &PluginPayload) -> Self {
        Value::Object(
            payload
                .fields
                .iter()
                .map(|f| (f.key.clone(), from_field_value(f.value.as_ref())))
                .collect(),
        )
    }

    /// Back to a payload; the value must be an object.
    pub(crate) fn into_payload(self) -> Result<PluginPayload, String> {
        match self {
            Value::Object(fields) => Ok(PluginPayload {
                fields: fields
                    .into_iter()
                    .map(|(key, value)| Field {
                        key,
                        value: Some(into_field_value(value)),
                    })
                    .collect(),
            }),
            other => Err(format!(
                "the output must be an object, not {}",
                other.type_name()
            )),
        }
    }
}

fn from_field_value(value: Option<&FieldValue>) -> Value {
    let Some(value) = value.and_then(|v| v.value.as_ref()) else {
        return Value::Null;
    };
    match value {
        Pb::String(s) => Value::String(s.clone()),
        Pb::Bool(b) => Value::Bool(*b),
        Pb::I32(n) => Value::I32(*n),
        Pb::I64(n) => Value::I64(*n),
        Pb::U32(n) => Value::U32(*n),
        Pb::U64(n) => Value::U64(*n),
        Pb::F32(n) => Value::F32(*n),
        Pb::F64(n) => Value::F64(*n),
        Pb::DateTime(t) => Value::DateTime(*t),
        Pb::Bytes(b) => Value::Bytes(b.clone()),
        Pb::Array(a) => Value::Array(a.value.iter().map(|v| from_field_value(Some(v))).collect()),
        Pb::Map(m) => Value::Map(
            m.value
                .iter()
                .map(|(k, v)| (k.clone(), from_field_value(Some(v))))
                .collect(),
        ),
        Pb::PackedF32(p) => Value::Packed(Packed::F32(p.values.clone())),
        Pb::PackedF64(p) => Value::Packed(Packed::F64(p.values.clone())),
        Pb::PackedI32(p) => Value::Packed(Packed::I32(p.values.clone())),
        Pb::PackedI64(p) => Value::Packed(Packed::I64(p.values.clone())),
        Pb::PackedU32(p) => Value::Packed(Packed::U32(p.values.clone())),
        Pb::PackedU64(p) => Value::Packed(Packed::U64(p.values.clone())),
        Pb::Object(m) => {
            // Proto maps have no order: sort, so output is deterministic.
            let mut fields: Vec<(String, Value)> = m
                .value
                .iter()
                .map(|(k, v)| (k.clone(), from_field_value(Some(v))))
                .collect();
            fields.sort_by(|a, b| a.0.cmp(&b.0));
            Value::Object(fields)
        }
    }
}

fn into_field_value(value: Value) -> FieldValue {
    let value = match value {
        Value::Null => None,
        Value::String(s) => Some(Pb::String(s)),
        Value::Bool(b) => Some(Pb::Bool(b)),
        Value::I32(n) => Some(Pb::I32(n)),
        Value::I64(n) => Some(Pb::I64(n)),
        Value::U32(n) => Some(Pb::U32(n)),
        Value::U64(n) => Some(Pb::U64(n)),
        Value::F32(n) => Some(Pb::F32(n)),
        Value::F64(n) => Some(Pb::F64(n)),
        Value::DateTime(t) => Some(Pb::DateTime(t)),
        Value::Bytes(b) => Some(Pb::Bytes(b)),
        Value::Array(items) => Some(Pb::Array(ArrayValue {
            value: items.into_iter().map(into_field_value).collect(),
        })),
        Value::Map(entries) => Some(Pb::Map(MapValue {
            value: entries
                .into_iter()
                .map(|(k, v)| (k, into_field_value(v)))
                .collect(),
        })),
        Value::Object(fields) => Some(Pb::Object(MapValue {
            value: fields
                .into_iter()
                .map(|(k, v)| (k, into_field_value(v)))
                .collect(),
        })),
        Value::Packed(p) => Some(match p {
            Packed::F32(values) => Pb::PackedF32(PackedF32 { values }),
            Packed::F64(values) => Pb::PackedF64(PackedF64 { values }),
            Packed::I32(values) => Pb::PackedI32(PackedI32 { values }),
            Packed::I64(values) => Pb::PackedI64(PackedI64 { values }),
            Packed::U32(values) => Pb::PackedU32(PackedU32 { values }),
            Packed::U64(values) => Pb::PackedU64(PackedU64 { values }),
        }),
    };
    FieldValue { value }
}

/// A JSON constant (a rule's `value`) as a typed value: shaped by the target
/// field's definition when it's known, else inferred (integers as I64, or U64
/// above `i64::MAX`; other numbers as F64; objects as OBJECT).
///
/// JSON has no bytes or datetime: for a BYTES target a string becomes its
/// UTF-8 bytes (or an array of 0–255 numbers is taken as is), and for a
/// DATETIME target a string is parsed as RFC 3339.
pub(crate) fn from_json(
    json: &serde_json::Value,
    def: Option<&FieldDefinition>,
) -> Result<Value, String> {
    use serde_json::Value as J;
    let Some(def) = def else {
        return Ok(match json {
            J::Null => Value::Null,
            J::Bool(b) => Value::Bool(*b),
            J::String(s) => Value::String(s.clone()),
            J::Number(n) => match (n.as_i64(), n.as_u64(), n.as_f64()) {
                (Some(i), _, _) => Value::I64(i),
                (None, Some(u), _) => Value::U64(u),
                (_, _, Some(f)) => Value::F64(f),
                _ => return Err(format!("unsupported number {n}")),
            },
            J::Array(items) => Value::Array(
                items
                    .iter()
                    .map(|v| from_json(v, None))
                    .collect::<Result<_, _>>()?,
            ),
            J::Object(map) => Value::Object(
                map.iter()
                    .map(|(k, v)| Ok((k.clone(), from_json(v, None)?)))
                    .collect::<Result<_, String>>()?,
            ),
        });
    };
    if json.is_null() {
        return Ok(Value::Null);
    }
    let field_type = def.r#type();
    let mismatch = || format!("{json} can't be a {}", field_type.as_str_name());
    match field_type {
        FieldType::String => json
            .as_str()
            .map(|s| Value::String(s.into()))
            .ok_or_else(mismatch),
        FieldType::Bool => json.as_bool().map(Value::Bool).ok_or_else(mismatch),
        FieldType::Datetime => {
            let s = json.as_str().ok_or_else(mismatch)?;
            let time = chrono::DateTime::parse_from_rfc3339(s)
                .map_err(|e| format!("{s:?} isn't an RFC 3339 datetime: {e}"))?;
            Ok(Value::DateTime(Timestamp {
                seconds: time.timestamp(),
                nanos: time.timestamp_subsec_nanos() as i32,
            }))
        }
        FieldType::Bytes => match json {
            serde_json::Value::String(s) => Ok(Value::Bytes(s.as_bytes().to_vec())),
            serde_json::Value::Array(items) => items
                .iter()
                .map(|v| {
                    v.as_u64()
                        .and_then(|n| u8::try_from(n).ok())
                        .ok_or_else(mismatch)
                })
                .collect::<Result<Vec<u8>, _>>()
                .map(Value::Bytes),
            _ => Err(mismatch()),
        },
        FieldType::Array => {
            let items = json.as_array().ok_or_else(mismatch)?;
            let nested = def.nested_type_definition.as_deref();
            Ok(Value::Array(
                items
                    .iter()
                    .map(|v| from_json(v, nested))
                    .collect::<Result<_, _>>()?,
            ))
        }
        FieldType::Map => {
            let map = json.as_object().ok_or_else(mismatch)?;
            let nested = def.nested_type_definition.as_deref();
            Ok(Value::Map(
                map.iter()
                    .map(|(k, v)| Ok((k.clone(), from_json(v, nested)?)))
                    .collect::<Result<_, String>>()?,
            ))
        }
        FieldType::Object => {
            let map = json.as_object().ok_or_else(mismatch)?;
            let mut fields = vec![];
            for (key, v) in map {
                let field = def
                    .object_fields
                    .iter()
                    .find(|f| f.key == *key)
                    .ok_or_else(|| format!("{key:?} isn't a field of this object"))?;
                fields.push((key.clone(), from_json(v, Some(field))?));
            }
            Ok(Value::Object(fields))
        }
        FieldType::I32
        | FieldType::I64
        | FieldType::U32
        | FieldType::U64
        | FieldType::F32
        | FieldType::F64 => {
            let n = json.as_number().ok_or_else(mismatch)?;
            let value = match (n.as_i64(), n.as_u64()) {
                (Some(i), _) => Value::I64(i),
                (None, Some(u)) => Value::U64(u),
                _ => Value::F64(n.as_f64().ok_or_else(mismatch)?),
            };
            coerce(value, def)
        }
        FieldType::PackedF32
        | FieldType::PackedF64
        | FieldType::PackedI32
        | FieldType::PackedI64
        | FieldType::PackedU32
        | FieldType::PackedU64 => {
            let items = json.as_array().ok_or_else(mismatch)?;
            let values = items
                .iter()
                .map(|v| from_json(v, None))
                .collect::<Result<Vec<_>, _>>()?;
            Packed::from_values(field_type, values).map(Value::Packed)
        }
        FieldType::Undefined => Err("the target field has no type".into()),
    }
}

/// Converts `value` to `def`'s type where that's lossless (e.g. I32 → I64,
/// a whole F64 → I32 that fits, OBJECT ↔ MAP); fails otherwise. Nested values
/// follow their definitions; an object's fields that `def` doesn't declare
/// are dropped.
pub(crate) fn coerce(value: Value, def: &FieldDefinition) -> Result<Value, String> {
    if value == Value::Null {
        return Ok(value);
    }
    let field_type = def.r#type();
    let mismatch = |v: &Value| {
        format!(
            "expected {}, got {}",
            field_type.as_str_name(),
            v.type_name()
        )
    };
    match field_type {
        FieldType::String => match value {
            Value::String(_) => Ok(value),
            other => Err(mismatch(&other)),
        },
        FieldType::Bool => match value {
            Value::Bool(_) => Ok(value),
            other => Err(mismatch(&other)),
        },
        FieldType::Datetime => match value {
            Value::DateTime(_) => Ok(value),
            other => Err(mismatch(&other)),
        },
        FieldType::Bytes => match value {
            Value::Bytes(_) => Ok(value),
            other => Err(mismatch(&other)),
        },
        FieldType::I32 | FieldType::I64 | FieldType::U32 | FieldType::U64 => {
            let whole = whole_number(&value).ok_or_else(|| mismatch(&value))?;
            let out_of_range = || format!("{whole} doesn't fit {}", field_type.as_str_name());
            Ok(match field_type {
                FieldType::I32 => Value::I32(i32::try_from(whole).map_err(|_| out_of_range())?),
                FieldType::I64 => Value::I64(i64::try_from(whole).map_err(|_| out_of_range())?),
                FieldType::U32 => Value::U32(u32::try_from(whole).map_err(|_| out_of_range())?),
                _ => Value::U64(u64::try_from(whole).map_err(|_| out_of_range())?),
            })
        }
        FieldType::F64 => match value {
            Value::F64(_) => Ok(value),
            Value::F32(f) => Ok(Value::F64(f64::from(f))),
            other => {
                let whole = whole_number(&other).ok_or_else(|| mismatch(&other))?;
                // Exact up to 2^53.
                if whole.unsigned_abs() <= 1 << 53 {
                    Ok(Value::F64(whole as f64))
                } else {
                    Err(format!("{whole} can't be an F64 exactly"))
                }
            }
        },
        FieldType::F32 => match value {
            Value::F32(_) => Ok(value),
            Value::F64(f) if f64::from(f as f32) == f || f.is_nan() => Ok(Value::F32(f as f32)),
            Value::F64(f) => Err(format!("{f} can't be an F32 exactly")),
            other => {
                let whole = whole_number(&other).ok_or_else(|| mismatch(&other))?;
                if whole.unsigned_abs() <= 1 << 24 {
                    Ok(Value::F32(whole as f32))
                } else {
                    Err(format!("{whole} can't be an F32 exactly"))
                }
            }
        },
        FieldType::Array => match value {
            // A packed array becomes an ARRAY of its element type.
            Value::Packed(p) => coerce(Value::Array(p.to_values()), def),
            Value::Array(items) => {
                let Some(nested) = def.nested_type_definition.as_deref() else {
                    return Ok(Value::Array(items));
                };
                let items = items
                    .into_iter()
                    .enumerate()
                    .map(|(i, v)| coerce(v, nested).map_err(|e| format!("[{i}]: {e}")))
                    .collect::<Result<_, _>>()?;
                Ok(Value::Array(items))
            }
            other => Err(mismatch(&other)),
        },
        FieldType::Map => {
            let entries: Vec<(String, Value)> = match value {
                Value::Map(m) => m.into_iter().collect(),
                Value::Object(f) => f,
                other => return Err(mismatch(&other)),
            };
            let nested = def.nested_type_definition.as_deref();
            let mut map = BTreeMap::new();
            for (k, v) in entries {
                let v = match nested {
                    Some(nested) => coerce(v, nested).map_err(|e| format!("{k:?}: {e}"))?,
                    None => v,
                };
                map.insert(k, v);
            }
            Ok(Value::Map(map))
        }
        FieldType::Object => {
            let fields: Vec<(String, Value)> = match value {
                Value::Object(f) => f,
                Value::Map(m) => m.into_iter().collect(),
                other => return Err(mismatch(&other)),
            };
            coerce_fields(fields, &def.object_fields).map(Value::Object)
        }
        FieldType::PackedF32
        | FieldType::PackedF64
        | FieldType::PackedI32
        | FieldType::PackedI64
        | FieldType::PackedU32
        | FieldType::PackedU64 => match value {
            Value::Packed(p) if p.field_type() == field_type => Ok(Value::Packed(p)),
            Value::Packed(p) => Packed::from_values(field_type, p.to_values()).map(Value::Packed),
            Value::Array(items) => Packed::from_values(field_type, items).map(Value::Packed),
            other => Err(mismatch(&other)),
        },
        FieldType::Undefined => Err("the target field has no type".into()),
    }
}

/// Shapes a payload to the target's input schema (see [`coerce`]).
pub(crate) fn coerce_payload(value: Value, schema: &SchemaDefinition) -> Result<Value, String> {
    match value {
        Value::Object(fields) => coerce_fields(fields, &schema.fields).map(Value::Object),
        other => Err(format!(
            "the output must be an object, not {}",
            other.type_name()
        )),
    }
}

fn coerce_fields(
    fields: Vec<(String, Value)>,
    defs: &[FieldDefinition],
) -> Result<Vec<(String, Value)>, String> {
    let mut out = vec![];
    for (key, value) in fields {
        // Fields the target doesn't declare are dropped.
        if let Some(def) = defs.iter().find(|d| d.key == key) {
            let value = coerce(value, def).map_err(|e| format!("{key}: {e}"))?;
            out.push((key, value));
        }
    }
    Ok(out)
}

/// An integer, or a whole float, as i128 (holds every u64 and i64).
fn whole_number(value: &Value) -> Option<i128> {
    match value {
        Value::I32(n) => Some(i128::from(*n)),
        Value::I64(n) => Some(i128::from(*n)),
        Value::U32(n) => Some(i128::from(*n)),
        Value::U64(n) => Some(i128::from(*n)),
        Value::F32(f) => whole_float(f64::from(*f)),
        Value::F64(f) => whole_float(*f),
        _ => None,
    }
}

fn whole_float(f: f64) -> Option<i128> {
    (f.is_finite() && f.fract() == 0.0 && f.abs() < 1.7e38).then_some(f as i128)
}

/// Whether a value of type `from` can (in principle) become a `to`, for
/// config-time checks: some conversions still depend on the value (range).
pub(crate) fn convertible(from: FieldType, to: FieldType) -> bool {
    use FieldType::*;
    let numeric = |t| matches!(t, I32 | I64 | U32 | U64 | F32 | F64);
    let sequence = |t: FieldType| t == Array || element_type(t).is_some();
    from == to
        || (numeric(from) && numeric(to))
        || (matches!(from, Map | Object) && matches!(to, Map | Object))
        // Element types are checked when the value is converted.
        || (sequence(from) && sequence(to))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn packed_arrays_round_trip_and_convert() {
        let packed = Value::Packed(Packed::F32(vec![0.5, -2.0]));
        let payload = Value::Object(vec![("v".into(), packed.clone())]).into_payload().unwrap();
        assert_eq!(
            Value::from_payload(&payload),
            Value::Object(vec![("v".into(), packed.clone())])
        );
        // To an ARRAY of F64 and back to another packed type, losslessly.
        let mut array_def = def(FieldType::Array);
        array_def.nested_type_definition = Some(Box::new(def(FieldType::F64)));
        assert_eq!(
            coerce(packed.clone(), &array_def).unwrap(),
            Value::Array(vec![Value::F64(0.5), Value::F64(-2.0)])
        );
        assert_eq!(
            coerce(packed, &def(FieldType::PackedF64)).unwrap(),
            Value::Packed(Packed::F64(vec![0.5, -2.0]))
        );
        let ints = Value::Array(vec![Value::I64(1), Value::I64(-3)]);
        assert_eq!(
            coerce(ints.clone(), &def(FieldType::PackedI32)).unwrap(),
            Value::Packed(Packed::I32(vec![1, -3]))
        );
        assert!(coerce(ints, &def(FieldType::PackedU32)).is_err(), "-3 isn't a u32");
        assert_eq!(
            from_json(&json!([1, 2.5]), Some(&def(FieldType::PackedF64))).unwrap(),
            Value::Packed(Packed::F64(vec![1.0, 2.5]))
        );
        assert!(convertible(FieldType::PackedF32, FieldType::Array));
        assert!(convertible(FieldType::Array, FieldType::PackedU64));
        assert!(!convertible(FieldType::PackedF32, FieldType::String));
    }

    fn def(field_type: FieldType) -> FieldDefinition {
        FieldDefinition {
            key: "x".into(),
            r#type: field_type as i32,
            ..Default::default()
        }
    }

    #[test]
    fn payload_round_trip_keeps_types() {
        let value = Value::Object(vec![
            ("s".into(), Value::String("hi".into())),
            ("u".into(), Value::U64(u64::MAX)),
            ("i".into(), Value::I64(-1)),
            ("b".into(), Value::Bytes(vec![0, 255])),
            (
                "t".into(),
                Value::DateTime(Timestamp {
                    seconds: 1,
                    nanos: 2,
                }),
            ),
            ("n".into(), Value::Null),
            ("a".into(), Value::Array(vec![Value::Bool(true)])),
            (
                "m".into(),
                Value::Map(BTreeMap::from([("k".into(), Value::F64(1.5))])),
            ),
            (
                "o".into(),
                Value::Object(vec![
                    ("a".into(), Value::I32(1)),
                    ("b".into(), Value::U32(2)),
                ]),
            ),
        ]);
        let payload = value.clone().into_payload().unwrap();
        assert_eq!(Value::from_payload(&payload), value);
    }

    #[test]
    fn lossless_coercion_only() {
        assert_eq!(
            coerce(Value::I32(5), &def(FieldType::I64)),
            Ok(Value::I64(5))
        );
        assert_eq!(
            coerce(Value::F64(7.0), &def(FieldType::U32)),
            Ok(Value::U32(7))
        );
        assert_eq!(
            coerce(Value::U64(3), &def(FieldType::F64)),
            Ok(Value::F64(3.0))
        );
        assert_eq!(
            coerce(Value::F64(0.5), &def(FieldType::F32)),
            Ok(Value::F32(0.5))
        );
        assert!(
            coerce(Value::F64(0.1), &def(FieldType::F32)).is_err(),
            "not exact"
        );
        assert!(coerce(Value::F64(7.5), &def(FieldType::I32)).is_err());
        assert!(coerce(Value::I64(-1), &def(FieldType::U64)).is_err());
        assert!(coerce(Value::U64(u64::MAX), &def(FieldType::I64)).is_err());
        assert!(coerce(Value::I64(1 << 60), &def(FieldType::F64)).is_err());
        assert!(
            coerce(Value::String("5".into()), &def(FieldType::I32)).is_err(),
            "no parsing"
        );
        assert_eq!(coerce(Value::Null, &def(FieldType::I32)), Ok(Value::Null));
        let map = coerce(
            Value::Object(vec![("k".into(), Value::I32(1))]),
            &FieldDefinition {
                nested_type_definition: Some(Box::new(def(FieldType::I64))),
                ..def(FieldType::Map)
            },
        )
        .unwrap();
        assert_eq!(
            map,
            Value::Map(BTreeMap::from([("k".into(), Value::I64(1))]))
        );
    }

    #[test]
    fn json_constants_follow_the_target_type() {
        assert_eq!(
            from_json(&json!(5), Some(&def(FieldType::U32))),
            Ok(Value::U32(5))
        );
        assert_eq!(
            from_json(&json!("ab"), Some(&def(FieldType::Bytes))),
            Ok(Value::Bytes(b"ab".to_vec()))
        );
        assert_eq!(
            from_json(&json!([1, 2]), Some(&def(FieldType::Bytes))),
            Ok(Value::Bytes(vec![1, 2]))
        );
        assert_eq!(
            from_json(
                &json!("1970-01-01T00:00:01.5Z"),
                Some(&def(FieldType::Datetime))
            ),
            Ok(Value::DateTime(Timestamp {
                seconds: 1,
                nanos: 500_000_000
            }))
        );
        assert!(from_json(&json!("x"), Some(&def(FieldType::I32))).is_err());
        assert!(from_json(&json!(300), Some(&def(FieldType::Bytes))).is_err());
        assert_eq!(from_json(&json!(u64::MAX), None), Ok(Value::U64(u64::MAX)));
    }
}
