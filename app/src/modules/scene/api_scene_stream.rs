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
use super::api_draft::DraftResponse;
use super::api_graph::{Edge, Node};
use super::api_stream::{COALESCE, StreamState, json_event};
use super::api_transfer::SceneCheckResponse;
use crate::modules::api_error::{ApiError, ErrorBody};
use crate::modules::director::handle::SceneStatus;
use crate::modules::scene::service::ServiceError;
use axum::Router;
use axum::extract::{Path, State};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::routing::get;
use futures_util::Stream;
use serde::Serialize;
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
        let (draft, draft_check) = match service.get_draft(id).await {
            Ok(draft) => (
                Some(DraftResponse::from(draft)),
                Some(service.check_draft(id).await?.into()),
            ),
            Err(ServiceError::NotFound("draft")) => (None, None),
            Err(e) => return Err(e),
        };
        Ok(SceneViewEvent {
            scene: scene.into(),
            nodes: nodes.into_iter().map(Node::from).collect(),
            edges: edges.into_iter().map(Edge::from).collect(),
            check: check.into(),
            draft,
            draft_check,
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

#[derive(OpenApi)]
#[openapi(
    paths(stream),
    components(schemas(SceneViewEvent, SceneStatusEvent, ErrorBody))
)]
pub(crate) struct Api;

/// `/scenes/by-id/{id}/stream`, with the list stream's state.
pub(crate) fn router(state: StreamState) -> Router {
    Router::new()
        .route("/scenes/by-id/{id}/stream", get(stream))
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
    async fn sends_the_scene_its_draft_status_and_deleted() {
        let f = fixture().await;
        let s = f.scene().await;
        let node = f.node(&s.id, "test-sink", Value::Null).await;
        let stopping = CancellationToken::new();
        let state = StreamState {
            scenes: f.service,
            stopping: stopping.clone(),
            interval: Duration::from_millis(100),
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
