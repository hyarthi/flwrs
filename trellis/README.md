# flwrs-trellis

Developer toolkit for [flwrs](https://github.com/hyarthi/flwrs) plugins, a lightweight server for pluggable workflows (data flows). Today it's the conformance suite: it runs a plugin, written in any language, against a fake hub and checks that it follows the [plugin protocol](https://github.com/hyarthi/flwrs/blob/main/docs/protocol.md) (version 2).

> **Status:** early development. More plugin-development tools will follow; expect breaking changes.

## Install and run

```sh
cargo install flwrs-trellis
flwrs-trellis path/to/unzipped-package          # every case
flwrs-trellis path/to/unzipped-package --list   # the cases
flwrs-trellis path/to/unzipped-package --case handshake -- --extra plugin-arg
```

The package folder is an unzipped flwrs plugin package, with `manifest.json` at its root. Each case starts the plugin afresh with the usual bootstrap arguments (`--id`, `--host`, `--port`, `--log-level`, the token in `FLWRS_TOKEN`) and checks one part of the protocol:

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

The exit status is 1 if a case fails, so it fits in CI.

## As a library

```rust,ignore
use flwrs_trellis::{Target, report, run};

let target = Target::from_package(std::path::Path::new("path/to/package"))?;
let results = run(&target, &[]).await; // an empty list runs every case
println!("{}", report(&results));
```

## License

Apache-2.0
