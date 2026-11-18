//! REST for files uploaded to a node (`…/nodes/by-id/{node_id}/uploads`),
//! and for emptying a node's folder (`…/workdir`).

use crate::modules::api_error::{ApiError, ErrorBody, api_error};
use crate::modules::scene::api::Svc;
use crate::modules::scene::service::NodeUpload;
use axum::Json;
use axum::Router;
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get};
use futures_util::StreamExt;
use serde::Serialize;
use tokio::io::AsyncWriteExt;
use utoipa::ToSchema;

/// A file uploaded to a node.
#[derive(Serialize, ToSchema)]
pub(crate) struct Upload {
    /// What the node's launch args call it (`"source": "upload"` fields).
    pub name: String,
    /// Hex SHA-256 of the content.
    pub sha256: String,
    pub size: i64,
    pub create_time: i64,
}

impl From<NodeUpload> for Upload {
    fn from(u: NodeUpload) -> Self {
        Self {
            name: u.name,
            sha256: u.sha256,
            size: u.size,
            create_time: u.create_time.timestamp_millis(),
        }
    }
}

const NODE: &str = "/scenes/by-id/{id}/nodes/by-id/{node_id}";

#[utoipa::path(
    put,
    path = "/by-id/{id}/nodes/by-id/{node_id}/uploads/{name}",
    operation_id = "put-node-upload",
    summary = "Upload a file to a node (or replace it)",
    description = "The raw body is the file. Name it in a `\"source\": \"upload\"` launch arg; the node gets it in its `config/` folder when it launches (a running node restarts). Names: letters, digits, '.', '_' and '-', 1 to 100 characters, not starting with '.'. Size limit: `[scenes] max_upload_bytes`.",
    params(
        ("id" = String, Path, description = "Scene ID"),
        ("node_id" = String, Path, description = "Node ID"),
        ("name" = String, Path, description = "File name"),
    ),
    request_body(content = Vec<u8>, content_type = "application/octet-stream"),
    responses(
        (status = 201, description = "Uploaded", body = Upload),
        (status = 200, description = "Replaced", body = Upload),
        (status = 404, description = "Scene or node not found", body = ErrorBody),
        (status = 413, description = "Too large", body = ErrorBody),
        (status = 422, description = "Invalid name", body = ErrorBody),
    )
)]
pub(super) async fn put_upload(
    State(service): State<Svc>,
    Path((id, node_id, name)): Path<(String, String, String)>,
    body: Body,
) -> Result<(StatusCode, Json<Upload>), ApiError> {
    let path = receive(service, body).await?;
    let (upload, created) = service.put_upload(&id, &node_id, &name, &path).await?;
    let status = if created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    Ok((status, Json(upload.into())))
}

/// Streams a request body to a fresh incoming file under
/// `[scenes] max_upload_bytes`, for [`Service::put_upload_in`].
pub(super) async fn receive(service: Svc, body: Body) -> Result<std::path::PathBuf, ApiError> {
    let path = service.incoming_path().await?;
    let limit = service.files().max_upload_bytes;
    let written: Result<(), ApiError> = async {
        let failed = |e: std::io::Error| {
            log::error!("Scenes API: storing an upload: {e}");
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal server error",
                vec![],
            )
        };
        let mut out = tokio::fs::File::create(&path).await.map_err(failed)?;
        let mut total: u64 = 0;
        let mut stream = body.into_data_stream();
        while let Some(chunk) = stream.next().await {
            let chunk =
                chunk.map_err(|e| api_error(StatusCode::BAD_REQUEST, e.to_string(), vec![]))?;
            total += chunk.len() as u64;
            if total > limit {
                return Err(api_error(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    format!(
                        "node uploads are limited to {limit} bytes ([scenes] max_upload_bytes)"
                    ),
                    vec![],
                ));
            }
            out.write_all(&chunk).await.map_err(failed)?;
        }
        out.flush().await.map_err(failed)?;
        Ok(())
    }
    .await;
    if let Err(e) = written {
        let _ = tokio::fs::remove_file(&path).await;
        return Err(e);
    }
    Ok(path)
}

#[utoipa::path(
    get,
    path = "/by-id/{id}/nodes/by-id/{node_id}/uploads",
    operation_id = "list-node-uploads",
    summary = "List the files uploaded to a node",
    params(
        ("id" = String, Path, description = "Scene ID"),
        ("node_id" = String, Path, description = "Node ID"),
    ),
    responses(
        (status = 200, description = "Uploads", body = [Upload]),
        (status = 404, description = "Not found", body = ErrorBody),
    )
)]
pub(super) async fn list_uploads(
    State(service): State<Svc>,
    Path((id, node_id)): Path<(String, String)>,
) -> Result<Json<Vec<Upload>>, ApiError> {
    let uploads = service.list_uploads(&id, &node_id).await?;
    Ok(Json(uploads.into_iter().map(Upload::from).collect()))
}

#[utoipa::path(
    get,
    path = "/by-id/{id}/nodes/by-id/{node_id}/uploads/{name}",
    operation_id = "get-node-upload",
    summary = "Get an uploaded file's details",
    params(
        ("id" = String, Path, description = "Scene ID"),
        ("node_id" = String, Path, description = "Node ID"),
        ("name" = String, Path, description = "File name"),
    ),
    responses(
        (status = 200, description = "Upload", body = Upload),
        (status = 404, description = "Not found", body = ErrorBody),
    )
)]
pub(super) async fn get_upload(
    State(service): State<Svc>,
    Path((id, node_id, name)): Path<(String, String, String)>,
) -> Result<Json<Upload>, ApiError> {
    Ok(Json(service.get_upload(&id, &node_id, &name).await?.into()))
}

#[utoipa::path(
    get,
    path = "/by-id/{id}/nodes/by-id/{node_id}/uploads/{name}/content",
    operation_id = "get-node-upload-content",
    summary = "Download an uploaded file",
    description = "Refused for a file a secret launch arg names.",
    params(
        ("id" = String, Path, description = "Scene ID"),
        ("node_id" = String, Path, description = "Node ID"),
        ("name" = String, Path, description = "File name"),
    ),
    responses(
        (status = 200, description = "The file", content_type = "application/octet-stream", body = Vec<u8>),
        (status = 403, description = "A secret", body = ErrorBody),
        (status = 404, description = "Not found", body = ErrorBody),
    )
)]
pub(super) async fn get_upload_content(
    State(service): State<Svc>,
    Path((id, node_id, name)): Path<(String, String, String)>,
) -> Result<Response, ApiError> {
    let (upload, path) = service.upload_content(&id, &node_id, &name).await?;
    file_response(upload, &path).await
}

/// An upload's bytes as a download.
pub(super) async fn file_response(
    upload: NodeUpload,
    path: &std::path::Path,
) -> Result<Response, ApiError> {
    let file = tokio::fs::File::open(path).await.map_err(|e| {
        log::error!("Scenes API: upload {} is missing: {e}", path.display());
        api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal server error",
            vec![],
        )
    })?;
    Ok((
        [
            (header::CONTENT_TYPE, "application/octet-stream".to_string()),
            (header::CONTENT_LENGTH, upload.size.to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{}\"", upload.name),
            ),
        ],
        Body::from_stream(tokio_util::io::ReaderStream::new(file)),
    )
        .into_response())
}

#[utoipa::path(
    delete,
    path = "/by-id/{id}/nodes/by-id/{node_id}/uploads/{name}",
    operation_id = "delete-node-upload",
    summary = "Delete an uploaded file",
    params(
        ("id" = String, Path, description = "Scene ID"),
        ("node_id" = String, Path, description = "Node ID"),
        ("name" = String, Path, description = "File name"),
    ),
    responses(
        (status = 204, description = "Deleted"),
        (status = 404, description = "Not found", body = ErrorBody),
        (status = 409, description = "The node's launch args still name it", body = ErrorBody),
    )
)]
pub(super) async fn delete_upload(
    State(service): State<Svc>,
    Path((id, node_id, name)): Path<(String, String, String)>,
) -> Result<StatusCode, ApiError> {
    service.delete_upload(&id, &node_id, &name).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(
    delete,
    path = "/by-id/{id}/nodes/by-id/{node_id}/workdir",
    operation_id = "reset-node-workdir",
    summary = "Empty a node's folder",
    description = "Removes the plugin's own files in the node's folder (its state, caches), so it starts over on its next launch. Only while the scene is disabled.",
    params(
        ("id" = String, Path, description = "Scene ID"),
        ("node_id" = String, Path, description = "Node ID"),
    ),
    responses(
        (status = 204, description = "Emptied"),
        (status = 404, description = "Not found", body = ErrorBody),
        (status = 409, description = "The scene is enabled", body = ErrorBody),
    )
)]
pub(super) async fn reset_workdir(
    State(service): State<Svc>,
    Path((id, node_id)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    service.reset_workdir(&id, &node_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub(super) fn routes() -> Router<Svc> {
    Router::new()
        .route(&format!("{NODE}/uploads"), get(list_uploads))
        .route(
            &format!("{NODE}/uploads/{{name}}"),
            get(get_upload)
                .put(put_upload)
                .delete(delete_upload)
                // The service enforces its own limit while streaming.
                .layer(DefaultBodyLimit::disable()),
        )
        .route(
            &format!("{NODE}/uploads/{{name}}/content"),
            get(get_upload_content),
        )
        .route(&format!("{NODE}/workdir"), delete(reset_workdir))
}

#[cfg(test)]
mod tests {
    use crate::modules::scene::api::Api;
    use crate::modules::scene::service::tests::fixture;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use serde_json::{Value, json};
    use tower::ServiceExt;

    async fn call(
        service: super::Svc,
        method: &str,
        uri: &str,
        body: Body,
    ) -> (StatusCode, bytes::Bytes) {
        let req = Request::builder()
            .method(method)
            .uri(uri)
            .body(body)
            .unwrap();
        let response = Api::build_router(service).oneshot(req).await.unwrap();
        let status = response.status();
        (
            status,
            response.into_body().collect().await.unwrap().to_bytes(),
        )
    }

    fn json(bytes: &[u8]) -> Value {
        serde_json::from_slice(bytes).unwrap_or(Value::Null)
    }

    #[tokio::test]
    async fn uploads_over_http() {
        let f = fixture().await;
        let svc = f.service;
        let s = f.scene().await;
        let args = json!({"schemas": {"in": {"fields": [{"key": "msg", "type": "STRING"}]}},
                          "script": "return payload", "model": "model.onnx", "key": "k.pem"});
        let node = f
            .node(&s.id, "test-script", json!({"launch_args": args}))
            .await;
        let base = format!("/scenes/by-id/{}/nodes/by-id/{}", s.id, node.id);

        // not uploaded yet: a problem for the scene
        let check = f.service.check(&s.id).await.unwrap();
        assert!(
            check
                .problems
                .iter()
                .any(|p| p.contains("names the uploaded file \"model.onnx\"")),
            "{check:?}"
        );

        let (status, body) = call(
            svc,
            "PUT",
            &format!("{base}/uploads/model.onnx"),
            Body::from("weights"),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body:?}");
        let up = json(&body);
        assert_eq!(up["size"], 7);
        assert_eq!(up["sha256"].as_str().unwrap().len(), 64);
        let (status, _) = call(
            svc,
            "PUT",
            &format!("{base}/uploads/model.onnx"),
            Body::from("weights v2"),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "replaced");
        let (status, _) = call(
            svc,
            "PUT",
            &format!("{base}/uploads/k.pem"),
            Body::from("PRIVATE"),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        let (status, _) = call(
            svc,
            "PUT",
            &format!("{base}/uploads/.hidden"),
            Body::from("x"),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

        // stored once, by name and content; the replaced content is gone
        let stored: Vec<String> = std::fs::read_dir(&f.service.files().files_dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| !n.starts_with('.'))
            .collect();
        assert_eq!(stored.len(), 2, "{stored:?}");
        assert!(
            stored
                .iter()
                .any(|n| n.starts_with("model_") && n.ends_with(".onnx"))
        );
        // (the fixture's plugin has no files on disk: that problem stays)
        let check = f.service.check(&s.id).await.unwrap();
        assert!(
            !check.problems.iter().any(|p| p.contains("uploaded file")),
            "{check:?}"
        );

        let (_, body) = call(svc, "GET", &format!("{base}/uploads"), Body::empty()).await;
        assert_eq!(json(&body).as_array().unwrap().len(), 2);
        let (status, body) = call(
            svc,
            "GET",
            &format!("{base}/uploads/model.onnx/content"),
            Body::empty(),
        )
        .await;
        assert_eq!((status, &body[..]), (StatusCode::OK, &b"weights v2"[..]));
        // a secret field's file can't be downloaded
        let (status, _) = call(
            svc,
            "GET",
            &format!("{base}/uploads/k.pem/content"),
            Body::empty(),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        // named by the launch args: can't be deleted
        let (status, body) = call(
            svc,
            "DELETE",
            &format!("{base}/uploads/model.onnx"),
            Body::empty(),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body:?}");

        // deleting the node removes its uploads and their files
        f.service.delete_node(&s.id, &node.id).await.unwrap();
        let left = std::fs::read_dir(&f.service.files().files_dir)
            .unwrap()
            .filter(|e| {
                !e.as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with('.')
            })
            .count();
        assert_eq!(left, 0);
    }

    #[tokio::test]
    async fn uploads_have_a_size_limit_and_workdirs_reset_while_disabled() {
        let f = fixture().await;
        let s = f.scene().await;
        let node = f.node(&s.id, "test-sink", Value::Null).await;
        let base = format!("/scenes/by-id/{}/nodes/by-id/{}", s.id, node.id);
        let big = vec![0u8; usize::try_from(f.service.files().max_upload_bytes).unwrap() + 1];
        let (status, _) = call(
            f.service,
            "PUT",
            &format!("{base}/uploads/big.bin"),
            Body::from(big),
        )
        .await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
        // an upload no launch arg names can be deleted
        call(
            f.service,
            "PUT",
            &format!("{base}/uploads/a.txt"),
            Body::from("a"),
        )
        .await;
        let (status, _) = call(
            f.service,
            "DELETE",
            &format!("{base}/uploads/a.txt"),
            Body::empty(),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);

        let dir = f.service.files().nodes_dir.join(&s.id).join(&node.id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("state.db"), "s").unwrap();
        let (status, _) = call(
            f.service,
            "DELETE",
            &format!("{base}/workdir"),
            Body::empty(),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert!(!dir.exists());
        // deleting a node of a disabled scene removes its folder too
        std::fs::create_dir_all(&dir).unwrap();
        f.service.delete_node(&s.id, &node.id).await.unwrap();
        assert!(!dir.exists());
    }
}
