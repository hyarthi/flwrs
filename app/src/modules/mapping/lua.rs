//! The Lua 5.4 tier of edge mappings, behind the `mapping-lua` feature.
//!
//! **Sandbox.** One Lua state per edge (created when the edge's mapping is
//! compiled, so scenes without Lua never create one). Only `string`, `table`,
//! `math` and `utf8` are loaded; `load`, `loadfile`, `dofile`,
//! `collectgarbage`, `require` and `print` are removed, and `io`, `os`,
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

use super::value::{self, Value};
use flwrs_plugin::schema::common::EventMetadata;
use flwrs_plugin::schema::schema::{
    FieldDefinition, SchemaDefinition, field_type::Enum as FieldType,
};
use mlua::{Function, HookTriggers, Lua, LuaOptions, StdLib, Table, VmState};
use pbjson_types::Timestamp;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

pub(crate) const DEFAULT_MAX_INSTRUCTIONS: u64 = 1_000_000;
pub(crate) const DEFAULT_TIMEOUT_MS: u64 = 50;
pub(crate) const DEFAULT_MAX_MEMORY_BYTES: u64 = 16 * 1024 * 1024;
const HOOK_EVERY: u32 = 1_000;
/// Marks our read-only proxies, so results that hand one back can be read.
const PROXY_MARK: &str = "__flwrs_readonly";

#[derive(Debug, Clone, Copy)]
pub(crate) struct Limits {
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
pub(crate) struct Runtime {
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

/// A compiled chunk: `function(payload, meta) <code> end`.
#[derive(Debug, Clone)]
pub(crate) struct Chunk(Function);

impl Runtime {
    pub(crate) fn new(limits: Limits) -> Result<Self, String> {
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

    /// Compiles `code` as the body of `function(payload, meta)`.
    pub(crate) fn chunk(&self, label: &str, code: &str) -> Result<Chunk, String> {
        // Syntax first, on the code alone, so errors point at what the user
        // wrote (not at the wrapper's `end`).
        self.lua
            .load(code)
            .set_name(format!("={label}"))
            .into_function()
            .map_err(|e| describe(&e))?;
        let source = format!("return function(payload, meta) {code}\nend");
        self.lua
            .load(source)
            .set_name(format!("={label}"))
            .set_environment(self.env.clone())
            .eval::<Function>()
            .map(Chunk)
            .map_err(|e| describe(&e))
    }

    /// Runs a chunk on one event, within the limits.
    pub(crate) fn call(
        &self,
        chunk: &Chunk,
        payload: &Value,
        meta: Option<&EventMetadata>,
    ) -> Result<mlua::Value, String> {
        let payload = self.to_lua(payload).map_err(|e| describe(&e))?;
        let meta = self.meta_table(meta).map_err(|e| describe(&e))?;
        self.budget.remaining.store(
            i64::try_from(self.limits.max_instructions).unwrap_or(i64::MAX),
            Ordering::Relaxed,
        );
        *self
            .budget
            .deadline
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Instant::now() + self.limits.timeout;
        chunk.0.call((payload, meta)).map_err(|e| match e {
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
            Value::Array(items) => {
                let t = self.lua.create_table()?;
                for (i, item) in items.iter().enumerate() {
                    t.raw_set(i + 1, self.to_lua(item)?)?;
                }
                mlua::Value::Table(readonly(&self.lua, t, &self.readonly_error, &self.next)?)
            }
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
pub(crate) fn truthy(value: &mlua::Value) -> bool {
    !matches!(value, mlua::Value::Nil | mlua::Value::Boolean(false))
}

/// A Lua result as a typed value, guided by `def` where known.
pub(crate) fn from_lua(value: mlua::Value, def: Option<&FieldDefinition>) -> Result<Value, String> {
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
pub(crate) fn payload_from_lua(
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
    use super::super::compile::{Schemas, compile};
    use super::super::{MappingSpec, path::Path};
    use super::*;
    use flwrs_plugin::schema::rules::parse_json;
    use flwrs_plugin::schema::schema::PluginPayload;
    use serde_json::json;

    fn spec(json: serde_json::Value) -> MappingSpec {
        MappingSpec::parse(&json).unwrap().unwrap()
    }

    fn target() -> SchemaDefinition {
        parse_json(
            r#"{"fields": [
                {"key": "text", "type": "STRING"},
                {"key": "n", "type": "I64"},
                {"key": "big", "type": "U64"},
                {"key": "when", "type": "DATETIME"},
                {"key": "raw", "type": "BYTES"},
                {"key": "list", "type": "ARRAY", "nested_type_definition": {"type": "I32"}},
                {"key": "tags", "type": "MAP", "nested_type_definition": {"type": "STRING"}}
            ]}"#,
        )
        .unwrap()
    }

    fn payload(fields: Vec<(&str, Value)>) -> PluginPayload {
        Value::Object(
            fields
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect(),
        )
        .into_payload()
        .unwrap()
    }

    fn input() -> PluginPayload {
        payload(vec![
            ("text", Value::String("hello".into())),
            ("n", Value::I64(41)),
            (
                "when",
                Value::DateTime(Timestamp {
                    seconds: 1_700_000_000,
                    nanos: 123_000_000,
                }),
            ),
            ("list", Value::Array(vec![Value::I32(1), Value::I32(2)])),
        ])
    }

    /// Runs one Lua rule targeting `target` and returns the result there.
    fn rule(code: &str, target_field: &str) -> Result<Option<Value>, String> {
        let t = target();
        let mapping = compile(
            &spec(json!({"rules": [{"target": target_field, "lua": code}]})),
            Schemas {
                source: None,
                target: Some(&t),
            },
        )
        .map_err(|e| e.join("; "))?;
        let out = mapping.apply(&input(), None)?.expect("not filtered");
        Ok(Path::parse(target_field)
            .unwrap()
            .get(&Value::from_payload(&out))
            .cloned())
    }

    #[test]
    fn rules_see_the_payload_and_set_typed_values() {
        assert_eq!(
            rule("return payload.text:upper()", "text"),
            Ok(Some(Value::String("HELLO".into())))
        );
        assert_eq!(rule("return payload.n + 1", "n"), Ok(Some(Value::I64(42))));
        assert_eq!(rule("return #payload.list", "n"), Ok(Some(Value::I64(2))));
        assert_eq!(
            rule(
                "local s = 0 for _, v in ipairs(payload.list) do s = s + v end return s",
                "n"
            ),
            Ok(Some(Value::I64(3)))
        );
        // nil removes the field, like `delete`.
        assert_eq!(rule("return nil", "text"), Ok(None));
    }

    #[test]
    fn values_follow_the_target_schema() {
        // Datetimes are epoch milliseconds, both ways.
        assert_eq!(
            rule("return payload.when", "when"),
            Ok(Some(Value::DateTime(Timestamp {
                seconds: 1_700_000_000,
                nanos: 123_000_000
            })))
        );
        assert_eq!(
            rule("return payload.when // 1000", "n"),
            Ok(Some(Value::I64(1_700_000_000)))
        );
        // An empty table is whatever the target field is.
        assert_eq!(rule("return {}", "list"), Ok(Some(Value::Array(vec![]))));
        assert_eq!(
            rule("return {}", "tags"),
            Ok(Some(Value::Map(BTreeMap::new())))
        );
        assert_eq!(
            rule("return {a = 'x'}", "tags"),
            Ok(Some(Value::Map(BTreeMap::from([(
                "a".into(),
                Value::String("x".into())
            )]))))
        );
        // Lua strings are bytes for a BYTES field; not-UTF-8 isn't a STRING.
        assert_eq!(
            rule("return '\\xff\\x00'", "raw"),
            Ok(Some(Value::Bytes(vec![0xff, 0])))
        );
        assert!(
            rule("return '\\xff'", "text")
                .unwrap_err()
                .contains("isn't UTF-8")
        );
        assert!(
            rule("return {1, 2}", "text")
                .unwrap_err()
                .contains("can't be a STRING")
        );
        assert!(rule("return 1.5", "n").is_err(), "not a whole number");
    }

    #[test]
    fn u64_above_lua_integers_is_an_error() {
        let mapping = compile(
            &spec(json!({"rules": [{"target": "big", "lua": "return payload.big"}]})),
            Schemas::default(),
        )
        .unwrap();
        let input = payload(vec![("big", Value::U64(u64::MAX))]);
        assert!(
            mapping
                .apply(&input, None)
                .unwrap_err()
                .contains("above Lua's integer range")
        );
        let fits = payload(vec![("big", Value::U64(i64::MAX as u64))]);
        assert!(mapping.apply(&fits, None).is_ok());
    }

    #[test]
    fn script_and_filter() {
        let t = target();
        let mapping = compile(
            &spec(json!({
                "filter": {"lua": "return payload.n > 40"},
                "script": {"lua": "if payload.text == 'skip' then return nil end\nreturn { text = payload.text .. '!', n = payload.n * 2, extra = 1 }"}
            })),
            Schemas { source: None, target: Some(&t) },
        )
        .unwrap();
        let out = mapping.apply(&input(), None).unwrap().unwrap();
        let out = Value::from_payload(&out);
        assert_eq!(
            out,
            Value::Object(vec![
                ("text".into(), Value::String("hello!".into())),
                ("n".into(), Value::I64(82)),
            ]),
            "shaped to the schema (order, types; `extra` dropped)"
        );
        // The filter drops it.
        let low = payload(vec![
            ("n", Value::I64(1)),
            ("text", Value::String("x".into())),
        ]);
        assert_eq!(mapping.apply(&low, None), Ok(None));
        // `return nil` drops it too.
        let skip = payload(vec![
            ("n", Value::I64(50)),
            ("text", Value::String("skip".into())),
        ]);
        assert_eq!(mapping.apply(&skip, None), Ok(None));
    }

    #[test]
    fn meta_is_visible() {
        let mapping = compile(
            &spec(json!({"rules": [{"target": "text", "lua": "return meta.trace_id .. ':' .. meta.hops"}]})),
            Schemas::default(),
        )
        .unwrap();
        let meta = EventMetadata {
            trace_id: "t1".into(),
            hops: 3,
            ..Default::default()
        };
        let out = mapping.apply(&input(), Some(&meta)).unwrap().unwrap();
        assert_eq!(
            Path::parse("text").unwrap().get(&Value::from_payload(&out)),
            Some(&Value::String("t1:3".into()))
        );
    }

    #[test]
    fn the_sandbox_keeps_scripts_in() {
        for (code, expected) in [
            (
                "return io.open('/etc/passwd')",
                "attempt to index a nil value",
            ),
            ("return os.execute('true')", "attempt to index a nil value"),
            ("return require('os')", "attempt to call a nil value"),
            ("return load('return 1')()", "attempt to call a nil value"),
            (
                "return dofile('/etc/passwd')",
                "attempt to call a nil value",
            ),
            (
                "return loadfile('/etc/passwd')",
                "attempt to call a nil value",
            ),
            (
                "return collectgarbage('count')",
                "attempt to call a nil value",
            ),
            ("return debug.getinfo(1)", "attempt to index a nil value"),
            ("x = 1 return 'x'", "assigning globals isn't allowed"),
            ("payload.text = 'changed' return 'x'", "read-only"),
            ("payload.list[1] = 5 return 'x'", "read-only"),
            ("string.upper = nil return 'x'", "read-only"),
            (
                "setmetatable(payload, {}) return 'x'",
                "protected metatable",
            ),
        ] {
            let err = rule(code, "text").unwrap_err();
            assert!(err.contains(expected), "{code:?}: {err:?}");
        }
        // What's meant to be there works.
        assert_eq!(
            rule(
                "return string.format('%d', math.max(1, 2)) .. utf8.char(65)",
                "text"
            ),
            Ok(Some(Value::String("2A".into())))
        );
        assert_eq!(
            rule("return tostring(getmetatable(payload))", "text"),
            Ok(Some(Value::String("false".into())))
        );
    }

    #[test]
    fn limits_stop_runaway_scripts() {
        let run = |limits: serde_json::Value, code: &str| {
            let mapping = compile(
                &spec(json!({"lua": limits, "rules": [{"target": "text", "lua": code}]})),
                Schemas::default(),
            )
            .unwrap();
            mapping.apply(&input(), None).unwrap_err()
        };
        let err = run(json!({}), "while true do end");
        assert!(err.contains("stopped after 1000000 instructions"), "{err}");
        let err = run(
            json!({"max_instructions": 1_000_000_000, "timeout_ms": 20}),
            "while true do end",
        );
        assert!(err.contains("stopped after 20 ms"), "{err}");
        let err = run(
            json!({"max_memory_bytes": 262_144}),
            "local t = {} for i = 1, 1e7 do t[i] = i end return 'x'",
        );
        assert!(err.contains("more than 262144 bytes of memory"), "{err}");
        // The state stays usable after a limit was hit.
        let mapping = compile(
            &spec(json!({"rules": [{"target": "text", "lua": "if payload.n > 100 then while true do end end return 'ok'"}]})),
            Schemas::default(),
        )
        .unwrap();
        let big = payload(vec![("n", Value::I64(101))]);
        assert!(mapping.apply(&big, None).is_err());
        assert!(mapping.apply(&input(), None).unwrap().is_some());
    }

    #[test]
    fn syntax_errors_and_limits_are_reported_at_compile_time() {
        let errors = compile(
            &spec(json!({"filter": {"lua": "return ("}, "rules": [{"target": "a", "lua": "return 1 +"}]})),
            Schemas::default(),
        )
        .unwrap_err();
        assert_eq!(errors.len(), 2, "{errors:?}");
        assert!(
            errors[0].starts_with("mapping.filter.lua: mapping.filter:1:"),
            "{errors:?}"
        );
        assert!(errors[1].starts_with("mapping.rules[0].lua:"), "{errors:?}");
        let bad_limits =
            MappingSpec::parse(&json!({"lua": {"timeout_ms": 0}, "rules": []})).unwrap_err();
        assert!(bad_limits[0].contains("mapping.lua.timeout_ms must be between 1 and 10000"));
    }
}
