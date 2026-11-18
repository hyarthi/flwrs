//! Reading an OpenAPI 3.1 document (JSON or YAML): checked with `oas3`, then
//! walked as JSON (local `$ref`s are JSON pointers; external ones refused,
//! the document must be self-contained), into the operations the server
//! routes: path templates, methods, parameters, request bodies.

use flwrs_plugin_kit::schema::external_refs;
pub use flwrs_plugin_kit::schema::{escape, resolve};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

/// HTTP methods an OpenAPI path item can have.
pub const METHODS: [&str; 8] = [
    "get", "put", "post", "delete", "options", "head", "patch", "trace",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Location {
    Path,
    Query,
    Header,
    Cookie,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Param {
    pub name: String,
    pub location: Location,
    pub required: bool,
    /// Query arrays: `?a=1&a=2` (explode, the default) or `?a=1,2`.
    pub explode: bool,
    /// Where its schema is in the document (a JSON pointer), if it has one.
    pub schema: Option<String>,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaKind {
    /// `application/json`, `…+json`: parsed, validated, typed.
    Json,
    /// `application/x-www-form-urlencoded`: parsed, validated, typed.
    Form,
    /// Anything else: passed through as bytes.
    Bytes,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Media {
    /// Lowercase, without parameters (`application/json`, `image/*`, `*/*`).
    pub media_type: String,
    pub kind: MediaKind,
    pub schema: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Body {
    pub required: bool,
    pub content: Vec<Media>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Operation {
    /// The template as in the document (`/devices/{id}`).
    pub path: String,
    /// Lowercase (`post`).
    pub method: String,
    pub operation_id: Option<String>,
    pub summary: Option<String>,
    pub params: Vec<Param>,
    pub body: Option<Body>,
    /// The status of a successful request: the lowest 2xx response (202 without one).
    pub success: u16,
}

/// A loaded document.
#[derive(Debug, Clone)]
pub struct Api {
    pub doc: Value,
    pub operations: Vec<Operation>,
    pub warnings: Vec<String>,
}

fn kind_of(media_type: &str) -> MediaKind {
    if media_type == "application/json" || media_type.ends_with("+json") {
        MediaKind::Json
    } else if media_type == "application/x-www-form-urlencoded" {
        MediaKind::Form
    } else {
        MediaKind::Bytes
    }
}

/// Parses the `openapi` setting: a JSON or YAML string, or a JSON object.
pub fn parse(setting: &Value) -> Result<Value, String> {
    match setting {
        Value::String(text) => yaml_serde::from_str::<Value>(text)
            .map_err(|e| format!("openapi: neither JSON nor YAML: {e}")),
        Value::Object(_) => Ok(setting.clone()),
        _ => Err("openapi must be a document (an object) or its text (JSON or YAML)".into()),
    }
}

/// Loads the document: every problem, or the operations.
pub fn load(doc: Value) -> Result<Api, Vec<String>> {
    let mut errors = vec![];
    let mut warnings = vec![];
    // The version first: older documents don't even parse as 3.1.
    match doc.get("openapi").and_then(Value::as_str) {
        Some(v) if v.starts_with("3.1.") || v == "3.1" => {}
        Some(v) => return Err(vec![format!("OpenAPI 3.1 is needed; this document is {v}")]),
        None => {
            return Err(vec![
                "not a valid OpenAPI document: no \"openapi\" version".into(),
            ]);
        }
    }
    let text = doc.to_string();
    match oas3::from_json(&text) {
        Err(e) => return Err(vec![format!("not a valid OpenAPI document: {e}")]),
        Ok(spec) => match spec.validate_version() {
            Ok(v) if v.major == 3 && v.minor == 1 => {}
            Ok(v) => return Err(vec![format!("OpenAPI 3.1 is needed; this document is {v}")]),
            Err(e) => return Err(vec![format!("openapi version: {e}")]),
        },
    }
    external_refs(&doc, "", &mut errors);
    if !errors.is_empty() {
        return Err(errors);
    }
    if doc
        .get("webhooks")
        .and_then(Value::as_object)
        .is_some_and(|w| !w.is_empty())
    {
        warnings.push("webhooks are requests this API sends: ignored".into());
    }
    if doc
        .get("security")
        .and_then(Value::as_array)
        .is_some_and(|s| !s.is_empty())
    {
        warnings.push(
            "security requirements aren't enforced from the document: the node's auth setting is"
                .into(),
        );
    }
    if let Some(servers) = doc.get("servers").and_then(Value::as_array) {
        for (i, server) in servers.iter().enumerate() {
            let url = server
                .get("url")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let path = url
                .split_once("://")
                .map_or(url, |(_, rest)| rest.split_once('/').map_or("", |(_, p)| p));
            if !path.trim_matches('/').is_empty() {
                warnings.push(format!(
                    "servers[{i}] has the path /{}: routes are served at the paths as written (set base_path to prefix them)",
                    path.trim_matches('/')
                ));
            }
        }
    }
    let mut operations = vec![];
    let mut shapes: BTreeMap<String, String> = BTreeMap::new();
    let paths = doc
        .get("paths")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    for (path, item) in &paths {
        let item_ptr = format!("/paths/{}", escape(path));
        if item.get("$ref").is_some() {
            errors.push(format!(
                "#{item_ptr}: path items by reference aren't supported"
            ));
            continue;
        }
        // The router: whole-segment parameters only, and no two templates
        // that differ only in their parameter names.
        let mut names = BTreeSet::new();
        let mut shape = String::new();
        for segment in path.split('/').skip(1) {
            shape.push('/');
            if segment.starts_with('{')
                && segment.ends_with('}')
                && segment.matches('{').count() == 1
            {
                names.insert(segment[1..segment.len() - 1].to_string());
                shape.push_str("{}");
            } else if segment.contains(['{', '}']) {
                errors.push(format!(
                    "{path}: a parameter must be a whole path segment (like /files/{{name}}), not part of one"
                ));
            } else {
                shape.push_str(segment);
            }
        }
        if !path.starts_with('/') {
            errors.push(format!("{path}: a path must start with /"));
        }
        if let Some(other) = shapes.insert(shape, path.clone()) {
            errors.push(format!(
                "{path} and {other} differ only in their parameter names"
            ));
        }
        let shared = params(
            &doc,
            item.get("parameters"),
            &format!("{item_ptr}/parameters"),
            &mut errors,
        );
        for method in METHODS {
            let Some(op) = item.get(method) else { continue };
            let op_ptr = format!("{item_ptr}/{method}");
            let mut op_params = params(
                &doc,
                op.get("parameters"),
                &format!("{op_ptr}/parameters"),
                &mut errors,
            );
            // Path-level parameters, unless the operation overrides them.
            for p in &shared {
                if !op_params
                    .iter()
                    .any(|o| o.name == p.name && o.location == p.location)
                {
                    op_params.push(p.clone());
                }
            }
            for name in &names {
                if !op_params
                    .iter()
                    .any(|p| p.location == Location::Path && p.name == *name)
                {
                    errors.push(format!(
                        "{method} {path}: the path parameter {name:?} isn't declared"
                    ));
                }
            }
            for p in op_params.iter().filter(|p| p.location == Location::Path) {
                if !names.contains(&p.name) {
                    errors.push(format!(
                        "{method} {path}: the path parameter {:?} isn't in the path",
                        p.name
                    ));
                }
            }
            if op_params.iter().any(|p| p.location == Location::Cookie) {
                warnings.push(format!(
                    "{method} {path}: cookie parameters aren't typed (cookies never reach events)"
                ));
            }
            if op
                .get("callbacks")
                .and_then(Value::as_object)
                .is_some_and(|c| !c.is_empty())
            {
                warnings.push(format!(
                    "{method} {path}: callbacks are requests this API sends: ignored"
                ));
            }
            let body = match op.get("requestBody") {
                None => None,
                Some(_) => match resolve(&doc, &format!("{op_ptr}/requestBody")) {
                    Err(e) => {
                        errors.push(e);
                        None
                    }
                    Ok((body, body_ptr)) => {
                        let required = body
                            .get("required")
                            .and_then(Value::as_bool)
                            .unwrap_or(false);
                        let content = body
                            .get("content")
                            .and_then(Value::as_object)
                            .map(|content| {
                                content
                                    .iter()
                                    .map(|(media_type, media)| {
                                        let media_type = media_type
                                            .split(';')
                                            .next()
                                            .unwrap_or_default()
                                            .trim()
                                            .to_ascii_lowercase();
                                        let schema = media.get("schema").map(|_| {
                                            format!(
                                                "{body_ptr}/content/{}/schema",
                                                escape(&media_type)
                                            )
                                        });
                                        Media {
                                            kind: kind_of(&media_type),
                                            media_type,
                                            schema,
                                        }
                                    })
                                    .collect()
                            })
                            .unwrap_or_default();
                        Some(Body { required, content })
                    }
                },
            };
            let success = op
                .get("responses")
                .and_then(Value::as_object)
                .into_iter()
                .flat_map(|r| r.keys())
                .filter_map(|code| match code.as_str() {
                    "2XX" | "2xx" => Some(200),
                    c => c.parse::<u16>().ok().filter(|c| (200..300).contains(c)),
                })
                .min()
                .unwrap_or(202);
            operations.push(Operation {
                path: path.clone(),
                method: method.to_string(),
                operation_id: op
                    .get("operationId")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                summary: op
                    .get("summary")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                params: op_params,
                body,
                success,
            });
        }
    }
    if errors.is_empty() {
        Ok(Api {
            doc,
            operations,
            warnings,
        })
    } else {
        Err(errors)
    }
}

fn params(
    doc: &Value,
    list: Option<&Value>,
    list_ptr: &str,
    errors: &mut Vec<String>,
) -> Vec<Param> {
    let Some(items) = list.and_then(Value::as_array) else {
        return vec![];
    };
    let mut out = vec![];
    for i in 0..items.len() {
        match resolve(doc, &format!("{list_ptr}/{i}")) {
            Err(e) => errors.push(e),
            Ok((p, ptr)) => {
                let name = p
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let location = match p.get("in").and_then(Value::as_str) {
                    Some("path") => Location::Path,
                    Some("query") => Location::Query,
                    Some("header") => Location::Header,
                    Some("cookie") => Location::Cookie,
                    other => {
                        errors.push(format!(
                            "#{ptr}: \"in\" {other:?} must be path, query, header or cookie"
                        ));
                        continue;
                    }
                };
                if p.get("content").is_some() {
                    errors.push(format!("#{ptr}: parameters with \"content\" (instead of \"schema\") aren't supported"));
                    continue;
                }
                let style = p.get("style").and_then(Value::as_str);
                if location == Location::Query && !matches!(style, None | Some("form")) {
                    errors.push(format!(
                        "#{ptr}: query style {style:?} isn't supported (only \"form\")"
                    ));
                }
                out.push(Param {
                    required: location == Location::Path
                        || p.get("required").and_then(Value::as_bool).unwrap_or(false),
                    explode: p.get("explode").and_then(Value::as_bool).unwrap_or(true),
                    schema: p.get("schema").map(|_| format!("{ptr}/schema")),
                    description: p
                        .get("description")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    name,
                    location,
                });
            }
        }
    }
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;

    pub(crate) const DEVICES: &str = r##"
openapi: 3.1.0
info: { title: Devices, version: "1" }
servers: [{ url: "https://api.example.com/v1" }]
paths:
  /devices/{id}:
    parameters:
      - { name: id, in: path, required: true, schema: { type: string } }
    post:
      operationId: report
      parameters:
        - { name: verbose, in: query, schema: { type: boolean } }
      requestBody:
        required: true
        content:
          application/json:
            schema: { $ref: "#/components/schemas/Reading" }
      responses: { "201": { description: ok }, "400": { description: bad } }
    get:
      responses: { "200": { description: ok } }
components:
  schemas:
    Reading:
      type: object
      properties:
        temp: { type: number }
        at: { type: string, format: date-time }
"##;

    #[test]
    fn loads_operations_from_yaml() {
        let api = load(parse(&Value::String(DEVICES.into())).unwrap()).unwrap();
        assert_eq!(api.operations.len(), 2);
        let post = api.operations.iter().find(|o| o.method == "post").unwrap();
        assert_eq!((post.path.as_str(), post.success), ("/devices/{id}", 201));
        assert_eq!(post.params.len(), 2, "the path-level id comes along");
        let body = post.body.as_ref().unwrap();
        assert!(body.required);
        assert_eq!(body.content[0].kind, MediaKind::Json);
        let (schema, ptr) = resolve(&api.doc, body.content[0].schema.as_deref().unwrap()).unwrap();
        assert_eq!(ptr, "/components/schemas/Reading");
        assert_eq!(schema["properties"]["temp"]["type"], "number");
        let get = api.operations.iter().find(|o| o.method == "get").unwrap();
        assert_eq!(get.success, 200);
        assert!(api.warnings[0].contains("servers[0] has the path /v1"));
    }

    #[test]
    fn refuses_what_it_cant_serve() {
        let errors = |doc: Value| load(doc).unwrap_err().join(" | ");
        let base = |paths: Value| json!({"openapi": "3.1.0", "info": {"title": "t", "version": "1"}, "paths": paths});
        assert!(
            errors(
                json!({"openapi": "3.0.3", "info": {"title": "t", "version": "1"}, "paths": {}})
            )
            .contains("OpenAPI 3.1 is needed")
        );
        assert!(errors(json!({"swagger": "2.0"})).contains("no \"openapi\" version"));
        let e = errors(base(
            json!({"/a": {"get": {"requestBody": {"$ref": "other.yaml#/x"}, "responses": {}}}}),
        ));
        assert!(e.contains("self-contained"), "{e}");
        let e = errors(base(json!({"/f/{name}.json": {"get": {"responses": {}}}})));
        assert!(e.contains("whole path segment"), "{e}");
        let e = errors(base(json!({
            "/a/{x}": {"get": {"parameters": [{"name": "x", "in": "path", "required": true}], "responses": {}}},
            "/a/{y}": {"get": {"parameters": [{"name": "y", "in": "path", "required": true}], "responses": {}}}
        })));
        assert!(e.contains("differ only in their parameter names"), "{e}");
        let e = errors(base(json!({"/a/{x}": {"get": {"responses": {}}}})));
        assert!(e.contains("isn't declared"), "{e}");
    }
}
