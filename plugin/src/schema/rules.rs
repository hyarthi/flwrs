//! Rules for payload schemas (`SchemaDefinition`), shared by the hub and plugins.
//!
//! - [`check_well_formed`]: structural validity of one schema.
//! - [`diff`]: structural equality of two schemas (field order and descriptions
//!   are ignored), reported as a list of human-readable differences.
//! - [`parse_json`]: reads a schema in proto3 JSON form, as used in plugin manifests.
//! - [`check_payload_depth`]: the protocol's nesting limit for payload values.

use super::payload::{
    FieldDefinition, FieldValue, PluginPayload, SchemaDefinition, field_type::Enum as FieldType,
    field_value::Value,
};
use std::collections::{BTreeMap, HashSet};

/// Maximum nesting depth of a schema or a payload value (the protocol limit).
pub const MAX_DEPTH: usize = crate::plugin::frame::MAX_NESTING_DEPTH;

/// Checks that no value in `payload` is nested deeper than [`MAX_DEPTH`]
/// levels (a top-level field is level 1).
pub fn check_payload_depth(payload: &PluginPayload) -> Result<(), String> {
    for field in &payload.fields {
        if let Some(value) = &field.value {
            value_depth(value, 1).map_err(|()| {
                format!("{}: nested deeper than {MAX_DEPTH} levels", field.key)
            })?;
        }
    }
    Ok(())
}

fn value_depth(value: &FieldValue, depth: usize) -> Result<(), ()> {
    if depth > MAX_DEPTH {
        return Err(());
    }
    match &value.value {
        Some(Value::Array(array)) => array
            .value
            .iter()
            .try_for_each(|v| value_depth(v, depth + 1)),
        Some(Value::Map(map) | Value::Object(map)) => map
            .value
            .values()
            .try_for_each(|v| value_depth(v, depth + 1)),
        _ => Ok(()),
    }
}

/// Parses a `SchemaDefinition` from its proto3 JSON form, e.g.
/// `{"fields": [{"key": "url", "type": "STRING"}]}`. Both `camelCase` and
/// `snake_case` field names are accepted.
pub fn parse_json(json: &str) -> Result<SchemaDefinition, String> {
    serde_json::from_str(json).map_err(|e| e.to_string())
}

/// Checks that a schema is structurally valid. Returns every problem found;
/// an empty list means the schema is well formed.
pub fn check_well_formed(schema: &SchemaDefinition) -> Vec<String> {
    let mut errors = vec![];
    check_fields(&schema.fields, "", 1, &mut errors);
    errors
}

fn check_fields(fields: &[FieldDefinition], parent: &str, depth: usize, errors: &mut Vec<String>) {
    let mut seen = HashSet::new();
    for field in fields {
        let path = join(parent, &field.key);
        if field.key.is_empty() {
            errors.push(format!("{}: field key is empty", display(parent)));
        } else if !seen.insert(field.key.as_str()) {
            errors.push(format!("{path}: duplicate field key"));
        }
        check_field(field, &path, depth, errors);
    }
}

fn check_field(field: &FieldDefinition, path: &str, depth: usize, errors: &mut Vec<String>) {
    if depth > MAX_DEPTH {
        errors.push(format!("{path}: nested deeper than {MAX_DEPTH} levels"));
        return;
    }
    let Ok(kind) = FieldType::try_from(field.r#type) else {
        errors.push(format!("{path}: unknown type {}", field.r#type));
        return;
    };
    match kind {
        FieldType::Undefined => errors.push(format!("{path}: type is UNDEFINED")),
        FieldType::Array | FieldType::Map => {
            let suffix = if kind == FieldType::Array { "[]" } else { "{}" };
            match &field.nested_type_definition {
                Some(nested) => check_field(nested, &format!("{path}{suffix}"), depth + 1, errors),
                None => errors.push(format!(
                    "{path}: {} needs a nested_type_definition",
                    kind.as_str_name()
                )),
            }
        }
        _ => {
            if field.nested_type_definition.is_some() {
                errors.push(format!(
                    "{path}: nested_type_definition is only allowed for ARRAY and MAP"
                ));
            }
        }
    }
    if !field.content_type.is_empty() && kind != FieldType::Bytes {
        errors.push(format!("{path}: content_type is only allowed for BYTES"));
    }
    if kind == FieldType::Object {
        if field.object_fields.is_empty() {
            errors.push(format!("{path}: OBJECT needs object_fields"));
        } else {
            check_fields(&field.object_fields, path, depth + 1, errors);
        }
    } else if !field.object_fields.is_empty() {
        errors.push(format!("{path}: object_fields are only allowed for OBJECT"));
    }
}

/// Compares the schema a plugin declared (`actual`) with the one its manifest
/// declares (`expected`). Field order and descriptions are ignored; content
/// types are compared. Returns
/// every difference; an empty list means the schemas match.
pub fn diff(expected: &SchemaDefinition, actual: &SchemaDefinition) -> Vec<String> {
    let mut diffs = vec![];
    diff_fields(&expected.fields, &actual.fields, "", &mut diffs);
    diffs
}

fn diff_fields(
    expected: &[FieldDefinition],
    actual: &[FieldDefinition],
    parent: &str,
    diffs: &mut Vec<String>,
) {
    // BTreeMap keeps the report in a stable (sorted) order.
    let expected: BTreeMap<&str, &FieldDefinition> =
        expected.iter().map(|f| (f.key.as_str(), f)).collect();
    let actual: BTreeMap<&str, &FieldDefinition> =
        actual.iter().map(|f| (f.key.as_str(), f)).collect();
    for (key, exp) in &expected {
        let path = join(parent, key);
        match actual.get(key) {
            None => diffs.push(format!("{path}: missing from plugin")),
            Some(act) => diff_field(exp, act, &path, diffs),
        }
    }
    for key in actual.keys().filter(|k| !expected.contains_key(*k)) {
        diffs.push(format!("{}: not in manifest", join(parent, key)));
    }
}

fn diff_field(
    expected: &FieldDefinition,
    actual: &FieldDefinition,
    path: &str,
    diffs: &mut Vec<String>,
) {
    if expected.r#type != actual.r#type {
        diffs.push(format!(
            "{path}: type {} in manifest, {} from plugin",
            type_name(expected.r#type),
            type_name(actual.r#type)
        ));
        return;
    }
    if expected.content_type != actual.content_type {
        diffs.push(format!(
            "{path}: content type {:?} in manifest, {:?} from plugin",
            expected.content_type, actual.content_type
        ));
    }
    let suffix = match FieldType::try_from(expected.r#type) {
        Ok(FieldType::Array) => "[]",
        Ok(FieldType::Map) => "{}",
        _ => "",
    };
    match (
        &expected.nested_type_definition,
        &actual.nested_type_definition,
    ) {
        (Some(exp), Some(act)) => diff_field(exp, act, &format!("{path}{suffix}"), diffs),
        (Some(_), None) => diffs.push(format!("{path}: nested type missing from plugin")),
        (None, Some(_)) => diffs.push(format!("{path}: nested type not in manifest")),
        (None, None) => {}
    }
    diff_fields(&expected.object_fields, &actual.object_fields, path, diffs);
}

fn type_name(value: i32) -> String {
    FieldType::try_from(value)
        .map(|t| t.as_str_name().to_string())
        .unwrap_or_else(|_| format!("<unknown {value}>"))
}

fn join(parent: &str, key: &str) -> String {
    if parent.is_empty() {
        key.to_string()
    } else {
        format!("{parent}.{key}")
    }
}

fn display(path: &str) -> &str {
    if path.is_empty() { "<root>" } else { path }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field(key: &str, t: FieldType) -> FieldDefinition {
        FieldDefinition {
            key: key.into(),
            r#type: t as i32,
            ..Default::default()
        }
    }

    fn nested(mut f: FieldDefinition, inner: FieldDefinition) -> FieldDefinition {
        f.nested_type_definition = Some(Box::new(inner));
        f
    }

    fn schema(fields: Vec<FieldDefinition>) -> SchemaDefinition {
        SchemaDefinition { fields }
    }

    #[test]
    fn well_formed_schema_passes() {
        let mut obj = field("point", FieldType::Object);
        obj.object_fields = vec![field("x", FieldType::F64), field("y", FieldType::F64)];
        let s = schema(vec![
            field("url", FieldType::String),
            nested(
                field("headers", FieldType::Map),
                nested(field("", FieldType::Array), field("", FieldType::String)),
            ),
            obj,
        ]);
        assert_eq!(check_well_formed(&s), Vec::<String>::new());
    }

    #[test]
    fn reports_structural_problems() {
        let mut obj = field("obj", FieldType::Object);
        obj.object_fields = vec![];
        let mut bad_obj_fields = field("str", FieldType::String);
        bad_obj_fields.object_fields = vec![field("x", FieldType::I32)];
        let s = schema(vec![
            field("", FieldType::String),
            field("dup", FieldType::String),
            field("dup", FieldType::I32),
            field("undef", FieldType::Undefined),
            field("list", FieldType::Array),
            nested(field("scalar", FieldType::I32), field("", FieldType::I32)),
            obj,
            bad_obj_fields,
            FieldDefinition {
                key: "weird".into(),
                r#type: 999,
                ..Default::default()
            },
        ]);
        let errors = check_well_formed(&s);
        for expected in [
            "<root>: field key is empty",
            "dup: duplicate field key",
            "undef: type is UNDEFINED",
            "list: ARRAY needs a nested_type_definition",
            "scalar: nested_type_definition is only allowed for ARRAY and MAP",
            "obj: OBJECT needs object_fields",
            "str: object_fields are only allowed for OBJECT",
            "weird: unknown type 999",
        ] {
            assert!(
                errors.contains(&expected.to_string()),
                "missing {expected:?} in {errors:?}"
            );
        }
    }

    #[test]
    fn rejects_excessive_depth() {
        let mut f = field("", FieldType::String);
        for _ in 0..MAX_DEPTH + 1 {
            f = nested(field("", FieldType::Array), f);
        }
        f.key = "deep".into();
        let errors = check_well_formed(&schema(vec![f]));
        assert!(
            errors.iter().any(|e| e.contains("nested deeper than")),
            "{errors:?}"
        );
    }

    #[test]
    fn content_type_only_on_bytes_and_compared() {
        let mut image = field("image", FieldType::Bytes);
        image.content_type = "image/jpeg".into();
        let mut label = field("label", FieldType::String);
        label.content_type = "text/plain".into();
        let errors = check_well_formed(&schema(vec![image.clone(), label]));
        assert_eq!(errors, ["label: content_type is only allowed for BYTES"]);
        assert!(check_well_formed(&schema(vec![field("v", FieldType::PackedF32)])).is_empty());

        let mut png = image.clone();
        png.content_type = "image/png".into();
        assert_eq!(
            diff(&schema(vec![image]), &schema(vec![png])),
            ["image: content type \"image/jpeg\" in manifest, \"image/png\" from plugin"]
        );
    }

    #[test]
    fn payload_depth_limit() {
        use crate::schema::payload::{ArrayValue, Field};
        let mut value = FieldValue {
            value: Some(Value::I32(1)),
        };
        for _ in 0..MAX_DEPTH - 1 {
            value = FieldValue {
                value: Some(Value::Array(ArrayValue { value: vec![value] })),
            };
        }
        let payload = |value: FieldValue| PluginPayload {
            fields: vec![Field {
                key: "deep".into(),
                value: Some(value),
            }],
        };
        assert!(check_payload_depth(&payload(value.clone())).is_ok());
        let deeper = FieldValue {
            value: Some(Value::Array(ArrayValue { value: vec![value] })),
        };
        assert_eq!(
            check_payload_depth(&payload(deeper)).unwrap_err(),
            "deep: nested deeper than 32 levels"
        );
    }

    #[test]
    fn diff_ignores_order_and_descriptions() {
        let mut described = field("a", FieldType::String);
        described.description = "something".into();
        let expected = schema(vec![described, field("b", FieldType::I64)]);
        let actual = schema(vec![
            field("b", FieldType::I64),
            field("a", FieldType::String),
        ]);
        assert!(diff(&expected, &actual).is_empty());
    }

    #[test]
    fn diff_reports_every_difference() {
        let mut exp_obj = field("obj", FieldType::Object);
        exp_obj.object_fields = vec![field("x", FieldType::I32)];
        let mut act_obj = field("obj", FieldType::Object);
        act_obj.object_fields = vec![field("x", FieldType::I64)];
        let expected = schema(vec![
            field("missing", FieldType::String),
            field("typed", FieldType::String),
            nested(
                field("list", FieldType::Array),
                field("", FieldType::String),
            ),
            exp_obj,
        ]);
        let actual = schema(vec![
            field("typed", FieldType::Bytes),
            nested(field("list", FieldType::Array), field("", FieldType::I32)),
            act_obj,
            field("extra", FieldType::Bool),
        ]);
        assert_eq!(
            diff(&expected, &actual),
            vec![
                "list[]: type STRING in manifest, I32 from plugin",
                "missing: missing from plugin",
                "obj.x: type I32 in manifest, I64 from plugin",
                "typed: type STRING in manifest, BYTES from plugin",
                "extra: not in manifest",
            ]
        );
    }

    #[test]
    fn parses_proto3_json_in_both_naming_styles() {
        let camel = r#"{"fields": [
            {"key": "url", "description": "HTTP URL", "type": "STRING"},
            {"key": "headers", "type": "MAP",
             "nestedTypeDefinition": {"type": "ARRAY", "nestedTypeDefinition": {"type": "STRING"}}}
        ]}"#;
        let snake = r#"{"fields": [
            {"key": "headers", "type": "MAP",
             "nested_type_definition": {"type": "ARRAY", "nested_type_definition": {"type": "STRING"}}},
            {"key": "url", "type": "STRING"}
        ]}"#;
        let a = parse_json(camel).unwrap();
        let b = parse_json(snake).unwrap();
        assert!(check_well_formed(&a).is_empty());
        assert!(diff(&a, &b).is_empty());
        assert!(parse_json(r#"{"fields": [{"key": "x", "type": "NOPE"}]}"#).is_err());
        assert!(parse_json("not json").is_err());
    }
}
