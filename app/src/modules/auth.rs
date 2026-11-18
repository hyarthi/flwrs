//! Who may use the management API (and so the web UI): `[http.auth]`.
//!
//! - **A secret key configured** (`secret_key`, `secret_key_file` or
//!   `FLWRS_SECRET_KEY`): a client either logs in with it
//!   (`POST /api/auth/login`), which sets a session cookie, or sends it on
//!   every request as `Authorization: Bearer <key>` (scripts). The cookie holds
//!   sealed claims, not the key (`session.rs`), so the server keeps no session
//!   state, and changing the key logs every session out.
//! - **No key:** only loopback clients, which also have to name the server by
//!   a loopback address (`Host`), so a web page can't reach the API through
//!   DNS rebinding. A WARN at start if `[http]` listens beyond loopback.
//!
//! Every API route except the public auth ones (`api.rs`) goes through the
//! guard (`guard.rs`), which puts a [`Principal`] into the request's
//! extensions, and refuses state-changing browser requests from another
//! origin. Sessions carry a provider and a subject (only `key` / `admin`
//! today), so other providers (DB users, OIDC) can come later.
//!
//! `[plugins] admin_token` stays a separate gate on top, for plugin changes.

pub(crate) mod api;
mod config;
mod guard;
mod limiter;
mod session;

pub(crate) use guard::{Principal, Via};

use crate::modules::api_error::{ApiError, api_error};
use axum::Router;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use config::AuthSettings;
use limiter::LoginLimiter;
use session::Sealer;
use std::sync::OnceLock;
use std::time::Duration;

/// The session cookie's name.
pub(crate) const COOKIE: &str = "flwrs_session";

static AUTH: OnceLock<Auth> = OnceLock::new();

/// Reads `[http.auth]` (once, at start-up). An invalid section is an error:
/// the server doesn't start rather than run with auth it wasn't meant to have.
pub(crate) fn init() -> Result<&'static Auth, String> {
    if let Some(auth) = AUTH.get() {
        return Ok(auth);
    }
    let settings = config::read()?;
    let http = flwrs_core::http::ServerConfig::read();
    let auth = Auth::new(settings);
    if http.enabled() {
        match &auth.key {
            Some(_) => log::info!(
                "API auth: secret key (sessions last {}); clients log in or send it as a bearer token",
                humanize(auth.session_ttl)
            ),
            None if is_loopback_host(http.host()) => log::info!(
                "API auth: no [http.auth] secret key, so the API and web UI answer loopback clients only"
            ),
            None => log::warn!(
                "API auth: no [http.auth] secret key, so the API and web UI answer loopback clients only, \
                 although [http] listens on {}; set a secret key to let other machines in",
                http.host()
            ),
        }
    }
    Ok(AUTH.get_or_init(|| auth))
}

/// The auth set up by [`init`].
pub(crate) fn get() -> &'static Auth {
    AUTH.get()
        .expect("auth::init runs at start-up, before the HTTP server")
}

pub(crate) struct Auth {
    /// `None`: loopback clients only.
    key: Option<Key>,
    session_ttl: Duration,
    /// Extra origins allowed to send state-changing requests (a proxy that
    /// serves the UI under another name), normalized.
    allowed_origins: Vec<String>,
    /// `None`: `Secure` when the request came over HTTPS through a proxy
    /// (`X-Forwarded-Proto: https`).
    secure_cookie: Option<bool>,
    limiter: LoginLimiter,
}

struct Key {
    /// SHA-256 of the key: bearer tokens are compared as digests, in constant
    /// time, so neither the content nor the length leaks through timing.
    digest: [u8; 32],
    sealer: Sealer,
}

impl Key {
    fn new(secret: &str) -> Self {
        Self {
            digest: sha256(secret.as_bytes()),
            sealer: Sealer::new(secret.as_bytes()),
        }
    }

    fn matches(&self, presented: &str) -> bool {
        constant_time_eq(&sha256(presented.as_bytes()), &self.digest)
    }
}

impl Auth {
    fn new(settings: AuthSettings) -> Self {
        Self {
            key: settings.secret_key.as_deref().map(Key::new),
            session_ttl: settings.session_ttl,
            allowed_origins: settings.allowed_origins,
            secure_cookie: settings.secure_cookie,
            limiter: LoginLimiter::new(),
        }
    }

    /// Whether only loopback clients get in (no key configured).
    pub(crate) fn loopback_only(&self) -> bool {
        self.key.is_none()
    }

    /// `router` behind the guard: every route, and its fallback, needs a
    /// [`Principal`]. Routes merged into the result afterwards don't.
    pub(crate) fn protect(&'static self, router: Router) -> Router {
        router.layer(axum::middleware::from_fn_with_state(self, guard::guard))
    }

    fn secure(&self, headers: &HeaderMap) -> bool {
        self.secure_cookie.unwrap_or_else(|| {
            headers
                .get("x-forwarded-proto")
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v.trim().eq_ignore_ascii_case("https"))
        })
    }

    /// `Set-Cookie` for a new session.
    fn session_cookie(&self, value: &str, headers: &HeaderMap) -> String {
        let secure = if self.secure(headers) { "; Secure" } else { "" };
        format!(
            "{COOKIE}={value}; Path=/; HttpOnly; SameSite=Strict; Max-Age={}{secure}",
            self.session_ttl.as_secs()
        )
    }

    /// `Set-Cookie` that removes the session.
    fn clear_cookie(&self, headers: &HeaderMap) -> String {
        let secure = if self.secure(headers) { "; Secure" } else { "" };
        format!("{COOKIE}=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0{secure}")
    }
}

/// A request that isn't let in: a 401 (with `WWW-Authenticate`, which
/// browsers don't answer with a dialog for `Bearer`) or a 403, in the API's
/// error shape.
#[derive(Debug)]
pub(crate) struct Denied {
    status: StatusCode,
    error: String,
    details: Vec<String>,
}

fn unauthorized(message: &str, details: Vec<String>) -> Denied {
    Denied {
        status: StatusCode::UNAUTHORIZED,
        error: message.into(),
        details,
    }
}

fn forbidden(message: &str, details: Vec<String>) -> Denied {
    Denied {
        status: StatusCode::FORBIDDEN,
        error: message.into(),
        details,
    }
}

impl IntoResponse for Denied {
    fn into_response(self) -> Response {
        let unauthorized = self.status == StatusCode::UNAUTHORIZED;
        let error: ApiError = api_error(self.status, self.error, self.details);
        let mut response = error.into_response();
        if unauthorized {
            response.headers_mut().insert(
                header::WWW_AUTHENTICATE,
                header::HeaderValue::from_static("Bearer realm=\"flwrs\""),
            );
        }
        response
    }
}

fn sha256(data: &[u8]) -> [u8; 32] {
    let digest = ring::digest::digest(&ring::digest::SHA256, data);
    let mut out = [0u8; 32];
    out.copy_from_slice(digest.as_ref());
    out
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Whether a configured listen address only takes loopback connections.
fn is_loopback_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.to_canonical().is_loopback())
}

fn humanize(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        _ if s.is_multiple_of(86_400) => format!("{}d", s / 86_400),
        _ if s.is_multiple_of(3_600) => format!("{}h", s / 3_600),
        _ if s.is_multiple_of(60) => format!("{}m", s / 60),
        _ => format!("{s}s"),
    }
}

#[cfg(test)]
pub(crate) mod tests;
