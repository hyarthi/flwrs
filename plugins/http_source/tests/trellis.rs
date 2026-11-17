//! The HTTP source checked with trellis: the protocol audit (requests as the
//! stimulus), requests becoming typed events (and the ones refused), HTTP
//! backpressure, Swagger UI, and the configurator (`*.configure.toml`).

use flwrs_trellis::{Audit, Outcome, StimulusCtx, Target, Tester, free_port};
use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex};

fn target() -> Target {
    Target::from_package(Path::new(env!("CARGO_MANIFEST_DIR")))
        .unwrap()
        .with_executable(env!("CARGO_BIN_EXE_flwrs-http-source"))
        .with_configurator_executable(env!("CARGO_BIN_EXE_flwrs-http-source-config"))
}

fn dir() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/trellis")
}

/// A request and the status it must get.
struct Req {
    method: &'static str,
    path: &'static str,
    content_type: Option<&'static str>,
    body: &'static str,
    status: u16,
}

const fn req(
    method: &'static str,
    path: &'static str,
    content_type: Option<&'static str>,
    body: &'static str,
    status: u16,
) -> Req {
    Req {
        method,
        path,
        content_type,
        body,
        status,
    }
}

type Sent = Arc<Mutex<Vec<tokio::task::JoinHandle<Result<(), String>>>>>;

/// One HTTP/1.1 request over a plain TCP connection: the status and the body.
async fn http(port: u16, r: &Req) -> Result<(u16, String), String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .map_err(|e| e.to_string())?;
    let mut head = format!(
        "{} {} HTTP/1.1\r\nhost: localhost\r\nconnection: close\r\ncontent-length: {}\r\n",
        r.method,
        r.path,
        r.body.len()
    );
    if let Some(ct) = r.content_type {
        head.push_str(&format!("content-type: {ct}\r\n"));
    }
    head.push_str("\r\n");
    stream
        .write_all(head.as_bytes())
        .await
        .map_err(|e| e.to_string())?;
    stream
        .write_all(r.body.as_bytes())
        .await
        .map_err(|e| e.to_string())?;
    let mut response = vec![];
    stream
        .read_to_end(&mut response)
        .await
        .map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(&response).into_owned();
    let status = text
        .split(' ')
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| format!("no status in {text:?}"))?;
    Ok((status, text))
}

/// A stimulus sending the requests in order, one per call. It doesn't wait
/// for the answers (with ack_mode confirmed an answer waits for the hub's
/// ACK, which trellis's fake hub sends only once the stimulus returned):
/// they're checked afterwards, from `sent`.
fn sender(port: u16, requests: Vec<Req>, sent: Sent) -> impl flwrs_trellis::Stimulus {
    let queue = Arc::new(Mutex::new(VecDeque::from(requests)));
    move |_: StimulusCtx| {
        let next = queue.lock().unwrap().pop_front();
        let sent = sent.clone();
        Box::pin(async move {
            let r = next.ok_or("no more requests")?;
            let task = tokio::spawn(async move {
                let (status, text) = http(port, &r)
                    .await
                    .map_err(|e| format!("{} {}: {e}", r.method, r.path))?;
                if status != r.status {
                    return Err(format!(
                        "{} {}: expected {}, got {status}: {text}",
                        r.method, r.path, r.status
                    ));
                }
                Ok(())
            });
            sent.lock().unwrap().push(task);
            // Give the request a moment to reach the plugin before the step's checks.
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            Ok(None)
        })
    }
}

async fn run(file: &str, port: u16, requests: Vec<Req>) {
    let sent: Sent = Arc::default();
    let mut tester = Tester::new(target())
        .var("http", port.to_string())
        .stimulus(sender(port, requests, sent.clone()));
    let result = tester.run_file(&dir().join(file)).await;
    let tasks: Vec<_> = std::mem::take(&mut *sent.lock().unwrap());
    let mut answers = vec![];
    for task in tasks {
        answers.push(task.await.unwrap());
    }
    let wrong: Vec<&String> = answers.iter().filter_map(|a| a.as_ref().err()).collect();
    assert!(
        result.outcome == Outcome::Pass && wrong.is_empty(),
        "{}\nrequests: {wrong:?}",
        flwrs_trellis::testing::report(std::slice::from_ref(&result))
    );
}

const JSON: Option<&str> = Some("application/json");

#[tokio::test(flavor = "multi_thread")]
async fn requests_become_typed_events() {
    let port = free_port();
    run(
        "requests.test.toml",
        port,
        vec![
            req(
                "POST",
                "/devices/d1?verbose=true",
                JSON,
                r#"{"temp": 21.5, "at": "2026-11-17T10:00:00Z", "tags": ["a", "b"]}"#,
                201,
            ),
            req(
                "POST",
                "/devices/d1",
                Some("application/octet-stream"),
                "raw",
                201,
            ),
            req(
                "POST",
                "/forms",
                Some("application/x-www-form-urlencoded"),
                "name=a+b&count=3",
                204,
            ),
            req("POST", "/devices/d1", JSON, r#"{"temp": 500}"#, 400),
            req("POST", "/devices/d1", Some("text/plain"), "hi", 415),
            req(
                "POST",
                "/devices/d1?verbose=maybe",
                JSON,
                r#"{"temp": 1}"#,
                400,
            ),
            req("POST", "/nowhere", JSON, "{}", 404),
            req("GET", "/devices/d2", None, "", 200),
            // Not events: Swagger UI's document, and readiness.
            req("GET", "/docs/openapi.json", None, "", 200),
            req("GET", "/readyz", None, "", 200),
        ],
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn backpressure_is_http_503() {
    let port = free_port();
    run(
        "backpressure.test.toml",
        port,
        vec![
            req("POST", "/devices/d1", JSON, r#"{"temp": 1}"#, 503),
            req("POST", "/devices/d1", JSON, r#"{"temp": 2}"#, 201),
        ],
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_configurator_derives_the_out_schema() {
    let mut tester = Tester::new(target());
    for file in [
        "devices.configure.toml",
        "yaml.configure.toml",
        "oneof.configure.toml",
    ] {
        let result = tester.run_file(&dir().join(file)).await;
        assert_eq!(
            result.outcome,
            Outcome::Pass,
            "{}",
            flwrs_trellis::testing::report(std::slice::from_ref(&result))
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn follows_the_plugin_protocol() {
    let port = free_port();
    let text = std::fs::read_to_string(dir().join("devices.json"))
        .unwrap()
        .replace("${http}", &port.to_string());
    let target = target()
        .with_launch_args(serde_json::from_str(&text).unwrap())
        .unwrap();
    // Every data-events stimulus: one request (the audit may fire several).
    let requests = (0..32)
        .map(|_| req("GET", "/devices/x", None, "", 200))
        .collect();
    let results = Audit::new(target)
        .stimulus(sender(port, requests, Sent::default()))
        .run()
        .await;
    assert!(
        results.iter().all(|r| r.outcome == Outcome::Pass),
        "{}",
        flwrs_trellis::report(&results)
    );
}
