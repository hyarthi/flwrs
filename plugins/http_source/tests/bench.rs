//! Benchmarks (`benches/*.bench.toml`) with HTTP clients in this process.
//! Not part of `cargo test`: run them with
//! `cargo test --release -p flwrs-http-source --test bench -- --ignored --nocapture`.
//! CPU and memory are the plugin's alone; the clients run in this process.

use flwrs_plugin::schema::payload::PluginPayload;
use flwrs_trellis::bench::BenchResult;
use flwrs_trellis::{Bencher, Target, free_port};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

/// A connection, plain or TLS.
trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

/// TLS for the clients (`None`: plain).
type ClientTls = Option<Arc<rustls::ClientConfig>>;

/// Connects (plain or TLS), retrying while the plugin starts.
async fn dial(port: u16, tls: &ClientTls) -> Option<Box<dyn Io>> {
    for _ in 0..200 {
        if let Ok(tcp) = TcpStream::connect(("127.0.0.1", port)).await {
            let _ = tcp.set_nodelay(true);
            match tls {
                None => return Some(Box::new(tcp)),
                Some(config) => {
                    let name = rustls::pki_types::ServerName::try_from("127.0.0.1").unwrap();
                    if let Ok(stream) = tokio_rustls::TlsConnector::from(config.clone())
                        .connect(name, tcp)
                        .await
                    {
                        return Some(Box::new(stream));
                    }
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    None
}

fn target() -> Target {
    Target::from_package(Path::new(env!("CARGO_MANIFEST_DIR")))
        .unwrap()
        .with_executable(env!("CARGO_BIN_EXE_flwrs-http-source"))
        .with_configurator_executable(env!("CARGO_BIN_EXE_flwrs-http-source-config"))
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

/// A keep-alive HTTP/1.1 connection.
struct Client(BufReader<Box<dyn Io>>);

impl Client {
    async fn connect(port: u16, tls: &ClientTls) -> Option<Self> {
        dial(port, tls).await.map(|io| Self(BufReader::new(io)))
    }

    /// POSTs a JSON body; the status.
    async fn post(&mut self, path: &str, body: &str) -> std::io::Result<u16> {
        let request = format!(
            "POST {path} HTTP/1.1\r\nhost: localhost\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
            body.len()
        );
        self.0.get_mut().write_all(request.as_bytes()).await?;
        let mut line = String::new();
        self.0.read_line(&mut line).await?;
        let status = line
            .split(' ')
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let mut length = 0usize;
        loop {
            line.clear();
            if self.0.read_line(&mut line).await? == 0 {
                return Err(std::io::ErrorKind::UnexpectedEof.into());
            }
            if line == "\r\n" {
                break;
            }
            if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                length = v.trim().parse().unwrap_or(0);
            }
        }
        let mut body = vec![0; length];
        self.0.read_exact(&mut body).await?;
        Ok(status)
    }
}

const BODY: &str = r#"{"temp": 21.5, "at": "2026-11-18T10:00:00Z", "tags": ["a", "b"]}"#;

/// `connections` clients posting back to back (reconnecting while the plugin starts).
fn flood(port: u16, connections: usize, tls: ClientTls) -> Vec<tokio::task::JoinHandle<()>> {
    (0..connections)
        .map(|_| {
            let tls = tls.clone();
            tokio::spawn(async move {
                loop {
                    let Some(mut client) = Client::connect(port, &tls).await else {
                        return;
                    };
                    while client.post("/devices/d1", BODY).await.is_ok() {}
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            })
        })
        .collect()
}

/// Workers, each posting the bodies it's sent on its own connection.
fn pool(port: u16, size: usize) -> Arc<Vec<tokio::sync::mpsc::UnboundedSender<String>>> {
    let workers = (0..size)
        .map(|_| {
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
            tokio::spawn(async move {
                let mut client = None;
                while let Some(body) = rx.recv().await {
                    if client.is_none() {
                        client = Client::connect(port, &None).await;
                    }
                    if let Some(c) = client.as_mut()
                        && c.post("/devices/d1", &body).await.is_err()
                    {
                        client = None;
                    }
                }
            });
            tx
        })
        .collect();
    Arc::new(workers)
}

/// The `tags[0]` of a typed body: the stimulus's id.
fn correlate(payload: &PluginPayload) -> Option<String> {
    let json = flwrs_trellis::payload::to_plain(payload);
    json.pointer("/paths/~1devices~1{id}/post/body/tags/0")
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

/// `connections` × `streams` concurrent HTTP/2 requests (h2c, or TLS
/// with ALPN `h2`), back to back (reconnecting while the plugin starts).
fn flood_h2(
    port: u16,
    connections: usize,
    streams: usize,
    tls: ClientTls,
) -> Vec<tokio::task::JoinHandle<()>> {
    use http_body_util::{BodyExt, Full};
    let scheme = if tls.is_some() { "https" } else { "http" };
    (0..connections)
        .map(|_| {
            let tls = tls.clone();
            tokio::spawn(async move {
                loop {
                    let Some(conn) = dial(port, &tls).await else {
                        return;
                    };
                    let io = hyper_util::rt::TokioIo::new(conn);
                    let Ok((sender, conn)) = hyper::client::conn::http2::handshake(
                        hyper_util::rt::TokioExecutor::new(),
                        io,
                    )
                    .await
                    else {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                        continue;
                    };
                    let connection = tokio::spawn(conn);
                    let tasks: Vec<_> = (0..streams)
                        .map(|_| {
                            let mut sender = sender.clone();
                            tokio::spawn(async move {
                                loop {
                                    let request = hyper::Request::post(format!(
                                        "{scheme}://127.0.0.1:{port}/devices/d1"
                                    ))
                                    .header("content-type", "application/json")
                                    .body(Full::new(bytes::Bytes::from_static(BODY.as_bytes())))
                                    .unwrap();
                                    let Ok(response) = sender.send_request(request).await else {
                                        return;
                                    };
                                    if response.into_body().collect().await.is_err() {
                                        return;
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
        })
        .collect()
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
        let name = file.to_string_lossy().to_string();
        let target = if name.contains("tls") {
            tls.target(target())
        } else {
            target()
        };
        let h2 = name.contains("h2");
        let client_tls = name
            .contains("tls")
            .then(|| tls.client(if h2 { &["h2"] } else { &["http/1.1"] }));
        let mut bencher = Bencher::new(target).var("http", port.to_string());
        let steady = file.to_string_lossy().contains("1k");
        let flooding = if steady {
            // One request per stimulus, handed to a pool of keep-alive
            // connections (new connections per request would pile up in
            // TIME_WAIT); not waiting for answers (confirmed ones wait for
            // the hub's ACK).
            let pool = pool(port, 16);
            let n = Arc::new(AtomicU64::new(0));
            bencher = bencher
                .stimulus(move |_| {
                    let i = n.fetch_add(1, Ordering::Relaxed);
                    let id = format!("m{i}");
                    let worker = pool[(i % pool.len() as u64) as usize].clone();
                    async move {
                        worker
                            .send(format!(r#"{{"temp": 1, "tags": ["{id}"]}}"#))
                            .map_err(|e| e.to_string())?;
                        Ok(Some(id))
                    }
                })
                .correlate(correlate);
            vec![]
        } else if h2 {
            // 1 connection × 32 streams, or (`8conns`) 8 × 4.
            if name.contains("8conns") {
                flood_h2(port, 8, 4, client_tls)
            } else {
                flood_h2(port, 1, 32, client_tls)
            }
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
