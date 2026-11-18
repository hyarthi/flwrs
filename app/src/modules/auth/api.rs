//! REST for logging in and out: `/api/auth/…`. `login`, `logout` and
//! `providers` are public; `session` is behind the guard.

use super::session::Claims;
use super::{Auth, Principal, Via, unauthorized};
use crate::modules::api_error::{ErrorBody, api_error, internal};
use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use utoipa::openapi::security::{ApiKey, ApiKeyValue, Http, HttpAuthScheme, SecurityScheme};
use utoipa::{Modify, OpenApi, ToSchema};

type A = &'static Auth;

/// Keys longer than this aren't even compared.
const MAX_PRESENTED_BYTES: usize = 4096;

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct LoginRequest {
    /// The server's secret key (`[http.auth]`).
    key: String,
}

/// Who is logged in.
#[derive(Serialize, ToSchema, Debug)]
pub(crate) struct SessionInfo {
    /// `key` (the secret key) or `loopback` (no key configured).
    provider: String,
    subject: String,
    roles: Vec<String>,
    /// When the session ends (RFC 3339); absent for bearer tokens and
    /// loopback clients.
    #[schema(format = DateTime)]
    expire_time: Option<String>,
    /// Whether logging out means anything (a session cookie).
    can_log_out: bool,
}

impl From<&Principal> for SessionInfo {
    fn from(p: &Principal) -> Self {
        Self {
            provider: p.provider.clone(),
            subject: p.subject.clone(),
            roles: p.roles.clone(),
            expire_time: p
                .expire_time
                .and_then(|t| DateTime::from_timestamp(t, 0))
                .map(|t| t.to_rfc3339_opts(SecondsFormat::Secs, true)),
            can_log_out: p.via == Via::Session,
        }
    }
}

/// How clients can log in.
#[derive(Serialize, ToSchema)]
pub(crate) struct AuthInfo {
    /// The ways to log in (empty when only loopback clients are let in).
    providers: Vec<Provider>,
    /// No secret key is configured: loopback clients get in without logging
    /// in, everyone else is refused.
    loopback_only: bool,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct Provider {
    /// `key`.
    id: String,
    /// What the login screen calls it.
    label: String,
}

#[utoipa::path(
    get,
    path = "/providers",
    operation_id = "auth-providers",
    summary = "How clients can log in",
    security(()),
    responses((status = 200, description = "Login options", body = AuthInfo))
)]
async fn providers(State(auth): State<A>) -> Json<AuthInfo> {
    let providers = if auth.loopback_only() {
        Vec::new()
    } else {
        vec![Provider {
            id: "key".into(),
            label: "Secret key".into(),
        }]
    };
    Json(AuthInfo {
        providers,
        loopback_only: auth.loopback_only(),
    })
}

#[utoipa::path(
    post,
    path = "/login",
    operation_id = "auth-login",
    summary = "Log in with the secret key",
    description = "Sets the session cookie (`flwrs_session`: HttpOnly, SameSite=Strict). Failed attempts are limited per client address (429 with `Retry-After`).",
    security(()),
    request_body = LoginRequest,
    responses(
        (status = 200, description = "Logged in; the cookie is set", body = SessionInfo),
        (status = 401, description = "Wrong key", body = ErrorBody),
        (status = 409, description = "No secret key is configured (loopback clients don't log in)", body = ErrorBody),
        (status = 429, description = "Too many failed attempts from this address", body = ErrorBody),
    )
)]
async fn login(
    State(auth): State<A>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(request): Json<LoginRequest>,
) -> Response {
    let Some(key) = &auth.key else {
        return api_error(
            StatusCode::CONFLICT,
            "no secret key is configured: loopback clients use the API without logging in",
            vec!["set [http.auth] secret_key to log in from other machines".into()],
        )
        .into_response();
    };
    if let Err(wait) = auth.limiter.check(peer.ip()) {
        let mut response = api_error(
            StatusCode::TOO_MANY_REQUESTS,
            "too many failed logins from this address; try again later",
            vec![],
        )
        .into_response();
        let seconds = wait.as_secs().max(1).to_string();
        if let Ok(value) = HeaderValue::from_str(&seconds) {
            response.headers_mut().insert(header::RETRY_AFTER, value);
        }
        return response;
    }
    if request.key.len() > MAX_PRESENTED_BYTES || !key.matches(request.key.trim()) {
        auth.limiter.failed(peer.ip());
        log::info!("API auth: failed login from {}", peer.ip());
        return unauthorized("wrong secret key", vec![]).into_response();
    }
    auth.limiter.succeeded(peer.ip());

    let now = Utc::now().timestamp();
    let ttl = i64::try_from(auth.session_ttl.as_secs()).unwrap_or(i64::MAX);
    let claims = Claims::new("key", "admin", now, now.saturating_add(ttl));
    let sealed = match key.sealer.seal(&claims) {
        Ok(sealed) => sealed,
        Err(e) => return internal("API auth: login", e).into_response(),
    };
    let principal = Principal {
        provider: claims.provider,
        subject: claims.subject,
        roles: claims.roles,
        via: Via::Session,
        expire_time: Some(claims.expire_time),
    };
    let mut response = Json(SessionInfo::from(&principal)).into_response();
    set_cookie(&mut response, &auth.session_cookie(&sealed, &headers));
    response
}

#[utoipa::path(
    post,
    path = "/logout",
    operation_id = "auth-logout",
    summary = "Log out: removes the session cookie",
    description = "Sessions aren't kept on the server: the cookie is removed from this browser. Changing the secret key ends every session.",
    security(()),
    responses((status = 204, description = "Logged out"))
)]
async fn logout(State(auth): State<A>, headers: HeaderMap) -> Response {
    let mut response = StatusCode::NO_CONTENT.into_response();
    set_cookie(&mut response, &auth.clear_cookie(&headers));
    response
}

#[utoipa::path(
    get,
    path = "/session",
    operation_id = "auth-session",
    summary = "Who is logged in",
    description = "401 when not logged in; 403 when the client isn't let in at all (no key configured and not a loopback client).",
    responses(
        (status = 200, description = "The caller", body = SessionInfo),
        (status = 401, description = "Not logged in", body = ErrorBody),
        (status = 403, description = "Not let in", body = ErrorBody),
    )
)]
async fn session(Extension(principal): Extension<Principal>) -> Json<SessionInfo> {
    Json(SessionInfo::from(&principal))
}

fn set_cookie(response: &mut Response, cookie: &str) {
    if let Ok(value) = HeaderValue::from_str(cookie) {
        response.headers_mut().insert(header::SET_COOKIE, value);
    }
    // Never cached by a proxy along the way.
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
}

#[derive(OpenApi)]
#[openapi(
    info(title = "Auth", description = "Logging in to the management API"),
    paths(providers, login, logout, session),
    components(schemas(LoginRequest, SessionInfo, AuthInfo, Provider, ErrorBody))
)]
pub(crate) struct Api;

impl Api {
    /// The routes anyone may call.
    pub(crate) fn public_router(auth: A) -> Router {
        Router::new()
            .route("/auth/providers", get(providers))
            .route("/auth/login", post(login))
            .route("/auth/logout", post(logout))
            .with_state(auth)
    }

    /// The routes that need a [`Principal`] (to be put behind the guard).
    pub(crate) fn protected_router() -> Router {
        Router::new().route("/auth/session", get(session))
    }
}

/// The API's security schemes (the secret key as a bearer token, the session
/// cookie), required by every operation that doesn't say otherwise.
pub(crate) struct SecuritySchemes;

impl Modify for SecuritySchemes {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        let components = openapi.components.get_or_insert_with(Default::default);
        components.add_security_scheme(
            "secret_key",
            SecurityScheme::Http(Http::new(HttpAuthScheme::Bearer)),
        );
        components.add_security_scheme(
            "session",
            SecurityScheme::ApiKey(ApiKey::Cookie(ApiKeyValue::new(super::COOKIE))),
        );
        openapi.security = Some(vec![
            utoipa::openapi::security::SecurityRequirement::new("secret_key", Vec::<String>::new()),
            utoipa::openapi::security::SecurityRequirement::new("session", Vec::<String>::new()),
        ]);
    }
}
