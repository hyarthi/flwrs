//! The WebSocket source: a WebSocket server that turns messages into events,
//! typed by an AsyncAPI 3.x document. The library is shared by the plugin and
//! its configurator (which derives the node's out schema).

pub mod config;
pub mod convert;
pub mod plugin;
pub mod server;
pub mod spec;
