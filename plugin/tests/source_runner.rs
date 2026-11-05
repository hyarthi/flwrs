mod common;

use common::{FakeHub, TOKEN, connection, listen};
use flwrs_plugin::plugin::core::InitializeRequest;
use flwrs_plugin::plugin::error::{InitializeError, ShutdownError, SourceError};
use flwrs_plugin::schema::common::{
    InitializeResponse, Shutdown, exit_code, log_level::Enum as LogLevel,
};
use flwrs_plugin::schema::schema::PluginPayload;
use flwrs_plugin::schema::source::{
    RuntimeSourceMessage, SourceMessage, runtime_source_message, source_message::Payload,
};
use flwrs_plugin::source::local_sink::LocalSink;
use flwrs_plugin::source::plugin::Source;
use flwrs_plugin::source::runner::{SourceRunner, SourceRunnerConfig};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

/// Emits one event, fails once (to exercise restart), then emits once per run
/// and idles forever.
struct TestSource {
    sink: Option<LocalSink>,
    failed_once: AtomicBool,
    runs: Arc<AtomicUsize>,
    shut_down: Arc<AtomicBool>,
}

impl Source for TestSource {
    fn initialize(
        &mut self,
        id: String,
        _: LogLevel,
        sink: LocalSink,
    ) -> Result<InitializeRequest, InitializeError> {
        self.sink = Some(sink);
        Ok(InitializeRequest::new()
            .with_id(id)
            .with_version(self.version()))
    }
    fn shutdown(&mut self) -> Result<(), ShutdownError> {
        self.shut_down.store(true, Ordering::SeqCst);
        Ok(())
    }
    fn version(&self) -> String {
        "1".into()
    }
    async fn run(&self) -> Result<(), SourceError> {
        self.runs.fetch_add(1, Ordering::SeqCst);
        let sink = self.sink.as_ref().unwrap();
        sink.emit(PluginPayload { fields: vec![] }).await?;
        if !self.failed_once.swap(true, Ordering::SeqCst) {
            return Err(SourceError::new("boom"));
        }
        loop {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

fn is_log(msg: &SourceMessage) -> bool {
    matches!(msg.payload, Some(Payload::Log(_)))
}

fn accept_msg() -> RuntimeSourceMessage {
    RuntimeSourceMessage {
        payload: Some(runtime_source_message::Payload::Initialize(
            InitializeResponse {},
        )),
    }
}

fn kind(msg: SourceMessage) -> &'static str {
    match msg.payload {
        Some(Payload::Initialize(init)) => {
            assert_eq!(init.token, TOKEN);
            "init"
        }
        Some(Payload::Event(e)) => {
            assert_eq!(e.plugin_id, "src-1");
            assert_eq!(e.plugin_version, "1");
            assert!(e.metadata.is_none(), "the hub fills in source metadata");
            "event"
        }
        Some(Payload::Error(e)) => {
            assert_eq!(e.message, "boom");
            "error"
        }
        other => panic!("unexpected {other:?}"),
    }
}

#[tokio::test]
async fn source_lifecycle_with_restart_and_reconnect() {
    let (listener, port) = listen().await;
    let runs = Arc::new(AtomicUsize::new(0));
    let shut_down = Arc::new(AtomicBool::new(false));
    let plugin = TestSource {
        sink: None,
        failed_once: AtomicBool::new(false),
        runs: runs.clone(),
        shut_down: shut_down.clone(),
    };

    let runner = tokio::spawn(async move {
        let mut runner = SourceRunner::initialize(
            plugin,
            SourceRunnerConfig {
                plugin_id: "src-1".into(),
                log_level: LogLevel::Info,
                hub_connection: connection(port),
            },
        )
        .await
        .unwrap();
        runner.run().await
    });
    let mut hub = FakeHub::accept(&listener).await;

    // Nothing but Initialize (and logs) until the hub accepts the plugin.
    assert_eq!(kind(hub.recv(is_log).await), "init");
    let early = tokio::time::timeout(Duration::from_millis(200), hub.recv(is_log));
    assert!(early.await.is_err(), "source ran before InitializeResponse");
    assert_eq!(runs.load(Ordering::SeqCst), 0);

    hub.send(&accept_msg()).await;
    let mut kinds = vec![];
    for _ in 0..3 {
        kinds.push(kind(hub.recv(is_log).await));
    }
    // The first run emits and fails; the restarted run emits again.
    assert_eq!(kinds, vec!["event", "error", "event"]);
    assert_eq!(runs.load(Ordering::SeqCst), 2);

    // Hub restart: `run` is cancelled, and restarted after re-acceptance.
    drop(hub);
    let mut hub = FakeHub::accept(&listener).await;
    assert_eq!(kind(hub.recv(is_log).await), "init");
    hub.send(&accept_msg()).await;
    assert_eq!(kind(hub.recv(is_log).await), "event");
    assert_eq!(runs.load(Ordering::SeqCst), 3);

    // Shutdown must interrupt the endless run loop.
    hub.send(&RuntimeSourceMessage {
        payload: Some(runtime_source_message::Payload::Shutdown(Shutdown {})),
    })
    .await;
    let exit: SourceMessage = hub.recv(is_log).await;
    match exit.payload {
        Some(Payload::Exit(exit)) => assert_eq!(exit.code, exit_code::Enum::Ok as i32),
        other => panic!("expected Exit, got {other:?}"),
    }
    runner.await.unwrap().unwrap();
    assert!(shut_down.load(Ordering::SeqCst));
}
