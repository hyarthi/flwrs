// build.rs

use std::path::Path;

fn main() {
    println!("cargo:rerun-if-changed=NULL");
    println!("cargo:rerun-if-changed=build.rs");
    prost_build::compile_protos(
        &[
            Path::new("schema/schema.proto"),
            Path::new("schema/common.proto"),
            Path::new("schema/sink.proto"),
            Path::new("schema/source.proto"),
            Path::new("schema/transform.proto"),
        ],
        &[Path::new("")],
    )
    .expect("protoc build failed");
}
