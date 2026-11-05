# flwrs

<img src="flwrs-logo.svg" alt="flwrs logo" width="100" height="100">

A lightweight framework and server for running pluggable workflows, inspired by [Node-RED](https://nodered.org/).

With flwrs you build workflows (data flows) of any complexity from **pre-compiled plugins** and **configuration**, without writing code. The same runtime is meant to run both in the data centre and at the edge, for anything from simple automation to complex pipelines, such as running ML models as workflow steps.

> **Status:** early development. The server core, plugin SDK, plugin registry, scene management (graphs of nodes and edges) and a first plugin (HTTP sink) are in place. Workflow execution (the director and the plugin hub) is still to come.
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

See [AGENTS.md](AGENTS.md) for a file-by-file map.

## Building

Requirements: Rust 1.94+ (edition 2024). `protoc` is bundled through a build dependency, so you don't need to install it.

The repo is a Cargo workspace. Run commands from the root:

```sh
cargo build --workspace            # everything
cargo build -p flwrs-app           # server only
cargo build -p flwrs-http-sink     # HTTP sink plugin only
cargo test --workspace             # all tests
```

Binaries end up in `target/debug/` (or `target/release/`).

## Running the server

Create a `config.toml` (pass a different path with `--config`). Every section is optional: `[http]` defaults to `0.0.0.0:80`, `[logging]` to console output at INFO, and `[db.main]` to `main.db`.

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
```

```sh
cargo run -p flwrs-app -- --config config.toml
```

**For development**, use the checked-in `config.dev.toml` from the repo root:

```sh
cargo run -p flwrs-app -- --config config.dev.toml   # serves on 127.0.0.1:8080
```

It keeps everything the server writes at run time (database, installed plugins, spool files) under `./.data/`, which is gitignored, so runtime data never mixes with the source tree.

- REST API: `http://localhost:8080/api/scenes`
- Swagger UI: `http://localhost:8080/swagger-ui`

## Plugin protocol

The protocol is the contract between the server and a plugin, so plugins can be written in any language. The Rust SDK in `plugin/` is one implementation of it.

> The server-side hub is still being built (phases 4–6 of the director plan), so the hub's behaviour described here is the target, not yet running code.

### Process and connection

A node runs its plugin in one of two ways (the node's `config.launch`):

- **Local** (default): the server starts the plugin executable as a child process, in the plugin's install folder, with `--id <instance id> --host <hub host> --port <hub port> --log-level <trace|debug|info|warn|error>`, followed by the node's `launch_args`. A fresh token for this launch is in the `FLWRS_TOKEN` environment variable.
- **Remote:** an operator starts the plugin on another machine, with the same arguments pointing at the hub's reachable address, and the node's pre-shared token in `FLWRS_TOKEN` (or `--token`; the environment is preferred, since arguments show up in `ps`). Until TLS is supported, run remote links over a trusted network or a tunnel.

The plugin opens one TCP connection to `host:port`. It carries frames in both directions until shutdown.

### Framing

Every message is one frame. All integers are unsigned 32-bit little-endian:

```
+-------------+-------------+---------------------------+------------------------+
| packet_len  | header_len  | header (header_len bytes) | protobuf message bytes |
|   u32 LE    |   u32 LE    | "Name: value" lines       |                        |
+-------------+-------------+---------------------------+------------------------+
              |<----------------------- packet_len bytes ----------------------->|
```

- `packet_len` counts everything after itself: `4 + header_len + message length`. The reference SDK rejects frames larger than 256 MiB, and the hub has a lower per-event limit (16 MiB by default).
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
   - Accepted: the hub answers `InitializeResponse`.
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

> **Security:** an uploaded plugin is code the server will run. Upload, upgrade and delete are **off by default** (`[plugins] allow_upload = true` turns them on). When they're on, callers need `Authorization: Bearer <[plugins] admin_token>`, or, if no token is configured, must connect from loopback. `config.dev.toml` enables uploads from loopback for development.

## Writing a plugin

**In Rust:** create a binary crate under `plugins/`, add it to `members` in the root `Cargo.toml`, depend on `flwrs-plugin`, implement `Sink`, `Source` or `Transform`, declare the plugin's schema, and run the matching runner (`SinkRunner`, `SourceRunner`, `TransformRunner`). The runner handles the connection (token from `FLWRS_TOKEN` or `ConnectionConfig::with_token`), reconnecting, logging, error reporting, metadata propagation and shutdown. A source emits events through the `LocalSink` handle it receives in `initialize`. [`plugins/http_sink`](plugins/http_sink) is a complete example, including its package files. Package it with `cargo xtask package-plugin <dir>`.

**In any other language:** build an executable that follows the [plugin protocol](#plugin-protocol). Generate message types from `plugin/schema/*.proto`, implement the framing and header lines (a few dozen lines in most languages), and follow the lifecycle and reconnect rule above.

## Contributing with AI agents

LLM agent instructions live in [AGENTS.md](AGENTS.md), not in tool-specific files such as `CLAUDE.md`.

## License

[Apache 2.0](LICENSE)
