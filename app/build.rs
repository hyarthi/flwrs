use std::path::Path;

fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rerun-if-changed=schema");

    // The scene export file format (.flwr). A bundled protoc, so builds
    // don't depend on a system install.
    let protoc = protoc_bin_vendored::protoc_bin_path().expect("vendored protoc not available");
    let includes =
        protoc_bin_vendored::include_path().expect("vendored protoc includes not available");
    prost_build::Config::new()
        .protoc_executable(protoc)
        .compile_protos(
            &[Path::new("schema/scene_file.proto")],
            &[Path::new("schema"), includes.as_path()],
        )
        .expect("protoc build failed");

    // OTLP metrics messages (vendored from opentelemetry-proto), for the
    // OpenTelemetry sink: only with the `otel` feature.
    if std::env::var_os("CARGO_FEATURE_OTEL").is_some() {
        let protoc = protoc_bin_vendored::protoc_bin_path().expect("vendored protoc not available");
        let out_dir = std::path::PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
        let descriptors = out_dir.join("otlp_descriptors.bin");
        prost_build::Config::new()
            .protoc_executable(protoc)
            .file_descriptor_set_path(&descriptors)
            .compile_protos(
                &[Path::new(
                    "schema/otlp/opentelemetry/proto/collector/metrics/v1/metrics_service.proto",
                )],
                &[Path::new("schema/otlp"), includes.as_path()],
            )
            .expect("protoc build failed (OTLP)");
        // OTLP JSON (for `.jsonl` telemetry files): the proto3 JSON mapping,
        // with enums as integers, as the OTLP JSON encoding requires.
        let descriptors = std::fs::read(&descriptors).expect("OTLP descriptors not written");
        pbjson_build::Builder::new()
            .register_descriptors(&descriptors)
            .expect("invalid OTLP descriptors")
            .use_integers_for_enums()
            .build(&[".opentelemetry"])
            .expect("pbjson build failed (OTLP)");
    }
}
