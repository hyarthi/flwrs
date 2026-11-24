//! The graph's settings, as a node stores them (the `graph` launch arg).
//!
//! ```json
//! {
//!   "nodes": {
//!     "camera": {"type": "source"},
//!     "palm":   {"type": "model", "model": "palm.onnx", "pre": {…}, "decode": {"type": "ssd_anchors", …}},
//!     "roi":    {"type": "crop", "rotate": {"from": 0, "to": 2, "angle": 90}, "scale": 2.6, "square": true},
//!     "hand":   {"type": "model", "model": "hand.onnx", "pre": {…}, "decode": {"type": "landmarks", …}}
//!   },
//!   "links": [
//!     {"from": "camera.frame", "to": "palm.frame"},
//!     {"from": "camera.frame", "to": "roi.frame"},
//!     {"from": "palm.objects", "to": "roi.objects"},
//!     {"from": "roi.regions",  "to": "hand.frame"}
//!   ]
//! }
//! ```
//!
//! Links go from an output port to an input port (`<node>.<port>`); every
//! input is linked exactly once. A `regions` list linked to a `frame` input
//! runs the node once per region ("for each"), and everything downstream of
//! it too, until the results are attached to the region's parent object.

use crate::backend::{Device, Runtime};
use crate::decode::Decoder;
use crate::pre::Pre;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The whole graph.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct GraphConfig {
    /// By id: lowercase letters, digits and `_`, starting with a letter.
    pub nodes: BTreeMap<String, NodeConfig>,
    #[serde(default)]
    pub links: Vec<Link>,
    /// Execution and resource groups; a model names its own in `groups`.
    /// They decide how models run, never what they compute.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<flwrs_types::groups::Group>,
}

/// A link from an output port to an input port, both `<node>.<port>`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Link {
    pub from: String,
    pub to: String,
}

/// A node, by `type`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum NodeConfig {
    /// The camera or video frame. Out: `frame`.
    Source {},
    /// A model: pre-processing, inference, decoding. In: `frame` (or
    /// `objects` with `tensor_input`); out: its decoder's result type.
    Model(Box<ModelConfig>),
    /// Regions of the frame around objects. In: `frame`, `objects`;
    /// out: `regions`.
    Crop(CropConfig),
    /// Objects kept by class, score and count. In / out: `objects`.
    Filter(FilterConfig),
    /// Objects followed across frames. In / out: `objects`, with track ids.
    Tracker(TrackerConfig),
    /// A built-in rule. In: `objects` (or `classes`); out: `events`.
    Rule(Box<RuleConfig>),
    /// A rule in Lua. In: `objects`; out: `events`.
    Lua(Box<LuaRuleConfig>),
    /// What leaves the plugin (at most one; without it, rule events). In:
    /// `objects`, for `changes` and `frames`.
    Emit(EmitConfig),
}

impl NodeConfig {
    pub fn type_name(&self) -> &'static str {
        match self {
            NodeConfig::Source {} => "source",
            NodeConfig::Model(_) => "model",
            NodeConfig::Crop(_) => "crop",
            NodeConfig::Filter(_) => "filter",
            NodeConfig::Tracker(_) => "tracker",
            NodeConfig::Rule(_) => "rule",
            NodeConfig::Lua(_) => "lua",
            NodeConfig::Emit(_) => "emit",
        }
    }
}

/// A model node.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ModelConfig {
    /// The ONNX file: the name of a file uploaded to the node.
    pub model: String,
    /// The model input's tensor name (default: its first input).
    #[serde(default)]
    pub input_name: Option<String>,
    /// How a frame becomes the input. Exactly one of `pre` and `tensor_input`.
    #[serde(default)]
    pub pre: Option<Pre>,
    /// The input built from objects (e.g. landmarks → a gesture classifier).
    #[serde(default)]
    pub tensor_input: Option<TensorInput>,
    pub decode: Decoder,
    #[serde(default)]
    pub labels: Option<Labels>,
    /// Where a CTC decoder's charset comes from.
    #[serde(default)]
    pub charset: Option<TextSource>,
    /// Results below this confidence are dropped as soon as the decoder
    /// knows it (default: 0.25 for detectors, 0 for the others; not for
    /// decoders whose results have no confidence).
    #[serde(default)]
    pub min_confidence: Option<f32>,
    /// How often it runs (default: every frame).
    #[serde(default)]
    pub sampling: Option<Sampling>,
    /// Where it runs; each field set here overrides its execution group's
    /// and the plugin's `execution` setting.
    #[serde(default)]
    pub execution: Option<Execution>,
    /// The groups it's in (`groups` of the graph), by id.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<String>,
    /// What the keypoints are (names and skeleton): `coco17`,
    /// `blazepose33`, `hand21`, `wholebody133`, `face5`, `face6`.
    #[serde(default)]
    pub keypoint_layout: Option<String>,
}

/// Where a model runs: the plugin's defaults (its `execution` setting) and
/// a model node's overrides. Unset fields fall back, finally to the build's
/// first runtime, the CPU, and no fallback.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(deny_unknown_fields)]
pub struct Execution {
    #[serde(default)]
    pub runtime: Option<Runtime>,
    #[serde(default)]
    pub device: Option<Device>,
    /// The execution provider (ONNX Runtime): `coreml`, `cuda`,
    /// `tensorrt`, `directml`, `openvino`, `webgpu`, `xnnpack`, or an
    /// `…ExecutionProvider` name. Default: the first the device offers.
    #[serde(default)]
    pub provider: Option<String>,
    /// The provider's options (e.g. CoreML `ModelFormat`: `MLProgram`).
    #[serde(default)]
    pub provider_options: BTreeMap<String, String>,
    /// A model that won't load or run on its device runs on the CPU
    /// instead, with a warning (off: it's an error).
    #[serde(default)]
    pub allow_fallback: Option<bool>,
}

impl Execution {
    /// The settings' own problems.
    pub fn problems(&self) -> Vec<String> {
        let mut problems = vec![];
        if !self.provider_options.is_empty() && self.provider.is_none() {
            problems.push("execution.provider_options need an execution.provider".into());
        }
        if self.runtime == Some(Runtime::Tract)
            && (self.device.is_some_and(|d| d != Device::Cpu) || self.provider.is_some())
        {
            problems.push("execution: tract runs on the CPU only (no device or provider)".into());
        }
        problems
    }

    /// These settings over `base`: each field set here wins.
    pub fn over(&self, base: &Execution) -> Execution {
        Execution {
            runtime: self.runtime.or(base.runtime),
            device: self.device.or(base.device),
            provider: self.provider.clone().or_else(|| base.provider.clone()),
            provider_options: if self.provider_options.is_empty() {
                base.provider_options.clone()
            } else {
                self.provider_options.clone()
            },
            allow_fallback: self.allow_fallback.or(base.allow_fallback),
        }
    }
}

/// A model input built from objects: their keypoints as a flat vector.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TensorInput {
    /// Values per keypoint: 2 (x, y) or 3 (x, y, score). Not with `embedder`.
    #[serde(default)]
    pub dims: usize,
    /// How coordinates are made relative.
    #[serde(default)]
    pub normalize: Normalize,
    /// The keypoints used (default: all), by index.
    #[serde(default)]
    pub keypoints: Option<Vec<usize>>,
    /// MediaPipe's gesture embedder (a file of the node): the input is its
    /// embedding of a hand's landmarks, world landmarks and handedness, from
    /// a `landmarks` decoder with `world` and `handedness`.
    #[serde(default)]
    pub embedder: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum Normalize {
    /// Frame pixels as they are.
    #[default]
    None,
    /// Relative to the object's box: 0..1 on each axis.
    Box,
    /// Relative to the first keypoint, scaled by the largest distance
    /// (translation and scale invariant: gesture classifiers).
    Wrist,
}

/// Class labels: a list, or where to read them.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(untagged)]
pub enum Labels {
    List(Vec<String>),
    From(LabelSource),
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum LabelSource {
    /// A built-in list: `coco80`, `imagenet1k`.
    Builtin(String),
    /// An uploaded text file, one label per line.
    File(String),
    /// The model's metadata: Ultralytics `names` (a Python dict literal).
    ModelMetadata(String),
    /// An uploaded Hugging Face `config.json` (`id2label`).
    HfConfig(String),
}

/// A text read from somewhere (a CTC charset: one entry per line).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum TextSource {
    File(String),
    ModelMetadata(String),
}

/// How often a model runs. Unlike execution policies, sampling changes the
/// results (a skipped frame reuses the last ones).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Sampling {
    /// Every n-th frame.
    EveryNFrames(u32),
    /// Once per tracked object, again every `refresh_frames` frames.
    PerTrack { refresh_frames: Option<u32> },
}

/// A class, by id or by label.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(untagged)]
pub enum ClassRef {
    Id(u32),
    Label(String),
}

/// A crop node: a (possibly rotated) region per object.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CropConfig {
    /// Only these objects (default: all).
    #[serde(default)]
    pub classes: Vec<ClassRef>,
    /// At most this many regions per frame, the highest scores first.
    #[serde(default = "d_max")]
    pub max: usize,
    /// The centre: this keypoint (default: the box's centre).
    #[serde(default)]
    pub center_keypoint: Option<usize>,
    /// The size: twice the distance between these two keypoints (default:
    /// the box).
    #[serde(default)]
    pub size_keypoints: Option<[usize; 2]>,
    /// The rotation: the direction from keypoint `from` to `to` turned to
    /// `angle` degrees (MediaPipe regions of interest).
    #[serde(default)]
    pub rotate: Option<Rotate>,
    #[serde(default = "d_one")]
    pub scale: f32,
    /// A shift along the region's own y axis, in region heights.
    #[serde(default)]
    pub shift_y: f32,
    #[serde(default)]
    pub square: bool,
    /// A fixed width / height ratio, growing the short side (top-down pose:
    /// 0.75).
    #[serde(default)]
    pub aspect: Option<f32>,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Rotate {
    pub from: usize,
    pub to: usize,
    pub angle: f32,
}

/// A filter node.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FilterConfig {
    #[serde(default)]
    pub classes: Vec<ClassRef>,
    #[serde(default)]
    pub min_confidence: Option<f32>,
    #[serde(default)]
    pub max: Option<usize>,
}

fn d_max() -> usize {
    8
}

fn d_one() -> f32 {
    1.0
}

/// A tracker node (ByteTrack-style).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TrackerConfig {
    /// At or above: matched first. Below: only keeps an existing track.
    #[serde(default = "default_high")]
    pub high_confidence: f32,
    /// Below this, a detection is ignored.
    #[serde(default = "default_low")]
    pub low_confidence: f32,
    /// What a detection needs to start a track.
    #[serde(default = "default_new_track")]
    pub new_track_confidence: f32,
    /// The least box overlap (IoU) to match a confident detection.
    #[serde(default = "default_min_iou")]
    pub min_iou: f32,
    /// The least overlap to match a weak one.
    #[serde(default = "default_min_iou_low")]
    pub min_iou_low: f32,
    /// Frames in a row before a track counts (and gets an id).
    #[serde(default = "default_min_hits")]
    pub min_hits: u32,
    /// How long a lost track waits to be picked up again, seconds.
    #[serde(default = "default_max_lost")]
    pub max_lost_s: f32,
    /// Match only within a class.
    #[serde(default = "default_true")]
    pub class_aware: bool,
    /// One-Euro smoothing of tracked objects' keypoints.
    #[serde(default)]
    pub smooth_keypoints: Option<SmoothKeypoints>,
    /// Run the detector feeding this tracker only on control frames.
    #[serde(default)]
    pub control: Option<ControlConfig>,
}

fn default_high() -> f32 {
    0.5
}
fn default_low() -> f32 {
    0.1
}
fn default_new_track() -> f32 {
    0.6
}
fn default_min_iou() -> f32 {
    0.2
}
fn default_min_iou_low() -> f32 {
    0.5
}
fn default_min_hits() -> u32 {
    3
}
fn default_max_lost() -> f32 {
    1.0
}
fn default_true() -> bool {
    true
}

/// Control frames: the detector runs on one, the tracker carries the objects
/// over the next `tracker_frames`, and models on tracked regions run on all.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ControlConfig {
    /// Frames between control frames (0: detect on every frame).
    #[serde(default = "default_tracker_frames")]
    pub tracker_frames: u32,
    /// Run the detector on its own thread, so a control frame doesn't hold
    /// up the others.
    #[serde(default = "default_true")]
    pub background: bool,
    /// The share of a tracked pose's keypoints that must be visible for it
    /// to steer the box; below it, a control frame runs at once.
    #[serde(default = "default_min_visible")]
    pub min_visible: f32,
    /// Control frames in a row that may miss a track before it ends (what a
    /// crop model sees on an empty background can't keep it alive).
    #[serde(default = "default_max_missed")]
    pub max_missed: u32,
    /// How gently a detection corrects the box: 1 follows it closely,
    /// higher keeps the bounds steadier.
    #[serde(default = "default_box_smoothing")]
    pub box_smoothing: f32,
}

fn default_tracker_frames() -> u32 {
    5
}
fn default_min_visible() -> f32 {
    0.5
}
fn default_max_missed() -> u32 {
    3
}
fn default_box_smoothing() -> f32 {
    3.0
}

/// A One-Euro filter's settings.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SmoothKeypoints {
    /// Hz: lower = smoother at rest.
    #[serde(default = "default_min_cutoff")]
    pub min_cutoff: f32,
    /// How fast smoothing eases with speed: higher = less lag.
    #[serde(default = "default_beta")]
    pub beta: f32,
}

fn default_min_cutoff() -> f32 {
    1.5
}
fn default_beta() -> f32 {
    0.02
}

/// What a rule reads.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RuleInput {
    #[default]
    Objects,
    /// A whole-frame classifier's classes (`label_changed`, `score_above`).
    Classes,
}

/// The point of a box that counts for lines and zones.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Anchor {
    /// Where people and cars touch the ground.
    #[default]
    BottomCenter,
    Center,
    TopCenter,
}

/// A rule node.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RuleConfig {
    /// In its events (default: the node's id).
    #[serde(default)]
    pub name: Option<String>,
    pub rule: RuleType,
    #[serde(default)]
    pub input: RuleInput,
    /// Only these classes (default: all).
    #[serde(default)]
    pub classes: Vec<ClassRef>,
    #[serde(default)]
    pub anchor: Anchor,
    /// How long a change must hold before it counts, seconds.
    #[serde(default)]
    pub debounce_s: f32,
    /// The least time between one track's events, seconds.
    #[serde(default)]
    pub cooldown_s: f32,
    /// The margin to change back: a distance (0..1) for lines and zones,
    /// objects for counts, a score for scores.
    #[serde(default)]
    pub hysteresis: f32,
}

/// What a rule watches. Names follow ONVIF Profile M where it has one.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum RuleType {
    /// Objects crossing a polyline. `in` = to the right of its direction.
    LineCrossing {
        line: Vec<[f32; 2]>,
        #[serde(default)]
        direction: LineDirection,
    },
    /// Objects entering or leaving a polygon.
    Zone {
        zone: Vec<[f32; 2]>,
        #[serde(default)]
        on: ZoneOn,
    },
    /// An object staying in a polygon for `seconds`.
    Loitering { zone: Vec<[f32; 2]>, seconds: f32 },
    /// How many objects (in a polygon, or in the frame) passing a limit.
    Counting {
        #[serde(default)]
        zone: Option<Vec<[f32; 2]>>,
        #[serde(default)]
        above: Option<u32>,
        #[serde(default)]
        below: Option<u32>,
    },
    /// Tracks starting or ending.
    Track {
        #[serde(default)]
        on: TrackOn,
        /// A track younger than this never happened (flicker), seconds.
        #[serde(default)]
        min_age_s: f32,
    },
    /// A track's (or the frame's) label changing.
    LabelChanged {
        /// A model run on the object (default: the object's own class).
        #[serde(default)]
        from: Option<String>,
    },
    /// A score rising past a threshold.
    ScoreAbove {
        threshold: f32,
        /// A model run on the object (default: the object's confidence).
        #[serde(default)]
        from: Option<String>,
        /// Which class's score (default: the top one).
        #[serde(default)]
        class: Option<ClassRef>,
    },
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LineDirection {
    #[default]
    Both,
    In,
    Out,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ZoneOn {
    #[default]
    Both,
    Enter,
    Exit,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TrackOn {
    #[default]
    Both,
    Started,
    Ended,
}

/// A Lua rule: `function(frame, objects, tracks, state)`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct LuaRuleConfig {
    /// In its events (default: the node's id).
    #[serde(default)]
    pub name: Option<String>,
    pub script: String,
    #[serde(default)]
    pub limits: LuaLimits,
}

/// A Lua rule's limits per frame (default: the sandbox's).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LuaLimits {
    #[serde(default)]
    pub max_instructions: Option<u64>,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    #[serde(default)]
    pub max_memory_bytes: Option<u64>,
}

/// What leaves the plugin.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EmitMode {
    /// Only rule events.
    #[default]
    Rules,
    /// Rule events, and the objects when who's there (or a label) changes.
    Changes,
    /// Rule events, and the objects at most every `min_interval_ms`.
    Frames,
}

/// An `emit` node.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EmitConfig {
    #[serde(default)]
    pub mode: EmitMode,
    /// The least time between two frames' objects (`changes`, `frames`).
    #[serde(default = "default_emit_interval")]
    pub min_interval_ms: u32,
    /// The most objects in one frame's event (the most confident).
    #[serde(default = "default_emit_objects")]
    pub max_objects: usize,
    /// Include keypoints.
    #[serde(default = "default_true")]
    pub keypoints: bool,
}

impl Default for EmitConfig {
    fn default() -> Self {
        Self {
            mode: EmitMode::Rules,
            min_interval_ms: default_emit_interval(),
            max_objects: default_emit_objects(),
            keypoints: true,
        }
    }
}

fn default_emit_interval() -> u32 {
    200
}
fn default_emit_objects() -> usize {
    32
}
