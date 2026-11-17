# flwrs-trellis

Developer toolkit for [flwrs](https://github.com/hyarthi/flwrs) plugins, a lightweight server for pluggable workflows (data flows). It works for plugins in any language:

- **`package`** builds a plugin and packages it as a ZIP the server accepts. The ZIP is checked with the server's own rules.
- **`audit`** runs the plugin against a fake hub and checks that it follows the [plugin protocol](https://github.com/hyarthi/flwrs/blob/main/docs/protocol.md) (version 2).
- **`test`** runs behavioural end-to-end tests: send events, trigger sources, and check ACKs, NACKs and outputs, through redeliveries, disconnects and restarts.
- **`bench`** measures the plugin's throughput, latency, startup time, CPU and memory.

> **Status:** early development; expect breaking changes. 0.0.1 was the protocol audit alone (then the `flwrs-trellis <package>` command); 0.0.2 adds `package`, `test` and `bench`, and the audit is now `flwrs-trellis audit <package>` (in Rust, `Audit::new(target).run()` instead of `run(&target, &[])`). 0.0.3 reads manifest version 2 packages and runs plugins as the server now does: in a node folder (a link to each of the package's top-level entries, and `config/` with the files the server delivers), with launch args delivered as arguments, environment variables or files, the node's own payload schemas, and uploaded files (`[uploads]`, `uploads` in test files, `--upload`, `Target::with_upload`). It needs `flwrs-plugin` 0.0.6. 0.0.4 fixes two bugs: `package` now includes the files the launch-args schema names (a code field's `definitions`), and `test` no longer matches a new connection's events with the previous connection's answers and outputs (after a `disconnect` or `restart` step). It needs `flwrs-plugin` 0.0.7. It speaks plugin protocol version 2.

## Install

```sh
cargo install flwrs-trellis --locked
flwrs-trellis --help
```

In a Rust plugin crate, you can use it as a dev-dependency instead, pinned by your `Cargo.lock` (see [As a library](#as-a-library)).

## `package`

```sh
flwrs-trellis package path/to/plugin                      # → <cargo target dir>/plugins/<id>-<version>-<platform>.zip
flwrs-trellis package path/to/plugin --target aarch64-unknown-linux-gnu   # cross-build (e.g. a Raspberry Pi)
flwrs-trellis package path/to/plugin --flavor edge --debug --out dist/
```

How the plugin is built depends on what its folder contains:

- **A Rust crate** (`Cargo.toml`): `cargo build` (release unless `--debug`). The crate's version must equal `manifest.json`'s, and the manifest's `platforms` becomes the build's platform.
- **A `[build]` section in `trellis.toml`**: its command runs, and its `executable` goes into the package. The command gets `TRELLIS_PROFILE`, `TRELLIS_FLAVOR` and `TRELLIS_TARGET` in its environment.
- **Neither**: the manifest's executable goes in as it is, e.g. a script.

The package holds the manifest, the executable (mode 0755), the files the manifest references, and the `[package] include` patterns. It's reproducible: the same input gives the same bytes.

## `audit`

```sh
flwrs-trellis audit path/to/package.zip             # every case
flwrs-trellis audit path/to/plugin --list           # the cases
flwrs-trellis audit path/to/plugin --case handshake -- --extra plugin-arg
```

The argument is a package ZIP or a plugin folder. A folder that needs a build is packaged first, so the run uses what would ship. Each case starts the plugin afresh with the usual bootstrap arguments (`--id`, `--host`, `--port`, `--log-level`, and the token in `FLWRS_TOKEN`), as the server would:

- **Working directory:** a temporary node folder with a link to each top-level entry of the package, so relative paths to the package's files work.
- **Launch args** (`--launch-args <file>` or `[audit] launch_args`): checked against the manifest's schema and delivered as the manifest says: command-line arguments, environment variables, or files in the node folder's `config/` (manifest version 2). A plugin that takes its payload schemas from its launch args (manifest version 2) is held to the schemas in them.
- **Uploaded files** that the launch args name (`"source": "upload"`): from `[uploads]` in `trellis.toml` or `--upload NAME=PATH`, linked into `config/`.

The cases:

| Case | Checks |
|---|---|
| `handshake` | `Initialize` first, with the right id, token, type, version, protocol range and schemas |
| `waits-for-acceptance` | no data, acks or pings before `InitializeResponse` |
| `answers-ping` | `PING` is answered with `PONG` and the same nonce |
| `data-events` | events flow by the rules: seq numbers, credit, acks, `input_seq` |
| `shuts-down` | `SHUTDOWN` is answered with `EXIT`, then the process exits |
| `stops-when-rejected` | `SHUTDOWN` instead of `InitializeResponse`: exits, no reconnect |
| `reconnects` | after a dropped connection: reconnects with the same `Initialize` |
| `closes-on-violation` | a frame with an unknown framing version closes the connection |

A source that only emits when something outside happens (an HTTP request, an MQTT message) needs a **stimulus**, otherwise `data-events` is skipped. Set one in `trellis.toml` (`[audit] stimulus`) or, in Rust, pass one to `Audit::stimulus`.

The exit status is 1 if a case fails, so it fits in CI.

## `test`

```sh
flwrs-trellis test path/to/plugin                              # trellis.toml [test] files, else tests/*.test.toml
flwrs-trellis test path/to/plugin tests/upper.test.toml --var mock=8081
flwrs-trellis test path/to/package.zip                         # no test files: a smoke test
flwrs-trellis test path/to/plugin --update                     # write actual outputs as the expected ones
flwrs-trellis test path/to/plugin --format junit > report.xml
```

A test file is TOML. Payloads live in JSON files next to it, as plain JSON shaped by the plugin's schema (`{"text": "hi", "n": 3}`):

```toml
name = "uppercases the text"
launch_args = "node.json"            # the node's launch_args (JSON), optional
uploads = { "model.onnx" = "model.onnx" }   # files uploaded to the node (on top of [uploads]), optional

[[steps]]
send = "payloads/hello.json"
expect_ack = "ack"
expect_outputs = [{ payload = "expected/hello.json" }]

[[steps]]
send = "payloads/hello.json"
expect_outputs = [{ path = "text", eq = "HELLO" }, { count = 1 }]

[[steps]]
send = "payloads/bad.json"
expect_ack = { nack = { retryable = false } }

[[steps]]
redeliver = 1                        # step 1's event again: same event id, attempt + 1
expect_outputs = [{ same_as_step = 1 }]

[[steps]]
restart = true                       # kill the plugin and start it again

[[steps]]
stimulus = ["curl", "-s", "-d", "{}", "http://127.0.0.1:${port}/in"]   # sources
expect_outputs = [{ match = "subset", payload = "expected/in.json" }]
```

- **Actions**, one per step:
  - `send`: send a payload file (sinks, transforms).
  - `redeliver = N`: send step N's event again.
  - `stimulus`: a command, or `true` for the stimulus given in code.
  - `wait_ms`: wait.
  - `grant = N`: give N credits.
  - `disconnect`: drop the connection and wait for the plugin to reconnect.
  - `restart`: kill the plugin and start it again.
- **`expect_ack`**: `"ack"`, `{ nack = {} }`, `{ nack = { retryable = … } }`, `"none"` or `"any"`.
- **`expect_outputs`**:
  - Positional items: the first item checks the first output, and so on.
    - `payload`: compare with a file, `match = "exact"` (the default) or `"subset"`.
    - `path` with `eq`, `ne`, `gt`, `ge`, `lt`, `le`, `contains` or `exists`: check a value inside the output. Paths are dot paths: `a.b`, `items[0]`, and quoted keys for unusual names, `paths["/devices/{id}"].post.body.temp`.
  - `count`: how many outputs came.
  - `same_as_step = N`: the outputs equal step N's.
  - Without `count`, the number of positional items is the number of outputs expected.
  - Outputs are always checked against the out schema.
- **Settings** for the whole file: `args`, `env`, `setup` / `teardown` commands, `initial_credit`, `return_credit` and `within_ms` (how long a step waits).
- **Variables**: `${port}` is a free port for the run (also in `TRELLIS_PORT`). `${name}` comes from `--var name=value` or `Tester::var`. Both are replaced in payload files, launch args, args and commands.
- **Payload files**: a file named `*.pb.json` holds proto3 JSON of a `PluginPayload` instead of plain JSON. In plain JSON, DATETIME values are RFC 3339 strings, and BYTES are a UTF-8 string or an array of numbers.

## `configure`

For plugins with a **configurator** (manifest `configurator`: a program that sets some of a node's settings from its others; see the plugin specification). `configure` runs it the way the server does, on `--launch-args` (and `--upload` files), and prints its result:

```sh
flwrs-trellis configure path/to/plugin --launch-args node.json
```

`with_launch_args` (and so `test`, `audit` and `bench`) run the configurator too, so the plugin gets what it would get on the server. Values the launch args give for configured fields are refused, as the server refuses them.

Configurator tests are `*.configure.toml` files, run by `test` next to the behavioural ones:

```toml
# tests/devices.configure.toml
name = "derives the out schema"
launch_args = "devices.json"                 # the node's settings (configured fields left out)
uploads = { "api.yaml" = "api.yaml" }        # optional
expect = "ok"                                # or "error"
values = "expected/devices.values.json"      # optional: the configured values, exactly (--update writes them)
warnings_contain = ["webhooks"]              # optional
# errors_contain = ["oneOf"]                 # with expect = "error"
```

In Rust, `Target::with_configurator_executable(env!("CARGO_BIN_EXE_<name>-config"))` runs the configurator cargo built, and `Target::configure(&args)` runs it alone.

## `bench`

```sh
flwrs-trellis bench path/to/plugin                          # trellis.toml [bench] files, else benches/*.bench.toml, else a default closed loop
flwrs-trellis bench path/to/plugin --format json > base.json
flwrs-trellis bench path/to/plugin --baseline base.json --max-regression 10
```

```toml
name = "small events"
payloads = ["payloads/small.json"]     # cycled; default: a sample from the in schema
warmup_ms = 1000
duration_ms = 5000
repetitions = 3

[load]
model = "closed"      # closed: keep in_flight events in flight; open: a fixed rate; step: rising rates until a limit
in_flight = 64
# rate = 1000
# start_rate = 100, step_rate = 100, step_ms = 2000, max_rate = 100000, max_p99_ms = 50
```

**What it measures:**
- **Latency.** For sinks and transforms: from sending an event to the plugin's ACK or NACK, so the plugin needs the ACKS capability. In open and step loops a thread paces the sends (an OS sleep: tokio's timer would wake about a millisecond late). When the sender slept until an event's time, latency counts from when it woke, so the harness's own timer isn't charged to the plugin (it's reported apart, as `pacing_us`: "harness late"). When the sender was already behind because the plugin's window was full, latency counts from when the event was meant to go out, so a stalled plugin can't hide its delays. For sources: events per second against a credit window, and with a stimulus, latency from stimulus to event.
- **Startup**: from launch until the plugin is ready.
- **Resources**: CPU and memory of the plugin's process tree, sampled every 100 ms.

Each run starts with a **calibration** against a no-op peer, which shows the fake hub's own ceiling. Results are reported per repetition and as medians with their spread. The report warns about conditions that make numbers unreliable, such as a debug build or a CPU frequency governor that isn't set to `performance`. `--baseline` compares against earlier JSON results and exits 1 on a regression. Shared CI runners are noisy, so use it to catch large regressions.

This benchmarks the plugin alone. The server's own costs (queues, mappings, other plugins) aren't included.

## `trellis.toml`

Optional settings for the plugin's developer, kept next to `manifest.json`:

```toml
[build]                          # not needed for a Rust crate
command = ["./build.sh"]
executable = "dist/plugin.py"

[package]
include = ["models/**"]
exclude = ["**/__pycache__/**"]

[flavors.edge]                   # flwrs-trellis package --flavor edge
features = []
no_default_features = true
manifest = "manifest.edge.json"

[uploads]                        # files uploaded to the node, for "source": "upload" launch args
"model.onnx" = "tests/model.onnx"

[audit]
launch_args = "tests/node.json"
args = []
stimulus = ["curl", "-s", "-d", "{}", "http://127.0.0.1:${port}/in"]

[test]
files = ["tests/*.test.toml"]

[bench]
files = ["benches/*.bench.toml"]
```

## As a library

Everything above is available as a library, for example in a Rust plugin's own integration tests, with trellis as a dev-dependency:

```toml
[dev-dependencies]
flwrs-trellis = "0.0.4"
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

```rust,ignore
// tests/trellis.rs
use flwrs_trellis::{Audit, Outcome, StimulusCtx, Target, Tester};
use std::path::Path;

fn target() -> Target {
    Target::from_package(Path::new(env!("CARGO_MANIFEST_DIR")))
        .unwrap()
        .with_executable(env!("CARGO_BIN_EXE_my-plugin")) // the binary cargo built for this test
}

#[tokio::test]
async fn follows_the_protocol() {
    let results = Audit::new(target()).run().await;
    assert!(results.iter().all(|r| r.outcome == Outcome::Pass), "{}", flwrs_trellis::report(&results));
}

#[tokio::test]
async fn behaves() {
    let mut tester = Tester::new(target())
        .var("mock", "8081")
        // Sources: a stimulus is any async closure; returning an id lets `correlate` find its event.
        .stimulus(|ctx: StimulusCtx| async move { trigger(ctx.url("/in")).await.map(|_| None) });
    let result = tester.run_file(Path::new("tests/basic.test.toml")).await;
    assert_eq!(result.outcome, Outcome::Pass, "{}", flwrs_trellis::testing::report(&[result.clone()]));
}
```

To get the command at the version your lockfile pins, without a global install, add a one-line example and an alias:

```rust,ignore
// examples/trellis.rs
fn main() -> std::process::ExitCode { flwrs_trellis::cli::main() }
```

```toml
# .cargo/config.toml
[alias]
trellis = "run --quiet --example trellis --"
```

Then `cargo trellis package .`, `cargo trellis bench .` and so on work in that repo.

## License

Apache-2.0
