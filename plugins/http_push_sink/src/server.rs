//! The subscriber side: `GET <base>/subscribe/{channel}` streams a channel
//! as Server-Sent Events (HTTP/1.1 and HTTP/2; `Last-Event-ID` or
//! `?last_event_id=` resumes), `GET <base>/poll/{channel}` long-polls it
//! (`?cursor=&max=&wait_s=`, a JSON answer). CORS for browsers
//! (`allowed_origins`); auth and limits come before, from the kit's core.

use crate::config::SseFormat;
use axum::Router;
use axum::body::Body;
use axum::extract::{Path as UrlPath, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use flwrs_plugin_kit::broadcast::{
    Broadcaster, Item, Refused, Stored, Subscriber, envelope, text_of,
};
use flwrs_plugin_kit::server::is_preflight;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;

pub struct App {
    pub broadcaster: Arc<Broadcaster>,
    pub sse_format: SseFormat,
    pub heartbeat: Duration,
    pub retry_ms: u64,
    pub poll_max_events: usize,
    pub poll_max_wait: Duration,
    pub allowed_origins: Vec<String>,
}

impl App {
    fn origin_allowed(&self, origin: &str) -> bool {
        self.allowed_origins
            .iter()
            .any(|a| a == "*" || a.eq_ignore_ascii_case(origin.trim_end_matches('/')))
    }
}

pub fn routes(app: Arc<App>, base_path: &str) -> Router {
    Router::new()
        .route(
            &format!("{base_path}/subscribe/{{*channel}}"),
            get(subscribe).options(preflight),
        )
        .route(
            &format!("{base_path}/poll/{{*channel}}"),
            get(poll).options(preflight),
        )
        .layer(middleware::from_fn_with_state(app.clone(), cors))
        .with_state(app)
}

/// CORS: preflights answered here (the kit lets them past auth), and the
/// allowed `Origin` echoed on answers.
async fn cors(State(app): State<Arc<App>>, request: Request, next: Next) -> Response {
    let origin = request
        .headers()
        .get(header::ORIGIN)
        .and_then(|o| o.to_str().ok())
        .filter(|o| app.origin_allowed(o))
        .and_then(|o| HeaderValue::from_str(o).ok());
    if is_preflight(&request) {
        let Some(origin) = origin else {
            return (StatusCode::FORBIDDEN, "this Origin may not subscribe").into_response();
        };
        let mut response = StatusCode::NO_CONTENT.into_response();
        let h = response.headers_mut();
        h.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin);
        h.insert(
            header::ACCESS_CONTROL_ALLOW_METHODS,
            HeaderValue::from_static("GET, OPTIONS"),
        );
        if let Some(asked) = request
            .headers()
            .get(header::ACCESS_CONTROL_REQUEST_HEADERS)
        {
            h.insert(header::ACCESS_CONTROL_ALLOW_HEADERS, asked.clone());
        }
        h.insert(
            header::ACCESS_CONTROL_MAX_AGE,
            HeaderValue::from_static("600"),
        );
        h.insert(header::VARY, HeaderValue::from_static("origin"));
        return response;
    }
    let mut response = next.run(request).await;
    if let Some(origin) = origin {
        let h = response.headers_mut();
        h.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin);
        h.insert(header::VARY, HeaderValue::from_static("origin"));
    }
    response
}

/// Reached only when `cors` let a preflight through (it never does).
async fn preflight() -> StatusCode {
    StatusCode::FORBIDDEN
}

fn refused(e: Refused) -> Response {
    match e {
        Refused::Channel(why) => (StatusCode::NOT_FOUND, why).into_response(),
        e => (
            StatusCode::SERVICE_UNAVAILABLE,
            [(header::RETRY_AFTER, "1")],
            e.to_string(),
        )
            .into_response(),
    }
}

/// One SSE event's text (`id:`, `event:`, `data:` lines).
pub fn sse_event(format: SseFormat, s: &Stored) -> String {
    let mut out = format!("id: {}\n", s.id);
    if let Some(e) = &s.message.event {
        // Field values can't hold line breaks.
        out.push_str(&format!("event: {}\n", e.replace(['\r', '\n'], " ")));
    }
    let data = match format {
        SseFormat::Json => envelope(s).to_string(),
        SseFormat::Raw => {
            let (text, base64) = text_of(&s.message.data);
            if base64 {
                out.push_str("encoding: base64\n");
            }
            text
        }
    };
    for line in data.split('\n') {
        out.push_str("data: ");
        out.push_str(line.strip_suffix('\r').unwrap_or(line));
        out.push('\n');
    }
    out.push('\n');
    out
}

fn sse_marker(event: &str, data: Value) -> String {
    format!("event: {event}\ndata: {data}\n\n")
}

#[derive(Deserialize, Default)]
struct SubscribeQuery {
    last_event_id: Option<String>,
}

async fn subscribe(
    State(app): State<Arc<App>>,
    UrlPath(channel): UrlPath<String>,
    Query(q): Query<SubscribeQuery>,
    headers: HeaderMap,
) -> Response {
    let last = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
        .or(q.last_event_id)
        .filter(|s| !s.is_empty());
    let subscriber = match app.broadcaster.subscribe(&channel, last.as_deref()) {
        Ok(s) => s,
        Err(e) => return refused(e),
    };
    let mut heartbeat =
        tokio::time::interval_at(tokio::time::Instant::now() + app.heartbeat, app.heartbeat);
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    struct State_ {
        app: Arc<App>,
        sub: Subscriber,
        heartbeat: tokio::time::Interval,
        started: bool,
        done: bool,
    }
    let state = State_ {
        app: app.clone(),
        sub: subscriber,
        heartbeat,
        started: false,
        done: false,
    };
    let stream = futures_util::stream::unfold(state, |mut st| async move {
        if st.done {
            return None;
        }
        if !st.started {
            st.started = true;
            let hello = format!("retry: {}\n: {}\n\n", st.app.retry_ms, st.sub.channel());
            return Some((
                Ok::<_, std::convert::Infallible>(bytes::Bytes::from(hello)),
                st,
            ));
        }
        let chunk = tokio::select! {
            item = st.sub.next() => match item {
                Item::Event(stored) => {
                    let text = sse_event(st.app.sse_format, &stored);
                    // Handed to the connection: that's a delivery.
                    st.sub.delivered(&stored);
                    text
                }
                Item::Gap { missed } => sse_marker("flwrs-gap", json!({ "missed": missed })),
                Item::Lagged => {
                    st.done = true;
                    sse_marker("flwrs-lagged", json!({}))
                }
                Item::Closed => return None,
            },
            _ = st.heartbeat.tick() => ": keep-alive\n\n".to_string(),
        };
        Some((Ok(bytes::Bytes::from(chunk)), st))
    });
    (
        [
            (header::CONTENT_TYPE, "text/event-stream"),
            (header::CACHE_CONTROL, "no-cache"),
            (header::HeaderName::from_static("x-accel-buffering"), "no"),
        ],
        Body::from_stream(stream),
    )
        .into_response()
}

#[derive(Deserialize, Default)]
struct PollQuery {
    cursor: Option<String>,
    max: Option<usize>,
    wait_s: Option<u64>,
}

async fn poll(
    State(app): State<Arc<App>>,
    UrlPath(channel): UrlPath<String>,
    Query(q): Query<PollQuery>,
) -> Response {
    let mut sub = match app
        .broadcaster
        .subscribe(&channel, q.cursor.as_deref().filter(|c| !c.is_empty()))
    {
        Ok(s) => s,
        Err(e) => return refused(e),
    };
    let max = q
        .max
        .unwrap_or(app.poll_max_events)
        .clamp(1, app.poll_max_events);
    let wait = q
        .wait_s
        .map_or(app.poll_max_wait, Duration::from_secs)
        .min(app.poll_max_wait);
    let mut items = sub.take_available(max);
    if items.is_empty()
        && let Ok(item) = tokio::time::timeout(wait, sub.next()).await
    {
        let end = matches!(item, Item::Lagged | Item::Closed);
        items.push(item);
        if !end {
            items.extend(sub.take_available(max - 1));
        }
    }
    let mut events = vec![];
    let mut missed: Option<Option<u64>> = None;
    for item in items {
        match item {
            Item::Event(stored) => {
                events.push(envelope(&stored));
                sub.delivered(&stored);
            }
            Item::Gap { missed: m } => {
                missed = Some(match (missed.flatten(), m, missed) {
                    (_, None, _) | (_, _, Some(None)) => None,
                    (a, Some(b), _) => Some(a.unwrap_or(0) + b),
                })
            }
            Item::Lagged | Item::Closed => break,
        }
    }
    let mut answer = json!({ "events": events, "next_cursor": sub.cursor_id() });
    if let Some(m) = missed {
        answer["missed"] = json!(m);
    }
    ([(header::CACHE_CONTROL, "no-store")], axum::Json(answer)).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use flwrs_plugin_kit::broadcast::Message;

    fn stored(data: &[u8], event: Option<&str>) -> Stored {
        Stored {
            id: "e-1".into(),
            seq: 1,
            message: Message {
                event: event.map(str::to_string),
                content_type: Some("text/plain".into()),
                data: data.to_vec(),
            },
        }
    }

    #[test]
    fn sse_text() {
        assert_eq!(
            sse_event(SseFormat::Raw, &stored(b"a\nb", Some("note"))),
            "id: e-1\nevent: note\ndata: a\ndata: b\n\n"
        );
        assert_eq!(
            sse_event(SseFormat::Raw, &stored(&[0xff], None)),
            "id: e-1\nencoding: base64\ndata: /w==\n\n"
        );
        let json = sse_event(SseFormat::Json, &stored(b"hi", None));
        assert_eq!(
            json,
            "id: e-1\ndata: {\"content_type\":\"text/plain\",\"data\":\"hi\",\"id\":\"e-1\"}\n\n"
        );
        assert_eq!(envelope(&stored(&[0xff], None))["encoding"], "base64");
    }
}
