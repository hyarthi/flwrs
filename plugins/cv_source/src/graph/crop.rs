//! The crop and filter nodes: which objects they take, and a crop's region
//! of the frame per object.

use super::config::{ClassRef, CropConfig, FilterConfig};
use crate::frame::Rect;
use crate::results::Object;
use std::f32::consts::PI;

/// A region a crop node cut: where, and the index of its object in the
/// crop's input.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Region {
    pub rect: Rect,
    pub parent: usize,
}

fn class_matches(classes: &[ClassRef], o: &Object) -> bool {
    classes.is_empty()
        || classes.iter().any(|c| match c {
            ClassRef::Id(id) => o.class_id == *id,
            ClassRef::Label(l) => o.class_name.as_deref() == Some(l.as_str()),
        })
}

/// Indices of the objects of these classes, the highest confidence first
/// (ties in input order), at most `max`.
fn select(objects: &[Object], classes: &[ClassRef], min_confidence: f32, max: usize) -> Vec<usize> {
    let mut idx: Vec<usize> = (0..objects.len())
        .filter(|&i| class_matches(classes, &objects[i]) && objects[i].confidence >= min_confidence)
        .collect();
    idx.sort_by(|a, b| objects[*b].confidence.total_cmp(&objects[*a].confidence));
    idx.truncate(max);
    idx
}

/// A filter node: the objects it keeps (in its order: highest confidence
/// first).
pub fn filter(f: &FilterConfig, objects: &[Object]) -> Vec<Object> {
    select(
        objects,
        &f.classes,
        f.min_confidence.unwrap_or(0.0),
        f.max.unwrap_or(usize::MAX),
    )
    .into_iter()
    .map(|i| objects[i].clone())
    .collect()
}

/// A crop node: a region per object it takes. Objects without the keypoints
/// its settings name, or whose region has no area, get none.
pub fn regions(c: &CropConfig, objects: &[Object]) -> Vec<Region> {
    select(objects, &c.classes, 0.0, c.max)
        .into_iter()
        .filter_map(|i| {
            Some(Region {
                rect: rect(c, &objects[i])?,
                parent: i,
            })
        })
        .collect()
}

/// The region for one object (MediaPipe-style regions of interest from
/// keypoints included).
pub fn rect(c: &CropConfig, o: &Object) -> Option<Rect> {
    let kp = |i: usize| o.keypoints.get(i).map(|k| (k.x, k.y));
    let (mut cx, mut cy) = ((o.bbox[0] + o.bbox[2]) / 2.0, (o.bbox[1] + o.bbox[3]) / 2.0);
    let (mut w, mut h) = (o.bbox[2] - o.bbox[0], o.bbox[3] - o.bbox[1]);
    if let Some(k) = c.center_keypoint {
        (cx, cy) = kp(k)?;
    }
    if let Some([a, b]) = c.size_keypoints {
        let ((ax, ay), (bx, by)) = (kp(a)?, kp(b)?);
        let d = ((ax - bx).powi(2) + (ay - by).powi(2)).sqrt() * 2.0;
        (w, h) = (d, d);
    }
    let mut rotation = 0.0;
    if let Some(r) = c.rotate {
        let ((x0, y0), (x1, y1)) = (kp(r.from)?, kp(r.to)?);
        rotation = r.angle.to_radians() - (-(y1 - y0)).atan2(x1 - x0);
        rotation -= 2.0 * PI * ((rotation + PI) / (2.0 * PI)).floor();
    }
    if c.square {
        let m = w.max(h);
        (w, h) = (m, m);
    }
    if let Some(aspect) = c.aspect {
        if w / h > aspect {
            h = w / aspect;
        } else {
            w = h * aspect;
        }
    }
    // Shift along the region's own y axis, then scale.
    let (sin, cos) = rotation.sin_cos();
    cx += -sin * c.shift_y * h;
    cy += cos * c.shift_y * h;
    let rect = Rect {
        cx,
        cy,
        w: w * c.scale,
        h: h * c.scale,
        rotation,
    };
    rect.is_usable().then_some(rect)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::config::Rotate;
    use crate::results::Keypoint;

    fn obj(class_id: u32, confidence: f32, bbox: [f32; 4], kps: &[(f32, f32)]) -> Object {
        Object {
            class_id,
            class_name: Some(["person", "dog"][class_id as usize % 2].into()),
            confidence,
            bbox,
            keypoints: kps
                .iter()
                .map(|&(x, y)| Keypoint {
                    x,
                    y,
                    confidence: 1.0,
                })
                .collect(),
            children: vec![],
        }
    }

    fn crop(v: serde_json::Value) -> CropConfig {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn objects_are_chosen_by_class_and_confidence() {
        let objects = [
            obj(0, 0.5, [0.0, 0.0, 10.0, 10.0], &[]),
            obj(1, 0.9, [0.0, 0.0, 10.0, 10.0], &[]),
            obj(0, 0.7, [0.0, 0.0, 10.0, 10.0], &[]),
        ];
        let parents = |c: CropConfig| {
            regions(&c, &objects)
                .iter()
                .map(|r| r.parent)
                .collect::<Vec<_>>()
        };
        assert_eq!(parents(crop(serde_json::json!({}))), [1, 2, 0]);
        assert_eq!(
            parents(crop(serde_json::json!({"classes": ["person"]}))),
            [2, 0]
        );
        assert_eq!(
            parents(crop(serde_json::json!({"classes": [1, 0], "max": 2}))),
            [1, 2]
        );
        let f: FilterConfig =
            serde_json::from_value(serde_json::json!({"min_confidence": 0.6})).unwrap();
        assert_eq!(
            filter(&f, &objects)
                .iter()
                .map(|o| o.confidence)
                .collect::<Vec<_>>(),
            [0.9, 0.7]
        );
    }

    #[test]
    fn regions_from_boxes() {
        let o = obj(0, 1.0, [10.0, 20.0, 30.0, 60.0], &[]);
        let r = rect(&crop(serde_json::json!({})), &o).unwrap();
        assert_eq!(
            (r.cx, r.cy, r.w, r.h, r.rotation),
            (20.0, 40.0, 20.0, 40.0, 0.0)
        );
        let r = rect(&crop(serde_json::json!({"square": true, "scale": 1.5})), &o).unwrap();
        assert_eq!((r.w, r.h), (60.0, 60.0));
        // Top-down pose: w / h = 0.75, the short side grown.
        let r = rect(&crop(serde_json::json!({"aspect": 0.75})), &o).unwrap();
        assert_eq!((r.w, r.h), (30.0, 40.0));
        let r = rect(&crop(serde_json::json!({"shift_y": -0.5})), &o).unwrap();
        assert_eq!(r.cy, 20.0);
        assert!(
            rect(
                &crop(serde_json::json!({})),
                &obj(0, 1.0, [5.0, 5.0, 5.0, 9.0], &[])
            )
            .is_none()
        );
    }

    #[test]
    fn regions_from_keypoints_rotate() {
        // A wrist at (50, 50) and a middle-finger base straight right of it
        // at (60, 50): turned to point up (90 degrees), a quarter turn.
        let o = obj(
            0,
            1.0,
            [40.0, 40.0, 60.0, 60.0],
            &[(50.0, 50.0), (55.0, 45.0), (60.0, 50.0)],
        );
        let c = CropConfig {
            rotate: Some(Rotate {
                from: 0,
                to: 2,
                angle: 90.0,
            }),
            center_keypoint: Some(0),
            size_keypoints: Some([0, 2]),
            ..crop(serde_json::json!({}))
        };
        let r = rect(&c, &o).unwrap();
        assert!(
            (r.rotation - std::f32::consts::FRAC_PI_2).abs() < 1e-5,
            "{}",
            r.rotation
        );
        assert_eq!((r.cx, r.cy, r.w, r.h), (50.0, 50.0, 20.0, 20.0));
        // Without the keypoints it names: no region.
        assert!(rect(&c, &obj(0, 1.0, [0.0, 0.0, 9.0, 9.0], &[(1.0, 1.0)])).is_none());
    }
}
