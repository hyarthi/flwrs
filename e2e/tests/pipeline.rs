//! The smoke test across the plugins: HTTP source → MQTT sink → (broker) →
//! MQTT source → HTTP push sink, in a real server, with an SSE client.
//!
//! The server and the four packages are built from this workspace (a debug
//! build, like `cargo test`'s), the plugins installed through the REST API,
//! the scene built and enabled through it; the broker (rumqttd) runs in this
//! process. A request to the HTTP source must come out of the push sink's
//! SSE stream, typed and mapped on the way.

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf()
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn wait_for_port(port: u16, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(Instant::now() < deadline, "{what} didn't listen on {port}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// An embedded MQTT broker (MQTT 5 listener); its port.
fn start_broker() -> u16 {
    let port = free_port();
    let config: rumqttd::Config = serde_json::from_value(json!({
        "id": 0,
        "router": { "max_connections": 100, "max_outgoing_packet_count": 200,
                    "max_segment_size": 1048576, "max_segment_count": 10 },
        "v5": { "1": { "name": "v5", "listen": format!("127.0.0.1:{port}"), "next_connection_delay_ms": 1,
                       "connections": { "connection_timeout_ms": 60000, "max_payload_size": 1048576,
                                        "max_inflight_count": 100, "dynamic_filters": true } } },
    }))
    .unwrap();
    std::thread::spawn(move || rumqttd::Broker::new(config).start().unwrap());
    wait_for_port(port, "the broker");
    port
}

/// The server binary, built like the rest of this test run.
fn server_binary() -> PathBuf {
    let status = Command::new(env!("CARGO"))
        .args(["build", "-q", "-p", "flwrs-app"])
        .current_dir(root())
        .status()
        .unwrap();
    assert!(status.success(), "building flwrs-app");
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| root().join("target"));
    target
        .join("debug")
        .join(format!("flwrs-app{}", std::env::consts::EXE_SUFFIX))
}

/// A plugin package (ZIP), built with trellis into `out`.
fn package(plugin: &str, out: &Path) -> PathBuf {
    let options = flwrs_trellis::package::PackageOptions {
        dir: root().join("plugins").join(plugin),
        target: None,
        flavor: None,
        release: false,
        out: Some(out.to_path_buf()),
        config: None,
    };
    flwrs_trellis::package::package(&options)
        .unwrap_or_else(|e| panic!("packaging {plugin}: {e}"))
        .path
}

/// The server, stopped when dropped.
struct Server {
    child: Child,
    base: String,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn start_server(dir: &Path) -> Server {
    let port = free_port();
    let config = format!(
        r#"
[http]
host = "127.0.0.1"
port = {port}

[logging]
format = "PLAIN"
level = "INFO"
[[logging.sinks]]
sink_type = "CONSOLE"

[db.main]
filename = "main.db"

[plugins]
dir = "plugins"
allow_upload = true

[hub]
host = "127.0.0.1"
port = 0

[director]
# Every plugin single-threaded (`--threads 1`): the smallest set-up works too.
plugin_threads = 1
spool_dir = "scenes"
nodes_dir = "nodes"
files_dir = "files"
"#
    );
    std::fs::write(dir.join("config.toml"), config).unwrap();
    let child = Command::new(server_binary())
        .args(["--config", "config.toml"])
        .current_dir(dir)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let server = Server {
        child,
        base: format!("http://127.0.0.1:{port}/api"),
    };
    wait_for_port(port, "the server");
    server
}

/// A REST call: the status and the JSON answer.
async fn call(
    client: &reqwest::Client,
    method: reqwest::Method,
    url: &str,
    body: Option<Value>,
) -> (u16, Value) {
    let mut request = client.request(method, url);
    if let Some(body) = body {
        request = request
            .header("content-type", "application/json")
            .body(body.to_string());
    }
    let response = request
        .send()
        .await
        .unwrap_or_else(|e| panic!("{url}: {e}"));
    let status = response.status().as_u16();
    let text = response.text().await.unwrap();
    (
        status,
        serde_json::from_str(&text).unwrap_or(Value::String(text)),
    )
}

/// Installs a package (multipart upload, built by hand).
async fn install(client: &reqwest::Client, base: &str, zip: &Path) {
    let boundary = "flwrs-e2e-boundary";
    let mut body = format!(
        "--{boundary}\r\ncontent-disposition: form-data; name=\"file\"; filename=\"{}\"\r\ncontent-type: application/zip\r\n\r\n",
        zip.file_name().unwrap().to_string_lossy()
    )
    .into_bytes();
    body.extend(std::fs::read(zip).unwrap());
    body.extend(format!("\r\n--{boundary}--\r\n").into_bytes());
    let response = client
        .post(format!("{base}/plugins"))
        .header(
            "content-type",
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(body)
        .send()
        .await
        .unwrap();
    let status = response.status().as_u16();
    assert!(
        status == 201 || status == 200,
        "installing {}: {status} {}",
        zip.display(),
        response.text().await.unwrap()
    );
}

const OPENAPI: &str = r#"
openapi: 3.1.0
info: { title: Readings, version: "1" }
paths:
  /readings:
    post:
      requestBody:
        required: true
        content:
          application/json:
            schema:
              type: object
              required: [device, temp]
              properties:
                device: { type: string }
                temp: { type: number }
      responses:
        "202": { description: Taken }
"#;

#[tokio::test(flavor = "multi_thread")]
async fn http_to_mqtt_to_sse() {
    let work = tempfile::tempdir().unwrap();
    let packages = work.path().join("packages");
    let data = work.path().join("server");
    std::fs::create_dir_all(&data).unwrap();
    let zips: Vec<PathBuf> = ["http_source", "mqtt_sink", "mqtt_source", "http_push_sink"]
        .iter()
        .map(|p| package(p, &packages))
        .collect();
    let broker = start_broker();
    let server = start_server(&data);
    let client = reqwest::Client::new();
    let base = server.base.clone();
    for zip in &zips {
        install(&client, &base, zip).await;
    }

    // The scene: four nodes, three edges.
    let (status, scene) = call(
        &client,
        reqwest::Method::POST,
        &format!("{base}/scenes"),
        Some(json!({"name": "e2e"})),
    )
    .await;
    assert!(status == 200 || status == 201, "{status} {scene}");
    let scene = format!("{base}/scenes/by-id/{}", scene["id"].as_str().unwrap());
    let (source_port, push_port) = (free_port(), free_port());
    let broker_url = format!("mqtt://127.0.0.1:{broker}");
    let mut ids = vec![];
    for (plugin, args) in [
        (
            "http-source",
            json!({ "listen": format!("127.0.0.1:{source_port}"), "openapi": OPENAPI }),
        ),
        (
            "mqtt-sink",
            json!({ "broker_url": broker_url, "default_topic": "e2e/readings" }),
        ),
        (
            "mqtt-source",
            json!({ "broker_url": broker_url, "subscriptions": [{ "filter": "e2e/#", "qos": 1 }] }),
        ),
        (
            "http-push-sink",
            json!({ "listen": format!("127.0.0.1:{push_port}") }),
        ),
    ] {
        let body =
            json!({ "plugin_id": plugin, "config": { "launch": "local", "launch_args": args } });
        let (status, node) = call(
            &client,
            reqwest::Method::POST,
            &format!("{scene}/nodes"),
            Some(body),
        )
        .await;
        assert!(status == 200 || status == 201, "{plugin}: {status} {node}");
        ids.push(node["id"].as_str().unwrap().to_string());
    }
    for (from, to, mapping) in [
        // The typed body, as JSON bytes, to the sink's default topic.
        (
            0,
            1,
            json!({ "mode": "replace", "rules": [
            { "target": "payload", "source": "paths[\"/readings\"].post.body", "encode": "json" } ] }),
        ),
        (1, 2, Value::Null),
        // The MQTT payload as the push event's data, on channel "readings".
        (
            2,
            3,
            json!({ "mode": "replace", "rules": [
            { "target": "data", "source": "payload" },
            { "target": "channel", "value": "readings" },
            { "target": "content_type", "value": "application/json" } ] }),
        ),
    ] {
        if mapping.is_null() {
            // MQTT sink → MQTT source isn't an edge: the broker is the link.
            let _ = (from, to);
            continue;
        }
        let body =
            json!({ "source_node_id": ids[from], "target_node_id": ids[to], "mapping": mapping });
        let (status, edge) = call(
            &client,
            reqwest::Method::POST,
            &format!("{scene}/edges"),
            Some(body),
        )
        .await;
        assert!(
            status == 200 || status == 201,
            "edge {from} → {to}: {status} {edge}"
        );
    }
    let (status, answer) = call(
        &client,
        reqwest::Method::POST,
        &format!("{scene}/enable"),
        None,
    )
    .await;
    assert!(status < 300, "enable: {status} {answer}");

    // Every node running.
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let (_, status) = call(
            &client,
            reqwest::Method::GET,
            &format!("{scene}/status"),
            None,
        )
        .await;
        let nodes = status["nodes"].as_array().cloned().unwrap_or_default();
        if nodes.len() == 4 && nodes.iter().all(|n| n["status"]["state"] == "RUNNING") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "not every node is running: {status}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // An SSE subscriber, then a request.
    wait_for_port(push_port, "the push sink");
    let sse = std::thread::spawn(move || -> String {
        let mut stream = TcpStream::connect(("127.0.0.1", push_port)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .unwrap();
        stream.write_all(b"GET /subscribe/readings HTTP/1.1\r\nhost: localhost\r\naccept: text/event-stream\r\n\r\n").unwrap();
        let mut lines = BufReader::new(stream);
        let mut seen = String::new();
        let mut line = String::new();
        while lines.read_line(&mut line).map(|n| n > 0).unwrap_or(false) {
            seen.push_str(&line);
            if line.starts_with("data: {") {
                return seen;
            }
            line.clear();
        }
        seen
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mut request = TcpStream::connect(("127.0.0.1", source_port)).unwrap();
    let body = r#"{"device": "d1", "temp": 21.5}"#;
    write!(
        request,
        "POST /readings HTTP/1.1\r\nhost: localhost\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut answer = String::new();
    request.read_to_string(&mut answer).unwrap();
    assert!(
        answer.starts_with("HTTP/1.1 202"),
        "the HTTP source answered {answer:?}"
    );

    let seen = sse.join().unwrap();
    let data = seen
        .lines()
        .find_map(|l| l.strip_prefix("data: "))
        .unwrap_or_else(|| panic!("no event on the stream: {seen:?}"));
    let event: Value = serde_json::from_str(data).unwrap();
    assert_eq!(event, json!({"device": "d1", "temp": 21.5}), "{seen}");
    assert!(seen.contains("id: "), "{seen}");

    let (status, _) = call(
        &client,
        reqwest::Method::POST,
        &format!("{scene}/disable"),
        None,
    )
    .await;
    assert!(status < 300);
    drop(server);
}
