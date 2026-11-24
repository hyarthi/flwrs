//! Submitting a draft to an enabled scene: its added and changed local nodes
//! pass their pre-flight checks **before** anything is committed, so live and
//! the running scene stay as they are until the new graph is ready.
//!
//! The checks run on a task of their own, without the service's lock. A
//! permanent problem marks the draft `FAILED`; retryable ones mark it
//! `WAITING` and the task retries, committing on a pass if the draft is
//! unchanged and live hasn't moved on. Every draft edit clears the mark
//! (`query::touch`), and the task, which watches the service's changes, then
//! stops and kills a running check.

use super::*;
use crate::modules::configurator::{self, PreflightTarget};
use crate::modules::director::handle::{PreflightIssue, PreflightRetryStatus};
use crate::modules::director::preflight::{WaitEnd, WaitProgress, gave_up_line, preflight_loop};
use crate::modules::node_config::PreflightRetry;
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use tokio::sync::{oneshot, watch};
use tokio_util::sync::CancellationToken;

/// A held-back submit's state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(crate) enum DraftSubmitState {
    /// Retryable problems: being checked again; it commits once they pass.
    Waiting,
    /// Not committed: see `problems` and `message`. Submit again to retry.
    Failed,
}

/// Why the draft's last submit hasn't reached live.
#[derive(Clone, Debug, PartialEq, Serialize, utoipa::ToSchema)]
pub(crate) struct DraftSubmit {
    pub state: DraftSubmitState,
    /// What happened, beyond the nodes' problems.
    pub message: Option<String>,
    pub problems: Vec<PreflightIssue>,
    /// When it got this state (Unix milliseconds).
    pub submit_time: Option<i64>,
    /// Where the retries stand (`WAITING`).
    pub retry: Option<PreflightRetryStatus>,
}

/// What a submit did.
#[derive(Debug)]
pub(crate) enum Submitted {
    /// Live is the draft's graph now.
    Committed(Scene),
    /// Held back by retryable problems: the scene as it still is.
    Waiting(Scene),
}

/// A wait in progress, per scene.
pub(in crate::modules::scene::service) struct SubmitWait {
    id: String,
    cancel: CancellationToken,
    progress: watch::Receiver<WaitProgress>,
}

/// How the first check of a submit came out.
enum First {
    Committed(Scene),
    Waiting,
    Refused(ServiceError),
}

/// Whether a wait still stands.
enum Standing {
    Yes,
    /// Its mark was cleared (an edit, a discard, a disable, a newer submit).
    Superseded,
    LiveMoved,
}

const LIVE_MOVED: &str = "live changed while the submit was waiting, so it wasn't committed: rebase the draft and submit again";
const RESTARTED: &str =
    "the server restarted while this submit was waiting; live wasn't changed: submit it again";

fn lines(issues: &[PreflightIssue]) -> Vec<String> {
    issues.iter().map(PreflightIssue::line).collect()
}

impl Service {
    /// Commits the draft, after its pre-flight checks when the scene stays
    /// enabled (see the module docs). Refused while the draft is behind live
    /// or has conflicts, and for an enabled scene while it has problems,
    /// unless `disable`.
    pub(crate) async fn submit(
        &'static self,
        scene_id: &str,
        base_revision: Option<i64>,
        disable: bool,
    ) -> Result<Submitted, ServiceError> {
        let draft = self.get_draft(scene_id).await?;
        if let Some(b) = base_revision
            && b != draft.row.base_revision
        {
            return Err(ServiceError::Conflict(
                format!(
                    "the draft is based on revision {}, not {b}: reload it",
                    draft.row.base_revision
                ),
                vec![],
            ));
        }
        let conflicts = draft.conflicts();
        if !conflicts.is_empty() {
            return Err(ServiceError::Conflict(
                "the draft has unresolved conflicts".into(),
                conflicts,
            ));
        }
        if draft.behind() {
            return Err(ServiceError::Conflict(
                format!(
                    "live has changed since the draft started (revision {} → {}): rebase the draft",
                    draft.row.base_revision, draft.live_revision
                ),
                vec![],
            ));
        }
        let check = self.check_draft(scene_id).await?;
        let scene = self.get_scene(scene_id).await?;
        // A new submit replaces a wait, and an earlier verdict.
        self.clear_submit(scene_id).await?;
        if scene.enabled && !disable && check.problems.is_empty() {
            let (snapshot, targets) = self.submit_targets(&draft).await?;
            if !targets.is_empty() {
                let retry = self.preflight_retry.for_scene(&scene.defaults);
                return self
                    .submit_after_checks(scene_id, draft.row.update_time, targets, snapshot, retry)
                    .await;
            }
        }
        self.commit_draft(scene_id, draft.row.update_time, None, &check, disable)
            .await
            .map(Submitted::Committed)
    }

    /// The checks a submit needs: the draft's added and changed local nodes
    /// (a rename or a new group membership alone starts nothing; if the
    /// membership changes a node's threads, the reload checks it).
    async fn submit_targets(
        &self,
        draft: &Draft,
    ) -> Result<(SceneSnapshot, Vec<PreflightTarget>), ServiceError> {
        let mut ids: HashSet<String> = draft
            .nodes
            .iter()
            .filter(|n| match n.change {
                Change::Added => true,
                Change::Changed => n.parts.iter().any(|p| may_restart(p)),
                _ => false,
            })
            .map(|n| n.node.id.clone())
            .collect();
        let snapshot = self.snapshot_in(Store::Draft, &draft.row.scene_id).await?;
        // A change that leaves the process running (being critical, say):
        // it would only see its own running version holding its devices.
        if !ids.is_empty() {
            let kept = self.nodes_kept_running(&snapshot);
            ids.retain(|id| !kept.contains(id));
        }
        let targets = if ids.is_empty() {
            vec![]
        } else {
            configurator::preflight_targets(&snapshot, &self.files.files_dir, Some(&ids))
        };
        Ok((snapshot, targets))
    }

    async fn submit_after_checks(
        &'static self,
        scene_id: &str,
        update_time: DateTime<Local>,
        targets: Vec<PreflightTarget>,
        snapshot: SceneSnapshot,
        retry: PreflightRetry,
    ) -> Result<Submitted, ServiceError> {
        let id = Ulid::generate().to_string();
        let cancel = CancellationToken::new();
        let (progress_tx, progress) = watch::channel(WaitProgress {
            issues: vec![],
            retry: PreflightRetryStatus {
                attempt: 1,
                max_attempts: retry.max_attempts,
                next_attempt_time: None,
            },
        });
        {
            let _guard = self.lock.lock().await;
            let mut conn = self.conn().await?;
            let scene = self.scene_row(&mut conn, scene_id).await?;
            let row = query::get(&mut conn, scene_id)
                .await?
                .ok_or_else(no_draft)?;
            if row.base_revision != scene.revision || row.update_time != update_time {
                return Err(ServiceError::Conflict(
                    "the draft or live changed while submitting; try again".into(),
                    vec![],
                ));
            }
            query::mark_waiting(&mut conn, scene_id, &id).await?;
            let replaced = self.submit_waits.lock().expect("not poisoned").insert(
                scene_id.to_string(),
                SubmitWait {
                    id: id.clone(),
                    cancel: cancel.clone(),
                    progress: progress.clone(),
                },
            );
            if let Some(old) = replaced {
                old.cancel.cancel();
            }
        }
        let (first_tx, first) = oneshot::channel();
        // On a task of its own: it outlives this request.
        tokio::spawn(self.run_wait(
            scene_id.to_string(),
            id,
            update_time,
            targets,
            snapshot,
            retry,
            cancel,
            (progress_tx, progress),
            first_tx,
        ));
        match first.await {
            Ok(First::Committed(scene)) => Ok(Submitted::Committed(scene)),
            Ok(First::Waiting) => Ok(Submitted::Waiting(self.get_scene(scene_id).await?)),
            Ok(First::Refused(e)) => Err(e),
            Err(_) => Err(ServiceError::Unknown("the pre-flight task ended".into())),
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_wait(
        &'static self,
        scene_id: String,
        id: String,
        update_time: DateTime<Local>,
        targets: Vec<PreflightTarget>,
        snapshot: SceneSnapshot,
        retry: PreflightRetry,
        cancel: CancellationToken,
        (progress_tx, mut progress): (watch::Sender<WaitProgress>, watch::Receiver<WaitProgress>),
        first: oneshot::Sender<First>,
    ) {
        let mut first = Some(first);
        let mut changes = self.changes();
        let limits = self.configure_limits();
        let work = preflight_loop(
            targets,
            limits,
            retry,
            cancel.clone(),
            progress_tx,
            self,
            snapshot,
        );
        tokio::pin!(work);
        let mut live_moved = false;
        let mut watching_progress = true;
        let end = loop {
            tokio::select! {
                end = &mut work => break end,
                changed = changes.changed() => {
                    if changed.is_err() {
                        cancel.cancel();
                        continue;
                    }
                    match self.wait_standing(&scene_id, &id).await {
                        Standing::Yes => {}
                        Standing::Superseded => cancel.cancel(),
                        Standing::LiveMoved => {
                            live_moved = true;
                            cancel.cancel();
                        }
                    }
                }
                changed = progress.changed(), if watching_progress => {
                    if changed.is_err() {
                        watching_progress = false;
                        continue;
                    }
                    let issues = progress.borrow_and_update().issues.clone();
                    if !issues.is_empty() {
                        if let Err(e) = self.store_wait_problems(&scene_id, &id, &issues).await {
                            log::warn!("Scene service: draft of [{scene_id}]: {e}");
                        }
                        if let Some(tx) = first.take() {
                            let _ = tx.send(First::Waiting);
                        }
                    }
                }
            }
        };
        let outcome = self
            .finish_wait(&scene_id, &id, update_time, end, live_moved)
            .await;
        {
            let mut waits = self.submit_waits.lock().expect("not poisoned");
            if waits.get(&scene_id).is_some_and(|w| w.id == id) {
                waits.remove(&scene_id);
            }
        }
        if let Some(tx) = first.take() {
            let _ = tx.send(outcome);
        }
    }

    /// Acts on a finished wait: commits, or marks the draft `FAILED`.
    async fn finish_wait(
        &self,
        scene_id: &str,
        id: &str,
        update_time: DateTime<Local>,
        end: WaitEnd,
        live_moved: bool,
    ) -> First {
        let refused = |message: String, details: Vec<String>| {
            First::Refused(ServiceError::Conflict(message, details))
        };
        match end {
            WaitEnd::Cancelled if live_moved => {
                self.fail_wait(scene_id, id, &[], Some(LIVE_MOVED)).await;
                refused(LIVE_MOVED.into(), vec![])
            }
            WaitEnd::Cancelled => refused(
                "the draft changed while its pre-flight check ran; submit it again".into(),
                vec![],
            ),
            WaitEnd::Problems { issues, gave_up } => {
                let message = gave_up.map(gave_up_line);
                self.fail_wait(scene_id, id, &issues, message.as_deref())
                    .await;
                let mut details = lines(&issues);
                details.extend(message);
                refused("the draft failed its pre-flight check".into(), details)
            }
            WaitEnd::Passed => {
                let committed = match self.check_draft(scene_id).await {
                    Ok(check) => {
                        self.commit_draft(scene_id, update_time, Some(id), &check, false)
                            .await
                    }
                    Err(e) => Err(e),
                };
                match committed {
                    Ok(scene) => First::Committed(scene),
                    Err(e) => {
                        let message = match &e {
                            ServiceError::Conflict(m, details) if !details.is_empty() => {
                                format!("not committed: {m}: {}", details.join("; "))
                            }
                            other => format!("not committed: {other}"),
                        };
                        self.fail_wait(scene_id, id, &[], Some(&message)).await;
                        First::Refused(e)
                    }
                }
            }
        }
    }

    async fn wait_standing(&self, scene_id: &str, id: &str) -> Standing {
        let Ok(mut conn) = self.conn().await else {
            return Standing::Yes; // looked at again on the next change
        };
        let (Ok(scene), Ok(Some(row))) = (
            query_sqlite::get_scene(&mut conn, scene_id).await,
            query::get(&mut conn, scene_id).await,
        ) else {
            return Standing::Superseded;
        };
        if row.submit_id.as_deref() != Some(id) || !scene.enabled {
            Standing::Superseded
        } else if row.base_revision != scene.revision {
            Standing::LiveMoved
        } else {
            Standing::Yes
        }
    }

    async fn store_wait_problems(
        &self,
        scene_id: &str,
        id: &str,
        issues: &[PreflightIssue],
    ) -> Result<(), ServiceError> {
        let _guard = self.lock.lock().await;
        let mut conn = self.conn().await?;
        let text = serde_json::to_string(issues).expect("issues serialize");
        query::set_wait_problems(&mut conn, scene_id, id, &text).await?;
        Ok(())
    }

    /// Marks the draft `FAILED`, if this wait is still its mark.
    async fn fail_wait(
        &self,
        scene_id: &str,
        id: &str,
        issues: &[PreflightIssue],
        message: Option<&str>,
    ) {
        let result: Result<(), ServiceError> = async {
            let _guard = self.lock.lock().await;
            let mut conn = self.conn().await?;
            let text = serde_json::to_string(issues).expect("issues serialize");
            query::mark_failed(&mut conn, scene_id, id, &text, message).await?;
            Ok(())
        }
        .await;
        if let Err(e) = result {
            log::error!("Scene service: couldn't mark the draft of [{scene_id}] as failed: {e}");
        }
    }

    /// Clears a draft's mark, and stops the wait behind it.
    pub(in crate::modules::scene::service) async fn clear_submit(
        &self,
        scene_id: &str,
    ) -> Result<(), ServiceError> {
        if let Some(wait) = self
            .submit_waits
            .lock()
            .expect("not poisoned")
            .remove(scene_id)
        {
            wait.cancel.cancel();
        }
        let _guard = self.lock.lock_read().await;
        let mut conn = self.conn().await?;
        if query::clear_submit(&mut conn, scene_id).await? > 0 {
            self.lock.bump();
        }
        Ok(())
    }

    /// At start-up: no wait survives a restart. Live never changed.
    pub(crate) async fn fail_stale_submits(&self) -> Result<(), ServiceError> {
        let mut conn = self.conn().await?;
        let failed = query::fail_all_waiting(&mut conn, RESTARTED).await?;
        if failed > 0 {
            log::warn!("Scene service: {failed} draft submit(s) were waiting at the last stop");
        }
        Ok(())
    }

    /// The draft's mark, as the API shows it.
    pub(super) fn draft_submit(&self, row: &DraftRow) -> Option<DraftSubmit> {
        let state = match row.submit_state.as_deref()? {
            "WAITING" => DraftSubmitState::Waiting,
            _ => DraftSubmitState::Failed,
        };
        let retry = (state == DraftSubmitState::Waiting)
            .then(|| {
                let waits = self.submit_waits.lock().expect("not poisoned");
                waits
                    .get(&row.scene_id)
                    .filter(|w| row.submit_id.as_deref() == Some(w.id.as_str()))
                    .map(|w| w.progress.borrow().retry.clone())
            })
            .flatten();
        Some(DraftSubmit {
            state,
            message: row.submit_message.clone(),
            problems: row
                .submit_problems
                .as_deref()
                .and_then(|t| serde_json::from_str(t).ok())
                .unwrap_or_default(),
            submit_time: row.submit_time.map(|t| t.timestamp_millis()),
            retry,
        })
    }

    /// Commits the draft in one transaction: the scene's graph becomes the
    /// draft's, the revision goes up once, the draft goes, and the director
    /// reloads the scene once (a diff: only changed nodes restart). With
    /// `wait`, only if that wait is still the draft's mark.
    pub(super) async fn commit_draft(
        &self,
        scene_id: &str,
        update_time: DateTime<Local>,
        wait: Option<&str>,
        check: &SceneCheck,
        disable: bool,
    ) -> Result<Scene, ServiceError> {
        let _guard = self.lock.lock().await;
        let mut conn = self.conn().await?;
        let scene = self.scene_row(&mut conn, scene_id).await?;
        // Re-checked under the lock: nothing moved since.
        let row = query::get(&mut conn, scene_id)
            .await?
            .ok_or_else(no_draft)?;
        if row.base_revision != scene.revision
            || row.update_time != update_time
            || wait.is_some_and(|id| row.submit_id.as_deref() != Some(id))
        {
            return Err(ServiceError::Conflict(
                "the draft or live changed while submitting; try again".into(),
                vec![],
            ));
        }
        let disable = disable && scene.enabled;
        if scene.enabled && !disable && !check.problems.is_empty() {
            return Err(ServiceError::Conflict(
                "the scene is enabled and the draft has problems: fix them, or submit and disable the scene".into(),
                check.problems.clone(),
            ));
        }
        // From the stored rows (configs as stored, so unchanged nodes stay
        // byte-identical).
        let mut uploads = uploads_by_node(query::all_uploads(&mut conn, scene_id).await?);
        let target = TargetGraph {
            name: row.name.clone(),
            max_hops: row.max_hops,
            defaults: row.defaults.clone(),
            groups: row.groups.clone(),
            nodes: query::all_nodes(&mut conn, scene_id)
                .await?
                .into_iter()
                .filter(|(_, m)| !m.removed)
                .map(|(n, _)| {
                    let files = uploads.remove(&n.id).unwrap_or_default();
                    TargetNode::from_row(n, files)
                })
                .collect(),
            edges: query::all_edges(&mut conn, scene_id)
                .await?
                .into_iter()
                .filter(|(_, m)| !m.removed)
                .map(|(e, _)| TargetEdge::from_row(e))
                .collect(),
        };
        let mut tx = conn.begin_with(WRITE_TX).await?;
        let applied = self.apply_graph(&mut tx, &scene, &target).await?;
        query::delete(&mut tx, scene_id).await?;
        if disable {
            query_sqlite::set_scene_enabled(&mut tx, scene_id, false).await?;
        }
        tx.commit().await?;
        self.after_apply(&mut conn, &scene, &applied, disable)
            .await?;
        log::info!(
            "Scene service: submitted the draft of scene [{scene_id}] (revision {}){}",
            scene.revision + 1,
            if disable { "; scene disabled" } else { "" }
        );
        self.scene_row(&mut conn, scene_id).await
    }

    /// Whether a wait's task is still registered.
    #[cfg(test)]
    pub(crate) fn has_submit_wait(&self, scene_id: &str) -> bool {
        self.submit_waits
            .lock()
            .expect("not poisoned")
            .contains_key(scene_id)
    }

    /// [`Service::submit`], for tests that expect it to commit.
    #[cfg(test)]
    pub(crate) async fn submit_draft(
        &'static self,
        scene_id: &str,
        base_revision: Option<i64>,
        disable: bool,
    ) -> Result<Scene, ServiceError> {
        match self.submit(scene_id, base_revision, disable).await? {
            Submitted::Committed(scene) => Ok(scene),
            Submitted::Waiting(_) => panic!("the submit is waiting"),
        }
    }
}

/// Whether a change to this part of a node can restart it. A check of a node
/// that keeps running would find its own device busy.
pub(crate) fn may_restart(part: &str) -> bool {
    !matches!(part, "name" | "config.groups")
}
