//! `GET /api/plugins/usage`: which scenes' nodes use which plugins (the
//! plugins pages: "used by", and why a delete is refused). Served by the
//! scene service, which knows the nodes.

use super::api::Svc;
use crate::modules::api_error::{ApiError, ErrorBody};
use crate::modules::scene::service::PluginUse;
use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use utoipa::OpenApi;

#[utoipa::path(
    get,
    path = "/usage",
    operation_id = "plugin-usage",
    summary = "Which nodes use which plugins",
    description = "Every node, live or only in a scene's draft, with its plugin and scene. A plugin that live nodes use can't be deleted (nodes only in drafts don't count).",
    responses((status = 200, description = "Nodes by plugin", body = [PluginUse]))
)]
pub(crate) async fn usage(State(service): State<Svc>) -> Result<Json<Vec<PluginUse>>, ApiError> {
    Ok(Json(service.plugin_usage().await?))
}

#[derive(OpenApi)]
#[openapi(paths(usage), components(schemas(PluginUse, ErrorBody)))]
pub(crate) struct Api;

pub(super) fn routes() -> Router<Svc> {
    Router::new().route("/plugins/usage", get(usage))
}
