//! Platforms without process groups or signals here: no graceful step, and
//! only the plugin's own process is killed.

use tokio::process::{Child, Command};

pub(super) fn configure(_command: &mut Command) {}

pub(super) struct Tree;

impl Tree {
    pub(super) fn attach(_child: &Child, _tag: &str) -> Self {
        Self
    }

    pub(super) fn request_stop(&self) -> bool {
        false
    }

    pub(super) fn kill(&self) {}
}
