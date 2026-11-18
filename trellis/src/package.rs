//! `package`: builds a plugin and packages it as a ZIP the server accepts
//! (`POST /api/plugins`).
//!
//! - A Rust crate (a `Cargo.toml` in the folder, no `[build]` in
//!   `trellis.toml`): `cargo build` (release unless asked otherwise, for the
//!   host or `--target`); the crate's version must equal the manifest's, and
//!   the manifest's `platforms` becomes the build's platform.
//! - `[build]` in `trellis.toml`: its command, then its `executable`.
//! - Neither: the manifest's executable as it is in the folder (a script).
//!
//! The ZIP is reproducible (sorted entries, fixed timestamps, explicit
//! permissions) and checked with the server's own rules before it's kept.

use crate::config::{Config, Flavor, expand};
use flwrs_plugin::package::{self, Limits, MANIFEST_FILE, Manifest, current_platform};
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use zip::write::SimpleFileOptions;

/// What to package.
#[derive(Debug, Clone)]
pub struct PackageOptions {
    /// The plugin folder (with `manifest.json`).
    pub dir: PathBuf,
    /// A Rust target triple to cross-build for.
    pub target: Option<String>,
    /// A flavor from `trellis.toml`.
    pub flavor: Option<String>,
    /// Release build (Rust crates); a debug build otherwise.
    pub release: bool,
    /// Where the ZIP goes (default: `<cargo target dir>/plugins/` for a Rust
    /// crate, `<dir>/dist/` otherwise).
    pub out: Option<PathBuf>,
    /// A `trellis.toml` other than `<dir>/trellis.toml`.
    pub config: Option<PathBuf>,
}

impl PackageOptions {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            target: None,
            flavor: None,
            release: true,
            out: None,
            config: None,
        }
    }
}

/// A package that was written and passed the server's checks.
#[derive(Debug, Clone)]
pub struct Packaged {
    pub path: PathBuf,
    pub manifest: Manifest,
    /// The platform written into the manifest (native builds).
    pub platform: Option<String>,
    /// ZIP size in bytes.
    pub size: u64,
    /// Executable size in bytes.
    pub executable_size: u64,
}

/// Builds and packages the plugin in `opts.dir`.
pub fn package(opts: &PackageOptions) -> Result<Packaged, String> {
    let dir = &opts.dir;
    let config = match &opts.config {
        Some(path) => Config::read(path)?,
        None => Config::load(dir)?,
    };
    let flavor: Option<&Flavor> = match &opts.flavor {
        None => None,
        Some(name) => Some(
            config
                .flavors
                .get(name)
                .ok_or_else(|| format!("no flavor {name:?} in trellis.toml"))?,
        ),
    };
    let manifest_file = flavor
        .and_then(|f| f.manifest.clone())
        .unwrap_or_else(|| MANIFEST_FILE.to_string());
    let manifest_text = std::fs::read_to_string(dir.join(&manifest_file))
        .map_err(|e| format!("{}: {e}", dir.join(&manifest_file).display()))?;
    let mut manifest_value: Value =
        serde_json::from_str(&manifest_text).map_err(|e| format!("{manifest_file}: {e}"))?;
    let manifest: Manifest = serde_json::from_value(manifest_value.clone())
        .map_err(|e| format!("{manifest_file}: {e}"))?;

    // Build.
    let profile = if opts.release { "release" } else { "debug" };
    // A Rust plugin's configurator is another binary of the crate, named as the file.
    let configurator_bin = manifest.configurator.as_ref().map(|c| {
        Path::new(&c.executable)
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default()
    });
    let mut configurator_built = None;
    let (executable, platform, default_out) = if let Some(build) = &config.build {
        let (program, args) = build
            .command
            .split_first()
            .ok_or("[build] command is empty")?;
        let mut command = Command::new(program);
        command
            .args(args)
            .current_dir(dir)
            .env("TRELLIS_PROFILE", profile)
            .env("TRELLIS_FLAVOR", opts.flavor.as_deref().unwrap_or(""));
        if let Some(target) = &opts.target {
            command.env("TRELLIS_TARGET", target);
        }
        let status = command
            .status()
            .map_err(|e| format!("couldn't run {program}: {e}"))?;
        if !status.success() {
            return Err(format!("the build command failed ({status})"));
        }
        (dir.join(&build.executable), None, dir.join("dist"))
    } else if dir.join("Cargo.toml").is_file() {
        let built = cargo_build(dir, opts, flavor, configurator_bin.as_deref())?;
        configurator_built = built.configurator.clone();
        if built.version != manifest.version {
            return Err(format!(
                "{manifest_file} version {} doesn't match Cargo.toml version {}",
                manifest.version, built.version
            ));
        }
        let platform = platform_of(opts.target.as_deref())?;
        (
            built.binary,
            Some(platform),
            built.target_dir.join("plugins"),
        )
    } else {
        (dir.join(&manifest.executable), None, dir.join("dist"))
    };
    if !executable.is_file() {
        return Err(format!(
            "the executable {} doesn't exist",
            executable.display()
        ));
    }
    if let Some(platform) = &platform {
        manifest_value["platforms"] = Value::Array(vec![Value::String(platform.clone())]);
    }

    // Files: path in the ZIP → (source, mode).
    let mut files: BTreeMap<String, (PathBuf, u32)> = BTreeMap::new();
    files.insert(manifest.executable.clone(), (executable.clone(), 0o755));
    if let Some(c) = &manifest.configurator {
        let source = configurator_built
            .clone()
            .unwrap_or_else(|| dir.join(&c.executable));
        if !source.is_file() {
            return Err(format!(
                "the configurator {} doesn't exist",
                source.display()
            ));
        }
        files.insert(c.executable.clone(), (source, 0o755));
    }
    for (field, path) in manifest.file_references() {
        if !matches!(field, "executable" | "configurator.executable") {
            files.insert(path.to_string(), (dir.join(path), 0o644));
        }
    }
    // Files the launch-args schema names (a code field's `definitions`).
    if let Some(schema_path) = &manifest.launch_args_schema {
        let text = std::fs::read_to_string(dir.join(schema_path))
            .map_err(|e| format!("{schema_path}: {e}"))?;
        let schema: Value =
            serde_json::from_str(&text).map_err(|e| format!("{schema_path}: {e}"))?;
        let (spec, _) = package::LaunchSpec::new(&manifest, Some(&schema));
        for (_, path) in spec.file_references() {
            files.insert(path.to_string(), (dir.join(path), 0o644));
        }
    }
    let excluded: Vec<glob::Pattern> = config
        .package
        .exclude
        .iter()
        .map(|p| glob::Pattern::new(p).map_err(|e| format!("exclude {p:?}: {e}")))
        .collect::<Result<_, _>>()?;
    if !config.package.include.is_empty() {
        for path in expand(dir, &config.package.include, "")? {
            let relative = path
                .strip_prefix(dir)
                .map_err(|_| format!("{} is outside the plugin folder", path.display()))?;
            let name = relative
                .components()
                .map(|c| c.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/");
            if name == MANIFEST_FILE || excluded.iter().any(|p| p.matches(&name)) {
                continue;
            }
            files.entry(name).or_insert((path, 0o644));
        }
    }

    // Write.
    let out_dir = opts.out.clone().unwrap_or(default_out);
    std::fs::create_dir_all(&out_dir).map_err(|e| format!("{}: {e}", out_dir.display()))?;
    let mut name = format!("{}-{}", manifest.id, manifest.version);
    // The flavor, unless the id already names it (`http-source-edge`).
    if let Some(flavor) = opts
        .flavor
        .as_ref()
        .filter(|f| !manifest.id.ends_with(&format!("-{f}")))
    {
        name.push_str(&format!("-{flavor}"));
    }
    if let Some(platform) = &platform {
        name.push_str(&format!("-{platform}"));
    }
    let out = out_dir.join(format!("{name}.zip"));
    let partial = out_dir.join(format!(".{name}.zip.partial"));
    let manifest_json = serde_json::to_string_pretty(&manifest_value).map_err(|e| e.to_string())?;
    if let Err(e) = write_zip(&partial, &manifest_json, &files) {
        let _ = std::fs::remove_file(&partial);
        return Err(e);
    }

    // The server's checks, for the platform the package is for.
    let check_platform = platform
        .clone()
        .or_else(|| manifest.platforms.as_ref().and_then(|p| p.first().cloned()))
        .unwrap_or_else(current_platform);
    if let Err(e) = package::validate(&partial, &Limits::default(), &check_platform, &|_| vec![]) {
        let _ = std::fs::remove_file(&partial);
        return Err(e.to_string());
    }
    std::fs::rename(&partial, &out).map_err(|e| format!("{}: {e}", out.display()))?;
    let size = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
    let executable_size = std::fs::metadata(&executable).map(|m| m.len()).unwrap_or(0);
    let manifest: Manifest = serde_json::from_value(manifest_value).map_err(|e| e.to_string())?;
    Ok(Packaged {
        path: out,
        manifest,
        platform,
        size,
        executable_size,
    })
}

fn write_zip(
    path: &Path,
    manifest_json: &str,
    files: &BTreeMap<String, (PathBuf, u32)>,
) -> Result<(), String> {
    let file = std::fs::File::create(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut zip = zip::ZipWriter::new(file);
    // Fixed timestamps (the ZIP epoch) and explicit modes: the same input
    // gives the same bytes.
    let options = |mode: u32| {
        SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated)
            .last_modified_time(zip::DateTime::default())
            .unix_permissions(mode)
    };
    zip.start_file(MANIFEST_FILE, options(0o644))
        .map_err(|e| e.to_string())?;
    zip.write_all(manifest_json.as_bytes())
        .map_err(|e| e.to_string())?;
    for (name, (source, mode)) in files {
        let data = std::fs::read(source).map_err(|e| format!("{}: {e}", source.display()))?;
        zip.start_file(name.as_str(), options(*mode))
            .map_err(|e| e.to_string())?;
        zip.write_all(&data).map_err(|e| e.to_string())?;
    }
    zip.finish().map_err(|e| e.to_string())?;
    Ok(())
}

struct Built {
    binary: PathBuf,
    /// The configurator's binary (a second binary of the crate).
    configurator: Option<PathBuf>,
    version: String,
    target_dir: PathBuf,
}

fn cargo() -> Command {
    Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
}

/// `cargo build` of the crate in `dir`; returns its binary.
fn cargo_build(
    dir: &Path,
    opts: &PackageOptions,
    flavor: Option<&Flavor>,
    configurator_bin: Option<&str>,
) -> Result<Built, String> {
    let manifest_path = dir.join("Cargo.toml");
    let output = cargo()
        .args([
            "metadata",
            "--format-version",
            "1",
            "--no-deps",
            "--manifest-path",
        ])
        .arg(&manifest_path)
        .output()
        .map_err(|e| format!("couldn't run cargo metadata: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "cargo metadata failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let metadata: Value =
        serde_json::from_slice(&output.stdout).map_err(|e| format!("cargo metadata: {e}"))?;
    let wanted = manifest_path
        .canonicalize()
        .map_err(|e| format!("{}: {e}", manifest_path.display()))?;
    let package = metadata["packages"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|p| {
            p["manifest_path"]
                .as_str()
                .and_then(|m| Path::new(m).canonicalize().ok())
                .is_some_and(|m| m == wanted)
        })
        .ok_or_else(|| format!("no package for {} in cargo metadata", wanted.display()))?;
    let name = package["name"].as_str().unwrap_or_default().to_string();
    let version = package["version"].as_str().unwrap_or_default().to_string();
    let bins: Vec<String> = package["targets"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|t| {
            t["kind"]
                .as_array()
                .is_some_and(|k| k.iter().any(|k| k == "bin"))
        })
        .filter_map(|t| t["name"].as_str().map(str::to_string))
        .collect();
    if let Some(c) = configurator_bin
        && !bins.iter().any(|b| b == c)
    {
        return Err(format!(
            "the manifest's configurator is {c:?}, but the crate {name} has no binary of that name"
        ));
    }
    let plugin_bins: Vec<String> = bins
        .iter()
        .filter(|b| Some(b.as_str()) != configurator_bin)
        .cloned()
        .collect();
    let bin = match plugin_bins.as_slice() {
        [one] => one.clone(),
        [] => return Err(format!("the crate {name} has no binary")),
        _ if bins.contains(&name) => name.clone(),
        _ => {
            return Err(format!(
                "the crate {name} has several binaries ({}) and none is named after it",
                bins.join(", ")
            ));
        }
    };
    let target_dir = PathBuf::from(metadata["target_directory"].as_str().unwrap_or("target"));

    let mut build = cargo();
    build
        .args(["build", "--manifest-path"])
        .arg(&manifest_path)
        .args(["--package", &name, "--bin", &bin]);
    if let Some(c) = configurator_bin {
        build.args(["--bin", c]);
    }
    if opts.release {
        build.arg("--release");
    }
    if let Some(target) = &opts.target {
        build.args(["--target", target]);
    }
    if let Some(flavor) = flavor {
        if flavor.no_default_features {
            build.arg("--no-default-features");
        }
        if !flavor.features.is_empty() {
            build.args(["--features", &flavor.features.join(",")]);
        }
    }
    let status = build
        .status()
        .map_err(|e| format!("couldn't run cargo build: {e}"))?;
    if !status.success() {
        return Err(format!("cargo build failed for {name}"));
    }
    let mut binary_dir = target_dir.clone();
    if let Some(target) = &opts.target {
        binary_dir.push(target);
    }
    binary_dir.push(if opts.release { "release" } else { "debug" });
    let windows = match &opts.target {
        Some(t) => t.contains("windows"),
        None => cfg!(windows),
    };
    let exe = |b: &str| binary_dir.join(format!("{b}{}", if windows { ".exe" } else { "" }));
    Ok(Built {
        configurator: configurator_bin.map(exe),
        binary: exe(&bin),
        version,
        target_dir,
    })
}

/// The manifest platform (`<os>-<arch>`, as `std::env::consts` names them)
/// of a Rust target triple, or of the host.
pub fn platform_of(triple: Option<&str>) -> Result<String, String> {
    let Some(triple) = triple else {
        return Ok(current_platform());
    };
    let arch_part = triple.split('-').next().unwrap_or_default();
    let arch = match arch_part {
        "x86_64" => "x86_64",
        "aarch64" | "arm64" => "aarch64",
        "i386" | "i586" | "i686" => "x86",
        a if a.starts_with("armv") || a.starts_with("thumbv") || a == "arm" => "arm",
        a if a.starts_with("riscv64") => "riscv64",
        a if a.starts_with("powerpc64") => "powerpc64",
        "s390x" => "s390x",
        "loongarch64" => "loongarch64",
        other => return Err(format!("unknown architecture {other:?} in {triple}")),
    };
    let os = if triple.contains("apple-darwin") {
        "macos"
    } else if triple.contains("apple-ios") {
        "ios"
    } else if triple.contains("android") {
        "android"
    } else if triple.contains("linux") {
        "linux"
    } else if triple.contains("windows") {
        "windows"
    } else if let Some(os) = [
        "freebsd",
        "netbsd",
        "openbsd",
        "dragonfly",
        "illumos",
        "solaris",
    ]
    .into_iter()
    .find(|os| triple.contains(os))
    {
        os
    } else {
        return Err(format!("unknown operating system in {triple}"));
    };
    Ok(format!("{os}-{arch}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platforms_of_targets() {
        for (triple, platform) in [
            ("x86_64-unknown-linux-gnu", "linux-x86_64"),
            ("aarch64-unknown-linux-musl", "linux-aarch64"),
            ("armv7-unknown-linux-gnueabihf", "linux-arm"),
            ("aarch64-apple-darwin", "macos-aarch64"),
            ("x86_64-pc-windows-msvc", "windows-x86_64"),
            ("i686-pc-windows-gnu", "windows-x86"),
            ("x86_64-unknown-freebsd", "freebsd-x86_64"),
            ("x86_64-unknown-illumos", "illumos-x86_64"),
            ("riscv64gc-unknown-linux-gnu", "linux-riscv64"),
        ] {
            assert_eq!(platform_of(Some(triple)).unwrap(), platform, "{triple}");
        }
        assert!(platform_of(Some("wasm32-unknown-unknown")).is_err());
        assert_eq!(platform_of(None).unwrap(), current_platform());
    }

    #[cfg(unix)]
    #[test]
    fn script_plugins_are_packaged_reproducibly_and_pass_the_server_checks() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("schema")).unwrap();
        std::fs::create_dir_all(root.join("models")).unwrap();
        std::fs::write(root.join("run.sh"), "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(root.join("run.sh"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        std::fs::write(
            root.join("schema/in.json"),
            r#"{"fields":[{"key":"t","type":"STRING"}]}"#,
        )
        .unwrap();
        std::fs::write(root.join("models/a.bin"), [1u8, 2, 3]).unwrap();
        std::fs::write(root.join("models/skip.tmp"), "x").unwrap();
        std::fs::write(
            root.join("manifest.json"),
            r#"{"manifest_version":1,"id":"demo","name":"Demo","version":"1.2.3","type":"SINK","executable":"run.sh","payload_schemas":{"in":"schema/in.json"}}"#,
        )
        .unwrap();
        std::fs::write(
            root.join("trellis.toml"),
            "[package]\ninclude = [\"models/**\"]\nexclude = [\"**/*.tmp\"]\n",
        )
        .unwrap();
        let first = package(&PackageOptions::new(root)).unwrap();
        assert_eq!(first.path, root.join("dist/demo-1.2.3.zip"));
        assert_eq!(first.platform, None);
        let bytes = std::fs::read(&first.path).unwrap();
        let second = package(&PackageOptions::new(root)).unwrap();
        assert_eq!(std::fs::read(&second.path).unwrap(), bytes, "reproducible");

        let mut archive = zip::ZipArchive::new(std::fs::File::open(&first.path).unwrap()).unwrap();
        let names: Vec<String> = archive
            .file_names()
            .map(|n| n.unwrap().to_string())
            .collect();
        assert!(names.contains(&"models/a.bin".to_string()), "{names:?}");
        assert!(!names.iter().any(|n| n.ends_with(".tmp")), "{names:?}");
        assert_eq!(
            archive.by_name("run.sh").unwrap().unix_mode().unwrap() & 0o777,
            0o755
        );
    }

    #[test]
    fn invalid_packages_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("run.sh"), "x").unwrap();
        std::fs::write(
            root.join("manifest.json"),
            r#"{"manifest_version":1,"id":"Bad Id","name":"x","version":"1.0.0","type":"SINK","executable":"run.sh","payload_schemas":{"in":"missing.json"}}"#,
        )
        .unwrap();
        let err = package(&PackageOptions::new(root)).unwrap_err();
        assert!(err.contains("missing.json"), "{err}");
        let partials: Vec<_> = std::fs::read_dir(root.join("dist")).unwrap().collect();
        assert!(partials.is_empty(), "nothing left behind");
    }
}
