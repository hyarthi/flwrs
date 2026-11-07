//! The director's engine: one worker per scene, so commands for a scene run
//! in order and a slow stop of one scene doesn't hold up the others.

use super::handle::{
    DirectorCommand, DirectorError, Overview, SceneState, SceneStatus, SceneSummary, StatsSnapshot,
    Usage,
};
use super::queue::{delete_files, purge_files};
use super::running::{Context, RunningScene};
use crate::modules::scene::service::{Service as SceneService, ServiceError};
use crate::modules::supervisor::NodeState;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

pub(crate) struct Engine {
    ctx: Context,
    scenes: &'static SceneService,
}

type Done = oneshot::Sender<Result<(), DirectorError>>;

impl Engine {
    pub(crate) fn new(ctx: Context, scenes: &'static SceneService) -> Self {
        Self { ctx, scenes }
    }

    /// Starts every enabled scene, then serves `commands` until `shutdown`
    /// (or until the command channel closes). Stops every scene before
    /// returning.
    pub(crate) async fn run(
        self,
        mut commands: mpsc::Receiver<DirectorCommand>,
        shutdown: CancellationToken,
    ) {
        let engine = Arc::new(self);
        clean_spool_dir(&engine).await;
        let mut workers: HashMap<String, mpsc::UnboundedSender<DirectorCommand>> = HashMap::new();
        let mut tasks = JoinSet::new();

        match engine.scenes.enabled_scene_ids().await {
            Ok(ids) => {
                log::info!("Director: starting {} enabled scenes", ids.len());
                for scene_id in ids {
                    let command = DirectorCommand::Reload {
                        scene_id,
                        done: None,
                    };
                    dispatch(&engine, &mut workers, &mut tasks, command);
                }
            }
            Err(err) => log::error!("Director: couldn't list the enabled scenes: {err}"),
        }

        loop {
            tokio::select! {
                _ = shutdown.cancelled() => break,
                command = commands.recv() => match command {
                    Some(command) => dispatch(&engine, &mut workers, &mut tasks, command),
                    None => break,
                },
            }
        }

        log::info!("Director: stopping all scenes");
        drop(workers); // each worker stops its scene when its channel closes
        tasks.join_all().await;
        log::debug!("Director: stopped");
    }
}

/// Removes orphaned spool files: folders of scenes that no longer exist,
/// and files of nodes their scene no longer has. Everything else stays:
/// durable queues replay it (disabled scenes keep theirs until they run).
async fn clean_spool_dir(engine: &Engine) {
    let dir = &engine.ctx.spill.spool_dir;
    let Ok(mut entries) = tokio::fs::read_dir(dir).await else {
        return; // not created yet
    };
    let mut removed = 0;
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        let scene_id = entry.file_name().to_string_lossy().into_owned();
        if !path.is_dir() {
            continue;
        }
        match engine.scenes.snapshot(&scene_id).await {
            Err(ServiceError::NotFound(_)) => {
                match tokio::fs::remove_dir_all(&path).await {
                    Ok(()) => removed += 1,
                    Err(err) => log::warn!("Director: couldn't remove {}: {err}", path.display()),
                }
            }
            Ok(snapshot) => {
                let Ok(mut files) = tokio::fs::read_dir(&path).await else {
                    continue;
                };
                while let Ok(Some(file)) = files.next_entry().await {
                    let name = file.file_name().to_string_lossy().into_owned();
                    let node = name.split('.').next().unwrap_or_default();
                    if name.ends_with(".db") && !snapshot.nodes.iter().any(|n| n.id == node) {
                        delete_files(&file.path()).await;
                        removed += 1;
                    }
                }
            }
            Err(err) => log::warn!("Director: can't check spool folder {scene_id}: {err}"),
        }
    }
    if removed > 0 {
        log::info!(
            "Director: removed {removed} orphaned spool entries from {}",
            dir.display()
        );
    }
}

fn dispatch(
    engine: &Arc<Engine>,
    workers: &mut HashMap<String, mpsc::UnboundedSender<DirectorCommand>>,
    tasks: &mut JoinSet<()>,
    command: DirectorCommand,
) {
    let scene_id = match &command {
        DirectorCommand::Reload { scene_id, .. }
        | DirectorCommand::Stop { scene_id, .. }
        | DirectorCommand::Status { scene_id, .. }
        | DirectorCommand::Purge { scene_id, .. } => scene_id.clone(),
        DirectorCommand::Overview { .. } | DirectorCommand::Snapshot { .. } => {
            // Asks every scene worker on a task of its own, so the command
            // loop keeps going meanwhile.
            let workers: Vec<_> = workers
                .iter()
                .map(|(id, tx)| (id.clone(), tx.clone()))
                .collect();
            let engine = engine.clone();
            tokio::spawn(async move {
                let snapshot = gather(&engine, workers).await;
                match command {
                    DirectorCommand::Overview { reply } => {
                        let _ = reply.send(snapshot.overview);
                    }
                    DirectorCommand::Snapshot { reply } => {
                        let _ = reply.send(snapshot);
                    }
                    _ => unreachable!(),
                }
            });
            return;
        }
    };
    if !workers.contains_key(&scene_id) {
        if let DirectorCommand::Status { reply, .. } = command {
            let _ = reply.send(None); // never loaded
            return;
        }
        let (tx, rx) = mpsc::unbounded_channel();
        tasks.spawn(scene_worker(engine.clone(), scene_id.clone(), rx));
        workers.insert(scene_id.clone(), tx);
    }
    if let Some(worker) = workers.get(&scene_id)
        && worker.send(command).is_err()
    {
        log::error!("Director: the worker for scene [{scene_id}] is gone");
    }
}

async fn scene_worker(
    engine: Arc<Engine>,
    scene_id: String,
    mut commands: mpsc::UnboundedReceiver<DirectorCommand>,
) {
    let mut running: Option<RunningScene> = None;
    let mut errors: Vec<String> = vec![];
    let mut backlog: VecDeque<DirectorCommand> = VecDeque::new();
    loop {
        let command = match backlog.pop_front() {
            Some(command) => command,
            None => match commands.recv().await {
                Some(command) => command,
                None => break,
            },
        };
        match command {
            DirectorCommand::Reload { done, .. } => {
                // Several queued reloads (e.g. a burst of edits) apply once,
                // with the latest graph.
                let mut dones: Vec<Done> = done.into_iter().collect();
                while let Ok(next) = commands.try_recv() {
                    match next {
                        DirectorCommand::Reload { done, .. } => dones.extend(done),
                        other => {
                            backlog.push_back(other);
                            break;
                        }
                    }
                }
                let result = reload(&engine, &scene_id, &mut running, &mut errors).await;
                for done in dones {
                    let _ = done.send(result.clone());
                }
            }
            DirectorCommand::Stop { done, .. } => {
                if let Some(scene) = running.take() {
                    scene.stop().await;
                }
                errors.clear();
                if let Some(done) = done {
                    let _ = done.send(Ok(()));
                }
            }
            DirectorCommand::Overview { .. } | DirectorCommand::Snapshot { .. } => {} // answered by the engine
            DirectorCommand::Purge { node_id, reply, .. } => {
                let purged = match running.as_ref() {
                    Some(scene) => scene.purge(node_id.as_deref()).await,
                    None => {
                        let dir = engine.ctx.spill.scene_dir(&scene_id);
                        purge_files(&dir, node_id.as_deref()).await
                    }
                };
                if purged > 0 {
                    log::info!(
                        "Director: purged {purged} queued events of scene [{scene_id}]{}",
                        node_id.map_or(String::new(), |n| format!(" node [{n}]"))
                    );
                }
                let _ = reply.send(purged);
            }
            DirectorCommand::Status { reply, .. } => {
                let status = SceneStatus {
                    state: match (&running, errors.is_empty()) {
                        (Some(_), _) => SceneState::Running,
                        (None, true) => SceneState::Stopped,
                        (None, false) => SceneState::Failed,
                    },
                    errors: errors.clone(),
                    max_hops: running.as_ref().map(|r| r.max_hops()),
                    nodes: running.as_ref().map(|r| r.reports()).unwrap_or_default(),
                };
                let _ = reply.send(Some(status));
            }
        }
    }
    if let Some(scene) = running {
        scene.stop().await;
    }
}

/// The overview, plus every scene's full status (for telemetry).
async fn gather(
    engine: &Engine,
    workers: Vec<(String, mpsc::UnboundedSender<DirectorCommand>)>,
) -> StatsSnapshot {
    let mut scenes = vec![];
    let mut statuses = vec![];
    for (scene_id, worker) in workers {
        let (reply, wait) = oneshot::channel();
        let command = DirectorCommand::Status {
            scene_id: scene_id.clone(),
            reply,
        };
        if worker.send(command).is_err() {
            continue;
        }
        if let Ok(Some(status)) = wait.await {
            let summary = SceneSummary {
                scene_id: scene_id.clone(),
                state: status.state,
                nodes: status.nodes.len(),
                running_nodes: status
                    .nodes
                    .iter()
                    .filter(|n| n.status.state == NodeState::Running)
                    .count(),
                errors: status.errors.len(),
            };
            scenes.push(summary);
            statuses.push((scene_id, status));
        }
    }
    scenes.sort_by(|a, b| a.scene_id.cmp(&b.scene_id));
    statuses.sort_by(|a, b| a.0.cmp(&b.0));
    let budget = &engine.ctx.budget;
    let (spill_memory_used, spill_memory_limit) = engine.ctx.spill.memory_usage();
    let (spill_disk_used, spill_disk_limit) = engine.ctx.spill.disk_usage();
    let overview = Overview {
        hub_address: engine.ctx.hub.local_addr().to_string(),
        queued_memory: Usage {
            used_bytes: (budget.total() - budget.available()) as u64,
            limit_bytes: budget.total() as u64,
        },
        spill_memory: Usage {
            used_bytes: spill_memory_used,
            limit_bytes: spill_memory_limit,
        },
        spill_disk: Usage {
            used_bytes: spill_disk_used,
            limit_bytes: spill_disk_limit,
        },
        scenes,
    };
    StatsSnapshot {
        overview,
        scenes: statuses,
    }
}

/// Loads the scene and brings it to its stored state: a diff against the
/// running graph (see `running.rs`), a fresh start, or a stop if it's disabled or
/// gone.
async fn reload(
    engine: &Engine,
    scene_id: &str,
    running: &mut Option<RunningScene>,
    errors: &mut Vec<String>,
) -> Result<(), DirectorError> {
    errors.clear();
    let snapshot = match engine.scenes.snapshot(scene_id).await {
        Ok(snapshot) if snapshot.scene.enabled => snapshot,
        Ok(_) => {
            log::debug!("Director: scene [{scene_id}] is disabled; not running it");
            if let Some(scene) = running.take() {
                scene.stop().await;
            }
            return Ok(());
        }
        Err(ServiceError::NotFound(_)) => {
            log::debug!("Director: scene [{scene_id}] is gone; not running it");
            if let Some(scene) = running.take() {
                scene.stop().await;
            }
            // Its queued events go with it.
            let spool = engine.ctx.spill.scene_dir(scene_id);
            if let Err(err) = tokio::fs::remove_dir_all(&spool).await
                && err.kind() != std::io::ErrorKind::NotFound
            {
                log::warn!("Director: couldn't remove {}: {err}", spool.display());
            }
            return Ok(());
        }
        Err(err) => {
            let message = format!("couldn't load the scene: {err}");
            log::error!("Director: scene [{scene_id}] {message}");
            if let Some(scene) = running.take() {
                scene.stop().await;
            }
            *errors = vec![message.clone()];
            return Err(DirectorError::Failed(message));
        }
    };
    let result = match running.as_mut() {
        Some(scene) => match scene.reload(&snapshot, &engine.ctx).await {
            Ok(_) => Ok(()),
            // The stored graph can't run: stop rather than keep an old one.
            Err(problems) => {
                if let Some(scene) = running.take() {
                    scene.stop().await;
                }
                Err(problems)
            }
        },
        None => {
            // A fresh start: durable queues adopt their files; other
            // leftovers go (RunningScene::start).
            RunningScene::start(&snapshot, &engine.ctx)
                .await
                .map(|scene| *running = Some(scene))
        }
    };
    result.map_err(|problems| {
        log::error!(
            "Director: scene [{scene_id}] can't run: {}",
            problems.join("; ")
        );
        *errors = problems;
        DirectorError::Failed(errors.join("; "))
    })?;
    Ok(())
}
