mod common;

use common::{FakeHub, TOKEN, connection, listen};
use flwrs_plugin::plugin::core::InitializeRequest;
use flwrs_plugin::plugin::error::{Error, InitializeError, ShutdownError, SinkError};
use flwrs_plugin::schema::common::{
    InitializeResponse, Shutdown, exit_code, log_level::Enum as LogLevel, msg_code::Enum as MsgCode,
};
use flwrs_plugin::schema::sink::{
    RuntimeSinkMessage, SinkEvent, SinkMessage, runtime_sink_message, sink_message::Payload,
};
use flwrs_plugin::sink::plugin::Sink;
use flwrs_plugin::sink::runner::{SinkRunner, SinkRunnerConfig};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::task::JoinHandle;

#[derive(Default)]
struct Counters {
    seen: Mutex<Vec<String>>,
    initialized: AtomicUsize,
    shut_down: AtomicUsize,
}

struct TestSink(Arc<Counters>);

impl Sink for TestSink {
    fn initialize(
        &mut self,
        id: String,
        _: LogLevel,
    ) -> Result<InitializeRequest, InitializeError> {
        self.0.initialized.fetch_add(1, Ordering::SeqCst);
        Ok(InitializeRequest::new()
            .with_id(id)
            .with_version("1".into()))
    }
    fn shutdown(&mut self) -> Result<(), ShutdownError> {
        self.0.shut_down.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    fn version(&self) -> String {
        "1".into()
    }
    fn consume_event(&mut self, event: SinkEvent) -> Result<(), SinkError> {
        if event.plugin_version != "1" {
            return Err(SinkError::new("bad version"));
        }
        self.0.seen.lock().unwrap().push(event.plugin_id);
        Ok(())
    }
}

fn is_log(msg: &SinkMessage) -> bool {
    matches!(msg.payload, Some(Payload::Log(_)))
}

fn event(version: &str) -> RuntimeSinkMessage {
    RuntimeSinkMessage {
        payload: Some(runtime_sink_message::Payload::Event(SinkEvent {
            plugin_id: "sink-1".into(),
            plugin_version: version.into(),
            payload: None,
            metadata: None,
        })),
    }
}

fn accept_msg() -> RuntimeSinkMessage {
    RuntimeSinkMessage {
        payload: Some(runtime_sink_message::Payload::Initialize(
            InitializeResponse {},
        )),
    }
}

fn shutdown_msg() -> RuntimeSinkMessage {
    RuntimeSinkMessage {
        payload: Some(runtime_sink_message::Payload::Shutdown(Shutdown {})),
    }
}

fn start(port: u16, counters: Arc<Counters>) -> JoinHandle<Result<(), Error>> {
    tokio::spawn(async move {
        let mut runner = SinkRunner::initialize(
            TestSink(counters),
            SinkRunnerConfig {
                plugin_id: "sink-1".into(),
                log_level: LogLevel::Trace,
                hub_connection: connection(port),
            },
        )
        .await
        .unwrap();
        runner.run().await
    })
}

/// Accepts a connection and checks its first frame is the expected Initialize.
async fn handshake(listener: &tokio::net::TcpListener) -> FakeHub {
    let mut hub = FakeHub::accept(listener).await;
    // Initialize must be the very first frame: don't skip logs here.
    let init: SinkMessage = hub.recv(|_| false).await;
    match init.payload {
        Some(Payload::Initialize(init)) => {
            assert_eq!(init.plugin_id, "sink-1");
            assert_eq!(init.plugin_version, "1");
            assert_eq!(init.token, TOKEN);
        }
        other => panic!("expected Initialize, got {other:?}"),
    }
    hub
}

async fn expect_exit_ok(hub: &mut FakeHub) {
    let exit: SinkMessage = hub.recv(is_log).await;
    match exit.payload {
        Some(Payload::Exit(exit)) => assert_eq!(exit.code, exit_code::Enum::Ok as i32),
        other => panic!("expected Exit, got {other:?}"),
    }
}

// The tests share the process-wide logger, so they run one at a time.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[tokio::test]
async fn sink_lifecycle() {
    let _serial = SERIAL.lock().await;
    let (listener, port) = listen().await;
    let counters = Arc::new(Counters::default());
    let runner = start(port, counters.clone());

    let mut hub = handshake(&listener).await;
    hub.send(&accept_msg()).await;
    hub.send(&event("1")).await;
    hub.send(&event("2")).await; // rejected by the plugin -> ErrorEvent
    let err: SinkMessage = hub.recv(is_log).await;
    match err.payload {
        Some(Payload::Error(e)) => assert_eq!(e.message, "bad version"),
        other => panic!("expected Error, got {other:?}"),
    }

    hub.send(&shutdown_msg()).await;
    expect_exit_ok(&mut hub).await;
    hub.expect_closed().await;

    runner.await.unwrap().unwrap();
    assert_eq!(*counters.seen.lock().unwrap(), vec!["sink-1".to_string()]);
    assert_eq!(counters.shut_down.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn reconnects_and_reinitializes_after_hub_restart() {
    let _serial = SERIAL.lock().await;
    let (listener, port) = listen().await;
    let counters = Arc::new(Counters::default());
    let runner = start(port, counters.clone());

    let mut hub = handshake(&listener).await;
    hub.send(&accept_msg()).await;
    hub.send(&event("1")).await;
    // Make sure the event got through before "restarting" the hub.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while counters.seen.lock().unwrap().is_empty() {
        assert!(tokio::time::Instant::now() < deadline);
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    drop(hub);

    // The plugin reconnects by itself and re-sends the same Initialize.
    let mut hub = handshake(&listener).await;
    hub.send(&accept_msg()).await;
    hub.send(&event("1")).await;
    hub.send(&shutdown_msg()).await;
    expect_exit_ok(&mut hub).await;

    runner.await.unwrap().unwrap();
    assert_eq!(counters.seen.lock().unwrap().len(), 2);
    // `Sink::initialize` runs once; the cached Initialize is re-sent.
    assert_eq!(counters.initialized.load(Ordering::SeqCst), 1);
    assert_eq!(counters.shut_down.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn shutdown_before_initialize_response_means_rejected() {
    let _serial = SERIAL.lock().await;
    let (listener, port) = listen().await;
    let counters = Arc::new(Counters::default());
    let runner = start(port, counters.clone());

    let mut hub = handshake(&listener).await;
    hub.send(&shutdown_msg()).await; // e.g. the token didn't match
    expect_exit_ok(&mut hub).await;
    hub.expect_closed().await;

    assert!(matches!(runner.await.unwrap(), Err(Error::Rejected)));
    assert_eq!(counters.shut_down.load(Ordering::SeqCst), 1);
    // It doesn't try again.
    let retry = tokio::time::timeout(std::time::Duration::from_millis(300), listener.accept());
    assert!(retry.await.is_err());
}

#[tokio::test]
async fn wrong_msg_code_from_hub_closes_and_reconnects() {
    let _serial = SERIAL.lock().await;
    let (listener, port) = listen().await;
    let counters = Arc::new(Counters::default());
    let runner = start(port, counters.clone());

    let mut hub = handshake(&listener).await;
    hub.send(&accept_msg()).await;
    // A sink must only receive RUNTIME_SINK_MESSAGE frames.
    hub.send_raw(MsgCode::RuntimeSourceMessage, &[]).await;
    hub.expect_closed().await;

    let mut hub = handshake(&listener).await;
    hub.send(&accept_msg()).await;
    hub.send(&shutdown_msg()).await;
    expect_exit_ok(&mut hub).await;
    runner.await.unwrap().unwrap();
}
