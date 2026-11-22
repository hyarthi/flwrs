//! Inference runtimes behind one trait: ONNX Runtime (`ort` feature) and
//! tract (`tract` feature). A session is one loaded model; it runs on the
//! device it was set up for, never one a runtime picks by its own policy.

use crate::pre::DType;
use crate::tensor::{Outputs, Tensor};
use std::path::Path;

#[cfg(feature = "ort")]
mod onnxruntime;
#[cfg(test)]
mod tests;
#[cfg(feature = "tract")]
mod tract;

/// A loaded model.
pub trait Session: Send {
    /// The model's input tensor names, in order.
    fn input_names(&self) -> &[String];
    /// Runs the model on one input; every output, as `f32`.
    fn run(&mut self, input: &str, tensor: Tensor) -> Result<Outputs, String>;
    /// A value of the model's metadata (ONNX `metadata_props`).
    fn metadata(&self, key: &str) -> Option<String>;
    /// Where it runs: the device, and the provider and vendor where the
    /// runtime says (`cpu`, `gpu (CoreMLExecutionProvider, Apple)`).
    fn device(&self) -> &str;
    /// The runtime's name.
    fn runtime(&self) -> Runtime;
}

/// An inference runtime.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Runtime {
    /// ONNX Runtime.
    Ort,
    /// tract: pure Rust, for devices ONNX Runtime doesn't support.
    Tract,
}

impl Runtime {
    /// The runtimes this build has, the default first.
    pub fn available() -> &'static [Runtime] {
        &[
            #[cfg(feature = "ort")]
            Runtime::Ort,
            #[cfg(feature = "tract")]
            Runtime::Tract,
        ]
    }

    pub fn name(self) -> &'static str {
        match self {
            Runtime::Ort => "ort",
            Runtime::Tract => "tract",
        }
    }
}

/// A kind of device a model runs on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Device {
    #[default]
    Cpu,
    Gpu,
    /// A neural engine (Apple's, a vendor NPU).
    Npu,
}

impl Device {
    pub fn name(self) -> &'static str {
        match self {
            Device::Cpu => "cpu",
            Device::Gpu => "gpu",
            Device::Npu => "npu",
        }
    }
}

/// How a session is set up.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Options {
    /// Threads for one run (0: the runtime's default). tract runs on one.
    pub threads: usize,
    pub device: Device,
    /// The execution provider (ONNX Runtime), short or full name.
    pub provider: Option<String>,
    pub provider_options: std::collections::BTreeMap<String, String>,
    /// The input it will be given: shape and type. tract compiles the model
    /// for it (dynamic dimensions made concrete); ONNX Runtime checks it.
    pub input: Option<(Vec<usize>, DType)>,
}

/// Loads a model with a runtime this build has.
pub fn load(runtime: Runtime, path: &Path, options: &Options) -> Result<Box<dyn Session>, String> {
    let label = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let session: Result<Box<dyn Session>, String> = match runtime {
        #[cfg(feature = "ort")]
        Runtime::Ort => onnxruntime::load(path, options).map(|s| Box::new(s) as Box<dyn Session>),
        #[cfg(feature = "tract")]
        Runtime::Tract => tract::load(path, options).map(|s| Box::new(s) as Box<dyn Session>),
        #[allow(unreachable_patterns)]
        other => Err(format!("this build has no {} runtime", other.name())),
    };
    session.map_err(|e| format!("{label} ({}): {e}", runtime.name()))
}

/// Shapes for runtimes that take signed dimensions.
#[cfg_attr(not(feature = "ort"), allow(dead_code))]
fn signed(shape: &[usize]) -> Vec<i64> {
    shape.iter().map(|d| *d as i64).collect()
}
