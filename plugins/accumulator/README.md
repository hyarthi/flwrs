# Accumulator (`flwrs-accumulator`)

A transform that aggregates events per key, over windows: averages per device per minute, counts per session, running totals. The author writes small Lua functions; the plugin keeps the state, runs the windows and their timers, and survives restarts.

## Settings

| Key | Default | Meaning |
|---|---|---|
| `schemas` | (required) | `{"in": …, "out": …}`: the payload schemas of the events it takes and emits. |
| `key` | | Dot path of the grouping key in the input (`device`, `tags["site"]`). Strings are used as they are, numbers and booleans as text. Empty: everything is one group. |
| `init` | | Lua, `function(key)`: a group's (or window's) initial state. Without it, the state starts as `nil`. |
| `step` | (required) | Lua, `function(state, payload, meta, key)`: returns the new state (`nil` forgets it) and, optionally, an output for this event (one table, or `emit_all({…})`). |
| `finish` | | Lua, `function(state, key, window)`: returns the output of a window that closes (or `nil`). Required with windows. `window` has `start_time` and `end_time` (epoch ms). |
| `window` | none | See below. |
| `late_close` | `"run"` | Windows that ended while the plugin wasn't running: `run` (`finish` runs for them when it starts, late) or `drop`. |
| `on_error` | `"nack"` | When `step` fails: `nack`, `nack_retry` or `drop`, as for the script transform. The state is left as it was. |
| `limits`, `state` | | As for the script transform: Lua limits per call; state limits, `sync`, `applied_window`, `max_file_bytes`. |

## Windows

Windows run on **processing time**: when the event arrives.

| `window` | Behaviour |
|---|---|
| `{"type": "none"}` (default) | One running aggregate per key that never closes; only `step` emits. `idle_ttl` (e.g. `"1h"`) forgets a key that wasn't updated for that long. |
| `{"type": "tumbling", "size": "1m"}` | Back-to-back windows of `size`, aligned to the epoch (UTC): a minute's window closes at the full minute. |
| `{"type": "hopping", "size": "5m", "hop": "1m"}` | Windows of `size` starting every `hop`: each event steps every window that covers it (at most 1 000). |
| `{"type": "session", "gap": "30s"}` | Per key: a window that starts with an event and stays open while the next one comes within `gap`; it closes `gap` after the last one. |

```json
{
  "key": "device",
  "init": "return { n = 0, sum = 0 }",
  "step": "state.n = state.n + 1; state.sum = state.sum + payload.temp; return state",
  "finish": "return { device = key, avg = state.sum / state.n, n = state.n, window_start = window.start_time }",
  "window": { "type": "tumbling", "size": "1m" }
}
```

A closing window's state is deleted. Windows are checked every 100 ms, so an output comes at most that late (later under load).

## Delivery

- **Inputs** are processed exactly once, like the script transform: each event's state changes, its `step` outputs and its event id are committed together, and a redelivered event gets the stored outputs without stepping again.
- **Window outputs** (`finish`) are committed with the window's removal into a queue in the state file, then sent and removed once the hub has confirmed them. So they're at least once: one can be sent twice if the plugin stops between the hub's confirmation and the removal. They start a new trace.
- **Restarts** keep everything: open windows, their timers, queued outputs. Windows that ended meanwhile close at start (`late_close`).

The state file is `state.db` in the node's folder, with the script transform's limits (`state.max_file_bytes` and the rest). The functions also get `load` / `store` / `delete` (state beyond the windows), `json`, `time.now()` and `log.info` / `log.warn`; `types/accumulator.d.lua` describes them for editors.

## Footprint

Measured with `cargo trellis bench plugins/accumulator` (`benches/`) on an Apple M2 (macOS, release build), 4 devices cycled. "Max load" is a closed loop with 64 events in flight; events are processed one at a time.

| Scenario | Events/s | p50 / p99 latency | CPU | RSS |
|---|---|---|---|---|
| Running aggregate (output per event), max load | ~12 800 | 1.2 / 3.0 ms | ~61 % | ~12 MiB |
| 1 s tumbling windows, max load | ~13 200 | 1.2 / 2.9 ms | ~55 % | ~11 MiB |
| 1 s tumbling windows, steady 1 000/s | 1 000 | 0.9 / 1.6 ms | ~4 % | ~10 MiB |

Binary 7.2 MB, package 3.1 MB. Storage speed matters as for the script transform (a commit per event).

## Tests

`cargo test -p flwrs-accumulator`: the windows (parsing, which windows cover a time), the engine (a running aggregate exactly once, tumbling, hopping and session windows closing on time, late windows dropped, `nil` forgetting a group, a failing step changing nothing), the settings, the package files against the CLI, and `tests/trellis.rs` (the trellis audit, and `tests/trellis/session.test.toml`: steps, then the session's window closing on its timer).
