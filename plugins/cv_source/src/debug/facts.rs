//! What's known about a frame, as text and shapes: the same facts for the
//! window's overlay and the console.

use crate::analytics::rules::RuleEvent;
use crate::frame::Affine;
use crate::graph::config::{GraphConfig, NodeConfig, RuleType};
use crate::graph::exec::Tick;
use crate::keypoints::{self, KeypointLayout};
use crate::results::{Class, Keypoint, Map, MapSummary, Object, Results};
use serde_json::{Value, json};
use std::collections::BTreeMap;

/// Keypoint layouts by model node.
pub type Layouts = BTreeMap<String, KeypointLayout>;

/// The layouts a graph's model nodes declare.
pub fn layouts_of(graph: &GraphConfig) -> Layouts {
    graph
        .nodes
        .iter()
        .filter_map(|(id, node)| match node {
            NodeConfig::Model(m) => {
                let name = m.keypoint_layout.as_deref()?;
                Some((id.clone(), keypoints::layout(name)?))
            }
            _ => None,
        })
        .collect()
}

/// A rule's line or zone, in normalised frame coordinates.
#[derive(Clone, Debug, PartialEq)]
pub struct RuleShape {
    pub name: String,
    pub points: Vec<[f32; 2]>,
    pub closed: bool,
}

/// The lines and zones of a graph's rules.
pub fn shapes_of(graph: &GraphConfig) -> Vec<RuleShape> {
    graph
        .nodes
        .iter()
        .filter_map(|(id, node)| {
            let NodeConfig::Rule(r) = node else {
                return None;
            };
            let (points, closed) = match &r.rule {
                RuleType::LineCrossing { line, .. } => (line.clone(), false),
                RuleType::Zone { zone, .. } | RuleType::Loitering { zone, .. } => {
                    (zone.clone(), true)
                }
                RuleType::Counting { zone: Some(z), .. } => (z.clone(), true),
                _ => return None,
            };
            Some(RuleShape {
                name: r.name.clone().unwrap_or_else(|| id.clone()),
                points,
                closed,
            })
        })
        .collect()
}

/// An event as one line.
pub fn event_line(e: &RuleEvent) -> String {
    let mut text = format!("{:.1}s {}: {} {}", e.time, e.rule, e.event, e.action);
    if let Some(id) = e.tracker_id {
        text.push_str(&format!(" track {id}"));
    }
    if let Some(c) = &e.class_name {
        text.push_str(&format!(" {c}"));
    }
    match (&e.previous, &e.label) {
        (Some(p), Some(l)) => text.push_str(&format!(" {p} -> {l}")),
        (None, Some(l)) => text.push_str(&format!(" {l}")),
        _ => {}
    }
    if let Some(n) = e.count {
        text.push_str(&format!(" count {n}"));
    }
    if let Some(s) = e.score {
        text.push_str(&format!(" {s:.2}"));
    }
    if let Some(d) = e.duration_s {
        text.push_str(&format!(" {d:.1}s"));
    }
    text
}

/// What's known about one detection.
#[derive(Clone, Debug, PartialEq)]
pub struct ObjectFacts {
    /// `<node>#<index>`.
    pub id: String,
    /// `person 0.93`.
    pub label: String,
    pub class: String,
    /// Picks the colour: its track's, else its class's.
    pub colour_key: String,
    pub bbox: [f32; 4],
    /// What the models run on it found.
    pub lines: Vec<String>,
    pub keypoints: Vec<Keypoint>,
    pub skeleton: Vec<(usize, usize)>,
    pub children: Vec<ObjectFacts>,
}

/// A map to lay over the frame.
#[derive(Clone, Debug, PartialEq)]
pub struct MapLayer {
    pub node: String,
    pub map: Map,
    /// Model-input pixels → frame pixels.
    pub affine: Affine,
}

/// What's known about a frame.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FrameFacts {
    pub objects: Vec<ObjectFacts>,
    /// Results without a box: whole-frame classes, map summaries, text.
    pub notes: Vec<String>,
    pub maps: Vec<MapLayer>,
    pub errors: Vec<String>,
    /// What the rules saw happen on this frame.
    pub events: Vec<RuleEvent>,
}

fn name(class_id: u32, class_name: &Option<String>) -> String {
    class_name
        .clone()
        .unwrap_or_else(|| format!("class {class_id}"))
}

fn class_text(c: &Class) -> String {
    format!("{} {:.2}", name(c.class_id, &c.class_name), c.confidence)
}

fn top_classes(classes: &[Class]) -> String {
    if classes.is_empty() {
        return "no class".into();
    }
    classes
        .iter()
        .take(3)
        .map(class_text)
        .collect::<Vec<_>>()
        .join(", ")
}

fn map_text(map: &Map) -> String {
    match &map.summary {
        MapSummary::Classes(classes) => {
            if classes.is_empty() {
                return "empty map".into();
            }
            classes
                .iter()
                .take(3)
                .map(|(c, share)| {
                    format!("{} {:.0}%", name(c.class_id, &c.class_name), share * 100.0)
                })
                .collect::<Vec<_>>()
                .join(", ")
        }
        MapSummary::Values {
            min,
            max,
            mean,
            above_threshold,
        } => {
            let above = above_threshold
                .map(|a| format!(", {:.0}% above threshold", a * 100.0))
                .unwrap_or_default();
            format!("min {min:.2}, mean {mean:.2}, max {max:.2}{above}")
        }
    }
}

/// A one-line description of a result that has no box of its own.
fn result_text(r: &Results) -> String {
    match r {
        Results::Objects(o) => format!("{} objects", o.len()),
        Results::Classes(c) => top_classes(c),
        Results::Map(m) => map_text(m),
        Results::Text(Some(t)) => format!("{:?} {:.2}", t.text, t.confidence),
        Results::Text(None) => "no text".into(),
        Results::Vector(v) if v.is_empty() => "no embedding".into(),
        Results::Vector(v) => format!("re-id ok ({}d)", v.len()),
        Results::Tensor(t) => {
            let shapes: Vec<String> = t.iter().map(|(n, s, _)| format!("{n} {s:?}")).collect();
            format!("tensors: {}", shapes.join(", "))
        }
    }
}

fn object_facts(node: &str, index: usize, o: &Object, layouts: &Layouts) -> ObjectFacts {
    let (mut lines, mut children) = (vec![], vec![]);
    for (child, result) in &o.children {
        match result {
            Results::Objects(list) => {
                if list.is_empty() {
                    lines.push(format!("{child}: nothing"));
                }
                children.extend(
                    list.iter()
                        .enumerate()
                        .map(|(i, c)| object_facts(child, i, c, layouts)),
                );
            }
            other => lines.push(format!("{child}: {}", result_text(other))),
        }
    }
    let class = name(o.class_id, &o.class_name);
    ObjectFacts {
        id: match o.tracker_id {
            Some(id) => format!("track {id}"),
            None => format!("{node}#{index}"),
        },
        label: format!("{class} {:.2}", o.confidence),
        colour_key: o
            .tracker_id
            .map_or_else(|| class.clone(), |id| format!("track {id}")),
        class,
        bbox: o.bbox,
        lines,
        keypoints: o.keypoints.clone(),
        skeleton: layouts
            .get(node)
            .filter(|l| l.points.len() == o.keypoints.len())
            .map(|l| l.skeleton.clone())
            .unwrap_or_default(),
        children,
    }
}

/// The facts of a tick.
pub fn facts(tick: &Tick, layouts: &Layouts) -> FrameFacts {
    let mut out = FrameFacts {
        errors: tick.errors.clone(),
        events: tick.events.clone(),
        ..FrameFacts::default()
    };
    for (node, result) in &tick.results {
        match result {
            Results::Objects(list) => out.objects.extend(
                list.iter()
                    .enumerate()
                    .map(|(i, o)| object_facts(node, i, o, layouts)),
            ),
            other => {
                out.notes.push(format!("{node}: {}", result_text(other)));
                if let (Results::Map(map), Some(affine)) = (other, tick.affines.get(node))
                    && map.width > 0
                    && map.height > 0
                {
                    out.maps.push(MapLayer {
                        node: node.clone(),
                        map: map.clone(),
                        affine: *affine,
                    });
                }
            }
        }
    }
    out
}

fn object_lines(o: &ObjectFacts, depth: usize, out: &mut Vec<String>) {
    let pad = "  ".repeat(depth + 1);
    let [x1, y1, x2, y2] = o.bbox;
    let points = if o.keypoints.is_empty() {
        String::new()
    } else {
        format!(" {} keypoints", o.keypoints.len())
    };
    out.push(format!(
        "{pad}{} {} [{x1:.0}, {y1:.0}, {x2:.0}, {y2:.0}]{points}",
        o.id, o.label
    ));
    for line in &o.lines {
        out.push(format!("{pad}  {line}"));
    }
    for child in &o.children {
        object_lines(child, depth + 1, out);
    }
}

/// The facts as console lines, without the frame's header.
pub fn text_lines(f: &FrameFacts) -> Vec<String> {
    let mut out = vec![];
    for o in &f.objects {
        object_lines(o, 0, &mut out);
    }
    out.extend(f.notes.iter().map(|n| format!("  {n}")));
    out.extend(f.errors.iter().map(|e| format!("  error: {e}")));
    out.extend(
        f.events
            .iter()
            .map(|e| format!("  event: {}", event_line(e))),
    );
    out
}

/// What counts as a change for `--on-change`: which classes are seen and
/// what they say, not where exactly.
pub fn signature(f: &FrameFacts) -> String {
    fn walk(o: &ObjectFacts, out: &mut Vec<String>) {
        out.push(o.class.clone());
        // Text and classes without their scores, which move every frame.
        out.extend(o.lines.iter().map(|l| strip_numbers(l)));
        o.children.iter().for_each(|c| walk(c, out));
    }
    let mut parts = vec![];
    f.objects.iter().for_each(|o| walk(o, &mut parts));
    parts.sort();
    parts.extend(f.notes.iter().map(|n| strip_numbers(n)));
    parts.extend(f.errors.iter().cloned());
    // An event is always news.
    parts.extend(f.events.iter().map(event_line));
    parts.join("|")
}

fn strip_numbers(text: &str) -> String {
    text.split_whitespace()
        .filter(|w| !w.trim_end_matches([',', '%']).parse::<f32>().is_ok())
        .collect::<Vec<_>>()
        .join(" ")
}

fn object_json(o: &ObjectFacts) -> Value {
    json!({
        "id": o.id,
        "label": o.label,
        "box": o.bbox,
        "facts": o.lines,
        "keypoints": o.keypoints.iter().map(|k| [k.x, k.y, k.confidence]).collect::<Vec<_>>(),
        "children": o.children.iter().map(object_json).collect::<Vec<_>>(),
    })
}

/// The facts as a JSON value, for `--json`.
pub fn to_json(f: &FrameFacts) -> Value {
    json!({
        "objects": f.objects.iter().map(object_json).collect::<Vec<_>>(),
        "notes": f.notes,
        "errors": f.errors,
        "events": f.events,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::results::Text;

    pub(crate) fn person() -> Object {
        Object {
            tracker_id: None,
            class_id: 0,
            class_name: Some("person".into()),
            confidence: 0.93,
            bbox: [10.0, 20.0, 50.0, 60.0],
            keypoints: vec![
                Keypoint {
                    x: 15.0,
                    y: 25.0,
                    confidence: 0.9,
                },
                Keypoint {
                    x: 45.0,
                    y: 55.0,
                    confidence: 0.9,
                },
            ],
            children: vec![
                (
                    "gesture".into(),
                    Results::Classes(vec![Class {
                        class_id: 3,
                        class_name: Some("wave".into()),
                        confidence: 0.81,
                    }]),
                ),
                ("reid".into(), Results::Vector(vec![0.5; 128])),
                (
                    "plate".into(),
                    Results::Text(Some(Text {
                        text: "AB 123".into(),
                        confidence: 0.7,
                    })),
                ),
            ],
        }
    }

    fn tick() -> Tick {
        Tick {
            results: vec![
                ("det".into(), Results::Objects(vec![person()])),
                (
                    "scene".into(),
                    Results::Classes(vec![Class {
                        class_id: 7,
                        class_name: None,
                        confidence: 0.5,
                    }]),
                ),
            ],
            errors: vec!["pose: failed".into()],
            ..Tick::default()
        }
    }

    #[test]
    fn the_console_lines_say_everything_known() {
        let f = facts(&tick(), &Layouts::new());
        assert_eq!(
            text_lines(&f),
            [
                "  det#0 person 0.93 [10, 20, 50, 60] 2 keypoints",
                "    gesture: wave 0.81",
                "    reid: re-id ok (128d)",
                "    plate: \"AB 123\" 0.70",
                "  scene: class 7 0.50",
                "  error: pose: failed",
            ]
        );
    }

    #[test]
    fn nested_objects_become_children() {
        let mut parent = person();
        parent.children = vec![("hand".into(), Results::Objects(vec![person()]))];
        let t = Tick {
            results: vec![("det".into(), Results::Objects(vec![parent]))],
            ..Tick::default()
        };
        let f = facts(&t, &Layouts::new());
        assert_eq!(f.objects[0].children[0].id, "hand#0");
    }

    #[test]
    fn a_known_layout_gives_a_skeleton_only_for_its_point_count() {
        let layout = keypoints::layout("coco17").unwrap();
        let mut o = person();
        o.keypoints = vec![
            Keypoint {
                x: 0.0,
                y: 0.0,
                confidence: 1.0
            };
            layout.points.len()
        ];
        let layouts = Layouts::from([("det".to_string(), layout)]);
        assert!(!object_facts("det", 0, &o, &layouts).skeleton.is_empty());
        assert!(
            object_facts("det", 0, &person(), &layouts)
                .skeleton
                .is_empty()
        );
    }

    #[test]
    fn the_signature_ignores_scores_and_positions() {
        let a = facts(&tick(), &Layouts::new());
        let mut moved = tick();
        if let Results::Objects(o) = &mut moved.results[0].1 {
            o[0].confidence = 0.5;
            o[0].bbox = [0.0, 0.0, 9.0, 9.0];
        }
        assert_eq!(signature(&a), signature(&facts(&moved, &Layouts::new())));
        let mut other = tick();
        if let Results::Objects(o) = &mut other.results[0].1 {
            o[0].class_name = Some("dog".into());
        }
        assert_ne!(signature(&a), signature(&facts(&other, &Layouts::new())));
    }

    #[test]
    fn json_carries_the_same_facts() {
        let j = to_json(&facts(&tick(), &Layouts::new()));
        assert_eq!(j["objects"][0]["id"], "det#0");
        assert_eq!(j["objects"][0]["facts"][1], "reid: re-id ok (128d)");
        assert_eq!(j["errors"][0], "pose: failed");
    }
}
