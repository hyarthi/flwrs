//! ONNX Runtime sessions, on the CPU execution provider (set explicitly:
//! the device is never left to ONNX Runtime's own policy).

use super::{Device, Options, Runtime, Session, signed};
use crate::tensor::{Output, Outputs, Tensor, TensorData};
use ort::memory::DeviceType;
use ort::session::Session as OrtSession;
use ort::value::{TensorElementType, ValueType};
use std::path::Path;
use std::sync::{Arc, OnceLock};

/// The process's one ONNX Runtime environment (it may be built only once).
fn environment() -> Result<&'static ort::environment::Environment, String> {
    static ENV: OnceLock<Result<ort::environment::Environment, String>> = OnceLock::new();
    ENV.get_or_init(|| {
        let logger: ort::logging::LoggerFunction =
            Arc::new(|level, category, _id, _location, message| {
                use ort::logging::LogLevel as L;
                let level = match level {
                    L::Verbose => log::Level::Trace,
                    L::Info => log::Level::Debug,
                    L::Warning => log::Level::Warn,
                    L::Error | L::Fatal => log::Level::Error,
                };
                log::log!(target: "onnxruntime", level, "{category}: {message}");
            });
        ort::environment::init()
            .with_name("flwrs-cv-source")
            .with_telemetry(false)
            .with_logger(logger)
            .build()
            .map_err(|e| format!("ONNX Runtime didn't start: {e}"))
    })
    .as_ref()
    .map_err(Clone::clone)
}

pub struct OrtModel {
    session: OrtSession,
    inputs: Vec<String>,
    metadata: Vec<(String, String)>,
    device: String,
}

/// A provider's full name from its short one (`coreml`); full names pass.
fn provider_name(short: &str) -> String {
    match short.to_ascii_lowercase().as_str() {
        "cpu" => "CPUExecutionProvider",
        "coreml" => "CoreMLExecutionProvider",
        "cuda" => "CUDAExecutionProvider",
        "tensorrt" => "TensorrtExecutionProvider",
        "directml" | "dml" => "DmlExecutionProvider",
        "openvino" => "OpenVINOExecutionProvider",
        "webgpu" => "WebGpuExecutionProvider",
        "xnnpack" => "XnnpackExecutionProvider",
        "qnn" => "QNNExecutionProvider",
        _ => return short.to_string(),
    }
    .to_string()
}

/// `gpu (CoreMLExecutionProvider, Apple)`.
fn describe(d: &ort::device::Device) -> String {
    let device_type = match d.hardware_device().ty() {
        DeviceType::CPU => "cpu",
        DeviceType::GPU => "gpu",
        DeviceType::NPU => "npu",
        DeviceType::FPGA => "fpga",
    };
    let vendor = d.hardware_device().vendor().unwrap_or_default();
    format!("{device_type} ({}, {vendor})", d.ep().unwrap_or_default())
}

pub fn load(path: &Path, options: &Options) -> Result<OrtModel, String> {
    let env = environment()?;
    // The device, chosen from the environment's devices. A session that
    // names none gets ONNX Runtime's default partitioning, which on macOS
    // puts parts of the graph on CoreML, whose float16 handling changed
    // results (a YOLO26 export lost its best detection). Appending the CPU
    // execution provider doesn't prevent that; selecting the device does.
    // Nodes a non-CPU provider can't take still run on the CPU.
    let provider = options.provider.as_deref().map(provider_name);
    let device_type = match options.device {
        Device::Cpu => DeviceType::CPU,
        Device::Gpu => DeviceType::GPU,
        Device::Npu => DeviceType::NPU,
    };
    let fits = |d: &ort::device::Device| {
        let ep = d.ep().unwrap_or_default();
        d.hardware_device().ty() == device_type
            && match &provider {
                Some(p) => ep == p,
                // On the CPU, ONNX Runtime's own provider unless one is named.
                None => options.device != Device::Cpu || ep == "CPUExecutionProvider",
            }
    };
    let Some(device) = env.devices().find(fits) else {
        let offered: Vec<String> = env.devices().map(|d| describe(&d)).collect();
        return Err(format!(
            "no {}{} device here (ONNX Runtime offers: {})",
            options.device.name(),
            provider.map(|p| format!(" {p}")).unwrap_or_default(),
            offered.join(", ")
        ));
    };
    let described = describe(&device);
    let ep = device.ep().unwrap_or_default().to_string();
    let provider_options: Vec<(String, String)> = options
        .provider_options
        .iter()
        .map(|(k, v)| (format!("{ep}.{k}"), v.clone()))
        .collect();
    let mut builder = OrtSession::builder(env)
        .map_err(|e| e.to_string())?
        .with_no_environment_execution_providers()
        .map_err(|e| e.to_string())?
        .with_devices([device], Some(&provider_options))
        .map_err(|e| e.to_string())?;
    if options.threads > 0 {
        builder = builder
            .with_intra_threads(options.threads)
            .map_err(|e| e.to_string())?;
    }
    let session = builder.commit_from_file(path).map_err(|e| e.to_string())?;
    let inputs = session
        .inputs()
        .iter()
        .map(|i| i.name().to_string())
        .collect();
    let metadata = match session.metadata() {
        Ok(m) => m
            .custom_keys()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|k| m.custom(&k).map(|v| (k, v)))
            .collect(),
        Err(_) => vec![],
    };
    Ok(OrtModel {
        session,
        inputs,
        metadata,
        device: described,
    })
}

impl Session for OrtModel {
    fn input_names(&self) -> &[String] {
        &self.inputs
    }

    fn run(&mut self, input: &str, tensor: Tensor) -> Result<Outputs, String> {
        self.run_many(vec![(input.to_string(), tensor)])
    }

    fn run_many(&mut self, inputs: Vec<(String, Tensor)>) -> Result<Outputs, String> {
        let mut values = Vec::with_capacity(inputs.len());
        for (name, tensor) in inputs {
            let shape = signed(&tensor.shape);
            let value: ort::session::SessionInputValue = match tensor.data {
                TensorData::F32(d) => {
                    ort::value::Tensor::from_array((shape, d)).map(|t| t.into_dyn().into())
                }
                TensorData::I32(d) => {
                    ort::value::Tensor::from_array((shape, d)).map(|t| t.into_dyn().into())
                }
                TensorData::U8(d) => {
                    ort::value::Tensor::from_array((shape, d)).map(|t| t.into_dyn().into())
                }
            }
            .map_err(|e| e.to_string())?;
            values.push((std::borrow::Cow::<str>::Owned(name), value));
        }
        let outputs = self.session.run(values).map_err(|e| e.to_string())?;
        let mut result = Outputs::new();
        for (name, value) in outputs.iter() {
            let ty = match value.dtype() {
                ValueType::Tensor { ty, .. } => *ty,
                other => return Err(format!("output {name:?} is a {other}, not a tensor")),
            };
            macro_rules! extract {
                ($t:ty) => {{
                    let (shape, data) = value
                        .try_extract_tensor::<$t>()
                        .map_err(|e| e.to_string())?;
                    (
                        shape.to_vec(),
                        data.iter().map(|v| *v as f32).collect::<Vec<f32>>(),
                    )
                }};
            }
            let (shape, data) = match ty {
                TensorElementType::Float32 => extract!(f32),
                TensorElementType::Float64 => extract!(f64),
                TensorElementType::Uint8 => extract!(u8),
                TensorElementType::Int8 => extract!(i8),
                TensorElementType::Uint16 => extract!(u16),
                TensorElementType::Int16 => extract!(i16),
                TensorElementType::Int32 => extract!(i32),
                TensorElementType::Uint32 => extract!(u32),
                TensorElementType::Int64 => extract!(i64),
                TensorElementType::Uint64 => extract!(u64),
                TensorElementType::Bool => {
                    let (shape, data) = value
                        .try_extract_tensor::<bool>()
                        .map_err(|e| e.to_string())?;
                    (
                        shape.to_vec(),
                        data.iter().map(|v| f32::from(u8::from(*v))).collect(),
                    )
                }
                other => {
                    return Err(format!(
                        "output {name:?} has elements of type {other}, which this plugin can't read"
                    ));
                }
            };
            let shape = shape.iter().map(|d| (*d).max(0) as usize).collect();
            result.insert(name.to_string(), Output::new(shape, data)?);
        }
        Ok(result)
    }

    fn metadata(&self, key: &str) -> Option<String> {
        self.metadata
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
    }

    fn device(&self) -> &str {
        &self.device
    }

    fn runtime(&self) -> Runtime {
        Runtime::Ort
    }
}
