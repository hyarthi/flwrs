# Vendored OTLP protocol definitions

The OpenTelemetry protocol (OTLP) messages for **metrics** only, copied unchanged from
[open-telemetry/opentelemetry-proto](https://github.com/open-telemetry/opentelemetry-proto)
(Apache-2.0, see `LICENSE`), at the version shipped with the `opentelemetry-proto` 0.33.1
crate (OTLP 1.10):

- `opentelemetry/proto/common/v1/common.proto`
- `opentelemetry/proto/resource/v1/resource.proto`
- `opentelemetry/proto/metrics/v1/metrics.proto`
- `opentelemetry/proto/collector/metrics/v1/metrics_service.proto`

`app/build.rs` compiles them with prost when the `otel` feature is on. flwrs builds the
`ExportMetricsServiceRequest` itself and posts it over OTLP/HTTP (protobuf), so it needs
neither the OpenTelemetry SDK nor a gRPC stack. To update: copy the same files from a newer
OTLP release; OTLP evolves backward compatibly.
