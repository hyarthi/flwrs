//! In-process interface to the director: how the scene and plugin services
//! ask it to (re)load or stop a scene's workflow.

use thiserror::Error;
use tokio::sync::{mpsc, oneshot};

/// Commands are processed in the order they're sent.
const CHANNEL_CAPACITY: usize = 1024;

#[derive(Error, Debug, Clone, PartialEq)]
pub(crate) enum DirectorError {
    #[error("the director isn't running")]
    Unavailable,
    #[error("{0}")]
    #[allow(dead_code)] // reported by the real director (later phase)
    Failed(String),
}

/// Run-time state of a scene, as reported by the director.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SceneStatus {
    pub running: bool,
}

#[derive(Debug)]
pub(crate) enum DirectorCommand {
    /// Load (or reload) the scene from the DB and run it.
    Reload {
        scene_id: String,
        done: Option<oneshot::Sender<Result<(), DirectorError>>>,
    },
    /// Stop the scene's workflow.
    Stop {
        scene_id: String,
        done: Option<oneshot::Sender<Result<(), DirectorError>>>,
    },
    Status {
        scene_id: String,
        reply: oneshot::Sender<Option<SceneStatus>>,
    },
}

#[derive(Clone, Debug)]
pub(crate) struct DirectorHandle {
    tx: mpsc::Sender<DirectorCommand>,
}

impl DirectorHandle {
    /// A handle and the receiving end that the director consumes.
    pub(crate) fn channel() -> (Self, mpsc::Receiver<DirectorCommand>) {
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        (Self { tx }, rx)
    }

    /// Queues a reload without waiting for it. Used after a committed change.
    pub(crate) fn request_reload(&self, scene_id: &str) {
        self.queue(DirectorCommand::Reload {
            scene_id: scene_id.to_string(),
            done: None,
        });
    }

    /// Queues a stop without waiting for it.
    pub(crate) fn request_stop(&self, scene_id: &str) {
        self.queue(DirectorCommand::Stop {
            scene_id: scene_id.to_string(),
            done: None,
        });
    }

    /// Reloads the scene and waits until the director is done.
    #[allow(dead_code)] // used by later phases (e.g. a synchronous reload endpoint)
    pub(crate) async fn reload(&self, scene_id: &str) -> Result<(), DirectorError> {
        let (done, wait) = oneshot::channel();
        self.tx
            .send(DirectorCommand::Reload {
                scene_id: scene_id.to_string(),
                done: Some(done),
            })
            .await
            .map_err(|_| DirectorError::Unavailable)?;
        wait.await.map_err(|_| DirectorError::Unavailable)?
    }

    #[allow(dead_code)] // used by the status endpoint (later phase)
    pub(crate) async fn status(
        &self,
        scene_id: &str,
    ) -> Result<Option<SceneStatus>, DirectorError> {
        let (reply, wait) = oneshot::channel();
        self.tx
            .send(DirectorCommand::Status {
                scene_id: scene_id.to_string(),
                reply,
            })
            .await
            .map_err(|_| DirectorError::Unavailable)?;
        wait.await.map_err(|_| DirectorError::Unavailable)
    }

    fn queue(&self, command: DirectorCommand) {
        // Never blocks the request path. The channel is large; if it's ever
        // full, the change is committed and the next reload picks it up.
        if let Err(e) = self.tx.try_send(command) {
            log::error!("Director: couldn't queue command: {e}");
        }
    }
}
