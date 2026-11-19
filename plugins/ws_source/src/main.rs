use clap::Parser;
use flwrs_plugin::source::runner::{SourceRunner, SourceRunnerConfig};
use flwrs_plugin_kit::server::Server;
use flwrs_ws_source::config::Args;
use flwrs_ws_source::plugin::WsSource;
use flwrs_ws_source::server::{App, routes};
use flwrs_ws_source::{convert, spec};
use std::process::ExitCode;
use std::sync::{Arc, OnceLock};
use tokio::sync::{Semaphore, watch};

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
    // The document, and the out schema it gives (the configurator's, checked).
    let api = match &settings.asyncapi {
        None => None,
        Some(text) => match spec::parse(&serde_json::Value::String(text.clone()))
            .map_err(|e| vec![e])
            .and_then(spec::load)
        {
            Ok(api) => Some(api),
            Err(problems) => return fail(problems),
        },
    };
    let out_schema = match convert::out_schema(api.as_ref()) {
        Ok(s) => s,
        Err(problems) => return fail(problems),
    };
    if let Some(configured) = &settings.out_schema {
        let same = flwrs_plugin::schema::rules::parse_json(&configured.to_string())
            .map(|c| flwrs_plugin::schema::rules::diff(&c, &out_schema).is_empty())
            .unwrap_or(false);
        if !same {
            return fail(vec!["out_schema doesn't match the asyncapi document (run the node's configurator again)".into()]);
        }
    }
    if let Some(api) = &api {
        for warning in &api.warnings {
            log::warn!("asyncapi: {warning}");
        }
    }
    let sink = Arc::new(OnceLock::new());
    let (stop_tx, stopping) = watch::channel(false);
    let app = Arc::new(App {
        sink: sink.clone(),
        out_schema: out_schema.clone(),
        headers: settings.server.headers.clone(),
        subprotocols: settings.subprotocols.clone(),
        allowed_origins: settings.allowed_origins.clone(),
        max_message_bytes: settings.max_message_bytes,
        read_buffer_bytes: settings.read_buffer_bytes,
        ping_interval: settings.ping_interval,
        enqueue_timeout: settings.enqueue_timeout,
        invalid_messages: settings.invalid_messages,
        connections: Arc::new(Semaphore::new(settings.server.limits.max_connections)),
        stopping,
        dropped: Default::default(),
    });
    let router = match routes(app, api.as_ref(), &settings.paths) {
        Ok(r) => r,
        Err(problems) => return fail(problems),
    };
    let probe = sink.clone();
    let ready = Arc::new(move || probe.get().is_some_and(|s| s.is_ready()));
    let server = match Server::start(settings.server.clone(), router, ready).await {
        Ok(server) => server,
        Err(e) => {
            return fail(vec![format!(
                "listening on {}: {e}",
                settings.server.listen
            )]);
        }
    };
    // Upgraded connections are the plugin's: they close when the server stops.
    let mut server_stopping = server.stopping();
    tokio::spawn(async move {
        if server_stopping.wait_for(|s| *s).await.is_ok() {
            let _ = stop_tx.send(true);
        }
    });
    let plugin = WsSource {
        sink,
        server: Some(server),
        out_schema,
    };
    let config = SourceRunnerConfig {
        plugin_id: args.bootstrap.id.clone(),
        log_level: args.bootstrap.log_level(),
        hub_connection: args.bootstrap.connection(),
    };
    let mut runner = match SourceRunner::initialize(plugin, config).await {
        Ok(runner) => runner,
        Err(err) => {
            eprintln!("Failed to initialize the source runner: {err}");
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
    use flwrs_ws_source::config::Args;
    use serde_json::{Value, json};
    use std::collections::BTreeSet;

    fn file(path: &str) -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(path)
    }

    fn json(path: &str) -> Value {
        serde_json::from_str(&std::fs::read_to_string(file(path)).unwrap()).unwrap()
    }

    /// The launch-args schema: the WebSocket source's own settings, then the
    /// server core's (`schema_properties`). `FLWRS_UPDATE=1` writes it.
    fn expected_schema() -> Value {
        let mut props = json!({
            "asyncapi": {
                "title": "AsyncAPI document", "x-flwrs-order": 1,
                "description": "The API to serve: an AsyncAPI 3.0 or 3.1 document, as JSON or YAML text, or as an object. The channels of its receive operations are the WebSocket paths (their addresses), and their messages' JSON Schema payloads type and validate what clients send. It must be self-contained (no external $refs).",
                "type": ["object", "string"], "x-flwrs-launch": { "via": "file" },
                "x-flwrs-template": "templates/asyncapi.yaml" },
            "paths": {
                "title": "Paths", "x-flwrs-order": 2,
                "description": "Without a document: the paths clients connect to (default [\"/ws\"]); every message is an event, as bytes.",
                "items": { "type": "string" }, "type": "array", "x-flwrs-launch": { "via": "file" } },
            "subprotocols": {
                "title": "Subprotocols", "x-flwrs-order": 3,
                "description": "The subprotocols the server speaks (Sec-WebSocket-Protocol), in order of preference; a client offering none of them still connects, without one.",
                "items": { "type": "string" }, "type": "array", "x-flwrs-launch": { "via": "file" } },
            "allowed_origins": {
                "title": "Allowed origins", "x-flwrs-order": 4,
                "description": "The Origin headers browsers may connect from (e.g. https://app.example.com); empty: any. Clients that send no Origin (not browsers) aren't affected.",
                "items": { "type": "string" }, "type": "array", "x-flwrs-launch": { "via": "file" } },
            "max_message_bytes": {
                "title": "Largest message (bytes)", "x-flwrs-order": 5,
                "description": "A larger message (or frame) closes the connection (1009).",
                "type": "integer", "minimum": 16, "default": 1048576 },
            "read_buffer_bytes": {
                "title": "Read buffer (bytes)", "x-flwrs-order": 10, "x-flwrs-advanced": true,
                "description": "Per connection, allocated up front (tungstenite's default is 128 KiB): smaller saves memory with many clients, and larger messages just take more reads.",
                "type": "integer", "minimum": 256, "default": 16384 },
            "ping_interval_s": {
                "title": "Ping interval (s)", "x-flwrs-order": 6, "x-flwrs-advanced": true,
                "description": "The server pings each client this often; a client that has sent nothing since the last ping is closed.",
                "type": "integer", "minimum": 1, "default": 30 },
            "enqueue_timeout_ms": {
                "title": "Enqueue timeout (ms)", "x-flwrs-order": 7,
                "description": "How long a message waits for the workflow to take it (messages are read one at a time, so a slow workflow slows the client down); then the connection closes with 1013 (try again later).",
                "type": "integer", "minimum": 1, "default": 5000 },
            "invalid_messages": {
                "title": "Invalid messages", "x-flwrs-order": 8,
                "description": "A message no definition of its channel takes (not JSON, or its payload schema fails): drop it (a WARN once a minute), or close the connection (1008).",
                "enum": ["drop", "close"], "default": "drop" },
            "out_schema": {
                "title": "Out schema (derived)", "x-flwrs-order": 9, "x-flwrs-advanced": true,
                "description": "Set by the plugin's configurator from the document: the common message fields and channels[\"<address>\"].{path_params, messages.<name>}.",
                "type": "object", "readOnly": true, "x-flwrs-launch": { "via": "file" } }
        });
        for (k, v) in flwrs_plugin_kit::server::schema_properties()
            .as_object()
            .unwrap()
        {
            props[k] = v.clone();
        }
        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "title": "WebSocket source settings", "type": "object", "additionalProperties": false,
            "properties": props
        })
    }

    /// The template the editor offers (`x-flwrs-template`) is a document
    /// this plugin serves as it is.
    #[test]
    fn the_template_is_a_working_document() {
        let text = std::fs::read_to_string(file("templates/asyncapi.yaml")).unwrap();
        let doc = flwrs_ws_source::spec::parse(&Value::String(text)).unwrap();
        let api = flwrs_ws_source::spec::load(doc).unwrap();
        let out = flwrs_ws_source::convert::out_schema(Some(&api)).unwrap();
        let out = serde_json::to_string(&flwrs_ws_source::convert::to_json(&out)).unwrap();
        assert!(out.contains("/devices/{device_id}"), "{out}");
        assert!(out.contains("temp"), "{out}");
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
    fn manifest_matches_the_crate() {
        let manifest = json("manifest.json");
        assert_eq!(manifest["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(manifest["type"], "SOURCE");
        assert_eq!(
            manifest["executable"],
            format!("bin/{}", env!("CARGO_PKG_NAME"))
        );
        assert_eq!(
            manifest["configurator"]["executable"],
            format!("bin/{}-config", env!("CARGO_PKG_NAME"))
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
