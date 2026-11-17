//! `flwrs-types`: types and type definitions the server and plugins share, so
//! they behave the same (e.g. edge mappings in the server and script
//! transforms in plugins). It's the place for any further shared types.
//!
//! - [`value`]: the typed value tree (mirrors the payload model: bytes,
//!   datetimes and u64 vs i64 survive), its conversion to and from
//!   `PluginPayload`, constants from JSON, and lossless shaping to a schema.
//! - [`json`]: the JSON codec (values to JSON text and back).
//! - [`path`]: dot paths (`a.b`, `a[0]`, `a["k"]`) into values and schemas.
//! - [`lua`] (feature `lua`): the Lua 5.4 sandbox, its limits, and
//!   schema-guided conversion between Lua and values.

pub mod json;
#[cfg(feature = "lua")]
pub mod lua;
pub mod path;
pub mod value;
