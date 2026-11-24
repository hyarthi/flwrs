//! What an ONNX file says about itself, read without loading the model: its
//! inputs and outputs, its metadata, and from those a proposed
//! pre-processing and decoder. The file is walked as protobuf, skipping the
//! weights, so a large model costs no memory and no runtime is needed.

use crate::decode::Decoder;
use crate::graph::config::{LabelSource, Labels};
use crate::labels::{self, Sources};
use crate::pre::Pre;
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::File;
use std::io::{self, BufReader, Read};
use std::path::Path;

/// The longest name or metadata value kept.
const MAX_TEXT: u64 = 1 << 20;
/// Inputs or outputs listed, at most.
const MAX_PORTS: usize = 256;
/// Dimensions of one tensor, at most.
const MAX_DIMS: usize = 16;

/// One input or output tensor.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Port {
    pub name: String,
    pub dtype: &'static str,
    /// `null`: decided at run time.
    pub shape: Vec<Option<i64>>,
}

/// What a model file says about itself.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct ModelInfo {
    pub inputs: Vec<Port>,
    pub outputs: Vec<Port>,
    /// The Ultralytics task, if the file names one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    /// Classes named in the file's metadata.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub classes: Option<usize>,
    /// Settings that fit the file, to start from.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proposal: Option<Value>,
    /// With no proposal: the decoders whose output shape this could be.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub candidates: Vec<&'static str>,
    /// The file's size.
    pub file_bytes: u64,
    /// Weights: the values in the file's initializers.
    pub parameters: u64,
    /// Its first input's first dimension is left open, so one run could take
    /// several crops (`batched` execution groups).
    pub dynamic_batch: bool,
    /// Multiply-adds per run, where this build can count them (`tract`
    /// feature, a model of at most 16 MiB with a fixed input size).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flops: Option<u64>,
    /// How an execution group might run it, from the facts above.
    pub recommendation: Recommendation,
}

/// A starting point for a model's execution group. It depends only on the
/// file, so a save gives the same values on any machine.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Recommendation {
    /// `sequential`, `parallel` or `batched`.
    pub mode: &'static str,
    /// `null`: most of the node's threads.
    pub threads: Option<u32>,
    pub reason: &'static str,
}

/// Up to this many multiply-adds a model counts as small.
const SMALL_FLOPS: u64 = 500_000_000;
/// Without a count: up to this many parameters.
const SMALL_PARAMETERS: u64 = 3_000_000;
/// The largest file whose multiply-adds are counted.
#[cfg(feature = "tract")]
const MAX_FLOPS_BYTES: u64 = 16 << 20;

fn recommend(flops: Option<u64>, parameters: u64, dynamic_batch: bool) -> Recommendation {
    let small = flops.map_or(parameters <= SMALL_PARAMETERS, |f| f <= SMALL_FLOPS);
    match (small, dynamic_batch) {
        (true, true) => Recommendation {
            mode: "batched",
            threads: Some(2),
            reason: "a small model with a dynamic batch",
        },
        (true, false) => Recommendation {
            mode: "parallel",
            threads: Some(2),
            reason: "a small model: a few threads, its crops side by side",
        },
        (false, _) => Recommendation {
            mode: "sequential",
            threads: None,
            reason: "a large model: most threads, one run at a time",
        },
    }
}

struct Walker {
    file: BufReader<File>,
    at: u64,
}

enum Field {
    Int(u64),
    /// A length-delimited field, ending at this offset.
    Bytes(u64),
}

fn bad(what: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("not an ONNX file ({what})"),
    )
}

impl Walker {
    fn varint(&mut self) -> io::Result<u64> {
        let mut value = 0u64;
        for shift in (0..64).step_by(7) {
            let mut byte = [0u8];
            self.file.read_exact(&mut byte)?;
            self.at += 1;
            value |= u64::from(byte[0] & 0x7f) << shift;
            if byte[0] & 0x80 == 0 {
                return Ok(value);
            }
        }
        Err(bad("a number is too long"))
    }

    fn skip_to(&mut self, end: u64) -> io::Result<()> {
        let by = i64::try_from(end - self.at).map_err(|_| bad("a field is too long"))?;
        self.file.seek_relative(by)?;
        self.at = end;
        Ok(())
    }

    /// Calls `f` for each field of the message ending at `end`. Whatever `f`
    /// leaves unread of a field is skipped.
    fn fields(
        &mut self,
        end: u64,
        f: &mut dyn FnMut(&mut Self, u64, Field) -> io::Result<()>,
    ) -> io::Result<()> {
        while self.at < end {
            let key = self.varint()?;
            let (number, wire) = (key >> 3, key & 7);
            match wire {
                0 => {
                    let v = self.varint()?;
                    f(self, number, Field::Int(v))?;
                }
                1 | 5 => {
                    let size = if wire == 1 { 8 } else { 4 };
                    let to = self.at + size;
                    if to > end {
                        return Err(bad("a field runs past its message"));
                    }
                    self.skip_to(to)?;
                }
                2 => {
                    let len = self.varint()?;
                    let to = self.at.checked_add(len).filter(|t| *t <= end);
                    let to = to.ok_or_else(|| bad("a field runs past its message"))?;
                    f(self, number, Field::Bytes(to))?;
                    if self.at > to {
                        return Err(bad("a field runs past its message"));
                    }
                    self.skip_to(to)?;
                }
                _ => return Err(bad("an unknown field type")),
            }
        }
        if self.at == end {
            Ok(())
        } else {
            Err(bad("a message runs past its end"))
        }
    }

    fn text(&mut self, end: u64) -> io::Result<Option<String>> {
        let len = end - self.at;
        if len > MAX_TEXT {
            return Ok(None);
        }
        let mut bytes = vec![0; len as usize];
        self.file.read_exact(&mut bytes)?;
        self.at = end;
        Ok(String::from_utf8(bytes).ok())
    }

    fn value_info(&mut self, end: u64) -> io::Result<Port> {
        let mut port = Port {
            name: String::new(),
            dtype: "unknown",
            shape: vec![],
        };
        self.fields(end, &mut |w, n, field| match (n, field) {
            (1, Field::Bytes(to)) => {
                port.name = w.text(to)?.unwrap_or_default();
                Ok(())
            }
            (2, Field::Bytes(to)) => w.fields(to, &mut |w, n, field| match (n, field) {
                (1, Field::Bytes(to)) => w.tensor_type(to, &mut port),
                _ => Ok(()),
            }),
            _ => Ok(()),
        })?;
        Ok(port)
    }

    fn tensor_type(&mut self, end: u64, port: &mut Port) -> io::Result<()> {
        self.fields(end, &mut |w, n, field| match (n, field) {
            (1, Field::Int(elem)) => {
                port.dtype = dtype(elem);
                Ok(())
            }
            (2, Field::Bytes(to)) => w.fields(to, &mut |w, n, field| match (n, field) {
                (1, Field::Bytes(to)) => {
                    let mut dim = None;
                    w.fields(to, &mut |_, n, field| {
                        if let (1, Field::Int(v)) = (n, field) {
                            dim = i64::try_from(v).ok();
                        }
                        Ok(())
                    })?;
                    if port.shape.len() == MAX_DIMS {
                        return Err(bad("too many dimensions"));
                    }
                    port.shape.push(dim);
                    Ok(())
                }
                _ => Ok(()),
            }),
            _ => Ok(()),
        })
    }
}

fn dtype(elem: u64) -> &'static str {
    match elem {
        1 => "f32",
        2 => "u8",
        3 => "i8",
        4 => "u16",
        5 => "i16",
        6 => "i32",
        7 => "i64",
        9 => "bool",
        10 => "f16",
        11 => "f64",
        12 => "u32",
        13 => "u64",
        _ => "unknown",
    }
}

/// Reads a model file's description and proposes settings for it.
pub fn inspect(path: &Path) -> Result<ModelInfo, String> {
    read(path).map_err(|e| e.to_string())
}

fn read(path: &Path) -> io::Result<ModelInfo> {
    let file = File::open(path)?;
    let size = file.metadata()?.len();
    let mut w = Walker {
        file: BufReader::with_capacity(64 * 1024, file),
        at: 0,
    };
    let (mut inputs, mut outputs) = (vec![], vec![]);
    let mut weights: HashSet<String> = HashSet::new();
    let mut metadata: HashMap<String, String> = HashMap::new();
    let mut has_graph = false;
    let mut parameters = 0u64;
    w.fields(size, &mut |w, n, field| match (n, field) {
        (7, Field::Bytes(to)) => {
            has_graph = true;
            w.fields(to, &mut |w, n, field| match (n, field) {
                // Weights: older exports list them among the inputs too.
                (5, Field::Bytes(to)) => {
                    let mut count = 1u64;
                    w.fields(to, &mut |w, n, field| match (n, field) {
                        (1, Field::Int(d)) => {
                            count = count.saturating_mul(d);
                            Ok(())
                        }
                        // Packed dimensions.
                        (1, Field::Bytes(to)) => {
                            while w.at < to {
                                count = count.saturating_mul(w.varint()?);
                            }
                            Ok(())
                        }
                        (8, Field::Bytes(to)) => {
                            weights.extend(w.text(to)?);
                            Ok(())
                        }
                        _ => Ok(()),
                    })?;
                    parameters = parameters.saturating_add(count);
                    Ok(())
                }
                (11, Field::Bytes(to)) if inputs.len() < MAX_PORTS => {
                    inputs.push(w.value_info(to)?);
                    Ok(())
                }
                (12, Field::Bytes(to)) if outputs.len() < MAX_PORTS => {
                    outputs.push(w.value_info(to)?);
                    Ok(())
                }
                _ => Ok(()),
            })
        }
        (14, Field::Bytes(to)) => {
            let (mut key, mut value) = (None, None);
            w.fields(to, &mut |w, n, field| match (n, field) {
                (1, Field::Bytes(to)) => {
                    key = w.text(to)?;
                    Ok(())
                }
                (2, Field::Bytes(to)) => {
                    value = w.text(to)?;
                    Ok(())
                }
                _ => Ok(()),
            })?;
            if let (Some(k), Some(v)) = (key, value) {
                metadata.insert(k, v);
            }
            Ok(())
        }
        _ => Ok(()),
    })?;
    if !has_graph {
        return Err(bad("it has no graph"));
    }
    inputs.retain(|p| !weights.contains(&p.name));
    let mut info = describe(inputs, outputs, &metadata);
    info.file_bytes = size;
    info.parameters = parameters;
    info.dynamic_batch = info
        .inputs
        .first()
        .is_some_and(|p| p.shape.len() >= 2 && p.shape[0].is_none());
    info.flops = count_flops(path, &info);
    info.recommendation = recommend(info.flops, parameters, info.dynamic_batch);
    Ok(info)
}

/// Multiply-adds per run, for one input of a fixed size (a batch of one).
#[cfg(feature = "tract")]
fn count_flops(path: &Path, info: &ModelInfo) -> Option<u64> {
    let [input] = &info.inputs[..] else {
        return None;
    };
    if info.file_bytes > MAX_FLOPS_BYTES {
        return None;
    }
    let (first, rest) = input.shape.split_first()?;
    let mut shape = vec![usize::try_from(first.unwrap_or(1)).ok()?];
    for d in rest {
        shape.push(usize::try_from((*d)?).ok()?);
    }
    let dtype = match input.dtype {
        "f32" => crate::pre::DType::F32,
        "i32" => crate::pre::DType::I32,
        "u8" => crate::pre::DType::U8,
        _ => return None,
    };
    crate::backend::flops(path, &shape, dtype)
}

#[cfg(not(feature = "tract"))]
fn count_flops(_path: &Path, _info: &ModelInfo) -> Option<u64> {
    None
}

fn describe(
    inputs: Vec<Port>,
    outputs: Vec<Port>,
    metadata: &HashMap<String, String>,
) -> ModelInfo {
    let task = metadata.get("task").cloned();
    let no_files = BTreeMap::new();
    let lookup = |k: &str| metadata.get(k).cloned();
    let sources = Sources {
        files: &no_files,
        metadata: &lookup,
    };
    let names = Labels::From(LabelSource::ModelMetadata("names".into()));
    let classes = metadata
        .contains_key("names")
        .then(|| labels::labels(Some(&names), &sources).ok())
        .flatten()
        .map(|l| l.len())
        .filter(|n| *n > 0);
    let mut info = ModelInfo {
        inputs,
        outputs,
        task,
        classes,
        ..Default::default()
    };
    let decode = match info.task.as_deref() {
        Some(task) => ultralytics_decoder(task, &info, metadata),
        None => decoder_by_outputs(&info.outputs),
    };
    // Only what the plugin would accept is proposed.
    let decode = decode.filter(|d| serde_json::from_value::<Decoder>(d.clone()).is_ok());
    match decode {
        Some(decode) => {
            let mut proposal = json!({ "decode": decode });
            if info.task.is_some() {
                if let Some(pre) = ultralytics_pre(&info, metadata) {
                    proposal["pre"] = pre;
                }
                if info.classes.is_some() {
                    proposal["labels"] = json!({ "model_metadata": "names" });
                }
            }
            info.proposal = Some(proposal);
        }
        None => info.candidates = candidates(&info.outputs),
    }
    info
}

/// Ultralytics exports' pre-processing: letterboxed RGB scaled to 0..1.
fn ultralytics_pre(info: &ModelInfo, metadata: &HashMap<String, String>) -> Option<Value> {
    let input = info.inputs.first()?;
    let (layout, h, w) = match input.shape[..] {
        [_, Some(3), h, w] => ("nchw", h, w),
        [_, h, w, Some(3)] => ("nhwc", h, w),
        _ => return None,
    };
    let declared = metadata
        .get("imgsz")
        .and_then(|s| serde_json::from_str::<Vec<i64>>(s).ok());
    let (h, w) = match (h, w, declared.as_deref()) {
        (Some(h), Some(w), _) => (h, w),
        (_, _, Some([h, w])) => (*h, *w),
        _ => return None,
    };
    let pre = json!({
        "width": w, "height": h, "resize": "letterbox", "pad": 114, "color": "rgb",
        "dtype": "f32", "scale": 1.0 / 255.0, "layout": layout,
    });
    serde_json::from_value::<Pre>(pre.clone())
        .ok()
        .filter(|p| p.problems().is_empty())
        .map(|_| pre)
}

fn ultralytics_decoder(
    task: &str,
    info: &ModelInfo,
    metadata: &HashMap<String, String>,
) -> Option<Value> {
    let [output] = &info.outputs[..] else {
        return None;
    };
    if task == "classify" {
        return Some(json!({ "type": "classify", "output": output.name, "softmax": false }));
    }
    if task != "detect" && task != "pose" {
        return None;
    }
    let classes = info.classes?;
    let (points, dims) = match metadata
        .get("kpt_shape")
        .and_then(|s| serde_json::from_str::<Vec<usize>>(s).ok())
        .as_deref()
    {
        Some([p, d]) if task == "pose" => (*p, *d),
        _ => (0, 0),
    };
    let row = (4 + classes + points * dims) as i64;
    let [_, a, b] = output.shape[..] else {
        return None;
    };
    let channels_first = match (a, b) {
        (Some(a), _) if a == row => true,
        (_, Some(b)) if b == row => false,
        // End-to-end: rows of box, score, class.
        (_, Some(6)) if points == 0 => {
            return Some(json!({ "type": "detections_e2e", "output": output.name }));
        }
        _ => return None,
    };
    Some(json!({
        "type": "yolo", "output": output.name, "channels_first": channels_first,
        "classes": classes, "keypoints": points,
    }))
}

/// Models without metadata, where output names or shapes settle it.
fn decoder_by_outputs(outputs: &[Port]) -> Option<Value> {
    let has = |name: &str| outputs.iter().any(|p| p.name == name);
    if has("logits") && has("pred_boxes") {
        return Some(json!({
            "type": "detr", "logits": "logits", "boxes": "pred_boxes", "activation": "sigmoid",
        }));
    }
    if has("dets") && has("keypoints") {
        return Some(json!({ "type": "rtmo", "dets": "dets", "keypoints": "keypoints" }));
    }
    if has("simcc_x") && has("simcc_y") {
        return Some(json!({ "type": "simcc", "x": "simcc_x", "y": "simcc_y", "split": 2.0 }));
    }
    let [output] = outputs else {
        return None;
    };
    match output.shape[..] {
        [_, Some(1), Some(17), Some(3)] => {
            Some(json!({ "type": "movenet", "output": output.name, "multi": false }))
        }
        [_, _, Some(56)] => {
            Some(json!({ "type": "movenet", "output": output.name, "multi": true }))
        }
        _ => None,
    }
}

/// The decoders an output's shape could belong to.
fn candidates(outputs: &[Port]) -> Vec<&'static str> {
    match outputs {
        [output] => match output.shape[..] {
            [_, _] => vec!["classify", "embedding", "landmarks", "ocr_det"],
            [_, _, Some(6)] => vec!["detections_e2e"],
            [_, _, _] => vec!["yolo", "ctc", "dense_map", "landmarks"],
            [_, _, _, _] => vec!["semantic", "heatmap", "dense_map", "ocr_det", "embedding"],
            _ => vec!["raw"],
        },
        // Several outputs with no telling names: per-stride heads, anchors.
        _ => vec!["gfl", "yunet", "ssd_anchors", "landmarks", "raw"],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tensor::Output;
    use crate::testing;

    fn file(bytes: &[u8]) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("m.onnx");
        std::fs::write(&path, bytes).unwrap();
        (dir, path)
    }

    fn outputs(shape: &[usize]) -> Output {
        Output::new(shape.to_vec(), vec![0.0; shape.iter().product()]).unwrap()
    }

    #[test]
    fn reads_ports_and_leaves_out_weights() {
        let (_d, path) = file(&testing::identity(&[1, 3, 8, 16], testing::UINT8));
        let info = inspect(&path).unwrap();
        assert_eq!(info.inputs[0].name, "x");
        assert_eq!(info.inputs[0].dtype, "u8");
        assert_eq!(
            info.inputs[0].shape,
            vec![Some(1), Some(3), Some(8), Some(16)]
        );
        assert_eq!(info.outputs[0].name, "y");

        let out = outputs(&[1, 2]);
        let (_d, path) = file(&testing::constant_outputs(
            &[1, 3, 8, 8],
            &[("o", &out)],
            &[],
            &[],
        ));
        let info = inspect(&path).unwrap();
        assert_eq!(info.inputs.len(), 1, "weights aren't inputs");
        assert_eq!(
            info.candidates,
            ["classify", "embedding", "landmarks", "ocr_det"]
        );
    }

    #[test]
    fn proposes_settings_from_ultralytics_metadata() {
        let names = "{0: 'person', 1: 'dog'}";
        let out = outputs(&[1, 6 + 51, 4]);
        let bytes = testing::constant_outputs(
            &[1, 3, 320, 640],
            &[("output0", &out)],
            &[],
            &[("task", "pose"), ("names", names), ("kpt_shape", "[17, 3]")],
        );
        let (_d, path) = file(&bytes);
        let info = inspect(&path).unwrap();
        assert_eq!(info.classes, Some(2));
        let proposal = info.proposal.unwrap();
        assert_eq!(proposal["decode"]["type"], "yolo");
        assert_eq!(proposal["decode"]["channels_first"], true);
        assert_eq!(proposal["decode"]["keypoints"], 17);
        assert_eq!(proposal["pre"]["width"], 640);
        assert_eq!(proposal["pre"]["height"], 320);
        assert_eq!(proposal["labels"]["model_metadata"], "names");
    }

    #[test]
    fn an_end_to_end_export_is_told_from_a_raw_one() {
        let out = outputs(&[1, 300, 6]);
        let bytes = testing::constant_outputs(
            &[1, 3, 64, 64],
            &[("output0", &out)],
            &[],
            &[("task", "detect"), ("names", "{0: 'a', 1: 'b', 2: 'c'}")],
        );
        let (_d, path) = file(&bytes);
        let proposal = inspect(&path).unwrap().proposal.unwrap();
        assert_eq!(proposal["decode"]["type"], "detections_e2e");
    }

    #[test]
    fn output_names_settle_it_without_metadata() {
        let (x, y) = (outputs(&[1, 17, 384]), outputs(&[1, 17, 512]));
        let bytes = testing::constant_outputs(
            &[1, 3, 256, 192],
            &[("simcc_x", &x), ("simcc_y", &y)],
            &[],
            &[],
        );
        let (_d, path) = file(&bytes);
        let info = inspect(&path).unwrap();
        assert_eq!(info.proposal.unwrap()["decode"]["type"], "simcc");
    }

    #[test]
    fn counts_weights_and_a_dynamic_batch() {
        let out = outputs(&[1, 2, 3]);
        let bytes = testing::constant_outputs(&[1, 3, 8, 8], &[("o", &out)], &[], &[]);
        let (_d, path) = file(&bytes);
        let info = inspect(&path).unwrap();
        assert_eq!(info.file_bytes, bytes.len() as u64);
        // The output's six values and the scalar `zero`.
        assert_eq!(info.parameters, 7);
        assert!(!info.dynamic_batch);
        assert_eq!(info.recommendation.mode, "parallel");

        let (_d, path) = file(&testing::mean_colour(None, 8, 8));
        let info = inspect(&path).unwrap();
        assert!(info.dynamic_batch);
        assert_eq!(info.inputs[0].shape[0], None);
        assert_eq!(info.recommendation.mode, "batched");
    }

    #[test]
    fn a_large_model_is_recommended_most_threads() {
        let r = recommend(None, SMALL_PARAMETERS + 1, true);
        assert_eq!((r.mode, r.threads), ("sequential", None));
        let r = recommend(Some(SMALL_FLOPS), SMALL_PARAMETERS + 1, false);
        assert_eq!(r.mode, "parallel", "a count outranks the parameters");
    }

    #[test]
    fn garbage_is_refused_not_a_panic() {
        for bytes in [
            &b""[..],
            b"not a model at all",
            &[0xff; 64],
            &[0x3a, 0xff, 0x7f],
        ] {
            let (_d, path) = file(bytes);
            assert!(inspect(&path).is_err(), "{bytes:?}");
        }
    }
}
