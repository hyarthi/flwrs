//! OpenMetrics text (`.om`, gzipped `.om.gz`): backfilled into Prometheus
//! with `promtool tsdb create-blocks-from openmetrics <file> <dir>` (from
//! there into Mimir / Thanos), or imported by VictoriaMetrics
//! (`/api/v1/import/prometheus`).
//!
//! A valid exposition can't be appended to (each family must be in one
//! piece, and the file ends with `# EOF`), so an open partition is a folder
//! (`.om.d`) with one part file per family, and the `.om` file is assembled
//! when the partition closes: families in a fixed order, each with its
//! `# TYPE` / `# UNIT` / `# HELP`, its samples grouped by series in time
//! order. Assembling holds one family's part file in memory at a time.
//!
//! Names follow the OpenTelemetry → Prometheus rules: dots become
//! underscores, the unit is a suffix (`By` → `_bytes`, `ms` →
//! `_milliseconds`; `{event}`-style and `1` units add none), counters'
//! samples end in `_total`. Labels: `scene_id`, `node_id`, `plugin_id`,
//! `plugin_version`. Timestamps are seconds with milliseconds.
//!
//! ```text
//! # TYPE flwrs_node_events_received counter
//! # HELP flwrs_node_events_received Events delivered to the node's plugin
//! flwrs_node_events_received_total{scene_id="01J…",node_id="01J…",plugin_id="http-sink",plugin_version="0.1.0"} 42 1793923200.000
//! ```

use super::{Appender, Format};
use crate::modules::telemetry::samples::{ALL, Kind, Metric, Samples};
use async_trait::async_trait;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::{self, Write};
use std::path::Path;
use std::time::UNIX_EPOCH;

#[derive(Default)]
pub(super) struct OpenMetrics {
    appender: Appender,
}

#[async_trait]
impl Format for OpenMetrics {
    async fn append(&mut self, dir: &Path, batch: &[Samples]) -> Result<(), String> {
        let mut parts: BTreeMap<String, String> = BTreeMap::new();
        for samples in batch {
            encode(samples, &mut parts);
        }
        let owned = dir.to_path_buf();
        tokio::task::spawn_blocking(move || std::fs::create_dir_all(owned))
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| format!("couldn't create {}: {e}", dir.display()))?;
        for (family, text) in parts {
            let path = dir.join(format!("{family}.part"));
            self.appender.append(&path, text.into_bytes()).await?;
        }
        Ok(())
    }
}

/// The OpenMetrics unit for one of ours, if it has one.
fn unit(metric: &Metric) -> Option<&'static str> {
    match metric.unit {
        "By" => Some("bytes"),
        "ms" => Some("milliseconds"),
        "s" => Some("seconds"),
        _ => None,
    }
}

/// The family name, e.g. `flwrs_node_queue_size_bytes`.
pub(super) fn family(metric: &Metric) -> String {
    let mut name = metric.name.replace('.', "_");
    if let Some(unit) = unit(metric) {
        name.push('_');
        name.push_str(unit);
    }
    name
}

fn escape(value: &str, out: &mut String) {
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            _ => out.push(c),
        }
    }
}

/// Appends one line per sample to its family's part.
fn encode(samples: &Samples, parts: &mut BTreeMap<String, String>) {
    let millis = samples
        .time
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());
    for sample in &samples.samples {
        let family = family(&sample.metric);
        let out = parts.entry(family.clone()).or_default();
        out.push_str(&family);
        if sample.metric.kind == Kind::Counter {
            out.push_str("_total");
        }
        if !sample.labels.is_empty() {
            out.push('{');
            for (i, (name, value)) in sample.labels.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&name.replace('.', "_"));
                out.push_str("=\"");
                escape(value, out);
                out.push('"');
            }
            out.push('}');
        }
        let v = sample.value;
        if v.fract() == 0.0 && v.abs() < 1e15 {
            let _ = write!(out, " {v:.0}");
        } else {
            let _ = write!(out, " {v}");
        }
        let _ = writeln!(out, " {}.{:03}", millis / 1_000, millis % 1_000);
    }
}

/// Writes the exposition of an open partition's folder to `out`.
pub(super) fn assemble(dir: &Path, out: &mut dyn Write) -> io::Result<()> {
    for metric in ALL {
        let family = family(metric);
        let path = dir.join(format!("{family}.part"));
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };
        let kind = match metric.kind {
            Kind::Counter => "counter",
            Kind::Gauge => "gauge",
        };
        writeln!(out, "# TYPE {family} {kind}")?;
        if let Some(unit) = unit(metric) {
            writeln!(out, "# UNIT {family} {unit}")?;
        }
        writeln!(out, "# HELP {family} {}", metric.description)?;
        // Group by series (name and labels), keeping each one's time order.
        // A torn last line (a crash) has no timestamp and is left out.
        let mut lines: Vec<(&str, &str)> = text
            .lines()
            .filter_map(|line| {
                let mut parts = line.rsplitn(3, ' ');
                let (_ts, _value, series) = (parts.next()?, parts.next()?, parts.next()?);
                Some((series, line))
            })
            .collect();
        lines.sort_by(|a, b| a.0.cmp(b.0)); // stable
        for (_, line) in lines {
            writeln!(out, "{line}")?;
        }
    }
    writeln!(out, "# EOF")
}

#[cfg(test)]
mod tests {
    use super::super::Writer;
    use super::super::tests::{gunzip, names_in, samples, sink};
    use super::*;
    use crate::modules::telemetry::samples::{NODE_EMITTED, QUEUE_BLOCKED, QUEUE_BYTES};

    #[test]
    fn family_names() {
        assert_eq!(family(&NODE_EMITTED), "flwrs_node_events_emitted");
        assert_eq!(family(&QUEUE_BYTES), "flwrs_node_queue_size_bytes");
        assert_eq!(
            family(&QUEUE_BLOCKED),
            "flwrs_node_queue_blocked_time_milliseconds"
        );
    }

    #[tokio::test]
    async fn a_closed_partition_is_a_valid_exposition() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = Writer::new(sink(dir.path(), "openmetrics", "")).unwrap();
        w.push(samples(3_600, 1.0));
        let mut second = samples(3_610, 2.0);
        second.samples[1].labels[1].1 = "n\"2".into(); // another series, escaped
        w.push(second);
        w.push(samples(3_620, 3.0));
        w.flush().await.unwrap();
        assert_eq!(
            names_in(dir.path()),
            ["flwrs-metrics.19700101T010000Z.om.d"]
        );
        w.push(samples(7_200, 4.0));
        w.finish().await.unwrap();
        assert_eq!(
            names_in(dir.path()),
            [
                "flwrs-metrics.19700101T010000Z.om.gz",
                "flwrs-metrics.19700101T020000Z.om.d",
            ]
        );
        let text = gunzip(&dir.path().join("flwrs-metrics.19700101T010000Z.om.gz"));
        let emitted: Vec<&str> = text
            .lines()
            .skip_while(|l| !l.starts_with("# TYPE flwrs_node_events_emitted "))
            .take(5)
            .collect();
        let labels = |node: &str| {
            format!(
                "{{scene_id=\"s1\",node_id=\"{node}\",plugin_id=\"letters\",plugin_version=\"1.0.0\"}}"
            )
        };
        assert_eq!(
            emitted,
            [
                "# TYPE flwrs_node_events_emitted counter".to_string(),
                "# HELP flwrs_node_events_emitted Events the node's plugin produced".into(),
                // Series grouped (sorted), each in time order.
                format!("flwrs_node_events_emitted_total{} 1 3600.000", labels("n1")),
                format!("flwrs_node_events_emitted_total{} 3 3620.000", labels("n1")),
                format!(
                    "flwrs_node_events_emitted_total{} 2 3610.000",
                    labels("n\\\"2")
                ),
            ]
        );
        assert!(text.contains(
            "# TYPE flwrs_node_queue_size_bytes gauge\n# UNIT flwrs_node_queue_size_bytes bytes\n"
        ));
        assert!(text.contains("flwrs_hub_queued_memory_usage_bytes 2048 3600.000\n"));
        assert!(text.ends_with("# EOF\n"));
        // Every family appears once.
        let types = text.lines().filter(|l| l.starts_with("# TYPE ")).count();
        assert_eq!(types, 5);
    }
}
