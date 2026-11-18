//! Benchmarks (`benches/*.bench.toml`) with WebSocket clients in this process.
//! Not part of `cargo test`: run them with
//! `cargo test --release -p flwrs-ws-source --test bench -- --ignored --nocapture`.
//! CPU and memory are the plugin's alone; the clients run in this process.

use flwrs_plugin::schema::payload::PluginPayload;
use flwrs_trellis::bench::BenchResult;
use flwrs_trellis::{Bencher, Target, free_port};
use futures_util::SinkExt;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;

fn target() -> Target {
    Target::from_package(Path::new(env!("CARGO_MANIFEST_DIR")))
        .unwrap()
        .with_executable(env!("CARGO_BIN_EXE_flwrs-ws-source"))
        .with_configurator_executable(env!("CARGO_BIN_EXE_flwrs-ws-source-config"))
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

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Connects once the plugin listens and is connected to its hub (503 before).
async fn connect(port: u16, tls: &ClientTls) -> Ws {
    let url = match tls {
        None => format!("ws://127.0.0.1:{port}/devices/d1"),
        Some(_) => format!("wss://127.0.0.1:{port}/devices/d1"),
    };
    loop {
        let connector = tls.clone().map(tokio_tungstenite::Connector::Rustls);
        if let Ok((ws, _)) =
            tokio_tungstenite::connect_async_tls_with_config(&url, None, true, connector).await
        {
            return ws;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// TLS for the clients (`None`: plain).
type ClientTls = Option<Arc<rustls::ClientConfig>>;

const READING: &str = r#"{"temp": 21.5, "at": "2026-11-18T10:00:00Z"}"#;

/// `connections` clients sending back to back (reconnecting after a close).
fn flood(port: u16, connections: usize, tls: ClientTls) -> Vec<tokio::task::JoinHandle<()>> {
    (0..connections)
        .map(|_| {
            let tls = tls.clone();
            tokio::spawn(async move {
                loop {
                    let mut ws = connect(port, &tls).await;
                    while ws.send(Message::text(READING)).await.is_ok() {}
                }
            })
        })
        .collect()
}

/// Workers, each sending the messages it's handed on its own connection.
fn pool(port: u16, size: usize) -> Arc<Vec<tokio::sync::mpsc::UnboundedSender<String>>> {
    let workers = (0..size)
        .map(|_| {
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
            tokio::spawn(async move {
                let mut ws: Option<Ws> = None;
                while let Some(text) = rx.recv().await {
                    if ws.is_none() {
                        ws = Some(connect(port, &None).await);
                    }
                    if let Some(w) = ws.as_mut()
                        && w.send(Message::text(text)).await.is_err()
                    {
                        ws = None;
                    }
                }
            });
            tx
        })
        .collect();
    Arc::new(workers)
}

/// The reading's `at`: the stimulus's id, as a time in the message.
fn correlate(payload: &PluginPayload) -> Option<String> {
    let json = flwrs_trellis::payload::to_plain(payload);
    json.pointer("/channels/~1devices~1{deviceId}/messages/reading/temp")
        .and_then(|v| v.as_f64())
        .map(|t| format!("{}", t as u64))
}

/// A self-signed certificate for 127.0.0.1, given to the plugin as its
/// `tls_cert` / `tls_key` uploads, and a client config trusting it.
struct Tls {
    _dir: tempfile::TempDir,
    cert: std::path::PathBuf,
    key: std::path::PathBuf,
    roots: rustls::RootCertStore,
}

impl Tls {
    fn new() -> Self {
        let certified = rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_string()]).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let (cert, key) = (dir.path().join("server.pem"), dir.path().join("server.key"));
        std::fs::write(&cert, certified.cert.pem()).unwrap();
        std::fs::write(&key, certified.signing_key.serialize_pem()).unwrap();
        let mut roots = rustls::RootCertStore::empty();
        roots.add(certified.cert.der().clone()).unwrap();
        Self {
            _dir: dir,
            cert,
            key,
            roots,
        }
    }

    /// The plugin with the certificate uploaded.
    fn target(&self, target: Target) -> Target {
        target
            .with_upload("server.pem", &self.cert)
            .unwrap()
            .with_upload("server.key", &self.key)
            .unwrap()
    }

    /// A client config offering `alpn` (e.g. `h2`, `http/1.1`).
    fn client(&self, alpn: &[&str]) -> Arc<rustls::ClientConfig> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut config = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(self.roots.clone())
            .with_no_client_auth();
        config.alpn_protocols = alpn.iter().map(|a| a.as_bytes().to_vec()).collect();
        Arc::new(config)
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "benchmarks: run explicitly, in release"]
async fn bench() {
    let mut results = vec![];
    let tls = Tls::new();
    for file in bench_files() {
        let port = free_port();
        let encrypted = file.to_string_lossy().contains("tls");
        let target = if encrypted {
            tls.target(target())
        } else {
            target()
        };
        let client_tls = encrypted.then(|| tls.client(&[]));
        let mut bencher = Bencher::new(target).var("ws", port.to_string());
        let flooding = if file.to_string_lossy().contains("1k") {
            let pool = pool(port, 16);
            let n = Arc::new(AtomicU64::new(0));
            bencher = bencher
                .stimulus(move |_| {
                    // The temperature is the id (the schema caps it at 100: modulo).
                    let i = n.fetch_add(1, Ordering::Relaxed) % 100;
                    let worker = pool[(i % pool.len() as u64) as usize].clone();
                    async move {
                        worker
                            .send(format!(r#"{{"temp": {i}}}"#))
                            .map_err(|e| e.to_string())?;
                        Ok(Some(i.to_string()))
                    }
                })
                .correlate(correlate);
            vec![]
        } else {
            flood(port, 32, client_tls)
        };
        results.push(bencher.run_file(&file).await.unwrap());
        for task in flooding {
            task.abort();
        }
    }
    save(&results);
}
