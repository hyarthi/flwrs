//! REST for a scene's run-time status (from the director).

use crate::modules::api_error::{ApiError, ErrorBody};
use crate::modules::director::handle::SceneStatus;
use crate::modules::scene::api::Svc;
use axum::extract::{Path, State};
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;
use utoipa::ToSchema;

#[derive(Serialize, ToSchema)]
pub(crate) struct SceneStatusResponse {
    pub scene_id: String,
    /// Whether the scene should run (its setting). `state` is what it does.
    pub enabled: bool,
    #[serde(flatten)]
    pub status: SceneStatus,
}

#[utoipa::path(
    get,
    path = "/by-id/{id}/status",
    operation_id = "scene-status",
    summary = "What the scene is doing: its state, and each node's state, queue and counters",
    description = "`state` is RUNNING, STOPPED (disabled, or not started since the server started) or FAILED (with `errors`). Per node: the plugin's state (LAUNCHING, WAITING, RUNNING, DISCONNECTED, UNHEALTHY, STOPPING, STOPPED, FAILED), launches and the last error; `counters` (emitted, received, dropped, mapping errors, restarts); and the inbound queue (depth, drops by reason, time blocked, spill).",
    params(("id" = String, Path, description = "Scene ID")),
    responses(
        (status = 200, description = "Status", body = SceneStatusResponse),
        (status = 404, description = "Not found", body = ErrorBody),
        (status = 503, description = "The director isn't answering", body = ErrorBody),
    )
)]
pub(super) async fn scene_status(
    State(service): State<Svc>,
    Path(id): Path<String>,
) -> Result<Json<SceneStatusResponse>, ApiError> {
    let (scene, status) = service.runtime_status(&id).await?;
    Ok(Json(SceneStatusResponse {
        scene_id: scene.id,
        enabled: scene.enabled,
        status,
    }))
}

pub(super) fn routes() -> Router<Svc> {
    Router::new().route("/scenes/by-id/{id}/status", get(scene_status))
}

#[cfg(test)]
mod tests {
    use crate::modules::director::handle::{DirectorCommand, SceneState, SceneStatus};
    use crate::modules::scene::api::Api;
    use crate::modules::scene::service::tests::fixture;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use serde_json::Value;
    use tower::ServiceExt;

    async fn get(service: super::Svc, uri: &str) -> (StatusCode, Value) {
        let req = Request::builder().uri(uri).body(Body::empty()).unwrap();
        let response = Api::build_router(service).oneshot(req).await.unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    #[tokio::test]
    async fn scene_status_over_http() {
        let f = fixture().await;
        let scene = f.scene().await;
        let uri = format!("/scenes/by-id/{}/status", scene.id);

        // The director answers (here: a stand-in).
        let answer = |status: Option<SceneStatus>| async {
            let mut rx = f.director.lock().await;
            match rx.recv().await {
                Some(DirectorCommand::Status { reply, .. }) => {
                    let _ = reply.send(status);
                }
                other => panic!("expected Status, got {other:?}"),
            }
        };
        let failed = SceneStatus {
            state: SceneState::Failed,
            errors: vec!["edge e: a sink can't send events".into()],
            max_hops: None,
            nodes: vec![],
        };
        let ((code, body), ()) = tokio::join!(get(f.service, &uri), answer(Some(failed)));
        assert_eq!(code, StatusCode::OK);
        assert_eq!(body["scene_id"], scene.id.as_str());
        assert_eq!(body["enabled"], false);
        assert_eq!(body["state"], "FAILED");
        assert_eq!(body["errors"][0], "edge e: a sink can't send events");

        // Never run since the server started: STOPPED.
        let ((code, body), ()) = tokio::join!(get(f.service, &uri), answer(None));
        assert_eq!(
            (code, &body["state"]),
            (StatusCode::OK, &Value::from("STOPPED"))
        );
        assert_eq!(body["nodes"], Value::Array(vec![]));

        let (code, _) = get(f.service, "/scenes/by-id/nope/status").await;
        assert_eq!(code, StatusCode::NOT_FOUND);
    }
}
