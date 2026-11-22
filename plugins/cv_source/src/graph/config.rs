//! The graph's settings, as a node stores them (the `graph` launch arg).
//!
//! ```json
//! {
//!   "nodes": {
//!     "camera": {"kind": "source"},
//!     "palm":   {"kind": "model", "model": "palm.onnx", "pre": {…}, "decode": {"type": "ssd_anchors", …}},
//!     "roi":    {"kind": "crop", "rotate": {"from": 0, "to": 2, "angle": 90}, "scale": 2.6, "square": true},
//!     "hand":   {"kind": "model", "model": "hand.onnx", "pre": {…}, "decode": {"type": "landmarks", …}}
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
}

/// A link from an output port to an input port, both `<node>.<port>`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Link {
    pub from: String,
    pub to: String,
}

/// A node, by `kind`. More kinds (`tracker`, `rule`, `lua`, `emit`) come
/// with the stages that run them.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
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
}

impl NodeConfig {
    pub fn kind(&self) -> &'static str {
        match self {
            NodeConfig::Source {} => "source",
            NodeConfig::Model(_) => "model",
            NodeConfig::Crop(_) => "crop",
            NodeConfig::Filter(_) => "filter",
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
    /// Where it runs; each field set here overrides the plugin's
    /// `execution` setting.
    #[serde(default)]
    pub execution: Option<Execution>,
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
    /// Values per keypoint: 2 (x, y) or 3 (x, y, score).
    pub dims: usize,
    /// How coordinates are made relative.
    #[serde(default)]
    pub normalize: Normalize,
    /// The keypoints used (default: all), by index.
    #[serde(default)]
    pub keypoints: Option<Vec<usize>>,
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
