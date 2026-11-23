//! A graph's run on generated models, on each runtime this build has.

use super::compile;
use crate::backend::Runtime;
use crate::frame::Frame;
use crate::graph::config::Execution;
use crate::graph::parse;
use crate::results::Results;
use crate::tensor::Output;
use crate::testing;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// A model file: its name, input shape, and fixed outputs (name, shape,
/// values).
type TestModel = (
    &'static str,
    &'static [usize],
    Vec<(&'static str, Vec<usize>, Vec<f32>)>,
);

struct Files {
    _dir: tempfile::TempDir,
    files: BTreeMap<String, PathBuf>,
}

/// Models whose outputs are fixed tensors, by file name: (input shape,
/// outputs).
fn files(models: &[TestModel]) -> Files {
    let dir = tempfile::tempdir().unwrap();
    let mut files = BTreeMap::new();
    for (name, input, outputs) in models {
        let outputs: Vec<(&str, Output)> = outputs
            .iter()
            .map(|(n, s, d)| (*n, Output::new(s.clone(), d.clone()).unwrap()))
            .collect();
        let named: Vec<(&str, &Output)> = outputs.iter().map(|(n, o)| (*n, o)).collect();
        let path = dir.path().join(name);
        std::fs::write(&path, testing::constant_outputs(input, &named, &[], &[])).unwrap();
        files.insert(name.to_string(), path);
    }
    Files { _dir: dir, files }
}

fn pre(w: u32, h: u32) -> Value {
    json!({"width": w, "height": h, "resize": "stretch", "color": "rgb", "dtype": "f32", "layout": "nchw"})
}

fn execution(runtime: Runtime) -> Execution {
    Execution {
        runtime: Some(runtime),
        ..Execution::default()
    }
}

/// Two detections (a person at 0.9, a dog at 0.8) on a 64x32 frame shown
/// as is.
fn detector() -> TestModel {
    (
        "det.onnx",
        &[1, 3, 32, 64],
        vec![(
            "rows",
            vec![1, 2, 6],
            vec![
                0.0, 0.0, 32.0, 32.0, 0.9, 0.0, 32.0, 0.0, 64.0, 32.0, 0.8, 1.0,
            ],
        )],
    )
}

fn objects(r: &Results) -> &[crate::results::Object] {
    match r {
        Results::Objects(o) => o,
        other => panic!("not objects: {other:?}"),
    }
}

#[test]
fn models_on_regions_attach_to_their_objects() {
    let f = files(&[
        detector(),
        (
            "cls.onnx",
            &[1, 3, 8, 8],
            vec![("probs", vec![1, 2], vec![0.3, 0.7])],
        ),
    ]);
    let graph = json!({
        "nodes": {
            "camera": {"type": "source"},
            "det": {"type": "model", "model": "det.onnx", "pre": pre(64, 32),
                    "decode": {"type": "detections_e2e", "output": "rows"}, "labels": ["person", "dog"]},
            "people": {"type": "crop", "classes": ["person"]},
            "cls": {"type": "model", "model": "cls.onnx", "pre": pre(8, 8),
                    "decode": {"type": "classify", "output": "probs", "softmax": false, "topk": 1}},
            "sure": {"type": "filter", "min_confidence": 0.85}
        },
        "links": [
            {"from": "camera.frame", "to": "det.frame"},
            {"from": "camera.frame", "to": "people.frame"},
            {"from": "det.objects", "to": "people.objects"},
            {"from": "people.regions", "to": "cls.frame"},
            {"from": "det.objects", "to": "sure.objects"}
        ]
    });
    let (graph, checked) = parse(&graph).unwrap();
    let frame = Frame::filled(64, 32, [1, 2, 3]).unwrap();
    for runtime in Runtime::available() {
        let (mut plan, warnings) =
            compile(&graph, &checked, &f.files, &execution(*runtime), 1).unwrap();
        assert!(warnings.is_empty());
        assert_eq!(plan.devices().len(), 2);
        let tick = plan.tick(&frame);
        assert!(tick.errors.is_empty(), "{:?}", tick.errors);
        let ids: Vec<&str> = tick.results.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(ids.len(), 2, "{ids:?}");
        let det = &tick.results.iter().find(|(id, _)| id == "det").unwrap().1;
        let det = objects(det);
        assert_eq!(det[0].bbox, [0.0, 0.0, 32.0, 32.0]);
        // The person got the classifier's result; the dog wasn't cropped.
        assert_eq!(det[0].children.len(), 1, "{runtime:?}");
        let (child, Results::Classes(c)) = &det[0].children[0] else {
            panic!()
        };
        assert_eq!((child.as_str(), c[0].class_id), ("cls", 1));
        assert!(det[1].children.is_empty());
        let sure = objects(&tick.results.iter().find(|(id, _)| id == "sure").unwrap().1);
        assert_eq!(sure.len(), 1);
        assert_eq!(tick.timing["cls"].runs, 1);
    }
}

#[test]
fn models_on_keypoints_run_per_object() {
    let f = files(&[
        // One "hand" with two landmarks over the whole 16x16 input.
        (
            "hand.onnx",
            &[1, 3, 16, 16],
            vec![("lm", vec![1, 6], vec![2.0, 4.0, 0.0, 6.0, 8.0, 0.0])],
        ),
        // A classifier on the 2 x 2 keypoint values.
        (
            "gesture.onnx",
            &[1, 4],
            vec![("probs", vec![1, 3], vec![0.1, 0.2, 0.7])],
        ),
    ]);
    let graph = json!({
        "nodes": {
            "camera": {"type": "source"},
            "hand": {"type": "model", "model": "hand.onnx", "pre": pre(16, 16),
                     "decode": {"type": "landmarks", "output": "lm", "points": 2, "dims": 3}},
            "gesture": {"type": "model", "model": "gesture.onnx", "tensor_input": {"dims": 2, "normalize": "wrist"},
                        "decode": {"type": "classify", "output": "probs", "softmax": false, "topk": 1}}
        },
        "links": [
            {"from": "camera.frame", "to": "hand.frame"},
            {"from": "hand.objects", "to": "gesture.objects"}
        ]
    });
    let (graph, checked) = parse(&graph).unwrap();
    let frame = Frame::filled(16, 16, [0, 0, 0]).unwrap();
    for runtime in Runtime::available() {
        let (mut plan, _) = compile(&graph, &checked, &f.files, &execution(*runtime), 1).unwrap();
        let tick = plan.tick(&frame);
        assert!(tick.errors.is_empty(), "{:?}", tick.errors);
        let hand = objects(&tick.results.iter().find(|(id, _)| id == "hand").unwrap().1);
        let (id, Results::Classes(c)) = &hand[0].children[0] else {
            panic!()
        };
        assert_eq!((id.as_str(), c[0].class_id), ("gesture", 2), "{runtime:?}");
        // The gesture node's own output: its first object's result.
        let Results::Classes(own) = &tick
            .results
            .iter()
            .find(|(id, _)| id == "gesture")
            .unwrap()
            .1
        else {
            panic!()
        };
        assert_eq!(own[0].class_id, 2);
    }
}

#[test]
fn sampling_reuses_the_last_results() {
    let f = files(&[detector()]);
    let graph = json!({
        "nodes": {
            "camera": {"type": "source"},
            "det": {"type": "model", "model": "det.onnx", "pre": pre(64, 32),
                    "decode": {"type": "detections_e2e", "output": "rows"}, "sampling": {"every_n_frames": 2}}
        },
        "links": [{"from": "camera.frame", "to": "det.frame"}]
    });
    let (graph, checked) = parse(&graph).unwrap();
    let frame = Frame::filled(64, 32, [0, 0, 0]).unwrap();
    let (mut plan, _) = compile(&graph, &checked, &f.files, &Execution::default(), 1).unwrap();
    let runs: Vec<u32> = (0..3)
        .map(|_| plan.tick(&frame).timing["det"].runs)
        .collect();
    assert_eq!(runs, [1, 0, 1]);
    assert_eq!(objects(&plan.tick(&frame).results[0].1).len(), 2);
}

#[test]
fn a_device_that_isnt_there_fails_or_falls_back() {
    let f = files(&[detector()]);
    let graph = |execution: Value| {
        json!({
            "nodes": {
                "camera": {"type": "source"},
                "det": {"type": "model", "model": "det.onnx", "pre": pre(64, 32),
                        "decode": {"type": "detections_e2e", "output": "rows"}, "execution": execution}
            },
            "links": [{"from": "camera.frame", "to": "det.frame"}]
        })
    };
    let missing = json!({"device": "gpu", "provider": "nonexistent"});
    for runtime in Runtime::available() {
        let (g, checked) = parse(&graph(missing.clone())).unwrap();
        let problems = compile(&g, &checked, &f.files, &execution(*runtime), 1)
            .err()
            .unwrap();
        assert!(
            problems[0].starts_with("nodes.det: det.onnx"),
            "{problems:?}"
        );
        let mut fallback = missing.clone();
        fallback["allow_fallback"] = json!(true);
        let (g, checked) = parse(&graph(fallback)).unwrap();
        let (mut plan, warnings) =
            compile(&g, &checked, &f.files, &execution(*runtime), 1).unwrap();
        assert!(
            warnings[0].contains("it runs on the CPU instead"),
            "{warnings:?}"
        );
        assert!(
            plan.devices()[0].1.starts_with("cpu"),
            "{:?}",
            plan.devices()
        );
        assert!(
            plan.tick(&Frame::filled(64, 32, [0, 0, 0]).unwrap())
                .errors
                .is_empty()
        );
    }
    // The plugin-level setting applies where the node sets nothing.
    let (g, checked) = parse(&graph(json!({}))).unwrap();
    let defaults = Execution {
        device: Some(crate::backend::Device::Gpu),
        provider: Some("nonexistent".into()),
        ..Execution::default()
    };
    assert!(compile(&g, &checked, &f.files, &defaults, 1).is_err());
}

/// A map a quarter of the input's size still covers the whole frame.
#[test]
fn a_small_map_is_placed_over_the_whole_input() {
    use crate::frame::Affine;
    use crate::results::{Map, MapSummary};
    let map = Results::Map(Map {
        width: 16,
        height: 8,
        values: vec![0.0; 128],
        summary: MapSummary::Values {
            min: 0.0,
            max: 0.0,
            mean: 0.0,
            above_threshold: None,
        },
    });
    // Input 64x32 on a 640x320 frame, shifted by (5, 7).
    let affine = Affine {
        a: 10.0,
        b: 0.0,
        c: 5.0,
        d: 0.0,
        e: 10.0,
        f: 7.0,
    };
    let placed = super::result_affine(affine, Some((64, 32)), &map);
    assert_eq!(placed.apply(0.0, 0.0), (5.0, 7.0));
    assert_eq!(placed.apply(16.0, 8.0), (645.0, 327.0));
    let objects = Results::Objects(vec![]);
    assert_eq!(
        super::result_affine(affine, Some((64, 32)), &objects),
        affine
    );
}

/// A tracker, a model per track, a built-in rule and a Lua rule together.
#[test]
fn tracks_rules_and_per_track_models_run_together() {
    let f = files(&[
        detector(),
        (
            "cls.onnx",
            &[1, 3, 8, 8],
            vec![("probs", vec![1, 2], vec![0.3, 0.7])],
        ),
    ]);
    let mut nodes = json!({
        "camera": {"type": "source"},
        "det": {"type": "model", "model": "det.onnx", "pre": pre(64, 32),
                "decode": {"type": "detections_e2e", "output": "rows"}, "labels": ["person", "dog"]},
        "tracks": {"type": "tracker"},
        "people": {"type": "crop", "classes": ["person"]},
        "cls": {"type": "model", "model": "cls.onnx", "pre": pre(8, 8), "sampling": {"per_track": {}},
                "decode": {"type": "classify", "output": "probs", "softmax": false, "topk": 1}},
        "seen": {"type": "rule", "rule": {"type": "track", "on": "started"}, "classes": ["person"]}
    });
    let mut links = json!([
        {"from": "camera.frame", "to": "det.frame"},
        {"from": "det.objects", "to": "tracks.objects"},
        {"from": "camera.frame", "to": "people.frame"},
        {"from": "tracks.objects", "to": "people.objects"},
        {"from": "people.regions", "to": "cls.frame"},
        {"from": "tracks.objects", "to": "seen.objects"}
    ]);
    if cfg!(feature = "lua") {
        nodes["script"] = json!({"type": "lua", "script": r#"
            for _, o in ipairs(objects) do
                if o.tracker_id and o.children.cls and once("a", true) then
                    return {event = "classified", label = tostring(o.children.cls[1].class_id)}
                end
            end
        "#});
        links
            .as_array_mut()
            .unwrap()
            .push(json!({"from": "tracks.objects", "to": "script.objects"}));
    }
    let (graph, checked) = parse(&json!({"nodes": nodes, "links": links})).unwrap();
    let frame = Frame::filled(64, 32, [1, 2, 3]).unwrap();
    for runtime in Runtime::available() {
        let (mut plan, _) = compile(&graph, &checked, &f.files, &execution(*runtime), 1).unwrap();
        let (mut runs, mut events) = (0, vec![]);
        for n in 0..8 {
            let tick = plan.tick_at(&frame, n as f64 / 30.0);
            assert!(tick.errors.is_empty(), "{:?}", tick.errors);
            let ids: Vec<&str> = tick.results.iter().map(|(id, _)| id.as_str()).collect();
            assert_eq!(
                ids,
                ["tracks"],
                "the tracker's output stands for the detector's"
            );
            runs += tick.timing.get("cls").map_or(0, |t| t.runs);
            events.extend(tick.events.iter().map(|e| (n, e.event.clone())));
            let o = objects(&tick.results[0].1);
            assert_eq!(o[0].children.len(), 1, "a reused result still attaches");
            if n >= 2 {
                assert_eq!(o[0].tracker_id, Some(1));
            }
        }
        // Unconfirmed on two frames, then once for the track.
        assert_eq!(runs, 3, "{runtime:?}");
        assert!(events.contains(&(2, "track".to_string())), "{events:?}");
        if cfg!(feature = "lua") {
            assert!(events.iter().any(|(_, e)| e == "classified"), "{events:?}");
        }
    }
}

#[test]
fn rules_and_trackers_are_placed_where_they_can_work() {
    let base = |extra: Value, links: Value| {
        let mut nodes = json!({
            "camera": {"type": "source"},
            "det": {"type": "model", "model": "det.onnx", "pre": pre(64, 32),
                    "decode": {"type": "detections_e2e", "output": "rows"}}
        });
        for (k, v) in extra.as_object().unwrap() {
            nodes[k] = v.clone();
        }
        let mut all = vec![json!({"from": "camera.frame", "to": "det.frame"})];
        all.extend(links.as_array().unwrap().iter().cloned());
        parse(&json!({"nodes": nodes, "links": all}))
            .err()
            .unwrap_or_default()
            .join("\n")
    };
    let line = json!({"type": "rule", "rule": {"type": "line_crossing", "line": [[0.5, 0.0], [0.5, 1.0]]}});
    assert!(
        base(
            json!({"r": line}),
            json!([{"from": "det.objects", "to": "r.objects"}])
        )
        .contains("must come from a tracker"),
    );
    assert_eq!(
        base(
            json!({"t": {"type": "tracker"}, "r": line}),
            json!([{"from": "det.objects", "to": "t.objects"}, {"from": "t.objects", "to": "r.objects"}]),
        ),
        ""
    );
    let count = json!({"type": "rule", "rule": {"type": "counting", "above": 2}});
    assert_eq!(
        base(
            json!({"r": count}),
            json!([{"from": "det.objects", "to": "r.objects"}])
        ),
        ""
    );
    let bad = json!({"type": "rule", "rule": {"type": "zone", "zone": [[0.0, 0.0], [2.0, 0.0]]}});
    let got = base(
        json!({"t": {"type": "tracker"}, "r": bad}),
        json!([{"from": "det.objects", "to": "t.objects"}, {"from": "t.objects", "to": "r.objects"}]),
    );
    assert!(
        got.contains("at least 3") && got.contains("normalised"),
        "{got}"
    );
}

/// Control frames: the detector runs every sixth frame, and sooner while a
/// new track waits to be confirmed; the crop model runs on all of them.
#[test]
fn control_frames_run_the_detector_less_and_the_crop_model_always() {
    let f = files(&[
        detector(),
        (
            "cls.onnx",
            &[1, 3, 8, 8],
            vec![("probs", vec![1, 2], vec![0.3, 0.7])],
        ),
    ]);
    for background in [false, true] {
        let graph = json!({
            "nodes": {
                "camera": {"type": "source"},
                "det": {"type": "model", "model": "det.onnx", "pre": pre(64, 32),
                        "decode": {"type": "detections_e2e", "output": "rows"}, "labels": ["person", "dog"]},
                "persons": {"type": "filter", "classes": ["person"]},
                "tracks": {"type": "tracker", "control": {"tracker_frames": 5, "background": background}},
                "crop": {"type": "crop"},
                "cls": {"type": "model", "model": "cls.onnx", "pre": pre(8, 8),
                        "decode": {"type": "classify", "output": "probs", "softmax": false, "topk": 1}}
            },
            "links": [
                {"from": "camera.frame", "to": "det.frame"},
                {"from": "det.objects", "to": "persons.objects"},
                {"from": "persons.objects", "to": "tracks.objects"},
                {"from": "camera.frame", "to": "crop.frame"},
                {"from": "tracks.objects", "to": "crop.objects"},
                {"from": "crop.regions", "to": "cls.frame"}
            ]
        });
        let (graph, checked) = parse(&graph).unwrap();
        let frame = Frame::filled(64, 32, [1, 2, 3]).unwrap();
        let runtime = Runtime::available()[0];
        let (mut plan, _) = compile(&graph, &checked, &f.files, &execution(runtime), 2).unwrap();
        plan.set_deterministic(true);
        let (mut det_runs, mut cls_runs) = (vec![], 0);
        for n in 0..20 {
            let tick = plan.tick_at(&frame, n as f64 / 30.0);
            assert!(tick.errors.is_empty(), "{:?}", tick.errors);
            if tick.timing.get("det").is_some_and(|t| t.runs > 0) {
                det_runs.push(n);
            }
            cls_runs += tick.timing.get("cls").map_or(0, |t| t.runs);
            let ids: Vec<&str> = tick.results.iter().map(|(id, _)| id.as_str()).collect();
            assert_eq!(ids, ["tracks"]);
            if n >= 2 {
                let o = objects(&tick.results[0].1);
                assert_eq!(o.len(), 1, "frame {n}");
                assert_eq!(o[0].tracker_id, Some(1), "frame {n}");
            }
        }
        // Three runs in a row confirm the track; then one in six.
        assert_eq!(det_runs, [0, 1, 2, 7, 12, 17], "background: {background}");
        assert_eq!(cls_runs, 20);
    }
}
