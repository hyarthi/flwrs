//! OTLP JSON Lines (`.jsonl`, gzipped `.jsonl.gz`): one OTLP `MetricsData`
//! per line, in the OTLP JSON encoding (camelCase keys, 64-bit integers as
//! strings, enums as integers), as the OpenTelemetry file exporter writes it.
//! Replayed into any backend by the OpenTelemetry Collector's
//! `otlpjsonfile` receiver (it reads gzip too), queried with DuckDB's
//! `otlp` extension or jq.
//!
//! The same messages as the OTLP sink, cumulative: each counter keeps its
//! start time (a new one after its scene restarted). The resource has
//! `service.name = flwrs`, `service.version` and a `service.instance.id`
//! that is new for every server start.

use super::{Appender, Format};
use crate::modules::telemetry::config::Temporality;
use crate::modules::telemetry::otlp::{Encoder, pb};
use crate::modules::telemetry::samples::Samples;
use async_trait::async_trait;
use std::collections::BTreeMap;
use std::path::Path;

pub(super) struct OtlpJson {
    encoder: Encoder,
    appender: Appender,
}

impl OtlpJson {
    pub(super) fn new() -> Self {
        let resource = BTreeMap::from([
            ("service.name".to_string(), "flwrs".to_string()),
            (
                "service.version".to_string(),
                env!("CARGO_PKG_VERSION").to_string(),
            ),
            (
                "service.instance.id".to_string(),
                ulid::Ulid::generate().to_string(),
            ),
        ]);
        Self {
            encoder: Encoder::new(Temporality::Cumulative, resource),
            appender: Appender::default(),
        }
    }

    fn encode(&mut self, samples: &Samples, out: &mut Vec<u8>) -> Result<(), String> {
        let request = self.encoder.request(samples);
        self.encoder.commit();
        let data = pb::metrics::MetricsData {
            resource_metrics: request.resource_metrics,
        };
        serde_json::to_writer(&mut *out, &data).map_err(|e| e.to_string())?;
        out.push(b'\n');
        Ok(())
    }
}

#[async_trait]
impl Format for OtlpJson {
    async fn append(&mut self, path: &Path, batch: &[Samples]) -> Result<(), String> {
        let mut bytes = vec![];
        for samples in batch {
            self.encode(samples, &mut bytes)?;
        }
        self.appender.append(path, bytes).await
    }
}

#[cfg(test)]
mod tests {
    use super::super::Writer;
    use super::super::tests::{gunzip, samples, sink};
    use serde_json::Value;

    #[tokio::test]
    async fn lines_are_otlp_json() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = Writer::new(sink(dir.path(), "otlp-json", "")).unwrap();
        w.push(samples(3_600, 1.0));
        w.push(samples(3_610, 4.0));
        w.push(samples(7_200, 9.0));
        w.finish().await.unwrap();
        let text = gunzip(&dir.path().join("flwrs-metrics.19700101T010000Z.jsonl.gz"));
        let lines: Vec<Value> = text
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 2);
        let rm = &lines[1]["resourceMetrics"][0];
        let attrs = rm["resource"]["attributes"].as_array().unwrap();
        assert!(
            attrs
                .iter()
                .any(|a| a["key"] == "service.name" && a["value"]["stringValue"] == "flwrs")
        );
        let metrics = rm["scopeMetrics"][0]["metrics"].as_array().unwrap();
        let emitted = metrics
            .iter()
            .find(|m| m["name"] == "flwrs.node.events.emitted")
            .unwrap();
        let sum = &emitted["sum"];
        assert_eq!(
            sum["aggregationTemporality"], 2,
            "cumulative, as an integer"
        );
        assert_eq!(sum["isMonotonic"], true);
        let point = &sum["dataPoints"][0];
        assert_eq!(point["asInt"], "4", "64-bit integers are strings");
        assert_eq!(
            point["startTimeUnixNano"], "3600000000000",
            "kept from the first"
        );
        assert_eq!(point["timeUnixNano"], "3610000000000");
        assert!(
            point["attributes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|a| a["key"] == "plugin.version")
        );
        let up = metrics
            .iter()
            .find(|m| m["name"] == "flwrs.node.up")
            .unwrap();
        assert!(up["gauge"]["dataPoints"].is_array());
    }
}
