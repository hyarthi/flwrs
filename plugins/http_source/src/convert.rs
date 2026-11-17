//! The node's out schema, from its OpenAPI document: the common request
//! fields, and `paths` → template → method → `{path_params, query, body}`
//! typed by the document's schemas. JSON Schema that flwrs can't express
//! (`oneOf`, free-form objects, …) is refused, naming where it is.

use crate::spec::{Api, Location, MediaKind, Operation, resolve};
use flwrs_plugin::schema::payload::field_type::Enum as FieldType;
use flwrs_plugin::schema::payload::{FieldDefinition, SchemaDefinition};
use serde_json::Value;

/// flwrs schemas nest at most this deep.
const MAX_DEPTH: usize = 32;

fn field(key: &str, t: FieldType) -> FieldDefinition {
    FieldDefinition {
        key: key.into(),
        r#type: t as i32,
        ..Default::default()
    }
}

fn described(mut f: FieldDefinition, description: &str) -> FieldDefinition {
    f.description = description.into();
    f
}

fn string_map(key: &str, element: FieldDefinition) -> FieldDefinition {
    FieldDefinition {
        nested_type_definition: Some(Box::new(element)),
        ..field(key, FieldType::Map)
    }
}

/// The fields every event has.
pub fn common_fields() -> Vec<FieldDefinition> {
    let strings = FieldDefinition {
        nested_type_definition: Some(Box::new(field("", FieldType::String))),
        ..field("", FieldType::Array)
    };
    vec![
        described(
            field("method", FieldType::String),
            "The request's method (POST)",
        ),
        described(
            field("path", FieldType::String),
            "The matched path template (/devices/{id})",
        ),
        described(
            field("uri", FieldType::String),
            "The request's path and query, as sent",
        ),
        described(
            string_map("headers", strings),
            "Request headers (lowercase names; credentials left out)",
        ),
        described(
            field("content_type", FieldType::String),
            "The body's media type",
        ),
        described(field("body", FieldType::Bytes), "The body, as sent"),
        described(
            field("remote_addr", FieldType::String),
            "The client's address",
        ),
        described(
            field("http_version", FieldType::String),
            "HTTP/1.1 or HTTP/2.0",
        ),
        described(
            field("principal", FieldType::String),
            "Who sent it: the token's label or the certificate's name",
        ),
        described(
            field("receive_time", FieldType::Datetime),
            "When the server received it",
        ),
    ]
}

/// The node's out schema.
pub fn out_schema(api: Option<&Api>) -> Result<SchemaDefinition, Vec<String>> {
    let mut fields = common_fields();
    let Some(api) = api else {
        return Ok(SchemaDefinition { fields });
    };
    let mut errors = vec![];
    let mut paths: Vec<FieldDefinition> = vec![];
    for op in &api.operations {
        match operation(api, op) {
            Err(e) => errors.extend(e),
            Ok(method) => match paths.iter_mut().find(|p| p.key == op.path) {
                Some(path) => path.object_fields.push(method),
                None => paths.push(FieldDefinition {
                    object_fields: vec![method],
                    ..field(&op.path, FieldType::Object)
                }),
            },
        }
    }
    if !errors.is_empty() {
        return Err(errors);
    }
    if !paths.is_empty() {
        fields.push(described(
            FieldDefinition { object_fields: paths, ..field("paths", FieldType::Object) },
            "The request, typed by the matching operation: paths[\"<template>\"].<method>.{path_params, query, body}",
        ));
    }
    Ok(SchemaDefinition { fields })
}

fn operation(api: &Api, op: &Operation) -> Result<FieldDefinition, Vec<String>> {
    let at = format!("{} {}", op.method, op.path);
    let mut errors = vec![];
    let mut parts = vec![];
    for (location, key) in [(Location::Path, "path_params"), (Location::Query, "query")] {
        let mut members = vec![];
        for p in op.params.iter().filter(|p| p.location == location) {
            let converted = match &p.schema {
                None => Ok(field("", FieldType::String)),
                Some(ptr) => convert(&api.doc, ptr, 0, &mut vec![]),
            };
            match converted {
                Ok(mut f) => {
                    if matches!(
                        FieldType::try_from(f.r#type),
                        Ok(FieldType::Object | FieldType::Map)
                    ) {
                        errors.push(format!(
                            "{at}: parameter {:?}: objects aren't supported as parameters",
                            p.name
                        ));
                        continue;
                    }
                    f.key = p.name.clone();
                    if let Some(d) = &p.description {
                        f.description = d.clone();
                    }
                    members.push(f);
                }
                Err(e) => errors.extend(
                    e.into_iter()
                        .map(|e| format!("{at}: parameter {:?}: {e}", p.name)),
                ),
            }
        }
        if !members.is_empty() {
            parts.push(FieldDefinition {
                object_fields: members,
                ..field(key, FieldType::Object)
            });
        }
    }
    // The body, typed when it's JSON or a form (every such media type must agree).
    if let Some(body) = &op.body {
        let mut typed: Option<(String, FieldDefinition)> = None;
        for media in body.content.iter().filter(|m| m.kind != MediaKind::Bytes) {
            let Some(ptr) = &media.schema else {
                errors.push(format!("{at}: {} has no schema", media.media_type));
                continue;
            };
            match convert(&api.doc, ptr, 0, &mut vec![]) {
                Err(e) => errors.extend(
                    e.into_iter()
                        .map(|e| format!("{at}: body ({}): {e}", media.media_type)),
                ),
                Ok(mut f) => {
                    f.key = "body".into();
                    if media.kind == MediaKind::Form
                        && FieldType::try_from(f.r#type) != Ok(FieldType::Object)
                    {
                        errors.push(format!("{at}: a form body's schema must be an object"));
                        continue;
                    }
                    match &typed {
                        Some((other, existing)) if !same_shape(existing, &f) => errors.push(format!(
                            "{at}: the body schemas of {other} and {} differ; one typed body per operation",
                            media.media_type
                        )),
                        Some(_) => {}
                        None => typed = Some((media.media_type.clone(), f)),
                    }
                }
            }
        }
        if let Some((_, f)) = typed {
            parts.push(f);
        }
    }
    if !errors.is_empty() {
        return Err(errors);
    }
    let mut method = FieldDefinition {
        object_fields: parts,
        ..field(&op.method, FieldType::Object)
    };
    method.description = op
        .summary
        .clone()
        .or_else(|| op.operation_id.clone())
        .unwrap_or_default();
    Ok(method)
}

fn same_shape(a: &FieldDefinition, b: &FieldDefinition) -> bool {
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
                    &format!("{ptr}/properties/{}", crate::spec::escape(key)),
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
                            &format!("{ptr}/properties/{}", crate::spec::escape(key)),
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

/// The out schema as JSON (what the configurator returns for `out_schema`).
pub fn to_json(schema: &SchemaDefinition) -> Value {
    serde_json::to_value(schema).expect("a schema serializes")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::{load, parse};
    use serde_json::json;

    fn api(doc: Value) -> Api {
        load(doc).unwrap()
    }

    fn doc(schemas: Value) -> Value {
        json!({
            "openapi": "3.1.0", "info": {"title": "t", "version": "1"},
            "paths": {"/things/{id}": {"post": {
                "parameters": [
                    {"name": "id", "in": "path", "required": true, "schema": {"type": "integer", "format": "int32"}},
                    {"name": "tag", "in": "query", "schema": {"type": "array", "items": {"type": "string"}}}
                ],
                "requestBody": {"content": {"application/json": {"schema": {"$ref": "#/components/schemas/Thing"}}}},
                "responses": {"204": {"description": "ok"}}
            }}},
            "components": {"schemas": schemas}
        })
    }

    #[test]
    fn types_follow_the_schemas() {
        let schemas = json!({"Thing": {"allOf": [{"$ref": "#/components/schemas/Base"}, {"type": "object", "properties": {
            "temp": {"type": ["number", "null"]},
            "at": {"type": "string", "format": "date-time"},
            "blob": {"type": "string", "contentEncoding": "base64"},
            "labels": {"type": "object", "additionalProperties": {"type": "string"}},
            "kind": {"enum": ["a", "b"]}
        }}]}, "Base": {"type": "object", "properties": {"name": {"type": "string"}}}});
        let schema = out_schema(Some(&api(doc(schemas)))).unwrap();
        let json = to_json(&schema);
        let post = &json["fields"].as_array().unwrap().last().unwrap()["objectFields"][0]["objectFields"]
            [0];
        let parts: Vec<&str> = post["objectFields"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["key"].as_str().unwrap())
            .collect();
        assert_eq!(parts, ["path_params", "query", "body"]);
        assert_eq!(post["objectFields"][0]["objectFields"][0]["type"], "I32");
        assert_eq!(post["objectFields"][1]["objectFields"][0]["type"], "ARRAY");
        let body: Vec<(String, String)> = post["objectFields"][2]["objectFields"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| {
                (
                    f["key"].as_str().unwrap().into(),
                    f["type"].as_str().unwrap().into(),
                )
            })
            .collect();
        // allOf parts first, then the properties in key order (JSON objects are sorted).
        let expected: Vec<(String, String)> = [
            ("name", "STRING"),
            ("at", "DATETIME"),
            ("blob", "BYTES"),
            ("kind", "STRING"),
            ("labels", "MAP"),
            ("temp", "F64"),
        ]
        .into_iter()
        .map(|(a, b)| (a.into(), b.into()))
        .collect();
        assert_eq!(body, expected);
        // Well formed, and parseable as the node's schema.
        assert!(flwrs_plugin::schema::rules::check_well_formed(&schema).is_empty());
        let reparsed = flwrs_plugin::schema::rules::parse_json(&json.to_string()).unwrap();
        assert!(flwrs_plugin::schema::rules::diff(&reparsed, &schema).is_empty());
    }

    #[test]
    fn refuses_what_flwrs_cant_carry() {
        let errors = |thing: Value| {
            out_schema(Some(&api(doc(json!({"Thing": thing})))))
                .unwrap_err()
                .join(" | ")
        };
        assert!(
            errors(json!({"oneOf": [{"type": "string"}, {"type": "integer"}]}))
                .contains("oneOf isn't supported")
        );
        assert!(errors(json!({"type": "object"})).contains("free-form object"));
        assert!(errors(json!({"type": ["string", "integer"]})).contains("several types"));
        assert!(errors(json!({"type": "array"})).contains("without \"items\""));
        let e = errors(
            json!({"type": "object", "properties": {"next": {"$ref": "#/components/schemas/Thing"}}}),
        );
        assert!(e.contains("recursive"), "{e}");
        // The place is named.
        assert!(
            errors(json!({"type": "object", "properties": {"x": {}}}))
                .contains("#/components/schemas/Thing/properties/x")
        );
    }

    #[test]
    fn without_a_document_only_the_common_fields() {
        let schema = out_schema(None).unwrap();
        assert_eq!(schema.fields.len(), common_fields().len());
        let yaml = crate::spec::tests::DEVICES;
        assert!(
            out_schema(Some(
                &load(parse(&Value::String(yaml.into())).unwrap()).unwrap()
            ))
            .is_ok()
        );
    }
}
