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

use super::config::{ControlConfig, Execution, GraphConfig, NodeConfig, Sampling, TensorInput};
use super::crop::{self, Region};
use super::{Checked, PortType, Ports};
use crate::analytics::control::Detector;
use crate::analytics::rules::{RuleEngine, RuleEvent, RuleInputs, ended_ids};
use crate::analytics::track::{TrackEvent, TrackInfo, Tracker};
use crate::frame::{Affine, Frame, Rect};
use crate::model::{Model, Timing, keypoint_input};
use crate::results::Results;
use crate::tensor::{Tensor, TensorData};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

enum NodeType {
    Source,
    Model {
        model: Box<Model>,
        tensor: Option<TensorInput>,
        /// `every_n_frames`, for a once-per-frame model.
        every: u32,
        last: Option<Results>,
        per_track: Option<PerTrack>,
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
    #[cfg(feature = "lua")]
    Lua {
        rule: Box<crate::analytics::script::LuaRule>,
        tracker: Option<String>,
    },
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

/// Builds the plan for a checked graph: every model loaded on its device
/// (with `files`, the node's files by name). Every problem is reported.
pub fn compile(
    graph: &GraphConfig,
    checked: &Checked,
    files: &BTreeMap<String, PathBuf>,
    defaults: &Execution,
    threads: usize,
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
    #[cfg(feature = "lua")]
    let layouts: BTreeMap<String, crate::keypoints::KeypointLayout> = graph
        .nodes
        .iter()
        .filter_map(|(id, n)| match n {
            NodeConfig::Model(m) => Some((
                id.clone(),
                crate::keypoints::layout(m.keypoint_layout.as_deref()?)?,
            )),
            _ => None,
        })
        .collect();
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
                let tensor_len = m.tensor_input.as_ref().and_then(|t| {
                    let n = t
                        .keypoints
                        .as_ref()
                        .map(Vec::len)
                        .or_else(|| keypoints_from(graph, &links, id))?;
                    Some(n * t.dims)
                });
                let control = controlled.get(id);
                let model_threads = if control.is_some() {
                    detector_threads
                } else {
                    frame_threads
                };
                match Model::load(m, files, defaults, model_threads, tensor_len) {
                    Ok((model, warning)) => {
                        warnings.extend(warning.map(|w| format!("nodes.{id}: {w}")));
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
        NodeType::Rule { .. } => true,
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
        },
        warnings,
    ))
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
        tick
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
        let objects_in = |values: &Values| match inputs.get("objects").and_then(|&s| values.get(s))
        {
            Some(Value::Results(Results::Objects(o))) => o.clone(),
            _ => vec![],
        };
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
            } => {
                let timing = tick.timing.entry(id.clone()).or_default();
                let reusable = per_track.as_ref().zip(region_track).and_then(|(p, track)| {
                    let (results, at) = p.results.get(&track)?;
                    let fresh = p
                        .refresh
                        .is_none_or(|n| seq.saturating_sub(*at) < u64::from(n.max(1)));
                    fresh.then(|| results.clone())
                });
                if let Some(results) = reusable {
                    Value::Results(results)
                } else if !in_region
                    && *every > 1
                    && !seq.is_multiple_of(*every as u64)
                    && last.is_some()
                {
                    Value::Results(last.clone().expect("checked"))
                } else if let Some(t) = tensor {
                    // Once per input object; each result goes with its object.
                    let source = inputs["objects"];
                    let objects = objects_in(values);
                    let mut per_object = vec![];
                    for o in &objects {
                        let r = match keypoint_input(t, o) {
                            None => Results::empty(output),
                            Some(v) => {
                                let tensor = Tensor {
                                    shape: vec![1, v.len()],
                                    data: TensorData::F32(v),
                                };
                                match model.infer(tensor, 1.0, 1.0) {
                                    Ok(run) => {
                                        timing.runs += 1;
                                        add(&mut timing.time, &run.timing);
                                        run.results
                                    }
                                    Err(e) => {
                                        tick.errors.push(format!("{id}: {e}"));
                                        Results::empty(output)
                                    }
                                }
                            }
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
