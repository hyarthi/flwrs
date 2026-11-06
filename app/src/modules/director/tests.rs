//! Director tests: real scenes (through the scene service) running real
//! plugin processes. The plugins are this test binary, re-run as children in
//! a chosen mode, built on the Rust SDK.

use super::engine::Engine;
use super::handle::{DirectorCommand, DirectorHandle, SceneState, SceneStatus};
use super::queue::{Budget, SpillShared};
use super::running::Context;
use crate::modules::hub::{Hub, HubConfig};
use crate::modules::scene::service::tests::{Fixture, edge_input, fixture, node_input};
use flwrs_core::db::Database;
use flwrs_plugin::plugin::core::{
    ConnectionConfig, FieldDefinition, InitializeRequest, SchemaDefinition,
};
use flwrs_plugin::plugin::error::{
    InitializeError, ShutdownError, SinkError, SourceError, TransformError,
};
use flwrs_plugin::schema::common::log_level::Enum as LogLevel;
use flwrs_plugin::schema::schema::{
    Field, FieldValue, PluginPayload, field_type, field_value::Value,
};
use flwrs_plugin::schema::{sink, transform};
use serde_json::json;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

const WAIT: Duration = Duration::from_secs(15);
const CHILD_TEST: &str = "modules::director::tests::test_plugin";
const MODE_ENV: &str = "FLWRS_TEST_PLUGIN";
const ARGS_ENV: &str = "FLWRS_TEST_ARGS";
const OUT_ENV: &str = "FLWRS_TEST_OUT";
const TEXT_SCHEMA_JSON: &str = r#"{"fields":[{"key":"text","type":"STRING"}]}"#;

// --- the test plugins (child processes) ---

fn text_schema() -> SchemaDefinition {
    SchemaDefinition::new().add_field(
        FieldDefinition::new()
            .with_key("text".into())
            .with_type(field_type::Enum::String),
    )
}

fn text_of(payload: &Option<PluginPayload>) -> String {
    payload
        .as_ref()
        .and_then(|p| p.fields.first())
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

/// Emits `a`, `b`, `c`, then is done.
struct Letters(Option<flwrs_plugin::source::local_sink::LocalSink>);

impl flwrs_plugin::source::plugin::Source for Letters {
    fn initialize(
        &mut self,
        id: String,
        _: LogLevel,
        sink: flwrs_plugin::source::local_sink::LocalSink,
    ) -> Result<InitializeRequest, InitializeError> {
        self.0 = Some(sink);
        Ok(InitializeRequest::new()
            .with_id(id)
            .with_version("1.0.0".into())
            .with_out_schema(text_schema()))
    }
    fn shutdown(&mut self) -> Result<(), ShutdownError> {
        Ok(())
    }
    fn version(&self) -> String {
        "1.0.0".into()
    }
    async fn run(&self) -> Result<(), SourceError> {
        for text in ["a", "b", "c"] {
            self.0.as_ref().unwrap().emit(text_payload(text)).await?;
        }
        Ok(())
    }
}

/// Emits `t1`, `t2`, … every 50 ms until it's stopped.
struct Ticker(Option<flwrs_plugin::source::local_sink::LocalSink>);

impl flwrs_plugin::source::plugin::Source for Ticker {
    fn initialize(
        &mut self,
        id: String,
        _: LogLevel,
        sink: flwrs_plugin::source::local_sink::LocalSink,
    ) -> Result<InitializeRequest, InitializeError> {
        self.0 = Some(sink);
        Ok(InitializeRequest::new()
            .with_id(id)
            .with_version("1.0.0".into())
            .with_out_schema(text_schema()))
    }
    fn shutdown(&mut self) -> Result<(), ShutdownError> {
        Ok(())
    }
    fn version(&self) -> String {
        "1.0.0".into()
    }
    async fn run(&self) -> Result<(), SourceError> {
        for n in 1.. {
            self.0
                .as_ref()
                .unwrap()
                .emit(text_payload(&format!("t{n}")))
                .await?;
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Ok(())
    }
}

/// Upper-cases (`upper`) or passes events through unchanged (`pass`).
struct Mapper {
    upper: bool,
}

impl flwrs_plugin::transform::plugin::Transform for Mapper {
    fn initialize(
        &mut self,
        id: String,
        _: LogLevel,
        _: flwrs_plugin::transform::local_sink::LocalSink,
    ) -> Result<InitializeRequest, InitializeError> {
        Ok(InitializeRequest::new()
            .with_id(id)
            .with_version("1.0.0".into())
            .with_in_schema(text_schema())
            .with_out_schema(text_schema()))
    }
    fn shutdown(&mut self) -> Result<(), ShutdownError> {
        Ok(())
    }
    fn version(&self) -> String {
        "1.0.0".into()
    }
    fn process_event(
        &mut self,
        event: transform::TransformEvent,
    ) -> Result<transform::TransformEvent, TransformError> {
        let text = text_of(&event.payload);
        let text = if self.upper {
            text.to_uppercase()
        } else {
            text
        };
        Ok(transform::TransformEvent {
            plugin_id: event.plugin_id,
            plugin_version: event.plugin_version,
            payload: Some(text_payload(&text)),
            metadata: None, // the SDK copies the input's
        })
    }
}

/// Appends `node text hops trace origin` for each event to a file.
struct Collector {
    id: String,
    out: PathBuf,
    /// Per event, to make the sink slower than its source.
    delay: Duration,
}

impl flwrs_plugin::sink::plugin::Sink for Collector {
    fn initialize(
        &mut self,
        id: String,
        _: LogLevel,
    ) -> Result<InitializeRequest, InitializeError> {
        Ok(InitializeRequest::new()
            .with_id(id)
            .with_version("1.0.0".into())
            .with_in_schema(text_schema()))
    }
    fn shutdown(&mut self) -> Result<(), ShutdownError> {
        Ok(())
    }
    fn version(&self) -> String {
        "1.0.0".into()
    }
    fn consume_event(&mut self, event: sink::SinkEvent) -> Result<(), SinkError> {
        use std::io::Write;
        std::thread::sleep(self.delay);
        // Like http_sink: events must be addressed to this instance.
        if event.plugin_id != self.id {
            return Err(SinkError::new(format!("event for {}", event.plugin_id)));
        }
        let meta = event.metadata.clone().unwrap_or_default();
        let line = format!(
            "{} {} {} {} {}\n",
            self.id,
            text_of(&event.payload),
            meta.hops,
            meta.trace_id,
            meta.origin_node_id
        );
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.out)
            .map_err(SinkError::new)?;
        file.write_all(line.as_bytes()).map_err(SinkError::new)
    }
}

/// Runs only as a child of the tests below.
#[tokio::test]
#[ignore = "runs as a child process of the director tests"]
async fn test_plugin() {
    let (Ok(mode), Ok(args)) = (std::env::var(MODE_ENV), std::env::var(ARGS_ENV)) else {
        return;
    };
    let args: Vec<&str> = args.split_whitespace().collect();
    let arg = |name: &str| {
        let i = args.iter().position(|a| *a == name).unwrap();
        args[i + 1].to_string()
    };
    let id = arg("--id");
    let connection = ConnectionConfig::new(arg("--host"), arg("--port").parse().unwrap());
    let result = match mode.as_str() {
        "source" => {
            use flwrs_plugin::source::runner::{SourceRunner, SourceRunnerConfig};
            let config = SourceRunnerConfig {
                plugin_id: id,
                log_level: LogLevel::Info,
                hub_connection: connection,
            };
            SourceRunner::initialize(Letters(None), config)
                .await
                .unwrap()
                .run()
                .await
        }
        "ticker" => {
            use flwrs_plugin::source::runner::{SourceRunner, SourceRunnerConfig};
            let config = SourceRunnerConfig {
                plugin_id: id,
                log_level: LogLevel::Info,
                hub_connection: connection,
            };
            SourceRunner::initialize(Ticker(None), config)
                .await
                .unwrap()
                .run()
                .await
        }
        "upper" | "pass" => {
            use flwrs_plugin::transform::runner::{TransformRunner, TransformRunnerConfig};
            let config = TransformRunnerConfig {
                plugin_id: id,
                log_level: LogLevel::Info,
                hub_connection: connection,
            };
            let mapper = Mapper {
                upper: mode == "upper",
            };
            TransformRunner::initialize(mapper, config)
                .await
                .unwrap()
                .run()
                .await
        }
        "sink" | "slow-sink" => {
            use flwrs_plugin::sink::runner::{SinkRunner, SinkRunnerConfig};
            let collector = Collector {
                id: id.clone(),
                out: std::env::var(OUT_ENV).unwrap().into(),
                delay: if mode == "slow-sink" {
                    Duration::from_millis(100)
                } else {
                    Duration::ZERO
                },
            };
            let config = SinkRunnerConfig {
                plugin_id: id,
                log_level: LogLevel::Info,
                hub_connection: connection,
            };
            SinkRunner::initialize(collector, config)
                .await
                .unwrap()
                .run()
                .await
        }
        other => panic!("unknown mode {other}"),
    };
    result.unwrap();
}

// --- test setup ---

struct Setup {
    f: Fixture,
    hub_addr: Option<std::net::SocketAddr>,
    dir: tempfile::TempDir,
    director: DirectorHandle,
    stop: CancellationToken,
    engine: Option<JoinHandle<()>>,
    hub_stop: CancellationToken,
}

impl Drop for Setup {
    fn drop(&mut self) {
        self.stop.cancel();
        self.hub_stop.cancel();
    }
}

/// Registers the test plugins: `t-source`, `t-ticker`, `t-upper`, `t-pass`,
/// `t-sink`, `t-slow-sink`.
async fn register_plugins(f: &Fixture, dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let exe = std::env::current_exe().unwrap();
    let out = dir.join("out.txt");
    let Database::SQLite(db) = f.plugins.service.db();
    let mut conn = db.get_connection().await.unwrap();
    for (id, plugin_type, mode, schemas) in [
        ("t-source", "SOURCE", "source", r#"{"out":"o.json"}"#),
        ("t-ticker", "SOURCE", "ticker", r#"{"out":"o.json"}"#),
        (
            "t-upper",
            "TRANSFORM",
            "upper",
            r#"{"in":"i.json","out":"o.json"}"#,
        ),
        (
            "t-pass",
            "TRANSFORM",
            "pass",
            r#"{"in":"i.json","out":"o.json"}"#,
        ),
        ("t-sink", "SINK", "sink", r#"{"in":"i.json"}"#),
        ("t-slow-sink", "SINK", "slow-sink", r#"{"in":"i.json"}"#),
    ] {
        let script = format!("{id}.sh");
        let path = dir.join(&script);
        std::fs::write(
            &path,
            format!(
                "#!/bin/sh\n[ -n \"$WARM_UP\" ] && exit 0\n{MODE_ENV}='{mode}' {OUT_ENV}='{}' {ARGS_ENV}=\"$*\" exec '{}' {CHILD_TEST} --exact --ignored --test-threads=1\n",
                out.display(),
                exe.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        // macOS scans a new executable on its first run; do that now.
        assert!(
            std::process::Command::new(&path)
                .env("WARM_UP", "1")
                .status()
                .unwrap()
                .success()
        );
        let manifest = format!(
            r#"{{"manifest_version":1,"id":"{id}","name":"{id}","version":"1.0.0","type":"{plugin_type}","executable":"{script}","payload_schemas":{schemas}}}"#
        );
        let has_in = plugin_type != "SOURCE";
        let has_out = plugin_type != "SINK";
        sqlx::query(
            "INSERT INTO plugins (id, name, version, \"type\", manifest, install_path, archive_sha256, in_schema, out_schema) \
             VALUES ($1, $1, '1.0.0', $2, $3, $4, 'x', $5, $6)",
        )
        .bind(id)
        .bind(plugin_type)
        .bind(manifest)
        .bind(dir.to_string_lossy().into_owned())
        .bind(has_in.then_some(TEXT_SCHEMA_JSON))
        .bind(has_out.then_some(TEXT_SCHEMA_JSON))
        .execute(&mut *conn)
        .await
        .unwrap();
    }
}

async fn setup() -> Setup {
    let f = fixture().await;
    let dir = tempfile::tempdir().unwrap();
    register_plugins(&f, dir.path()).await;
    Setup {
        f,
        hub_addr: None,
        dir,
        director: DirectorHandle::channel().0,
        stop: CancellationToken::new(),
        engine: None,
        hub_stop: CancellationToken::new(),
    }
}

impl Setup {
    /// Starts the director (with its own hub). It starts the enabled scenes.
    async fn start_director(&mut self, max_hops: u32) {
        let hub = Hub::bind(HubConfig::default()).await.unwrap();
        self.hub_addr = Some(hub.local_addr());
        let (running, token) = (hub.clone(), self.hub_stop.clone());
        tokio::spawn(async move { running.run(token).await });
        let ctx = Context {
            hub,
            budget: Budget::new(64 * 1024 * 1024),
            spill: SpillShared::new(
                self.dir.path().join("spool"),
                false,
                64 * 1024 * 1024,
                1024 * 1024 * 1024,
                0,
            ),
            max_hops,
            log_level: "info".into(),
        };
        let (handle, commands): (DirectorHandle, mpsc::Receiver<DirectorCommand>) =
            DirectorHandle::channel();
        self.director = handle;
        let engine = Engine::new(ctx, self.f.service);
        self.engine = Some(tokio::spawn(engine.run(commands, self.stop.clone())));
    }

    async fn node(&self, scene: &str, plugin: &str) -> String {
        self.f
            .service
            .create_node(scene, node_input(plugin, json!(null)))
            .await
            .unwrap()
            .id
    }

    async fn edge(&self, scene: &str, from: &str, to: &str) {
        self.f
            .service
            .create_edge(scene, edge_input(from, to, json!(null)))
            .await
            .unwrap();
    }

    async fn enabled_scene(&self) -> String {
        let scene = self.f.scene().await.id;
        // Enabling with no nodes is fine; nodes are added before reloading.
        self.f.service.set_enabled(&scene, true).await.unwrap();
        scene
    }

    /// Lines the sinks wrote: `node text hops trace origin`.
    fn lines(&self) -> Vec<Vec<String>> {
        std::fs::read_to_string(self.dir.path().join("out.txt"))
            .unwrap_or_default()
            .lines()
            .map(|l| l.split(' ').map(str::to_string).collect())
            .collect()
    }

    async fn wait_lines(&self, n: usize) -> Vec<Vec<String>> {
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            let lines = self.lines();
            if lines.len() >= n {
                return lines;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for {n} lines, got {lines:?}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    async fn status(&self, scene: &str) -> SceneStatus {
        self.director.status(scene).await.unwrap().unwrap()
    }

    /// Each node's pid and launch count: they stay the same while a node
    /// keeps running.
    async fn processes(&self, scene: &str) -> HashMap<String, (Option<u32>, u64)> {
        self.status(scene)
            .await
            .nodes
            .into_iter()
            .map(|n| (n.node_id, (n.status.pid, n.status.launches)))
            .collect()
    }

    /// Waits until `node` has written a line whose text satisfies `test`.
    async fn wait_text(&self, node: &str, test: impl Fn(&str) -> bool) {
        let deadline = tokio::time::Instant::now() + WAIT;
        while !self.lines().iter().any(|l| l[0] == node && test(&l[1])) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for {node}; lines: {:?}",
                self.lines()
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    fn count(&self, node: &str) -> usize {
        self.lines().iter().filter(|l| l[0] == node).count()
    }
}

fn texts(lines: &[Vec<String>], node: &str) -> Vec<String> {
    let mut texts: Vec<String> = lines
        .iter()
        .filter(|l| l[0] == node)
        .map(|l| l[1].clone())
        .collect();
    texts.sort();
    texts
}

// --- tests ---

#[tokio::test]
async fn source_transform_sink_pipeline_with_metadata_status_and_stop() {
    let mut s = setup().await;
    s.start_director(32).await;
    let scene = s.enabled_scene().await;
    let source = s.node(&scene, "t-source").await;
    let upper = s.node(&scene, "t-upper").await;
    let sink = s.node(&scene, "t-sink").await;
    s.edge(&scene, &source, &upper).await;
    s.edge(&scene, &upper, &sink).await;
    s.director.reload(&scene).await.unwrap();

    let lines = s.wait_lines(3).await;
    assert_eq!(texts(&lines, &sink), ["A", "B", "C"]);
    for line in &lines {
        assert_eq!(
            line[2], "2",
            "source → transform → sink is two hops: {line:?}"
        );
        assert_eq!(line[4], source, "origin is the source node");
        assert_eq!(line[3].len(), 26, "trace id is a ULID");
    }
    let mut traces: Vec<&String> = lines.iter().map(|l| &l[3]).collect();
    traces.dedup();
    assert_eq!(traces.len(), 3, "one trace per source event");

    let status = s.status(&scene).await;
    assert_eq!(status.state, SceneState::Running);
    assert_eq!(
        status.max_hops,
        Some(32),
        "no scene limit: the server default"
    );
    assert_eq!(status.nodes.len(), 3);
    let sink_report = status.nodes.iter().find(|n| n.node_id == sink).unwrap();
    assert_eq!(sink_report.queue.as_ref().unwrap().delivered, 3);
    // Counters: what each node produced and received.
    let counters = |id: &str| {
        status
            .nodes
            .iter()
            .find(|n| n.node_id == id)
            .unwrap()
            .counters
            .clone()
    };
    let (s_c, u_c, k_c) = (counters(&source), counters(&upper), counters(&sink));
    assert_eq!((s_c.emitted, s_c.received), (3, 0), "source");
    assert_eq!((u_c.emitted, u_c.received), (3, 3), "transform");
    assert_eq!((k_c.emitted, k_c.received), (0, 3), "sink");
    for c in [&s_c, &u_c, &k_c] {
        assert_eq!((c.dropped, c.mapping_errors, c.restarts), (0, 0, 0));
    }
    // The server overview lists the scene.
    let overview = s.director.overview().await.unwrap();
    let summary = overview
        .scenes
        .iter()
        .find(|x| x.scene_id == scene)
        .unwrap();
    assert_eq!(summary.state, SceneState::Running);
    assert_eq!((summary.nodes, summary.running_nodes), (3, 3));
    assert_eq!(overview.queued_memory.limit_bytes, 64 * 1024 * 1024);
    assert!(overview.hub_address.starts_with("127.0.0.1:"));

    s.director.request_stop(&scene);
    let status = s.status(&scene).await;
    assert_eq!(status.state, SceneState::Stopped);
    assert!(status.nodes.is_empty());
}

#[tokio::test]
async fn fan_out_and_fan_in() {
    let mut s = setup().await;
    s.start_director(32).await;
    let scene = s.enabled_scene().await;
    let first = s.node(&scene, "t-source").await;
    let second = s.node(&scene, "t-source").await;
    let both = s.node(&scene, "t-sink").await;
    let only_first = s.node(&scene, "t-sink").await;
    s.edge(&scene, &first, &both).await;
    s.edge(&scene, &second, &both).await;
    s.edge(&scene, &first, &only_first).await;
    s.director.reload(&scene).await.unwrap();

    let lines = s.wait_lines(9).await;
    assert_eq!(texts(&lines, &both), ["a", "a", "b", "b", "c", "c"]);
    assert_eq!(texts(&lines, &only_first), ["a", "b", "c"]);
}

#[tokio::test]
async fn a_cycle_is_stopped_by_the_scenes_hop_limit() {
    let mut s = setup().await;
    s.start_director(32).await; // server default
    let scene = s.enabled_scene().await;
    let limit = crate::modules::scene::service::SceneInput {
        name: "scene".into(),
        max_hops: Some(6), // the scene's own limit wins
    };
    s.f.service.update_scene(&scene, &limit).await.unwrap();
    let source = s.node(&scene, "t-source").await;
    let t1 = s.node(&scene, "t-pass").await;
    let t2 = s.node(&scene, "t-pass").await;
    let sink = s.node(&scene, "t-sink").await;
    s.edge(&scene, &source, &t1).await;
    s.edge(&scene, &t1, &t2).await;
    s.edge(&scene, &t2, &t1).await;
    s.edge(&scene, &t2, &sink).await;
    s.director.reload(&scene).await.unwrap();

    // Each event reaches the sink at hop 3 (s→t1→t2→sink) and 5 (one more
    // lap); the next lap would be hop 7 > 6.
    let lines = s.wait_lines(6).await;
    let mut hops: Vec<&str> = lines.iter().map(|l| l[2].as_str()).collect();
    hops.sort();
    assert_eq!(hops, ["3", "3", "3", "5", "5", "5"]);
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let status = s.status(&scene).await;
        assert_eq!(status.max_hops, Some(6));
        let t2_report = status.nodes.iter().find(|n| n.node_id == t2).unwrap();
        if t2_report.dropped_hops == 3 {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "{status:?}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(s.lines().len(), 6, "nothing more gets through");
}

#[tokio::test]
async fn a_reload_keeps_unchanged_nodes_and_swaps_edges_mappings_and_hop_limit() {
    let mut s = setup().await;
    s.start_director(32).await;
    let scene = s.enabled_scene().await;
    let ticker = s.node(&scene, "t-ticker").await;
    let a = s.node(&scene, "t-sink").await;
    let to_a =
        s.f.service
            .create_edge(&scene, edge_input(&ticker, &a, json!(null)))
            .await
            .unwrap()
            .id;
    s.director.reload(&scene).await.unwrap();
    s.wait_text(&a, |t| t == "t3").await;
    let before = s.processes(&scene).await;
    assert!(before.values().all(|(pid, launches)| pid.is_some() && *launches == 1));

    // A new sink behind a mapping: only it starts.
    let b = s.node(&scene, "t-sink").await;
    let to_b =
        s.f.service
            .create_edge(
                &scene,
                edge_input(&ticker, &b, json!({"rules": [{"target": "text", "value": "mapped"}]})),
            )
            .await
            .unwrap()
            .id;
    s.director.reload(&scene).await.unwrap();
    s.wait_text(&b, |t| t == "mapped").await;
    let after = s.processes(&scene).await;
    assert_eq!(after[&ticker], before[&ticker], "the source kept running");
    assert_eq!(after[&a], before[&a], "the first sink kept running");
    let a_before = s.count(&a);
    s.wait_text(&a, |t| t == format!("t{}", a_before + 5)).await;

    // Edges and mappings change without any restart.
    s.f.service.delete_edge(&scene, &to_a).await.unwrap();
    s.f.service
        .update_edge(
            &scene,
            &to_b,
            edge_input(&ticker, &b, json!({"rules": [{"target": "text", "value": "changed"}]})),
        )
        .await
        .unwrap();
    s.director.reload(&scene).await.unwrap();
    s.wait_text(&b, |t| t == "changed").await;
    assert_eq!(s.processes(&scene).await, after, "nothing restarted");
    tokio::time::sleep(Duration::from_millis(200)).await; // events in flight
    let a_count = s.count(&a);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(s.count(&a), a_count, "the first sink gets nothing any more");

    // So does the hop limit.
    let limit = crate::modules::scene::service::SceneInput {
        name: "scene".into(),
        max_hops: Some(7),
    };
    s.f.service.update_scene(&scene, &limit).await.unwrap();
    s.director.reload(&scene).await.unwrap();
    let status = s.status(&scene).await;
    assert_eq!(status.max_hops, Some(7));
    assert_eq!(s.processes(&scene).await, after, "nothing restarted");
    // The kept nodes' counters carry on.
    let source = status.nodes.iter().find(|n| n.node_id == ticker).unwrap();
    assert!(source.counters.emitted as usize > a_count, "{source:?}");

    // A removed node stops; the rest keep running.
    s.f.service.delete_node(&scene, &a).await.unwrap();
    s.director.reload(&scene).await.unwrap();
    let now = s.processes(&scene).await;
    assert_eq!(now.len(), 2);
    assert_eq!(now[&ticker], after[&ticker]);
    assert_eq!(now[&b], after[&b]);
}

#[tokio::test]
async fn a_changed_node_restarts_alone() {
    let mut s = setup().await;
    s.start_director(32).await;
    let scene = s.enabled_scene().await;
    let ticker = s.node(&scene, "t-ticker").await;
    let transform = s.node(&scene, "t-pass").await;
    let sink = s.node(&scene, "t-sink").await;
    s.edge(&scene, &ticker, &transform).await;
    s.edge(&scene, &transform, &sink).await;
    s.director.reload(&scene).await.unwrap();
    s.wait_text(&sink, |t| t.starts_with('t')).await;
    let before = s.processes(&scene).await;

    // Another plugin for the transform: only it restarts.
    s.f.service
        .update_node(&scene, &transform, node_input("t-upper", json!(null)))
        .await
        .unwrap();
    s.director.reload(&scene).await.unwrap();
    s.wait_text(&sink, |t| t.starts_with('T')).await;
    let after = s.processes(&scene).await;
    assert_eq!(after[&ticker], before[&ticker]);
    assert_eq!(after[&sink], before[&sink]);
    assert_ne!(after[&transform].0, before[&transform].0, "a new process");

    // A config change counts as a change too.
    s.f.service
        .update_node(
            &scene,
            &transform,
            node_input("t-upper", json!({"runtime": {"init_timeout_ms": 20000}})),
        )
        .await
        .unwrap();
    s.director.reload(&scene).await.unwrap();
    let again = s.processes(&scene).await;
    assert_ne!(again[&transform].0, after[&transform].0);
    assert_eq!(again[&ticker], before[&ticker]);
}

#[tokio::test]
async fn a_changed_remote_node_keeps_its_plugin_connected() {
    let mut s = setup().await;
    s.start_director(32).await;
    let scene = s.enabled_scene().await;
    let ticker = s.node(&scene, "t-ticker").await;
    let token = "remote-sink-shared-token";
    let config = |capacity: u64| {
        json!({"launch": "remote", "remote_id": "far-sink", "token": token,
               "runtime": {"queue": {"capacity": capacity}}})
    };
    let remote =
        s.f.service
            .create_node(&scene, node_input("t-sink", config(100)))
            .await
            .unwrap()
            .id;
    s.edge(&scene, &ticker, &remote).await;
    s.director.reload(&scene).await.unwrap();
    let addr = s.hub_addr.unwrap();
    let mut plugin = tokio::process::Command::new(s.dir.path().join("t-sink.sh"))
        .args([
            "--id",
            "far-sink",
            "--host",
            &addr.ip().to_string(),
            "--port",
            &addr.port().to_string(),
        ])
        .env("FLWRS_TOKEN", token)
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    s.wait_text("far-sink", |t| t == "t3").await;

    // The node changes (its queue): the hub drops the connection without a
    // Shutdown, and the plugin reconnects to the new node.
    s.f.service
        .update_node(&scene, &remote, node_input("t-sink", config(200)))
        .await
        .unwrap();
    s.director.reload(&scene).await.unwrap();
    let count = s.count("far-sink");
    s.wait_text("far-sink", |t| {
        t.strip_prefix('t')
            .and_then(|n| n.parse::<usize>().ok())
            .is_some_and(|n| n > count + 3)
    })
    .await;
    assert!(plugin.try_wait().unwrap().is_none(), "the remote plugin kept running");
    let status = s.status(&scene).await;
    let node = status.nodes.iter().find(|n| n.node_id == remote).unwrap();
    assert_eq!(node.status.state, crate::modules::supervisor::NodeState::Running);
}

#[tokio::test]
async fn a_scene_that_fails_its_checks_doesnt_start() {
    let mut s = setup().await;
    s.start_director(32).await;
    let scene = s.enabled_scene().await;
    let source = s.node(&scene, "t-source").await;
    let sink = s.node(&scene, "t-sink").await;
    s.edge(&scene, &source, &sink).await;
    // Break the graph behind the scene service's back: a sink → source edge.
    let Database::SQLite(db) = s.f.plugins.service.db();
    let mut conn = db.get_connection().await.unwrap();
    sqlx::query(
        "INSERT INTO scene_edges (id, scene_id, source_node_id, target_node_id) VALUES ('bad', $1, $2, $3)",
    )
    .bind(&scene)
    .bind(&sink)
    .bind(&source)
    .execute(&mut *conn)
    .await
    .unwrap();

    let err = s.director.reload(&scene).await.unwrap_err();
    assert!(
        err.to_string().contains("a sink can't send events"),
        "{err}"
    );
    let status = s.status(&scene).await;
    assert_eq!(status.state, SceneState::Failed);
    assert_eq!(
        status.errors,
        [
            "edge bad: a sink can't send events",
            "edge bad: a source can't receive events"
        ]
    );
    assert!(status.nodes.is_empty(), "nothing was started");
}

#[tokio::test]
async fn enabled_scenes_start_with_the_director_and_stop_with_it() {
    let mut s = setup().await;
    let scene = s.enabled_scene().await;
    let source = s.node(&scene, "t-source").await;
    let sink = s.node(&scene, "t-sink").await;
    s.edge(&scene, &source, &sink).await;
    let disabled = s.f.scene().await.id;
    s.node(&disabled, "t-source").await;

    s.start_director(32).await;
    let lines = s.wait_lines(3).await;
    assert_eq!(texts(&lines, &sink), ["a", "b", "c"]);
    assert!(s.director.status(&disabled).await.unwrap().is_none());

    s.stop.cancel();
    tokio::time::timeout(WAIT, s.engine.take().unwrap())
        .await
        .expect("the director stops")
        .unwrap();
}

#[tokio::test]
async fn a_remote_node_is_reached_by_its_remote_id() {
    let mut s = setup().await;
    s.start_director(32).await;
    let scene = s.enabled_scene().await;
    let source = s.node(&scene, "t-source").await;
    let token = "remote-sink-shared-token";
    let remote =
        s.f.service
            .create_node(
                &scene,
                node_input(
                    "t-sink",
                    json!({"launch": "remote", "remote_id": "far-sink", "token": token}),
                ),
            )
            .await
            .unwrap()
            .id;
    s.edge(&scene, &source, &remote).await;
    s.director.reload(&scene).await.unwrap();

    // The operator starts the remote plugin with its remote_id, not the node id.
    let addr = s.hub_addr.unwrap();
    let mut plugin = tokio::process::Command::new(s.dir.path().join("t-sink.sh"))
        .args([
            "--id",
            "far-sink",
            "--host",
            &addr.ip().to_string(),
            "--port",
            &addr.port().to_string(),
        ])
        .env("FLWRS_TOKEN", token)
        .kill_on_drop(true)
        .spawn()
        .unwrap();

    let lines = s.wait_lines(3).await;
    assert_eq!(texts(&lines, "far-sink"), ["a", "b", "c"]);
    s.director.request_stop(&scene);
    let status = s.status(&scene).await;
    assert_eq!(status.state, SceneState::Stopped);
    // The remote plugin got Shutdown and exits by itself.
    let exit = tokio::time::timeout(WAIT, plugin.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(exit.success());
}

#[tokio::test]
async fn spill_enabled_queues_deliver_in_order_and_spool_files_are_cleaned_up() {
    let mut s = setup().await;
    // A leftover from an "earlier run" is removed when the director starts.
    let spool = s.dir.path().join("spool");
    std::fs::create_dir_all(spool.join("old-scene")).unwrap();
    std::fs::write(spool.join("old-scene/node.db"), b"stale").unwrap();
    s.start_director(32).await;
    let deadline = tokio::time::Instant::now() + WAIT;
    while spool.join("old-scene").exists() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "leftover not removed"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let scene = s.enabled_scene().await;
    let source = s.node(&scene, "t-source").await;
    let sink = s
        .f
        .service
        .create_node(
            &scene,
            node_input(
                "t-slow-sink",
                json!({"runtime": {"queue": {"spill": {"enabled": true, "memory_max_events": 1, "write_batch": 1}}}}),
            ),
        )
        .await
        .unwrap()
        .id;
    s.edge(&scene, &source, &sink).await;
    s.director.reload(&scene).await.unwrap();

    let lines = s.wait_lines(3).await;
    let order: Vec<&str> = lines.iter().map(|l| l[1].as_str()).collect();
    assert_eq!(order, ["a", "b", "c"], "delivered in order");
    // The counters settle once the last delivery is acknowledged by the queue.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let status = s.status(&scene).await;
    let queue = status
        .nodes
        .iter()
        .find(|n| n.node_id == sink)
        .and_then(|n| n.queue.clone())
        .unwrap();
    assert!(queue.spill, "{queue:?}");
    assert_eq!(queue.delivered, 3);
    // The sink is slower than the source: events went to disk and came back.
    assert!(queue.spilled >= 1 && queue.refilled >= 1, "{queue:?}");
    assert!(!queue.spilling && queue.disk_len == 0, "drained: {queue:?}");

    s.director.request_stop(&scene);
    assert_eq!(s.status(&scene).await.state, SceneState::Stopped);
    assert!(
        !spool.join(&scene).exists(),
        "the scene's spool folder is removed"
    );
}

#[tokio::test]
async fn mappings_are_applied_and_survive_export_and_import() {
    let mut s = setup().await;
    s.start_director(32).await;
    let scene = s.enabled_scene().await;
    let source = s.node(&scene, "t-source").await;
    let upper = s.node(&scene, "t-upper").await;
    let sink = s.node(&scene, "t-sink").await;
    // source → upper copies the field under its own name (a no-op copy);
    // upper → sink replaces the payload with a constant built from rules.
    s.f.service
        .create_edge(
            &scene,
            edge_input(
                &source,
                &upper,
                json!({"rules": [{"target": "text", "source": "text"}]}),
            ),
        )
        .await
        .unwrap();
    s.f.service
        .create_edge(
            &scene,
            edge_input(
                &upper,
                &sink,
                json!({"mode": "replace", "rules": [{"target": "text", "value": "mapped"}]}),
            ),
        )
        .await
        .unwrap();
    s.director.reload(&scene).await.unwrap();
    let lines = s.wait_lines(3).await;
    assert_eq!(texts(&lines, &sink), ["mapped", "mapped", "mapped"]);
    let status = s.status(&scene).await;
    assert!(
        status.nodes.iter().all(|n| n.mapping_errors == 0),
        "{status:?}"
    );

    // Move the scene: export, import as a new scene, run that one.
    let export = s.f.service.export_scene(&scene, false).await.unwrap();
    s.f.service.set_enabled(&scene, false).await.unwrap();
    s.director.request_stop(&scene);
    let imported = s.f.service.import_scene(&export.bytes).await.unwrap();
    assert!(imported.check.problems.is_empty(), "{:?}", imported.check);
    let copy = imported.scene.id;
    s.f.service.set_enabled(&copy, true).await.unwrap();
    s.director.reload(&copy).await.unwrap();
    let copied_sink =
        s.f.service
            .list_nodes(&copy)
            .await
            .unwrap()
            .into_iter()
            .find(|n| n.plugin_id == "t-sink")
            .unwrap()
            .id;
    let lines = s.wait_lines(6).await;
    assert_eq!(texts(&lines, &copied_sink), ["mapped", "mapped", "mapped"]);
}

#[cfg(feature = "mapping-lua")]
#[tokio::test]
async fn lua_filters_and_scripts_run_and_survive_export_and_import() {
    let mut s = setup().await;
    s.start_director(32).await;
    let scene = s.enabled_scene().await;
    let source = s.node(&scene, "t-source").await;
    let sink = s.node(&scene, "t-sink").await;
    s.f.service
        .create_edge(
            &scene,
            edge_input(
                &source,
                &sink,
                json!({
                    "filter": {"lua": "return payload.text ~= 'b'"},
                    "script": {"lua": "return { text = payload.text:upper() .. meta.hops }"}
                }),
            ),
        )
        .await
        .unwrap();
    s.director.reload(&scene).await.unwrap();
    // "b" is filtered out; the others are reshaped (hop 1: source → sink).
    let lines = s.wait_lines(2).await;
    assert_eq!(texts(&lines, &sink), ["A1", "C1"]);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(s.lines().len(), 2, "nothing for the filtered event");

    let export = s.f.service.export_scene(&scene, false).await.unwrap();
    s.f.service.set_enabled(&scene, false).await.unwrap();
    s.director.request_stop(&scene);
    let imported = s.f.service.import_scene(&export.bytes).await.unwrap();
    assert!(imported.check.problems.is_empty(), "{:?}", imported.check);
    let copy = imported.scene.id;
    s.f.service.set_enabled(&copy, true).await.unwrap();
    s.director.reload(&copy).await.unwrap();
    let copied_sink =
        s.f.service
            .list_nodes(&copy)
            .await
            .unwrap()
            .into_iter()
            .find(|n| n.plugin_id == "t-sink")
            .unwrap()
            .id;
    let lines = s.wait_lines(4).await;
    assert_eq!(texts(&lines, &copied_sink), ["A1", "C1"]);
}

/// Without Lua support, a scene with a Lua mapping can exist (e.g. imported)
/// but doesn't start: it's a launch blocker.
#[cfg(not(feature = "mapping-lua"))]
#[tokio::test]
async fn without_lua_a_scene_with_lua_doesnt_start() {
    let mut s = setup().await;
    s.start_director(32).await;
    let scene = s.enabled_scene().await;
    let source = s.node(&scene, "t-source").await;
    let sink = s.node(&scene, "t-sink").await;
    // The scene service refuses Lua in this build; put it there like an import would.
    let Database::SQLite(db) = s.f.plugins.service.db();
    let mut conn = db.get_connection().await.unwrap();
    sqlx::query(
        "INSERT INTO scene_edges (id, scene_id, source_node_id, target_node_id, mapping) VALUES ('lua', $1, $2, $3, $4)",
    )
    .bind(&scene)
    .bind(&source)
    .bind(&sink)
    .bind(r#"{"script": {"lua": "return payload"}}"#)
    .execute(&mut *conn)
    .await
    .unwrap();
    let err = s.director.reload(&scene).await.unwrap_err();
    assert!(
        err.to_string().contains("built without Lua support"),
        "{err}"
    );
    let status = s.status(&scene).await;
    assert_eq!(status.state, SceneState::Failed);
    assert!(status.nodes.is_empty(), "nothing started");
}
