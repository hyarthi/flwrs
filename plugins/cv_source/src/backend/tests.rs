//! Generated models through each runtime this build has.

use super::{Options, Runtime, load};
use crate::decode::fixtures;
use crate::frame::Frame;
use crate::graph::config::{Execution, ModelConfig};
use crate::model::Model;
use crate::pre::DType;
use crate::results::Results;
use crate::tensor::{Output, Tensor, TensorData};
use crate::testing::{self, Rng};
use std::collections::BTreeMap;
use std::path::PathBuf;

fn write(dir: &tempfile::TempDir, name: &str, bytes: &[u8]) -> PathBuf {
    let path = dir.path().join(name);
    std::fs::write(&path, bytes).unwrap();
    path
}

fn options(shape: &[usize], dtype: DType) -> Options {
    Options {
        threads: 1,
        input: Some((shape.to_vec(), dtype)),
        ..Options::default()
    }
}

#[test]
fn inputs_of_each_type_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let shape = [1, 3, 4, 4];
    let mut rng = Rng::new(7);
    let values: Vec<f32> = (0..48).map(|_| rng.below(256) as f32).collect();
    for runtime in Runtime::available() {
        for (elem, dtype, data) in [
            (testing::FLOAT, DType::F32, TensorData::F32(values.clone())),
            (
                testing::INT32,
                DType::I32,
                TensorData::I32(values.iter().map(|v| *v as i32).collect()),
            ),
            (
                testing::UINT8,
                DType::U8,
                TensorData::U8(values.iter().map(|v| *v as u8).collect()),
            ),
        ] {
            let path = write(
                &dir,
                &format!("identity-{elem}.onnx"),
                &testing::identity(&shape, elem),
            );
            let mut session = load(*runtime, &path, &options(&shape, dtype)).unwrap();
            assert_eq!(session.input_names(), ["x"]);
            assert!(session.device().starts_with("cpu"), "{}", session.device());
            assert_eq!(session.runtime(), *runtime);
            let out = session
                .run(
                    "x",
                    Tensor {
                        shape: shape.to_vec(),
                        data,
                    },
                )
                .unwrap();
            assert_eq!(out["y"].shape(), shape, "{runtime:?} {dtype:?}");
            assert_eq!(out["y"].data(), values.as_slice(), "{runtime:?} {dtype:?}");
        }
    }
}

/// Every decoder fixture as a model's outputs: decoded after a run, the
/// results are the same as decoding the tensors directly.
#[test]
fn every_decoder_runs_through_each_runtime() {
    let dir = tempfile::tempdir().unwrap();
    let input = [1, 3, 8, 8];
    let zeros = || Tensor {
        shape: input.to_vec(),
        data: TensorData::F32(vec![0.0; 3 * 64]),
    };
    for f in fixtures::all() {
        let outputs = f.outputs();
        let named: Vec<(&str, &Output)> = outputs.iter().map(|(n, o)| (n.as_str(), o)).collect();
        let path = write(
            &dir,
            &format!("{}.onnx", f.name),
            &testing::constant_outputs(&input, &named, &[], &[]),
        );
        let expected = f.decode(&outputs).unwrap();
        for runtime in Runtime::available() {
            let mut session = load(*runtime, &path, &options(&input, DType::F32))
                .unwrap_or_else(|e| panic!("{}: {e}", f.name));
            let got = session
                .run("x", zeros())
                .unwrap_or_else(|e| panic!("{} {runtime:?}: {e}", f.name));
            assert_eq!(f.decode(&got).unwrap(), expected, "{} {runtime:?}", f.name);
        }
    }
}

/// Integer and quantised outputs are read as `f32`.
#[test]
fn integer_outputs_are_read_as_floats() {
    let dir = tempfile::tempdir().unwrap();
    let input = [1, 3, 2, 2];
    let a = Output::new(vec![1, 4], vec![0.0, 1.0, 200.0, 255.0]).unwrap();
    let b = Output::new(vec![2], vec![-128.0, 127.0]).unwrap();
    let c = Output::new(vec![1], vec![-70_000.0]).unwrap();
    let model = testing::constant_outputs(
        &input,
        &[("a", &a), ("b", &b), ("c", &c)],
        &[
            ("a", testing::UINT8),
            ("b", testing::INT8),
            ("c", testing::INT32),
        ],
        &[],
    );
    let path = write(&dir, "ints.onnx", &model);
    for runtime in Runtime::available() {
        let mut session = load(*runtime, &path, &options(&input, DType::F32)).unwrap();
        let out = session
            .run(
                "x",
                Tensor {
                    shape: input.to_vec(),
                    data: TensorData::F32(vec![1.0; 12]),
                },
            )
            .unwrap();
        assert_eq!(out["a"], a, "{runtime:?}");
        assert_eq!(out["b"], b, "{runtime:?}");
        assert_eq!(out["c"], c, "{runtime:?}");
    }
}

/// A model node on a runtime, on the CPU.
fn load_on(
    config: &ModelConfig,
    files: &BTreeMap<String, PathBuf>,
    runtime: Runtime,
) -> Result<Model, String> {
    let execution = Execution {
        runtime: Some(runtime),
        ..Execution::default()
    };
    Model::load(config, files, &execution, 1, None).map(|(m, warning)| {
        assert!(warning.is_none());
        m
    })
}

fn model_config(v: serde_json::Value) -> ModelConfig {
    serde_json::from_value(v).unwrap()
}

/// A model node end to end: its file and labels from the model's metadata,
/// a letterboxed frame, boxes back in frame pixels.
#[test]
fn a_model_node_runs_on_a_frame() {
    let dir = tempfile::tempdir().unwrap();
    let rows = Output::new(vec![1, 1, 6], vec![1.0, 10.0, 11.0, 12.0, 0.9, 1.0]).unwrap();
    let names = "{0: 'cat', 1: 'dog'}";
    let bytes = testing::constant_outputs(
        &[1, 3, 32, 32],
        &[("rows", &rows)],
        &[],
        &[("names", names)],
    );
    let files: BTreeMap<String, PathBuf> =
        [("det.onnx".to_string(), write(&dir, "det.onnx", &bytes))].into();
    let config = model_config(serde_json::json!({
        "model": "det.onnx",
        "pre": {"width": 32, "height": 32, "resize": "letterbox", "pad": 114, "color": "rgb", "dtype": "f32", "layout": "nchw"},
        "decode": {"type": "detections_e2e", "output": "rows"},
        "labels": {"model_metadata": "names"}
    }));
    // 64x32 into 32x32: scale 2, the content in model rows 8..24.
    let frame = Frame::filled(64, 32, [10, 20, 30]).unwrap();
    for runtime in Runtime::available() {
        let mut model = load_on(&config, &files, *runtime).unwrap();
        assert_eq!(model.labels(), ["cat", "dog"]);
        let run = model.run(&frame, frame.full()).unwrap();
        let Results::Objects(o) = &run.results else {
            panic!()
        };
        assert_eq!(o.len(), 1);
        assert_eq!(o[0].class_name.as_deref(), Some("dog"));
        assert_eq!(o[0].bbox, [2.0, 4.0, 22.0, 8.0], "{runtime:?}");
        assert_eq!(run.affine.apply(0.0, 8.0), (0.0, 0.0));
    }
}

#[test]
fn load_problems_are_named() {
    let dir = tempfile::tempdir().unwrap();
    let bytes = testing::identity(&[1, 3, 2, 2], testing::FLOAT);
    let files: BTreeMap<String, PathBuf> =
        [("m.onnx".to_string(), write(&dir, "m.onnx", &bytes))].into();
    let config = |extra: serde_json::Value| {
        let mut v = serde_json::json!({
            "model": "m.onnx",
            "pre": {"width": 2, "height": 2, "resize": "stretch", "color": "rgb", "dtype": "f32", "layout": "nchw"},
            "decode": {"type": "raw", "outputs": ["y"]}
        });
        v.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        model_config(v)
    };
    let runtime = Runtime::available()[0];
    let e = load_on(
        &config(serde_json::json!({"model": "other.onnx"})),
        &files,
        runtime,
    )
    .err()
    .unwrap();
    assert!(e.contains("isn't among the node's files"), "{e}");
    let e = load_on(
        &config(serde_json::json!({"input_name": "images"})),
        &files,
        runtime,
    )
    .err()
    .unwrap();
    assert!(e.contains("no input \"images\" (it has: x)"), "{e}");
    let e = load_on(
        &config(serde_json::json!({"labels": {"model_metadata": "names"}})),
        &files,
        runtime,
    )
    .err()
    .unwrap();
    assert!(e.contains("metadata has no \"names\""), "{e}");
    let broken: BTreeMap<String, PathBuf> = [(
        "m.onnx".to_string(),
        write(&dir, "broken.onnx", b"not a model"),
    )]
    .into();
    let e = load_on(&config(serde_json::json!({})), &broken, runtime)
        .err()
        .unwrap();
    assert!(e.starts_with("broken.onnx ("), "{e}");
    let mut ok = load_on(&config(serde_json::json!({})), &files, runtime).unwrap();
    let frame = Frame::filled(2, 2, [1, 2, 3]).unwrap();
    let Results::Tensor(t) = ok.run(&frame, frame.full()).unwrap().results else {
        panic!()
    };
    assert_eq!(
        t[0].2,
        [1.0, 1.0, 1.0, 1.0, 2.0, 2.0, 2.0, 2.0, 3.0, 3.0, 3.0, 3.0]
    );
}
