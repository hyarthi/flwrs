//! REST for the server page: `/api/server/…`.

use super::catalog::{lookup, sections};
use super::document::{self, SECRET};
use super::{Problem, Reverted};
use crate::modules::api_error::{ApiError, ErrorBody, api_error};
use axum::extract::ConnectInfo;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::net::SocketAddr;
use utoipa::{OpenApi, ToSchema};

/// The build's optional features that are on.
fn features() -> Vec<&'static str> {
    let mut out = vec![];
    for (on, name) in [
        (cfg!(feature = "web-ui"), "web-ui"),
        (cfg!(feature = "mapping-lua"), "mapping-lua"),
        (cfg!(feature = "otel"), "otel"),
        (cfg!(feature = "telemetry-file"), "telemetry-file"),
        (cfg!(feature = "remote"), "remote"),
        (cfg!(feature = "swagger-ui"), "swagger-ui"),
    ] {
        if on {
            out.push(name);
        }
    }
    out
}

/// The server and its build.
#[derive(Serialize, ToSchema)]
pub(crate) struct ServerInfo {
    pub version: String,
    /// The optional build features that are on (`web-ui`, `remote`, …).
    pub features: Vec<&'static str>,
    /// `<os>-<arch>`.
    pub platform: String,
    pub pid: u32,
    /// When this process started (Unix seconds).
    pub start_time: i64,
    /// The config file (absolute where it can be resolved).
    pub config_path: String,
    /// `[http] config_edit`: the page may change the file.
    pub config_edit: bool,
    /// Why the page can't write the file, if it can't.
    pub read_only_reason: Option<String>,
    /// The file differs from what the server started with: a restart applies it.
    pub restart_needed: bool,
    /// `exec` or `exit` (`[server] restart`).
    pub restart_mode: String,
    /// A restarted config waits to be confirmed by this time (Unix seconds).
    pub confirm_deadline: Option<i64>,
    /// The last change that was put back, and why.
    pub last_revert: Option<Reverted>,
    /// The key the page must send again to save or restart (`[http.auth]` has one).
    pub step_up: bool,
    /// Where the management API and plugins connect, as running.
    pub http_address: String,
    pub hub_address: String,
    pub hub_tls_address: Option<String>,
    /// `[director] max_hops`: the hop limit of scenes that don't set their own.
    pub max_hops: u32,
}

#[utoipa::path(get, path = "/info", operation_id = "server-info", summary = "The server, its build, and its config file's state",
    responses((status = 200, description = "The info", body = ServerInfo)))]
async fn info() -> Json<ServerInfo> {
    let http = flwrs_core::http::ServerConfig::read();
    let hub = crate::modules::hub::get();
    let path = super::path();
    Json(ServerInfo {
        version: env!("CARGO_PKG_VERSION").into(),
        features: features(),
        platform: format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
        pid: std::process::id(),
        start_time: super::start_time(),
        config_path: std::path::absolute(path)
            .unwrap_or_else(|_| path.to_path_buf())
            .display()
            .to_string(),
        config_edit: http.config_edit(),
        read_only_reason: super::read_only_reason(),
        restart_needed: super::current_text().is_ok_and(|t| t != super::startup_text()),
        restart_mode: format!("{:?}", super::restart_mode()).to_lowercase(),
        confirm_deadline: super::pending_deadline(),
        last_revert: super::last_revert(),
        step_up: !crate::modules::auth::get().loopback_only(),
        http_address: format!("{}:{}", http.host(), http.port()),
        hub_address: hub.local_addr().to_string(),
        hub_tls_address: hub.tls_addr().map(|a| a.to_string()),
        max_hops: crate::modules::director::config::DirectorConfig::read().max_hops,
    })
}

/// A section as the page shows it.
#[derive(Serialize, ToSchema)]
pub(crate) struct SectionView {
    /// Its dotted path (`http.auth`).
    pub key: String,
    pub title: String,
    pub summary: String,
    /// JSON Schema (descriptions, defaults, `$defs`).
    #[schema(value_type = Object)]
    pub schema: Value,
    /// What the file sets (secrets as the placeholder).
    #[schema(value_type = Object)]
    pub values: Value,
    /// The values in effect: the file's, else the defaults (secrets as the placeholder).
    #[schema(value_type = Option<Object>)]
    pub effective: Option<Value>,
    /// Top-level key → `file`, `env` or `default`.
    pub sources: Map<String, Value>,
    /// Keys the page can't change (`""`: the whole section).
    pub locked: Vec<String>,
    /// Keys holding secrets.
    pub secrets: Vec<String>,
}

/// The config file for the page.
#[derive(Serialize, ToSchema)]
pub(crate) struct ConfigView {
    pub path: String,
    /// The file with its secrets replaced by `secret`; null if it doesn't parse.
    pub text: Option<String>,
    /// What stands for a secret's value; sent back as it is, the value stays.
    pub secret: &'static str,
    pub sections: Vec<SectionView>,
    /// The file's problems now.
    pub problems: Vec<Problem>,
    /// Saving is possible (`[http] config_edit` on, the file writable).
    pub editable: bool,
    pub read_only_reason: Option<String>,
}

fn internal(e: impl std::fmt::Display) -> ApiError {
    api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string(), vec![])
}

fn editable_reason() -> Option<String> {
    if !flwrs_core::http::ServerConfig::read().config_edit() {
        return Some("editing is off: set [http] config_edit = true in the file".into());
    }
    super::read_only_reason()
}

#[utoipa::path(get, path = "/config", operation_id = "server-config", summary = "The config file: text (secrets hidden), sections with schemas and values, problems",
    responses((status = 200, description = "The config", body = ConfigView)))]
async fn config() -> Result<Json<ConfigView>, ApiError> {
    let text = super::current_text().map_err(internal)?;
    let parsed = document::parse(&text).ok();
    let table: toml::Table = toml::from_str(&text).unwrap_or_default();
    let env_key = std::env::var("FLWRS_SECRET_KEY").is_ok();
    let views = sections()
        .iter()
        .map(|s| {
            let value = lookup(&table, s);
            let mut effective = (s.parse)(value).ok();
            if let Some(e) = &mut effective {
                document::redact_json(e, s);
            }
            let set: Vec<String> = value
                .and_then(|v| v.as_table())
                .map(|t| t.keys().cloned().collect())
                .unwrap_or_default();
            let mut sources = Map::new();
            if let Some(props) = (s.schema)()["properties"].as_object() {
                for key in props.keys() {
                    let source = if set.contains(key) {
                        "file"
                    } else if s.key == "http.auth" && key == "secret_key" && env_key {
                        "env"
                    } else {
                        "default"
                    };
                    sources.insert(key.clone(), Value::from(source));
                }
            }
            SectionView {
                key: s.key.into(),
                title: s.title.into(),
                summary: s.summary.into(),
                schema: (s.schema)(),
                values: parsed
                    .as_ref()
                    .map(|d| document::file_values(d, s))
                    .unwrap_or_else(|| Value::Object(Map::new())),
                effective,
                sources,
                locked: s
                    .locked
                    .iter()
                    .map(|p| p.trim_start_matches('/').to_string())
                    .collect(),
                secrets: s
                    .secrets
                    .iter()
                    .map(|p| p.trim_start_matches('/').to_string())
                    .collect(),
            }
        })
        .collect();
    let reason = editable_reason();
    Ok(Json(ConfigView {
        path: super::path().display().to_string(),
        text: document::redact(&text).ok(),
        secret: SECRET,
        sections: views,
        problems: super::check(&text),
        editable: reason.is_none(),
        read_only_reason: reason,
    }))
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct CheckRequest {
    /// The whole file (secrets may be the placeholder).
    pub text: String,
}

/// What saving `text` would do.
#[derive(Serialize, ToSchema)]
pub(crate) struct CheckResponse {
    pub problems: Vec<Problem>,
    /// Locked keys it changes (refused).
    pub locked: Vec<String>,
    /// It differs from what the server runs with: a restart applies it.
    pub restart_needed: bool,
    /// Each section's values in `text` (as `SectionView.values`), for the forms.
    pub values: Map<String, Value>,
}

/// `text` with its secrets put back, then checked.
fn evaluate(text: &str) -> Result<(String, CheckResponse), ApiError> {
    let current = super::current_text().map_err(internal)?;
    let restored = document::restore_secrets(text, &current).map_err(|problems| {
        api_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "secret placeholders without a value",
            problems,
        )
    })?;
    let values = document::parse(text)
        .map(|doc| {
            sections()
                .iter()
                .map(|s| (s.key.to_string(), document::file_values(&doc, s)))
                .collect()
        })
        .unwrap_or_default();
    let response = CheckResponse {
        problems: super::check(&restored),
        locked: super::locked_changes(&current, &restored),
        restart_needed: restored != super::startup_text(),
        values,
    };
    Ok((restored, response))
}

#[utoipa::path(post, path = "/config/check", operation_id = "check-server-config", summary = "Check a config text (nothing saved)",
    request_body = CheckRequest,
    responses((status = 200, description = "The checks", body = CheckResponse), (status = 422, description = "Secret placeholders the file has no value for", body = ErrorBody)))]
async fn check(Json(body): Json<CheckRequest>) -> Result<Json<CheckResponse>, ApiError> {
    Ok(Json(evaluate(&body.text)?.1))
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct FormRequest {
    /// The file as the page has it now.
    pub text: String,
    /// The section's key (`hub`, `http.auth`, …).
    pub section: String,
    /// The section's values as the form has them: left-out keys are removed
    /// (their defaults apply).
    #[schema(value_type = Object)]
    pub values: Value,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct FormResponse {
    /// The file with the section changed (comments elsewhere kept).
    pub text: String,
}

#[utoipa::path(post, path = "/config/form", operation_id = "apply-server-config-form", summary = "Write a section's form values into a config text (nothing saved)",
    request_body = FormRequest,
    responses((status = 200, description = "The new text", body = FormResponse), (status = 422, description = "The text or values can't be written", body = ErrorBody)))]
async fn form(Json(body): Json<FormRequest>) -> Result<Json<FormResponse>, ApiError> {
    document::apply_form(&body.text, &body.section, &body.values)
        .map(|text| Json(FormResponse { text }))
        .map_err(|e| api_error(StatusCode::UNPROCESSABLE_ENTITY, e, vec![]))
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct SaveRequest {
    pub text: String,
    /// `[http.auth]`'s key again (step-up), when it has one.
    #[serde(default)]
    pub secret_key: Option<String>,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct SaveResponse {
    pub restart_needed: bool,
}

fn step_up(peer: SocketAddr, key: Option<&str>) -> Result<(), ApiError> {
    crate::modules::auth::get()
        .step_up(peer.ip(), key)
        .map_err(|wait| match wait {
            Some(wait) => api_error(
                StatusCode::TOO_MANY_REQUESTS,
                format!(
                    "too many wrong keys; try again in {} s",
                    wait.as_secs().max(1)
                ),
                vec![],
            ),
            None => api_error(
                StatusCode::UNAUTHORIZED,
                "the secret key is needed (again) for this",
                vec!["send [http.auth]'s key as secret_key".into()],
            ),
        })
}

#[utoipa::path(put, path = "/config", operation_id = "save-server-config", summary = "Save the config file",
    description = "Needs `[http] config_edit = true` and a writable file, the secret key again (step-up), no problems, and no changed locked keys (`[http.auth]`, `[http] config_edit`, `[plugins] admin_token` / `allow_upload` / `dir`). Written to `<config>.new`, synced, the old file kept as `<config>.bak`, then renamed. Applies on the next restart.",
    request_body = SaveRequest,
    responses((status = 200, description = "Saved", body = SaveResponse), (status = 401, description = "Step-up needed", body = ErrorBody),
        (status = 403, description = "Editing is off, the file is read-only, or a locked key changed", body = ErrorBody),
        (status = 422, description = "Problems", body = ErrorBody), (status = 429, description = "Too many wrong keys", body = ErrorBody)))]
async fn save(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Json(body): Json<SaveRequest>,
) -> Result<Json<SaveResponse>, ApiError> {
    if let Some(reason) = editable_reason() {
        return Err(api_error(
            StatusCode::FORBIDDEN,
            "the config can't be saved from here",
            vec![reason],
        ));
    }
    step_up(peer, body.secret_key.as_deref())?;
    let (text, checked) = evaluate(&body.text)?;
    if !checked.locked.is_empty() {
        return Err(api_error(
            StatusCode::FORBIDDEN,
            "locked settings can only be changed in the file itself",
            checked.locked,
        ));
    }
    if !checked.problems.is_empty() {
        return Err(api_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "the config has problems",
            checked
                .problems
                .into_iter()
                .map(|p| match p.section {
                    Some(s) => format!("[{s}] {}", p.message),
                    None => p.message,
                })
                .collect(),
        ));
    }
    super::write(&text).map_err(internal)?;
    log::info!(
        "Config: {} saved from the server page",
        super::path().display()
    );
    Ok(Json(SaveResponse {
        restart_needed: checked.restart_needed,
    }))
}

#[derive(Deserialize, ToSchema, Default)]
pub(crate) struct StepUp {
    #[serde(default)]
    pub secret_key: Option<String>,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct RestartResponse {
    /// The config changed: the page must confirm it after the restart.
    pub confirm: bool,
}

#[utoipa::path(post, path = "/restart", operation_id = "restart-server", summary = "Restart the server",
    description = "Step-up, then pre-flight checks of the file (checks, folders, changed ports); then a graceful stop and a restart (`[server] restart`). A changed config must be confirmed (`POST …/config/confirm`) within `[server] confirm_timeout_s`, or the previous one comes back.",
    request_body = StepUp,
    responses((status = 202, description = "Restarting", body = RestartResponse), (status = 401, description = "Step-up needed", body = ErrorBody),
        (status = 409, description = "Pre-flight problems", body = ErrorBody)))]
async fn restart(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    body: Option<Json<StepUp>>,
) -> Result<(StatusCode, Json<RestartResponse>), ApiError> {
    let body = body.map(|Json(b)| b).unwrap_or_default();
    step_up(peer, body.secret_key.as_deref())?;
    let text = super::current_text().map_err(internal)?;
    let problems = super::preflight(&text, super::startup_text());
    if !problems.is_empty() {
        return Err(api_error(
            StatusCode::CONFLICT,
            "the config can't start",
            problems,
        ));
    }
    let confirm = text != super::startup_text();
    log::info!("Config: restart requested from the server page");
    // Answer first, then stop.
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        super::request_restart(true);
    });
    Ok((StatusCode::ACCEPTED, Json(RestartResponse { confirm })))
}

#[utoipa::path(post, path = "/config/confirm", operation_id = "confirm-server-config", summary = "Keep the config the server restarted with",
    responses((status = 204, description = "Confirmed")))]
async fn confirm() -> StatusCode {
    super::confirm();
    log::info!("Config: the changed config was confirmed");
    StatusCode::NO_CONTENT
}

#[utoipa::path(post, path = "/config/revert", operation_id = "revert-server-config", summary = "Put the previous config back and restart",
    request_body = StepUp,
    responses((status = 202, description = "Reverting"), (status = 401, description = "Step-up needed", body = ErrorBody),
        (status = 409, description = "Nothing to revert", body = ErrorBody)))]
async fn revert(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    body: Option<Json<StepUp>>,
) -> Result<StatusCode, ApiError> {
    if super::pending_deadline().is_none() {
        return Err(api_error(
            StatusCode::CONFLICT,
            "no changed config is waiting to be confirmed",
            vec![],
        ));
    }
    let body = body.map(|Json(b)| b).unwrap_or_default();
    step_up(peer, body.secret_key.as_deref())?;
    super::revert_now().map_err(internal)?;
    Ok(StatusCode::ACCEPTED)
}

#[utoipa::path(delete, path = "/config/reverted", operation_id = "dismiss-server-config-revert", summary = "Forget the last revert's note",
    responses((status = 204, description = "Forgotten")))]
async fn dismiss_revert() -> StatusCode {
    super::dismiss_revert();
    StatusCode::NO_CONTENT
}

#[derive(OpenApi)]
#[openapi(
    info(
        title = "Server",
        description = "The server page: its config file and restarts"
    ),
    paths(
        info,
        config,
        check,
        form,
        save,
        restart,
        confirm,
        revert,
        dismiss_revert
    ),
    components(schemas(
        ServerInfo,
        Reverted,
        ConfigView,
        SectionView,
        Problem,
        CheckRequest,
        CheckResponse,
        FormRequest,
        FormResponse,
        SaveRequest,
        SaveResponse,
        StepUp,
        RestartResponse,
        ErrorBody
    ))
)]
pub(crate) struct Api;

pub(crate) fn router() -> Router {
    Router::new()
        .route("/server/info", get(info))
        .route("/server/config", get(config).put(save))
        .route("/server/config/check", post(check))
        .route("/server/config/form", post(form))
        .route("/server/config/confirm", post(confirm))
        .route("/server/config/revert", post(revert))
        .route(
            "/server/config/reverted",
            axum::routing::delete(dismiss_revert),
        )
        .route("/server/restart", post(restart))
}
