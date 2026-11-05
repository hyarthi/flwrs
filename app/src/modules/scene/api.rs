use crate::modules::api_error::{ApiError, ErrorBody, api_error, internal};
use crate::modules::scene::api_graph;
use crate::modules::scene::service::{self, ListFilters, Service, ServiceError};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, OpenApi, ToSchema};

pub(crate) type Svc = &'static Service;

impl From<ServiceError> for ApiError {
    fn from(e: ServiceError) -> Self {
        match e {
            ServiceError::NotFound(what) => {
                api_error(StatusCode::NOT_FOUND, format!("{what} not found"), vec![])
            }
            ServiceError::Invalid(details) => {
                api_error(StatusCode::UNPROCESSABLE_ENTITY, "invalid request", details)
            }
            ServiceError::Conflict(message, details) => {
                api_error(StatusCode::CONFLICT, message, details)
            }
            e @ (ServiceError::Query(_)
            | ServiceError::Connection(_)
            | ServiceError::Unknown(_)) => internal("Scenes API", e),
        }
    }
}

#[derive(Serialize, Deserialize, ToSchema)]
pub(crate) struct Scene {
    pub id: String,
    pub name: String,
    /// Whether the scene's workflow runs (true) or is dormant (false).
    pub enabled: bool,
    pub create_time: i64,
    pub update_time: i64,
}

impl From<service::Scene> for Scene {
    fn from(value: service::Scene) -> Self {
        Self {
            id: value.id,
            name: value.name,
            enabled: value.enabled,
            create_time: value.create_time.timestamp_millis(),
            update_time: value.update_time.timestamp_millis(),
        }
    }
}

#[derive(Serialize, Deserialize, ToSchema)]
pub(crate) struct SceneRequest {
    pub name: String,
}

const DEFAULT_LIMIT: u32 = 50;

#[derive(Deserialize, IntoParams, Clone)]
pub(crate) struct ListFiltersParams {
    pub offset: Option<u32>,
    pub limit: Option<u32>,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct ListScenesResponse {
    scenes: Vec<Scene>,
    has_more: bool,
}

#[utoipa::path(
    get,
    path = "/by-id/{id}",
    operation_id = "get-scene",
    summary = "Get scene by ID",
    params(("id" = String, Path, description = "ID of the scene to fetch")),
    responses(
        (status = 200, description = "Scene", body = Scene),
        (status = 404, description = "Not found", body = ErrorBody),
    )
)]
async fn get_scene(
    State(service): State<Svc>,
    Path(id): Path<String>,
) -> Result<Json<Scene>, ApiError> {
    Ok(Json(service.get_scene(&id).await?.into()))
}

#[utoipa::path(
    post,
    path = "",
    operation_id = "create-scene",
    summary = "Create a new scene",
    description = "New scenes are disabled (dormant).",
    request_body(content = SceneRequest, content_type = "application/json"),
    responses(
        (status = 200, description = "Created scene", body = Scene),
        (status = 422, description = "Invalid request", body = ErrorBody),
    )
)]
async fn create_scene(
    State(service): State<Svc>,
    Json(req): Json<SceneRequest>,
) -> Result<Json<Scene>, ApiError> {
    Ok(Json(service.create_scene(&req.name).await?.into()))
}

#[utoipa::path(
    put,
    path = "/by-id/{id}",
    operation_id = "update-scene",
    summary = "Rename a scene",
    request_body(content = SceneRequest, content_type = "application/json"),
    params(("id" = String, Path, description = "ID of the scene to update")),
    responses(
        (status = 200, description = "Updated scene", body = Scene),
        (status = 404, description = "Not found", body = ErrorBody),
        (status = 422, description = "Invalid request", body = ErrorBody),
    )
)]
async fn update_scene(
    State(service): State<Svc>,
    Path(id): Path<String>,
    Json(req): Json<SceneRequest>,
) -> Result<Json<Scene>, ApiError> {
    Ok(Json(service.update_scene(&id, &req.name).await?.into()))
}

#[utoipa::path(
    delete,
    path = "/by-id/{id}",
    operation_id = "delete-scene",
    summary = "Delete a scene with its nodes and edges",
    description = "An enabled scene is stopped first.",
    params(("id" = String, Path, description = "ID of the scene to delete")),
    responses(
        (status = 200, description = "Deleted"),
        (status = 404, description = "Not found", body = ErrorBody),
    )
)]
async fn delete_scene(
    State(service): State<Svc>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    service.delete_scene(&id).await?;
    Ok(StatusCode::OK)
}

#[utoipa::path(
    get,
    path = "",
    operation_id = "list-scenes",
    summary = "List scenes (paginated)",
    params(ListFiltersParams),
    responses((status = 200, description = "Scene page", body = ListScenesResponse))
)]
async fn list_scenes(
    State(service): State<Svc>,
    Query(filters): Query<ListFiltersParams>,
) -> Result<Json<ListScenesResponse>, ApiError> {
    let filters = ListFilters::new(
        i64::from(filters.offset.unwrap_or(0)),
        i64::from(filters.limit.unwrap_or(DEFAULT_LIMIT)),
    );
    let (scenes, has_more) = service.list_scenes(filters).await?;
    Ok(Json(ListScenesResponse {
        scenes: scenes.into_iter().map(From::from).collect(),
        has_more,
    }))
}

#[utoipa::path(
    post,
    path = "/by-id/{id}/enable",
    operation_id = "enable-scene",
    summary = "Enable a scene: the director starts its workflow",
    description = "Every node's plugin must be registered and installed. Enabling an enabled scene does nothing.",
    params(("id" = String, Path, description = "Scene ID")),
    responses(
        (status = 200, description = "Enabled scene", body = Scene),
        (status = 404, description = "Not found", body = ErrorBody),
        (status = 409, description = "Can't be enabled; details list the problems", body = ErrorBody),
    )
)]
async fn enable_scene(
    State(service): State<Svc>,
    Path(id): Path<String>,
) -> Result<Json<Scene>, ApiError> {
    Ok(Json(service.set_enabled(&id, true).await?.into()))
}

#[utoipa::path(
    post,
    path = "/by-id/{id}/disable",
    operation_id = "disable-scene",
    summary = "Disable a scene: the director stops its workflow",
    params(("id" = String, Path, description = "Scene ID")),
    responses(
        (status = 200, description = "Disabled scene", body = Scene),
        (status = 404, description = "Not found", body = ErrorBody),
    )
)]
async fn disable_scene(
    State(service): State<Svc>,
    Path(id): Path<String>,
) -> Result<Json<Scene>, ApiError> {
    Ok(Json(service.set_enabled(&id, false).await?.into()))
}

#[derive(OpenApi)]
#[openapi(
    info(
        title = "Scenes",
        description = "Scenes API: scenes, their nodes and edges"
    ),
    paths(
        list_scenes,
        create_scene,
        get_scene,
        update_scene,
        delete_scene,
        enable_scene,
        disable_scene,
        api_graph::list_nodes,
        api_graph::create_node,
        api_graph::get_node,
        api_graph::update_node,
        api_graph::delete_node,
        api_graph::list_edges,
        api_graph::create_edge,
        api_graph::get_edge,
        api_graph::update_edge,
        api_graph::delete_edge,
    ),
    components(schemas(
        Scene,
        SceneRequest,
        ListScenesResponse,
        ErrorBody,
        api_graph::Node,
        api_graph::NodeRequest,
        api_graph::Edge,
        api_graph::EdgeRequest,
    ))
)]
pub(crate) struct Api;

impl Api {
    pub(crate) fn build_router(service: Svc) -> Router {
        Router::new()
            .route("/scenes", get(list_scenes).post(create_scene))
            .route(
                "/scenes/by-id/{id}",
                get(get_scene).put(update_scene).delete(delete_scene),
            )
            .route("/scenes/by-id/{id}/enable", post(enable_scene))
            .route("/scenes/by-id/{id}/disable", post(disable_scene))
            .merge(api_graph::routes())
            .with_state(service)
    }
}
