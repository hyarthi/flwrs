//! The server page's back end: the config file seen by section (schemas,
//! effective values, sources), checked with the server's own config structs,
//! edited (`[http] config_edit`, locked keys, step-up, a safe write), and the
//! server restarted from the page, with confirm-or-revert for a changed
//! config.
//!
//! Files beside the config (`<config>` = its path):
//! - `<config>.bak`: the file before the page's last save.
//! - `<config>.last-good`: the config the server ran with before a restart
//!   that changed it, until the page confirms the new one.
//! - `<config>.pending.json`: that restart's marker (`written_time`,
//!   `timeout_s`). A server starting with a marker older than its timeout
//!   (the new config never got confirmed, e.g. it crashed) puts
//!   `last-good` back first; a fresh one is watched, and reverted (with a
//!   restart) if not confirmed in time.
//! - `<config>.reverted.json`: why the last revert happened, for the page.

pub(crate) mod api;
pub(crate) mod catalog;
pub(crate) mod document;
mod preflight;

use catalog::{RestartMode, Section, lookup, sections};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use utoipa::ToSchema;

/// What the server started with.
struct Startup {
    path: PathBuf,
    text: String,
    start_time: i64,
    /// A changed config to confirm by this time (Unix seconds).
    deadline: Option<i64>,
}

static STARTUP: OnceLock<Startup> = OnceLock::new();
static RESTART: AtomicBool = AtomicBool::new(false);

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

#[derive(Serialize, Deserialize)]
struct Marker {
    written_time: i64,
    timeout_s: u64,
}

/// Why the last change was reverted.
#[derive(Serialize, Deserialize, ToSchema, Clone, Debug)]
pub(crate) struct Reverted {
    pub time: i64,
    pub reason: String,
}

/// First thing at start, before the config is read: a pending change that
/// timed out is reverted; the text the server runs with is kept.
pub(crate) fn at_start() {
    let path = flwrs_core::config::main_config_path();
    let deadline = match check_pending(&path, now()) {
        Pending::None => None,
        Pending::Waiting(due) => Some(due),
        // The logger isn't up yet (it's configured from this file).
        Pending::Reverted => {
            eprintln!(
                "Config: reverted {} to the last good config (a change wasn't confirmed in time)",
                path.display()
            );
            None
        }
        Pending::RevertFailed(e) => {
            eprintln!(
                "Config: couldn't revert {} to the last good config: {e}",
                path.display()
            );
            None
        }
    };
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let _ = STARTUP.set(Startup {
        path,
        text,
        start_time: now(),
        deadline,
    });
}

fn startup() -> &'static Startup {
    STARTUP.get_or_init(|| Startup {
        path: flwrs_core::config::main_config_path(),
        text: String::new(),
        start_time: now(),
        deadline: None,
    })
}

enum Pending {
    None,
    /// A changed config to confirm by this time.
    Waiting(i64),
    Reverted,
    RevertFailed(std::io::Error),
}

/// A marker beside `path`: still waiting at `now`, or overdue (then the
/// last good config is put back).
fn check_pending(path: &Path, now: i64) -> Pending {
    let Some(marker) = std::fs::read(sibling(path, ".pending.json"))
        .ok()
        .and_then(|b| serde_json::from_slice::<Marker>(&b).ok())
    else {
        return Pending::None;
    };
    let due = marker.written_time + marker.timeout_s as i64;
    if now <= due {
        return Pending::Waiting(due);
    }
    match revert(
        path,
        "the changed config wasn't confirmed in time (the server didn't come up, or the page didn't confirm it)",
    ) {
        Ok(()) => Pending::Reverted,
        Err(e) => Pending::RevertFailed(e),
    }
}

/// The config file's path.
pub(crate) fn path() -> &'static Path {
    &startup().path
}

/// The file as it is now.
pub(crate) fn current_text() -> std::io::Result<String> {
    std::fs::read_to_string(path())
}

/// The text the server started with.
pub(crate) fn startup_text() -> &'static str {
    &startup().text
}

pub(crate) fn start_time() -> i64 {
    startup().start_time
}

/// The changed config's confirm deadline, while it's pending.
pub(crate) fn pending_deadline() -> Option<i64> {
    let deadline = startup().deadline?;
    sibling(path(), ".pending.json")
        .exists()
        .then_some(deadline)
}

pub(crate) fn last_revert() -> Option<Reverted> {
    std::fs::read(sibling(path(), ".reverted.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
}

/// Puts the last good config back, notes why, and drops the marker.
fn revert(path: &Path, reason: &str) -> std::io::Result<()> {
    let good = sibling(path, ".last-good");
    std::fs::copy(&good, path)?;
    let _ = std::fs::remove_file(sibling(path, ".pending.json"));
    let _ = std::fs::remove_file(&good);
    let note = Reverted {
        time: now(),
        reason: reason.to_string(),
    };
    std::fs::write(
        sibling(path, ".reverted.json"),
        serde_json::to_vec(&note).unwrap_or_default(),
    )
}

/// The page has seen the last revert's note.
pub(crate) fn dismiss_revert() {
    let _ = std::fs::remove_file(sibling(path(), ".reverted.json"));
}

/// The page confirmed the changed config: it stays.
pub(crate) fn confirm() {
    let _ = std::fs::remove_file(sibling(path(), ".pending.json"));
    let _ = std::fs::remove_file(sibling(path(), ".last-good"));
}

/// Back to the last good config now (the page's "revert"), then a restart.
pub(crate) fn revert_now() -> std::io::Result<()> {
    revert(path(), "reverted from the server page")?;
    request_restart(false);
    Ok(())
}

/// Watches a pending change: reverted, with a restart, if not confirmed in time.
pub(crate) fn watch_pending() {
    let Some(deadline) = startup().deadline else {
        return;
    };
    log::warn!(
        "Config: the changed config must be confirmed from the server page within {} s, or it's reverted",
        (deadline - now()).max(0)
    );
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(
            (deadline - now()).max(0) as u64
        ))
        .await;
        if pending_deadline().is_some() {
            log::warn!(
                "Config: the changed config wasn't confirmed in time; reverting and restarting"
            );
            match revert(path(), "the changed config wasn't confirmed in time") {
                Ok(()) => request_restart(false),
                Err(e) => log::error!("Config: couldn't revert to the last good config: {e}"),
            }
        }
    });
}

/// Stops the server for a restart. With `mark` and a changed file, the
/// config it ran with is kept as the last good one and a marker written, so
/// the new one must be confirmed.
pub(crate) fn request_restart(mark: bool) {
    if mark
        && let Ok(text) = current_text()
        && text != startup_text()
    {
        let timeout = toml::from_str::<toml::Table>(&text)
            .map(|doc| catalog::server_section(&doc).confirm_timeout_s.max(30))
            .unwrap_or(120);
        let good = std::fs::write(sibling(path(), ".last-good"), startup_text());
        let marker = serde_json::to_vec(&Marker {
            written_time: now(),
            timeout_s: timeout,
        })
        .unwrap_or_default();
        match good.and_then(|()| std::fs::write(sibling(path(), ".pending.json"), marker)) {
            Ok(()) => {
                let _ = std::fs::remove_file(sibling(path(), ".reverted.json"));
            }
            Err(e) => log::error!(
                "Config: couldn't keep the last good config ({e}); restarting without confirm-or-revert"
            ),
        }
    }
    RESTART.store(true, Ordering::SeqCst);
    flwrs_core::registry::request_stop();
}

pub(crate) fn restart_requested() -> bool {
    RESTART.load(Ordering::SeqCst)
}

/// The restart mode the server runs with.
pub(crate) fn restart_mode() -> RestartMode {
    toml::from_str::<toml::Table>(startup_text())
        .map(|doc| catalog::server_section(&doc).restart)
        .unwrap_or(RestartMode::Exec)
}

/// Exit status of a stop that asks a service manager for a restart.
pub(crate) const EXIT_RESTART: i32 = 75;
/// Set for a process started by a restart (non-Unix): wait for the old one.
const RESTART_WAIT_ENV: &str = "FLWRS_RESTART_WAIT_MS";

/// After the services stopped: start again as `[server] restart` says.
/// Doesn't return.
pub(crate) fn perform_restart() -> ! {
    match restart_mode() {
        RestartMode::Exit => {
            eprintln!("Restart: exiting with status {EXIT_RESTART} for the service manager");
            std::process::exit(EXIT_RESTART)
        }
        RestartMode::Exec => {
            let exe = std::env::current_exe()
                .unwrap_or_else(|_| PathBuf::from(std::env::args_os().next().unwrap_or_default()));
            let mut command = std::process::Command::new(&exe);
            command.args(std::env::args_os().skip(1));
            #[cfg(unix)]
            {
                use std::os::unix::process::CommandExt;
                let e = command.exec(); // only returns on failure
                eprintln!("Restart: couldn't exec {}: {e}", exe.display());
                std::process::exit(1)
            }
            #[cfg(not(unix))]
            {
                command.env(RESTART_WAIT_ENV, "1500");
                match command.spawn() {
                    Ok(_) => std::process::exit(0),
                    Err(e) => {
                        eprintln!("Restart: couldn't start {}: {e}", exe.display());
                        std::process::exit(1)
                    }
                }
            }
        }
    }
}

/// A process started by a restart waits for the old one to let go of its
/// ports (non-Unix restarts start the new process before the old exits).
pub(crate) fn wait_after_restart() {
    if let Some(ms) = std::env::var(RESTART_WAIT_ENV)
        .ok()
        .and_then(|v| v.parse().ok())
    {
        std::thread::sleep(std::time::Duration::from_millis(ms));
    }
}

// ---- checking a config text ----

/// A problem with a config text: its section (if any) and what's wrong.
#[derive(Serialize, ToSchema, Debug, Clone, PartialEq)]
pub(crate) struct Problem {
    pub section: Option<String>,
    pub message: String,
}

fn problem(section: Option<&str>, message: impl Into<String>) -> Problem {
    Problem {
        section: section.map(str::to_string),
        message: message.into(),
    }
}

/// Every problem the server would have with `text`: TOML syntax, unknown
/// sections and keys, values its structs refuse, and each section's own rules.
pub(crate) fn check(text: &str) -> Vec<Problem> {
    let doc: toml::Table = match toml::from_str(text) {
        Ok(doc) => doc,
        Err(e) => return vec![problem(None, e.to_string().trim().to_string())],
    };
    let mut problems = vec![];
    for (key, value) in &doc {
        let known = sections().iter().any(|s| s.path()[0] == key.as_str());
        if !known {
            problems.push(problem(None, format!("unknown section [{key}]")));
        } else if key == "db" {
            for name in value.as_table().into_iter().flat_map(|t| t.keys()) {
                if name != "main" {
                    problems.push(problem(
                        Some("db.main"),
                        format!("[db.{name}]: only [db.main] is used"),
                    ));
                }
            }
        }
    }
    for section in sections() {
        let value = lookup(&doc, section);
        if let Err(e) = (section.parse)(value) {
            problems.push(problem(Some(section.key), e));
            continue;
        }
        for e in (section.extra)(value) {
            problems.push(problem(Some(section.key), e));
        }
        if let Some(value) = value {
            let schema = (section.schema)();
            let children = catalog::child_keys(section);
            unknown_keys(value, &schema, &schema, &children, "", &mut |e| {
                problems.push(problem(Some(section.key), e))
            });
        }
    }
    problems
}

fn resolve<'a>(
    root: &'a serde_json::Value,
    schema: &'a serde_json::Value,
) -> &'a serde_json::Value {
    match schema.get("$ref").and_then(|r| r.as_str()) {
        Some(r) => r
            .strip_prefix("#/")
            .map(|p| format!("/{p}"))
            .and_then(|p| root.pointer(&p))
            .unwrap_or(schema),
        None => schema,
    }
}

fn unknown_keys(
    value: &toml::Value,
    schema: &serde_json::Value,
    root: &serde_json::Value,
    skip: &[&str],
    at: &str,
    report: &mut dyn FnMut(String),
) {
    let schema = resolve(root, schema);
    match value {
        toml::Value::Table(table) => {
            let Some(props) = schema.get("properties").and_then(|p| p.as_object()) else {
                return; // a map: any key
            };
            for (key, v) in table {
                if skip.contains(&key.as_str()) {
                    continue;
                }
                let here = if at.is_empty() {
                    key.clone()
                } else {
                    format!("{at}.{key}")
                };
                match props.get(key) {
                    Some(s) => unknown_keys(v, s, root, &[], &here, report),
                    None => report(format!("unknown key {here:?}")),
                }
            }
        }
        toml::Value::Array(items) => {
            if let Some(item_schema) = schema.get("items") {
                for (i, v) in items.iter().enumerate() {
                    unknown_keys(v, item_schema, root, &[], &format!("{at}[{i}]"), report);
                }
            }
        }
        _ => {}
    }
}

/// The locked keys whose values differ between `old` and `new` (as
/// `[section] key`, or `[section]` for a whole one).
pub(crate) fn locked_changes(old: &str, new: &str) -> Vec<String> {
    let (Ok(old), Ok(new)) = (
        toml::from_str::<toml::Table>(old),
        toml::from_str::<toml::Table>(new),
    ) else {
        return vec![];
    };
    let at = |doc: &toml::Table, section: &Section, pointer: &str| -> Option<toml::Value> {
        let mut value = lookup(doc, section)?.clone();
        for part in pointer.split('/').filter(|p| !p.is_empty()) {
            value = value.get(part)?.clone();
        }
        Some(value)
    };
    let mut out = vec![];
    for section in sections() {
        for pointer in section.locked {
            if at(&old, section, pointer) != at(&new, section, pointer) {
                out.push(if pointer.is_empty() {
                    format!("[{}]", section.key)
                } else {
                    format!("[{}] {}", section.key, pointer.trim_start_matches('/'))
                });
            }
        }
    }
    out
}

/// Why the page can't write the file, if it can't.
pub(crate) fn read_only_reason() -> Option<String> {
    let path = path();
    match std::fs::metadata(path) {
        Err(e) => return Some(format!("{}: {e}", path.display())),
        Ok(m) if m.permissions().readonly() => {
            return Some(format!("{} is read-only", path.display()));
        }
        Ok(_) => {}
    }
    let dir = path
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let probe = dir.join(format!(".flwrs-write-check-{}", ulid::Ulid::generate()));
    match std::fs::write(&probe, b"") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            None
        }
        Err(e) => Some(format!("its folder {} isn't writable: {e}", dir.display())),
    }
}

/// Writes `text` as the config: to `<config>.new`, synced, the current file
/// kept as `<config>.bak`, then renamed over it.
pub(crate) fn write(text: &str) -> std::io::Result<()> {
    use std::io::Write;
    let path = path();
    let new = sibling(path, ".new");
    {
        let mut file = std::fs::File::create(&new)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
    }
    if let Ok(meta) = std::fs::metadata(path) {
        let _ = std::fs::set_permissions(&new, meta.permissions());
        std::fs::copy(path, sibling(path, ".bak"))?;
    }
    std::fs::rename(&new, path)
}

pub(crate) use preflight::preflight;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checks_report_syntax_unknown_keys_and_values() {
        assert!(check("").is_empty());
        assert!(
            check(include_str!("../../../config.dev.toml")).is_empty(),
            "{:?}",
            check(include_str!("../../../config.dev.toml"))
        );
        let p = check("[http\nport = 1");
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].section, None);
        let p = check(
            "[nope]\n[http]\nprot = 1\n[hub]\nport = \"x\"\n[http.auth]\nsession_ttl = \"forever\"\n[db.other]\n[server]\nconfirm_timeout_s = 5\n[logging]\nlevel = \"LOUD\"\n",
        );
        let text: Vec<String> = p
            .iter()
            .map(|p| format!("{:?}: {}", p.section, p.message))
            .collect();
        for expected in [
            "None: unknown section [nope]",
            "Some(\"http\"): unknown key \"prot\"",
            "Some(\"db.main\"): [db.other]: only [db.main] is used",
            "Some(\"server\"): confirm_timeout_s must be at least 30",
        ] {
            assert!(
                text.iter().any(|t| t == expected),
                "{expected} in {text:#?}"
            );
        }
        assert!(
            text.iter().any(|t| t.starts_with("Some(\"hub\")")),
            "{text:#?}"
        );
        assert!(
            text.iter()
                .any(|t| t.starts_with("Some(\"http.auth\")") && t.contains("session_ttl")),
            "{text:#?}"
        );
        assert!(
            text.iter().any(|t| t.starts_with("Some(\"logging\")")),
            "{text:#?}"
        );
        // Nested and array keys too.
        let p = check(
            "[hub.tls]\nport = 1\ncert = \"c\"\nkey = \"k\"\nextra = 1\n[[logging.sinks]]\nsink_type = \"CONSOLE\"\ncolour = true\n",
        );
        let text: Vec<&str> = p.iter().map(|p| p.message.as_str()).collect();
        // (a strict struct names it itself)
        assert!(
            text.iter().any(|t| t.contains("unknown field `extra`")),
            "{text:?}"
        );
        assert!(
            text.contains(&"unknown key \"sinks[0].colour\""),
            "{text:?}"
        );
    }

    #[test]
    fn an_unconfirmed_change_is_reverted_when_overdue() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "new").unwrap();
        assert!(matches!(check_pending(&path, 100), Pending::None));
        std::fs::write(sibling(&path, ".last-good"), "good").unwrap();
        let marker = serde_json::to_vec(&Marker {
            written_time: 100,
            timeout_s: 60,
        })
        .unwrap();
        std::fs::write(sibling(&path, ".pending.json"), marker).unwrap();
        // Within the timeout: waiting, nothing touched.
        assert!(matches!(check_pending(&path, 160), Pending::Waiting(160)));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");
        // Overdue: the last good config is back, the marker gone, the reason noted.
        assert!(matches!(check_pending(&path, 161), Pending::Reverted));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "good");
        assert!(!sibling(&path, ".pending.json").exists());
        assert!(!sibling(&path, ".last-good").exists());
        let note: Reverted =
            serde_json::from_slice(&std::fs::read(sibling(&path, ".reverted.json")).unwrap())
                .unwrap();
        assert!(note.reason.contains("confirmed"));
        assert!(matches!(check_pending(&path, 500), Pending::None));
    }

    #[test]
    fn locked_keys_are_found() {
        let old = "[http]\nport = 1\n[http.auth]\nsecret_key = \"0123456789abcdef\"\n[plugins]\ndir = \"a\"\n";
        assert!(locked_changes(old, old).is_empty());
        assert!(locked_changes(old, &old.replace("port = 1", "port = 2")).is_empty());
        assert_eq!(
            locked_changes(
                old,
                &old.replace("0123456789abcdef", "fedcba9876543210")
                    .replace("dir = \"a\"", "dir = \"b\"\nallow_upload = true")
            ),
            ["[http.auth]", "[plugins] allow_upload", "[plugins] dir"]
        );
        assert_eq!(
            locked_changes(old, &format!("{old}[http.x]\n")),
            Vec::<String>::new()
        );
        assert_eq!(
            locked_changes(
                old,
                &old.replace("[http]\n", "[http]\nconfig_edit = true\n")
            ),
            ["[http] config_edit"]
        );
    }

    #[test]
    fn schemas_carry_descriptions_and_defaults() {
        let hub = sections().iter().find(|s| s.key == "hub").unwrap();
        let schema = (hub.schema)();
        assert_eq!(schema["properties"]["credit_window"]["default"], 64);
        assert!(
            schema["properties"]["host"]["description"]
                .as_str()
                .unwrap()
                .contains("listens")
        );
        assert!(
            schema["properties"]["tls"]["$ref"]
                .as_str()
                .unwrap()
                .starts_with("#/$defs/"),
            "{}",
            schema["properties"]["tls"]
        );
        assert!(schema["$defs"]["HubTlsConfig"]["properties"]["cert"].is_object());
        // Referenced types keep their declared order too (the forms' rows).
        let logging = sections().iter().find(|s| s.key == "logging").unwrap();
        let schema = (logging.schema)();
        assert_eq!(schema["$defs"]["SinkConfig"]["x-order"][0], "sink_type");
        assert_eq!(
            schema["x-order"],
            serde_json::json!(["id", "format", "level", "sinks"])
        );
        let server = sections().iter().find(|s| s.key == "server").unwrap();
        let schema = (server.schema)();
        assert_eq!(schema["properties"]["restart"]["default"], "exec");
        // Every section's defaults parse back.
        for section in sections() {
            assert!((section.parse)(None).is_ok(), "{}", section.key);
        }
    }
}
