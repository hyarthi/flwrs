use crate::modules::scene;
use flwrs_core::http::HttpServer;
use axum::Router;
use lazy_static::lazy_static;
use std::sync::Arc;
use utoipa::OpenApi;
use utoipa_swagger_ui::SwaggerUi;

lazy_static! {
    static ref HTTP_SERVER: Arc<HttpServer> = Arc::new(HttpServer::new(
        vec![scene::api::Api::build_router(),],
        Some(Router::new().merge(
            SwaggerUi::new("/swagger-ui").url("/api-docs/openapi.json", OpenApiSpec::openapi())
        ))
    ));
}

pub fn server() -> &'static HttpServer {
    HTTP_SERVER.as_ref()
}

#[derive(OpenApi)]
#[openapi(
    nest(
        (path = "/api/scenes", api = scene::api::Api),
    )
)]
struct OpenApiSpec;
