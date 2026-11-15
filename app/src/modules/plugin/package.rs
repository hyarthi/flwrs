//! Plugin packages: the ZIP checks and extraction live in
//! `flwrs_plugin::package` (shared with `flwrs-trellis`); the server adds its
//! `runtime_defaults` check. Test fixtures for the plugin service live here.

pub(crate) use flwrs_plugin::package::{Limits, Package, PackageError};
use std::path::Path;

/// Validates the ZIP at `zip_path` and, if it's valid, extracts it into `dest`
/// (which must not exist yet).
pub(crate) fn extract_and_validate(
    zip_path: &Path,
    dest: &Path,
    limits: &Limits,
    platform: &str,
) -> Result<Package, PackageError> {
    flwrs_plugin::package::extract_and_validate(
        zip_path,
        dest,
        limits,
        platform,
        &super::manifest::check_runtime_defaults,
    )
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    pub(crate) const PLATFORM: &str = "test-os-test-arch";

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
}
