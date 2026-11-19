//! Remote plugin definitions as JSON: a manifest with `"launch": "remote"`
//! and the files it references (schemas, docs, icon), registered without a
//! ZIP (`POST /api/plugins/remote`, the web UI's wizard). The server packs
//! them into a package and installs it like any upload, so a definition goes
//! through the same checks and is stored, served and used like a package.

use flwrs_plugin::package::{MANIFEST_FILE, check_relative_path};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;
use utoipa::ToSchema;

/// At most this many files beside the manifest.
pub(crate) const MAX_FILES: usize = 64;
/// At most this many bytes in all (decoded).
pub(crate) const MAX_BYTES: usize = 8 * 1024 * 1024;
/// The request body's limit (room for base64 and JSON).
pub(crate) const MAX_BODY_BYTES: usize = MAX_BYTES * 2;

/// A remote plugin definition.
#[derive(Deserialize, Serialize, ToSchema, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub(crate) struct Definition {
    /// `manifest.json` (version 2, `"launch": "remote"`; see `docs/plugin-spec.md`).
    #[schema(value_type = Object)]
    pub manifest: Value,
    /// The files the manifest references, by path in the package.
    #[serde(default)]
    pub files: BTreeMap<String, DefinitionFile>,
}

/// One file's content: JSON (a schema), text (docs, an SVG) or base64 (a PNG).
#[derive(Deserialize, Serialize, ToSchema, Debug, Clone, PartialEq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum DefinitionFile {
    #[schema(value_type = Object)]
    Json(Value),
    Text(String),
    Base64(String),
}

impl DefinitionFile {
    fn bytes(&self) -> Result<Vec<u8>, String> {
        use base64::Engine;
        match self {
            DefinitionFile::Json(v) => Ok(serde_json::to_vec_pretty(v).expect("JSON serializes")),
            DefinitionFile::Text(t) => Ok(t.as_bytes().to_vec()),
            DefinitionFile::Base64(b) => base64::engine::general_purpose::STANDARD
                .decode(b.trim())
                .map_err(|e| format!("not valid base64: {e}")),
        }
    }
}

impl Definition {
    /// Checks the definition's shape and writes it as a package ZIP at
    /// `zip_path`. The package checks (manifest, references, schemas) come
    /// after, at install.
    pub(crate) fn write_package(&self, zip_path: &Path) -> Result<(), Vec<String>> {
        let mut errors = vec![];
        if !self.manifest.is_object() {
            errors.push("manifest must be a JSON object".into());
        } else if self.manifest.get("launch").and_then(Value::as_str) != Some("remote") {
            errors.push(
                "manifest.launch must be \"remote\": a definition describes a plugin that runs elsewhere (upload a package for one this server runs)"
                    .into(),
            );
        }
        if self.files.len() > MAX_FILES {
            errors.push(format!("{} files; at most {MAX_FILES}", self.files.len()));
        }
        let mut contents = vec![];
        let mut total = 0usize;
        for (path, file) in &self.files {
            if let Err(e) = check_relative_path(path) {
                errors.push(format!("files.{path}: the path {e}"));
                continue;
            }
            let path = path.trim_start_matches("./");
            if path == MANIFEST_FILE {
                errors.push(format!("files.{path}: the manifest goes in `manifest`"));
                continue;
            }
            match file.bytes() {
                Ok(bytes) => {
                    total += bytes.len();
                    contents.push((path.to_string(), bytes));
                }
                Err(e) => errors.push(format!("files.{path}: {e}")),
            }
        }
        if total > MAX_BYTES {
            errors.push(format!("the files hold {total} bytes; at most {MAX_BYTES}"));
        }
        if !errors.is_empty() {
            return Err(errors);
        }
        let write = || -> zip::result::ZipResult<()> {
            let mut zip = zip::ZipWriter::new(std::fs::File::create(zip_path)?);
            let opts = zip::write::SimpleFileOptions::default().unix_permissions(0o644);
            zip.start_file(MANIFEST_FILE, opts)?;
            zip.write_all(&serde_json::to_vec_pretty(&self.manifest).expect("JSON serializes"))?;
            for (path, bytes) in &contents {
                zip.start_file(path.as_str(), opts)?;
                zip.write_all(bytes)?;
            }
            zip.finish()?;
            Ok(())
        };
        write().map_err(|e| vec![format!("couldn't write the package: {e}")])
    }

    /// Reads an installed definition back from its folder: the manifest and
    /// every other file (`.json` files that parse as JSON, then UTF-8 text,
    /// else base64).
    pub(crate) fn read(dir: &Path) -> std::io::Result<Self> {
        use base64::Engine;
        let mut manifest = Value::Null;
        let mut files = BTreeMap::new();
        let mut pending = vec![dir.to_path_buf()];
        while let Some(folder) = pending.pop() {
            for entry in std::fs::read_dir(&folder)? {
                let entry = entry?;
                let path = entry.path();
                if entry.file_type()?.is_dir() {
                    pending.push(path);
                    continue;
                }
                let relative = path
                    .strip_prefix(dir)
                    .expect("under the folder")
                    .components()
                    .map(|c| c.as_os_str().to_string_lossy())
                    .collect::<Vec<_>>()
                    .join("/");
                let bytes = std::fs::read(&path)?;
                if relative == MANIFEST_FILE {
                    manifest = serde_json::from_slice(&bytes).map_err(std::io::Error::other)?;
                    continue;
                }
                let file = match std::str::from_utf8(&bytes) {
                    Ok(text) if relative.ends_with(".json") => serde_json::from_str(text)
                        .map(DefinitionFile::Json)
                        .unwrap_or_else(|_| DefinitionFile::Text(text.to_string())),
                    Ok(text) => DefinitionFile::Text(text.to_string()),
                    Err(_) => DefinitionFile::Base64(
                        base64::engine::general_purpose::STANDARD.encode(&bytes),
                    ),
                };
                files.insert(relative, file);
            }
        }
        Ok(Self { manifest, files })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn round_trips_through_a_package() {
        let dir = tempfile::tempdir().unwrap();
        let def: Definition = serde_json::from_value(json!({
            "manifest": {"manifest_version": 2, "id": "far", "name": "Far", "version": "1.0.0",
                         "type": "SOURCE", "launch": "remote",
                         "payload_schemas": {"out": "schema/out.json"},
                         "ui": {"icon": "ui/icon.png", "docs": "README.md"}},
            "files": {
                "schema/out.json": {"json": {"fields": [{"key": "a", "type": "STRING"}]}},
                "README.md": {"text": "# Far"},
                "ui/icon.png": {"base64": "iVBORw0KGgo="}
            }
        }))
        .unwrap();
        let zip = dir.path().join("p.zip");
        def.write_package(&zip).unwrap();
        let out = dir.path().join("out");
        let package = super::super::package::extract_and_validate(
            &zip,
            &out,
            &Default::default(),
            "linux-x86_64",
        )
        .unwrap();
        assert!(package.manifest.is_remote_only());
        let back = Definition::read(&out).unwrap();
        assert_eq!(back.manifest, def.manifest);
        assert_eq!(back.files, def.files);
    }

    #[test]
    fn refuses_what_isnt_a_definition() {
        let dir = tempfile::tempdir().unwrap();
        let def: Definition = serde_json::from_value(json!({
            "manifest": {"manifest_version": 2, "id": "x", "executable": "bin/x"},
            "files": {"../x": {"text": ""}, "manifest.json": {"json": {}}, "a.png": {"base64": "!!"}}
        }))
        .unwrap();
        let errors = def.write_package(&dir.path().join("p.zip")).unwrap_err();
        assert_eq!(errors.len(), 4, "{errors:?}");
        assert!(errors[0].contains("launch must be \"remote\""));
    }
}
