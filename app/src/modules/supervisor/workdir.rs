//! A local node's folder: `<[director] nodes_dir>/<scene-id>/<node-id>/`, the
//! plugin's working directory (the launch contract in `docs/plugin-spec.md`).
//!
//! - One link per top-level entry of the plugin's install folder, so the
//!   plugin finds its package files by relative path. Unix: symbolic links.
//!   Windows: junctions for folders (no special rights needed), hard links
//!   for files (a copy if that fails, e.g. across drives). Other targets:
//!   copies. Which entries are the server's links is recorded next to the
//!   folder (`<node-id>.links`), so a plugin's own file with the same name
//!   is never touched.
//! - `config/`: the files the server provides (generated from the node's
//!   `launch_args`, links to uploaded files). Read-only by permission
//!   (`0555` / `0444` on Unix; the read-only attribute on Windows): that
//!   stops accidents, not a plugin that changes the permissions back.
//! - Everything else is the plugin's own and stays until the node is deleted.
//!
//! [`prepare`] brings the folder up to date before each launch, touching only
//! what differs; it never runs while the node's plugin does.

use flwrs_plugin::package::{CONFIG_DIR, ConfigContent, ConfigFile};
use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// What a launch needs in the node's folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Workdir {
    /// The node's folder (absolute): the plugin's working directory.
    pub dir: PathBuf,
    /// The plugin's install folder (absolute), linked into `dir`.
    pub install_dir: PathBuf,
    /// What `config/` must contain.
    pub files: Vec<ConfigFile>,
    /// The node's uploaded files: name → stored file (absolute).
    pub uploads: Vec<(String, PathBuf)>,
}

impl Workdir {
    /// The stored file of the node's upload `name`.
    pub(crate) fn upload(&self, name: &str) -> Option<PathBuf> {
        self.uploads
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, path)| path.clone())
    }
}

/// `<nodes_dir>/<scene-id>/<node-id>`, absolute.
pub(crate) fn node_dir(nodes_dir: &Path, scene_id: &str, node_id: &str) -> PathBuf {
    let dir = nodes_dir.join(scene_id).join(node_id);
    std::path::absolute(&dir).unwrap_or(dir)
}

/// The file recording which top-level entries of `dir` are the server's links.
fn record_path(dir: &Path) -> PathBuf {
    let mut name = dir.file_name().unwrap_or_default().to_os_string();
    name.push(".links");
    dir.with_file_name(name)
}

/// Brings the node's folder up to date: creates it, links the package's
/// entries, and makes `config/` hold exactly `files`. `upload` finds an
/// uploaded file by name. Blocking: run it on a blocking thread.
pub(crate) fn prepare(
    workdir: &Workdir,
    upload: &dyn Fn(&str) -> Option<PathBuf>,
) -> io::Result<()> {
    let dir = &workdir.dir;
    if !dir.exists() {
        fs::create_dir_all(dir)?;
        private(dir)?;
    }
    link_package(dir, &workdir.install_dir)?;
    sync_config(&dir.join(CONFIG_DIR), &workdir.files, upload)
}

/// Removes a node's folder (and its record). Blocking.
pub(crate) fn remove(dir: &Path) -> io::Result<()> {
    let config = dir.join(CONFIG_DIR);
    if config.is_dir() {
        writable_dir(&config)?;
        for entry in fs::read_dir(&config)? {
            let _ = writable_file(&entry?.path());
        }
    }
    match fs::remove_dir_all(dir) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    match fs::remove_file(record_path(dir)) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// Removes a scene's node folders (`<nodes_dir>/<scene-id>`). Blocking.
pub(crate) fn remove_scene(nodes_dir: &Path, scene_id: &str) -> io::Result<()> {
    let scene = nodes_dir.join(scene_id);
    let entries = match fs::read_dir(&scene) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    for entry in entries {
        let path = entry?.path();
        if path.is_dir() {
            remove(&path)?;
        }
    }
    fs::remove_dir_all(&scene)
}

/// Links every top-level entry of `install` into `dir`, and removes links
/// recorded earlier that the package no longer has.
fn link_package(dir: &Path, install: &Path) -> io::Result<()> {
    let record = record_path(dir);
    let recorded: BTreeSet<String> = match fs::read_to_string(&record) {
        Ok(text) => text.lines().map(String::from).collect(),
        Err(e) if e.kind() == io::ErrorKind::NotFound => BTreeSet::new(),
        Err(e) => return Err(e),
    };
    let mut wanted = vec![];
    for entry in fs::read_dir(install)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == CONFIG_DIR {
            continue; // refused at upload; never shadow the node's own
        }
        wanted.push((name, entry.path(), entry.file_type()?.is_dir()));
    }
    // Record everything that may be ours before touching anything, so an
    // interrupted run never leaves an unrecorded link behind.
    let mut all: BTreeSet<String> = recorded.clone();
    all.extend(wanted.iter().map(|(n, _, _)| n.clone()));
    write_record(&record, &all)?;

    for name in recorded
        .iter()
        .filter(|n| !wanted.iter().any(|(w, _, _)| w == *n))
    {
        remove_entry(&dir.join(name))?;
    }
    let mut ours = BTreeSet::new();
    for (name, target, is_dir) in &wanted {
        let path = dir.join(name);
        match fs::symlink_metadata(&path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
            Ok(_) if !recorded.contains(name) => {
                log::warn!(
                    "Node folder {}: {name} is the plugin's own file; not linking the package's {name} over it",
                    dir.display()
                );
                continue;
            }
            Ok(_) if links_to(&path, target) => {
                ours.insert(name.clone());
                continue;
            }
            Ok(_) => remove_entry(&path)?,
        }
        link(target, &path, *is_dir)?;
        ours.insert(name.clone());
    }
    write_record(&record, &ours)
}

fn write_record(record: &Path, names: &BTreeSet<String>) -> io::Result<()> {
    let text: String = names.iter().map(|n| format!("{n}\n")).collect();
    let temp = record.with_extension("links.tmp");
    fs::write(&temp, text)?;
    fs::rename(&temp, record)
}

/// Makes `config` hold exactly `files`, writing only what differs.
fn sync_config(
    config: &Path,
    files: &[ConfigFile],
    upload: &dyn Fn(&str) -> Option<PathBuf>,
) -> io::Result<()> {
    if files.is_empty() && !config.exists() {
        return Ok(());
    }
    if config.exists() {
        writable_dir(config)?;
    } else {
        fs::create_dir(config)?;
    }
    for entry in fs::read_dir(config)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if !files.iter().any(|f| f.name == name) {
            let path = entry.path();
            let _ = writable_file(&path);
            remove_entry(&path)?;
        }
    }
    for file in files {
        let path = config.join(&file.name);
        match &file.content {
            ConfigContent::Data(bytes) => {
                let same = fs::symlink_metadata(&path).is_ok_and(|m| m.is_file())
                    && fs::read(&path).is_ok_and(|current| &current == bytes);
                if !same {
                    let temp = config.join(format!(".{}.tmp", file.name));
                    fs::write(&temp, bytes)?;
                    read_only_file(&temp)?;
                    replace(&temp, &path)?;
                }
                read_only_file(&path)?;
            }
            ConfigContent::Upload(name) => {
                let target = upload(name).ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::NotFound,
                        format!("the node has no uploaded file named {name:?}"),
                    )
                })?;
                if !(fs::symlink_metadata(&path).is_ok() && links_to(&path, &target)) {
                    if fs::symlink_metadata(&path).is_ok() {
                        let _ = writable_file(&path);
                        remove_entry(&path)?;
                    }
                    link(&target, &path, false)?;
                }
            }
        }
    }
    read_only_dir(config)
}

/// Renames `from` over `to` (on Windows, a read-only `to` is removed first).
fn replace(from: &Path, to: &Path) -> io::Result<()> {
    #[cfg(windows)]
    if fs::symlink_metadata(to).is_ok() {
        let _ = writable_file(to);
        remove_entry(to)?;
    }
    fs::rename(from, to)
}

/// Removes a link, file or folder.
fn remove_entry(path: &Path) -> io::Result<()> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    let file_type = meta.file_type();
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileTypeExt;
        if file_type.is_symlink_dir() {
            return fs::remove_dir(path); // a junction or directory link
        }
    }
    if file_type.is_symlink() || !file_type.is_dir() {
        fs::remove_file(path)
    } else {
        fs::remove_dir_all(path)
    }
}

/// Whether `path` is (still) the link to `target` that [`link`] makes.
/// Hard links and copies can't be told apart from other files, so they're
/// always made again.
fn links_to(path: &Path, target: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink())
        && fs::read_link(path).is_ok_and(|t| same_path(&t, target))
}

fn same_path(a: &Path, b: &Path) -> bool {
    a == b
        || match (fs::canonicalize(a), fs::canonicalize(b)) {
            (Ok(a), Ok(b)) => a == b,
            _ => false,
        }
}

#[cfg(unix)]
fn link(target: &Path, path: &Path, _is_dir: bool) -> io::Result<()> {
    std::os::unix::fs::symlink(target, path)
}

#[cfg(windows)]
fn link(target: &Path, path: &Path, is_dir: bool) -> io::Result<()> {
    if is_dir {
        junction::create(target, path)
    } else {
        fs::hard_link(target, path).or_else(|_| fs::copy(target, path).map(|_| ()))
    }
}

#[cfg(not(any(unix, windows)))]
fn link(target: &Path, path: &Path, is_dir: bool) -> io::Result<()> {
    if is_dir {
        copy_dir(target, path)
    } else {
        fs::copy(target, path).map(|_| ())
    }
}

#[cfg(not(any(unix, windows)))]
fn copy_dir(from: &Path, to: &Path) -> io::Result<()> {
    fs::create_dir(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let dest = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &dest)?;
        } else {
            fs::copy(entry.path(), dest)?;
        }
    }
    Ok(())
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
}

/// The node's folder holds secrets: only the server's user may enter it.
fn private(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    return set_mode(dir, 0o700);
    #[cfg(not(unix))]
    {
        let _ = dir;
        Ok(())
    }
}

fn read_only_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    return set_mode(dir, 0o555);
    // Windows ignores the read-only attribute on folders for this purpose.
    #[cfg(not(unix))]
    {
        let _ = dir;
        Ok(())
    }
}

fn writable_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    return set_mode(dir, 0o755);
    #[cfg(not(unix))]
    {
        let _ = dir;
        Ok(())
    }
}

fn read_only_file(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    return set_mode(path, 0o444);
    #[cfg(not(unix))]
    {
        let mut perms = fs::metadata(path)?.permissions();
        perms.set_readonly(true);
        fs::set_permissions(path, perms)
    }
}

fn writable_file(path: &Path) -> io::Result<()> {
    // A link's own permissions don't matter (and setting them would follow it).
    if fs::symlink_metadata(path)?.file_type().is_symlink() {
        return Ok(());
    }
    #[cfg(unix)]
    return set_mode(path, 0o644);
    #[cfg(not(unix))]
    {
        let mut perms = fs::metadata(path)?.permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        perms.set_readonly(false);
        fs::set_permissions(path, perms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data(name: &str, text: &str) -> ConfigFile {
        ConfigFile {
            name: name.into(),
            content: ConfigContent::Data(text.as_bytes().to_vec()),
        }
    }

    fn no_uploads(_: &str) -> Option<PathBuf> {
        None
    }

    struct Setup {
        _temp: tempfile::TempDir,
        install: PathBuf,
        nodes: PathBuf,
    }

    fn setup() -> Setup {
        let temp = tempfile::tempdir().unwrap();
        let install = temp.path().join("plugins/p/1.0.0");
        fs::create_dir_all(install.join("bin")).unwrap();
        fs::write(install.join("bin/run"), "#!/bin/sh\n").unwrap();
        fs::write(install.join("manifest.json"), "{}").unwrap();
        Setup {
            nodes: temp.path().join("nodes"),
            install,
            _temp: temp,
        }
    }

    fn workdir(s: &Setup, files: Vec<ConfigFile>) -> Workdir {
        Workdir {
            dir: node_dir(&s.nodes, "scene", "node"),
            install_dir: s.install.clone(),
            files,
            uploads: vec![],
        }
    }

    #[test]
    fn links_the_package_and_writes_config() {
        let s = setup();
        let w = workdir(
            &s,
            vec![data("script.lua", "return 1"), data("schemas.json", "{}")],
        );
        prepare(&w, &no_uploads).unwrap();
        assert_eq!(
            fs::read_to_string(w.dir.join("bin/run")).unwrap(),
            "#!/bin/sh\n"
        );
        assert_eq!(
            fs::read_to_string(w.dir.join("manifest.json")).unwrap(),
            "{}"
        );
        assert_eq!(
            fs::read_to_string(w.dir.join("config/script.lua")).unwrap(),
            "return 1"
        );
        // config/ is read-only by permission
        assert!(fs::write(w.dir.join("config/script.lua"), "x").is_err());
        #[cfg(unix)]
        assert!(fs::write(w.dir.join("config/new"), "x").is_err());
        // the plugin's own files stay; a second run changes only what differs
        fs::write(w.dir.join("state.db"), "s").unwrap();
        let w = workdir(&s, vec![data("script.lua", "return 2")]);
        prepare(&w, &no_uploads).unwrap();
        assert_eq!(
            fs::read_to_string(w.dir.join("config/script.lua")).unwrap(),
            "return 2"
        );
        assert!(!w.dir.join("config/schemas.json").exists());
        assert_eq!(fs::read_to_string(w.dir.join("state.db")).unwrap(), "s");
    }

    #[test]
    fn follows_a_new_package_version() {
        let s = setup();
        let w = workdir(&s, vec![]);
        prepare(&w, &no_uploads).unwrap();
        assert!(!w.dir.join(CONFIG_DIR).exists());
        let v2 = s.install.with_file_name("2.0.0");
        fs::create_dir_all(v2.join("bin")).unwrap();
        fs::write(v2.join("bin/run"), "v2").unwrap();
        fs::write(v2.join("README.md"), "docs").unwrap();
        let w = Workdir {
            install_dir: v2,
            ..w
        };
        prepare(&w, &no_uploads).unwrap();
        assert_eq!(fs::read_to_string(w.dir.join("bin/run")).unwrap(), "v2");
        assert_eq!(fs::read_to_string(w.dir.join("README.md")).unwrap(), "docs");
        // gone from the package: gone from the node folder
        assert!(fs::symlink_metadata(w.dir.join("manifest.json")).is_err());
    }

    #[test]
    fn never_replaces_the_plugins_own_files() {
        let s = setup();
        let dir = node_dir(&s.nodes, "scene", "node");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("manifest.json"), "mine").unwrap();
        prepare(&workdir(&s, vec![]), &no_uploads).unwrap();
        assert_eq!(
            fs::read_to_string(dir.join("manifest.json")).unwrap(),
            "mine"
        );
        assert!(dir.join("bin/run").exists());
    }

    #[test]
    fn links_uploads_and_reports_missing_ones() {
        let s = setup();
        let store = s.nodes.with_file_name("files");
        fs::create_dir_all(&store).unwrap();
        let stored = store.join("model_abc.onnx");
        fs::write(&stored, "weights").unwrap();
        let file = ConfigFile {
            name: "model.onnx".into(),
            content: ConfigContent::Upload("model.onnx".into()),
        };
        let w = workdir(&s, vec![file]);
        let found = |name: &str| (name == "model.onnx").then(|| stored.clone());
        prepare(&w, &found).unwrap();
        assert_eq!(
            fs::read_to_string(w.dir.join("config/model.onnx")).unwrap(),
            "weights"
        );
        prepare(&w, &found).unwrap();
        let err = prepare(&w, &no_uploads).unwrap_err();
        assert!(err.to_string().contains("model.onnx"), "{err}");
    }

    #[test]
    fn removes_node_and_scene_folders() {
        let s = setup();
        let w = workdir(&s, vec![data("a.json", "{}")]);
        prepare(&w, &no_uploads).unwrap();
        remove(&w.dir).unwrap();
        assert!(!w.dir.exists());
        assert!(!record_path(&w.dir).exists());
        remove(&w.dir).unwrap(); // already gone
        prepare(&w, &no_uploads).unwrap();
        remove_scene(&s.nodes, "scene").unwrap();
        assert!(!s.nodes.join("scene").exists());
        remove_scene(&s.nodes, "scene").unwrap();
    }
}
