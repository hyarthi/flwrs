//! The WebSocket sink checked with trellis against an in-test server
//! (tokio-tungstenite): the protocol audit, ack write (checked by what the
//! server received, handshake included), ack reply, and a refused handshake.

use flwrs_trellis::{Audit, Outcome, Target, Tester};
use futures_util::{SinkExt, StreamExt};
use std::path::Path;
use std::sync::{Arc, Mutex};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http::{HeaderValue, StatusCode};

fn target() -> Target {
    Target::from_package(Path::new(env!("CARGO_MANIFEST_DIR")))
        .unwrap()
        .with_executable(env!("CARGO_BIN_EXE_flwrs-ws-sink"))
}

#[derive(Debug, Clone, Default)]
struct Received {
    path: String,
    authorization: Option<String>,
    origin: Option<String>,
    protocol: Option<String>,
    messages: Vec<Message>,
}

type Log = Arc<Mutex<Vec<Received>>>;

/// The test server; returns its port and what it received, per connection.
// The handshake callback's error type is tungstenite's.
#[allow(clippy::result_large_err)]
async fn server() -> (u16, Log) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let log: Log = Arc::default();
    let all = log.clone();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let log = all.clone();
            tokio::spawn(async move {
                let mut seen = Received::default();
                let callback = |request: &Request,
                                mut response: Response|
                 -> Result<Response, ErrorResponse> {
                    let header = |n: &str| {
                        request
                            .headers()
                            .get(n)
                            .and_then(|v| v.to_str().ok())
                            .map(str::to_string)
                    };
                    seen.path = request.uri().path().to_string();
                    seen.authorization = header("authorization");
                    seen.origin = header("origin");
                    seen.protocol = header("sec-websocket-protocol");
                    if seen.path == "/reject" {
                        let mut refused = ErrorResponse::new(None);
                        *refused.status_mut() = StatusCode::UNAUTHORIZED;
                        return Err(refused);
                    }
                    if let Some(p) = seen.protocol.as_deref().and_then(|p| p.split(',').next()) {
                        response.headers_mut().insert(
                            "sec-websocket-protocol",
                            HeaderValue::from_str(p.trim()).unwrap(),
                        );
                    }
                    Ok(response)
                };
                let Ok(mut ws) = tokio_tungstenite::accept_hdr_async(stream, callback).await else {
                    return;
                };
                let index = {
                    let mut log = log.lock().unwrap();
                    log.push(seen.clone());
                    log.len() - 1
                };
                while let Some(Ok(message)) = ws.next().await {
                    if !(message.is_text() || message.is_binary()) {
                        continue;
                    }
                    log.lock().unwrap()[index].messages.push(message.clone());
                    if seen.path == "/drop" {
                        let _ = ws.close(None).await;
                        return;
                    }
                    if seen.path == "/echo" {
                        let json: serde_json::Value =
                            serde_json::from_slice(&message.into_data()).unwrap_or_default();
                        let id = json["id"].clone();
                        let answer = if json.get("fail").is_some() {
                            serde_json::json!({"id": id, "error": "failed"})
                        } else {
                            serde_json::json!({"id": id, "ok": true})
                        };
                        // Something unrelated first: the sink must ignore it.
                        let _ = ws.send(Message::text(r#"{"event": "tick"}"#)).await;
                        let _ = ws.send(Message::text(answer.to_string())).await;
                    }
                }
            });
        }
    });
    (port, log)
}

async fn run(file: &str, port: u16) {
    let mut tester = Tester::new(target()).var("ws", port.to_string());
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/trellis")
        .join(file);
    let result = tester.run_file(&path).await;
    assert_eq!(
        result.outcome,
        Outcome::Pass,
        "{}",
        flwrs_trellis::testing::report(std::slice::from_ref(&result))
    );
}

#[tokio::test]
async fn follows_the_plugin_protocol() {
    let text = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/trellis/audit.json"),
    )
    .unwrap();
    let target = target()
        .with_launch_args(serde_json::from_str(&text).unwrap())
        .unwrap();
    let results = Audit::new(target).run().await;
    assert!(
        results.iter().all(|r| r.outcome == Outcome::Pass),
        "{}",
        flwrs_trellis::report(&results)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn acknowledges_once_written() {
    let (port, log) = server().await;
    run("write.test.toml", port).await;
    let log = log.lock().unwrap().clone();
    let main = log.iter().find(|r| r.path == "/record").unwrap();
    assert_eq!(main.messages, [Message::text(r#"{"n": 1}"#)]);
    assert_eq!(main.authorization.as_deref(), Some("Bearer secret"));
    assert_eq!(main.origin.as_deref(), Some("https://flwrs.test"));
    assert_eq!(main.protocol.as_deref(), Some("v1"));
    let other = log.iter().find(|r| r.path == "/record/b").unwrap();
    assert_eq!(other.messages, [Message::binary(b"raw bytes".to_vec())]);
    assert!(!log.iter().any(|r| r.path == "/recorder"));
}

#[tokio::test(flavor = "multi_thread")]
async fn acknowledges_once_the_server_replied() {
    let (port, log) = server().await;
    run("reply.test.toml", port).await;
    let log = log.lock().unwrap().clone();
    let echo = log.iter().find(|r| r.path == "/echo").unwrap();
    let sent: Vec<serde_json::Value> = echo
        .messages
        .iter()
        .map(|m| serde_json::from_slice(&m.clone().into_data()).unwrap())
        .collect();
    // The generated id went into the JSON; a given correlation_id was used as is.
    assert_eq!(sent[0]["n"], 2);
    assert!(sent[0]["id"].as_str().is_some_and(|id| !id.is_empty()));
    assert_eq!(sent.last().unwrap()["id"], "c-77");
}

#[tokio::test(flavor = "multi_thread")]
async fn nacks_while_the_handshake_is_refused() {
    let (port, _) = server().await;
    run("reject.test.toml", port).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn redials_after_the_server_closes() {
    let (port, log) = server().await;
    run("drop.test.toml", port).await;
    let connections = log
        .lock()
        .unwrap()
        .iter()
        .filter(|r| r.path == "/drop")
        .count();
    assert_eq!(connections, 2);
}
