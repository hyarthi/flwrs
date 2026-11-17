//! Benchmarks (`benches/*.bench.toml`) against an embedded broker (rumqttd, no subscribers).
//! Not part of `cargo test`: run them with
//! `cargo test --release -p flwrs-mqtt-sink --test bench -- --ignored --nocapture`.
//! CPU and memory are the plugin's alone; the other side runs in this process.

use flwrs_trellis::bench::BenchResult;
use flwrs_trellis::{Bencher, Target};
use std::path::Path;
use std::time::Duration;

fn target() -> Target {
    Target::from_package(Path::new(env!("CARGO_MANIFEST_DIR")))
        .unwrap()
        .with_executable(env!("CARGO_BIN_EXE_flwrs-mqtt-sink"))
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

#[tokio::test(flavor = "multi_thread")]
#[ignore = "benchmarks: run explicitly, in release"]
async fn bench() {
    let (v4, v5) = start_broker();
    let mut results = vec![];
    for file in bench_files() {
        // MQTT 3.1.1 benches use the 3.1.1 listener.
        let port = if file.to_string_lossy().contains("v311") {
            v4
        } else {
            v5
        };
        let mut bencher = Bencher::new(target()).var("broker", port.to_string());
        results.push(bencher.run_file(&file).await.unwrap());
    }
    save(&results);
}
