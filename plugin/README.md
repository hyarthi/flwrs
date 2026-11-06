# flwrs-plugin

The plugin protocol and reference Rust SDK for [flwrs](https://github.com/hyarthi/flwrs), a lightweight server for pluggable workflows (data flows), inspired by Node-RED.

A flwrs plugin is a standalone executable that the server launches (or that runs on another machine) and that connects back to the server's hub over TCP. This crate gives you:

- the protocol messages, generated from [`schema/*.proto`](schema) (sources, transforms, sinks, and the typed payload model);
- the wire framing (`plugin::frame`): length-prefixed frames with `Version` / `Msg-Code` header lines;
- the `Source`, `Transform` and `Sink` traits, and runners (`SourceRunner`, `TransformRunner`, `SinkRunner`) that handle the connection, `Initialize`, reconnecting with backoff, logging to the hub, error reporting, event metadata and shutdown.

> **Status:** early development. The protocol is at version `1.0.0` but not yet released as stable; expect breaking changes.

## Writing a sink

```rust,ignore
use flwrs_plugin::plugin::core::{ConnectionConfig, InitializeRequest};
use flwrs_plugin::plugin::error::{InitializeError, ShutdownError, SinkError};
use flwrs_plugin::schema::common::log_level::Enum as LogLevel;
use flwrs_plugin::schema::sink::SinkEvent;
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
    fn consume_event(&mut self, event: SinkEvent) -> Result<(), SinkError> {
        println!("{:?}", event.payload);
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
    let config = SinkRunnerConfig {
        plugin_id: arg("--id"),
        log_level: LogLevel::Info,
        hub_connection: ConnectionConfig::new(arg("--host"), arg("--port").parse()?),
    };
    SinkRunner::initialize(Printer, config).await?.run().await?;
    Ok(())
}
```

(`arg` stands for your command-line parsing, e.g. with `clap`.) The flwrs repository's [`plugins/http_sink`](https://github.com/hyarthi/flwrs/tree/main/plugins/http_sink) is a complete example, including its package files.

## Other languages

The contract is the protocol, not this crate: any executable that speaks the framing and the messages in `schema/` is a plugin. The protocol is specified in the flwrs [README](https://github.com/hyarthi/flwrs#plugin-protocol), and the repository has minimal examples in Python, Go, TypeScript and Java.

## License

Apache-2.0
