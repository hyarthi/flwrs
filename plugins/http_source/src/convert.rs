//! The node's out schema, from its OpenAPI document: the common request
//! fields, and `paths` → template → method → `{path_params, query, body}`
//! typed by the document's schemas. JSON Schema that flwrs can't express
//! (`oneOf`, free-form objects, …) is refused, naming where it is.

use crate::spec::{Api, Location, MediaKind, Operation};
use flwrs_plugin::schema::payload::field_type::Enum as FieldType;
use flwrs_plugin::schema::payload::{FieldDefinition, SchemaDefinition};
pub use flwrs_plugin_kit::schema::convert;
use flwrs_plugin_kit::schema::{field, same_shape};
use serde_json::Value;

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
