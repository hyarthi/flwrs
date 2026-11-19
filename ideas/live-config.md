# Idea: applying config changes without a restart

Status: idea, not scheduled. Written after the web UI's server page, which saves the config file but applies every change with a restart.

## Today

Every section is read once at start: `flwrs_core::config::main_config()` is a lazy global, and each module copies its section into its own state at init (`HubConfig` into the hub, `DirectorConfig` into the engine, `PluginsConfig` into the plugin service, the logger built once, the telemetry sinks spawned once). A saved file changes nothing until the restart, which stops every scene and loses the events in memory and spill queues.

## What it would take

### The foundation (needed by everything else)

1. **One source of truth for settings that can change:** a settings registry with a `watch` channel (or `ArcSwap`) per section, filled from the file at start and again on "apply". Every reader of a live setting reads the current value from it at the point of use, instead of a copy taken at init; services that must *act* on a change (the logger, telemetry, the hub's budget) subscribe. `main_config()` stays for what's read once (the runtime, the database).
2. **A change plan:** the effective values of the running config and of the saved file, compared per key (the server page already has both as JSON, from the same structs), and each changed key classified as *live*, *reload* (applied by reloading the affected scenes, the director's existing diff reload) or *restart*. The page shows the plan before saving ("3 settings apply now, 1 needs a restart") and offers **Apply** (live + reload) and **Restart** (everything).
3. **Apply as a unit:** every live change validated first (the checks the page already runs), then applied section by section; a section that fails to apply rolls back the ones already applied, and the running config only moves to the new values once all have. Risky live changes (the HTTP address) go through the same confirm-or-revert as restarts.
4. **Restart-only keys stay restart-only:** the page marks them, and "Apply" leaves them for the next restart (the file says one thing, the server runs another: shown as now, "restart needed").

### Per section

| Section | Live (read at use, swap the value) | Needs work | Restart only |
|---|---|---|---|
| `[server]` | `restart`, `confirm_timeout_s` (read when a restart happens: trivial) | | |
| `[scenes]` | upload and import limits (read per request) | `stream_interval_ms` (open streams keep their ticker; new ones get it, or tickers re-read it) | |
| `[plugins]` | `max_unpacked_bytes`, `max_entries`, `max_compression_ratio`, `configure_*`, `remote_configurator_urls` (read per use) | `max_upload_bytes` (today an axum `DefaultBodyLimit` layer built with the router: move to a per-request check) | `dir` (and the locked `allow_upload`, `admin_token`: could be live, kept for the file) |
| `[logging]` | `level` (`log::set_max_level` + the logger's own level) | `sinks`, `id`, `format`: `log::set_logger` is once per process, so the installed logger becomes a forwarder to a swappable `LogLogger`, rebuilt on apply (file handles reopened) | |
| `[director]` | `memory_soft_inflight_bytes`, `spill_max_bytes`, `spill_min_free_bytes` (the shared budgets' limits as atomics) | `max_hops`, `plugin_threads`, `spill_default`: a **reload** of the enabled scenes (the diff reload already handles a changed hop limit cheaply; changed cores restart the affected local nodes; a changed spill default changes queue kinds, so those nodes' queues restart and memory queues lose their events: the plan says so) | `spool_dir`, `nodes_dir`, `files_dir` (data would have to move) |
| `[hub]` | `handshake_timeout_ms`, `max_handshake_bytes`, `max_event_bytes`, `max_pending_*`, `default_max_unacked` / `max_unacked`, `credit_window`, `heartbeat_interval_ms`: new connections get them (existing ones keep what they negotiated until they reconnect; a forced reconnect could apply them everywhere) | `max_inflight_bytes` (resize the hub-wide budget: shrinking waits for usage to fall); `[hub.tls]` certificates (see `tls-cert-hot-reload.md`) | `host`, `port` (local plugins are given the port at launch; every node would reconnect or relaunch) |
| `[telemetry]` | | the telemetry service respawns its sinks with the new section (OTLP cumulative series restart with a new start time; file sinks reopen their partitions) | |
| `[http]` | `swagger_ui` (a per-request toggle on the routes) | `host`, `port`, `enabled`: bind the new listener before closing the old, behind confirm-or-revert (the page's own connection moves) | |
| `[http.auth]` (locked) | `session_ttl`, `allowed_origins`, `secure_cookie` (behind a swap) | `secret_key`: every session ends (the page re-asks for the key) | |
| `[runtime]` | | | `threads` (the tokio runtime is built once) |
| `[db.main]` | | | `filename` |

## Suggested order

1. The foundation, plus the trivial ones: `[server]`, `[scenes]` and `[plugins]` limits, the logging level, the director's budgets, `[http] swagger_ui`. Most changes people make day to day are here.
2. Reload-based: the director's `max_hops` / `plugin_threads` / `spill_default`, with the plan saying which scenes reload and what that costs.
3. Rebuilds: logger sinks, telemetry sinks, the hub's per-connection limits (+ an optional "reconnect all"), `max_inflight_bytes`.
4. Maybe later, behind confirm-or-revert: the HTTP listener, auth settings.

## Risks

- Two sources of truth while it's half done: every reader of a live key must move to the registry, or a change silently doesn't apply (a test per live key: change it, apply, observe).
- Partial applies: the rollback must restore the old values in the same order.
- Behaviour mid-flight: in-progress requests and connections keep the old values; the plan and the docs must say so.
