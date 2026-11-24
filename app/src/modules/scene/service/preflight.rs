//! Pre-flight checks (manifest `configurator.preflight`) from the scene
//! service: on request, and before a scene is enabled. The programs run
//! without the service's lock, so a slow check never holds up other changes.

use super::{SceneSnapshot, Service, ServiceError, Store};
use crate::modules::configurator::{self, PreflightTarget};
use crate::modules::director::running::{Context, RunningNodeView, nodes_kept, releasing_nodes};
use crate::modules::node_config::LaunchMode;
use flwrs_plugin::package::configure::has_preflight;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use tokio_util::sync::CancellationToken;
use utoipa::ToSchema;

/// What a check found on this machine.
#[derive(Serialize, ToSchema, Debug, Clone, PartialEq)]
pub(crate) struct PreflightRun {
    /// No node has a problem.
    pub ok: bool,
    /// Some problem is permanent: enabling the scene would be refused.
    pub permanent: bool,
    pub nodes: Vec<NodePreflight>,
}

#[derive(Serialize, ToSchema, Debug, Clone, PartialEq)]
pub(crate) struct NodePreflight {
    pub node_id: String,
    pub plugin_id: String,
    /// Whether a check ran for this node.
    pub checked: bool,
    /// Why not (a remote node, a plugin without a pre-flight check).
    pub skipped: Option<String>,
    pub problems: Vec<NodePreflightProblem>,
    pub warnings: Vec<String>,
}

#[derive(Serialize, ToSchema, Debug, Clone, PartialEq)]
pub(crate) struct NodePreflightProblem {
    pub message: String,
    /// It may go away by itself; otherwise it's permanent.
    pub retryable: bool,
}

/// What the director has running, for working out `releasing`.
#[derive(Default)]
pub(crate) struct RunningScenes {
    context: Option<Context>,
    scenes: HashMap<String, Vec<RunningNodeView>>,
}

impl Service {
    /// The director's surroundings (at its start), to plan a graph with.
    pub(crate) fn set_director_context(&self, context: Context) {
        self.running.lock().expect("not poisoned").context = Some(context);
    }

    /// What the scene runs now (`None`: nothing).
    pub(crate) fn publish_running(&self, scene_id: &str, nodes: Option<Vec<RunningNodeView>>) {
        let mut running = self.running.lock().expect("not poisoned");
        match nodes {
            Some(nodes) => {
                running.scenes.insert(scene_id.to_string(), nodes);
            }
            None => {
                running.scenes.remove(scene_id);
            }
        }
    }

    /// Sets each target's `releasing`: the running nodes of its plugin, in
    /// this scene, that moving to `snapshot` would stop.
    pub(crate) fn fill_releasing(&self, snapshot: &SceneSnapshot, targets: &mut [PreflightTarget]) {
        let (nodes, context) = {
            let running = self.running.lock().expect("not poisoned");
            (
                running.scenes.get(&snapshot.scene.id).cloned(),
                running.context.clone(),
            )
        };
        let releasing = match (nodes, context) {
            (Some(nodes), Some(context)) => releasing_nodes(&nodes, snapshot, &context),
            _ => HashMap::new(),
        };
        for target in targets {
            target.releasing = releasing
                .get(&target.plugin_id)
                .cloned()
                .unwrap_or_default();
        }
    }

    /// The nodes that moving a running scene to `snapshot` wouldn't restart:
    /// nothing about them needs checking.
    pub(super) fn nodes_kept_running(&self, snapshot: &SceneSnapshot) -> HashSet<String> {
        let running = self.running.lock().expect("not poisoned");
        match (running.scenes.get(&snapshot.scene.id), &running.context) {
            (Some(nodes), Some(context)) => nodes_kept(nodes, snapshot, context),
            _ => HashSet::new(),
        }
    }

    /// Runs the check for the scene's nodes (or one), starting nothing.
    pub(crate) async fn preflight(
        &self,
        scene_id: &str,
        node_id: Option<&str>,
        draft: bool,
    ) -> Result<PreflightRun, ServiceError> {
        let store = if draft {
            // 404 without a draft.
            self.get_draft(scene_id).await?;
            Store::Draft
        } else {
            Store::Live
        };
        let snapshot = self.snapshot_in(store, scene_id).await?;
        if let Some(id) = node_id
            && !snapshot.nodes.iter().any(|n| n.id == id)
        {
            return Err(ServiceError::NotFound("node"));
        }
        Ok(self.run_preflight(&snapshot, true, node_id).await)
    }

    /// What a node's configurator says about these uploads, without storing
    /// anything; `None` when its plugin has no describe mode. Only the upload
    /// settings of `launch_args` are used.
    pub(crate) async fn describe_node(
        &self,
        scene_id: &str,
        node_id: &str,
        draft: bool,
        launch_args: &serde_json::Value,
    ) -> Result<Option<serde_json::Map<String, serde_json::Value>>, ServiceError> {
        let store = if draft {
            self.get_draft(scene_id).await?;
            Store::Draft
        } else {
            Store::Live
        };
        let snapshot = self.snapshot_in(store, scene_id).await?;
        let node = snapshot
            .nodes
            .iter()
            .find(|n| n.id == node_id)
            .ok_or(ServiceError::NotFound("node"))?;
        if node.config.launch != LaunchMode::Local {
            return Ok(None);
        }
        let Some(package) = snapshot
            .plugins
            .get(&node.plugin_id)
            .and_then(configurator::PackageRef::of)
        else {
            return Ok(None);
        };
        let uploads = node
            .uploads
            .iter()
            .map(|u| {
                (
                    u.name.clone(),
                    self.files
                        .files_dir
                        .join(super::uploads::stored_name(&u.name, &u.sha256)),
                )
            })
            .collect();
        configurator::describe(
            &package,
            launch_args,
            node_id,
            uploads,
            &self.configure_limits(),
        )
        .await
        .map_err(ServiceError::Invalid)
    }

    /// For a check on request: a node's own running instance holds what the
    /// node needs, so it mustn't count as something else being in the way.
    fn add_own_instances(&self, scene_id: &str, targets: &mut [PreflightTarget]) {
        let running = self.running.lock().expect("not poisoned");
        let Some(nodes) = running.scenes.get(scene_id) else {
            return;
        };
        for target in targets {
            if target.releasing.iter().any(|r| r.node_id == target.node_id) {
                continue;
            }
            if let Some(own) = nodes
                .iter()
                .find(|n| n.id() == target.node_id)
                .and_then(RunningNodeView::as_releasing)
            {
                target.releasing.push(own);
            }
        }
    }

    /// With `running`, the check knows what a running scene would release.
    /// `only` limits it to one node; the whole graph still decides what stops.
    async fn run_preflight(
        &self,
        snapshot: &SceneSnapshot,
        running: bool,
        only: Option<&str>,
    ) -> PreflightRun {
        let mut targets = configurator::preflight_targets(snapshot, &self.files.files_dir, None);
        if running {
            self.fill_releasing(snapshot, &mut targets);
            self.add_own_instances(&snapshot.scene.id, &mut targets);
        }
        targets.retain(|t| only.is_none_or(|id| t.node_id == id));
        let reports: HashMap<String, _> = configurator::run_preflight(
            &targets,
            &self.configure_limits(),
            &CancellationToken::new(),
        )
        .await
        .into_iter()
        .collect();
        let nodes: Vec<NodePreflight> = snapshot
            .nodes
            .iter()
            .filter(|node| only.is_none_or(|id| node.id == id))
            .map(|node| {
                let report = reports.get(&node.id);
                let skipped = report.is_none().then(|| {
                    if node.config.launch != LaunchMode::Local {
                        "a remote node runs elsewhere".to_string()
                    } else if snapshot.plugins.contains_key(&node.plugin_id) {
                        debug_assert!(
                            !snapshot.plugins[&node.plugin_id]
                                .parsed_manifest()
                                .is_some_and(|m| has_preflight(&m))
                        );
                        "its plugin has no pre-flight check".to_string()
                    } else {
                        "its plugin isn't installed".to_string()
                    }
                });
                NodePreflight {
                    node_id: node.id.clone(),
                    plugin_id: node.plugin_id.clone(),
                    checked: report.is_some(),
                    skipped,
                    problems: report
                        .map(|r| {
                            r.problems
                                .iter()
                                .map(|p| NodePreflightProblem {
                                    message: p.message.clone(),
                                    retryable: p.retryable,
                                })
                                .collect()
                        })
                        .unwrap_or_default(),
                    warnings: report.map(|r| r.warnings.clone()).unwrap_or_default(),
                }
            })
            .collect();
        let problems = || nodes.iter().flat_map(|n| &n.problems);
        PreflightRun {
            ok: problems().next().is_none(),
            permanent: problems().any(|p| !p.retryable),
            nodes,
        }
    }

    /// Refuses to enable a scene with a permanent problem. Retryable ones
    /// pass: the director waits them out. A scene that's already enabled or
    /// fails its check is left to `set_enabled`.
    pub(super) async fn preflight_before_enable(&self, id: &str) -> Result<(), ServiceError> {
        let snapshot = self.snapshot(id).await?;
        if snapshot.scene.enabled
            || configurator::preflight_targets(&snapshot, &self.files.files_dir, None).is_empty()
            || !self.check(id).await?.problems.is_empty()
        {
            return Ok(());
        }
        // Nothing of a disabled scene runs: nothing to release.
        let run = self.run_preflight(&snapshot, false, None).await;
        if !run.permanent {
            return Ok(());
        }
        let details = run
            .nodes
            .iter()
            .flat_map(|n| {
                n.problems
                    .iter()
                    .filter(|p| !p.retryable)
                    .map(|p| format!("node {}: {}", n.node_id, p.message))
            })
            .collect();
        Err(ServiceError::Conflict(
            "scene can't be enabled".into(),
            details,
        ))
    }
}
