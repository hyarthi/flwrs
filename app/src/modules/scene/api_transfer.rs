//! REST for moving scenes between servers (`.flwr` files), and for checking
//! whether a scene can run on this one.

use crate::modules::api_error::{ApiError, ErrorBody, api_error};
use crate::modules::scene::api::{Scene, Svc};
use crate::modules::scene::scene_file::MAX_FILE_BYTES;
use crate::modules::scene::service::SceneCheck;
use axum::extract::{DefaultBodyLimit, Multipart, Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

/// Media type of `.flwr` files.
const MEDIA_TYPE: &str = "application/vnd.flwrs.scene";

/// Whether a scene can run on this server.
#[derive(Serialize, Deserialize, ToSchema)]
pub(crate) struct SceneCheckResponse {
    /// No problems: the scene can be enabled.
    pub launchable: bool,
    /// What blocks enabling, e.g. a plugin that isn't installed here, or a
    /// remote node without a token.
    pub problems: Vec<String>,
    /// Worth knowing, but not blocking, e.g. a different plugin version.
    pub warnings: Vec<String>,
}

impl From<SceneCheck> for SceneCheckResponse {
    fn from(check: SceneCheck) -> Self {
        Self {
            launchable: check.problems.is_empty(),
            problems: check.problems,
            warnings: check.warnings,
        }
    }
}

#[derive(Serialize, Deserialize, ToSchema)]
pub(crate) struct ImportResponse {
    /// The new scene (new ids; disabled).
    pub scene: Scene,
    #[serde(flatten)]
    pub check: SceneCheckResponse,
}

#[derive(Deserialize, IntoParams)]
pub(crate) struct ExportParams {
    /// Include remote nodes' pre-shared tokens. Off by default; a file with
    /// secrets must be handled like one.
    pub include_secrets: Option<bool>,
}

/// Multipart form for imports.
#[derive(ToSchema)]
#[allow(dead_code)] // documentation only
pub(crate) struct SceneUpload {
    /// A `.flwr` file from `GET …/export`.
    #[schema(value_type = String, format = Binary)]
    file: Vec<u8>,
}

#[utoipa::path(
    get,
    path = "/by-id/{id}/export",
    operation_id = "export-scene",
    summary = "Export a scene with its nodes and edges as a .flwr file",
    description = "The file can be imported on another flwrs server. Remote nodes carry their `remote_id`; their tokens only with `include_secrets=true`.",
    params(("id" = String, Path, description = "Scene ID"), ExportParams),
    responses(
        (status = 200, description = "The .flwr file", content_type = "application/vnd.flwrs.scene", body = Vec<u8>),
        (status = 404, description = "Not found", body = ErrorBody),
    )
)]
pub(super) async fn export_scene(
    State(service): State<Svc>,
    Path(id): Path<String>,
    Query(params): Query<ExportParams>,
) -> Result<Response, ApiError> {
    let export = service
        .export_scene(&id, params.include_secrets.unwrap_or(false))
        .await?;
    Ok((
        [
            (header::CONTENT_TYPE, MEDIA_TYPE.to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{}\"", export.file_name),
            ),
        ],
        export.bytes,
    )
        .into_response())
}

#[utoipa::path(
    post,
    path = "/import",
    operation_id = "import-scene",
    summary = "Import a scene from a .flwr file",
    description = "Creates a new, disabled scene with new ids. The file's structure must be valid; plugins this server lacks don't block the import, but are listed as problems until they're installed.",
    request_body(content = SceneUpload, content_type = "multipart/form-data"),
    responses(
        (status = 201, description = "Imported", body = ImportResponse),
        (status = 400, description = "No `file` field", body = ErrorBody),
        (status = 413, description = "File too large"),
        (status = 422, description = "Not a valid scene file; details list every problem", body = ErrorBody),
    )
)]
pub(super) async fn import_scene(
    State(service): State<Svc>,
    mut multipart: Multipart,
) -> Result<(StatusCode, Json<ImportResponse>), ApiError> {
    let bad =
        |e: axum::extract::multipart::MultipartError| api_error(e.status(), e.body_text(), vec![]);
    while let Some(mut field) = multipart.next_field().await.map_err(bad)? {
        if field.name() != Some("file") {
            continue;
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = field.chunk().await.map_err(bad)? {
            if bytes.len() + chunk.len() > MAX_FILE_BYTES {
                return Err(api_error(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    format!("scene files are limited to {MAX_FILE_BYTES} bytes"),
                    vec![],
                ));
            }
            bytes.extend_from_slice(&chunk);
        }
        let imported = service.import_scene(&bytes).await?;
        return Ok((
            StatusCode::CREATED,
            Json(ImportResponse {
                scene: imported.scene.into(),
                check: imported.check.into(),
            }),
        ));
    }
    Err(api_error(
        StatusCode::BAD_REQUEST,
        "missing multipart field 'file' with the .flwr file",
        vec![],
    ))
}

#[utoipa::path(
    get,
    path = "/by-id/{id}/check",
    operation_id = "check-scene",
    summary = "Whether the scene can run on this server, and why not",
    params(("id" = String, Path, description = "Scene ID")),
    responses(
        (status = 200, description = "Check result", body = SceneCheckResponse),
        (status = 404, description = "Not found", body = ErrorBody),
    )
)]
pub(super) async fn check_scene(
    State(service): State<Svc>,
    Path(id): Path<String>,
) -> Result<Json<SceneCheckResponse>, ApiError> {
    Ok(Json(service.check(&id).await?.into()))
}

pub(super) fn routes() -> Router<Svc> {
    Router::new()
        .route("/scenes/by-id/{id}/export", get(export_scene))
        .route("/scenes/by-id/{id}/check", get(check_scene))
        .route(
            "/scenes/import",
            // Room for the multipart framing around the file.
            post(import_scene).layer(DefaultBodyLimit::max(MAX_FILE_BYTES + 64 * 1024)),
        )
}

#[cfg(test)]
mod tests {
    use crate::modules::scene::api::Api;
    use crate::modules::scene::service::tests::fixture;
    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use http_body_util::BodyExt;
    use serde_json::{Value, json};
    use tower::ServiceExt;

    async fn send(
        service: super::Svc,
        req: Request<Body>,
    ) -> (StatusCode, header::HeaderMap, Vec<u8>) {
        let response = Api::build_router(service).oneshot(req).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let body = response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec();
        (status, headers, body)
    }

    fn json_request(method: &str, uri: &str, body: Value) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(uri)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    fn get(uri: &str) -> Request<Body> {
        Request::builder().uri(uri).body(Body::empty()).unwrap()
    }

    fn upload(file: &[u8]) -> Request<Body> {
        let boundary = "flwrs-test-boundary";
        let mut body = format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"scene.flwr\"\r\nContent-Type: application/octet-stream\r\n\r\n"
        )
        .into_bytes();
        body.extend_from_slice(file);
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
        Request::builder()
            .method("POST")
            .uri("/scenes/import")
            .header(
                header::CONTENT_TYPE,
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(Body::from(body))
            .unwrap()
    }

    fn json_of(body: &[u8]) -> Value {
        serde_json::from_slice(body).unwrap()
    }

    #[tokio::test]
    async fn export_import_and_check_over_http() {
        let f = fixture().await;
        let svc = f.service;
        let (_, _, scene) = send(
            svc,
            json_request("POST", "/scenes", json!({"name": "Line 3", "max_hops": 40})),
        )
        .await;
        let sid = json_of(&scene)["id"].as_str().unwrap().to_string();
        let (status, _, _) = send(
            svc,
            json_request(
                "POST",
                &format!("/scenes/by-id/{sid}/nodes"),
                json!({"plugin_id": "test-sink", "config": {"launch": "remote", "remote_id": "line-3-out", "token": "a-long-enough-shared-secret"}}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);

        let (status, headers, check) = send(svc, get(&format!("/scenes/by-id/{sid}/check"))).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            json_of(&check),
            json!({"launchable": true, "problems": [], "warnings": []})
        );

        let (status, headers_export, file) =
            send(svc, get(&format!("/scenes/by-id/{sid}/export"))).await;
        assert_eq!(status, StatusCode::OK, "{headers:?}");
        assert_eq!(
            headers_export[header::CONTENT_TYPE],
            "application/vnd.flwrs.scene"
        );
        assert_eq!(
            headers_export[header::CONTENT_DISPOSITION],
            "attachment; filename=\"Line_3.flwr\""
        );
        assert_eq!(&file[..4], b"FLWR");

        let (status, _, body) = send(svc, upload(&file)).await;
        assert_eq!(
            status,
            StatusCode::CREATED,
            "{}",
            String::from_utf8_lossy(&body)
        );
        let imported = json_of(&body);
        assert_eq!(imported["scene"]["name"], "Line 3");
        assert_eq!(imported["scene"]["max_hops"], 40);
        assert_eq!(imported["scene"]["enabled"], false);
        assert_ne!(imported["scene"]["id"], json!(sid));
        assert_eq!(imported["launchable"], false, "the token wasn't exported");
        assert!(
            imported["problems"][0]
                .as_str()
                .unwrap()
                .contains("config.token is required")
        );

        let new_id = imported["scene"]["id"].as_str().unwrap();
        let (_, _, nodes) = send(svc, get(&format!("/scenes/by-id/{new_id}/nodes"))).await;
        let nodes = json_of(&nodes);
        assert_eq!(nodes[0]["config"]["remote_id"], "line-3-out");
        assert_eq!(nodes[0]["has_token"], false);
        assert_eq!(nodes[0]["plugin_version"], "1.0.0");

        let (status, _, err) = send(svc, upload(b"not a scene")).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(
            json_of(&err)["details"][0]
                .as_str()
                .unwrap()
                .contains("not a flwrs scene file")
        );
        let (status, _, _) = send(svc, get("/scenes/by-id/nope/export")).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
}
