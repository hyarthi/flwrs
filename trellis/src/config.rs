//! `trellis.toml`: the plugin developer's settings, next to `manifest.json`.
//! Optional; every section has defaults. The manifest stays a runtime file
//! (the server reads it strictly), so build and test settings live here.
//!
//! ```toml
//! [build]                         # omitted for a Rust crate (cargo build)
//! command = ["./build.sh"]        # any language: run from the plugin folder
//! executable = "dist/plugin.py"   # what becomes manifest.executable in the ZIP
//!
//! [package]
//! include = ["models/**"]         # extra files, besides the manifest's references
//! exclude = ["**/__pycache__/**"]
//!
//! [flavors.edge]                  # trellis package --flavor edge
//! features = []
//! no_default_features = true
//! manifest = "manifest.edge.json"
//!
//! [audit]
//! launch_args = "tests/node.json"
//! args = ["--verbose"]
//! stimulus = ["curl", "-s", "-d", "{}", "http://127.0.0.1:${port}/in"]
//!
//! [test]
//! files = ["tests/*.test.toml"]
//!
//! [bench]
//! files = ["benches/*.bench.toml"]
//! ```

use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const CONFIG_FILE: &str = "trellis.toml";

#[derive(Deserialize, Debug, Clone, Default)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    pub build: Option<Build>,
    pub package: PackageSection,
    pub flavors: BTreeMap<String, Flavor>,
    pub audit: AuditSection,
    pub test: FilesSection,
    pub bench: FilesSection,
    /// The folder the file was read from: relative paths start there.
    #[serde(skip)]
    pub dir: PathBuf,
}

/// A build command for plugins that aren't a Rust crate.
#[derive(Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct Build {
    pub command: Vec<String>,
    /// The built executable, relative to the plugin folder.
    pub executable: String,
}

#[derive(Deserialize, Debug, Clone, Default)]
#[serde(deny_unknown_fields, default)]
pub struct PackageSection {
    pub include: Vec<String>,
    pub exclude: Vec<String>,
}

/// A build variant (e.g. edge vs data centre).
#[derive(Deserialize, Debug, Clone, Default)]
#[serde(deny_unknown_fields, default)]
pub struct Flavor {
    /// Cargo features (Rust crates).
    pub features: Vec<String>,
    pub no_default_features: bool,
    /// A manifest to use instead of `manifest.json`.
    pub manifest: Option<String>,
}

#[derive(Deserialize, Debug, Clone, Default)]
#[serde(deny_unknown_fields, default)]
pub struct AuditSection {
    /// A JSON file with the node's `launch_args`.
    pub launch_args: Option<String>,
    /// Extra plugin arguments.
    pub args: Vec<String>,
    /// A command that makes a source emit.
    pub stimulus: Option<Vec<String>>,
}

#[derive(Deserialize, Debug, Clone, Default)]
#[serde(deny_unknown_fields, default)]
pub struct FilesSection {
    /// Glob patterns, relative to the plugin folder.
    pub files: Vec<String>,
}

impl Config {
    /// `trellis.toml` in `dir` if it's there, else the defaults.
    pub fn load(dir: &Path) -> Result<Self, String> {
        let path = dir.join(CONFIG_FILE);
        if !path.is_file() {
            return Ok(Self {
                dir: dir.to_path_buf(),
                ..Self::default()
            });
        }
        Self::read(&path)
    }

    /// A config file at `path`.
    pub fn read(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut config: Config =
            toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        config.dir = path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        Ok(config)
    }

    /// Test files: the config's patterns (default `tests/*.test.toml`).
    pub fn test_files(&self) -> Result<Vec<PathBuf>, String> {
        expand(&self.dir, &self.test.files, "tests/*.test.toml")
    }

    /// Bench files: the config's patterns (default `benches/*.bench.toml`).
    pub fn bench_files(&self) -> Result<Vec<PathBuf>, String> {
        expand(&self.dir, &self.bench.files, "benches/*.bench.toml")
    }
}

/// Files matching `patterns` (or `default`) under `dir`, sorted.
pub(crate) fn expand(
    dir: &Path,
    patterns: &[String],
    default: &str,
) -> Result<Vec<PathBuf>, String> {
    let defaults = [default.to_string()];
    let patterns = if patterns.is_empty() {
        &defaults[..]
    } else {
        patterns
    };
    let mut files = vec![];
    for pattern in patterns {
        // `dir/**` means everything under dir (the glob crate would match folders only).
        let pattern = match pattern.strip_suffix("**") {
            Some(base) if base.is_empty() || base.ends_with('/') => format!("{pattern}/*"),
            _ => pattern.clone(),
        };
        let full = dir.join(&pattern);
        let entries = glob::glob(&full.to_string_lossy()).map_err(|e| format!("{pattern}: {e}"))?;
        for entry in entries {
            let path = entry.map_err(|e| e.to_string())?;
            if path.is_file() {
                files.push(path);
            }
        }
    }
    files.sort();
    files.dedup();
    Ok(files)
}
