//! Benchmarks (`benches/*.bench.toml`) against an embedded broker (rumqttd) and a publisher in this process.
//! Not part of `cargo test`: run them with
//! `cargo test --release -p flwrs-mqtt-source --test bench -- --ignored --nocapture`.
//! CPU and memory are the plugin's alone; the other side runs in this process.

use flwrs_trellis::bench::BenchResult;
use flwrs_trellis::{Bencher, Target};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

fn target() -> Target {
    Target::from_package(Path::new(env!("CARGO_MANIFEST_DIR")))
        .unwrap()
        .with_executable(env!("CARGO_BIN_EXE_flwrs-mqtt-source"))
}

fn bench_files() -> Vec<std::path::PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("benches");
    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.to_string_lossy().ends_with(".bench.toml"))
        .collect();
    files.sort();
    files
}

/// Prints the report and writes the results to target/bench/<plugin>.json.
fn save(results: &[BenchResult]) {
    println!("{}", flwrs_trellis::bench::report(results));
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/bench");
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join(format!("{}.json", env!("CARGO_PKG_NAME")));
    std::fs::write(&file, serde_json::to_string_pretty(results).unwrap()).unwrap();
    println!("results: {}", file.display());
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// An embedded broker (rumqttd) with an MQTT 3.1.1 and an MQTT 5 listener.
fn start_broker() -> (u16, u16) {
    let (v4, v5) = (free_port(), free_port());
    let listener = |name: &str, port: u16| {
        serde_json::json!({ "1": {
            "name": name, "listen": format!("127.0.0.1:{port}"), "next_connection_delay_ms": 1,
            "connections": { "connection_timeout_ms": 60000, "max_payload_size": 1048576,
                             "max_inflight_count": 500, "dynamic_filters": true }
        }})
    };
    let config: rumqttd::Config = serde_json::from_value(serde_json::json!({
        "id": 0,
        "router": { "max_connections": 100, "max_outgoing_packet_count": 1000,
                    "max_segment_size": 104857600, "max_segment_count": 10 },
        "v4": listener("v4", v4), "v5": listener("v5", v5),
    }))
    .unwrap();
    std::thread::spawn(move || rumqttd::Broker::new(config).start().unwrap());
    for port in [v4, v5] {
        for _ in 0..100 {
            if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    (v4, v5)
}

/// An MQTT 5 publisher whose event loop runs in the background.
fn publisher(port: u16, id: &str) -> rumqttc::v5::AsyncClient {
    let mut options = rumqttc::v5::MqttOptions::new(id, "127.0.0.1", port);
    options.set_outgoing_inflight_upper_limit(200);
    let (client, mut events) = rumqttc::v5::AsyncClient::new(options, 1000);
    tokio::spawn(async move { while events.poll().await.is_ok() {} });
    client
}

/// Publishes QoS 1 messages as fast as the broker takes them.
fn flood(port: u16) -> tokio::task::JoinHandle<()> {
    let client = publisher(port, "flood");
    let payload = "x".repeat(100);
    tokio::spawn(async move {
        loop {
            if client
                .publish(
                    "bench/flood",
                    rumqttc::v5::mqttbytes::QoS::AtLeastOnce,
                    false,
                    payload.clone(),
                )
                .await
                .is_err()
            {
                return;
            }
        }
    })
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "benchmarks: run explicitly, in release"]
async fn bench() {
    let (v4, v5) = start_broker();
    let _ = v4;
    let mut results = vec![];
    for file in bench_files() {
        let steady = file.to_string_lossy().contains("1k");
        let mut bencher = Bencher::new(target()).var("broker", v5.to_string());
        let flooding = if steady {
            // One message per stimulus, its payload a unique id: latency from publish to event.
            let client = Arc::new(publisher(v5, "stimulus"));
            let n = Arc::new(AtomicU64::new(0));
            bencher = bencher
                .stimulus(move |_| {
                    let (client, id) = (
                        client.clone(),
                        format!("m{}", n.fetch_add(1, Ordering::Relaxed)),
                    );
                    async move {
                        client
                            .publish(
                                "bench/one",
                                rumqttc::v5::mqttbytes::QoS::AtLeastOnce,
                                false,
                                id.clone(),
                            )
                            .await
                            .map_err(|e| e.to_string())?;
                        Ok(Some(id))
                    }
                })
                .correlate(|payload| {
                    payload
                        .fields
                        .iter()
                        .find(|f| f.key == "payload")
                        .and_then(|f| match f.value.as_ref()?.value.as_ref()? {
                            flwrs_plugin::schema::payload::field_value::Value::Bytes(b) => {
                                String::from_utf8(b.clone()).ok()
                            }
                            _ => None,
                        })
                });
            None
        } else {
            Some(flood(v5))
        };
        results.push(bencher.run_file(&file).await.unwrap());
        if let Some(flood) = flooding {
            flood.abort();
        }
    }
    save(&results);
}
