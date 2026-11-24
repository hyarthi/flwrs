//! `flwrs-cv-source rule-test`: runs one Lua rule node of a graph on frames
//! written as JSON lines, with no camera and no models, and writes the
//! events it returns as JSON lines. For testing a rule on made-up or
//! recorded inputs: the rule runs exactly as in a node (same sandbox,
//! helpers, `held` / `once` on the frames' own clock, `state`).
//!
//! A frame (one line; blank lines and lines starting with `#` are skipped):
//!
//! ```text
//! {"time": 1.5, "width": 1280, "height": 720,
//!  "objects": [{"tracker_id": 1, "class_name": "person", "box": [x1, y1, x2, y2],
//!               "kp": {"left_wrist": [x, y, confidence], …},
//!               "children": {"hand": [{…an object…}], "gesture": [{"class_name": "thumb_up", "confidence": 0.9}]}}],
//!  "tracks": [{"id": 1, "box": [...], "vx": 0, "vy": 0, "age": 1.0, "lost": false}],
//!  "ended": [3]}
//! ```
//!
//! Only `time` is required. `width` / `height` default to 1280 x 720. An
//! object's keypoints are `keypoints` (a list) or `kp` (by name, with the
//! names of the model feeding it in the graph); a point is `[x, y]`,
//! `[x, y, confidence]` or `{x, y, confidence}` (confidence 1 when left
//! out). A child is a list of objects (items with a `box`), a list of
//! classes (items without one), or a string (text). Without `tracks`, every
//! object with a `tracker_id` is a track (its box, no speed, its age since
//! it was first seen); without `ended`, a track missing from a frame ends
//! on it.

use crate::analytics::rules::RuleEvent;
use crate::analytics::script::{LuaRule, ScriptInput};
use crate::analytics::track::{TrackEvent, TrackInfo};
use crate::graph::config::GraphConfig;
use crate::results::{Class, Keypoint, Object, Results, Text};
use clap::Parser;
use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::process::ExitCode;

/// Runs a graph's Lua rule on frames written as JSON lines.
#[derive(Parser, Debug)]
#[command(name = "flwrs-cv-source rule-test")]
pub struct RuleTestArgs {
    /// The graph: a JSON file with the graph itself, a node's launch args
    /// (`{"graph": …}`), or an example (`{"description", "graph"}`).
    #[arg(long)]
    pub graph: PathBuf,
    /// The Lua rule node to run (default: the graph's only one).
    #[arg(long)]
    pub node: Option<String>,
    /// The frames, JSON lines (default: standard input).
    #[arg(long)]
    pub frames: Option<PathBuf>,
}

pub fn main(argv: impl IntoIterator<Item = String>) -> ExitCode {
    let args = RuleTestArgs::parse_from(argv);
    let run = || -> Result<Vec<String>, String> {
        let text = std::fs::read_to_string(&args.graph)
            .map_err(|e| format!("{}: {e}", args.graph.display()))?;
        let json: Value =
            serde_json::from_str(&text).map_err(|e| format!("{}: {e}", args.graph.display()))?;
        let graph = graph_of(json)?;
        let frames: Box<dyn BufRead> = match &args.frames {
            Some(path) => Box::new(std::io::BufReader::new(
                std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?,
            )),
            None => Box::new(std::io::stdin().lock()),
        };
        let mut out = std::io::stdout().lock();
        run_rule(&graph, args.node.as_deref(), frames, &mut |e| {
            let line = serde_json::to_string(e).expect("events serialise");
            let _ = writeln!(out, "{line}");
        })
    };
    match run() {
        Ok(errors) if errors.is_empty() => ExitCode::SUCCESS,
        Ok(errors) => {
            errors.iter().for_each(|e| eprintln!("{e}"));
            ExitCode::FAILURE
        }
        Err(e) => {
            eprintln!("{e}");
            ExitCode::from(2)
        }
    }
}

/// The graph out of any of the accepted shapes, checked.
fn graph_of(mut json: Value) -> Result<GraphConfig, String> {
    if json.get("nodes").is_none()
        && let Some(inner) = json.get_mut("graph")
    {
        json = inner.take();
    }
    let (graph, _) = crate::graph::parse(&json).map_err(|p| p.join("\n"))?;
    Ok(graph)
}

/// Runs the rule on every frame; `emit` gets each event. Returns the
/// rule's errors (by frame), or the first problem with the input.
pub fn run_rule(
    graph: &GraphConfig,
    node: Option<&str>,
    frames: impl BufRead,
    emit: &mut dyn FnMut(&RuleEvent),
) -> Result<Vec<String>, String> {
    let node = match node {
        Some(n) => n.to_string(),
        None => {
            let lua: Vec<&String> = graph
                .nodes
                .iter()
                .filter(|(_, n)| matches!(n, crate::graph::config::NodeConfig::Lua(_)))
                .map(|(id, _)| id)
                .collect();
            match lua.as_slice() {
                [one] => (*one).clone(),
                [] => return Err("the graph has no lua node".into()),
                _ => return Err("the graph has several lua nodes: pick one with --node".into()),
            }
        }
    };
    let (mut rule, _) = crate::graph::exec::lua_rule(graph, &node)?;
    let mut tracker = Tracks::default();
    let mut errors = vec![];
    for (n, line) in frames.lines().enumerate() {
        let line = line.map_err(|e| format!("reading the frames: {e}"))?;
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let at = |e: String| format!("frames line {}: {e}", n + 1);
        let frame: FrameIn = serde_json::from_str(trimmed).map_err(|e| at(e.to_string()))?;
        let objects = frame
            .objects
            .iter()
            .map(|o| object(o, &rule, None))
            .collect::<Result<Vec<_>, _>>()
            .map_err(at)?;
        let (tracks, ended) = tracker.update(&frame, &objects);
        let input = ScriptInput {
            objects: &objects,
            tracks: &tracks,
            track_events: &ended,
            seq: n as u64,
            width: frame.width,
            height: frame.height,
            time: frame.time,
        };
        match rule.update(&input) {
            Ok(events) => events.iter().for_each(&mut *emit),
            Err(e) => errors.push(at(e)),
        }
    }
    Ok(errors)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FrameIn {
    time: f64,
    #[serde(default = "default_width")]
    width: u32,
    #[serde(default = "default_height")]
    height: u32,
    #[serde(default)]
    objects: Vec<ObjectIn>,
    #[serde(default)]
    tracks: Option<Vec<TrackIn>>,
    #[serde(default)]
    ended: Option<Vec<u64>>,
}

fn default_width() -> u32 {
    1280
}

fn default_height() -> u32 {
    720
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ObjectIn {
    #[serde(default)]
    tracker_id: Option<u64>,
    #[serde(default)]
    class_id: u32,
    #[serde(default)]
    class_name: Option<String>,
    #[serde(default = "one")]
    confidence: f32,
    #[serde(default, rename = "box")]
    bbox: [f32; 4],
    #[serde(default)]
    keypoints: Vec<Value>,
    #[serde(default)]
    kp: BTreeMap<String, Value>,
    #[serde(default)]
    children: BTreeMap<String, Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TrackIn {
    id: u64,
    #[serde(default)]
    class_id: u32,
    #[serde(default)]
    class_name: Option<String>,
    #[serde(default, rename = "box")]
    bbox: [f32; 4],
    #[serde(default)]
    vx: f32,
    #[serde(default)]
    vy: f32,
    #[serde(default)]
    age: f32,
    #[serde(default)]
    lost: bool,
}

fn one() -> f32 {
    1.0
}

fn point(v: &Value) -> Result<Keypoint, String> {
    let num = |v: &Value| v.as_f64().map(|n| n as f32);
    let p = match v {
        Value::Array(a) if (2..=3).contains(&a.len()) => {
            (num(&a[0]), num(&a[1]), a.get(2).map_or(Some(1.0), num))
        }
        Value::Object(o) => (
            o.get("x").and_then(num),
            o.get("y").and_then(num),
            o.get("confidence").map_or(Some(1.0), num),
        ),
        _ => (None, None, None),
    };
    match p {
        (Some(x), Some(y), Some(confidence)) => Ok(Keypoint { x, y, confidence }),
        _ => Err(format!(
            "a point is [x, y], [x, y, confidence] or {{x, y, confidence}}, not {v}"
        )),
    }
}

/// An input object as the engine's, its named keypoints placed by the
/// names of the model it comes from (`node`, or the rule's own objects).
fn object(o: &ObjectIn, rule: &LuaRule, node: Option<&str>) -> Result<Object, String> {
    let mut keypoints = o
        .keypoints
        .iter()
        .map(point)
        .collect::<Result<Vec<_>, _>>()?;
    if !o.kp.is_empty() {
        let names = rule.keypoint_names(node).ok_or_else(|| {
            format!(
                "`kp` needs keypoint names, but {} has no keypoint_layout in the graph: use `keypoints`",
                node.map_or("the model feeding the rule".to_string(), |n| format!("node {n}"))
            )
        })?;
        if keypoints.is_empty() {
            keypoints = vec![
                Keypoint {
                    x: 0.0,
                    y: 0.0,
                    confidence: 0.0
                };
                names.len()
            ];
        }
        for (name, v) in &o.kp {
            let i = names
                .iter()
                .position(|n| n == name)
                .ok_or_else(|| format!("no keypoint named {name:?}"))?;
            *keypoints
                .get_mut(i)
                .ok_or_else(|| format!("keypoint {name:?} is beyond `keypoints`"))? = point(v)?;
        }
    }
    let mut children = vec![];
    for (child, v) in &o.children {
        let results = match v {
            Value::String(text) => Results::Text(Some(Text {
                text: text.clone(),
                confidence: 1.0,
            })),
            Value::Array(items)
                if items.iter().all(|i| i.get("box").is_some()) && !items.is_empty() =>
            {
                let mut list = vec![];
                for item in items {
                    let parsed: ObjectIn = serde_json::from_value(item.clone())
                        .map_err(|e| format!("children.{child}: {e}"))?;
                    list.push(object(&parsed, rule, Some(child))?);
                }
                Results::Objects(list)
            }
            Value::Array(items) => {
                let mut list = vec![];
                for item in items {
                    let c =
                        ClassIn::deserialize(item).map_err(|e| format!("children.{child}: {e}"))?;
                    list.push(Class {
                        class_id: c.class_id,
                        class_name: c.class_name,
                        confidence: c.confidence,
                    });
                }
                Results::Classes(list)
            }
            other => return Err(format!("children.{child}: a list or a string, not {other}")),
        };
        children.push((child.clone(), results));
    }
    Ok(Object {
        tracker_id: o.tracker_id,
        class_id: o.class_id,
        class_name: o.class_name.clone(),
        confidence: o.confidence,
        bbox: o.bbox,
        keypoints,
        children,
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ClassIn {
    #[serde(default)]
    class_id: u32,
    #[serde(default)]
    class_name: Option<String>,
    #[serde(default = "one")]
    confidence: f32,
}

/// Tracks a frame doesn't list: its tracked objects, each with its first
/// time, and the ones gone since the frame before.
#[derive(Default)]
struct Tracks {
    first_seen: BTreeMap<u64, f64>,
    last: BTreeMap<u64, TrackInfo>,
}

impl Tracks {
    fn update(&mut self, frame: &FrameIn, objects: &[Object]) -> (Vec<TrackInfo>, Vec<TrackEvent>) {
        let tracks: Vec<TrackInfo> = match &frame.tracks {
            Some(list) => list
                .iter()
                .map(|t| TrackInfo {
                    id: t.id,
                    class_id: t.class_id,
                    class_name: t.class_name.clone(),
                    bbox: t.bbox,
                    velocity: [t.vx, t.vy],
                    age_s: t.age,
                    lost: t.lost,
                })
                .collect(),
            None => objects
                .iter()
                .filter_map(|o| {
                    let id = o.tracker_id?;
                    let first = *self.first_seen.entry(id).or_insert(frame.time);
                    Some(TrackInfo {
                        id,
                        class_id: o.class_id,
                        class_name: o.class_name.clone(),
                        bbox: o.bbox,
                        velocity: [0.0, 0.0],
                        age_s: (frame.time - first) as f32,
                        lost: false,
                    })
                })
                .collect(),
        };
        let now: BTreeMap<u64, TrackInfo> = tracks.iter().map(|t| (t.id, t.clone())).collect();
        let ended: Vec<TrackEvent> = match &frame.ended {
            Some(ids) => ids
                .iter()
                .map(|id| {
                    TrackEvent::Ended(self.last.get(id).cloned().unwrap_or(TrackInfo {
                        id: *id,
                        class_id: 0,
                        class_name: None,
                        bbox: [0.0; 4],
                        velocity: [0.0; 2],
                        age_s: 0.0,
                        lost: true,
                    }))
                })
                .collect(),
            None => self
                .last
                .iter()
                .filter(|(id, _)| !now.contains_key(id))
                .map(|(_, t)| TrackEvent::Ended(t.clone()))
                .collect(),
        };
        for e in &ended {
            if let TrackEvent::Ended(t) = e {
                self.first_seen.remove(&t.id);
            }
        }
        self.last = now;
        (tracks, ended)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn graph(script: &str) -> GraphConfig {
        graph_of(serde_json::json!({"description": "x", "graph": {
            "nodes": {
                "camera": {"type": "source"},
                "pose": {"type": "model", "model": "p.onnx",
                    "pre": {"width": 64, "height": 64, "resize": "stretch", "color": "rgb", "dtype": "f32", "layout": "nchw"},
                    "decode": {"type": "yolo", "output": "o", "channels_first": true, "classes": 1, "keypoints": 17},
                    "keypoint_layout": "coco17"},
                "people": {"type": "tracker"},
                "hand": {"type": "model", "model": "h.onnx",
                    "pre": {"width": 64, "height": 64, "resize": "stretch", "color": "rgb", "dtype": "f32", "layout": "nchw"},
                    "decode": {"type": "landmarks", "output": "o", "points": 21, "dims": 3},
                    "keypoint_layout": "hand21"},
                "roi": {"type": "crop"},
                "rule": {"type": "lua", "script": script}
            },
            "links": [
                {"from": "camera.frame", "to": "pose.frame"},
                {"from": "pose.objects", "to": "people.objects"},
                {"from": "camera.frame", "to": "roi.frame"},
                {"from": "people.objects", "to": "roi.objects"},
                {"from": "roi.regions", "to": "hand.frame"},
                {"from": "people.objects", "to": "rule.objects"}
            ]
        }}))
        .unwrap()
    }

    fn run(script: &str, frames: &str) -> (Vec<RuleEvent>, Vec<String>) {
        let mut events = vec![];
        let errors = run_rule(&graph(script), None, frames.as_bytes(), &mut |e| {
            events.push(e.clone())
        })
        .unwrap();
        (events, errors)
    }

    #[test]
    fn named_keypoints_children_and_held_on_the_frames_clock() {
        let script = r#"
            local out = {}
            for _, o in ipairs(objects) do
              local w = o.kp.left_wrist
              local h = o.children.hand and o.children.hand[1]
              local tip = h and h.kp.index_finger_tip
              local g = h and h.children.gesture and h.children.gesture[1]
              if once(o.tracker_id, held(o.tracker_id, w.y < 100, 0.5)) then
                out[#out + 1] = {event = "up", action = g and g.class_name or "?",
                  tracker_id = o.tracker_id, data = {tip = tip and tip.x}}
              end
            end
            return out
        "#;
        let frame = |t: f64, y: f64| {
            format!(
                r#"{{"time": {t}, "objects": [{{"tracker_id": 4, "box": [0, 0, 10, 10],
                  "kp": {{"left_wrist": [5, {y}, 0.9]}},
                  "children": {{"hand": [{{"box": [0, 0, 5, 5], "kp": {{"index_finger_tip": [7, 8]}},
                    "children": {{"gesture": [{{"class_name": "thumb_up", "confidence": 0.8}}]}}}}]}}}}]}}"#
            )
            .replace('\n', " ")
        };
        let frames = [
            frame(0.0, 50.0),
            "# a comment".into(),
            frame(0.4, 50.0),
            frame(0.6, 50.0),
            frame(0.8, 50.0),
        ]
        .join("\n");
        let (events, errors) = run(script, &frames);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!(events[0].time, 0.6);
        assert_eq!(events[0].action, "thumb_up");
        assert_eq!(events[0].tracker_id, Some(4));
        assert_eq!(events[0].data.as_ref().unwrap()["tip"], 7.0);
    }

    #[test]
    fn tracks_are_derived_and_end_when_missing() {
        let script = r#"
            state.tracks = state.tracks or {}
            local out = {}
            for _, t in ipairs(tracks) do
              if not state.tracks[t.id] then
                state.tracks[t.id] = true
                out[#out + 1] = {event = "track", action = "new", tracker_id = t.id}
              end
            end
            return out
        "#;
        let frames = r#"{"time": 0, "objects": [{"tracker_id": 1, "box": [0, 0, 1, 1]}]}
{"time": 0.1, "objects": []}
{"time": 0.2, "objects": [{"tracker_id": 1, "box": [0, 0, 1, 1]}]}"#;
        // The track ended on the second frame, so its state went with it and
        // it's new again on the third.
        let (events, _) = run(script, frames);
        assert_eq!(
            events.iter().map(|e| e.time).collect::<Vec<_>>(),
            vec![0.0, 0.2]
        );
    }

    #[test]
    fn problems_are_reported() {
        let (_, errors) = run("error('boom')", r#"{"time": 0}"#);
        assert!(
            errors[0].contains("line 1") && errors[0].contains("boom"),
            "{errors:?}"
        );
        let bad = |frames: &str| {
            run_rule(&graph("return nil"), None, frames.as_bytes(), &mut |_| {}).unwrap_err()
        };
        assert!(bad(r#"{"objects": []}"#).contains("time"));
        assert!(
            bad(r#"{"time": 0, "objects": [{"kp": {"nose_tip": [1, 2]}}]}"#).contains("nose_tip")
        );
        assert!(bad(r#"{"time": 0, "objects": [{"keypoints": [[1]]}]}"#).contains("a point"));
        assert!(
            run_rule(
                &graph("return nil"),
                Some("nope"),
                "".as_bytes(),
                &mut |_| {}
            )
            .unwrap_err()
            .contains("nope")
        );
    }
}
