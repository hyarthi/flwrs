//! InfluxDB line protocol (`.lp`): read by Telegraf (`inputs.file` /
//! `inputs.tail`), `influx write --file`, InfluxDB 2 / 3, VictoriaMetrics
//! (`/write`) and QuestDB.
//!
//! One line per measurement and tag set per collection, with every metric
//! as a field. The measurement is the metric name's first two parts, the
//! field the rest, dots turned into underscores:
//! `flwrs.node.queue.depth` → measurement `flwrs_node`, field `queue_depth`.
//! Labels become tags (`scene_id`, `node_id`, `plugin_id`,
//! `plugin_version`). All fields are integers (`i` suffix); timestamps are
//! nanoseconds.
//!
//! ```text
//! flwrs_node,scene_id=01J…,node_id=01J…,plugin_id=http-sink,plugin_version=0.1.0 events_received=42i,up=1i 1793923200000000000
//! ```

use super::{Appender, Format};
use crate::modules::telemetry::samples::Samples;
use async_trait::async_trait;
use std::collections::HashMap;
use std::fmt::Write;
use std::path::Path;
use std::time::UNIX_EPOCH;

#[derive(Default)]
pub(super) struct Influx {
    appender: Appender,
}

#[async_trait]
impl Format for Influx {
    async fn append(&mut self, path: &Path, batch: &[Samples]) -> Result<(), String> {
        let mut text = String::new();
        for samples in batch {
            encode(samples, &mut text);
        }
        self.appender.append(path, text.into_bytes()).await
    }
}

/// Splits `flwrs.node.queue.depth` into `flwrs_node` and `queue_depth`.
fn split(name: &str) -> (String, String) {
    let mut parts = name.splitn(3, '.');
    let measurement = [parts.next(), parts.next()]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join("_");
    let field = parts.next().unwrap_or("value").replace('.', "_");
    (measurement, field)
}

/// Escapes a measurement (`,` and space) or a tag key, tag value or field
/// key (also `=`). Line breaks can't be escaped; they become spaces.
fn escape(text: &str, measurement: bool, out: &mut String) {
    for c in text.chars() {
        match c {
            ',' | ' ' => out.push('\\'),
            '=' if !measurement => out.push('\\'),
            '\n' | '\r' => {
                out.push_str("\\ ");
                continue;
            }
            _ => {}
        }
        out.push(c);
    }
}

pub(super) fn encode(samples: &Samples, out: &mut String) {
    let nanos = samples
        .time
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    // Lines in first-seen order; fields in sample order.
    let mut lines: Vec<(String, Vec<(String, i64)>)> = vec![];
    let mut index: HashMap<String, usize> = HashMap::new();
    for sample in &samples.samples {
        let (measurement, field) = split(sample.metric.name);
        let mut key = String::new();
        escape(&measurement, true, &mut key);
        for (name, value) in &sample.labels {
            if value.is_empty() {
                continue; // line protocol has no empty tag values
            }
            key.push(',');
            escape(&name.replace('.', "_"), false, &mut key);
            key.push('=');
            escape(value, false, &mut key);
        }
        let at = *index.entry(key.clone()).or_insert_with(|| {
            lines.push((key, vec![]));
            lines.len() - 1
        });
        lines[at].1.push((field, sample.value as i64));
    }
    for (key, fields) in lines {
        out.push_str(&key);
        out.push(' ');
        for (i, (field, value)) in fields.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            escape(field, false, out);
            let _ = write!(out, "={value}i");
        }
        let _ = writeln!(out, " {nanos}");
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::samples;
    use super::*;

    #[test]
    fn one_line_per_measurement_and_tag_set() {
        let mut text = String::new();
        encode(&samples(1_793_923_200, 5.0), &mut text);
        assert_eq!(
            text,
            "flwrs_hub queued_memory_usage=2048i 1793923200000000000\n\
             flwrs_node,scene_id=s1,node_id=n1,plugin_id=letters,plugin_version=1.0.0 \
             events_emitted=5i,up=1i,queue_size=512i,queue_blocked_time=3i 1793923200000000000\n"
        );
    }

    #[test]
    fn escaping() {
        let mut out = String::new();
        escape("a b,c=d", false, &mut out);
        assert_eq!(out, r"a\ b\,c\=d");
        assert_eq!(split("flwrs.scene.up"), ("flwrs_scene".into(), "up".into()));
    }
}
