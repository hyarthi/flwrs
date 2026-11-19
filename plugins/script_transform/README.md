# Script transform (`flwrs-script-transform`)

A transform that runs a Lua script on each event. The script can keep state per node (counters, last values, deduplication), and every state change is applied **exactly once**, even when the hub delivers an event again after a crash or a lost connection.

## Settings

| Key | Default | Meaning |
|---|---|---|
| `schemas` | (required) | `{"in": …, "out": …}`: the payload schemas of the events the script takes and returns (`SchemaDefinition`s, e.g. `{"fields": [{"key": "device", "type": "STRING"}]}`). The node's edges are checked against them. |
| `script` | (required) | The Lua code: the body of `function(payload, meta)`. |
| `on_error` | `"nack"` | When the script fails (an error, a limit, an output that doesn't fit `out`): `nack` (the event is rejected and dead-lettered), `nack_retry` (rejected; the hub delivers it again later), `drop` (acknowledged with no outputs). The state is left as it was either way. |
| `limits` | | Per event: `max_instructions` (1 000 000), `timeout_ms` (100), `max_memory_bytes` (16 MiB). |
| `state` | | `max_keys` (100 000), `max_value_bytes` (64 KiB), `max_bytes` (256 MiB), `sync` (`normal`, or `full`: every commit on disk before the event is acknowledged), `applied_window` (10 000: how many recent events' outputs are kept to answer a redelivery), `max_file_bytes` (1 GiB: the state file's size on disk; see *Space*). |

**Templates:** in the web UI, a new node starts with a `schemas` and a `script` that fit each other ([`templates/`](templates): a temperature reading in; °F, an alert flag and a per-device count out, kept in the node's state). They work as they are; edit them into your own.

## The script

```lua
-- count readings per device, and pass on the ones above the device's average
local key = "avg:" .. payload.device
local s = load(key) or { n = 0, sum = 0 }
s.n = s.n + 1
s.sum = s.sum + payload.temp
store(key, s)
if payload.temp > s.sum / s.n then
  return { device = payload.device, temp = payload.temp, avg = s.sum / s.n }
end
-- nothing returned: no output for this event
```

- **`payload`** is the input event (read-only); **`meta`** has `event_id`, `trace_id`, `hops`, `origin_node_id` and `create_time` (epoch ms).
- **Return** a table (one output), `nil` (none), or `emit_all({ a, b, … })` (several). Outputs are shaped by the `out` schema: values are converted where that loses nothing, and fields it doesn't declare are dropped (logged once each, at debug level).
- **State:** `load(key)`, `store(key, value [, {ttl = "10m"}])`, `delete(key)`. Keys are strings (1 to 256 bytes); values are numbers, strings (binary too), booleans and tables, and come back as they were stored (integers stay integers). A key with a `ttl` is forgotten after that long.
- **Libraries:** `string`, `table`, `math`, `utf8`, `json` (`encode` / `decode`), `time.now()` (epoch ms) and `log.info(…)` / `log.warn(…)` (to the plugin's log, at most 20 lines per 10 s). There's no file, network or OS access, and globals can't be assigned: nothing carries over between events except through the state.
- `types/script.d.lua` describes all of this for editors (lua-language-server).

## Exactly-once state

The hub delivers an event again if it wasn't acknowledged (a crash, a lost connection), with the same `event_id`. Running the script again would apply its `store`s twice. So for each event:

1. the script runs; its `store`s and `delete`s are collected (its own `load`s see them);
2. those changes, the outputs and the event id are committed to `state.db` in **one transaction**;
3. the outputs are sent and the event is acknowledged.

A redelivered event whose id is in the last `applied_window` commits gets the stored outputs again, without running the script. So state changes happen exactly once, and outputs at least once (a consumer may see the same outputs twice). A script that fails changes nothing.

**Space.** `state.max_file_bytes` caps the state file on disk (with its write-ahead log). The redelivery records are kept in partitions (tables of the same file, so a commit stays atomic): a partition is retired whole once it's outside `applied_window`, and the oldest ones also go when the file reaches 90 % of the limit (the window in which a redelivery is recognized gets shorter). The file gives the freed space back to the OS. The node's own keys are never dropped: when they alone fill the file, events that would add data fail (`on_error` decides what happens to them) until the script deletes keys; `max_keys` and `max_bytes` limit them too.

The state file is in the node's folder, so it's kept across restarts, reloads and upgrades, and deleted with the node (or emptied with `DELETE …/nodes/by-id/{id}/workdir` while the scene is disabled). Events are processed one at a time, in order.

## Footprint

Measured with `cargo trellis bench plugins/script_transform` (`benches/`) on an Apple M2 (macOS, release build), 4 devices cycled. "Max load" is a closed loop with 64 events in flight; events are processed one at a time, so throughput is per node.

| Scenario | Events/s | p50 / p99 latency | CPU | RSS |
|---|---|---|---|---|
| Stateless script, max load | ~15 500 | 1.0 / 3.0 ms | ~63 % | ~12 MiB |
| `load` + `store` per event, max load | ~11 900 | 1.3 / 3.6 ms | ~58 % | ~12 MiB |
| The same, `sync = full` | ~9 700 | 1.7 / 3.5 ms | ~47 % | ~12 MiB |
| `load` + `store`, steady 1 000/s | 1 000 | 0.15 / 3.1 ms | ~5 % | ~10 MiB |

The steady-rate row was remeasured with trellis 0.0.5's pacing (latency from when the harness woke for each send); its p99 varied between runs (0.3–6 ms), like the harness's own calibration on that machine. Binary 6.9 MB, package 3.0 MB (Lua 5.4 and SQLite are compiled in). Every event commits to `state.db` (the outbox), so storage speed matters: macOS doesn't flush to the device on each `fsync` by default, so `sync = full` costs more on Linux with a slow SD card than shown here.

## Tests

`cargo test -p flwrs-script-transform`: the store (overlay reads, types kept, TTL, limits, the outbox window, reopening), the engine (exactly-once counting, a failing script changes nothing, none / one / several outputs, the helpers), the settings files, the package files against the CLI, and `tests/trellis.rs` (the trellis audit, and `tests/trellis/counter.test.toml`: counting, a redelivery, a NACK, state across a restart).
