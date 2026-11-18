//! Benchmarks (`benches/*.bench.toml`) with SSE subscribers in this process.
//! Not part of `cargo test`: run them with
//! `cargo test --release -p flwrs-http-push-sink --test bench -- --ignored --nocapture`.
//! CPU and memory are the plugin's alone; the subscribers run in this process.

use flwrs_trellis::bench::BenchResult;
use flwrs_trellis::{Bencher, Target, free_port};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn target() -> Target {
    Target::from_package(Path::new(env!("CARGO_MANIFEST_DIR")))
        .unwrap()
        .with_executable(env!("CARGO_BIN_EXE_flwrs-http-push-sink"))
}

fn bench_files() -> Vec<std::path::PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("benches");
    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.to_string_lossy().ends_with(".bench.toml"))
        // FLWRS_BENCH=<part of a file name>: only those.
        .filter(|p| match std::env::var("FLWRS_BENCH") {
            Ok(only) => p.to_string_lossy().contains(&only),
            Err(_) => true,
        })
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

/// An SSE subscriber reading `/subscribe/bench` until the end, again and again
/// (the plugin restarts between runs); `read` counts the bytes.
fn subscriber(port: u16, read: Arc<AtomicU64>, tls: ClientTls) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            if let Ok(tcp) = tokio::net::TcpStream::connect(("127.0.0.1", port)).await {
                let _ = tcp.set_nodelay(true);
                let stream: Option<Box<dyn Io>> = match &tls {
                    None => Some(Box::new(tcp)),
                    Some(config) => {
                        let name = rustls::pki_types::ServerName::try_from("127.0.0.1").unwrap();
                        tokio_rustls::TlsConnector::from(config.clone())
                            .connect(name, tcp)
                            .await
                            .ok()
                            .map(|s| Box::new(s) as Box<dyn Io>)
                    }
                };
                if let Some(mut s) = stream {
                    let request = "GET /subscribe/bench HTTP/1.1\r\nhost: localhost\r\naccept: text/event-stream\r\n\r\n";
                    if s.write_all(request.as_bytes()).await.is_ok() {
                        let mut buf = vec![0u8; 64 * 1024];
                        while let Ok(n) = s.read(&mut buf).await {
                            if n == 0 {
                                break;
                            }
                            read.fetch_add(n as u64, Ordering::Relaxed);
                        }
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
}

/// A connection, plain or TLS.
trait Io: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send> Io for T {}

/// TLS for the clients (`None`: plain).
type ClientTls = Option<Arc<rustls::ClientConfig>>;

/// `streams` SSE subscribers over one HTTP/2 (h2c) connection, again and
/// again (the plugin restarts between runs); `read` counts the bytes.
fn subscribers_h2(port: u16, streams: usize, read: Arc<AtomicU64>) -> tokio::task::JoinHandle<()> {
    use http_body_util::{BodyExt, Empty};
    tokio::spawn(async move {
        loop {
            let Ok(tcp) = tokio::net::TcpStream::connect(("127.0.0.1", port)).await else {
                tokio::time::sleep(Duration::from_millis(20)).await;
                continue;
            };
            let _ = tcp.set_nodelay(true);
            let io = hyper_util::rt::TokioIo::new(tcp);
            let Ok((sender, conn)) =
                hyper::client::conn::http2::handshake(hyper_util::rt::TokioExecutor::new(), io)
                    .await
            else {
                tokio::time::sleep(Duration::from_millis(20)).await;
                continue;
            };
            let connection = tokio::spawn(conn);
            let tasks: Vec<_> = (0..streams)
                .map(|_| {
                    let mut sender = sender.clone();
                    let read = read.clone();
                    tokio::spawn(async move {
                        let request =
                            hyper::Request::get(format!("http://127.0.0.1:{port}/subscribe/bench"))
                                .body(Empty::<bytes::Bytes>::new())
                                .unwrap();
                        let Ok(response) = sender.send_request(request).await else {
                            return;
                        };
                        let mut body = response.into_body();
                        while let Some(Ok(frame)) = body.frame().await {
                            if let Some(data) = frame.data_ref() {
                                read.fetch_add(data.len() as u64, Ordering::Relaxed);
                            }
                        }
                    })
                })
                .collect();
            for t in tasks {
                let _ = t.await;
            }
            connection.abort();
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
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
        let count: usize = if file.to_string_lossy().contains("1000subs") {
            1000
        } else {
            1
        };
        let encrypted = file.to_string_lossy().contains("tls");
        let target = if encrypted {
            tls.target(target())
        } else {
            target()
        };
        let client_tls: ClientTls = encrypted.then(|| tls.client(&["http/1.1"]));
        let read = Arc::new(AtomicU64::new(0));
        let subscribers: Vec<_> = if file.to_string_lossy().contains("h2") {
            // HTTP/2: 125 streams per connection.
            let connections = count.div_ceil(125);
            (0..connections)
                .map(|_| subscribers_h2(port, count / connections, read.clone()))
                .collect()
        } else {
            (0..count)
                .map(|_| subscriber(port, read.clone(), client_tls.clone()))
                .collect()
        };
        let mut bencher = Bencher::new(target).var("http", port.to_string());
        results.push(bencher.run_file(&file).await.unwrap());
        for s in subscribers {
            s.abort();
        }
        println!(
            "{}: {count} subscriber(s) read {:.1} MiB",
            file.display(),
            read.load(Ordering::Relaxed) as f64 / 1048576.0
        );
    }
    save(&results);
}
