//! `GET /api/scenes/by-id/{id}/stream`: one scene, live (Server-Sent Events),
//! for the scene view.
//!
//! - `scene`: the scene, its live graph and check, and its draft (with the
//!   draft's check) if it has one. Sent first, then after each change that
//!   alters it (another tab, an API client; bursts coalesced, repeats left
//!   out).
//! - `status`: the director's `SceneStatus` (state, per-node reports), every
//!   `[scenes] stream_interval_ms`.
//! - `deleted`: the scene is gone; the stream ends.
//! - `stream_error`: the scene couldn't be read (`{error, details}`); the
//!   stream goes on.
//!
//! The stream ends when the HTTP server starts shutting down.

use super::api::Scene;
use super::api::Svc;
use super::api_draft::DraftResponse;
use super::api_graph::{Edge, Node};
use super::api_stream::{COALESCE, StreamState, json_event};
use super::api_transfer::SceneCheckResponse;
use crate::modules::api_error::{ApiError, ErrorBody};
use crate::modules::director::handle::SceneStatus;
use crate::modules::scene::service::draft::Change;
use crate::modules::scene::service::{Node as ServiceNode, NodeQueueView, ServiceError};
use axum::Router;
use axum::extract::{Path, State};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::routing::get;
use futures_util::Stream;
use serde::Serialize;
use std::collections::BTreeMap;
use std::convert::Infallible;
use tokio::sync::watch;
use tokio::time::{Interval, MissedTickBehavior};
use utoipa::{OpenApi, ToSchema};

/// The `scene` event: what the scene view shows.
#[derive(Serialize, ToSchema)]
pub(crate) struct SceneViewEvent {
    pub scene: Scene,
    /// The live graph.
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    /// The live graph's check.
    pub check: SceneCheckResponse,
    /// The scene's draft; `null` = none.
    pub draft: Option<DraftResponse>,
    /// The draft's check; `null` without a draft.
    pub draft_check: Option<SceneCheckResponse>,
    /// Each receiving node's inbound queue (live graph), as the director
    /// sets it up: kind and when-full policy. Nodes of missing plugins are
    /// left out.
    pub queues: BTreeMap<String, NodeQueueView>,
    /// The same for the draft's graph; `null` without a draft.
    pub draft_queues: Option<BTreeMap<String, NodeQueueView>>,
}

/// The `status` event.
#[derive(Serialize, ToSchema)]
pub(crate) struct SceneStatusEvent {
    /// When it was taken (Unix milliseconds), for rates.
    pub time: i64,
    #[serde(flatten)]
    pub status: SceneStatus,
}

#[utoipa::path(
    get,
    path = "/by-id/{id}/stream",
    operation_id = "scene-stream",
    summary = "One scene, live (Server-Sent Events)",
    description = "Events: `scene` (`SceneViewEvent`: the scene, its live graph and check, its draft and the draft's check; first, and after each change to it), `status` (`SceneStatusEvent`: the director's state and per-node reports, every `[scenes] stream_interval_ms`), `deleted` (the scene is gone; the stream ends), `stream_error` (`ErrorBody`). Keep-alive comments every 15 s.",
    params(("id" = String, Path, description = "Scene ID")),
    responses(
        (status = 200, description = "An event stream", content_type = "text/event-stream", body = String),
        (status = 404, description = "No such scene", body = ErrorBody),
    )
)]
async fn stream(
    State(state): State<StreamState>,
    Path(id): Path<String>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, ApiError> {
    state.scenes.get_scene(&id).await?;
    Ok(Sse::new(feed(state, id)).keep_alive(KeepAlive::default()))
}

struct Feed {
    state: StreamState,
    id: String,
    scene_changes: watch::Receiver<u64>,
    plugin_changes: watch::Receiver<u64>,
    ticker: Interval,
    /// The last `scene` event's data (a change that doesn't alter it isn't sent).
    last: Option<String>,
    first: bool,
    done: bool,
}

fn feed(state: StreamState, id: String) -> impl Stream<Item = Result<Event, Infallible>> {
    let mut ticker = tokio::time::interval(state.interval);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let feed = Feed {
        scene_changes: state.scenes.changes(),
        plugin_changes: state.scenes.plugin_changes(),
        state,
        id,
        ticker,
        last: None,
        first: true,
        done: false,
    };
    futures_util::stream::unfold(feed, |mut feed| async move {
        let event = feed.next().await?;
        Some((Ok(event), feed))
    })
}

/// What reading the scene gave.
enum View {
    Event(Event),
    Unchanged,
    Deleted,
}

impl Feed {
    async fn next(&mut self) -> Option<Event> {
        if self.done {
            return None;
        }
        if self.first {
            self.first = false;
            self.scene_changes.mark_unchanged();
            self.plugin_changes.mark_unchanged();
            if let Some(event) = self.view_event().await {
                return Some(event);
            }
        }
        loop {
            let stopping = self.state.stopping.clone();
            tokio::select! {
                biased;
                () = stopping.cancelled() => return None,
                changed = self.scene_changes.changed() => {
                    changed.ok()?;
                    if let Some(event) = self.after_change().await? {
                        return Some(event);
                    }
                }
                changed = self.plugin_changes.changed() => {
                    changed.ok()?;
                    if let Some(event) = self.after_change().await? {
                        return Some(event);
                    }
                }
                _ = self.ticker.tick() => {
                    if let Some(event) = self.status().await {
                        return Some(event);
                    }
                }
            }
        }
    }

    /// Waits for the rest of a burst, then reads the scene once. `None` =
    /// the server is stopping; `Some(None)` = nothing to send.
    async fn after_change(&mut self) -> Option<Option<Event>> {
        let stopping = self.state.stopping.clone();
        tokio::select! {
            () = stopping.cancelled() => return None,
            () = tokio::time::sleep(COALESCE) => {}
        }
        self.scene_changes.mark_unchanged();
        self.plugin_changes.mark_unchanged();
        Some(self.view_event().await)
    }

    /// The `scene` event if it changed, or `deleted` (and the end).
    async fn view_event(&mut self) -> Option<Event> {
        match self.view().await {
            View::Event(event) => Some(event),
            View::Unchanged => None,
            View::Deleted => {
                self.done = true;
                Some(Event::default().event("deleted").data("{}"))
            }
        }
    }

    async fn view(&mut self) -> View {
        match self.read().await {
            Ok(view) => {
                let data = serde_json::to_string(&view).unwrap_or_else(|_| "{}".into());
                if self.last.as_deref() == Some(&data) {
                    return View::Unchanged;
                }
                self.last = Some(data.clone());
                View::Event(Event::default().event("scene").data(data))
            }
            Err(ServiceError::NotFound("scene")) => View::Deleted,
            Err(e) => {
                log::warn!("Scene stream [{}]: couldn't read the scene: {e}", self.id);
                self.last = None;
                View::Event(json_event(
                    "stream_error",
                    &ErrorBody {
                        error: "couldn't read the scene".into(),
                        details: vec![],
                    },
                ))
            }
        }
    }

    async fn read(&self) -> Result<SceneViewEvent, ServiceError> {
        let service = self.state.scenes;
        let id = &self.id;
        let scene = service.get_scene(id).await?;
        let nodes = service.list_nodes(id).await?;
        let edges = service.list_edges(id).await?;
        let check = service.check(id).await?;
        let spill = self.state.spill_default;
        let pairs = |edges: Vec<(&str, &str)>| -> Vec<(String, String)> {
            edges
                .into_iter()
                .map(|(a, b)| (a.to_string(), b.to_string()))
                .collect()
        };
        let queues = service
            .node_queues(
                &nodes,
                &pairs(
                    edges
                        .iter()
                        .map(|e| (e.source_node_id.as_str(), e.target_node_id.as_str()))
                        .collect(),
                ),
                scene.defaults.durable,
                spill,
            )
            .await;
        let (draft, draft_check, draft_queues) = match service.get_draft(id).await {
            Ok(draft) => {
                let present: Vec<ServiceNode> = draft
                    .nodes
                    .iter()
                    .filter(|n| n.change != Change::Removed)
                    .map(|n| n.node.clone())
                    .collect();
                let edges = pairs(
                    draft
                        .edges
                        .iter()
                        .filter(|e| e.change != Change::Removed)
                        .map(|e| {
                            (
                                e.edge.source_node_id.as_str(),
                                e.edge.target_node_id.as_str(),
                            )
                        })
                        .collect(),
                );
                let queues = service
                    .node_queues(&present, &edges, draft.row.defaults.durable, spill)
                    .await;
                (
                    Some(DraftResponse::from(draft)),
                    Some(service.check_draft(id).await?.into()),
                    Some(queues),
                )
            }
            Err(ServiceError::NotFound("draft")) => (None, None, None),
            Err(e) => return Err(e),
        };
        Ok(SceneViewEvent {
            scene: scene.into(),
            nodes: nodes.into_iter().map(Node::from).collect(),
            edges: edges.into_iter().map(Edge::from).collect(),
            check: check.into(),
            draft,
            draft_check,
            queues,
            draft_queues,
        })
    }

    /// `None` if the director doesn't answer within an interval (skipped).
    async fn status(&self) -> Option<Event> {
        let status = tokio::time::timeout(
            self.state.interval,
            self.state.scenes.runtime_status(&self.id),
        )
        .await
        .ok()?
        .ok()?
        .1;
        Some(json_event(
            "status",
            &SceneStatusEvent {
                time: chrono::Utc::now().timestamp_millis(),
                status,
            },
        ))
    }
}

// ---- a node's logs ----

/// The node's records kept so far (the `logs` event, and the snapshot).
#[derive(Serialize, ToSchema)]
pub(crate) struct NodeLogs {
    /// Oldest first; at most `node_logs::KEEP`.
    pub records: Vec<crate::modules::node_logs::Record>,
}

/// A node of the scene, live or in its draft.
pub(super) async fn scene_node(scenes: Svc, id: &str, node_id: &str) -> Result<(), ApiError> {
    use crate::modules::scene::service::Store;
    scenes.get_scene(id).await?;
    if scenes.get_node_in(Store::Live, id, node_id).await.is_ok()
        || scenes.get_node_in(Store::Draft, id, node_id).await.is_ok()
    {
        Ok(())
    } else {
        Err(ServiceError::NotFound("node").into())
    }
}

#[utoipa::path(
    get,
    path = "/by-id/{id}/nodes/by-id/{node_id}/logs",
    operation_id = "node-logs",
    summary = "A node's recent logs",
    description = "The last records kept in memory for the node (the server log has them all): the plugin's log and error events, its stdout and stderr lines, and the server's notes about it (started, exited, relaunching, failed). Restarting the server empties them.",
    params(("id" = String, Path, description = "Scene ID"), ("node_id" = String, Path, description = "Node ID")),
    responses(
        (status = 200, description = "The records", body = NodeLogs),
        (status = 404, description = "No such scene or node", body = ErrorBody),
    )
)]
async fn logs(
    State(state): State<StreamState>,
    Path((id, node_id)): Path<(String, String)>,
) -> Result<axum::Json<NodeLogs>, ApiError> {
    scene_node(state.scenes, &id, &node_id).await?;
    Ok(axum::Json(NodeLogs {
        records: crate::modules::node_logs::snapshot(&node_id),
    }))
}

#[utoipa::path(
    get,
    path = "/by-id/{id}/nodes/by-id/{node_id}/logs/stream",
    operation_id = "node-logs-stream",
    summary = "A node's logs, live (Server-Sent Events)",
    description = "Events: `logs` (`NodeLogs`: the records kept so far, first), then `log` (a `Record`) for each new one; `lagged` (`{missed}`) when the reader fell behind and records were skipped. Ends when the server stops.",
    params(("id" = String, Path, description = "Scene ID"), ("node_id" = String, Path, description = "Node ID")),
    responses(
        (status = 200, description = "An event stream", content_type = "text/event-stream", body = String),
        (status = 404, description = "No such scene or node", body = ErrorBody),
    )
)]
async fn logs_stream(
    State(state): State<StreamState>,
    Path((id, node_id)): Path<(String, String)>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, ApiError> {
    use tokio::sync::broadcast::error::RecvError;
    scene_node(state.scenes, &id, &node_id).await?;
    let (records, rx) = crate::modules::node_logs::subscribe(&node_id);
    let first = json_event("logs", &NodeLogs { records });
    let stopping = state.stopping.clone();
    let rest = futures_util::stream::unfold((rx, stopping), |(mut rx, stopping)| async move {
        let event = tokio::select! {
            () = stopping.cancelled() => return None,
            next = rx.recv() => match next {
                Ok(record) => json_event("log", &record),
                Err(RecvError::Lagged(missed)) => json_event("lagged", &serde_json::json!({ "missed": missed })),
                Err(RecvError::Closed) => return None,
            },
        };
        Some((Ok(event), (rx, stopping)))
    });
    let stream =
        futures_util::StreamExt::chain(futures_util::stream::once(async move { Ok(first) }), rest);
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

#[derive(OpenApi)]
#[openapi(
    paths(stream, logs, logs_stream),
    components(schemas(
        SceneViewEvent,
        SceneStatusEvent,
        NodeQueueView,
        NodeLogs,
        crate::modules::node_logs::Record,
        crate::modules::node_logs::Source,
        ErrorBody
    ))
)]
pub(crate) struct Api;

/// `/scenes/by-id/{id}/stream`, with the list stream's state.
pub(crate) fn router(state: StreamState) -> Router {
    Router::new()
        .route("/scenes/by-id/{id}/stream", get(stream))
        .route("/scenes/by-id/{id}/nodes/by-id/{node_id}/logs", get(logs))
        .route(
            "/scenes/by-id/{id}/nodes/by-id/{node_id}/logs/stream",
            get(logs_stream),
        )
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::director::handle::{DirectorCommand, SceneState};
    use crate::modules::scene::api_stream::tests::Reader;
    use crate::modules::scene::service::tests::{fixture, node_input};
    use crate::modules::scene::service::{Position, SceneInput, Store};
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use serde_json::Value;
    use std::collections::BTreeMap;
    use std::time::Duration;
    use tokio_util::sync::CancellationToken;
    use tower::ServiceExt;

    #[tokio::test]
    async fn streams_a_nodes_logs() {
        use crate::modules::node_logs::{self, Source};
        let f = fixture().await;
        let s = f.scene().await;
        let node = f.node(&s.id, "test-sink", Value::Null).await;
        node_logs::push(
            &node.id,
            log::Level::Info,
            Source::Stdout,
            "before",
            Default::default(),
        );
        let stopping = CancellationToken::new();
        let app = router(StreamState {
            scenes: f.service,
            stopping: stopping.clone(),
            interval: Duration::from_millis(100),
            spill_default: false,
        });
        let missing = app
            .clone()
            .oneshot(
                Request::get(format!("/scenes/by-id/{}/nodes/by-id/nope/logs", s.id))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);

        let response = app
            .oneshot(
                Request::get(format!(
                    "/scenes/by-id/{}/nodes/by-id/{}/logs/stream",
                    s.id, node.id
                ))
                .body(Body::empty())
                .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let mut reader = Reader::new(response);
        let first = reader.next("logs").await;
        assert_eq!(first["records"][0]["message"], "before");
        assert_eq!(first["records"][0]["source"], "stdout");
        node_logs::push(
            &node.id,
            log::Level::Error,
            Source::Plugin,
            "after",
            Default::default(),
        );
        let next = reader.next("log").await;
        assert_eq!(next["message"], "after");
        assert_eq!(next["level"], "error");
        stopping.cancel();
        assert!(reader.ended().await);
        node_logs::forget(&node.id);
    }

    #[tokio::test]
    async fn sends_the_scene_its_draft_status_and_deleted() {
        let f = fixture().await;
        let s = f.scene().await;
        let node = f.node(&s.id, "test-sink", Value::Null).await;
        let stopping = CancellationToken::new();
        let state = StreamState {
            scenes: f.service,
            stopping: stopping.clone(),
            interval: Duration::from_millis(100),
            spill_default: false,
        };

        let missing = router(state.clone())
            .oneshot(
                Request::get("/scenes/by-id/nope/stream")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);

        let response = router(state)
            .oneshot(
                Request::get(format!("/scenes/by-id/{}/stream", s.id))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let mut reader = Reader::new(response);

        let first = reader.next("scene").await;
        assert_eq!(first["scene"]["id"], s.id.as_str());
        assert_eq!(first["nodes"][0]["id"], node.id.as_str());
        assert_eq!(first["nodes"][0]["position"], Value::Null);
        assert!(first["draft"].is_null());
        assert_eq!(
            first["queues"][&node.id],
            serde_json::json!({"kind": "memory", "when_full": "block"})
        );
        assert!(first["draft_check"].is_null());

        // A draft edit: the draft comes along, with its check.
        let mut added = node_input("test-sink", Value::Null);
        added.position = Some(Position { x: 10.0, y: 20.0 });
        let draft_node = f
            .service
            .save_node(Store::Draft, &s.id, None, added)
            .await
            .unwrap();
        let second = reader.next("scene").await;
        let nodes = second["draft"]["nodes"].as_array().unwrap();
        let new = nodes
            .iter()
            .find(|n| n["id"] == draft_node.id.as_str())
            .unwrap();
        assert_eq!(new["change"], "added");
        assert_eq!(new["position"]["x"], 10.0);
        assert!(second["draft_check"]["launchable"].is_boolean());

        // Moving a node is a change the view sees (layout), not a draft change.
        f.service
            .set_layout(
                &s.id,
                &BTreeMap::from([(node.id.clone(), Some(Position { x: 1.0, y: 2.0 }))]),
                &BTreeMap::new(),
            )
            .await
            .unwrap();
        let third = reader.next("scene").await;
        assert_eq!(third["nodes"][0]["position"]["y"], 2.0);
        let moved = third["draft"]["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|n| n["id"] == node.id.as_str())
            .unwrap();
        assert_eq!(moved["position"]["y"], 2.0);
        assert_eq!(moved["change"], "unchanged");

        // A change elsewhere doesn't repeat the event; status still comes.
        f.service
            .create_scene(&SceneInput::named("other"))
            .await
            .unwrap();
        let mut commands = f.director.lock().await;
        let answer = async {
            loop {
                if let Some(DirectorCommand::Status { reply, .. }) = commands.recv().await {
                    let _ = reply.send(None);
                }
            }
        };
        let status = tokio::select! {
            status = reader.next_any() => status,
            () = answer => unreachable!(),
        };
        assert_eq!(status.0, "status", "{status:?}");
        assert_eq!(status.1["state"], serde_json::json!(SceneState::Stopped));
        drop(commands);

        f.service.delete_scene(&s.id).await.unwrap();
        reader.next("deleted").await;
        assert!(reader.ended().await, "the stream ends with the scene");
    }
}
