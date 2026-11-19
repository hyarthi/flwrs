//! `POST …/scenes/by-id/{id}/mapping/test`: tries an edge mapping out for
//! the mapping editor (see `service/mapping_test.rs`).

use super::api::Svc;
use crate::modules::api_error::{ApiError, ErrorBody};
use crate::modules::scene::service::Store;
use crate::modules::scene::service::mapping_test::{MappingTest, SchemaField, TestOutcome};
use axum::extract::{Path, State};
use axum::routing::post;
use axum::{Json, Router};
use serde::Deserialize;
use utoipa::{OpenApi, ToSchema};

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct MappingTestRequest {
    /// The edge's ends (they needn't be connected yet).
    pub source_node_id: String,
    pub target_node_id: String,
    /// Read the nodes from the scene's draft (default) or from live.
    #[serde(default = "yes")]
    pub draft: bool,
    /// The mapping to try (`null` = none: events pass through).
    #[serde(default)]
    #[schema(value_type = Option<Object>)]
    pub mapping: serde_json::Value,
    /// A sample payload as the source would send it: JSON shaped by its
    /// `out` schema (BYTES as base64, DATETIME as RFC 3339). Without one,
    /// only the schemas and the mapping's problems come back.
    #[schema(value_type = Option<Object>)]
    pub payload: Option<serde_json::Value>,
}

fn yes() -> bool {
    true
}

#[utoipa::path(
    post,
    path = "/by-id/{id}/mapping/test",
    operation_id = "test-mapping",
    summary = "Try an edge mapping on a sample payload",
    description = "Returns both nodes' payload schemas (`null` when unknown), the mapping's problems as saving it would report them, and, for a `payload`, what the mapping makes of it: `output` (the payload sent along the edge), `filtered`, or `error`. Lua runs with the mapping's limits. Nothing is saved or sent; a draft isn't started.",
    params(("id" = String, Path, description = "Scene ID")),
    request_body(content = MappingTestRequest, content_type = "application/json"),
    responses(
        (status = 200, description = "The test", body = MappingTest),
        (status = 404, description = "No such scene or node (in the draft, or live)", body = ErrorBody),
    )
)]
pub(crate) async fn test_mapping(
    State(service): State<Svc>,
    Path(id): Path<String>,
    Json(req): Json<MappingTestRequest>,
) -> Result<Json<MappingTest>, ApiError> {
    // Without a draft, its nodes are live's.
    let store = if req.draft && service.get_draft(&id).await.is_ok() {
        Store::Draft
    } else {
        Store::Live
    };
    Ok(Json(
        service
            .test_mapping(
                &id,
                store,
                &req.source_node_id,
                &req.target_node_id,
                &req.mapping,
                req.payload.as_ref(),
            )
            .await?,
    ))
}

#[derive(OpenApi)]
#[openapi(
    paths(test_mapping),
    components(schemas(MappingTestRequest, MappingTest, SchemaField, TestOutcome, ErrorBody))
)]
pub(crate) struct Api;

pub(super) fn routes() -> Router<Svc> {
    Router::new().route("/scenes/by-id/{id}/mapping/test", post(test_mapping))
}
