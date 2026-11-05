//! Project automation. Run with `cargo xtask <command>`.
//!
//! Commands:
//! - `package-plugin <dir> [--debug]`: builds the Rust plugin in `plugins/<dir>/`
//!   and packages it as a ZIP (manifest + executable + referenced files) in
//!   `target/plugins/`, ready to upload with `POST /api/plugins`.

use serde_json::Value;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use zip::write::SimpleFileOptions;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("package-plugin") => package_plugin(&args[1..]),
        _ => Err(USAGE.to_string()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

const USAGE: &str = "usage: cargo xtask package-plugin <plugin-dir> [--debug]\n\
                     e.g.  cargo xtask package-plugin http_sink";

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask lives in the workspace root")
        .to_path_buf()
}

/// `<os>-<arch>` of the machine running the build, in manifest notation.
fn host_platform() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

fn package_plugin(args: &[String]) -> Result<(), String> {
    let dir_name = args.first().filter(|a| !a.starts_with('-')).ok_or(USAGE)?;
    let release = !args.iter().any(|a| a == "--debug");
    let root = workspace_root();
    let plugin_dir = root.join("plugins").join(dir_name);

    let mut manifest: Value = read_json(&plugin_dir.join("manifest.json"))?;
    let (package_name, package_version) = cargo_package(&root, &plugin_dir.join("Cargo.toml"))?;
    let version = manifest["version"]
        .as_str()
        .ok_or("manifest.json has no version")?
        .to_string();
    if version != package_version {
        return Err(format!(
            "manifest.json version {version} doesn't match Cargo.toml version {package_version}"
        ));
    }
    let id = manifest["id"]
        .as_str()
        .ok_or("manifest.json has no id")?
        .to_string();
    let executable = manifest["executable"]
        .as_str()
        .ok_or("manifest.json has no executable")?
        .to_string();

    // Build the plugin binary.
    let mut build = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()));
    build
        .current_dir(&root)
        .args(["build", "--package", &package_name]);
    if release {
        build.arg("--release");
    }
    let status = build
        .status()
        .map_err(|e| format!("failed to run cargo build: {e}"))?;
    if !status.success() {
        return Err(format!("cargo build failed for {package_name}"));
    }
    let binary = root
        .join("target")
        .join(if release { "release" } else { "debug" })
        .join(format!("{package_name}{}", std::env::consts::EXE_SUFFIX));

    // A native binary only runs on the platform it was built for.
    let platform = host_platform();
    manifest["platforms"] = Value::Array(vec![Value::String(platform.clone())]);

    let mut files: Vec<(String, PathBuf, u32)> = vec![(executable, binary, 0o755)];
    for pointer in [
        "/launch_args_schema",
        "/payload_schemas/in",
        "/payload_schemas/out",
    ] {
        if let Some(path) = manifest.pointer(pointer).and_then(Value::as_str) {
            files.push((path.to_string(), plugin_dir.join(path), 0o644));
        }
    }

    let out_dir = root.join("target").join("plugins");
    std::fs::create_dir_all(&out_dir).map_err(|e| e.to_string())?;
    let out = out_dir.join(format!("{id}-{version}-{platform}.zip"));
    let mut zip =
        zip::ZipWriter::new(File::create(&out).map_err(|e| format!("{}: {e}", out.display()))?);
    let options = |mode: u32| SimpleFileOptions::default().unix_permissions(mode);
    zip.start_file("manifest.json", options(0o644))
        .map_err(|e| e.to_string())?;
    zip.write_all(serde_json::to_string_pretty(&manifest).unwrap().as_bytes())
        .map_err(|e| e.to_string())?;
    for (name, source, mode) in files {
        let data = std::fs::read(&source).map_err(|e| format!("{}: {e}", source.display()))?;
        zip.start_file(&name, options(mode))
            .map_err(|e| e.to_string())?;
        zip.write_all(&data).map_err(|e| e.to_string())?;
    }
    zip.finish().map_err(|e| e.to_string())?;
    println!("{}", out.display());
    Ok(())
}

fn read_json(path: &Path) -> Result<Value, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))
}

/// Name and version of the package whose manifest is at `manifest_path`.
fn cargo_package(root: &Path, manifest_path: &Path) -> Result<(String, String), String> {
    let output = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
        .current_dir(root)
        .args(["metadata", "--no-deps", "--format-version", "1"])
        .output()
        .map_err(|e| format!("failed to run cargo metadata: {e}"))?;
    let metadata: Value = serde_json::from_slice(&output.stdout).map_err(|e| e.to_string())?;
    let wanted = manifest_path
        .canonicalize()
        .map_err(|e| format!("{}: {e}", manifest_path.display()))?;
    metadata["packages"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|p| p["manifest_path"].as_str().map(Path::new) == Some(wanted.as_path()))
        .and_then(|p| {
            Some((
                p["name"].as_str()?.to_string(),
                p["version"].as_str()?.to_string(),
            ))
        })
        .ok_or_else(|| format!("{} isn't a workspace member", manifest_path.display()))
}
