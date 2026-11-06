# AGENTS.md

Instructions and a map of the repo for LLM coding agents.

## Rules for agents

- **All LLM instructions go in `AGENTS.md`.** Don't create or edit tool-specific files such as `CLAUDE.md`, `.cursorrules` or `.github/copilot-instructions.md`. If a subfolder needs its own instructions, add an `AGENTS.md` in that folder.
- Keep this file up to date when you add, move or remove modules, crates or plugins.
- The repo is a Cargo workspace (root `Cargo.toml`). Run `cargo build/test/clippy --workspace` from the root, or `-p <crate>` for one crate. There's one `Cargo.lock` and one `target/` at the root.
- Dependency versions live only in `[workspace.dependencies]`. Crates reference them with `<dep>.workspace = true` (plus extra `features` if needed). Package metadata (version, edition, rust-version, license, repository) and lints are inherited the same way. The exception is `flwrs-plugin`'s version: the SDK is published to crates.io on its own (0.0.2 so far), so it has its own `version` in `plugin/Cargo.toml`; bump it for each release (a published version can't be reused). Its crates.io page is `plugin/README.md`. New Rust plugins go under `plugins/` and must be **added to `members` in the root `Cargo.toml`**. Members are listed explicitly on purpose: with a `plugins/*` glob, any folder under `plugins/` without a `Cargo.toml` (such as a non-Rust plugin) would break every cargo command.
- Crates use Rust edition 2024 (the latest stable edition) with `rust-version = "1.94"` (set by sqlx 0.9; raise it only when a dependency needs it). `unsafe` code is denied workspace-wide (`unsafe_code = "deny"`). The **only** exception is `app/src/modules/supervisor/process/windows.rs` (Windows API calls for stopping plugin processes), allowed at its module declaration; every `unsafe` block there has a `// SAFETY:` comment. Don't add `unsafe` anywhere else, and don't widen the exception without asking.
- **Platforms:** the server must run on all mainstream OSes: Windows, macOS, Linux and the common Unixes (the BSDs, illumos). OS-specific code goes behind `cfg(unix)` / `cfg(windows)` with a fallback for other targets (see `supervisor/process/`), never assumptions such as `/bin/sh` or signals in shared code. Tests that spawn shell scripts are `cfg(unix)`. Prefer std over helper crates where std covers it, e.g. `std::sync::LazyLock` rather than `lazy_static`.
- `plugin/` generates Rust from `plugin/schema/*.proto` with `prost-build`, using a bundled `protoc` (`protoc-bin-vendored`), so no system install is needed. `app/` does the same for its own file format, `app/schema/scene_file.proto` (`.flwr` scene exports); the proto conventions below apply there too, and it's versioned separately (the version in the file header).
- Before finishing, run `cargo clippy --workspace --all-targets` and `cargo test --workspace`, and keep clippy clean. `flwrs-app` has three default features: `mapping-lua` (Lua edge mappings, via `mlua` with vendored Lua 5.4), `otel` (the OpenTelemetry telemetry sink: vendored OTLP protos + reqwest) and `telemetry-file` (the local file telemetry sink; flate2). When touching anything feature-gated, also run `cargo clippy -p flwrs-app --all-targets --no-default-features` and `cargo test -p flwrs-app --no-default-features`, and keep both warning-free (for telemetry, also `--no-default-features --features telemetry-file` and `… --features otel`: the `otlp-json` file format needs both) (code only used with a feature gets `#[cfg_attr(not(feature = "…"), allow(dead_code))]` with a reason). Tests whose expectations differ use `#[cfg(feature = "…")]` / `#[cfg(not(...))]`.
- **Plugin types are called "type", never "kind"** (DB columns, manifest fields, Rust names, API fields, docs): `plugins."type"`, manifest `"type": "SINK"`, `PluginType`. In Rust, a field that can't be called `type` is named `plugin_type` and mapped with `#[serde(rename = "type")]` / `#[sqlx(rename = "type")]`.
- **Naming of time and date fields** (DB columns, proto fields, Rust struct fields, JSON keys): use a `_time` suffix for timestamps (date + time) and `_date` for date-only values. Never `_at` / `_on`. Follow the existing verb-stem pattern: `create_time`, `update_time`, `start_time`, `finish_time`, `enqueue_time`. The suffix tells you the type at a glance.
- `ideas/` holds design notes for possible future work, one Markdown file per idea, each with a status line at the top. They aren't specs: check with the user before implementing one, and update or remove the note when an idea is adopted or dropped.
- **Dev runs** use `config.dev.toml` from the repo root (`cargo run -p flwrs-app -- --config config.dev.toml`). All runtime data (DB, installed plugins, spool files, logs) goes under `./.data/`, which is gitignored. When adding a setting that writes to disk, point it at `.data/` in `config.dev.toml`. Never write runtime data into the source tree.
- App write transactions start with `BEGIN IMMEDIATE` (`conn.begin_with(...)`): a deferred transaction that reads and then writes fails with "database is locked" instead of waiting when another connection is busy.
- Migration scripts run inside a transaction, so they must not contain their own `BEGIN`/`COMMIT`. Write SQLite syntax: there are no enum types (use `TEXT` with a `CHECK`), and JSON goes in `TEXT` with `CHECK (json_valid(...))`. Index names are global per database, so prefix them with the table name (`<table>__<column>_idx`). Every `up` script needs a matching `down` script.
- **Plugins are language-agnostic, on purpose.** flwrs is meant to support a varied ecosystem of plugins written in any language. The contract between server and plugin is the wire framing plus `plugin/schema/*.proto`, not the Rust SDK. So:
  - Proto conventions: every enum's zero value is named `UNDEFINED` (never `UNSPECIFIED`), so a value that was never set can't be mistaken for a real one. Enums are wrapped in a message (`message Foo { enum Enum { … } }`) to scope their value names. Messages shared by all plugin types (`Exit`, `ExitCode`, `Shutdown`, `InitializeResponse`, `LogEvent`, `ErrorEvent`) live in `common.proto`. Payload values are plain scalars in a `oneof`, not Google wrapper types. `plugin/src/schema/mod.rs` has tests that enforce the zero-value convention.
  - Treat the `.proto` files as a public, versioned API. Changes must be backward compatible: add new fields, messages and enum values; never renumber, reuse or change the type of an existing field number or enum value. Mark removed fields `reserved`.
  - Framing (`plugin/src/plugin/frame.rs`): new *optional* header lines may be added freely (readers ignore unknown names). A new *required* header, a new `MsgCode` value or any change to the frame layout needs a new `PROTOCOL_VERSION`, and the hub must keep accepting the old one. (The `Key: Value` header is the unreleased 1.0.0 baseline.)
  - Every protocol rule a plugin must follow (Initialize first, data only after `InitializeResponse`, the reconnect rule, metadata propagation by transforms) is written down in README.md, *Plugin protocol*. Keep it in sync when the SDK's behaviour changes.
  - Don't let Rust-only concepts leak into the protocol: anything a plugin must do or send has to be expressible in protobuf and plain TCP.
  - `flwrs-plugin` is the reference SDK, one implementation among many possible ones. Plugin behaviour the server relies on must be documented in the protocol section of README.md, not just implemented in the Rust runners.
  - Plugins don't have to be Rust. A non-Rust plugin's source lives in `plugins/<name>/` with its own build files. It's simply not listed as a workspace member; nothing else is needed.
  - `examples/plugins/` holds demo plugins in other languages (Python, Go, TypeScript, Java). They're deliberately minimal and unsupported: keep them small, in sync with protocol changes, and out of the Cargo workspace.

## What flwrs is

flwrs is a framework and server for running pluggable workflows (data flows), inspired by [Node-RED](https://nodered.org/). Users build workflows from pre-compiled modules (plugins) and configuration, not by writing code. Goals:

- **Lightweight and scalable:** the same runtime runs in a data centre and at the edge.
- **Flexible:** from simple automation up to complex pipelines, for example running ML models as workflow steps.

### Plugin model

- Each plugin is a **separate executable, in any language**. The server launches it as a child process. The plugin then connects back to the flwrs runtime (the "hub") over TCP, using the host and port given on its command line. The connection is usually loopback, but plugins can also run on other machines (distributed deployment).
- Any executable that speaks the wire format below and the messages in `plugin/schema/*.proto` is a valid plugin. The full spec for plugin authors is in README.md, section *Plugin protocol*.
- There are three plugin types:
  - **Source:** produces events, such as a trigger or an input.
  - **Transform:** takes in events and emits changed events.
  - **Sink:** consumes events, such as sending an HTTP request.
- The first frame on every connection is the plugin's `Initialize`: its instance id (`--id`), version, `token` (from `FLWRS_TOKEN` or `--token`) and the schema of its input and/or output payload (`SchemaDefinition`).
- **Wire format:** every message is framed as `u32 LE packet length` + `u32 LE header length` + `header` + a protobuf message body. The header is ASCII `Name: value` lines ending in CRLF, with required `Version: 1.0.0` and `Msg-Code: <common.MsgCode>`, which names the body's message type (e.g. 3 = `SinkMessage`). The framing lives in `plugin/src/plugin/frame.rs` and is public (`read_frame`, `write_message`, `Frame::decode`, the `WireMessage` trait), so the hub can reuse it.
- **Lifecycle:** the hub answers `Initialize` with `InitializeResponse` (accepted) or `Shutdown` (rejected). Data events flow only after `InitializeResponse`. The runtime sends events and finally `Shutdown`. The plugin reports failures as `ErrorEvent`s and its logs as `LogEvent`s, and answers `Shutdown` with an `Exit` message. If the connection drops, the SDK runners reconnect with backoff (1 s doubling to 30 s, jitter) and re-send the cached `Initialize`; `Shutdown` before `InitializeResponse` makes `run` return `Error::Rejected`.
- **Event metadata:** data events carry `common.EventMetadata` (event id, trace id, hops, origin node, create time), managed by the hub. Sources leave it unset; transforms copy the input's metadata onto their outputs (`TransformRunner` does this automatically).
- Payloads are lists of typed `Field`s. Supported types: string, i32, u32, i64, u64, f32, f64, bool, datetime, bytes, array, map (all values share one type) and object (named fields with their own types). A `FieldValue` with nothing set means null.

### Glossary

- **Scene:** a stored workflow definition: a name, an optional hop limit, and a graph of nodes and edges. Persisted in SQLite, exposed through REST, and portable between servers as a `.flwr` file (export / import).
- **Director:** runs the enabled scenes (`app/src/modules/director/`): loads each scene's graph, starts its nodes through the supervisor, and routes events between them through bounded queues. It also runs the hub.
- **Hub / runtime:** the server side that plugins connect to (`app/src/modules/hub/`): the TCP listener, the handshake and the per-connection reader and writer. The director tells it which nodes to expect (`Hub::expect` → `NodeLink`).
- **Supervisor:** runs one plugin instance per node (`app/src/modules/supervisor/`): launches local plugins as child processes, health-checks and relaunches them, waits for remote ones, and runs the shutdown sequence.
- **Instance id vs plugin id:** in the protocol, `plugin_id` (in `Initialize`, events, logs) is the **instance** id: the node id passed as `--id` for local plugins, or a remote node's `config.remote_id` (falls back to the node id). In the DB and REST API, `plugin_id` on a node is the installed plugin package's id. The node id changes when a scene is imported elsewhere; a `remote_id` doesn't, and isn't unique (the token tells nodes with the same `remote_id` apart).
- **Nodes and plugins** aren't linked by a foreign key (since `1.0.0-5`): an imported scene may name plugins this server doesn't have. The scene service's `check` reports such problems, and enabling is refused until there are none. Deleting a plugin that nodes use is still refused (checked in the same write transaction as the delete).

## Repository contents

```
.
├── AGENTS.md            this file: agent instructions and repo map
├── README.md            human-facing overview
├── flwrs-logo.svg       project logo (100×100, shown under the title in README.md)
├── Cargo.toml           workspace manifest: members, shared package metadata, dependency versions, lints
├── Cargo.lock           single lockfile for every crate
├── config.dev.toml      dev server config: runtime data goes to ./.data/ (gitignored); uploads allowed from loopback
├── .cargo/config.toml   `cargo xtask` alias
├── ideas/               design notes for possible future work (not scheduled; not binding)
├── LICENSE              Apache 2.0
├── core/                flwrs-core: shared library (config, logging, DB, HTTP, service registry)
├── app/                 flwrs-app: the flwrs server binary (schema/scene_file.proto: the .flwr export format)
├── plugin/              flwrs-plugin: SDK and wire protocol for building plugins
├── plugins/             base plugin library (one folder per plugin)
│   └── http_sink/       flwrs-http-sink: sink that sends HTTP requests
├── examples/plugins/    demo HTTP sinks in Python, Go, TypeScript (Node.js) and Java (not workspace members)
└── xtask/               project automation: `cargo xtask package-plugin <dir>`
```

### `core/` — `flwrs-core` (library)

Infrastructure shared by the server and by future binaries.

| Path | Description |
|---|---|
| `src/lib.rs` | Module exports. |
| `src/args.rs` | CLI args (`clap`): `-c/--config <path>`, default `config.toml`. |
| `src/config.rs` | `Source` trait for reading config by key path (`read_string`, `read_int`, `sub`, …). `main_config()` is a lazy global loaded from the CLI config path; the process exits if the file is missing or invalid. `read_struct::<T>()` deserializes a section into a struct. |
| `src/config/toml_source.rs` | TOML implementation of `Source`. |
| `src/logging.rs` | Logging config types (`[logging]` section: `id`, `format`, `level`, `sinks`). `init_main_logger()` installs the logger and sets the `log` max level; call it first thing in `main`. If the section is missing or invalid, it falls back to a console logger at INFO. |
| `src/logging/loglog.rs` | `LogLogger`: a `log::Log` that fans records out to several sinks: console and rotating file (via `flexi_logger`), and syslog. |
| `src/db.rs` | `Database` enum (SQLite only for now), `DbError`, and `build_db(name, ups, downs, version)`, which opens the DB from the `[db.<name>]` config and runs migrations. |
| `src/db/migrations.rs` | `MigrationDef` parses migration file names shaped `MAJOR.MINOR.PATCH-BUILD.{up,down}.sql` and hashes their contents. `Version` holds the parsed version; its `Ord` compares major, then minor, patch and build number. |
| `src/db/sqlite.rs` | SQLite pool setup (`sqlx`; WAL journal, 5 s busy timeout) and the `Migrator`. Each migration runs in its own transaction and is tracked in the `migrations` table. If an upgrade fails, the migrations applied earlier in that run are rolled back with their `down` scripts. |
| `src/http.rs` | `HttpServer`, an `axum` server registered as a service. API routers are nested under `/api`; root routers (for example Swagger UI) are merged at `/`. Reads its settings from the `[http]` section (`host`, default `0.0.0.0`; `port`, default `80`). |
| `src/registry.rs` | `ServiceRegistry` and the `Service` trait (`id`, `start(shutdown)`): starts every registered service, cancels them on Ctrl+C (or SIGTERM on Unix) and waits for them to stop. |
| `resources/migrations/sqlite/migrator.sql` | Schema of the migrator's own `migrations` tracking table. |

### `app/` — `flwrs-app` (server binary)

| Path | Description |
|---|---|
| `build.rs` | Compiles `schema/scene_file.proto` (the `.flwr` scene export format) with `prost-build` and the bundled `protoc`. |
| `schema/scene_file.proto` | `SceneFile` (name, `max_hops`, nodes with `key` / type / plugin id and version / config JSON, edges by node key with mapping JSON, export time, `includes_secrets`, exporter version). A public file format: evolve it backward compatibly. |
| `schema/otlp/` | Vendored OTLP metrics protos (OpenTelemetry Authors, Apache-2.0; see its README), compiled only with the `otel` feature. Update by copying newer upstream files. |
| `src/main.rs` | Entry point: sets up the logger, then initialises in order the hub (binds its port), the director handle, the plugin registry and the scene service (each later one uses the earlier ones), builds the service registry and runs it until shutdown. |
| `src/registry.rs` | Registers the app's services: the director (which runs the hub), telemetry (only if a sink is configured) and the HTTP server. |
| `src/http.rs` | Global `HttpServer` with the module routers, Swagger UI at `/swagger-ui` and the OpenAPI spec at `/api-docs/openapi.json` (`utoipa`). |
| `src/modules.rs` | List of feature modules. |
| `src/modules/rate_limit.rs` | `RateLimit`: allows one event per interval (e.g. one WARN per problem per minute; the rest go to DEBUG). Used by the hub, the director and the telemetry sinks. |
| `src/modules/api_error.rs` | Shared REST error response `{error, details[]}` (`ErrorBody`, `ApiError`, `api_error`, `internal`). Every API uses it. |
| `src/modules/node_config.rs` | A node's `config`: `NodeConfig` (`launch` local/remote, `runtime`, `token`, `remote_id`, `launch_args`; `instance_id(node_id)`; `validate` = `validate_own` (no plugin needed, token may be missing: what imports check) + token presence + launch args against the plugin), `RuntimeConfig` with range checks (also used for manifest `runtime_defaults`), validation against the plugin's launch-args rules, and `redacted()` for API output. All structs are strict (`deny_unknown_fields`). |
| `src/modules/mapping.rs` | Edge mapping JSON shape (`MappingSpec`: `mode`, `filter`, `rules`, `script`; rules with exactly one of `source` / `value` / `lua` / `delete`), its validation (incl. path syntax, so it's checked even where plugins are missing, e.g. imports), `uses_lua()`, and the optional `lua` limits (`LuaLimits`: `max_instructions` ≤ 10⁹, `timeout_ms` ≤ 10 000, `max_memory_bytes` 256 KiB–1 GiB). |
| `src/modules/mapping/value.rs` | The typed value tree (mirrors `FieldValue`: bytes, datetime, u64 vs i64 survive), both-way `PluginPayload` conversion (top-level field order kept, nested object keys sorted), `from_json` for constants (shaped by the target field; BYTES from a UTF-8 string or 0–255 array, DATETIME from RFC 3339), lossless `coerce` / `coerce_payload` to a schema (undeclared fields dropped), `convertible` for config-time checks. |
| `src/modules/mapping/path.rs` | Dot paths (`a.b`, `a[0]`, `a["k"]`; bare keys `[A-Za-z0-9_-]`; depth ≤ 32): `parse`, `get`, `set` (creates levels, may append), `delete`, and `resolve` against a `SchemaDefinition` (OBJECT keys must exist, any MAP key, ARRAY indexes). |
| `src/modules/mapping/compile.rs` | `compile(spec, Schemas { source, target }) -> Arc<dyn Mapping>`: rules compiled once; paths resolved against the schemas that are known, constants converted, copies type-checked, Lua chunks compiled (syntax); every problem returned. `Mapping::apply(input, meta) -> Option<payload>` (`None` = filtered or dropped): `filter`, then a `script` (whole output) or the rules (merge / replace; copy, missing source = no-op; set; delete; Lua rule, `nil` = delete), then shaping to the target's schema. `LuaParts` holds the edge's Lua state and chunks; without the feature it refuses any Lua ("built without Lua support"). |
| `src/modules/mapping/lua.rs` | Lua 5.4 tier (`mapping-lua` feature). `Runtime`: one state per edge, only `string` / `table` / `math` / `utf8`, `load` / `dofile` / `loadfile` / `collectgarbage` / `require` / `print` removed, an environment that refuses global assignment, read-only library tables; limits per call (instruction budget and wall-clock deadline via a hook every 1 000 instructions, memory limit per state). `chunk` checks the user's code alone first (exact syntax error lines), then wraps it as `function(payload, meta)`. `payload` / `meta` are deep read-only proxies (locked metatables, `__len` / `__pairs`). Conversions: u64 > `i64::MAX` is an error, datetimes as epoch ms, results guided by the target schema (empty table → ARRAY or MAP, strings → BYTES for BYTES fields). |
| `src/modules/scene.rs` | Scene module: `init(director)` at start-up, then `service()`. |
| `src/modules/scene/api.rs` | REST for scenes: `GET/POST /api/scenes`, `GET/PUT/DELETE /api/scenes/by-id/{id}` (request: `name`, optional `max_hops`), `POST …/enable`, `POST …/disable`, with offset/limit pagination. Holds the scenes OpenAPI doc (including node and edge paths) and the router (`State<&'static Service>`). |
| `src/modules/scene/api_transfer.rs` | REST for moving scenes: `GET …/by-id/{id}/export` (`?include_secrets=true` to include tokens; `application/vnd.flwrs.scene`, attachment `<name>.flwr`), `POST /api/scenes/import` (multipart `file`, ≤ 16 MiB; 201 with the new scene plus `launchable` / `problems` / `warnings`), `GET …/by-id/{id}/check` (the same three, for the web UI's "broken" state). |
| `src/modules/scene/api_status.rs` | `GET …/by-id/{id}/status`: the scene's `enabled` plus the director's `SceneStatus` (`STOPPED` if the director never ran it; 404 for an unknown scene; 503 if the director doesn't answer, via `ServiceError::Unavailable`). Uses `Service::runtime_status`. |
| `src/modules/scene/scene_file.rs` | The `.flwr` format: `"FLWR"` + u32 LE version length + version (`1.0.0`) + protobuf `SceneFile` (generated from `app/schema/scene_file.proto` by `app/build.rs`). `encode`, `decode` (magic, readable versions, size cap), `file_name` (safe download name). |
| `src/modules/scene/service/transfer.rs` | `export_scene` (remote nodes always carry `remote_id`, tokens only with secrets; node configs and mappings as JSON text, so integers stay integers) and `import_scene` (new ids, in file order; scene disabled; strict checks of the file's own structure with every problem reported, and graph rules inside the transaction; plugin fit is left to `check`). |
| `src/modules/scene/api_graph.rs` | REST for a scene's graph: `…/nodes`, `…/nodes/by-id/{node_id}`, `…/edges`, `…/edges/by-id/{edge_id}` (list, create, get, replace, delete). Request bodies are strict; node responses redact the token (`has_token` instead). |
| `src/modules/scene/graph.rs` | `nodes_in_cycles` (Tarjan's strongly connected components, self-loops included). |
| `src/modules/scene/service.rs` | Scenes, nodes and edges: models, `ServiceError` (`NotFound(what)`, `Invalid(details)`, `Conflict(message, details)`), and the service. Every change runs in one transaction under a lock; nodes are validated against their plugin (type, `config`, launch args), and whole-scene rules (edge direction by node type, no `block` queue policy on cycles) are checked before commit. `check(scene)` → `SceneCheck { problems, warnings }`: per node, plugin installed, type matches, config valid against the plugin (problems; problems that don't need the plugin are listed even when it's missing), each edge's mapping compiled against the nodes' plugin schemas (problems), and a different plugin version than configured (warning); edge saves compile the mapping too (422); `set_enabled` refuses while there are problems. Nodes record `plugin_version`. Scene settings: `name` and an optional `max_hops` (1–10 000, `MAX_HOPS_LIMIT`; changing it on an enabled scene reloads it, a rename doesn't). Committed changes to enabled scenes go to the director; enabling checks that all plugins are installed. A remote node's token is kept when an update omits it. |
| `src/modules/scene/service/query_sqlite.rs` | SQLite queries for scenes, nodes and edges (they take `&mut SqliteConnection`, so they work inside transactions). |
| `src/modules/plugin.rs` | Plugin registry module: `init()` (run once at start-up from `main.rs`: prepares folders, checks installations) and `service()`. |
| `src/modules/plugin/config.rs` | `[plugins]` section: `dir` (default `./plugins`), `allow_upload` (default off), `admin_token`, upload and unpack limits. |
| `src/modules/plugin/manifest.rs` | `Manifest` (strict serde, `deny_unknown_fields`), its validation (id pattern, semver, safe paths, platform, payload schemas required by type), `launch_args_to_argv` (styles `flags` / `positional` / `json`, reserved bootstrap args) and `validate_launch_args` (JSON Schema + conversion rules). |
| `src/modules/plugin/package.rs` | ZIP inspection and extraction: zip-slip (absolute, `..`, drive-letter paths), symlinks, duplicates, entry count, unpacked size, compression ratio; manifest and referenced-file checks; extraction with real-size enforcement, sanitized permissions and the executable bit. All checks run before anything is written. |
| `src/modules/plugin/service.rs`, `service/query_sqlite.rs` | `Service`: install / upgrade / delete / get / list, serialized by a lock. Staging under `<dir>/.staging`, atomic folder renames, rollback on failure, `<dir>/.trash` for swapped-out packages. Upgrade re-validates every node's `launch_args`; delete is refused while nodes use the plugin. |
| `src/modules/plugin/api.rs` | REST: `GET /api/plugins`, `GET /api/plugins/by-id/{id}`, `GET …/launch-args-schema`, `POST /api/plugins` (multipart `file`), `PUT /api/plugins/by-id/{id}`, `DELETE …`. Mutating routes are gated (`allow_upload`, then admin token or loopback). Errors are `{error, details[]}`. |
| `src/modules/status_api.rs` | `GET /api/status`: the director's `Overview` (scenes, hub address, memory and disk use). Router state is the `&'static DirectorHandle`; 503 if the director doesn't answer. |
| `src/modules/telemetry.rs` | Telemetry: optional sinks for the server's stats, from `[telemetry]`; registered as a service only if a sink is configured. Without the `otel` / `telemetry-file` feature, `[telemetry.otlp]` / `[[telemetry.file]]` gets a WARN at start; two file entries with the same `dir` and `format` are refused (ERROR). |
| `src/modules/telemetry/config.rs` | `TelemetryConfig` (`otlp`, `files` = `[[telemetry.file]]`), `FileConfig` → `FileSink` (`resolve`: `format`, `dir`, intervals, `partition` (default `1d`) ≥ 1 min in whole minutes, `max_files` (30) and `max_bytes` (1 GiB) with 0 = no limit (`None`), optional `max_age`, `compress`; durations like `30m` / `1d` via `parse_duration`), `OtlpConfig` (strict) and `OtlpConfig::resolve(env)`: config, then the metrics-specific `OTEL_*` variable, then the generic one, then defaults (endpoint `localhost:4318/v1/metrics`, 60 s, 10 s, cumulative); headers / resource attributes from `k=v,…` lists (percent-decoded); `service.name`, `service.version`, `service.instance.id` (configured or a ULID per start). |
| `src/modules/telemetry/samples.rs` | The neutral sample list every sink writes: `Metric` (name, counter / gauge, UCUM unit, description; `ALL`), `Sample` (labels `scene.id`, `node.id`, `plugin.id`, `plugin.version`), `from_snapshot(StatsSnapshot)`. |
| `src/modules/telemetry/otlp.rs` | The OpenTelemetry sink (`otel` feature): hand-built `ExportMetricsServiceRequest`s (generated from the vendored protos in `app/schema/otlp/`), posted over OTLP/HTTP protobuf with async reqwest. Counters as monotonic sums (cumulative with tracked start times and reset detection, or delta committed only after a successful export), gauges as gauges. Rate-limited WARN on failure, `Retry-After` honoured, a last export at shutdown. `Encoder` (the request building and per-series state) is shared with the `otlp-json` file format; the generated code includes pbjson serde impls (OTLP JSON: enums as integers). |
| `src/modules/telemetry/file.rs` | The file sink (`telemetry-file` feature): `Writer` (buffers collections; `flush` writes each into its UTC-aligned partition, `flwrs-metrics.<YYYYMMDDTHHMMSSZ>.<ext>`), `sweep` when a partition opens (closes every other open partition of the format: gzip or OpenMetrics assembly via `<closed>.tmp` + rename, and retention by count / age / bytes; never the current one), `Appender` (appends + `sync_data`, ends a torn last line first), `run`. Format extensions are unique (`jsonl`, `db`, `lp`, `om`), so formats can share a folder. |
| `src/modules/telemetry/file/{otlp_json,sqlite,influx,openmetrics}.rs` | The formats: OTLP JSON Lines (`otel` too; cumulative), SQLite (schema documented in README.md, `SCHEMA_VERSION` 1; one pool per open partition, a transaction per flush, the series table reloaded on reopen), InfluxDB line protocol (measurement = first two name parts, fields = the rest, integers), OpenMetrics (per-family part files in `<partition>.om.d/` while open, assembled with `# TYPE` / `# UNIT` / `# HELP`, series grouped, `# EOF` on close). |
| `src/modules/hub.rs` | Hub module: `init()` binds the listener from `[hub]` at start-up (so the actual port is known before plugins are launched); `get()` returns it. The director runs its accept loop, so the hub outlives every scene at shutdown. |
| `src/modules/hub/config.rs` | `[hub]` section (strict): `host` (default `127.0.0.1`), `port` (default `0` = a free port), `max_event_bytes` (16 MiB), `max_inflight_bytes` (512 MiB, the hub-wide queue budget), `handshake_timeout_ms` (10 s), `max_handshake_bytes` (1 MiB). |
| `src/modules/hub/wire.rs` | Maps the per-type protocol messages onto one shape: `InitInfo`, `DataEvent`, `PluginMsg` (plugin → runtime), `RuntimeMsg` (`Accept` / `Shutdown` / `Event`), `decode` / `encode` per plugin type, and the code ↔ type helpers. |
| `src/modules/hub/service.rs` | `Hub`: `bind`, `run`, `local_addr`, `expect(ExpectedNode) -> NodeLink`. Handshake per connection: first frame within the timeout and size limit, a plugin → runtime `Msg-Code`, `Initialize`; then instance id (matched against each expectation's `instance_id`; when several share it, e.g. a `remote_id`, the token decides, preferring an unconnected node), token (constant-time), type, version and payload schemas (well-formed, and structurally equal to the manifest's). Unknown id / wrong token → `Shutdown`, logged (rate-limited), nothing reported; a mismatch → `Shutdown` and `HubEvent::Rejected { reasons }`; accepted → `InitializeResponse`, then `HubEvent::Connected`. Newest connection wins. Running: one reader (code check, oversized messages skipped, logs and errors written to the server log tagged `[scene/node]`, events and `Exit` forwarded) and one writer per connection. `NodeLink`: `recv` (`HubEvent`s, bounded: a slow director stops the reader, which backpressures the plugin over TCP), `send(RuntimeMsg)`, `is_connected`, `disconnect`, `renew_token` (for a relaunch: closes the connection; the old token stops working), `sender()` (a cloneable `LinkSender`, valid until the link is dropped); dropping the link closes the connection. Logs, at shutdown of a connection after `Exit`, at DEBUG rather than WARN. |
| `src/modules/hub/tests.rs` | Hub tests: a raw fake plugin for each handshake and runtime case, and end-to-end tests with the real SDK `TransformRunner` running in a **child process** (the test binary re-runs the ignored `sdk_transform_child` test), so the SDK's process-wide logger stays out of the hub's process. |
| `src/modules/supervisor.rs` | Process supervisor module: runs one plugin instance per node. Used by the director. |
| `src/modules/supervisor/spec.rs` | `NodeSpec::resolve(scene, node, &NodeConfig, &Plugin, hub_addr, log_level)`: re-validates the node config, resolves the executable (absolute path; working directory = install folder), the bootstrap args (`--id --host --port --log-level`, with `0.0.0.0` replaced by loopback) plus `launch_args`, the payload schemas, the `Timings` and the `QueueSpec` (inbound queue: capacity, bytes, `when_full`, block timeout, spill flag) (precedence: node `runtime` → manifest `runtime_defaults` → built-in defaults). `NodeSpec::expected(token)` builds the hub expectation. |
| `src/modules/supervisor/process.rs` | `PluginProcess`: spawn (token in `FLWRS_TOKEN`, stdin null, `kill_on_drop`), stdout (INFO) and stderr (WARN) forwarded to the server log line by line, tagged `[scene/node]`, lines cut at 8 KiB; `wait`, `wait_for`, `terminate` (ask the process tree to stop, grace, kill; never signals a reaped pid); on drop, kills the tree if still running. `new_token()`: 256 bits from `getrandom`, hex. The OS-specific part is one `os` module per family: |
| `src/modules/supervisor/process/unix.rs` | Linux, macOS, BSDs, illumos: the plugin leads its own process group (`process_group(0)`); stop = SIGTERM to the group, kill = SIGKILL to the group (`nix::killpg`). A Ctrl+C in the server's terminal doesn't reach plugins directly. |
| `src/modules/supervisor/process/windows.rs` | Windows: the plugin starts with `CREATE_NEW_PROCESS_GROUP` and in a Job Object with kill-on-close (`windows-sys`); stop = Ctrl-Break to its console process group (skipped if the server has no console), kill = terminate the Job (the whole tree; also when the server exits). The workspace's only `unsafe`. Type-checked from macOS with a scratch crate against `x86_64-pc-windows-msvc`; there's no Windows CI yet. |
| `src/modules/supervisor/process/other.rs` | Any other target: no graceful step; only the plugin's own process is killed. |
| `src/modules/supervisor/node.rs` | `supervise(spec, &hub, output) -> NodeHandle`: one task per node. States `Launching` / `Waiting` (remote) → `Running` → `Stopping` → `Stopped`, plus `Disconnected`, `Unhealthy`, `Failed`. Local: a fresh token per launch (`NodeLink::renew_token`), `init_timeout` health check, relaunch after `restart_delay` (optional exponential backoff with jitter, reset after `reset_after`), `reconnect_timeout` after a disconnect, an unexpected `Exit` gets `kill_grace` before a kill, `max_restarts` counts consecutive launches that never reached `Running`; a rejected handshake → `Failed`, no relaunch. Remote: no process; `Waiting` / `Running` / `Disconnected` / `Failed`. Stop: `Shutdown`, `shutdown_grace` (events still forwarded), then SIGTERM / Ctrl-Break, `kill_grace`, then a kill of the process tree. Data events go to `output` as `NodeOutput { node_id, event, size }` (cancel-safe forwarding). `NodeHandle`: `status`, `watch`, `sender` (stable across relaunches), `stop().await`; dropping it stops the node. |
| `src/modules/supervisor/tests.rs` | Supervisor tests. The plugins are this test binary re-run through a small shell script in a chosen mode (silent, connect-only, crash, unexpected `Exit`, ignores `Shutdown` and SIGTERM, wrong version, emits events, a real SDK sink). Scripts are run once on creation (`WARM_UP=1`), because macOS scans a new executable on its first run. Unix-only where they spawn processes. |
| `src/modules/director.rs` | Director module: `init()` creates the command channel (`DirectorHandle`) at start-up; `service()` is the registry service. |
| `src/modules/director/config.rs` | `[director]` section (strict): `max_hops` (default 32; the server-wide default, overridden per scene by `scenes.max_hops`), `spool_dir` (spill files, `<spool_dir>/<scene-id>/<node-id>.<ulid>.db`, one per queue), `spill_default` (false), `memory_soft_inflight_bytes` (128 MiB, hub-wide memory soft limit of spilling queues), `spill_max_bytes` (10 GiB, all spool files), `spill_min_free_bytes` (1 GiB free-space guard). |
| `src/modules/director/engine.rs` | `Engine::run(commands, shutdown)`: wipes the spool directory (leftovers of an earlier run), starts every enabled scene, then one **worker per scene** (commands for a scene run in order; a slow stop doesn't hold up other scenes). A burst of queued reloads is applied once. `Reload` re-reads the scene: running → `RunningScene::reload` (the diff; a graph that fails its checks stops the scene, `FAILED`), not running → a fresh start, disabled or deleted → stopped. On shutdown, every scene is stopped. |
| `src/modules/director/running.rs` | `Plan::new(&SceneSnapshot, &Context)` re-checks the graph (plugins registered, node config, type matches plugin, edge direction, no `block` on a cycle; all problems reported, nothing changed on failure) and resolves each node's `NodeKey` (plugin id, version, archive hash and path, type, config, effective `when_full` and spill). `RunningScene::start` (a reload from nothing) and **`reload`, a diff**: nodes with the same key keep running (process, queue, counters); added and changed nodes get new queues first (each queue its own spool file, `<node-id>.<ulid>.db`), then every kept producer's routing table (`Routes`: out-edges with compiled mappings, `max_hops`, in a `watch` channel) is swapped, then changed **remote** nodes are re-expected (the hub drops the old connection without `Shutdown`, so the plugin reconnects), then removed and changed nodes stop (`stop_nodes`: sources, transforms, sinks; routers drain up to 1 s; queues finished), then the remaining new nodes launch; returns a `ReloadSummary`. Queues: `block` by default, `drop_newest` for nodes on a cycle; spill when the node or `spill_default` asks; the scene's spool folder is wiped at a fresh start and removed after stop. One **router** per node and one **writer** per receiving node. The router applies each out-edge's compiled mapping (errors counted in `NodeReport.mapping_errors` and logged, rate-limited; mapped payloads re-measured for the queue), fills in the event metadata (new trace for source events, keeps a transform's; hop count +1; new `event_id` per handover), drops events over the hop limit (the scene's `max_hops`, else `[director] max_hops`), and pushes a copy to each out-edge's queue . The writer waits until its node is `Running`, then hands events to the plugin (requeued at the front if the connection just went away). `stop()`: sources, then transforms, then sinks. `NodeReport` for status: supervisor status, `counters` (`NodeCounters`: emitted, received, dropped, mapping errors, restarts), queue stats, hop drops, mapping errors. |
| `src/modules/director/queue.rs` | `NodeQueue`: a node's inbound queue. **Without spill:** bounded by count and bytes; `when_full` `block` (waits up to `block_timeout` for room and budget), `drop_newest`, `drop_oldest` (evicts); a hub-wide byte budget (`Budget`, `[hub] max_inflight_bytes`) shared by all queues. **With spill:** a memory soft limit (per node, and hub-wide via the `memory` `Gauge`), then disk (`pending` → spool file) with hard limits (per node, hub-wide `disk` gauge, free-space guard); once anything is outside memory, every new event goes there too (strict FIFO), refilled at the watermark, back to memory-only when the disk is empty. `drop_oldest` with a full disk evicts the oldest event (in memory); the disk may then exceed its own limit by up to the memory limit until the next refill. **Blocked time** (`blocked_ms`) is the time `block` pushes waited. **Size rule:** only the hub-wide hard limit (`max_inflight_bytes`) makes an event too large (dropped, WARN); an event over a node's limits is accepted alone (empty memory, or empty disk). `finish()` closes the queue and waits for the spill task to delete the file. `QueueStats` (incl. `spilling`, `disk_len`, `spilled`, `refilled`, `disk_blocked`). |
| `src/modules/director/queue/spill.rs` | The spill task, the only code touching a queue's spool file (SQLite: WAL, `synchronous = NORMAL`, incremental vacuum; `events(seq, enqueue_time, size, event)` with the protobuf-encoded event). Refills first (oldest first; at least one event when memory is empty, even one over the memory limit), then writes pending events in batches (`write_batch`, 5 ms window), checking free space first (`fs4`). Progress-based loop: it sleeps when nothing can move, so it can never spin. Tests run on a multi-threaded runtime, so a task that never yields fails on a timeout instead of hanging. |
| `src/modules/director/tests.rs` | Director tests with real scenes and real SDK plugin processes (this test binary re-run as `test_plugin` children: a letters source, a ticker source, upper / pass transforms, a sink appending to a file): pipeline with metadata, fan-out and fan-in, a cycle stopped by the hop limit, diff reload (unchanged nodes keep their pids while edges, mappings, the hop limit and removed nodes change; a changed node restarts alone; a changed remote node's plugin stays connected), a scene failing its checks, start-up and shutdown. Plus queue unit tests in `queue.rs`. Unix-only. |
| `src/modules/director/handle.rs` | `DirectorHandle` (cloneable): `request_reload` / `request_stop` (queued, non-blocking, in order), `reload`, `status` and `overview` (awaitable). `DirectorCommand` (incl. `Overview`), `DirectorError`, `SceneStatus` (`state` RUNNING / STOPPED / FAILED, load `errors`, the `max_hops` in effect, per-node `NodeReport`s), `Overview` (hub address, `Usage` of the queue budget, spill memory and disk, `SceneSummary` per scene). Status types derive `utoipa::ToSchema`, so the API returns them as is. |
| `src/modules/director/service.rs` | The director as a registry service: builds the `Engine` from the hub, `[director]`, `[hub] max_inflight_bytes` and the scene service; runs the hub's accept loop and stops it only after every scene has stopped (so plugins still get `Shutdown`). |
| `src/db.rs` | The `main` DB (lazy), its embedded migrations, and `test_db()` for tests (a fully migrated DB in a temp file). |
| `resources/migrations/sqlite/main/` | Migrations for the main DB. `1.0.0-1` creates `scenes`. `1.0.0-2` creates the plugin registry (`plugins`: id, name, version, `"type"`, manifest and schema copies, install path, archive hash) and the scene graph: `scene_nodes` (plugin instances: `type` is SOURCE/SINK/TRANSFORM, `plugin_id`, JSON-object `config`) and `scene_edges` (directed handovers `source_node_id → target_node_id`, cycles and self-loops allowed, at most one per direction). Nodes reference `plugins (id, "type")` (`ON DELETE RESTRICT`, so a node's type must match its plugin's type, and a used plugin can't be deleted); `scene_edges.mapping` is optional JSON. Other foreign keys cascade on delete and keep both ends of an edge in the same scene. `1.0.0-3` adds `scenes.enabled` (BOOLEAN, default FALSE). `1.0.0-4` adds `scenes.max_hops` (INTEGER, NULL = the server default, CHECK 1–10 000). `1.0.0-5` rebuilds `scene_nodes` and `scene_edges` to drop the nodes → plugins foreign key (imported scenes may name missing plugins) and adds `scene_nodes.plugin_version`; its down script drops nodes whose plugin isn't registered. |

**Pattern for a new app module:**
- `modules/<name>.rs`: `init(...)` called once from `main.rs`, then `service()`.
- `<name>/service.rs`: logic, models and error type.
- `<name>/service/query_sqlite.rs`: queries taking `&mut SqliteConnection`, so they work in transactions.
- `<name>/api.rs`: an axum router with `State<&'static Service>`, so tests can build it with their own service. Errors via `api_error::ApiError`, plus a `utoipa` `OpenApi`.

Then wire the router into `app/src/http.rs`, and add migrations under `resources/migrations/sqlite/main/`. Tests use `db::test_db()`, plus the fixtures in `plugin::service::tests` / `scene::service::tests`.

### `plugin/` — `flwrs-plugin` (plugin SDK, `cdylib` + `rlib`)

The protocol definition (`schema/`), plus the reference Rust SDK for implementing it. Plugins in other languages generate their own code from `schema/*.proto` and implement the framing themselves.

| Path | Description |
|---|---|
| `build.rs` | Compiles the `.proto` files with `prost-build` and the bundled `protoc`. |
| `schema/schema.proto` | Core data model: `FieldType`, `FieldValue`, `Field`, `FieldDefinition`, `SchemaDefinition`, `PluginPayload`. |
| `schema/common.proto` | Shared by all types: `MsgCode` (frame header codes), `PluginType`, `LogLevel`, `LogEvent`, `ErrorEvent`, `ExitCode`, `Exit`, `Shutdown`, `InitializeResponse`, `EventMetadata`. |
| `schema/source.proto` | Source ⇄ runtime messages (`SourceMessage`, `RuntimeSourceMessage`). |
| `schema/transform.proto` | Transform ⇄ runtime messages. |
| `schema/sink.proto` | Sink ⇄ runtime messages. |
| `src/schema/mod.rs` | Includes the generated protobuf modules (`schema`, `common`, `source`, `transform`, `sink`) and their serde impls (proto3 JSON, generated by `pbjson-build`). Clippy is silenced for generated code only. |
| `src/schema/rules.rs` | Payload schema rules shared by hub and plugins: `check_well_formed`, `diff` (structural equality, order and descriptions ignored) and `parse_json` (proto3 JSON, camelCase or snake_case). |
| `src/plugin/core.rs` | Builder types `InitializeRequest`, `SchemaDefinition` and `FieldDefinition`, which convert into the protobuf types. Also `ConnectionConfig` (host, port, optional token, `ReconnectPolicy`), `TOKEN_ENV` (`FLWRS_TOKEN`) and `MAX_RECONNECT_DELAY` (30 s, the protocol maximum). |
| `src/plugin/error.rs` | Plugin error types (`Error`, `InitializeError`, `SinkError`, `SourceError`, `TransformError`, …). |
| `src/plugin/frame.rs` | Public wire framing (see *Plugin model*): header lines (`Version`, `Msg-Code`) and their parsing rules, `Frame` (code + body), `read_frame`, `read_frame_limited` (skips oversized bodies without buffering them; used by the hub), `write_frame`, `write_message`, `encode_message`, `Frame::decode` (checks the code), and the `WireMessage` trait mapping each top-level message type to its `MsgCode`. Unit tests cover every header rule. |
| `src/plugin/msg_client.rs` | `HubClient`: the reconnectable connection. Shared writer, and connection stages `Down` → `Initializing` (`Initialize` sent: system messages allowed) → `Ready` (`InitializeResponse` received: data allowed); sends below their stage fail fast. `Session`: the receiving side of one connection, read on its own task (cancel-safe), ending on close, read error, a wrong `Msg-Code` or a failed write. |
| `src/plugin/runtime.rs` | Helpers shared by the runners: reconnect `Backoff` (exponential, capped, jitter), `open_session` (connect with retries + send `Initialize`), `accept`, `SessionEnd`, `finish` (flush logs, send `Exit`, close) and the `ErrorEvent` builder. |
| `src/plugin/logger.rs` | `log::Log` implementation that forwards plugin logs to the hub as `LogEvent`s, wrapped in the plugin type's message type. Keeps the record's level and structured key-values. Records go into a bounded queue (10 000, drops the oldest and reports the count) and are sent in order by one background task once `Initialize` is on the wire, so logs written while disconnected arrive after the reconnect. After the runner finishes, records go to stderr. |
| `src/sink/{plugin,runner}.rs` | `Sink` trait (`initialize`, `consume_event`, `shutdown`, `version`) and `SinkRunner`: `initialize` only installs the logger; `run` calls `Sink::initialize` once, then connects, sends `Initialize` (with the token) and handles runtime messages, reconnecting as needed. |
| `src/source/{plugin,runner,local_sink}.rs` | `Source` trait: `initialize` receives a cloneable `LocalSink` handle, and `async fn run` emits events through it. `SourceRunner` starts `run` only after `InitializeResponse`, cancels it on `Shutdown` or a disconnect, restarts it after a reconnect, and restarts it with a backoff if it fails. `LocalSink::emit` fails fast while not connected. |
| `src/transform/{plugin,runner,local_sink}.rs` | `Transform` trait (`process_event`) and `TransformRunner`. The runner sends each returned event back to the hub, copying the input's `metadata` if the output has none. `LocalSink` can emit extra events. |
| `tests/` | Integration tests that drive each runner against a fake hub over TCP (`tests/common`): lifecycle, Initialize-first and data-after-accept, hub restart and re-initialize, rejection by `Shutdown`, a wrong `Msg-Code`, metadata propagation, logs queued before `Initialize`. The logger is process-wide, so each file is its own process and tests within a file run one at a time. |

**To write a Rust plugin:** create a binary crate under `plugins/`, add it to `members` in the root `Cargo.toml`, and depend on `flwrs-plugin.workspace = true`. Implement `Sink`, `Source` or `Transform`, declare the schema with the `core.rs` builders, parse `--id/--host/--port/--log-level` (and an optional `--token`; the runner falls back to `FLWRS_TOKEN`) and run the matching `*Runner`. Keep `-h` free for `--help`. `plugins/http_sink` is the reference example.

### `plugins/` — base plugin library

| Plugin | Type | Description |
|---|---|---|
| `http_sink/` (`flwrs-http-sink`) | Sink | Sends an HTTP request for each event, built with `reqwest`. Payload fields: `url` (string), `method` (string), `headers` (map → array of strings), `body` (string or bytes). CLI flags set the timeouts and verbose logging. Requests are sent fire-and-forget on a tokio task. Exits with status 1 if the run fails or the hub rejects it. Package files: `manifest.json`, `schema/launch_args.json`, `schema/in.json`; tests keep them in sync with the code (version, input schema, CLI flags). |

**Plugin packages.** A plugin is distributed as a ZIP with `manifest.json` at its root, the executable it points to, and the schema files it references (format in README.md, *Plugin packages*). Rust plugins in this repo are packaged with `cargo xtask package-plugin <dir>` → `target/plugins/<id>-<version>-<platform>.zip` (the host platform is written into the manifest). A Rust plugin that's packaged needs a `manifest.json` next to its `Cargo.toml`, with the same version.

### `examples/plugins/` — demo plugins in other languages

One folder per example, each self-contained (`manifest.json`, `schema/in.json` with `url` / `method` / `body` strings, `build.sh` → `dist/<id>-0.0.1.zip`; generated code and build output are gitignored). `README.md` there lists what they implement and leave out. Protos are read from `../../../plugin/schema/`.

| Example | Notes |
|---|---|
| `http_sink_python_example/` | `plugin.py` (shebang executable), code generated into `gen/` by `grpcio-tools`; the host needs `protobuf`. |
| `http_sink_go_example/` | `main.go`, code generated into `pb/` by `protoc` + `protoc-gen-go` (all three protos mapped to one Go package, since they have no `go_package`). |
| `http_sink_node_example/` | `plugin.ts` run directly by Node ≥ 23.6 (type stripping; `tsc --noEmit` only checks), `protobufjs` loads the protos at run time (`keepCase`); the package carries `node_modules` and a copy of the protos in `proto/`. |
| `http_sink_java_example/` | `HttpSink.java`; Maven generates the code (`io.github.ascopes:protobuf-maven-plugin`) and shades one jar; `bin/http-sink-java-example` runs it. |

## Configuration (server)

The server reads a TOML file (`--config`, default `./config.toml`):

```toml
[http]
host = "0.0.0.0"
port = 8080

[logging]            # optional: defaults to console at INFO
id = "flwrs"         # syslog process name and log file prefix
format = "PLAIN"
level = "INFO"       # TRACE | DEBUG | INFO | WARN | ERROR

[[logging.sinks]]
sink_type = "CONSOLE"  # CONSOLE | FILE | SYSLOG
# file_directory = "logs/"; file_max_size_bytes; file_max_log_history (FILE only)

[db.main]
filename = "main.db"   # default: <name>.db; parent folders are created

[hub]                  # where plugins connect
host = "127.0.0.1"     # a reachable address (or 0.0.0.0) for remote plugins
port = 0               # 0 = pick a free port; remote plugins need a fixed one
# max_event_bytes = 16777216; max_inflight_bytes = 536870912; handshake_timeout_ms = 10000; max_handshake_bytes = 1048576

[director]
# max_hops = 32        # events are dropped after this many handovers (stops runaway loops); scenes can set their own
# spool_dir = "./scenes"  # spill-to-disk files: <spool_dir>/<scene-id>/<node-id>.<ulid>.db
# spill_default = false   # spill for nodes that don't set runtime.queue.spill.enabled
# memory_soft_inflight_bytes = 134217728; spill_max_bytes = 10737418240; spill_min_free_bytes = 1073741824

[telemetry.otlp]       # optional: export stats to an OpenTelemetry collector (otel feature)
# endpoint = "http://collector:4318/v1/metrics"; headers = { authorization = "…" }
# interval_ms = 60000; timeout_ms = 10000; temporality = "cumulative"; service_name = "flwrs"
# unset keys fall back to the standard OTEL_* environment variables

[[telemetry.file]]     # optional, repeatable: stats in local files (telemetry-file feature)
format = "sqlite"      # otlp-json (needs otel) | sqlite | influx | openmetrics
# dir = "./telemetry"; interval_ms = 10000; flush_interval_ms = 60000; partition = "1d" (UTC)
# max_files = 30 (current included); max_bytes = 1073741824 (0 = no limit for either); max_age = "90d"; compress = true (text formats)

[plugins]
dir = "./plugins"      # where packages are installed (<dir>/<plugin-id>/)
allow_upload = false   # upload/upgrade/delete endpoints; uploaded plugins are code this server runs
# admin_token = "…"    # if set: mutating endpoints need "Authorization: Bearer <token>"; if not: loopback clients only
# max_upload_bytes = 268435456; max_unpacked_bytes = 1073741824; max_entries = 10000; max_compression_ratio = 200
```

## Current state / known gaps

This is early-stage code. Keep these in mind before relying on the behaviour:

- **Single node only:** one server runs every scene it has. There's no clustering: no sharing scenes across servers, no failover or handover of scenes between servers, no load balancing. Remote plugins can run on other machines, but they all connect to one hub.
- **Plugins and UI:** the base plugin library has one plugin (`http_sink`), plus the unsupported demos in `examples/plugins/` (not in the automated tests). Scenes are managed through the REST API only; there's no web UI yet.

- **Remote links** are plain TCP (no TLS yet).
- **Process trees on Unix:** the plugin's whole process group is signalled while the plugin runs. If the plugin itself dies first, processes it started that are still running aren't cleaned up (on Windows the Job Object kills them).
- **Windows:** the Windows-specific code is type-checked and linted for `x86_64-pc-windows-msvc`, but there's no Windows CI, and the process tests are Unix-only (they use shell scripts). Run them on Windows (with `.cmd` equivalents) before relying on it.
- **Director:** at-most-once delivery (spill files extend capacity, not guarantees: they are deleted when their node or scene stops, and wiped when a scene starts and when the director starts). Reloads are diffs: only changed nodes restart, and a restarting node's already-queued events are dropped (new ones wait in its new queue). Status: `GET /api/scenes/by-id/{id}/status` and `GET /api/status`; counters are in memory, per node, and restart when the node does; history only through the telemetry sinks (OTLP or files; the server has no query API for the files, and no Prometheus scrape endpoint).
- **Mappings:** dot-notation and Lua work end to end (saved, checked against the plugins' schemas, evaluated by the director, exported and imported). A server built without `mapping-lua` refuses Lua when saving, and lists it as a problem in `check` (imports go through; enabling is refused; the director won't start such a scene). Lua calls run on the router task: a script near its time limit delays that producer's other events. A plugin upgrade doesn't re-check mappings; one that no longer fits shows up in `check` and makes the scene fail on its next load.
- **Bulk graph edits:** each node or edge change is its own transaction, and reloads an enabled scene (a diff; queued reloads are applied once). To restructure a live scene in one step without intermediate states running, disable it, edit, then enable it again.
- **SDK restarts:** inside a plugin, a source whose `run` keeps failing is restarted every second indefinitely, and reconnects retry forever. The supervisor's limits (`reconnect_timeout_ms`, `max_restarts`) bound this from the outside for local plugins.
