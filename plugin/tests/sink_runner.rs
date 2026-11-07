mod common;

use common::{FakeHub, connection, listen, payload, text};
use flwrs_plugin::plugin::core::{InboundEvent, InitializeRequest};
use flwrs_plugin::plugin::error::{Error, InitializeError, ShutdownError, SinkError};
use flwrs_plugin::schema::payload::field_value::Value;
use flwrs_plugin::schema::protocol::{
    Ack, EventMetadata, Exit, Nack, Ping, Pong, capability::Enum as Capability, exit_code,
    log_level::Enum as LogLevel, msg_code::Enum as MsgCode, plugin_type::Enum as PluginType,
};
use flwrs_plugin::sink::plugin::Sink;
use flwrs_plugin::sink::runner::{DEFAULT_MAX_UNACKED, SinkRunner, SinkRunnerConfig};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::task::JoinHandle;

#[derive(Default)]
struct Counters {
    seen: Mutex<Vec<InboundEvent>>,
    initialized: AtomicUsize,
    shut_down: AtomicUsize,
    /// "slow" events wait for this.
    gate: tokio::sync::Notify,
}

/// Accepts events whose `text` field is "ok"; "bad" fails for good, "later"
/// fails with a retryable error; "slow" is accepted once `gate` opens.
struct TestSink(Arc<Counters>);

impl Sink for TestSink {
    fn initialize(&mut self, id: String, _: LogLevel) -> Result<InitializeRequest, InitializeError> {
        self.0.initialized.fetch_add(1, Ordering::SeqCst);
        Ok(InitializeRequest::new().with_id(id).with_version("1".into()))
    }
    fn shutdown(&mut self) -> Result<(), ShutdownError> {
        self.0.shut_down.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    fn version(&self) -> String {
        "1".into()
    }
    async fn consume_event(&self, event: InboundEvent) -> Result<(), SinkError> {
        let text = match event.payload.fields.first().and_then(|f| f.value.clone()) {
            Some(value) => match value.value {
                Some(Value::String(s)) => s,
                _ => String::new(),
            },
            None => String::new(),
        };
        match text.as_str() {
            "bad" => Err(SinkError::new("bad event")),
            "later" => Err(SinkError::retryable("try again later")),
            "slow" => {
                self.0.gate.notified().await;
                self.0.seen.lock().unwrap().push(event);
                Ok(())
            }
            _ => {
                self.0.seen.lock().unwrap().push(event);
                Ok(())
            }
        }
    }
}

fn start(port: u16, counters: Arc<Counters>) -> JoinHandle<Result<(), Error>> {
    start_with(port, counters, 1)
}

fn start_with(
    port: u16,
    counters: Arc<Counters>,
    max_concurrent: usize,
) -> JoinHandle<Result<(), Error>> {
    tokio::spawn(async move {
        let mut runner = SinkRunner::initialize(
            TestSink(counters),
            SinkRunnerConfig::new("sink-1".into(), LogLevel::Trace, connection(port))
                .with_max_concurrent(max_concurrent),
        )
        .await
        .unwrap();
        runner.run().await
    })
}

async fn expect_exit_ok(hub: &mut FakeHub) {
    let exit: Exit = hub.expect().await;
    assert_eq!(exit.code, exit_code::Enum::Ok as i32);
}

// The tests share the process-wide logger, so they run one at a time.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[tokio::test]
async fn sink_lifecycle_with_acks_and_nacks() {
    let _serial = SERIAL.lock().await;
    let (listener, port) = listen().await;
    let counters = Arc::new(Counters::default());
    let runner = start(port, counters.clone());

    let (mut hub, init) = FakeHub::handshake(&listener).await;
    assert_eq!(init.instance_id, "sink-1");
    assert_eq!(init.plugin_version, "1");
    assert_eq!(init.plugin_type, PluginType::Sink as i32);
    assert!(init.in_schema.is_some() && init.out_schema.is_none());
    assert_eq!(
        init.capabilities,
        [Capability::Acks as i32, Capability::Credits as i32]
    );
    assert_eq!(init.max_unacked, DEFAULT_MAX_UNACKED);

    hub.accept_plugin(&[Capability::Acks], 0).await;
    let metadata = EventMetadata {
        event_id: "e1".into(),
        trace_id: "t1".into(),
        hops: 1,
        ..Default::default()
    };
    hub.send_event(payload(vec![text("text", "ok")]), Some(metadata.clone()))
        .await;
    hub.send_event(payload(vec![text("text", "bad")]), None).await;
    hub.send_event(payload(vec![text("text", "later")]), None).await;

    let ack: Ack = hub.expect().await;
    assert_eq!((ack.up_to, ack.seqs), (0, vec![1]));
    let nack: Nack = hub.expect().await;
    assert_eq!((nack.seq, nack.retry, nack.reason.as_str()), (2, false, "bad event"));
    let nack: Nack = hub.expect().await;
    assert_eq!((nack.seq, nack.retry), (3, true));

    hub.shutdown("done").await;
    expect_exit_ok(&mut hub).await;
    hub.expect_closed().await;

    common::finished(runner).await.unwrap();
    let seen = counters.seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!((seen[0].seq, seen[0].attempt), (1, 1));
    assert_eq!(seen[0].metadata, Some(metadata));
    assert_eq!(counters.shut_down.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn one_at_a_time_by_default() {
    let _serial = SERIAL.lock().await;
    let (listener, port) = listen().await;
    let counters = Arc::new(Counters::default());
    let runner = start(port, counters.clone());

    let (mut hub, _) = FakeHub::handshake(&listener).await;
    hub.accept_plugin(&[Capability::Acks], 0).await;
    hub.send_event(payload(vec![text("text", "slow")]), None).await;
    hub.send_event(payload(vec![text("text", "ok")]), None).await;
    // The second event waits for the first.
    hub.expect_quiet(Duration::from_millis(200)).await;
    counters.gate.notify_one();
    let ack: Ack = hub.expect().await;
    assert_eq!(ack.seqs, vec![1]);
    let ack: Ack = hub.expect().await;
    assert_eq!(ack.seqs, vec![2]);
    hub.shutdown("").await;
    expect_exit_ok(&mut hub).await;
    common::finished(runner).await.unwrap();
}

#[tokio::test]
async fn concurrent_events_are_acked_as_they_finish_and_drained_on_shutdown() {
    let _serial = SERIAL.lock().await;
    let (listener, port) = listen().await;
    let counters = Arc::new(Counters::default());
    let runner = start_with(port, counters.clone(), 4);

    let (mut hub, _) = FakeHub::handshake(&listener).await;
    hub.accept_plugin(&[Capability::Acks], 0).await;
    hub.send_event(payload(vec![text("text", "slow")]), None).await;
    hub.send_event(payload(vec![text("text", "ok")]), None).await;
    // The second finishes first and is acked by itself (not cumulatively,
    // which would also cover the first).
    let ack: Ack = hub.expect().await;
    assert_eq!((ack.up_to, ack.seqs), (0, vec![2]));

    // Shutdown while the first is still running: it's finished and acked
    // before the Exit.
    hub.shutdown("").await;
    hub.expect_quiet(Duration::from_millis(200)).await;
    counters.gate.notify_one();
    let ack: Ack = hub.expect().await;
    assert_eq!((ack.up_to, ack.seqs), (0, vec![1]));
    expect_exit_ok(&mut hub).await;
    common::finished(runner).await.unwrap();
    assert_eq!(counters.seen.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn no_acks_unless_the_hub_enables_them() {
    let _serial = SERIAL.lock().await;
    let (listener, port) = listen().await;
    let counters = Arc::new(Counters::default());
    let runner = start(port, counters.clone());

    let (mut hub, _) = FakeHub::handshake(&listener).await;
    hub.accept_plugin(&[], 0).await;
    hub.send_event(payload(vec![text("text", "ok")]), None).await;
    hub.send_event(payload(vec![text("text", "bad")]), None).await;
    hub.shutdown("").await;
    // No ACK or NACK before the Exit.
    expect_exit_ok(&mut hub).await;
    common::finished(runner).await.unwrap();
    assert_eq!(counters.seen.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn answers_ping_and_honours_the_heartbeat_timeout() {
    let _serial = SERIAL.lock().await;
    let (listener, port) = listen().await;
    let counters = Arc::new(Counters::default());
    let runner = start(port, counters.clone());

    let (mut hub, _) = FakeHub::handshake(&listener).await;
    hub.accept_plugin(&[Capability::Acks], 100).await;
    hub.send(&Ping { nonce: 42 }).await;
    let pong: Pong = hub.expect().await;
    assert_eq!(pong.nonce, 42);
    // Then silence: after three intervals the plugin gives up on this
    // connection and reconnects.
    let started = tokio::time::Instant::now();
    hub.expect_closed().await;
    assert!(started.elapsed() >= Duration::from_millis(250), "{:?}", started.elapsed());

    let (mut hub, _) = FakeHub::handshake(&listener).await;
    hub.accept_plugin(&[], 0).await;
    hub.shutdown("").await;
    expect_exit_ok(&mut hub).await;
    common::finished(runner).await.unwrap();
}

#[tokio::test]
async fn reconnects_and_reinitializes_after_hub_restart() {
    let _serial = SERIAL.lock().await;
    let (listener, port) = listen().await;
    let counters = Arc::new(Counters::default());
    let runner = start(port, counters.clone());

    let (mut hub, first) = FakeHub::handshake(&listener).await;
    hub.accept_plugin(&[Capability::Acks], 0).await;
    hub.send_event(payload(vec![text("text", "ok")]), None).await;
    let ack: Ack = hub.expect().await;
    assert_eq!((ack.up_to, ack.seqs), (0, vec![1]));
    drop(hub);

    // The plugin reconnects by itself and re-sends the same Initialize; the
    // new connection's events start at seq 1 again.
    let (mut hub, again) = FakeHub::handshake(&listener).await;
    assert_eq!(again, first);
    hub.accept_plugin(&[Capability::Acks], 0).await;
    hub.send_event(payload(vec![text("text", "ok")]), None).await;
    let ack: Ack = hub.expect().await;
    assert_eq!((ack.up_to, ack.seqs), (0, vec![1]));
    hub.shutdown("").await;
    expect_exit_ok(&mut hub).await;

    common::finished(runner).await.unwrap();
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

    let (mut hub, _) = FakeHub::handshake(&listener).await;
    hub.shutdown("wrong token").await;
    expect_exit_ok(&mut hub).await;
    hub.expect_closed().await;

    assert!(matches!(common::finished(runner).await, Err(Error::Rejected)));
    assert_eq!(counters.shut_down.load(Ordering::SeqCst), 1);
    // It doesn't try again.
    let retry = tokio::time::timeout(Duration::from_millis(300), listener.accept());
    assert!(retry.await.is_err());
}

#[tokio::test]
async fn protocol_violations_by_the_hub_close_and_reconnect() {
    let _serial = SERIAL.lock().await;
    let (listener, port) = listen().await;
    let counters = Arc::new(Counters::default());
    let runner = start(port, counters.clone());

    // A code a sink never receives.
    let (mut hub, _) = FakeHub::handshake(&listener).await;
    hub.accept_plugin(&[], 0).await;
    hub.send_raw(&FakeHub::header(MsgCode::Log), &[]).await;
    hub.expect_closed().await;

    // Data before InitializeResponse.
    let (mut hub, _) = FakeHub::handshake(&listener).await;
    hub.send_event(payload(vec![]), None).await;
    hub.expect_closed().await;

    // A gap in the hub's sequence numbers.
    let (mut hub, _) = FakeHub::handshake(&listener).await;
    hub.accept_plugin(&[], 0).await;
    let mut header = FakeHub::header(MsgCode::Event);
    header.seq = 5;
    hub.send_raw(&header, &[]).await;
    hub.expect_closed().await;

    let (mut hub, _) = FakeHub::handshake(&listener).await;
    hub.accept_plugin(&[], 0).await;
    hub.shutdown("").await;
    expect_exit_ok(&mut hub).await;
    common::finished(runner).await.unwrap();
    assert!(counters.seen.lock().unwrap().is_empty());
}
