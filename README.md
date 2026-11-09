# flwrs

<img src="flwrs-logo.svg" alt="flwrs logo" width="100" height="100">

A lightweight framework and server for running pluggable dynamic workflows, inspired by [Node-RED](https://nodered.org/).

With flwrs you build workflows (data flows) of any complexity from **pre-compiled plugins** and **configuration**, with little to no code. The same runtime is meant to run both in the data centre and at the edge, for anything from simple automation to complex pipelines, such as running ML/AI models as workflow steps.

> **Status:** early development; expect breaking changes. What works today:
>
> - the server core, the [plugin protocol](docs/protocol.md) (version 2) with its Rust SDK and a conformance suite, the plugin registry (package upload), and scene management over REST;
> - running scenes: the director launches and supervises plugins (local, or remote over TLS with tokens or client certificates), routes events through bounded queues (credit-based flow control, at-least-once delivery with acks, optionally durable on disk), applies edge mappings (dot notation and Lua), and applies edits to a running scene as a diff, restarting only what changed;
> - scene export / import between servers, status endpoints, and telemetry export (OpenTelemetry, or local files);
> - one base plugin (HTTP sink, Rust), plus minimal demo plugins in Python, Go, TypeScript and Java.
>
> See the [roadmap](#roadmap) for what's missing.
> 
> I'm using this project as a learning/test bed to experiment with Rust, edge and heavy-load event systems and various ML/AI models.

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

**Cargo features** of the server, all on by default: `mapping-lua` (Lua edge mappings), `otel` (OpenTelemetry export), `telemetry-file` (telemetry files) and `remote` (remote plugins: tokens, `remote_id`, the TLS listener and client certificates). For a small edge device, or a target without a working C toolchain, leave out what you don't need, down to a bare server that only runs local plugins: `cargo build -p flwrs-app --no-default-features`. A server built without a feature can still import scenes that use it (Lua mappings, remote nodes), but lists them as problems and won't run them.

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
host = "127.0.0.1"   # local plugins connect here (plain TCP)
port = 7400          # 0 = pick a free port
# max_inflight_bytes = 536870912   # all queued events, across scenes
# heartbeat_interval_ms = 10000    # PING every connection this often (0 = never)
# credit_window = 64               # events a producer with CREDITS may have in flight
# max_unacked = 1024               # upper bound for a consumer's window (default_max_unacked = 64)

[hub.tls]            # optional: a second listener, with TLS, for remote plugins (remote feature)
port = 7401
cert = "hub.pem"     # the hub's certificate chain and key (PEM)
key = "hub.key"
# client_ca = "clients-ca.pem"     # CA of the client certificates of mtls nodes
# host = "0.0.0.0"

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

The protocol is the contract between the server and a plugin, so plugins can be written in any language. **The specification is [`docs/protocol.md`](docs/protocol.md)**; the messages are in [`plugin/schema/`](plugin/schema). The Rust SDK in `plugin/` is one implementation of it. In short:

- The server starts a local plugin with `--id <instance id> --host <hub host> --port <hub port> --log-level <level>` (plus the node's launch arguments) and a per-launch token in `FLWRS_TOKEN`. A remote plugin is started by an operator, and authenticates with the node's pre-shared token or a client certificate. Every plugin opens one TCP connection to the hub, or a TLS connection to its TLS listener.
- Every message is a frame: `u32 LE length | u8 framing version (2) | u32 LE header length | FrameHeader | body`. The protobuf `FrameHeader` names the body's message (`MsgCode`), and carries the routing metadata of data events, so the hub forwards bodies without decoding them.
- The first frame is the plugin's `Initialize` (instance id, token, type, version, payload schemas, protocol versions, capabilities). The hub answers `InitializeResponse` or, to reject it, `Shutdown`. Then data events flow, `PING` is answered with `PONG`, and `Shutdown` is answered with `Exit`. A plugin that loses its connection reconnects with backoff (at most 30 s).
- Optional capabilities: **`ACKS`** (acknowledgements and redelivery: at-least-once delivery) and **`CREDITS`** (credit-based flow control for what a plugin produces). A plugin without them still works, with TCP backpressure and at-most-once delivery on its hops.

**Check a plugin** in any language with the conformance suite, which runs it against a fake hub:

```sh
cargo run -p flwrs-trellis -- path/to/unzipped-package   # --list shows the cases
```

## Scenes

A **scene** is a workflow: a directed graph whose **nodes** are plugin instances and whose **edges** are data handovers between them. Cycles are allowed. New scenes are **disabled** (dormant); enabling one hands it to the director, which launches its plugins and routes events between them. Changes to an enabled scene apply while it runs: only nodes whose plugin or config changed restart, added nodes start, removed nodes stop, and edges, mappings and the hop limit change without restarting anything. A changed remote node's plugin isn't sent `Shutdown`; the hub drops its connection and it reconnects to the updated node. Enabled scenes start again when the server starts.

| Request | Purpose |
|---|---|
| `GET/POST /api/scenes`, `GET/PUT/DELETE /api/scenes/by-id/{id}` | List, create, get, update (`name`, `max_hops`, `defaults`), delete scenes (deleting removes nodes, edges and queued events). |
| `POST /api/scenes/by-id/{id}/enable`, `…/disable` | Run or stop the workflow. Enabling requires every node's plugin to be installed. |
| `GET/POST /api/scenes/by-id/{id}/nodes`, `GET/PUT/DELETE …/nodes/by-id/{node_id}` | Manage nodes. A node runs a registered plugin; its `type` is the plugin's type. |
| `GET/POST /api/scenes/by-id/{id}/edges`, `GET/PUT/DELETE …/edges/by-id/{edge_id}` | Manage edges. A SINK can't send and a SOURCE can't receive; at most one edge per direction between two nodes. |
| `GET /api/scenes/by-id/{id}/export`, `POST /api/scenes/import` | Move a scene to another server as a `.flwr` file (see below). |
| `GET /api/scenes/by-id/{id}/check` | Whether the scene can run here: `launchable`, `problems` (block enabling), `warnings`. |
| `GET /api/scenes/by-id/{id}/status` | What the scene is doing right now (see *Status*, below). |
| `DELETE /api/scenes/by-id/{id}/spool`, `…/nodes/by-id/{node_id}/spool` | Start the queues of the scene's nodes over, or one node's: the lever for corrupted, overflowing or lagging spool files. Every queued event goes, including those handed to a plugin and not acknowledged yet (they aren't delivered again), and spool files are deleted (running queues start fresh ones). For a disabled scene, that's what its durable queues kept. Also brings back a durable queue whose file couldn't be opened. Returns `purged_events`. |

Invalid requests return **422** with every problem listed in `details`.

**Hop limit:** every handover of an event between nodes counts as a hop, and an event is dropped (and counted) once it exceeds the limit. That stops events from circling forever in a cycle. The limit is `[director] max_hops` (32) unless the scene sets its own `max_hops` (1 to 10 000), for complex scenes with long chains or deliberate loops:

```json
{ "name": "enrichment pipeline", "max_hops": 200, "defaults": { "durable": true } }
```

**Scene defaults** (`defaults`, all optional) apply to every node that doesn't set its own: `durable` (durable queues, below), `remote_auth` (`token` or `mtls`) and `require_tls` (remote nodes, below).

**Node config** (all optional):

```json
{
  "launch": "local",
  "launch_args": { "http_timeout_seconds": 10 },
  "runtime": { "init_timeout_ms": 10000, "restart_delay_ms": 5000, "queue": { "when_full": "drop_oldest" } }
}
```

- **`launch`:** `local` (the hub starts the plugin) or `remote` (the plugin runs elsewhere and connects to the hub).
- **`remote_id`** (remote nodes): the id the remote plugin is started with (`--id`). Defaults to the node id, but unlike the node id it stays the same when the scene is exported and imported elsewhere. Several nodes may share one; their credentials tell them apart.
- **`auth`** (remote nodes): `token` (a pre-shared secret of at least 16 characters, in `token`) or `mtls` (a client certificate on the TLS listener: one that chains to `[hub.tls] client_ca` and names the `remote_id` in its SAN or CN, or the one pinned in `tls.client_cert_sha256`). Default: the scene's `remote_auth`, else `token`. **`require_tls`** (token nodes): only accept the plugin over the TLS listener; default: the scene's, else true when the hub has a TLS listener. A remote plugin built on the Rust SDK takes its TLS settings from `FLWRS_TLS_CA`, `FLWRS_TLS_SERVER_NAME`, `FLWRS_TLS_CERT` and `FLWRS_TLS_KEY`.
- **`token`:** never returned by the API (`has_token` instead). Omitting it on update keeps the stored one.
- **`launch_args`:** checked against the plugin's launch-args schema and argument rules. Local nodes only.
- **`runtime`:** timeouts, restarts, the event queue and redelivery (`delivery`: `max_attempts`, 5, and `retry_delay_ms`, 1 000). Unknown keys are rejected. A node on a cycle can't use `"when_full": "block"`, because blocking can deadlock there.

**Event queues.** Every node that receives events has an inbound queue. By default it holds up to 1 024 events or 64 MiB in memory; when it's full, `when_full` decides: `block` (the default: the producer waits, up to `block_timeout_ms`), `drop_newest` (the incoming event is dropped) or `drop_oldest` (as many of the oldest queued events as needed are evicted to fit it). `max_evictions` caps how many one incoming event may evict: if fitting it would take more, nothing is evicted and the incoming event is dropped instead, so one unexpectedly large event can't empty the queue (no limit by default). All queues share a hub-wide budget (`[hub] max_inflight_bytes`, 512 MiB); when it's used up, a `drop_oldest` queue evicts its own oldest events to make room (within `max_evictions`), never another queue's.

With **spill to disk** (`"queue": {"spill": {"enabled": true}}`, or `[director] spill_default = true`), a queue keeps up to `memory_max_events` / `memory_max_bytes` (256 / 8 MiB) in memory, and puts the rest in a per-node SQLite file under `[director] spool_dir`, up to `disk_max_events` / `disk_max_bytes` (1 000 000 / 1 GiB). The order is kept exactly. A spilling queue's file is deleted when its node or scene stops. A free-space guard (`spill_min_free_bytes`, 1 GiB) keeps a burst from filling the disk. Spill is for bursts: on SD cards and cheap flash, a permanently slow consumer wears the storage out.

**Delivery.** For a plugin that offers `ACKS` (the Rust SDK does), the hub keeps every event it delivered until the plugin acks it, at most the plugin's window at a time. It delivers an event again, with the same `event_id` and a higher attempt number, after a disconnect, a crash, or a NACK with retry (after `retry_delay_ms`). After `max_attempts` deliveries it's dead-lettered (dropped, logged and counted). So delivery is at-least-once while the server runs: consumers may see an event twice. A changed node keeps its queue across a reload, unless its queue kind changes (spill or durable switched on or off).

**Durable queues** (`"queue": {"durable": true}`, or the scene's `defaults.durable`): every event is committed to the node's spool file before its producer is told it's taken, and stays there until the consumer acks it, so it survives a server restart or a power loss. Limits are the spill ones (`spill.*`): memory is a cache of the oldest events, and the disk limits make the queue full. When a durable node's scene is disabled, its file stays and is delivered when the scene runs again (or dropped with `DELETE …/spool`). Files of deleted scenes and removed nodes are deleted, also at start-up.

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
  - the inbound `queue`: depth in events and bytes, drops by reason, time producers spent blocked (`blocked_ms`), delivery (`in_flight`, `redelivered`, `nacked`, `dead_lettered`), and disk details (`spill`, `durable`, on disk, written, refilled, whether the free-space guard is holding writes).
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

- **The file** (`.flwr`, `application/vnd.flwrs.scene`) holds the scene's name, hop limit and defaults, every node (type, plugin id and version, config) and every edge (with its mapping). Format: the ASCII magic `FLWR`, a u32 little-endian length, the format version (`1.0.0`), then a protobuf `SceneFile` ([`app/schema/scene_file.proto`](app/schema/scene_file.proto)).
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

**In Rust:** create a binary crate under `plugins/`, add it to `members` in the root `Cargo.toml`, depend on `flwrs-plugin`, implement `Sink`, `Source` or `Transform`, declare the plugin's schema, and run the matching runner (`SinkRunner`, `SourceRunner`, `TransformRunner`). The runner handles the connection (token from `FLWRS_TOKEN` or `ConnectionConfig::with_token`; TLS with the `tls` feature), the handshake, heartbeats, credit, acks, reconnecting, logging and shutdown.

- A sink's `consume_event` gets an `InboundEvent` (payload, metadata, attempt); `Ok` acks it, an error NACKs it (`SinkError::retryable` to have it delivered again).
- A transform's `process_event` returns any number of outputs, sent tied to their input (so the trace continues), then the input is acked. A stateful transform returns none until it's ready, and can emit later through its `LocalSink`: `emit` starts a new trace, `emit_linked` continues an earlier input's.
- A source emits through the `LocalSink` it gets in `initialize`: `emit` (waits for credit), or `emit_confirmed` (also waits until the hub has stored the event).

[`plugins/http_sink`](plugins/http_sink) is a complete example, including its package files. Package it with `cargo xtask package-plugin <dir>`.

**In any other language:** build an executable that follows the [plugin protocol](docs/protocol.md). Generate message types from `plugin/schema/*.proto`, implement the framing (a few dozen lines in most languages), and follow the lifecycle and reconnect rule; section 16 of the spec lists the minimum. [`examples/plugins/`](examples/plugins) has minimal demo HTTP sinks in Python, Go, TypeScript (Node.js) and Java, each with a `build.sh` that produces an uploadable package. Check yours with the conformance suite (`cargo run -p flwrs-trellis -- <package folder>`).

## Roadmap

flwrs is in early development. The main things still missing, roughly in order:

1. **A set of base plugins.** Common sources, transforms and sinks (timers, MQTT, HTTP in and out, files, databases, scripting, ML inference, …), so typical workflows need no custom plugin. Today there's only the HTTP sink.
2. **Alternative storage back-ends**, such as PostgreSQL. Today scenes, nodes, edges and the plugin registry live in SQLite only (the `Database` enum in `flwrs-core` has a single variant); a shared database is also a step towards running several servers.
3. **A web UI** for building and running scenes visually (nodes, edges, mappings, status), instead of through the REST API.
4. **Running as a cluster.** The server currently runs in **single-node mode only**: one server runs every scene it has, and there's no swarming, no handover of scenes between servers, and no load balancing. Remote plugins can run on other machines, but they all connect to one hub. Running properly in a data-centre environment (in-cluster) needs scenes spread across servers, failover, and load balancing.

Smaller known gaps (no Windows CI, durable delivery only within one server) are listed in [AGENTS.md](AGENTS.md), *Current state / known gaps*.

## Contributing with AI agents

LLM agent instructions live in [AGENTS.md](AGENTS.md), not in tool-specific files such as `CLAUDE.md`.

## License

[Apache 2.0](LICENSE)
