//! What the decoders share: output lookup with shape checks, NaN-safe
//! arithmetic, and NMS.

use crate::results::Object;
use crate::tensor::{Output, Outputs};

/// The most objects a detection decoder returns.
pub const MAX_DETECTIONS: usize = 300;
/// The most candidates NMS looks at (the highest scores), so a model that
/// scores everything high can't make it quadratic in its output size.
pub const MAX_CANDIDATES: usize = 5_000;

/// What a decoder knows besides the tensors.
pub struct Context<'a> {
    /// The model input's size, in pixels.
    pub width: f32,
    pub height: f32,
    pub labels: &'a [String],
    pub charset: &'a [String],
    /// Results below it are dropped (the model node's cutoff; 0 keeps all).
    pub min_confidence: f32,
}

impl Context<'_> {
    pub fn label(&self, id: u32) -> Option<String> {
        self.labels.get(id as usize).cloned()
    }

    /// The label, or a decoder's own name for its one class.
    pub fn label_or(&self, id: u32, default: &str) -> Option<String> {
        self.label(id).or_else(|| Some(default.to_string()))
    }
}

/// An output by name.
pub fn get<'a>(outputs: &'a Outputs, name: &str) -> Result<&'a Output, String> {
    outputs.get(name).ok_or_else(|| {
        let have: Vec<&str> = outputs.keys().map(String::as_str).collect();
        format!(
            "the model has no output {name:?} (it has: {})",
            have.join(", ")
        )
    })
}

/// An output's shape as `N` dimensions: leading 1s (batch) dropped, or 1s
/// added in front, to fit.
pub fn dims<const N: usize>(out: &Output, name: &str, what: &str) -> Result<[usize; N], String> {
    let mut shape: Vec<usize> = out.shape().to_vec();
    while shape.len() > N && shape[0] == 1 {
        shape.remove(0);
    }
    while shape.len() < N {
        shape.insert(0, 1);
    }
    shape.try_into().map_err(|_| {
        format!(
            "output {name:?} has shape {:?}; expected {what}",
            out.shape()
        )
    })
}

/// An error unless `cond`.
pub fn ensure(cond: bool, message: impl FnOnce() -> String) -> Result<(), String> {
    if cond { Ok(()) } else { Err(message()) }
}

/// Whether a score reaches a threshold; NaN never does.
pub fn reaches(score: f32, threshold: f32) -> bool {
    score >= threshold
}

pub fn sigmoid(x: f32) -> f32 {
    1.0 / (1.0 + (-x).exp())
}

/// NaN as the lowest value, for comparisons.
fn key(v: f32) -> f32 {
    if v.is_nan() { f32::NEG_INFINITY } else { v }
}

/// The index and value of the largest value (NaN never wins); None if empty.
pub fn argmax(values: &[f32]) -> Option<(usize, f32)> {
    let mut best: Option<(usize, f32)> = None;
    for (i, &v) in values.iter().enumerate() {
        if best.is_none_or(|(_, b)| key(v) > key(b)) {
            best = Some((i, v));
        }
    }
    best
}

/// Softmax in place (stable; NaN and infinities give zeros, never NaN).
pub fn softmax(values: &mut [f32]) {
    let max = values
        .iter()
        .copied()
        .filter(|v| v.is_finite())
        .fold(f32::NEG_INFINITY, f32::max);
    if !max.is_finite() {
        values.iter_mut().for_each(|v| *v = 0.0);
        return;
    }
    for v in values.iter_mut() {
        *v = if v.is_finite() { (*v - max).exp() } else { 0.0 };
    }
    let sum: f32 = values.iter().sum();
    if sum > 0.0 {
        values.iter_mut().for_each(|v| *v /= sum);
    }
}

/// A class id from a float tensor value (end-to-end rows).
pub fn class_of(v: f32) -> Option<u32> {
    (0.0..1e6).contains(&v).then(|| v.round() as u32)
}

pub fn cxcywh(cx: f32, cy: f32, w: f32, h: f32) -> [f32; 4] {
    [cx - w / 2.0, cy - h / 2.0, cx + w / 2.0, cy + h / 2.0]
}

fn iou(a: &[f32; 4], b: &[f32; 4]) -> f32 {
    let ix = (a[2].min(b[2]) - a[0].max(b[0])).max(0.0);
    let iy = (a[3].min(b[3]) - a[1].max(b[1])).max(0.0);
    let inter = ix * iy;
    let union = (a[2] - a[0]) * (a[3] - a[1]) + (b[2] - b[0]) * (b[3] - b[1]) - inter;
    if union > 0.0 { inter / union } else { 0.0 }
}

/// Greedy per-class NMS, the highest confidence first; at most `max` kept.
/// Objects whose confidence or box isn't finite are dropped first.
pub fn nms(mut objects: Vec<Object>, threshold: f32, max: usize) -> Vec<Object> {
    objects.retain(|o| o.confidence.is_finite() && o.bbox.iter().all(|v| v.is_finite()));
    objects.sort_by(|a, b| b.confidence.total_cmp(&a.confidence));
    objects.truncate(MAX_CANDIDATES);
    let mut keep: Vec<Object> = vec![];
    for o in objects {
        if keep.len() == max {
            break;
        }
        if keep
            .iter()
            .all(|k| k.class_id != o.class_id || iou(&k.bbox, &o.bbox) < threshold)
        {
            keep.push(o);
        }
    }
    keep
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(class_id: u32, confidence: f32, bbox: [f32; 4]) -> Object {
        Object {
            class_id,
            class_name: None,
            confidence,
            bbox,
            keypoints: vec![],
            children: vec![],
        }
    }

    #[test]
    fn helpers_are_nan_safe() {
        assert_eq!(argmax(&[f32::NAN, 1.0, 3.0, f32::NAN]), Some((2, 3.0)));
        assert_eq!(argmax(&[]), None);
        let mut v = [1.0, f32::NAN, 1.0];
        softmax(&mut v);
        assert_eq!(v, [0.5, 0.0, 0.5]);
        let mut v = [f32::NAN; 2];
        softmax(&mut v);
        assert_eq!(v, [0.0, 0.0]);
        assert_eq!(class_of(2.0), Some(2));
        assert_eq!(class_of(-1.0), None);
        assert_eq!(class_of(f32::INFINITY), None);
    }

    #[test]
    fn nms_keeps_the_best_per_class() {
        let kept = nms(
            vec![
                obj(0, 0.5, [0.0, 0.0, 10.0, 10.0]),
                obj(0, 0.9, [1.0, 1.0, 10.0, 10.0]),
                obj(1, 0.4, [0.0, 0.0, 10.0, 10.0]),
                obj(0, 0.3, [50.0, 50.0, 60.0, 60.0]),
                obj(0, f32::NAN, [0.0, 0.0, 1.0, 1.0]),
            ],
            0.5,
            10,
        );
        let scores: Vec<f32> = kept.iter().map(|o| o.confidence).collect();
        assert_eq!(scores, [0.9, 0.4, 0.3]);
    }

    #[test]
    fn shapes_fit_or_say_why() {
        let out = Output::new(vec![1, 2, 3], vec![0.0; 6]).unwrap();
        assert_eq!(dims::<2>(&out, "o", "[N, C]").unwrap(), [2, 3]);
        assert_eq!(dims::<4>(&out, "o", "").unwrap(), [1, 1, 2, 3]);
        assert!(
            dims::<1>(&out, "o", "[C]")
                .unwrap_err()
                .contains("[1, 2, 3]")
        );
    }
}
