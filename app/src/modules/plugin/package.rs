//! Plugin packages: inspects an uploaded ZIP, validates its manifest and the
//! files it references, and extracts it. Every check runs before anything is
//! written to disk.

use super::manifest::{MANIFEST_FILE, Manifest};
use flwrs_plugin::schema::rules;
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

#[derive(Clone, Debug)]
pub(crate) struct Limits {
    pub max_unpacked_bytes: u64,
    pub max_entries: usize,
    pub max_compression_ratio: u64,
}

/// A validated package, ready to register.
#[derive(Debug)]
pub(crate) struct Package {
    pub manifest: Manifest,
    /// The manifest as compact JSON.
    pub manifest_json: String,
    /// Contents of the referenced files, as compact JSON.
    pub launch_args_schema: Option<String>,
    pub in_schema: Option<String>,
    pub out_schema: Option<String>,
}

#[derive(Error, Debug)]
pub(crate) enum PackageError {
    #[error("invalid plugin package: {}", .0.join("; "))]
    Invalid(Vec<String>),
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
}

fn invalid(msg: impl Into<String>) -> PackageError {
    PackageError::Invalid(vec![msg.into()])
}

/// Validates the ZIP at `zip_path` and, if it's valid, extracts it into `dest`
/// (which must not exist yet).
pub(crate) fn extract_and_validate(
    zip_path: &Path,
    dest: &Path,
    limits: &Limits,
    platform: &str,
) -> Result<Package, PackageError> {
    let mut archive = ZipArchive::new(File::open(zip_path)?)
        .map_err(|e| invalid(format!("not a valid ZIP archive: {e}")))?;
    let entries = inspect_entries(&mut archive, limits)?;
    let package = read_package(&mut archive, &entries, platform)?;
    extract(
        &mut archive,
        &entries,
        dest,
        limits,
        &package.manifest.executable,
    )?;
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
            Some(value.to_string())
        }
    };
    let mut payload_schema = |field: &str,
                              path: &Option<String>|
     -> Result<Option<String>, PackageError> {
        let Some(path) = path else { return Ok(None) };
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
    executable: &str,
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
    make_executable(&dest.join(executable))?;
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
pub(crate) mod tests {
    use super::*;
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    pub(crate) const PLATFORM: &str = "test-os-test-arch";

    pub(crate) fn limits() -> Limits {
        Limits {
            max_unpacked_bytes: 64 * 1024 * 1024,
            max_entries: 100,
            max_compression_ratio: 200,
        }
    }

    pub(crate) const IN_SCHEMA: &str =
        r#"{"fields": [{"key": "url", "type": "STRING"}, {"key": "body", "type": "BYTES"}]}"#;
    pub(crate) const LAUNCH_SCHEMA: &str = r#"{"type": "object",
        "properties": {"timeout": {"type": "integer", "minimum": 1}}, "additionalProperties": false}"#;

    pub(crate) fn manifest_json(id: &str, version: &str) -> String {
        format!(
            r#"{{"manifest_version": 1, "id": "{id}", "name": "Test sink", "version": "{version}",
                "type": "SINK", "executable": "bin/run", "platforms": ["{PLATFORM}"],
                "launch_args_schema": "schema/launch.json", "payload_schemas": {{"in": "schema/in.json"}}}}"#
        )
    }

    pub(crate) enum Item<'a> {
        File(&'a str, &'a [u8]),
        Dir(&'a str),
        Symlink(&'a str, &'a str),
    }

    pub(crate) fn build_zip(path: &Path, items: &[Item]) {
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
    pub(crate) fn valid_package(path: &Path, id: &str, version: &str) {
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
        let result = extract_and_validate(&zip_path, &dir.path().join("out"), &limits(), PLATFORM);
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
        let package = extract_and_validate(&zip_path, &out, &limits(), PLATFORM).unwrap();
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
        let err =
            extract_and_validate(&zip_path, &dir.path().join("out"), &small, PLATFORM).unwrap_err();
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
        let err =
            extract_and_validate(&zip_path, &dir.path().join("out"), &small, PLATFORM).unwrap_err();
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
}
