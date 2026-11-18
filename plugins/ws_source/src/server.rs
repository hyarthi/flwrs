//! WebSocket connections → events. The upgrade (a `GET` on a channel's
//! address, or on one of the `paths` without a document) goes through the
//! kit's server core first (auth, limits, health); then the Origin check, a
//! free connection slot and the subprotocol. Each text or binary message
//! becomes an event: with a document, JSON messages are matched against the
//! channel's messages (the first whose payload schema validates), typed
//! under `channels["<address>"]`; a message no definition takes is dropped
//! or closes the connection. Messages are read one at a time and each waits
//! until it's sent to the hub, so a slow workflow slows the client down
//! (TCP backpressure); one not taken in time closes the connection (1013).

use crate::config::InvalidMessages;
use crate::spec::{Api, Channel};
use axum::Router;
use axum::extract::ws::rejection::WebSocketUpgradeRejection;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade, close_code};
use axum::extract::{Path as UrlPath, Request};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use flwrs_plugin::schema::payload::{FieldDefinition, PluginPayload, SchemaDefinition};
use flwrs_plugin::source::local_sink::LocalSink;
use flwrs_plugin_kit::schema::validator;
use flwrs_plugin_kit::server::{ConnInfo, HeaderPolicy, Principal};
use flwrs_types::value::{Value as FValue, coerce_payload, from_json_base64};
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::{Semaphore, watch};

/// What every connection shares.
pub struct App {
    pub sink: Arc<OnceLock<LocalSink>>,
    pub out_schema: SchemaDefinition,
    pub headers: HeaderPolicy,
    pub subprotocols: Vec<String>,
    pub allowed_origins: Vec<String>,
    pub max_message_bytes: usize,
    pub read_buffer_bytes: usize,
    pub ping_interval: Duration,
    pub enqueue_timeout: Duration,
    pub invalid_messages: InvalidMessages,
    /// Open WebSockets (the kit's `max_connections`: upgraded connections
    /// leave the server core's own count).
    pub connections: Arc<Semaphore>,
    /// Set when the plugin stops: connections close with 1001.
    pub stopping: watch::Receiver<bool>,
    /// Messages dropped (no definition took them), for a WARN once a minute.
    pub dropped: Dropped,
}

#[derive(Default)]
pub struct Dropped {
    count: AtomicU64,
    last_warn: Mutex<Option<Instant>>,
}

impl Dropped {
    fn note(&self, why: &str) {
        let n = self.count.fetch_add(1, Ordering::Relaxed) + 1;
        let mut last = self.last_warn.lock().unwrap_or_else(|e| e.into_inner());
        if last.is_none_or(|t| t.elapsed() >= Duration::from_secs(60)) {
            *last = Some(Instant::now());
            log::warn!("dropped a message the document doesn't describe ({why}); {n} so far");
        } else {
            log::debug!("dropped a message the document doesn't describe ({why})");
        }
    }
}

/// A message definition, ready to match.
struct MessageRt {
    name: String,
    json: bool,
    validator: Option<jsonschema::Validator>,
    /// Typed in the out schema (JSON with a payload schema).
    typed: bool,
}

/// A path the server takes connections on.
struct Endpoint {
    address: String,
    /// `channels["<address>"]` in the out schema (with a document).
    def: Option<FieldDefinition>,
    params: bool,
    messages: Vec<MessageRt>,
}

fn object_field<'a>(fields: &'a [FieldDefinition], key: &str) -> Option<&'a FieldDefinition> {
    fields.iter().find(|f| f.key == key)
}

fn endpoint(app: &App, api: &Api, channel: &Channel) -> Result<Endpoint, Vec<String>> {
    let mut errors = vec![];
    let def = object_field(&app.out_schema.fields, "channels")
        .and_then(|c| object_field(&c.object_fields, &channel.address))
        .cloned();
    let typed_names: Vec<String> = def
        .as_ref()
        .and_then(|d| object_field(&d.object_fields, "messages"))
        .map(|m| m.object_fields.iter().map(|f| f.key.clone()).collect())
        .unwrap_or_default();
    let mut messages = vec![];
    for m in &channel.messages {
        let validator = match (&m.payload, m.json) {
            (Some(ptr), true) => match validator(&api.doc, ptr) {
                Ok(v) => Some(v),
                Err(e) => {
                    errors.push(format!("{}: message {:?}: {e}", channel.address, m.name));
                    None
                }
            },
            _ => None,
        };
        messages.push(MessageRt {
            name: m.name.clone(),
            json: m.json,
            validator,
            typed: typed_names.contains(&m.name),
        });
    }
    if errors.is_empty() {
        Ok(Endpoint {
            address: channel.address.clone(),
            def,
            params: !channel.params.is_empty(),
            messages,
        })
    } else {
        Err(errors)
    }
}

/// The routes: one per channel address (with a document), or per path.
pub fn routes(app: Arc<App>, api: Option<&Api>, paths: &[String]) -> Result<Router, Vec<String>> {
    let mut endpoints = vec![];
    let mut errors = vec![];
    match api {
        Some(api) => {
            for channel in &api.channels {
                match endpoint(&app, api, channel) {
                    Ok(e) => endpoints.push(e),
                    Err(e) => errors.extend(e),
                }
            }
        }
        None => {
            for p in paths {
                endpoints.push(Endpoint {
                    address: p.clone(),
                    def: None,
                    params: false,
                    messages: vec![],
                });
            }
        }
    }
    if !errors.is_empty() {
        return Err(errors);
    }
    let mut router = Router::new();
    for ep in endpoints {
        let address = ep.address.clone();
        let ep = Arc::new(ep);
        let app = app.clone();
        router = router.route(
            &address,
            any(
                move |params: Option<UrlPath<HashMap<String, String>>>,
                      ws: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
                      request: Request| {
                    let (app, ep) = (app.clone(), ep.clone());
                    async move {
                        upgrade(
                            app,
                            ep,
                            params.map(|p| p.0).unwrap_or_default(),
                            ws.ok(),
                            request,
                        )
                        .await
                    }
                },
            ),
        );
    }
    Ok(router)
}

/// What a connection's events carry from its upgrade request.
struct Context {
    connection_id: String,
    uri: String,
    headers: FValue,
    remote_addr: FValue,
    principal: FValue,
    path_params: Map<String, Value>,
}

fn origin_allowed(allowed: &[String], headers: &HeaderMap) -> bool {
    if allowed.is_empty() {
        return true;
    }
    // Non-browser clients send no Origin; the check is for browsers.
    match headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) {
        None => true,
        Some(origin) => allowed
            .iter()
            .any(|a| a.eq_ignore_ascii_case(origin.trim_end_matches('/'))),
    }
}

async fn upgrade(
    app: Arc<App>,
    ep: Arc<Endpoint>,
    params: HashMap<String, String>,
    ws: Option<WebSocketUpgrade>,
    request: Request,
) -> Response {
    let Some(ws) = ws else {
        return (
            StatusCode::UPGRADE_REQUIRED,
            [(header::UPGRADE, "websocket")],
            "a WebSocket endpoint",
        )
            .into_response();
    };
    let (parts, _) = request.into_parts();
    if !origin_allowed(&app.allowed_origins, &parts.headers) {
        return (StatusCode::FORBIDDEN, "this Origin may not connect").into_response();
    }
    if app.sink.get().is_none_or(|s| !s.is_connected()) {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            [(header::RETRY_AFTER, "1")],
            "not connected to its workflow",
        )
            .into_response();
    }
    let Ok(slot) = app.connections.clone().try_acquire_owned() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            [(header::RETRY_AFTER, "1")],
            "too many connections",
        )
            .into_response();
    };
    let filtered = app.headers.filter(&parts.headers);
    let conn = parts.extensions.get::<ConnInfo>();
    let principal = parts.extensions.get::<Principal>().map(|p| p.0.clone());
    let ctx = Context {
        connection_id: ulid::Ulid::generate().to_string(),
        uri: parts
            .uri
            .path_and_query()
            .map(|p| p.to_string())
            .unwrap_or_default(),
        headers: FValue::Map(
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
        ),
        remote_addr: conn.map_or(FValue::Null, |c| FValue::String(c.remote_addr.to_string())),
        principal: principal
            .filter(|p| !p.is_empty())
            .map_or(FValue::Null, FValue::String),
        path_params: params
            .into_iter()
            .map(|(k, v)| (k, Value::String(v)))
            .collect(),
    };
    let ws = ws
        .max_message_size(app.max_message_bytes)
        .max_frame_size(app.max_message_bytes)
        .read_buffer_size(app.read_buffer_bytes)
        .protocols(app.subprotocols.clone());
    ws.on_upgrade(move |socket| async move {
        log::debug!("connection {} on {}", ctx.connection_id, ep.address);
        serve(&app, &ep, &ctx, socket).await;
        drop(slot);
        log::debug!("connection {} closed", ctx.connection_id);
    })
}

/// Waits until the plugin stops (the watch's guard dropped at once, so the
/// connection's future stays `Send`).
async fn stopped(stopping: &mut watch::Receiver<bool>) {
    let _ = stopping.wait_for(|s| *s).await;
}

async fn close(socket: &mut WebSocket, code: u16, reason: &str) {
    let frame = CloseFrame {
        code,
        reason: reason.into(),
    };
    let _ = socket.send(Message::Close(Some(frame))).await;
}

async fn serve(app: &App, ep: &Endpoint, ctx: &Context, mut socket: WebSocket) {
    let mut stopping = app.stopping.clone();
    if *stopping.borrow() {
        return close(&mut socket, close_code::AWAY, "stopping").await;
    }
    let mut ping = tokio::time::interval(app.ping_interval);
    ping.tick().await;
    let mut answered = true;
    loop {
        let message = tokio::select! {
            m = socket.recv() => m,
            _ = ping.tick() => {
                if !answered {
                    log::debug!("connection {}: no pong, closing", ctx.connection_id);
                    return close(&mut socket, close_code::POLICY, "no pong").await;
                }
                answered = false;
                if socket.send(Message::Ping(Default::default())).await.is_err() {
                    return;
                }
                continue;
            }
            _ = stopped(&mut stopping) => return close(&mut socket, close_code::AWAY, "stopping").await,
        };
        let (data, text) = match message {
            None | Some(Err(_)) | Some(Ok(Message::Close(_))) => return,
            Some(Ok(Message::Pong(_))) => {
                answered = true;
                continue;
            }
            Some(Ok(Message::Ping(_))) => continue,
            Some(Ok(Message::Text(t))) => (t.as_bytes().to_vec(), true),
            Some(Ok(Message::Binary(b))) => (b.to_vec(), false),
        };
        // Any message shows the client is alive.
        answered = true;
        let payload = match event(app, ep, ctx, data, text) {
            Ok(p) => p,
            Err(why) => match app.invalid_messages {
                InvalidMessages::Drop => {
                    app.dropped.note(&why);
                    continue;
                }
                InvalidMessages::Close => {
                    log::debug!("connection {}: {why}", ctx.connection_id);
                    let reason: String = why.chars().take(100).collect();
                    return close(&mut socket, close_code::POLICY, &reason).await;
                }
            },
        };
        let Some(sink) = app.sink.get() else { return };
        // Stopping wins over a wait for the workflow.
        let sent = tokio::select! {
            r = tokio::time::timeout(app.enqueue_timeout, sink.emit(payload)) => matches!(r, Ok(Ok(()))),
            _ = stopped(&mut stopping) => return close(&mut socket, close_code::AWAY, "stopping").await,
        };
        if !sent {
            return close(
                &mut socket,
                close_code::AGAIN,
                "the workflow didn't take the message in time",
            )
            .await;
        }
    }
}

/// Which message definition takes a message: its name and typed value.
fn classify(ep: &Endpoint, data: &[u8]) -> Result<(Option<String>, Option<Value>), String> {
    if ep.messages.is_empty() {
        return Ok((None, None));
    }
    let parsed = serde_json::from_slice::<Value>(data).ok();
    let mut problems = vec![];
    if let Some(value) = &parsed {
        for m in ep.messages.iter().filter(|m| m.json) {
            match &m.validator {
                None => return Ok((Some(m.name.clone()), None)),
                Some(v) => match v.iter_errors(value).next() {
                    None => return Ok((Some(m.name.clone()), m.typed.then(|| value.clone()))),
                    Some(e) => problems.push(format!("{}: {e}", m.name)),
                },
            }
        }
    }
    // Not JSON (or no JSON message took it): a non-JSON message, as bytes.
    if let Some(m) = ep.messages.iter().find(|m| !m.json) {
        return Ok((Some(m.name.clone()), None));
    }
    if parsed.is_none() {
        Err("not JSON".into())
    } else {
        Err(format!("no message matches: {}", problems.join("; ")))
    }
}

fn event(
    app: &App,
    ep: &Endpoint,
    ctx: &Context,
    data: Vec<u8>,
    text: bool,
) -> Result<PluginPayload, String> {
    let (name, typed) = classify(ep, &data)?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let mut fields = vec![
        (
            "connection_id".to_string(),
            FValue::String(ctx.connection_id.clone()),
        ),
        ("path".into(), FValue::String(ep.address.clone())),
        ("uri".into(), FValue::String(ctx.uri.clone())),
        (
            "message_type".into(),
            FValue::String(if text { "text" } else { "binary" }.into()),
        ),
        (
            "message".into(),
            name.clone().map_or(FValue::Null, FValue::String),
        ),
        ("data".into(), FValue::Bytes(data)),
        ("headers".into(), ctx.headers.clone()),
        ("remote_addr".into(), ctx.remote_addr.clone()),
        ("principal".into(), ctx.principal.clone()),
        (
            "receive_time".into(),
            FValue::DateTime(pbjson_types::Timestamp {
                seconds: now.as_secs() as i64,
                nanos: now.subsec_nanos() as i32,
            }),
        ),
    ];
    if let Some(def) = &ep.def {
        let mut channel = Map::new();
        if ep.params {
            channel.insert("path_params".into(), Value::Object(ctx.path_params.clone()));
        }
        if let (Some(name), Some(value)) = (name, typed) {
            channel.insert(
                "messages".into(),
                Value::Object(Map::from_iter([(name, value)])),
            );
        }
        let typed = from_json_base64(&Value::Object(channel), Some(def))?;
        fields.push((
            "channels".into(),
            FValue::Object(vec![(ep.address.clone(), typed)]),
        ));
    }
    coerce_payload(FValue::Object(fields), &app.out_schema)?.into_payload()
}
