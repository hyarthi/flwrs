# AGENTS.md

Instructions and a map of the repo for LLM coding agents.

## Rules for agents

- **All LLM instructions go in `AGENTS.md`.** Don't create or edit tool-specific files such as `CLAUDE.md`, `.cursorrules` or `.github/copilot-instructions.md`. If a subfolder needs its own instructions, add an `AGENTS.md` in that folder.
- Keep this file up to date when you add, move or remove modules, crates or plugins.
- The repo is a Cargo workspace (root `Cargo.toml`). Run `cargo build/test/clippy --workspace` from the root, or `-p <crate>` for one crate. There's one `Cargo.lock` and one `target/` at the root.
- Dependency versions live only in `[workspace.dependencies]`. Crates reference them with `<dep>.workspace = true` (plus extra `features` if needed). Package metadata (version, edition, rust-version, license, repository) and lints are inherited the same way. New Rust plugins go under `plugins/` and must be **added to `members` in the root `Cargo.toml`**. Members are listed explicitly on purpose: with a `plugins/*` glob, any folder under `plugins/` without a `Cargo.toml` (such as a non-Rust plugin) would break every cargo command.
- Crates use Rust edition 2024 (the latest stable edition) with `rust-version = "1.94"` (set by sqlx 0.9; raise it only when a dependency needs it). `unsafe` code is forbidden workspace-wide. Prefer std over helper crates where std covers it, e.g. `std::sync::LazyLock` rather than `lazy_static`.
- `plugin/` generates Rust from `plugin/schema/*.proto` with `prost-build`, using a bundled `protoc` (`protoc-bin-vendored`), so no system install is needed.
- Before finishing, run `cargo clippy --workspace --all-targets` and `cargo test --workspace`, and keep clippy clean.
- **Plugin types are called "type", never "kind"** (DB columns, manifest fields, Rust names, API fields, docs): `plugins."type"`, manifest `"type": "SINK"`, `PluginType`. In Rust, a field that can't be called `type` is named `plugin_type` and mapped with `#[serde(rename = "type")]` / `#[sqlx(rename = "type")]`.
- **Naming of time and date fields** (DB columns, proto fields, Rust struct fields, JSON keys): use a `_time` suffix for timestamps (date + time) and `_date` for date-only values. Never `_at` / `_on`. Follow the existing verb-stem pattern: `create_time`, `update_time`, `start_time`, `finish_time`, `enqueue_time`. The suffix tells you the type at a glance.
- `ideas/` holds design notes for possible future work, one Markdown file per idea, each with a status line at the top. They aren't specs: check with the user before implementing one, and update or remove the note when an idea is adopted or dropped.
- **Dev runs** use `config.dev.toml` from the repo root (`cargo run -p flwrs-app -- --config config.dev.toml`). All runtime data (DB, installed plugins, spool files, logs) goes under `./.data/`, which is gitignored. When adding a setting that writes to disk, point it at `.data/` in `config.dev.toml`. Never write runtime data into the source tree.
- Migration scripts run inside a transaction, so they must not contain their own `BEGIN`/`COMMIT`. Write SQLite syntax: there are no enum types (use `TEXT` with a `CHECK`), and JSON goes in `TEXT` with `CHECK (json_valid(...))`. Index names are global per database, so prefix them with the table name (`<table>__<column>_idx`). Every `up` script needs a matching `down` script.
- **Plugins are language-agnostic, on purpose.** flwrs is meant to support a varied ecosystem of plugins written in any language. The contract between server and plugin is the wire framing plus `plugin/schema/*.proto`, not the Rust SDK. So:
  - Proto conventions: every enum's zero value is named `UNDEFINED` (never `UNSPECIFIED`), so a value that was never set can't be mistaken for a real one. Enums are wrapped in a message (`message Foo { enum Enum { … } }`) to scope their value names. Messages shared by all plugin types (`Exit`, `ExitCode`, `Shutdown`, `InitializeResponse`, `LogEvent`, `ErrorEvent`) live in `common.proto`. Payload values are plain scalars in a `oneof`, not Google wrapper types. `plugin/src/schema/mod.rs` has tests that enforce the zero-value convention.
  - Treat the `.proto` files as a public, versioned API. Changes must be backward compatible: add new fields, messages and enum values; never renumber, reuse or change the type of an existing field number or enum value. Mark removed fields `reserved`.
  - Framing (`plugin/src/plugin/frame.rs`): new *optional* header lines may be added freely (readers ignore unknown names). A new *required* header, a new `MsgCode` value or any change to the frame layout needs a new `PROTOCOL_VERSION`, and the hub must keep accepting the old one. (The `Key: Value` header is the unreleased 1.0.0 baseline.)
  - Every protocol rule a plugin must follow (Initialize first, data only after `InitializeResponse`, the reconnect rule, metadata propagation by transforms) is written down in README.md, *Plugin protocol*. Keep it in sync when the SDK's behaviour changes.
  - Don't let Rust-only concepts leak into the protocol: anything a plugin must do or send has to be expressible in protobuf and plain TCP.
  - `flwrs-plugin` is the reference SDK, one implementation among many possible ones. Plugin behaviour the server relies on must be documented in the protocol section of README.md, not just implemented in the Rust runners.
  - Plugins don't have to be Rust. A non-Rust plugin's source lives in `plugins/<name>/` with its own build files. It's simply not listed as a workspace member; nothing else is needed.

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

- **Scene:** a stored workflow definition. Today it has only an id and a name; it's persisted in SQLite and exposed through REST.
- **Director:** the planned service that will run scenes and plugins. It's a stub for now.
- **Hub / runtime:** the server side that launches plugins and that they connect to. The plugin-side client exists (with reconnect); the hub-side listener and the process launcher are not written yet.
- **Instance id vs plugin id:** in the protocol, `plugin_id` (in `Initialize`, events, logs) is the **instance** id, i.e. the scene node's id passed as `--id`. In the DB and REST API, `plugin_id` on a node is the installed plugin package's id.

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
├── app/                 flwrs-app: the flwrs server binary
├── plugin/              flwrs-plugin: SDK and wire protocol for building plugins
├── plugins/             base plugin library (one folder per plugin)
│   └── http_sink/       flwrs-http-sink: sink that sends HTTP requests
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
| `src/db/sqlite.rs` | SQLite pool setup (`sqlx`) and the `Migrator`. Each migration runs in its own transaction and is tracked in the `migrations` table. If an upgrade fails, the migrations applied earlier in that run are rolled back with their `down` scripts. |
| `src/http.rs` | `HttpServer`, an `axum` server registered as a service. API routers are nested under `/api`; root routers (for example Swagger UI) are merged at `/`. Reads its settings from the `[http]` section (`host`, default `0.0.0.0`; `port`, default `80`). |
| `src/registry.rs` | `Service` trait (`id`, `start(CancellationToken)`) and `ServiceRegistry`, which spawns the services. It stops all of them on Ctrl+C, on SIGTERM, or when any one service fails, and returns the first error. |
| `resources/migrations/sqlite/migrator.sql` | Schema of the migrator's own `migrations` tracking table. |

### `app/` — `flwrs-app` (server binary)

| Path | Description |
|---|---|
| `src/main.rs` | Entry point: sets up the logger, then initialises in order the director handle, the plugin registry and the scene service (each later one uses the earlier ones), builds the service registry and runs it until shutdown. |
| `src/registry.rs` | Registers the app's services: the director and the HTTP server. |
| `src/http.rs` | Global `HttpServer` with the module routers, Swagger UI at `/swagger-ui` and the OpenAPI spec at `/api-docs/openapi.json` (`utoipa`). |
| `src/db.rs` | The `main` DB. Embeds the migrations from `resources/migrations/sqlite/main/` and builds the DB lazily. |
| `src/modules.rs` | List of feature modules. |
| `src/modules/api_error.rs` | Shared REST error response `{error, details[]}` (`ErrorBody`, `ApiError`, `api_error`, `internal`). Every API uses it. |
| `src/modules/node_config.rs` | A node's `config`: `NodeConfig` (`launch` local/remote, `runtime`, `token`, `launch_args`), `RuntimeConfig` with range checks (also used for manifest `runtime_defaults`), validation against the plugin's launch-args rules, and `redacted()` for API output. All structs are strict (`deny_unknown_fields`). |
| `src/modules/mapping.rs` | Edge mapping JSON shape (`MappingSpec`: `mode`, `filter`, `rules`, `script`; rules with exactly one of `source` / `value` / `lua` / `delete`) and its validation. Evaluation comes in the mapping phases. |
| `src/modules/scene.rs` | Scene module: `init(director)` at start-up, then `service()`. |
| `src/modules/scene/api.rs` | REST for scenes: `GET/POST /api/scenes`, `GET/PUT/DELETE /api/scenes/by-id/{id}`, `POST …/enable`, `POST …/disable`, with offset/limit pagination. Holds the scenes OpenAPI doc (including node and edge paths) and the router (`State<&'static Service>`). |
| `src/modules/scene/api_graph.rs` | REST for a scene's graph: `…/nodes`, `…/nodes/by-id/{node_id}`, `…/edges`, `…/edges/by-id/{edge_id}` (list, create, get, replace, delete). Request bodies are strict; node responses redact the token (`has_token` instead). |
| `src/modules/scene/graph.rs` | `nodes_in_cycles` (Tarjan's strongly connected components, self-loops included). |
| `src/modules/scene/service.rs` | Scenes, nodes and edges: models, `ServiceError` (`NotFound(what)`, `Invalid(details)`, `Conflict(message, details)`), and the service. Every change runs in one transaction under a lock; nodes are validated against their plugin (type, `config`, launch args), and whole-scene rules (edge direction by node type, no `block` queue policy on cycles) are checked before commit. Committed changes to enabled scenes go to the director; enabling checks that all plugins are installed. A remote node's token is kept when an update omits it. |
| `src/modules/scene/service/query_sqlite.rs` | SQLite queries for scenes, nodes and edges (they take `&mut SqliteConnection`, so they work inside transactions). |
| `src/modules/plugin.rs` | Plugin registry module: `init()` (run once at start-up from `main.rs`: prepares folders, checks installations) and `service()`. |
| `src/modules/plugin/config.rs` | `[plugins]` section: `dir` (default `./plugins`), `allow_upload` (default off), `admin_token`, upload and unpack limits. |
| `src/modules/plugin/manifest.rs` | `Manifest` (strict serde, `deny_unknown_fields`), its validation (id pattern, semver, safe paths, platform, payload schemas required by type), `launch_args_to_argv` (styles `flags` / `positional` / `json`, reserved bootstrap args) and `validate_launch_args` (JSON Schema + conversion rules). |
| `src/modules/plugin/package.rs` | ZIP inspection and extraction: zip-slip (absolute, `..`, drive-letter paths), symlinks, duplicates, entry count, unpacked size, compression ratio; manifest and referenced-file checks; extraction with real-size enforcement, sanitized permissions and the executable bit. All checks run before anything is written. |
| `src/modules/plugin/service.rs`, `service/query_sqlite.rs` | `Service`: install / upgrade / delete / get / list, serialized by a lock. Staging under `<dir>/.staging`, atomic folder renames, rollback on failure, `<dir>/.trash` for swapped-out packages. Upgrade re-validates every node's `launch_args`; delete is refused while nodes use the plugin. |
| `src/modules/plugin/api.rs` | REST: `GET /api/plugins`, `GET /api/plugins/by-id/{id}`, `GET …/launch-args-schema`, `POST /api/plugins` (multipart `file`), `PUT /api/plugins/by-id/{id}`, `DELETE …`. Mutating routes are gated (`allow_upload`, then admin token or loopback). Errors are `{error, details[]}`. |
| `src/modules/director.rs` | Director module: `init()` creates the command channel (`DirectorHandle`) at start-up; `service()` is the registry service. |
| `src/modules/director/handle.rs` | `DirectorHandle` (cloneable): `request_reload` / `request_stop` (queued, non-blocking, in order), `reload` and `status` (awaitable). `DirectorCommand`, `DirectorError`, `SceneStatus`. |
| `src/modules/director/service.rs` | The director as a registry service. For now a **no-op**: it receives commands and logs them; running scenes comes in later phases. |
| `src/db.rs` | The `main` DB (lazy), its embedded migrations, and `test_db()` for tests (a fully migrated DB in a temp file). |
| `resources/migrations/sqlite/main/` | Migrations for the main DB. `1.0.0-1` creates `scenes`. `1.0.0-2` creates the plugin registry (`plugins`: id, name, version, `"type"`, manifest and schema copies, install path, archive hash) and the scene graph: `scene_nodes` (plugin instances: `type` is SOURCE/SINK/TRANSFORM, `plugin_id`, JSON-object `config`) and `scene_edges` (directed handovers `source_node_id → target_node_id`, cycles and self-loops allowed, at most one per direction). Nodes reference `plugins (id, "type")` (`ON DELETE RESTRICT`, so a node's type must match its plugin's type, and a used plugin can't be deleted); `scene_edges.mapping` is optional JSON. Other foreign keys cascade on delete and keep both ends of an edge in the same scene. `1.0.0-3` adds `scenes.enabled` (BOOLEAN, default FALSE). |

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
| `src/plugin/frame.rs` | Public wire framing (see *Plugin model*): header lines (`Version`, `Msg-Code`) and their parsing rules, `Frame` (code + body), `read_frame`, `write_frame`, `write_message`, `encode_message`, `Frame::decode` (checks the code), and the `WireMessage` trait mapping each top-level message type to its `MsgCode`. Unit tests cover every header rule. |
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
| `http_sink/` (`flwrs-http-sink`) | Sink | Sends an HTTP request for each event, built with `reqwest`. Payload fields: `url` (string), `method` (string), `headers` (map → array of strings), `body` (string or bytes). CLI flags set the timeouts and verbose logging. Requests are sent fire-and-forget on a tokio task. Package files: `manifest.json`, `schema/launch_args.json`, `schema/in.json`; tests keep them in sync with the code (version, input schema, CLI flags). |

**Plugin packages.** A plugin is distributed as a ZIP with `manifest.json` at its root, the executable it points to, and the schema files it references (format in README.md, *Plugin packages*). Rust plugins in this repo are packaged with `cargo xtask package-plugin <dir>` → `target/plugins/<id>-<version>-<platform>.zip` (the host platform is written into the manifest). A Rust plugin that's packaged needs a `manifest.json` next to its `Cargo.toml`, with the same version.

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

[plugins]
dir = "./plugins"      # where packages are installed (<dir>/<plugin-id>/)
allow_upload = false   # upload/upgrade/delete endpoints; uploaded plugins are code this server runs
# admin_token = "…"    # if set: mutating endpoints need "Authorization: Bearer <token>"; if not: loopback clients only
# max_upload_bytes = 268435456; max_unpacked_bytes = 1073741824; max_entries = 10000; max_compression_ratio = 200
```

## Current state / known gaps

This is early-stage code. Keep these in mind before relying on the behaviour:

- **Hub:** the hub side of the plugin protocol (a TCP listener in `app` that checks `Initialize`, launches plugin processes and routes events between them) is not implemented yet (phases 4–6). The SDK side of the protocol (header lines, token, metadata, reconnect) is done and tested against a fake hub.
- **Director:** a no-op. It receives reload, stop and status commands from the scene and plugin services and logs them, but doesn't run scenes yet (phases 3–6 of `DIRECTOR_PLAN.tmp.md`).
- **Mappings:** only the JSON shape is validated. Dot paths aren't checked against payload schemas yet, Lua chunks aren't compiled yet, and nothing is evaluated (mapping phases).
- **Bulk graph edits:** each node or edge change is its own transaction, and reloads an enabled scene. To restructure a live scene in one step, disable it, edit, then enable it again.
- **Restarts:** a source whose `run` keeps failing is restarted every second indefinitely. There's no retry limit yet. Reconnects also retry forever (the hub's health check kills a local plugin that doesn't come back in time).
