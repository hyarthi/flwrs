use crate::modules::{director, plugin, scene, status_api};
use axum::Router;
use flwrs_core::http::HttpServer;
use std::sync::LazyLock;
use utoipa::OpenApi;
use utoipa_swagger_ui::SwaggerUi;

static HTTP_SERVER: LazyLock<HttpServer> = LazyLock::new(|| {
    HttpServer::new(
        vec![
            scene::api::Api::build_router(scene::service()),
            plugin::api::Api::build_router(plugin::service()),
            status_api::Api::build_router(director::init()),
        ],
        Some(Router::new().merge(
            SwaggerUi::new("/swagger-ui").url("/api-docs/openapi.json", OpenApiSpec::openapi()),
        )),
    )
});

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
