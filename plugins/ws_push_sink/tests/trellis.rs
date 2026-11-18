//! The WebSocket push sink checked with trellis: the protocol audit,
//! `delivered` acks (held for clients, refused after the timeout), and
//! envelope frames with replay, gaps, binary data, auth and refusals.
//! Clients (tokio-tungstenite) run as tasks the stimulus starts.

use flwrs_trellis::{Audit, Outcome, StimulusCtx, Target, Tester, free_port};
use futures_util::StreamExt;
use serde_json::Value;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

type Fired = std::pin::Pin<
    Box<
        dyn std::future::Future<Output = Result<Option<flwrs_trellis::Correlation>, String>> + Send,
    >,
>;
type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

fn target() -> Target {
    Target::from_package(Path::new(env!("CARGO_MANIFEST_DIR")))
        .unwrap()
        .with_executable(env!("CARGO_BIN_EXE_flwrs-ws-push-sink"))
}

fn dir() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/trellis")
}

const TOKEN: (&str, &str) = ("authorization", "Bearer s3cret-token");

fn header_name(k: &str) -> tokio_tungstenite::tungstenite::http::HeaderName {
    tokio_tungstenite::tungstenite::http::HeaderName::from_bytes(k.as_bytes()).unwrap()
}

/// Connects; `Err(status)` when the upgrade is refused.
async fn connect(port: u16, path: &str, headers: &[(&str, &str)]) -> Result<Ws, u16> {
    let mut request = format!("ws://127.0.0.1:{port}{path}")
        .into_client_request()
        .unwrap();
    for (k, v) in headers {
        request
            .headers_mut()
            .insert(header_name(k), v.parse().unwrap());
    }
    match tokio_tungstenite::connect_async(request).await {
        Ok((ws, _)) => Ok(ws),
        Err(tokio_tungstenite::tungstenite::Error::Http(r)) => Err(r.status().as_u16()),
        Err(e) => panic!("{path}: {e}"),
    }
}

/// A client that keeps every message it gets in `seen`.
fn listen(port: u16, path: &'static str, delay: Duration, seen: Arc<Mutex<Vec<Message>>>) {
    tokio::spawn(async move {
        tokio::time::sleep(delay).await;
        let Ok(mut ws) = connect(port, path, &[]).await else {
            return;
        };
        while let Some(Ok(m)) = ws.next().await {
            seen.lock().unwrap().push(m);
        }
    });
}

/// The messages that arrive within `wait`.
async fn messages(ws: &mut Ws, wait: Duration) -> Vec<Message> {
    let mut out = vec![];
    while let Ok(Some(Ok(m))) = tokio::time::timeout(wait, ws.next()).await {
        out.push(m);
    }
    out
}

fn json(m: &Message) -> Value {
    match m {
        Message::Text(t) => serde_json::from_str(t.as_str()).unwrap_or(Value::Null),
        _ => Value::Null,
    }
}

async fn run(file: &str, port: u16, stimulus: impl flwrs_trellis::Stimulus + 'static) {
    let mut tester = Tester::new(target())
        .var("http", port.to_string())
        .stimulus(stimulus);
    let result = tester.run_file(&dir().join(file)).await;
    assert_eq!(
        result.outcome,
        Outcome::Pass,
        "{}",
        flwrs_trellis::testing::report(std::slice::from_ref(&result))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn delivered_events_wait_for_a_client() {
    let port = free_port();
    let news = Arc::new(Mutex::new(vec![]));
    let later = Arc::new(Mutex::new(vec![]));
    let calls = Arc::new(AtomicUsize::new(0));
    let (n, l) = (news.clone(), later.clone());
    let stimulus = move |_: StimulusCtx| {
        let call = calls.fetch_add(1, Ordering::SeqCst);
        let (n, l) = (n.clone(), l.clone());
        Box::pin(async move {
            match call {
                0 => {
                    listen(port, "/ws/news", Duration::ZERO, n);
                    tokio::time::sleep(Duration::from_millis(300)).await;
                }
                _ => listen(port, "/ws/later", Duration::from_millis(300), l),
            }
            Ok(None)
        }) as Fired
    };
    run("delivered.test.toml", port, stimulus).await;
    assert_eq!(*news.lock().unwrap(), [Message::text("hello")]);
    // The refused event is out of the ring: the late client gets only its own.
    assert_eq!(*later.lock().unwrap(), [Message::text("held")]);
}

async fn buffered_checks(port: u16) -> Result<(), String> {
    let short = Duration::from_millis(300);
    if connect(port, "/ws/feed", &[]).await.err() != Some(401) {
        return Err("without a token: not 401".into());
    }
    // An id of another run: a gap (count unknown), then the ring (the last 3).
    let mut ws = connect(port, "/ws/feed?after=old-1", &[TOKEN])
        .await
        .map_err(|s| format!("stale id: {s}"))?;
    let got: Vec<Value> = messages(&mut ws, short).await.iter().map(json).collect();
    let data: Vec<&str> = got.iter().filter_map(|v| v["data"].as_str()).collect();
    if got
        .first()
        .map(|g| (g["type"].as_str(), g["missed"].is_null()))
        != Some((Some("gap"), true))
        || data != ["two", "three", "four"]
    {
        return Err(format!("stale id: {got:?}"));
    }
    if got[1]["event"] != "note" {
        return Err(format!("envelope: {got:?}"));
    }
    // From the first one's id: the other two, no gap.
    let first = got[1]["id"].as_str().ok_or("no id")?.to_string();
    let mut ws = connect(port, &format!("/ws/feed?after={first}"), &[TOKEN])
        .await
        .map_err(|s| format!("resume: {s}"))?;
    let got: Vec<Value> = messages(&mut ws, short).await.iter().map(json).collect();
    if got.len() != 2 || got.iter().any(|g| g["type"] == "gap") {
        return Err(format!("resume: {got:?}"));
    }
    // An id the ring dropped (seq 1): a counted gap.
    let dropped = first.replace("-2", "-0");
    let mut ws = connect(port, &format!("/ws/feed?after={dropped}"), &[TOKEN])
        .await
        .map_err(|s| format!("gap: {s}"))?;
    let got: Vec<Value> = messages(&mut ws, short).await.iter().map(json).collect();
    if got.first().map(|g| g["missed"].clone()) != Some(1.into()) {
        return Err(format!("counted gap: {got:?}"));
    }
    // Binary data: base64 in the envelope.
    let mut ws = connect(port, "/ws/bin?after=old-0", &[TOKEN])
        .await
        .map_err(|s| format!("binary: {s}"))?;
    let got: Vec<Value> = messages(&mut ws, short).await.iter().map(json).collect();
    let e = got.get(1).ok_or(format!("binary: {got:?}"))?;
    if e["data"] != "/wAB"
        || e["encoding"] != "base64"
        || e["content_type"] != "application/octet-stream"
    {
        return Err(format!("binary: {got:?}"));
    }
    // Refusals: a channel not allowed, an Origin not allowed (an allowed one connects).
    if connect(port, "/ws/other", &[TOKEN]).await.err() != Some(404) {
        return Err("unknown channel: not 404".into());
    }
    if connect(
        port,
        "/ws/feed",
        &[TOKEN, ("origin", "https://evil.example.com")],
    )
    .await
    .err()
        != Some(403)
    {
        return Err("another origin: not 403".into());
    }
    connect(
        port,
        "/ws/feed",
        &[TOKEN, ("origin", "https://app.example.com")],
    )
    .await
    .map_err(|s| format!("allowed origin: {s}"))?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn envelopes_replay_and_refusals() {
    let port = free_port();
    let stimulus = move |_: StimulusCtx| {
        Box::pin(async move {
            buffered_checks(port).await?;
            Ok(None)
        }) as Fired
    };
    run("buffered.test.toml", port, stimulus).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn follows_the_plugin_protocol() {
    let target = target()
        .with_launch_args(
            serde_json::from_str(&std::fs::read_to_string(dir().join("audit.json")).unwrap())
                .unwrap(),
        )
        .unwrap();
    let results = Audit::new(target).run().await;
    assert!(
        results.iter().all(|r| r.outcome == Outcome::Pass),
        "{}",
        flwrs_trellis::report(&results)
    );
}
