//! Shared code for flwrs' own plugins (unpublished): the HTTP server core
//! ([`server`]) used by the HTTP and WebSocket sources and push sinks, and
//! JSON Schema inside API documents ([`schema`]) for the sources.

pub mod schema;
pub mod server;
