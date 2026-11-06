//! `[telemetry]` config: optional sinks for the server's own stats.

use flwrs_core::config::{self, main_config};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct TelemetryConfig {
    /// Export to an OpenTelemetry collector (OTLP/HTTP). Present = on.
    pub otlp: Option<OtlpConfig>,
    /// Local files (`[[telemetry.file]]`), one entry per output format.
    #[serde(rename = "file")]
    pub files: Vec<FileConfig>,
}

/// The format of a file sink; each has its own file extension, so several
/// formats can share a folder.
#[derive(Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum FileFormat {
    /// OTLP JSON Lines (`.jsonl`): one OTLP `MetricsData` per line, as the
    /// OpenTelemetry file exporter writes it. Needs the `otel` feature.
    OtlpJson,
    /// SQLite (`.db`), with the schema documented in README.md.
    Sqlite,
    /// InfluxDB line protocol (`.lp`).
    Influx,
    /// OpenMetrics text (`.om`), written when a partition closes.
    Openmetrics,
}

#[cfg_attr(not(feature = "telemetry-file"), allow(dead_code))]
impl FileFormat {
    pub(crate) fn name(self) -> &'static str {
        match self {
            FileFormat::OtlpJson => "otlp-json",
            FileFormat::Sqlite => "sqlite",
            FileFormat::Influx => "influx",
            FileFormat::Openmetrics => "openmetrics",
        }
    }
}

/// A `[[telemetry.file]]` entry. Durations are strings like `30m`, `6h`, `1d`.
#[derive(Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub(crate) struct FileConfig {
    pub format: FileFormat,
    /// Folder for the files; created if missing.
    #[serde(default = "default_file_dir")]
    pub dir: PathBuf,
    /// How often stats are collected.
    #[serde(default = "default_file_interval_ms")]
    pub interval_ms: u64,
    /// How often collected stats are written (fewer, bigger writes wear
    /// flash storage less). A crash loses at most this much.
    #[serde(default = "default_flush_interval_ms")]
    pub flush_interval_ms: u64,
    /// Length of one file (partition), aligned to UTC.
    #[serde(default = "default_partition")]
    pub partition: String,
    /// Files (partitions) kept, the current one included; the oldest go
    /// first. 0 = no limit.
    #[serde(default = "default_max_files")]
    pub max_files: u32,
    /// The oldest closed files are deleted while this entry's files take
    /// more than this many bytes. 0 = no limit.
    #[serde(default = "default_max_bytes")]
    pub max_bytes: u64,
    /// Files whose partition ended longer ago than this are deleted.
    #[serde(default)]
    pub max_age: Option<String>,
    /// gzip closed partitions of the text formats (not SQLite).
    #[serde(default = "default_compress")]
    pub compress: bool,
}

fn default_file_dir() -> PathBuf {
    PathBuf::from("./telemetry")
}
fn default_file_interval_ms() -> u64 {
    10_000
}
fn default_flush_interval_ms() -> u64 {
    60_000
}
fn default_partition() -> String {
    "1d".into()
}
/// Daily files for a month.
fn default_max_files() -> u32 {
    30
}
/// 1 GiB.
fn default_max_bytes() -> u64 {
    1 << 30
}
fn default_compress() -> bool {
    true
}

/// A file sink's settings, checked.
#[cfg_attr(not(feature = "telemetry-file"), allow(dead_code))]
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct FileSink {
    pub format: FileFormat,
    pub dir: PathBuf,
    pub interval: Duration,
    pub flush_interval: Duration,
    pub partition: Duration,
    /// `None` = no limit (configured as 0).
    pub max_files: Option<u32>,
    pub max_bytes: Option<u64>,
    pub max_age: Option<Duration>,
    pub compress: bool,
}

/// The shortest partition (and the unit partitions are aligned on).
#[cfg_attr(not(feature = "telemetry-file"), allow(dead_code))]
pub(crate) const MIN_PARTITION: Duration = Duration::from_secs(60);

#[cfg_attr(not(feature = "telemetry-file"), allow(dead_code))]
impl FileConfig {
    pub(crate) fn resolve(&self) -> Result<FileSink, String> {
        let partition = parse_duration(&self.partition).map_err(|e| format!("partition: {e}"))?;
        if partition < MIN_PARTITION || partition.as_secs() % 60 != 0 {
            return Err(format!(
                "partition: {:?} must be whole minutes, at least 1m",
                self.partition
            ));
        }
        let max_age = self
            .max_age
            .as_deref()
            .map(parse_duration)
            .transpose()
            .map_err(|e| format!("max_age: {e}"))?;
        let interval = Duration::from_millis(self.interval_ms.max(1_000));
        Ok(FileSink {
            format: self.format,
            dir: self.dir.clone(),
            interval,
            flush_interval: Duration::from_millis(self.flush_interval_ms).max(interval),
            partition,
            max_files: (self.max_files > 0).then_some(self.max_files),
            max_age,
            max_bytes: (self.max_bytes > 0).then_some(self.max_bytes),
            compress: self.compress,
        })
    }
}

/// `90s`, `30m`, `6h`, `1d`, `2w`.
#[cfg_attr(not(feature = "telemetry-file"), allow(dead_code))]
pub(crate) fn parse_duration(text: &str) -> Result<Duration, String> {
    let text = text.trim();
    let split = text
        .find(|c: char| !c.is_ascii_digit())
        .ok_or_else(|| format!("{text:?} has no unit (s, m, h, d, w)"))?;
    let (number, unit) = text.split_at(split);
    let n: u64 = number
        .parse()
        .map_err(|_| format!("{text:?} doesn't start with a whole number"))?;
    let seconds = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3_600,
        "d" => 86_400,
        "w" => 604_800,
        _ => return Err(format!("{text:?}: unknown unit {unit:?} (s, m, h, d, w)")),
    };
    n.checked_mul(seconds)
        .filter(|&s| s > 0)
        .map(Duration::from_secs)
        .ok_or_else(|| format!("{text:?} is out of range"))
}

impl TelemetryConfig {
    pub(crate) fn read() -> Self {
        // A missing or invalid section falls back to the defaults (no sinks;
        // errors are printed by the config reader).
        config::read_struct(main_config(), &["telemetry".to_string()]).unwrap_or_default()
    }
}

#[derive(Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Temporality {
    /// Totals since the start (OTLP default; Prometheus-style backends).
    Cumulative,
    /// Change since the last export (Datadog / Dynatrace-style backends).
    Delta,
}

/// Every key is optional; unset keys fall back to the standard
/// `OTEL_*` environment variables, then to the defaults.
#[derive(Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct OtlpConfig {
    /// Full URL of the metrics endpoint, e.g. `http://collector:4318/v1/metrics`.
    pub endpoint: Option<String>,
    /// Extra HTTP headers, e.g. `Authorization`. Values aren't logged.
    pub headers: BTreeMap<String, String>,
    pub interval_ms: Option<u64>,
    pub timeout_ms: Option<u64>,
    pub temporality: Option<Temporality>,
    pub service_name: Option<String>,
    /// Identifies this server; default: a new id per start.
    pub service_instance_id: Option<String>,
    /// Extra resource attributes, e.g. `deployment.environment = "edge"`.
    pub resource: BTreeMap<String, String>,
}

pub(crate) const DEFAULT_ENDPOINT: &str = "http://localhost:4318/v1/metrics";
pub(crate) const DEFAULT_INTERVAL_MS: u64 = 60_000;
pub(crate) const DEFAULT_TIMEOUT_MS: u64 = 10_000;

/// The OTLP settings with config, environment and defaults applied.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Otlp {
    pub endpoint: String,
    pub headers: BTreeMap<String, String>,
    pub interval_ms: u64,
    pub timeout_ms: u64,
    pub temporality: Temporality,
    /// Resource attributes, including `service.*`.
    pub resource: BTreeMap<String, String>,
    /// Problems worth a WARN (e.g. an unsupported protocol in the environment).
    pub warnings: Vec<String>,
}

// Without the otel feature, `[telemetry.otlp]` is still parsed (to warn
// that it's ignored), but never resolved.
#[cfg_attr(not(feature = "otel"), allow(dead_code))]
impl OtlpConfig {
    /// Precedence like the OpenTelemetry SDKs: this config, then the
    /// metrics-specific variable, then the generic one, then the default.
    pub(crate) fn resolve(&self, env: impl Fn(&str) -> Option<String>) -> Otlp {
        let mut warnings = vec![];
        let endpoint = self
            .endpoint
            .clone()
            .or_else(|| env("OTEL_EXPORTER_OTLP_METRICS_ENDPOINT"))
            .or_else(|| {
                env("OTEL_EXPORTER_OTLP_ENDPOINT")
                    .map(|base| format!("{}/v1/metrics", base.trim_end_matches('/')))
            })
            .unwrap_or_else(|| DEFAULT_ENDPOINT.to_string());
        if let Some(protocol) = env("OTEL_EXPORTER_OTLP_METRICS_PROTOCOL")
            .or_else(|| env("OTEL_EXPORTER_OTLP_PROTOCOL"))
            && protocol != "http/protobuf"
        {
            warnings.push(format!(
                "OTLP protocol {protocol:?} isn't supported; using http/protobuf"
            ));
        }
        let mut headers = BTreeMap::new();
        for var in [
            "OTEL_EXPORTER_OTLP_HEADERS",
            "OTEL_EXPORTER_OTLP_METRICS_HEADERS",
        ] {
            if let Some(list) = env(var) {
                headers.extend(parse_list(&list));
            }
        }
        headers.extend(self.headers.clone());
        let millis = |own: Option<u64>, vars: &[&str], default: u64| {
            own.or_else(|| vars.iter().find_map(|v| env(v)?.trim().parse().ok()))
                .unwrap_or(default)
        };
        let interval_ms = millis(
            self.interval_ms,
            &["OTEL_METRIC_EXPORT_INTERVAL"],
            DEFAULT_INTERVAL_MS,
        )
        .max(1_000);
        let timeout_ms = millis(
            self.timeout_ms,
            &[
                "OTEL_EXPORTER_OTLP_METRICS_TIMEOUT",
                "OTEL_EXPORTER_OTLP_TIMEOUT",
            ],
            DEFAULT_TIMEOUT_MS,
        )
        .max(100);
        let temporality = self.temporality.unwrap_or_else(|| {
            match env("OTEL_EXPORTER_OTLP_METRICS_TEMPORALITY_PREFERENCE")
                .map(|v| v.to_ascii_lowercase())
                .as_deref()
            {
                // `lowmemory` means delta for counters, which is all we send as sums.
                Some("delta") | Some("lowmemory") => Temporality::Delta,
                _ => Temporality::Cumulative,
            }
        });
        let mut resource = BTreeMap::new();
        if let Some(list) = env("OTEL_RESOURCE_ATTRIBUTES") {
            resource.extend(parse_list(&list));
        }
        resource.extend(self.resource.clone());
        let service_name = self
            .service_name
            .clone()
            .or_else(|| env("OTEL_SERVICE_NAME"))
            .or_else(|| resource.get("service.name").cloned())
            .unwrap_or_else(|| "flwrs".into());
        resource.insert("service.name".into(), service_name);
        resource.insert("service.version".into(), env!("CARGO_PKG_VERSION").into());
        let instance = self
            .service_instance_id
            .clone()
            .or_else(|| resource.get("service.instance.id").cloned())
            .unwrap_or_else(|| ulid::Ulid::generate().to_string());
        resource.insert("service.instance.id".into(), instance);
        Otlp {
            endpoint,
            headers,
            interval_ms,
            timeout_ms,
            temporality,
            resource,
            warnings,
        }
    }
}

/// `k1=v1,k2=v2` with percent-encoded values (the W3C Baggage style the
/// OTEL variables use).
fn parse_list(list: &str) -> Vec<(String, String)> {
    list.split(',')
        .filter_map(|pair| {
            let (k, v) = pair.split_once('=')?;
            let k = k.trim();
            (!k.is_empty()).then(|| (k.to_string(), percent_decode(v.trim())))
        })
        .collect()
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(hex) = s.get(i + 1..i + 3)
            && let Ok(b) = u8::from_str_radix(hex, 16)
        {
            out.push(b);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let vars: BTreeMap<String, String> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k| vars.get(k).cloned()
    }

    #[test]
    fn file_sinks() {
        let config: TelemetryConfig = toml::from_str(
            r#"
            [[file]]
            format = "sqlite"
            dir = "x"
            partition = "6h"
            max_age = "2w"

            [[file]]
            format = "otlp-json"
            "#,
        )
        .unwrap();
        let sqlite = config.files[0].resolve().unwrap();
        assert_eq!(sqlite.format, FileFormat::Sqlite);
        assert_eq!(sqlite.partition, Duration::from_secs(6 * 3_600));
        assert_eq!(sqlite.max_age, Some(Duration::from_secs(14 * 86_400)));
        assert_eq!(sqlite.interval, Duration::from_secs(10));
        assert_eq!(sqlite.flush_interval, Duration::from_secs(60));
        let otlp = config.files[1].resolve().unwrap();
        assert_eq!(
            (otlp.max_files, otlp.max_bytes, otlp.compress),
            (Some(30), Some(1 << 30), true),
            "defaults: a month of daily files, 1 GiB"
        );
        assert_eq!(otlp.partition, Duration::from_secs(86_400));

        let mut bad = config.files[0].clone();
        bad.partition = "30s".into();
        assert!(bad.resolve().unwrap_err().contains("at least 1m"));
        bad.partition = "1y".into();
        assert!(bad.resolve().unwrap_err().contains("unknown unit"));
        let mut unlimited = config.files[0].clone();
        (unlimited.max_files, unlimited.max_bytes) = (0, 0);
        let unlimited = unlimited.resolve().unwrap();
        assert_eq!(
            (unlimited.max_files, unlimited.max_bytes),
            (None, None),
            "0 = no limit"
        );
        assert!(
            toml::from_str::<TelemetryConfig>("[[file]]\nformat = \"csv\"").is_err(),
            "unknown format"
        );
        assert!(
            toml::from_str::<TelemetryConfig>("[[file]]\nformat = \"influx\"\nrotate = 1").is_err()
        );
    }

    #[test]
    fn defaults() {
        let o = OtlpConfig::default().resolve(env(&[]));
        assert_eq!(o.endpoint, DEFAULT_ENDPOINT);
        assert_eq!((o.interval_ms, o.timeout_ms), (60_000, 10_000));
        assert_eq!(o.temporality, Temporality::Cumulative);
        assert_eq!(o.resource["service.name"], "flwrs");
        assert_eq!(o.resource["service.instance.id"].len(), 26, "a ULID");
        assert!(o.warnings.is_empty());
    }

    #[test]
    fn environment_then_config_wins() {
        let vars = env(&[
            ("OTEL_EXPORTER_OTLP_ENDPOINT", "https://otel.example:4318/"),
            ("OTEL_EXPORTER_OTLP_HEADERS", "api-key=abc%3D,x-team=edge"),
            ("OTEL_METRIC_EXPORT_INTERVAL", "15000"),
            ("OTEL_EXPORTER_OTLP_METRICS_TEMPORALITY_PREFERENCE", "Delta"),
            (
                "OTEL_RESOURCE_ATTRIBUTES",
                "deployment.environment=prod,service.instance.id=gw-1",
            ),
            ("OTEL_SERVICE_NAME", "plant-7"),
            ("OTEL_EXPORTER_OTLP_PROTOCOL", "grpc"),
        ]);
        let o = OtlpConfig::default().resolve(&vars);
        assert_eq!(o.endpoint, "https://otel.example:4318/v1/metrics");
        assert_eq!(o.headers["api-key"], "abc=");
        assert_eq!(o.interval_ms, 15_000);
        assert_eq!(o.temporality, Temporality::Delta);
        assert_eq!(o.resource["deployment.environment"], "prod");
        assert_eq!(o.resource["service.instance.id"], "gw-1");
        assert_eq!(o.resource["service.name"], "plant-7");
        assert!(o.warnings[0].contains("grpc"));

        let config = OtlpConfig {
            endpoint: Some("http://local:4318/v1/metrics".into()),
            headers: BTreeMap::from([("x-team".into(), "core".into())]),
            temporality: Some(Temporality::Cumulative),
            service_name: Some("from-config".into()),
            ..Default::default()
        };
        let o = config.resolve(&vars);
        assert_eq!(o.endpoint, "http://local:4318/v1/metrics");
        assert_eq!(
            o.headers["x-team"], "core",
            "config overrides the environment"
        );
        assert_eq!(o.headers["api-key"], "abc=");
        assert_eq!(o.temporality, Temporality::Cumulative);
        assert_eq!(o.resource["service.name"], "from-config");
        // The metrics-specific endpoint is used as is.
        let o = OtlpConfig::default().resolve(env(&[(
            "OTEL_EXPORTER_OTLP_METRICS_ENDPOINT",
            "http://m:9/custom",
        )]));
        assert_eq!(o.endpoint, "http://m:9/custom");
    }
}
