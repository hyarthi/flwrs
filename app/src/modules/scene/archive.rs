//! Scene archives: what an export with files writes (`.far`: a ZIP of
//! `scene.flwr` + `files/<stem>_<sha256><.ext>`), and what an import reads:
//! a bare `.flwr`, or a ZIP, tar or tar.gz with exactly one `*.flwr` at its
//! root. An archive's other files are found by their SHA-256, wherever they
//! are and whatever they're called, so hand-made and repacked archives work.
//!
//! Every format gets the same checks: no absolute or `..` paths, only regular
//! files and folders (no links or devices), a limit on entries, and the
//! unpacked bytes counted while unpacking (a gzip stream has no size up front).

use super::scene_file::{EXTENSION, MAGIC, MAX_FILE_BYTES};
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, BufReader, Read, Write};
use std::path::{Component, Path, PathBuf};

/// What an import file is, by its first bytes (not its name).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    /// A bare `.flwr`.
    Scene,
    Zip,
    Tar,
    TarGz,
}

impl Kind {
    pub(crate) fn detect(head: &[u8]) -> Option<Kind> {
        if head.starts_with(MAGIC) {
            Some(Kind::Scene)
        } else if head.starts_with(b"PK\x03\x04") || head.starts_with(b"PK\x05\x06") {
            Some(Kind::Zip)
        } else if head.starts_with(&[0x1f, 0x8b]) {
            Some(Kind::TarGz)
        } else if head.len() >= 262 && &head[257..262] == b"ustar" {
            Some(Kind::Tar)
        } else {
            None
        }
    }
}

/// Limits on an imported archive.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Limits {
    pub max_bytes: u64,
    pub max_entries: usize,
}

/// An unpacked archive.
#[derive(Debug)]
pub(crate) struct Unpacked {
    /// The `.flwr` file's bytes.
    pub scene: Vec<u8>,
    /// Every other regular file, by SHA-256 (unpacked under the work folder).
    pub files: HashMap<String, PathBuf>,
}

/// Unpacks the archive at `path` (of `kind`, not `Scene`) into `work` (an
/// empty folder). Blocking.
pub(crate) fn unpack(
    path: &Path,
    kind: Kind,
    work: &Path,
    limits: Limits,
) -> Result<Unpacked, String> {
    let mut sink = Sink::new(work, limits);
    match kind {
        Kind::Scene => return Err("not an archive".into()),
        Kind::Zip => {
            let file = File::open(path).map_err(|e| e.to_string())?;
            let mut zip = zip::ZipArchive::new(BufReader::new(file))
                .map_err(|e| format!("not a valid ZIP archive: {e}"))?;
            if zip.len() > limits.max_entries {
                return Err(format!(
                    "the archive has {} entries; the limit is {}",
                    zip.len(),
                    limits.max_entries
                ));
            }
            for i in 0..zip.len() {
                let mut entry = zip
                    .by_index(i)
                    .map_err(|e| format!("unreadable ZIP entry #{i}: {e}"))?;
                let name = entry
                    .name()
                    .map_err(|e| format!("ZIP entry #{i} has an unreadable name: {e}"))?
                    .to_string();
                if entry.is_dir() {
                    sink.check_path(&name)?;
                    continue;
                }
                if entry.is_symlink() || !entry.is_file() {
                    return Err(format!(
                        "{name}: only regular files and folders are allowed"
                    ));
                }
                sink.add(&name, &mut entry)?;
            }
        }
        Kind::Tar => {
            let file = File::open(path).map_err(|e| e.to_string())?;
            read_tar(tar::Archive::new(BufReader::new(file)), &mut sink)?;
        }
        Kind::TarGz => {
            let file = File::open(path).map_err(|e| e.to_string())?;
            let gz = flate2::read::GzDecoder::new(BufReader::new(file));
            read_tar(tar::Archive::new(gz), &mut sink)?;
        }
    }
    sink.finish()
}

fn read_tar<R: Read>(mut archive: tar::Archive<R>, sink: &mut Sink) -> Result<(), String> {
    let bad = |e: io::Error| format!("not a valid tar archive: {e}");
    for entry in archive.entries().map_err(bad)? {
        let mut entry = entry.map_err(bad)?;
        sink.count_entry()?;
        let name = entry.path_bytes().into_owned();
        let name = String::from_utf8_lossy(&name).into_owned();
        match entry.header().entry_type() {
            tar::EntryType::Directory => {
                sink.check_path(&name)?;
            }
            tar::EntryType::Regular | tar::EntryType::Continuous => sink.add(&name, &mut entry)?,
            // Metadata entries some tar writers add; they carry no files.
            tar::EntryType::XGlobalHeader | tar::EntryType::XHeader => {}
            other => {
                return Err(format!(
                    "{name}: only regular files and folders are allowed (found {other:?})"
                ));
            }
        }
    }
    Ok(())
}

/// Collects an archive's files under `work`, with the limits.
struct Sink<'a> {
    work: &'a Path,
    limits: Limits,
    entries: usize,
    /// Files written to `work` so far (their names).
    written: usize,
    bytes: u64,
    scenes: Vec<(String, Vec<u8>)>,
    files: HashMap<String, PathBuf>,
}

impl<'a> Sink<'a> {
    fn new(work: &'a Path, limits: Limits) -> Self {
        Self {
            work,
            limits,
            entries: 0,
            written: 0,
            bytes: 0,
            scenes: vec![],
            files: HashMap::new(),
        }
    }

    fn count_entry(&mut self) -> Result<(), String> {
        self.entries += 1;
        if self.entries > self.limits.max_entries {
            return Err(format!(
                "the archive has more than {} entries",
                self.limits.max_entries
            ));
        }
        Ok(())
    }

    /// A safe relative path: no absolute, drive-letter or `..` parts.
    fn check_path(&self, name: &str) -> Result<PathBuf, String> {
        let bytes = name.as_bytes();
        if name.starts_with('/')
            || name.contains('\\')
            || (bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic())
        {
            return Err(format!("{name}: absolute or non-portable path"));
        }
        let mut clean = PathBuf::new();
        for part in Path::new(name).components() {
            match part {
                Component::Normal(p) => clean.push(p),
                Component::CurDir => {}
                _ => return Err(format!("{name}: path must stay inside the archive")),
            }
        }
        Ok(clean)
    }

    fn add(&mut self, name: &str, content: &mut dyn Read) -> Result<(), String> {
        let path = self.check_path(name)?;
        let left = self.limits.max_bytes.saturating_sub(self.bytes);
        let too_big = || {
            format!(
                "the archive unpacks to more than {} bytes",
                self.limits.max_bytes
            )
        };
        let is_scene =
            path.components().count() == 1 && path.extension().is_some_and(|e| e == EXTENSION);
        if is_scene {
            let mut bytes = vec![];
            content
                .take(MAX_FILE_BYTES as u64 + 1)
                .read_to_end(&mut bytes)
                .map_err(|e| format!("{name}: {e}"))?;
            if bytes.len() > MAX_FILE_BYTES {
                return Err(format!("{name}: larger than {MAX_FILE_BYTES} bytes"));
            }
            self.bytes += bytes.len() as u64;
            if self.bytes > self.limits.max_bytes {
                return Err(too_big());
            }
            self.scenes.push((name.to_string(), bytes));
            return Ok(());
        }
        // Content is all that matters: write it under a neutral name, then hash it.
        self.written += 1;
        let out = self.work.join(self.written.to_string());
        let mut file = File::create(&out).map_err(|e| e.to_string())?;
        let written =
            io::copy(&mut content.take(left + 1), &mut file).map_err(|e| format!("{name}: {e}"))?;
        if written > left {
            return Err(too_big());
        }
        self.bytes += written;
        file.flush().map_err(|e| e.to_string())?;
        drop(file);
        let sha = sha256::try_digest(out.as_path()).map_err(|e| e.to_string())?;
        match self.files.entry(sha) {
            // The same content twice.
            std::collections::hash_map::Entry::Occupied(_) => {
                let _ = fs::remove_file(&out);
            }
            std::collections::hash_map::Entry::Vacant(slot) => {
                slot.insert(out);
            }
        }
        Ok(())
    }

    fn finish(mut self) -> Result<Unpacked, String> {
        match self.scenes.len() {
            1 => Ok(Unpacked {
                scene: self.scenes.pop().expect("one").1,
                files: self.files,
            }),
            0 => Err(format!(
                "the archive has no .{EXTENSION} scene file at its root"
            )),
            _ => Err(format!(
                "the archive has more than one .{EXTENSION} scene file at its root: {}",
                self.scenes
                    .iter()
                    .map(|(n, _)| n.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        }
    }
}

/// Writes a `.far` archive to `out`: `scene.flwr` and `files/<name>` for
/// each `(name, path)`. Blocking.
pub(crate) fn write_far(out: &Path, scene: &[u8], files: &[(String, PathBuf)]) -> io::Result<()> {
    let mut zip = zip::ZipWriter::new(File::create(out)?);
    let stored = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Stored)
        .large_file(true);
    let deflated = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    zip.start_file(format!("scene.{EXTENSION}"), deflated)?;
    zip.write_all(scene)?;
    for (name, path) in files {
        // Uploads are often already compressed (models, archives): stored as is.
        zip.start_file(format!("files/{name}"), stored)?;
        io::copy(&mut File::open(path)?, &mut zip)?;
    }
    zip.finish()?.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits() -> Limits {
        Limits {
            max_bytes: 1024 * 1024,
            max_entries: 100,
        }
    }

    const SCENE: &[u8] = b"FLWR\x05\x00\x00\x001.0.0";

    fn sha(bytes: &[u8]) -> String {
        sha256::digest(bytes)
    }

    fn zip_of(path: &Path, entries: &[(&str, &[u8])]) {
        let mut zip = zip::ZipWriter::new(File::create(path).unwrap());
        for (name, data) in entries {
            zip.start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            zip.write_all(data).unwrap();
        }
        zip.finish().unwrap();
    }

    fn tar_of(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut builder = tar::Builder::new(vec![]);
        for (name, data) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append_data(&mut header, name, *data).unwrap();
        }
        builder.into_inner().unwrap()
    }

    fn unpack_file(bytes: &[u8]) -> Result<Unpacked, String> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("in");
        fs::write(&path, bytes).unwrap();
        let work = dir.path().join("work");
        fs::create_dir(&work).unwrap();
        let kind = Kind::detect(bytes).ok_or("unknown kind")?;
        let unpacked = unpack(&path, kind, &work, limits())?;
        // read the files before the temp dir goes
        for p in unpacked.files.values() {
            assert!(p.is_file());
        }
        Ok(unpacked)
    }

    #[test]
    fn far_round_trip_finds_files_by_hash() {
        let dir = tempfile::tempdir().unwrap();
        let model = dir.path().join("model");
        fs::write(&model, b"weights").unwrap();
        let far = dir.path().join("s.far");
        write_far(&far, SCENE, &[("model_x.onnx".into(), model)]).unwrap();
        let bytes = fs::read(&far).unwrap();
        assert_eq!(Kind::detect(&bytes), Some(Kind::Zip));
        let unpacked = unpack_file(&bytes).unwrap();
        assert_eq!(unpacked.scene, SCENE);
        assert!(unpacked.files.contains_key(&sha(b"weights")));
    }

    #[test]
    fn tar_and_tar_gz_with_any_layout() {
        let tar = tar_of(&[
            ("my.flwr", SCENE),
            ("anywhere/renamed.bin", b"weights"),
            ("dup", b"weights"),
        ]);
        assert_eq!(Kind::detect(&tar), Some(Kind::Tar));
        let unpacked = unpack_file(&tar).unwrap();
        assert_eq!(unpacked.files.len(), 1);
        assert!(unpacked.files.contains_key(&sha(b"weights")));

        let mut gz = flate2::write::GzEncoder::new(vec![], flate2::Compression::default());
        gz.write_all(&tar).unwrap();
        let gz = gz.finish().unwrap();
        assert_eq!(Kind::detect(&gz), Some(Kind::TarGz));
        assert_eq!(unpack_file(&gz).unwrap().scene, SCENE);
    }

    #[test]
    fn refuses_unsafe_or_ambiguous_archives() {
        let dir = tempfile::tempdir().unwrap();
        let zip = dir.path().join("z");
        type Entries<'a> = Vec<(&'a str, &'a [u8])>;
        let cases: Vec<(Entries, &str)> = vec![
            (vec![("a.bin", b"x")], "no .flwr scene file"),
            (vec![("a.flwr", SCENE), ("b.flwr", SCENE)], "more than one"),
            (vec![("sub/a.flwr", SCENE)], "no .flwr scene file"),
            (
                vec![("a.flwr", SCENE), ("../x", b"x")],
                "inside the archive",
            ),
        ];
        for (entries, expected) in cases {
            zip_of(&zip, &entries);
            let e = unpack_file(&fs::read(&zip).unwrap()).unwrap_err();
            assert!(e.contains(expected), "{expected:?} not in {e:?}");
        }
        // too big, unpacked
        let big = vec![0u8; 2 * 1024 * 1024];
        zip_of(&zip, &[("a.flwr", SCENE), ("big", &big)]);
        let e = unpack_file(&fs::read(&zip).unwrap()).unwrap_err();
        assert!(e.contains("unpacks to more than"), "{e}");
        // links aren't allowed in tar
        let mut builder = tar::Builder::new(vec![]);
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Symlink);
        header.set_size(0);
        header.set_cksum();
        builder
            .append_link(&mut header, "evil", "/etc/passwd")
            .unwrap();
        let e = unpack_file(&builder.into_inner().unwrap()).unwrap_err();
        assert!(e.contains("only regular files"), "{e}");
        assert!(Kind::detect(b"hello").is_none());
    }
}
