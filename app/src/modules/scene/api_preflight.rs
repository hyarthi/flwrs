//! REST for pre-flight checks: can these nodes run on this machine now?
//! Nothing is started or changed.

use crate::modules::api_error::{ApiError, ErrorBody};
use crate::modules::scene::api::Svc;
use crate::modules::scene::service::preflight::PreflightRun;
use axum::extract::{Path, Query, State};
use axum::routing::post;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use utoipa::{IntoParams, ToSchema};

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub(crate) struct PreflightQuery {
    /// Check the scene's draft instead of its live graph.
    #[serde(default)]
    draft: bool,
}

#[utoipa::path(
    post,
    path = "/by-id/{id}/preflight",
    operation_id = "scene-preflight",
    summary = "Check whether the scene's nodes can run on this machine now",
    description = "Runs the pre-flight check of every local node whose plugin declares one (`configurator.preflight`), in parallel, without starting anything. Remote nodes are skipped.",
    params(("id" = String, Path, description = "Scene ID"), PreflightQuery),
    responses(
        (status = 200, description = "What the checks found", body = PreflightRun),
        (status = 404, description = "Not found (or no draft)", body = ErrorBody),
    )
)]
pub(super) async fn preflight_scene(
    State(service): State<Svc>,
    Path(id): Path<String>,
    Query(query): Query<PreflightQuery>,
) -> Result<Json<PreflightRun>, ApiError> {
    Ok(Json(service.preflight(&id, None, query.draft).await?))
}

#[utoipa::path(
    post,
    path = "/by-id/{id}/nodes/by-id/{node_id}/preflight",
    operation_id = "node-preflight",
    summary = "Check whether one node can run on this machine now",
    description = "The scene's pre-flight check, for one node.",
    params(
        ("id" = String, Path, description = "Scene ID"),
        ("node_id" = String, Path, description = "Node ID"),
        PreflightQuery,
    ),
    responses(
        (status = 200, description = "What the check found", body = PreflightRun),
        (status = 404, description = "Not found (or no draft)", body = ErrorBody),
    )
)]
pub(super) async fn preflight_node(
    State(service): State<Svc>,
    Path((id, node_id)): Path<(String, String)>,
    Query(query): Query<PreflightQuery>,
) -> Result<Json<PreflightRun>, ApiError> {
    Ok(Json(
        service.preflight(&id, Some(&node_id), query.draft).await?,
    ))
}

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct DescribeRequest {
    /// Launch args naming the uploads to describe (only upload settings are
    /// used, so the rest may be incomplete).
    #[schema(value_type = Object)]
    launch_args: Value,
}

#[derive(Serialize, ToSchema)]
pub(super) struct DescribeResponse {
    /// The configured fields' values for these files; `null`: the plugin
    /// doesn't describe files.
    #[schema(value_type = Option<Object>)]
    values: Option<Map<String, Value>>,
}

#[utoipa::path(
    post,
    path = "/by-id/{id}/nodes/by-id/{node_id}/describe",
    operation_id = "node-describe",
    summary = "Ask a node's configurator about its uploaded files",
    description = "Runs the plugin's read-only describe mode (`configurator.describe`) on the named uploads: what an editor proposes for a picked file. Nothing is stored.",
    params(
        ("id" = String, Path, description = "Scene ID"),
        ("node_id" = String, Path, description = "Node ID"),
        PreflightQuery,
    ),
    request_body(content = DescribeRequest, content_type = "application/json"),
    responses(
        (status = 200, description = "What it says", body = DescribeResponse),
        (status = 404, description = "Not found (or no draft)", body = ErrorBody),
        (status = 422, description = "The files can't be described", body = ErrorBody),
    )
)]
pub(super) async fn describe_node(
    State(service): State<Svc>,
    Path((id, node_id)): Path<(String, String)>,
    Query(query): Query<PreflightQuery>,
    Json(request): Json<DescribeRequest>,
) -> Result<Json<DescribeResponse>, ApiError> {
    let values = service
        .describe_node(&id, &node_id, query.draft, &request.launch_args)
        .await?;
    Ok(Json(DescribeResponse { values }))
}

pub(super) fn routes() -> Router<Svc> {
    Router::new()
        .route("/scenes/by-id/{id}/preflight", post(preflight_scene))
        .route(
            "/scenes/by-id/{id}/nodes/by-id/{node_id}/preflight",
            post(preflight_node),
        )
        .route(
            "/scenes/by-id/{id}/nodes/by-id/{node_id}/describe",
            post(describe_node),
        )
}

#[cfg(test)]
mod tests {
    use crate::modules::scene::api::Api;
    use crate::modules::scene::service::tests::fixture;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use serde_json::Value;
    use tower::ServiceExt;

    async fn post(service: super::Svc, uri: &str) -> (StatusCode, Value) {
        let req = Request::post(uri).body(Body::empty()).unwrap();
        let response = Api::build_router(service).oneshot(req).await.unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    #[tokio::test]
    async fn preflight_over_http() {
        let f = fixture().await;
        let scene = f.scene().await;
        let base = format!("/scenes/by-id/{}", scene.id);
        let (code, body) = post(f.service, &format!("{base}/preflight")).await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(
            (&body["ok"], &body["permanent"]),
            (&Value::Bool(true), &Value::Bool(false))
        );
        // No draft, no such node, no such scene.
        let (code, _) = post(f.service, &format!("{base}/preflight?draft=true")).await;
        assert_eq!(code, StatusCode::NOT_FOUND);
        let (code, _) = post(f.service, &format!("{base}/nodes/by-id/nope/preflight")).await;
        assert_eq!(code, StatusCode::NOT_FOUND);
        let (code, _) = post(f.service, "/scenes/by-id/nope/preflight").await;
        assert_eq!(code, StatusCode::NOT_FOUND);
    }
}
