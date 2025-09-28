use crate::modules::scene;
use crate::modules::scene::service::ServiceError;
use axum::extract::{Path, Query};
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use chrono::Local;
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, OpenApi, ToSchema};

#[derive(Serialize, Deserialize, ToSchema)]
pub(crate) struct Scene {
    pub id: String,
    pub name: String,
    pub create_time: i64,
    pub update_time: i64,
}

impl From<scene::service::Scene> for Scene {
    fn from(value: scene::service::Scene) -> Self {
        Self {
            id: value.id,
            name: value.name,
            create_time: value.create_time.timestamp_millis(),
            update_time: value.update_time.timestamp_millis(),
        }
    }
}

#[derive(Serialize, Deserialize, ToSchema)]
pub(crate) struct SceneRequest {
    pub name: String,
}

impl Into<scene::service::Scene> for SceneRequest {
    fn into(self) -> scene::service::Scene {
        scene::service::Scene {
            id: "".to_string(),
            name: self.name,
            create_time: Local::now(),
            update_time: Local::now(),
        }
    }
}

#[utoipa::path(
    get,
    path = "/by-id/{id}",
    operation_id = "get-scene",
    description = "Get scene by ID",
    summary = "Get scene by ID",
    responses(
        (status = 200, description = "Scene", body = Scene),
        (status = 404, description = "Not found"),
        (status = 500, description = "Internal Server Error"),
    ),
    params(
        ("id" = String, Path, description = "ID of the scene to fetch")
    )
)]
async fn get_scene(Path(id): Path<String>) -> Result<Json<Scene>, StatusCode> {
    log::trace!("Scenes API: getting scene [{id}]");
    match scene::service().await.get_scene(id.as_str()).await {
        Ok(scene) => {
            log::trace!("Scenes API: returning scene [{id}]");
            Ok(Json(Scene::from(scene)))
        }
        Err(e) => match e {
            ServiceError::NotFound => {
                log::trace!("Scenes API: Failed to get scene [{id}]: not found");
                Err(StatusCode::NOT_FOUND)
            }
            _ => {
                log::error!("Scenes API: Failed to get scene [{id}]: {e}");
                Err(StatusCode::INTERNAL_SERVER_ERROR)
            }
        },
    }
}

#[utoipa::path(
    post,
    path = "",
    operation_id = "create-scene",
    description = "Create a new scene",
    summary = "Create a new scene",
    request_body(
        content = SceneRequest,
        description = "Scene creation request",
        content_type = "application/json"
    ),
    responses(
        (status = 200, description = "Created scene", body = Scene),
        (status = 400, description = "Bad request"),
        (status = 409, description = "Conflict"),
        (status = 500, description = "Internal Server Error"),
    ),
)]
async fn create_scene(Json(scene): Json<SceneRequest>) -> Result<Json<Scene>, StatusCode> {
    log::trace!("Scenes API: creating scene");
    match scene::service().await.create_scene(scene.into()).await {
        Ok(scene) => {
            log::trace!("Scenes API: created scene [{id}]", id = scene.id);
            Ok(Json(Scene::from(scene)))
        }
        Err(e) => match e {
            ServiceError::NotFound => {
                log::trace!("Scenes API: Failed to create scene: not found");
                Err(StatusCode::NOT_FOUND)
            }
            ServiceError::Conflict => {
                log::trace!("Scenes API: Failed to create scene: conflict");
                Err(StatusCode::CONFLICT)
            }
            _ => {
                log::error!("Scenes API: Failed to create scene: {e}");
                Err(StatusCode::INTERNAL_SERVER_ERROR)
            }
        },
    }
}

#[utoipa::path(
    put,
    path = "/by-id/{id}",
    operation_id = "update-scene",
    description = "Update an existing scene by ID",
    summary = "Update an existing scene by ID",
    request_body(
        content = SceneRequest,
        description = "Scene update request",
        content_type = "application/json"
    ),
    responses(
        (status = 200, description = "Updated scene", body = Scene),
        (status = 400, description = "Bad request"),
        (status = 404, description = "Not found"),
        (status = 500, description = "Internal Server Error"),
    ),
    params(
                ("id" = String, Path, description = "ID of the scene to update")
    )
)]
async fn update_scene(
    Path(id): Path<String>,
    Json(scene): Json<SceneRequest>,
) -> Result<Json<Scene>, StatusCode> {
    log::trace!("Scenes API: Updating scene [{id}]");
    let mut input: scene::service::Scene = scene.into();
    input.id = id.clone();
    match scene::service().await.update_scene(input).await {
        Ok(result) => {
            log::trace!("Scenes API: returning updated result [{id}]");
            Ok(Json(Scene::from(result)))
        }
        Err(e) => match e {
            ServiceError::NotFound => {
                log::trace!("Scenes API: Failed to update scene [{id}]: not found");
                Err(StatusCode::NOT_FOUND)
            }
            ServiceError::Conflict => {
                log::trace!("Scenes API: Failed to update scene [{id}]: conflict");
                Err(StatusCode::CONFLICT)
            }
            _ => {
                log::error!("Scenes API: Failed to update scene [{id}]: {e}");
                Err(StatusCode::INTERNAL_SERVER_ERROR)
            }
        },
    }
}

#[utoipa::path(
    delete,
    path = "/by-id/{id}",
    operation_id = "delete-scene",
    description = "Delete an existing scene by ID",
    summary = "Delete an existing scene by ID",
    responses(
        (status = 200, description = "Success"),
        (status = 404, description = "Not found"),
        (status = 500, description = "Internal Server Error"),
    ),
    params(
        ("id" = String, Path, description = "ID of the scene to delete")
    )
)]
pub(crate) async fn delete_scene(Path(id): Path<String>) -> Result<StatusCode, StatusCode> {
    log::trace!("Scenes API: Deleting scene [{id}]");
    match scene::service().await.delete_scene(id.as_str()).await {
        Ok(_) => {
            log::trace!("Scenes API: Deleted scene [{id}]");
            Ok(StatusCode::OK)
        }
        Err(e) => match e {
            ServiceError::NotFound => {
                log::trace!("Scenes API: Failed to delete scene [{id}]: not found");
                Err(StatusCode::NOT_FOUND)
            }
            ServiceError::Conflict => {
                log::trace!("Scenes API: Failed to delete scene [{id}]: conflict");
                Err(StatusCode::CONFLICT)
            }
            _ => {
                log::error!("Scenes API: Failed to delete scene [{id}]: {e}");
                Err(StatusCode::INTERNAL_SERVER_ERROR)
            }
        },
    }
}

const DEFAULT_LIMIT: u32 = 50;

#[derive(Deserialize, IntoParams, Clone)]
pub(crate) struct ListFilters {
    pub offset: Option<u32>,
    pub limit: Option<u32>,
}

impl Into<scene::service::ListFilters> for ListFilters {
    fn into(self) -> scene::service::ListFilters {
        scene::service::ListFilters::new(
            i64::from(self.offset.unwrap_or(0)),
            i64::from(self.limit.unwrap_or(DEFAULT_LIMIT)),
        )
    }
}

#[derive(Serialize, ToSchema)]
pub(crate) struct ListScenesResponse {
    scenes: Vec<Scene>,
    has_more: bool,
}

#[utoipa::path(
    get,
    path = "",
    operation_id = "list-scenes",
    description = "List scenes (paginated)",
    summary = "List scenes (paginated)",
    responses(
        (status = 200, description = "Scene page", body = ListScenesResponse),
        (status = 500, description = "Internal Server Error"),
    ),
    params(
        ListFilters
    )
)]
async fn list_scenes(
    Query(filters): Query<ListFilters>,
) -> Result<Json<ListScenesResponse>, StatusCode> {
    log::trace!(
        "Scenes API: Listing scenes [{offset}:{limit}]",
        offset = filters.offset.unwrap_or(0),
        limit = filters.limit.unwrap_or(DEFAULT_LIMIT)
    );
    match scene::service()
        .await
        .list_scenes(filters.clone().into())
        .await
    {
        Ok((scenes, has_more)) => {
            log::trace!(
                "Scenes API: Returning list scene result [{offset}:{limit}]",
                offset = filters.offset.unwrap_or(0),
                limit = filters.limit.unwrap_or(DEFAULT_LIMIT)
            );
            Ok(Json(ListScenesResponse {
                scenes: scenes.into_iter().map(From::from).collect(),
                has_more,
            }))
        }
        Err(e) => match e {
            _ => {
                log::error!("Scenes API: Failed to list scenes: {e}");
                Err(StatusCode::INTERNAL_SERVER_ERROR)
            }
        },
    }
}

#[derive(OpenApi)]
#[openapi(
    info(title = "Scenes", description = "Scenes API",),
    paths(list_scenes, create_scene, get_scene, update_scene, delete_scene,),
    components(schemas(Scene, SceneRequest, ListScenesResponse,))
)]
pub(crate) struct Api;

impl Api {
    pub(crate) fn build_router() -> Router {
        Router::new()
            .route("/scenes", get(list_scenes).post(create_scene))
            .route(
                "/scenes/by-id/{id}",
                get(get_scene).put(update_scene).delete(delete_scene),
            )
    }
}
