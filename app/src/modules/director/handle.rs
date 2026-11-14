//! In-process interface to the director: how the scene and plugin services
//! ask it to (re)load or stop a scene's workflow.

use super::running::NodeReport;
use crate::modules::hub::HubConnections;
use serde::Serialize;
use thiserror::Error;
use tokio::sync::{mpsc, oneshot};

/// Commands are processed in the order they're sent.
const CHANNEL_CAPACITY: usize = 1024;

#[derive(Error, Debug, Clone, PartialEq)]
pub(crate) enum DirectorError {
    #[error("the director isn't running")]
    Unavailable,
    #[error("{0}")]
    Failed(String),
}

/// Run-time state of a scene, as reported by the director.
#[derive(Debug, Clone, PartialEq, Serialize, utoipa::ToSchema)]
pub(crate) struct SceneStatus {
    pub state: SceneState,
    /// Why the scene couldn't start (state `FAILED`).
    pub errors: Vec<String>,
    /// The hop limit in effect while running (the scene's or the default).
    pub max_hops: Option<u32>,
    pub nodes: Vec<NodeReport>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(crate) enum SceneState {
    Running,
    Stopped,
    /// The scene failed its load-time checks; nothing of it runs.
    Failed,
}

/// The whole server's run-time picture.
#[derive(Debug, Clone, PartialEq, Serialize, utoipa::ToSchema)]
pub(crate) struct Overview {
    /// Where plugins connect.
    pub hub_address: String,
    /// The hub's connections now, and its refusals so far.
    pub hub_connections: HubConnections,
    /// The hub-wide budget for queued events (`[hub] max_inflight_bytes`).
    pub queued_memory: Usage,
    /// Spilling queues' memory (`[director] memory_soft_inflight_bytes`, a soft limit).
    pub spill_memory: Usage,
    /// All spool files (`[director] spill_max_bytes`).
    pub spill_disk: Usage,
    /// Scenes the director has handled since it started.
    pub scenes: Vec<SceneSummary>,
}

/// Everything the director knows right now, for telemetry sinks.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct StatsSnapshot {
    pub overview: Overview,
    /// `(scene id, status)`, sorted by id.
    pub scenes: Vec<(String, SceneStatus)>,
}

#[derive(Debug, Clone, PartialEq, Serialize, utoipa::ToSchema)]
pub(crate) struct Usage {
    pub used_bytes: u64,
    pub limit_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, utoipa::ToSchema)]
pub(crate) struct SceneSummary {
    pub scene_id: String,
    pub state: SceneState,
    pub nodes: usize,
    /// Nodes whose plugin is connected and accepted.
    pub running_nodes: usize,
    /// Why it couldn't start, if it failed.
    pub errors: usize,
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
    /// Start a scene's queues (or one node's) over: if it runs, its queued and
    /// in-flight events go and spool files are replaced; else its spool files
    /// are deleted. Replies with how many events went.
    Purge {
        scene_id: String,
        node_id: Option<String>,
        reply: oneshot::Sender<u64>,
    },
    Overview {
        reply: oneshot::Sender<Overview>,
    },
    /// The overview plus every scene's full status (for telemetry sinks).
    #[cfg_attr(not(feature = "otel"), allow(dead_code))]
    Snapshot {
        reply: oneshot::Sender<StatsSnapshot>,
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
    #[allow(dead_code)] // used by tests; kept for a synchronous reload endpoint
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

    /// Drops the queued events of `scene_id`'s nodes, or of `node_id` only.
    pub(crate) async fn purge(
        &self,
        scene_id: &str,
        node_id: Option<&str>,
    ) -> Result<u64, DirectorError> {
        let (reply, wait) = oneshot::channel();
        self.tx
            .send(DirectorCommand::Purge {
                scene_id: scene_id.to_string(),
                node_id: node_id.map(str::to_string),
                reply,
            })
            .await
            .map_err(|_| DirectorError::Unavailable)?;
        wait.await.map_err(|_| DirectorError::Unavailable)
    }

    /// Every scene's state, plus hub-wide memory and disk use.
    pub(crate) async fn overview(&self) -> Result<Overview, DirectorError> {
        let (reply, wait) = oneshot::channel();
        self.tx
            .send(DirectorCommand::Overview { reply })
            .await
            .map_err(|_| DirectorError::Unavailable)?;
        wait.await.map_err(|_| DirectorError::Unavailable)
    }

    /// The overview and every scene's status, in one round.
    #[cfg_attr(not(feature = "otel"), allow(dead_code))]
    pub(crate) async fn snapshot(&self) -> Result<StatsSnapshot, DirectorError> {
        let (reply, wait) = oneshot::channel();
        self.tx
            .send(DirectorCommand::Snapshot { reply })
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
