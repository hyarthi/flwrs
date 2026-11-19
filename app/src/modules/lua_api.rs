//! `POST /api/lua/check`: a Lua code field's syntax, for editors (errors
//! marked as you type). The check saving runs (`flwrs_types::lua::check_syntax`:
//! Lua 5.4, the code as the body of `function(<params>)`); nothing runs.
//! Builds without Lua (`mapping-lua` off) answer `available: false`.

use crate::modules::api_error::{ApiError, ErrorBody, api_error};
use axum::extract::DefaultBodyLimit;
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use utoipa::{OpenApi, ToSchema};

/// The largest code checked (larger code fields aren't accepted anywhere).
const MAX_CODE_BYTES: usize = 1024 * 1024;

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
#[cfg_attr(
    not(feature = "mapping-lua"),
    allow(dead_code, reason = "checked with Lua only")
)]
pub(crate) struct LuaCheckRequest {
    pub code: String,
    /// The function's parameters (e.g. `["payload", "meta"]`).
    #[serde(default)]
    pub params: Vec<String>,
}

#[derive(Serialize, ToSchema, Debug, PartialEq)]
pub(crate) struct LuaCheckResponse {
    /// Whether this server can check Lua (built with it).
    pub available: bool,
    /// Lua stops at the first error: at most one.
    pub errors: Vec<LuaError>,
}

#[derive(Serialize, ToSchema, Debug, PartialEq)]
pub(crate) struct LuaError {
    /// 1-based, in the code as given; `null` if Lua didn't say.
    pub line: Option<u32>,
    pub message: String,
}

const LABEL: &str = "code";

#[cfg_attr(
    not(feature = "mapping-lua"),
    allow(dead_code, reason = "used with Lua only")
)]
fn parse_error(text: &str) -> LuaError {
    // `code:<line>: <message>`
    let rest = text.strip_prefix(LABEL).and_then(|r| r.strip_prefix(':'));
    if let Some((line, message)) = rest.and_then(|r| r.split_once(": "))
        && let Ok(line) = line.parse()
    {
        return LuaError {
            line: Some(line),
            message: message.to_string(),
        };
    }
    LuaError {
        line: None,
        message: text.to_string(),
    }
}

#[utoipa::path(
    post,
    path = "/check",
    operation_id = "check-lua",
    summary = "Check Lua code's syntax",
    description = "As saving a node's code field or a mapping checks it: Lua 5.4, the code as the body of `function(<params>)`. Nothing runs. At most one error (Lua stops at the first), with its line. `available: false` when the server is built without Lua.",
    request_body(content = LuaCheckRequest, content_type = "application/json"),
    responses(
        (status = 200, description = "The check", body = LuaCheckResponse),
        (status = 422, description = "Bad parameter names", body = ErrorBody),
    )
)]
async fn check(Json(req): Json<LuaCheckRequest>) -> Result<Json<LuaCheckResponse>, ApiError> {
    let ident = |p: &str| {
        p == "..."
            || (p.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
                && p.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
    };
    if let Some(bad) = req.params.iter().find(|p| !ident(p)) {
        return Err(api_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid parameter name",
            vec![format!("{bad:?} isn't a Lua name")],
        ));
    }
    Ok(Json(run(req).await))
}

#[cfg(feature = "mapping-lua")]
async fn run(req: LuaCheckRequest) -> LuaCheckResponse {
    let result = tokio::task::spawn_blocking(move || {
        flwrs_types::lua::check_syntax(LABEL, &req.code, &req.params)
    })
    .await
    .unwrap_or_else(|e| Err(format!("the check failed: {e}")));
    LuaCheckResponse {
        available: true,
        errors: result.err().map(|e| parse_error(&e)).into_iter().collect(),
    }
}

#[cfg(not(feature = "mapping-lua"))]
async fn run(_req: LuaCheckRequest) -> LuaCheckResponse {
    LuaCheckResponse {
        available: false,
        errors: vec![],
    }
}

#[derive(OpenApi)]
#[openapi(
    info(title = "Lua", description = "Checks for Lua code fields"),
    paths(check),
    components(schemas(LuaCheckRequest, LuaCheckResponse, LuaError, ErrorBody))
)]
pub(crate) struct Api;

pub(crate) fn router() -> Router {
    Router::new()
        .route("/lua/check", post(check))
        .layer(DefaultBodyLimit::max(MAX_CODE_BYTES + 4096))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, header};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    async fn call(body: serde_json::Value) -> (StatusCode, serde_json::Value) {
        let response = router()
            .oneshot(
                Request::post("/lua/check")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    #[tokio::test]
    async fn reports_the_first_syntax_error_with_its_line() {
        let (status, ok) =
            call(serde_json::json!({"code": "return payload", "params": ["payload"]})).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(ok["errors"], serde_json::json!([]));
        let (_, bad) =
            call(serde_json::json!({"code": "local x = 1\nreturn (", "params": []})).await;
        #[cfg(feature = "mapping-lua")]
        {
            assert_eq!(bad["available"], true);
            assert_eq!(bad["errors"][0]["line"], 2, "{bad}");
            assert!(
                bad["errors"][0]["message"]
                    .as_str()
                    .unwrap()
                    .contains("near")
            );
        }
        #[cfg(not(feature = "mapping-lua"))]
        assert_eq!(bad["available"], false);
        let (status, _) = call(serde_json::json!({"code": "", "params": ["a b"]})).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[test]
    fn parses_lua_messages() {
        assert_eq!(
            parse_error("code:3: unexpected symbol near 'x'"),
            LuaError {
                line: Some(3),
                message: "unexpected symbol near 'x'".into()
            }
        );
        assert_eq!(parse_error("weird").line, None);
    }
}
