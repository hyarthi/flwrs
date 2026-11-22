//! Decoder settings: how a model's output tensors become results. The
//! decoder belongs to the model *export*, not its architecture (the same
//! YOLO26n exports as end-to-end rows or as DETR-style queries), so it's set
//! per model node. Each decoder produces one result type, which is the model
//! node's output port.
//!
//! Output names are the model's own tensor names. Coordinates come out in
//! model-input pixels; the engine maps them to frame pixels.
//!
//! The confidence cutoff is the model node's (`min_confidence`), passed in
//! the [`Context`]: a decoder drops what's below it as soon as it knows the
//! confidence (before NMS, before reading keypoints), so nothing downstream
//! sees it.

use crate::graph::PortType;
use serde::{Deserialize, Serialize};

#[cfg(test)]
pub(crate) mod fixtures;
mod run;
#[cfg(test)]
mod tests;
pub mod util;

pub use util::Context;

fn d_iou() -> f32 {
    0.45
}

fn d_topk() -> usize {
    5
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DetrActivation {
    /// Independent per-class scores (RT-DETR, RF-DETR, D-FINE).
    Sigmoid,
    /// Softmax over classes + a last "no object" class (the original DETR).
    SoftmaxNoObject,
    /// Already probabilities.
    None,
}

/// A decoder, by `type`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Decoder {
    /// Single-label classification: logits or probabilities `[1, C]` → top-k.
    Classify {
        output: String,
        softmax: bool,
        #[serde(default = "d_topk")]
        topk: usize,
    },
    /// YOLO raw (one-to-many) predictions: rows of box (+ objectness) +
    /// class scores (+ keypoints) → NMS. v5 / v7 / v8 / 11 / 26 raw, YOLOX
    /// (with `grid`), YOLO-pose (with `keypoints`).
    Yolo {
        output: String,
        /// true: `[1, C, N]` (v8+ raw); false: `[1, N, C]` (v5, YOLOX).
        channels_first: bool,
        classes: usize,
        #[serde(default)]
        objectness: bool,
        #[serde(default)]
        keypoints: usize,
        /// Raw grid outputs (YOLOX): the strides to decode cells with.
        #[serde(default)]
        grid: Vec<u32>,
        #[serde(default = "d_iou")]
        iou: f32,
    },
    /// End-to-end detections, already decoded: rows of `x1 y1 x2 y2 score
    /// class` (YOLOv10, YOLO26 end-to-end, exports with NMS inside).
    DetectionsE2e { output: String },
    /// DETR family: per-query class logits + `cxcywh` boxes normalised to
    /// the input; no NMS.
    Detr {
        logits: String,
        boxes: String,
        activation: DetrActivation,
    },
    /// GFL heads (NanoDet-Plus): per stride class scores + a distribution
    /// over `reg_max + 1` bins per side → NMS.
    Gfl {
        cls: Vec<String>,
        reg: Vec<String>,
        strides: Vec<u32>,
        reg_max: usize,
        #[serde(default = "d_iou")]
        iou: f32,
    },
    /// Anchor-based SSD with generated anchors, MediaPipe-style (palm,
    /// BlazePose and BlazeFace detectors): boxes + keypoints per anchor.
    SsdAnchors {
        boxes: String,
        scores: String,
        /// One entry per layer; layers with the same stride share a grid.
        strides: Vec<u32>,
        keypoints: usize,
        #[serde(default = "d_iou")]
        iou: f32,
    },
    /// YuNet face detection: per stride cls / obj / bbox / kps outputs on a
    /// prior grid.
    Yunet {
        strides: Vec<u32>,
        keypoints: usize,
        #[serde(default = "d_iou")]
        iou: f32,
    },
    /// MoveNet: single `[1, 1, 17, 3]` or multi-person `[1, 6, 56]`, `(y, x,
    /// score)` normalised.
    Movenet { output: String, multi: bool },
    /// RTMO (one-stage pose): dets `[1, N, 5]` + keypoints `[1, N, K, 3]`.
    Rtmo { dets: String, keypoints: String },
    /// SimCC (RTMPose, RTMW): per-axis classification vectors, `split` bins
    /// per pixel.
    Simcc { x: String, y: String, split: f32 },
    /// Heatmaps `[1, K, H, W]` (HRNet, ViTPose): argmax + sub-pixel refinement.
    Heatmap { output: String },
    /// Directly regressed landmarks in input pixels (MediaPipe hand, pose,
    /// face mesh): `dims` values per point; the score from `visibility_index`
    /// (sigmoid) if any; an optional presence output.
    Landmarks {
        output: String,
        points: usize,
        dims: usize,
        #[serde(default)]
        visibility_index: Option<usize>,
        #[serde(default)]
        presence: Option<String>,
        #[serde(default)]
        presence_sigmoid: bool,
    },
    /// Semantic segmentation logits `[1, C, H, W]` → class ids.
    Semantic { output: String },
    /// A single-channel map (alpha matte, depth): values + summary statistics.
    DenseMap {
        output: String,
        #[serde(default)]
        threshold: Option<f32>,
    },
    /// DB text detection: a probability map → text boxes.
    OcrDet {
        output: String,
        threshold: f32,
        min_area: usize,
    },
    /// CTC sequence decoding with a charset (blank = 0).
    Ctc { output: String },
    /// Embeddings `[1, D]`, L2-normalised (re-id for tracking).
    Embedding { output: String },
    /// Selected output tensors as they are, for a model-to-model link or a
    /// rule.
    Raw { outputs: Vec<String> },
}

impl Decoder {
    /// The result type it produces (the model node's output port).
    pub fn result_type(&self) -> PortType {
        match self {
            Decoder::Classify { .. } => PortType::Classes,
            Decoder::Yolo { .. }
            | Decoder::DetectionsE2e { .. }
            | Decoder::Detr { .. }
            | Decoder::Gfl { .. }
            | Decoder::SsdAnchors { .. }
            | Decoder::Yunet { .. }
            | Decoder::Movenet { .. }
            | Decoder::Rtmo { .. }
            | Decoder::Simcc { .. }
            | Decoder::Heatmap { .. }
            | Decoder::Landmarks { .. }
            | Decoder::OcrDet { .. } => PortType::Objects,
            Decoder::Semantic { .. } | Decoder::DenseMap { .. } => PortType::Map,
            Decoder::Ctc { .. } => PortType::Text,
            Decoder::Embedding { .. } => PortType::Vector,
            Decoder::Raw { .. } => PortType::Tensor,
        }
    }

    /// How many keypoints each object has, where the settings say (other
    /// decoders take it from the model's output).
    pub fn keypoint_count(&self) -> Option<usize> {
        match self {
            Decoder::Yolo { keypoints, .. }
            | Decoder::SsdAnchors { keypoints, .. }
            | Decoder::Yunet { keypoints, .. } => Some(*keypoints),
            Decoder::Landmarks { points, .. } => Some(*points),
            Decoder::Movenet { .. } => Some(17),
            _ => None,
        }
    }

    /// The confidence cutoff when the model node doesn't set one; None when
    /// the results have no confidence (maps, embeddings, raw tensors), so
    /// none can be set.
    pub fn default_min_confidence(&self) -> Option<f32> {
        match self {
            Decoder::Yolo { .. }
            | Decoder::DetectionsE2e { .. }
            | Decoder::Detr { .. }
            | Decoder::Gfl { .. }
            | Decoder::SsdAnchors { .. }
            | Decoder::Yunet { .. }
            | Decoder::Movenet { .. }
            | Decoder::Rtmo { .. } => Some(0.25),
            Decoder::Classify { .. }
            | Decoder::Simcc { .. }
            | Decoder::Heatmap { .. }
            | Decoder::Landmarks { .. }
            | Decoder::OcrDet { .. }
            | Decoder::Ctc { .. } => Some(0.0),
            Decoder::Semantic { .. }
            | Decoder::DenseMap { .. }
            | Decoder::Embedding { .. }
            | Decoder::Raw { .. } => None,
        }
    }

    /// Its `type`.
    pub fn type_name(&self) -> String {
        serde_json::to_value(self)
            .ok()
            .and_then(|v| v["type"].as_str().map(String::from))
            .unwrap_or_default()
    }

    /// Whether its results need a CTC charset (`charset`).
    pub fn needs_charset(&self) -> bool {
        matches!(self, Decoder::Ctc { .. })
    }

    /// The settings' own problems (thresholds in range, sizes not zero).
    pub fn problems(&self) -> Vec<String> {
        let mut problems = vec![];
        let mut unit = |name: &str, v: f32| {
            if !(0.0..=1.0).contains(&v) {
                problems.push(format!("decode.{name} must be between 0 and 1"));
            }
        };
        match self {
            Decoder::Yolo { iou, .. }
            | Decoder::Gfl { iou, .. }
            | Decoder::SsdAnchors { iou, .. }
            | Decoder::Yunet { iou, .. } => unit("iou", *iou),
            Decoder::OcrDet { threshold, .. } => unit("threshold", *threshold),
            Decoder::DenseMap {
                threshold: Some(t), ..
            } if !t.is_finite() => {
                problems.push("decode.threshold must be a finite number".into());
            }
            _ => {}
        }
        match self {
            Decoder::Classify { topk: 0, .. } => {
                problems.push("decode.topk must be at least 1".into())
            }
            Decoder::Yolo { classes: 0, .. } => {
                problems.push("decode.classes must be at least 1".into())
            }
            Decoder::Gfl {
                cls, reg, strides, ..
            } if cls.len() != strides.len() || reg.len() != strides.len() => problems
                .push("decode.cls, decode.reg and decode.strides must have the same length".into()),
            Decoder::SsdAnchors { strides, .. } | Decoder::Yunet { strides, .. }
                if strides.is_empty() =>
            {
                problems.push("decode.strides must not be empty".into())
            }
            Decoder::Simcc { split, .. } if !(*split > 0.0 && split.is_finite()) => {
                problems.push("decode.split must be a positive number".into())
            }
            Decoder::Landmarks { points: 0, .. } | Decoder::Landmarks { dims: 0, .. } => {
                problems.push("decode.points and decode.dims must be at least 1".into())
            }
            Decoder::Landmarks {
                dims,
                visibility_index: Some(v),
                ..
            } if v >= dims => problems.push(format!(
                "decode.visibility_index must be below decode.dims ({dims})"
            )),
            Decoder::Raw { outputs, .. } if outputs.is_empty() => {
                problems.push("decode.outputs must not be empty".into())
            }
            _ => {}
        }
        problems
    }
}
