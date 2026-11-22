//! The vision source's engine: frames and pre-processing, inference
//! runtimes, decoders, labels and keypoint layouts, the model graph and its
//! settings. No flwrs protocol code: the plugin (`main.rs`) builds on it,
//! and a transform-style ML plugin could too.

#[cfg(not(any(feature = "ort", feature = "tract")))]
compile_error!("flwrs-cv-source needs an inference runtime: enable the `ort` or `tract` feature");

pub mod backend;
pub mod decode;
pub mod frame;
pub mod graph;
pub mod keypoints;
pub mod labels;
pub mod model;
pub mod pre;
pub mod results;
pub mod tensor;
#[cfg(test)]
pub(crate) mod testing;
