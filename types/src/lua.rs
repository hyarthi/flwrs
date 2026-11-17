//! The Lua 5.4 sandbox (feature `lua`): edge mappings in the server, and
//! plugins that run user code (script transforms).
//!
//! **Sandbox.** One Lua state per edge (created when the edge's mapping is
//! compiled, so scenes without Lua never create one). Only `string`, `table`,
//! `math` and `utf8` are loaded; `load`, `loadfile`, `dofile`,
//! `collectgarbage`, `require` and `print` are removed, `json` (encode and
//! decode, see [`crate::json`]) is added, and `io`, `os`,
//! `debug`, `package` and `coroutine` are never loaded. Chunks run in an
//! environment where globals can be read but not assigned (so nothing carries
//! over between events), and the library tables are read-only. `payload` and
//! `meta` are read-only all the way down (proxies with a locked metatable).
//!
//! **Limits per call:** an instruction budget and a wall-clock timeout, both
//! checked by a hook every [`HOOK_EVERY`] instructions, and a memory limit
//! for the state. A call that breaks one is a mapping error.
//!
//! **Values:** strings and bytes are Lua strings; integers are
//! exact (a u64 above `i64::MAX` is an error); datetimes are epoch
//! milliseconds; arrays are 1-based tables; maps and objects are tables;
//! null is `nil`. Results are converted back guided by the target schema, so
//! an empty `{}` becomes an array or a map as the target field says.

use crate::json;
use crate::value::{self, Value};
use flwrs_plugin::schema::payload::{
    FieldDefinition, SchemaDefinition, field_type::Enum as FieldType,
};
use flwrs_plugin::schema::protocol::EventMetadata;
/// The Lua binding, for hosts that install their own functions
/// ([`Runtime::with_host`]).
pub use mlua;
use mlua::{Function, HookTriggers, Lua, LuaOptions, MultiValue, StdLib, Table, VmState};
use pbjson_types::Timestamp;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

pub const DEFAULT_MAX_INSTRUCTIONS: u64 = 1_000_000;
pub const DEFAULT_TIMEOUT_MS: u64 = 50;
pub const DEFAULT_MAX_MEMORY_BYTES: u64 = 16 * 1024 * 1024;
const HOOK_EVERY: u32 = 1_000;
/// Marks our read-only proxies, so results that hand one back can be read.
const PROXY_MARK: &str = "__flwrs_readonly";

#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub max_instructions: u64,
    pub timeout: Duration,
    pub max_memory_bytes: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_instructions: DEFAULT_MAX_INSTRUCTIONS,
            timeout: Duration::from_millis(DEFAULT_TIMEOUT_MS),
            max_memory_bytes: DEFAULT_MAX_MEMORY_BYTES,
        }
    }
}

/// What the hook checks during a call.
struct Budget {
    remaining: AtomicI64,
    deadline: Mutex<Instant>,
}

/// One edge's Lua state and its compiled chunks.
pub struct Runtime {
    lua: Lua,
    limits: Limits,
    budget: Arc<Budget>,
    env: Table,
    readonly_error: Function,
    next: Function,
}

impl std::fmt::Debug for Runtime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("lua::Runtime")
            .field("limits", &self.limits)
            .finish()
    }
}

/// An argument of [`Runtime::call_with`].
pub enum Arg<'a> {
    /// A value the code can read but not change (like `payload`).
    ReadOnly(&'a Value),
    /// A value as an ordinary, writable Lua value (like an accumulator's state).
    Plain(&'a Value),
    /// Event metadata (`event_id`, `trace_id`, …), read-only.
    Meta(Option<&'a EventMetadata>),
    /// Any Lua value.
    Lua(mlua::Value),
}

/// Installs a host's functions and tables into the globals, before they're
/// made read-only.
pub type Install<'a> = &'a dyn Fn(&Lua, &Table) -> mlua::Result<()>;

/// A compiled chunk: `function(<params>) <code> end`.
#[derive(Debug, Clone)]
pub struct Chunk(Function);

impl Runtime {
    /// A sandbox with the standard libraries (`string`, `table`, `math`,
    /// `utf8`, `json`).
    pub fn new(limits: Limits) -> Result<Self, String> {
        Self::with_host(limits, &|_, _| Ok(()))
    }

    /// A sandbox with the host's own globals too (e.g. a plugin's `load` /
    /// `store`, [`time_library`], [`log_library`]). Tables installed this way
    /// are read-only like the standard libraries.
    pub fn with_host(limits: Limits, install: Install) -> Result<Self, String> {
        let lua = Lua::new_with(
            StdLib::STRING | StdLib::TABLE | StdLib::MATH | StdLib::UTF8,
            LuaOptions::default(),
        )
        .map_err(|e| format!("couldn't start Lua: {e}"))?;
        let setup = || -> mlua::Result<(Table, Function, Function)> {
            let globals = lua.globals();
            let next: Function = globals.get("next")?;
            for name in [
                "load",
                "loadfile",
                "dofile",
                "collectgarbage",
                "require",
                "print",
            ] {
                globals.raw_set(name, mlua::Nil)?;
            }
            let readonly_error = lua.create_function(|_, ()| -> mlua::Result<()> {
                Err(mlua::Error::runtime("this table is read-only"))
            })?;
            let no_globals = lua.create_function(|_, ()| -> mlua::Result<()> {
                Err(mlua::Error::runtime(
                    "assigning globals isn't allowed (use local variables)",
                ))
            })?;
            globals.raw_set("json", json_library(&lua)?)?;
            install(&lua, &globals)?;
            // Library tables become read-only views.
            let safe = lua.create_table()?;
            for pair in globals.pairs::<mlua::Value, mlua::Value>() {
                let (key, value) = pair?;
                let value = match value {
                    mlua::Value::Table(t) => {
                        mlua::Value::Table(readonly(&lua, t, &readonly_error, &next)?)
                    }
                    other => other,
                };
                safe.raw_set(key, value)?;
            }
            let env = lua.create_table()?;
            let meta = lua.create_table()?;
            meta.raw_set("__index", safe)?;
            meta.raw_set("__newindex", no_globals)?;
            meta.raw_set("__metatable", false)?;
            env.set_metatable(Some(meta))?;
            Ok((env, readonly_error, next))
        };
        let (env, readonly_error, next) =
            setup().map_err(|e| format!("couldn't set up Lua: {e}"))?;
        lua.set_memory_limit(usize::try_from(limits.max_memory_bytes).unwrap_or(usize::MAX))
            .map_err(|e| format!("couldn't limit Lua's memory: {e}"))?;

        let budget = Arc::new(Budget {
            remaining: AtomicI64::new(0),
            deadline: Mutex::new(Instant::now()),
        });
        let hook_budget = budget.clone();
        let max = limits.max_instructions;
        let timeout = limits.timeout;
        lua.set_hook(
            HookTriggers::new().every_nth_instruction(HOOK_EVERY),
            move |_, _| {
                let left = hook_budget
                    .remaining
                    .fetch_sub(i64::from(HOOK_EVERY), Ordering::Relaxed);
                if left <= i64::from(HOOK_EVERY) {
                    return Err(mlua::Error::runtime(format!(
                        "stopped after {max} instructions (lua.max_instructions)"
                    )));
                }
                if Instant::now()
                    > *hook_budget
                        .deadline
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                {
                    return Err(mlua::Error::runtime(format!(
                        "stopped after {} ms (lua.timeout_ms)",
                        timeout.as_millis()
                    )));
                }
                Ok(VmState::Continue)
            },
        )
        .map_err(|e| format!("couldn't limit Lua: {e}"))?;
        Ok(Self {
            lua,
            limits,
            budget,
            env,
            readonly_error,
            next,
        })
    }

    /// The Lua state (e.g. to make values for [`Arg::Lua`]).
    pub fn lua(&self) -> &Lua {
        &self.lua
    }

    /// Compiles `code` as the body of `function(payload, meta)`.
    pub fn chunk(&self, label: &str, code: &str) -> Result<Chunk, String> {
        self.chunk_with(label, code, &["payload", "meta"])
    }

    /// Compiles `code` as the body of `function(<params>)`.
    pub fn chunk_with(&self, label: &str, code: &str, params: &[&str]) -> Result<Chunk, String> {
        // Syntax first, on the code alone, so errors point at what the user
        // wrote (not at the wrapper's `end`).
        self.lua
            .load(code)
            .set_name(format!("={label}"))
            .into_function()
            .map_err(|e| describe(&e))?;
        let source = format!("return function({}) {code}\nend", params.join(", "));
        self.lua
            .load(source)
            .set_name(format!("={label}"))
            .set_environment(self.env.clone())
            .eval::<Function>()
            .map(Chunk)
            .map_err(|e| describe(&e))
    }

    /// Runs a chunk on one event, within the limits.
    pub fn call(
        &self,
        chunk: &Chunk,
        payload: &Value,
        meta: Option<&EventMetadata>,
    ) -> Result<mlua::Value, String> {
        let results = self.call_with(chunk, vec![Arg::ReadOnly(payload), Arg::Meta(meta)])?;
        Ok(results.into_iter().next().unwrap_or(mlua::Value::Nil))
    }

    /// Runs a chunk with any arguments, within the limits; every result.
    pub fn call_with(&self, chunk: &Chunk, args: Vec<Arg>) -> Result<MultiValue, String> {
        let mut values = MultiValue::new();
        for arg in args {
            let value = match arg {
                Arg::ReadOnly(v) => self.to_lua(v),
                Arg::Plain(v) => plain_lua(&self.lua, v),
                Arg::Meta(m) => self.meta_table(m).map(mlua::Value::Table),
                Arg::Lua(v) => Ok(v),
            };
            values.push_back(value.map_err(|e| describe(&e))?);
        }
        self.budget.remaining.store(
            i64::try_from(self.limits.max_instructions).unwrap_or(i64::MAX),
            Ordering::Relaxed,
        );
        *self
            .budget
            .deadline
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Instant::now() + self.limits.timeout;
        chunk.0.call(values).map_err(|e| match e {
            mlua::Error::MemoryError(_) => format!(
                "used more than {} bytes of memory (lua.max_memory_bytes)",
                self.limits.max_memory_bytes
            ),
            other => describe(&other),
        })
    }

    fn meta_table(&self, meta: Option<&EventMetadata>) -> mlua::Result<Table> {
        let t = self.lua.create_table()?;
        if let Some(m) = meta {
            t.raw_set("event_id", m.event_id.as_str())?;
            t.raw_set("trace_id", m.trace_id.as_str())?;
            t.raw_set("hops", m.hops)?;
            t.raw_set("origin_node_id", m.origin_node_id.as_str())?;
            if let Some(time) = &m.create_time {
                t.raw_set("create_time", epoch_ms(time))?;
            }
        }
        readonly(&self.lua, t, &self.readonly_error, &self.next)
    }

    fn array_to_lua(&self, items: &[Value]) -> mlua::Result<mlua::Value> {
        let t = self.lua.create_table()?;
        for (i, item) in items.iter().enumerate() {
            t.raw_set(i + 1, self.to_lua(item)?)?;
        }
        Ok(mlua::Value::Table(readonly(
            &self.lua,
            t,
            &self.readonly_error,
            &self.next,
        )?))
    }

    /// A read-only Lua view of a value.
    fn to_lua(&self, value: &Value) -> mlua::Result<mlua::Value> {
        Ok(match value {
            Value::Null => mlua::Value::Nil,
            Value::String(s) => mlua::Value::String(self.lua.create_string(s)?),
            Value::Bytes(b) => mlua::Value::String(self.lua.create_string(b)?),
            Value::Bool(b) => mlua::Value::Boolean(*b),
            Value::I32(n) => mlua::Value::Integer(i64::from(*n)),
            Value::I64(n) => mlua::Value::Integer(*n),
            Value::U32(n) => mlua::Value::Integer(i64::from(*n)),
            Value::U64(n) => mlua::Value::Integer(i64::try_from(*n).map_err(|_| {
                mlua::Error::runtime(format!("{n} is above Lua's integer range (u64 > i64::MAX)"))
            })?),
            Value::F32(n) => mlua::Value::Number(f64::from(*n)),
            Value::F64(n) => mlua::Value::Number(*n),
            Value::DateTime(t) => mlua::Value::Integer(epoch_ms(t)),
            Value::Array(items) => self.array_to_lua(items)?,
            // A packed array is a plain Lua sequence of numbers.
            Value::Packed(p) => self.array_to_lua(&p.to_values())?,
            Value::Map(map) => {
                let t = self.lua.create_table()?;
                for (k, v) in map {
                    t.raw_set(k.as_str(), self.to_lua(v)?)?;
                }
                mlua::Value::Table(readonly(&self.lua, t, &self.readonly_error, &self.next)?)
            }
            Value::Object(fields) => {
                let t = self.lua.create_table()?;
                for (k, v) in fields {
                    t.raw_set(k.as_str(), self.to_lua(v)?)?;
                }
                mlua::Value::Table(readonly(&self.lua, t, &self.readonly_error, &self.next)?)
            }
        })
    }
}

/// `json.encode(value) -> string` and `json.decode(string) -> value`: the
/// JSON codec (`crate::json`), on plain Lua values (tables are arrays when
/// they're sequences, else objects; decoded tables are ordinary, writable
/// ones). At most `json::MAX_DEPTH` levels; errors are Lua errors.
fn json_library(lua: &Lua) -> mlua::Result<Table> {
    let library = lua.create_table()?;
    library.raw_set(
        "encode",
        lua.create_function(|_, value: mlua::Value| {
            if too_deep(&value, 0) {
                return Err(mlua::Error::runtime(format!(
                    "json.encode: nested deeper than {} levels (or a table contains itself)",
                    json::MAX_DEPTH
                )));
            }
            let value = from_lua(value, None)
                .map_err(|e| mlua::Error::runtime(format!("json.encode: {e}")))?;
            json::encode(&value).map_err(|e| mlua::Error::runtime(format!("json.encode: {e}")))
        })?,
    )?;
    library.raw_set(
        "decode",
        lua.create_function(|lua, text: mlua::LuaString| {
            let value = json::decode(&text.as_bytes(), None)
                .map_err(|e| mlua::Error::runtime(format!("json.decode: {e}")))?;
            plain_lua(lua, &value)
        })?,
    )?;
    Ok(library)
}

/// Whether a Lua value nests deeper than the JSON limit (which also stops
/// tables that contain themselves).
fn too_deep(value: &mlua::Value, depth: usize) -> bool {
    let mlua::Value::Table(t) = value else {
        return false;
    };
    if depth >= json::MAX_DEPTH {
        return true;
    }
    unproxy(t.clone())
        .pairs::<mlua::Value, mlua::Value>()
        .flatten()
        .any(|(_, v)| too_deep(&v, depth + 1))
}

/// `time.now()`: the current time in epoch milliseconds (UTC).
pub fn time_library(lua: &Lua) -> mlua::Result<Table> {
    let library = lua.create_table()?;
    library.raw_set(
        "now",
        lua.create_function(|_, ()| {
            Ok(std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
                .unwrap_or(0))
        })?,
    )?;
    Ok(library)
}

/// A log level of [`log_library`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Info,
    Warn,
}

/// `log.info(...)` and `log.warn(...)`: the arguments, as text joined by
/// spaces, go to `sink` (the host decides where, and how often).
pub fn log_library(
    lua: &Lua,
    sink: Arc<dyn Fn(LogLevel, String) + Send + Sync>,
) -> mlua::Result<Table> {
    let library = lua.create_table()?;
    for (name, level) in [("info", LogLevel::Info), ("warn", LogLevel::Warn)] {
        let sink = sink.clone();
        library.raw_set(
            name,
            lua.create_function(move |_, args: MultiValue| {
                let text: Vec<String> = args
                    .iter()
                    .map(|v| match v {
                        mlua::Value::String(s) => s.to_string_lossy(),
                        mlua::Value::Table(_) => "table".into(),
                        other => other
                            .to_string()
                            .unwrap_or_else(|_| other.type_name().into()),
                    })
                    .collect();
                sink(level, text.join(" "));
                Ok(())
            })?,
        )?;
    }
    Ok(library)
}

/// A Lua value as a typed value without a schema, keeping what a store needs:
/// integers vs floats, and text vs bytes (a string that isn't UTF-8 is BYTES).
/// Tables are arrays when they're sequences, else objects with string keys.
pub fn from_lua_lossless(value: mlua::Value) -> Result<Value, String> {
    if too_deep(&value, 0) {
        return Err(format!(
            "nested deeper than {} levels (or a table contains itself)",
            json::MAX_DEPTH
        ));
    }
    lossless(value)
}

fn lossless(value: mlua::Value) -> Result<Value, String> {
    Ok(match value {
        mlua::Value::Nil => Value::Null,
        mlua::Value::Boolean(b) => Value::Bool(b),
        mlua::Value::Integer(n) => Value::I64(n),
        mlua::Value::Number(n) => Value::F64(n),
        mlua::Value::String(s) => match s.to_str() {
            Ok(text) => Value::String(text.to_string()),
            Err(_) => Value::Bytes(s.as_bytes().to_vec()),
        },
        mlua::Value::Table(t) => {
            let t = unproxy(t);
            let len = t.raw_len();
            let count = t.pairs::<mlua::Value, mlua::Value>().count();
            if len > 0 && len == count {
                let mut items = vec![];
                for i in 1..=len {
                    items.push(lossless(t.raw_get(i).map_err(|e| describe(&e))?)?);
                }
                Value::Array(items)
            } else {
                let mut fields = vec![];
                for pair in t.pairs::<mlua::Value, mlua::Value>() {
                    let (k, v) = pair.map_err(|e| describe(&e))?;
                    fields.push((string_key(k)?, lossless(v)?));
                }
                fields.sort_by(|a, b| a.0.cmp(&b.0));
                Value::Object(fields)
            }
        }
        other => return Err(format!("a {} can't be stored", other.type_name())),
    })
}

/// A value as ordinary Lua values (writable tables).
pub fn plain_lua(lua: &Lua, value: &Value) -> mlua::Result<mlua::Value> {
    Ok(match value {
        Value::Null => mlua::Value::Nil,
        Value::String(s) => mlua::Value::String(lua.create_string(s)?),
        Value::Bytes(b) => mlua::Value::String(lua.create_string(b)?),
        Value::Bool(b) => mlua::Value::Boolean(*b),
        Value::I32(n) => mlua::Value::Integer(i64::from(*n)),
        Value::I64(n) => mlua::Value::Integer(*n),
        Value::U32(n) => mlua::Value::Integer(i64::from(*n)),
        Value::U64(n) => mlua::Value::Integer(i64::try_from(*n).map_err(|_| {
            mlua::Error::runtime(format!("{n} is above Lua's integer range (u64 > i64::MAX)"))
        })?),
        Value::F32(n) => mlua::Value::Number(f64::from(*n)),
        Value::F64(n) => mlua::Value::Number(*n),
        Value::DateTime(t) => mlua::Value::Integer(epoch_ms(t)),
        Value::Array(items) => {
            let t = lua.create_table()?;
            for (i, item) in items.iter().enumerate() {
                t.raw_set(i + 1, plain_lua(lua, item)?)?;
            }
            mlua::Value::Table(t)
        }
        Value::Packed(p) => plain_lua(lua, &Value::Array(p.to_values()))?,
        Value::Map(map) => {
            let t = lua.create_table()?;
            for (k, v) in map {
                t.raw_set(k.as_str(), plain_lua(lua, v)?)?;
            }
            mlua::Value::Table(t)
        }
        Value::Object(fields) => {
            let t = lua.create_table()?;
            for (k, v) in fields {
                t.raw_set(k.as_str(), plain_lua(lua, v)?)?;
            }
            mlua::Value::Table(t)
        }
    })
}

/// A proxy that reads through to `real` and refuses writes.
fn readonly(lua: &Lua, real: Table, error: &Function, next: &Function) -> mlua::Result<Table> {
    let proxy = lua.create_table()?;
    let meta = lua.create_table()?;
    let len_of = real.clone();
    let pairs_of = real.clone();
    let next = next.clone();
    meta.raw_set("__index", real)?;
    meta.raw_set("__newindex", error.clone())?;
    meta.raw_set(
        "__len",
        lua.create_function(move |_, _: mlua::Value| Ok(len_of.raw_len()))?,
    )?;
    meta.raw_set(
        "__pairs",
        lua.create_function(move |_, _: mlua::Value| {
            Ok((next.clone(), pairs_of.clone(), mlua::Nil))
        })?,
    )?;
    meta.raw_set(PROXY_MARK, true)?;
    // getmetatable() returns this instead, and setmetatable() refuses.
    meta.raw_set("__metatable", false)?;
    proxy.set_metatable(Some(meta))?;
    Ok(proxy)
}

/// The table behind one of our proxies, or the table itself.
fn unproxy(table: Table) -> Table {
    if let Some(meta) = table.metatable()
        && meta.raw_get::<bool>(PROXY_MARK).unwrap_or(false)
        && let Ok(real) = meta.raw_get::<Table>("__index")
    {
        return real;
    }
    table
}

fn epoch_ms(t: &Timestamp) -> i64 {
    t.seconds.saturating_mul(1000) + i64::from(t.nanos) / 1_000_000
}

/// The innermost message of a Lua error (without mlua's wrapping).
/// Checks `code`'s syntax as the body of `function(<params>)`, in a bare
/// Lua state (nothing runs). The error names `label` and the line, as the
/// user wrote it.
pub fn check_syntax(label: &str, code: &str, params: &[String]) -> Result<(), String> {
    let lua = Lua::new_with(StdLib::NONE, LuaOptions::default())
        .map_err(|e| format!("couldn't start Lua: {e}"))?;
    lua.load(code)
        .set_name(format!("={label}"))
        .into_function()
        .map_err(|e| describe(&e))?;
    // `...` is fine in a main chunk but not inside the function.
    let source = format!("return function({}) {code}\nend", params.join(", "));
    lua.load(source)
        .set_name(format!("={label}"))
        .into_function()
        .map(|_| ())
        .map_err(|e| describe(&e))
}

fn describe(err: &mlua::Error) -> String {
    match err {
        mlua::Error::CallbackError { cause, .. } => describe(cause),
        mlua::Error::RuntimeError(msg) | mlua::Error::SyntaxError { message: msg, .. } => {
            msg.lines().next().unwrap_or(msg).to_string()
        }
        other => other
            .to_string()
            .lines()
            .next()
            .unwrap_or_default()
            .to_string(),
    }
}

/// Whether a Lua result counts as true (everything but `nil` and `false`).
pub fn truthy(value: &mlua::Value) -> bool {
    !matches!(value, mlua::Value::Nil | mlua::Value::Boolean(false))
}

/// A Lua result as a typed value, guided by `def` where known.
pub fn from_lua(value: mlua::Value, def: Option<&FieldDefinition>) -> Result<Value, String> {
    let field_type = def.map(|d| d.r#type());
    let converted = match value {
        mlua::Value::Nil => return Ok(Value::Null),
        mlua::Value::Boolean(b) => Value::Bool(b),
        mlua::Value::Integer(n) if field_type == Some(FieldType::Datetime) => {
            Value::DateTime(Timestamp {
                seconds: n.div_euclid(1000),
                nanos: (n.rem_euclid(1000) * 1_000_000) as i32,
            })
        }
        mlua::Value::Integer(n) => Value::I64(n),
        mlua::Value::Number(n) => Value::F64(n),
        mlua::Value::String(s) if field_type == Some(FieldType::Bytes) => {
            Value::Bytes(s.as_bytes().to_vec())
        }
        mlua::Value::String(s) => Value::String(
            s.to_str()
                .map_err(|_| "string isn't UTF-8 (use a BYTES field for binary data)".to_string())?
                .to_string(),
        ),
        mlua::Value::Table(t) => return table_value(unproxy(t), def),
        other => return Err(format!("a {} can't be a value", other.type_name())),
    };
    match def {
        Some(def) => value::coerce(converted, def),
        None => Ok(converted),
    }
}

fn table_value(table: Table, def: Option<&FieldDefinition>) -> Result<Value, String> {
    let err = |e: mlua::Error| describe(&e);
    let nested = def.and_then(|d| d.nested_type_definition.as_deref());
    match def.map(|d| d.r#type()) {
        Some(FieldType::Array) => {
            let mut items = vec![];
            for i in 1..=table.raw_len() {
                let item: mlua::Value = table.raw_get(i).map_err(err)?;
                items.push(from_lua(item, nested).map_err(|e| format!("[{}]: {e}", i - 1))?);
            }
            Ok(Value::Array(items))
        }
        Some(FieldType::Map) => {
            let mut map = BTreeMap::new();
            for pair in table.pairs::<mlua::Value, mlua::Value>() {
                let (k, v) = pair.map_err(err)?;
                let key = string_key(k)?;
                let v = from_lua(v, nested).map_err(|e| format!("{key:?}: {e}"))?;
                map.insert(key, v);
            }
            Ok(Value::Map(map))
        }
        Some(FieldType::Object) => object_value(
            table,
            def.map(|d| d.object_fields.as_slice()).unwrap_or_default(),
        ),
        Some(packed) if value::element_type(packed).is_some() => {
            let mut items = vec![];
            for i in 1..=table.raw_len() {
                let item: mlua::Value = table.raw_get(i).map_err(err)?;
                items.push(from_lua(item, None).map_err(|e| format!("[{}]: {e}", i - 1))?);
            }
            value::Packed::from_values(packed, items).map(Value::Packed)
        }
        Some(other) => Err(format!("a table can't be a {}", other.as_str_name())),
        None => {
            // No schema: a sequence is an array, anything else an object.
            let len = table.raw_len();
            let count = table.pairs::<mlua::Value, mlua::Value>().count();
            if len > 0 && len == count {
                table_value(
                    table,
                    Some(&FieldDefinition {
                        r#type: FieldType::Array as i32,
                        ..Default::default()
                    }),
                )
            } else {
                object_value(table, &[])
                    .map_err(|_| "a table with non-string keys needs an ARRAY or MAP target".into())
            }
        }
    }
}

/// Fields in the schema's order (declared ones only); with no declared
/// fields (no schema), every string key, sorted.
fn object_value(table: Table, defs: &[FieldDefinition]) -> Result<Value, String> {
    let err = |e: mlua::Error| describe(&e);
    if !defs.is_empty() {
        let mut fields = vec![];
        for def in defs {
            let v: mlua::Value = table.raw_get(def.key.as_str()).map_err(err)?;
            if !v.is_nil() {
                let v = from_lua(v, Some(def)).map_err(|e| format!("{}: {e}", def.key))?;
                fields.push((def.key.clone(), v));
            }
        }
        return Ok(Value::Object(fields));
    }
    let mut fields = vec![];
    for pair in table.pairs::<mlua::Value, mlua::Value>() {
        let (k, v) = pair.map_err(err)?;
        let key = string_key(k)?;
        fields.push((key, from_lua(v, None)?));
    }
    fields.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(Value::Object(fields))
}

fn string_key(key: mlua::Value) -> Result<String, String> {
    match key {
        mlua::Value::String(s) => s
            .to_str()
            .map(|s| s.to_string())
            .map_err(|_| "keys must be UTF-8 strings".into()),
        other => Err(format!("keys must be strings, not {}", other.type_name())),
    }
}

/// A whole-payload script result, shaped to the target's schema.
pub fn payload_from_lua(
    value: mlua::Value,
    schema: Option<&SchemaDefinition>,
) -> Result<Value, String> {
    let root = schema.map(|s| FieldDefinition {
        r#type: FieldType::Object as i32,
        object_fields: s.fields.clone(),
        ..Default::default()
    });
    match value {
        mlua::Value::Table(t) => table_value(
            unproxy(t),
            Some(root.as_ref().unwrap_or(&FieldDefinition {
                r#type: FieldType::Object as i32,
                ..Default::default()
            })),
        ),
        other => Err(format!(
            "a script must return a table or nil, not {}",
            other.type_name()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(code: &str, payload: Value) -> Result<Value, String> {
        let runtime = Runtime::new(Limits::default())?;
        let chunk = runtime.chunk("test", code)?;
        let result = runtime.call(&chunk, &payload, None)?;
        from_lua(result, None)
    }

    #[test]
    fn json_encode_and_decode() {
        let payload = Value::Object(vec![
            ("n".into(), Value::I64(3)),
            ("tags".into(), Value::Array(vec![Value::String("a".into())])),
        ]);
        // the read-only payload encodes like any table
        assert_eq!(
            run("return json.encode(payload)", payload.clone()),
            Ok(Value::String(r#"{"n":3,"tags":["a"]}"#.into()))
        );
        // decoded tables are ordinary, writable ones
        assert_eq!(
            run(
                r#"local t = json.decode('{"a": [1, 2], "b": "x"}'); t.c = #t.a; return t"#,
                Value::Null
            ),
            Ok(Value::Object(vec![
                ("a".into(), Value::Array(vec![Value::I64(1), Value::I64(2)])),
                ("b".into(), Value::String("x".into())),
                ("c".into(), Value::I64(2)),
            ]))
        );
        assert_eq!(
            run("return json.encode({})", Value::Null),
            Ok(Value::String("{}".into()))
        );
        // errors are Lua errors (a mapping error for the caller)
        assert!(
            run("return json.decode('{oops')", Value::Null)
                .unwrap_err()
                .contains("json.decode: not JSON")
        );
        let e = run("local t = {}; t.t = t; return json.encode(t)", Value::Null).unwrap_err();
        assert!(e.contains("deeper than"), "{e}");
        // the library is read-only like the others
        assert!(run("json.encode = nil; return 1", Value::Null).is_err());
    }

    #[test]
    fn syntax_is_checked_as_a_function_body() {
        assert!(check_syntax("x", "return payload", &["payload".into()]).is_ok());
        let e = check_syntax("x", "return (", &[]).unwrap_err();
        assert!(e.starts_with("x:1:"), "{e}");
        assert!(check_syntax("x", "return ...", &[]).is_err());
    }

    #[test]
    fn hosts_add_functions_and_call_with_any_arguments() {
        let logged = Arc::new(Mutex::new(vec![]));
        let sink_log = logged.clone();
        let runtime = Runtime::with_host(Limits::default(), &move |lua, globals| {
            globals.raw_set("time", time_library(lua)?)?;
            let sink_log = sink_log.clone();
            globals.raw_set(
                "log",
                log_library(
                    lua,
                    Arc::new(move |level, text| sink_log.lock().unwrap().push((level, text))),
                )?,
            )?;
            globals.raw_set("double", lua.create_function(|_, n: i64| Ok(n * 2))?)?;
            Ok(())
        })
        .unwrap();
        let chunk = runtime
            .chunk_with(
                "step",
                "state.n = state.n + double(payload.x); log.info('n is', state.n, true); return state, time.now() > 0",
                &["state", "payload"],
            )
            .unwrap();
        let state = Value::Object(vec![("n".into(), Value::I64(1))]);
        let payload = Value::Object(vec![("x".into(), Value::I64(5))]);
        let results = runtime
            .call_with(&chunk, vec![Arg::Plain(&state), Arg::ReadOnly(&payload)])
            .unwrap();
        let mut results = results.into_iter();
        // the state was writable; the payload wasn't touched
        assert_eq!(
            from_lua_lossless(results.next().unwrap()).unwrap(),
            Value::Object(vec![("n".into(), Value::I64(11))])
        );
        assert_eq!(results.next(), Some(mlua::Value::Boolean(true)));
        assert_eq!(
            *logged.lock().unwrap(),
            [(LogLevel::Info, "n is 11 true".to_string())]
        );
        // host tables are read-only too
        let chunk = runtime.chunk_with("t", "time.now = nil", &[]).unwrap();
        assert!(runtime.call_with(&chunk, vec![]).is_err());
        let chunk = runtime
            .chunk_with("t", "payload.x = 1", &["payload"])
            .unwrap();
        assert!(
            runtime
                .call_with(&chunk, vec![Arg::ReadOnly(&payload)])
                .is_err()
        );
    }

    #[test]
    fn lossless_keeps_bytes_and_numbers_apart() {
        let lua = Lua::new();
        let value: mlua::Value = lua
            .load(r#"return { i = 1, f = 1.0, text = "hé", raw = "\xff\x00", list = {1, 2} }"#)
            .eval()
            .unwrap();
        assert_eq!(
            from_lua_lossless(value).unwrap(),
            Value::Object(vec![
                ("f".into(), Value::F64(1.0)),
                ("i".into(), Value::I64(1)),
                (
                    "list".into(),
                    Value::Array(vec![Value::I64(1), Value::I64(2)])
                ),
                ("raw".into(), Value::Bytes(vec![0xff, 0])),
                ("text".into(), Value::String("hé".into())),
            ])
        );
        let cyclic: mlua::Value = lua.load("local t = {}; t.t = t; return t").eval().unwrap();
        assert!(from_lua_lossless(cyclic).is_err());
    }
}
