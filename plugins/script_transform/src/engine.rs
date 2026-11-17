//! Runs the script on one input: answered from the outbox if it was already
//! applied, else the script runs, and its state changes, outputs and event
//! id are committed together (see `store`).

use crate::host::{self, SharedState, State};
use crate::store::Store;
use flwrs_plugin::schema::payload::{PluginPayload, SchemaDefinition};
use flwrs_plugin::schema::protocol::EventMetadata;
use flwrs_types::lua::mlua::{self, Table};
use flwrs_types::lua::{self, Chunk, Limits, Runtime};
use flwrs_types::value::{self, Value};
use std::collections::HashSet;
use std::sync::{Arc, Mutex};

/// Marks the table `emit_all` returns.
const MANY: &str = "__flwrs_emit_all";
/// The script's parameters.
pub const PARAMS: [&str; 2] = ["payload", "meta"];

pub struct Engine {
    runtime: Runtime,
    script: Chunk,
    output: SchemaDefinition,
    state: SharedState,
    /// Output keys the schema doesn't declare, already reported.
    undeclared: HashSet<String>,
}

impl Engine {
    pub fn new(
        script: &str,
        output: SchemaDefinition,
        limits: Limits,
        store: Store,
    ) -> Result<Self, String> {
        let state = Arc::new(Mutex::new(State {
            store,
            writes: Default::default(),
        }));
        let host_state = state.clone();
        let runtime = Runtime::with_host(limits, &move |lua, globals| {
            host::install_state(lua, globals, host_state.clone())?;
            host::install_helpers(lua, globals)?;
            install_emit_all(lua, globals)
        })?;
        let script = runtime
            .chunk_with("script", script, &PARAMS)
            .map_err(|e| format!("script: {e}"))?;
        Ok(Self {
            runtime,
            script,
            output,
            state,
            undeclared: HashSet::new(),
        })
    }

    /// The outputs for one input. An error leaves the state as it was.
    pub fn process(
        &mut self,
        payload: &PluginPayload,
        meta: Option<&EventMetadata>,
    ) -> Result<Vec<PluginPayload>, String> {
        let event_id = meta
            .map(|m| m.event_id.as_str())
            .filter(|id| !id.is_empty());
        if let Some(id) = event_id
            && let Some(outputs) = host::lock(&self.state).store.applied(id)?
        {
            log::debug!("event {id} was already applied: sending its outputs again");
            return Ok(outputs);
        }
        host::lock(&self.state).writes.clear();
        let result = self.run(payload, meta);
        let mut guard = host::lock(&self.state);
        let committed = result.and_then(|outputs| {
            let State { store, writes } = &mut *guard;
            store.commit(writes, event_id, &outputs).map(|()| outputs)
        });
        guard.writes.clear();
        committed
    }

    fn run(
        &mut self,
        payload: &PluginPayload,
        meta: Option<&EventMetadata>,
    ) -> Result<Vec<PluginPayload>, String> {
        let input = Value::from_payload(payload);
        let result = self.runtime.call(&self.script, &input, meta)?;
        outputs(result, &self.output, &mut self.undeclared)
    }
}

/// A script's result as outputs: `nil` (none), `emit_all({…})` (several) or
/// one table, each shaped by `schema`. Fields it doesn't declare are dropped;
/// each name is logged once (in `undeclared`), so a typo can be found.
pub fn outputs(
    result: mlua::Value,
    schema: &SchemaDefinition,
    undeclared: &mut HashSet<String>,
) -> Result<Vec<PluginPayload>, String> {
    let items = match result {
        mlua::Value::Nil => vec![],
        mlua::Value::Table(t) if is_many(&t) => {
            let list: Table = t.raw_get("items").map_err(|e| format!("emit_all: {e}"))?;
            list.sequence_values::<mlua::Value>()
                .collect::<mlua::Result<Vec<_>>>()
                .map_err(|e| format!("emit_all: {e}"))?
        }
        other => vec![other],
    };
    items
        .into_iter()
        .enumerate()
        .map(|(i, item)| {
            if let mlua::Value::Table(t) = &item {
                for (key, _) in t.pairs::<String, mlua::Value>().flatten() {
                    if !schema.fields.iter().any(|f| f.key == key) && undeclared.insert(key.clone()) {
                        log::debug!(
                            "the script returns a field {key:?} the out schema doesn't declare; it's dropped"
                        );
                    }
                }
            }
            lua::payload_from_lua(item, Some(schema))
                .and_then(|v| value::coerce_payload(v, schema))
                .and_then(Value::into_payload)
                .map_err(|e| format!("output {}: {e}", i + 1))
        })
        .collect()
}

/// `emit_all(list)`: several outputs from one input.
pub fn install_emit_all(lua: &mlua::Lua, globals: &Table) -> mlua::Result<()> {
    let marker = lua.create_table()?;
    marker.raw_set(MANY, true)?;
    globals.raw_set(
        "emit_all",
        lua.create_function(move |lua, items: Table| {
            let many = lua.create_table()?;
            many.raw_set("items", items)?;
            many.set_metatable(Some(marker.clone()))?;
            Ok(many)
        })?,
    )
}

fn is_many(t: &Table) -> bool {
    t.metatable()
        .and_then(|m| m.raw_get::<bool>(MANY).ok())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::StateSettings;
    use flwrs_plugin::schema::rules::parse_json;

    fn engine(script: &str) -> Engine {
        let output = parse_json(
            r#"{"fields": [{"key": "device", "type": "STRING"}, {"key": "n", "type": "U64"}]}"#,
        )
        .unwrap();
        Engine::new(
            script,
            output,
            Limits::default(),
            Store::memory(StateSettings::default()).unwrap(),
        )
        .unwrap()
    }

    fn input(device: &str) -> PluginPayload {
        Value::Object(vec![("device".into(), Value::String(device.into()))])
            .into_payload()
            .unwrap()
    }

    fn meta(id: &str) -> EventMetadata {
        EventMetadata {
            event_id: id.into(),
            ..Default::default()
        }
    }

    fn outputs(out: &[PluginPayload]) -> Vec<Value> {
        out.iter().map(Value::from_payload).collect()
    }

    fn count(device: &str, n: u64) -> Value {
        Value::Object(vec![
            ("device".into(), Value::String(device.into())),
            ("n".into(), Value::U64(n)),
        ])
    }

    const COUNTER: &str = "local n = (load(payload.device) or 0) + 1
        store(payload.device, n)
        return { device = payload.device, n = n, typo = 1 }";

    #[test]
    fn counts_per_key_exactly_once() {
        let mut e = engine(COUNTER);
        assert_eq!(
            outputs(&e.process(&input("a"), Some(&meta("e1"))).unwrap()),
            [count("a", 1)]
        );
        assert_eq!(
            outputs(&e.process(&input("a"), Some(&meta("e2"))).unwrap()),
            [count("a", 2)]
        );
        assert_eq!(
            outputs(&e.process(&input("b"), Some(&meta("e3"))).unwrap()),
            [count("b", 1)]
        );
        // a redelivery: the same outputs, and the count didn't move
        assert_eq!(
            outputs(&e.process(&input("a"), Some(&meta("e2"))).unwrap()),
            [count("a", 2)]
        );
        assert_eq!(
            outputs(&e.process(&input("a"), Some(&meta("e4"))).unwrap()),
            [count("a", 3)]
        );
        // without an event id: no outbox, just runs
        assert_eq!(
            outputs(&e.process(&input("a"), None).unwrap()),
            [count("a", 4)]
        );
    }

    #[test]
    fn a_failing_script_changes_nothing() {
        let mut e = engine(
            "store('k', 1); if payload.device == 'bad' then error('boom') end
             return { n = load('k') }",
        );
        let err = e.process(&input("bad"), Some(&meta("e1"))).unwrap_err();
        assert!(err.contains("boom"), "{err}");
        let state = host::lock(&e.state);
        assert_eq!(state.store.applied("e1").unwrap(), None);
        assert_eq!(state.store.get(&state.writes, "k").unwrap(), None);
    }

    #[test]
    fn none_one_or_several_outputs() {
        let mut e = engine(
            "if payload.device == 'none' then return nil end
             if payload.device == 'many' then
                 return emit_all({ { device = 'x', n = 1 }, { device = 'y', n = 2 } })
             end
             return { device = payload.device }",
        );
        assert!(e.process(&input("none"), None).unwrap().is_empty());
        assert_eq!(
            outputs(&e.process(&input("many"), None).unwrap()),
            [count("x", 1), count("y", 2)]
        );
        assert_eq!(
            outputs(&e.process(&input("one"), None).unwrap()),
            [Value::Object(vec![(
                "device".into(),
                Value::String("one".into())
            )])]
        );
        // an output that doesn't fit the schema is an error
        let mut e = engine("return { n = 'x' }");
        assert!(
            e.process(&input("a"), None)
                .unwrap_err()
                .starts_with("output 1:")
        );
    }

    #[test]
    fn helpers_are_there() {
        let mut e = engine(
            "log.info('saw', payload.device)
             store('t', time.now(), { ttl = '1h' })
             return { n = math.floor(load('t') / 1000) > 0 and 1 or 0 }",
        );
        assert_eq!(
            outputs(&e.process(&input("a"), None).unwrap()),
            [Value::Object(vec![("n".into(), Value::U64(1))])]
        );
        let mut e = engine("store('t', 1, { ttl = 'soon' })");
        assert!(e.process(&input("a"), None).unwrap_err().contains("ttl"));
    }
}
