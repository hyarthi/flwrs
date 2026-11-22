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

use super::config::{Execution, GraphConfig, NodeConfig, Sampling, TensorInput};
use super::crop::{self, Region};
use super::{Checked, PortType, Ports};
use crate::frame::{Frame, Rect};
use crate::model::{Model, Timing, keypoint_input};
use crate::results::Results;
use crate::tensor::{Tensor, TensorData};
use std::collections::BTreeMap;
use std::path::PathBuf;

enum Kind {
    Source,
    Model {
        model: Box<Model>,
        tensor: Option<TensorInput>,
        /// `every_n_frames`, for a once-per-frame model.
        every: u32,
        last: Option<Results>,
    },
    Crop(super::config::CropConfig),
    Filter(super::config::FilterConfig),
}

struct Node {
    id: String,
    kind: Kind,
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
}

/// A graph ready to run: every model loaded.
pub struct Plan {
    nodes: Vec<Node>,
    /// Once-per-frame nodes, in order.
    top: Vec<usize>,
    /// Per crop node: its per-region nodes, in order.
    scopes: Vec<(usize, Vec<usize>)>,
    seq: u64,
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
        let kind = match config {
            NodeConfig::Source {} => Kind::Source,
            NodeConfig::Crop(c) => Kind::Crop(c.clone()),
            NodeConfig::Filter(f) => Kind::Filter(f.clone()),
            NodeConfig::Model(m) => {
                let tensor_len = m.tensor_input.as_ref().and_then(|t| {
                    let n = t
                        .keypoints
                        .as_ref()
                        .map(Vec::len)
                        .or_else(|| keypoints_from(graph, &links, id))?;
                    Some(n * t.dims)
                });
                match Model::load(m, files, defaults, threads, tensor_len) {
                    Ok((model, warning)) => {
                        warnings.extend(warning.map(|w| format!("nodes.{id}: {w}")));
                        let every = match m.sampling {
                            Some(Sampling::EveryNFrames(n)) => n.max(1),
                            _ => 1,
                        };
                        Kind::Model {
                            model: Box::new(model),
                            tensor: m.tensor_input.clone(),
                            every,
                            last: None,
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
            kind,
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
    let top = (0..nodes.len())
        .filter(|&i| scope_of(i).is_none())
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
            seq: 0,
        },
        warnings,
    ))
}

/// Values during a tick: the frame's, and a region's over them.
struct Values<'a> {
    base: &'a mut [Option<Value>],
    region: Option<(usize, Rect, &'a mut BTreeMap<usize, Value>)>,
}

impl Values<'_> {
    fn get(&self, i: usize) -> Option<&Value> {
        if let Some((_, _, item)) = &self.region
            && let Some(v) = item.get(&i)
        {
            return Some(v);
        }
        self.base[i].as_ref()
    }

    /// The frame region a `frame` input linked from node `i` gets: in a
    /// crop's scope, the crop's regions give the current region.
    fn frame_rect(&self, i: usize) -> Option<Rect> {
        if let Some((crop, rect, _)) = &self.region
            && i == *crop
        {
            return Some(*rect);
        }
        match self.get(i)? {
            Value::Frame(r) => Some(*r),
            _ => None,
        }
    }

    fn get_mut(&mut self, i: usize) -> Option<&mut Value> {
        if let Some((_, _, item)) = &mut self.region
            && item.contains_key(&i)
        {
            return item.get_mut(&i);
        }
        self.base[i].as_mut()
    }

    fn set(&mut self, i: usize, v: Value) {
        match &mut self.region {
            Some((_, _, item)) => {
                item.insert(i, v);
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
            .filter_map(|n| match &n.kind {
                Kind::Model { model, .. } => {
                    Some((n.id.clone(), model.session().device().to_string()))
                }
                _ => None,
            })
            .collect()
    }

    /// The node ids, in run order.
    pub fn order(&self) -> Vec<&str> {
        self.nodes.iter().map(|n| n.id.as_str()).collect()
    }

    /// Runs the graph on a frame.
    pub fn tick(&mut self, frame: &Frame) -> Tick {
        let seq = self.seq;
        self.seq += 1;
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
                let mut item = BTreeMap::new();
                {
                    let mut values = Values {
                        base: &mut base,
                        region: Some((c, region.rect, &mut item)),
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
        for &i in &self.top {
            let n = &self.nodes[i];
            if let (Kind::Model { .. } | Kind::Filter(_), Some(Value::Results(r))) =
                (&n.kind, base[i].take())
            {
                tick.results.push((n.id.clone(), r));
            }
        }
        tick
    }

    fn step(&mut self, i: usize, frame: &Frame, values: &mut Values, seq: u64, tick: &mut Tick) {
        let in_region = values.region.is_some();
        let inputs = self.nodes[i].inputs.clone();
        let (id, output) = (self.nodes[i].id.clone(), self.nodes[i].output);
        let objects_in = |values: &Values| match inputs.get("objects").and_then(|&s| values.get(s))
        {
            Some(Value::Results(Results::Objects(o))) => o.clone(),
            _ => vec![],
        };
        let value = match &mut self.nodes[i].kind {
            Kind::Source => Value::Frame(frame.full()),
            Kind::Crop(c) => {
                let objects = objects_in(values);
                Value::Regions(crop::regions(c, &objects))
            }
            Kind::Filter(f) => {
                Value::Results(Results::Objects(crop::filter(f, &objects_in(values))))
            }
            Kind::Model {
                model,
                tensor,
                every,
                last,
            } => {
                let timing = tick.timing.entry(id.clone()).or_default();
                if !in_region && *every > 1 && !seq.is_multiple_of(*every as u64) && last.is_some()
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
                    Value::Results(results)
                }
            }
        };
        values.set(i, value);
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
