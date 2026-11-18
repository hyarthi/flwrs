use crate::modules::api_error::{ApiError, api_error};
use crate::modules::auth::api::SecuritySchemes;
use crate::modules::{auth, director, plugin, scene, status_api};
use axum::Router;
use axum::http::StatusCode;
use flwrs_core::http::{HttpServer, ServerConfig};
use std::sync::LazyLock;
use tokio_util::sync::CancellationToken;
use utoipa::OpenApi;

static HTTP_SERVER: LazyLock<HttpServer> = LazyLock::new(|| {
    let ui = ServerConfig::read().swagger_ui();
    let mut server = HttpServer::new(vec![], Some(root(ui)));
    let stopping = server.stopping();
    server.merge_api(api(auth::get(), stopping));
    server
});

/// The management API (under `/api`): everything behind the auth guard, but
/// for the public auth routes (login, logout, providers).
fn api(auth: &'static auth::Auth, stopping: CancellationToken) -> Router {
    let stream = scene::api_stream::StreamState {
        scenes: scene::service(),
        stopping,
        interval: scene::config::ScenesConfig::read().stream_interval(),
    };
    let protected = Router::new()
        .merge(scene::api::Api::build_router(scene::service()))
        .merge(scene::api_stream::router(stream.clone()))
        .merge(scene::api_scene_stream::router(stream))
        .merge(plugin::api::Api::build_router(plugin::service()))
        .merge(status_api::Api::build_router(director::init()))
        .merge(auth::api::Api::protected_router())
        .fallback(api_not_found);
    auth.protect(protected)
        .merge(auth::api::Api::public_router(auth))
}

/// Unknown API paths answer with the API's error shape (not the web UI).
async fn api_not_found() -> ApiError {
    api_error(StatusCode::NOT_FOUND, "no such API endpoint", vec![])
}

/// What's served outside `/api`: the API docs, and the web UI (feature
/// `web-ui`) for every other path.
fn root(swagger_ui: bool) -> Router {
    let router = docs(swagger_ui);
    #[cfg(feature = "web-ui")]
    let router = router.merge(crate::modules::web_ui::router());
    router
}

/// The OpenAPI document at `/api-docs/openapi.json`, and Swagger UI at
/// `/swagger-ui` when `[http] swagger_ui` is on and the build has it
/// (feature `swagger-ui`).
fn docs(ui: bool) -> Router {
    #[cfg(feature = "swagger-ui")]
    if ui {
        return Router::new().merge(
            utoipa_swagger_ui::SwaggerUi::new("/swagger-ui")
                .url("/api-docs/openapi.json", OpenApiSpec::openapi()),
        );
    }
    #[cfg(not(feature = "swagger-ui"))]
    if ui {
        log::info!(
            "[http] swagger_ui: this build has no Swagger UI (feature swagger-ui); the OpenAPI document is still served"
        );
    }
    let spec = OpenApiSpec::openapi();
    Router::new().route(
        "/api-docs/openapi.json",
        axum::routing::get(move || {
            let spec = spec.clone();
            async move { axum::Json(spec) }
        }),
    )
}

pub fn server() -> &'static HttpServer {
    &HTTP_SERVER
}

#[derive(OpenApi)]
#[openapi(
    modifiers(&SecuritySchemes),
    nest(
        (path = "/api/auth", api = auth::api::Api),
        (path = "/api/scenes", api = scene::api::Api),
        (path = "/api/scenes", api = scene::api_stream::Api),
        (path = "/api/scenes", api = scene::api_scene_stream::Api),
        (path = "/api/scenes", api = scene::api_draft::Api),
        (path = "/api/plugins", api = plugin::api::Api),
        (path = "/api/status", api = status_api::Api),
    )
)]
struct OpenApiSpec;
