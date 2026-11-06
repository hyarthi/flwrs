# flwrs

<img src="flwrs-logo.svg" alt="flwrs logo" width="100" height="100">

A lightweight framework and server for running pluggable workflows, inspired by [Node-RED](https://nodered.org/).

With flwrs you build workflows (data flows) of any complexity from **pre-compiled plugins** and **configuration**, without writing code. The same runtime is meant to run both in the data centre and at the edge, for anything from simple automation to complex pipelines, such as running ML models as workflow steps.

> **Status:** early development; expect breaking changes. What works today:
>
> - the server core, the plugin protocol and its Rust SDK, the plugin registry (package upload), and scene management over REST;
> - running scenes: the director launches and supervises plugins (local or remote), routes events through bounded queues with optional spill to disk, applies edge mappings (dot notation and Lua), and applies edits to a running scene as a diff, restarting only what changed;
> - scene export / import between servers, status endpoints, and telemetry export (OpenTelemetry, or local files);
> - one base plugin (HTTP sink, Rust), plus minimal demo plugins in Python, Go, TypeScript and Java.
>
> See the [roadmap](#roadmap) for what's missing.
> 
> I'm using this project as a learning/test bed to experiment with Rust and local/open ML/AI models.

## Concepts

- **Plugins** are standalone executables, **written in any language**. The server launches each one as a child process, and it connects back over TCP, usually on loopback, but plugins can also run on other machines. They exchange length-prefixed protobuf messages. That way flwrs can support a varied ecosystem of plugins: anything that speaks the [plugin protocol](#plugin-protocol) works, whether it's Rust, Python, Go, C++ or something else. There are three types:
  - **Source:** emits events, such as a trigger or an input.
  - **Transform:** receives events and emits changed events.
  - **Sink:** consumes events, such as sending an HTTP request.
- Each plugin declares a **schema** for the payloads it accepts and/or produces: typed fields such as string, number, bool, datetime, bytes, array, map and object.
- **Scenes** are stored workflow definitions, managed through the server's REST API.

## Repository layout

| Folder | Crate | Purpose |
|---|---|---|
| `core/` | `flwrs-core` | Shared library: config, logging, SQLite + migrations, HTTP server, service registry. |
| `app/` | `flwrs-app` | The flwrs server. |
| `plugin/` | `flwrs-plugin` | Plugin protocol definition (`schema/*.proto`) and the reference Rust SDK: wire framing, `Source`/`Transform`/`Sink` traits and runners. |
| `plugins/` | — | Base plugin library. Currently `http_sink` (Rust). Plugins in other languages can live here too. |
| `examples/plugins/` | — | Minimal, unsupported demo plugins (HTTP sinks in Python, Go, TypeScript and Java). |
| `xtask/` | — | Project automation: `cargo xtask package-plugin <dir>`. |
| `ideas/` | — | Design notes for possible future work. |

See [AGENTS.md](AGENTS.md) for a file-by-file map.

## Building

Requirements: Rust 1.94+ (edition 2024), and a C compiler for the target (for the bundled SQLite, TLS and Lua sources). `protoc` is bundled through a build dependency, so you don't need to install it.

The repo is a Cargo workspace. Run commands from the root:

```sh
cargo build --workspace            # everything
cargo build -p flwrs-app           # server only
cargo build -p flwrs-http-sink     # HTTP sink plugin only
cargo test --workspace             # all tests
```

Binaries end up in `target/debug/` (or `target/release/`).

**Optional features** of the server, all on by default: `mapping-lua` (Lua edge mappings), `otel` (exporting stats to OpenTelemetry) and `telemetry-file` (writing stats to local files). `--no-default-features` builds without any of them, for small edge devices; add back what you need, e.g. `--no-default-features --features telemetry-file`.

**Lua mappings** (the `mapping-lua` feature) are built in by default. For a target without a working C toolchain, or with very little flash, build the server without them: `cargo build -p flwrs-app --no-default-features --features otel,telemetry-file` (or with no features at all). Such a server can still import scenes that use Lua, but won't run them (see *Lua*, below).

## Running the server

Create a `config.toml` (pass a different path with `--config`). Every section is optional: `[http]` defaults to `0.0.0.0:80`, `[logging]` to console output at INFO, `[db.main]` to `main.db`, `[hub]` (where plugins connect) to `127.0.0.1` on a free port, `[director]` to a hop limit of 32, `[plugins]` to `./plugins` with uploads off, and `[telemetry]` to no export at all.

```toml
[http]
host = "0.0.0.0"
port = 8080

[logging]
format = "PLAIN"
level = "INFO"

[[logging.sinks]]
sink_type = "CONSOLE"

[db.main]
filename = "main.db"

[hub]
host = "127.0.0.1"   # a reachable address for remote plugins
port = 7400          # 0 = pick a free port
# max_inflight_bytes = 536870912   # all queued events, across scenes

[director]
# max_hops = 32      # an event is dropped after this many handovers (stops loops); scenes can override it
# spool_dir = "./scenes"     # spill-to-disk queue files
# spill_default = false      # spill for nodes that don't say
```

```sh
cargo run -p flwrs-app -- --config config.toml
```

**For development**, use the checked-in `config.dev.toml` from the repo root:

```sh
cargo run -p flwrs-app -- --config config.dev.toml   # serves on 127.0.0.1:8080
```

It keeps everything the server writes at run time (database, installed plugins, spool files, telemetry files) under `./.data/`, which is gitignored, so runtime data never mixes with the source tree.

- REST API: `http://localhost:8080/api/scenes`
- Swagger UI: `http://localhost:8080/swagger-ui`

## Plugin protocol

The protocol is the contract between the server and a plugin, so plugins can be written in any language. The Rust SDK in `plugin/` is one implementation of it.

### Process and connection

A node runs its plugin in one of two ways (the node's `config.launch`):

- **Local** (default): the server starts the plugin executable as a child process, in the plugin's install folder, with `--id <instance id> --host <hub host> --port <hub port> --log-level <trace|debug|info|warn|error>`, followed by the node's `launch_args`. A fresh token for this launch is in the `FLWRS_TOKEN` environment variable.
- **Remote:** an operator starts the plugin on another machine, with the same arguments pointing at the hub's reachable address, and the node's pre-shared token in `FLWRS_TOKEN` (or `--token`; the environment is preferred, since arguments show up in `ps`). Until TLS is supported, run remote links over a trusted network or a tunnel.

The plugin opens one TCP connection to `host:port`. It carries frames in both directions until shutdown.

**Supervision of local plugins** (timings are per node in `config.runtime`, with defaults from the manifest's `runtime_defaults`, then built-in):

- The plugin must be accepted (`Initialize` → `InitializeResponse`) within `init_timeout_ms` (10 s) of being started. Otherwise it's killed (SIGTERM, `kill_grace_ms` = 2 s, SIGKILL) and relaunched after `restart_delay_ms` (5 s), with a new token. `restart_backoff` makes the delay grow exponentially (off by default). After `max_restarts` consecutive launches that never got accepted (unlimited by default), the node is `Failed`.
- A plugin rejected for its type, version or schemas isn't relaunched: the same executable would fail the same way.
- If it disconnects but keeps running, it has `reconnect_timeout_ms` (60 s) to come back. If its process exits, or it sends an `Exit` nobody asked for, it's relaunched.
- To stop it, the hub sends `Shutdown` and waits `shutdown_grace_ms` (10 s) for the process to exit. Then it asks the OS to stop the plugin and every process the plugin started, waits `kill_grace_ms`, and kills them:
  - **Linux, macOS and other Unixes:** each plugin runs in its own process group; SIGTERM, then SIGKILL, go to the whole group.
  - **Windows:** each plugin runs in its own console process group and in a Job Object; a Ctrl-Break event, then terminating the Job, which also takes the plugin's child processes down if the server itself exits.

  A plugin should therefore exit on `Shutdown`, and treat SIGTERM (Unix) or Ctrl-Break (Windows) as a last request to stop.
- Whatever it writes to stdout and stderr ends up in the server log, tagged with scene and node.

### Framing

Every message is one frame. All integers are unsigned 32-bit little-endian:

```
+-------------+-------------+---------------------------+------------------------+
| packet_len  | header_len  | header (header_len bytes) | protobuf message bytes |
|   u32 LE    |   u32 LE    | "Name: value" lines       |                        |
+-------------+-------------+---------------------------+------------------------+
              |<----------------------- packet_len bytes ----------------------->|
```

- `packet_len` counts everything after itself: `4 + header_len + message length`. The reference SDK rejects frames larger than 256 MiB. The hub skips (and logs) messages larger than its per-event limit (`[hub] max_event_bytes`, 16 MiB by default, lowered per node by `runtime.queue.max_event_bytes`), and closes a connection whose first frame is over 1 MiB (`max_handshake_bytes`) or that sends no `Initialize` within 10 s (`handshake_timeout_ms`).
- The header is HTTP-style lines, for example:

  ```
  Version: 1.0.0\r\n
  Msg-Code: 3\r\n
  ```

  - ASCII only, at most 4 KiB. Each line is `Name: value` and ends with CRLF; readers also accept a bare LF. The last line must end with a line break too. There's no blank line at the end: `header_len` says where the header ends.
  - Names are case-insensitive, and whitespace around values is trimmed. A duplicate name is an error. **Unknown names are ignored**, so headers can be added later.
  - Required: `Version` (currently `1.0.0`) and `Msg-Code`. A missing or unknown value, a duplicate, a malformed line or a non-ASCII byte is a frame error, and the connection is closed.
- `Msg-Code` says which message type the body holds (`schema.common.MsgCode`):

  | Code | Message | Direction |
  |---|---|---|
  | 1 | `schema.source.SourceMessage` | source → runtime |
  | 2 | `schema.source.RuntimeSourceMessage` | runtime → source |
  | 3 | `schema.sink.SinkMessage` | sink → runtime |
  | 4 | `schema.sink.RuntimeSinkMessage` | runtime → sink |
  | 5 | `schema.transform.TransformMessage` | transform → runtime |
  | 6 | `schema.transform.RuntimeTransformMessage` | runtime → transform |

  `0` (`UNDEFINED`) is never valid. A plugin sends only its own type's plugin → runtime code, and receives only the matching runtime → plugin code. Anything else is a protocol violation: the receiver closes the connection.

### Messages

The schemas are in [`plugin/schema/`](plugin/schema). Generate code for your language with `protoc` (or `buf`). Each message type has a `oneof payload` that says what it carries.

- **Data messages** travel along the scene's edges: `SourceEvent` (from sources), `TransformEvent` (into and out of transforms) and `SinkEvent` (into sinks). Each carries a `PluginPayload` and `metadata` (`common.EventMetadata`: event id, trace id, hop count, origin node, create time).
- **System messages** are between one plugin and the hub, and are never routed: `Initialize` / `InitializeResponse`, `LogEvent`, `ErrorEvent`, `Shutdown` and `Exit`.

### Lifecycle

1. **Initialize.** The first frame on every connection is the plugin's `Initialize`: its instance id (`--id`), its version, its `token`, and its payload schemas (`schema` for sources and sinks, `in_schema` / `out_schema` for transforms). The hub checks the id, token, type, version and schemas against the node and the installed plugin.
   - Accepted: the hub answers `InitializeResponse`. If the node already had a connection, the new one replaces it (the old one is closed).
   - Rejected (unknown id, wrong token, mismatch): the hub answers `Shutdown` and closes. The plugin must honour `Shutdown` at any time, including before `InitializeResponse`, and must not reconnect after it.
2. **Run.** After `Initialize`, the plugin may send `LogEvent`s (`log`) and `ErrorEvent`s (`error`). **Data events may be sent only after `InitializeResponse`.**
   - Sources send `SourceEvent`s whenever they have data. Leave `metadata` unset; the hub fills it in.
   - Sinks receive `SinkEvent`s.
   - Transforms receive a `TransformEvent` and answer with a `TransformEvent` (and may send extra ones). **A transform copies the input event's `metadata` onto every output event produced from it**, so traces and the hop limit survive the transform.
   - A failed event is reported as an `ErrorEvent`. The hub only logs it; the plugin keeps running.
   - **Sends may block.** When downstream is slower, the hub stops reading from the plugin and its TCP writes block. That's flow control, not an error.
3. **Shutdown.** The runtime sends `Shutdown`. The plugin cleans up, sends `common.Exit` with a code (`OK` / `INTERRUPTED` / `ERROR`) and exits. An `Exit` the hub didn't ask for means the plugin is stopping on its own, and a local plugin is relaunched.

**Reconnect rule.** If the connection drops without a `Shutdown`, the plugin keeps running and reconnects, with backoff (for example 1 s, doubling up to at most 30 s, with some jitter). On every new connection it re-sends the same `Initialize` (same token) and waits for `InitializeResponse` again before sending data. Work tied to the old connection is dropped: the Rust SDK cancels a source's `run` and restarts it after the reconnect, and data sent while disconnected fails. This lets plugins ride out hub restarts and network blips. For local plugins, the hub gives a reconnecting plugin 60 s by default before treating it as unhealthy.

### Compatibility

Every enum's zero value is `UNDEFINED`, so a field that was never set can't be mistaken for a real value. A `FieldValue` with no variant set means null. Payload field types are string, i32, u32, i64, u64, f32, f64, bool, datetime, bytes, array, map and object.

The `.proto` files evolve backward compatibly: fields, messages and enum values are only ever added, never renumbered or reused. New optional frame headers can be added freely; a new required header or a new `Msg-Code` comes with a new protocol `Version`, and the hub keeps accepting the old one.

## Scenes

A **scene** is a workflow: a directed graph whose **nodes** are plugin instances and whose **edges** are data handovers between them. Cycles are allowed. New scenes are **disabled** (dormant); enabling one hands it to the director, which launches its plugins and routes events between them. Changes to an enabled scene apply while it runs: only nodes whose plugin or config changed restart, added nodes start, removed nodes stop, and edges, mappings and the hop limit change without restarting anything. A changed remote node's plugin isn't sent `Shutdown`; the hub drops its connection and it reconnects to the updated node. Enabled scenes start again when the server starts.

| Request | Purpose |
|---|---|
| `GET/POST /api/scenes`, `GET/PUT/DELETE /api/scenes/by-id/{id}` | List, create, get, update (`name`, `max_hops`), delete scenes (deleting removes nodes and edges). |
| `POST /api/scenes/by-id/{id}/enable`, `…/disable` | Run or stop the workflow. Enabling requires every node's plugin to be installed. |
| `GET/POST /api/scenes/by-id/{id}/nodes`, `GET/PUT/DELETE …/nodes/by-id/{node_id}` | Manage nodes. A node runs a registered plugin; its `type` is the plugin's type. |
| `GET/POST /api/scenes/by-id/{id}/edges`, `GET/PUT/DELETE …/edges/by-id/{edge_id}` | Manage edges. A SINK can't send and a SOURCE can't receive; at most one edge per direction between two nodes. |
| `GET /api/scenes/by-id/{id}/export`, `POST /api/scenes/import` | Move a scene to another server as a `.flwr` file (see below). |
| `GET /api/scenes/by-id/{id}/check` | Whether the scene can run here: `launchable`, `problems` (block enabling), `warnings`. |
| `GET /api/scenes/by-id/{id}/status` | What the scene is doing right now (see *Status*, below). |

Invalid requests return **422** with every problem listed in `details`.

**Hop limit:** every handover of an event between nodes counts as a hop, and an event is dropped (and counted) once it exceeds the limit. That stops events from circling forever in a cycle. The limit is `[director] max_hops` (32) unless the scene sets its own `max_hops` (1 to 10 000), for complex scenes with long chains or deliberate loops:

```json
{ "name": "enrichment pipeline", "max_hops": 200 }
```

**Node config** (all optional):

```json
{
  "launch": "local",
  "launch_args": { "http_timeout_seconds": 10 },
  "runtime": { "init_timeout_ms": 10000, "restart_delay_ms": 5000, "queue": { "when_full": "drop_oldest" } }
}
```

- **`launch`:** `local` (the hub starts the plugin) or `remote` (the plugin runs elsewhere and connects with `token`, a pre-shared secret of at least 16 characters).
- **`remote_id`** (remote nodes): the id the remote plugin is started with (`--id`). Defaults to the node id, but unlike the node id it stays the same when the scene is exported and imported elsewhere. Several nodes may share one; their tokens tell them apart.
- **`token`:** never returned by the API (`has_token` instead). Omitting it on update keeps the stored one.
- **`launch_args`:** checked against the plugin's launch-args schema and argument rules. Local nodes only.
- **`runtime`:** timeouts, restarts and the event queue. Unknown keys are rejected. A node on a cycle can't use `"when_full": "block"`, because blocking can deadlock there.

**Event queues.** Every node that receives events has an inbound queue. By default it holds up to 1 024 events or 64 MiB in memory; when it's full, `when_full` decides: `block` (the default: the producer waits, up to `block_timeout_ms`), `drop_newest` or `drop_oldest`. All queues share a hub-wide budget (`[hub] max_inflight_bytes`, 512 MiB).

With **spill to disk** (`"queue": {"spill": {"enabled": true}}`, or `[director] spill_default = true`), a queue keeps up to `memory_max_events` / `memory_max_bytes` (256 / 8 MiB) in memory, and puts the rest in a per-node SQLite file under `[director] spool_dir`, up to `disk_max_events` / `disk_max_bytes` (1 000 000 / 1 GiB). The order is kept exactly. Spool files are deleted when their node or scene stops (a node that restarts on a reload starts with an empty queue), so delivery stays at-most-once. A free-space guard (`spill_min_free_bytes`, 1 GiB) keeps a burst from filling the disk. Spill is for bursts: on SD cards and cheap flash, a permanently slow consumer wears the storage out.

**Event size:** an event larger than a node's own limits is still accepted on its own (into an empty queue, or onto an empty disk), since it only uses the shared budget for a while. Only an event larger than the hub-wide limit (`max_inflight_bytes`) is dropped, with a WARN.

**Edge mapping** (optional; `null` passes events through unchanged):

```json
{ "mode": "merge", "rules": [ { "target": "url", "source": "endpoint" }, { "target": "method", "value": "POST" } ] }
```

Rules run in order. Each sets its `target` path from exactly one of:

- `source`: a path in the **input** event (rules never read each other's output). If the input doesn't have it, the rule does nothing;
- `value`: a constant, converted to the target field's type (for a `BYTES` field a string becomes its UTF-8 bytes; for `DATETIME` a string is read as RFC 3339);
- `delete: true`: remove the target;
- `lua`: Lua code that `return`s the value (see *Lua*, below). Returning `nil` removes the target, like `delete`.

`mode` is `merge` (the default: start from the input event) or `replace` (start empty, so only the rules' targets are sent). **Paths** are dot notation: `a.b.c`, indexes `readings[0].value`, and quoted keys for unusual names `headers["content-type"]`. Missing levels are created when a target is set.

The output is then shaped to the target node's input schema. Conversions that lose nothing happen on their own (I32 → I64, a whole F64 → I32 that fits, OBJECT ↔ MAP). Fields the target doesn't declare are dropped. A value that can't be converted is a **mapping error**: the event isn't sent along that edge, and it's logged (rate-limited) and counted per node.

**Checked when saved:** paths must exist in the source node's output schema and the target node's input schema, constants must fit their target field, and a copy can't go between incompatible types (e.g. I32 into STRING). Problems come back as 422, all at once, and Lua code is compiled to check its syntax.

**Lua** (5.4). Beyond single `lua` rules, a mapping can have:

```json
{
  "filter": { "lua": "return payload.temperature > 30" },
  "script": { "lua": "return { url = 'https://api.example/' .. payload.device, alert = payload.temperature > 40 }" },
  "lua": { "max_instructions": 1000000, "timeout_ms": 50, "max_memory_bytes": 16777216 }
}
```

- `filter`: if it returns `false` or `nil`, the event isn't sent along this edge (Node-RED-style switch routing).
- `script` (instead of `rules`): returns the whole output; `return nil` drops the event on this edge.
- Code sees `payload` (the input event) and `meta` (`event_id`, `trace_id`, `hops`, `origin_node_id`, `create_time`), both read-only. Results are built as new tables.
- **Sandbox:** only `string`, `table`, `math` and `utf8` are available. No files, processes, environment, network or loading other code (`io`, `os`, `require`, `load`, `dofile` and `debug` aren't there). Globals can't be assigned, so nothing carries over between events. Each edge has its own Lua state.
- **Limits per call** (`lua`, optional; defaults shown above): instructions, wall-clock time, memory. A script that breaks one, or fails, is a mapping error: the event is skipped on that edge, logged and counted.
- **Values:** strings and bytes are Lua strings; integers are exact (a U64 above `i64::MAX` is an error); floats are numbers; a DATETIME is epoch milliseconds (UTC); arrays are 1-based tables; maps and objects are tables; null is `nil`. Results are converted back by the target's schema, so `{}` becomes an empty array or map as the target field says, and a string becomes BYTES for a BYTES field.
- **A server built without Lua** refuses Lua mappings when they're saved. Scenes with Lua can still be **imported** there, but they can't be enabled: their Lua mappings are listed as problems.

### Status

- **`GET /api/scenes/by-id/{id}/status`**: the scene's `state` (`RUNNING`, `STOPPED`, or `FAILED` with `errors`), its hop limit, and per node:
  - the plugin's state (`LAUNCHING`, `WAITING`, `RUNNING`, `DISCONNECTED`, `UNHEALTHY`, `STOPPING`, `STOPPED`, `FAILED`), its pid, launches and last error;
  - `counters`: `emitted` (events the plugin produced), `received` (events delivered to it), `dropped`, `mapping_errors`, `restarts`;
  - the inbound `queue`: depth in events and bytes, drops by reason, time producers spent blocked (`blocked_ms`), and spill details (on disk, written, refilled, whether the free-space guard is holding writes).
- **`GET /api/status`**: the server at a glance: every scene the director has handled since it started (state, node counts), the hub's address, and memory and disk in use against their limits (the hub-wide queue budget, spill memory, spool files).

Counters start at zero when a node starts: when its scene starts, or when a reload restarts that node. Nodes a reload leaves running keep counting.

### Telemetry export

The same stats can be exported continuously. Sinks are configured in `[telemetry]`, and are all optional:

- **OpenTelemetry** (`[telemetry.otlp]`, the `otel` feature): exports to an OpenTelemetry collector or backend over **OTLP/HTTP (protobuf)**, every 60 s by default.

  ```toml
  [telemetry.otlp]
  endpoint = "http://collector:4318/v1/metrics"   # default: OTEL_EXPORTER_OTLP_(METRICS_)ENDPOINT, else localhost:4318
  headers = { authorization = "Bearer …" }       # values are never logged
  # interval_ms = 60000; timeout_ms = 10000; temporality = "cumulative"  # or "delta"
  # service_name = "flwrs"; service_instance_id = "plant-7"; resource = { "deployment.environment" = "edge" }
  ```

  Unset keys fall back to the standard `OTEL_*` environment variables (`OTEL_EXPORTER_OTLP_ENDPOINT`, `…_HEADERS`, `…_TIMEOUT`, `OTEL_METRIC_EXPORT_INTERVAL`, `OTEL_EXPORTER_OTLP_METRICS_TEMPORALITY_PREFERENCE`, `OTEL_SERVICE_NAME`, `OTEL_RESOURCE_ATTRIBUTES`). Metrics are named `flwrs.node.events.emitted`, `flwrs.node.queue.depth`, `flwrs.hub.queued_memory.usage` and so on, labelled `scene.id`, `node.id`, `plugin.id`, `plugin.version`; counters are monotonic sums, gauges are gauges. A collector that's unreachable only produces warnings; the server keeps running. gRPC isn't supported (use the collector's HTTP port, 4318).
- **Local files** (`[[telemetry.file]]`, the `telemetry-file` feature): stats written to a folder, one file per time partition (a day by default), in a format that common tools read. Add one entry per format you want; several formats can share a folder.

  ```toml
  [[telemetry.file]]
  format = "sqlite"            # otlp-json | sqlite | influx | openmetrics
  dir = "/var/lib/flwrs/telemetry"
  # interval_ms = 10000        # how often stats are collected
  # flush_interval_ms = 60000  # how often they're written (a crash loses at most this much)
  # partition = "1d"           # length of one file: 30m, 1h, 6h, 1d, 1w, …; aligned to UTC
  # max_files = 30             # files kept, the current one included (default: a month of daily files)
  # max_bytes = 1073741824     # total size of this entry's files (default 1 GiB)
  # max_age = "90d"            # optional: also delete files older than this
  # compress = true            # gzip closed files of the text formats

  [[telemetry.file]]
  format = "otlp-json"
  dir = "/var/lib/flwrs/telemetry"
  ```

  | `format` | Files | Read by |
  |---|---|---|
  | `otlp-json` (needs `otel` too) | `.jsonl` → `.jsonl.gz`: one OTLP `MetricsData` per line, the OpenTelemetry file exporter's format | The OpenTelemetry Collector's `otlpjsonfile` receiver (replays into any backend), DuckDB's `otlp` extension, jq |
  | `sqlite` | `.db`, the schema below | Grafana (SQLite data source), Datasette, DuckDB, any SQL client |
  | `influx` | `.lp` → `.lp.gz`: InfluxDB line protocol, a line per node per collection (`flwrs_node,scene_id=…,node_id=…,plugin_id=…,plugin_version=… events_received=42i,queue_depth=0i,… <ns>`) | Telegraf (`inputs.file` / `inputs.tail`), `influx write`, InfluxDB, VictoriaMetrics, QuestDB |
  | `openmetrics` | `.om.d/` (open) → `.om.gz`: OpenMetrics text, assembled when the partition closes (`flwrs_node_events_received_total{…} 42 1793923200.000`) | `promtool tsdb create-blocks-from openmetrics` (backfill into Prometheus), VictoriaMetrics |

  Files are named `flwrs-metrics.<UTC start, e.g. 20261106T000000Z>.<extension>`. The current partition is appended to at every flush (and after a restart within it); when it ends, it's closed (gzipped, or assembled for OpenMetrics; a partition left open by a stopped server is closed at the next start), and the oldest files are deleted while there are more than `max_files` or they take more than `max_bytes` together (or are older than `max_age`), so the disk can't fill up. `0` turns `max_files` or `max_bytes` off, e.g. on a large network share. The current file is never deleted.

  **SQLite schema** (version 1, in `meta`):

  ```sql
  meta    (key TEXT PRIMARY KEY, value TEXT)  -- schema_version, partition_start, partition_end (RFC 3339), writer
  series  (id INTEGER PRIMARY KEY, metric TEXT, type TEXT,  -- 'counter' | 'gauge'
           unit TEXT, description TEXT,
           scene_id TEXT, node_id TEXT, plugin_id TEXT, plugin_version TEXT)  -- '' where a label doesn't apply
  samples (time INTEGER,                    -- Unix time, seconds
           series_id INTEGER REFERENCES series (id), value REAL,
           PRIMARY KEY (series_id, time))
  metrics -- view: time, metric, type, unit, scene_id, node_id, plugin_id, plugin_version, value
  ```

  For example `SELECT datetime(time, 'unixepoch'), value FROM metrics WHERE metric = 'flwrs.node.queue.depth' AND node_id = '…' ORDER BY time`. Counters restart at zero when their scene restarts.

### Moving scenes between servers

A scene can be exported from one flwrs server and imported into another, ready to run there with the same configuration:

```sh
curl -o line3.flwr http://source:8080/api/scenes/by-id/<id>/export          # tokens left out
curl -o line3.flwr "http://source:8080/api/scenes/by-id/<id>/export?include_secrets=true"
curl -F file=@line3.flwr http://destination:8080/api/scenes/import
```

- **The file** (`.flwr`, `application/vnd.flwrs.scene`) holds the scene's name and hop limit, every node (type, plugin id and version, config) and every edge (with its mapping). Format: the ASCII magic `FLWR`, a u32 little-endian length, the format version (`1.0.0`), then a protobuf `SceneFile` ([`app/schema/scene_file.proto`](app/schema/scene_file.proto)).
- **Secrets:** remote nodes' tokens are left out unless you export with `include_secrets=true`. Treat such a file like a password. Without tokens, the imported remote nodes need one set before the scene can be enabled.
- **Ids:** an import always creates a new scene with new node and edge ids, so the same file can be imported twice. Remote nodes keep their `remote_id` (the export fills it in from the node id if it was never set), so remote plugins don't need to be restarted with new ids.
- **Mappings travel with the scene** (as JSON, Lua code included), and the destination checks them against **its** plugins and build: a mapping that names a field the destination's plugin version doesn't have, Lua code with a syntax error, or any Lua on a server built without Lua support is reported as a problem (blocking enabling), not an import error. Only an invalid mapping shape (e.g. a malformed path) makes the file itself invalid.
- **Imports always arrive disabled.** The file's own structure is checked strictly (422 with every problem). Plugins the destination doesn't have, or has with another type, don't block the import: they're reported as `problems` (in the import response and by `…/check`), and enabling is refused until they're fixed, e.g. by uploading the plugin. A different plugin version is only a warning.

## Plugin packages

Plugins are installed into the server as **packages**: a ZIP with `manifest.json` at its root, the executable, and any files the manifest references. Everything else (assets, models, libraries) is optional.

```
http-sink-0.0.1-macos-aarch64.zip
├── manifest.json
├── bin/flwrs-http-sink
├── schema/launch_args.json      JSON Schema for the node's launch_args (optional)
└── schema/in.json               payload schema(s), required by plugin type
```

```json
{
  "manifest_version": 1,
  "id": "http-sink",
  "name": "HTTP sink",
  "version": "0.0.1",
  "type": "SINK",
  "description": "Sends an HTTP request for each event.",
  "executable": "bin/flwrs-http-sink",
  "platforms": ["macos-aarch64"],
  "launch_args_schema": "schema/launch_args.json",
  "launch_args_style": "flags",
  "payload_schemas": { "in": "schema/in.json" }
}
```

| Field | Required | Meaning |
|---|---|---|
| `manifest_version` | yes | `1`. |
| `id` | yes | Unique plugin id, `^[a-z0-9][a-z0-9_-]{0,63}$`; also the install folder name. |
| `name`, `version` | yes | Display name, and a semantic version. The version is what the plugin must report in `Initialize`. |
| `type` | yes | `SOURCE`, `SINK` or `TRANSFORM`. |
| `description` | no | Free text. |
| `executable` | yes | Path inside the ZIP. A native binary, or a script with a shebang (`#!/usr/bin/env python3`). |
| `platforms` | no | `<os>-<arch>` targets (`linux-x86_64`, `linux-aarch64`, `macos-aarch64`, …). The upload fails on another platform. Omit it for scripts that run anywhere. |
| `launch_args_schema` | no | JSON Schema for a node's `launch_args`. External `$ref`s aren't resolved. |
| `launch_args_style` | no | How `launch_args` becomes arguments: `flags` (default: `{"http_timeout_seconds": 60, "verbose": true}` → `--http-timeout-seconds 60 --verbose`), `positional` (a list of strings, passed verbatim) or `json` (`--launch-args '<json>'`). The hub's own arguments (`--id`, `--host`, `--port`, `--log-level`, `--token`) can't be overridden. |
| `payload_schemas` | by type | `in` for SINK, `out` for SOURCE, both for TRANSFORM. Each is a `SchemaDefinition` in proto3 JSON, e.g. `{"fields": [{"key": "url", "type": "STRING"}]}`. The plugin must declare exactly these schemas in `Initialize`. |
| `runtime_defaults` | no | Object with defaults for the node's runtime settings (timeouts, restart delay, …). |

**Managing plugins** (REST, documented in Swagger UI):

| Request | Purpose |
|---|---|
| `POST /api/plugins` with multipart field `file` | Upload and register a package. Responds 201; 409 if the id exists; 422 with a list of every problem if the package is invalid. |
| `GET /api/plugins`, `GET /api/plugins/by-id/{id}` | List or get registered plugins. |
| `GET /api/plugins/by-id/{id}/launch-args-schema` | The plugin's launch-args JSON Schema. |
| `PUT /api/plugins/by-id/{id}` with multipart `file` | Upgrade. The id and type must stay the same; refused (409) if any scene node's `launch_args` would become invalid. |
| `DELETE /api/plugins/by-id/{id}` | Deregister and remove the files. Refused (409) while scene nodes use the plugin. |

```sh
cargo xtask package-plugin http_sink                     # → target/plugins/http-sink-0.0.1-<platform>.zip
curl -F file=@target/plugins/http-sink-0.0.1-macos-aarch64.zip http://localhost:8080/api/plugins
```

> **Security:** an uploaded plugin is code the server will run. Upload, upgrade and delete are **off by default** (`[plugins] allow_upload = true` turns them on). When they're on, callers need `Authorization: Bearer <[plugins] admin_token>`, or, if no token is configured, must connect from loopback. `config.dev.toml` enables uploads from loopback for development.

## Writing a plugin

**In Rust:** create a binary crate under `plugins/`, add it to `members` in the root `Cargo.toml`, depend on `flwrs-plugin`, implement `Sink`, `Source` or `Transform`, declare the plugin's schema, and run the matching runner (`SinkRunner`, `SourceRunner`, `TransformRunner`). The runner handles the connection (token from `FLWRS_TOKEN` or `ConnectionConfig::with_token`), reconnecting, logging, error reporting, metadata propagation and shutdown. A source emits events through the `LocalSink` handle it receives in `initialize`. [`plugins/http_sink`](plugins/http_sink) is a complete example, including its package files. Package it with `cargo xtask package-plugin <dir>`.

**In any other language:** build an executable that follows the [plugin protocol](#plugin-protocol). Generate message types from `plugin/schema/*.proto`, implement the framing and header lines (a few dozen lines in most languages), and follow the lifecycle and reconnect rule above. [`examples/plugins/`](examples/plugins) has minimal demo HTTP sinks in Python, Go, TypeScript (Node.js) and Java, each about 150 lines with a `build.sh` that produces an uploadable package.

## Roadmap

flwrs is in early development. The main things still missing, roughly in order:

1. **A set of base plugins.** Common sources, transforms and sinks (timers, MQTT, HTTP in and out, files, databases, scripting, ML inference, …), so typical workflows need no custom plugin. Today there's only the HTTP sink.
2. **Alternative storage back-ends**, such as PostgreSQL. Today scenes, nodes, edges and the plugin registry live in SQLite only (the `Database` enum in `flwrs-core` has a single variant); a shared database is also a step towards running several servers.
3. **A web UI** for building and running scenes visually (nodes, edges, mappings, status), instead of through the REST API.
4. **Running as a cluster.** The server currently runs in **single-node mode only**: one server runs every scene it has, and there's no swarming, no handover of scenes between servers, and no load balancing. Remote plugins can run on other machines, but they all connect to one hub. Running properly in a data-centre environment (in-cluster) needs scenes spread across servers, failover, and load balancing.

Smaller known gaps (no TLS for remote plugins yet, at-most-once delivery, no Windows CI) are listed in [AGENTS.md](AGENTS.md), *Current state / known gaps*.

## Contributing with AI agents

LLM agent instructions live in [AGENTS.md](AGENTS.md), not in tool-specific files such as `CLAUDE.md`.

## License

[Apache 2.0](LICENSE)
