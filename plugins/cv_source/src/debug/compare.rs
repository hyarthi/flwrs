//! Where a device's results differ from the CPU's, for `--compare-cpu`.

use crate::results::{Class, MapSummary, Object, Results};

/// Boxes overlapping less than this are different boxes.
const MIN_IOU: f32 = 0.8;
/// Confidences further apart than this differ.
const MAX_CONFIDENCE_GAP: f32 = 0.1;
/// Keypoints further than this share of their object's size differ.
const MAX_KEYPOINT_SHIFT: f32 = 0.05;
/// A map's statistics further apart than this share of their range differ.
const MAX_MAP_SHIFT: f32 = 0.05;

fn iou(a: [f32; 4], b: [f32; 4]) -> f32 {
    let w = (a[2].min(b[2]) - a[0].max(b[0])).max(0.0);
    let h = (a[3].min(b[3]) - a[1].max(b[1])).max(0.0);
    let inter = w * h;
    let union = (a[2] - a[0]) * (a[3] - a[1]) + (b[2] - b[0]) * (b[3] - b[1]) - inter;
    if union > 0.0 { inter / union } else { 1.0 }
}

fn objects(node: &str, device: &[Object], cpu: &[Object], out: &mut Vec<String>) {
    if device.len() != cpu.len() {
        out.push(format!(
            "{node}: {} objects, the CPU finds {}",
            device.len(),
            cpu.len()
        ));
    }
    let mut free: Vec<&Object> = cpu.iter().collect();
    for (i, o) in device.iter().enumerate() {
        // The best-overlapping CPU object of the same class.
        let best = free
            .iter()
            .enumerate()
            .filter(|(_, c)| c.class_id == o.class_id)
            .map(|(n, c)| (n, iou(o.bbox, c.bbox)))
            .max_by(|a, b| a.1.total_cmp(&b.1));
        let Some((n, overlap)) = best.filter(|(_, v)| *v >= MIN_IOU) else {
            if device.len() == cpu.len() {
                out.push(format!("{node}#{i}: no matching object on the CPU"));
            }
            continue;
        };
        let c = free.remove(n);
        let _ = overlap;
        if (o.confidence - c.confidence).abs() > MAX_CONFIDENCE_GAP {
            out.push(format!(
                "{node}#{i}: confidence {:.2}, the CPU's {:.2}",
                o.confidence, c.confidence
            ));
        }
        if o.keypoints.len() == c.keypoints.len() {
            let size = (o.bbox[2] - o.bbox[0]).max(o.bbox[3] - o.bbox[1]).max(1.0);
            let moved = o
                .keypoints
                .iter()
                .zip(&c.keypoints)
                .filter(|(p, q)| p.confidence >= 0.3 && q.confidence >= 0.3)
                .filter(|(p, q)| (p.x - q.x).hypot(p.y - q.y) > size * MAX_KEYPOINT_SHIFT)
                .count();
            if moved > 0 {
                out.push(format!("{node}#{i}: {moved} keypoints moved"));
            }
        } else {
            out.push(format!(
                "{node}#{i}: {} keypoints, the CPU's {}",
                o.keypoints.len(),
                c.keypoints.len()
            ));
        }
        for (child, r) in &o.children {
            if let Some((_, other)) = c.children.iter().find(|(id, _)| id == child) {
                compare(&format!("{node}#{i}/{child}"), r, other, out);
            }
        }
    }
}

fn classes(node: &str, device: &[Class], cpu: &[Class], out: &mut Vec<String>) {
    match (device.first(), cpu.first()) {
        (Some(a), Some(b)) if a.class_id != b.class_id => out.push(format!(
            "{node}: top class {}, the CPU's {}",
            a.class_name
                .clone()
                .unwrap_or_else(|| a.class_id.to_string()),
            b.class_name
                .clone()
                .unwrap_or_else(|| b.class_id.to_string()),
        )),
        (Some(a), Some(b)) if (a.confidence - b.confidence).abs() > MAX_CONFIDENCE_GAP => {
            out.push(format!(
                "{node}: top confidence {:.2}, the CPU's {:.2}",
                a.confidence, b.confidence
            ));
        }
        (None, Some(_)) | (Some(_), None) => out.push(format!(
            "{node}: {} classes, the CPU finds {}",
            device.len(),
            cpu.len()
        )),
        _ => {}
    }
}

/// Adds what differs between a device's result and the CPU's.
pub fn compare(node: &str, device: &Results, cpu: &Results, out: &mut Vec<String>) {
    match (device, cpu) {
        (Results::Objects(a), Results::Objects(b)) => objects(node, a, b, out),
        (Results::Classes(a), Results::Classes(b)) => classes(node, a, b, out),
        (Results::Text(a), Results::Text(b)) => {
            if a.as_ref().map(|t| &t.text) != b.as_ref().map(|t| &t.text) {
                let show = |t: &Option<crate::results::Text>| {
                    t.as_ref()
                        .map_or("nothing".to_string(), |t| format!("{:?}", t.text))
                };
                out.push(format!("{node}: read {}, the CPU {}", show(a), show(b)));
            }
        }
        (Results::Vector(a), Results::Vector(b)) => {
            let similarity: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
            if a.len() != b.len() || similarity < 0.95 {
                out.push(format!(
                    "{node}: embedding similarity to the CPU's {similarity:.2}"
                ));
            }
        }
        (Results::Map(a), Results::Map(b)) => match (&a.summary, &b.summary) {
            (MapSummary::Classes(x), MapSummary::Classes(y)) => {
                if x.first().map(|(c, _)| c.class_id) != y.first().map(|(c, _)| c.class_id) {
                    out.push(format!(
                        "{node}: the map's main class differs from the CPU's"
                    ));
                }
            }
            (
                MapSummary::Values {
                    min: a0,
                    max: a1,
                    mean: am,
                    ..
                },
                MapSummary::Values {
                    min: b0,
                    max: b1,
                    mean: bm,
                    ..
                },
            ) => {
                let range = (b1 - b0).abs().max(1e-6);
                if (am - bm).abs() > range * MAX_MAP_SHIFT
                    || (a1 - b1).abs() > range * MAX_MAP_SHIFT * 2.0
                    || (a0 - b0).abs() > range * MAX_MAP_SHIFT * 2.0
                {
                    out.push(format!("{node}: map mean {am:.2}, the CPU's {bm:.2}"));
                }
            }
            _ => out.push(format!("{node}: a different map type from the CPU's")),
        },
        (Results::Tensor(_), Results::Tensor(_)) => {}
        _ => out.push(format!("{node}: a different result type from the CPU's")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::debug::facts::tests::person;

    fn diff(a: Results, b: Results) -> Vec<String> {
        let mut out = vec![];
        compare("n", &a, &b, &mut out);
        out
    }

    #[test]
    fn the_same_results_have_no_differences() {
        let r = Results::Objects(vec![person()]);
        assert_eq!(diff(r.clone(), r), Vec::<String>::new());
    }

    #[test]
    fn differences_are_named() {
        let a = person();
        let mut far = a.clone();
        far.confidence = 0.4;
        assert!(
            diff(
                Results::Objects(vec![a.clone()]),
                Results::Objects(vec![far])
            )[0]
            .contains("confidence")
        );
        assert!(
            diff(Results::Objects(vec![a.clone()]), Results::Objects(vec![]))[0]
                .contains("the CPU finds 0")
        );
        let mut moved = a.clone();
        moved.keypoints[0].x += 30.0;
        assert!(
            diff(
                Results::Objects(vec![a.clone()]),
                Results::Objects(vec![moved])
            )[0]
            .contains("keypoints moved")
        );
        let mut gesture = a.clone();
        if let Results::Classes(c) = &mut gesture.children[0].1 {
            c[0].class_id = 9;
        }
        let d = diff(Results::Objects(vec![a]), Results::Objects(vec![gesture]));
        assert!(d[0].starts_with("n#0/gesture: top class"), "{d:?}");
    }

    #[test]
    fn small_changes_are_tolerated() {
        let a = person();
        let mut near = a.clone();
        near.confidence += 0.02;
        near.bbox[2] += 0.5;
        near.keypoints[0].x += 0.5;
        assert!(diff(Results::Objects(vec![a]), Results::Objects(vec![near])).is_empty());
    }
}
