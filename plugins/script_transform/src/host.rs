//! What scripts get besides the sandbox's standard libraries: the state
//! functions (`load`, `store`, `delete`), `time.now()`, and `log.info` /
//! `log.warn` (rate-limited). Shared with the accumulator.

use crate::store::{Store, Writes, parse_duration_ms};
use flwrs_types::lua::mlua::{self, Lua, Table};
use flwrs_types::lua::{self, LogLevel};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

/// The store and the current input's changes, shared with the host functions.
pub struct State {
    pub store: Store,
    pub writes: Writes,
}

pub type SharedState = Arc<Mutex<State>>;

pub fn lock(state: &SharedState) -> std::sync::MutexGuard<'_, State> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

fn runtime_error(e: String) -> mlua::Error {
    mlua::Error::runtime(e)
}

/// Installs `load(key)`, `store(key, value [, {ttl = "10m"}])` and
/// `delete(key)`.
pub fn install_state(lua: &Lua, globals: &Table, state: SharedState) -> mlua::Result<()> {
    let s = state.clone();
    globals.raw_set(
        "load",
        lua.create_function(move |lua, key: String| {
            let guard = lock(&s);
            let value = guard
                .store
                .get(&guard.writes, &key)
                .map_err(runtime_error)?;
            drop(guard);
            match value {
                Some(v) => lua::plain_lua(lua, &v),
                None => Ok(mlua::Value::Nil),
            }
        })?,
    )?;
    let s = state.clone();
    globals.raw_set(
        "store",
        lua.create_function(
            move |_, (key, value, opts): (String, mlua::Value, Option<Table>)| {
                let ttl = match opts {
                    None => None,
                    Some(opts) => match opts.raw_get::<Option<String>>("ttl")? {
                        None => None,
                        Some(text) => Some(
                            parse_duration_ms(&text)
                                .map_err(|e| runtime_error(format!("store: ttl {e}")))?,
                        ),
                    },
                };
                if value.is_nil() {
                    return Err(runtime_error(
                        "store: the value is nil (use delete to remove a key)".into(),
                    ));
                }
                let value = lua::from_lua_lossless(value)
                    .map_err(|e| runtime_error(format!("store {key:?}: {e}")))?;
                let mut guard = lock(&s);
                let State { store, writes } = &mut *guard;
                store
                    .put(writes, &key, value, ttl)
                    .map_err(|e| runtime_error(format!("store: {e}")))
            },
        )?,
    )?;
    globals.raw_set(
        "delete",
        lua.create_function(move |_, key: String| {
            let mut guard = lock(&state);
            let State { store, writes } = &mut *guard;
            store
                .remove(writes, &key)
                .map_err(|e| runtime_error(format!("delete: {e}")))
        })?,
    )?;
    Ok(())
}

/// Installs `time` and `log`. Log lines go to the plugin's log (and so to
/// the hub), at most [`LOG_BURST`] per [`LOG_WINDOW`]; the rest are counted
/// and reported once the window ends.
pub fn install_helpers(lua: &Lua, globals: &Table) -> mlua::Result<()> {
    globals.raw_set("time", lua::time_library(lua)?)?;
    let limiter = Arc::new(Mutex::new(LogLimiter::default()));
    let sink = Arc::new(move |level: LogLevel, text: String| {
        let mut l = limiter.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(dropped) = l.allow() {
            if dropped > 0 {
                log::warn!(
                    "script: {dropped} log lines were dropped (more than {LOG_BURST} per {}s)",
                    LOG_WINDOW.as_secs()
                );
            }
            match level {
                LogLevel::Info => log::info!("script: {text}"),
                LogLevel::Warn => log::warn!("script: {text}"),
            }
        }
    });
    globals.raw_set("log", lua::log_library(lua, sink)?)?;
    Ok(())
}

pub const LOG_BURST: u32 = 20;
pub const LOG_WINDOW: Duration = Duration::from_secs(10);

#[derive(Default)]
struct LogLimiter {
    start: Option<Instant>,
    sent: u32,
    dropped: u64,
}

impl LogLimiter {
    /// `Some(dropped since the last allowed line)` if this line may go out.
    fn allow(&mut self) -> Option<u64> {
        let now = Instant::now();
        if self
            .start
            .is_none_or(|s| now.duration_since(s) >= LOG_WINDOW)
        {
            self.start = Some(now);
            self.sent = 0;
        }
        if self.sent < LOG_BURST {
            self.sent += 1;
            Some(std::mem::take(&mut self.dropped))
        } else {
            self.dropped += 1;
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_log_limiter_counts_what_it_drops() {
        let mut l = LogLimiter::default();
        for _ in 0..LOG_BURST {
            assert_eq!(l.allow(), Some(0));
        }
        assert_eq!(l.allow(), None);
        assert_eq!(l.allow(), None);
        l.start = Some(Instant::now() - LOG_WINDOW);
        assert_eq!(l.allow(), Some(2));
    }
}
