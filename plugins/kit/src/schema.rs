//! JSON Schema inside larger documents (OpenAPI, AsyncAPI): local `$ref`s as
//! JSON pointers, conversion to flwrs payload types (refusing what flwrs
//! can't express, naming where), and validators that resolve references
//! within the document. Shared by the HTTP and WebSocket sources.

use flwrs_plugin::schema::payload::field_type::Enum as FieldType;
use flwrs_plugin::schema::payload::{FieldDefinition, SchemaDefinition};
use serde_json::Value;

/// Escapes a key for a JSON pointer.
pub fn escape(key: &str) -> String {
    key.replace('~', "~0").replace('/', "~1")
}

/// Follows local `$ref`s from `pointer` to the object they lead to; returns
/// it and its pointer.
pub fn resolve<'a>(doc: &'a Value, pointer: &str) -> Result<(&'a Value, String), String> {
    let mut pointer = pointer.to_string();
    for _ in 0..32 {
        let value = doc
            .pointer(&pointer)
            .ok_or_else(|| format!("#{pointer} doesn't exist"))?;
        match value.get("$ref").and_then(Value::as_str) {
            Some(r) => {
                pointer = r
                    .strip_prefix('#')
                    .ok_or_else(|| {
                        format!("#{pointer}: only local references (#/…) are allowed, not {r:?}")
                    })?
                    .to_string();
            }
            None => return Ok((value, pointer)),
        }
    }
    Err(format!("#{pointer}: references nest too deep (a cycle?)"))
}

/// Every `$ref` that isn't local (`#/…`): the document must be self-contained.
pub fn external_refs(value: &Value, at: &str, out: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            for (k, v) in map {
                if k == "$ref"
                    && let Some(r) = v.as_str()
                    && !r.starts_with('#')
                {
                    out.push(format!(
                        "#{at}: the reference {r:?} points outside the document; it must be self-contained"
                    ));
                }
                external_refs(v, &format!("{at}/{}", escape(k)), out);
            }
        }
        Value::Array(items) => {
            for (i, v) in items.iter().enumerate() {
                external_refs(v, &format!("{at}/{i}"), out);
            }
        }
        _ => {}
    }
}

/// flwrs schemas nest at most this deep.
pub const MAX_DEPTH: usize = 32;

pub fn field(key: &str, t: FieldType) -> FieldDefinition {
    FieldDefinition {
        key: key.into(),
        r#type: t as i32,
        ..Default::default()
    }
}

pub fn same_shape(a: &FieldDefinition, b: &FieldDefinition) -> bool {
    flwrs_plugin::schema::rules::diff(
        &SchemaDefinition {
            fields: vec![a.clone()],
        },
        &SchemaDefinition {
            fields: vec![b.clone()],
        },
    )
    .is_empty()
}

/// The non-null type of a schema (`type: [string, "null"]` is a string).
fn single_type(schema: &Value) -> Result<Option<String>, String> {
    match schema.get("type") {
        None => Ok(None),
        Some(Value::String(t)) => Ok(Some(t.clone())),
        Some(Value::Array(types)) => {
            let real: Vec<&str> = types
                .iter()
                .filter_map(Value::as_str)
                .filter(|t| *t != "null")
                .collect();
            match real.as_slice() {
                [one] => Ok(Some(one.to_string())),
                [] => Err("only \"null\" isn't a type flwrs can carry".into()),
                _ => Err(format!(
                    "several types ({}) aren't supported",
                    real.join(", ")
                )),
            }
        }
        Some(other) => Err(format!("\"type\" {other} isn't valid")),
    }
}

/// Converts the schema at `ptr` (following `$ref`s). `stack`: the
/// references being expanded, to refuse recursive schemas.
pub fn convert(
    doc: &Value,
    ptr: &str,
    depth: usize,
    stack: &mut Vec<String>,
) -> Result<FieldDefinition, Vec<String>> {
    if depth > MAX_DEPTH {
        return Err(vec![format!(
            "#{ptr}: nested deeper than {MAX_DEPTH} levels"
        )]);
    }
    let (schema, ptr) = resolve(doc, ptr).map_err(|e| vec![e])?;
    if stack.contains(&ptr) {
        return Err(vec![format!("#{ptr}: recursive schemas aren't supported")]);
    }
    stack.push(ptr.clone());
    let result = convert_inner(doc, schema, &ptr, depth, stack);
    stack.pop();
    result.map(|mut f| {
        if let Some(d) = schema.get("description").and_then(Value::as_str) {
            f.description = d.into();
        }
        f
    })
}

fn convert_inner(
    doc: &Value,
    schema: &Value,
    ptr: &str,
    depth: usize,
    stack: &mut Vec<String>,
) -> Result<FieldDefinition, Vec<String>> {
    let unsupported = |what: &str| {
        Err(vec![format!(
            "#{ptr}: {what} isn't supported (flwrs has no type for it)"
        )])
    };
    if schema == &Value::Bool(true) || schema.as_object().is_some_and(|o| o.is_empty()) {
        return unsupported("a schema that allows anything");
    }
    for keyword in [
        "oneOf",
        "anyOf",
        "not",
        "if",
        "patternProperties",
        "prefixItems",
        "dependentSchemas",
        "$dynamicRef",
    ] {
        if schema.get(keyword).is_some() {
            return unsupported(keyword);
        }
    }
    if let Some(all) = schema.get("allOf").and_then(Value::as_array) {
        // Objects merged: every part must be an object (by reference or inline).
        let mut fields: Vec<FieldDefinition> = vec![];
        for i in 0..all.len() {
            let part = convert(doc, &format!("{ptr}/allOf/{i}"), depth, stack)?;
            if FieldType::try_from(part.r#type) != Ok(FieldType::Object) {
                return Err(vec![format!(
                    "#{ptr}/allOf/{i}: allOf is supported for objects only"
                )]);
            }
            for f in part.object_fields {
                match fields.iter().find(|e| e.key == f.key) {
                    Some(e) if !same_shape(e, &f) => {
                        return Err(vec![format!("#{ptr}: allOf parts disagree on {:?}", f.key)]);
                    }
                    Some(_) => {}
                    None => fields.push(f),
                }
            }
        }
        if let Some(props) = schema.get("properties").and_then(Value::as_object) {
            for key in props.keys() {
                let mut f = convert(
                    doc,
                    &format!("{ptr}/properties/{}", escape(key)),
                    depth + 1,
                    stack,
                )?;
                f.key = key.clone();
                fields.retain(|e| e.key != *key);
                fields.push(f);
            }
        }
        return Ok(FieldDefinition {
            object_fields: fields,
            ..field("", FieldType::Object)
        });
    }
    let declared = single_type(schema).map_err(|e| vec![format!("#{ptr}: {e}")])?;
    let inferred = declared.or_else(|| {
        if schema.get("properties").is_some() || schema.get("additionalProperties").is_some() {
            Some("object".into())
        } else if schema.get("items").is_some() {
            Some("array".into())
        } else if schema
            .get("enum")
            .and_then(Value::as_array)
            .is_some_and(|e| e.iter().all(Value::is_string))
        {
            Some("string".into())
        } else {
            None
        }
    });
    let format = schema
        .get("format")
        .and_then(Value::as_str)
        .unwrap_or_default();
    match inferred.as_deref() {
        Some("string") => Ok(field(
            "",
            match format {
                "date-time" => FieldType::Datetime,
                "binary" | "byte" => FieldType::Bytes,
                _ if schema.get("contentEncoding").and_then(Value::as_str) == Some("base64") => {
                    FieldType::Bytes
                }
                _ => FieldType::String,
            },
        )),
        Some("integer") => Ok(field(
            "",
            if format == "int32" {
                FieldType::I32
            } else {
                FieldType::I64
            },
        )),
        Some("number") => Ok(field(
            "",
            if format == "float" {
                FieldType::F32
            } else {
                FieldType::F64
            },
        )),
        Some("boolean") => Ok(field("", FieldType::Bool)),
        Some("array") => {
            if schema.get("items").is_none() {
                return unsupported("an array without \"items\"");
            }
            let items = convert(doc, &format!("{ptr}/items"), depth + 1, stack)?;
            Ok(FieldDefinition {
                nested_type_definition: Some(Box::new(items)),
                ..field("", FieldType::Array)
            })
        }
        Some("object") => {
            let props = schema.get("properties").and_then(Value::as_object);
            let extra = schema.get("additionalProperties").filter(|a| a.is_object());
            match (props, extra) {
                (Some(_), Some(_)) => {
                    unsupported("an object with both properties and an additionalProperties schema")
                }
                (Some(props), None) => {
                    let mut errors = vec![];
                    let mut fields = vec![];
                    for key in props.keys() {
                        match convert(
                            doc,
                            &format!("{ptr}/properties/{}", escape(key)),
                            depth + 1,
                            stack,
                        ) {
                            Ok(mut f) => {
                                f.key = key.clone();
                                fields.push(f);
                            }
                            Err(e) => errors.extend(e),
                        }
                    }
                    if errors.is_empty() {
                        Ok(FieldDefinition {
                            object_fields: fields,
                            ..field("", FieldType::Object)
                        })
                    } else {
                        Err(errors)
                    }
                }
                (None, Some(_)) => {
                    let values = convert(
                        doc,
                        &format!("{ptr}/additionalProperties"),
                        depth + 1,
                        stack,
                    )?;
                    Ok(FieldDefinition {
                        nested_type_definition: Some(Box::new(values)),
                        ..field("", FieldType::Map)
                    })
                }
                (None, None) => unsupported("a free-form object (no properties)"),
            }
        }
        Some(other) => Err(vec![format!("#{ptr}: type {other:?} isn't supported")]),
        None => unsupported("a schema without a type"),
    }
}

/// Validates against the schema at `ptr`: the whole document with a root
/// `$ref`, so `#/components/…` resolve (its other keys aren't keywords).
pub fn validator(doc: &Value, ptr: &str) -> Result<jsonschema::Validator, String> {
    let mut schema = doc.clone();
    if let Value::Object(map) = &mut schema {
        map.insert(
            "$ref".into(),
            Value::String(format!("#{}", uri_fragment(ptr))),
        );
    }
    jsonschema::options()
        .with_draft(jsonschema::Draft::Draft202012)
        .build(&schema)
        .map_err(|e| format!("#{ptr}: {e}"))
}

/// A JSON pointer as a URI fragment (RFC 6901 §6): characters a fragment
/// can't hold (e.g. a path template's `{` and `}`) percent-encoded.
fn uri_fragment(ptr: &str) -> String {
    let mut out = String::with_capacity(ptr.len());
    for b in ptr.bytes() {
        let keep = b.is_ascii_alphanumeric() || b"-._~!$&'()*+,;=:@/".contains(&b);
        if keep {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}
