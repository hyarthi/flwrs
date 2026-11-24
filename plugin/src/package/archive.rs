//! Plugin package archives: inspects a ZIP, validates its manifest and the
//! files it references, and extracts it. Every check runs before anything is
//! written to disk.

use super::launch::LaunchSpec;
use super::manifest::{MANIFEST_FILE, Manifest, RESERVED_ENTRY};
use crate::schema::rules;
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};
use thiserror::Error;
use zip::ZipArchive;

/// Size limit for the manifest and the schema files it references.
const MAX_METADATA_FILE_BYTES: u64 = 4 * 1024 * 1024;
/// Entries smaller than this skip the compression-ratio check (tiny files
/// compress extremely well without being dangerous).
const RATIO_CHECK_MIN_BYTES: u64 = 1024 * 1024;

/// Limits on a package's contents (zip-bomb and resource guards).
#[derive(Clone, Debug)]
pub struct Limits {
    /// Maximum total size once unpacked.
    pub max_unpacked_bytes: u64,
    /// Maximum number of entries.
    pub max_entries: usize,
    /// Maximum compression ratio of a single large entry.
    pub max_compression_ratio: u64,
}

impl Default for Limits {
    /// The server's defaults (`[plugins]` section).
    fn default() -> Self {
        Self {
            max_unpacked_bytes: 1024 * 1024 * 1024,
            max_entries: 10_000,
            max_compression_ratio: 200,
        }
    }
}

/// Extra manifest checks of the caller (the server checks `runtime_defaults`
/// against its runtime settings); returns problems, empty if none.
pub type ExtraCheck<'a> = &'a dyn Fn(&Manifest) -> Vec<String>;

/// A validated package, ready to register.
#[derive(Debug)]
pub struct Package {
    pub manifest: Manifest,
    /// The manifest as compact JSON.
    pub manifest_json: String,
    /// Contents of the referenced files, as compact JSON.
    pub launch_args_schema: Option<String>,
    /// A per-node schema's `default`, if it has one.
    pub in_schema: Option<String>,
    pub out_schema: Option<String>,
    /// Problems that don't refuse the package (unknown `x-*` fields and
    /// `x-flwrs-*` keywords).
    pub warnings: Vec<String>,
}

/// Why a package was refused.
#[derive(Error, Debug)]
pub enum PackageError {
    #[error("invalid plugin package: {}", .0.join("; "))]
    Invalid(Vec<String>),
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
}

fn invalid(msg: impl Into<String>) -> PackageError {
    PackageError::Invalid(vec![msg.into()])
}

/// Validates the ZIP at `zip_path` without extracting it: everything the
/// server checks at upload for a hub running on `platform`.
pub fn validate(
    zip_path: &Path,
    limits: &Limits,
    platform: &str,
    extra: ExtraCheck,
) -> Result<Package, PackageError> {
    let mut archive = ZipArchive::new(File::open(zip_path)?)
        .map_err(|e| invalid(format!("not a valid ZIP archive: {e}")))?;
    let entries = inspect_entries(&mut archive, limits)?;
    read_package(&mut archive, &entries, platform, extra)
}

/// Validates the ZIP at `zip_path` and, if it's valid, extracts it into `dest`
/// (which must not exist yet).
pub fn extract_and_validate(
    zip_path: &Path,
    dest: &Path,
    limits: &Limits,
    platform: &str,
    extra: ExtraCheck,
) -> Result<Package, PackageError> {
    let mut archive = ZipArchive::new(File::open(zip_path)?)
        .map_err(|e| invalid(format!("not a valid ZIP archive: {e}")))?;
    let entries = inspect_entries(&mut archive, limits)?;
    let package = read_package(&mut archive, &entries, platform, extra)?;
    let mut executables = vec![package.manifest.executable.as_str()];
    if let Some(c) = &package.manifest.configurator {
        executables.push(c.executable.as_str());
    }
    executables.retain(|e| !e.is_empty()); // a remote plugin definition
    extract(&mut archive, &entries, dest, limits, &executables)?;
    Ok(package)
}

struct Entry {
    index: usize,
    path: PathBuf,
    is_dir: bool,
    size: u64,
    unix_mode: Option<u32>,
}

/// First pass over the central directory: paths, types and declared sizes.
fn inspect_entries(
    archive: &mut ZipArchive<File>,
    limits: &Limits,
) -> Result<Vec<Entry>, PackageError> {
    if archive.len() > limits.max_entries {
        return Err(invalid(format!(
            "package has {} entries; the limit is {}",
            archive.len(),
            limits.max_entries
        )));
    }
    let mut errors = vec![];
    let mut entries = vec![];
    let mut seen = HashSet::new();
    let mut total: u64 = 0;
    for index in 0..archive.len() {
        let file = archive
            .by_index(index)
            .map_err(|e| invalid(format!("unreadable ZIP entry #{index}: {e}")))?;
        let raw_name = file
            .name()
            .map(|n| n.to_string())
            .unwrap_or_else(|_| format!("#{index}"));
        // `enclosed_name` quietly turns `/etc/x` into `etc/x`; reject such names
        // outright instead of rewriting them. Also Windows-style paths.
        let raw_bytes = raw_name.as_bytes();
        if raw_name.starts_with('/')
            || raw_name.contains('\\')
            || (raw_bytes.len() >= 2 && raw_bytes[1] == b':' && raw_bytes[0].is_ascii_alphabetic())
        {
            errors.push(format!("{raw_name}: absolute or non-portable path"));
            continue;
        }
        let Some(path) = file.enclosed_name() else {
            errors.push(format!(
                "{raw_name}: unsafe path (absolute or outside the package)"
            ));
            continue;
        };
        if path.components().any(|c| matches!(c, Component::ParentDir)) {
            errors.push(format!("{raw_name}: path must not contain '..'"));
            continue;
        }
        if file.is_symlink() {
            errors.push(format!("{raw_name}: symbolic links aren't allowed"));
            continue;
        }
        if !file.is_dir() && !file.is_file() {
            errors.push(format!(
                "{raw_name}: only regular files and directories are allowed"
            ));
            continue;
        }
        let normalized: PathBuf = path
            .components()
            .filter(|c| !matches!(c, Component::CurDir))
            .collect();
        if normalized.as_os_str().is_empty() {
            continue; // the root directory itself
        }
        if !seen.insert(normalized.clone()) {
            errors.push(format!("{raw_name}: duplicate entry"));
            continue;
        }
        let size = file.size();
        let compressed = file.compressed_size().max(1);
        if size >= RATIO_CHECK_MIN_BYTES && size / compressed > limits.max_compression_ratio {
            errors.push(format!(
                "{raw_name}: compression ratio {}:1 exceeds the limit of {}:1",
                size / compressed,
                limits.max_compression_ratio
            ));
        }
        total = total.saturating_add(size);
        entries.push(Entry {
            index,
            path: normalized,
            is_dir: file.is_dir(),
            size,
            unix_mode: file.unix_mode(),
        });
    }
    if total > limits.max_unpacked_bytes {
        errors.push(format!(
            "package unpacks to {total} bytes; the limit is {}",
            limits.max_unpacked_bytes
        ));
    }
    if errors.is_empty() {
        Ok(entries)
    } else {
        Err(PackageError::Invalid(errors))
    }
}

/// Reads and validates the manifest and every file it references.
fn read_package(
    archive: &mut ZipArchive<File>,
    entries: &[Entry],
    platform: &str,
    extra: ExtraCheck,
) -> Result<Package, PackageError> {
    let find = |path: &str| -> Option<&Entry> {
        let wanted: PathBuf = Path::new(path)
            .components()
            .filter(|c| !matches!(c, Component::CurDir))
            .collect();
        entries.iter().find(|e| e.path == wanted)
    };

    let manifest_entry = find(MANIFEST_FILE)
        .filter(|e| !e.is_dir)
        .ok_or_else(|| invalid(format!("{MANIFEST_FILE} is missing from the package root")))?;
    let manifest_text = read_text(archive, manifest_entry, MANIFEST_FILE)?;
    let manifest_value: serde_json::Value = serde_json::from_str(&manifest_text)
        .map_err(|e| invalid(format!("{MANIFEST_FILE}: not valid JSON: {e}")))?;
    let manifest: Manifest = serde_json::from_value(manifest_value.clone())
        .map_err(|e| invalid(format!("{MANIFEST_FILE}: {e}")))?;

    let mut errors = manifest.validate(platform);
    let mut warnings = manifest.warnings();
    errors.extend(extra(&manifest));
    if entries
        .iter()
        .any(|e| e.path.components().next() == Some(Component::Normal(RESERVED_ENTRY.as_ref())))
    {
        errors.push(format!(
            "the package can't have a top-level {RESERVED_ENTRY:?} entry (the name of a node's folder for its config files)"
        ));
    }
    for (field, path) in manifest.file_references() {
        match find(path) {
            Some(e) if !e.is_dir => {}
            Some(_) => errors.push(format!("{field} {path:?} is a directory, not a file")),
            None => errors.push(format!("{field} {path:?} is missing from the package")),
        }
    }
    if !errors.is_empty() {
        return Err(PackageError::Invalid(errors));
    }

    let launch_args_schema = match &manifest.launch_args_schema {
        None => None,
        Some(path) => {
            let text = read_text(archive, find(path).expect("checked above"), path)?;
            let value: serde_json::Value = serde_json::from_str(&text).map_err(|e| {
                invalid(format!("launch_args_schema {path:?}: not valid JSON: {e}"))
            })?;
            // Remote and file $ref resolution is compiled out (see the workspace
            // Cargo.toml), so an external $ref fails here instead of being fetched.
            if let Err(e) = jsonschema::validator_for(&value) {
                errors.push(format!(
                    "launch_args_schema {path:?}: not a valid JSON Schema: {e}"
                ));
            }
            Some(value)
        }
    };
    let (spec, report) = LaunchSpec::new(&manifest, launch_args_schema.as_ref());
    errors.extend(report.errors);
    warnings.extend(report.warnings);
    for (field, path) in spec.file_references() {
        match find(path) {
            Some(e) if !e.is_dir => {}
            _ => errors.push(format!("{field} {path:?} is missing from the package")),
        }
    }
    // A template for a value that isn't text is that value, as JSON.
    for t in spec.template_fields().iter().filter(|t| !t.text) {
        if let Some(entry) = find(&t.path).filter(|e| !e.is_dir) {
            let text = read_text(archive, entry, &t.path)?;
            if let Err(e) = serde_json::from_str::<serde_json::Value>(&text) {
                errors.push(format!(
                    "launch_args_schema {}: x-flwrs-template {:?} isn't JSON (the property isn't a string): {e}",
                    t.pointer, t.path
                ));
            }
        }
    }
    // A graph's catalog, against the schema it refers to.
    if let Some(schema) = &launch_args_schema {
        for g in spec.graph_fields() {
            if let Some(entry) = find(&g.catalog).filter(|e| !e.is_dir) {
                let text = read_text(archive, entry, &g.catalog)?;
                if let Err(problems) = super::graph::Catalog::parse(&text, schema) {
                    errors.extend(problems.into_iter().map(|p| {
                        format!(
                            "launch_args_schema {}: x-flwrs-graph {:?}: {p}",
                            g.pointer, g.catalog
                        )
                    }));
                }
            }
        }
    }
    let launch_args_schema = launch_args_schema.map(|v| v.to_string());
    let mut payload_schema = |field: &str,
                              source: &Option<super::manifest::SchemaSource>|
     -> Result<Option<String>, PackageError> {
        let Some(path) = source.as_ref().and_then(|s| s.file()) else {
            return Ok(None);
        };
        let text = read_text(archive, find(path).expect("checked above"), path)?;
        match rules::parse_json(&text) {
            Ok(schema) => {
                errors.extend(
                    rules::check_well_formed(&schema)
                        .into_iter()
                        .map(|e| format!("{field} {path:?}: {e}")),
                );
                let value: serde_json::Value = serde_json::from_str(&text).expect("parsed above");
                Ok(Some(value.to_string()))
            }
            Err(e) => {
                errors.push(format!(
                    "{field} {path:?}: not a valid SchemaDefinition: {e}"
                ));
                Ok(None)
            }
        }
    };
    let in_schema = payload_schema("payload_schemas.in", &manifest.payload_schemas.input)?;
    let out_schema = payload_schema("payload_schemas.out", &manifest.payload_schemas.output)?;
    if !errors.is_empty() {
        return Err(PackageError::Invalid(errors));
    }

    Ok(Package {
        manifest_json: manifest_value.to_string(),
        manifest,
        launch_args_schema,
        in_schema,
        out_schema,
        warnings,
    })
}

fn read_text(
    archive: &mut ZipArchive<File>,
    entry: &Entry,
    label: &str,
) -> Result<String, PackageError> {
    if entry.size > MAX_METADATA_FILE_BYTES {
        return Err(invalid(format!(
            "{label} is {} bytes; the limit is {MAX_METADATA_FILE_BYTES}",
            entry.size
        )));
    }
    let file = archive
        .by_index(entry.index)
        .map_err(|e| invalid(format!("{label}: unreadable: {e}")))?;
    let mut text = String::new();
    file.take(MAX_METADATA_FILE_BYTES + 1)
        .read_to_string(&mut text)
        .map_err(|e| invalid(format!("{label}: not readable as UTF-8 text: {e}")))?;
    Ok(text)
}

/// Writes the entries under `dest`. Sizes are enforced on the actual bytes,
/// since a crafted archive can lie about them in its headers.
fn extract(
    archive: &mut ZipArchive<File>,
    entries: &[Entry],
    dest: &Path,
    limits: &Limits,
    executables: &[&str],
) -> Result<(), PackageError> {
    fs::create_dir(dest)?;
    let mut written: u64 = 0;
    for entry in entries {
        let target = dest.join(&entry.path);
        if entry.is_dir {
            fs::create_dir_all(&target)?;
            continue;
        }
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = archive
            .by_index(entry.index)
            .map_err(|e| invalid(format!("{}: unreadable: {e}", entry.path.display())))?;
        let mut out = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&target)?;
        let budget = limits.max_unpacked_bytes.saturating_sub(written);
        let mut limited = file.take(budget.min(entry.size) + 1);
        let copied = io::copy(&mut limited, &mut out)
            .map_err(|e| invalid(format!("{}: failed to extract: {e}", entry.path.display())))?;
        if copied > entry.size || copied > budget {
            return Err(invalid(format!(
                "{}: actual size exceeds the declared size or the unpacked limit",
                entry.path.display()
            )));
        }
        written += copied;
        set_permissions(&target, entry.unix_mode)?;
    }
    // ZIPs made on Windows don't carry the executable bit.
    for executable in executables {
        make_executable(&dest.join(executable))?;
    }
    Ok(())
}

#[cfg(unix)]
fn set_permissions(path: &Path, mode: Option<u32>) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    // Keep only rwx bits: no setuid, setgid or sticky bits from the archive.
    let mode = mode.map(|m| m & 0o777).unwrap_or(0o644) | 0o600;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn set_permissions(_: &Path, _: Option<u32>) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn make_executable(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = fs::metadata(path)?.permissions().mode();
    fs::set_permissions(path, fs::Permissions::from_mode(mode | 0o755))
}

#[cfg(not(unix))]
fn make_executable(_: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    const PLATFORM: &str = "test-os-test-arch";

    fn no_extra(_: &Manifest) -> Vec<String> {
        vec![]
    }

    fn limits() -> Limits {
        Limits {
            max_unpacked_bytes: 64 * 1024 * 1024,
            max_entries: 100,
            max_compression_ratio: 200,
        }
    }

    const IN_SCHEMA: &str =
        r#"{"fields": [{"key": "url", "type": "STRING"}, {"key": "body", "type": "BYTES"}]}"#;
    const LAUNCH_SCHEMA: &str = r#"{"type": "object",
        "properties": {"timeout": {"type": "integer", "minimum": 1}}, "additionalProperties": false}"#;

    fn manifest_json(id: &str, version: &str) -> String {
        format!(
            r#"{{"manifest_version": 1, "id": "{id}", "name": "Test sink", "version": "{version}",
                "type": "SINK", "executable": "bin/run", "platforms": ["{PLATFORM}"],
                "launch_args_schema": "schema/launch.json", "payload_schemas": {{"in": "schema/in.json"}}}}"#
        )
    }

    enum Item<'a> {
        File(&'a str, &'a [u8]),
        Dir(&'a str),
        Symlink(&'a str, &'a str),
    }

    fn build_zip(path: &Path, items: &[Item]) {
        let mut zip = zip::ZipWriter::new(File::create(path).unwrap());
        let opts = SimpleFileOptions::default().unix_permissions(0o644);
        for item in items {
            match item {
                Item::File(name, data) => {
                    zip.start_file(*name, opts).unwrap();
                    zip.write_all(data).unwrap();
                }
                Item::Dir(name) => zip.add_directory(*name, opts).unwrap(),
                Item::Symlink(name, target) => zip.add_symlink(*name, *target, opts).unwrap(),
            }
        }
        zip.finish().unwrap();
    }

    /// A complete, valid package.
    fn valid_package(path: &Path, id: &str, version: &str) {
        build_zip(
            path,
            &[
                Item::File("manifest.json", manifest_json(id, version).as_bytes()),
                Item::Dir("bin/"),
                Item::File("bin/run", b"#!/bin/sh\necho hi\n"),
                Item::Dir("assets/"),
                Item::File("schema/launch.json", LAUNCH_SCHEMA.as_bytes()),
                Item::File("schema/in.json", IN_SCHEMA.as_bytes()),
                Item::File("assets/readme.txt", b"optional extra file"),
            ],
        );
    }

    fn run(items: &[Item]) -> (tempfile::TempDir, Result<Package, PackageError>) {
        let dir = tempfile::tempdir().unwrap();
        let zip_path = dir.path().join("p.zip");
        build_zip(&zip_path, items);
        let result = extract_and_validate(
            &zip_path,
            &dir.path().join("out"),
            &limits(),
            PLATFORM,
            &no_extra,
        );
        (dir, result)
    }

    fn errors_of(result: Result<Package, PackageError>) -> Vec<String> {
        match result {
            Err(PackageError::Invalid(errors)) => errors,
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    fn base_items<'a>(manifest: &'a str) -> Vec<Item<'a>> {
        vec![
            Item::File("manifest.json", manifest.as_bytes()),
            Item::File("bin/run", b"#!/bin/sh\n"),
            Item::File("schema/launch.json", LAUNCH_SCHEMA.as_bytes()),
            Item::File("schema/in.json", IN_SCHEMA.as_bytes()),
        ]
    }

    #[test]
    fn valid_package_is_extracted() {
        let dir = tempfile::tempdir().unwrap();
        let zip_path = dir.path().join("p.zip");
        valid_package(&zip_path, "test-sink", "1.0.0");
        let out = dir.path().join("out");
        let package =
            extract_and_validate(&zip_path, &out, &limits(), PLATFORM, &no_extra).unwrap();
        assert_eq!(package.manifest.id, "test-sink");
        assert!(package.launch_args_schema.is_some() && package.in_schema.is_some());
        assert!(package.out_schema.is_none());
        assert!(out.join("assets/readme.txt").is_file());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(out.join("bin/run"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o111, 0o111, "executable bit set");
        }
    }

    #[test]
    fn validate_checks_without_extracting_and_runs_extra_checks() {
        let dir = tempfile::tempdir().unwrap();
        let zip_path = dir.path().join("p.zip");
        valid_package(&zip_path, "test-sink", "1.0.0");
        let package = validate(&zip_path, &limits(), PLATFORM, &no_extra).unwrap();
        assert_eq!(package.manifest.id, "test-sink");
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            1,
            "only the ZIP"
        );

        let extra = |m: &Manifest| vec![format!("extra check saw {}", m.id)];
        let err = validate(&zip_path, &limits(), PLATFORM, &extra).unwrap_err();
        assert!(
            err.to_string().contains("extra check saw test-sink"),
            "{err}"
        );
    }

    #[test]
    fn rejects_missing_manifest_and_executable() {
        let (_d, r) = run(&[Item::File("bin/run", b"x")]);
        assert!(errors_of(r)[0].contains("manifest.json is missing"));

        let manifest = manifest_json("a", "1.0.0");
        let (_d, r) = run(&[
            Item::File("manifest.json", manifest.as_bytes()),
            Item::File("schema/launch.json", LAUNCH_SCHEMA.as_bytes()),
            Item::File("schema/in.json", IN_SCHEMA.as_bytes()),
        ]);
        assert!(
            errors_of(r)
                .iter()
                .any(|e| e.contains("executable \"bin/run\" is missing"))
        );
    }

    #[test]
    fn rejects_unsafe_paths_and_symlinks() {
        let manifest = manifest_json("a", "1.0.0");
        let mut items = base_items(&manifest);
        items.push(Item::File("../evil", b"x"));
        items.push(Item::File("/etc/evil", b"x"));
        items.push(Item::File("C:/evil", b"x"));
        items.push(Item::Symlink("link", "/etc/passwd"));
        let (dir, r) = run(&items);
        let errors = errors_of(r);
        assert!(errors.iter().any(|e| e.contains("../evil")), "{errors:?}");
        assert!(errors.iter().any(|e| e.contains("/etc/evil")), "{errors:?}");
        assert!(errors.iter().any(|e| e.contains("C:/evil")), "{errors:?}");
        assert!(
            errors.iter().any(|e| e.contains("symbolic links")),
            "{errors:?}"
        );
        assert!(!dir.path().join("out").exists(), "nothing extracted");
        assert!(!dir.path().join("evil").exists());
    }

    #[test]
    fn rejects_duplicates_and_too_many_entries() {
        let manifest = manifest_json("a", "1.0.0");
        let mut items = base_items(&manifest);
        items.push(Item::File("./bin/run", b"again"));
        let (_d, r) = run(&items);
        assert!(errors_of(r).iter().any(|e| e.contains("duplicate entry")));

        let dir = tempfile::tempdir().unwrap();
        let zip_path = dir.path().join("p.zip");
        let names: Vec<String> = (0..5).map(|i| format!("f{i}")).collect();
        build_zip(
            &zip_path,
            &names
                .iter()
                .map(|n| Item::File(n, b"x"))
                .collect::<Vec<_>>(),
        );
        let small = Limits {
            max_entries: 3,
            ..limits()
        };
        let err = extract_and_validate(
            &zip_path,
            &dir.path().join("out"),
            &small,
            PLATFORM,
            &no_extra,
        )
        .unwrap_err();
        assert!(err.to_string().contains("5 entries; the limit is 3"));
    }

    #[test]
    fn rejects_zip_bombs() {
        let manifest = manifest_json("a", "1.0.0");
        let zeros = vec![0u8; 4 * 1024 * 1024]; // compresses ~1000:1
        let mut items = base_items(&manifest);
        items.push(Item::File("bomb.bin", &zeros));
        let (_d, r) = run(&items);
        assert!(errors_of(r).iter().any(|e| e.contains("compression ratio")));

        let dir = tempfile::tempdir().unwrap();
        let zip_path = dir.path().join("p.zip");
        let mut items = base_items(&manifest);
        items.push(Item::File("big.bin", &zeros));
        build_zip(&zip_path, &items);
        let small = Limits {
            max_unpacked_bytes: 1024 * 1024,
            max_compression_ratio: 1_000_000,
            ..limits()
        };
        let err = extract_and_validate(
            &zip_path,
            &dir.path().join("out"),
            &small,
            PLATFORM,
            &no_extra,
        )
        .unwrap_err();
        assert!(err.to_string().contains("the limit is 1048576"), "{err}");
    }

    #[test]
    fn rejects_wrong_platform_and_bad_schemas() {
        let manifest = manifest_json("a", "1.0.0");
        let dir = tempfile::tempdir().unwrap();
        let zip_path = dir.path().join("p.zip");
        build_zip(&zip_path, &base_items(&manifest));
        let err = extract_and_validate(
            &zip_path,
            &dir.path().join("out"),
            &limits(),
            "linux-x86_64",
            &no_extra,
        )
        .unwrap_err();
        assert!(err.to_string().contains("this hub runs on linux-x86_64"));

        let bad_launch = r#"{"type": 12}"#;
        let remote_ref = r#"{"$ref": "https://example.com/schema.json"}"#;
        let bad_payload = r#"{"fields": [{"key": "x", "type": "UNDEFINED"}]}"#;
        for (launch, payload, expected) in [
            (bad_launch, IN_SCHEMA, "not a valid JSON Schema"),
            (remote_ref, IN_SCHEMA, "not a valid JSON Schema"),
            (LAUNCH_SCHEMA, bad_payload, "type is UNDEFINED"),
            (
                LAUNCH_SCHEMA,
                "{\"fields\": 3}",
                "not a valid SchemaDefinition",
            ),
            ("not json", IN_SCHEMA, "not valid JSON"),
        ] {
            let (_d, r) = run(&[
                Item::File("manifest.json", manifest.as_bytes()),
                Item::File("bin/run", b"x"),
                Item::File("schema/launch.json", launch.as_bytes()),
                Item::File("schema/in.json", payload.as_bytes()),
            ]);
            let errors = errors_of(r);
            assert!(
                errors.iter().any(|e| e.contains(expected)),
                "{expected}: {errors:?}"
            );
        }
    }

    #[test]
    fn rejects_invalid_manifests() {
        let (_d, r) = run(&[Item::File("manifest.json", b"{not json")]);
        assert!(errors_of(r)[0].contains("not valid JSON"));
        let (_d, r) = run(&[Item::File("manifest.json", br#"{"id": "a"}"#)]);
        assert!(errors_of(r)[0].contains("missing field"));
        let manifest = manifest_json("Bad_ID", "x");
        let (_d, r) = run(&base_items(&manifest));
        let errors = errors_of(r);
        assert!(
            errors.iter().any(|e| e.contains("id \"Bad_ID\""))
                && errors.iter().any(|e| e.contains("version \"x\""))
        );
    }

    const V2_MANIFEST: &str = r#"{"manifest_version": 2, "id": "script", "name": "Script",
        "version": "0.1.0", "type": "SINK", "executable": "bin/run",
        "launch_args_schema": "schema/launch.json",
        "payload_schemas": {"in": {"from_launch_args": "/schemas/in", "default": "schema/in.json"}},
        "ui": {"icon": "ui/icon.svg", "categories": ["Scripting"]},
        "x-vendor": true}"#;
    const V2_LAUNCH_SCHEMA: &str = r#"{"type": "object", "properties": {
        "schemas": {"type": "object", "properties": {"in": {"type": "object"}}},
        "script": {"type": "string", "contentMediaType": "text/x-lua",
                   "x-flwrs-code": {"language": "lua", "params": ["payload"], "definitions": "types/script.d.lua"},
                   "x-flwrs-hint": 1}}}"#;

    #[test]
    fn graph_catalogs_are_checked_against_the_schema() {
        let launch = r##"{"type": "object", "properties": {
            "schemas": {"type": "object", "properties": {"in": {"type": "object"}}},
            "graph": {"type": "object", "x-flwrs-graph": {"catalog": "schema/catalog.json"}}},
            "$defs": {"a": {"properties": {"type": {"const": "a"}}}}}"##;
        let good: &[u8] = br##"{"format": 1, "port_types": {"x": {"title": "X"}},
            "node_types": {"a": {"title": "A", "settings": "#/$defs/a"}}}"##;
        let bad: &[u8] = br##"{"format": 1, "port_types": {"x": {"title": "X"}},
            "node_types": {"a": {"title": "A", "settings": "#/$defs/nope"}}}"##;
        let items = |catalog: Option<&'static [u8]>| {
            let mut items = vec![
                Item::File("manifest.json", V2_MANIFEST.as_bytes()),
                Item::File("bin/run", b"#!/bin/sh\n"),
                Item::File("schema/launch.json", launch.as_bytes()),
                Item::File("schema/in.json", IN_SCHEMA.as_bytes()),
                Item::File("ui/icon.svg", b"<svg/>"),
            ];
            if let Some(c) = catalog {
                items.push(Item::File("schema/catalog.json", c));
            }
            items
        };
        let (_d, r) = run(&items(Some(good)));
        r.unwrap();
        for (catalog, expected) in [
            (Some(bad), "isn't in the launch-args"),
            (None, "is missing"),
        ] {
            let (_d, r) = run(&items(catalog));
            let errors = errors_of(r);
            assert!(errors.iter().any(|e| e.contains(expected)), "{errors:?}");
        }
    }

    #[test]
    fn templates_must_be_there_and_json_for_values() {
        let launch = r#"{"type": "object", "properties": {
            "schemas": {"type": "object", "properties": {"in": {"type": "object"}},
                        "x-flwrs-template": "templates/schemas.json"},
            "script": {"type": "string", "x-flwrs-template": "templates/script.lua"}}}"#;
        let items = |schemas: &'static [u8], script: bool| {
            let mut items = vec![
                Item::File("manifest.json", V2_MANIFEST.as_bytes()),
                Item::File("bin/run", b"#!/bin/sh\n"),
                Item::File("schema/launch.json", launch.as_bytes()),
                Item::File("schema/in.json", IN_SCHEMA.as_bytes()),
                Item::File("ui/icon.svg", b"<svg/>"),
                Item::File("templates/schemas.json", schemas),
            ];
            if script {
                // Text: anything goes.
                items.push(Item::File(
                    "templates/script.lua",
                    b"return payload -- {not json",
                ));
            }
            items
        };
        let (_d, r) = run(&items(br#"{"in": {"fields": []}}"#, true));
        r.unwrap();
        let (_d, r) = run(&items(b"in: {}", true));
        let errors = errors_of(r);
        assert!(
            errors.iter().any(|e| e.contains("isn't JSON")),
            "{errors:?}"
        );
        let (_d, r) = run(&items(br#"{"in": {}}"#, false));
        let errors = errors_of(r);
        assert!(
            errors
                .iter()
                .any(|e| e.contains("\"templates/script.lua\" is missing")),
            "{errors:?}"
        );
    }

    #[test]
    fn version_2_package() {
        let items = |defs: bool, config_entry: bool| {
            let mut items = vec![
                Item::File("manifest.json", V2_MANIFEST.as_bytes()),
                Item::File("bin/run", b"#!/bin/sh\n"),
                Item::File("schema/launch.json", V2_LAUNCH_SCHEMA.as_bytes()),
                Item::File("schema/in.json", IN_SCHEMA.as_bytes()),
                Item::File("ui/icon.svg", b"<svg/>"),
            ];
            if defs {
                items.push(Item::File("types/script.d.lua", b"---@meta\n"));
            }
            if config_entry {
                items.push(Item::File("config/x.json", b"{}"));
            }
            items
        };
        let (_d, r) = run(&items(true, false));
        let package = r.unwrap();
        // the per-node schema's default is the package's schema
        assert!(package.in_schema.is_some());
        assert_eq!(package.warnings.len(), 2, "{:?}", package.warnings);
        assert!(package.warnings.iter().any(|w| w.contains("x-vendor")));
        assert!(package.warnings.iter().any(|w| w.contains("x-flwrs-hint")));

        let (_d, r) = run(&items(false, false));
        let errors = errors_of(r);
        assert!(
            errors
                .iter()
                .any(|e| e.contains("\"types/script.d.lua\" is missing")),
            "{errors:?}"
        );
        let (_d, r) = run(&items(true, true));
        let errors = errors_of(r);
        assert!(
            errors
                .iter()
                .any(|e| e.contains("top-level \"config\" entry")),
            "{errors:?}"
        );
    }
}
