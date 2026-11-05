use super::service::{Plugin, Service, ServiceError};
use axum::extract::{ConnectInfo, DefaultBodyLimit, Multipart, Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;
use std::net::SocketAddr;
use std::path::PathBuf;
use tokio::io::AsyncWriteExt;
use utoipa::{OpenApi, ToSchema};

type Svc = &'static Service;

#[derive(Serialize, ToSchema)]
pub(crate) struct PluginInfo {
    pub id: String,
    pub name: String,
    pub version: String,
    /// SOURCE, SINK or TRANSFORM.
    #[serde(rename = "type")]
    pub plugin_type: String,
    pub description: Option<String>,
    /// `<os>-<arch>` targets, or absent if the package runs anywhere.
    pub platforms: Option<Vec<String>>,
    /// flags, positional or json.
    pub launch_args_style: String,
    pub has_launch_args_schema: bool,
    /// Payload schema the plugin accepts (SINK, TRANSFORM), as `SchemaDefinition` JSON.
    #[schema(value_type = Option<Object>)]
    pub in_schema: Option<serde_json::Value>,
    /// Payload schema the plugin emits (SOURCE, TRANSFORM), as `SchemaDefinition` JSON.
    #[schema(value_type = Option<Object>)]
    pub out_schema: Option<serde_json::Value>,
    /// Whether the package's files are present on disk.
    pub installed: bool,
    pub archive_sha256: String,
    pub create_time: i64,
    pub update_time: i64,
}

impl PluginInfo {
    fn new(plugin: Plugin, service: Svc) -> Self {
        let installed = service.is_installed(&plugin);
        let manifest = plugin.parsed_manifest();
        let json = |s: &Option<String>| s.as_deref().and_then(|s| serde_json::from_str(s).ok());
        Self {
            installed,
            platforms: manifest.as_ref().and_then(|m| m.platforms.clone()),
            launch_args_style: manifest
                .as_ref()
                .map(|m| serde_json::to_value(m.launch_args_style).ok())
                .and_then(|v| v.and_then(|v| v.as_str().map(str::to_string)))
                .unwrap_or_else(|| "flags".into()),
            has_launch_args_schema: plugin.launch_args_schema.is_some(),
            in_schema: json(&plugin.in_schema),
            out_schema: json(&plugin.out_schema),
            id: plugin.id,
            name: plugin.name,
            version: plugin.version,
            plugin_type: plugin.plugin_type,
            description: plugin.description,
            archive_sha256: plugin.archive_sha256,
            create_time: plugin.create_time.timestamp_millis(),
            update_time: plugin.update_time.timestamp_millis(),
        }
    }
}

/// Error response with a list of details (e.g. every problem in a package).
#[derive(Serialize, ToSchema)]
pub(crate) struct ErrorBody {
    pub error: String,
    pub details: Vec<String>,
}

type ApiError = (StatusCode, Json<ErrorBody>);

fn api_error(status: StatusCode, error: impl Into<String>, details: Vec<String>) -> ApiError {
    (
        status,
        Json(ErrorBody {
            error: error.into(),
            details,
        }),
    )
}

impl From<ServiceError> for ApiError {
    fn from(e: ServiceError) -> Self {
        let message = e.to_string();
        match e {
            ServiceError::NotFound => api_error(StatusCode::NOT_FOUND, message, vec![]),
            ServiceError::AlreadyExists(id) => api_error(
                StatusCode::CONFLICT,
                message,
                vec![format!("use PUT /api/plugins/by-id/{id} to upgrade it")],
            ),
            ServiceError::InvalidPackage(details) => {
                api_error(StatusCode::UNPROCESSABLE_ENTITY, message, details)
            }
            ServiceError::InUse(nodes) => api_error(StatusCode::CONFLICT, message, nodes),
            ServiceError::IncompatibleNodes(problems) => {
                api_error(StatusCode::CONFLICT, message, problems)
            }
            ServiceError::Io(_)
            | ServiceError::Query(_)
            | ServiceError::Connection(_)
            | ServiceError::Internal(_) => {
                log::error!("Plugins API: {message}");
                api_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal server error",
                    vec![],
                )
            }
        }
    }
}

/// Mutating endpoints run uploaded code eventually, so they're gated:
/// `[plugins] allow_upload` must be on, and the caller must present the admin
/// token (if one is configured) or connect from a loopback address.
fn authorize(service: Svc, headers: &HeaderMap, peer: SocketAddr) -> Result<(), ApiError> {
    let config = service.config();
    if !config.allow_upload {
        return Err(api_error(
            StatusCode::FORBIDDEN,
            "plugin changes are disabled",
            vec!["set [plugins] allow_upload = true in the server config".into()],
        ));
    }
    match &config.admin_token {
        Some(token) => {
            let presented = headers
                .get(header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("Bearer "));
            if presented.is_some_and(|p| constant_time_eq(p.as_bytes(), token.as_bytes())) {
                Ok(())
            } else {
                Err(api_error(
                    StatusCode::UNAUTHORIZED,
                    "missing or wrong admin token",
                    vec![],
                ))
            }
        }
        None if peer.ip().is_loopback() => Ok(()),
        None => Err(api_error(
            StatusCode::FORBIDDEN,
            "plugin changes are only allowed from loopback without an admin token",
            vec!["set [plugins] admin_token to allow remote clients".into()],
        )),
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Removes a saved upload if the handler returns early.
struct SavedUpload(PathBuf);

impl Drop for SavedUpload {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Streams the `file` field of a multipart upload to a staging file.
async fn save_upload(service: Svc, mut multipart: Multipart) -> Result<SavedUpload, ApiError> {
    let bad =
        |e: axum::extract::multipart::MultipartError| api_error(e.status(), e.body_text(), vec![]);
    while let Some(mut field) = multipart.next_field().await.map_err(bad)? {
        if field.name() != Some("file") {
            continue;
        }
        let saved = SavedUpload(service.new_upload_path());
        let mut out = tokio::fs::File::create(&saved.0)
            .await
            .map_err(|e| ApiError::from(ServiceError::Io(e)))?;
        while let Some(chunk) = field.chunk().await.map_err(bad)? {
            out.write_all(&chunk)
                .await
                .map_err(|e| ApiError::from(ServiceError::Io(e)))?;
        }
        out.flush()
            .await
            .map_err(|e| ApiError::from(ServiceError::Io(e)))?;
        return Ok(saved);
    }
    Err(api_error(
        StatusCode::BAD_REQUEST,
        "missing multipart field 'file' with the plugin ZIP",
        vec![],
    ))
}

/// Multipart form for uploads.
#[derive(ToSchema)]
#[allow(dead_code)] // documentation only
pub(crate) struct PluginUpload {
    /// The plugin package (ZIP with manifest.json at its root).
    #[schema(value_type = String, format = Binary)]
    file: Vec<u8>,
}

#[utoipa::path(
    get,
    path = "",
    operation_id = "list-plugins",
    summary = "List registered plugins",
    responses((status = 200, description = "Registered plugins", body = [PluginInfo]))
)]
async fn list_plugins(State(service): State<Svc>) -> Result<Json<Vec<PluginInfo>>, ApiError> {
    let plugins = service.list().await?;
    Ok(Json(
        plugins
            .into_iter()
            .map(|p| PluginInfo::new(p, service))
            .collect(),
    ))
}

#[utoipa::path(
    get,
    path = "/by-id/{id}",
    operation_id = "get-plugin",
    summary = "Get a registered plugin",
    params(("id" = String, Path, description = "Plugin id")),
    responses(
        (status = 200, description = "Plugin", body = PluginInfo),
        (status = 404, description = "Not found", body = ErrorBody),
    )
)]
async fn get_plugin(
    State(service): State<Svc>,
    Path(id): Path<String>,
) -> Result<Json<PluginInfo>, ApiError> {
    Ok(Json(PluginInfo::new(service.get(&id).await?, service)))
}

#[utoipa::path(
    get,
    path = "/by-id/{id}/launch-args-schema",
    operation_id = "get-plugin-launch-args-schema",
    summary = "Get the JSON Schema of a plugin's launch_args",
    params(("id" = String, Path, description = "Plugin id")),
    responses(
        (status = 200, description = "JSON Schema", body = Object),
        (status = 404, description = "Plugin not found, or it has no launch_args schema", body = ErrorBody),
    )
)]
async fn get_launch_args_schema(
    State(service): State<Svc>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let plugin = service.get(&id).await?;
    let schema = plugin
        .launch_args_schema
        .and_then(|s| serde_json::from_str(&s).ok())
        .ok_or_else(|| {
            api_error(
                StatusCode::NOT_FOUND,
                "plugin has no launch_args schema",
                vec![],
            )
        })?;
    Ok(Json(schema))
}

#[utoipa::path(
    post,
    path = "",
    operation_id = "upload-plugin",
    summary = "Upload and register a plugin package",
    description = "Requires `[plugins] allow_upload = true`, and the admin token (if configured) or a loopback client.",
    request_body(content = PluginUpload, content_type = "multipart/form-data"),
    responses(
        (status = 201, description = "Registered", body = PluginInfo),
        (status = 400, description = "Bad request", body = ErrorBody),
        (status = 401, description = "Missing or wrong admin token", body = ErrorBody),
        (status = 403, description = "Uploads disabled, or non-loopback client without a token", body = ErrorBody),
        (status = 409, description = "Plugin id already registered", body = ErrorBody),
        (status = 413, description = "Upload too large"),
        (status = 422, description = "Invalid package; details list every problem", body = ErrorBody),
    )
)]
async fn upload_plugin(
    State(service): State<Svc>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    multipart: Multipart,
) -> Result<(StatusCode, Json<PluginInfo>), ApiError> {
    authorize(service, &headers, peer)?;
    let upload = save_upload(service, multipart).await?;
    let plugin = service.install(&upload.0).await?;
    Ok((StatusCode::CREATED, Json(PluginInfo::new(plugin, service))))
}

#[utoipa::path(
    put,
    path = "/by-id/{id}",
    operation_id = "upgrade-plugin",
    summary = "Upgrade a plugin with a new package",
    description = "The package must have the same id and type. Rejected if any scene node's launch_args would become invalid.",
    params(("id" = String, Path, description = "Plugin id")),
    request_body(content = PluginUpload, content_type = "multipart/form-data"),
    responses(
        (status = 200, description = "Upgraded", body = PluginInfo),
        (status = 401, description = "Missing or wrong admin token", body = ErrorBody),
        (status = 403, description = "Changes disabled, or non-loopback client without a token", body = ErrorBody),
        (status = 404, description = "Not found", body = ErrorBody),
        (status = 409, description = "Scene nodes would become invalid; details list them", body = ErrorBody),
        (status = 413, description = "Upload too large"),
        (status = 422, description = "Invalid package", body = ErrorBody),
    )
)]
async fn upgrade_plugin(
    State(service): State<Svc>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Path(id): Path<String>,
    headers: HeaderMap,
    multipart: Multipart,
) -> Result<Json<PluginInfo>, ApiError> {
    authorize(service, &headers, peer)?;
    let upload = save_upload(service, multipart).await?;
    let plugin = service.upgrade(&id, &upload.0).await?;
    Ok(Json(PluginInfo::new(plugin, service)))
}

#[utoipa::path(
    delete,
    path = "/by-id/{id}",
    operation_id = "delete-plugin",
    summary = "Deregister a plugin and remove its files",
    params(("id" = String, Path, description = "Plugin id")),
    responses(
        (status = 204, description = "Removed"),
        (status = 401, description = "Missing or wrong admin token", body = ErrorBody),
        (status = 403, description = "Changes disabled, or non-loopback client without a token", body = ErrorBody),
        (status = 404, description = "Not found", body = ErrorBody),
        (status = 409, description = "Used by scene nodes; details list them", body = ErrorBody),
    )
)]
async fn delete_plugin(
    State(service): State<Svc>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<StatusCode, ApiError> {
    authorize(service, &headers, peer)?;
    service.delete(&id).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(OpenApi)]
#[openapi(
    info(title = "Plugins", description = "Plugin registry API"),
    paths(
        list_plugins,
        get_plugin,
        get_launch_args_schema,
        upload_plugin,
        upgrade_plugin,
        delete_plugin
    ),
    components(schemas(PluginInfo, ErrorBody, PluginUpload))
)]
pub(crate) struct Api;

impl Api {
    pub(crate) fn build_router(service: Svc) -> Router {
        let limit = usize::try_from(service.config().max_upload_bytes).unwrap_or(usize::MAX);
        Router::new()
            .route("/plugins", get(list_plugins).post(upload_plugin))
            .route(
                "/plugins/by-id/{id}",
                get(get_plugin).put(upgrade_plugin).delete(delete_plugin),
            )
            .route(
                "/plugins/by-id/{id}/launch-args-schema",
                get(get_launch_args_schema),
            )
            .layer(DefaultBodyLimit::max(limit))
            .with_state(service)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::plugin::package::tests::valid_package;
    use crate::modules::plugin::service::tests::{Fixture, fixture};
    use axum::body::Body;
    use axum::extract::connect_info::MockConnectInfo;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    const BOUNDARY: &str = "flwrs-test-boundary";

    fn multipart_body(zip: &[u8]) -> Vec<u8> {
        let mut body = format!(
            "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"p.zip\"\r\n\
             Content-Type: application/zip\r\n\r\n"
        )
        .into_bytes();
        body.extend_from_slice(zip);
        body.extend_from_slice(format!("\r\n--{BOUNDARY}--\r\n").as_bytes());
        body
    }

    fn package_bytes(f: &Fixture, id: &str, version: &str) -> Vec<u8> {
        let path = f.dir.path().join(format!("{id}-{version}.zip"));
        valid_package(&path, id, version);
        std::fs::read(path).unwrap()
    }

    async fn call(
        f: &Fixture,
        peer: &str,
        request: Request<Body>,
    ) -> (StatusCode, serde_json::Value) {
        let addr: SocketAddr = peer.parse().unwrap();
        let app = Api::build_router(f.service).layer(MockConnectInfo(addr));
        let response = app.oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
        )
    }

    fn upload_request(
        method: &str,
        uri: &str,
        body: Vec<u8>,
        token: Option<&str>,
    ) -> Request<Body> {
        let mut builder = Request::builder().method(method).uri(uri).header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={BOUNDARY}"),
        );
        if let Some(t) = token {
            builder = builder.header(header::AUTHORIZATION, format!("Bearer {t}"));
        }
        builder.body(Body::from(body)).unwrap()
    }

    const LOCAL: &str = "127.0.0.1:5000";
    const REMOTE: &str = "203.0.113.7:5000";

    #[tokio::test]
    async fn uploads_are_disabled_by_default() {
        let f = fixture(|_| {}).await;
        let body = multipart_body(&package_bytes(&f, "test-sink", "1.0.0"));
        let (status, json) = call(&f, LOCAL, upload_request("POST", "/plugins", body, None)).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert!(
            json["details"][0]
                .as_str()
                .unwrap()
                .contains("allow_upload")
        );
    }

    #[tokio::test]
    async fn loopback_only_without_token() {
        let f = fixture(|c| c.allow_upload = true).await;
        let bytes = package_bytes(&f, "test-sink", "1.0.0");
        let (status, _) = call(
            &f,
            REMOTE,
            upload_request("POST", "/plugins", multipart_body(&bytes), None),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (status, json) = call(
            &f,
            LOCAL,
            upload_request("POST", "/plugins", multipart_body(&bytes), None),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{json}");
        assert_eq!(json["id"], "test-sink");
        assert_eq!(json["installed"], true);
        assert_eq!(json["in_schema"]["fields"][0]["key"], "url");
    }

    #[tokio::test]
    async fn admin_token_is_required_when_configured() {
        let f = fixture(|c| {
            c.allow_upload = true;
            c.admin_token = Some("s3cret".into());
        })
        .await;
        let bytes = package_bytes(&f, "test-sink", "1.0.0");
        for token in [None, Some("wrong")] {
            let (status, _) = call(
                &f,
                LOCAL,
                upload_request("POST", "/plugins", multipart_body(&bytes), token),
            )
            .await;
            assert_eq!(status, StatusCode::UNAUTHORIZED);
        }
        let (status, _) = call(
            &f,
            REMOTE,
            upload_request("POST", "/plugins", multipart_body(&bytes), Some("s3cret")),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
    }

    #[tokio::test]
    async fn full_lifecycle_over_http() {
        let f = fixture(|c| c.allow_upload = true).await;
        let v1 = package_bytes(&f, "test-sink", "1.0.0");
        let (status, _) = call(
            &f,
            LOCAL,
            upload_request("POST", "/plugins", multipart_body(&v1), None),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        let (status, json) = call(
            &f,
            LOCAL,
            upload_request("POST", "/plugins", multipart_body(&v1), None),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{json}");

        let get = |uri: &str| Request::builder().uri(uri).body(Body::empty()).unwrap();
        let (status, json) = call(&f, REMOTE, get("/plugins")).await;
        assert_eq!(
            (status, json.as_array().unwrap().len()),
            (StatusCode::OK, 1)
        );
        let (status, json) = call(
            &f,
            REMOTE,
            get("/plugins/by-id/test-sink/launch-args-schema"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["properties"]["timeout"]["type"], "integer");
        let (status, _) = call(&f, REMOTE, get("/plugins/by-id/nope")).await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        let v2 = package_bytes(&f, "test-sink", "1.1.0");
        let (status, json) = call(
            &f,
            LOCAL,
            upload_request("PUT", "/plugins/by-id/test-sink", multipart_body(&v2), None),
        )
        .await;
        assert_eq!(
            (status, json["version"].as_str()),
            (StatusCode::OK, Some("1.1.0"))
        );

        let delete = Request::builder()
            .method("DELETE")
            .uri("/plugins/by-id/test-sink")
            .body(Body::empty())
            .unwrap();
        let (status, _) = call(&f, LOCAL, delete).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn invalid_package_lists_problems() {
        let f = fixture(|c| c.allow_upload = true).await;
        let (status, json) = call(
            &f,
            LOCAL,
            upload_request("POST", "/plugins", multipart_body(b"not a zip"), None),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(
            json["details"][0]
                .as_str()
                .unwrap()
                .contains("not a valid ZIP")
        );

        let empty = format!("--{BOUNDARY}--\r\n").into_bytes();
        let (status, json) = call(&f, LOCAL, upload_request("POST", "/plugins", empty, None)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{json}");
    }

    #[tokio::test]
    async fn oversized_upload_is_rejected() {
        let f = fixture(|c| {
            c.allow_upload = true;
            c.max_upload_bytes = 1024;
        })
        .await;
        let body = multipart_body(&vec![7u8; 4096]);
        let (status, _) = call(&f, LOCAL, upload_request("POST", "/plugins", body, None)).await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(
            std::fs::read_dir(f.dir.path().join("plugins/.staging"))
                .unwrap()
                .count(),
            0
        );
    }
}
