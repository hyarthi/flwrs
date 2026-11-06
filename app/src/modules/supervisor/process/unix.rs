//! Unix (Linux, macOS, the BSDs, illumos): each plugin leads its own process
//! group, so SIGTERM and SIGKILL reach the plugin and every process it
//! started. It also keeps a Ctrl+C in the server's terminal (sent to the
//! foreground group) from hitting plugins directly: the server stops them
//! in order instead.

use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use tokio::process::{Child, Command};

pub(super) fn configure(command: &mut Command) {
    command.process_group(0); // a new group, with the plugin's pid as its id
}

pub(super) struct Tree {
    group: Option<Pid>,
}

impl Tree {
    pub(super) fn attach(child: &Child, _tag: &str) -> Self {
        Self {
            group: child
                .id()
                .and_then(|pid| i32::try_from(pid).ok())
                .map(Pid::from_raw),
        }
    }

    /// SIGTERM to the whole group. Returns whether it was sent.
    pub(super) fn request_stop(&self) -> bool {
        self.group
            .is_some_and(|group| killpg(group, Signal::SIGTERM).is_ok())
    }

    /// SIGKILL to the whole group.
    pub(super) fn kill(&self) {
        if let Some(group) = self.group {
            let _ = killpg(group, Signal::SIGKILL);
        }
    }
}
