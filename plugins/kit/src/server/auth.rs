//! Inbound authentication: `none`, `bearer`, `api_key`, `mtls`. A request
//! that passes gets a [`Principal`]: the label of the token or key it used,
//! or its client certificate's name. That never includes the secret itself.

use super::ConnInfo;
use axum::extract::Request;
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use std::collections::BTreeMap;
use std::sync::Arc;

/// Who sent a request: a token's or key's label, a certificate's name (or
/// `sha256:<fingerprint>`), or empty without auth.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Principal(pub String);

/// Where an API key is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyPlace {
    Header,
    Query,
}

/// Labelled secrets, kept as SHA-256 digests and compared in constant time.
#[derive(Clone, PartialEq, Eq)]
pub struct Secrets(Vec<(String, Vec<u8>)>);

impl std::fmt::Debug for Secrets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let labels: Vec<&str> = self.0.iter().map(|(l, _)| l.as_str()).collect();
        f.debug_tuple("Secrets").field(&labels).finish()
    }
}

impl Secrets {
    /// From `{"label": "secret"}`.
    pub fn new(by_label: BTreeMap<String, String>) -> Self {
        Self(
            by_label
                .into_iter()
                .map(|(label, secret)| (label, sha256::digest(secret).into_bytes()))
                .collect(),
        )
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The label of the matching secret. Every secret is compared, so the
    /// time taken doesn't tell which one (or whether one) matched.
    pub fn check(&self, candidate: &str) -> Option<&str> {
        let digest = sha256::digest(candidate).into_bytes();
        let mut found = None;
        for (label, secret) in &self.0 {
            if constant_time_eq(secret, &digest) {
                found = Some(label.as_str());
            }
        }
        found
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Auth {
    None,
    Bearer(Secrets),
    ApiKey {
        name: String,
        place: KeyPlace,
        keys: Secrets,
    },
    /// A client certificate the TLS layer verified against the client CA,
    /// optionally limited to these SHA-256 fingerprints (lowercase hex) or
    /// names (SAN DNS / URI, or CN). Both empty: any verified certificate.
    Mtls {
        fingerprints: Vec<String>,
        names: Vec<String>,
    },
}

/// Why a request was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// 401: no credentials, or wrong ones.
    Unauthorized,
    /// 403: a verified certificate that isn't allowed.
    Forbidden,
}

impl Auth {
    /// Header names that carry secrets (never copied into events).
    pub fn secret_headers(&self) -> Vec<String> {
        match self {
            Auth::ApiKey {
                name,
                place: KeyPlace::Header,
                ..
            } => vec![name.to_ascii_lowercase()],
            _ => vec![],
        }
    }

    /// Checks one request: its `Authorization` header, query string and
    /// connection.
    pub fn check(
        &self,
        authorization: Option<&str>,
        headers: &axum::http::HeaderMap,
        query: Option<&str>,
        conn: Option<&ConnInfo>,
    ) -> Result<Principal, Refusal> {
        match self {
            Auth::None => Ok(Principal::default()),
            Auth::Bearer(tokens) => {
                let token = authorization
                    .and_then(|v| v.split_once(' '))
                    .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
                    .map(|(_, t)| t.trim())
                    .ok_or(Refusal::Unauthorized)?;
                tokens
                    .check(token)
                    .map(|l| Principal(l.to_string()))
                    .ok_or(Refusal::Unauthorized)
            }
            Auth::ApiKey { name, place, keys } => {
                let key = match place {
                    KeyPlace::Header => headers
                        .get(name.as_str())
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_string),
                    KeyPlace::Query => query_param(query.unwrap_or_default(), name),
                }
                .ok_or(Refusal::Unauthorized)?;
                keys.check(&key)
                    .map(|l| Principal(l.to_string()))
                    .ok_or(Refusal::Unauthorized)
            }
            Auth::Mtls {
                fingerprints,
                names,
            } => {
                let cert = conn
                    .and_then(|c| c.client_cert.as_ref())
                    .ok_or(Refusal::Unauthorized)?;
                let allowed_name = cert.names.iter().find(|n| names.contains(n));
                let any = fingerprints.is_empty() && names.is_empty();
                if any || fingerprints.contains(&cert.sha256) || allowed_name.is_some() {
                    let principal = allowed_name
                        .or(cert.names.first())
                        .cloned()
                        .unwrap_or_else(|| format!("sha256:{}", cert.sha256));
                    Ok(Principal(principal))
                } else {
                    Err(Refusal::Forbidden)
                }
            }
        }
    }
}

/// The first value of `name` in a query string (percent-decoded).
fn query_param(query: &str, name: &str) -> Option<String> {
    query_pairs(query)
        .into_iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v)
}

/// A query string or form body (`application/x-www-form-urlencoded`) as
/// decoded name / value pairs, in order (a name may repeat).
pub fn query_pairs(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            (percent_decode(k), percent_decode(v))
        })
        .collect()
}

fn percent_decode(s: &str) -> String {
    let hex = |b: u8| (b as char).to_digit(16).map(|d| d as u8);
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => match (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                (Some(hi), Some(lo)) => {
                    out.push(hi << 4 | lo);
                    i += 2;
                }
                _ => out.push(b'%'),
            },
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The middleware: refuses with 401 / 403, or adds the [`Principal`].
pub async fn layer(auth: Arc<Auth>, mut request: Request, next: Next) -> Response {
    let authorization = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    let result = auth.check(
        authorization,
        request.headers(),
        request.uri().query(),
        request.extensions().get::<ConnInfo>(),
    );
    match result {
        Ok(principal) => {
            request.extensions_mut().insert(principal);
            next.run(request).await
        }
        Err(refusal) => {
            super::note_refusal(match refusal {
                Refusal::Unauthorized => "unauthorized",
                Refusal::Forbidden => "forbidden",
            });
            match refusal {
                Refusal::Unauthorized => {
                    let mut response = StatusCode::UNAUTHORIZED.into_response();
                    if matches!(*auth, Auth::Bearer(_)) {
                        response
                            .headers_mut()
                            .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
                    }
                    response
                }
                Refusal::Forbidden => StatusCode::FORBIDDEN.into_response(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::ClientCert;
    use axum::http::HeaderMap;

    fn secrets() -> Secrets {
        Secrets::new(
            [
                ("ci".to_string(), "s3cret".to_string()),
                ("ops".to_string(), "other".to_string()),
            ]
            .into(),
        )
    }

    #[test]
    fn secrets_match_by_label_without_keeping_the_secret() {
        let s = secrets();
        assert_eq!(s.check("s3cret"), Some("ci"));
        assert_eq!(s.check("other"), Some("ops"));
        assert_eq!(s.check("s3cre"), None);
        assert!(!format!("{s:?}").contains("s3cret"));
    }

    #[test]
    fn bearer_tokens() {
        let auth = Auth::Bearer(secrets());
        let h = HeaderMap::new();
        assert_eq!(
            auth.check(Some("Bearer s3cret"), &h, None, None),
            Ok(Principal("ci".into()))
        );
        assert_eq!(
            auth.check(Some("bearer  s3cret"), &h, None, None),
            Ok(Principal("ci".into()))
        );
        for bad in [
            None,
            Some("Bearer nope"),
            Some("Basic s3cret"),
            Some("s3cret"),
        ] {
            assert_eq!(
                auth.check(bad, &h, None, None),
                Err(Refusal::Unauthorized),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn api_keys_in_a_header_or_the_query() {
        let mut h = HeaderMap::new();
        h.insert("x-key", "other".parse().unwrap());
        let auth = Auth::ApiKey {
            name: "x-key".into(),
            place: KeyPlace::Header,
            keys: secrets(),
        };
        assert_eq!(
            auth.check(None, &h, None, None),
            Ok(Principal("ops".into()))
        );
        assert_eq!(
            auth.check(None, &HeaderMap::new(), None, None),
            Err(Refusal::Unauthorized)
        );
        assert_eq!(auth.secret_headers(), ["x-key"]);
        let auth = Auth::ApiKey {
            name: "key".into(),
            place: KeyPlace::Query,
            keys: secrets(),
        };
        assert_eq!(
            auth.check(None, &h, Some("a=1&key=s3cret"), None),
            Ok(Principal("ci".into()))
        );
        assert_eq!(
            auth.check(None, &h, Some("key=s3cre%74"), None),
            Ok(Principal("ci".into()))
        );
        assert_eq!(
            auth.check(None, &h, Some("key=nope"), None),
            Err(Refusal::Unauthorized)
        );
    }

    #[test]
    fn client_certificates() {
        let conn = |names: &[&str]| ConnInfo {
            client_cert: Some(ClientCert {
                sha256: "ab12".into(),
                names: names.iter().map(|n| n.to_string()).collect(),
            }),
            ..ConnInfo::test()
        };
        let h = HeaderMap::new();
        let any = Auth::Mtls {
            fingerprints: vec![],
            names: vec![],
        };
        assert_eq!(
            any.check(None, &h, None, Some(&conn(&["dev-1"]))),
            Ok(Principal("dev-1".into()))
        );
        assert_eq!(
            any.check(None, &h, None, Some(&conn(&[]))),
            Ok(Principal("sha256:ab12".into()))
        );
        assert_eq!(
            any.check(None, &h, None, Some(&ConnInfo::test())),
            Err(Refusal::Unauthorized)
        );
        let named = Auth::Mtls {
            fingerprints: vec![],
            names: vec!["dev-2".into()],
        };
        assert_eq!(
            named.check(None, &h, None, Some(&conn(&["dev-1", "dev-2"]))),
            Ok(Principal("dev-2".into()))
        );
        assert_eq!(
            named.check(None, &h, None, Some(&conn(&["dev-1"]))),
            Err(Refusal::Forbidden)
        );
        let pinned = Auth::Mtls {
            fingerprints: vec!["ab12".into()],
            names: vec![],
        };
        assert!(pinned.check(None, &h, None, Some(&conn(&[]))).is_ok());
    }
}
