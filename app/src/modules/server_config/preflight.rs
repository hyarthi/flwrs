//! Before a restart: can the config start? It must pass the checks, its
//! folders must be creatable and writable, and ports that changed must be
//! free (ports the server already holds are its own).

use super::catalog::{lookup, sections};
use serde_json::Value;
use std::path::{Path, PathBuf};

/// Every reason `text` couldn't start (empty: it can, as far as can be told).
pub(crate) fn preflight(text: &str, running: &str) -> Vec<String> {
    let mut problems: Vec<String> = super::check(text)
        .into_iter()
        .map(|p| match p.section {
            Some(s) => format!("[{s}] {}", p.message),
            None => p.message,
        })
        .collect();
    if !problems.is_empty() {
        return problems;
    }
    let new = effective(text);
    let old = effective(running);
    for (label, dir) in folders(&new) {
        if let Err(e) = writable(&dir) {
            problems.push(format!("{label} {}: {e}", dir.display()));
        }
    }
    for (label, host, port) in ports(&new) {
        let same = ports(&old)
            .iter()
            .any(|(l, h, p)| *l == label && *h == host && *p == port);
        if port == 0 || same {
            continue;
        }
        let host = host.trim_matches(['[', ']']).to_string();
        if let Err(e) = std::net::TcpListener::bind((host.as_str(), port)) {
            problems.push(format!("{label} {host}:{port} can't be bound: {e}"));
        }
    }
    problems
}

/// Each section's effective values (defaults where the file says nothing).
fn effective(text: &str) -> serde_json::Map<String, Value> {
    let doc: toml::Table = toml::from_str(text).unwrap_or_default();
    sections()
        .iter()
        .filter_map(|s| Some((s.key.to_string(), (s.parse)(lookup(&doc, s)).ok()?)))
        .collect()
}

fn folders(cfg: &serde_json::Map<String, Value>) -> Vec<(String, PathBuf)> {
    let get = |section: &str, key: &str| {
        cfg.get(section)
            .and_then(|s| s.get(key))
            .and_then(Value::as_str)
            .map(PathBuf::from)
    };
    let mut out = vec![];
    let db = get("db.main", "filename").unwrap_or_else(|| PathBuf::from("main.db"));
    if let Some(parent) = db.parent() {
        out.push(("[db.main] filename's folder".into(), parent.to_path_buf()));
    }
    for (section, key) in [
        ("plugins", "dir"),
        ("director", "spool_dir"),
        ("director", "nodes_dir"),
        ("director", "files_dir"),
    ] {
        if let Some(dir) = get(section, key) {
            out.push((format!("[{section}] {key}"), dir));
        }
    }
    for (i, sink) in cfg
        .get("logging")
        .and_then(|l| l.get("sinks"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
    {
        if sink.get("sink_type").and_then(Value::as_str) == Some("FILE")
            && let Some(dir) = sink.get("file_directory").and_then(Value::as_str)
        {
            out.push((
                format!("[logging] sinks[{i}].file_directory"),
                PathBuf::from(dir),
            ));
        }
    }
    for (i, file) in cfg
        .get("telemetry")
        .and_then(|t| t.get("file"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
    {
        if let Some(dir) = file.get("dir").and_then(Value::as_str) {
            out.push((format!("[telemetry] file[{i}].dir"), PathBuf::from(dir)));
        }
    }
    out
}

fn ports(cfg: &serde_json::Map<String, Value>) -> Vec<(String, String, u16)> {
    let port = |v: Option<&Value>| {
        v.and_then(Value::as_u64)
            .and_then(|p| u16::try_from(p).ok())
    };
    let host = |v: Option<&Value>| v.and_then(Value::as_str).unwrap_or_default().to_string();
    let mut out = vec![];
    if let Some(http) = cfg.get("http")
        && http.get("enabled") != Some(&Value::Bool(false))
        && let Some(p) = port(http.get("port"))
    {
        out.push(("[http]".into(), host(http.get("host")), p));
    }
    if let Some(hub) = cfg.get("hub") {
        if let Some(p) = port(hub.get("port")) {
            out.push(("[hub]".into(), host(hub.get("host")), p));
        }
        if let Some(tls) = hub.get("tls").filter(|t| t.is_object())
            && let Some(p) = port(tls.get("port"))
        {
            out.push(("[hub.tls]".into(), host(tls.get("host")), p));
        }
    }
    out
}

/// The folder exists and takes a file, or can be created (its nearest
/// existing ancestor takes one).
fn writable(dir: &Path) -> Result<(), String> {
    let dir = if dir.as_os_str().is_empty() {
        Path::new(".")
    } else {
        dir
    };
    let mut existing = dir;
    while !existing.exists() {
        existing = match existing.parent() {
            Some(p) if !p.as_os_str().is_empty() => p,
            _ => Path::new("."),
        };
    }
    if !existing.is_dir() {
        return Err(format!("{} isn't a folder", existing.display()));
    }
    let probe = existing.join(format!(".flwrs-preflight-{}", ulid::Ulid::generate()));
    std::fs::write(&probe, b"").map_err(|e| format!("not writable ({e})"))?;
    let _ = std::fs::remove_file(probe);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_bad_folders_and_taken_ports() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a-file");
        std::fs::write(&file, "").unwrap();
        let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = taken.local_addr().unwrap().port();
        let text = format!(
            "[plugins]\ndir = {:?}\n[director]\nnodes_dir = {:?}\n[http]\nhost = \"127.0.0.1\"\nport = {port}\n",
            file.join("sub").display().to_string(),
            dir.path().join("new/nested").display().to_string(),
        );
        let problems = preflight(&text, "");
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert!(problems[0].starts_with("[plugins] dir"), "{problems:?}");
        assert!(problems[1].starts_with("[http] 127.0.0.1:"), "{problems:?}");
        // The port the server already listens on is its own.
        assert_eq!(preflight(&text, &text).len(), 1);
        // A config that fails its checks says only that.
        assert_eq!(
            preflight("[http]\nport = \"x\"", ""),
            preflight("[http]\nport = \"x\"", "x")
        );
    }
}
