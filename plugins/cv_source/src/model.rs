//! A model node's engine: its session, labels and charset, and one run on a
//! frame region: pre-processing, inference, decoding, results mapped to
//! frame pixels.

use crate::backend::{self, Device, Options, Runtime, Session};
use crate::decode::Context;
use crate::frame::{Affine, Frame, Rect};
use crate::graph::config::{Execution, ModelConfig, Normalize, TensorInput};
use crate::labels::{self, Sources};
use crate::pre;
use crate::results::{Object, Results};
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
        };
        Ok((model, warning))
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

/// A `tensor_input` model's input from an object's keypoints: `[x, y]` or
/// `[x, y, confidence]` per keypoint used, flat. None when the object lacks
/// a keypoint it needs, or the normalisation has nothing to scale by.
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
