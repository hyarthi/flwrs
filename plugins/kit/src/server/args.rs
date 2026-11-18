//! The server's settings: a clap struct that HTTP plugins flatten into
//! their own `Args`, its checks (every problem reported), and the matching
//! launch-args schema properties ([`schema_properties`]), which a plugin's
//! `schema/launch_args.json` includes and its package tests compare.

use super::auth::{Auth, KeyPlace, Secrets};
use super::headers::HeaderPolicy;
use super::tls::{ClientAuth, TlsFiles, server_config};
use clap::{Args, ValueEnum};
use flwrs_plugin::cli::read_json;
use rustls::ServerConfig;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

#[derive(Args, Debug, Clone)]
pub struct ServerArgs {
    /// Address and port to listen on
    #[arg(long, default_value = "127.0.0.1:8080")]
    pub listen: SocketAddr,

    /// PEM file: the server's certificate chain (turns TLS on)
    #[arg(long)]
    pub tls_cert: Option<PathBuf>,

    /// PEM file: the server's private key
    #[arg(long)]
    pub tls_key: Option<PathBuf>,

    /// PEM file: the CA(s) of client certificates (mTLS)
    #[arg(long)]
    pub tls_client_ca: Option<PathBuf>,

    /// With tls_client_ca: whether clients must present a certificate
    #[arg(long, value_enum)]
    pub tls_client_auth: Option<ClientAuthArg>,

    /// Accept cleartext HTTP/2 (prior knowledge) besides HTTP/1.1, without TLS
    #[arg(long)]
    pub h2c: bool,

    /// JSON file: limits (max_body_bytes, max_header_bytes, request_timeout_ms,
    /// header_read_timeout_ms, max_connections, max_concurrent_requests, h2_max_streams,
    /// h2_max_send_buf_bytes)
    #[arg(long)]
    pub limits: Option<PathBuf>,

    /// Inbound authentication (required on a non-loopback address; none must be explicit there)
    #[arg(long, value_enum)]
    pub auth: Option<AuthArg>,

    /// JSON file (secret): {"label": "token or key"} for bearer and api_key
    #[arg(long)]
    pub auth_secrets: Option<PathBuf>,

    /// api_key: the header or query parameter carrying the key
    #[arg(long, default_value = "x-api-key")]
    pub api_key_name: String,

    /// api_key: where the key is
    #[arg(long, value_enum, default_value = "header")]
    pub api_key_in: KeyPlaceArg,

    /// JSON file: mtls allowlist, {"fingerprints": [SHA-256 hex], "names": [SAN or CN]}
    #[arg(long)]
    pub mtls_allow: Option<PathBuf>,

    /// Liveness path (200 while the process runs; no auth)
    #[arg(long, default_value = "/healthz")]
    pub health_path: String,

    /// Readiness path (200 while the plugin can make progress, else 503; no auth)
    #[arg(long, default_value = "/readyz")]
    pub ready_path: String,

    /// JSON file: the only request headers events may carry (lowercase names)
    #[arg(long)]
    pub header_allowlist: Option<PathBuf>,

    /// JSON file: request headers events never carry (default: authorization, cookies, x-api-key)
    #[arg(long)]
    pub header_denylist: Option<PathBuf>,
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientAuthArg {
    #[value(name = "optional")]
    Optional,
    #[value(name = "required")]
    Required,
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthArg {
    #[value(name = "none")]
    None,
    #[value(name = "bearer")]
    Bearer,
    #[value(name = "api_key")]
    ApiKey,
    #[value(name = "mtls")]
    Mtls,
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyPlaceArg {
    #[value(name = "header")]
    Header,
    #[value(name = "query")]
    Query,
}

/// Limits, with safe defaults.
#[derive(Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields, default)]
pub struct Limits {
    pub max_body_bytes: usize,
    pub max_header_bytes: usize,
    /// Until the response starts (a stream's body may go on).
    pub request_timeout_ms: u64,
    /// For the TLS handshake and each request's headers (slow clients).
    pub header_read_timeout_ms: u64,
    pub max_connections: usize,
    pub max_concurrent_requests: usize,
    pub h2_max_streams: u32,
    /// HTTP/2: what one stream may have waiting to be sent before its writer
    /// waits (hyper's default is ~400 KiB: per slow SSE subscriber).
    pub h2_max_send_buf_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_body_bytes: 1024 * 1024,
            max_header_bytes: 16 * 1024,
            request_timeout_ms: 30_000,
            header_read_timeout_ms: 10_000,
            max_connections: 1024,
            max_concurrent_requests: 256,
            h2_max_streams: 128,
            h2_max_send_buf_bytes: 4 * 1024,
        }
    }
}

#[derive(Deserialize, Debug, Default)]
#[serde(deny_unknown_fields, default)]
struct MtlsAllow {
    fingerprints: Vec<String>,
    names: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ServerSettings {
    pub listen: SocketAddr,
    pub tls: Option<Arc<ServerConfig>>,
    pub h2c: bool,
    pub limits: Limits,
    pub auth: Arc<Auth>,
    pub headers: HeaderPolicy,
    pub health_path: String,
    pub ready_path: String,
    /// Logged at start (e.g. `auth: none` on a public address).
    pub warnings: Vec<String>,
}

impl ServerSettings {
    pub fn request_timeout(&self) -> Duration {
        Duration::from_millis(self.limits.request_timeout_ms)
    }

    pub fn header_read_timeout(&self) -> Duration {
        Duration::from_millis(self.limits.header_read_timeout_ms)
    }

    /// Plain HTTP on loopback with defaults: for tests and local tools.
    pub fn local(port: u16) -> Self {
        Self {
            listen: SocketAddr::from(([127, 0, 0, 1], port)),
            tls: None,
            h2c: false,
            limits: Limits::default(),
            auth: Arc::new(Auth::None),
            headers: HeaderPolicy::default(),
            health_path: "/healthz".into(),
            ready_path: "/readyz".into(),
            warnings: vec![],
        }
    }
}

fn normalize_fingerprint(f: &str) -> String {
    f.chars()
        .filter(|c| *c != ':')
        .collect::<String>()
        .to_ascii_lowercase()
}

impl ServerArgs {
    pub fn settings(&self) -> Result<ServerSettings, Vec<String>> {
        let mut problems = vec![];
        let mut warnings = vec![];
        let limits = self
            .limits
            .as_deref()
            .and_then(|p| {
                read_json::<Limits>(p)
                    .map_err(|e| problems.push(format!("--limits {e}")))
                    .ok()
            })
            .unwrap_or_default();
        for (name, value) in [
            ("max_body_bytes", limits.max_body_bytes as u64),
            ("request_timeout_ms", limits.request_timeout_ms),
            ("header_read_timeout_ms", limits.header_read_timeout_ms),
            ("max_connections", limits.max_connections as u64),
            (
                "max_concurrent_requests",
                limits.max_concurrent_requests as u64,
            ),
            ("h2_max_streams", limits.h2_max_streams.into()),
            ("h2_max_send_buf_bytes", limits.h2_max_send_buf_bytes as u64),
        ] {
            if value == 0 {
                problems.push(format!("limits.{name} must be at least 1"));
            }
        }
        if limits.h2_max_send_buf_bytes > u32::MAX as usize {
            problems.push("limits.h2_max_send_buf_bytes must be at most 4294967295".into());
        }
        if limits.max_header_bytes < 8192 {
            problems.push("limits.max_header_bytes must be at least 8192".into());
        }

        // TLS
        let auth_kind = self.auth;
        let client_auth = match (self.tls_client_auth, auth_kind) {
            (Some(ClientAuthArg::Optional), Some(AuthArg::Mtls)) => {
                problems.push("auth: mtls needs tls_client_auth: required (or unset)".into());
                ClientAuth::Required
            }
            (_, Some(AuthArg::Mtls)) | (Some(ClientAuthArg::Required), _) => ClientAuth::Required,
            _ => ClientAuth::Optional,
        };
        let tls = match (&self.tls_cert, &self.tls_key) {
            (Some(cert), Some(key)) => {
                if self.h2c {
                    problems.push(
                        "h2c is cleartext HTTP/2: with TLS, HTTP/2 is negotiated anyway".into(),
                    );
                }
                server_config(&TlsFiles {
                    cert,
                    key,
                    client_ca: self.tls_client_ca.as_deref(),
                    client_auth,
                })
                .map_err(|e| problems.extend(e))
                .ok()
            }
            (None, None) => {
                if self.tls_client_ca.is_some() {
                    problems.push("tls_client_ca needs tls_cert and tls_key".into());
                }
                None
            }
            _ => {
                problems.push("tls_cert and tls_key go together".into());
                None
            }
        };
        if self.tls_client_auth.is_some() && self.tls_client_ca.is_none() {
            problems.push("tls_client_auth needs tls_client_ca".into());
        }

        // Auth
        let secrets = self.auth_secrets.as_deref().and_then(|p| {
            read_json::<BTreeMap<String, String>>(p)
                .map_err(|e| problems.push(format!("--auth-secrets {e}")))
                .ok()
        });
        if let Some(s) = &secrets
            && s.iter()
                .any(|(label, secret)| label.is_empty() || secret.is_empty())
        {
            problems.push("auth_secrets: labels and secrets can't be empty".into());
        }
        let needs_secrets = matches!(auth_kind, Some(AuthArg::Bearer | AuthArg::ApiKey));
        if secrets.is_some() && !needs_secrets {
            problems.push("auth_secrets is for auth: bearer or api_key".into());
        }
        if self.mtls_allow.is_some() && auth_kind != Some(AuthArg::Mtls) {
            problems.push("mtls_allow is for auth: mtls".into());
        }
        let secrets = || match &secrets {
            Some(s) if !s.is_empty() => Some(Secrets::new(s.clone())),
            _ => None,
        };
        let auth = match auth_kind {
            None | Some(AuthArg::None) => {
                if !self.listen.ip().is_loopback() {
                    if auth_kind.is_none() {
                        problems.push(format!(
                            "listening on {} (not loopback) needs auth: bearer, api_key or mtls, or auth: none set explicitly",
                            self.listen
                        ));
                    } else {
                        warnings.push(format!(
                            "auth: none on {}: anyone who can reach it can send",
                            self.listen
                        ));
                    }
                }
                Some(Auth::None)
            }
            Some(AuthArg::Bearer) => match secrets() {
                Some(tokens) => Some(Auth::Bearer(tokens)),
                None => {
                    problems.push("auth: bearer needs auth_secrets (at least one token)".into());
                    None
                }
            },
            Some(AuthArg::ApiKey) => match secrets() {
                Some(keys) if !self.api_key_name.is_empty() => Some(Auth::ApiKey {
                    name: self.api_key_name.to_ascii_lowercase(),
                    place: match self.api_key_in {
                        KeyPlaceArg::Header => KeyPlace::Header,
                        KeyPlaceArg::Query => KeyPlace::Query,
                    },
                    keys,
                }),
                Some(_) => {
                    problems.push("api_key_name can't be empty".into());
                    None
                }
                None => {
                    problems.push("auth: api_key needs auth_secrets (at least one key)".into());
                    None
                }
            },
            Some(AuthArg::Mtls) => {
                if self.tls_client_ca.is_none() {
                    problems.push("auth: mtls needs tls_client_ca (and tls_cert, tls_key)".into());
                }
                let allow = self
                    .mtls_allow
                    .as_deref()
                    .and_then(|p| {
                        read_json::<MtlsAllow>(p)
                            .map_err(|e| problems.push(format!("--mtls-allow {e}")))
                            .ok()
                    })
                    .unwrap_or_default();
                let fingerprints: Vec<String> = allow
                    .fingerprints
                    .iter()
                    .map(|f| normalize_fingerprint(f))
                    .collect();
                for f in &fingerprints {
                    if f.len() != 64 || !f.chars().all(|c| c.is_ascii_hexdigit()) {
                        problems.push(format!("mtls_allow.fingerprints: {f:?} isn't a SHA-256 fingerprint (64 hex digits)"));
                    }
                }
                Some(Auth::Mtls {
                    fingerprints,
                    names: allow.names,
                })
            }
        };

        // Paths and headers
        for (name, path) in [
            ("health_path", &self.health_path),
            ("ready_path", &self.ready_path),
        ] {
            if !path.starts_with('/') || path.contains(['{', '}', '*', '?', '#']) {
                problems.push(format!(
                    "{name} {path:?} must be a plain path starting with /"
                ));
            }
        }
        if self.health_path == self.ready_path {
            problems.push("health_path and ready_path must differ".into());
        }
        let list = |flag: &str, p: &Option<PathBuf>, problems: &mut Vec<String>| {
            p.as_deref().and_then(|p| {
                read_json::<Vec<String>>(p)
                    .map_err(|e| problems.push(format!("--{flag} {e}")))
                    .ok()
            })
        };
        let allow = list("header-allowlist", &self.header_allowlist, &mut problems);
        let deny = list("header-denylist", &self.header_denylist, &mut problems);
        if allow.is_some() && deny.is_some() {
            problems
                .push("header_allowlist and header_denylist: set one (the allowlist wins)".into());
        }

        match auth {
            Some(auth) if problems.is_empty() => {
                let headers = HeaderPolicy::new(allow, deny, &auth.secret_headers());
                Ok(ServerSettings {
                    listen: self.listen,
                    tls,
                    h2c: self.h2c,
                    limits,
                    auth: Arc::new(auth),
                    headers,
                    health_path: self.health_path.clone(),
                    ready_path: self.ready_path.clone(),
                    warnings,
                })
            }
            _ => Err(problems),
        }
    }
}

/// The launch-args schema properties for [`ServerArgs`] (JSON Schema 2020-12
/// with flwrs annotations), for a plugin's `schema/launch_args.json`.
pub fn schema_properties() -> serde_json::Value {
    let group = |order: u32, advanced: bool, mut v: serde_json::Value| {
        let o = v.as_object_mut().expect("an object");
        o.insert("x-flwrs-group".into(), "HTTP server".into());
        o.insert("x-flwrs-order".into(), order.into());
        if advanced {
            o.insert("x-flwrs-advanced".into(), true.into());
        }
        v
    };
    let upload = |title: &str, description: &str| {
        serde_json::json!({ "title": title, "description": description, "type": "string",
                            "x-flwrs-launch": { "via": "file", "source": "upload" } })
    };
    let mut key = upload("TLS key", "Uploaded PEM file: the server's private key.");
    key["writeOnly"] = true.into();
    serde_json::json!({
        "listen": group(100, false, serde_json::json!({
            "title": "Listen on", "description": "Address and port. Anything but loopback needs auth (or auth: none set explicitly).",
            "type": "string", "default": "127.0.0.1:8080", "x-flwrs-listen": true })),
        "auth": group(101, false, serde_json::json!({
            "title": "Authentication", "description": "none, bearer (Authorization: Bearer <token>), api_key (a header or query parameter) or mtls (client certificates).",
            "enum": ["none", "bearer", "api_key", "mtls"] })),
        "auth_secrets": group(102, false, serde_json::json!({
            "title": "Tokens / keys", "description": "bearer and api_key: {\"label\": \"secret\"}. The label of the one a request used becomes its event's principal.",
            "type": "object", "additionalProperties": { "type": "string", "minLength": 1 }, "writeOnly": true,
            "x-flwrs-launch": { "via": "file" } })),
        "api_key_name": group(103, true, serde_json::json!({
            "title": "API key name", "type": "string", "minLength": 1, "default": "x-api-key" })),
        "api_key_in": group(104, true, serde_json::json!({
            "title": "API key in", "enum": ["header", "query"], "default": "header" })),
        "mtls_allow": group(105, true, serde_json::json!({
            "title": "Allowed client certificates", "description": "mtls: SHA-256 fingerprints or names (SAN DNS / URI, or CN). Empty: any certificate the client CA signed.",
            "type": "object", "additionalProperties": false,
            "properties": { "fingerprints": { "type": "array", "items": { "type": "string" } },
                            "names": { "type": "array", "items": { "type": "string" } } } })),
        "tls_cert": group(110, false, upload("TLS certificate", "Uploaded PEM file: the server's certificate chain. Turns TLS on (HTTP/2 and HTTP/1.1 by ALPN).")),
        "tls_key": group(111, false, key),
        "tls_client_ca": group(112, true, upload("Client CA", "Uploaded PEM file: the CA(s) of client certificates (mTLS).")),
        "tls_client_auth": group(113, true, serde_json::json!({
            "title": "Client certificates", "description": "With a client CA: optional or required (required for auth: mtls).",
            "enum": ["optional", "required"] })),
        "h2c": group(114, true, serde_json::json!({
            "title": "Cleartext HTTP/2", "description": "Without TLS: accept HTTP/2 with prior knowledge besides HTTP/1.1.",
            "type": "boolean", "default": false })),
        "limits": group(120, true, serde_json::json!({
            "title": "Limits", "type": "object", "additionalProperties": false,
            "properties": {
                "max_body_bytes": { "type": "integer", "minimum": 1, "default": 1_048_576 },
                "max_header_bytes": { "type": "integer", "minimum": 8192, "default": 16_384 },
                "request_timeout_ms": { "type": "integer", "minimum": 1, "default": 30_000 },
                "header_read_timeout_ms": { "type": "integer", "minimum": 1, "default": 10_000 },
                "max_connections": { "type": "integer", "minimum": 1, "default": 1024 },
                "max_concurrent_requests": { "type": "integer", "minimum": 1, "default": 256 },
                "h2_max_streams": { "type": "integer", "minimum": 1, "default": 128 },
                "h2_max_send_buf_bytes": { "type": "integer", "minimum": 1, "maximum": 4_294_967_295u64, "default": 4096,
                    "description": "HTTP/2: what one stream may have waiting to be sent before its writer waits. Memory per stream with slow readers (e.g. SSE subscribers): with 1 000 HTTP/2 SSE subscribers, 4 KiB gave ~3x the throughput and a fifth of the memory of 64 KiB, at ~10 % less for a single fast stream." } } })),
        "health_path": group(121, true, serde_json::json!({ "title": "Liveness path", "type": "string", "default": "/healthz" })),
        "ready_path": group(122, true, serde_json::json!({ "title": "Readiness path", "type": "string", "default": "/readyz" })),
        "header_allowlist": group(123, true, serde_json::json!({
            "title": "Header allowlist", "description": "The only request headers events carry (wins over the denylist).",
            "type": "array", "items": { "type": "string" } })),
        "header_denylist": group(124, true, serde_json::json!({
            "title": "Header denylist", "description": "Request headers events never carry. Default: authorization, proxy-authorization, cookie, set-cookie, x-api-key (and the API key's header).",
            "type": "array", "items": { "type": "string" } })),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::{CommandFactory, Parser};

    #[derive(Parser)]
    struct Cli {
        #[command(flatten)]
        server: ServerArgs,
    }

    fn args(extra: &[&str]) -> ServerArgs {
        let mut argv = vec!["p"];
        argv.extend_from_slice(extra);
        Cli::try_parse_from(argv).unwrap().server
    }

    fn file(dir: &tempfile::TempDir, name: &str, text: &str) -> String {
        let path = dir.path().join(name);
        std::fs::write(&path, text).unwrap();
        path.to_string_lossy().into_owned()
    }

    #[test]
    fn the_schema_matches_the_flags() {
        let props = schema_properties();
        let from_schema: std::collections::BTreeSet<String> = props
            .as_object()
            .unwrap()
            .keys()
            .map(|k| k.replace('_', "-"))
            .collect();
        let from_cli: std::collections::BTreeSet<String> = Cli::command()
            .get_arguments()
            .filter_map(|a| a.get_long().map(str::to_string))
            .collect();
        assert_eq!(from_schema, from_cli);
    }

    #[test]
    fn loopback_needs_no_auth_but_anything_else_does() {
        let s = args(&[]).settings().unwrap();
        assert_eq!(*s.auth, Auth::None);
        let e = args(&["--listen", "0.0.0.0:80"]).settings().unwrap_err();
        assert!(e[0].contains("needs auth"), "{e:?}");
        let s = args(&["--listen", "0.0.0.0:80", "--auth", "none"])
            .settings()
            .unwrap();
        assert!(s.warnings[0].contains("auth: none"));
    }

    #[test]
    fn bearer_and_api_keys() {
        let dir = tempfile::tempdir().unwrap();
        let secrets = file(&dir, "s.json", r#"{"ci": "t1"}"#);
        let s = args(&["--auth", "bearer", "--auth-secrets", &secrets])
            .settings()
            .unwrap();
        assert!(matches!(&*s.auth, Auth::Bearer(t) if t.len() == 1));
        let s = args(&[
            "--auth",
            "api_key",
            "--auth-secrets",
            &secrets,
            "--api-key-name",
            "X-Key",
        ])
        .settings()
        .unwrap();
        assert!(
            !s.headers.passes("x-key"),
            "the key's header never reaches events"
        );
        let e = args(&["--auth", "bearer"]).settings().unwrap_err();
        assert!(e[0].contains("needs auth_secrets"));
        let e = args(&["--auth-secrets", &secrets]).settings().unwrap_err();
        assert!(e[0].contains("is for auth: bearer"));
    }

    #[test]
    fn reports_every_problem() {
        let dir = tempfile::tempdir().unwrap();
        let limits = file(
            &dir,
            "l.json",
            r#"{"max_body_bytes": 0, "max_header_bytes": 100}"#,
        );
        let allow = file(&dir, "a.json", r#"{"fingerprints": ["zz"]}"#);
        let e = args(&[
            "--limits",
            &limits,
            "--tls-cert",
            "c.pem",
            "--tls-client-auth",
            "required",
            "--auth",
            "mtls",
            "--mtls-allow",
            &allow,
            "--health-path",
            "healthz",
            "--ready-path",
            "healthz",
        ])
        .settings()
        .unwrap_err();
        for expected in [
            "max_body_bytes",
            "max_header_bytes",
            "tls_cert and tls_key go together",
            "tls_client_auth needs tls_client_ca",
            "auth: mtls needs tls_client_ca",
            "isn't a SHA-256 fingerprint",
            "must be a plain path",
            "must differ",
        ] {
            assert!(
                e.iter().any(|p| p.contains(expected)),
                "{expected:?} not in {e:?}"
            );
        }
    }
}
