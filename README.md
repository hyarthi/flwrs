# flwrs

<img src="flwrs-logo.svg" alt="flwrs logo" width="100" height="100">

A lightweight framework and server for running pluggable dynamic workflows, inspired by [Node-RED](https://nodered.org/).

With flwrs you build workflows (data flows) of any complexity from **pre-compiled plugins** and **configuration**, with little to no code. The same runtime is meant to run both in the data centre and at the edge, for anything from simple automation to complex pipelines, such as running ML/AI models as workflow steps.

> **Status:** early development; expect breaking changes. What works today:
>
> - the server core, the [plugin protocol](docs/protocol.md) (version 2) with its Rust SDK ([`flwrs-plugin`](https://crates.io/crates/flwrs-plugin)) and a plugin developer toolkit ([`flwrs-trellis`](https://crates.io/crates/flwrs-trellis): package, audit, test, bench), and the plugin registry (package upload);
> - a **web UI** served by the server: a visual scene editor (drafts with undo, review and submit; node settings generated from each plugin's schema; an edge mapping editor with a live test), the live scenes list, plugin pages, an overview with live charts, and the server's own configuration; everything it does is also in the REST API;
> - running scenes: the director launches and supervises plugins (local, or remote over TLS with tokens or client certificates), routes events through bounded queues (credit-based flow control, at-least-once delivery with acks, optionally durable on disk), applies edge mappings (dot notation and Lua), and applies edits to a running scene as a diff, restarting only what changed;
> - scene export / import between servers, status endpoints, and telemetry export (OpenTelemetry, or local files);
> - eleven base plugins in Rust (HTTP, WebSocket and MQTT in and out, push to browsers, a ticker, Lua scripting, windowed aggregation), plus minimal demo plugins in Python, Go, TypeScript and Java.
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
- **Scenes** are stored workflow definitions: graphs of plugin instances (nodes) and the handovers between them (edges), built in the web UI's editor or through the REST API.

## Repository layout

| Folder | Crate | Purpose |
|---|---|---|
| `core/` | `flwrs-core` | Shared library: config, logging, SQLite + migrations, HTTP server, service registry. |
| `app/` | `flwrs-app` | The flwrs server, and its web UI (`app/web/`: Svelte + Vite, built and embedded by the server's build). |
| `plugin/` | `flwrs-plugin` | Plugin protocol definition (`schema/*.proto`) and the reference Rust SDK: wire framing, `Source`/`Transform`/`Sink` traits and runners. |
| `types/` | `flwrs-types` | Types shared by the server and plugins: the typed value tree, dot paths and the Lua sandbox (unpublished). |
| `plugins/` | — | Base plugin library. Currently `http_sink`, `ticker`, `script_transform`, `accumulator`, `mqtt_sink`, `mqtt_source`, `ws_sink`, `http_source`, `ws_source`, `http_push_sink` and `ws_push_sink` (Rust), and `kit` (code they share: the HTTP server core, the push sinks' broadcast core). Plugins in other languages can live here too. |
| `e2e/` | `flwrs-e2e` | End-to-end tests: the real server and plugin packages built from the workspace, installed and wired through the REST API, with real clients (HTTP → MQTT → SSE). |
| `examples/plugins/` | — | Minimal, unsupported demo plugins (HTTP sinks in Python, Go, TypeScript and Java). |
| `trellis/` | `flwrs-trellis` | Developer toolkit for plugins (any language): `package`, `audit` (protocol conformance), `test`, `bench`. `cargo trellis <command>` in this repo. |
| `docs/` | — | Specifications for plugin authors: the [plugin protocol](docs/protocol.md) and the [plugin specification](docs/plugin-spec.md) (packages, manifests, launching). |
| `ideas/` | — | Design notes for possible future work. |

See [AGENTS.md](AGENTS.md) for a file-by-file map.

## Building

Requirements: Rust 1.94+ (edition 2024), and a C compiler for the target (for the bundled SQLite, TLS and Lua sources). `protoc` is bundled through a build dependency, so you don't need to install it. The server's web UI (the `web-ui` feature, in `full` and `edge`) also needs **Node.js 20.19+ and npm** at build time: the build runs `npm ci` (when `app/web/node_modules` is missing or older than the lockfile) and `npm run build`, and embeds the result (set `FLWRS_NPM` to use another npm). Barebones builds (`--no-default-features`) don't need Node.

The repo is a Cargo workspace. Run commands from the root:

```sh
cargo build --workspace            # everything
cargo build -p flwrs-app           # server only
cargo build -p flwrs-http-sink     # HTTP sink plugin only
cargo nextest run --workspace      # all tests, in parallel (cargo-nextest; settings in .config/nextest.toml)
cargo test --workspace --doc       # doctests (nextest doesn't run them)
cargo test --workspace             # all tests without nextest (one test binary after another)
```

Binaries end up in `target/debug/` (or `target/release/`). The tests run fastest with [cargo-nextest](https://nexte.st) (`cargo install cargo-nextest --locked`): each test is its own process, and all test binaries run at once.

**Build flavours** of the server, as cargo features: `full` (the default: everything), `edge` (for mid-range edge devices: `mapping-lua` for Lua edge mappings, `otel` for OpenTelemetry export, `telemetry-file` for telemetry files, `remote` for remote plugins with tokens, `remote_id`, the TLS listener and client certificates, and `web-ui` for the web UI; no Swagger UI), and barebones, `--no-default-features` (low-end devices: none of them, a server that only runs local plugins), e.g. `cargo build -p flwrs-app --no-default-features --features edge`. Features can be added one by one too (`--no-default-features --features telemetry-file`). A server built without a feature can still import scenes that use it (Lua mappings, remote nodes), but lists them as problems and won't run them.

### Building for a deployment

A release build of the server, and a package of each plugin to install into it:

```sh
cargo build --release -p flwrs-app                    # full flavour → target/release/flwrs-app (web UI embedded)
cargo trellis package plugins/http_sink --out dist/   # release build of a plugin → dist/http-sink-0.0.1-<platform>.zip
```

`--target <triple>` cross-builds either (e.g. `aarch64-unknown-linux-gnu` for a Raspberry Pi), and `--flavor edge` packages a plugin's edge flavour where it has one. Then put the server binary next to a `config.toml` (below) and start it; install the packages from the web UI's Plugins page (with `[plugins] allow_upload = true`) or with `curl` (*Plugin packages*, below).

## Running the server

Create a `config.toml` (pass a different path with `--config`). Every section is optional: `[http]` defaults to `0.0.0.0:80`, `[logging]` to console output at INFO, `[db.main]` to `main.db`, `[hub]` (where plugins connect) to `127.0.0.1` on a free port, `[director]` to a hop limit of 32, `[plugins]` to `./plugins` with uploads off, and `[telemetry]` to no export at all.

```toml
[http]
host = "0.0.0.0"
port = 8080
# enabled = true      # false: no management API and no web UI (scenes still run; e.g. an edge box once configured)
# swagger_ui = false  # opt-in: Swagger UI at /swagger-ui (builds with the swagger-ui feature); /api-docs/openapi.json is always served
# config_edit = false # let the web UI's server page edit this file (some keys stay locked; saving asks for the key again; only set here)

[server]             # restarts from the web UI's server page
# restart = "exec"     # exec: restart in place (Unix: same PID, so systemd / launchd keep tracking it); exit: stop with status 75 for a service manager
# confirm_timeout_s = 120  # after a restart with a changed config, confirm it on the page within this time or the previous config comes back

[http.auth]          # who may use the API and the web UI; without a key, loopback clients only
# secret_key = "…"    # or secret_key_file = "/run/secrets/flwrs-key", or FLWRS_SECRET_KEY; at least 16 bytes (openssl rand -hex 32)
# session_ttl = "7d"  # how long a login lasts
# allowed_origins = ["https://flwrs.example.com"]   # a proxy serving the UI under another name
# secure_cookie = true   # default: when a proxy says the request came over HTTPS (X-Forwarded-Proto)

[runtime]
# threads = 0        # cores the server uses: 1 = single-threaded, N = main + N - 1 workers; 0 / unset = one worker per core

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
# max_pending_connections = 256    # connections still in their handshake; more are closed at once (0 = no limit)
# max_pending_per_ip = 8           # the same per source address; loopback not counted (0 = no limit)

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
# nodes_dir = "./nodes"      # local nodes' folders (their working directories)
# files_dir = "./files"      # files uploaded to nodes
# plugin_threads = 0         # cores each local plugin may use, for nodes that don't set runtime.threads (0 = the plugins decide)

[scenes]
# max_upload_bytes = 1073741824   # one file uploaded to a node
# max_import_bytes = 1073741824   # one scene import (and what an archive unpacks to)
# stream_interval_ms = 1000       # how often the live list (GET /api/scenes/stream) sends status
```

```sh
cargo run -p flwrs-app -- --config config.toml
```

**For development**, use the checked-in `config.dev.toml` from the repo root:

```sh
cargo dev   # = cargo run --bin flwrs-app --target-dir target/dev -- --config config.dev.toml; serves on 127.0.0.1:8080
```

`cargo dev` builds into its own folder, `target/dev`: a plain build of the server enables other dependency features than `cargo test --workspace`, so sharing `target/debug` they'd rebuild a good part of the workspace after each other. It keeps everything the server writes at run time (database, installed plugins, node folders, uploaded files, spool files, telemetry files) under `./.data/`, which is gitignored, so runtime data never mixes with the source tree. To start over, stop the server and run `chmod -R u+w .data && rm -rf .data` (node folders' `config/` are read-only, so a plain `rm -rf` stops there).

- Web UI: `http://localhost:8080/` (builds with the `web-ui` feature): it opens on the overview; the scene editor has a help dialog (the ⓘ under the zoom buttons, or `?`)
- REST API: `http://localhost:8080/api/scenes`
- Swagger UI: `http://localhost:8080/swagger-ui` (opt-in: `config.dev.toml` sets `[http] swagger_ui = true`); the OpenAPI document: `http://localhost:8080/api-docs/openapi.json`

### Access to the API and the web UI

The management API and the web UI are behind `[http.auth]`:

- **With a secret key** (`secret_key`, `secret_key_file` or the `FLWRS_SECRET_KEY` environment variable; checked at start, at least 16 bytes): the web UI asks for it once and keeps a session cookie (`flwrs_session`: HttpOnly, `SameSite=Strict`, `session_ttl` long). The cookie holds signed and encrypted claims, never the key, and the server keeps no sessions, so changing the key logs everyone out. Scripts send the key on each request instead: `Authorization: Bearer <key>`. `POST /api/auth/login` (`{"key": "…"}`), `POST /api/auth/logout`, `GET /api/auth/session` (who's calling), `GET /api/auth/providers`. Failed logins are limited per client address (429). The key travels with every login or bearer request, so put TLS in front of the server (a reverse proxy) when it's reached over a network.
- **Without a key:** only loopback clients get in, and only when they name the server by a loopback address (`localhost`, `127.0.0.1`, `[::1]`), so a web page can't reach it through DNS rebinding. Note that behind a reverse proxy on the same machine every client looks like loopback: set a key there.
- Browsers' state-changing requests from another origin are refused (`Origin` must be the server itself or in `allowed_origins`), unless they carry the key as a bearer token.
- Unknown `/api/…` paths are JSON 404s (after auth). The OpenAPI document and Swagger UI are public, but their calls need auth too.

## Plugin protocol

The protocol is the contract between the server and a plugin, so plugins can be written in any language. **The specification is [`docs/protocol.md`](docs/protocol.md)**; the messages are in [`plugin/schema/`](plugin/schema). The Rust SDK in `plugin/` is one implementation of it. In short:

- The server starts a local plugin with `--id <instance id> --host <hub host> --port <hub port> --log-level <level>` (plus the node's launch arguments) and a per-launch token in `FLWRS_TOKEN`. A remote plugin is started by an operator, and authenticates with the node's pre-shared token or a client certificate. Every plugin opens one TCP connection to the hub, or a TLS connection to its TLS listener. Until its handshake is done, a connection counts against `[hub] max_pending_connections` (256) and `max_pending_per_ip` (8; loopback isn't counted); over either, the hub closes new connections at once, so a flood of connections that never authenticate can't use up the server's file descriptors or memory. A flood from many addresses still needs a firewall or proxy in front of the hub.
- Every message is a frame: `u32 LE length | u8 framing version (2) | u32 LE header length | FrameHeader | body`. The protobuf `FrameHeader` names the body's message (`MsgCode`), and carries the routing metadata of data events, so the hub forwards bodies without decoding them.
- The first frame is the plugin's `Initialize` (instance id, token, type, version, payload schemas, protocol versions, capabilities). The hub answers `InitializeResponse` or, to reject it, `Shutdown`. Then data events flow, `PING` is answered with `PONG`, and `Shutdown` is answered with `Exit`. A plugin that loses its connection reconnects with backoff (at most 30 s).
- Optional capabilities: **`ACKS`** (acknowledgements and redelivery: at-least-once delivery) and **`CREDITS`** (credit-based flow control for what a plugin produces). A plugin without them still works, with TCP backpressure and at-most-once delivery on its hops.

**Check a plugin** in any language with trellis, which runs it against a fake hub ([trellis/README.md](trellis/README.md)):

```sh
cargo trellis audit path/to/package.zip      # protocol conformance (--list shows the cases)
cargo trellis test path/to/plugin-folder     # behaviour tests (*.test.toml), or a smoke test
cargo trellis bench path/to/plugin-folder    # throughput, latency, CPU and memory
```

## Scenes

A **scene** is a workflow: a directed graph whose **nodes** are plugin instances and whose **edges** are data handovers between them. Cycles are allowed. New scenes are **disabled** (dormant); enabling one hands it to the director, which launches its plugins and routes events between them. Changes to an enabled scene apply while it runs: only nodes whose plugin or config changed restart, added nodes start, removed nodes stop, and edges, mappings and the hop limit change without restarting anything. A changed remote node's plugin isn't sent `Shutdown`; the hub drops its connection and it reconnects to the updated node. Enabled scenes start again when the server starts.

| Request | Purpose |
|---|---|
| `GET/POST /api/scenes`, `GET/PUT/DELETE /api/scenes/by-id/{id}` | List, create, get, update (`name`, `max_hops`, `defaults`), delete scenes (deleting removes nodes, edges and queued events). |
| `POST /api/scenes/by-id/{id}/enable`, `…/disable` | Run or stop the workflow. Enabling requires every node's plugin to be installed. |
| `GET/POST /api/scenes/by-id/{id}/nodes`, `GET/PUT/DELETE …/nodes/by-id/{node_id}` | Manage nodes. A node runs a registered plugin; its `type` is the plugin's type. |
| `GET/POST /api/scenes/by-id/{id}/edges`, `GET/PUT/DELETE …/edges/by-id/{edge_id}` | Manage edges. A SINK can't send and a SOURCE can't receive; at most one edge per direction between two nodes. |
| `GET /api/scenes/by-id/{id}/export`, `POST /api/scenes/import` | Move a scene to another server as a `.flwr` file or a `.far` archive with its nodes' files (see below). |
| `PUT/GET/DELETE …/nodes/by-id/{node_id}/uploads/{name}`, `GET …/uploads`, `GET …/uploads/{name}/content` | Files uploaded to a node, for settings that name one (a model, a certificate, a spec). Raw body, up to `[scenes] max_upload_bytes`. A running node restarts with the new file; a file a setting names can't be deleted, and one a secret setting names can't be downloaded. |
| `DELETE …/nodes/by-id/{node_id}/workdir` | Empty a node's folder (the plugin's state and caches), while the scene is disabled. |
| `GET /api/scenes/by-id/{id}/check` | Whether the scene can run here: `launchable`, `problems` (block enabling), `warnings`. |
| `GET /api/scenes/by-id/{id}/status` | What the scene is doing right now (see *Status*, below). |
| `GET /api/scenes/stream` | The scenes list, live (Server-Sent Events; the web UI's list): `scenes` (every scene with its node and edge counts and its check, sent first and after each change to scenes or plugins), `status` (each running scene's state and event counters, every `[scenes] stream_interval_ms`, 1 s by default). |
| `…/scenes/by-id/{id}/draft…` | The scene's **draft**: edit the graph apart from the live one, then submit it at once (see *Drafts*, below). |
| `POST /api/scenes/by-id/{id}/graph/batch` | Several changes to the live graph in one step, all or nothing (see *Drafts*, below). |
| `GET /api/scenes/by-id/{id}/stream`, `…/nodes/by-id/{node_id}/logs/stream`, `PATCH …/layout`, `POST …/mapping/test` | What the web UI's scene view uses: the scene, its draft, its check and its status, live (Server-Sent Events); a node's recent logs, live; node and edge positions on the canvas (layout only, never a change to the scene); an edge mapping tried on a sample event. |
| `DELETE /api/scenes/by-id/{id}/spool`, `…/nodes/by-id/{node_id}/spool` | Start the queues of the scene's nodes over, or one node's: the lever for corrupted, overflowing or lagging spool files. Every queued event goes, including those handed to a plugin and not acknowledged yet (they aren't delivered again), and spool files are deleted (running queues start fresh ones). For a disabled scene, that's what its durable queues kept. Also brings back a durable queue whose file couldn't be opened. Returns `purged_events`. |

Invalid requests return **422** with every problem listed in `details`.

Every scene has a **`revision`**: it counts committed changes to its live graph (settings, nodes, edges, uploads).

### Drafts and batches

Each change through the endpoints above is committed (and a running scene reloaded) at once. To restructure a scene without its intermediate states running, edit a **draft** and submit it, or send a **batch**.

**Drafts** (`/api/scenes/by-id/{id}/draft…`; one per scene, shared by every client):

- `POST …/draft` starts one from the live graph (any draft edit does too); `GET` shows it, `DELETE` discards it. Every node and edge in it carries its state against live: `unchanged`, `added`, `changed` (with the `parts` that differ: `name`, `plugin`, `config.<key>`, `uploads`), or `removed` (kept as a shadow, until `…/restore`). Node and edge ids are the live ones.
- Its graph is edited with the same requests as live, under `…/draft`: `PUT …/draft/settings`, `…/draft/nodes…`, `…/draft/edges…`, `…/draft/nodes/by-id/{node_id}/uploads/{name}`. A draft takes an **incomplete node** (one that doesn't validate against its plugin yet; its configurator runs once it does); structural errors (an unknown plugin, an unreadable config, a broken graph rule) are refused as live.
- `GET …/draft/diff` lists what would change; `GET …/draft/check` is the scene check on the draft.
- `POST …/draft/submit` (`{"base_revision": n, "disable": false}`, both optional) commits it in one transaction: one revision, one reload (only changed nodes restart), and the draft goes. An enabled scene takes a draft only without problems, unless `disable` disables it in the same step.
- If live changed since the draft started (direct edits, a batch), submit asks for a **rebase** first: `POST …/draft/rebase` merges per node, edge and the settings what changed on one side only, and marks what both sides changed as a **conflict**; `POST …/draft/resolve` (`{"target": "node", "id": "…", "keep": "draft" | "live"}`) settles each before submitting.

**Batches** (`POST /api/scenes/by-id/{id}/graph/batch`): a list of operations (`op`: `update_scene`, `create_node`, `update_node`, `delete_node`, `create_edge`, `update_edge`, `delete_edge`), each checked as its own endpoint checks it, in order. Created nodes and edges can take a `key`, and later operations name them `"$<key>"`. Everything is then written in one transaction, with one revision and one reload; nothing is written if an operation fails (its index is in the error: `ops[2] (create_edge): …`). `base_revision` makes it refuse (409) if the scene changed meanwhile.

```json
{ "base_revision": 7, "ops": [
  { "op": "create_node", "key": "parse", "plugin_id": "script-transform", "config": { "launch_args": { "script": "return payload" } } },
  { "op": "delete_edge", "id": "01J…" },
  { "op": "create_edge", "source_node_id": "01J…", "target_node_id": "$parse" }
] }
```

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
- **`launch_args`:** the node's settings, checked against the plugin's launch-args schema and delivery rules ([plugin spec](docs/plugin-spec.md#3-node-settings-the-launch-args-schema)); for code settings, the code's syntax too. **Secrets** (the schema's `writeOnly` properties) are never returned: responses list them in `launch_args_secrets` (`{"/api_key": true}`: set or not), an update that leaves one out keeps it, and `null` clears it. Settings that name an uploaded file need it uploaded (`…/uploads/{name}`) before the scene can be enabled. Local nodes; a remote node only carries them for its payload schemas, when its plugin takes those from its settings.
- **`runtime`:** timeouts, restarts, the event queue, redelivery (`delivery`: `max_attempts`, 5, and `retry_delay_ms`, 1 000), and `threads`: the cores a local plugin may use (1 = single-threaded; 0 = the plugin decides; default: the plugin's `runtime_defaults.threads`, else `[director] plugin_threads`), passed as its manifest's `threads` says ([plugin spec](docs/plugin-spec.md#55-the-core-count)). Unknown keys are rejected. A node on a cycle can't use `"when_full": "block"`, because blocking can deadlock there.

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

**JSON in bytes.** Bodies usually travel as BYTES (or STRING) holding JSON. A copy rule can decode or encode it:

```json
{ "mode": "replace", "rules": [ { "target": ".", "source": "body", "decode": "json" } ] }
{ "mode": "replace", "rules": [ { "target": "body", "source": ".", "encode": "json" }, { "target": "topic", "value": "readings" } ] }
```

- `"decode": "json"`: the source (BYTES or STRING) is parsed as JSON and shaped by the target field, like a constant (a string becomes BYTES as base64, a DATETIME is RFC 3339), except that object keys the target doesn't declare are dropped. Target `"."` makes the decoded object the whole output (with `mode: replace`).
- `"encode": "json"`: the source value becomes JSON text in a BYTES or STRING target. Source `"."` is the whole input event. Integers stay exact numbers (a JavaScript reader loses precision above 2^53), datetimes are RFC 3339 in UTC, bytes are base64, packed arrays are arrays.
- Text that isn't JSON, or JSON that doesn't fit the target, is a mapping error. At most 32 levels of nesting.

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
- Code sees `payload` (the input event) and `meta` (`event_id`, `trace_id`, `hops`, `origin_node_id`, `create_time`), both read-only. Results are built as new tables. `json.encode(value)` and `json.decode(text)` convert to and from JSON text (the same rules as the codec above; decoded tables are ordinary, writable ones).
- **Sandbox:** only `string`, `table`, `math`, `utf8` and `json` are available. No files, processes, environment, network or loading other code (`io`, `os`, `require`, `load`, `dofile` and `debug` aren't there). Globals can't be assigned, so nothing carries over between events. Each edge has its own Lua state.
- **Limits per call** (`lua`, optional; defaults shown above): instructions, wall-clock time, memory. A script that breaks one, or fails, is a mapping error: the event is skipped on that edge, logged and counted.
- **Values:** strings and bytes are Lua strings; integers are exact (a U64 above `i64::MAX` is an error); floats are numbers; a DATETIME is epoch milliseconds (UTC); arrays are 1-based tables; maps and objects are tables; null is `nil`. Results are converted back by the target's schema, so `{}` becomes an empty array or map as the target field says, and a string becomes BYTES for a BYTES field.
- **A server built without Lua** refuses Lua mappings when they're saved. Scenes with Lua can still be **imported** there, but they can't be enabled: their Lua mappings are listed as problems.

### Status

- **`GET /api/scenes/by-id/{id}/status`**: the scene's `state` (`RUNNING`, `STOPPED`, or `FAILED` with `errors`), its hop limit, and per node:
  - the plugin's state (`LAUNCHING`, `WAITING`, `RUNNING`, `DISCONNECTED`, `UNHEALTHY`, `STOPPING`, `STOPPED`, `FAILED`), its pid, launches and last error;
  - `counters`: `emitted` (events the plugin produced), `received` (events delivered to it), `dropped`, `mapping_errors`, `restarts`;
  - the inbound `queue`: depth in events and bytes, drops by reason, time producers spent blocked (`blocked_ms`), delivery (`in_flight`, `redelivered`, `nacked`, `dead_lettered`), and disk details (`spill`, `durable`, on disk, written, refilled, whether the free-space guard is holding writes);
  - the plugin's `link`, for spotting slow links and setup problems: `heartbeats` (`last_rtt_ms`, the latest round trip of the hub's `PING`; `answered`; `missed`, not answered within three heartbeat intervals; `unexpected`, `PONG`s with a nonce the hub didn't send) and `connections` (`accepted`; `rejected`, declarations that don't match the node; `replaced` by a newer connection, e.g. two plugins started with the same id; and lost ones by cause: `closed_by_plugin` without `Exit`, `timed_out`, `io_errors` such as resets, `protocol_violations`). Closes the hub asks for (stop, reload, token renewal) aren't counted.
- **`GET /api/status`**: the server at a glance: every scene the director has handled since it started (state, node counts, its nodes' `emitted` / `received` / `dropped` counters added up), the hub's address and `hub_connections` (open now, still in their handshake, closed at once for the pending limits, handshakes that failed before reaching a node: bad or late `Initialize`, TLS, unknown id, wrong credentials), and memory and disk in use against their limits (the hub-wide queue budget, spill memory, spool files).

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

  Unset keys fall back to the standard `OTEL_*` environment variables (`OTEL_EXPORTER_OTLP_ENDPOINT`, `…_HEADERS`, `…_TIMEOUT`, `OTEL_METRIC_EXPORT_INTERVAL`, `OTEL_EXPORTER_OTLP_METRICS_TEMPORALITY_PREFERENCE`, `OTEL_SERVICE_NAME`, `OTEL_RESOURCE_ATTRIBUTES`). Metrics are named `flwrs.node.events.emitted`, `flwrs.node.queue.depth`, `flwrs.node.heartbeat.rtt`, `flwrs.node.connections.lost.timed_out`, `flwrs.hub.connections.open`, `flwrs.hub.queued_memory.usage` and so on (the full list is `ALL` in `app/src/modules/telemetry/samples.rs`), labelled `scene.id`, `node.id`, `plugin.id`, `plugin.version`; counters are monotonic sums, gauges are gauges. A collector that's unreachable only produces warnings; the server keeps running. gRPC isn't supported (use the collector's HTTP port, 4318).
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
curl -o line3.far "http://source:8080/api/scenes/by-id/<id>/export?format=far"   # with the nodes' files
curl -F file=@line3.flwr http://destination:8080/api/scenes/import
```

- **The file** (`.flwr`, `application/vnd.flwrs.scene`) holds the scene's name, hop limit and defaults, every node (type, plugin id and version, config, the list of its uploaded files) and every edge (with its mapping). Format: the ASCII magic `FLWR`, a u32 little-endian length, the format version (`1.0.0`), then a protobuf `SceneFile` ([`app/schema/scene_file.proto`](app/schema/scene_file.proto)).
- **Secrets:** remote nodes' tokens and the plugins' secret settings are left out unless you export with `include_secrets=true`. Treat such a file like a password. Without them, the imported nodes need them set again (the import warns which) before the scene can be enabled.
- **Uploaded files:** `?format=far` exports a `.far` archive: a ZIP of the scene file and the nodes' uploaded files (a secret setting's file only with secrets). The import also takes a ZIP, tar or tar.gz with one `.flwr` at its root: files are matched by content (SHA-256) wherever they are in it. Uploads the import doesn't carry are reported, and the scene can't be enabled until they're uploaded. Limits: `[scenes] max_import_bytes` (the file, and what an archive unpacks to) and `max_import_entries`.
- **Ids:** an import always creates a new scene with new node and edge ids, so the same file can be imported twice. Remote nodes keep their `remote_id` (the export fills it in from the node id if it was never set), so remote plugins don't need to be restarted with new ids.
- **Mappings travel with the scene** (as JSON, Lua code included), and the destination checks them against **its** plugins and build: a mapping that names a field the destination's plugin version doesn't have, Lua code with a syntax error, or any Lua on a server built without Lua support is reported as a problem (blocking enabling), not an import error. Only an invalid mapping shape (e.g. a malformed path) makes the file itself invalid.
- **Imports always arrive disabled.** The file's own structure is checked strictly (422 with every problem). Plugins the destination doesn't have, or has with another type, don't block the import: they're reported as `problems` (in the import response and by `…/check`), and enabling is refused until they're fixed, e.g. by uploading the plugin. A different plugin version is only a warning.

## Plugin packages

Plugins are installed into the server as **packages**: a ZIP with `manifest.json` at its root, the executable, and the files the manifest references.

```json
{
  "manifest_version": 2,
  "id": "ticker",
  "name": "Ticker",
  "version": "0.0.1",
  "type": "SOURCE",
  "executable": "bin/flwrs-ticker",
  "launch_args_schema": "schema/launch_args.json",
  "payload_schemas": { "out": "schema/out.json" },
  "ui": { "icon": "ui/ticker.svg", "categories": ["Triggers"], "docs": "README.md" }
}
```

The manifest names the plugin's type and version, its payload schemas (fixed, or taken from each node's settings), and a JSON Schema of a node's settings (`launch_args`). That schema also says how each setting reaches the plugin (a command-line argument, an environment variable, or a file in the node's `config/` folder), which ones are secrets, which hold code, which have a **template** (a sample the editor fills an empty field with, e.g. an API document or a script), and how a UI should present them. A local plugin runs in a folder of its own per node, where it can keep state. **The format and the launch contract are specified in [`docs/plugin-spec.md`](docs/plugin-spec.md)**, for plugin authors in any language.

**Managing plugins** (REST, documented in Swagger UI):

| Request | Purpose |
|---|---|
| `POST /api/plugins` with multipart field `file` | Upload and register a package. Responds 201; 409 if the id exists; 422 with a list of every problem if the package is invalid. |
| `GET /api/plugins`, `GET /api/plugins/by-id/{id}` | List or get registered plugins (with `node_defaults`: what a node gets for each run-time setting it leaves out). |
| `GET /api/plugins/by-id/{id}/launch-args-schema` | The plugin's launch-args JSON Schema. |
| `GET …/by-id/{id}/ui/{icon\|docs}`, `…/definitions/{path}`, `…/templates/{path}` | Files the package names for editors: its icon and docs (its README), a code field's definitions (completion), a field's template. Only those files, sandboxed. |
| `POST /api/plugins/remote`, `GET/PUT …/by-id/{id}/definition`, `GET …/by-id/{id}/export` | **Remote plugin definitions**: a plugin that runs elsewhere, described here (type, payload schemas, settings) so its nodes can be placed, wired and checked; the web UI has a wizard for them. Need the admin token or loopback, not `allow_upload` (they run nothing here). |
| `PUT /api/plugins/by-id/{id}` with multipart `file` | Upgrade. The id and type must stay the same; refused (409) if any scene node's `launch_args` would become invalid. |
| `DELETE /api/plugins/by-id/{id}` | Deregister and remove the files. Refused (409) while scene nodes use the plugin. |

```sh
cargo trellis package plugins/http_sink                 # → target/plugins/http-sink-0.0.1-<platform>.zip
curl -F file=@target/plugins/http-sink-0.0.1-macos-aarch64.zip http://localhost:8080/api/plugins
```

> **Security:** an uploaded plugin is code the server will run. Upload, upgrade and delete are **off by default** (`[plugins] allow_upload = true` turns them on). When they're on, callers need, on top of the API's own auth, the admin token in `X-Flwrs-Admin-Token: <[plugins] admin_token>`, or, if no token is configured, must connect from loopback. `config.dev.toml` enables uploads from loopback for development.

## Writing a plugin

Plugins are packaged and run as described in the [plugin specification](docs/plugin-spec.md).

**In Rust:** create a binary crate under `plugins/`, add it to `members` in the root `Cargo.toml`, depend on `flwrs-plugin`, implement `Sink`, `Source` or `Transform`, declare the plugin's schema, parse the server's bootstrap arguments with `flwrs_plugin::cli::Bootstrap` (feature `cli`, flattened into your clap arguments), and run the matching runner (`SinkRunner`, `SourceRunner`, `TransformRunner`). The runner handles the connection (token from `FLWRS_TOKEN` or `ConnectionConfig::with_token`; TLS with the `tls` feature), the handshake, heartbeats, credit, acks, reconnecting, logging and shutdown.

- A sink's `consume_event` gets an `InboundEvent` (payload, metadata, attempt); `Ok` acks it, an error NACKs it (`SinkError::retryable` to have it delivered again).
- A transform's `process_event` returns any number of outputs, sent tied to their input (so the trace continues), then the input is acked. A stateful transform returns none until it's ready, and can emit later through its `LocalSink`: `emit` starts a new trace, `emit_linked` continues an earlier input's.
- A source emits through the `LocalSink` it gets in `initialize`: `emit` (waits for credit), or `emit_confirmed` (also waits until the hub has stored the event). `ready` waits for credit without sending, so a source can drop stale data instead of queueing it.

[`plugins/http_sink`](plugins/http_sink) (a sink) and [`plugins/ticker`](plugins/ticker) (a source) are complete examples, including its package files and its trellis tests (`tests/trellis.rs`, with trellis as a dev-dependency). Package it with `cargo trellis package plugins/http_sink` (`--target <triple>` to cross-build, e.g. for a Raspberry Pi).

**In any other language:** build an executable that follows the [plugin protocol](docs/protocol.md). Generate message types from `plugin/schema/*.proto`, implement the framing (a few dozen lines in most languages), and follow the lifecycle and reconnect rule; section 16 of the spec lists the minimum. [`examples/plugins/`](examples/plugins) has minimal demo HTTP sinks in Python, Go, TypeScript (Node.js) and Java, each with a `build.sh` that produces an uploadable package. Check yours with trellis (`cargo trellis audit <package>`; `test` and `bench` too), and package it with `cargo trellis package` and a `[build]` command in `trellis.toml`.

## Roadmap

flwrs is in early development. The main things still missing, roughly in order:

1. **More base plugins.** Common sources, transforms and sinks, so typical workflows need no custom plugin: still missing are files, databases and ML inference, among others. Today there are:
   - the HTTP sink ([plugins/http_sink](plugins/http_sink): a request per event);
   - the ticker ([plugins/ticker](plugins/ticker): interval or cron);
   - the script transform ([plugins/script_transform](plugins/script_transform): Lua per event, keyed state kept exactly once);
   - the accumulator ([plugins/accumulator](plugins/accumulator): per-key aggregates over tumbling, hopping or session windows);
   - the MQTT sink ([plugins/mqtt_sink](plugins/mqtt_sink): MQTT 5 or 3.1.1, acknowledged once the broker has the message);
   - the MQTT source ([plugins/mqtt_source](plugins/mqtt_source): messages acknowledged to the broker once the hub has their events; shared subscriptions);
   - the WebSocket sink ([plugins/ws_sink](plugins/ws_sink): acknowledged when written, or when the server replies);
   - the HTTP source ([plugins/http_source](plugins/http_source): an OpenAPI 3.1 API whose requests become typed events);
   - the WebSocket source ([plugins/ws_source](plugins/ws_source): an AsyncAPI 3.x document's channels, messages typed by their payload schemas);
   - the HTTP push sink ([plugins/http_push_sink](plugins/http_push_sink): events broadcast to subscribers over Server-Sent Events and long polling, with replay);
   - the WebSocket push sink ([plugins/ws_push_sink](plugins/ws_push_sink): events broadcast to connected WebSocket clients, with replay).
2. **Alternative storage back-ends**, such as PostgreSQL. Today scenes, nodes, edges and the plugin registry live in SQLite only (the `Database` enum in `flwrs-core` has a single variant); a shared database is also a step towards running several servers.
3. **Running as a cluster.** The server currently runs in **single-node mode only**: one server runs every scene it has, and there's no swarming, no handover of scenes between servers, and no load balancing. Remote plugins can run on other machines, but they all connect to one hub. Running properly in a data-centre environment (in-cluster) needs scenes spread across servers, failover, and load balancing.
4. **Profiling and optimisation.** Throughput, latency and memory are measured per plugin with `trellis bench` (each plugin's README has a *Footprint* section), so far on an Apple M2 (but needs a proper dedicated benchmark test - just playing around so far, no serious measurements yet); edge devices (an Intel NUC, a Raspberry Pi) are next. Done so far: credit handed to a source's waiting senders in order, longest-waiting first (up to 1.7× for sources whose clients outrun their credit); per-connection buffer settings for the WebSocket plugins (~130 → ~11 KiB per push-sink client); a small HTTP/2 send buffer for the server plugins (1 000 HTTP/2 SSE subscribers: ~3.7× the throughput at a fifth of the memory); trellis's own timer lateness kept out of its latency numbers. Open: batching writes to many WebSocket clients, durable queues' fixed 5 ms batching window, keyed parallelism in the script transforms, TLS handshake cost when connections churn, and the HTTP and WebSocket sources' binary sizes (18–22 MB). See `ideas/` for the designs.
5. **The web UI, beyond a single admin.** The UI itself is in: login with the secret key, the overview (live charts while it's open), the scenes list, the scene editor (canvas, drafts with undo and review, node settings with templates, the edge mapping editor with a live test, live flow and health on the canvas), node logs and stats, plugin pages and the remote plugin wizard, and the server's configuration with guarded restarts. Next: users in a database and single sign-on (OIDC), drafts per user (today one per scene), history beyond what an open page has seen (from the telemetry files), and settings that apply without a restart.

Beyond these, [`ideas/`](ideas) collects design notes for possible future work, one per idea with its status: for example event-time windows, keyed parallelism, byte-based credits, idempotency keys, TLS certificate hot reload, HTTP/3 and WebTransport, a WebSocket client source, persistent replay for the push sinks, settings that apply without a restart, and UI history from the telemetry files. They aren't commitments: each is weighed before it's built.

Smaller known gaps (no Windows CI, durable delivery only within one server) are listed in [AGENTS.md](AGENTS.md), *Current state / known gaps*.

## Contributing with AI agents

LLM agent instructions live in [AGENTS.md](AGENTS.md), not in tool-specific files such as `CLAUDE.md`.

## License

[Apache 2.0](LICENSE)
