//! tract sessions: the model compiled for its one input's shape (symbolic
//! dimensions made concrete throughout the graph), on the CPU, one thread.

use super::{Options, Runtime, Session};
use crate::pre::DType;
use crate::tensor::{Output, Outputs, Tensor, TensorData};
use std::collections::HashMap;
use std::path::Path;
use tract_onnx::prelude::*;
use tract_onnx::tract_hir::infer::{GenericFactoid, InferenceModelExt};

pub struct TractModel {
    plan: std::sync::Arc<TypedSimplePlan>,
    inputs: Vec<String>,
    outputs: Vec<String>,
    metadata: Vec<(String, String)>,
}

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

pub fn load(path: &Path, options: &Options) -> Result<TractModel, String> {
    if options.device != super::Device::Cpu || options.provider.is_some() {
        return Err(format!(
            "tract runs on the CPU only (asked for {}{})",
            options.device.name(),
            options
                .provider
                .as_ref()
                .map(|p| format!(" with {p}"))
                .unwrap_or_default()
        ));
    }
    let (shape, dtype) = options
        .input
        .clone()
        .ok_or("tract needs the input's shape to compile the model")?;
    let mut model = tract_onnx::onnx().model_for_path(path).map_err(err)?;
    let inputs: Vec<String> = model
        .input_outlets()
        .map_err(err)?
        .iter()
        .map(|o| model.node(o.node).name.clone())
        .collect();
    if inputs.len() != 1 {
        return Err(format!(
            "the model has {} inputs ({}); tract runs models with one",
            inputs.len(),
            inputs.join(", ")
        ));
    }
    let outputs: Vec<String> = model
        .output_outlets()
        .map_err(err)?
        .iter()
        .map(|o| {
            model
                .outlet_label(*o)
                .map(String::from)
                .unwrap_or_else(|| model.node(o.node).name.clone())
        })
        .collect();
    let metadata = model
        .properties
        .iter()
        .filter_map(|(k, v)| {
            let key = k.strip_prefix("onnx.metadata_props.")?;
            let value = v
                .to_plain_array_view::<String>()
                .ok()?
                .iter()
                .next()?
                .clone();
            Some((key.to_string(), value))
        })
        .collect();
    // Symbolic dimensions of the input (`batch_size`, `height`) are
    // replaced everywhere they occur, so shapes computed from them inside
    // the graph agree with the concrete input.
    let declared = model.input_fact(0).map_err(err)?.clone();
    let symbols: HashMap<Symbol, TDim> = declared
        .shape
        .dims()
        .zip(&shape)
        .filter_map(|(d, v)| match d {
            GenericFactoid::Only(TDim::Sym(s)) => Some((s.clone(), TDim::Val(*v as i64))),
            _ => None,
        })
        .collect();
    if !symbols.is_empty() {
        model.set_symbols(&symbols).map_err(err)?;
    }
    let fact = match dtype {
        DType::F32 => f32::fact(&shape),
        DType::I32 => i32::fact(&shape),
        DType::U8 => u8::fact(&shape),
    };
    model = model.with_input_fact(0, fact.into()).map_err(err)?;
    let plan = model
        .into_optimized()
        .map_err(err)?
        .into_runnable()
        .map_err(err)?;
    Ok(TractModel {
        plan,
        inputs,
        outputs,
        metadata,
    })
}

impl Session for TractModel {
    fn input_names(&self) -> &[String] {
        &self.inputs
    }

    fn run(&mut self, input: &str, tensor: Tensor) -> Result<Outputs, String> {
        if input != self.inputs[0] {
            return Err(format!(
                "the model has no input {input:?} (it has {:?})",
                self.inputs[0]
            ));
        }
        let input = match &tensor.data {
            TensorData::F32(d) => tract_onnx::prelude::Tensor::from_shape(&tensor.shape, d),
            TensorData::I32(d) => tract_onnx::prelude::Tensor::from_shape(&tensor.shape, d),
            TensorData::U8(d) => tract_onnx::prelude::Tensor::from_shape(&tensor.shape, d),
        }
        .map_err(err)?;
        let results = self.plan.run(tvec!(input.into())).map_err(err)?;
        let mut outputs = Outputs::new();
        for (name, value) in self.outputs.iter().zip(results.iter()) {
            let as_f32 = value
                .cast_to::<f32>()
                .map_err(|e| format!("output {name:?}: {e}"))?;
            let data: Vec<f32> = as_f32
                .to_plain_array_view::<f32>()
                .map_err(err)?
                .iter()
                .copied()
                .collect();
            outputs.insert(name.clone(), Output::new(value.shape().to_vec(), data)?);
        }
        Ok(outputs)
    }

    fn metadata(&self, key: &str) -> Option<String> {
        self.metadata
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
    }

    fn device(&self) -> &str {
        "cpu"
    }

    fn runtime(&self) -> Runtime {
        Runtime::Tract
    }
}
