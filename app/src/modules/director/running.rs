//! One running scene: its nodes (supervised), their inbound queues, and the
//! routing between them.
//!
//! **Reload is a diff:** nodes whose plugin, type, config and
//! queue settings are unchanged keep running (process, queue and counters);
//! added nodes start, removed nodes stop, changed nodes restart. Edges,
//! mappings and the hop limit are swapped in each producer's routing table,
//! with no restart: every event is routed with one table, the old or the new.

use super::queue::{
    Budget, DropReason, DurableSettings, NodeQueue, Pushed, QueueKind, QueueSettings, QueueStats,
    Queued, SpillSettings, SpillShared, delete_files,
};
use crate::modules::hub::wire::{DataEvent, Negotiated};
use crate::modules::hub::{Hub, HubError, LinkSender, Routed};
use crate::modules::mapping::{self, Mapping, MappingSpec, Schemas};
use crate::modules::node_config::WhenFull;
use crate::modules::plugin::manifest::PluginType;
use crate::modules::rate_limit::RateLimit;
use crate::modules::scene::graph::nodes_in_cycles;
use crate::modules::node_config::NodeConfig;
use crate::modules::scene::service::{Edge, SceneSnapshot};
use crate::modules::supervisor::{
    DeliverySpec, Launch, NodeHandle, NodeOutput, NodeSpec, NodeState, NodeStatus, Output,
    Surroundings, supervise,
};
use flwrs_plugin::schema::payload::PluginPayload;
use flwrs_plugin::schema::protocol::EventMetadata;
use serde::Serialize;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::{mpsc, watch};
use tokio::task::{JoinHandle, JoinSet};
use tokio_util::sync::CancellationToken;
use ulid::Ulid;
use utoipa::ToSchema;

/// Events from one node's plugin waiting to be routed. When full, the
/// node's supervisor stops reading from the hub: backpressure.
const OUTPUT_CAPACITY: usize = 64;
const DROP_LOG_INTERVAL: Duration = Duration::from_secs(10);
/// How long a stopped node's router may take to pass on its last events.
const ROUTER_DRAIN: Duration = Duration::from_secs(1);

/// What the director needs from its surroundings to run scenes.
#[derive(Clone)]
pub(crate) struct Context {
    pub hub: Hub,
    pub budget: Budget,
    /// Spool directory and hub-wide limits for spill-to-disk queues.
    pub spill: SpillShared,
    pub max_hops: u32,
    pub log_level: String,
}

/// A node's run-time report, for status.
#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub(crate) struct NodeReport {
    pub node_id: String,
    /// The plugin the node runs, and its version.
    pub plugin_id: String,
    pub plugin_version: String,
    pub status: NodeStatus,
    /// The headline numbers; details are in `queue` and the fields below.
    pub counters: NodeCounters,
    /// Inbound queue (sinks and transforms).
    pub queue: Option<QueueStats>,
    /// Events from this node dropped for exceeding the hop limit.
    pub dropped_hops: u64,
    /// Events from this node that an out-edge's mapping couldn't map (they
    /// weren't sent along that edge).
    pub mapping_errors: u64,
}

/// Per-node totals since the scene started.
#[derive(Debug, Clone, Default, PartialEq, Serialize, ToSchema)]
pub(crate) struct NodeCounters {
    /// Events the node's plugin produced (sources and transforms).
    pub emitted: u64,
    /// Events handed to the node's plugin (sinks and transforms).
    pub received: u64,
    /// Events for this node that were dropped: queue full, block timeout,
    /// budget, too large, evicted, disk errors, dead-lettered (out of
    /// delivery attempts); plus this node's events dropped for the hop limit.
    pub dropped: u64,
    /// This node's events that an out-edge's mapping couldn't map.
    pub mapping_errors: u64,
    /// Relaunches of the plugin (local nodes).
    pub restarts: u64,
}

/// What a node needs to keep running across a reload. If any of it
/// changes, the node restarts.
#[derive(Debug, Clone, PartialEq)]
struct NodeKey {
    plugin_id: String,
    plugin_version: String,
    /// A re-uploaded package counts as a change even with the same version.
    archive_sha256: String,
    install_path: String,
    node_type: String,
    config: NodeConfig,
    /// The queue policy in effect: its default depends on whether the node
    /// is on a cycle, which edge changes can alter.
    when_full: Option<WhenFull>,
    /// Memory, spill or durable (the scene's default can change it).
    kind: Option<QueueKind>,
}

/// A node of a checked graph.
struct PlannedNode {
    spec: NodeSpec,
    key: NodeKey,
    /// Inbound queue (sinks and transforms).
    queue: Option<QueueSettings>,
    kind: QueueKind,
}

/// A scene's graph, checked and ready to run.
struct Plan {
    nodes: Vec<PlannedNode>,
    edges: Vec<Edge>,
    /// Compiled once per load, never per event. Edges without one pass
    /// events through.
    mappings: HashMap<String, Arc<dyn Mapping>>,
    max_hops: u32,
}

impl Plan {
    /// Checks the graph (the director doesn't trust that the DB is valid).
    /// Returns every problem.
    fn new(snapshot: &SceneSnapshot, ctx: &Context) -> Result<Self, Vec<String>> {
        let scene_id = &snapshot.scene.id;
        let mut errors = vec![];

        // --- nodes ---
        let mut specs: Vec<NodeSpec> = vec![];
        for node in &snapshot.nodes {
            let Some(plugin) = snapshot.plugins.get(&node.plugin_id) else {
                errors.push(format!(
                    "node {}: plugin {} isn't registered",
                    node.id, node.plugin_id
                ));
                continue;
            };
            let at = Surroundings {
                hub_addr: ctx.hub.local_addr(),
                tls_available: ctx.hub.tls_addr().is_some(),
                log_level: &ctx.log_level,
                scene_defaults: &snapshot.scene.defaults,
            };
            match NodeSpec::resolve(scene_id, &node.id, &node.config, plugin, &at) {
                Ok(spec) if spec.plugin_type.as_str() != node.node_type => errors.push(format!(
                    "node {}: type {} doesn't match plugin {} ({})",
                    node.id,
                    node.node_type,
                    plugin.id,
                    spec.plugin_type.as_str()
                )),
                Ok(spec) => specs.push(spec),
                Err(problems) => errors.extend(
                    problems
                        .into_iter()
                        .map(|p| format!("node {}: {p}", node.id)),
                ),
            }
        }
        let types: HashMap<&str, PluginType> = specs
            .iter()
            .map(|s| (s.node_id.as_str(), s.plugin_type))
            .collect();

        // --- edges ---
        let mut mappings: HashMap<String, Arc<dyn Mapping>> = HashMap::new();
        for edge in &snapshot.edges {
            let (from, to) = (
                types.get(edge.source_node_id.as_str()),
                types.get(edge.target_node_id.as_str()),
            );
            if from == Some(&PluginType::Sink) {
                errors.push(format!("edge {}: a sink can't send events", edge.id));
            }
            if to == Some(&PluginType::Source) {
                errors.push(format!("edge {}: a source can't receive events", edge.id));
            }
            if let Some(json) = &edge.mapping {
                let schemas = |id: &str, out: bool| {
                    specs.iter().find(|s| s.node_id == id).and_then(|s| {
                        if out {
                            s.out_schema.as_ref()
                        } else {
                            s.in_schema.as_ref()
                        }
                    })
                };
                let compiled = MappingSpec::parse(json).and_then(|spec| {
                    let spec = spec.ok_or_else(Vec::new)?;
                    mapping::compile(
                        &spec,
                        Schemas {
                            source: schemas(&edge.source_node_id, true),
                            target: schemas(&edge.target_node_id, false),
                        },
                    )
                });
                match compiled {
                    Ok(compiled) => {
                        mappings.insert(edge.id.clone(), compiled);
                    }
                    Err(problems) if problems.is_empty() => {} // null: pass-through
                    Err(problems) => errors.extend(
                        problems
                            .into_iter()
                            .map(|p| format!("edge {}: {p}", edge.id)),
                    ),
                }
            }
        }
        let node_ids: Vec<&str> = snapshot.nodes.iter().map(|n| n.id.as_str()).collect();
        let pairs: Vec<(&str, &str)> = snapshot
            .edges
            .iter()
            .map(|e| (e.source_node_id.as_str(), e.target_node_id.as_str()))
            .collect();
        let cyclic = nodes_in_cycles(&node_ids, &pairs);
        for spec in &specs {
            if cyclic.contains(spec.node_id.as_str())
                && spec.queue.when_full == Some(WhenFull::Block)
            {
                errors.push(format!(
                    "node {}: queue.when_full \"block\" isn't allowed on a cycle (it could deadlock)",
                    spec.node_id
                ));
            }
        }
        if !errors.is_empty() {
            return Err(errors);
        }

        let nodes = specs
            .into_iter()
            .map(|spec| {
                let node = snapshot
                    .nodes
                    .iter()
                    .find(|n| n.id == spec.node_id)
                    .expect("specs come from nodes");
                let plugin = &snapshot.plugins[&node.plugin_id];
                let queue = (spec.plugin_type != PluginType::Source).then(|| {
                    let default_policy = if cyclic.contains(spec.node_id.as_str()) {
                        WhenFull::DropNewest
                    } else {
                        WhenFull::Block
                    };
                    QueueSettings {
                        capacity: spec.queue.capacity,
                        max_bytes: spec.queue.max_bytes,
                        when_full: spec.queue.when_full.unwrap_or(default_policy),
                        block_timeout: spec.queue.block_timeout,
                    }
                });
                let durable = spec
                    .queue
                    .durable
                    .or(snapshot.scene.defaults.durable)
                    .unwrap_or(false);
                let spill = spec
                    .queue
                    .spill_enabled
                    .unwrap_or(ctx.spill.default_enabled);
                let kind = match (durable, spill) {
                    (true, _) => QueueKind::Durable,
                    (false, true) => QueueKind::Spill,
                    (false, false) => QueueKind::Memory,
                };
                let key = NodeKey {
                    plugin_id: plugin.id.clone(),
                    plugin_version: plugin.version.clone(),
                    archive_sha256: plugin.archive_sha256.clone(),
                    install_path: plugin.install_path.clone(),
                    node_type: node.node_type.clone(),
                    config: node.config.clone(),
                    when_full: queue.as_ref().map(|q| q.when_full),
                    kind: queue.is_some().then_some(kind),
                };
                PlannedNode {
                    spec,
                    key,
                    queue,
                    kind,
                }
            })
            .collect();
        Ok(Self {
            nodes,
            edges: snapshot.edges.clone(),
            mappings,
            max_hops: snapshot.scene.max_hops.unwrap_or(ctx.max_hops),
        })
    }

    /// The routing table of `node_id`: its out-edges, to the given queues.
    fn routes(&self, node_id: &str, queues: &HashMap<String, Arc<NodeQueue>>) -> Arc<Routes> {
        let targets = self
            .edges
            .iter()
            .filter(|e| e.source_node_id == node_id)
            .filter(|e| self.nodes.iter().any(|n| n.spec.node_id == e.target_node_id))
            .filter_map(|e| {
                Some(Target {
                    edge_id: e.id.clone(),
                    node_id: e.target_node_id.clone(),
                    queue: queues.get(&e.target_node_id)?.clone(),
                    mapping: self.mappings.get(&e.id).cloned(),
                })
            })
            .collect();
        Arc::new(Routes {
            targets,
            max_hops: self.max_hops,
        })
    }
}

struct RunningNode {
    id: String,
    plugin_id: String,
    plugin_version: String,
    plugin_type: PluginType,
    key: NodeKey,
    remote: bool,
    handle: NodeHandle,
    queue: Option<Arc<NodeQueue>>,
    /// The node's routing table, swapped on reload.
    routes: watch::Sender<Arc<Routes>>,
    router: JoinHandle<()>,
    deliverer: Option<DelivererHandle>,
    dropped_hops: Arc<AtomicU64>,
    mapping_errors: Arc<AtomicU64>,
    emitted: Arc<AtomicU64>,
}

/// A running deliverer; stopping it puts its in-flight events back into the
/// queue (which may outlive the node, e.g. for a changed node on reload).
struct DelivererHandle {
    task: JoinHandle<()>,
    stop: CancellationToken,
}

impl DelivererHandle {
    async fn stop(self) {
        self.stop.cancel();
        let mut task = self.task;
        if tokio::time::timeout(ROUTER_DRAIN, &mut task).await.is_err() {
            task.abort();
        }
    }
}

impl RunningNode {
    async fn stop_deliverer(&mut self) {
        if let Some(deliverer) = self.deliverer.take() {
            deliverer.stop().await;
        }
    }
}

pub(crate) struct RunningScene {
    scene_id: String,
    /// The hop limit in effect: the scene's own, or the server default.
    max_hops: u32,
    /// `<spool_dir>/<scene-id>`: removed when the scene stops.
    spool_dir: std::path::PathBuf,
    nodes: Vec<RunningNode>,
}

/// One producer's out-edges and the hop limit, as one swappable table.
struct Routes {
    targets: Vec<Target>,
    max_hops: u32,
}

/// One out-edge, resolved for routing.
struct Target {
    edge_id: String,
    node_id: String,
    queue: Arc<NodeQueue>,
    /// `None`: events pass through unchanged.
    mapping: Option<Arc<dyn Mapping>>,
}

/// What a reload did, for the log.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct ReloadSummary {
    pub kept: usize,
    pub restarted: usize,
    pub added: usize,
    pub removed: usize,
}

impl std::fmt::Display for ReloadSummary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} nodes kept, {} restarted, {} added, {} removed",
            self.kept, self.restarted, self.added, self.removed
        )
    }
}

/// A new inbound queue. Each queue has its own spool file, since a
/// replaced node's old queue may still be closing. With `adopt` (a fresh
/// start), a durable queue takes over the node's existing files.
fn new_queue(
    planned: &PlannedNode,
    scene_id: &str,
    ctx: &Context,
    adopt: bool,
) -> Option<Arc<NodeQueue>> {
    let settings = planned.queue.clone()?;
    let dir = ctx.spill.scene_dir(scene_id);
    Some(match planned.kind {
        QueueKind::Durable => NodeQueue::new_durable(
            settings,
            ctx.budget.clone(),
            DurableSettings {
                spec: planned.spec.queue.spill.clone(),
                dir,
                node_id: planned.spec.node_id.clone(),
                shared: ctx.spill.clone(),
                adopt,
            },
        ),
        QueueKind::Spill => NodeQueue::new(
            settings,
            ctx.budget.clone(),
            Some(SpillSettings {
                spec: planned.spec.queue.spill.clone(),
                file: dir.join(format!("{}.{}.db", planned.spec.node_id, Ulid::generate())),
                shared: ctx.spill.clone(),
            }),
        ),
        QueueKind::Memory => NodeQueue::new(settings, ctx.budget.clone(), None),
    })
}

/// Supervises a node and starts its router (and delivery, if it receives).
fn launch(
    planned: &PlannedNode,
    queue: Option<Arc<NodeQueue>>,
    routes: Arc<Routes>,
    ctx: &Context,
) -> RunningNode {
    let spec = &planned.spec;
    let (tx, rx) = mpsc::channel(OUTPUT_CAPACITY);
    let handle = supervise(spec.clone(), &ctx.hub, tx);
    let (routes_tx, routes_rx) = watch::channel(routes);
    let dropped_hops = Arc::new(AtomicU64::new(0));
    let mapping_errors = Arc::new(AtomicU64::new(0));
    let emitted = Arc::new(AtomicU64::new(0));
    let (acks_tx, acks_rx) = mpsc::unbounded_channel();
    let router = tokio::spawn(route(Router {
        tag: spec.tag(),
        node_id: spec.node_id.clone(),
        is_source: spec.plugin_type == PluginType::Source,
        routes: routes_rx,
        dropped_hops: dropped_hops.clone(),
        mapping_errors: mapping_errors.clone(),
        emitted: emitted.clone(),
        outputs: rx,
        producer: handle.sender(),
        acks: queue.is_some().then_some(acks_tx),
    }));
    let deliverer = queue.as_ref().map(|queue| {
        let stop = CancellationToken::new();
        let task = tokio::spawn(deliver(Deliverer {
            tag: spec.tag(),
            queue: queue.clone(),
            sender: handle.sender(),
            status: handle.watch(),
            acks: acks_rx,
            delivery: spec.delivery.clone(),
            stop: stop.clone(),
        }));
        DelivererHandle { task, stop }
    });
    RunningNode {
        id: spec.node_id.clone(),
        plugin_id: spec.plugin_id.clone(),
        plugin_version: spec.plugin_version.clone(),
        plugin_type: spec.plugin_type,
        key: planned.key.clone(),
        remote: matches!(spec.launch, Launch::Remote { .. }),
        handle,
        queue,
        routes: routes_tx,
        router,
        deliverer,
        dropped_hops,
        mapping_errors,
        emitted,
    }
}

/// Stops sources first, then transforms, then sinks, so events already on
/// their way can still drain; then their routing and queues. With `remove`
/// (the nodes are gone), durable queues delete their files too; otherwise
/// they keep them for the next start.
async fn stop_nodes(nodes: Vec<RunningNode>, remove: bool) {
    let mut groups: [Vec<NodeHandle>; 3] = Default::default();
    let mut rest = vec![];
    for node in nodes {
        let group = match node.plugin_type {
            PluginType::Source => 0,
            PluginType::Transform => 1,
            PluginType::Sink => 2,
        };
        groups[group].push(node.handle);
        rest.push((node.router, node.deliverer, node.queue));
    }
    for group in groups {
        let mut stopping = JoinSet::new();
        for handle in group {
            stopping.spawn(handle.stop());
        }
        stopping.join_all().await;
    }
    for (mut router, deliverer, queue) in rest {
        // A stopped node's router ends once it has passed on what's left.
        if tokio::time::timeout(ROUTER_DRAIN, &mut router).await.is_err() {
            router.abort();
        }
        if let Some(deliverer) = deliverer {
            deliverer.stop().await;
        }
        if let Some(queue) = queue {
            queue.finish(remove).await;
        }
    }
}

impl RunningScene {
    /// Checks the graph, then starts every node and the routing. Nothing is
    /// started if any check fails; every problem is returned.
    pub(crate) async fn start(snapshot: &SceneSnapshot, ctx: &Context) -> Result<Self, Vec<String>> {
        let plan = Plan::new(snapshot, ctx)?;
        let scene_id = snapshot.scene.id.clone();
        let mut scene = Self {
            spool_dir: ctx.spill.scene_dir(&scene_id),
            max_hops: plan.max_hops,
            scene_id,
            nodes: vec![],
        };
        scene.clean_spool(&plan).await;
        scene.apply(plan, ctx, true).await;
        log::info!(
            "[{}] scene started with {} nodes (max_hops {})",
            scene.scene_id,
            scene.nodes.len(),
            scene.max_hops
        );
        Ok(scene)
    }

    /// Moves the running scene to the new graph (see the module docs). If
    /// the new graph fails its checks, nothing is changed and every problem
    /// is returned (the engine then stops the scene).
    pub(crate) async fn reload(
        &mut self,
        snapshot: &SceneSnapshot,
        ctx: &Context,
    ) -> Result<ReloadSummary, Vec<String>> {
        let plan = Plan::new(snapshot, ctx)?;
        let summary = self.apply(plan, ctx, false).await;
        log::info!(
            "[{}] scene reloaded: {summary} (max_hops {})",
            self.scene_id,
            self.max_hops
        );
        Ok(summary)
    }

    async fn apply(&mut self, plan: Plan, ctx: &Context, fresh: bool) -> ReloadSummary {
        let mut old: HashMap<String, RunningNode> =
            self.nodes.drain(..).map(|n| (n.id.clone(), n)).collect();
        let mut summary = ReloadSummary::default();
        let mut kept: HashMap<String, RunningNode> = HashMap::new();
        for planned in &plan.nodes {
            let id = &planned.spec.node_id;
            match old.get(id) {
                Some(node) if node.key == planned.key => {
                    kept.extend(old.remove_entry(id));
                }
                Some(_) => summary.restarted += 1,
                None => summary.added += 1,
            }
        }
        summary.kept = kept.len();
        summary.removed = old.len() - summary.restarted;

        // A changed node keeps its queue (with the new limits), so what's
        // queued for it survives the restart; unless the queue's kind
        // changed (spill on or off), which needs a new one. Added nodes get
        // new queues. Either way producers can route to them right away, and
        // events wait there until the new plugin runs.
        let mut queues: HashMap<String, Arc<NodeQueue>> = kept
            .values()
            .filter_map(|n| Some((n.id.clone(), n.queue.clone()?)))
            .collect();
        for planned in plan.nodes.iter().filter(|p| !kept.contains_key(&p.spec.node_id)) {
            let id = &planned.spec.node_id;
            let reusable = old.get_mut(id).filter(|n| {
                planned.queue.is_some()
                    && n.queue.as_ref().is_some_and(|q| q.kind() == planned.kind)
            });
            if let Some(old_node) = reusable {
                let queue = old_node.queue.take().expect("checked above");
                queue.reconfigure(planned.queue.clone().expect("checked above"));
                queues.insert(id.clone(), queue);
            } else if let Some(queue) = new_queue(planned, &self.scene_id, ctx, fresh) {
                queues.insert(id.clone(), queue);
            }
        }
        // Swap the kept producers' routing tables.
        for node in kept.values() {
            node.routes.send_replace(plan.routes(&node.id, &queues));
        }

        // A changed remote node: the new expectation takes over the hub's
        // connection without a Shutdown, so the remote plugin reconnects to
        // it (Shutdown would make it exit for good). Then stop what's gone
        // or changed; then start the rest (a changed local node only after
        // its old process stopped).
        let mut started: HashMap<String, RunningNode> = HashMap::new();
        let start = |planned: &PlannedNode, started: &mut HashMap<String, RunningNode>| {
            let id = planned.spec.node_id.clone();
            let node = launch(planned, queues.get(&id).cloned(), plan.routes(&id, &queues), ctx);
            started.insert(id, node);
        };
        for planned in &plan.nodes {
            let id = &planned.spec.node_id;
            let replaces_remote = old.get(id).is_some_and(|n| n.remote);
            if replaces_remote && matches!(planned.spec.launch, Launch::Remote { .. }) {
                // Only one deliverer per queue: the old one goes first.
                if let Some(old_node) = old.get_mut(id) {
                    old_node.stop_deliverer().await;
                }
                start(planned, &mut started);
            }
        }
        // Removed nodes, and changed ones whose queue couldn't be kept.
        stop_nodes(old.into_values().collect(), true).await;
        for planned in &plan.nodes {
            let id = &planned.spec.node_id;
            if !kept.contains_key(id) && !started.contains_key(id) {
                start(planned, &mut started);
            }
        }

        self.nodes = plan
            .nodes
            .iter()
            .filter_map(|p| {
                let id = &p.spec.node_id;
                kept.remove(id).or_else(|| started.remove(id))
            })
            .collect();
        self.max_hops = plan.max_hops;
        summary
    }

    pub(crate) fn max_hops(&self) -> u32 {
        self.max_hops
    }

    pub(crate) fn reports(&self) -> Vec<NodeReport> {
        self.nodes
            .iter()
            .map(|n| {
                let status = n.handle.status();
                let queue = n.queue.as_ref().map(|q| q.stats());
                let dropped_hops = n.dropped_hops.load(Ordering::Relaxed);
                let mapping_errors = n.mapping_errors.load(Ordering::Relaxed);
                let counters = NodeCounters {
                    emitted: n.emitted.load(Ordering::Relaxed),
                    received: queue.as_ref().map_or(0, |q| q.delivered),
                    dropped: dropped_hops
                        + queue.as_ref().map_or(0, |q| {
                            q.dropped_full
                                + q.dropped_timeout
                                + q.dropped_budget
                                + q.dropped_oversized
                                + q.evicted
                                + q.dropped_disk_error
                                + q.dead_lettered
                        }),
                    mapping_errors,
                    restarts: status.launches.saturating_sub(1),
                };
                NodeReport {
                    node_id: n.id.clone(),
                    plugin_id: n.plugin_id.clone(),
                    plugin_version: n.plugin_version.clone(),
                    status,
                    counters,
                    queue,
                    dropped_hops,
                    mapping_errors,
                }
            })
            .collect()
    }

    /// Stops every node (sources, then transforms, then sinks). Durable
    /// queues keep their files (delivered when the scene runs again); the
    /// scene's spool folder goes if that leaves it empty.
    pub(crate) async fn stop(self) {
        stop_nodes(self.nodes, false).await;
        // Fails (harmlessly) if durable files are left in it.
        let _ = tokio::fs::remove_dir(&self.spool_dir).await;
        log::info!("[{}] scene stopped", self.scene_id);
    }

    /// Drops the queued events of every node (or of `node_id`).
    pub(crate) async fn purge(&self, node_id: Option<&str>) -> u64 {
        let mut purged = 0;
        for node in &self.nodes {
            if node_id.is_none_or(|id| id == node.id)
                && let Some(queue) = &node.queue
            {
                purged += queue.purge().await;
            }
        }
        purged
    }

    /// Before a fresh start: leftover files of nodes that aren't durable
    /// (now) are from an earlier run and go; durable nodes adopt theirs.
    async fn clean_spool(&self, plan: &Plan) {
        let Ok(mut entries) = tokio::fs::read_dir(&self.spool_dir).await else {
            return; // nothing there
        };
        let durable: Vec<&str> = plan
            .nodes
            .iter()
            .filter(|n| n.kind == QueueKind::Durable)
            .map(|n| n.spec.node_id.as_str())
            .collect();
        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            let node = name.split('.').next().unwrap_or_default();
            if !durable.contains(&node) && name.ends_with(".db") {
                log::debug!("[{}] removing leftover spool file {name}", self.scene_id);
                delete_files(&path).await;
            }
        }
    }
}

struct Router {
    tag: String,
    node_id: String,
    is_source: bool,
    routes: watch::Receiver<Arc<Routes>>,
    dropped_hops: Arc<AtomicU64>,
    mapping_errors: Arc<AtomicU64>,
    emitted: Arc<AtomicU64>,
    outputs: mpsc::Receiver<NodeOutput>,
    /// The node's own connection: ACK / NACK and credit for its events.
    producer: LinkSender,
    /// The node's own deliverer (sinks, transforms): its acks go there, after
    /// the events the plugin sent before them have been routed.
    acks: Option<mpsc::UnboundedSender<AckMsg>>,
}

/// Events routed to durable queues whose commit (and the producer's ack)
/// is still outstanding.
type Commits = Vec<JoinHandle<()>>;

/// A consumer's answer to events the hub delivered.
#[derive(Debug)]
pub(super) enum AckMsg {
    Acked {
        conn_id: u64,
        up_to: u64,
        seqs: Vec<u64>,
    },
    Nacked {
        conn_id: u64,
        seq: u64,
        reason: String,
        retry: bool,
    },
}

struct RouteLogs {
    hops: RateLimit,
    drops: RateLimit,
    oversized: RateLimit,
    mapping: RateLimit,
}

/// Handles one node's plugin output in order: routes its events along the
/// out-edges (filling in the event metadata: the hub is its authority) and
/// answers the producer; passes its acks on to its deliverer.
async fn route(mut r: Router) {
    let logs = RouteLogs {
        hops: RateLimit::new(DROP_LOG_INTERVAL),
        drops: RateLimit::new(DROP_LOG_INTERVAL),
        oversized: RateLimit::new(DROP_LOG_INTERVAL),
        mapping: RateLimit::new(DROP_LOG_INTERVAL),
    };
    let mut commits: Commits = vec![];
    while let Some(output) = r.outputs.recv().await {
        commits.retain(|c| !c.is_finished());
        let ack = match output.item {
            Output::Event {
                conn_id,
                seq,
                event,
                size,
            } => {
                r.emitted.fetch_add(1, Ordering::Relaxed);
                let (outcome, tickets) = route_event(&r, &logs, event, size).await;
                if tickets.is_empty() {
                    r.producer.routed(conn_id, seq, outcome).await;
                } else {
                    // Stored only once committed; the next event needn't wait.
                    let producer = r.producer.clone();
                    commits.push(tokio::spawn(async move {
                        let mut stored = true;
                        for (queue, ticket) in tickets {
                            stored &= queue.committed(ticket).await;
                        }
                        let outcome = if stored {
                            outcome
                        } else {
                            Routed::Rejected {
                                reason: "couldn't store it".into(),
                                retry: true,
                            }
                        };
                        producer.routed(conn_id, seq, outcome).await;
                    }));
                }
                continue;
            }
            Output::Acked {
                conn_id,
                up_to,
                seqs,
            } => AckMsg::Acked {
                conn_id,
                up_to,
                seqs,
            },
            Output::Nacked {
                conn_id,
                seq,
                reason,
                retry,
            } => AckMsg::Nacked {
                conn_id,
                seq,
                reason,
                retry,
            },
        };
        // A transform's ack of an input takes effect only once the outputs
        // it sent before it are stored (a crash in between can't lose them).
        for commit in commits.drain(..) {
            let _ = commit.await;
        }
        if let Some(acks) = &r.acks {
            let _ = acks.send(ack);
        }
    }
}

/// Routes one event; returns what to tell the producer, and the commits of
/// durable targets to wait for first.
async fn route_event(
    r: &Router,
    logs: &RouteLogs,
    event: DataEvent,
    size: usize,
) -> (Routed, Vec<(Arc<NodeQueue>, u64)>) {
    // One table per event: the one in effect now (a reload swaps it).
    let routes = r.routes.borrow().clone();
    // A transform's output carries its input's metadata (attached by the hub).
    let mut meta = if r.is_source {
        EventMetadata::default()
    } else {
        event.metadata.clone().unwrap_or_default()
    };
    if meta.trace_id.is_empty() {
        // A new trace: from a source, or a transform output without an input.
        meta.trace_id = Ulid::generate().to_string();
        meta.origin_node_id = r.node_id.clone();
        meta.create_time = Some(now());
        meta.hops = 0;
    }
    meta.hops = meta.hops.saturating_add(1);
    if meta.hops > routes.max_hops {
        r.dropped_hops.fetch_add(1, Ordering::Relaxed);
        if logs.hops.allow() {
            log::error!(
                "{} dropped an event of trace {} after {} hops (max_hops = {})",
                r.tag,
                meta.trace_id,
                meta.hops - 1,
                routes.max_hops
            );
        }
        let outcome = Routed::Rejected {
            reason: format!("over the hop limit ({})", routes.max_hops),
            retry: false,
        };
        return (outcome, vec![]);
    }
    // Decoded only if an edge maps it.
    let mut decoded: Option<Result<PluginPayload, String>> = None;
    let (mut accepted, mut dropped, mut failed) = (0usize, 0usize, 0usize);
    let mut tickets = vec![];
    for target in &routes.targets {
        let (body, size) = match &target.mapping {
            None => (event.body.clone(), size),
            Some(mapping) => {
                let input = decoded
                    .get_or_insert_with(|| event.payload().map_err(|e| e.to_string()));
                let mapped = match input {
                    Ok(input) => mapping.apply(input, Some(&meta)).map_err(|e| e.to_string()),
                    Err(err) => Err(format!("undecodable event: {err}")),
                };
                match mapped {
                    // Mapped payloads are re-measured for the queue's limits.
                    Ok(Some(mapped)) => {
                        let mapped = DataEvent::from_payload(mapped);
                        let size = mapped.size();
                        (mapped.body, size)
                    }
                    Ok(None) => {
                        accepted += 1; // filtered out on this edge
                        continue;
                    }
                    Err(err) => {
                        failed += 1;
                        r.mapping_errors.fetch_add(1, Ordering::Relaxed);
                        if logs.mapping.allow() {
                            log::warn!(
                                "{} edge {}: couldn't map an event for node {}, skipped: {err}",
                                r.tag,
                                target.edge_id,
                                target.node_id
                            );
                        }
                        continue;
                    }
                }
            }
        };
        let event = DataEvent {
            body,
            metadata: Some(EventMetadata {
                event_id: Ulid::generate().to_string(),
                ..meta.clone()
            }),
            attempt: 1,
        };
        let (pushed, ticket) = target.queue.push_tracked(event, size).await;
        if let Some(ticket) = ticket {
            tickets.push((target.queue.clone(), ticket));
        }
        match pushed {
            Pushed::Queued => accepted += 1,
            Pushed::Dropped(reason) => {
                dropped += 1;
                if reason == DropReason::Oversized {
                    if logs.oversized.allow() {
                        log::warn!(
                            "{} dropped an event of {} bytes for node {}: larger than the hub's memory limit ([hub] max_inflight_bytes)",
                            r.tag,
                            size,
                            target.node_id
                        );
                    }
                } else if reason != DropReason::Closed && logs.drops.allow() {
                    log::warn!(
                        "{} dropped an event for node {}: {reason:?}",
                        r.tag,
                        target.node_id
                    );
                }
            }
        }
    }
    let outcome = if dropped > 0 {
        Routed::Rejected {
            reason: format!("dropped by {dropped} of {} targets", routes.targets.len()),
            retry: true,
        }
    } else if failed > 0 && accepted == 0 {
        Routed::Rejected {
            reason: "no out-edge's mapping could map it".into(),
            retry: false,
        }
    } else {
        Routed::Accepted
    };
    (outcome, tickets)
}

struct Deliverer {
    tag: String,
    queue: Arc<NodeQueue>,
    sender: LinkSender,
    status: watch::Receiver<NodeStatus>,
    acks: mpsc::UnboundedReceiver<AckMsg>,
    delivery: DeliverySpec,
    stop: CancellationToken,
}

/// Events handed to the plugin and not acked yet, on one connection.
struct InFlight {
    conn: Option<(u64, Negotiated)>,
    /// By seq on that connection.
    items: BTreeMap<u64, Queued>,
}

/// Hands a node's queued events to its plugin, waiting while it isn't
/// running. With ACKS, keeps each event until the plugin acks it (at most
/// `max_unacked` at a time), and redelivers it after a disconnect or a NACK
/// with retry; without, an event counts as delivered once sent.
async fn deliver(mut d: Deliverer) {
    let dead_log = RateLimit::new(DROP_LOG_INTERVAL);
    let mut flight = InFlight {
        conn: None,
        items: BTreeMap::new(),
    };
    let mut paused_until: Option<Instant> = None;
    loop {
        // A new connection (or none): what was in flight comes back first.
        let current = d.sender.connection();
        if current.map(|c| c.0) != flight.conn.map(|c| c.0) {
            requeue_all(&d, &mut flight, &dead_log);
            flight.conn = current;
        }
        let running = d.status.borrow().state == NodeState::Running && flight.conn.is_some();
        let window = match flight.conn {
            Some((_, n)) if n.acks => flight.items.len() < n.max_unacked.max(1) as usize,
            _ => true,
        };
        let paused = paused_until.is_some_and(|t| Instant::now() < t);
        let can_send = running && window && !paused;
        let resume = async {
            match paused_until {
                Some(t) => tokio::time::sleep_until(t.into()).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            _ = d.stop.cancelled() => {
                requeue_all(&d, &mut flight, &dead_log);
                return;
            }
            Some(ack) = d.acks.recv() => {
                handle_ack(&d, &mut flight, ack, &mut paused_until, &dead_log);
            }
            changed = d.status.changed() => if changed.is_err() {
                return; // the node is gone
            },
            _ = resume, if paused => paused_until = None,
            item = d.queue.pop(), if can_send => {
                let Some(item) = item else {
                    return; // closed
                };
                send(&mut d, &mut flight, item, &dead_log).await;
            }
        }
        d.queue.set_in_flight(flight.items.len());
    }
}

async fn send(d: &mut Deliverer, flight: &mut InFlight, item: Queued, dead_log: &RateLimit) {
    match d.sender.send_event(&item.event).await {
        Ok((conn_id, seq)) => {
            if flight.conn.map(|c| c.0) != Some(conn_id) {
                // It reconnected meanwhile: what was in flight on the old
                // connection goes back first (behind this one, sadly).
                requeue_all(d, flight, dead_log);
                flight.conn = d.sender.connection().filter(|c| c.0 == conn_id);
            }
            match flight.conn {
                Some((_, n)) if n.acks => {
                    flight.items.insert(seq, item);
                }
                _ => d.queue.delivered(item),
            }
        }
        Err(HubError::NotConnected) => {
            // It went away meanwhile: keep the event, wait for a change.
            d.queue.requeue(item);
            let _ = tokio::time::timeout(Duration::from_millis(100), d.status.changed()).await;
        }
        Err(err) => log::error!("{} couldn't deliver an event: {err}", d.tag),
    }
}

fn handle_ack(
    d: &Deliverer,
    flight: &mut InFlight,
    ack: AckMsg,
    paused_until: &mut Option<Instant>,
    dead_log: &RateLimit,
) {
    let current = flight.conn.map(|c| c.0);
    match ack {
        AckMsg::Acked {
            conn_id,
            up_to,
            seqs,
        } if Some(conn_id) == current => {
            let rest = flight.items.split_off(&(up_to + 1));
            let done = std::mem::replace(&mut flight.items, rest);
            for (_, item) in done {
                d.queue.delivered(item);
            }
            for seq in seqs {
                if let Some(item) = flight.items.remove(&seq) {
                    d.queue.delivered(item);
                }
            }
        }
        AckMsg::Nacked {
            conn_id,
            seq,
            reason,
            retry,
        } if Some(conn_id) == current => {
            let Some(mut item) = flight.items.remove(&seq) else {
                return;
            };
            d.queue.count_nacked();
            if retry && item.event.attempt < d.delivery.max_attempts {
                item.event.attempt += 1;
                d.queue.count_redelivered();
                d.queue.requeue(item);
                *paused_until = Some(Instant::now() + d.delivery.retry_delay);
            } else {
                if dead_log.allow() {
                    log::warn!(
                        "{} gave up on an event after {} attempts: {reason}",
                        d.tag,
                        item.event.attempt
                    );
                }
                d.queue.dead_letter(item);
            }
        }
        // About an earlier connection: its events were requeued already.
        _ => {}
    }
}

/// Puts every in-flight event back at the front of the queue, in order, for
/// redelivery (or dead-letters it if it's out of attempts).
fn requeue_all(d: &Deliverer, flight: &mut InFlight, dead_log: &RateLimit) {
    for (_, mut item) in std::mem::take(&mut flight.items).into_iter().rev() {
        if item.event.attempt >= d.delivery.max_attempts {
            if dead_log.allow() {
                log::warn!(
                    "{} gave up on an event after {} attempts (connection lost each time)",
                    d.tag,
                    item.event.attempt
                );
            }
            d.queue.dead_letter(item);
            continue;
        }
        item.event.attempt += 1;
        d.queue.count_redelivered();
        d.queue.requeue(item);
    }
    d.queue.set_in_flight(0);
}

fn now() -> pbjson_types::Timestamp {
    let since = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    pbjson_types::Timestamp {
        seconds: i64::try_from(since.as_secs()).unwrap_or(i64::MAX),
        nanos: i32::try_from(since.subsec_nanos()).unwrap_or(0),
    }
}
