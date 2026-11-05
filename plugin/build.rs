use std::path::{Path, PathBuf};

fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rerun-if-changed=schema");

    // Use a bundled protoc so builds don't depend on a system install.
    let protoc = protoc_bin_vendored::protoc_bin_path().expect("vendored protoc not available");
    let well_known_includes =
        protoc_bin_vendored::include_path().expect("vendored protoc includes not available");
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR not set"));
    let descriptor_path = out_dir.join("proto_descriptor.bin");

    prost_build::Config::new()
        .protoc_executable(protoc)
        .file_descriptor_set_path(&descriptor_path)
        // Well-known types (Timestamp) come from pbjson-types, which adds serde support.
        .compile_well_known_types()
        .extern_path(".google.protobuf", "::pbjson_types")
        .compile_protos(
            &[
                Path::new("./schema/schema.proto"),
                Path::new("./schema/common.proto"),
                Path::new("./schema/sink.proto"),
                Path::new("./schema/source.proto"),
                Path::new("./schema/transform.proto"),
            ],
            &[Path::new("."), well_known_includes.as_path()],
        )
        .expect("protoc build failed");

    // serde (proto3 JSON mapping) for every message, e.g. to read schema files from plugin manifests.
    let descriptors = std::fs::read(&descriptor_path).expect("descriptor set not written");
    pbjson_build::Builder::new()
        .register_descriptors(&descriptors)
        .expect("invalid descriptor set")
        .build(&[".schema"])
        .expect("pbjson build failed");
}
