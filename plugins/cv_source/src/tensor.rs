//! Tensors in and out of a model. Inputs keep their element type; outputs
//! are converted to `f32` whatever the model's type (integers, quantised
//! `u8` / `i8`, `f16`, `f64`), so decoders see one kind of tensor.

use std::collections::BTreeMap;

/// A model input's values.
#[derive(Clone, Debug, PartialEq)]
pub enum TensorData {
    F32(Vec<f32>),
    I32(Vec<i32>),
    U8(Vec<u8>),
}

impl TensorData {
    pub fn len(&self) -> usize {
        match self {
            TensorData::F32(v) => v.len(),
            TensorData::I32(v) => v.len(),
            TensorData::U8(v) => v.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// A model input.
#[derive(Clone, Debug, PartialEq)]
pub struct Tensor {
    pub shape: Vec<usize>,
    pub data: TensorData,
}

/// A model output, as `f32`, row-major. Its data always fills its shape.
#[derive(Clone, Debug, PartialEq)]
pub struct Output {
    shape: Vec<usize>,
    data: Vec<f32>,
}

impl Output {
    pub fn new(shape: Vec<usize>, data: Vec<f32>) -> Result<Self, String> {
        let expected = shape.iter().try_fold(1usize, |n, d| n.checked_mul(*d));
        if expected != Some(data.len()) {
            return Err(format!(
                "a tensor of shape {shape:?} can't hold {} values",
                data.len()
            ));
        }
        Ok(Self { shape, data })
    }

    pub fn shape(&self) -> &[usize] {
        &self.shape
    }

    pub fn data(&self) -> &[f32] {
        &self.data
    }

    pub fn into_parts(self) -> (Vec<usize>, Vec<f32>) {
        (self.shape, self.data)
    }
}

/// A run's outputs, by the model's tensor names.
pub type Outputs = BTreeMap<String, Output>;
