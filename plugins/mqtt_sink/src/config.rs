//! The command line (see `schema/launch_args.json`), the password from the
//! environment, and their checks: every problem is reported, not just the
//! first.

use crate::tls::{TlsFiles, client_config};
use crate::topic::{check_filter, check_topic};
use clap::{Parser, ValueEnum};
use flwrs_plugin::cli::{Bootstrap, read_json};
use rustls::ClientConfig;
use serde::Deserialize;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// The broker password's environment variable (a secret: never on the command line).
pub const PASSWORD_ENV: &str = "MQTT_PASSWORD";

/// flwrs sink plugin that publishes each event to an MQTT broker
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

    /// Client id (unique per broker); default flwrs-<instance id>
    #[arg(long)]
    pub client_id: Option<String>,

    /// Keep the broker session across connections (clean start off)
    #[arg(long)]
    pub keep_session: bool,

    /// MQTT 5: how long the broker keeps the session after a disconnect
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

    /// QoS 1 / 2 messages awaiting the broker's answer at once (1 keeps the order strictly)
    #[arg(long, default_value_t = 16)]
    pub max_inflight: u16,

    /// Largest MQTT packet, in bytes (a bigger message is rejected)
    #[arg(long, default_value_t = 1_048_576)]
    pub max_packet_bytes: u32,

    /// JSON file: the Last Will, {"topic", "payload", "qos", "retain"}
    #[arg(long)]
    pub last_will: Option<PathBuf>,

    /// Topic for events that don't name one
    #[arg(long)]
    pub default_topic: Option<String>,

    /// QoS for events that don't set one
    #[arg(long, default_value_t = 1)]
    pub default_qos: u8,

    /// JSON file: the topic filters events may name a topic within (+ and # allowed)
    #[arg(long)]
    pub allowed_topics: Option<PathBuf>,
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtocolVersion {
    #[value(name = "5")]
    V5,
    #[value(name = "3.1.1")]
    V311,
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
    pub client_id: String,
    pub clean_start: bool,
    pub session_expiry_s: Option<u32>,
    pub keepalive: Duration,
    pub credentials: Option<(String, String)>,
    pub max_inflight: u16,
    pub max_packet_bytes: u32,
    pub last_will: Option<LastWill>,
    pub default_topic: Option<String>,
    pub default_qos: u8,
    pub allowed_topics: Vec<String>,
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
    // [v6]:port, host:port, or a host alone.
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
        let client_id = self
            .client_id
            .clone()
            .filter(|c| !c.is_empty())
            .unwrap_or_else(|| format!("flwrs-{}", self.bootstrap.id));
        if self.session_expiry_s.is_some() && !v5 {
            problems.push("session_expiry_s is MQTT 5 only".into());
        }
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
            if let Err(e) = check_topic(&will.topic) {
                problems.push(format!("last_will.topic: {e}"));
            }
            if will.qos > 2 {
                problems.push("last_will.qos must be 0, 1 or 2".into());
            }
        }
        let default_topic = self.default_topic.clone().filter(|t| !t.is_empty());
        if let Some(topic) = &default_topic
            && let Err(e) = check_topic(topic)
        {
            problems.push(format!("default_topic: {e}"));
        }
        if self.default_qos > 2 {
            problems.push("default_qos must be 0, 1 or 2".into());
        }
        let allowed_topics: Vec<String> = self
            .allowed_topics
            .as_deref()
            .and_then(|p| {
                read_json::<Vec<String>>(p)
                    .map_err(|e| problems.push(format!("--allowed-topics {e}")))
                    .ok()
            })
            .unwrap_or_default();
        for filter in &allowed_topics {
            if let Err(e) = check_filter(filter) {
                problems.push(format!("allowed_topics: {e}"));
            }
        }
        if default_topic.is_none() && allowed_topics.is_empty() {
            problems.push(
                "set default_topic, or allowed_topics for events that name their topic".into(),
            );
        }
        match broker {
            Some((host, port, _)) if problems.is_empty() => Ok(Settings {
                host,
                port,
                tls,
                insecure_skip_verify: self.insecure_skip_verify,
                version: self.protocol_version,
                client_id,
                clean_start: !self.keep_session,
                session_expiry_s: self.session_expiry_s,
                keepalive: Duration::from_secs(self.keepalive_s.into()),
                credentials,
                max_inflight: self.max_inflight,
                max_packet_bytes: self.max_packet_bytes,
                last_will,
                default_topic,
                default_qos: self.default_qos,
                allowed_topics,
            }),
            _ => Err(problems),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(extra: &[&str]) -> Args {
        let mut argv = vec!["mqtt-sink", "--id", "n1", "--port", "1"];
        argv.extend_from_slice(extra);
        Args::try_parse_from(argv).unwrap()
    }

    #[test]
    fn broker_urls() {
        assert_eq!(
            parse_broker_url("mqtt://h").unwrap(),
            ("h".into(), 1883, false)
        );
        assert_eq!(
            parse_broker_url("mqtts://h:1").unwrap(),
            ("h".into(), 1, true)
        );
        assert_eq!(
            parse_broker_url("MQTT://h:2/").unwrap(),
            ("h".into(), 2, false)
        );
        assert_eq!(
            parse_broker_url("mqtt://[::1]:3").unwrap(),
            ("::1".into(), 3, false)
        );
        assert_eq!(
            parse_broker_url("mqtts://[::1]").unwrap(),
            ("::1".into(), 8883, true)
        );
        for bad in [
            "h:1",
            "http://h",
            "mqtt://",
            "mqtt://h:0",
            "mqtt://h:x",
            "mqtt://u@h",
            "mqtt://h/t",
        ] {
            assert!(parse_broker_url(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn reads_the_settings() {
        let s = args(&[
            "--broker-url",
            "mqtt://b",
            "--default-topic",
            "t",
            "--username",
            "u",
        ])
        .settings_with(Some("p".into()))
        .unwrap();
        assert_eq!((s.host.as_str(), s.port), ("b", 1883));
        assert_eq!(s.client_id, "flwrs-n1");
        assert_eq!(s.credentials, Some(("u".into(), "p".into())));
        assert!(s.clean_start && s.tls.is_none());
        let s = args(&[
            "--broker-url",
            "mqtts://b",
            "--default-topic",
            "t",
            "--keep-session",
        ])
        .settings_with(None)
        .unwrap();
        assert!(s.tls.is_some() && !s.clean_start);
    }

    #[test]
    fn reports_every_problem() {
        let dir = tempfile::tempdir().unwrap();
        let filters = dir.path().join("allowed.json");
        std::fs::write(&filters, r#"["a/#", "b/#/c"]"#).unwrap();
        let e = args(&[
            "--broker-url",
            "mqtt://b",
            "--protocol-version",
            "3.1.1",
            "--session-expiry-s",
            "5",
            "--keepalive-s",
            "1",
            "--default-topic",
            "x/+",
            "--default-qos",
            "3",
            "--tls-ca",
            "ca.pem",
            "--allowed-topics",
            filters.to_str().unwrap(),
        ])
        .settings_with(Some("p".into()))
        .unwrap_err();
        for expected in [
            "MQTT 5 only",
            "keepalive_s",
            "default_topic",
            "default_qos",
            "need an mqtts://",
            "MQTT_PASSWORD is set",
            "b/#/c",
        ] {
            assert!(
                e.iter().any(|p| p.contains(expected)),
                "{expected:?} not in {e:?}"
            );
        }
        let e = args(&["--broker-url", "mqtt://b"])
            .settings_with(None)
            .unwrap_err();
        assert!(e[0].contains("default_topic, or allowed_topics"));
    }
}
