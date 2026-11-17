//! Benchmarks (`benches/*.bench.toml`) against a WebSocket server in this process (/sink discards, /echo replies).
//! Not part of `cargo test`: run them with
//! `cargo test --release -p flwrs-ws-sink --test bench -- --ignored --nocapture`.
//! CPU and memory are the plugin's alone; the other side runs in this process.

use flwrs_trellis::bench::BenchResult;
use flwrs_trellis::{Bencher, Target};
use futures_util::{SinkExt, StreamExt};
use std::path::Path;
use tokio_tungstenite::tungstenite::Message;

fn target() -> Target {
    Target::from_package(Path::new(env!("CARGO_MANIFEST_DIR")))
        .unwrap()
        .with_executable(env!("CARGO_BIN_EXE_flwrs-ws-sink"))
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

// The handshake callback's error type is tungstenite's.
#[allow(clippy::result_large_err)]
async fn server() -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut path = String::new();
                let callback =
                    |request: &tokio_tungstenite::tungstenite::handshake::server::Request,
                     response| {
                        path = request.uri().path().to_string();
                        Ok(response)
                    };
                let Ok(mut ws) = tokio_tungstenite::accept_hdr_async(stream, callback).await else {
                    return;
                };
                while let Some(Ok(message)) = ws.next().await {
                    if path == "/echo" && (message.is_text() || message.is_binary()) {
                        let json: serde_json::Value =
                            serde_json::from_slice(&message.into_data()).unwrap_or_default();
                        let answer = serde_json::json!({"id": json["id"], "ok": true});
                        if ws.send(Message::text(answer.to_string())).await.is_err() {
                            return;
                        }
                    }
                }
            });
        }
    });
    port
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "benchmarks: run explicitly, in release"]
async fn bench() {
    let port = server().await;
    let mut results = vec![];
    for file in bench_files() {
        let mut bencher = Bencher::new(target()).var("ws", port.to_string());
        results.push(bencher.run_file(&file).await.unwrap());
    }
    save(&results);
}
