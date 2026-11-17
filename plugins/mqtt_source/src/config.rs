//! The command line (see `schema/launch_args.json`), the password from the
//! environment, and their checks: every problem is reported, not just the
//! first.

use crate::tls::{TlsFiles, client_config};
use crate::topic::check_subscription;
use clap::{Parser, ValueEnum};
use flwrs_plugin::cli::{Bootstrap, read_json};
use rustls::ClientConfig;
use serde::Deserialize;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// The broker password's environment variable (a secret: never on the command line).
pub const PASSWORD_ENV: &str = "MQTT_PASSWORD";
/// MQTT 5: how long the broker keeps a persistent session by default.
pub const DEFAULT_SESSION_EXPIRY_S: u32 = 3600;

/// flwrs source plugin that turns MQTT messages into events
#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
pub struct Args {
    #[command(flatten)]
    pub bootstrap: Bootstrap,

    /// The broker: mqtt://host[:1883] or mqtts://host[:8883]
    #[arg(long)]
    pub broker_url: String,

    /// MQTT protocol version
    #[arg(long, value_enum, default_value = "5")]
    pub protocol_version: ProtocolVersion,

    /// JSON file: the subscriptions, [{"filter", "qos", "no_local", "retain_as_published", "retain_handling"}]
    #[arg(long)]
    pub subscriptions: PathBuf,

    /// When a message is acknowledged to the broker: once the hub has the event, or once it's sent
    #[arg(long, value_enum, default_value = "confirmed")]
    pub ack_mode: AckMode,

    /// Client id (unique per broker); default flwrs-<instance id>
    #[arg(long)]
    pub client_id: Option<String>,

    /// Start without the broker session (messages queued while away are lost)
    #[arg(long)]
    pub clean_start: bool,

    /// MQTT 5: how long the broker keeps the session after a disconnect (default 3600)
    #[arg(long)]
    pub session_expiry_s: Option<u32>,

    /// Seconds between keepalive pings
    #[arg(long, default_value_t = 30)]
    pub keepalive_s: u16,

    /// Broker user name (the password comes from MQTT_PASSWORD)
    #[arg(long)]
    pub username: Option<String>,

    /// PEM file: the CA(s) of the broker's certificate (default: the bundled Mozilla roots)
    #[arg(long)]
    pub tls_ca: Option<PathBuf>,

    /// PEM file: a client certificate (mTLS)
    #[arg(long)]
    pub tls_cert: Option<PathBuf>,

    /// PEM file: the client certificate's key
    #[arg(long)]
    pub tls_key: Option<PathBuf>,

    /// Development only: accept any broker certificate
    #[arg(long)]
    pub insecure_skip_verify: bool,

    /// QoS 1 / 2 messages the broker may send before they're acknowledged (MQTT 5 Receive Maximum)
    #[arg(long, default_value_t = 64)]
    pub max_inflight: u16,

    /// QoS 0 messages kept while the hub can't take events; more are dropped
    #[arg(long, default_value_t = 1024)]
    pub max_qos0_buffered: u32,

    /// Largest MQTT packet the broker may send, in bytes
    #[arg(long, default_value_t = 1_048_576)]
    pub max_packet_bytes: u32,

    /// JSON file: the Last Will, {"topic", "payload", "qos", "retain"}
    #[arg(long)]
    pub last_will: Option<PathBuf>,
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtocolVersion {
    #[value(name = "5")]
    V5,
    #[value(name = "3.1.1")]
    V311,
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum AckMode {
    /// After the hub confirmed the event (at least once, end to end).
    #[value(name = "confirmed")]
    Confirmed,
    /// Once the event is sent to the hub (faster; a crash can lose events in flight).
    #[value(name = "on_emit")]
    OnEmit,
}

/// MQTT 5 retain handling: when the broker sends retained messages.
#[derive(Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum RetainHandling {
    /// At every subscribe.
    #[default]
    Send,
    /// Only when the subscription is new.
    NewOnly,
    Never,
}

#[derive(Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Subscription {
    pub filter: String,
    #[serde(default = "one")]
    pub qos: u8,
    /// MQTT 5: not the messages this client publishes itself.
    #[serde(default)]
    pub no_local: bool,
    /// MQTT 5: keep the retain flag as published (else it's set only on
    /// retained messages sent at subscribe time).
    #[serde(default)]
    pub retain_as_published: bool,
    #[serde(default)]
    pub retain_handling: RetainHandling,
}

fn one() -> u8 {
    1
}

#[derive(Deserialize, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct LastWill {
    pub topic: String,
    #[serde(default)]
    pub payload: String,
    #[serde(default)]
    pub qos: u8,
    #[serde(default)]
    pub retain: bool,
}

#[derive(Debug, Clone)]
pub struct Settings {
    pub host: String,
    pub port: u16,
    pub tls: Option<Arc<ClientConfig>>,
    pub insecure_skip_verify: bool,
    pub version: ProtocolVersion,
    pub subscriptions: Vec<Subscription>,
    pub ack_mode: AckMode,
    pub client_id: String,
    pub clean_start: bool,
    pub session_expiry_s: Option<u32>,
    pub keepalive: Duration,
    pub credentials: Option<(String, String)>,
    pub max_inflight: u16,
    pub max_qos0_buffered: usize,
    pub max_packet_bytes: u32,
    pub last_will: Option<LastWill>,
}

/// `mqtt://host[:port]` / `mqtts://host[:port]` → (host, port, tls).
pub fn parse_broker_url(url: &str) -> Result<(String, u16, bool), String> {
    let bad = || format!("--broker-url {url:?} must be mqtt://host[:port] or mqtts://host[:port]");
    let (scheme, rest) = url.split_once("://").ok_or_else(bad)?;
    let (tls, default_port) = match scheme.to_ascii_lowercase().as_str() {
        "mqtt" | "tcp" => (false, 1883),
        "mqtts" | "ssl" => (true, 8883),
        _ => return Err(bad()),
    };
    let rest = rest.strip_suffix('/').unwrap_or(rest);
    if rest.is_empty() || rest.contains(['/', '?', '#', '@']) {
        return Err(bad());
    }
    let (host, port) = if let Some(v6) = rest.strip_prefix('[') {
        let (host, after) = v6.split_once(']').ok_or_else(bad)?;
        match after.strip_prefix(':') {
            Some(port) => (host, Some(port)),
            None if after.is_empty() => (host, None),
            None => return Err(bad()),
        }
    } else {
        match rest.rsplit_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (rest, None),
        }
    };
    let port = match port {
        Some(p) => p.parse::<u16>().ok().filter(|p| *p > 0).ok_or_else(bad)?,
        None => default_port,
    };
    if host.is_empty() {
        return Err(bad());
    }
    Ok((host.to_string(), port, tls))
}

impl Args {
    pub fn settings(&self) -> Result<Settings, Vec<String>> {
        self.settings_with(std::env::var(PASSWORD_ENV).ok())
    }

    /// The settings, with the password given (from [`PASSWORD_ENV`]).
    pub fn settings_with(&self, password: Option<String>) -> Result<Settings, Vec<String>> {
        let mut problems = vec![];
        let broker = parse_broker_url(&self.broker_url)
            .map_err(|e| problems.push(e))
            .ok();
        let v5 = self.protocol_version == ProtocolVersion::V5;
        let tls_files = self.tls_ca.is_some() || self.tls_cert.is_some() || self.tls_key.is_some();
        let tls = match &broker {
            Some((_, _, true)) => client_config(&TlsFiles {
                ca: self.tls_ca.as_deref(),
                cert: self.tls_cert.as_deref(),
                key: self.tls_key.as_deref(),
                insecure_skip_verify: self.insecure_skip_verify,
            })
            .map_err(|e| problems.extend(e))
            .ok(),
            Some((_, _, false)) if tls_files || self.insecure_skip_verify => {
                problems.push("TLS settings need an mqtts:// broker_url".into());
                None
            }
            _ => None,
        };
        let subscriptions: Vec<Subscription> = read_json(&self.subscriptions)
            .map_err(|e| problems.push(format!("--subscriptions {e}")))
            .unwrap_or_default();
        if subscriptions.is_empty() && problems.is_empty() {
            problems.push("subscriptions: at least one is needed".into());
        }
        for (i, s) in subscriptions.iter().enumerate() {
            if let Err(e) = check_subscription(&s.filter) {
                problems.push(format!("subscriptions[{i}]: {e}"));
            }
            if s.qos > 2 {
                problems.push(format!("subscriptions[{i}].qos must be 0, 1 or 2"));
            }
            let v5_only =
                s.no_local || s.retain_as_published || s.retain_handling != RetainHandling::Send;
            if v5_only && !v5 {
                problems.push(format!(
                    "subscriptions[{i}]: no_local, retain_as_published and retain_handling are MQTT 5 only"
                ));
            }
        }
        let client_id = self
            .client_id
            .clone()
            .filter(|c| !c.is_empty())
            .unwrap_or_else(|| format!("flwrs-{}", self.bootstrap.id));
        let session_expiry_s = match (self.session_expiry_s, v5) {
            (Some(_), false) => {
                problems.push("session_expiry_s is MQTT 5 only".into());
                None
            }
            (Some(s), true) => Some(s),
            // A persistent session needs an expiry in MQTT 5 (0 = it ends at the disconnect).
            (None, true) if !self.clean_start => Some(DEFAULT_SESSION_EXPIRY_S),
            (None, _) => None,
        };
        if self.keepalive_s < 5 {
            problems.push("keepalive_s must be at least 5".into());
        }
        let password = password.filter(|p| !p.is_empty());
        let credentials = match (&self.username, password) {
            (Some(user), password) if !user.is_empty() => {
                Some((user.clone(), password.unwrap_or_default()))
            }
            (_, Some(_)) => {
                problems.push(format!("{PASSWORD_ENV} is set but username isn't"));
                None
            }
            _ => None,
        };
        if self.max_inflight == 0 {
            problems.push("max_inflight must be at least 1".into());
        }
        if self.max_packet_bytes < 128 {
            problems.push("max_packet_bytes must be at least 128".into());
        }
        let last_will = self.last_will.as_deref().and_then(|p| {
            read_json::<LastWill>(p)
                .map_err(|e| problems.push(format!("--last-will {e}")))
                .ok()
        });
        if let Some(will) = &last_will {
            if will.topic.is_empty() || will.topic.contains(['+', '#', '\0']) {
                problems.push("last_will.topic must be a topic name (no wildcards)".into());
            }
            if will.qos > 2 {
                problems.push("last_will.qos must be 0, 1 or 2".into());
            }
        }
        match broker {
            Some((host, port, _)) if problems.is_empty() => Ok(Settings {
                host,
                port,
                tls,
                insecure_skip_verify: self.insecure_skip_verify,
                version: self.protocol_version,
                subscriptions,
                ack_mode: self.ack_mode,
                client_id,
                clean_start: self.clean_start,
                session_expiry_s,
                keepalive: Duration::from_secs(self.keepalive_s.into()),
                credentials,
                max_inflight: self.max_inflight,
                max_qos0_buffered: self.max_qos0_buffered as usize,
                max_packet_bytes: self.max_packet_bytes,
                last_will,
            }),
            _ => Err(problems),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(subscriptions: &str, extra: &[&str]) -> (tempfile::TempDir, Args) {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("subs.json");
        std::fs::write(&file, subscriptions).unwrap();
        let file = file.to_string_lossy().into_owned();
        let mut argv = vec![
            "src",
            "--id",
            "n1",
            "--port",
            "1",
            "--broker-url",
            "mqtt://b",
            "--subscriptions",
            &file,
        ];
        argv.extend_from_slice(extra);
        let args = Args::try_parse_from(argv).unwrap();
        (dir, args)
    }

    #[test]
    fn reads_the_settings() {
        let (_d, a) = args(
            r#"[{"filter": "a/#"}, {"filter": "$share/g/b/+", "qos": 2, "no_local": true}]"#,
            &[],
        );
        let s = a.settings_with(None).unwrap();
        assert_eq!(s.subscriptions[0].qos, 1);
        assert_eq!(s.subscriptions[1].qos, 2);
        assert_eq!(s.client_id, "flwrs-n1");
        assert!(!s.clean_start);
        assert_eq!(s.session_expiry_s, Some(DEFAULT_SESSION_EXPIRY_S));
        assert_eq!(s.ack_mode, AckMode::Confirmed);
        let (_d, a) = args(
            r#"[{"filter": "a"}]"#,
            &["--clean-start", "--ack-mode", "on_emit"],
        );
        let s = a.settings_with(None).unwrap();
        assert_eq!((s.session_expiry_s, s.ack_mode), (None, AckMode::OnEmit));
        let (_d, a) = args(r#"[{"filter": "a"}]"#, &["--protocol-version", "3.1.1"]);
        assert_eq!(a.settings_with(None).unwrap().session_expiry_s, None);
    }

    #[test]
    fn reports_every_problem() {
        let (_d, a) = args(
            r#"[{"filter": "a/#/b"}, {"filter": "c", "qos": 3}, {"filter": "d", "retain_handling": "never"}]"#,
            &[
                "--protocol-version",
                "3.1.1",
                "--session-expiry-s",
                "5",
                "--keepalive-s",
                "1",
                "--tls-ca",
                "x.pem",
            ],
        );
        let e = a.settings_with(Some("p".into())).unwrap_err();
        for expected in [
            "subscriptions[0]",
            "subscriptions[1].qos",
            "subscriptions[2]: no_local",
            "session_expiry_s is MQTT 5 only",
            "keepalive_s",
            "need an mqtts://",
            "MQTT_PASSWORD is set",
        ] {
            assert!(
                e.iter().any(|p| p.contains(expected)),
                "{expected:?} not in {e:?}"
            );
        }
        let (_d, a) = args("[]", &[]);
        assert!(a.settings_with(None).unwrap_err()[0].contains("at least one"));
        let (_d, a) = args(r#"[{"filter": "a", "bogus": 1}]"#, &[]);
        assert!(a.settings_with(None).unwrap_err()[0].contains("--subscriptions"));
    }
}
