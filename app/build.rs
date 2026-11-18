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

    // The web UI: only with the `web-ui` feature.
    if std::env::var_os("CARGO_FEATURE_WEB_UI").is_some() {
        build_web_ui();
    }
}

/// Builds the web UI (`web/`, Svelte + Vite) into `$OUT_DIR/web`, which
/// `modules/web_ui.rs` embeds. `npm ci` runs only when `node_modules` is
/// missing or older than the lockfile. Builds of other profiles or feature
/// sets can run this at the same time, so a file lock serializes them.
fn build_web_ui() {
    use std::path::PathBuf;
    use std::process::{Command, Stdio};

    let manifest_dir =
        PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let web = manifest_dir.join("web");
    let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR")).join("web");

    for path in [
        "src",
        "public",
        "scripts",
        "index.html",
        "package.json",
        "package-lock.json",
        "vite.config.ts",
        "svelte.config.js",
        "tsconfig.json",
    ] {
        println!("cargo::rerun-if-changed=web/{path}");
    }
    println!("cargo::rerun-if-env-changed=FLWRS_NPM");

    // npm is `npm.cmd` on Windows (build scripts run on the host).
    let npm = std::env::var("FLWRS_NPM")
        .unwrap_or_else(|_| if cfg!(windows) { "npm.cmd" } else { "npm" }.to_string());
    let run = |args: &[&str]| {
        // npm's output goes to stderr: cargo reads a build script's stdout
        // for its own instructions.
        let status = Command::new(&npm)
            .args(args)
            .current_dir(&web)
            .env("FLWRS_WEB_OUT_DIR", &out)
            .stdin(Stdio::null())
            .stdout(std::io::stderr())
            .status()
            .unwrap_or_else(|e| {
                panic!(
                    "the web UI (feature `web-ui`) needs Node.js >= 20.19 and npm to build, \
                     and `{npm}` couldn't be run: {e}. Install Node.js (or point FLWRS_NPM at npm), \
                     or build without the feature (barebones: --no-default-features)"
                )
            });
        assert!(
            status.success(),
            "`{npm} {}` in app/web failed ({status}); see the output above",
            args.join(" ")
        );
    };

    let lock = std::fs::File::create(web.join(".build-lock")).expect("web UI build lock");
    lock.lock().expect("web UI build lock");

    let modified = |p: PathBuf| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    let installed = modified(web.join("node_modules/.package-lock.json"));
    let wanted = modified(web.join("package-lock.json"));
    if installed.is_none() || installed < wanted {
        run(&["ci", "--no-audit", "--no-fund"]);
    }
    run(&["run", "build"]);
}
