//! Error responses shared by the REST APIs: `{"error": "...", "details": [...]}`.

use axum::Json;
use axum::http::StatusCode;
use serde::Serialize;
use utoipa::ToSchema;

/// Error response. `details` lists every individual problem (e.g. each
/// validation failure), so clients can show them all at once.
#[derive(Serialize, ToSchema, Debug)]
pub(crate) struct ErrorBody {
    pub error: String,
    pub details: Vec<String>,
}

pub(crate) type ApiError = (StatusCode, Json<ErrorBody>);

pub(crate) fn api_error(
    status: StatusCode,
    error: impl Into<String>,
    details: Vec<String>,
) -> ApiError {
    (
        status,
        Json(ErrorBody {
            error: error.into(),
            details,
        }),
    )
}

/// A 500 whose cause is logged, not returned.
pub(crate) fn internal(context: &str, cause: impl std::fmt::Display) -> ApiError {
    log::error!("{context}: {cause}");
    api_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "internal server error",
        vec![],
    )
}
