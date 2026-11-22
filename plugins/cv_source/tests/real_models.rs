//! Real models against reference results (ignored: needs the models, the
//! images and the references, which aren't in the repository).
//!
//! `FLWRS_CV_MODELS` is a folder with the example graphs' files (models,
//! label and config files, by the names the graphs use), `FLWRS_CV_IMAGES`
//! one with JPEG images, and `FLWRS_CV_REFERENCE` one with a
//! `<graph>.jsonl` per example graph: one line per image, `{"image": <file
//! stem>, "results": [[<node id>, <result>], …]}`, a result being
//! `{"objects": [{"class", "score", "bbox", "kps"?}]}`, `{"classes":
//! [[id, label, score], …]}`, `{"map": {"summary": …}}`, `{"text": [text,
//! score]}` or `{"embedding": {"dim"}}`; an object's `children` are
//! `[[<node id>, <result>], …]`, what ran on its region. Each graph runs
//! whole; its once-per-frame models' results are compared, with what ran on
//! their objects. `FLWRS_CV_RUNTIME=tract` uses tract.
//!
//! ```sh
//! FLWRS_CV_MODELS=… FLWRS_CV_IMAGES=… FLWRS_CV_REFERENCE=… \
//!   cargo test -p flwrs-cv-source --release --test real_models -- --ignored --nocapture
//! ```

use flwrs_cv_source::backend::Runtime;
use flwrs_cv_source::frame::Frame;
use flwrs_cv_source::graph::config::Execution;
use flwrs_cv_source::graph::{self, exec};
use flwrs_cv_source::results::{MapSummary, Object, Results};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

fn folder(var: &str) -> PathBuf {
    PathBuf::from(
        std::env::var(var).unwrap_or_else(|_| panic!("set {var} (see the file's comment)")),
    )
}

fn iou(a: [f32; 4], b: [f32; 4]) -> f32 {
    let ix = (a[2].min(b[2]) - a[0].max(b[0])).max(0.0);
    let iy = (a[3].min(b[3]) - a[1].max(b[1])).max(0.0);
    let inter = ix * iy;
    inter / ((a[2] - a[0]) * (a[3] - a[1]) + (b[2] - b[0]) * (b[3] - b[1]) - inter)
}

fn f(v: &Value) -> f32 {
    v.as_f64().unwrap_or(f64::NAN) as f32
}

/// Clear detections only (scores near a threshold may flip either way).
const CLEAR: f32 = 0.35;

/// Differences between our objects and the reference's, if any.
fn compare_objects(ours: &[Object], reference: &[Value]) -> Vec<String> {
    let mut problems = vec![];
    let matches = |o: &Object, r: &Value| {
        let rb: Vec<f32> = r["bbox"].as_array().unwrap().iter().map(f).collect();
        o.class_id as u64 == r["class"].as_u64().unwrap()
            && iou(o.bbox, [rb[0], rb[1], rb[2], rb[3]]) > 0.9
            && (o.confidence - f(&r["score"])).abs() < 0.03
    };
    for r in reference.iter().filter(|r| f(&r["score"]) >= CLEAR) {
        match ours.iter().find(|o| matches(o, r)) {
            None => problems.push(format!("missing {r}")),
            Some(o) => {
                // What ran on it (models on its region): by node id.
                for child in r["children"].as_array().cloned().unwrap_or_default() {
                    let id = child[0].as_str().unwrap();
                    match o.children.iter().find(|(c, _)| c == id) {
                        None => problems.push(format!("no {id} result on {r}")),
                        Some((_, ours)) => problems.extend(
                            compare(ours, &child[1])
                                .into_iter()
                                .map(|p| format!("{id}: {p}")),
                        ),
                    }
                }
                // Keypoints within 2 % of the object's size (at least 2 px)
                // where both are confident: near-ties in a pose model's
                // argmax move a point by a bin or two.
                let size = (o.bbox[2] - o.bbox[0]).max(o.bbox[3] - o.bbox[1]);
                let near = (0.02 * size).max(2.0);
                let kps = r["kps"].as_array().cloned().unwrap_or_default();
                for (k, rk) in o.keypoints.iter().zip(&kps) {
                    let rk: Vec<f32> = rk.as_array().unwrap().iter().map(f).collect();
                    if k.confidence > 0.5
                        && rk[2] > 0.5
                        && ((k.x - rk[0]).abs() > near || (k.y - rk[1]).abs() > near)
                    {
                        problems.push(format!("keypoint ({}, {}) vs {rk:?} in {r}", k.x, k.y));
                        break;
                    }
                }
            }
        }
    }
    for o in ours.iter().filter(|o| o.confidence >= CLEAR + 0.05) {
        if !reference.iter().any(|r| matches(o, r)) {
            problems.push(format!(
                "extra {} {:.2} {:?}",
                o.class_id, o.confidence, o.bbox
            ));
        }
    }
    problems
}

fn compare(ours: &Results, reference: &Value) -> Vec<String> {
    match ours {
        Results::Objects(o) => compare_objects(o, reference["objects"].as_array().unwrap()),
        Results::Classes(c) => {
            let top = &reference["classes"][0];
            if c[0].class_id as u64 != top[0].as_u64().unwrap()
                || (c[0].confidence - f(&top[2])).abs() > 0.02
            {
                vec![format!("top class {:?} vs {top}", c[0])]
            } else {
                vec![]
            }
        }
        Results::Map(m) => match &m.summary {
            MapSummary::Classes(c) => {
                let top = &reference["map"]["summary"][0];
                let name = c[0].0.class_name.clone().unwrap_or_default();
                if name != top["label"].as_str().unwrap()
                    || (c[0].1 - f(&top["share"])).abs() > 0.02
                {
                    vec![format!("top class {name} {} vs {top}", c[0].1)]
                } else {
                    vec![]
                }
            }
            MapSummary::Values { min, max, mean, .. } => {
                let s = &reference["map"]["summary"];
                let near = |a: f32, b: &Value| (a - f(b)).abs() <= 0.01 + 0.01 * f(b).abs();
                if near(*min, &s["min"]) && near(*max, &s["max"]) && near(*mean, &s["mean"]) {
                    vec![]
                } else {
                    vec![format!("min {min} max {max} mean {mean} vs {s}")]
                }
            }
        },
        Results::Text(t) => {
            let text = t.as_ref().map(|t| t.text.as_str()).unwrap_or_default();
            let r = reference["text"][0].as_str().unwrap();
            if text == r {
                vec![]
            } else {
                vec![format!("{text:?} vs {r:?}")]
            }
        }
        Results::Vector(v) => {
            let dim = reference["embedding"]["dim"].as_u64().unwrap() as usize;
            if v.len() == dim {
                vec![]
            } else {
                vec![format!("{} values vs {dim}", v.len())]
            }
        }
        Results::Tensor(_) => vec![],
    }
}

fn frame(path: &Path) -> Frame {
    let img = image::open(path).unwrap().to_rgb8();
    Frame::new(img.width(), img.height(), img.into_raw()).unwrap()
}

#[test]
#[ignore = "needs real models, images and reference results (see the file's comment)"]
fn real_models_match_the_reference() {
    let (models, images, reference) = (
        folder("FLWRS_CV_MODELS"),
        folder("FLWRS_CV_IMAGES"),
        folder("FLWRS_CV_REFERENCE"),
    );
    let runtime = match std::env::var("FLWRS_CV_RUNTIME").as_deref() {
        Ok("tract") => Runtime::Tract,
        _ => Runtime::Ort,
    };
    let files: BTreeMap<String, PathBuf> = std::fs::read_dir(&models)
        .unwrap()
        .map(|e| e.unwrap().path())
        .map(|p| (p.file_name().unwrap().to_string_lossy().into_owned(), p))
        .collect();
    let examples = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/graphs");
    let mut graphs: Vec<PathBuf> = std::fs::read_dir(examples)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    graphs.sort();
    let (mut compared, mut failures) = (0, vec![]);
    for path in graphs {
        let name = path.file_stem().unwrap().to_string_lossy().into_owned();
        let Ok(lines) = std::fs::read_to_string(reference.join(format!("{name}.jsonl"))) else {
            println!("{name}: no reference, skipped");
            continue;
        };
        let example: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let (graph, checked) = graph::parse(&example["graph"]).unwrap();
        let execution = Execution {
            runtime: Some(runtime),
            ..Execution::default()
        };
        let mut plan = match exec::compile(&graph, &checked, &files, &execution, 4) {
            Ok((plan, _)) => plan,
            Err(problems) => {
                failures.extend(problems.into_iter().map(|p| format!("{name}: {p}")));
                continue;
            }
        };
        for line in lines.lines() {
            let line: Value = serde_json::from_str(line).unwrap();
            let image = line["image"].as_str().unwrap();
            let frame = frame(&images.join(format!("{image}.jpg")));
            let tick = plan.tick(&frame);
            failures.extend(
                tick.errors
                    .iter()
                    .map(|e| format!("{name} on {image}: {e}")),
            );
            for (id, results) in &tick.results {
                let reference = line["results"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|r| r[0] == id.as_str())
                    .map(|r| &r[1]);
                let Some(reference) = reference else { continue };
                compared += 1;
                for p in compare(results, reference) {
                    failures.push(format!("{name}/{id} on {image}: {p}"));
                }
            }
        }
        println!("{name}: compared");
    }
    println!("{compared} runs compared, {} differences", failures.len());
    assert!(compared > 0, "nothing to compare");
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
