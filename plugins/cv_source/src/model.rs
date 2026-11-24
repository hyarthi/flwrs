//! A model node's engine: its session, labels and charset, and one run on a
//! frame region: pre-processing, inference, decoding, results mapped to
//! frame pixels.

use crate::backend::{self, Device, Options, Runtime, Session};
use crate::decode::Context;
use crate::frame::{Affine, Frame, Rect};
use crate::graph::config::{Execution, ModelConfig, Normalize, TensorInput};
use crate::labels::{self, Sources};
use crate::pre;
use crate::results::{HAND_POSE, Object, Results};
use crate::tensor::{Tensor, TensorData};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// A loaded model node.
pub struct Model {
    config: ModelConfig,
    session: Box<dyn Session>,
    input: String,
    labels: Vec<String>,
    charset: Vec<String>,
    /// The cutoff in effect: the node's, else the decoder's default.
    min_confidence: f32,
    /// Turns a hand into this model's input.
    embedder: Option<Embedder>,
    /// The input for one item, when it's known: shape (batch first) and type.
    input_shape: Option<(Vec<usize>, pre::DType)>,
}

/// MediaPipe's gesture embedder.
struct Embedder {
    session: Box<dyn Session>,
    output: String,
}

/// Its inputs, by the names the model gives them.
const EMBEDDER_INPUTS: [&str; 3] = ["hand", "handedness", "world_hand"];

impl Embedder {
    fn run(&mut self, inputs: [Vec<f32>; 3]) -> Result<Vec<f32>, String> {
        let [hand, right, world] = inputs;
        let points = hand.len() / 3;
        let tensor = |shape: Vec<usize>, data| Tensor {
            shape,
            data: TensorData::F32(data),
        };
        let mut out = self.session.run_many(vec![
            (EMBEDDER_INPUTS[0].into(), tensor(vec![1, points, 3], hand)),
            (EMBEDDER_INPUTS[1].into(), tensor(vec![1, 1], right)),
            (EMBEDDER_INPUTS[2].into(), tensor(vec![1, points, 3], world)),
        ])?;
        let value = out
            .remove(&self.output)
            .ok_or_else(|| format!("the embedder gave no output {:?}", self.output))?;
        Ok(value.into_parts().1)
    }

    /// Loads it and runs it once, which gives its embedding's length.
    fn load(
        name: &str,
        files: &BTreeMap<String, PathBuf>,
        runtime: Runtime,
        options: &Options,
    ) -> Result<(Self, usize), String> {
        let path = files
            .get(name)
            .ok_or_else(|| format!("the embedder {name:?} isn't among the node's files"))?;
        let mut session = backend::load(runtime, path, options)?;
        for wanted in EMBEDDER_INPUTS {
            if !session.input_names().iter().any(|n| n == wanted) {
                return Err(format!(
                    "{name}: not a gesture embedder: no input {wanted:?} (it has: {})",
                    session.input_names().join(", ")
                ));
            }
        }
        let zeros = |n: usize, shape: Vec<usize>| Tensor {
            shape,
            data: TensorData::F32(vec![0.0; n]),
        };
        let outputs = session
            .run_many(vec![
                (EMBEDDER_INPUTS[0].into(), zeros(63, vec![1, 21, 3])),
                (EMBEDDER_INPUTS[1].into(), zeros(1, vec![1, 1])),
                (EMBEDDER_INPUTS[2].into(), zeros(63, vec![1, 21, 3])),
            ])
            .map_err(|e| format!("{name}: the first run failed: {e}"))?;
        let [(output, value)] = <[_; 1]>::try_from(outputs.into_iter().collect::<Vec<_>>())
            .map_err(|_| format!("{name}: a gesture embedder has one output"))?;
        let len = value.data().len();
        Ok((Self { session, output }, len))
    }
}

/// One run's results and where its time went.
#[derive(Debug)]
pub struct Run {
    /// Boxes and keypoints in frame pixels; maps in model-input pixels,
    /// placed on the frame by `affine`.
    pub results: Results,
    pub affine: Affine,
    pub timing: Timing,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Timing {
    pub pre: Duration,
    pub infer: Duration,
    pub decode: Duration,
}

impl Model {
    /// Loads a model node: its file from the node's files (by name), its
    /// labels and charset, on the runtime and device its `execution` says
    /// (over `defaults`). Loading includes one warm-up run on a zero input
    /// when the input's shape is known (`tensor_len` for a `tensor_input`
    /// model): a model that compiles for a device but can't run there fails
    /// here. With `allow_fallback`, a model that fails on another device is
    /// loaded on the CPU instead, and the returned warning says so.
    pub fn load(
        config: &ModelConfig,
        files: &BTreeMap<String, PathBuf>,
        defaults: &Execution,
        threads: usize,
        tensor_len: Option<usize>,
    ) -> Result<(Self, Option<String>), String> {
        let path = files.get(&config.model).ok_or_else(|| {
            format!(
                "the model file {:?} isn't among the node's files",
                config.model
            )
        })?;
        let execution = config.execution.clone().unwrap_or_default().over(defaults);
        let runtime = match execution.runtime {
            Some(r) => r,
            None => *Runtime::available()
                .first()
                .ok_or("this build has no runtime")?,
        };
        // The embedder first: its output is this model's input.
        let (embedder, tensor_len) = match config
            .tensor_input
            .as_ref()
            .and_then(|t| t.embedder.as_deref())
        {
            None => (None, tensor_len),
            Some(name) => {
                let base = Options {
                    threads,
                    device: execution.device.unwrap_or_default(),
                    provider: execution.provider.clone(),
                    provider_options: execution.provider_options.clone(),
                    input: None,
                };
                let elsewhere = base.device != Device::Cpu || base.provider.is_some();
                let loaded = match Embedder::load(name, files, runtime, &base) {
                    Err(_) if elsewhere && execution.allow_fallback == Some(true) => {
                        let cpu = Options {
                            device: Device::Cpu,
                            provider: None,
                            provider_options: BTreeMap::new(),
                            ..base.clone()
                        };
                        Embedder::load(name, files, runtime, &cpu)
                    }
                    other => other,
                };
                let (embedder, len) = loaded?;
                (Some(embedder), Some(len))
            }
        };
        let input = match (&config.pre, tensor_len) {
            (Some(p), _) => Some((
                match p.layout {
                    pre::Layout::Nchw => vec![1, 3, p.height as usize, p.width as usize],
                    pre::Layout::Nhwc => vec![1, p.height as usize, p.width as usize, 3],
                },
                p.dtype,
            )),
            (None, Some(n)) => Some((vec![1, n], pre::DType::F32)),
            (None, None) => None,
        };
        let input_shape = input.clone();
        let options = Options {
            threads,
            device: execution.device.unwrap_or_default(),
            provider: execution.provider.clone(),
            provider_options: execution.provider_options.clone(),
            input: input.clone(),
        };
        let open = |options: &Options| -> Result<Box<dyn Session>, String> {
            let mut session = backend::load(runtime, path, options)?;
            if let Some((shape, dtype)) = &input {
                // An input name the model doesn't have is reported below.
                let name = match &config.input_name {
                    Some(n) if session.input_names().contains(n) => n.clone(),
                    Some(_) => return Ok(session),
                    None => session.input_names().first().cloned().unwrap_or_default(),
                };
                let n = shape.iter().product();
                let data = match dtype {
                    pre::DType::F32 => TensorData::F32(vec![0.0; n]),
                    pre::DType::I32 => TensorData::I32(vec![0; n]),
                    pre::DType::U8 => TensorData::U8(vec![0; n]),
                };
                session
                    .run(
                        &name,
                        Tensor {
                            shape: shape.clone(),
                            data,
                        },
                    )
                    .map_err(|e| format!("{}: the first run failed: {e}", config.model))?;
            }
            Ok(session)
        };
        let elsewhere = options.device != Device::Cpu || options.provider.is_some();
        let (session, warning) = match open(&options) {
            Ok(s) => (s, None),
            Err(e) if elsewhere && execution.allow_fallback == Some(true) => {
                let cpu = Options {
                    device: Device::Cpu,
                    provider: None,
                    provider_options: BTreeMap::new(),
                    ..options.clone()
                };
                let session = open(&cpu)?;
                let warning = format!(
                    "{}: couldn't run on {} ({e}); it runs on the CPU instead",
                    config.model,
                    options.device.name()
                );
                (session, Some(warning))
            }
            Err(e) => return Err(e),
        };
        let input = match &config.input_name {
            Some(name) if session.input_names().contains(name) => name.clone(),
            Some(name) => {
                return Err(format!(
                    "{}: the model has no input {name:?} (it has: {})",
                    config.model,
                    session.input_names().join(", ")
                ));
            }
            None => session
                .input_names()
                .first()
                .cloned()
                .ok_or_else(|| format!("{}: the model has no inputs", config.model))?,
        };
        let metadata = |key: &str| session.metadata(key);
        let sources = Sources {
            files,
            metadata: &metadata,
        };
        let labels = labels::labels(config.labels.as_ref(), &sources)
            .map_err(|e| format!("{}: {e}", config.model))?;
        let charset = labels::charset(config.charset.as_ref(), &sources)
            .map_err(|e| format!("{}: {e}", config.model))?;
        let model = Self {
            config: config.clone(),
            session,
            input,
            labels,
            charset,
            min_confidence: config
                .min_confidence
                .or(config.decode.default_min_confidence())
                .unwrap_or(0.0),
            embedder,
            input_shape,
        };
        Ok((model, warning))
    }

    /// This model's input for an object (`tensor_input`), in a frame of
    /// `width` x `height`. `None`: the object lacks what it takes.
    pub fn object_input(
        &mut self,
        t: &TensorInput,
        o: &Object,
        width: u32,
        height: u32,
    ) -> Result<Option<Tensor>, String> {
        let values = match &mut self.embedder {
            Some(embedder) => match hand_input(o, width as f32, height as f32) {
                Some(inputs) => Some(embedder.run(inputs)?),
                None => None,
            },
            None => keypoint_input(t, o),
        };
        Ok(values.map(|v| Tensor {
            shape: vec![1, v.len()],
            data: TensorData::F32(v),
        }))
    }

    pub fn config(&self) -> &ModelConfig {
        &self.config
    }

    pub fn session(&self) -> &dyn Session {
        self.session.as_ref()
    }

    /// The confidence cutoff in effect.
    pub fn min_confidence(&self) -> f32 {
        self.min_confidence
    }

    pub fn labels(&self) -> &[String] {
        &self.labels
    }

    /// Runs on a region of the frame (a model with `pre`).
    pub fn run(&mut self, frame: &Frame, rect: Rect) -> Result<Run, String> {
        let settings = self
            .config
            .pre
            .as_ref()
            .ok_or("this model takes its input from objects, not a frame")?;
        let start = Instant::now();
        let prepared = pre::prepare(frame, rect, settings)?;
        let pre = start.elapsed();
        let (width, height) = (settings.width as f32, settings.height as f32);
        let mut run = self.infer(prepared.tensor, width, height)?;
        run.results.map_to_frame(&prepared.affine);
        run.affine = prepared.affine;
        run.timing.pre = pre;
        Ok(run)
    }

    /// A filled input for one item, or `None` when its shape isn't known.
    fn filled_input(&self, value: f32) -> Option<Tensor> {
        let (shape, dtype) = self.input_shape.as_ref()?;
        let n = shape.iter().product();
        let data = match dtype {
            pre::DType::F32 => TensorData::F32(vec![value; n]),
            pre::DType::I32 => TensorData::I32(vec![(value * 255.0) as i32; n]),
            pre::DType::U8 => TensorData::U8(vec![(value * 255.0) as u8; n]),
        };
        Some(Tensor {
            shape: shape.clone(),
            data,
        })
    }

    /// The time of `runs` inference runs on a zero input, the fastest one
    /// (what a thread count is compared by).
    pub fn timed_runs(&mut self, runs: u32) -> Result<Duration, String> {
        let tensor = self
            .filled_input(0.0)
            .ok_or("the model's input shape isn't known")?;
        let mut best = Duration::MAX;
        for _ in 0..runs {
            let start = Instant::now();
            self.session.run(&self.input, tensor.clone())?;
            best = best.min(start.elapsed());
        }
        Ok(best)
    }

    /// Whether items may share one run: the same two inputs give the same
    /// outputs alone and as a batch of two. The reason when they don't.
    pub fn batch_check(&mut self) -> Result<(), String> {
        let (a, b) = match (self.filled_input(0.0), self.filled_input(0.5)) {
            (Some(a), Some(b)) => (a, b),
            _ => return Err("its input shape isn't known".into()),
        };
        let alone = [
            self.session.run(&self.input, a.clone())?,
            self.session.run(&self.input, b.clone())?,
        ];
        let stacked = stack(&[a, b]).ok_or("its inputs can't be stacked")?;
        let together = self.session.run(&self.input, stacked)?;
        let parts = split(together, 2).ok_or("its outputs have no batch dimension")?;
        for (one, part) in alone.iter().zip(&parts) {
            if !same_outputs(one, part) {
                return Err("a batch gives different outputs than single runs".into());
            }
        }
        Ok(())
    }

    /// Runs several items (each a batch of one) as one batch. `Err`: the run
    /// or the split failed; the caller runs them one by one instead.
    pub fn infer_batch(
        &mut self,
        tensors: &[Tensor],
        width: f32,
        height: f32,
    ) -> Result<Vec<Run>, String> {
        let stacked = stack(tensors).ok_or("the inputs can't be stacked")?;
        let start = Instant::now();
        let outputs = self.session.run(&self.input, stacked)?;
        let infer = start.elapsed() / tensors.len().max(1) as u32;
        let parts = split(outputs, tensors.len()).ok_or("the outputs have no batch dimension")?;
        let cx = Context {
            width,
            height,
            labels: &self.labels,
            charset: &self.charset,
            min_confidence: self.min_confidence,
        };
        parts
            .iter()
            .map(|part| {
                let start = Instant::now();
                let results = self
                    .config
                    .decode
                    .decode(part, &cx)
                    .map_err(|e| format!("{}: {e}", self.config.model))?;
                Ok(Run {
                    results,
                    affine: Affine::IDENTITY,
                    timing: Timing {
                        pre: Duration::ZERO,
                        infer,
                        decode: start.elapsed(),
                    },
                })
            })
            .collect()
    }

    /// Runs on an input tensor built elsewhere; `width` and `height` are what
    /// the decoder takes the input's size to be.
    pub fn infer(&mut self, tensor: Tensor, width: f32, height: f32) -> Result<Run, String> {
        let start = Instant::now();
        let outputs = self
            .session
            .run(&self.input, tensor)
            .map_err(|e| format!("{}: {e}", self.config.model))?;
        let infer = start.elapsed();
        let start = Instant::now();
        let cx = Context {
            width,
            height,
            labels: &self.labels,
            charset: &self.charset,
            min_confidence: self.min_confidence,
        };
        let results = self
            .config
            .decode
            .decode(&outputs, &cx)
            .map_err(|e| format!("{}: {e}", self.config.model))?;
        Ok(Run {
            results,
            affine: Affine::IDENTITY,
            timing: Timing {
                pre: Duration::ZERO,
                infer,
                decode: start.elapsed(),
            },
        })
    }
}

/// Items of one shape (each a batch of one) as one batch.
fn stack(tensors: &[Tensor]) -> Option<Tensor> {
    let first = tensors.first()?;
    if first.shape.first() != Some(&1) || tensors.iter().any(|t| t.shape != first.shape) {
        return None;
    }
    let mut shape = first.shape.clone();
    shape[0] = tensors.len();
    macro_rules! join {
        ($variant:ident) => {{
            let mut all = Vec::with_capacity(first.data.len() * tensors.len());
            for t in tensors {
                let TensorData::$variant(d) = &t.data else {
                    return None;
                };
                all.extend_from_slice(d);
            }
            TensorData::$variant(all)
        }};
    }
    let data = match first.data {
        TensorData::F32(_) => join!(F32),
        TensorData::I32(_) => join!(I32),
        TensorData::U8(_) => join!(U8),
    };
    Some(Tensor { shape, data })
}

/// A batch's outputs as each item's (batch of one). `None` when an output's
/// first dimension isn't the batch.
fn split(outputs: crate::tensor::Outputs, n: usize) -> Option<Vec<crate::tensor::Outputs>> {
    let mut parts = vec![crate::tensor::Outputs::new(); n];
    for (name, output) in outputs {
        let (mut shape, data) = output.into_parts();
        if n == 0 || shape.first() != Some(&n) {
            return None;
        }
        shape[0] = 1;
        let each = data.len() / n;
        if each == 0 {
            return None;
        }
        for (part, values) in parts.iter_mut().zip(data.chunks_exact(each)) {
            let one = crate::tensor::Output::new(shape.clone(), values.to_vec()).ok()?;
            part.insert(name.clone(), one);
        }
    }
    Some(parts)
}

/// The same outputs, within float tolerance.
fn same_outputs(a: &crate::tensor::Outputs, b: &crate::tensor::Outputs) -> bool {
    a.len() == b.len()
        && a.iter().all(|(name, x)| {
            b.get(name).is_some_and(|y| {
                let largest = x.data().iter().fold(0f32, |m, v| m.max(v.abs()));
                let tolerance = 1e-4 + largest * 1e-3;
                x.shape() == y.shape()
                    && x.data()
                        .iter()
                        .zip(y.data())
                        .all(|(p, q)| (p == q) || (p - q).abs() <= tolerance)
            })
        })
}

/// A `tensor_input` model's input from an object's keypoints: `[x, y]` or
/// `[x, y, confidence]` per keypoint used, flat. None when the object lacks
/// a keypoint it needs, or the normalisation has nothing to scale by.
/// Moves the first point to the origin and scales by the larger of the x and
/// y extents, as MediaPipe normalises a hand for its gesture embedder.
fn normalize_hand(points: &mut [f32]) {
    let Some(&[ox, oy, oz]) = points.first_chunk::<3>() else {
        return;
    };
    let (mut x0, mut x1, mut y0, mut y1) = (f32::MAX, f32::MIN, f32::MAX, f32::MIN);
    for p in points.as_chunks_mut::<3>().0 {
        p[0] -= ox;
        p[1] -= oy;
        p[2] -= oz;
        (x0, x1) = (x0.min(p[0]), x1.max(p[0]));
        (y0, y1) = (y0.min(p[1]), y1.max(p[1]));
    }
    let scale = (x1 - x0).max(y1 - y0) + 1e-5;
    points.iter_mut().for_each(|v| *v /= scale);
}

/// A hand's inputs for the gesture embedder: image landmarks, the
/// right-hand score, world landmarks.
fn hand_input(o: &Object, width: f32, height: f32) -> Option<[Vec<f32>; 3]> {
    let (_, pose) = o.children.iter().find(|(id, _)| id == HAND_POSE)?;
    let Results::Tensor(parts) = pose else {
        return None;
    };
    let part = |name: &str| {
        parts
            .iter()
            .find(|(n, _, _)| n == name)
            .map(|(_, _, values)| values)
    };
    let (depth, world, right) = (part("depth")?, part("world")?, part("right")?);
    if o.keypoints.is_empty() || depth.len() != o.keypoints.len() || width <= 0.0 || height <= 0.0 {
        return None;
    }
    let longest = width.max(height);
    let mut hand = Vec::with_capacity(o.keypoints.len() * 3);
    for (k, z) in o.keypoints.iter().zip(depth) {
        // Normalised to the frame, its aspect ratio undone. Depth stays
        // relative to the width, as in MediaPipe.
        hand.push((k.x - width / 2.0) / longest + 0.5);
        hand.push((k.y - height / 2.0) / longest + 0.5);
        hand.push(z / width);
    }
    let mut world = world.clone();
    normalize_hand(&mut hand);
    normalize_hand(&mut world);
    let finite = hand
        .iter()
        .chain(&world)
        .chain(right)
        .all(|v| v.is_finite());
    finite.then(|| [hand, right.clone(), world])
}

pub fn keypoint_input(t: &TensorInput, o: &Object) -> Option<Vec<f32>> {
    let points: Vec<_> = match &t.keypoints {
        Some(idx) => idx
            .iter()
            .map(|i| o.keypoints.get(*i).copied())
            .collect::<Option<_>>()?,
        None => o.keypoints.clone(),
    };
    let first = points.first()?;
    let (ox, oy, sx, sy) = match t.normalize {
        Normalize::None => (0.0, 0.0, 1.0, 1.0),
        Normalize::Box => {
            let (w, h) = (o.bbox[2] - o.bbox[0], o.bbox[3] - o.bbox[1]);
            (o.bbox[0], o.bbox[1], w, h)
        }
        Normalize::Wrist => {
            let far = points
                .iter()
                .map(|k| ((k.x - first.x).powi(2) + (k.y - first.y).powi(2)).sqrt())
                .fold(0.0, f32::max);
            (first.x, first.y, far, far)
        }
    };
    if !(sx > 0.0 && sy > 0.0) {
        return None;
    }
    let mut v = Vec::with_capacity(points.len() * t.dims);
    for k in &points {
        v.push((k.x - ox) / sx);
        v.push((k.y - oy) / sy);
        if t.dims == 3 {
            v.push(k.confidence);
        }
    }
    v.iter().all(|x| x.is_finite()).then_some(v)
}

#[cfg(test)]
mod hand_tests {
    use super::*;
    use crate::results::Keypoint;

    fn hand(rotated: bool) -> Object {
        let k = |x, y| Keypoint {
            x,
            y,
            confidence: 1.0,
        };
        let mut world = vec![1.0, 2.0, 3.0, 2.0, 2.0, 3.0, 1.0, 6.0, 5.0];
        if rotated {
            world.reverse();
        }
        Object {
            tracker_id: None,
            class_id: 0,
            class_name: None,
            confidence: 1.0,
            bbox: [0.0, 0.0, 10.0, 10.0],
            keypoints: vec![k(100.0, 50.0), k(150.0, 50.0), k(100.0, 150.0)],
            children: vec![(
                HAND_POSE.into(),
                Results::Tensor(vec![
                    ("depth".into(), vec![3], vec![20.0, 40.0, 0.0]),
                    ("world".into(), vec![3, 3], world),
                    ("right".into(), vec![1], vec![0.9]),
                ]),
            )],
        }
    }

    #[test]
    fn a_hand_is_normalised_as_mediapipe_does() {
        let [image, right, world] = hand_input(&hand(false), 400.0, 200.0).unwrap();
        // Extent 100 px of the longest side 400; depth is relative to the width.
        let scale = 0.25 + 1e-5;
        let want = [
            0.0,
            0.0,
            0.0,
            0.125 / scale,
            0.0,
            0.05 / scale,
            0.0,
            0.25 / scale,
            -0.05 / scale,
        ];
        for (a, b) in image.iter().zip(want) {
            assert!((a - b).abs() < 1e-5, "{image:?}");
        }
        assert_eq!(right, vec![0.9]);
        // World landmarks: their own origin and extent (y spans 4).
        let w = 4.0 + 1e-5;
        let want = [0.0, 0.0, 0.0, 1.0 / w, 0.0, 0.0, 0.0, 4.0 / w, 2.0 / w];
        for (a, b) in world.iter().zip(want) {
            assert!((a - b).abs() < 1e-5, "{world:?}");
        }
    }

    #[test]
    fn a_hand_without_its_pose_gives_no_input() {
        let mut o = hand(false);
        o.children.clear();
        assert!(hand_input(&o, 400.0, 200.0).is_none());
    }

    #[test]
    fn a_rotated_region_turns_the_world_landmarks_and_scales_depth() {
        let mut r = Results::Objects(vec![hand(false)]);
        // A quarter turn, 2 frame pixels per model pixel.
        let affine = Affine {
            a: 0.0,
            b: -2.0,
            c: 0.0,
            d: 2.0,
            e: 0.0,
            f: 0.0,
        };
        r.map_to_frame(&affine);
        let Results::Objects(o) = r else { panic!() };
        let Results::Tensor(parts) = &o[0].children[0].1 else {
            panic!()
        };
        assert_eq!(parts[0].2[0], 20.0 * 2.0 / 0.4);
        assert_eq!(&parts[1].2[..3], &[-2.0, 1.0, 3.0]);
    }
}
