//! A checked graph compiled into a fixed plan (every model loaded), and its
//! run on a frame.
//!
//! A tick runs every once-per-frame node in order, then each crop node's
//! per-region nodes once per region. Results go with the object they're
//! about: a model run on a region attaches its result to the region's
//! object (`children`); a model on keypoints (`tensor_input`) runs once per
//! object of its input and attaches each result to that object. A node that
//! fails on a frame gives nothing of its type for it, and the tick reports
//! why; the other nodes run on.
//!
//! Execution groups change how the for-each runs (one by one, across
//! threads, or as a batch), never what it gives: items are put back in their
//! order whichever finishes first.

use super::config::{ControlConfig, Execution, GraphConfig, NodeConfig, Sampling, TensorInput};
use super::crop::{self, Region};
use super::{Checked, PortType, Ports, groups};
use crate::analytics::control::Detector;
use crate::analytics::rules::{RuleEngine, RuleEvent, RuleInputs, ended_ids};
use crate::analytics::track::{TrackEvent, TrackInfo, Tracker};
use crate::frame::{Affine, Frame, Rect};
use crate::model::{Model, Timing};
use crate::pre;
use crate::results::{Object, Results};
use crate::tensor::Tensor;
use flwrs_types::groups::{Execution as GroupExecution, Mode};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// The most sessions of one model running a for-each side by side.
const MAX_WORKERS: usize = 8;
/// The most items in one batched run.
const MAX_BATCH: usize = 16;
/// What measuring thread counts at start may take in all.
const MEASURE_BUDGET: Duration = Duration::from_millis(300);
/// Larger models aren't measured: loading them again would slow the start.
const MEASURE_MAX_BYTES: u64 = 16 << 20;
/// Timed runs per thread count.
const MEASURE_RUNS: u32 = 3;

enum NodeType {
    Source,
    Model {
        model: Box<Model>,
        tensor: Option<TensorInput>,
        /// `every_n_frames`, for a once-per-frame model.
        every: u32,
        last: Option<Results>,
        per_track: Option<PerTrack>,
        concurrency: Concurrency,
    },
    Crop(super::config::CropConfig),
    Filter(super::config::FilterConfig),
    /// A model that runs on control frames only.
    Detector(Box<Detector>),
    Tracker {
        tracker: Box<Tracker>,
        /// The detector node, with control frames.
        detector: Option<usize>,
    },
    Rule {
        engine: Box<RuleEngine>,
        /// The tracker its objects come from.
        tracker: Option<String>,
    },
    /// Notes the objects that leave the plugin.
    Emit,
    #[cfg(feature = "lua")]
    Lua {
        rule: Box<crate::analytics::script::LuaRule>,
        tracker: Option<String>,
    },
}

/// How a model runs its for-each items.
struct Concurrency {
    mode: Mode,
    /// With `parallel`: one session per worker, each with its share of the
    /// group's threads. The model's own session has them all, for a frame
    /// with a single item.
    replicas: Vec<Model>,
}

/// One item of a for-each.
enum Unit {
    Frame(Rect),
    Object(Object),
}

/// An item's outcome.
struct UnitOut {
    results: Results,
    error: Option<String>,
    ran: bool,
    timing: Timing,
}

/// Which models may reuse their last results when a frame runs late: those
/// with a sampling policy, in a group below the highest priority.
struct Priority {
    /// Node → how many due runs in a row it may skip.
    may_skip: BTreeMap<usize, u32>,
    skipped: BTreeMap<usize, u32>,
    yielded: BTreeSet<usize>,
    started: Instant,
    /// The time between the last two frames on the stream's clock.
    budget: Option<Duration>,
    last_time: Option<f64>,
}

impl Priority {
    fn start(&mut self, time: f64) {
        self.started = Instant::now();
        self.yielded.clear();
        self.budget = self
            .last_time
            .map(|last| time - last)
            .filter(|gap| *gap > 0.0 && gap.is_finite())
            .map(Duration::from_secs_f64);
        self.last_time = Some(time);
    }

    fn over(&self, i: usize) -> bool {
        self.may_skip.get(&i).is_some_and(|limit| {
            self.skipped.get(&i).copied().unwrap_or(0) < *limit
                && self.budget.is_some_and(|b| self.started.elapsed() > b)
        })
    }
}

/// `sampling.per_track`: a model's last result per track.
#[derive(Default)]
struct PerTrack {
    /// Run again after this many frames (`None`: once per track).
    refresh: Option<u32>,
    results: BTreeMap<u64, (Results, u64)>,
}

struct Node {
    id: String,
    node_type: NodeType,
    /// Input port → the node it's linked from.
    inputs: BTreeMap<&'static str, usize>,
    output: PortType,
}

/// What flows between nodes during a tick.
#[derive(Clone)]
enum Value {
    /// The frame, or a region of it.
    Frame(Rect),
    Regions(Vec<Region>),
    Results(Results),
    /// A rule's output: its events are in the tick.
    Events,
}

/// A node's time over a tick.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct NodeTiming {
    /// How many times it ran (per region, per object), 0 when it reused its
    /// last results (sampling).
    pub runs: u32,
    pub time: Timing,
}

/// A tick's outcome.
#[derive(Debug, Default)]
pub struct Tick {
    /// The once-per-frame model and filter nodes' results, in run order;
    /// what ran on their objects is in the objects' `children`.
    pub results: Vec<(String, Results)>,
    /// Nodes that failed on this frame (they gave nothing).
    pub errors: Vec<String>,
    pub timing: BTreeMap<String, NodeTiming>,
    /// Once-per-frame models: their result's pixels → frame pixels (what
    /// places a map on the frame; a map may be smaller than the input).
    pub affines: BTreeMap<String, Affine>,
    /// Every confirmed track, by tracker node.
    pub tracks: BTreeMap<String, Vec<TrackInfo>>,
    /// Tracks that started or ended on this frame, by tracker node.
    pub track_events: BTreeMap<String, Vec<TrackEvent>>,
    /// What the rules saw happen on this frame.
    pub events: Vec<RuleEvent>,
    /// What an `emit` node's objects are: their node and them.
    pub emit_objects: Option<(String, Vec<crate::results::Object>)>,
    /// Models that reused their last results because the frame ran late
    /// (`priority` of their execution group).
    pub yielded: Vec<String>,
}

/// A graph ready to run: every model loaded.
pub struct Plan {
    nodes: Vec<Node>,
    /// Once-per-frame nodes, in order.
    top: Vec<usize>,
    /// Per crop node: its per-region nodes, in order.
    scopes: Vec<(usize, Vec<usize>)>,
    /// Rules, after the per-region nodes: they see what ran on the objects.
    rules: Vec<usize>,
    /// Nodes whose objects a tracker passes on: only its output is reported.
    tracked: BTreeSet<usize>,
    seq: u64,
    /// The stream's clock for this tick, seconds.
    time: f64,
    /// Detectors that finished a run this tick: the time of its frame.
    fresh: BTreeMap<usize, f64>,
    priority: Option<Priority>,
    deterministic: bool,
}

/// Loading problems (the plan isn't built) or the plan and its warnings
/// (models that fell back to the CPU).
pub type Compiled = Result<(Plan, Vec<String>), Vec<String>>;

/// The keypoints an object feeding `node` has, where the settings say: a
/// `tensor_input` model's input size.
fn keypoints_from(
    graph: &GraphConfig,
    links: &BTreeMap<String, String>,
    node: &str,
) -> Option<usize> {
    let from = links.get(&format!("{node}.objects"))?;
    let (src, _) = from.split_once('.')?;
    match graph.nodes.get(src)? {
        NodeConfig::Model(m) => m.decode.keypoint_count(),
        NodeConfig::Filter(_) => keypoints_from(graph, links, src),
        _ => None,
    }
}

/// Whether `node`'s objects come from a hand landmarks model that reports
/// what a gesture embedder takes.
fn feeds_hand_pose(graph: &GraphConfig, links: &BTreeMap<String, String>, node: &str) -> bool {
    let Some((src, _)) = links
        .get(&format!("{node}.objects"))
        .and_then(|f| f.split_once('.'))
    else {
        return false;
    };
    match graph.nodes.get(src) {
        Some(NodeConfig::Model(m)) => matches!(
            m.decode,
            crate::decode::Decoder::Landmarks {
                points: 21,
                world: Some(_),
                handedness: Some(_),
                ..
            }
        ),
        Some(NodeConfig::Filter(_)) => feeds_hand_pose(graph, links, src),
        _ => false,
    }
}

/// The tracker and the model the objects reaching `node` come from
/// (through trackers and filters).
fn origin<'a>(
    graph: &'a GraphConfig,
    links: &BTreeMap<String, String>,
    node: &str,
) -> (Option<String>, Option<&'a super::config::ModelConfig>) {
    let (mut tracker, mut at) = (None, node.to_string());
    for _ in 0..=graph.nodes.len() {
        let Some((src, _)) = links
            .get(&format!("{at}.objects"))
            .and_then(|f| f.split_once('.'))
        else {
            break;
        };
        match graph.nodes.get(src) {
            Some(NodeConfig::Tracker(_)) => {
                tracker.get_or_insert_with(|| src.to_string());
            }
            Some(NodeConfig::Filter(_)) => {}
            Some(NodeConfig::Model(m)) => return (tracker, Some(m)),
            _ => break,
        }
        at = src.to_string();
    }
    (tracker, None)
}

/// Every model node's keypoint layout, by node: names the keypoints of what
/// a model found, for Lua rules.
#[cfg(feature = "lua")]
fn keypoint_layouts(graph: &GraphConfig) -> BTreeMap<String, crate::keypoints::KeypointLayout> {
    graph
        .nodes
        .iter()
        .filter_map(|(id, n)| match n {
            NodeConfig::Model(m) => Some((
                id.clone(),
                crate::keypoints::layout(m.keypoint_layout.as_deref()?)?,
            )),
            _ => None,
        })
        .collect()
}

/// A graph's Lua rule node on its own, set up as [`compile`] sets it up
/// (keypoint names from the models feeding it), with the tracker it reads,
/// if any. Loads no model: for running a rule on recorded or made-up
/// inputs (`flwrs-cv-source rule-test`).
#[cfg(feature = "lua")]
pub fn lua_rule(
    graph: &GraphConfig,
    node: &str,
) -> Result<(crate::analytics::script::LuaRule, Option<String>), String> {
    let Some(NodeConfig::Lua(config)) = graph.nodes.get(node) else {
        return Err(format!("the graph has no lua node {node:?}"));
    };
    let links: BTreeMap<String, String> = graph
        .links
        .iter()
        .map(|l| (l.to.clone(), l.from.clone()))
        .collect();
    let (tracker, model) = origin(graph, &links, node);
    let layout = model
        .and_then(|m| m.keypoint_layout.as_deref())
        .and_then(crate::keypoints::layout);
    let rule =
        crate::analytics::script::LuaRule::new(node, config, layout, keypoint_layouts(graph))?;
    Ok((rule, tracker))
}

/// The model feeding `node`'s objects, through filters.
fn origin_model<'a>(
    graph: &'a GraphConfig,
    links: &BTreeMap<String, String>,
    node: &str,
) -> Option<(String, &'a super::config::ModelConfig)> {
    let mut at = node.to_string();
    for _ in 0..=graph.nodes.len() {
        let (src, _) = links.get(&format!("{at}.objects"))?.split_once('.')?;
        match graph.nodes.get(src)? {
            NodeConfig::Filter(_) => at = src.to_string(),
            NodeConfig::Model(m) => return Some((src.to_string(), m)),
            _ => return None,
        }
    }
    None
}

/// How a model's session and for-each are set up.
struct Launch {
    /// The model's own session.
    intra: usize,
    /// With `parallel`: workers, and each one's session threads.
    workers: usize,
    worker_intra: usize,
    mode: Mode,
}

/// What a model's group, its for-each and the core budget make of it.
/// `implicit`: the threads it gets without a group's say.
fn launch_of(
    group: Option<&GroupExecution>,
    measured: Option<usize>,
    implicit: usize,
    items: Option<usize>,
    allowance: usize,
    limit: Option<usize>,
) -> Launch {
    let mut launch = Launch {
        intra: implicit,
        workers: 1,
        worker_intra: implicit,
        mode: Mode::Sequential,
    };
    if let Some(e) = group {
        let threads = e.threads.map(|t| t as usize).or(measured);
        match (e.mode.unwrap_or(Mode::Sequential), items) {
            (Mode::Parallel, Some(items)) => {
                let total = threads.unwrap_or(allowance).max(1);
                launch.workers = total.min(items).clamp(1, MAX_WORKERS);
                launch.intra = total;
                launch.worker_intra = (total / launch.workers).max(1);
                launch.mode = Mode::Parallel;
            }
            (mode, items) => {
                launch.intra = threads.unwrap_or(implicit);
                if mode == Mode::Batched && items.is_some() {
                    launch.mode = Mode::Batched;
                }
            }
        }
    }
    if let Some(limit) = limit {
        launch.workers = launch.workers.min(limit.max(1));
        let cap = |threads: usize, most: usize| {
            if threads == 0 {
                most
            } else {
                threads.min(most)
            }
        };
        launch.intra = cap(launch.intra, limit.max(1));
        launch.worker_intra = cap(launch.worker_intra, (limit / launch.workers).max(1));
    }
    launch
}

/// Why a model can't run its items as one batch.
fn batch_problem(model: &mut Model, path: Option<&PathBuf>) -> Option<String> {
    if model.session().runtime() == crate::backend::Runtime::Tract {
        return Some("tract compiles a model for one input shape".into());
    }
    let info = match path.map(|p| crate::inspect::inspect(p)) {
        Some(Ok(info)) => info,
        Some(Err(e)) => return Some(e),
        None => return Some("its file isn't there".into()),
    };
    let input = match &model.config().input_name {
        Some(name) => info.inputs.iter().find(|p| &p.name == name),
        None => info.inputs.first(),
    };
    if !input.is_some_and(|p| p.shape.first() == Some(&None)) {
        return Some("its input has a fixed batch size".into());
    }
    model.batch_check().err()
}

/// The fastest thread count for an execution group left without one: its
/// largest model, timed on a few runs. Bounded by `spent`.
fn measure_group(
    graph: &GraphConfig,
    group: &str,
    files: &BTreeMap<String, PathBuf>,
    defaults: &Execution,
    allowance: usize,
    spent: &mut Duration,
) -> Option<usize> {
    let candidates: Vec<usize> = [1, 2, 4].into_iter().filter(|c| *c <= allowance).collect();
    if candidates.len() < 2 {
        return None;
    }
    let size = |p: &Path| std::fs::metadata(p).map_or(u64::MAX, |m| m.len());
    let (_, config) = graph
        .nodes
        .iter()
        .filter_map(|(id, n)| match n {
            NodeConfig::Model(m)
                if m.pre.is_some()
                    && groups::execution_of(graph, id).is_some_and(|(g, _)| g == group) =>
            {
                Some((size(files.get(&m.model)?), m))
            }
            _ => None,
        })
        .filter(|(bytes, _)| *bytes <= MEASURE_MAX_BYTES)
        .max_by_key(|(bytes, _)| *bytes)?;
    let mut timed: Vec<(usize, Duration)> = vec![];
    for threads in candidates {
        if *spent >= MEASURE_BUDGET {
            break;
        }
        let started = Instant::now();
        let time = Model::load(config, files, defaults, threads, None)
            .and_then(|(mut m, _)| m.timed_runs(MEASURE_RUNS));
        *spent += started.elapsed();
        timed.push((threads, time.ok()?));
    }
    let best = timed.iter().map(|(_, t)| *t).min()?;
    // Fewer threads unless more are clearly faster.
    let (threads, time) = timed
        .iter()
        .find(|(_, t)| t.as_secs_f64() <= best.as_secs_f64() * 1.1)?;
    log::info!(
        "group {group}: measured {threads} threads for {} ({:.1} ms a run)",
        config.model,
        time.as_secs_f64() * 1e3
    );
    Some(*threads)
}

/// Builds the plan for a checked graph: every model loaded on its device
/// (with `files`, the node's files by name). Every problem is reported.
/// The execution groups' threads must fit `threads` when it's set.
pub fn compile(
    graph: &GraphConfig,
    checked: &Checked,
    files: &BTreeMap<String, PathBuf>,
    defaults: &Execution,
    threads: usize,
) -> Compiled {
    if let Some(problem) = groups::budget_problem(graph, threads) {
        return Err(vec![problem]);
    }
    compile_with(graph, checked, files, defaults, threads, true)
}

/// As `compile`, to see whether the models load, where the node's real core
/// count isn't known: no thread budget, no measuring, no extra sessions.
pub fn compile_probe(
    graph: &GraphConfig,
    checked: &Checked,
    files: &BTreeMap<String, PathBuf>,
    defaults: &Execution,
    threads: usize,
) -> Compiled {
    compile_with(graph, checked, files, defaults, threads, false)
}

fn compile_with(
    graph: &GraphConfig,
    checked: &Checked,
    files: &BTreeMap<String, PathBuf>,
    defaults: &Execution,
    threads: usize,
    full: bool,
) -> Compiled {
    let links: BTreeMap<String, String> = graph
        .links
        .iter()
        .map(|l| (l.to.clone(), l.from.clone()))
        .collect();
    let index: BTreeMap<&str, usize> = checked
        .order
        .iter()
        .enumerate()
        .map(|(i, id)| (id.as_str(), i))
        .collect();
    let (mut nodes, mut problems, mut warnings) = (vec![], vec![], vec![]);
    // The models that detect for a tracker with control frames.
    let mut controlled: BTreeMap<String, (ControlConfig, u32)> = BTreeMap::new();
    for (id, node) in &graph.nodes {
        if let NodeConfig::Tracker(t) = node
            && let Some(control) = t.control
            && let Some((model, _)) = origin_model(graph, &links, id)
        {
            controlled.insert(model, (control, t.min_hits));
        }
    }
    let cores = if threads > 0 {
        threads
    } else {
        std::thread::available_parallelism().map_or(1, |n| n.get())
    };
    let wants_background = controlled.values().any(|(c, _)| c.background);
    let background = wants_background && cores >= 2;
    if wants_background && !background {
        warnings.push(
            "control.background needs at least 2 cores: the detector runs on the frame loop".into(),
        );
    }
    // The detector runs on few frames, so the frame loop's models keep every
    // core, and the detector overlaps them on half.
    let (frame_threads, detector_threads) = if background {
        (cores, cores / 2)
    } else {
        (threads, threads)
    };
    // What an execution group that leaves `threads` open may use.
    let allowance = {
        let open = groups::open_groups(graph).max(1) as u64;
        let left = (cores as u64).saturating_sub(groups::thread_sum(graph));
        ((left / open) as usize).max(1)
    };
    if full && threads == 0 && groups::thread_sum(graph) > cores as u64 {
        warnings.push(format!(
            "the execution groups' threads add up to {}, more than this machine's {cores} cores",
            groups::thread_sum(graph)
        ));
    }
    let mut measured: BTreeMap<&str, Option<usize>> = BTreeMap::new();
    let mut measuring = Duration::ZERO;
    #[cfg(feature = "lua")]
    let layouts = keypoint_layouts(graph);
    for id in &checked.order {
        let config = &graph.nodes[id];
        let ports = Ports::of(config);
        let inputs = ports
            .inputs
            .iter()
            .filter_map(|(name, _)| {
                let from = links.get(&format!("{id}.{name}"))?;
                Some((*name, index[from.split_once('.')?.0]))
            })
            .collect();
        let node_type = match config {
            NodeConfig::Source {} => NodeType::Source,
            NodeConfig::Crop(c) => NodeType::Crop(c.clone()),
            NodeConfig::Filter(f) => NodeType::Filter(f.clone()),
            NodeConfig::Tracker(t) => NodeType::Tracker {
                tracker: Box::new(Tracker::new(t.clone())),
                detector: t
                    .control
                    .and_then(|_| origin_model(graph, &links, id))
                    .map(|(model, _)| index[model.as_str()]),
            },
            NodeConfig::Rule(r) => NodeType::Rule {
                engine: Box::new(RuleEngine::new(id, (**r).clone())),
                tracker: origin(graph, &links, id).0,
            },
            NodeConfig::Emit(_) => NodeType::Emit,
            #[cfg(feature = "lua")]
            NodeConfig::Lua(l) => {
                let (tracker, model) = origin(graph, &links, id);
                let layout = model
                    .and_then(|m| m.keypoint_layout.as_deref())
                    .and_then(crate::keypoints::layout);
                match crate::analytics::script::LuaRule::new(id, l, layout, layouts.clone()) {
                    Ok(rule) => NodeType::Lua {
                        rule: Box::new(rule),
                        tracker,
                    },
                    Err(e) => {
                        problems.push(format!("nodes.{id}: {e}"));
                        continue;
                    }
                }
            }
            #[cfg(not(feature = "lua"))]
            NodeConfig::Lua(_) => {
                problems.push(format!("nodes.{id}: this build has no Lua (feature lua)"));
                continue;
            }
            NodeConfig::Model(m) => {
                if m.tensor_input
                    .as_ref()
                    .is_some_and(|t| t.embedder.is_some())
                    && !feeds_hand_pose(graph, &links, id)
                {
                    problems.push(format!(
                        "nodes.{id}: tensor_input.embedder needs its objects from a landmarks decoder with 21 points, world and handedness"
                    ));
                    continue;
                }
                let tensor_len = m.tensor_input.as_ref().and_then(|t| {
                    let n = t
                        .keypoints
                        .as_ref()
                        .map(Vec::len)
                        .or_else(|| keypoints_from(graph, &links, id))?;
                    Some(n * t.dims)
                });
                let control = controlled.get(id);
                let implicit = if control.is_some() {
                    detector_threads
                } else {
                    frame_threads
                };
                let group = groups::execution_of(graph, id);
                let group_defaults = group.map(|(_, e)| groups::over(e, defaults));
                let model_defaults = group_defaults.as_ref().unwrap_or(defaults);
                // How many items its for-each can have, if it has one.
                let items = match checked.scope[id].as_ref().map(|c| &graph.nodes[c]) {
                    Some(NodeConfig::Crop(c)) => Some(c.max),
                    _ => m.tensor_input.as_ref().map(|_| MAX_WORKERS),
                };
                let in_parallel =
                    group.is_some_and(|(_, e)| e.mode == Some(Mode::Parallel) && items.is_some());
                let measured_threads = match group {
                    Some((name, e)) if full && e.threads.is_none() && !in_parallel => {
                        *measured.entry(name).or_insert_with(|| {
                            measure_group(
                                graph,
                                name,
                                files,
                                model_defaults,
                                allowance,
                                &mut measuring,
                            )
                        })
                    }
                    _ => None,
                };
                let launch = launch_of(
                    group.map(|(_, e)| e),
                    measured_threads,
                    implicit,
                    items,
                    allowance,
                    groups::cpu_limit(graph, id),
                );
                match Model::load(m, files, model_defaults, launch.intra, tensor_len) {
                    Ok((mut model, warning)) => {
                        warnings.extend(warning.map(|w| format!("nodes.{id}: {w}")));
                        let mut mode = launch.mode;
                        if mode == Mode::Batched
                            && let Some(why) = batch_problem(&mut model, files.get(&m.model))
                        {
                            warnings.push(format!(
                                "nodes.{id}: batched mode isn't possible ({why}): it runs its items one by one"
                            ));
                            mode = Mode::Sequential;
                        }
                        let mut replicas = vec![];
                        if full && mode == Mode::Parallel && launch.workers > 1 {
                            for _ in 0..launch.workers {
                                match Model::load(
                                    m,
                                    files,
                                    model_defaults,
                                    launch.worker_intra,
                                    tensor_len,
                                ) {
                                    Ok((replica, _)) => replicas.push(replica),
                                    Err(e) => {
                                        warnings.push(format!(
                                            "nodes.{id}: {} workers instead of {}: {e}",
                                            replicas.len(),
                                            launch.workers
                                        ));
                                        break;
                                    }
                                }
                            }
                        }
                        if let Some((control, hits)) = control {
                            let runs_apart = background && control.background;
                            match Detector::new(Box::new(model), control, *hits, runs_apart) {
                                Ok(d) => {
                                    let ports = Ports::of(config);
                                    let inputs = ports
                                        .inputs
                                        .iter()
                                        .filter_map(|(name, _)| {
                                            let from = links.get(&format!("{id}.{name}"))?;
                                            Some((*name, index[from.split_once('.')?.0]))
                                        })
                                        .collect();
                                    nodes.push(Node {
                                        id: id.clone(),
                                        node_type: NodeType::Detector(Box::new(d)),
                                        inputs,
                                        output: ports
                                            .outputs
                                            .first()
                                            .map_or(PortType::Objects, |(_, t)| *t),
                                    });
                                }
                                Err(e) => problems.push(format!("nodes.{id}: {e}")),
                            }
                            continue;
                        }
                        let every = match m.sampling {
                            Some(Sampling::EveryNFrames(n)) => n.max(1),
                            _ => 1,
                        };
                        NodeType::Model {
                            model: Box::new(model),
                            tensor: m.tensor_input.clone(),
                            every,
                            last: None,
                            per_track: match m.sampling {
                                Some(Sampling::PerTrack { refresh_frames }) => Some(PerTrack {
                                    refresh: refresh_frames,
                                    results: BTreeMap::new(),
                                }),
                                _ => None,
                            },
                            concurrency: Concurrency { mode, replicas },
                        }
                    }
                    Err(e) => {
                        problems.push(format!("nodes.{id}: {e}"));
                        continue;
                    }
                }
            }
        };
        let output = ports
            .outputs
            .first()
            .map(|(_, t)| *t)
            .unwrap_or(PortType::Frame);
        nodes.push(Node {
            id: id.clone(),
            node_type,
            inputs,
            output,
        });
    }
    if !problems.is_empty() {
        return Err(problems);
    }
    let scope_of = |i: usize| {
        checked.scope[&nodes[i].id]
            .as_ref()
            .map(|c| index[c.as_str()])
    };
    let is_rule = |i: usize| match &nodes[i].node_type {
        NodeType::Rule { .. } | NodeType::Emit => true,
        #[cfg(feature = "lua")]
        NodeType::Lua { .. } => true,
        _ => false,
    };
    let top = (0..nodes.len())
        .filter(|&i| scope_of(i).is_none() && !is_rule(i))
        .collect();
    let rules = (0..nodes.len()).filter(|&i| is_rule(i)).collect();
    let tracked = nodes
        .iter()
        .filter(|n| matches!(n.node_type, NodeType::Tracker { .. }))
        .flat_map(|n| {
            // Its input, up through filters to the model.
            let mut chain = vec![];
            let mut at = n.inputs.get("objects").copied();
            while let Some(i) = at {
                chain.push(i);
                at = match nodes[i].node_type {
                    NodeType::Filter(_) => nodes[i].inputs.get("objects").copied(),
                    _ => None,
                };
            }
            chain
        })
        .collect();
    let mut scopes: Vec<(usize, Vec<usize>)> = vec![];
    for i in 0..nodes.len() {
        if let Some(c) = scope_of(i) {
            match scopes.iter_mut().find(|(s, _)| *s == c) {
                Some((_, members)) => members.push(i),
                None => scopes.push((c, vec![i])),
            }
        }
    }
    scopes.sort_by_key(|(c, _)| *c);
    let priority = priority_of(graph, &nodes);
    Ok((
        Plan {
            nodes,
            top,
            scopes,
            rules,
            tracked,
            seq: 0,
            time: 0.0,
            fresh: BTreeMap::new(),
            priority,
            deterministic: false,
        },
        warnings,
    ))
}

/// The models that may skip a due run when a frame runs late; none when no
/// group sets a priority, or all are at the same one.
fn priority_of(graph: &GraphConfig, nodes: &[Node]) -> Option<Priority> {
    let of = |id: &str| groups::execution_of(graph, id).map(|(_, e)| e.priority);
    let set: Vec<i32> = graph
        .nodes
        .keys()
        .filter_map(|id| of(id).flatten())
        .collect();
    let top = set.iter().copied().max()?;
    let mut may_skip = BTreeMap::new();
    for (i, n) in nodes.iter().enumerate() {
        let NodeType::Model {
            every, per_track, ..
        } = &n.node_type
        else {
            continue;
        };
        let Some(priority) = of(&n.id) else {
            continue;
        };
        if priority.unwrap_or(0) >= top {
            continue;
        }
        let limit = match per_track {
            Some(p) => p.refresh.unwrap_or(0),
            None if *every > 1 => 1,
            None => 0,
        };
        if limit > 0 {
            may_skip.insert(i, limit);
        }
    }
    (!may_skip.is_empty()).then(|| Priority {
        may_skip,
        skipped: BTreeMap::new(),
        yielded: BTreeSet::new(),
        started: Instant::now(),
        budget: None,
        last_time: None,
    })
}

/// One item on one session.
fn run_one(
    model: &mut Model,
    tensor: Option<&TensorInput>,
    unit: &Unit,
    frame: &Frame,
    output: PortType,
) -> UnitOut {
    let mut out = UnitOut {
        results: Results::empty(output),
        error: None,
        ran: false,
        timing: Timing::default(),
    };
    let run = match (unit, tensor) {
        (Unit::Frame(rect), _) => model.run(frame, *rect).map(Some),
        (Unit::Object(o), Some(t)) => {
            let started = Instant::now();
            let prepared = model.object_input(t, o, frame.width(), frame.height());
            out.timing.pre += started.elapsed();
            prepared.and_then(|input| match input {
                Some(input) => model.infer(input, 1.0, 1.0).map(Some),
                None => Ok(None),
            })
        }
        (Unit::Object(_), None) => Ok(None),
    };
    match run {
        Ok(Some(run)) => {
            out.ran = true;
            add(&mut out.timing, &run.timing);
            out.results = run.results;
        }
        Ok(None) => {}
        Err(e) => out.error = Some(e),
    }
    out
}

/// Every item across the sessions: each worker takes the next one, and the
/// outcomes are put back in the items' order.
fn run_parallel(
    replicas: &mut [Model],
    tensor: Option<&TensorInput>,
    units: &[Unit],
    frame: &Frame,
    output: PortType,
) -> Vec<UnitOut> {
    let next = AtomicUsize::new(0);
    let slots: Vec<Mutex<Option<UnitOut>>> = units.iter().map(|_| Mutex::new(None)).collect();
    std::thread::scope(|scope| {
        for session in replicas.iter_mut().take(units.len()) {
            let (next, slots) = (&next, &slots);
            scope.spawn(move || {
                loop {
                    let k = next.fetch_add(1, Ordering::Relaxed);
                    let Some(unit) = units.get(k) else {
                        break;
                    };
                    let out = run_one(session, tensor, unit, frame, output);
                    *slots[k].lock().unwrap_or_else(|e| e.into_inner()) = Some(out);
                }
            });
        }
    });
    slots
        .into_iter()
        .map(|slot| {
            slot.into_inner()
                .unwrap_or_else(|e| e.into_inner())
                .unwrap_or_else(|| UnitOut {
                    results: Results::empty(output),
                    error: Some("a worker stopped before this item".into()),
                    ran: false,
                    timing: Timing::default(),
                })
        })
        .collect()
}

/// Items in batches of at most `MAX_BATCH`. A batch that fails runs one by
/// one, so an item's result doesn't depend on its neighbours.
fn run_batched(
    model: &mut Model,
    tensor: Option<&TensorInput>,
    units: &[Unit],
    frame: &Frame,
    output: PortType,
) -> Vec<UnitOut> {
    let mut outs: Vec<UnitOut> = units
        .iter()
        .map(|_| UnitOut {
            results: Results::empty(output),
            error: None,
            ran: false,
            timing: Timing::default(),
        })
        .collect();
    let settings = model.config().pre.clone();
    let (mut inputs, mut meta): (Vec<Tensor>, Vec<(usize, Option<Affine>)>) = (vec![], vec![]);
    for (k, unit) in units.iter().enumerate() {
        let started = Instant::now();
        let prepared = match (unit, tensor, &settings) {
            (Unit::Frame(rect), _, Some(s)) => {
                pre::prepare(frame, *rect, s).map(|p| Some((p.tensor, Some(p.affine))))
            }
            (Unit::Object(o), Some(t), _) => model
                .object_input(t, o, frame.width(), frame.height())
                .map(|i| i.map(|i| (i, None))),
            _ => Ok(None),
        };
        outs[k].timing.pre = started.elapsed();
        match prepared {
            Ok(Some((input, affine))) => {
                inputs.push(input);
                meta.push((k, affine));
            }
            Ok(None) => {}
            Err(e) => outs[k].error = Some(e),
        }
    }
    let (width, height) = settings
        .as_ref()
        .map_or((1.0, 1.0), |s| (s.width as f32, s.height as f32));
    for (chunk, owners) in inputs.chunks(MAX_BATCH).zip(meta.chunks(MAX_BATCH)) {
        let batch = if chunk.len() > 1 {
            model.infer_batch(chunk, width, height).ok()
        } else {
            None
        };
        let runs: Vec<Result<crate::model::Run, String>> = match batch {
            Some(runs) => runs.into_iter().map(Ok).collect(),
            None => chunk
                .iter()
                .map(|input| model.infer(input.clone(), width, height))
                .collect(),
        };
        for (run, (k, affine)) in runs.into_iter().zip(owners) {
            let out = &mut outs[*k];
            match run {
                Ok(mut run) => {
                    if let Some(affine) = affine {
                        run.results.map_to_frame(affine);
                    }
                    out.ran = true;
                    add(&mut out.timing, &run.timing);
                    out.results = run.results;
                }
                Err(e) => out.error = Some(e),
            }
        }
    }
    outs
}

/// A model's for-each items, by its group's mode.
fn run_units(
    model: &mut Model,
    concurrency: &mut Concurrency,
    tensor: Option<&TensorInput>,
    units: &[Unit],
    frame: &Frame,
    output: PortType,
) -> Vec<UnitOut> {
    match concurrency.mode {
        Mode::Parallel if units.len() > 1 && concurrency.replicas.len() > 1 => {
            run_parallel(&mut concurrency.replicas, tensor, units, frame, output)
        }
        Mode::Batched if units.len() > 1 => run_batched(model, tensor, units, frame, output),
        _ => units
            .iter()
            .map(|u| run_one(model, tensor, u, frame, output))
            .collect(),
    }
}

/// A for-each scope run member by member, in a concurrent mode.
struct ScopeRun<'a> {
    crop: usize,
    regions: &'a [Region],
    tracks: Vec<Option<u64>>,
    frame: &'a Frame,
    seq: u64,
    /// Per region: its members' values.
    items: Vec<BTreeMap<usize, Value>>,
    /// Per region, per member: its errors, reported in the sequential order.
    errors: Vec<Vec<Vec<String>>>,
}

/// The region being run: its crop node, rectangle, values and track.
struct ActiveRegion<'a> {
    crop: usize,
    rect: Rect,
    item: &'a mut BTreeMap<usize, Value>,
    track: Option<u64>,
}

/// Values during a tick: the frame's, and a region's over them.
struct Values<'a> {
    base: &'a mut [Option<Value>],
    region: Option<ActiveRegion<'a>>,
}

impl Values<'_> {
    fn get(&self, i: usize) -> Option<&Value> {
        if let Some(region) = &self.region
            && let Some(v) = region.item.get(&i)
        {
            return Some(v);
        }
        self.base[i].as_ref()
    }

    /// The frame region a `frame` input linked from node `i` gets: in a
    /// crop's scope, the crop's regions give the current region.
    fn frame_rect(&self, i: usize) -> Option<Rect> {
        if let Some(region) = &self.region
            && i == region.crop
        {
            return Some(region.rect);
        }
        match self.get(i)? {
            Value::Frame(r) => Some(*r),
            _ => None,
        }
    }

    fn get_mut(&mut self, i: usize) -> Option<&mut Value> {
        if let Some(region) = &mut self.region
            && region.item.contains_key(&i)
        {
            return region.item.get_mut(&i);
        }
        self.base[i].as_mut()
    }

    fn set(&mut self, i: usize, v: Value) {
        match &mut self.region {
            Some(region) => {
                region.item.insert(i, v);
            }
            None => self.base[i] = Some(v),
        }
    }
}

impl Plan {
    /// Every model's device, by node id (`gpu (…)`, `cpu (…)`).
    pub fn devices(&self) -> Vec<(String, String)> {
        self.nodes
            .iter()
            .filter_map(|n| match &n.node_type {
                NodeType::Model { model, .. } => {
                    Some((n.id.clone(), model.session().device().to_string()))
                }
                NodeType::Detector(d) => Some((n.id.clone(), d.device())),
                _ => None,
            })
            .collect()
    }

    /// Wait for every detection, so a run on a file repeats exactly.
    pub fn set_deterministic(&mut self, on: bool) {
        self.deterministic = on;
        for n in &mut self.nodes {
            if let NodeType::Detector(d) = &mut n.node_type {
                d.set_deterministic(on);
            }
        }
    }

    /// The node ids, in run order.
    pub fn order(&self) -> Vec<&str> {
        self.nodes.iter().map(|n| n.id.as_str()).collect()
    }

    /// Runs the graph on a frame, the stream's clock counting frames at
    /// 30 per second.
    pub fn tick(&mut self, frame: &Frame) -> Tick {
        let time = self.seq as f64 / 30.0;
        self.tick_at(frame, time)
    }

    /// Runs the graph on a frame at `time`: seconds on the stream's clock,
    /// which the tracker and rules measure durations by.
    pub fn tick_at(&mut self, frame: &Frame, time: f64) -> Tick {
        let seq = self.seq;
        self.seq += 1;
        self.time = time.max(self.time);
        self.fresh.clear();
        if let Some(p) = &mut self.priority {
            p.start(time);
        }
        let mut tick = Tick::default();
        let mut base: Vec<Option<Value>> = vec![None; self.nodes.len()];
        for i in self.top.clone() {
            let mut values = Values {
                base: &mut base,
                region: None,
            };
            self.step(i, frame, &mut values, seq, &mut tick);
        }
        for (c, members) in self.scopes.clone() {
            let Some(Value::Regions(regions)) = base[c].clone() else {
                continue;
            };
            if members.iter().any(|&m| self.is_concurrent(m)) {
                self.run_scope_concurrent(c, &members, &regions, frame, &mut base, &mut tick);
                continue;
            }
            let parent = self.nodes[c].inputs["objects"];
            for region in regions {
                let track = match &base[parent] {
                    Some(Value::Results(Results::Objects(o))) => {
                        o.get(region.parent).and_then(|o| o.tracker_id)
                    }
                    _ => None,
                };
                let mut item = BTreeMap::new();
                {
                    let mut values = Values {
                        base: &mut base,
                        region: Some(ActiveRegion {
                            crop: c,
                            rect: region.rect,
                            item: &mut item,
                            track,
                        }),
                    };
                    for &m in &members {
                        self.step(m, frame, &mut values, seq, &mut tick);
                    }
                }
                // The models run on the region go with its object.
                for &m in &members {
                    let on_region = self.nodes[m].inputs.get("frame") == Some(&c);
                    if let (true, Some(Value::Results(r))) = (on_region, item.remove(&m))
                        && let Some(Value::Results(Results::Objects(objects))) =
                            base[parent].as_mut()
                        && let Some(o) = objects.get_mut(region.parent)
                    {
                        o.children.push((self.nodes[m].id.clone(), r));
                    }
                }
            }
        }
        self.steer_tracks(&mut base);
        for i in self.rules.clone() {
            let mut values = Values {
                base: &mut base,
                region: None,
            };
            self.step(i, frame, &mut values, seq, &mut tick);
        }
        for &i in &self.top {
            if self.tracked.contains(&i) {
                continue;
            }
            let n = &self.nodes[i];
            if let (
                NodeType::Model { .. } | NodeType::Filter(_) | NodeType::Tracker { .. },
                Some(Value::Results(r)),
            ) = (&n.node_type, base[i].take())
            {
                tick.results.push((n.id.clone(), r));
            }
        }
        // What a model kept per track goes when the track does.
        let ended: BTreeSet<u64> = tick
            .track_events
            .values()
            .flat_map(|e| ended_ids(e))
            .collect();
        if !ended.is_empty() {
            for n in &mut self.nodes {
                if let NodeType::Model {
                    per_track: Some(p), ..
                } = &mut n.node_type
                {
                    p.results.retain(|id, _| !ended.contains(id));
                }
            }
        }
        self.finish_priority(&mut tick);
        tick
    }

    /// Notes who yielded: a model that ran starts its count of skips over.
    fn finish_priority(&mut self, tick: &mut Tick) {
        let Some(p) = &mut self.priority else {
            return;
        };
        for &i in p.may_skip.keys() {
            let id = &self.nodes[i].id;
            if p.yielded.contains(&i) {
                *p.skipped.entry(i).or_default() += 1;
                log::debug!("{id}: reused its last results, the frame ran late");
                tick.yielded.push(id.clone());
            } else if tick.timing.get(id).is_some_and(|t| t.runs > 0) {
                p.skipped.insert(i, 0);
            }
        }
    }

    /// The frame is past its time and this model may reuse its last results.
    fn over_budget(&self, i: usize) -> bool {
        !self.deterministic && self.priority.as_ref().is_some_and(|p| p.over(i))
    }

    fn note_yield(&mut self, i: usize) {
        if let Some(p) = &mut self.priority {
            p.yielded.insert(i);
        }
    }

    fn is_concurrent(&self, i: usize) -> bool {
        matches!(
            &self.nodes[i].node_type,
            NodeType::Model { concurrency, .. } if concurrency.mode != Mode::Sequential
        )
    }

    /// A crop's per-region nodes with a concurrent model among them: member
    /// by member over all regions (a member needs only its own region's
    /// earlier members), then everything in the order one by one would give.
    fn run_scope_concurrent(
        &mut self,
        c: usize,
        members: &[usize],
        regions: &[Region],
        frame: &Frame,
        base: &mut [Option<Value>],
        tick: &mut Tick,
    ) {
        let parent = self.nodes[c].inputs["objects"];
        let tracks = regions
            .iter()
            .map(|region| match &base[parent] {
                Some(Value::Results(Results::Objects(o))) => {
                    o.get(region.parent).and_then(|o| o.tracker_id)
                }
                _ => None,
            })
            .collect();
        let mut run = ScopeRun {
            crop: c,
            regions,
            tracks,
            frame,
            seq: self.seq - 1,
            items: vec![BTreeMap::new(); regions.len()],
            errors: vec![vec![vec![]; members.len()]; regions.len()],
        };
        for (pos, &m) in members.iter().enumerate() {
            if self.is_concurrent(m) {
                self.run_member(m, pos, &mut run, base, tick);
                continue;
            }
            for (r, region) in regions.iter().enumerate() {
                let before = tick.errors.len();
                let mut values = Values {
                    base: &mut *base,
                    region: Some(ActiveRegion {
                        crop: c,
                        rect: region.rect,
                        item: &mut run.items[r],
                        track: run.tracks[r],
                    }),
                };
                self.step(m, frame, &mut values, run.seq, tick);
                run.errors[r][pos] = tick.errors.split_off(before);
            }
        }
        for (r, region) in regions.iter().enumerate() {
            for errors in &mut run.errors[r] {
                tick.errors.append(errors);
            }
            for &m in members {
                let on_region = self.nodes[m].inputs.get("frame") == Some(&c);
                if let (true, Some(Value::Results(result))) = (on_region, run.items[r].remove(&m))
                    && let Some(Value::Results(Results::Objects(objects))) = base[parent].as_mut()
                    && let Some(o) = objects.get_mut(region.parent)
                {
                    o.children.push((self.nodes[m].id.clone(), result));
                }
            }
        }
    }

    /// One concurrent model over all regions of a scope.
    fn run_member(
        &mut self,
        m: usize,
        pos: usize,
        run: &mut ScopeRun,
        base: &mut [Option<Value>],
        tick: &mut Tick,
    ) {
        let may_yield = self.over_budget(m);
        let mut yielded = false;
        let inputs = self.nodes[m].inputs.clone();
        let (id, output) = (self.nodes[m].id.clone(), self.nodes[m].output);
        let NodeType::Model {
            model,
            tensor,
            per_track,
            concurrency,
            ..
        } = &mut self.nodes[m].node_type
        else {
            return;
        };
        let n = run.regions.len();
        let mut done: Vec<Option<Results>> = vec![None; n];
        let mut units = vec![];
        // Each unit's region; with `tensor_input`, a region has one per object.
        let mut owners = vec![];
        let mut objects_of = vec![0; n];
        for r in 0..n {
            if let Some((p, track)) = per_track.as_ref().zip(run.tracks[r])
                && let Some((results, at)) = p.results.get(&track)
            {
                let fresh = p
                    .refresh
                    .is_none_or(|f| run.seq.saturating_sub(*at) < u64::from(f.max(1)));
                if fresh || may_yield {
                    yielded |= !fresh;
                    done[r] = Some(results.clone());
                    continue;
                }
            }
            let value = |i: usize| run.items[r].get(&i).or(base[i].as_ref());
            if tensor.is_some() {
                let objects = match inputs.get("objects").and_then(|&s| value(s)) {
                    Some(Value::Results(Results::Objects(o))) => o.clone(),
                    _ => vec![],
                };
                objects_of[r] = objects.len();
                for o in objects {
                    owners.push(r);
                    units.push(Unit::Object(o));
                }
            } else {
                let rect = inputs.get("frame").and_then(|&s| {
                    if s == run.crop {
                        return Some(run.regions[r].rect);
                    }
                    match value(s)? {
                        Value::Frame(rect) => Some(*rect),
                        _ => None,
                    }
                });
                match rect {
                    Some(rect) => {
                        owners.push(r);
                        units.push(Unit::Frame(rect));
                    }
                    None => done[r] = Some(Results::empty(output)),
                }
            }
        }
        let outs = run_units(
            model,
            concurrency,
            tensor.as_ref(),
            &units,
            run.frame,
            output,
        );
        let timing = tick.timing.entry(id.clone()).or_default();
        let mut per_region: Vec<Vec<Results>> = vec![vec![]; n];
        for (out, &r) in outs.into_iter().zip(&owners) {
            if out.ran {
                timing.runs += 1;
            }
            add(&mut timing.time, &out.timing);
            if let Some(e) = out.error {
                run.errors[r][pos].push(format!("{id}: {e}"));
            }
            per_region[r].push(out.results);
        }
        for r in 0..n {
            let result = if let Some(reused) = done[r].take() {
                if tensor.is_none()
                    && let (Some(p), Some(track)) = (per_track.as_mut(), run.tracks[r])
                    && !p.results.contains_key(&track)
                {
                    p.results.insert(track, (reused.clone(), run.seq));
                }
                reused
            } else if tensor.is_some() {
                let results = std::mem::take(&mut per_region[r]);
                debug_assert_eq!(results.len(), objects_of[r]);
                let source = inputs["objects"];
                let target = if run.items[r].contains_key(&source) {
                    run.items[r].get_mut(&source)
                } else {
                    base[source].as_mut()
                };
                if let Some(Value::Results(Results::Objects(objects))) = target {
                    for (o, x) in objects.iter_mut().zip(&results) {
                        o.children.push((id.clone(), x.clone()));
                    }
                }
                results
                    .into_iter()
                    .next()
                    .unwrap_or_else(|| Results::empty(output))
            } else {
                let result = per_region[r]
                    .pop()
                    .unwrap_or_else(|| Results::empty(output));
                if let (Some(p), Some(track)) = (per_track.as_mut(), run.tracks[r]) {
                    p.results.insert(track, (result.clone(), run.seq));
                }
                result
            };
            run.items[r].insert(m, Value::Results(result));
        }
        if yielded {
            self.note_yield(m);
        }
    }

    /// With control frames: lets each tracked object's pose (from the models
    /// run on its crop) steer its box, and asks for a detection when one is
    /// too weak or a track needs it.
    fn steer_tracks(&mut self, base: &mut [Option<Value>]) {
        let time = self.time;
        for (t, value) in base.iter_mut().enumerate() {
            let NodeType::Tracker {
                tracker,
                detector: Some(detector),
            } = &mut self.nodes[t].node_type
            else {
                continue;
            };
            let detector = *detector;
            let mut weak = false;
            if let Some(Value::Results(Results::Objects(objects))) = value.as_mut() {
                for o in objects.iter_mut() {
                    let Some(id) = o.tracker_id else {
                        continue;
                    };
                    let pose = o.children.iter_mut().find_map(|(_, r)| match r {
                        Results::Objects(list) => list.iter_mut().find(|c| !c.keypoints.is_empty()),
                        _ => None,
                    });
                    if let Some(pose) = pose
                        && !tracker.refine(id, &mut pose.keypoints, time)
                    {
                        weak = true;
                    }
                }
            }
            let urgent = tracker.wants_detection(time);
            if let NodeType::Detector(d) = &mut self.nodes[detector].node_type {
                if urgent {
                    d.request();
                }
                if weak {
                    d.suggest();
                }
            }
        }
    }

    fn step(&mut self, i: usize, frame: &Frame, values: &mut Values, seq: u64, tick: &mut Tick) {
        let in_region = values.region.is_some();
        let region_track = values.region.as_ref().and_then(|r| r.track);
        let time = self.time;
        let inputs = self.nodes[i].inputs.clone();
        let (id, output) = (self.nodes[i].id.clone(), self.nodes[i].output);
        let names: Vec<String> = if matches!(self.nodes[i].node_type, NodeType::Emit) {
            self.nodes.iter().map(|n| n.id.clone()).collect()
        } else {
            vec![]
        };
        let objects_in = |values: &Values| match inputs.get("objects").and_then(|&s| values.get(s))
        {
            Some(Value::Results(Results::Objects(o))) => o.clone(),
            _ => vec![],
        };
        let may_yield = self.over_budget(i);
        let mut yielded = false;
        let value = match &mut self.nodes[i].node_type {
            NodeType::Source => Value::Frame(frame.full()),
            NodeType::Crop(c) => {
                let objects = objects_in(values);
                Value::Regions(crop::regions(c, &objects))
            }
            NodeType::Filter(f) => {
                Value::Results(Results::Objects(crop::filter(f, &objects_in(values))))
            }
            NodeType::Detector(detector) => {
                let rect = inputs.get("frame").and_then(|&s| values.frame_rect(s));
                let done = rect.and_then(|r| detector.step(frame, r, time));
                let mut value = Results::empty(output);
                if let Some(done) = done {
                    match done.results {
                        Ok((results, t)) => {
                            let timing = tick.timing.entry(id.clone()).or_default();
                            timing.runs += 1;
                            add(&mut timing.time, &t);
                            value = results;
                            self.fresh.insert(i, done.time);
                        }
                        Err(e) => tick.errors.push(format!("{id}: {e}")),
                    }
                }
                Value::Results(value)
            }
            NodeType::Tracker { tracker, detector } => {
                let objects = objects_in(values);
                let update = match detector {
                    Some(d) => {
                        let found = self.fresh.get(d).map(|&at| (objects.as_slice(), at));
                        tracker.advance(found, time)
                    }
                    None => tracker.update(&objects, time),
                };
                tick.tracks.insert(id.clone(), update.tracks);
                tick.track_events.insert(id.clone(), update.events);
                Value::Results(Results::Objects(update.objects))
            }
            NodeType::Emit => {
                if let Some(&from) = inputs.get("objects") {
                    let objects = objects_in(values);
                    tick.emit_objects = Some((names[from].clone(), objects));
                }
                Value::Events
            }
            NodeType::Rule { engine, tracker } => {
                let classes = match inputs.get("classes").and_then(|&s| values.get(s)) {
                    Some(Value::Results(Results::Classes(c))) => c.clone(),
                    _ => vec![],
                };
                let objects = objects_in(values);
                let name = tracker.as_deref().unwrap_or_default();
                let input = RuleInputs {
                    objects: &objects,
                    classes: &classes,
                    tracks: tick.tracks.get(name).map_or(&[], Vec::as_slice),
                    track_events: tick.track_events.get(name).map_or(&[], Vec::as_slice),
                    width: frame.width() as f32,
                    height: frame.height() as f32,
                    time,
                };
                let events = engine.update(&input);
                tick.events.extend(events);
                Value::Events
            }
            #[cfg(feature = "lua")]
            NodeType::Lua { rule, tracker } => {
                let objects = objects_in(values);
                let name = tracker.as_deref().unwrap_or_default();
                let input = crate::analytics::script::ScriptInput {
                    objects: &objects,
                    tracks: tick.tracks.get(name).map_or(&[], Vec::as_slice),
                    track_events: tick.track_events.get(name).map_or(&[], Vec::as_slice),
                    seq,
                    width: frame.width(),
                    height: frame.height(),
                    time,
                };
                match rule.update(&input) {
                    Ok(events) => tick.events.extend(events),
                    Err(e) => tick.errors.push(format!("{id}: {e}")),
                }
                Value::Events
            }
            NodeType::Model {
                model,
                tensor,
                every,
                last,
                per_track,
                concurrency,
            } => {
                let timing = tick.timing.entry(id.clone()).or_default();
                let reusable = per_track.as_ref().zip(region_track).and_then(|(p, track)| {
                    let (results, at) = p.results.get(&track)?;
                    let fresh = p
                        .refresh
                        .is_none_or(|n| seq.saturating_sub(*at) < u64::from(n.max(1)));
                    yielded |= !fresh && may_yield;
                    (fresh || may_yield).then(|| results.clone())
                });
                let due = seq.is_multiple_of((*every).max(1) as u64);
                if let Some(results) = reusable {
                    Value::Results(results)
                } else if !in_region && *every > 1 && (!due || may_yield) && last.is_some() {
                    yielded |= due;
                    Value::Results(last.clone().expect("checked"))
                } else if let Some(t) = tensor {
                    // Once per input object; each result goes with its object.
                    let source = inputs["objects"];
                    let objects = objects_in(values);
                    let mut per_object = vec![];
                    if concurrency.mode != Mode::Sequential {
                        let units: Vec<Unit> = objects.iter().cloned().map(Unit::Object).collect();
                        let outs = run_units(model, concurrency, Some(t), &units, frame, output);
                        for out in outs {
                            if out.ran {
                                timing.runs += 1;
                            }
                            add(&mut timing.time, &out.timing);
                            if let Some(e) = out.error {
                                tick.errors.push(format!("{id}: {e}"));
                            }
                            per_object.push(out.results);
                        }
                    }
                    for o in objects
                        .iter()
                        .filter(|_| concurrency.mode == Mode::Sequential)
                    {
                        let started = std::time::Instant::now();
                        let prepared = model.object_input(t, o, frame.width(), frame.height());
                        timing.time.pre += started.elapsed();
                        let r = match prepared {
                            Err(e) => {
                                tick.errors.push(format!("{id}: {e}"));
                                Results::empty(output)
                            }
                            Ok(None) => Results::empty(output),
                            Ok(Some(tensor)) => match model.infer(tensor, 1.0, 1.0) {
                                Ok(run) => {
                                    timing.runs += 1;
                                    add(&mut timing.time, &run.timing);
                                    run.results
                                }
                                Err(e) => {
                                    tick.errors.push(format!("{id}: {e}"));
                                    Results::empty(output)
                                }
                            },
                        };
                        per_object.push(r);
                    }
                    if let Some(Value::Results(Results::Objects(objects))) = values.get_mut(source)
                    {
                        for (o, r) in objects.iter_mut().zip(&per_object) {
                            o.children.push((id.clone(), r.clone()));
                        }
                    }
                    let first = per_object
                        .into_iter()
                        .next()
                        .unwrap_or_else(|| Results::empty(output));
                    if !in_region {
                        *last = Some(first.clone());
                    }
                    Value::Results(first)
                } else {
                    let rect = inputs.get("frame").and_then(|&s| values.frame_rect(s));
                    let results = match rect.map(|r| model.run(frame, r)) {
                        Some(Ok(run)) => {
                            timing.runs += 1;
                            add(&mut timing.time, &run.timing);
                            if !in_region {
                                let input =
                                    model.config().pre.as_ref().map(|p| (p.width, p.height));
                                tick.affines.insert(
                                    id.clone(),
                                    result_affine(run.affine, input, &run.results),
                                );
                            }
                            run.results
                        }
                        Some(Err(e)) => {
                            tick.errors.push(format!("{id}: {e}"));
                            Results::empty(output)
                        }
                        None => Results::empty(output),
                    };
                    if !in_region {
                        *last = Some(results.clone());
                    }
                    if let (Some(p), Some(track)) = (per_track.as_mut(), region_track) {
                        p.results.insert(track, (results.clone(), seq));
                    }
                    Value::Results(results)
                }
            }
        };
        if yielded {
            self.note_yield(i);
        }
        values.set(i, value);
    }
}

/// Pixels of a run's result → frame pixels: a map smaller than the model's
/// input covers all of it, so its pixels are scaled up first.
fn result_affine(affine: Affine, input: Option<(u32, u32)>, results: &Results) -> Affine {
    let (Results::Map(map), Some((w, h))) = (results, input) else {
        return affine;
    };
    if map.width == 0 || map.height == 0 {
        return affine;
    }
    let (sx, sy) = (w as f32 / map.width as f32, h as f32 / map.height as f32);
    Affine {
        a: affine.a * sx,
        b: affine.b * sy,
        d: affine.d * sx,
        e: affine.e * sy,
        ..affine
    }
}

fn add(total: &mut Timing, t: &Timing) {
    total.pre += t.pre;
    total.infer += t.infer;
    total.decode += t.decode;
}

#[cfg(test)]
#[path = "exec_tests.rs"]
mod tests;
