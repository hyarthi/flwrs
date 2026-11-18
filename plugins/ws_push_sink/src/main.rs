use clap::Parser;
use flwrs_plugin::sink::runner::{SinkRunner, SinkRunnerConfig};
use flwrs_plugin_kit::broadcast::Broadcaster;
use flwrs_plugin_kit::server::Server;
use flwrs_ws_push_sink::config::Args;
use flwrs_ws_push_sink::plugin::PushSink;
use flwrs_ws_push_sink::server::{App, routes};
use std::process::ExitCode;
use std::sync::Arc;

fn fail(problems: Vec<String>) -> ExitCode {
    for problem in problems {
        eprintln!("{problem}");
    }
    ExitCode::from(2)
}

fn main() -> ExitCode {
    let args = Args::parse();
    // `--threads` (optional, from the server) sizes the runtime.
    match flwrs_plugin::rt::runtime(args.bootstrap.threads) {
        Ok(runtime) => runtime.block_on(run(args)),
        Err(e) => {
            eprintln!("Failed to start the runtime: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn run(args: Args) -> ExitCode {
    let settings = match args.settings() {
        Ok(s) => s,
        Err(problems) => return fail(problems),
    };
    let broadcaster = Broadcaster::new(settings.broadcast.config.clone());
    let app = Arc::new(App {
        broadcaster: broadcaster.clone(),
        frame: settings.frame,
        subprotocols: settings.subprotocols.clone(),
        allowed_origins: settings.allowed_origins.clone(),
        ping_interval: settings.ping_interval,
        write_timeout: settings.write_timeout,
        max_client_message_bytes: settings.max_client_message_bytes,
        read_buffer_bytes: settings.read_buffer_bytes,
        write_buffer_bytes: settings.write_buffer_bytes,
    });
    let router = routes(app, &settings.base_path);
    let server = match Server::start(settings.server.clone(), router, Arc::new(|| true)).await {
        Ok(server) => server,
        Err(e) => {
            return fail(vec![format!(
                "listening on {}: {e}",
                settings.server.listen
            )]);
        }
    };
    // Connections close when the server stops, however it stops (upgraded
    // connections are the plugin's, not the server core's).
    let mut stopping = server.stopping();
    let closing = broadcaster.clone();
    tokio::spawn(async move {
        if stopping.wait_for(|s| *s).await.is_ok() {
            closing.close();
        }
    });
    let plugin = PushSink {
        broadcaster,
        ack_mode: settings.broadcast.ack_mode,
        server: Some(server),
    };
    let config = SinkRunnerConfig::new(
        args.bootstrap.id.clone(),
        args.bootstrap.log_level(),
        args.bootstrap.connection(),
    )
    .with_max_unacked(settings.max_in_flight as u32)
    .with_max_concurrent(settings.max_in_flight);
    let mut runner = match SinkRunner::initialize(plugin, config).await {
        Ok(runner) => runner,
        Err(err) => {
            eprintln!("Failed to initialize the sink runner: {err}");
            return ExitCode::FAILURE;
        }
    };
    match runner.run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            log::error!("Plugin stopped with an error: {err}");
            ExitCode::FAILURE
        }
    }
}

/// Keeps the package files in sync with the code.
#[cfg(test)]
mod package_tests {
    use clap::CommandFactory;
    use flwrs_plugin::cli::Bootstrap;
    use flwrs_plugin::package::{LaunchSpec, Manifest};
    use flwrs_ws_push_sink::config::Args;
    use serde_json::{Value, json};
    use std::collections::BTreeSet;

    fn file(path: &str) -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(path)
    }

    fn json(path: &str) -> Value {
        serde_json::from_str(&std::fs::read_to_string(file(path)).unwrap()).unwrap()
    }

    /// The launch-args schema: the push sink's own settings, then the
    /// broadcast core's and the server core's. `FLWRS_UPDATE=1` writes it.
    fn expected_schema() -> Value {
        let mut props = json!({
            "base_path": {
                "title": "Base path", "x-flwrs-order": 1,
                "description": "A prefix for the route: <base>/ws/{channel} (?after=<id> resumes from the replay ring).",
                "type": "string", "default": "" },
            "frame": {
                "title": "Messages", "x-flwrs-order": 2,
                "description": "How an event becomes a message: auto (text if the data is UTF-8, else binary), text, binary, or envelope: JSON text {id, event, content_type, data, encoding?}, with {\"type\": \"gap\", \"missed\"} markers, so clients can resume (?after=<id>) and see what they missed.",
                "enum": ["auto", "text", "binary", "envelope"], "default": "auto" },
            "subprotocols": {
                "title": "Subprotocols", "x-flwrs-order": 3, "x-flwrs-advanced": true,
                "description": "The subprotocols the server speaks (Sec-WebSocket-Protocol), in order of preference.",
                "type": "array", "items": { "type": "string" }, "x-flwrs-launch": { "via": "file" } },
            "allowed_origins": {
                "title": "Allowed origins", "x-flwrs-order": 4,
                "description": "The Origins browsers may connect from, or [\"*\"]; empty: any. Clients without an Origin (not browsers) aren't affected.",
                "type": "array", "items": { "type": "string" }, "x-flwrs-launch": { "via": "file" } },
            "ping_interval_s": {
                "title": "Ping interval (s)", "x-flwrs-order": 5, "x-flwrs-advanced": true,
                "description": "The server pings each client this often; a client that has sent nothing since the last ping is closed.",
                "type": "integer", "minimum": 1, "default": 30 },
            "write_timeout_ms": {
                "title": "Write timeout (ms)", "x-flwrs-order": 6, "x-flwrs-advanced": true,
                "description": "A message not written within this long (a stuck client) closes the connection.",
                "type": "integer", "minimum": 1, "default": 10000 },
            "max_client_message_bytes": {
                "title": "Largest client message", "x-flwrs-order": 7, "x-flwrs-advanced": true,
                "description": "Client messages are ignored; larger ones close the connection (1009).",
                "type": "integer", "minimum": 128, "default": 4096 },
            "max_in_flight": {
                "title": "Events in flight", "x-flwrs-order": 8, "x-flwrs-advanced": true,
                "description": "Events handled at once (with ack_mode delivered: waiting for a subscriber at once); the hub holds the rest.",
                "type": "integer", "minimum": 1, "default": 64 },
            "read_buffer_bytes": {
                "title": "Read buffer (bytes)", "x-flwrs-order": 9, "x-flwrs-advanced": true,
                "description": "Per connection, allocated up front: clients only send pings and closes here, so a small one saves memory with many clients.",
                "type": "integer", "minimum": 256, "default": 4096 },
            "write_buffer_bytes": {
                "title": "Write buffer (bytes)", "x-flwrs-order": 10, "x-flwrs-advanced": true,
                "description": "Per connection: how much is gathered before a write. It grows to fit a larger message (and stays that large for the connection), so large events cost memory per client whatever this is; 0 writes each message at once.",
                "type": "integer", "minimum": 0, "default": 16384 }
        });
        for part in [
            flwrs_plugin_kit::broadcast::schema_properties(),
            flwrs_plugin_kit::server::schema_properties(),
        ] {
            for (k, v) in part.as_object().unwrap() {
                props[k] = v.clone();
            }
        }
        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "title": "WebSocket push sink settings", "type": "object", "additionalProperties": false,
            "properties": props
        })
    }

    #[test]
    fn launch_args_schema_is_up_to_date() {
        let expected = expected_schema();
        if std::env::var("FLWRS_UPDATE").is_ok() {
            let mut text = serde_json::to_string_pretty(&expected).unwrap();
            text.push('\n');
            std::fs::write(file("schema/launch_args.json"), text).unwrap();
        }
        assert_eq!(
            json("schema/launch_args.json"),
            expected,
            "run with FLWRS_UPDATE=1 to rewrite it"
        );
    }

    #[test]
    fn manifest_and_in_schema_match_the_crate() {
        let manifest = json("manifest.json");
        assert_eq!(manifest["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(manifest["type"], "SINK");
        assert_eq!(
            manifest["executable"],
            format!("bin/{}", env!("CARGO_PKG_NAME"))
        );
        let file_schema =
            flwrs_plugin::schema::rules::parse_json(&json("schema/in.json").to_string()).unwrap();
        assert!(
            flwrs_plugin::schema::rules::diff(
                &file_schema,
                &flwrs_plugin_kit::broadcast::in_schema()
            )
            .is_empty()
        );
    }

    #[test]
    fn launch_args_schema_matches_the_cli() {
        let manifest: Manifest = serde_json::from_value(json("manifest.json")).unwrap();
        let schema = json("schema/launch_args.json");
        let (spec, report) = LaunchSpec::new(&manifest, Some(&schema));
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
        let from_schema: BTreeSet<String> = spec.fields().map(|f| f.target.clone()).collect();
        let from_cli: BTreeSet<String> = Args::command()
            .get_arguments()
            .filter_map(|a| a.get_long().map(str::to_string))
            .filter(|l| !Bootstrap::NAMES.contains(&l.as_str()))
            .collect();
        assert_eq!(from_schema, from_cli);
    }
}
