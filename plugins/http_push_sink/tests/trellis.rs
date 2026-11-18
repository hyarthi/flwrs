//! The HTTP push sink checked with trellis: the protocol audit, `delivered`
//! acks (held for subscribers, refused after the timeout), and replay, long
//! polling, gaps, SSE resumption, binary data, auth and CORS. HTTP goes over
//! plain TCP; subscribers run as tasks the stimulus starts.

use flwrs_trellis::{Audit, Outcome, StimulusCtx, Target, Tester, free_port};
use serde_json::Value;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn target() -> Target {
    Target::from_package(Path::new(env!("CARGO_MANIFEST_DIR")))
        .unwrap()
        .with_executable(env!("CARGO_BIN_EXE_flwrs-http-push-sink"))
}

/// What a stimulus call returns.
type Fired = std::pin::Pin<
    Box<
        dyn std::future::Future<Output = Result<Option<flwrs_trellis::Correlation>, String>> + Send,
    >,
>;

fn dir() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/trellis")
}

fn head(method: &str, path: &str, headers: &[(&str, &str)]) -> String {
    let mut h = format!("{method} {path} HTTP/1.1\r\nhost: localhost\r\n");
    for (k, v) in headers {
        h.push_str(&format!("{k}: {v}\r\n"));
    }
    h + "\r\n"
}

/// A request answered in full (`connection: close`): status, headers, body.
async fn request(
    port: u16,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
) -> Result<(u16, String, String), String> {
    let mut all = headers.to_vec();
    all.push(("connection", "close"));
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .map_err(|e| e.to_string())?;
    stream
        .write_all(head(method, path, &all).as_bytes())
        .await
        .map_err(|e| e.to_string())?;
    let mut response = vec![];
    stream
        .read_to_end(&mut response)
        .await
        .map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(&response).into_owned();
    let (head, body) = text
        .split_once("\r\n\r\n")
        .ok_or_else(|| format!("{path}: no answer: {text:?}"))?;
    let status = head
        .split(' ')
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or("no status")?;
    // Chunked bodies (axum's JSON isn't; enough for these answers).
    Ok((status, head.to_ascii_lowercase(), body.to_string()))
}

/// An SSE subscriber: everything it reads goes into `seen`.
fn subscribe(
    port: u16,
    path: &'static str,
    headers: Vec<(String, String)>,
    delay: Duration,
    seen: Arc<Mutex<String>>,
) {
    tokio::spawn(async move {
        tokio::time::sleep(delay).await;
        let Ok(mut stream) = tokio::net::TcpStream::connect(("127.0.0.1", port)).await else {
            return;
        };
        let headers: Vec<(&str, &str)> = headers
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        if stream
            .write_all(head("GET", path, &headers).as_bytes())
            .await
            .is_err()
        {
            return;
        }
        let mut buf = [0u8; 4096];
        while let Ok(n) = stream.read(&mut buf).await {
            if n == 0 {
                break;
            }
            seen.lock()
                .unwrap()
                .push_str(&String::from_utf8_lossy(&buf[..n]));
        }
    });
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
async fn delivered_events_wait_for_a_subscriber() {
    let port = free_port();
    let news = Arc::new(Mutex::new(String::new()));
    let later = Arc::new(Mutex::new(String::new()));
    let calls = Arc::new(AtomicUsize::new(0));
    let (n, l) = (news.clone(), later.clone());
    let stimulus = move |_: StimulusCtx| {
        let call = calls.fetch_add(1, Ordering::SeqCst);
        let (n, l) = (n.clone(), l.clone());
        Box::pin(async move {
            match call {
                0 => {
                    subscribe(port, "/subscribe/news", vec![], Duration::ZERO, n);
                    tokio::time::sleep(Duration::from_millis(300)).await;
                }
                _ => subscribe(
                    port,
                    "/subscribe/later",
                    vec![],
                    Duration::from_millis(300),
                    l,
                ),
            }
            Ok(None)
        }) as Fired
    };
    run("delivered.test.toml", port, stimulus).await;
    let news = news.lock().unwrap().clone();
    assert!(news.contains("content-type: text/event-stream"), "{news}");
    assert!(news.contains("retry: 3000"), "{news}");
    assert!(news.contains("event: note\ndata: hello\n\n"), "{news}");
    assert!(
        news.contains(": keep-alive"),
        "heartbeats every second: {news}"
    );
    let later = later.lock().unwrap().clone();
    assert!(later.contains("data: held"), "{later}");
    // The refused event is out of the ring: nobody gets it later.
    assert!(!news.contains("lost") && !later.contains("lost"));
}

async fn buffered_checks(port: u16) -> Result<(), String> {
    let auth = ("authorization", "Bearer s3cret-token");
    let (status, _, _) = request(port, "GET", "/poll/feed?wait_s=1", &[]).await?;
    if status != 401 {
        return Err(format!("without a token: {status}"));
    }
    // A cursor of another run: a gap (count unknown), then the ring (the last 3).
    let (status, headers, body) = request(
        port,
        "GET",
        "/poll/feed?cursor=old-1&wait_s=1",
        &[auth, ("origin", "https://app.example.com")],
    )
    .await?;
    let answer: Value = serde_json::from_str(&body).map_err(|e| format!("{e}: {body}"))?;
    let data: Vec<&str> = answer["events"]
        .as_array()
        .ok_or("no events")?
        .iter()
        .filter_map(|e| e["data"].as_str())
        .collect();
    if status != 200
        || data != ["two", "three", "four"]
        || !answer["missed"].is_null()
        || answer.get("missed").is_none()
    {
        return Err(format!("stale cursor: {status} {body}"));
    }
    if !headers.contains("access-control-allow-origin: https://app.example.com") {
        return Err(format!("no CORS header: {headers}"));
    }
    if answer["events"][0]["event"] != "note" {
        return Err(format!("event type: {body}"));
    }
    // From the first one's id: the other two, no gap.
    let first = answer["events"][0]["id"]
        .as_str()
        .ok_or("no id")?
        .to_string();
    let (_, _, body) = request(
        port,
        "GET",
        &format!("/poll/feed?cursor={first}&wait_s=1"),
        &[auth],
    )
    .await?;
    let answer: Value = serde_json::from_str(&body).map_err(|e| e.to_string())?;
    if answer["events"].as_array().map(Vec::len) != Some(2) || answer.get("missed").is_some() {
        return Err(format!("resume: {body}"));
    }
    // SSE resuming by Last-Event-ID: the two after it.
    let seen = Arc::new(Mutex::new(String::new()));
    let headers = vec![
        ("authorization".into(), auth.1.into()),
        ("last-event-id".into(), first.clone()),
    ];
    subscribe(
        port,
        "/subscribe/feed",
        headers,
        Duration::ZERO,
        seen.clone(),
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    let text = seen.lock().unwrap().clone();
    if !text.contains("data: three") || !text.contains("data: four") || text.contains("data: two") {
        return Err(format!("SSE resume: {text}"));
    }
    // An id the ring dropped (seq 1): a counted gap.
    let dropped = first.replace("-2", "-0");
    let (_, _, body) = request(
        port,
        "GET",
        &format!("/poll/feed?cursor={dropped}&wait_s=1"),
        &[auth],
    )
    .await?;
    let answer: Value = serde_json::from_str(&body).map_err(|e| e.to_string())?;
    if answer["missed"] != 1 {
        return Err(format!("counted gap: {body}"));
    }
    // Caught up: waits, then answers nothing with the same cursor.
    let next = answer["next_cursor"]
        .as_str()
        .ok_or("no cursor")?
        .to_string();
    let (_, _, body) = request(
        port,
        "GET",
        &format!("/poll/feed?cursor={next}&wait_s=1"),
        &[auth],
    )
    .await?;
    let answer: Value = serde_json::from_str(&body).map_err(|e| e.to_string())?;
    if answer["events"].as_array().map(Vec::len) != Some(0)
        || answer["next_cursor"] != next.as_str()
    {
        return Err(format!("caught up: {body}"));
    }
    // Binary data: base64 in the envelope.
    let (_, _, body) = request(port, "GET", "/poll/bin?cursor=old-0&wait_s=1", &[auth]).await?;
    let answer: Value = serde_json::from_str(&body).map_err(|e| e.to_string())?;
    let e = &answer["events"][0];
    if e["data"] != "/wAB"
        || e["encoding"] != "base64"
        || e["content_type"] != "application/octet-stream"
    {
        return Err(format!("binary: {body}"));
    }
    // Not an allowed channel: 404.
    let (status, _, _) = request(port, "GET", "/poll/other?wait_s=1", &[auth]).await?;
    if status != 404 {
        return Err(format!("unknown channel: {status}"));
    }
    // Preflights: no token needed; an allowed origin gets 204, another 403.
    let pre = |origin: &'static str| {
        [
            ("origin", origin),
            ("access-control-request-method", "GET"),
            ("access-control-request-headers", "authorization"),
        ]
    };
    let (status, headers, _) = request(
        port,
        "OPTIONS",
        "/subscribe/feed",
        &pre("https://app.example.com"),
    )
    .await?;
    if status != 204 || !headers.contains("access-control-allow-headers: authorization") {
        return Err(format!("preflight: {status} {headers}"));
    }
    let (status, _, _) = request(
        port,
        "OPTIONS",
        "/subscribe/feed",
        &pre("https://evil.example.com"),
    )
    .await?;
    if status != 403 {
        return Err(format!("preflight from another origin: {status}"));
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn replay_long_polling_and_cors() {
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
