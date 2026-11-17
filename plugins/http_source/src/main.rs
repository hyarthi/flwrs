use clap::Parser;
use flwrs_http_source::config::Args;
use flwrs_http_source::plugin::HttpSource;
use flwrs_http_source::server::{App, routes};
use flwrs_http_source::{convert, spec};
use flwrs_plugin::source::runner::{SourceRunner, SourceRunnerConfig};
use flwrs_plugin_kit::server::Server;
use std::process::ExitCode;
use std::sync::{Arc, OnceLock};

fn fail(problems: Vec<String>) -> ExitCode {
    for problem in problems {
        eprintln!("{problem}");
    }
    ExitCode::from(2)
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = Args::parse();
    let settings = match args.settings() {
        Ok(s) => s,
        Err(problems) => return fail(problems),
    };
    // The document, and the out schema it gives (the configurator's, checked).
    let api = match &settings.openapi {
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
            return fail(vec![
                "out_schema doesn't match the openapi document (run the node's configurator again)"
                    .into(),
            ]);
        }
    }
    let sink = Arc::new(OnceLock::new());
    let app = Arc::new(App {
        sink: sink.clone(),
        out_schema: out_schema.clone(),
        ack_mode: settings.ack_mode,
        enqueue_timeout: settings.enqueue_timeout,
        hide_error_details: settings.hide_error_details,
        headers: settings.server.headers.clone(),
        max_body_bytes: settings.server.limits.max_body_bytes,
    });
    let mut protected = axum::Router::new();
    let mut public = axum::Router::new();
    if let Some(api) = &api {
        match routes(app, api, &settings.base_path) {
            Ok(r) => protected = r,
            Err(problems) => return fail(problems),
        }
        if settings.swagger_ui.enabled {
            let docs = swagger_ui(&settings.swagger_ui.path, api.doc.clone());
            if settings.swagger_ui.public {
                public = docs;
            } else {
                protected = protected.merge(docs);
            }
        }
        for warning in &api.warnings {
            log::warn!("openapi: {warning}");
        }
    }
    let probe = sink.clone();
    let ready = Arc::new(move || probe.get().is_some_and(|s| s.is_ready()));
    let server =
        match Server::start_with_public(settings.server.clone(), protected, public, ready).await {
            Ok(server) => server,
            Err(e) => {
                return fail(vec![format!(
                    "listening on {}: {e}",
                    settings.server.listen
                )]);
            }
        };
    let plugin = HttpSource {
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

#[cfg(feature = "swagger-ui")]
fn swagger_ui(path: &str, doc: serde_json::Value) -> axum::Router {
    utoipa_swagger_ui::SwaggerUi::new(path.to_string())
        .external_url_unchecked(format!("{path}/openapi.json"), doc)
        .into()
}

#[cfg(not(feature = "swagger-ui"))]
fn swagger_ui(_: &str, _: serde_json::Value) -> axum::Router {
    unreachable!("refused in the settings without the feature")
}

/// Keeps the package files in sync with the code.
#[cfg(test)]
mod package_tests {
    use clap::CommandFactory;
    use flwrs_http_source::config::Args;
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

    /// The launch-args schema: the HTTP source's own settings, then the
    /// server core's (`schema_properties`). `FLWRS_UPDATE=1` writes it.
    fn expected_schema() -> Value {
        let mut props = json!({
            "openapi": {
                "title": "OpenAPI document", "x-flwrs-order": 1,
                "description": "The API to serve: an OpenAPI 3.1 document, as JSON or YAML text, or as an object. It must be self-contained (no external $refs). Without it, the node only answers health and readiness checks.",
                "type": ["object", "string"], "x-flwrs-launch": { "via": "file" } },
            "base_path": {
                "title": "Base path", "x-flwrs-order": 2,
                "description": "A prefix for every route (e.g. /v1).", "type": "string", "default": "" },
            "ack_mode": {
                "title": "Answer", "x-flwrs-order": 3,
                "description": "confirmed: a request gets its 2xx once the hub has its event (committed, with a durable queue), so a client that retries on 5xx gets at-least-once delivery. accepted: once the event is sent (faster; a crash can lose it).",
                "enum": ["confirmed", "accepted"], "default": "confirmed" },
            "enqueue_timeout_ms": {
                "title": "Enqueue timeout (ms)", "x-flwrs-order": 4,
                "description": "How long a request waits for the workflow to take its event; then 503 with Retry-After.",
                "type": "integer", "minimum": 1, "default": 5000 },
            "hide_error_details": {
                "title": "Hide error details", "x-flwrs-order": 5, "x-flwrs-advanced": true,
                "description": "Answer 400s without the validation messages.", "type": "boolean", "default": false },
            "swagger_ui": {
                "title": "Swagger UI", "x-flwrs-order": 6, "x-flwrs-advanced": true,
                "description": "Serve Swagger UI for the document at path (and the document at <path>/openapi.json). Behind the node's auth unless public.",
                "type": "object", "additionalProperties": false,
                "properties": {
                    "enabled": { "type": "boolean", "default": false },
                    "path": { "type": "string", "default": "/docs" },
                    "public": { "type": "boolean", "default": false } } },
            "out_schema": {
                "title": "Out schema (derived)", "x-flwrs-order": 7, "x-flwrs-advanced": true,
                "description": "Set by the plugin's configurator from the document: the common request fields and paths[\"<template>\"].<method>.{path_params, query, body}.",
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
            "title": "HTTP source settings", "type": "object", "additionalProperties": false,
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
