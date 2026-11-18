//! The middleware in front of every protected API route.

use super::config::normalize_origin;
use super::{Auth, COOKIE, Denied, forbidden, unauthorized};
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderMap, Method, Uri, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use std::net::SocketAddr;

/// Who is calling: put into every protected request's extensions.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Principal {
    /// `key`, or `loopback` when no key is configured.
    pub provider: String,
    pub subject: String,
    pub roles: Vec<String>,
    pub via: Via,
    /// Unix seconds, for sessions.
    pub expire_time: Option<i64>,
}

/// How the request proved who it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Via {
    /// The session cookie.
    Session,
    /// `Authorization: Bearer <key>`: not something a browser sends on its
    /// own, so no cross-origin check.
    Bearer,
    /// No key configured: a loopback client.
    Loopback,
}

pub(crate) async fn guard(
    State(auth): State<&'static Auth>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    mut request: Request,
    next: Next,
) -> Response {
    let principal = match auth.authenticate(request.uri(), request.headers(), peer) {
        Ok(principal) => principal,
        Err(denied) => return denied.into_response(),
    };
    if let Err(denied) = auth.check_origin(
        request.method(),
        request.uri(),
        request.headers(),
        &principal,
    ) {
        return denied.into_response();
    }
    request.extensions_mut().insert(principal);
    next.run(request).await
}

impl Auth {
    pub(crate) fn authenticate(
        &self,
        uri: &Uri,
        headers: &HeaderMap,
        peer: SocketAddr,
    ) -> Result<Principal, Denied> {
        let Some(key) = &self.key else {
            if !peer.ip().to_canonical().is_loopback() {
                return Err(forbidden(
                    "the API only answers loopback clients: no secret key is configured",
                    vec!["set [http.auth] secret_key (or secret_key_file, or FLWRS_SECRET_KEY) to let other machines log in".into()],
                ));
            }
            // A loopback peer reached through another name is a web page using
            // DNS rebinding (or a proxy, which needs a key anyway).
            if !host(uri, headers).is_some_and(is_loopback_name) {
                return Err(forbidden(
                    "without a secret key, the API must be reached by a loopback address (localhost, 127.0.0.1, [::1])",
                    vec!["set [http.auth] secret_key to reach it through other names (a proxy, a DNS name)".into()],
                ));
            }
            return Ok(Principal {
                provider: "loopback".into(),
                subject: "local".into(),
                roles: Vec::new(),
                via: Via::Loopback,
                expire_time: None,
            });
        };

        if let Some(value) = headers.get(header::AUTHORIZATION) {
            let token = value.to_str().ok().and_then(bearer);
            return match token {
                Some(token) if key.matches(token) => Ok(Principal {
                    provider: "key".into(),
                    subject: "admin".into(),
                    roles: Vec::new(),
                    via: Via::Bearer,
                    expire_time: None,
                }),
                Some(_) => Err(unauthorized("wrong secret key", vec![])),
                None => Err(unauthorized(
                    "unsupported Authorization header",
                    vec![
                        "send `Authorization: Bearer <secret key>`, or log in for a session".into(),
                    ],
                )),
            };
        }

        let Some(cookie) = cookie(headers, COOKIE) else {
            return Err(unauthorized("not logged in", vec![]));
        };
        let now = chrono::Utc::now().timestamp();
        match key.sealer.open(cookie) {
            Some(claims) if claims.expire_time > now => Ok(Principal {
                provider: claims.provider,
                subject: claims.subject,
                roles: claims.roles,
                via: Via::Session,
                expire_time: Some(claims.expire_time),
            }),
            _ => Err(unauthorized(
                "the session has expired or is no longer valid; log in again",
                vec![],
            )),
        }
    }

    /// Browsers send `Origin` with every state-changing request. One from
    /// another origin is refused (cross-site request forgery), unless the
    /// request carries the key itself. `SameSite=Strict` already keeps the
    /// cookie off cross-site requests; this also covers loopback mode, which
    /// has no cookie.
    pub(crate) fn check_origin(
        &self,
        method: &Method,
        uri: &Uri,
        headers: &HeaderMap,
        principal: &Principal,
    ) -> Result<(), Denied> {
        if principal.via == Via::Bearer
            || matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS)
        {
            return Ok(());
        }
        let Some(origin) = headers.get(header::ORIGIN) else {
            return Ok(());
        };
        let origin = origin.to_str().ok().and_then(normalize_origin);
        let same = origin.as_deref().is_some_and(|origin| {
            let authority = origin.split_once("://").map_or("", |(_, a)| a);
            let own = host(uri, headers).map(|h| {
                let h = h.to_ascii_lowercase();
                // `Host` may carry the default port; `normalize_origin` drops it.
                h.strip_suffix(":80")
                    .or_else(|| h.strip_suffix(":443"))
                    .map(str::to_string)
                    .unwrap_or(h)
            });
            own.as_deref() == Some(authority) || self.allowed_origins.iter().any(|o| o == origin)
        });
        if same {
            Ok(())
        } else {
            Err(forbidden(
                "cross-origin request refused",
                vec![format!(
                    "Origin {} isn't this server; if a proxy serves the UI under another name, list it in [http.auth] allowed_origins",
                    headers
                        .get(header::ORIGIN)
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("?")
                )],
            ))
        }
    }
}

/// The token of `Bearer <token>` (scheme case-insensitive).
fn bearer(value: &str) -> Option<&str> {
    let (scheme, token) = value.trim().split_once(' ')?;
    scheme
        .eq_ignore_ascii_case("bearer")
        .then(|| token.trim())
        .filter(|t| !t.is_empty())
}

/// A cookie's value from the `Cookie` headers.
pub(super) fn cookie<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .find_map(|pair| {
            let (k, v) = pair.trim().split_once('=')?;
            (k == name).then_some(v.trim())
        })
        .filter(|v| !v.is_empty())
}

/// The host the client asked for: `Host`, or the URI's authority (HTTP/2).
fn host<'a>(uri: &'a Uri, headers: &'a HeaderMap) -> Option<&'a str> {
    headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .or_else(|| uri.authority().map(|a| a.as_str()))
}

/// `localhost`, `*.localhost`, `127.x.x.x` or `[::1]`, with or without a port.
fn is_loopback_name(host: &str) -> bool {
    let name = if let Some(rest) = host.strip_prefix('[') {
        rest.split(']').next().unwrap_or("")
    } else {
        host.rsplit_once(':').map_or(host, |(name, _)| name)
    };
    let name = name.to_ascii_lowercase();
    name == "localhost"
        || name.ends_with(".localhost")
        || name
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.to_canonical().is_loopback())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_names() {
        for host in [
            "localhost",
            "localhost:8080",
            "app.localhost:1",
            "127.0.0.1:80",
            "127.1.2.3",
            "[::1]:8080",
            "[::1]",
        ] {
            assert!(is_loopback_name(host), "{host}");
        }
        for host in [
            "example.com",
            "evil.com:8080",
            "10.0.0.1:80",
            "localhost.evil.com",
            "[::2]:80",
            "",
        ] {
            assert!(!is_loopback_name(host), "{host}");
        }
    }

    #[test]
    fn bearer_and_cookie_parsing() {
        assert_eq!(bearer("Bearer abc"), Some("abc"));
        assert_eq!(bearer("bearer  abc "), Some("abc"));
        assert_eq!(bearer("Basic abc"), None);
        assert_eq!(bearer("Bearer "), None);
        let mut headers = HeaderMap::new();
        headers.append(header::COOKIE, "a=1; flwrs_session=xyz".parse().unwrap());
        headers.append(header::COOKIE, "b=2".parse().unwrap());
        assert_eq!(cookie(&headers, "flwrs_session"), Some("xyz"));
        assert_eq!(cookie(&headers, "b"), Some("2"));
        assert_eq!(cookie(&headers, "c"), None);
    }
}
