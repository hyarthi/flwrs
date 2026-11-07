//! The suite against plugins built on the Rust SDK (this test binary, re-run
//! as a child process), which must pass, and against a broken one, which
//! mustn't.
#![cfg(unix)]

use flwrs_trellis::{Outcome, Target, run};
use flwrs_plugin::plugin::core::{
    ConnectionConfig, FieldDefinition, InboundEvent, InitializeRequest, SchemaDefinition,
};
use flwrs_plugin::plugin::error::{
    InitializeError, ShutdownError, SinkError, SourceError, TransformError,
};
use flwrs_plugin::schema::payload::{PluginPayload, field_type::Enum as FieldType};
use flwrs_plugin::schema::protocol::log_level::Enum as LogLevel;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

const MODE_ENV: &str = "FLWRS_CONFORMANCE_MODE";
const ARGS_ENV: &str = "FLWRS_CONFORMANCE_ARGS";
const SCHEMA: &str = r#"{"fields":[{"key":"text","type":"STRING"}]}"#;

fn schema() -> SchemaDefinition {
    SchemaDefinition::new().add_field(
        FieldDefinition::new()
            .with_key("text".into())
            .with_type(FieldType::String),
    )
}

fn request(id: String) -> InitializeRequest {
    InitializeRequest::new()
        .with_id(id)
        .with_version("1.0.0".into())
        .with_in_schema(schema())
        .with_out_schema(schema())
}

struct Sink;
impl flwrs_plugin::sink::plugin::Sink for Sink {
    fn initialize(&mut self, id: String, _: LogLevel) -> Result<InitializeRequest, InitializeError> {
        Ok(request(id))
    }
    fn shutdown(&mut self) -> Result<(), ShutdownError> {
        Ok(())
    }
    fn version(&self) -> String {
        "1.0.0".into()
    }
    async fn consume_event(&self, _: InboundEvent) -> Result<(), SinkError> {
        Ok(())
    }
}

struct Pass;
impl flwrs_plugin::transform::plugin::Transform for Pass {
    fn initialize(
        &mut self,
        id: String,
        _: LogLevel,
        _: flwrs_plugin::transform::local_sink::LocalSink,
    ) -> Result<InitializeRequest, InitializeError> {
        Ok(request(id))
    }
    fn shutdown(&mut self) -> Result<(), ShutdownError> {
        Ok(())
    }
    fn version(&self) -> String {
        "1.0.0".into()
    }
    fn process_event(&mut self, event: InboundEvent) -> Result<Vec<PluginPayload>, TransformError> {
        Ok(vec![event.payload])
    }
}

struct Ticker(Option<flwrs_plugin::source::local_sink::LocalSink>);
impl flwrs_plugin::source::plugin::Source for Ticker {
    fn initialize(
        &mut self,
        id: String,
        _: LogLevel,
        sink: flwrs_plugin::source::local_sink::LocalSink,
    ) -> Result<InitializeRequest, InitializeError> {
        self.0 = Some(sink);
        Ok(request(id))
    }
    fn shutdown(&mut self) -> Result<(), ShutdownError> {
        Ok(())
    }
    fn version(&self) -> String {
        "1.0.0".into()
    }
    async fn run(&self) -> Result<(), SourceError> {
        loop {
            let payload = flwrs_trellis::sample_payload(&schema().into());
            self.0.as_ref().unwrap().emit(payload).await?;
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    }
}

/// Runs only as a child of the tests below.
#[tokio::test]
#[ignore = "runs as a child process of the conformance tests"]
async fn plugin_child() {
    let (Ok(mode), Ok(args)) = (std::env::var(MODE_ENV), std::env::var(ARGS_ENV)) else {
        return;
    };
    let args: Vec<&str> = args.split_whitespace().collect();
    let arg = |name: &str| args[args.iter().position(|a| *a == name).unwrap() + 1].to_string();
    let (id, connection) = (
        arg("--id"),
        ConnectionConfig::new(arg("--host"), arg("--port").parse().unwrap()),
    );
    let result = match mode.as_str() {
        "sink" => {
            use flwrs_plugin::sink::runner::{SinkRunner, SinkRunnerConfig};
            let config = SinkRunnerConfig::new(id, LogLevel::Debug, connection);
            SinkRunner::initialize(Sink, config).await.unwrap().run().await
        }
        "transform" => {
            use flwrs_plugin::transform::runner::{TransformRunner, TransformRunnerConfig};
            let config = TransformRunnerConfig::new(id, LogLevel::Debug, connection);
            TransformRunner::initialize(Pass, config).await.unwrap().run().await
        }
        "source" => {
            use flwrs_plugin::source::runner::{SourceRunner, SourceRunnerConfig};
            let config = SourceRunnerConfig {
                plugin_id: id,
                log_level: LogLevel::Debug,
                hub_connection: connection,
            };
            SourceRunner::initialize(Ticker(None), config)
                .await
                .unwrap()
                .run()
                .await
        }
        other => panic!("unknown mode {other}"),
    };
    // Rejected is an expected end in one case.
    if let Err(err) = result
        && !matches!(err, flwrs_plugin::plugin::error::Error::Rejected)
    {
        panic!("{err}");
    }
}

/// A package folder whose executable re-runs this test binary in `mode`.
fn package(dir: &Path, mode: &str, plugin_type: &str) -> Target {
    let exe = std::env::current_exe().unwrap();
    let script = dir.join("run.sh");
    let body = match mode {
        "silent" => "#!/bin/sh\nexec sleep 60\n".to_string(),
        _ => format!(
            "#!/bin/sh\n{MODE_ENV}='{mode}' {ARGS_ENV}=\"$*\" exec '{}' plugin_child --exact --ignored --test-threads=1\n",
            exe.display()
        ),
    };
    std::fs::write(&script, body).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(dir.join("schema.json"), SCHEMA).unwrap();
    let schemas = match plugin_type {
        "SOURCE" => r#"{"out":"schema.json"}"#,
        "SINK" => r#"{"in":"schema.json"}"#,
        _ => r#"{"in":"schema.json","out":"schema.json"}"#,
    };
    std::fs::write(
        dir.join("manifest.json"),
        format!(
            r#"{{"manifest_version":1,"id":"c","name":"c","version":"1.0.0","type":"{plugin_type}","executable":"run.sh","payload_schemas":{schemas}}}"#
        ),
    )
    .unwrap();
    Target::from_package(dir).unwrap()
}

async fn check(mode: &str, plugin_type: &str) {
    let dir = tempfile::tempdir().unwrap();
    let target = package(dir.path(), mode, plugin_type);
    let results = run(&target, &[]).await;
    let report = flwrs_trellis::report(&results);
    assert!(
        results.iter().all(|r| r.outcome == Outcome::Pass),
        "{mode}:\n{report}"
    );
}

#[tokio::test]
async fn sdk_sink_passes() {
    check("sink", "SINK").await;
}

#[tokio::test]
async fn sdk_transform_passes() {
    check("transform", "TRANSFORM").await;
}

#[tokio::test]
async fn sdk_source_passes() {
    check("source", "SOURCE").await;
}

#[tokio::test]
async fn a_plugin_that_never_connects_fails() {
    let dir = tempfile::tempdir().unwrap();
    let target = package(dir.path(), "silent", "SINK");
    let results = run(&target, &["handshake".into()]).await;
    assert!(
        matches!(&results[0].outcome, Outcome::Fail(why) if why.contains("didn't connect")),
        "{results:?}"
    );
}
