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
            "camera": {"kind": "source"},
            "det": {"kind": "model", "model": "det.onnx", "pre": pre(64, 32),
                    "decode": {"type": "detections_e2e", "output": "rows"}, "labels": ["person", "dog"]},
            "people": {"kind": "crop", "classes": ["person"]},
            "cls": {"kind": "model", "model": "cls.onnx", "pre": pre(8, 8),
                    "decode": {"type": "classify", "output": "probs", "softmax": false, "topk": 1}},
            "sure": {"kind": "filter", "min_confidence": 0.85}
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
            "camera": {"kind": "source"},
            "hand": {"kind": "model", "model": "hand.onnx", "pre": pre(16, 16),
                     "decode": {"type": "landmarks", "output": "lm", "points": 2, "dims": 3}},
            "gesture": {"kind": "model", "model": "gesture.onnx", "tensor_input": {"dims": 2, "normalize": "wrist"},
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
            "camera": {"kind": "source"},
            "det": {"kind": "model", "model": "det.onnx", "pre": pre(64, 32),
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
                "camera": {"kind": "source"},
                "det": {"kind": "model", "model": "det.onnx", "pre": pre(64, 32),
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
