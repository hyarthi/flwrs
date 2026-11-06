//! One running scene: its nodes (supervised), their inbound queues, and the
//! routing between them.
//!
//! **Reload is a diff:** nodes whose plugin, type, config and
//! queue settings are unchanged keep running (process, queue and counters);
//! added nodes start, removed nodes stop, changed nodes restart. Edges,
//! mappings and the hop limit are swapped in each producer's routing table,
//! with no restart: every event is routed with one table, the old or the new.

use super::queue::{
    Budget, DropReason, NodeQueue, Pushed, QueueSettings, QueueStats, SpillSettings, SpillShared,
};
use crate::modules::hub::wire::{DataEvent, RuntimeMsg};
use crate::modules::hub::{Hub, HubError, LinkSender};
use crate::modules::mapping::{self, Mapping, MappingSpec, Schemas};
use crate::modules::node_config::WhenFull;
use crate::modules::plugin::manifest::PluginType;
use crate::modules::rate_limit::RateLimit;
use crate::modules::scene::graph::nodes_in_cycles;
use crate::modules::node_config::NodeConfig;
use crate::modules::scene::service::{Edge, SceneSnapshot};
use crate::modules::supervisor::{
    Launch, NodeHandle, NodeOutput, NodeSpec, NodeState, NodeStatus, supervise,
};
use flwrs_plugin::schema::common::EventMetadata;
use prost::Message;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::{mpsc, watch};
use tokio::task::{JoinHandle, JoinSet};
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
    /// budget, too large, evicted, disk errors; plus this node's events
    /// dropped for the hop limit.
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
    spill: bool,
}

/// A node of a checked graph.
struct PlannedNode {
    spec: NodeSpec,
    key: NodeKey,
    /// Inbound queue (sinks and transforms).
    queue: Option<QueueSettings>,
    spill: bool,
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
            match NodeSpec::resolve(
                scene_id,
                &node.id,
                &node.config,
                plugin,
                ctx.hub.local_addr(),
                &ctx.log_level,
            ) {
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
                let spill = queue.is_some()
                    && spec
                        .queue
                        .spill_enabled
                        .unwrap_or(ctx.spill.default_enabled);
                let key = NodeKey {
                    plugin_id: plugin.id.clone(),
                    plugin_version: plugin.version.clone(),
                    archive_sha256: plugin.archive_sha256.clone(),
                    install_path: plugin.install_path.clone(),
                    node_type: node.node_type.clone(),
                    config: node.config.clone(),
                    when_full: queue.as_ref().map(|q| q.when_full),
                    spill,
                };
                PlannedNode {
                    spec,
                    key,
                    queue,
                    spill,
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
            .filter_map(|e| {
                let target = self
                    .nodes
                    .iter()
                    .find(|n| n.spec.node_id == e.target_node_id)?;
                Some(Target {
                    edge_id: e.id.clone(),
                    node_id: e.target_node_id.clone(),
                    instance_id: target.spec.instance_id.clone(),
                    plugin_version: target.spec.plugin_version.clone(),
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
    deliverer: Option<JoinHandle<()>>,
    dropped_hops: Arc<AtomicU64>,
    mapping_errors: Arc<AtomicU64>,
    emitted: Arc<AtomicU64>,
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
    /// What the target plugin expects in `plugin_id` (its node id, or a
    /// remote node's `remote_id`).
    instance_id: String,
    plugin_version: String,
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
/// replaced node's old queue may still be closing.
fn new_queue(planned: &PlannedNode, scene_id: &str, ctx: &Context) -> Option<Arc<NodeQueue>> {
    let settings = planned.queue.clone()?;
    let spill = planned.spill.then(|| SpillSettings {
        spec: planned.spec.queue.spill.clone(),
        file: ctx.spill.scene_dir(scene_id).join(format!(
            "{}.{}.db",
            planned.spec.node_id,
            Ulid::generate()
        )),
        shared: ctx.spill.clone(),
    });
    Some(NodeQueue::new(settings, ctx.budget.clone(), spill))
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
    let router = tokio::spawn(route(Router {
        tag: spec.tag(),
        node_id: spec.node_id.clone(),
        is_source: spec.plugin_type == PluginType::Source,
        routes: routes_rx,
        dropped_hops: dropped_hops.clone(),
        mapping_errors: mapping_errors.clone(),
        emitted: emitted.clone(),
        outputs: rx,
    }));
    let deliverer = queue.as_ref().map(|queue| {
        tokio::spawn(deliver(
            spec.tag(),
            queue.clone(),
            handle.sender(),
            handle.watch(),
        ))
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
/// their way can still drain; then their routing and queues.
async fn stop_nodes(nodes: Vec<RunningNode>) {
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
            deliverer.abort();
        }
        // Spilled events are dropped with the node (at-most-once delivery).
        if let Some(queue) = queue {
            queue.finish().await;
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
        scene.apply(plan, ctx).await;
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
        let summary = self.apply(plan, ctx).await;
        log::info!(
            "[{}] scene reloaded: {summary} (max_hops {})",
            self.scene_id,
            self.max_hops
        );
        Ok(summary)
    }

    async fn apply(&mut self, plan: Plan, ctx: &Context) -> ReloadSummary {
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

        // New nodes (added or restarted) get new queues, so producers can
        // route to them right away; their events wait there until the new
        // plugin runs.
        let mut queues: HashMap<String, Arc<NodeQueue>> = kept
            .values()
            .filter_map(|n| Some((n.id.clone(), n.queue.clone()?)))
            .collect();
        for planned in plan.nodes.iter().filter(|p| !kept.contains_key(&p.spec.node_id)) {
            if let Some(queue) = new_queue(planned, &self.scene_id, ctx) {
                queues.insert(planned.spec.node_id.clone(), queue);
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
            let replaces_remote = old
                .get(&planned.spec.node_id)
                .is_some_and(|n| n.remote);
            if replaces_remote && matches!(planned.spec.launch, Launch::Remote { .. }) {
                start(planned, &mut started);
            }
        }
        stop_nodes(old.into_values().collect()).await;
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

    /// Stops every node (sources, then transforms, then sinks) and removes
    /// the scene's spool folder.
    pub(crate) async fn stop(self) {
        stop_nodes(self.nodes).await;
        if self.spool_dir.exists()
            && let Err(err) = tokio::fs::remove_dir_all(&self.spool_dir).await
        {
            log::warn!(
                "[{}] couldn't remove {}: {err}",
                self.scene_id,
                self.spool_dir.display()
            );
        }
        log::info!("[{}] scene stopped", self.scene_id);
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
}

/// Routes one producing node's events along its out-edges, filling in the
/// event metadata (the hub is its authority).
async fn route(mut r: Router) {
    let hop_log = RateLimit::new(DROP_LOG_INTERVAL);
    let drop_log = RateLimit::new(DROP_LOG_INTERVAL);
    let oversized_log = RateLimit::new(DROP_LOG_INTERVAL);
    let mapping_log = RateLimit::new(DROP_LOG_INTERVAL);
    while let Some(output) = r.outputs.recv().await {
        r.emitted.fetch_add(1, Ordering::Relaxed);
        // One table per event: the one in effect now (a reload swaps it).
        let routes = r.routes.borrow().clone();
        let mut meta = if r.is_source {
            EventMetadata::default()
        } else {
            output.event.metadata.clone().unwrap_or_default()
        };
        if meta.trace_id.is_empty() {
            // A new trace: from a source, or emitted by a transform on its own.
            meta.trace_id = Ulid::generate().to_string();
            meta.origin_node_id = r.node_id.clone();
            meta.create_time = Some(now());
            meta.hops = 0;
        }
        meta.hops = meta.hops.saturating_add(1);
        if meta.hops > routes.max_hops {
            r.dropped_hops.fetch_add(1, Ordering::Relaxed);
            if hop_log.allow() {
                log::error!(
                    "{} dropped an event of trace {} after {} hops (max_hops = {})",
                    r.tag,
                    meta.trace_id,
                    meta.hops - 1,
                    routes.max_hops
                );
            }
            continue;
        }
        for target in &routes.targets {
            let (payload, size) = match &target.mapping {
                None => (output.event.payload.clone(), output.size),
                Some(mapping) => {
                    let input = output.event.payload.clone().unwrap_or_default();
                    match mapping.apply(&input, Some(&meta)) {
                        // Mapped payloads are re-measured for the queue's limits.
                        Ok(Some(mapped)) => {
                            let size = mapped.encoded_len();
                            (Some(mapped), size)
                        }
                        Ok(None) => continue, // filtered out on this edge
                        Err(err) => {
                            r.mapping_errors.fetch_add(1, Ordering::Relaxed);
                            if mapping_log.allow() {
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
                instance_id: target.instance_id.clone(),
                plugin_version: target.plugin_version.clone(),
                payload,
                metadata: Some(EventMetadata {
                    event_id: Ulid::generate().to_string(),
                    ..meta.clone()
                }),
            };
            match target.queue.push(event, size).await {
                Pushed::Dropped(DropReason::Oversized) if oversized_log.allow() => log::warn!(
                    "{} dropped an event of {} bytes for node {}: larger than the hub's memory limit ([hub] max_inflight_bytes)",
                    r.tag,
                    size,
                    target.node_id
                ),
                Pushed::Dropped(reason)
                    if reason != DropReason::Closed
                        && reason != DropReason::Oversized
                        && drop_log.allow() =>
                {
                    log::warn!(
                        "{} dropped an event for node {}: {reason:?}",
                        r.tag,
                        target.node_id
                    );
                }
                _ => {}
            }
        }
    }
}

/// Hands a node's queued events to its plugin, waiting while it isn't running.
async fn deliver(
    tag: String,
    queue: Arc<NodeQueue>,
    sender: LinkSender,
    mut status: watch::Receiver<NodeStatus>,
) {
    loop {
        if status
            .wait_for(|s| s.state == NodeState::Running)
            .await
            .is_err()
        {
            return; // the node is gone
        }
        let Some(item) = queue.pop().await else {
            return; // closed
        };
        match sender.send(RuntimeMsg::Event(item.event.clone())).await {
            Ok(()) => queue.delivered(),
            Err(HubError::NotConnected) => {
                // It went away meanwhile: keep the event, wait for a change.
                queue.requeue(item);
                let _ = tokio::time::timeout(Duration::from_millis(100), status.changed()).await;
            }
            Err(err) => log::error!("{tag} couldn't deliver an event: {err}"),
        }
    }
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
