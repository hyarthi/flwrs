//! Plugin child processes: spawn, capture output, terminate.

use super::spec::LocalLaunch;
use flwrs_plugin::plugin::core::TOKEN_ENV;
use std::io;
use std::process::{ExitStatus, Stdio};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, BufReader};
use tokio::process::{Child, Command};

/// Longer output lines are cut (and the rest of the line skipped), so a
/// plugin can't make the server buffer without bound.
const MAX_OUTPUT_LINE: usize = 8 * 1024;

// How a plugin's process tree is asked to stop and killed, per OS family.
#[cfg(unix)]
#[path = "process/unix.rs"]
mod os;
// The one place in the workspace allowed to use `unsafe`: Windows API calls
// (see the module docs and the rule in AGENTS.md).
#[cfg(windows)]
#[allow(unsafe_code)]
#[path = "process/windows.rs"]
mod os;
#[cfg(not(any(unix, windows)))]
#[path = "process/other.rs"]
mod os;

/// A fresh launch token: 256 random bits from the OS, hex-encoded.
pub(crate) fn new_token() -> String {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).expect("the OS random number generator is available");
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub(crate) struct PluginProcess {
    child: Child,
    pid: Option<u32>,
    /// The plugin's process tree (its process group / Job Object).
    tree: os::Tree,
    exited: Option<ExitStatus>,
}

impl PluginProcess {
    /// Starts the plugin in its install folder, with the token in the
    /// environment (not on the command line, where `ps` would show it), and
    /// stdout/stderr forwarded to the server log under `tag`. The plugin gets
    /// its own process group (Unix) or console process group and Job Object
    /// (Windows), so stopping it reaches its child processes too. The process
    /// tree is killed if this value is dropped while it's still running.
    pub(crate) fn spawn(
        launch: &LocalLaunch,
        node_id: &str,
        token: &str,
        tag: &str,
    ) -> io::Result<Self> {
        let mut command = Command::new(&launch.program);
        command
            .args(launch.argv(node_id))
            .current_dir(&launch.cwd)
            .env(TOKEN_ENV, token)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        os::configure(&mut command);
        let mut child = command.spawn()?;
        let tree = os::Tree::attach(&child, tag);
        if let Some(out) = child.stdout.take() {
            tokio::spawn(forward_output(
                out,
                tag.to_string(),
                log::Level::Info,
                "stdout",
            ));
        }
        if let Some(err) = child.stderr.take() {
            tokio::spawn(forward_output(
                err,
                tag.to_string(),
                log::Level::Warn,
                "stderr",
            ));
        }
        Ok(Self {
            pid: child.id(),
            child,
            tree,
            exited: None,
        })
    }

    pub(crate) fn pid(&self) -> Option<u32> {
        self.pid
    }

    /// Waits for the process to exit. Cancel-safe.
    pub(crate) async fn wait(&mut self) -> io::Result<ExitStatus> {
        if let Some(status) = self.exited {
            return Ok(status);
        }
        let status = self.child.wait().await?;
        self.exited = Some(status);
        Ok(status)
    }

    /// Waits up to `grace` for the process to exit by itself.
    pub(crate) async fn wait_for(&mut self, grace: Duration) -> Option<ExitStatus> {
        tokio::time::timeout(grace, self.wait()).await.ok()?.ok()
    }

    /// Asks the process tree to stop (SIGTERM on Unix, Ctrl-Break on
    /// Windows), gives it up to `grace`, then kills it (SIGKILL / terminating
    /// the Job Object). Returns the exit status.
    pub(crate) async fn terminate(&mut self, grace: Duration) -> Option<ExitStatus> {
        if let Some(status) = self.exited {
            return Some(status);
        }
        // Don't signal a pid that may already belong to another process.
        if let Ok(Some(status)) = self.child.try_wait() {
            self.exited = Some(status);
            return Some(status);
        }
        if self.tree.request_stop()
            && let Some(status) = self.wait_for(grace).await
        {
            return Some(status);
        }
        log::debug!(
            "Plugin process {:?} didn't stop when asked; killing it",
            self.pid
        );
        self.tree.kill();
        let _ = self.child.start_kill();
        self.wait().await.ok()
    }
}

impl Drop for PluginProcess {
    fn drop(&mut self) {
        // `kill_on_drop` covers the plugin itself; this covers its children.
        if self.exited.is_none() && !matches!(self.child.try_wait(), Ok(Some(_))) {
            self.tree.kill();
        }
    }
}

/// Writes each output line to the server log, cutting overlong lines.
async fn forward_output(
    stream: impl AsyncRead + Unpin,
    tag: String,
    level: log::Level,
    name: &str,
) {
    let mut reader = BufReader::new(stream);
    let mut line = Vec::new();
    loop {
        line.clear();
        let read = (&mut reader)
            .take(MAX_OUTPUT_LINE as u64)
            .read_until(b'\n', &mut line)
            .await;
        match read {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        let complete = line.ends_with(b"\n");
        let text = String::from_utf8_lossy(&line);
        let text = text.trim_end_matches(['\r', '\n']);
        if complete || line.len() < MAX_OUTPUT_LINE {
            log::log!(level, "{tag} {name}: {text}");
            continue;
        }
        log::log!(
            level,
            "{tag} {name}: {text} [line cut at {MAX_OUTPUT_LINE} bytes]"
        );
        // Skip the rest of the overlong line.
        loop {
            line.clear();
            match (&mut reader)
                .take(MAX_OUTPUT_LINE as u64)
                .read_until(b'\n', &mut line)
                .await
            {
                Ok(0) | Err(_) => return,
                Ok(_) if line.ends_with(b"\n") => break,
                Ok(_) => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_long_hex_and_distinct() {
        let a = new_token();
        assert_eq!(a.len(), 64);
        assert!(a.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_ne!(a, new_token());
    }
}
