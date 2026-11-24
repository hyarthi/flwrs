//! Trellis against plugins built on the Rust SDK (this test binary, re-run
//! as a child process): `audit` must pass them and fail a broken one; `test`
//! and `bench` run against them with stimuli, NACKs, redeliveries and restarts.
#![cfg(unix)]

use flwrs_plugin::plugin::core::{
    ConnectionConfig, FieldDefinition, InboundEvent, InitializeRequest, SchemaDefinition,
};
use flwrs_plugin::plugin::error::{
    InitializeError, ShutdownError, SinkError, SourceError, TransformError,
};
use flwrs_plugin::schema::payload::{
    Field, FieldValue, PluginPayload, field_type::Enum as FieldType, field_value::Value,
};
use flwrs_plugin::schema::protocol::log_level::Enum as LogLevel;
use flwrs_trellis::bench::{BenchFile, Model};
use flwrs_trellis::{Audit, Bencher, Outcome, StimulusCtx, Target, Tester};
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
    fn initialize(
        &mut self,
        id: String,
        _: LogLevel,
    ) -> Result<InitializeRequest, InitializeError> {
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

struct Upper;
impl flwrs_plugin::transform::plugin::Transform for Upper {
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
    /// Uppercases `text`; "fail" is refused, "retry" is refused as retryable.
    fn process_event(&mut self, event: InboundEvent) -> Result<Vec<PluginPayload>, TransformError> {
        let text = text_of(&event.payload);
        match text.as_str() {
            "fail" => Err(TransformError::new("refused")),
            "retry" => Err(TransformError::retryable("try again")),
            _ => Ok(vec![text_payload(&text.to_uppercase())]),
        }
    }
}

fn text_of(payload: &PluginPayload) -> String {
    payload
        .fields
        .iter()
        .find(|f| f.key == "text")
        .and_then(|f| f.value.as_ref())
        .and_then(|v| match &v.value {
            Some(Value::String(s)) => Some(s.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

fn text_payload(text: &str) -> PluginPayload {
    PluginPayload {
        fields: vec![Field {
            key: "text".into(),
            value: Some(FieldValue {
                value: Some(Value::String(text.into())),
            }),
        }],
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

/// Emits one event per line received on its trigger port (the line is the text).
struct Triggered(Option<flwrs_plugin::source::local_sink::LocalSink>, u16);
impl flwrs_plugin::source::plugin::Source for Triggered {
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
        use tokio::io::AsyncBufReadExt;
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", self.1))
            .await
            .map_err(SourceError::new)?;
        loop {
            let (stream, _) = listener.accept().await.map_err(SourceError::new)?;
            let mut lines = tokio::io::BufReader::new(stream).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                self.0.as_ref().unwrap().emit(text_payload(&line)).await?;
            }
        }
    }
}

/// Runs only as a child of the tests below.
#[tokio::test]
#[ignore = "runs as a child process of the trellis tests"]
async fn plugin_child() {
    let (Ok(mode), Ok(args)) = (std::env::var(MODE_ENV), std::env::var(ARGS_ENV)) else {
        return;
    };
    let args: Vec<&str> = args.split_whitespace().collect();
    let arg = |name: &str| {
        args.iter()
            .position(|a| *a == name)
            .map(|i| args[i + 1].to_string())
    };
    let (id, connection) = (
        arg("--id").unwrap(),
        ConnectionConfig::new(
            arg("--host").unwrap(),
            arg("--port").unwrap().parse().unwrap(),
        ),
    );
    let result = match mode.as_str() {
        "sink" => {
            use flwrs_plugin::sink::runner::{SinkRunner, SinkRunnerConfig};
            let config = SinkRunnerConfig::new(id, LogLevel::Debug, connection);
            SinkRunner::initialize(Sink, config)
                .await
                .unwrap()
                .run()
                .await
        }
        "transform" => {
            use flwrs_plugin::transform::runner::{TransformRunner, TransformRunnerConfig};
            let config = TransformRunnerConfig::new(id, LogLevel::Debug, connection);
            TransformRunner::initialize(Upper, config)
                .await
                .unwrap()
                .run()
                .await
        }
        "source" | "triggered" => {
            use flwrs_plugin::source::runner::{SourceRunner, SourceRunnerConfig};
            let config = SourceRunnerConfig {
                plugin_id: id,
                log_level: LogLevel::Debug,
                hub_connection: connection,
            };
            if mode == "source" {
                SourceRunner::initialize(Ticker(None), config)
                    .await
                    .unwrap()
                    .run()
                    .await
            } else {
                let port = arg("--trigger-port").unwrap().parse().unwrap();
                SourceRunner::initialize(Triggered(None, port), config)
                    .await
                    .unwrap()
                    .run()
                    .await
            }
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

async fn audit(mode: &str, plugin_type: &str) {
    let dir = tempfile::tempdir().unwrap();
    let target = package(dir.path(), mode, plugin_type);
    let results = Audit::new(target).run().await;
    let report = flwrs_trellis::report(&results);
    assert!(
        results.iter().all(|r| r.outcome == Outcome::Pass),
        "{mode}:\n{report}"
    );
}

#[tokio::test]
async fn sdk_sink_passes() {
    audit("sink", "SINK").await;
}

#[tokio::test]
async fn sdk_transform_passes() {
    audit("transform", "TRANSFORM").await;
}

#[tokio::test]
async fn sdk_source_passes() {
    audit("source", "SOURCE").await;
}

/// A remote plugin definition (no executable) packages without a build, and
/// the remote plugin given apart passes the audit through it.
#[tokio::test]
async fn a_remote_definition_packages_and_its_plugin_is_audited() {
    let dir = tempfile::tempdir().unwrap();
    let local = package(dir.path(), "sink", "SINK");
    let def = tempfile::tempdir().unwrap();
    std::fs::write(def.path().join("schema.json"), SCHEMA).unwrap();
    std::fs::write(
        def.path().join("manifest.json"),
        r#"{"manifest_version":2,"id":"far","name":"Far","version":"1.0.0","type":"SINK","launch":"remote","payload_schemas":{"in":"schema.json"}}"#,
    )
    .unwrap();
    let out = tempfile::tempdir().unwrap();
    let packaged = flwrs_trellis::package::package(&flwrs_trellis::package::PackageOptions {
        out: Some(out.path().to_path_buf()),
        ..flwrs_trellis::package::PackageOptions::new(def.path())
    })
    .unwrap();
    assert_eq!(packaged.executable_size, 0);
    assert_eq!(packaged.path.file_name().unwrap(), "far-1.0.0.zip");
    let target = Target::from_zip(&packaged.path).unwrap();
    assert!(target.is_remote());
    // Nothing to run without the remote plugin.
    let results = Audit::new(target.clone()).run().await;
    assert!(results.iter().all(|r| r.outcome != Outcome::Pass));
    let target = target.with_executable(local.program.clone());
    let results = Audit::new(target).run().await;
    assert!(
        results.iter().all(|r| r.outcome == Outcome::Pass),
        "{}",
        flwrs_trellis::report(&results)
    );
}

/// A remote plugin as trellis sees it: started by commands (with a fixed id
/// and token), its payload schema set by its configurator endpoint. (Multi-
/// threaded: `with_launch_args` blocks its thread while the endpoint answers.)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_remote_plugin_is_started_by_commands_and_configured_at_its_endpoint() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    // The endpoint: answers every request with the in schema.
    let endpoint = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/configure", endpoint.local_addr().unwrap());
    let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let seen = requests.clone();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = endpoint.accept().await {
            let mut buf = vec![0u8; 65536];
            let mut len = 0;
            // Headers, then the body by Content-Length.
            loop {
                let n = stream.read(&mut buf[len..]).await.unwrap();
                len += n;
                let text = String::from_utf8_lossy(&buf[..len]).to_string();
                if let Some(end) = text.find("\r\n\r\n") {
                    let length: usize = text
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse().unwrap())
                        })
                        .unwrap_or(0);
                    if len >= end + 4 + length || n == 0 {
                        seen.lock().unwrap().push(text[end + 4..].to_string());
                        break;
                    }
                }
            }
            let body = format!(r#"{{"values":{{"in_schema":{SCHEMA}}}}}"#);
            let answer = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(answer.as_bytes()).await.unwrap();
        }
    });

    let dir = tempfile::tempdir().unwrap();
    let local = package(dir.path(), "sink", "SINK");
    let def = tempfile::tempdir().unwrap();
    write(
        def.path(),
        "schema/launch_args.json",
        r#"{"type": "object", "properties": {"source": {"type": "string"}, "in_schema": {"type": "object"}}}"#,
    );
    write(
        def.path(),
        "manifest.json",
        r#"{"manifest_version":2,"id":"far","name":"Far","version":"1.0.0","type":"SINK","launch":"remote",
            "launch_args_schema":"schema/launch_args.json","configurator":{"fields":["in_schema"]},
            "payload_schemas":{"in":{"from_launch_args":"/in_schema"}}}"#,
    );
    let remote = flwrs_trellis::remote::RemoteLaunch {
        start: vec![
            local.program.to_string_lossy().into_owned(),
            "--id".into(),
            "${id}".into(),
            "--host".into(),
            "${hub_host}".into(),
            "--port".into(),
            "${hub_port}".into(),
        ],
        id: Some("edge-1".into()),
        token: Some("pre-shared".into()),
        configurator: Some(flwrs_trellis::remote::RemoteConfigurator {
            url,
            auth: None,
            ca: None,
            send_secrets: false,
        }),
        dir: def.path().to_path_buf(),
        ..Default::default()
    };
    let target = Target::from_package(def.path())
        .unwrap()
        .with_remote(remote)
        .with_launch_args(serde_json::json!({"source": "s"}))
        .unwrap();
    assert!(target.in_schema.is_some(), "the endpoint's schema");
    let request: serde_json::Value = serde_json::from_str(&requests.lock().unwrap()[0]).unwrap();
    assert_eq!(request["node_id"], "edge-1");
    assert_eq!(request["fields"], serde_json::json!(["in_schema"]));
    let results = Audit::new(target).run().await;
    assert!(
        results.iter().all(|r| r.outcome == Outcome::Pass),
        "{}",
        flwrs_trellis::report(&results)
    );
}

#[tokio::test]
async fn a_plugin_that_never_connects_fails() {
    let dir = tempfile::tempdir().unwrap();
    let target = package(dir.path(), "silent", "SINK");
    let results = Audit::new(target).only(&["handshake".into()]).run().await;
    assert!(
        matches!(&results[0].outcome, Outcome::Fail(why) if why.contains("didn't connect")),
        "{results:?}"
    );
}

/// A triggered source: emits once per line sent to its trigger port.
fn triggered(dir: &Path) -> (Target, u16) {
    let port = flwrs_trellis::free_port();
    let target = package(dir, "triggered", "SOURCE")
        .with_launch_args(serde_json::json!({ "trigger_port": port }))
        .unwrap();
    (target, port)
}

async fn trigger(port: u16, line: String) -> Result<(), String> {
    use tokio::io::AsyncWriteExt;
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .map_err(|e| e.to_string())?;
    stream
        .write_all(format!("{line}\n").as_bytes())
        .await
        .map_err(|e| e.to_string())
}

#[tokio::test]
async fn a_stimulus_makes_a_waiting_source_pass_data_events() {
    let dir = tempfile::tempdir().unwrap();
    let (target, port) = triggered(dir.path());
    let only = ["data-events".to_string()];
    let without = Audit::new(target.clone()).only(&only).run().await;
    assert!(
        matches!(without[0].outcome, Outcome::Skip(_)),
        "{without:?}"
    );
    let with = Audit::new(target)
        .only(&only)
        .stimulus(move |_: StimulusCtx| async move {
            // The source binds its port when it's accepted; retry briefly.
            for _ in 0..50 {
                if trigger(port, "hello".into()).await.is_ok() {
                    return Ok(None);
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            Err("the trigger port never opened".into())
        })
        .run()
        .await;
    assert_eq!(with[0].outcome, Outcome::Pass, "{with:?}");
}

fn write(dir: &Path, name: &str, text: &str) {
    let path = dir.join(name);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

#[tokio::test]
async fn transform_tests_cover_outputs_nacks_redelivery_and_restarts() {
    let dir = tempfile::tempdir().unwrap();
    let target = package(dir.path(), "transform", "TRANSFORM");
    let t = dir.path().join("tests");
    write(&t, "p/hello.json", r#"{"text": "hello"}"#);
    write(&t, "p/fail.json", r#"{"text": "fail"}"#);
    write(&t, "p/retry.json", r#"{"text": "retry"}"#);
    write(&t, "e/hello.json", r#"{"text": "HELLO"}"#);
    write(
        &t,
        "upper.test.toml",
        r#"
name = "uppercase"

[[steps]]
send = "p/hello.json"
expect_ack = "ack"
expect_outputs = [{ payload = "e/hello.json" }]

[[steps]]
send = "p/hello.json"
expect_outputs = [{ path = "text", eq = "HELLO" }, { count = 1 }]

[[steps]]
send = "p/fail.json"
expect_ack = { nack = { retryable = false } }
expect_outputs = [{ count = 0 }]

[[steps]]
send = "p/retry.json"
expect_ack = { nack = { retryable = true } }

[[steps]]
redeliver = 1
expect_ack = "ack"
expect_outputs = [{ same_as_step = 1 }]

[[steps]]
disconnect = true

[[steps]]
send = "p/hello.json"
expect_outputs = [{ match = "subset", payload = "e/hello.json" }]

[[steps]]
restart = true

[[steps]]
send = "p/hello.json"
expect_ack = "ack"
"#,
    );
    let mut tester = Tester::new(target.clone());
    let result = tester.run_file(&t.join("upper.test.toml")).await;
    let report = flwrs_trellis::testing::report(std::slice::from_ref(&result));
    assert_eq!(result.outcome, Outcome::Pass, "{report}");
    assert_eq!(result.steps.len(), 9);

    // A wrong expectation fails and says why; --update writes the snapshot.
    write(&t, "e/wrong.json", r#"{"text": "nope"}"#);
    write(
        &t,
        "wrong.test.toml",
        "[[steps]]\nsend = \"p/hello.json\"\nexpect_outputs = [{ payload = \"e/wrong.json\" }]\n",
    );
    let failed = tester.run_file(&t.join("wrong.test.toml")).await;
    assert!(
        matches!(&failed.steps[0].outcome, Outcome::Fail(why) if why.contains(r#"text: "HELLO", expected "nope""#)),
        "{failed:?}"
    );
    let mut updater = Tester::new(target).update(true);
    let updated = updater.run_file(&t.join("wrong.test.toml")).await;
    assert_eq!(updated.outcome, Outcome::Pass, "{updated:?}");
    let written: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(t.join("e/wrong.json")).unwrap()).unwrap();
    assert_eq!(written, serde_json::json!({"text": "HELLO"}));
}

#[tokio::test]
async fn sink_smoke_and_source_stimulus_with_correlation() {
    let dir = tempfile::tempdir().unwrap();
    let sink = package(&dir.path().join("sink").tap_mkdir(), "sink", "SINK");
    let smoke = Tester::new(sink).smoke().await;
    assert_eq!(smoke.outcome, Outcome::Pass, "{smoke:?}");

    let (source, port) = triggered(&dir.path().join("src").tap_mkdir());
    let t = dir.path().join("src");
    write(
        &t,
        "s.test.toml",
        "[[steps]]\nstimulus = true\nexpect_outputs = [{ path = \"text\", eq = \"id-0\" }]\n\n[[steps]]\nstimulus = true\nexpect_outputs = [{ path = \"text\", eq = \"id-1\" }]\n",
    );
    let mut tester = Tester::new(source)
        .stimulus(move |ctx: StimulusCtx| async move {
            let id = format!("id-{}", ctx.iteration);
            for _ in 0..50 {
                if trigger(port, id.clone()).await.is_ok() {
                    return Ok(Some(id));
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            Err("the trigger port never opened".into())
        })
        .correlate(|payload| Some(text_of(payload)));
    let result = tester.run_file(&t.join("s.test.toml")).await;
    assert_eq!(result.outcome, Outcome::Pass, "{result:?}");
}

trait TapMkdir {
    fn tap_mkdir(self) -> Self;
}
impl TapMkdir for std::path::PathBuf {
    fn tap_mkdir(self) -> Self {
        std::fs::create_dir_all(&self).unwrap();
        self
    }
}

fn quick_bench(model: Model) -> BenchFile {
    let mut file = BenchFile::default_named("quick");
    file.warmup_ms = 200;
    file.duration_ms = 700;
    file.repetitions = 1;
    file.load.model = model;
    file.load.rate = 200.0;
    file
}

#[tokio::test]
async fn bench_measures_transforms_and_sources() {
    let dir = tempfile::tempdir().unwrap();
    let transform = package(&dir.path().join("t").tap_mkdir(), "transform", "TRANSFORM");
    let mut bencher = Bencher::new(transform.clone());
    let closed = bencher.run(&quick_bench(Model::Closed)).await.unwrap();
    let run = &closed.runs[0];
    assert!(run.events > 0 && run.throughput > 0.0, "{closed:?}");
    assert!(run.latency_us.is_some() && run.errors == 0, "{closed:?}");
    assert!(
        run.resources.as_ref().is_some_and(|r| r.rss_max_bytes > 0),
        "{closed:?}"
    );
    assert!(
        closed.calibration.as_ref().is_some_and(|c| c.events > 0),
        "{closed:?}"
    );

    let open = Bencher::new(transform)
        .run(&quick_bench(Model::Open))
        .await
        .unwrap();
    // ~200 ev/s for 0.7 s.
    let events = open.runs[0].events;
    assert!((100..=160).contains(&events), "{events} events: {open:?}");

    let source = package(&dir.path().join("s").tap_mkdir(), "source", "SOURCE");
    let ticks = Bencher::new(source)
        .run(&quick_bench(Model::Closed))
        .await
        .unwrap();
    // The ticker emits every 100 ms.
    assert!((4..=10).contains(&ticks.runs[0].events), "{ticks:?}");
    let table = flwrs_trellis::bench::report(&[closed, open, ticks]);
    assert!(table.contains("ev/s"), "{table}");
}
