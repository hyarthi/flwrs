//! Supervisor tests. The "plugins" are this test binary, re-run as a child
//! process through a small shell script, in a chosen (mis)behaviour mode:
//! that's a real executable with real argv, environment and signals.

use super::node::{NodeHandle, NodeOutput, NodeState, NodeStatus, supervise};
use super::spec::{Backoff, Launch, LocalLaunch, NodeSpec, Timings};
use crate::modules::hub::{Hub, HubConfig};
#[cfg_attr(not(feature = "remote"), allow(unused_imports))]
use crate::modules::node_config::{NodeConfig, RuntimeConfig};
use crate::modules::plugin::manifest::PluginType;
use flwrs_plugin::plugin::frame::{PROTOCOL_VERSION, read_frame, write_frame, write_message};
use flwrs_plugin::schema::payload::{
    Field, FieldDefinition, FieldValue, PluginPayload, SchemaDefinition, field_type,
    field_value::Value,
};
use flwrs_plugin::schema::protocol::{
    Event, Exit, FrameHeader, Initialize, exit_code, msg_code::Enum as MsgCode,
    plugin_type::Enum as PbPluginType,
};
use prost::Message;
#[cfg_attr(not(feature = "remote"), allow(unused_imports))]
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

const WAIT: Duration = Duration::from_secs(10);
const CHILD_TEST: &str = "modules::supervisor::tests::fake_plugin";
const MODE_ENV: &str = "FLWRS_FAKE_MODE";
const ARGS_ENV: &str = "FLWRS_FAKE_ARGS";

fn text_schema() -> SchemaDefinition {
    SchemaDefinition {
        fields: vec![FieldDefinition {
            key: "text".into(),
            r#type: field_type::Enum::String as i32,
            ..Default::default()
        }],
    }
}

/// Fast timings for tests.
fn timings() -> Timings {
    Timings {
        init_timeout: Duration::from_secs(5),
        reconnect_timeout: Duration::from_secs(5),
        restart_delay: Duration::from_millis(50),
        backoff: None,
        kill_grace: Duration::from_millis(500),
        max_restarts: None,
        shutdown_grace: Duration::from_secs(5),
    }
}

struct Env {
    dir: tempfile::TempDir,
    hub: Hub,
    stop_hub: CancellationToken,
}

impl Drop for Env {
    fn drop(&mut self) {
        self.stop_hub.cancel();
    }
}

impl Env {
    async fn new() -> Self {
        let hub = Hub::bind(HubConfig::default()).await.unwrap();
        let stop_hub = CancellationToken::new();
        let (running, token) = (hub.clone(), stop_hub.clone());
        tokio::spawn(async move { running.run(token).await });
        Self {
            dir: tempfile::tempdir().unwrap(),
            hub,
            stop_hub,
        }
    }

    fn launches_file(&self) -> PathBuf {
        self.dir.path().join("launches")
    }

    /// Launches recorded by the script (one line each).
    fn launches(&self) -> usize {
        std::fs::read_to_string(self.launches_file())
            .map(|s| s.lines().count())
            .unwrap_or(0)
    }

    /// A plugin executable running the fake plugin in `mode`.
    #[cfg(unix)]
    fn script(&self, mode: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = self.dir.path().join(format!("plugin-{mode}"));
        let exe = std::env::current_exe().unwrap();
        std::fs::write(
            &path,
            format!(
                "#!/bin/sh\n[ -n \"$WARM_UP\" ] && exit 0\necho launched >> '{}'\n{MODE_ENV}='{mode}' {ARGS_ENV}=\"$*\" exec '{}' {CHILD_TEST} --exact --ignored --test-threads=1\n",
                self.launches_file().display(),
                exe.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        // A new executable's first run can be slow (the OS may check it
        // first); take that hit here, not in a timeout.
        let warm_up = std::process::Command::new(&path)
            .env("WARM_UP", "1")
            .status();
        assert!(warm_up.unwrap().success());
        path
    }

    fn spec(&self, plugin_type: PluginType, program: PathBuf, timings: Timings) -> NodeSpec {
        let (in_schema, out_schema) = match plugin_type {
            PluginType::Source => (None, Some(text_schema())),
            _ => (Some(text_schema()), None),
        };
        NodeSpec {
            scene_id: "scene".into(),
            node_id: "node".into(),
            instance_id: "node".into(),
            plugin_id: "fake".into(),
            plugin_type,
            plugin_version: "1.0.0".into(),
            in_schema,
            out_schema,
            launch: Launch::Local(Box::new(LocalLaunch {
                workdir: super::workdir::Workdir {
                    dir: self.dir.path().join("nodes/scene/node"),
                    install_dir: {
                        let install = self.dir.path().join("install");
                        std::fs::create_dir_all(&install).unwrap();
                        install
                    },
                    files: vec![],
                    uploads: vec![],
                },
                program,
                hub_addr: self.hub.local_addr(),
                log_level: "info".into(),
                threads: None,
                args: vec![],
                env: vec![],
            })),
            timings,
            queue: super::spec::QueueSpec::resolve(
                &RuntimeConfig::default(),
                &RuntimeConfig::default(),
            ),
            delivery: super::spec::DeliverySpec::resolve(
                &RuntimeConfig::default(),
                &RuntimeConfig::default(),
            ),
            max_event_bytes: None,
        }
    }

    fn start(&self, spec: NodeSpec) -> (NodeHandle, mpsc::Receiver<NodeOutput>) {
        let (tx, rx) = mpsc::channel(16);
        (supervise(spec, &self.hub, tx), rx)
    }
}

async fn wait_until(
    handle: &NodeHandle,
    what: &str,
    f: impl Fn(&NodeStatus) -> bool,
) -> NodeStatus {
    let mut watch = handle.watch();
    match tokio::time::timeout(WAIT, watch.wait_for(|s| f(s))).await {
        Ok(Ok(status)) => status.clone(),
        _ => panic!(
            "timed out waiting for {what}; status: {:?}",
            handle.status()
        ),
    }
}

async fn wait_state(handle: &NodeHandle, state: NodeState) -> NodeStatus {
    wait_until(handle, &format!("{state:?}"), |s| s.state == state).await
}

// --- the fake plugin (child process) ---

/// Runs only as a child of the tests below.
#[tokio::test]
#[ignore = "runs as a child process of the supervisor tests"]
async fn fake_plugin() {
    let (Ok(mode), Ok(args)) = (std::env::var(MODE_ENV), std::env::var(ARGS_ENV)) else {
        return;
    };
    let args: Vec<&str> = args.split_whitespace().collect();
    let arg = |name: &str| {
        let i = args
            .iter()
            .position(|a| *a == name)
            .expect("bootstrap arg present");
        args[i + 1].to_string()
    };
    let (id, host, port) = (arg("--id"), arg("--host"), arg("--port"));
    let token = std::env::var("FLWRS_TOKEN").expect("token in the environment");
    let addr = format!("{host}:{port}");

    match mode.as_str() {
        "silent" => tokio::time::sleep(Duration::from_secs(60)).await,
        "connect-only" => {
            let _stream = TcpStream::connect(&addr).await.unwrap();
            tokio::time::sleep(Duration::from_secs(60)).await;
        }
        "sdk-sink" => run_sdk_sink(&id, &host, port.parse().unwrap()).await,
        mode => run_raw(mode, &id, &addr, &token).await,
    }
}

async fn run_sdk_sink(id: &str, host: &str, port: u16) {
    use flwrs_plugin::plugin::core::{
        ConnectionConfig, FieldDefinition, InitializeRequest, SchemaDefinition,
    };
    use flwrs_plugin::sink::runner::{SinkRunner, SinkRunnerConfig};
    struct Noop;
    impl flwrs_plugin::sink::plugin::Sink for Noop {
        fn initialize(
            &mut self,
            id: String,
            _: flwrs_plugin::schema::protocol::log_level::Enum,
        ) -> Result<InitializeRequest, flwrs_plugin::plugin::error::InitializeError> {
            Ok(InitializeRequest::new()
                .with_id(id)
                .with_version("1.0.0".into())
                .with_in_schema(
                    SchemaDefinition::new().add_field(
                        FieldDefinition::new()
                            .with_key("text".into())
                            .with_type(field_type::Enum::String),
                    ),
                ))
        }
        fn shutdown(&mut self) -> Result<(), flwrs_plugin::plugin::error::ShutdownError> {
            Ok(())
        }
        fn version(&self) -> String {
            "1.0.0".into()
        }
        async fn consume_event(
            &self,
            _: flwrs_plugin::plugin::core::InboundEvent,
        ) -> Result<(), flwrs_plugin::plugin::error::SinkError> {
            Ok(())
        }
    }
    let mut runner = SinkRunner::initialize(
        Noop,
        SinkRunnerConfig::new(
            id.into(),
            flwrs_plugin::schema::protocol::log_level::Enum::Info,
            ConnectionConfig::new(host, port),
        ),
    )
    .await
    .unwrap();
    runner.run().await.unwrap();
}

/// Raw-protocol modes: a sink (or, for `source-emit`, a source) that
/// completes the handshake, then misbehaves as asked.
async fn run_raw(mode: &str, id: &str, addr: &str, token: &str) {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let version = if mode == "wrong-version" {
        "9.9.9"
    } else {
        "1.0.0"
    };
    let is_source = mode == "source-emit";
    let (plugin_type, in_schema, out_schema) = if is_source {
        (PbPluginType::Source, None, Some(text_schema()))
    } else {
        (PbPluginType::Sink, Some(text_schema()), None)
    };
    let init = Initialize {
        instance_id: id.into(),
        plugin_version: version.into(),
        plugin_type: plugin_type as i32,
        token: token.into(),
        in_schema,
        out_schema,
        min_protocol_version: PROTOCOL_VERSION,
        max_protocol_version: PROTOCOL_VERSION,
        capabilities: vec![],
        max_unacked: 0,
    };
    write_message(&mut stream, &init).await.unwrap();
    // Accept, or Shutdown if rejected (then just exit).
    let reply = read_frame(&mut stream).await.unwrap().unwrap();
    if reply.code() != MsgCode::InitializeResponse {
        return;
    }

    match mode {
        "crash" => {
            tokio::time::sleep(Duration::from_millis(200)).await;
            std::process::exit(1);
        }
        "exit-msg" => {
            let exit = Exit {
                code: exit_code::Enum::Error as i32,
                message: "giving up".into(),
            };
            write_message(&mut stream, &exit).await.unwrap();
            // ...but never actually exits: the supervisor must kill it.
            tokio::time::sleep(Duration::from_secs(60)).await;
        }
        "stubborn" => {
            // Ignores Shutdown and SIGTERM (the handler replaces the default
            // action); only SIGKILL works.
            #[cfg(unix)]
            let _sigterm =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).unwrap();
            tokio::time::sleep(Duration::from_secs(60)).await;
        }
        "with-child" => {
            // Starts a process of its own (in the plugin's working directory
            // the test can find its pid), then ignores Shutdown.
            // Never waited for on purpose: it must die with the process group.
            #[allow(clippy::zombie_processes)]
            let grandchild = std::process::Command::new("sleep")
                .arg("300")
                .spawn()
                .unwrap();
            std::fs::write("grandchild.pid", grandchild.id().to_string()).unwrap();
            tokio::time::sleep(Duration::from_secs(60)).await;
        }
        "source-emit" => {
            for (seq, text) in ["a", "b", "c"].into_iter().enumerate() {
                let header = FrameHeader {
                    code: MsgCode::Event as i32,
                    seq: seq as u64 + 1,
                    ..Default::default()
                };
                let body = Event {
                    payload: Some(PluginPayload {
                        fields: vec![Field {
                            key: "text".into(),
                            value: Some(FieldValue {
                                value: Some(Value::String(text.into())),
                            }),
                        }],
                    }),
                }
                .encode_to_vec();
                write_frame(&mut stream, &header, &body).await.unwrap();
            }
            tokio::time::sleep(Duration::from_secs(60)).await;
        }
        other => panic!("unknown mode {other}"),
    }
}

// --- tests ---

#[cfg(unix)]
#[tokio::test]
async fn never_connecting_plugin_is_relaunched_until_max_restarts() {
    let env = Env::new().await;
    let (handle, _out) = env.start(env.spec(
        PluginType::Sink,
        env.script("silent"),
        Timings {
            init_timeout: Duration::from_millis(300),
            max_restarts: Some(2),
            ..timings()
        },
    ));
    let status = wait_state(&handle, NodeState::Failed).await;
    assert_eq!(status.launches, 3, "{status:?}");
    assert_eq!(status.failed_launches, 3);
    assert_eq!(status.pid, None);
    let error = status.last_error.unwrap();
    assert!(
        error.contains("max_restarts = 2") && error.contains("no Initialize"),
        "{error}"
    );
    assert_eq!(env.launches(), 3);
    // Failed means no more launches.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(env.launches(), 3);
    assert_eq!(handle.stop().await.state, NodeState::Stopped);
}

#[cfg(unix)]
#[tokio::test]
async fn connected_but_silent_plugin_is_unhealthy() {
    let env = Env::new().await;
    let (handle, _out) = env.start(env.spec(
        PluginType::Sink,
        env.script("connect-only"),
        Timings {
            init_timeout: Duration::from_millis(300),
            ..timings()
        },
    ));
    let status = wait_until(&handle, "a second launch", |s| s.launches >= 2).await;
    assert!(status.failed_launches >= 1);
    assert!(
        status
            .last_error
            .unwrap()
            .contains("no Initialize within 300 ms")
    );
    assert_eq!(handle.stop().await.state, NodeState::Stopped);
}

#[cfg(unix)]
#[tokio::test]
async fn crash_after_running_is_relaunched() {
    let env = Env::new().await;
    let (handle, _out) = env.start(env.spec(PluginType::Sink, env.script("crash"), timings()));
    wait_state(&handle, NodeState::Running).await;
    let status = wait_until(&handle, "a relaunch that runs", |s| {
        s.launches >= 2 && s.state == NodeState::Running
    })
    .await;
    // Reaching Running resets the failed-launch count.
    assert_eq!(status.failed_launches, 0);
    assert!(status.last_error.unwrap().contains("process exited"));
    assert_eq!(handle.stop().await.state, NodeState::Stopped);
}

#[cfg(unix)]
#[tokio::test]
async fn unexpected_exit_message_kills_and_relaunches() {
    let env = Env::new().await;
    let (handle, _out) = env.start(env.spec(PluginType::Sink, env.script("exit-msg"), timings()));
    let status = wait_until(&handle, "a relaunch", |s| s.launches >= 2).await;
    let error = status.last_error.unwrap();
    assert!(
        error.contains("unexpected Exit (code ERROR: giving up)"),
        "{error}"
    );
    assert_eq!(handle.stop().await.state, NodeState::Stopped);
}

#[cfg(unix)]
#[tokio::test]
async fn rejected_plugin_fails_without_relaunch() {
    let env = Env::new().await;
    let (handle, _out) =
        env.start(env.spec(PluginType::Sink, env.script("wrong-version"), timings()));
    let status = wait_state(&handle, NodeState::Failed).await;
    assert_eq!(status.launches, 1);
    assert!(
        status
            .last_error
            .unwrap()
            .contains("version: 1.0.0 in the registry")
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(env.launches(), 1);
    handle.stop().await;
}

#[cfg(unix)]
#[tokio::test]
async fn missing_executable_counts_as_a_failed_launch() {
    let env = Env::new().await;
    let (handle, _out) = env.start(env.spec(
        PluginType::Sink,
        env.dir.path().join("does-not-exist"),
        Timings {
            max_restarts: Some(0),
            ..timings()
        },
    ));
    let status = wait_state(&handle, NodeState::Failed).await;
    assert_eq!(status.launches, 1);
    assert!(status.last_error.unwrap().contains("couldn't start"));
    handle.stop().await;
}

#[cfg(unix)]
#[tokio::test]
async fn sdk_plugin_stops_on_shutdown_without_signals() {
    let env = Env::new().await;
    let (handle, _out) = env.start(env.spec(
        PluginType::Sink,
        env.script("sdk-sink"),
        Timings {
            // Long graces: stopping fast proves the plugin exited on Shutdown.
            shutdown_grace: Duration::from_secs(20),
            kill_grace: Duration::from_secs(20),
            ..timings()
        },
    ));
    let running = wait_state(&handle, NodeState::Running).await;
    assert!(running.pid.is_some());
    // The handle's sender reaches the plugin.
    handle
        .sender()
        .send_event(&crate::modules::hub::wire::DataEvent::from_payload(
            PluginPayload::default(),
        ))
        .await
        .unwrap();
    let started = Instant::now();
    let status = handle.stop().await;
    assert_eq!(status.state, NodeState::Stopped);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(status.launches, 1);
}

#[cfg(unix)]
#[tokio::test]
async fn plugin_ignoring_shutdown_and_sigterm_is_killed() {
    let env = Env::new().await;
    let (handle, _out) = env.start(env.spec(
        PluginType::Sink,
        env.script("stubborn"),
        Timings {
            shutdown_grace: Duration::from_millis(300),
            kill_grace: Duration::from_millis(300),
            ..timings()
        },
    ));
    wait_state(&handle, NodeState::Running).await;
    let started = Instant::now();
    assert_eq!(handle.stop().await.state, NodeState::Stopped);
    let took = started.elapsed();
    assert!(
        took >= Duration::from_millis(550) && took < Duration::from_secs(5),
        "Shutdown grace, then SIGTERM grace, then SIGKILL: {took:?}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn stopping_a_plugin_stops_its_child_processes_too() {
    use nix::sys::signal::kill;
    use nix::unistd::Pid;
    let env = Env::new().await;
    let (handle, _out) = env.start(env.spec(
        PluginType::Sink,
        env.script("with-child"),
        Timings {
            shutdown_grace: Duration::from_millis(300),
            ..timings()
        },
    ));
    wait_state(&handle, NodeState::Running).await;
    // The plugin writes it in its working directory: the node's folder.
    let pid_file = env.dir.path().join("nodes/scene/node/grandchild.pid");
    let deadline = Instant::now() + WAIT;
    let grandchild = loop {
        if let Ok(pid) = std::fs::read_to_string(&pid_file) {
            break Pid::from_raw(pid.trim().parse().unwrap());
        }
        assert!(
            Instant::now() < deadline,
            "the plugin didn't start its child"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert!(kill(grandchild, None).is_ok(), "the grandchild is running");

    handle.stop().await;
    // Signalled with the plugin's process group; it's not our child, so it
    // can't be reaped here: poll until it's gone.
    let deadline = Instant::now() + WAIT;
    while kill(grandchild, None).is_ok() {
        assert!(
            Instant::now() < deadline,
            "the plugin's child process survived"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[cfg(unix)]
#[tokio::test]
async fn data_events_are_forwarded_with_the_node_id() {
    let env = Env::new().await;
    let (handle, mut out) =
        env.start(env.spec(PluginType::Source, env.script("source-emit"), timings()));
    let mut texts = vec![];
    for _ in 0..3 {
        let output = tokio::time::timeout(WAIT, out.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&*output.node_id, "node");
        let super::node::Output::Event { size, event, .. } = output.item else {
            panic!("expected an event, got {:?}", output.item);
        };
        assert!(size > 0);
        let field = &event.payload().unwrap().fields[0];
        if let Some(Value::String(s)) = field.value.as_ref().and_then(|v| v.value.clone()) {
            texts.push(s);
        }
    }
    assert_eq!(texts, ["a", "b", "c"]);
    handle.stop().await;
}

#[tokio::test]
async fn remote_node_waits_runs_and_reports_disconnects() {
    let env = Env::new().await;
    let mut spec = env.spec(PluginType::Sink, PathBuf::new(), timings());
    spec.launch = Launch::Remote {
        auth: crate::modules::hub::NodeAuth::local("remote-token-0123456789".into()),
    };
    let (handle, _out) = env.start(spec);
    assert_eq!(handle.status().state, NodeState::Waiting);

    let mut stream = TcpStream::connect(env.hub.local_addr()).await.unwrap();
    let init = Initialize {
        instance_id: "node".into(),
        plugin_version: "1.0.0".into(),
        plugin_type: PbPluginType::Sink as i32,
        token: "remote-token-0123456789".into(),
        in_schema: Some(text_schema()),
        min_protocol_version: PROTOCOL_VERSION,
        max_protocol_version: PROTOCOL_VERSION,
        ..Default::default()
    };
    write_message(&mut stream, &init).await.unwrap();
    wait_state(&handle, NodeState::Running).await;
    drop(stream);
    wait_state(&handle, NodeState::Disconnected).await;
    assert_eq!(handle.stop().await.state, NodeState::Stopped);
}

// --- spec resolution ---

#[test]
fn plugins_get_the_server_log_level() {
    let level = super::spec::server_log_level();
    assert!(["trace", "debug", "info", "warn", "error"].contains(&level.as_str()));
}

#[test]
fn timings_precedence_node_then_manifest_then_builtin() {
    let node = RuntimeConfig::parse(&serde_json::json!({
        "init_timeout_ms": 1, "restart_backoff": {"multiplier": 3.0}
    }))
    .unwrap();
    let manifest = RuntimeConfig::parse(&serde_json::json!({
        "init_timeout_ms": 2, "restart_delay_ms": 7, "max_restarts": 4
    }))
    .unwrap();
    let t = Timings::resolve(&node, &manifest);
    assert_eq!(t.init_timeout, Duration::from_millis(1));
    assert_eq!(t.restart_delay, Duration::from_millis(7));
    assert_eq!(t.max_restarts, Some(4));
    assert_eq!(t.reconnect_timeout, Duration::from_secs(60));
    assert_eq!(t.kill_grace, Duration::from_secs(2));
    assert_eq!(
        t.backoff,
        Some(Backoff {
            multiplier: 3.0,
            max_delay: Duration::from_secs(300),
            reset_after: Duration::from_secs(300),
        })
    );
    assert_eq!(
        Timings::resolve(&RuntimeConfig::default(), &RuntimeConfig::default()).backoff,
        None
    );
}

#[cfg(feature = "remote")] // creates remote nodes
#[tokio::test]
async fn spec_from_an_installed_plugin() {
    let f = crate::modules::plugin::service::tests::fixture(|_| {}).await;
    let plugin = f
        .service
        .install(&f.upload("test-sink", "1.0.0"))
        .await
        .unwrap();
    let hub: std::net::SocketAddr = "0.0.0.0:7400".parse().unwrap();
    let defaults = crate::modules::node_config::SceneDefaults::default();
    let spec = NodeSpec::resolve(
        "s",
        "n",
        &NodeConfig::default(),
        &plugin,
        &super::spec::Surroundings {
            hub_addr: hub,
            tls_available: false,
            log_level: "debug",
            scene_defaults: &defaults,
            nodes_dir: Path::new("nodes"),
            files_dir: Path::new("files"),
            uploads: &[],
            plugin_threads: None,
        },
    )
    .unwrap();
    let Launch::Local(local) = &spec.launch else {
        panic!("expected a local launch");
    };
    assert!(local.program.is_absolute() && local.program.ends_with("bin/run"));
    assert!(local.workdir.install_dir.is_dir());
    assert!(local.workdir.dir.is_absolute() && local.workdir.dir.ends_with("nodes/s/n"));
    assert_eq!(
        local.argv("n"),
        [
            "--id",
            "n",
            "--host",
            "127.0.0.1",
            "--port",
            "7400",
            "--log-level",
            "debug"
        ]
    );
    assert_eq!(spec.plugin_type, PluginType::Sink);
    assert!(spec.in_schema.is_some());

    let remote = NodeSpec::resolve(
        "s",
        "n",
        &NodeConfig::parse(
            &serde_json::json!({"launch": "remote", "token": "remote-token-0123456789"}),
        )
        .unwrap(),
        &plugin,
        &super::spec::Surroundings {
            hub_addr: hub,
            tls_available: false,
            log_level: "debug",
            scene_defaults: &defaults,
            nodes_dir: Path::new("nodes"),
            files_dir: Path::new("files"),
            uploads: &[],
            plugin_threads: None,
        },
    )
    .unwrap();
    assert!(matches!(
        remote.launch,
        Launch::Remote { auth: crate::modules::hub::NodeAuth::Token { ref token, .. } } if token == "remote-token-0123456789"
    ));

    let errors = NodeSpec::resolve(
        "s",
        "n",
        &NodeConfig::parse(
            &serde_json::json!({"runtime": {"init_timeout_ms": 0}, "launch_args": {"port": 1}}),
        )
        .unwrap(),
        &plugin,
        &super::spec::Surroundings {
            hub_addr: hub,
            tls_available: false,
            log_level: "debug",
            scene_defaults: &defaults,
            nodes_dir: Path::new("nodes"),
            files_dir: Path::new("files"),
            uploads: &[],
            plugin_threads: None,
        },
    )
    .unwrap_err();
    assert_eq!(errors.len(), 2, "{errors:?}");
}

#[cfg(unix)] // Unix-style absolute paths
#[test]
fn local_nodes_get_their_uploads_and_files() {
    use crate::modules::scene::service::NodeUpload;
    use flwrs_plugin::package::ConfigContent;
    let plugin = crate::modules::plugin::service::Plugin {
        id: "model".into(),
        name: "Model".into(),
        version: "1.0.0".into(),
        plugin_type: "SINK".into(),
        description: None,
        manifest: r#"{"manifest_version":2,"id":"model","name":"Model","version":"1.0.0","type":"SINK","executable":"bin/run","launch_args_schema":"l.json","payload_schemas":{"in":"i.json"}}"#.into(),
        launch_args_schema: Some(
            r#"{"type":"object","properties":{
                "model":{"type":"string","x-flwrs-launch":{"via":"file","source":"upload"}},
                "settings":{"type":"object"},
                "password":{"type":"string","writeOnly":true,"x-flwrs-launch":{"via":"env","alias":"MODEL_PASSWORD"}}}}"#
                .into(),
        ),
        in_schema: Some(r#"{"fields":[{"key":"x","type":"STRING"}]}"#.into()),
        out_schema: None,
        install_path: "/opt/plugins/model/1.0.0".into(),
        archive_sha256: "x".into(),
        create_time: Default::default(),
        update_time: Default::default(),
    };
    let config = NodeConfig::parse(&serde_json::json!({"launch_args": {
        "model": "resnet.onnx", "settings": {"a": 1}, "password": "pw"
    }}))
    .unwrap();
    let upload = NodeUpload {
        node_id: "n".into(),
        name: "resnet.onnx".into(),
        sha256: "ab".repeat(32),
        size: 3,
        create_time: Default::default(),
    };
    let defaults = crate::modules::node_config::SceneDefaults::default();
    let resolve = |uploads: &[NodeUpload]| {
        NodeSpec::resolve(
            "s",
            "n",
            &config,
            &plugin,
            &super::spec::Surroundings {
                hub_addr: "127.0.0.1:7400".parse().unwrap(),
                tls_available: false,
                log_level: "info",
                scene_defaults: &defaults,
                nodes_dir: Path::new("/data/nodes"),
                files_dir: Path::new("/data/files"),
                uploads,
                plugin_threads: None,
            },
        )
    };
    let spec = resolve(std::slice::from_ref(&upload)).unwrap();
    let Launch::Local(local) = &spec.launch else {
        panic!("expected a local launch");
    };
    let config_dir = Path::new("/data/nodes/s/n/config");
    assert_eq!(local.workdir.dir, Path::new("/data/nodes/s/n"));
    assert_eq!(
        local.args,
        [
            "--model".to_string(),
            config_dir
                .join("resnet.onnx")
                .to_string_lossy()
                .into_owned(),
            "--settings".into(),
            config_dir
                .join("settings.json")
                .to_string_lossy()
                .into_owned(),
        ]
    );
    assert_eq!(
        local.env,
        [("MODEL_PASSWORD".to_string(), "pw".to_string())]
    );
    assert_eq!(
        local.workdir.upload("resnet.onnx").unwrap(),
        Path::new("/data/files").join(format!("resnet_{}.onnx", "ab".repeat(32)))
    );
    assert!(local.workdir.files.iter().any(
        |f| f.name == "resnet.onnx" && f.content == ConfigContent::Upload("resnet.onnx".into())
    ));
    // not uploaded: the node can't start
    let errors = resolve(&[]).unwrap_err();
    assert!(
        errors[0].contains("no uploaded file named \"resnet.onnx\""),
        "{errors:?}"
    );
}

#[test]
fn the_core_count_goes_as_the_manifest_says() {
    use super::spec::{ThreadsDelivery, threads_delivery};
    let manifest = |threads: serde_json::Value| -> flwrs_plugin::package::Manifest {
        let mut m = serde_json::json!({"manifest_version": 2, "id": "p", "name": "P", "version": "1.0.0",
            "type": "SINK", "executable": "run", "payload_schemas": {"in": "i.json"}});
        if !threads.is_null() {
            m["threads"] = threads;
        }
        serde_json::from_value(m).unwrap()
    };
    let arg = manifest(serde_json::json!({"via": "arg", "name": "threads"}));
    let go = manifest(serde_json::json!({"via": "env", "name": "GOMAXPROCS"}));
    let jvm = manifest(
        serde_json::json!({"via": "env", "name": "JAVA_TOOL_OPTIONS", "value": "-XX:ActiveProcessorCount={n}"}),
    );
    assert_eq!(
        threads_delivery(&arg, Some(2)),
        Some(ThreadsDelivery::Arg("threads".into(), "2".into()))
    );
    assert_eq!(
        threads_delivery(&go, Some(1)),
        Some(ThreadsDelivery::Env("GOMAXPROCS".into(), "1".into()))
    );
    assert_eq!(
        threads_delivery(&jvm, Some(4)),
        Some(ThreadsDelivery::Env(
            "JAVA_TOOL_OPTIONS".into(),
            "-XX:ActiveProcessorCount=4".into()
        ))
    );
    // 0 or nothing: the plugin decides; a plugin without the declaration gets nothing.
    assert_eq!(threads_delivery(&arg, Some(0)), None);
    assert_eq!(threads_delivery(&arg, None), None);
    assert_eq!(
        threads_delivery(&manifest(serde_json::Value::Null), Some(2)),
        None
    );

    // In the argument list (after the bootstrap arguments) or the environment.
    let launch = |threads| LocalLaunch {
        program: "run".into(),
        workdir: super::workdir::Workdir {
            dir: "d".into(),
            install_dir: "i".into(),
            files: vec![],
            uploads: vec![],
        },
        hub_addr: "127.0.0.1:1".parse().unwrap(),
        log_level: "info".into(),
        threads,
        args: vec!["--own".into(), "x".into()],
        env: vec![("OWN".into(), "y".into())],
    };
    let l = launch(threads_delivery(&arg, Some(3)));
    assert_eq!(l.argv("n")[8..], ["--threads", "3", "--own", "x"]);
    assert_eq!(l.environment(), [("OWN".to_string(), "y".to_string())]);
    let l = launch(threads_delivery(&go, Some(3)));
    assert_eq!(l.argv("n")[8..], ["--own", "x"]);
    assert_eq!(
        l.environment()[1],
        ("GOMAXPROCS".to_string(), "3".to_string())
    );
}
