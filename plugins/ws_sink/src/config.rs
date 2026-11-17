//! The command line (see `schema/launch_args.json`), the Authorization value
//! from the environment, and their checks: every problem is reported.

use crate::json_path::{self, JsonPath};
use crate::tls::{TlsFiles, client_config};
use clap::{Parser, ValueEnum};
use flwrs_plugin::cli::{Bootstrap, read_json};
use rustls::ClientConfig;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio_tungstenite::tungstenite::http::{HeaderName, HeaderValue};

/// The Authorization header's environment variable (a secret).
pub const AUTHORIZATION_ENV: &str = "WS_AUTHORIZATION";

/// Headers the WebSocket handshake sets itself.
const RESERVED_HEADERS: [&str; 7] = [
    "host",
    "upgrade",
    "connection",
    "sec-websocket-key",
    "sec-websocket-version",
    "sec-websocket-protocol",
    "sec-websocket-extensions",
];

/// flwrs sink plugin that sends each event as a WebSocket message
#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
pub struct Args {
    #[command(flatten)]
    pub bootstrap: Bootstrap,

    /// The server: ws://… or wss://…
    #[arg(long)]
    pub url: String,

    /// JSON file: URL prefixes that events may name their own url within
    #[arg(long)]
    pub allowed_urls: Option<PathBuf>,

    /// JSON file: extra handshake headers, {"name": "value"} (Authorization comes from WS_AUTHORIZATION)
    #[arg(long)]
    pub headers: Option<PathBuf>,

    /// JSON file: subprotocols to offer, ["…"]
    #[arg(long)]
    pub subprotocols: Option<PathBuf>,

    /// The Origin header
    #[arg(long)]
    pub origin: Option<String>,

    /// PEM file: the CA(s) of the server's certificate (default: the bundled Mozilla roots)
    #[arg(long)]
    pub tls_ca: Option<PathBuf>,

    /// PEM file: a client certificate (mTLS)
    #[arg(long)]
    pub tls_cert: Option<PathBuf>,

    /// PEM file: the client certificate's key
    #[arg(long)]
    pub tls_key: Option<PathBuf>,

    /// Development only: accept any server certificate
    #[arg(long)]
    pub insecure_skip_verify: bool,

    /// Connect with the first event, not at start
    #[arg(long)]
    pub lazy_connect: bool,

    /// Seconds between pings; a ping unanswered by the next one means a dead connection
    #[arg(long, default_value_t = 20)]
    pub ping_interval_s: u32,

    /// Largest message, sent or received, in bytes
    #[arg(long, default_value_t = 16 * 1024 * 1024)]
    pub max_message_bytes: u32,

    /// Connections at most (one per URL)
    #[arg(long, default_value_t = 16)]
    pub max_connections: u32,

    /// Seconds after which an unused connection to an event's url is closed
    #[arg(long, default_value_t = 300)]
    pub idle_timeout_s: u32,

    /// Default message type when an event doesn't set one
    #[arg(long, value_enum, default_value = "text")]
    pub message_type: MessageType,

    /// When an event is acknowledged: once its message is written, or once the server replied
    #[arg(long, value_enum, default_value = "write")]
    pub ack: AckMode,

    /// ack reply: where the request's id goes in the outgoing JSON (generated ids)
    #[arg(long)]
    pub request_id_path: Option<String>,

    /// ack reply: where a reply's id is in the incoming JSON
    #[arg(long)]
    pub reply_id_path: Option<String>,

    /// ack reply: a reply with a value here (not null or false) is a failure
    #[arg(long)]
    pub reply_error_path: Option<String>,

    /// ack reply: milliseconds to wait for the reply (then a retryable NACK)
    #[arg(long, default_value_t = 10_000)]
    pub reply_timeout_ms: u32,

    /// Events handled at once (1 keeps the order)
    #[arg(long, default_value_t = 1)]
    pub max_concurrent: u32,
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageType {
    #[value(name = "text")]
    Text,
    #[value(name = "binary")]
    Binary,
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum AckMode {
    #[value(name = "write")]
    Write,
    #[value(name = "reply")]
    Reply,
}

#[derive(Debug, Clone)]
pub struct Reply {
    pub request_id_path: Option<JsonPath>,
    pub reply_id_path: JsonPath,
    pub reply_error_path: Option<JsonPath>,
    pub timeout: Duration,
}

#[derive(Debug, Clone)]
pub struct Settings {
    pub url: String,
    pub allowed_urls: Vec<String>,
    pub headers: Vec<(HeaderName, HeaderValue)>,
    pub subprotocols: Vec<String>,
    pub tls: Option<Arc<ClientConfig>>,
    pub insecure_skip_verify: bool,
    pub lazy_connect: bool,
    pub ping_interval: Duration,
    pub max_message_bytes: usize,
    pub max_connections: usize,
    pub idle_timeout: Duration,
    pub message_type: MessageType,
    pub reply: Option<Reply>,
    pub max_concurrent: u32,
}

/// A ws:// or wss:// URL with a host (checked loosely: the handshake parses it fully).
pub fn check_url(url: &str) -> Result<bool, String> {
    let lower = url.to_ascii_lowercase();
    let (tls, rest) = if let Some(rest) = lower.strip_prefix("wss://") {
        (true, rest)
    } else if let Some(rest) = lower.strip_prefix("ws://") {
        (false, rest)
    } else {
        return Err(format!("{url:?} must start with ws:// or wss://"));
    };
    let host = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if host.is_empty() || host.contains('@') || url.contains(char::is_whitespace) {
        return Err(format!(
            "{url:?} isn't a WebSocket URL with a host (and no credentials)"
        ));
    }
    Ok(tls)
}

/// Whether `url` is within `prefix`: the same, or below it at a `/` or `?`
/// boundary (so ws://a.com doesn't allow ws://a.com.evil.net).
pub fn within(prefix: &str, url: &str) -> bool {
    match url.strip_prefix(prefix) {
        Some("") => true,
        Some(rest) => prefix.ends_with('/') || rest.starts_with(['/', '?']),
        None => false,
    }
}

impl Args {
    pub fn settings(&self) -> Result<Settings, Vec<String>> {
        self.settings_with(std::env::var(AUTHORIZATION_ENV).ok())
    }

    pub fn settings_with(&self, authorization: Option<String>) -> Result<Settings, Vec<String>> {
        let mut problems = vec![];
        let mut any_tls = check_url(&self.url)
            .map_err(|e| problems.push(format!("url: {e}")))
            .unwrap_or(false);
        let allowed_urls: Vec<String> = self
            .allowed_urls
            .as_deref()
            .and_then(|p| {
                read_json(p)
                    .map_err(|e| problems.push(format!("--allowed-urls {e}")))
                    .ok()
            })
            .unwrap_or_default();
        for prefix in &allowed_urls {
            match check_url(prefix) {
                Ok(tls) => any_tls |= tls,
                Err(e) => problems.push(format!("allowed_urls: {e}")),
            }
        }
        let mut headers = vec![];
        let file: BTreeMap<String, String> = self
            .headers
            .as_deref()
            .and_then(|p| {
                read_json(p)
                    .map_err(|e| problems.push(format!("--headers {e}")))
                    .ok()
            })
            .unwrap_or_default();
        let authorization = authorization
            .filter(|a| !a.is_empty())
            .map(|a| ("authorization".to_string(), a));
        for (name, value) in file.into_iter().chain(authorization) {
            let lower = name.to_ascii_lowercase();
            if RESERVED_HEADERS.contains(&lower.as_str()) {
                problems.push(format!("headers: {name} is set by the WebSocket handshake"));
                continue;
            }
            match (
                HeaderName::try_from(lower.as_str()),
                HeaderValue::try_from(value.as_str()),
            ) {
                (Ok(n), Ok(v)) => headers.push((n, v)),
                _ => problems.push(format!(
                    "headers: {name:?} isn't a valid header name and value"
                )),
            }
        }
        if let Some(origin) = &self.origin {
            match HeaderValue::try_from(origin.as_str()) {
                Ok(v) => headers.push((HeaderName::from_static("origin"), v)),
                Err(_) => problems.push("origin isn't a valid header value".into()),
            }
        }
        let subprotocols: Vec<String> = self
            .subprotocols
            .as_deref()
            .and_then(|p| {
                read_json(p)
                    .map_err(|e| problems.push(format!("--subprotocols {e}")))
                    .ok()
            })
            .unwrap_or_default();
        for s in &subprotocols {
            if s.is_empty()
                || !s
                    .chars()
                    .all(|c| c.is_ascii_graphic() && !",;\"()<>@:/[]?={}\\".contains(c))
            {
                problems.push(format!("subprotocols: {s:?} isn't a token"));
            }
        }
        let tls_files = self.tls_ca.is_some() || self.tls_cert.is_some() || self.tls_key.is_some();
        let tls = if any_tls {
            client_config(&TlsFiles {
                ca: self.tls_ca.as_deref(),
                cert: self.tls_cert.as_deref(),
                key: self.tls_key.as_deref(),
                insecure_skip_verify: self.insecure_skip_verify,
            })
            .map_err(|e| problems.extend(e))
            .ok()
        } else {
            if tls_files || self.insecure_skip_verify {
                problems.push("TLS settings need a wss:// url (or allowed_urls)".into());
            }
            None
        };
        if self.ping_interval_s == 0 {
            problems.push("ping_interval_s must be at least 1".into());
        }
        if self.max_message_bytes < 1024 {
            problems.push("max_message_bytes must be at least 1024".into());
        }
        if self.max_connections == 0 {
            problems.push("max_connections must be at least 1".into());
        }
        if self.max_concurrent == 0 {
            problems.push("max_concurrent must be at least 1".into());
        }
        let mut path = |name: &str, p: &Option<String>| {
            p.as_deref().and_then(|p| {
                json_path::parse(p)
                    .map_err(|e| problems.push(format!("{name}: {e}")))
                    .ok()
            })
        };
        let request_id_path = path("request_id_path", &self.request_id_path);
        let reply_id_path = path("reply_id_path", &self.reply_id_path);
        let reply_error_path = path("reply_error_path", &self.reply_error_path);
        let reply = match self.ack {
            AckMode::Write => {
                if self.request_id_path.is_some()
                    || self.reply_id_path.is_some()
                    || self.reply_error_path.is_some()
                {
                    problems.push(
                        "request_id_path, reply_id_path and reply_error_path need ack: reply"
                            .into(),
                    );
                }
                None
            }
            AckMode::Reply => match reply_id_path {
                Some(reply_id_path) => Some(Reply {
                    request_id_path,
                    reply_id_path,
                    reply_error_path,
                    timeout: Duration::from_millis(self.reply_timeout_ms.max(1).into()),
                }),
                None => {
                    if self.reply_id_path.is_none() {
                        problems.push("ack: reply needs reply_id_path".into());
                    }
                    None
                }
            },
        };
        if !problems.is_empty() {
            return Err(problems);
        }
        Ok(Settings {
            url: self.url.clone(),
            allowed_urls,
            headers,
            subprotocols,
            tls,
            insecure_skip_verify: self.insecure_skip_verify,
            lazy_connect: self.lazy_connect,
            ping_interval: Duration::from_secs(self.ping_interval_s.into()),
            max_message_bytes: self.max_message_bytes as usize,
            max_connections: self.max_connections as usize,
            idle_timeout: Duration::from_secs(self.idle_timeout_s.into()),
            message_type: self.message_type,
            reply,
            max_concurrent: self.max_concurrent,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(extra: &[&str]) -> Args {
        let mut argv = vec!["ws", "--id", "n", "--port", "1"];
        argv.extend_from_slice(extra);
        Args::try_parse_from(argv).unwrap()
    }

    #[test]
    fn urls() {
        assert_eq!(check_url("ws://h/x"), Ok(false));
        assert_eq!(check_url("WSS://h:1?q"), Ok(true));
        for bad in ["http://h", "ws://", "ws:///x", "ws://u@h", "ws://h /x"] {
            assert!(check_url(bad).is_err(), "{bad}");
        }
        assert!(within("ws://a.com", "ws://a.com"));
        assert!(within("ws://a.com", "ws://a.com/x"));
        assert!(within("ws://a.com/api/", "ws://a.com/api/x"));
        assert!(!within("ws://a.com", "ws://a.com.evil.net"));
        assert!(!within("ws://a.com/api", "ws://a.com/apix"));
    }

    #[test]
    fn reads_the_settings() {
        let s = args(&["--url", "ws://h", "--origin", "https://me"])
            .settings_with(Some("Bearer t".into()))
            .unwrap();
        assert!(s.reply.is_none() && s.tls.is_none());
        let names: Vec<&str> = s.headers.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["authorization", "origin"]);
        let s = args(&[
            "--url",
            "wss://h",
            "--ack",
            "reply",
            "--reply-id-path",
            "id",
            "--request-id-path",
            "meta.id",
        ])
        .settings_with(None)
        .unwrap();
        assert!(s.tls.is_some());
        assert_eq!(s.reply.unwrap().request_id_path.unwrap().len(), 2);
    }

    #[test]
    fn reports_every_problem() {
        let dir = tempfile::tempdir().unwrap();
        let headers = dir.path().join("h.json");
        std::fs::write(&headers, r#"{"Upgrade": "x", "x-ok": "1"}"#).unwrap();
        let e = args(&[
            "--url",
            "http://h",
            "--headers",
            headers.to_str().unwrap(),
            "--tls-ca",
            "ca.pem",
            "--ack",
            "reply",
            "--max-connections",
            "0",
        ])
        .settings_with(None)
        .unwrap_err();
        for expected in [
            "url:",
            "Upgrade is set by",
            "need a wss://",
            "needs reply_id_path",
            "max_connections",
        ] {
            assert!(
                e.iter().any(|p| p.contains(expected)),
                "{expected:?} not in {e:?}"
            );
        }
        let e = args(&["--url", "ws://h", "--reply-id-path", "id"])
            .settings_with(None)
            .unwrap_err();
        assert!(e[0].contains("need ack: reply"));
    }
}
