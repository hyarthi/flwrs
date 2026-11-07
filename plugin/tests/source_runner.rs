mod common;

use common::{FakeHub, connection, listen, payload, text};
use flwrs_plugin::plugin::core::InitializeRequest;
use flwrs_plugin::plugin::error::{InitializeError, ShutdownError, SourceError};
use flwrs_plugin::schema::protocol::{
    Ack, Credit, ErrorEvent, Exit, Nack, capability::Enum as Capability, exit_code,
    log_level::Enum as LogLevel, msg_code::Enum as MsgCode, plugin_type::Enum as PluginType,
};
use flwrs_plugin::source::local_sink::LocalSink;
use flwrs_plugin::source::plugin::Source;
use flwrs_plugin::source::runner::{SourceRunner, SourceRunnerConfig};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

/// How a test drives the source's `run`.
#[derive(Clone, Copy)]
enum Mode {
    /// Emit one event, fail once (to exercise restart), then emit once per
    /// run and idle.
    FailOnce,
    /// Emit `n` events as fast as credit allows, then idle.
    Burst(usize),
    /// Emit confirmed events and report each outcome.
    Confirmed(usize),
}

struct TestSource {
    mode: Mode,
    sink: Option<LocalSink>,
    failed_once: AtomicBool,
    runs: Arc<AtomicUsize>,
    shut_down: Arc<AtomicBool>,
    outcomes: mpsc::UnboundedSender<Result<(), String>>,
}

impl Source for TestSource {
    fn initialize(
        &mut self,
        id: String,
        _: LogLevel,
        sink: LocalSink,
    ) -> Result<InitializeRequest, InitializeError> {
        self.sink = Some(sink);
        Ok(InitializeRequest::new().with_id(id).with_version(self.version()))
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
        match self.mode {
            Mode::FailOnce => {
                sink.emit(payload(vec![])).await?;
                if !self.failed_once.swap(true, Ordering::SeqCst) {
                    return Err(SourceError::new("boom"));
                }
            }
            Mode::Burst(n) => {
                for i in 0..n {
                    sink.emit(payload(vec![text("n", &i.to_string())])).await?;
                }
            }
            Mode::Confirmed(n) => {
                for _ in 0..n {
                    let outcome = sink
                        .emit_confirmed(payload(vec![]))
                        .await
                        .map_err(|e| e.to_string());
                    self.outcomes.send(outcome).unwrap();
                }
            }
        }
        loop {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

struct Started {
    runner: tokio::task::JoinHandle<Result<(), flwrs_plugin::plugin::error::Error>>,
    runs: Arc<AtomicUsize>,
    shut_down: Arc<AtomicBool>,
    outcomes: mpsc::UnboundedReceiver<Result<(), String>>,
}

fn start(port: u16, mode: Mode) -> Started {
    let runs = Arc::new(AtomicUsize::new(0));
    let shut_down = Arc::new(AtomicBool::new(false));
    let (tx, outcomes) = mpsc::unbounded_channel();
    let plugin = TestSource {
        mode,
        sink: None,
        failed_once: AtomicBool::new(false),
        runs: runs.clone(),
        shut_down: shut_down.clone(),
        outcomes: tx,
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
    Started {
        runner,
        runs,
        shut_down,
        outcomes,
    }
}

/// `kind` of the next non-log frame, checking event headers on the way.
async fn kind(hub: &mut FakeHub, expected_seq: &mut u64) -> &'static str {
    let frame = hub.next().await;
    match frame.code() {
        MsgCode::Event => {
            assert_eq!(frame.header.seq, *expected_seq, "seq numbers have no gaps");
            *expected_seq += 1;
            assert_eq!(frame.header.input_seq, 0);
            assert!(frame.header.metadata.is_none(), "the hub fills in metadata");
            "event"
        }
        MsgCode::Error => {
            let e: ErrorEvent = frame.decode().unwrap();
            assert_eq!(e.message, "boom");
            "error"
        }
        other => panic!("unexpected {other:?}"),
    }
}

async fn stop(hub: &mut FakeHub, started: Started) {
    hub.shutdown("").await;
    // Events sent before the plugin saw the Shutdown may still arrive.
    let exit: Exit = loop {
        let frame = hub.next().await;
        if frame.code() != MsgCode::Event {
            break frame.decode().unwrap();
        }
    };
    assert_eq!(exit.code, exit_code::Enum::Ok as i32);
    tokio::time::timeout(Duration::from_secs(5), started.runner)
        .await
        .expect("the runner didn't finish")
        .unwrap()
        .unwrap();
    assert!(started.shut_down.load(Ordering::SeqCst));
}

/// The source's next confirmation outcome (with a timeout, so a bug fails
/// the test instead of hanging it).
async fn outcome(started: &mut Started) -> Result<(), String> {
    tokio::time::timeout(Duration::from_secs(5), started.outcomes.recv())
        .await
        .expect("timed out waiting for a confirmation")
        .unwrap()
}

// The tests share the process-wide logger, so they run one at a time.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[tokio::test]
async fn source_lifecycle_with_restart_and_reconnect() {
    let _serial = SERIAL.lock().await;
    let (listener, port) = listen().await;
    let started = start(port, Mode::FailOnce);

    let (mut hub, init) = FakeHub::handshake(&listener).await;
    assert_eq!(init.plugin_type, PluginType::Source as i32);
    assert!(init.out_schema.is_some() && init.in_schema.is_none());
    // Nothing but logs until the hub accepts the plugin.
    hub.expect_quiet(Duration::from_millis(200)).await;
    assert_eq!(started.runs.load(Ordering::SeqCst), 0);

    hub.accept_plugin(&[], 0).await;
    let mut seq = 1;
    let mut kinds = vec![];
    for _ in 0..3 {
        kinds.push(kind(&mut hub, &mut seq).await);
    }
    // The first run emits and fails; the restarted run emits again.
    assert_eq!(kinds, vec!["event", "error", "event"]);
    assert_eq!(started.runs.load(Ordering::SeqCst), 2);

    // Hub restart: `run` is cancelled, and restarted after re-acceptance,
    // with seq numbers starting again at 1.
    drop(hub);
    let (mut hub, _) = FakeHub::handshake(&listener).await;
    hub.accept_plugin(&[], 0).await;
    let mut seq = 1;
    assert_eq!(kind(&mut hub, &mut seq).await, "event");
    assert_eq!(started.runs.load(Ordering::SeqCst), 3);

    // Shutdown must interrupt the endless run loop.
    stop(&mut hub, started).await;
}

#[tokio::test]
async fn credits_limit_what_the_source_sends() {
    let _serial = SERIAL.lock().await;
    let (listener, port) = listen().await;
    let started = start(port, Mode::Burst(5));

    let (mut hub, _) = FakeHub::handshake(&listener).await;
    hub.accept_plugin(&[Capability::Credits], 0).await;
    // No credit yet: nothing is sent.
    hub.expect_quiet(Duration::from_millis(150)).await;
    hub.send(&Credit { events: 2 }).await;
    let mut seq = 1;
    for _ in 0..2 {
        assert_eq!(kind(&mut hub, &mut seq).await, "event");
    }
    hub.expect_quiet(Duration::from_millis(150)).await;
    hub.send(&Credit { events: 10 }).await;
    for _ in 0..3 {
        assert_eq!(kind(&mut hub, &mut seq).await, "event");
    }
    hub.expect_quiet(Duration::from_millis(100)).await;
    stop(&mut hub, started).await;
}

#[tokio::test]
async fn emit_confirmed_waits_for_the_hubs_answer() {
    let _serial = SERIAL.lock().await;
    let (listener, port) = listen().await;
    let mut started = start(port, Mode::Confirmed(3));

    let (mut hub, _) = FakeHub::handshake(&listener).await;
    hub.accept_plugin(&[Capability::Acks], 0).await;
    // One at a time: the next event comes only after the answer.
    let mut seq = 1;
    assert_eq!(kind(&mut hub, &mut seq).await, "event");
    hub.expect_quiet(Duration::from_millis(100)).await;
    hub.send(&Ack { up_to: 1, seqs: vec![] }).await;
    assert_eq!(outcome(&mut started).await, Ok(()));

    assert_eq!(kind(&mut hub, &mut seq).await, "event");
    hub.send(&Nack {
        seq: 2,
        reason: "too large".into(),
        retry: false,
    })
    .await;
    assert_eq!(
        outcome(&mut started).await,
        Err("too large".to_string())
    );

    // A lost connection fails the pending confirmation and cancels `run`;
    // depending on which comes first, the failure is reported or not.
    assert_eq!(kind(&mut hub, &mut seq).await, "event");
    drop(hub);

    let (mut hub, _) = FakeHub::handshake(&listener).await;
    hub.accept_plugin(&[Capability::Acks], 0).await;
    let mut seq = 1;
    // The restarted run emits again (its first event).
    assert_eq!(kind(&mut hub, &mut seq).await, "event");
    hub.send(&Ack { up_to: 1, seqs: vec![] }).await;
    let mut next = outcome(&mut started).await;
    if next == Err("the connection to the hub was lost".to_string()) {
        next = outcome(&mut started).await;
    }
    assert_eq!(next, Ok(()));
    stop(&mut hub, started).await;
}
