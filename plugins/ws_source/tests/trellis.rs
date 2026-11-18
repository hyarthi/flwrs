//! The WebSocket source checked with trellis: the protocol audit (messages
//! as the stimulus), messages becoming typed events (and the ones dropped or
//! refused), closing on invalid messages, backpressure (1013), and the
//! configurator (`*.configure.toml`).

use flwrs_trellis::{Audit, Outcome, StimulusCtx, Target, Tester, free_port};
use futures_util::{SinkExt, StreamExt};
use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;

fn target() -> Target {
    Target::from_package(Path::new(env!("CARGO_MANIFEST_DIR")))
        .unwrap()
        .with_executable(env!("CARGO_BIN_EXE_flwrs-ws-source"))
        .with_configurator_executable(env!("CARGO_BIN_EXE_flwrs-ws-source-config"))
}

fn dir() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/trellis")
}

/// What happens to a message's connection.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Then {
    /// It stays open (the message taken or dropped).
    Open,
    /// The server closes it with this code.
    Closed(u16),
    /// The upgrade is refused with this status.
    Refused(u16),
}

/// A message sent on a new connection to `path`.
struct Msg {
    path: &'static str,
    message: Message,
    then: Then,
}

fn text(path: &'static str, t: &'static str, then: Then) -> Msg {
    Msg {
        path,
        message: Message::text(t),
        then,
    }
}

type Sent = Arc<Mutex<Vec<tokio::task::JoinHandle<Result<(), String>>>>>;

async fn send(port: u16, m: Msg) -> Result<(), String> {
    let url = format!("ws://127.0.0.1:{port}{}", m.path);
    let mut ws = match tokio_tungstenite::connect_async(&url).await {
        Ok((ws, _)) => ws,
        Err(tokio_tungstenite::tungstenite::Error::Http(r))
            if m.then == Then::Refused(r.status().as_u16()) =>
        {
            return Ok(());
        }
        Err(e) => return Err(format!("{url}: {e}")),
    };
    if let Then::Refused(status) = m.then {
        return Err(format!("{url}: connected, expected {status}"));
    }
    ws.send(m.message)
        .await
        .map_err(|e| format!("{url}: {e}"))?;
    // Whether the server closes the connection within a moment.
    let closed = tokio::time::timeout(Duration::from_millis(1000), async {
        loop {
            match ws.next().await {
                Some(Ok(Message::Close(frame))) => {
                    return Some(frame.map_or(1005, |f| u16::from(f.code)));
                }
                Some(Ok(_)) => continue,
                _ => return Some(1006),
            }
        }
    })
    .await
    .ok()
    .flatten();
    match (m.then, closed) {
        (Then::Open, None) => Ok(()),
        (Then::Closed(want), Some(got)) if want == got => Ok(()),
        (want, got) => Err(format!(
            "{url}: expected {want:?}, the server closed with {got:?}"
        )),
    }
}

/// A stimulus sending the messages in order, one per call, each on its own
/// connection. Outcomes are checked afterwards, from `sent`.
fn sender(port: u16, messages: Vec<Msg>, sent: Sent) -> impl flwrs_trellis::Stimulus {
    let queue = Arc::new(Mutex::new(VecDeque::from(messages)));
    move |_: StimulusCtx| {
        let next = queue.lock().unwrap().pop_front();
        let sent = sent.clone();
        Box::pin(async move {
            let m = next.ok_or("no more messages")?;
            sent.lock().unwrap().push(tokio::spawn(send(port, m)));
            // Give the message a moment to reach the plugin before the step's checks.
            tokio::time::sleep(Duration::from_millis(150)).await;
            Ok(None)
        })
    }
}

async fn run(file: &str, port: u16, messages: Vec<Msg>) {
    let sent: Sent = Arc::default();
    let mut tester = Tester::new(target())
        .var("ws", port.to_string())
        .stimulus(sender(port, messages, sent.clone()));
    let result = tester.run_file(&dir().join(file)).await;
    let tasks: Vec<_> = std::mem::take(&mut *sent.lock().unwrap());
    let mut wrong = vec![];
    for task in tasks {
        if let Err(e) = task.await.unwrap() {
            wrong.push(e);
        }
    }
    assert!(
        result.outcome == Outcome::Pass && wrong.is_empty(),
        "{}\nmessages: {wrong:?}",
        flwrs_trellis::testing::report(std::slice::from_ref(&result))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn messages_become_typed_events() {
    let port = free_port();
    run(
        "messages.test.toml",
        port,
        vec![
            text(
                "/devices/d1",
                r#"{"temp": 21.5, "at": "2026-11-18T10:00:00Z"}"#,
                Then::Open,
            ),
            Msg {
                path: "/devices/d2",
                message: Message::binary(br#"{"online": true}"#.to_vec()),
                then: Then::Open,
            },
            text("/devices/d1", r#"{"temp": 500}"#, Then::Open),
            text("/devices/d1", "not json", Then::Open),
            text("/nowhere", "{}", Then::Refused(404)),
        ],
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn invalid_messages_can_close_the_connection() {
    let port = free_port();
    run(
        "close.test.toml",
        port,
        vec![
            text("/devices/d1", r#"{"temp": "warm"}"#, Then::Closed(1008)),
            text("/devices/d1", r#"{"temp": 1}"#, Then::Open),
        ],
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_slow_workflow_closes_with_1013() {
    let port = free_port();
    run(
        "backpressure.test.toml",
        port,
        vec![
            text("/devices/d1", r#"{"temp": 1}"#, Then::Closed(1013)),
            text("/devices/d1", r#"{"temp": 2}"#, Then::Open),
        ],
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_configurator_derives_the_out_schema() {
    let mut tester = Tester::new(target()).update(std::env::var("FLWRS_UPDATE").is_ok());
    let result = tester
        .run_file(&dir().join("readings.configure.toml"))
        .await;
    assert_eq!(
        result.outcome,
        Outcome::Pass,
        "{}",
        flwrs_trellis::testing::report(std::slice::from_ref(&result))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn follows_the_plugin_protocol() {
    let port = free_port();
    let text = std::fs::read_to_string(dir().join("audit.json"))
        .unwrap()
        .replace("${ws}", &port.to_string());
    let target = target()
        .with_launch_args(serde_json::from_str(&text).unwrap())
        .unwrap();
    // Every data-events stimulus: one message on /ws (no document: bytes).
    let messages = (0..32).map(|_| text_msg()).collect();
    let results = Audit::new(target)
        .stimulus(sender(port, messages, Sent::default()))
        .run()
        .await;
    assert!(
        results.iter().all(|r| r.outcome == Outcome::Pass),
        "{}",
        flwrs_trellis::report(&results)
    );
}

fn text_msg() -> Msg {
    text("/ws", "hello", Then::Open)
}
