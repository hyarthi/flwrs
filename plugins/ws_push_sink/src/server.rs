//! The subscriber side: `GET <base>/ws/{channel}` upgrades to a WebSocket
//! (`?after=<id>` resumes from the ring). Before the upgrade: the Origin
//! check (403) and a subscription (404 for a channel that isn't allowed, 503
//! beyond the limits); auth and limits come first, from the kit's core. Then
//! each event is one message, by `frame`; an event counts as delivered once
//! its message is written. Client messages are ignored (pings answered).

use crate::config::Frame;
use axum::Router;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade, close_code};
use axum::extract::{Path as UrlPath, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use flwrs_plugin_kit::broadcast::{Broadcaster, Item, Refused, Stored, Subscriber, envelope};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

pub struct App {
    pub broadcaster: Arc<Broadcaster>,
    pub frame: Frame,
    pub subprotocols: Vec<String>,
    pub allowed_origins: Vec<String>,
    pub ping_interval: Duration,
    pub write_timeout: Duration,
    pub max_client_message_bytes: usize,
    pub read_buffer_bytes: usize,
    pub write_buffer_bytes: usize,
}

pub fn routes(app: Arc<App>, base_path: &str) -> Router {
    Router::new()
        .route(&format!("{base_path}/ws/{{*channel}}"), get(upgrade))
        .with_state(app)
}

#[derive(Deserialize, Default)]
struct UpgradeQuery {
    after: Option<String>,
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
            .any(|a| a == "*" || a.eq_ignore_ascii_case(origin.trim_end_matches('/'))),
    }
}

async fn upgrade(
    State(app): State<Arc<App>>,
    UrlPath(channel): UrlPath<String>,
    Query(q): Query<UpgradeQuery>,
    headers: HeaderMap,
    ws: Result<WebSocketUpgrade, axum::extract::ws::rejection::WebSocketUpgradeRejection>,
) -> Response {
    let Ok(ws) = ws else {
        return (
            StatusCode::UPGRADE_REQUIRED,
            [(header::UPGRADE, "websocket")],
            "a WebSocket endpoint",
        )
            .into_response();
    };
    if !origin_allowed(&app.allowed_origins, &headers) {
        return (StatusCode::FORBIDDEN, "this Origin may not connect").into_response();
    }
    // Subscribed before the upgrade: refusals are HTTP answers, and the
    // replay starts where the request asked.
    let subscriber = match app
        .broadcaster
        .subscribe(&channel, q.after.as_deref().filter(|a| !a.is_empty()))
    {
        Ok(s) => s,
        Err(Refused::Channel(why)) => return (StatusCode::NOT_FOUND, why).into_response(),
        Err(e) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                [(header::RETRY_AFTER, "1")],
                e.to_string(),
            )
                .into_response();
        }
    };
    ws.max_message_size(app.max_client_message_bytes)
        .max_frame_size(app.max_client_message_bytes)
        .protocols(app.subprotocols.clone())
        .read_buffer_size(app.read_buffer_bytes)
        .write_buffer_size(app.write_buffer_bytes)
        .on_upgrade(move |socket| async move { serve(&app, subscriber, socket).await })
}

/// An event as a message, by `frame`.
pub fn message(frame: Frame, s: &Stored) -> Message {
    let data = &s.message.data;
    match frame {
        Frame::Envelope => Message::text(envelope(s).to_string()),
        Frame::Binary => Message::binary(data.clone()),
        Frame::Text => Message::text(String::from_utf8_lossy(data).into_owned()),
        Frame::Auto => match std::str::from_utf8(data) {
            Ok(text) => Message::text(text.to_string()),
            Err(_) => Message::binary(data.clone()),
        },
    }
}

async fn close(socket: &mut WebSocket, code: u16, reason: &str) {
    let frame = CloseFrame {
        code,
        reason: reason.into(),
    };
    let _ = socket.send(Message::Close(Some(frame))).await;
}

async fn serve(app: &App, mut sub: Subscriber, mut socket: WebSocket) {
    let mut ping = tokio::time::interval(app.ping_interval);
    ping.tick().await;
    let mut answered = true;
    loop {
        tokio::select! {
            item = sub.next() => {
                let (out, stored) = match item {
                    Item::Event(stored) => (message(app.frame, &stored), Some(stored)),
                    Item::Gap { missed } => {
                        if app.frame != Frame::Envelope {
                            // Only envelope clients can tell a gap from data.
                            log::debug!("{}: a subscriber missed {missed:?} events", sub.channel());
                            continue;
                        }
                        (Message::text(json!({ "type": "gap", "missed": missed }).to_string()), None)
                    }
                    Item::Lagged => return close(&mut socket, close_code::POLICY, "fell behind the replay buffer").await,
                    Item::Closed => return close(&mut socket, close_code::AWAY, "stopping").await,
                };
                match tokio::time::timeout(app.write_timeout, socket.send(out)).await {
                    Ok(Ok(())) => {
                        if let Some(stored) = stored {
                            sub.delivered(&stored);
                        }
                    }
                    Ok(Err(_)) => return,
                    Err(_) => {
                        log::debug!("{}: a subscriber didn't take a message in time", sub.channel());
                        return;
                    }
                }
            }
            incoming = socket.recv() => match incoming {
                None | Some(Err(_)) | Some(Ok(Message::Close(_))) => return,
                // Anything shows the client is alive; its messages are ignored.
                Some(Ok(_)) => answered = true,
            },
            _ = ping.tick() => {
                if !answered {
                    return close(&mut socket, close_code::POLICY, "no pong").await;
                }
                answered = false;
                if socket.send(Message::Ping(Default::default())).await.is_err() {
                    return;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flwrs_plugin_kit::broadcast::Message as Msg;

    fn stored(data: &[u8]) -> Stored {
        Stored {
            id: "e-1".into(),
            seq: 1,
            message: Msg {
                event: Some("note".into()),
                content_type: None,
                data: data.to_vec(),
            },
        }
    }

    #[test]
    fn frames() {
        assert_eq!(message(Frame::Auto, &stored(b"hi")), Message::text("hi"));
        assert_eq!(
            message(Frame::Auto, &stored(&[0xff])),
            Message::binary(vec![0xff])
        );
        assert_eq!(
            message(Frame::Binary, &stored(b"hi")),
            Message::binary(b"hi".to_vec())
        );
        let Message::Text(t) = message(Frame::Envelope, &stored(&[0xff])) else {
            panic!()
        };
        let v: serde_json::Value = serde_json::from_str(t.as_str()).unwrap();
        assert_eq!(
            (
                v["id"].as_str(),
                v["event"].as_str(),
                v["encoding"].as_str()
            ),
            (Some("e-1"), Some("note"), Some("base64"))
        );
    }
}
