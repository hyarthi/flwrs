//! REST for the server's run-time overview (`GET /api/status`).

use crate::modules::api_error::{ApiError, ErrorBody, api_error};
use crate::modules::director::DirectorHandle;
use crate::modules::director::handle::{Overview, SceneState, SceneSummary, Usage};
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use utoipa::OpenApi;

type Director = &'static DirectorHandle;

#[utoipa::path(
    get,
    path = "",
    operation_id = "server-status",
    summary = "The server at a glance: every scene's state, and memory and disk use",
    description = "Scenes the director has handled since it started, with node counts; the hub address; the hub-wide queue budget, and the spill memory and disk in use. Details per scene: `GET /api/scenes/by-id/{id}/status`.",
    responses(
        (status = 200, description = "Overview", body = Overview),
        (status = 503, description = "The director isn't answering", body = ErrorBody),
    )
)]
async fn server_status(State(director): State<Director>) -> Result<Json<Overview>, ApiError> {
    director.overview().await.map(Json).map_err(|e| {
        api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            format!("director: {e}"),
            vec![],
        )
    })
}

#[derive(OpenApi)]
#[openapi(
    info(title = "Status", description = "Server run-time status"),
    paths(server_status),
    components(schemas(Overview, Usage, SceneSummary, SceneState, ErrorBody))
)]
pub(crate) struct Api;

impl Api {
    pub(crate) fn build_router(director: Director) -> Router {
        Router::new()
            .route("/status", get(server_status))
            .with_state(director)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::director::handle::DirectorCommand;
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    #[tokio::test]
    async fn server_status_over_http() {
        let (handle, mut rx) = DirectorHandle::channel();
        let handle: Director = Box::leak(Box::new(handle));
        let answer = async move {
            match rx.recv().await {
                Some(DirectorCommand::Overview { reply }) => {
                    let _ = reply.send(Overview {
                        hub_address: "127.0.0.1:7400".into(),
                        queued_memory: Usage {
                            used_bytes: 10,
                            limit_bytes: 100,
                        },
                        spill_memory: Usage {
                            used_bytes: 0,
                            limit_bytes: 50,
                        },
                        spill_disk: Usage {
                            used_bytes: 0,
                            limit_bytes: 500,
                        },
                        scenes: vec![SceneSummary {
                            scene_id: "s1".into(),
                            state: SceneState::Running,
                            nodes: 2,
                            running_nodes: 1,
                            errors: 0,
                        }],
                    });
                }
                other => panic!("expected Overview, got {other:?}"),
            }
            rx // keep the channel open until the request is done
        };
        let request = async {
            let req = Request::builder()
                .uri("/status")
                .body(Body::empty())
                .unwrap();
            let response = Api::build_router(handle).oneshot(req).await.unwrap();
            let status = response.status();
            let bytes = response.into_body().collect().await.unwrap().to_bytes();
            (
                status,
                serde_json::from_slice::<serde_json::Value>(&bytes).unwrap(),
            )
        };
        let ((code, body), _rx) = tokio::join!(request, answer);
        assert_eq!(code, StatusCode::OK);
        assert_eq!(body["hub_address"], "127.0.0.1:7400");
        assert_eq!(body["queued_memory"]["used_bytes"], 10);
        assert_eq!(body["scenes"][0]["state"], "RUNNING");
        assert_eq!(body["scenes"][0]["running_nodes"], 1);

        // No director: 503.
        let (handle, rx) = DirectorHandle::channel();
        drop(rx);
        let handle: Director = Box::leak(Box::new(handle));
        let req = Request::builder()
            .uri("/status")
            .body(Body::empty())
            .unwrap();
        let response = Api::build_router(handle).oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }
}
