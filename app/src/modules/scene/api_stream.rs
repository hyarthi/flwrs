//! `GET /api/scenes/stream`: the scenes list, live (Server-Sent Events).
//!
//! - `scenes`: every scene with its graph's size and its check, sent first
//!   and again after each change to scenes or plugins (bursts coalesced).
//! - `status`: the director's summary of the scenes it runs (state, nodes,
//!   event counters), every `[scenes] stream_interval_ms`.
//! - `stream_error`: the list couldn't be read (`{error, details}`); the
//!   stream goes on.
//!
//! The stream ends when the HTTP server starts shutting down.
//!
//! `GET /api/status/stream` (the overview page's) is the same feed, with
//! `overview` events (the director's whole `Overview`: hub connections,
//! memory and disk use, every scene's summary) in place of `status`.

use crate::modules::api_error::ErrorBody;
use crate::modules::director::handle::{Overview, SceneState, SceneSummary, Usage};
use crate::modules::hub::HubConnections;
use crate::modules::scene::api::Svc;
use crate::modules::scene::service::SceneRow;
use axum::Router;
use axum::extract::State;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::routing::get;
use futures_util::Stream;
use serde::Serialize;
use std::convert::Infallible;
use std::time::Duration;
use tokio::sync::watch;
use tokio::time::{Interval, MissedTickBehavior};
use tokio_util::sync::CancellationToken;
use utoipa::{OpenApi, ToSchema};

/// Changes within this long after the first one go out as one `scenes` event.
pub(super) const COALESCE: Duration = Duration::from_millis(150);

#[derive(Clone)]
pub(crate) struct StreamState {
    pub scenes: Svc,
    /// Ends the streams (the HTTP server is stopping).
    pub stopping: CancellationToken,
    pub interval: Duration,
    /// `[director] spill_default` (the scene view's queue kinds).
    pub spill_default: bool,
}

/// A scene in the live list.
#[derive(Serialize, ToSchema, Debug)]
pub(crate) struct SceneListRow {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub max_hops: Option<u32>,
    pub create_time: i64,
    pub update_time: i64,
    pub nodes: usize,
    pub edges: usize,
    /// Why it can't be enabled (`…/check`); empty = it can run.
    pub problems: Vec<String>,
    pub warnings: Vec<String>,
}

impl From<SceneRow> for SceneListRow {
    fn from(row: SceneRow) -> Self {
        Self {
            id: row.scene.id,
            name: row.scene.name,
            enabled: row.scene.enabled,
            max_hops: row.scene.max_hops,
            create_time: row.scene.create_time.timestamp_millis(),
            update_time: row.scene.update_time.timestamp_millis(),
            nodes: row.nodes,
            edges: row.edges,
            problems: row.check.problems,
            warnings: row.check.warnings,
        }
    }
}

/// The `scenes` event.
#[derive(Serialize, ToSchema)]
pub(crate) struct ScenesEvent {
    pub scenes: Vec<SceneListRow>,
}

/// The `status` event.
#[derive(Serialize, ToSchema)]
pub(crate) struct StatusEvent {
    /// When it was taken (Unix milliseconds), for rates.
    pub time: i64,
    /// The scenes the director has handled since it started; any other is
    /// stopped.
    pub scenes: Vec<SceneSummary>,
}

/// The `overview` event (`GET /api/status/stream`).
#[derive(Serialize, ToSchema)]
pub(crate) struct OverviewEvent {
    /// When it was taken (Unix milliseconds), for rates.
    pub time: i64,
    #[serde(flatten)]
    pub overview: Overview,
}

/// Which periodic event a feed sends.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Periodic {
    /// `status`: the scenes' summaries (the scenes list).
    Status,
    /// `overview`: everything (the overview page).
    Overview,
}

#[utoipa::path(
    get,
    path = "/stream",
    operation_id = "status-stream",
    summary = "The server at a glance, live (Server-Sent Events)",
    description = "Events: `scenes` (`ScenesEvent`, as in `GET /api/scenes/stream`: every scene with its check; first, and after each change to scenes or plugins), `overview` (`OverviewEvent`: `GET /api/status` with the time it was taken, every `[scenes] stream_interval_ms`), `stream_error`. Keep-alive comments every 15 s. No history: a client keeps its own samples.",
    responses(
        (status = 200, description = "An event stream", content_type = "text/event-stream", body = String),
    )
)]
async fn status_stream(
    State(state): State<StreamState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    Sse::new(feed(state, Periodic::Overview)).keep_alive(KeepAlive::default())
}

#[utoipa::path(
    get,
    path = "/stream",
    operation_id = "scenes-stream",
    summary = "The scenes list, live (Server-Sent Events)",
    description = "Events: `scenes` (`ScenesEvent`: every scene with its node and edge counts and its check; first, and after each change to scenes or plugins), `status` (`StatusEvent`: the director's per-scene state and event counters, every `[scenes] stream_interval_ms`), `stream_error` (`ErrorBody`: the list couldn't be read). Keep-alive comments every 15 s.",
    responses(
        (status = 200, description = "An event stream", content_type = "text/event-stream", body = String),
    )
)]
async fn stream(
    State(state): State<StreamState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    Sse::new(feed(state, Periodic::Status)).keep_alive(KeepAlive::default())
}

struct Feed {
    periodic: Periodic,
    state: StreamState,
    scene_changes: watch::Receiver<u64>,
    plugin_changes: watch::Receiver<u64>,
    ticker: Interval,
    first: bool,
}

fn feed(state: StreamState, periodic: Periodic) -> impl Stream<Item = Result<Event, Infallible>> {
    let mut ticker = tokio::time::interval(state.interval);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let feed = Feed {
        periodic,
        scene_changes: state.scenes.changes(),
        plugin_changes: state.scenes.plugin_changes(),
        state,
        ticker,
        first: true,
    };
    futures_util::stream::unfold(feed, |mut feed| async move {
        let event = feed.next().await?;
        Some((Ok(event), feed))
    })
}

impl Feed {
    async fn next(&mut self) -> Option<Event> {
        if self.first {
            self.first = false;
            self.scene_changes.mark_unchanged();
            self.plugin_changes.mark_unchanged();
            return Some(self.scenes().await);
        }
        loop {
            let stopping = self.state.stopping.clone();
            tokio::select! {
                biased;
                () = stopping.cancelled() => return None,
                changed = self.scene_changes.changed() => {
                    if changed.is_err() {
                        return None;
                    }
                    return self.after_change().await;
                }
                changed = self.plugin_changes.changed() => {
                    if changed.is_err() {
                        return None;
                    }
                    return self.after_change().await;
                }
                _ = self.ticker.tick() => {
                    if let Some(event) = self.status().await {
                        return Some(event);
                    }
                }
            }
        }
    }

    /// Waits for the rest of a burst, then sends the list once.
    async fn after_change(&mut self) -> Option<Event> {
        let stopping = self.state.stopping.clone();
        tokio::select! {
            () = stopping.cancelled() => return None,
            () = tokio::time::sleep(COALESCE) => {}
        }
        self.scene_changes.mark_unchanged();
        self.plugin_changes.mark_unchanged();
        Some(self.scenes().await)
    }

    async fn scenes(&self) -> Event {
        match self.state.scenes.overview_rows().await {
            Ok(rows) => json_event(
                "scenes",
                &ScenesEvent {
                    scenes: rows.into_iter().map(SceneListRow::from).collect(),
                },
            ),
            Err(e) => {
                log::warn!("Scenes stream: couldn't read the scenes: {e}");
                json_event(
                    "stream_error",
                    &ErrorBody {
                        error: "couldn't read the scenes".into(),
                        details: vec![],
                    },
                )
            }
        }
    }

    /// `None` if the director doesn't answer within an interval (skipped).
    async fn status(&self) -> Option<Event> {
        let overview =
            tokio::time::timeout(self.state.interval, self.state.scenes.director().overview())
                .await
                .ok()?
                .ok()?;
        let time = chrono::Utc::now().timestamp_millis();
        Some(match self.periodic {
            Periodic::Status => json_event(
                "status",
                &StatusEvent {
                    time,
                    scenes: overview.scenes,
                },
            ),
            Periodic::Overview => json_event("overview", &OverviewEvent { time, overview }),
        })
    }
}

pub(super) fn json_event(name: &str, data: &impl Serialize) -> Event {
    let data = serde_json::to_string(data).unwrap_or_else(|_| "{}".into());
    Event::default().event(name).data(data)
}

#[derive(OpenApi)]
#[openapi(
    paths(status_stream),
    components(schemas(
        OverviewEvent,
        Overview,
        Usage,
        HubConnections,
        ScenesEvent,
        SceneListRow,
        ErrorBody
    ))
)]
pub(crate) struct StatusApi;

#[derive(OpenApi)]
#[openapi(
    paths(stream),
    components(schemas(
        ScenesEvent,
        SceneListRow,
        StatusEvent,
        SceneSummary,
        SceneState,
        ErrorBody
    ))
)]
pub(crate) struct Api;

/// `/scenes/stream`, with its own state (merged into the API next to the
/// scenes router).
pub(crate) fn router(state: StreamState) -> Router {
    Router::new()
        .route("/scenes/stream", get(stream))
        .route("/status/stream", get(status_stream))
        .with_state(state)
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::modules::director::handle::{DirectorCommand, Overview, Usage};
    use crate::modules::scene::service::SceneInput;
    use crate::modules::scene::service::tests::fixture;
    use axum::body::{Body, BodyDataStream};
    use axum::http::{Request, StatusCode, header};
    use flwrs_core::db::Database;
    use futures_util::StreamExt;
    use serde_json::{Value, json};
    use tower::ServiceExt;

    /// Reads an event stream's named events.
    pub(crate) struct Reader {
        body: BodyDataStream,
        buffer: String,
    }

    impl Reader {
        pub(crate) fn new(response: axum::response::Response) -> Self {
            Self {
                body: response.into_body().into_data_stream(),
                buffer: String::new(),
            }
        }

        /// Reads to the end of the stream; what was left as text.
        pub(crate) async fn rest(&mut self) -> String {
            use futures_util::StreamExt;
            while let Some(Ok(chunk)) = self.body.next().await {
                self.buffer.push_str(&String::from_utf8_lossy(&chunk));
            }
            std::mem::take(&mut self.buffer)
        }

        /// The next event (keep-alive comments skipped): its name and data.
        pub(crate) async fn next_any(&mut self) -> (String, Value) {
            let wait = async {
                loop {
                    while let Some(end) = self.buffer.find("\n\n") {
                        let block: String = self.buffer.drain(..end + 2).collect();
                        let mut event = None;
                        let mut data = String::new();
                        for line in block.lines() {
                            if let Some(v) = line.strip_prefix("event:") {
                                event = Some(v.trim().to_string());
                            } else if let Some(v) = line.strip_prefix("data:") {
                                data.push_str(v.trim_start());
                            }
                        }
                        if let Some(event) = event {
                            return (event, serde_json::from_str(&data).unwrap());
                        }
                    }
                    let chunk = self.body.next().await.expect("stream ended").unwrap();
                    self.buffer.push_str(std::str::from_utf8(&chunk).unwrap());
                }
            };
            tokio::time::timeout(Duration::from_secs(5), wait)
                .await
                .expect("no event")
        }

        /// Reads until the next event named `name` (others skipped).
        pub(crate) async fn next(&mut self, name: &str) -> Value {
            let wait = async {
                loop {
                    let (event, data) = self.next_any().await;
                    if event == name {
                        return data;
                    }
                }
            };
            tokio::time::timeout(Duration::from_secs(5), wait)
                .await
                .unwrap_or_else(|_| panic!("no {name} event"))
        }

        pub(crate) async fn ended(&mut self) -> bool {
            tokio::time::timeout(Duration::from_secs(5), async {
                while let Some(chunk) = self.body.next().await {
                    chunk.unwrap();
                }
            })
            .await
            .is_ok()
        }
    }

    #[tokio::test]
    async fn sends_the_list_then_changes_and_status_and_ends_on_stopping() {
        let f = fixture().await;
        let s = f.scene().await;
        f.node(&s.id, "test-sink", Value::Null).await;
        // A node whose plugin isn't here (as after an import).
        let other = f.node(&s.id, "test-sink", Value::Null).await;
        let Database::SQLite(db) = f.plugins.service.db();
        sqlx::query("UPDATE scene_nodes SET plugin_id = 'missing-plugin' WHERE id = $1")
            .bind(&other.id)
            .execute(&mut *db.get_connection().await.unwrap())
            .await
            .unwrap();

        let stopping = CancellationToken::new();
        let app = router(StreamState {
            scenes: f.service,
            stopping: stopping.clone(),
            interval: Duration::from_millis(100),
            spill_default: false,
        });
        let response = app
            .oneshot(Request::get("/scenes/stream").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "text/event-stream"
        );
        let mut reader = Reader::new(response);

        // First: the list, with sizes and the check.
        let first = reader.next("scenes").await;
        let row = &first["scenes"][0];
        assert_eq!(row["id"], s.id.as_str());
        assert_eq!(row["nodes"], 2);
        assert_eq!(row["edges"], 0);
        assert_eq!(row["enabled"], false);
        assert!(
            row["problems"][0]
                .as_str()
                .unwrap()
                .contains("missing-plugin"),
            "{row}"
        );

        // A change: the list again (newest first).
        f.service
            .create_scene(&SceneInput::named("second"))
            .await
            .unwrap();
        let second = reader.next("scenes").await;
        assert_eq!(second["scenes"].as_array().unwrap().len(), 2);
        assert_eq!(second["scenes"][0]["name"], "second");

        // A plugin change counts too (checks depend on the plugins).
        f.plugins
            .service
            .install(&f.plugins.upload("another-sink", "1.0.0"))
            .await
            .unwrap();
        assert_eq!(
            reader.next("scenes").await["scenes"]
                .as_array()
                .unwrap()
                .len(),
            2
        );

        // Status, from the director.
        let mut commands = f.director.lock().await;
        let answer = async {
            loop {
                if let Some(DirectorCommand::Overview { reply }) = commands.recv().await {
                    let usage = Usage {
                        used_bytes: 0,
                        limit_bytes: 1,
                    };
                    let _ = reply.send(Overview {
                        hub_address: "127.0.0.1:1".into(),
                        hub_connections: Default::default(),
                        queued_memory: usage.clone(),
                        spill_memory: usage.clone(),
                        spill_disk: usage,
                        scenes: vec![SceneSummary {
                            scene_id: s.id.clone(),
                            state: SceneState::Running,
                            nodes: 2,
                            running_nodes: 2,
                            errors: 0,
                            emitted: 10,
                            received: 9,
                            dropped: 1,
                        }],
                    });
                }
            }
        };
        let status = tokio::select! {
            status = reader.next("status") => status,
            () = answer => unreachable!(),
        };
        assert!(status["time"].as_i64().unwrap() > 0);
        assert_eq!(
            status["scenes"][0],
            json!({"scene_id": s.id, "state": "RUNNING", "nodes": 2, "running_nodes": 2, "errors": 0, "emitted": 10, "received": 9, "dropped": 1})
        );

        stopping.cancel();
        assert!(
            reader.ended().await,
            "the stream ends when the server stops"
        );
    }

    #[tokio::test]
    async fn the_status_stream_sends_scenes_and_the_whole_overview() {
        let f = fixture().await;
        let s = f.scene().await;
        let app = router(StreamState {
            scenes: f.service,
            stopping: CancellationToken::new(),
            interval: Duration::from_millis(100),
            spill_default: false,
        });
        let response = app
            .oneshot(Request::get("/status/stream").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let mut reader = Reader::new(response);
        assert_eq!(
            reader.next("scenes").await["scenes"][0]["id"],
            s.id.as_str()
        );
        let mut commands = f.director.lock().await;
        let answer = async {
            loop {
                if let Some(DirectorCommand::Overview { reply }) = commands.recv().await {
                    let _ = reply.send(Overview {
                        hub_address: "127.0.0.1:7400".into(),
                        hub_connections: Default::default(),
                        queued_memory: Usage {
                            used_bytes: 5,
                            limit_bytes: 10,
                        },
                        spill_memory: Usage {
                            used_bytes: 0,
                            limit_bytes: 1,
                        },
                        spill_disk: Usage {
                            used_bytes: 0,
                            limit_bytes: 1,
                        },
                        scenes: vec![],
                    });
                }
            }
        };
        let overview = tokio::select! {
            o = reader.next("overview") => o,
            () = answer => unreachable!(),
        };
        assert!(overview["time"].as_i64().unwrap() > 0);
        assert_eq!(overview["hub_address"], "127.0.0.1:7400");
        assert_eq!(
            overview["queued_memory"],
            json!({"used_bytes": 5, "limit_bytes": 10})
        );
        assert_eq!(overview["hub_connections"]["connected"], 0);
    }
}
