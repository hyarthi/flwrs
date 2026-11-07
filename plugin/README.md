# flwrs-plugin

The plugin protocol and reference Rust SDK for [flwrs](https://github.com/hyarthi/flwrs), a lightweight server for pluggable workflows (data flows), inspired by Node-RED.

A flwrs plugin is a standalone executable that the server launches (or that runs on another machine) and that connects back to the server's hub over TCP. This crate gives you:

- the protocol messages, generated from [`schema/*.proto`](schema) (`flwrs.protocol`, and the typed payload model `flwrs.payload`);
- the wire framing (`plugin::frame`): length-prefixed frames with a protobuf `FrameHeader`;
- the `Source`, `Transform` and `Sink` traits, and runners (`SourceRunner`, `TransformRunner`, `SinkRunner`) that handle the connection, `Initialize`, heartbeats, credit-based flow control, acknowledgements, reconnecting with backoff, logging to the hub and shutdown;
- with the `tls` feature (on by default), connecting to the hub's TLS listener, with an optional client certificate (`ConnectionConfig::with_tls`, or the `FLWRS_TLS_*` environment variables).

> **Status:** early development. This version speaks plugin protocol version 2 (framing version 2), which isn't compatible with 0.0.2 and earlier. Expect breaking changes until it's released as stable.

## Writing a sink

```rust,ignore
use flwrs_plugin::plugin::core::{ConnectionConfig, InboundEvent, InitializeRequest};
use flwrs_plugin::plugin::error::{InitializeError, ShutdownError, SinkError};
use flwrs_plugin::schema::protocol::log_level::Enum as LogLevel;
use flwrs_plugin::sink::plugin::Sink;
use flwrs_plugin::sink::runner::{SinkRunner, SinkRunnerConfig};

struct Printer;

impl Sink for Printer {
    fn initialize(&mut self, id: String, _: LogLevel) -> Result<InitializeRequest, InitializeError> {
        // Declare the payload this sink accepts; it must match the package's manifest.
        Ok(InitializeRequest::new()
            .with_id(id)
            .with_version(self.version())
            .with_in_schema(/* a SchemaDefinition */))
    }
    // Ok (when the future finishes) acknowledges the event; an error rejects
    // it (SinkError::retryable: the hub delivers it again later). Several
    // events may run at once with SinkRunnerConfig::with_max_concurrent.
    async fn consume_event(&self, event: InboundEvent) -> Result<(), SinkError> {
        println!("{:?} (attempt {})", event.payload, event.attempt);
        Ok(())
    }
    fn shutdown(&mut self) -> Result<(), ShutdownError> {
        Ok(())
    }
    fn version(&self) -> String {
        env!("CARGO_PKG_VERSION").into()
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // The server passes --id, --host, --port and --log-level; the token is in FLWRS_TOKEN.
    let config = SinkRunnerConfig::new(
        arg("--id"),
        LogLevel::Info,
        ConnectionConfig::new(arg("--host"), arg("--port").parse()?),
    );
    SinkRunner::initialize(Printer, config).await?.run().await?;
    Ok(())
}
```

(`arg` stands for your command-line parsing, e.g. with `clap`.) The flwrs repository's [`plugins/http_sink`](https://github.com/hyarthi/flwrs/tree/main/plugins/http_sink) is a complete example, including its package files.

## Other languages

The contract is the protocol, not this crate: any executable that speaks the framing and the messages in `schema/` is a plugin. The protocol is specified in [`docs/protocol.md`](https://github.com/hyarthi/flwrs/blob/main/docs/protocol.md), the repository has minimal examples in Python, Go, TypeScript and Java, and a conformance suite (`flwrs-trellis`) checks a plugin in any language against it.

## License

Apache-2.0
