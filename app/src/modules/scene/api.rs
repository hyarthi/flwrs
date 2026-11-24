use crate::modules::api_error::{ApiError, ErrorBody, api_error, internal};
use crate::modules::node_config::SceneDefaults;
use crate::modules::scene::groups::SceneGroup;
use crate::modules::scene::service::{self, ListFilters, Service, ServiceError};
use crate::modules::scene::{
    api_draft, api_graph, api_mapping, api_preflight, api_status, api_transfer, api_uploads,
};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use flwrs_types::groups::Group;
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
            ServiceError::Unavailable(message) => {
                api_error(StatusCode::SERVICE_UNAVAILABLE, message, vec![])
            }
            ServiceError::Forbidden(message) => api_error(StatusCode::FORBIDDEN, message, vec![]),
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
    /// Events are dropped after this many handovers (stops runaway loops in
    /// cycles). `null` = the server default (`[director] max_hops`, 32).
    pub max_hops: Option<u32>,
    /// Defaults for the scene's nodes; a node's own setting wins.
    pub defaults: SceneDefaults,
    /// Groups of the scene's nodes. A node names the ones it's in
    /// (`config.groups`).
    #[serde(default)]
    #[schema(value_type = Vec<SceneGroup>)]
    pub groups: Vec<Group>,
    /// Counts committed changes to the live graph (settings, nodes, edges,
    /// uploads). A draft records the one it's based on; the bulk endpoint
    /// can require it (`base_revision`).
    pub revision: i64,
    pub create_time: i64,
    pub update_time: i64,
}

impl From<service::Scene> for Scene {
    fn from(value: service::Scene) -> Self {
        Self {
            id: value.id,
            name: value.name,
            enabled: value.enabled,
            max_hops: value.max_hops,
            defaults: value.defaults,
            groups: value.groups,
            revision: value.revision,
            create_time: value.create_time.timestamp_millis(),
            update_time: value.update_time.timestamp_millis(),
        }
    }
}

#[derive(Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct SceneRequest {
    pub name: String,
    /// Hop limit for this scene, 1 to 10 000. Omitted or `null` = the
    /// server default. Changing it on an enabled scene restarts the scene.
    #[serde(default)]
    pub max_hops: Option<u32>,
    /// Defaults for the scene's nodes: `durable` (queues kept on disk until
    /// acknowledged), `remote_auth` (`token` or `mtls`), `require_tls`.
    /// Omitted = none. Changing them on an enabled scene reloads it.
    #[serde(default)]
    pub defaults: SceneDefaults,
    /// The scene's groups, replacing the ones it has. Omitted = kept. Only
    /// `resources` groups are accepted: a scene has no use for `placement`
    /// or `execution` policies, and refuses them.
    #[serde(default)]
    #[schema(value_type = Option<Vec<SceneGroup>>)]
    pub groups: Option<Vec<Group>>,
}

impl From<SceneRequest> for service::SceneInput {
    fn from(req: SceneRequest) -> Self {
        Self {
            name: req.name,
            max_hops: req.max_hops,
            defaults: req.defaults,
            groups: req.groups,
        }
    }
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
    Ok(Json(service.create_scene(&req.into()).await?.into()))
}

#[utoipa::path(
    put,
    path = "/by-id/{id}",
    operation_id = "update-scene",
    summary = "Replace a scene's settings (name, hop limit, defaults, groups)",
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
    Ok(Json(service.update_scene(&id, &req.into()).await?.into()))
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
    description = "Every node's plugin must be registered and installed, and local nodes whose plugin declares a pre-flight check must have no permanent problem on this machine (retryable ones don't refuse it: the scene goes to WAITING). Enabling an enabled scene does nothing.",
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

/// `X-Flwrs-Undo-Group` (and `X-Flwrs-Undo-Label`): draft changes made by
/// requests with the same group are one undo step (e.g. a paste's nodes and
/// edges), named by the label if given.
pub(crate) const UNDO_GROUP_HEADER: &str = "x-flwrs-undo-group";
pub(crate) const UNDO_LABEL_HEADER: &str = "x-flwrs-undo-label";

async fn undo_group(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let group = {
        let headers = request.headers();
        let header = |name: &str| {
            headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(|v| v.chars().take(200).collect::<String>())
                .filter(|v| !v.is_empty())
        };
        header(UNDO_GROUP_HEADER).map(|g| (g, header(UNDO_LABEL_HEADER)))
    };
    crate::modules::scene::service::draft::history::with_group(group, next.run(request)).await
}

#[derive(OpenApi)]
#[openapi(
    info(
        title = "Scenes",
        description = "Scenes API: scenes, their nodes and edges, export and import"
    ),
    paths(
        list_scenes,
        create_scene,
        get_scene,
        update_scene,
        delete_scene,
        enable_scene,
        disable_scene,
        api_transfer::export_scene,
        api_transfer::import_scene,
        api_transfer::check_scene,
        api_status::scene_status,
        api_status::purge_scene_spool,
        api_status::purge_node_spool,
        api_preflight::preflight_scene,
        api_preflight::preflight_node,
        api_preflight::describe_node,
        api_graph::list_nodes,
        api_graph::create_node,
        api_graph::get_node,
        api_graph::update_node,
        api_graph::delete_node,
        api_graph::configure_node,
        api_graph::list_edges,
        api_graph::create_edge,
        api_graph::get_edge,
        api_graph::update_edge,
        api_graph::delete_edge,
        api_graph::set_layout,
        api_uploads::put_upload,
        api_uploads::list_uploads,
        api_uploads::get_upload,
        api_uploads::get_upload_content,
        api_uploads::delete_upload,
        api_uploads::reset_workdir,
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
        api_graph::LayoutRequest,
        crate::modules::scene::service::Position,
        api_transfer::SceneCheckResponse,
        api_transfer::ImportResponse,
        api_transfer::SceneUpload,
        api_transfer::ExportFormat,
        api_status::SceneStatusResponse,
        api_status::PurgeResponse,
        crate::modules::director::handle::PreflightIssue,
        crate::modules::director::handle::FailedUpdate,
        service::draft::submit::DraftSubmit,
        service::draft::submit::DraftSubmitState,
        crate::modules::director::handle::PreflightRetryStatus,
        crate::modules::node_config::PreflightRetryOverride,
        service::preflight::PreflightRun,
        service::preflight::NodePreflight,
        service::preflight::NodePreflightProblem,
        api_uploads::Upload,
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
            .merge(api_transfer::routes())
            .merge(api_status::routes())
            .merge(api_preflight::routes())
            .merge(api_uploads::routes())
            .merge(api_draft::routes())
            .merge(api_mapping::routes())
            .merge(super::api_usage::routes())
            .with_state(service)
            .layer(axum::middleware::from_fn(undo_group))
    }
}
