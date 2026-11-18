use crate::modules::{director, plugin, scene, status_api};
use axum::Router;
use flwrs_core::http::{HttpServer, ServerConfig};
use std::sync::LazyLock;
use utoipa::OpenApi;

static HTTP_SERVER: LazyLock<HttpServer> = LazyLock::new(|| {
    let ui = ServerConfig::read().swagger_ui();
    HttpServer::new(
        vec![
            scene::api::Api::build_router(scene::service()),
            plugin::api::Api::build_router(plugin::service()),
            status_api::Api::build_router(director::init()),
        ],
        Some(docs(ui)),
    )
});

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
    nest(
        (path = "/api/scenes", api = scene::api::Api),
        (path = "/api/plugins", api = plugin::api::Api),
        (path = "/api/status", api = status_api::Api),
    )
)]
struct OpenApiSpec;
