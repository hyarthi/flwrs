//! The accumulator: `step(state, payload, meta, key)` per event and window,
//! `finish(state, key, window)` when a window closes.
//!
//! Everything lives in the node's state store, as keys (so one commit covers
//! an input's or a timer's whole effect, exactly once):
//! - `acc:<start>:<key>`: a window's state (`start` 0 without windows);
//! - `win:<end>:<start>:<key>`: open windows, by end time (the timers);
//! - `ses:<key>`: a key's open session (`{start, end}`);
//! - `out:<seq>`: outputs of closed windows waiting to be sent.
//!
//! Times are zero-padded so key order is time order.

use crate::window::Window;
use flwrs_plugin::schema::payload::{PluginPayload, SchemaDefinition};
use flwrs_plugin::schema::protocol::EventMetadata;
use flwrs_script_transform::engine::{install_emit_all, outputs};
use flwrs_script_transform::host::{self, SharedState, State};
use flwrs_script_transform::store::{Store, Writes, now_ms};
use flwrs_types::lua::mlua;
use flwrs_types::lua::{self, Arg, Chunk, Limits, Runtime};
use flwrs_types::path::Path;
use flwrs_types::value::Value;
use std::collections::HashSet;
use std::sync::{Arc, Mutex};

/// Timers closed per round.
const CLOSE_BATCH: usize = 500;

pub const INIT_PARAMS: [&str; 1] = ["key"];
pub const STEP_PARAMS: [&str; 4] = ["state", "payload", "meta", "key"];
pub const FINISH_PARAMS: [&str; 3] = ["state", "key", "window"];

/// What happens to windows that ended while the plugin wasn't running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LateClose {
    /// `finish` runs for them when the plugin starts (late).
    Run,
    /// They're discarded without `finish`.
    Drop,
}

/// The accumulator's code and settings.
pub struct Code<'a> {
    pub init: Option<&'a str>,
    pub step: &'a str,
    pub finish: Option<&'a str>,
}

pub struct Accumulator {
    runtime: Runtime,
    init: Option<Chunk>,
    step: Chunk,
    finish: Option<Chunk>,
    key: Option<Path>,
    window: Window,
    output: SchemaDefinition,
    late_close: LateClose,
    state: SharedState,
    undeclared: HashSet<String>,
    /// When this run started: windows that ended before are "late".
    started: i64,
    /// The last `out:` sequence number used.
    last_out: u64,
}

fn pad(n: i64) -> String {
    format!("{:020}", n.max(0))
}

fn state_key(start: i64, key: &str) -> String {
    format!("acc:{}:{key}", pad(start))
}

fn timer_key(end: i64, start: i64, key: &str) -> String {
    format!("win:{}:{}:{key}", pad(end), pad(start))
}

fn session_key(key: &str) -> String {
    format!("ses:{key}")
}

/// A window as a value (stored in the timer index, passed to `finish`).
fn window_value(key: &str, start: i64, end: i64) -> Value {
    Value::Object(vec![
        ("key".into(), Value::String(key.into())),
        ("start_time".into(), Value::I64(start)),
        ("end_time".into(), Value::I64(end)),
    ])
}

fn field(v: &Value, name: &str) -> Option<Value> {
    match v {
        Value::Object(fields) => fields
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone()),
        _ => None,
    }
}

fn int(v: &Value, name: &str) -> Option<i64> {
    match field(v, name)? {
        Value::I64(n) => Some(n),
        Value::U64(n) => i64::try_from(n).ok(),
        Value::I32(n) => Some(n.into()),
        _ => None,
    }
}

impl Accumulator {
    pub fn new(
        code: Code,
        key: Option<Path>,
        window: Window,
        output: SchemaDefinition,
        late_close: LateClose,
        limits: Limits,
        store: Store,
    ) -> Result<Self, String> {
        let state = Arc::new(Mutex::new(State {
            store,
            writes: Writes::default(),
        }));
        let host_state = state.clone();
        let runtime = Runtime::with_host(limits, &move |lua, globals| {
            host::install_state(lua, globals, host_state.clone())?;
            host::install_helpers(lua, globals)?;
            install_emit_all(lua, globals)
        })?;
        let chunk = |label: &str, code: &str, params: &[&str]| {
            runtime
                .chunk_with(label, code, params)
                .map_err(|e| format!("{label}: {e}"))
        };
        let init = code
            .init
            .map(|c| chunk("init", c, &INIT_PARAMS))
            .transpose()?;
        let step = chunk("step", code.step, &STEP_PARAMS)?;
        let finish = code
            .finish
            .map(|c| chunk("finish", c, &FINISH_PARAMS))
            .transpose()?;
        let last_out = host::lock(&state)
            .store
            .scan("out:", "out;", usize::MAX)?
            .last()
            .and_then(|(k, _)| k.strip_prefix("out:")?.parse().ok())
            .unwrap_or(0);
        Ok(Self {
            runtime,
            init,
            step,
            finish,
            key,
            window,
            output,
            late_close,
            state,
            undeclared: HashSet::new(),
            started: now_ms(),
            last_out,
        })
    }

    /// One input at time `now`: `step` for each window it belongs to. Its
    /// outputs go back tied to the input; a session it ends is closed (its
    /// outputs queued). Exactly once, like the script transform.
    pub fn process_at(
        &mut self,
        payload: &PluginPayload,
        meta: Option<&EventMetadata>,
        now: i64,
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
        let result = self.apply(payload, meta, now);
        let mut guard = host::lock(&self.state);
        let committed = result.and_then(|outputs| {
            let State { store, writes } = &mut *guard;
            store.commit(writes, event_id, &outputs).map(|()| outputs)
        });
        guard.writes.clear();
        committed
    }

    fn apply(
        &mut self,
        payload: &PluginPayload,
        meta: Option<&EventMetadata>,
        now: i64,
    ) -> Result<Vec<PluginPayload>, String> {
        let input = Value::from_payload(payload);
        let key = self.key_of(&input);
        // (start, end) of each window to step; end None = no timer.
        let slots: Vec<(i64, Option<i64>)> = match self.window {
            Window::None { .. } => vec![(0, None)],
            Window::Tumbling { .. } | Window::Hopping { .. } => self
                .window
                .covering(now)
                .into_iter()
                .map(|(s, e)| (s, Some(e)))
                .collect(),
            Window::Session { gap } => {
                let open = self.get(&session_key(&key))?;
                let (start, old_end) = match open
                    .as_ref()
                    .and_then(|s| Some((int(s, "start_time")?, int(s, "end_time")?)))
                {
                    Some((start, end)) if now < end => (start, Some(end)),
                    Some((start, end)) => {
                        // The timer hasn't closed it yet: close it now.
                        self.close(&key, start, end, false)?;
                        (now, None)
                    }
                    None => (now, None),
                };
                let end = now + gap;
                if let Some(old) = old_end {
                    self.remove(&timer_key(old, start, &key))?;
                }
                self.put(
                    &timer_key(end, start, &key),
                    window_value(&key, start, end),
                    None,
                )?;
                self.put(&session_key(&key), window_value(&key, start, end), None)?;
                vec![(start, None)]
            }
        };
        let idle_ttl = match self.window {
            Window::None { idle_ttl } => idle_ttl,
            _ => None,
        };
        let mut emitted = vec![];
        for (start, end) in slots {
            let skey = state_key(start, &key);
            let current = match self.get(&skey)? {
                Some(state) => state,
                None => {
                    if let Some(end) = end {
                        self.put(
                            &timer_key(end, start, &key),
                            window_value(&key, start, end),
                            None,
                        )?;
                    }
                    self.initial(&key)?
                }
            };
            let results = self.runtime.call_with(
                &self.step,
                vec![
                    Arg::Plain(&current),
                    Arg::ReadOnly(&input),
                    Arg::Meta(meta),
                    Arg::Lua(mlua::Value::String(self.runtime_string(&key)?)),
                ],
            )?;
            let mut results = results.into_iter();
            let new_state = results.next().unwrap_or(mlua::Value::Nil);
            let emit = results.next().unwrap_or(mlua::Value::Nil);
            if new_state.is_nil() {
                self.remove(&skey)?;
            } else {
                let value =
                    lua::from_lua_lossless(new_state).map_err(|e| format!("step's state: {e}"))?;
                self.put(&skey, value, idle_ttl)?;
            }
            emitted.extend(outputs(emit, &self.output, &mut self.undeclared)?);
        }
        Ok(emitted)
    }

    /// Closes the windows that ended by `now` (at most a batch): `finish`
    /// runs (unless a late window is dropped), its outputs are queued, and
    /// the window's state goes, in one commit. Returns how many closed.
    pub fn close_due(&mut self, now: i64) -> Result<usize, String> {
        let due = host::lock(&self.state).store.scan(
            "win:",
            &format!("win:{}", pad(now + 1)),
            CLOSE_BATCH,
        )?;
        if due.is_empty() {
            return Ok(0);
        }
        host::lock(&self.state).writes.clear();
        let result = (|| {
            for (timer, window) in &due {
                let (Some(Value::String(key)), Some(start), Some(end)) = (
                    field(window, "key"),
                    int(window, "start_time"),
                    int(window, "end_time"),
                ) else {
                    log::warn!("dropping a malformed window timer {timer:?}");
                    self.remove(timer)?;
                    continue;
                };
                let late = end < self.started;
                self.close(&key, start, end, late && self.late_close == LateClose::Drop)?;
            }
            Ok::<(), String>(())
        })();
        let mut guard = host::lock(&self.state);
        let committed = result.and_then(|()| {
            let State { store, writes } = &mut *guard;
            store.commit(writes, None, &[])
        });
        guard.writes.clear();
        committed.map(|()| due.len())
    }

    /// One window's end (within the current changes): `finish` unless
    /// `discard`, outputs queued, its state, timer and session removed.
    fn close(&mut self, key: &str, start: i64, end: i64, discard: bool) -> Result<(), String> {
        let skey = state_key(start, key);
        let finished = match (&self.finish, discard) {
            (Some(finish), false) => {
                let state = self.get(&skey)?.unwrap_or(Value::Null);
                let window = window_value(key, start, end);
                let results = self.runtime.call_with(
                    finish,
                    vec![
                        Arg::Plain(&state),
                        Arg::Lua(mlua::Value::String(self.runtime_string(key)?)),
                        Arg::ReadOnly(&window),
                    ],
                )?;
                results.into_iter().next().unwrap_or(mlua::Value::Nil)
            }
            _ => mlua::Value::Nil,
        };
        for output in outputs(finished, &self.output, &mut self.undeclared)? {
            self.last_out = self.last_out.max(now_ms() as u64 * 1000) + 1;
            let out = Value::from_payload(&output);
            self.put(&format!("out:{:020}", self.last_out), out, None)?;
        }
        self.remove(&skey)?;
        self.remove(&timer_key(end, start, key))?;
        if let Some(open) = self.get(&session_key(key))?
            && int(&open, "start_time") == Some(start)
        {
            self.remove(&session_key(key))?;
        }
        Ok(())
    }

    /// Queued outputs of closed windows, oldest first.
    pub fn pending(&self, limit: usize) -> Result<Vec<(String, PluginPayload)>, String> {
        host::lock(&self.state)
            .store
            .scan("out:", "out;", limit)?
            .into_iter()
            .map(|(k, v)| Ok((k, v.into_payload()?)))
            .collect()
    }

    /// Removes queued outputs once they're sent.
    pub fn sent(&mut self, keys: &[String]) -> Result<(), String> {
        let mut guard = host::lock(&self.state);
        let State { store, writes } = &mut *guard;
        writes.clear();
        for k in keys {
            store.remove(writes, k)?;
        }
        let result = store.commit(writes, None, &[]);
        writes.clear();
        result
    }

    fn key_of(&self, input: &Value) -> String {
        match self.key.as_ref().and_then(|p| p.get(input)) {
            None | Some(Value::Null) => String::new(),
            Some(Value::String(s)) => s.clone(),
            Some(Value::Bool(b)) => b.to_string(),
            Some(Value::I32(n)) => n.to_string(),
            Some(Value::I64(n)) => n.to_string(),
            Some(Value::U32(n)) => n.to_string(),
            Some(Value::U64(n)) => n.to_string(),
            Some(other) => flwrs_types::json::encode(other).unwrap_or_default(),
        }
    }

    fn initial(&self, key: &str) -> Result<Value, String> {
        let Some(init) = &self.init else {
            return Ok(Value::Null);
        };
        let results = self.runtime.call_with(
            init,
            vec![Arg::Lua(mlua::Value::String(self.runtime_string(key)?))],
        )?;
        let state = results.into_iter().next().unwrap_or(mlua::Value::Nil);
        lua::from_lua_lossless(state).map_err(|e| format!("init's state: {e}"))
    }

    fn runtime_string(&self, s: &str) -> Result<mlua::LuaString, String> {
        self.runtime
            .lua()
            .create_string(s)
            .map_err(|e| e.to_string())
    }

    fn get(&self, key: &str) -> Result<Option<Value>, String> {
        let guard = host::lock(&self.state);
        guard.store.get(&guard.writes, key)
    }

    fn put(&self, key: &str, value: Value, ttl: Option<i64>) -> Result<(), String> {
        let mut guard = host::lock(&self.state);
        let State { store, writes } = &mut *guard;
        store.put(writes, key, value, ttl)
    }

    fn remove(&self, key: &str) -> Result<(), String> {
        let mut guard = host::lock(&self.state);
        let State { store, writes } = &mut *guard;
        store.remove(writes, key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flwrs_plugin::schema::rules::parse_json;
    use flwrs_script_transform::store::StateSettings;

    const STEP: &str = "state.n = state.n + 1; state.sum = state.sum + payload.v
        return state, { key = key, n = state.n }";
    const FINISH: &str =
        "return { key = key, n = state.n, sum = state.sum, start = window.start_time }";

    fn acc(window: Window, late_close: LateClose) -> Accumulator {
        let output = parse_json(
            r#"{"fields": [{"key": "key", "type": "STRING"}, {"key": "n", "type": "I64"},
                {"key": "sum", "type": "F64"}, {"key": "start", "type": "I64"}]}"#,
        )
        .unwrap();
        Accumulator::new(
            Code {
                init: Some("return { n = 0, sum = 0 }"),
                step: STEP,
                finish: Some(FINISH),
            },
            Some(Path::parse("device").unwrap()),
            window,
            output,
            late_close,
            Limits::default(),
            Store::memory(StateSettings::default()).unwrap(),
        )
        .unwrap()
    }

    fn input(device: &str, v: f64) -> PluginPayload {
        Value::Object(vec![
            ("device".into(), Value::String(device.into())),
            ("v".into(), Value::F64(v)),
        ])
        .into_payload()
        .unwrap()
    }

    fn meta(id: &str) -> EventMetadata {
        EventMetadata {
            event_id: id.into(),
            ..Default::default()
        }
    }

    fn n_of(p: &PluginPayload) -> Value {
        field(&Value::from_payload(p), "n").unwrap()
    }

    fn pending(a: &mut Accumulator) -> Vec<Value> {
        let items = a.pending(100).unwrap();
        let keys: Vec<String> = items.iter().map(|(k, _)| k.clone()).collect();
        a.sent(&keys).unwrap();
        items.iter().map(|(_, p)| Value::from_payload(p)).collect()
    }

    #[test]
    fn a_running_aggregate_per_key_exactly_once() {
        let mut a = acc(Window::None { idle_ttl: None }, LateClose::Run);
        let out = a
            .process_at(&input("x", 1.0), Some(&meta("e1")), 0)
            .unwrap();
        assert_eq!(n_of(&out[0]), Value::I64(1));
        let out = a
            .process_at(&input("x", 2.0), Some(&meta("e2")), 0)
            .unwrap();
        assert_eq!(n_of(&out[0]), Value::I64(2));
        // a redelivery doesn't step again
        let out = a
            .process_at(&input("x", 2.0), Some(&meta("e2")), 0)
            .unwrap();
        assert_eq!(n_of(&out[0]), Value::I64(2));
        let out = a
            .process_at(&input("y", 5.0), Some(&meta("e3")), 0)
            .unwrap();
        assert_eq!(n_of(&out[0]), Value::I64(1), "keys are separate");
        // no windows: nothing ever closes
        assert_eq!(a.close_due(i64::MAX / 2).unwrap(), 0);
    }

    #[test]
    fn tumbling_windows_close_on_time() {
        let mut a = acc(Window::Tumbling { size: 1_000 }, LateClose::Run);
        a.process_at(&input("x", 1.0), None, 100).unwrap();
        a.process_at(&input("x", 2.0), None, 900).unwrap();
        a.process_at(&input("y", 7.0), None, 950).unwrap();
        assert_eq!(a.close_due(999).unwrap(), 0);
        assert_eq!(a.close_due(1_000).unwrap(), 2);
        let mut out = pending(&mut a);
        out.sort_by_key(|v| format!("{v:?}"));
        assert_eq!(field(&out[0], "sum"), Some(Value::F64(3.0)));
        assert_eq!(field(&out[0], "start"), Some(Value::I64(0)));
        assert_eq!(field(&out[1], "key"), Some(Value::String("y".into())));
        assert!(a.pending(10).unwrap().is_empty(), "sent ones are gone");
        // the next window starts over
        let out = a.process_at(&input("x", 1.0), None, 1_500).unwrap();
        assert_eq!(n_of(&out[0]), Value::I64(1));
    }

    #[test]
    fn hopping_windows_each_get_the_event() {
        let mut a = acc(
            Window::Hopping {
                size: 3_000,
                hop: 1_000,
            },
            LateClose::Run,
        );
        let out = a.process_at(&input("x", 1.0), None, 2_500).unwrap();
        assert_eq!(out.len(), 3, "one step per window");
        assert_eq!(a.close_due(3_000).unwrap(), 1);
        assert_eq!(field(&pending(&mut a)[0], "start"), Some(Value::I64(0)));
        assert_eq!(a.close_due(5_000).unwrap(), 2);
    }

    #[test]
    fn sessions_stay_open_while_events_come() {
        let mut a = acc(Window::Session { gap: 1_000 }, LateClose::Run);
        a.process_at(&input("x", 1.0), None, 0).unwrap();
        a.process_at(&input("x", 1.0), None, 800).unwrap(); // extends to 1800
        assert_eq!(a.close_due(1_000).unwrap(), 0);
        // past the gap: the old session closes, a new one starts
        let out = a.process_at(&input("x", 1.0), None, 2_500).unwrap();
        assert_eq!(n_of(&out[0]), Value::I64(1));
        let closed = pending(&mut a);
        assert_eq!(field(&closed[0], "n"), Some(Value::I64(2)));
        assert_eq!(a.close_due(3_500).unwrap(), 1);
        assert_eq!(field(&pending(&mut a)[0], "start"), Some(Value::I64(2_500)));
    }

    #[test]
    fn late_windows_can_be_dropped() {
        // (these windows ended long before the engine started: "late")
        let mut a = acc(Window::Tumbling { size: 1_000 }, LateClose::Drop);
        a.process_at(&input("x", 1.0), None, 100).unwrap();
        assert_eq!(a.close_due(1_000).unwrap(), 1);
        assert!(pending(&mut a).is_empty());
    }

    #[test]
    fn nil_state_forgets_the_key() {
        let output = parse_json(r#"{"fields": [{"key": "n", "type": "I64"}]}"#).unwrap();
        let mut a = Accumulator::new(
            Code {
                init: None,
                step: "local n = (state or 0) + 1
                       if n == 3 then return nil, { n = n } end
                       return n",
                finish: None,
            },
            None,
            Window::None { idle_ttl: None },
            output,
            LateClose::Run,
            Limits::default(),
            Store::memory(StateSettings::default()).unwrap(),
        )
        .unwrap();
        for _ in 0..2 {
            assert!(a.process_at(&input("x", 0.0), None, 0).unwrap().is_empty());
        }
        let out = a.process_at(&input("x", 0.0), None, 0).unwrap();
        assert_eq!(n_of(&out[0]), Value::I64(3));
        // forgotten: counting starts over
        a.process_at(&input("x", 0.0), None, 0).unwrap();
        assert!(a.get(&state_key(0, "")).unwrap() == Some(Value::I64(1)));
    }

    #[test]
    fn a_failing_step_changes_nothing() {
        let output = parse_json(r#"{"fields": [{"key": "n", "type": "I64"}]}"#).unwrap();
        let mut a = Accumulator::new(
            Code {
                init: None,
                step: "if payload.v > 10 then error('too big') end return (state or 0) + 1",
                finish: None,
            },
            None,
            Window::Tumbling { size: 1_000 },
            output,
            LateClose::Run,
            Limits::default(),
            Store::memory(StateSettings::default()).unwrap(),
        )
        .unwrap();
        assert!(
            a.process_at(&input("x", 99.0), Some(&meta("e1")), 100)
                .unwrap_err()
                .contains("too big")
        );
        // no state, no timer was left behind
        assert_eq!(a.close_due(10_000).unwrap(), 0);
    }
}
