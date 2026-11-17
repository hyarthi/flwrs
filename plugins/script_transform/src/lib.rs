//! The script transform: a Lua script per event, with keyed state kept
//! exactly once. The state store and the Lua host are shared with the
//! accumulator.

pub mod config;
pub mod engine;
pub mod host;
pub mod plugin;
pub mod store;
