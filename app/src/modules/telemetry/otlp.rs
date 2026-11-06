//! The OpenTelemetry sink: builds OTLP `ExportMetricsServiceRequest`s from
//! our samples and posts them over OTLP/HTTP (protobuf), with the async
//! reqwest client. No OpenTelemetry SDK, no extra thread.
//!
//! Counters are monotonic sums (cumulative or delta), gauges are gauges.
//! Cumulative series keep their start time; a counter that went down (its
//! scene restarted) starts a new series start. With delta, the reported
//! change only counts as sent once an export succeeded, so a failed export
//! isn't lost.

use super::config::{Otlp, Temporality};
use super::samples::{Kind, Sample, Samples};
use crate::modules::director::DirectorHandle;
use crate::modules::rate_limit::RateLimit;
use prost::Message;
use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio_util::sync::CancellationToken;

#[allow(clippy::all, dead_code)] // generated code
pub(crate) mod pb {
    pub mod opentelemetry {
        pub mod proto {
            pub mod common {
                pub mod v1 {
                    include!(concat!(
                        env!("OUT_DIR"),
                        "/opentelemetry.proto.common.v1.rs"
                    ));
                    include!(concat!(
                        env!("OUT_DIR"),
                        "/opentelemetry.proto.common.v1.serde.rs"
                    ));
                }
            }
            pub mod resource {
                pub mod v1 {
                    include!(concat!(
                        env!("OUT_DIR"),
                        "/opentelemetry.proto.resource.v1.rs"
                    ));
                    include!(concat!(
                        env!("OUT_DIR"),
                        "/opentelemetry.proto.resource.v1.serde.rs"
                    ));
                }
            }
            pub mod metrics {
                pub mod v1 {
                    include!(concat!(
                        env!("OUT_DIR"),
                        "/opentelemetry.proto.metrics.v1.rs"
                    ));
                    include!(concat!(
                        env!("OUT_DIR"),
                        "/opentelemetry.proto.metrics.v1.serde.rs"
                    ));
                }
            }
            pub mod collector {
                pub mod metrics {
                    pub mod v1 {
                        include!(concat!(
                            env!("OUT_DIR"),
                            "/opentelemetry.proto.collector.metrics.v1.rs"
                        ));
                        include!(concat!(
                            env!("OUT_DIR"),
                            "/opentelemetry.proto.collector.metrics.v1.serde.rs"
                        ));
                    }
                }
            }
        }
    }
    pub(crate) use opentelemetry::proto::collector::metrics::v1 as collector;
    pub(crate) use opentelemetry::proto::common::v1 as common;
    pub(crate) use opentelemetry::proto::metrics::v1 as metrics;
    pub(crate) use opentelemetry::proto::resource::v1 as resource;
}

use pb::common::{AnyValue, InstrumentationScope, KeyValue, any_value};
use pb::metrics::{
    AggregationTemporality, Gauge, Metric, NumberDataPoint, ResourceMetrics, ScopeMetrics, Sum,
    metric, number_data_point,
};

/// A series: metric name plus labels.
type SeriesKey = (&'static str, Vec<(&'static str, String)>);

#[derive(Debug, Clone, Copy)]
struct Series {
    /// Start of the current cumulative run (nanoseconds since the epoch).
    start: u64,
    /// The last value seen (to notice resets).
    last: f64,
    /// Delta: the value the last successful export was based on, and when.
    sent: f64,
    sent_time: u64,
}

/// Builds OTLP messages from samples, keeping each counter series' state
/// (start time, last value, what was sent). Shared by the OTLP sink and the
/// OTLP JSON file format.
pub(crate) struct Encoder {
    temporality: Temporality,
    /// Resource attributes, including `service.*`.
    resource: BTreeMap<String, String>,
    series: HashMap<SeriesKey, Series>,
    /// Series states to commit if the export in flight succeeds (delta).
    pending: Vec<(SeriesKey, f64, u64)>,
}

pub(crate) struct Exporter {
    config: Otlp,
    client: reqwest::Client,
    encoder: Encoder,
}

fn nanos(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
}

fn attr(key: &str, value: &str) -> KeyValue {
    KeyValue {
        key: key.to_string(),
        value: Some(AnyValue {
            value: Some(any_value::Value::StringValue(value.to_string())),
        }),
        ..Default::default()
    }
}

impl Encoder {
    pub(crate) fn new(temporality: Temporality, resource: BTreeMap<String, String>) -> Self {
        Self {
            temporality,
            resource,
            series: HashMap::new(),
            pending: vec![],
        }
    }

    /// The OTLP request for one collection.
    pub(crate) fn request(
        &mut self,
        samples: &Samples,
    ) -> pb::collector::ExportMetricsServiceRequest {
        let now = nanos(samples.time);
        let delta = self.temporality == Temporality::Delta;
        self.pending.clear();
        // One OTLP metric per metric name, with all its series as points.
        let mut metrics: Vec<Metric> = vec![];
        let mut index: HashMap<&'static str, usize> = HashMap::new();
        for sample in &samples.samples {
            let Sample {
                metric,
                labels,
                value,
            } = sample;
            let attributes = labels.iter().map(|(k, v)| attr(k, v)).collect();
            let (start, value) = match metric.kind {
                Kind::Gauge => (0, *value),
                Kind::Counter => {
                    let key: SeriesKey = (metric.name, labels.clone());
                    let state = self.series.entry(key.clone()).or_insert(Series {
                        start: now,
                        last: *value,
                        sent: 0.0,
                        sent_time: now,
                    });
                    if *value < state.last {
                        // Went down: its scene restarted; a new run starts.
                        state.start = now;
                        state.sent = 0.0;
                        state.sent_time = now;
                    }
                    state.last = *value;
                    if delta {
                        self.pending.push((key, *value, now));
                        (state.sent_time, value - state.sent)
                    } else {
                        (state.start, *value)
                    }
                }
            };
            let point = NumberDataPoint {
                attributes,
                start_time_unix_nano: start,
                time_unix_nano: now,
                value: Some(number_data_point::Value::AsInt(value as i64)),
                ..Default::default()
            };
            let at = *index.entry(metric.name).or_insert_with(|| {
                let data = match metric.kind {
                    Kind::Counter => metric::Data::Sum(Sum {
                        data_points: vec![],
                        aggregation_temporality: if delta {
                            AggregationTemporality::Delta as i32
                        } else {
                            AggregationTemporality::Cumulative as i32
                        },
                        is_monotonic: true,
                    }),
                    Kind::Gauge => metric::Data::Gauge(Gauge {
                        data_points: vec![],
                    }),
                };
                metrics.push(Metric {
                    name: metric.name.to_string(),
                    description: metric.description.to_string(),
                    unit: metric.unit.to_string(),
                    data: Some(data),
                    ..Default::default()
                });
                metrics.len() - 1
            });
            match &mut metrics[at].data {
                Some(metric::Data::Sum(sum)) => sum.data_points.push(point),
                Some(metric::Data::Gauge(gauge)) => gauge.data_points.push(point),
                _ => {}
            }
        }
        pb::collector::ExportMetricsServiceRequest {
            resource_metrics: vec![ResourceMetrics {
                resource: Some(pb::resource::Resource {
                    attributes: self.resource.iter().map(|(k, v)| attr(k, v)).collect(),
                    ..Default::default()
                }),
                scope_metrics: vec![ScopeMetrics {
                    scope: Some(InstrumentationScope {
                        name: "flwrs".into(),
                        version: env!("CARGO_PKG_VERSION").into(),
                        ..Default::default()
                    }),
                    metrics,
                    ..Default::default()
                }],
                ..Default::default()
            }],
        }
    }

    /// After a successful export: the deltas just sent are the new base.
    pub(crate) fn commit(&mut self) {
        for (key, value, time) in self.pending.drain(..) {
            if let Some(state) = self.series.get_mut(&key) {
                state.sent = value;
                state.sent_time = time;
            }
        }
    }
}

impl Exporter {
    pub(crate) fn new(config: Otlp) -> Result<Self, String> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(config.timeout_ms))
            .build()
            .map_err(|e| format!("couldn't create the HTTP client: {e}"))?;
        let encoder = Encoder::new(config.temporality, config.resource.clone());
        Ok(Self {
            config,
            client,
            encoder,
        })
    }

    /// The OTLP request for one collection.
    pub(crate) fn request(
        &mut self,
        samples: &Samples,
    ) -> pb::collector::ExportMetricsServiceRequest {
        self.encoder.request(samples)
    }

    /// Posts one collection. On a 429 / 503 with `Retry-After`, returns how
    /// long to wait before the next attempt.
    pub(crate) async fn export(
        &mut self,
        samples: &Samples,
    ) -> Result<(), (String, Option<Duration>)> {
        let body = self.request(samples).encode_to_vec();
        let mut request = self
            .client
            .post(&self.config.endpoint)
            .header("Content-Type", "application/x-protobuf")
            .body(body);
        for (name, value) in &self.config.headers {
            request = request.header(name.as_str(), value.as_str());
        }
        let response = request.send().await.map_err(|e| (e.to_string(), None))?;
        let status = response.status();
        if status.is_success() {
            self.encoder.commit();
            return Ok(());
        }
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<u64>().ok())
            .map(Duration::from_secs);
        Err((format!("the collector answered {status}"), retry_after))
    }
}

/// Exports the director's stats every `interval` until `shutdown`, then
/// once more (best effort). Failures are logged (rate-limited), never fatal.
pub(crate) async fn run(
    director: &'static DirectorHandle,
    config: Otlp,
    shutdown: CancellationToken,
) {
    for warning in &config.warnings {
        log::warn!("Telemetry (OTLP): {warning}");
    }
    let interval = Duration::from_millis(config.interval_ms);
    log::info!(
        "Telemetry (OTLP): exporting to {} every {} s ({:?})",
        config.endpoint,
        interval.as_secs(),
        config.temporality
    );
    let mut exporter = match Exporter::new(config) {
        Ok(exporter) => exporter,
        Err(err) => return log::error!("Telemetry (OTLP): {err}"),
    };
    let failures = RateLimit::new(Duration::from_secs(300));
    let mut not_before = Instant::now();
    loop {
        let stopping = tokio::select! {
            _ = shutdown.cancelled() => true,
            _ = tokio::time::sleep(interval) => false,
        };
        if Instant::now() >= not_before || stopping {
            match director.snapshot().await {
                Ok(snapshot) => {
                    let samples = super::samples::from_snapshot(&snapshot, SystemTime::now());
                    match exporter.export(&samples).await {
                        Ok(()) => log::debug!(
                            "Telemetry (OTLP): exported {} samples",
                            samples.samples.len()
                        ),
                        Err((err, retry_after)) => {
                            if let Some(wait) = retry_after {
                                not_before = Instant::now() + wait;
                            }
                            if failures.allow() {
                                log::warn!("Telemetry (OTLP): export failed: {err}");
                            }
                        }
                    }
                }
                Err(err) => log::debug!("Telemetry (OTLP): no snapshot: {err}"),
            }
        }
        if stopping {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::config::OtlpConfig;
    use super::super::samples::{NODE_EMITTED, QUEUE_DEPTH};
    use super::*;

    fn config(temporality: Temporality, endpoint: &str) -> Otlp {
        OtlpConfig {
            endpoint: Some(endpoint.into()),
            temporality: Some(temporality),
            headers: [("authorization".to_string(), "Bearer t".to_string())].into(),
            ..Default::default()
        }
        .resolve(|_| None)
    }

    fn samples(at_secs: u64, emitted: f64, depth: f64) -> Samples {
        let labels = vec![("scene.id", "s".to_string()), ("node.id", "n".to_string())];
        Samples {
            time: UNIX_EPOCH + Duration::from_secs(at_secs),
            samples: vec![
                Sample {
                    metric: NODE_EMITTED,
                    labels: labels.clone(),
                    value: emitted,
                },
                Sample {
                    metric: QUEUE_DEPTH,
                    labels,
                    value: depth,
                },
            ],
        }
    }

    fn points(request: &pb::collector::ExportMetricsServiceRequest) -> Vec<(String, u64, i64)> {
        let mut out = vec![];
        for m in &request.resource_metrics[0].scope_metrics[0].metrics {
            let points = match m.data.as_ref().unwrap() {
                metric::Data::Sum(s) => &s.data_points,
                metric::Data::Gauge(g) => &g.data_points,
                _ => unreachable!(),
            };
            for p in points {
                let Some(number_data_point::Value::AsInt(v)) = p.value else {
                    panic!()
                };
                out.push((m.name.clone(), p.start_time_unix_nano / 1_000_000_000, v));
            }
        }
        out
    }

    #[test]
    fn cumulative_counters_keep_their_start_and_reset() {
        let mut e = Exporter::new(config(Temporality::Cumulative, "http://x")).unwrap();
        let r = e.request(&samples(100, 5.0, 2.0));
        let m = &r.resource_metrics[0].scope_metrics[0].metrics;
        assert!(
            matches!(&m[0].data, Some(metric::Data::Sum(s)) if s.is_monotonic
            && s.aggregation_temporality == AggregationTemporality::Cumulative as i32)
        );
        assert_eq!(m[0].unit, "{event}");
        assert!(matches!(&m[1].data, Some(metric::Data::Gauge(_))));
        assert_eq!(
            points(&r),
            [
                ("flwrs.node.events.emitted".into(), 100, 5),
                ("flwrs.node.queue.depth".into(), 0, 2)
            ]
        );
        assert_eq!(
            points(&e.request(&samples(110, 9.0, 0.0)))[0],
            ("flwrs.node.events.emitted".into(), 100, 9)
        );
        // The scene restarted: the counter went down, so a new run starts.
        assert_eq!(
            points(&e.request(&samples(120, 1.0, 0.0)))[0],
            ("flwrs.node.events.emitted".into(), 120, 1)
        );
        // The resource names the service.
        let resource = r.resource_metrics[0].resource.as_ref().unwrap();
        assert!(resource.attributes.iter().any(|a| a.key == "service.name"));
    }

    #[test]
    fn delta_counters_report_changes_and_keep_unsent_ones() {
        let mut e = Exporter::new(config(Temporality::Delta, "http://x")).unwrap();
        assert_eq!(points(&e.request(&samples(100, 5.0, 0.0)))[0].2, 5);
        e.encoder.commit();
        assert_eq!(
            points(&e.request(&samples(110, 8.0, 0.0)))[0],
            ("flwrs.node.events.emitted".into(), 100, 3)
        );
        // Not committed (the export failed): the next delta still includes it.
        assert_eq!(points(&e.request(&samples(120, 10.0, 0.0)))[0].2, 5);
        e.encoder.commit();
        assert_eq!(points(&e.request(&samples(130, 10.0, 0.0)))[0].2, 0);
    }

    #[tokio::test]
    async fn posts_protobuf_with_the_headers() {
        use axum::{Router, body::Bytes, http::HeaderMap, routing::post};
        use std::sync::{Arc, Mutex};
        let seen: Arc<Mutex<Vec<(HeaderMap, Bytes)>>> = Arc::default();
        let store = seen.clone();
        let app = Router::new().route(
            "/v1/metrics",
            post(move |headers: HeaderMap, body: Bytes| {
                let store = store.clone();
                async move {
                    store.lock().unwrap().push((headers, body));
                    ""
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await });

        let mut e = Exporter::new(config(
            Temporality::Cumulative,
            &format!("http://{addr}/v1/metrics"),
        ))
        .unwrap();
        e.export(&samples(100, 5.0, 1.0)).await.unwrap();
        let (headers, body) = seen.lock().unwrap().pop().unwrap();
        assert_eq!(headers["content-type"], "application/x-protobuf");
        assert_eq!(headers["authorization"], "Bearer t");
        let request = pb::collector::ExportMetricsServiceRequest::decode(body).unwrap();
        assert_eq!(points(&request).len(), 2);

        // A collector that isn't there is an error, not a panic.
        let mut down = Exporter::new(config(
            Temporality::Cumulative,
            "http://127.0.0.1:9/v1/metrics",
        ))
        .unwrap();
        assert!(down.export(&samples(100, 1.0, 1.0)).await.is_err());
    }
}
