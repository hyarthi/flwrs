//! The director's engine: one worker per scene, so commands for a scene run
//! in order and a slow stop of one scene doesn't hold up the others.

use super::handle::{
    DirectorCommand, DirectorError, FailedUpdate, Overview, PreflightIssue, PreflightRetryStatus,
    SceneState, SceneStatus, SceneSummary, StatsSnapshot, Usage,
};
use super::preflight::{WaitEnd, WaitProgress, gave_up_line, preflight_loop};
use super::queue::{delete_files, purge_files};
use super::running::{Context, NodeFailure, RunningScene, nodes_to_start};
use crate::modules::configurator::{self, PreflightTarget};
use crate::modules::scene::service::{SceneSnapshot, Service as SceneService, ServiceError};
use crate::modules::supervisor::{NodeState, workdir};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::{JoinHandle, JoinSet};
use tokio_util::sync::CancellationToken;

pub(crate) struct Engine {
    ctx: Context,
    scenes: &'static SceneService,
}

type Done = oneshot::Sender<Result<(), DirectorError>>;

/// A load held back by its pre-flight check. It runs on a task of its own,
/// so the scene's worker keeps answering commands.
struct Waiting {
    cancel: CancellationToken,
    task: JoinHandle<WaitEnd>,
    progress: watch::Receiver<WaitProgress>,
    snapshot: SceneSnapshot,
    dones: Vec<Done>,
}

impl Waiting {
    /// Stops the checks (their programs are killed); returns who's waiting.
    fn cancel(self) -> Vec<Done> {
        self.cancel.cancel();
        self.dones
    }
}

/// Resolves when the wait (if any) ends. Cancel-safe.
async fn wait_end(waiting: &mut Option<Waiting>) -> WaitEnd {
    match waiting {
        Some(w) => (&mut w.task).await.unwrap_or_else(|e| WaitEnd::Problems {
            issues: vec![PreflightIssue {
                node_id: String::new(),
                message: format!("the pre-flight check failed: {e}"),
                retryable: false,
            }],
            gave_up: None,
        }),
        None => std::future::pending().await,
    }
}

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
        engine.scenes.set_director_context(engine.ctx.clone());
        clean_spool_dir(&engine).await;
        clean_nodes_dir(&engine).await;
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
            Err(ServiceError::NotFound(_)) => match tokio::fs::remove_dir_all(&path).await {
                Ok(()) => removed += 1,
                Err(err) => log::warn!("Director: couldn't remove {}: {err}", path.display()),
            },
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

/// Removes the folders of scenes and nodes that no longer exist. A disabled
/// scene's nodes keep theirs.
async fn clean_nodes_dir(engine: &Engine) {
    let dir = engine.ctx.nodes_dir.clone();
    let Ok(mut entries) = tokio::fs::read_dir(&dir).await else {
        return; // not created yet
    };
    let mut orphans: Vec<(String, Option<Vec<String>>)> = vec![];
    while let Ok(Some(entry)) = entries.next_entry().await {
        if !entry.path().is_dir() {
            continue;
        }
        let scene_id = entry.file_name().to_string_lossy().into_owned();
        match engine.scenes.snapshot(&scene_id).await {
            Err(ServiceError::NotFound(_)) => orphans.push((scene_id, None)),
            Ok(snapshot) => {
                let Ok(mut nodes) = tokio::fs::read_dir(entry.path()).await else {
                    continue;
                };
                let mut gone = vec![];
                while let Ok(Some(node)) = nodes.next_entry().await {
                    let name = node.file_name().to_string_lossy().into_owned();
                    let node_id = name.strip_suffix(".links").unwrap_or(&name).to_string();
                    if !snapshot.nodes.iter().any(|n| n.id == node_id) && !gone.contains(&node_id) {
                        gone.push(node_id);
                    }
                }
                if !gone.is_empty() {
                    orphans.push((scene_id, Some(gone)));
                }
            }
            Err(err) => log::warn!("Director: can't check node folders of {scene_id}: {err}"),
        }
    }
    if orphans.is_empty() {
        return;
    }
    let shown = dir.display().to_string();
    let removed = tokio::task::spawn_blocking(move || {
        let mut removed = 0;
        for (scene_id, nodes) in orphans {
            let result = match nodes {
                None => workdir::remove_scene(&dir, &scene_id).map(|()| 1),
                Some(nodes) => nodes.iter().try_fold(0, |n, node| {
                    workdir::remove(&dir.join(&scene_id).join(node)).map(|()| n + 1)
                }),
            };
            match result {
                Ok(n) => removed += n,
                Err(err) => {
                    log::warn!("Director: couldn't remove node folders of {scene_id}: {err}")
                }
            }
        }
        removed
    })
    .await
    .unwrap_or(0);
    if removed > 0 {
        log::info!("Director: removed {removed} orphaned node folder(s) from {shown}");
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
    let mut waiting: Option<Waiting> = None;
    // The problems behind a `FAILED` from a pre-flight check.
    let mut issues: Vec<PreflightIssue> = vec![];
    // An update the running scene couldn't take: it keeps its old graph.
    let mut failed_update: Option<FailedUpdate> = None;
    // Nodes that reached `Failed`.
    let (node_failed, mut node_failures) = mpsc::unbounded_channel();
    loop {
        engine
            .scenes
            .publish_running(&scene_id, running.as_ref().map(|r| r.view()));
        if let Some(scene) = running.as_mut() {
            scene.watch_started(&node_failed);
        }
        let command = match backlog.pop_front() {
            Some(command) => command,
            None => tokio::select! {
                biased;
                command = commands.recv() => match command {
                    Some(command) => command,
                    None => break,
                },
                Some(failure) = node_failures.recv() => {
                    on_node_failed(&scene_id, failure, &mut running, &mut errors).await;
                    if running.is_none() {
                        failed_update = None;
                        issues.clear();
                    }
                    continue;
                }
                end = wait_end(&mut waiting) => {
                    let wait = waiting.take().expect("a wait just ended");
                    let result = finish_wait(
                        &engine, &scene_id, end, &wait.snapshot,
                        &mut running, &mut errors, &mut issues, &mut failed_update,
                    )
                    .await;
                    for done in wait.dones {
                        let _ = done.send(result.clone());
                    }
                    continue;
                }
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
                // A newer graph replaces what was being waited for.
                if let Some(wait) = waiting.take() {
                    dones.extend(wait.cancel());
                }
                issues.clear();
                let result = match load(&engine, &scene_id, &mut running, &mut errors).await {
                    Err(result) => result,
                    Ok(snapshot) => {
                        let targets = preflight_targets(&engine, running.as_ref(), &snapshot);
                        if targets.is_empty() {
                            let applied =
                                apply(&engine, &scene_id, &snapshot, &mut running, &mut errors)
                                    .await;
                            if applied.is_ok() {
                                failed_update = None;
                            }
                            applied
                        } else {
                            waiting = Some(start_wait(
                                &engine,
                                targets,
                                snapshot,
                                std::mem::take(&mut dones),
                            ));
                            Ok(())
                        }
                    }
                };
                if running.is_none() {
                    failed_update = None;
                }
                for done in dones {
                    let _ = done.send(result.clone());
                }
            }
            DirectorCommand::Stop { done, .. } => {
                if let Some(wait) = waiting.take() {
                    for done in wait.cancel() {
                        let _ = done.send(Ok(()));
                    }
                }
                if let Some(scene) = running.take() {
                    scene.stop().await;
                }
                errors.clear();
                issues.clear();
                failed_update = None;
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
                let progress = waiting.as_ref().map(|w| w.progress.borrow().clone());
                let status = SceneStatus {
                    state: match (&progress, &running, errors.is_empty()) {
                        (Some(_), ..) => SceneState::Waiting,
                        (None, Some(_), _) => SceneState::Running,
                        (None, None, true) => SceneState::Stopped,
                        (None, None, false) => SceneState::Failed,
                    },
                    errors: match &progress {
                        Some(p) => p.issues.iter().map(PreflightIssue::line).collect(),
                        None => errors.clone(),
                    },
                    max_hops: running.as_ref().map(|r| r.max_hops()),
                    nodes: running.as_ref().map(|r| r.reports()).unwrap_or_default(),
                    preflight: match &progress {
                        Some(p) => p.issues.clone(),
                        None => issues.clone(),
                    },
                    preflight_retry: progress.map(|p| p.retry),
                    failed_update: failed_update.clone(),
                };
                let _ = reply.send(Some(status));
            }
        }
    }
    engine.scenes.publish_running(&scene_id, None);
    if let Some(wait) = waiting.take() {
        wait.cancel();
    }
    if let Some(scene) = running {
        scene.stop().await;
    }
}

/// A node has failed for good. A critical one stops the scene (`FAILED`,
/// saying what a person needs to know; after a reload the change stays
/// committed, nothing is rolled back). Any other is taken offline and the
/// scene runs on.
async fn on_node_failed(
    scene_id: &str,
    failure: NodeFailure,
    running: &mut Option<RunningScene>,
    errors: &mut Vec<String>,
) {
    let node_id = failure.node_id.as_str();
    let Some(scene) = running.as_mut() else {
        return;
    };
    // Reported for a node since replaced by another reload: not this one.
    let Some(critical) = scene.failed_critical(node_id) else {
        return;
    };
    if !critical {
        log::warn!(
            "Director: scene [{scene_id}]: node {node_id} failed and is offline; the scene runs on"
        );
        scene.take_offline(node_id).await;
        return;
    }
    let Some(report) = scene.reports().into_iter().find(|n| n.node_id == node_id) else {
        return;
    };
    let last = report
        .status
        .last_error
        .unwrap_or_else(|| "no error reported".into());
    let stderr = crate::modules::node_logs::snapshot(node_id)
        .into_iter()
        .rev()
        .find(|r| r.source == crate::modules::node_logs::Source::Stderr)
        .map(|r| format!("; last stderr: {}", r.message))
        .unwrap_or_default();
    let message = if failure.after_swap {
        format!(
            "critical node {node_id} failed after the switch-over to the new version and never started: {last}{stderr}. \
             The previous process may not have released a device it held. The change is committed and wasn't rolled back: \
             check the device, then disable and enable the scene, or submit a fix"
        )
    } else {
        format!(
            "critical node {node_id} failed: {last}{stderr}. Fix the cause, then disable and enable the scene, or submit a fix"
        )
    };
    log::error!("Director: scene [{scene_id}]: {message}");
    if let Some(scene) = running.take() {
        scene.stop().await;
    }
    *errors = vec![message];
}

/// The pre-flight checks a load of `snapshot` needs: its local nodes that
/// would start and whose plugin declares one.
fn preflight_targets(
    engine: &Engine,
    running: Option<&RunningScene>,
    snapshot: &SceneSnapshot,
) -> Vec<PreflightTarget> {
    let files_dir = &engine.ctx.files_dir;
    // Most scenes have none: no need to plan the graph for them.
    if configurator::preflight_targets(snapshot, files_dir, None).is_empty() {
        return vec![];
    }
    match nodes_to_start(running, snapshot, &engine.ctx) {
        Some(ids) => configurator::preflight_targets(snapshot, files_dir, Some(&ids)),
        None => vec![],
    }
}

fn start_wait(
    engine: &Engine,
    targets: Vec<PreflightTarget>,
    snapshot: SceneSnapshot,
    dones: Vec<Done>,
) -> Waiting {
    let retry = engine
        .ctx
        .preflight_retry
        .for_scene(&snapshot.scene.defaults);
    let cancel = CancellationToken::new();
    let (progress_tx, progress) = watch::channel(WaitProgress {
        issues: vec![],
        retry: PreflightRetryStatus {
            attempt: 1,
            max_attempts: retry.max_attempts,
            next_attempt_time: None,
        },
    });
    let task = tokio::spawn(preflight_loop(
        targets,
        engine.scenes.configure_limits(),
        retry,
        cancel.clone(),
        progress_tx,
        engine.scenes,
        snapshot.clone(),
    ));
    Waiting {
        cancel,
        task,
        progress,
        snapshot,
        dones,
    }
}

/// Acts on a finished wait: applies the load, or reports the failure. A
/// scene that's running keeps its old graph and stays `RUNNING`, with the
/// failed update in its status; one that isn't goes to `FAILED`.
#[allow(clippy::too_many_arguments)]
async fn finish_wait(
    engine: &Engine,
    scene_id: &str,
    end: WaitEnd,
    snapshot: &SceneSnapshot,
    running: &mut Option<RunningScene>,
    errors: &mut Vec<String>,
    issues: &mut Vec<PreflightIssue>,
    failed_update: &mut Option<FailedUpdate>,
) -> Result<(), DirectorError> {
    match end {
        WaitEnd::Passed => {
            let applied = apply(engine, scene_id, snapshot, running, errors).await;
            if applied.is_ok() {
                *failed_update = None;
            }
            applied
        }
        WaitEnd::Cancelled => Ok(()),
        WaitEnd::Problems {
            issues: found,
            gave_up,
        } => {
            let mut lines: Vec<String> = found.iter().map(PreflightIssue::line).collect();
            lines.extend(gave_up.map(gave_up_line));
            let shown = lines.join("; ");
            if running.is_some() {
                log::error!(
                    "Director: scene [{scene_id}] keeps its previous graph: the update failed its pre-flight check: {shown}"
                );
                *failed_update = Some(FailedUpdate {
                    errors: lines,
                    problems: found,
                    fail_time: chrono::Utc::now().timestamp_millis(),
                });
            } else {
                log::error!("Director: scene [{scene_id}] failed its pre-flight check: {shown}");
                *errors = lines;
                *issues = found;
            }
            Err(DirectorError::Failed(shown))
        }
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
                emitted: status.nodes.iter().map(|n| n.counters.emitted).sum(),
                received: status.nodes.iter().map(|n| n.counters.received).sum(),
                dropped: status.nodes.iter().map(|n| n.counters.dropped).sum(),
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
        hub_connections: engine.ctx.hub.connections(),
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

/// Reads the scene to run. A scene that's disabled, gone or unreadable is
/// stopped here, and `Err` holds the load's result.
async fn load(
    engine: &Engine,
    scene_id: &str,
    running: &mut Option<RunningScene>,
    errors: &mut Vec<String>,
) -> Result<SceneSnapshot, Result<(), DirectorError>> {
    errors.clear();
    match engine.scenes.snapshot(scene_id).await {
        Ok(snapshot) if snapshot.scene.enabled => Ok(snapshot),
        Ok(_) => {
            log::debug!("Director: scene [{scene_id}] is disabled; not running it");
            if let Some(scene) = running.take() {
                scene.stop().await;
            }
            Err(Ok(()))
        }
        Err(ServiceError::NotFound(_)) => {
            log::debug!("Director: scene [{scene_id}] is gone; not running it");
            if let Some(scene) = running.take() {
                scene.stop().await;
            }
            // Its node folders and queued events go with it.
            let (nodes_dir, id) = (engine.ctx.nodes_dir.clone(), scene_id.to_string());
            match tokio::task::spawn_blocking(move || workdir::remove_scene(&nodes_dir, &id)).await
            {
                Ok(Ok(())) => {}
                Ok(Err(err)) => {
                    log::warn!("Director: couldn't remove the node folders of [{scene_id}]: {err}")
                }
                Err(err) => {
                    log::warn!("Director: couldn't remove the node folders of [{scene_id}]: {err}")
                }
            }
            let spool = engine.ctx.spill.scene_dir(scene_id);
            if let Err(err) = tokio::fs::remove_dir_all(&spool).await
                && err.kind() != std::io::ErrorKind::NotFound
            {
                log::warn!("Director: couldn't remove {}: {err}", spool.display());
            }
            Err(Ok(()))
        }
        Err(err) => {
            let message = format!("couldn't load the scene: {err}");
            log::error!("Director: scene [{scene_id}] {message}");
            if let Some(scene) = running.take() {
                scene.stop().await;
            }
            *errors = vec![message.clone()];
            Err(Err(DirectorError::Failed(message)))
        }
    }
}

/// Brings the scene to `snapshot`: a diff against the running graph (see
/// `running.rs`) or a fresh start.
async fn apply(
    engine: &Engine,
    scene_id: &str,
    snapshot: &SceneSnapshot,
    running: &mut Option<RunningScene>,
    errors: &mut Vec<String>,
) -> Result<(), DirectorError> {
    let result = match running.as_mut() {
        Some(scene) => match scene.reload(snapshot, &engine.ctx).await {
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
            RunningScene::start(snapshot, &engine.ctx)
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
