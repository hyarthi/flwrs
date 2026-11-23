//! The debug mode (`flwrs-cv-source debug`): runs a node's graph with no hub
//! and shows what it sees, in a window or on the console, so that what's
//! tuned here is what's deployed.

pub mod canvas;
pub mod compare;
pub mod compose;
pub mod facts;
pub mod font;
pub mod record;
mod run;
#[cfg(feature = "debug-window")]
mod window;

pub use run::{DebugArgs, main};
