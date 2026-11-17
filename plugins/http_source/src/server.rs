//! Requests → events. Per operation of the document: content type (415),
//! path and query parameters (converted from text by their schema type, then
//! validated: 400), the body (JSON or a form: parsed and validated, 400;
//! other media types as bytes), then the event (the envelope plus
//! `paths[template][method]`), emitted by the ack mode. Auth, sizes and
//! limits come before, from the kit's server core.

use crate::config::AckMode;
use crate::spec::{Api, Location, MediaKind, Operation, Param, resolve};
use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path as UrlPath, Request};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{MethodFilter, MethodRouter};
use flwrs_plugin::plugin::core::Unconfirmed;
use flwrs_plugin::schema::payload::{FieldDefinition, SchemaDefinition};
use flwrs_plugin::source::local_sink::LocalSink;
use flwrs_plugin_kit::server::{ConnInfo, HeaderPolicy, Principal, query_pairs};
use flwrs_types::value::{Value as FValue, coerce_payload, from_json_base64};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// What every request handler shares.
pub struct App {
    pub sink: Arc<OnceLock<LocalSink>>,
    pub out_schema: SchemaDefinition,
    pub ack_mode: AckMode,
    pub enqueue_timeout: Duration,
    pub hide_error_details: bool,
    pub headers: HeaderPolicy,
    pub max_body_bytes: usize,
}

/// The JSON type a parameter or form field has (to convert its text).
#[derive(Debug, Clone, PartialEq)]
enum Shape {
    String,
    Integer,
    Number,
    Boolean,
    Array(Box<Shape>),
    Other,
}

struct ParamRt {
    param: Param,
    shape: Shape,
    validator: Option<jsonschema::Validator>,
}

struct MediaRt {
    media_type: String,
    kind: MediaKind,
    validator: Option<jsonschema::Validator>,
    /// Form bodies: each property's shape.
    form: BTreeMap<String, Shape>,
}

struct OpRt {
    op: Operation,
    params: Vec<ParamRt>,
    body_required: bool,
    media: Vec<MediaRt>,
    /// `paths[template][method]` in the out schema.
    def: Option<FieldDefinition>,
}

fn shape(doc: &Value, ptr: &str, depth: usize) -> Shape {
    let Ok((schema, ptr)) = resolve(doc, ptr) else {
        return Shape::Other;
    };
    let ty = match schema.get("type") {
        Some(Value::String(t)) => Some(t.as_str()),
        Some(Value::Array(ts)) => ts.iter().filter_map(Value::as_str).find(|t| *t != "null"),
        _ => None,
    };
    match ty {
        Some("string") => Shape::String,
        Some("integer") => Shape::Integer,
        Some("number") => Shape::Number,
        Some("boolean") => Shape::Boolean,
        Some("array") if depth < 4 => {
            Shape::Array(Box::new(shape(doc, &format!("{ptr}/items"), depth + 1)))
        }
        None if schema.get("enum").is_some() => Shape::String,
        _ => Shape::Other,
    }
}

/// Validates against the schema at `ptr`: the whole document with a root
/// `$ref`, so `#/components/…` resolve (its other keys aren't keywords).
fn validator(doc: &Value, ptr: &str) -> Result<jsonschema::Validator, String> {
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

fn object_field<'a>(fields: &'a [FieldDefinition], key: &str) -> Option<&'a FieldDefinition> {
    fields.iter().find(|f| f.key == key)
}

/// The router for the document's operations (under `base_path`).
pub fn routes(app: Arc<App>, api: &Api, base_path: &str) -> Result<Router, Vec<String>> {
    let mut errors = vec![];
    let paths_def = object_field(&app.out_schema.fields, "paths");
    let mut by_path: BTreeMap<String, MethodRouter> = BTreeMap::new();
    for op in &api.operations {
        let mut params = vec![];
        for p in &op.params {
            let (shape, validator) = match &p.schema {
                None => (Shape::String, None),
                Some(ptr) => match validator(&api.doc, ptr) {
                    Ok(v) => (shape(&api.doc, ptr, 0), Some(v)),
                    Err(e) => {
                        errors.push(e);
                        continue;
                    }
                },
            };
            params.push(ParamRt {
                param: p.clone(),
                shape,
                validator,
            });
        }
        let mut media = vec![];
        for m in op.body.iter().flat_map(|b| &b.content) {
            let validator = match (&m.schema, m.kind) {
                (Some(ptr), MediaKind::Json | MediaKind::Form) => match validator(&api.doc, ptr) {
                    Ok(v) => Some(v),
                    Err(e) => {
                        errors.push(e);
                        continue;
                    }
                },
                _ => None,
            };
            let mut form = BTreeMap::new();
            if let (MediaKind::Form, Some(ptr)) = (m.kind, &m.schema)
                && let Ok((schema, at)) = resolve(&api.doc, ptr)
                && let Some(props) = schema.get("properties").and_then(Value::as_object)
            {
                for key in props.keys() {
                    form.insert(
                        key.clone(),
                        shape(
                            &api.doc,
                            &format!("{at}/properties/{}", crate::spec::escape(key)),
                            0,
                        ),
                    );
                }
            }
            media.push(MediaRt {
                media_type: m.media_type.clone(),
                kind: m.kind,
                validator,
                form,
            });
        }
        let def = paths_def
            .and_then(|p| object_field(&p.object_fields, &op.path))
            .and_then(|p| object_field(&p.object_fields, &op.method))
            .cloned();
        let rt = Arc::new(OpRt {
            op: op.clone(),
            params,
            body_required: op.body.as_ref().is_some_and(|b| b.required),
            media,
            def,
        });
        let filter = match op.method.as_str() {
            "get" => MethodFilter::GET,
            "put" => MethodFilter::PUT,
            "post" => MethodFilter::POST,
            "delete" => MethodFilter::DELETE,
            "options" => MethodFilter::OPTIONS,
            "head" => MethodFilter::HEAD,
            "patch" => MethodFilter::PATCH,
            _ => MethodFilter::TRACE,
        };
        let app = app.clone();
        let handler = move |request: Request| {
            let (app, rt) = (app.clone(), rt.clone());
            async move { handle(&app, &rt, request).await }
        };
        let path = format!("{base_path}{}", op.path);
        let router = by_path
            .remove(&path)
            .unwrap_or_default()
            .on(filter, handler);
        by_path.insert(path, router);
    }
    if !errors.is_empty() {
        return Err(errors);
    }
    Ok(by_path
        .into_iter()
        .fold(Router::new(), |r, (path, methods)| r.route(&path, methods)))
}

/// A problem answer (`application/problem+json`).
fn problem(app: &App, status: StatusCode, title: &str, details: Vec<String>) -> Response {
    let mut body = json!({"type": "about:blank", "title": title, "status": status.as_u16()});
    if !app.hide_error_details && !details.is_empty() {
        body["errors"] = json!(details);
    }
    let mut response = (status, body.to_string()).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/problem+json"),
    );
    if status == StatusCode::SERVICE_UNAVAILABLE {
        response
            .headers_mut()
            .insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
    }
    response
}

/// A parameter's text as the JSON value its schema type wants.
fn convert_text(text: &str, shape: &Shape) -> Result<Value, String> {
    match shape {
        Shape::Integer => text
            .parse::<i64>()
            .map(Value::from)
            .map_err(|_| format!("{text:?} isn't an integer")),
        Shape::Number => text
            .parse::<f64>()
            .ok()
            .and_then(serde_json::Number::from_f64)
            .map(Value::Number)
            .ok_or_else(|| format!("{text:?} isn't a number")),
        Shape::Boolean => match text {
            "true" => Ok(Value::Bool(true)),
            "false" => Ok(Value::Bool(false)),
            _ => Err(format!("{text:?} isn't true or false")),
        },
        _ => Ok(Value::String(text.to_string())),
    }
}

fn convert_values(values: &[String], shape: &Shape, explode: bool) -> Result<Value, String> {
    match shape {
        Shape::Array(items) => {
            let parts: Vec<&str> = if explode {
                values.iter().map(String::as_str).collect()
            } else {
                values.iter().flat_map(|v| v.split(',')).collect()
            };
            parts
                .iter()
                .map(|v| convert_text(v, items))
                .collect::<Result<Vec<_>, _>>()
                .map(Value::Array)
        }
        _ => convert_text(
            values.first().map(String::as_str).unwrap_or_default(),
            shape,
        ),
    }
}

fn check(
    validator: &Option<jsonschema::Validator>,
    value: &Value,
    label: &str,
    problems: &mut Vec<String>,
) {
    if let Some(v) = validator {
        for error in v.iter_errors(value) {
            let at = error.instance_path().to_string();
            problems.push(if at.is_empty() {
                format!("{label}: {error}")
            } else {
                format!("{label}{at}: {error}")
            });
        }
    }
}

/// The media type a request's `Content-Type` matches, if any.
fn matching<'a>(media: &'a [MediaRt], content_type: &str) -> Option<&'a MediaRt> {
    let essence = content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    let major = essence.split('/').next().unwrap_or_default();
    media
        .iter()
        .find(|m| m.media_type == essence)
        .or_else(|| media.iter().find(|m| m.media_type == format!("{major}/*")))
        .or_else(|| media.iter().find(|m| m.media_type == "*/*"))
}

async fn handle(app: &App, rt: &OpRt, request: Request) -> Response {
    let (mut parts, body) = request.into_parts();
    let mut problems = vec![];

    // Parameters.
    let path_values: HashMap<String, String> =
        match <UrlPath<HashMap<String, String>> as axum::extract::FromRequestParts<()>>::from_request_parts(&mut parts, &()).await {
            Ok(UrlPath(values)) => values,
            Err(_) => HashMap::new(),
        };
    let mut query_values: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (k, v) in query_pairs(parts.uri.query().unwrap_or_default()) {
        query_values.entry(k).or_default().push(v);
    }
    let mut path_params = Map::new();
    let mut query = Map::new();
    for p in &rt.params {
        let (values, label, out) = match p.param.location {
            Location::Path => (
                path_values.get(&p.param.name).map(|v| vec![v.clone()]),
                "path",
                &mut path_params,
            ),
            Location::Query => (
                query_values.get(&p.param.name).cloned(),
                "query",
                &mut query,
            ),
            Location::Header | Location::Cookie => continue,
        };
        let label = format!("{label}.{}", p.param.name);
        match values {
            None if p.param.required => problems.push(format!("{label}: required")),
            None => {}
            Some(values) => match convert_values(&values, &p.shape, p.param.explode) {
                Err(e) => problems.push(format!("{label}: {e}")),
                Ok(value) => {
                    check(&p.validator, &value, &label, &mut problems);
                    out.insert(p.param.name.clone(), value);
                }
            },
        }
    }

    // The body.
    let raw = match axum::body::to_bytes(body, app.max_body_bytes).await {
        Ok(bytes) => bytes,
        Err(_) => {
            return problem(
                app,
                StatusCode::PAYLOAD_TOO_LARGE,
                "The body is too large",
                vec![],
            );
        }
    };
    let content_type = parts
        .headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let mut typed_body = None;
    if raw.is_empty() {
        if rt.body_required {
            problems.push("body: required".into());
        }
    } else if !rt.media.is_empty() {
        match matching(&rt.media, &content_type) {
            None => {
                let accepted: Vec<&str> = rt.media.iter().map(|m| m.media_type.as_str()).collect();
                return problem(
                    app,
                    StatusCode::UNSUPPORTED_MEDIA_TYPE,
                    "Unsupported content type",
                    vec![format!(
                        "{content_type:?} isn't one of: {}",
                        accepted.join(", ")
                    )],
                );
            }
            Some(media) => match media.kind {
                MediaKind::Json => match serde_json::from_slice::<Value>(&raw) {
                    Err(e) => problems.push(format!("body: not JSON: {e}")),
                    Ok(value) => {
                        check(&media.validator, &value, "body", &mut problems);
                        typed_body = Some(value);
                    }
                },
                MediaKind::Form => {
                    let text = String::from_utf8_lossy(&raw);
                    let mut grouped: BTreeMap<String, Vec<String>> = BTreeMap::new();
                    for (k, v) in query_pairs(&text) {
                        grouped.entry(k).or_default().push(v);
                    }
                    let mut form = Map::new();
                    for (key, values) in grouped {
                        let shape = media.form.get(&key).cloned().unwrap_or(Shape::String);
                        match convert_values(&values, &shape, true) {
                            Ok(v) => {
                                form.insert(key, v);
                            }
                            Err(e) => problems.push(format!("body.{key}: {e}")),
                        }
                    }
                    let value = Value::Object(form);
                    check(&media.validator, &value, "body", &mut problems);
                    typed_body = Some(value);
                }
                MediaKind::Bytes => {}
            },
        }
    }
    if !problems.is_empty() {
        return problem(
            app,
            StatusCode::BAD_REQUEST,
            "The request doesn't match the API",
            problems,
        );
    }

    // The event.
    let payload = match event(
        app,
        rt,
        &parts,
        &content_type,
        raw,
        path_params,
        query,
        typed_body,
    ) {
        Ok(p) => p,
        Err(e) => {
            return problem(
                app,
                StatusCode::BAD_REQUEST,
                "The request can't become an event",
                vec![e],
            );
        }
    };
    emit(app, payload, rt.op.success).await
}

#[allow(clippy::too_many_arguments)]
fn event(
    app: &App,
    rt: &OpRt,
    parts: &axum::http::request::Parts,
    content_type: &str,
    raw: Bytes,
    path_params: Map<String, Value>,
    query: Map<String, Value>,
    body: Option<Value>,
) -> Result<flwrs_plugin::schema::payload::PluginPayload, String> {
    let filtered = app.headers.filter(&parts.headers);
    let headers = FValue::Map(
        filtered
            .headers
            .into_iter()
            .map(|(k, vs)| {
                (
                    k,
                    FValue::Array(vs.into_iter().map(FValue::String).collect()),
                )
            })
            .collect(),
    );
    let conn = parts.extensions.get::<ConnInfo>();
    let principal = parts
        .extensions
        .get::<Principal>()
        .map(|p| p.0.clone())
        .unwrap_or_default();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let mut fields = vec![
        (
            "method".to_string(),
            FValue::String(parts.method.as_str().to_string()),
        ),
        ("path".into(), FValue::String(rt.op.path.clone())),
        (
            "uri".into(),
            FValue::String(
                parts
                    .uri
                    .path_and_query()
                    .map(|p| p.to_string())
                    .unwrap_or_default(),
            ),
        ),
        ("headers".into(), headers),
        (
            "content_type".into(),
            if content_type.is_empty() {
                FValue::Null
            } else {
                FValue::String(content_type.into())
            },
        ),
        ("body".into(), FValue::Bytes(raw.to_vec())),
        (
            "remote_addr".into(),
            conn.map_or(FValue::Null, |c| FValue::String(c.remote_addr.to_string())),
        ),
        (
            "http_version".into(),
            FValue::String(format!("{:?}", parts.version)),
        ),
        (
            "principal".into(),
            if principal.is_empty() {
                FValue::Null
            } else {
                FValue::String(principal)
            },
        ),
        (
            "receive_time".into(),
            FValue::DateTime(pbjson_types::Timestamp {
                seconds: now.as_secs() as i64,
                nanos: now.subsec_nanos() as i32,
            }),
        ),
    ];
    if let Some(def) = &rt.def {
        let mut op = Map::new();
        if !path_params.is_empty() {
            op.insert("path_params".into(), Value::Object(path_params));
        }
        if !query.is_empty() {
            op.insert("query".into(), Value::Object(query));
        }
        if let Some(body) = body
            && object_field(&def.object_fields, "body").is_some()
        {
            op.insert("body".into(), body);
        }
        let typed = from_json_base64(&Value::Object(op), Some(def))?;
        let method = FValue::Object(vec![(rt.op.method.clone(), typed)]);
        fields.push((
            "paths".into(),
            FValue::Object(vec![(rt.op.path.clone(), method)]),
        ));
    }
    coerce_payload(FValue::Object(fields), &app.out_schema)?.into_payload()
}

async fn emit(
    app: &App,
    payload: flwrs_plugin::schema::payload::PluginPayload,
    success: u16,
) -> Response {
    let ok = || {
        StatusCode::from_u16(success)
            .unwrap_or(StatusCode::ACCEPTED)
            .into_response()
    };
    let Some(sink) = app.sink.get().filter(|s| s.is_connected()) else {
        return problem(
            app,
            StatusCode::SERVICE_UNAVAILABLE,
            "Not connected to its workflow",
            vec![],
        );
    };
    let busy = || {
        problem(
            app,
            StatusCode::SERVICE_UNAVAILABLE,
            "The workflow didn't take the event in time",
            vec![],
        )
    };
    match app.ack_mode {
        AckMode::Accepted => {
            match tokio::time::timeout(app.enqueue_timeout, sink.emit(payload)).await {
                Ok(Ok(())) => ok(),
                _ => busy(),
            }
        }
        AckMode::Confirmed => {
            match tokio::time::timeout(app.enqueue_timeout, sink.emit_tracked(payload)).await {
                Ok(Ok(confirmation)) => match confirmation.confirmed().await {
                    Ok(()) => ok(),
                    Err(Unconfirmed::Refused {
                        reason,
                        retry: false,
                    }) => problem(
                        app,
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "The workflow refused the event",
                        vec![reason],
                    ),
                    Err(_) => busy(),
                },
                _ => busy(),
            }
        }
    }
}
