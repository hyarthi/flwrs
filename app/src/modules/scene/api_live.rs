//! A node's live feeds, for whoever observes it: which channels it has, and
//! one stream per channel. Opening a stream is what tells the plugin to do the
//! work; closing the last one stops it.
//!
//! A channel's media type picks how it's streamed: raster images as
//! `multipart/x-mixed-replace` (an `<img src>` shows it), JSON as Server-Sent
//! Events. Anything else is listed but not served, so a plugin can't make the
//! browser render active content from this origin.

use super::api::Svc;
use super::api_scene_stream::scene_node;
use super::api_stream::json_event;
use crate::modules::api_error::{ApiError, ErrorBody, api_error};
use crate::modules::hub::live::{LiveFeeds, LiveFrame, LiveSubscription};
use axum::body::{Body, Bytes};
use axum::extract::{Path, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use futures_util::Stream;
use serde::Serialize;
use std::convert::Infallible;
use tokio_util::sync::CancellationToken;
use utoipa::{OpenApi, ToSchema};

#[derive(Clone)]
pub(crate) struct LiveState {
    pub scenes: Svc,
    pub feeds: LiveFeeds,
    /// Ends the streams (the HTTP server is stopping).
    pub stopping: CancellationToken,
}

/// How a channel can be shown.
#[derive(Serialize, ToSchema, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum LiveDisplay {
    /// A stream for an `<img>` (`multipart/x-mixed-replace`).
    Image,
    /// Server-Sent Events.
    Json,
}

fn display_of(media_type: &str) -> Option<LiveDisplay> {
    let media_type = media_type.to_ascii_lowercase();
    match media_type.as_str() {
        "image/jpeg" | "image/png" | "image/webp" => Some(LiveDisplay::Image),
        "application/json" => Some(LiveDisplay::Json),
        other if other.starts_with("application/") && other.ends_with("+json") => {
            Some(LiveDisplay::Json)
        }
        _ => None,
    }
}

/// A channel of the node's plugin.
#[derive(Serialize, ToSchema, Debug)]
pub(crate) struct LiveChannelView {
    pub channel: String,
    /// What the channel carries (an IANA media type).
    pub media_type: String,
    /// How it can be streamed; null = this server can't show it.
    pub display: Option<LiveDisplay>,
    /// Streams open on it now.
    pub viewers: usize,
}

/// The node's live channels.
#[derive(Serialize, ToSchema, Debug)]
pub(crate) struct LiveChannels {
    /// Those its plugin announced on its current connection; empty while it's
    /// not connected, or if it has none.
    pub channels: Vec<LiveChannelView>,
}

/// A `frame` event of a JSON channel.
#[derive(Serialize, ToSchema)]
pub(crate) struct LiveJsonFrame {
    /// Counts up per channel; a gap = items skipped.
    pub seq: u64,
    /// When it was captured (Unix milliseconds), if the plugin said.
    pub capture_time: Option<i64>,
    /// The item.
    #[schema(value_type = Object)]
    pub data: serde_json::Value,
}

#[utoipa::path(
    get,
    path = "/by-id/{id}/nodes/by-id/{node_id}/live",
    operation_id = "node-live-channels",
    summary = "A node's live channels",
    description = "The observation channels (stats, previews) its plugin announced on its current connection. Listing them costs the plugin nothing.",
    params(("id" = String, Path, description = "Scene ID"), ("node_id" = String, Path, description = "Node ID")),
    responses(
        (status = 200, description = "The channels", body = LiveChannels),
        (status = 404, description = "No such scene or node", body = ErrorBody),
    )
)]
async fn channels(
    State(state): State<LiveState>,
    Path((id, node_id)): Path<(String, String)>,
) -> Result<Json<LiveChannels>, ApiError> {
    scene_node(state.scenes, &id, &node_id).await?;
    let channels = state
        .feeds
        .channels(&node_id)
        .into_iter()
        .map(|c| LiveChannelView {
            display: display_of(&c.media_type),
            channel: c.channel,
            media_type: c.media_type,
            viewers: c.viewers,
        })
        .collect();
    Ok(Json(LiveChannels { channels }))
}

#[utoipa::path(
    get,
    path = "/by-id/{id}/nodes/by-id/{node_id}/live/{channel}/stream",
    operation_id = "node-live-stream",
    summary = "A live channel's stream",
    description = "Watches the channel for as long as the response is open: the plugin produces it only while someone does. Images (`image/jpeg`, `image/png`, `image/webp`) come as `multipart/x-mixed-replace`, for an `<img>`. JSON comes as Server-Sent Events: `frame` (`LiveJsonFrame`). Only the newest item is kept, so a slow reader skips items. A stream stays open while the plugin restarts, and ends when the channel changes its media type (list the channels again) or the server stops. Nothing is stored.",
    params(
        ("id" = String, Path, description = "Scene ID"),
        ("node_id" = String, Path, description = "Node ID"),
        ("channel" = String, Path, description = "Channel name"),
    ),
    responses(
        (status = 200, description = "The stream", content_type = "multipart/x-mixed-replace", body = String),
        (status = 404, description = "No such scene, node or channel", body = ErrorBody),
        (status = 415, description = "This server can't show the channel's media type", body = ErrorBody),
    )
)]
async fn stream(
    State(state): State<LiveState>,
    Path((id, node_id, channel)): Path<(String, String, String)>,
) -> Result<Response, ApiError> {
    scene_node(state.scenes, &id, &node_id).await?;
    let announced = state
        .feeds
        .channels(&node_id)
        .into_iter()
        .find(|c| c.channel == channel)
        .ok_or_else(|| api_error(StatusCode::NOT_FOUND, "no such live channel", vec![]))?;
    let Some(display) = display_of(&announced.media_type) else {
        return Err(api_error(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "this server can't show the channel's media type",
            vec![announced.media_type],
        ));
    };
    let subscription = state
        .feeds
        .subscribe(&node_id, &channel)
        .ok_or_else(|| api_error(StatusCode::NOT_FOUND, "no such live channel", vec![]))?;
    let mut response = match display {
        LiveDisplay::Image => image_stream(subscription, state.stopping),
        LiveDisplay::Json => Sse::new(json_stream(subscription, state.stopping))
            .keep_alive(KeepAlive::default())
            .into_response(),
    };
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("default-src 'none'; sandbox"),
    );
    Ok(response)
}

/// The next item, or `None` when the stream should end.
async fn next_frame(
    subscription: &mut LiveSubscription,
    stopping: &CancellationToken,
) -> Option<std::sync::Arc<LiveFrame>> {
    tokio::select! {
        () = stopping.cancelled() => None,
        frame = subscription.next() => frame,
    }
}

/// Each part is followed at once by the next boundary: a browser shows a part
/// when it sees the boundary, so this doesn't hold a frame back until the
/// next one is ready.
fn image_stream(subscription: LiveSubscription, stopping: CancellationToken) -> Response {
    let boundary = format!("flwrs-live-{}", ulid::Ulid::generate());
    let content_type = format!("multipart/x-mixed-replace; boundary={boundary}");
    let first = Bytes::from(format!("--{boundary}\r\n"));
    let rest = futures_util::stream::unfold(
        (subscription, stopping, boundary),
        |(mut subscription, stopping, boundary)| async move {
            let frame = next_frame(&mut subscription, &stopping).await?;
            let head = format!(
                "Content-Type: {}\r\nContent-Length: {}\r\n\r\n",
                frame.media_type,
                frame.data.len()
            );
            let tail = format!("\r\n--{boundary}\r\n");
            let mut part = Vec::with_capacity(head.len() + frame.data.len() + tail.len());
            part.extend_from_slice(head.as_bytes());
            part.extend_from_slice(&frame.data);
            part.extend_from_slice(tail.as_bytes());
            Some((
                Ok::<_, Infallible>(Bytes::from(part)),
                (subscription, stopping, boundary),
            ))
        },
    );
    let body = futures_util::StreamExt::chain(
        futures_util::stream::once(async move { Ok::<_, Infallible>(first) }),
        rest,
    );
    let mut response = Body::from_stream(body).into_response();
    if let Ok(value) = HeaderValue::from_str(&content_type) {
        response.headers_mut().insert(header::CONTENT_TYPE, value);
    }
    response
}

fn json_stream(
    subscription: LiveSubscription,
    stopping: CancellationToken,
) -> impl Stream<Item = Result<Event, Infallible>> {
    futures_util::stream::unfold(
        (subscription, stopping),
        |(mut subscription, stopping)| async move {
            loop {
                let frame = next_frame(&mut subscription, &stopping).await?;
                // Parsed, so that what's sent is one valid JSON line whatever
                // the plugin wrote. A bad item is skipped.
                let Ok(data) = serde_json::from_slice(&frame.data) else {
                    log::debug!("A live feed item isn't valid JSON; skipped");
                    continue;
                };
                let event = json_event(
                    "frame",
                    &LiveJsonFrame {
                        seq: frame.seq,
                        capture_time: frame.capture_time,
                        data,
                    },
                );
                return Some((Ok(event), (subscription, stopping)));
            }
        },
    )
}

#[derive(OpenApi)]
#[openapi(
    paths(channels, stream),
    components(schemas(LiveChannels, LiveChannelView, LiveDisplay, LiveJsonFrame, ErrorBody))
)]
pub(crate) struct Api;

pub(crate) fn router(state: LiveState) -> Router {
    Router::new()
        .route(
            "/scenes/by-id/{id}/nodes/by-id/{node_id}/live",
            get(channels),
        )
        .route(
            "/scenes/by-id/{id}/nodes/by-id/{node_id}/live/{channel}/stream",
            get(stream),
        )
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::auth::Auth;
    use crate::modules::scene::api_stream::tests::Reader;
    use crate::modules::scene::service::tests::{Fixture, fixture};
    use axum::extract::connect_info::MockConnectInfo;
    use axum::http::Request;
    use flwrs_plugin::schema::protocol::LiveFeed;
    use serde_json::Value;
    use std::net::SocketAddr;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tower::ServiceExt;

    const WAIT: Duration = Duration::from_secs(5);

    fn feed(channel: &str, media_type: &str, data: &[u8], seq: u64) -> LiveFeed {
        LiveFeed {
            channel: channel.into(),
            media_type: media_type.into(),
            data: data.to_vec(),
            seq,
            capture_time: Some(pbjson_types::Timestamp {
                seconds: 2,
                nanos: 5_000_000,
            }),
        }
    }

    struct Setup {
        app: Router,
        feeds: LiveFeeds,
        stopping: CancellationToken,
        base: String,
        node: String,
        /// Keeps the test's database.
        _fixture: Fixture,
    }

    async fn setup() -> Setup {
        let f = fixture().await;
        let s = f.scene().await;
        let node = f.node(&s.id, "test-sink", Value::Null).await;
        let feeds = LiveFeeds::default();
        let stopping = CancellationToken::new();
        let app = router(LiveState {
            scenes: f.service,
            feeds: feeds.clone(),
            stopping: stopping.clone(),
        });
        feeds.connected(&node.id, 1);
        Setup {
            app,
            feeds,
            stopping,
            base: format!("/scenes/by-id/{}/nodes/by-id/{}/live", s.id, node.id),
            node: node.id,
            _fixture: f,
        }
    }

    async fn get(app: &Router, uri: &str) -> Response {
        app.clone()
            .oneshot(Request::get(uri).body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    async fn json(response: Response) -> Value {
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&body).unwrap()
    }

    #[test]
    fn only_safe_media_types_are_shown() {
        assert_eq!(display_of("image/jpeg"), Some(LiveDisplay::Image));
        assert_eq!(display_of("IMAGE/PNG"), Some(LiveDisplay::Image));
        assert_eq!(display_of("application/json"), Some(LiveDisplay::Json));
        assert_eq!(display_of("application/geo+json"), Some(LiveDisplay::Json));
        for active in ["image/svg+xml", "text/html", "application/xml", "video/mp4"] {
            assert_eq!(display_of(active), None, "{active}");
        }
    }

    #[tokio::test]
    async fn lists_channels_and_refuses_what_it_cant_show() {
        let s = setup().await;
        s.feeds
            .receive(&s.node, 1, feed("stats", "application/json", b"", 0));
        s.feeds
            .receive(&s.node, 1, feed("page", "text/html", b"", 0));
        let list = json(get(&s.app, &s.base).await).await;
        assert_eq!(list["channels"][0]["channel"], "page");
        assert_eq!(list["channels"][0]["display"], Value::Null);
        assert_eq!(list["channels"][1]["display"], "json");
        assert_eq!(list["channels"][1]["viewers"], 0);

        let html = get(&s.app, &format!("{}/page/stream", s.base)).await;
        assert_eq!(html.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
        let unknown = get(&s.app, &format!("{}/nope/stream", s.base)).await;
        assert_eq!(unknown.status(), StatusCode::NOT_FOUND);
        let wrong = get(&s.app, "/scenes/by-id/nope/nodes/by-id/nope/live").await;
        assert_eq!(wrong.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            s.feeds.channels(&s.node)[1].viewers,
            0,
            "nothing subscribed"
        );
    }

    #[tokio::test]
    async fn the_routes_are_behind_the_guard() {
        let s = setup().await;
        let peer: SocketAddr = "203.0.113.7:5000".parse().unwrap();
        let app = Router::new()
            .merge(Auth::with_key("0123456789abcdef-test-key").protect(s.app.clone()))
            .layer(MockConnectInfo(peer));
        let refused = get(&app, &s.base).await;
        assert_eq!(refused.status(), StatusCode::UNAUTHORIZED);
        let allowed = app
            .oneshot(
                Request::get(&s.base)
                    .header("authorization", "Bearer 0123456789abcdef-test-key")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(allowed.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn json_channels_stream_as_events_and_end_when_the_server_stops() {
        let s = setup().await;
        s.feeds
            .receive(&s.node, 1, feed("stats", "application/json", b"", 0));
        let response = get(&s.app, &format!("{}/stats/stream", s.base)).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        assert_eq!(s.feeds.channels(&s.node)[0].viewers, 1);
        let mut reader = Reader::new(response);

        // Not JSON: skipped, and the stream goes on.
        s.feeds
            .receive(&s.node, 1, feed("stats", "application/json", b"{oops", 1));
        tokio::task::yield_now().await;
        s.feeds.receive(
            &s.node,
            1,
            feed("stats", "application/json", b"{\"fps\": 5}", 2),
        );
        let frame = reader.next("frame").await;
        assert_eq!(frame["seq"], 2);
        assert_eq!(frame["capture_time"], 2005);
        assert_eq!(frame["data"]["fps"], 5);

        s.stopping.cancel();
        let body = tokio::time::timeout(WAIT, reader.rest()).await.unwrap();
        assert!(!body.contains("oops"));
    }

    #[tokio::test]
    async fn images_stream_as_multipart_and_a_gone_viewer_is_released() {
        let s = setup().await;
        s.feeds
            .receive(&s.node, 1, feed("preview", "image/jpeg", b"", 0));
        // A real server: dropping the socket must end the response.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(axum::serve(listener, s.app.clone()).into_future());

        let mut socket = tokio::net::TcpStream::connect(addr).await.unwrap();
        let request = format!(
            "GET {}/preview/stream HTTP/1.1\r\nHost: localhost\r\n\r\n",
            s.base
        );
        socket.write_all(request.as_bytes()).await.unwrap();
        let mut seen = Vec::new();
        let mut read_until = async |seen: &mut Vec<u8>, what: &[u8]| {
            tokio::time::timeout(WAIT, async {
                while !seen.windows(what.len()).any(|w| w == what) {
                    let mut chunk = [0u8; 4096];
                    let n = socket.read(&mut chunk).await.unwrap();
                    assert!(n > 0, "closed early");
                    seen.extend_from_slice(&chunk[..n]);
                }
            })
            .await
            .expect("timed out reading the stream");
        };
        read_until(
            &mut seen,
            b"multipart/x-mixed-replace; boundary=flwrs-live-",
        )
        .await;
        read_until(&mut seen, b"\r\n--flwrs-live-").await;
        assert_eq!(s.feeds.channels(&s.node)[0].viewers, 1);

        s.feeds
            .receive(&s.node, 1, feed("preview", "image/jpeg", b"JPEGDATA", 1));
        read_until(&mut seen, b"JPEGDATA\r\n--flwrs-live-").await;
        let text = String::from_utf8_lossy(&seen);
        assert!(text.contains("Content-Type: image/jpeg\r\nContent-Length: 8\r\n\r\nJPEGDATA"));
        assert!(
            text.to_ascii_lowercase()
                .contains("content-security-policy: default-src 'none'; sandbox")
        );

        drop(socket);
        tokio::time::timeout(WAIT, async {
            while s.feeds.channels(&s.node)[0].viewers != 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the viewer was never released");
        server.abort();
    }
}
