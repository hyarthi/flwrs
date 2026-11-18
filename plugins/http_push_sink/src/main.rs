use clap::Parser;
use flwrs_http_push_sink::config::Args;
use flwrs_http_push_sink::plugin::PushSink;
use flwrs_http_push_sink::server::{App, routes};
use flwrs_plugin::sink::runner::{SinkRunner, SinkRunnerConfig};
use flwrs_plugin_kit::broadcast::Broadcaster;
use flwrs_plugin_kit::server::Server;
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
        sse_format: settings.sse_format,
        heartbeat: settings.heartbeat,
        retry_ms: settings.retry_ms,
        poll_max_events: settings.poll_max_events,
        poll_max_wait: settings.poll_max_wait,
        allowed_origins: settings.allowed_origins.clone(),
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
    // Streams end when the server stops, however it stops.
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
    use flwrs_http_push_sink::config::Args;
    use flwrs_plugin::cli::Bootstrap;
    use flwrs_plugin::package::{LaunchSpec, Manifest};
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
                "description": "A prefix for the routes: <base>/subscribe/{channel} (SSE) and <base>/poll/{channel} (long polling).",
                "type": "string", "default": "" },
            "sse_format": {
                "title": "SSE data", "x-flwrs-order": 2,
                "description": "raw: the data as data: lines (non-UTF-8 data as base64, with an encoding: base64 line). json: one line with a JSON envelope {id, event, content_type, data, encoding?}, as long polls answer.",
                "enum": ["raw", "json"], "default": "raw" },
            "heartbeat_s": {
                "title": "SSE keep-alive (s)", "x-flwrs-order": 3, "x-flwrs-advanced": true,
                "description": "A comment line this often keeps proxies from closing idle streams.",
                "type": "integer", "minimum": 1, "default": 15 },
            "retry_ms": {
                "title": "SSE retry hint (ms)", "x-flwrs-order": 4, "x-flwrs-advanced": true,
                "description": "How long clients (EventSource) wait before reconnecting.",
                "type": "integer", "minimum": 0, "default": 3000 },
            "poll_max_events": {
                "title": "Long poll: most events", "x-flwrs-order": 5, "x-flwrs-advanced": true,
                "description": "Most events in one answer (a request's max is capped by it).",
                "type": "integer", "minimum": 1, "default": 100 },
            "poll_max_wait_s": {
                "title": "Long poll: longest wait (s)", "x-flwrs-order": 6, "x-flwrs-advanced": true,
                "description": "Longest a poll waits for an event (a request's wait_s is capped by it); shorter than limits.request_timeout_ms.",
                "type": "integer", "minimum": 1, "default": 25 },
            "allowed_origins": {
                "title": "Allowed origins (CORS)", "x-flwrs-order": 7,
                "description": "The Origins browsers may subscribe from (e.g. https://app.example.com), or [\"*\"] for any. Empty: no CORS headers (same-origin pages and non-browser clients only).",
                "type": "array", "items": { "type": "string" }, "x-flwrs-launch": { "via": "file" } },
            "max_in_flight": {
                "title": "Events in flight", "x-flwrs-order": 8, "x-flwrs-advanced": true,
                "description": "Events handled at once (with ack_mode delivered: waiting for a subscriber at once); the hub holds the rest.",
                "type": "integer", "minimum": 1, "default": 64 }
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
            "title": "HTTP push sink settings", "type": "object", "additionalProperties": false,
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
