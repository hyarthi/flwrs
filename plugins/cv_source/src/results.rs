//! What a model node produces: one kind of result per port type.
//! Coordinates are model-input pixels out of a decoder, frame pixels once
//! the engine has mapped them ([`Results::map_to_frame`]).

use crate::frame::Affine;
use crate::graph::PortType;

/// A point of an object, with its confidence (or visibility), 0..1.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Keypoint {
    pub x: f32,
    pub y: f32,
    pub confidence: f32,
}

/// An object's `children` key for what a hand's landmarks model reports
/// beyond its keypoints, for a gesture embedder: `depth`, `world`, `right`.
pub const HAND_POSE: &str = "hand_pose";

/// MediaPipe's scale for a hand model's depth (`normalize_z`).
const HAND_DEPTH_SCALE: f32 = 0.4;

/// A detected object.
#[derive(Clone, Debug, PartialEq)]
pub struct Object {
    /// Its track, from a tracker node (`None`: not tracked, or not confirmed).
    pub tracker_id: Option<u64>,
    pub class_id: u32,
    /// From the model's labels, if it has them.
    pub class_name: Option<String>,
    /// 0..1.
    pub confidence: f32,
    /// `[x1, y1, x2, y2]`.
    pub bbox: [f32; 4],
    pub keypoints: Vec<Keypoint>,
    /// What other model nodes found about it, by node id, in run order:
    /// the models run on its region, or on its keypoints.
    pub children: Vec<(String, Results)>,
}

/// A class with its score (classifiers).
#[derive(Clone, Debug, PartialEq)]
pub struct Class {
    pub class_id: u32,
    pub class_name: Option<String>,
    pub confidence: f32,
}

/// A per-pixel map in model-input pixels (`width` x `height`) and its
/// summary.
#[derive(Clone, Debug, PartialEq)]
pub struct Map {
    pub width: usize,
    pub height: usize,
    /// Class ids (segmentation) or values (depth, alpha), row-major.
    pub values: Vec<f32>,
    pub summary: MapSummary,
}

#[derive(Clone, Debug, PartialEq)]
pub enum MapSummary {
    /// The classes covering most of the map: (class, share 0..1), largest first.
    Classes(Vec<(Class, f32)>),
    /// Value statistics; `above_threshold` is the share over the decoder's
    /// threshold, if it has one.
    Values {
        min: f32,
        max: f32,
        mean: f32,
        above_threshold: Option<f32>,
    },
}

/// Recognised text and its confidence (OCR).
#[derive(Clone, Debug, PartialEq)]
pub struct Text {
    pub text: String,
    pub confidence: f32,
}

/// A model node's result.
#[derive(Clone, Debug, PartialEq)]
pub enum Results {
    Objects(Vec<Object>),
    Classes(Vec<Class>),
    Map(Map),
    /// None: nothing read, or below the model's cutoff.
    Text(Option<Text>),
    /// An embedding, L2-normalised.
    Vector(Vec<f32>),
    /// Raw tensors: (name, shape, values).
    Tensor(Vec<(String, Vec<usize>, Vec<f32>)>),
}

impl Results {
    /// Nothing, of a port type (a node that didn't run or failed).
    pub fn empty(port: PortType) -> Results {
        match port {
            PortType::Classes => Results::Classes(vec![]),
            PortType::Map => Results::Map(Map {
                width: 0,
                height: 0,
                values: vec![],
                summary: MapSummary::Values {
                    min: 0.0,
                    max: 0.0,
                    mean: 0.0,
                    above_threshold: None,
                },
            }),
            PortType::Text => Results::Text(None),
            PortType::Vector => Results::Vector(vec![]),
            PortType::Tensor => Results::Tensor(vec![]),
            PortType::Objects | PortType::Frame | PortType::Regions | PortType::Events => {
                Results::Objects(vec![])
            }
        }
    }

    pub fn port_type(&self) -> PortType {
        match self {
            Results::Objects(_) => PortType::Objects,
            Results::Classes(_) => PortType::Classes,
            Results::Map(_) => PortType::Map,
            Results::Text(_) => PortType::Text,
            Results::Vector(_) => PortType::Vector,
            Results::Tensor(_) => PortType::Tensor,
        }
    }

    /// Model-input pixels → frame pixels (boxes and keypoints; maps stay in
    /// model pixels, with the affine kept by the caller).
    pub fn map_to_frame(&mut self, affine: &Affine) {
        if let Results::Objects(objects) = self {
            for o in objects {
                o.bbox = affine.map_box(o.bbox);
                for k in &mut o.keypoints {
                    (k.x, k.y) = affine.apply(k.x, k.y);
                }
                let pose = o.children.iter_mut().find(|(id, _)| id == HAND_POSE);
                if let Some((_, Results::Tensor(parts))) = pose {
                    let scale = affine.a.hypot(affine.d);
                    for (name, _, values) in parts {
                        match name.as_str() {
                            // In frame pixels, like x.
                            "depth" => values.iter_mut().for_each(|z| {
                                *z *= scale / HAND_DEPTH_SCALE;
                            }),
                            // Metres: turned with the region, not scaled.
                            "world" if scale > 0.0 => {
                                for p in values.as_chunks_mut::<3>().0 {
                                    let (x, y) = (p[0], p[1]);
                                    p[0] = (affine.a * x + affine.b * y) / scale;
                                    p[1] = (affine.d * x + affine.e * y) / scale;
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
    }

    /// Makes the results safe to pass on, whatever the model produced:
    /// confidences finite and within 0..1, objects with a box that isn't
    /// finite dropped, keypoints that aren't finite set to (0, 0) with
    /// confidence 0.
    pub fn sanitize(&mut self) {
        let unit = |v: f32| {
            if v.is_finite() {
                v.clamp(0.0, 1.0)
            } else {
                0.0
            }
        };
        match self {
            Results::Objects(objects) => {
                objects.retain(|o| o.bbox.iter().all(|v| v.is_finite()));
                for o in objects {
                    o.confidence = unit(o.confidence);
                    let [x1, y1, x2, y2] = o.bbox;
                    o.bbox = [x1.min(x2), y1.min(y2), x1.max(x2), y1.max(y2)];
                    for k in &mut o.keypoints {
                        if k.x.is_finite() && k.y.is_finite() {
                            k.confidence = unit(k.confidence);
                        } else {
                            *k = Keypoint {
                                x: 0.0,
                                y: 0.0,
                                confidence: 0.0,
                            };
                        }
                    }
                }
            }
            Results::Classes(classes) => {
                for c in classes {
                    c.confidence = unit(c.confidence);
                }
            }
            Results::Map(map) => {
                for v in &mut map.values {
                    if !v.is_finite() {
                        *v = 0.0;
                    }
                }
                match &mut map.summary {
                    MapSummary::Classes(classes) => {
                        for (c, share) in classes {
                            c.confidence = unit(c.confidence);
                            *share = unit(*share);
                        }
                    }
                    MapSummary::Values {
                        min,
                        max,
                        mean,
                        above_threshold,
                    } => {
                        for v in [min, max, mean] {
                            if !v.is_finite() {
                                *v = 0.0;
                            }
                        }
                        if let Some(a) = above_threshold {
                            *a = unit(*a);
                        }
                    }
                }
            }
            Results::Text(Some(t)) => t.confidence = unit(t.confidence),
            Results::Text(None) => {}
            Results::Vector(v) => {
                if v.iter().any(|x| !x.is_finite()) {
                    v.iter_mut().for_each(|x| *x = 0.0);
                }
            }
            Results::Tensor(_) => {}
        }
    }
}
