//! The HTTP push sink: an HTTP server that broadcasts events to subscribers
//! over Server-Sent Events and long polling, on the kit's broadcast core.

pub mod config;
pub mod plugin;
pub mod server;
