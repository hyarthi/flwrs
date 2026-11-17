//! The HTTP source: an HTTP server that turns requests into events, routed
//! and validated by an OpenAPI 3.1 document. The library is shared by the
//! plugin and its configurator (which derives the node's out schema).

pub mod config;
pub mod convert;
pub mod plugin;
pub mod server;
pub mod spec;
