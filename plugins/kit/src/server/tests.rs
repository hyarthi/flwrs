//! The server core end to end in one process: plain HTTP/1.1, h2c, TLS with
//! ALPN, mTLS, auth, limits, health and readiness, WebSocket upgrades, and
//! shutdown.

use super::*;
use axum::extract::Path as UrlPath;
use axum::extract::ws::{Message as WsMessage, WebSocketUpgrade};
use axum::routing::{any, post};
use futures_util::{SinkExt, StreamExt};
use rcgen::{BasicConstraints, CertificateParams, CertifiedIssuer, IsCa, KeyPair};
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn routes() -> Router {
    Router::new()
        .route(
            "/echo",
            get(|request: Request| async move {
                let principal = request
                    .extensions()
                    .get::<Principal>()
                    .map(|p| p.0.clone())
                    .unwrap_or_default();
                let conn = request.extensions().get::<ConnInfo>().expect("conn info");
                format!("{principal}|{}|{:?}", conn.tls, request.version())
            }),
        )
        .route(
            "/body",
            post(|body: axum::body::Bytes| async move { body.len().to_string() }),
        )
        .route(
            "/slow/{ms}",
            get(|UrlPath(ms): UrlPath<u64>| async move {
                tokio::time::sleep(Duration::from_millis(ms)).await;
                "done"
            }),
        )
        .route(
            "/ws",
            any(|ws: WebSocketUpgrade| async move {
                ws.on_upgrade(|mut socket| async move {
                    while let Some(Ok(WsMessage::Text(t))) = socket.recv().await {
                        let _ = socket.send(WsMessage::Text(t)).await;
                    }
                })
            }),
        )
}

fn always() -> Readiness {
    Arc::new(|| true)
}

async fn start(settings: ServerSettings) -> Server {
    Server::start(settings, routes(), always()).await.unwrap()
}

fn url(server: &Server, path: &str) -> String {
    format!("http://{}{path}", server.local_addr())
}

/// A CA, a server certificate for localhost, and client certificates.
struct Pki {
    dir: tempfile::TempDir,
    ca: CertifiedIssuer<'static, KeyPair>,
}

impl Pki {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let ca = CertifiedIssuer::self_signed(params, KeyPair::generate().unwrap()).unwrap();
        std::fs::write(dir.path().join("ca.pem"), ca.pem()).unwrap();
        let pki = Self { dir, ca };
        pki.leaf("server", &["localhost"]);
        pki
    }

    /// Writes `<name>.pem` and `<name>.key`; returns cert + key PEM.
    fn leaf(&self, name: &str, names: &[&str]) -> Vec<u8> {
        let params =
            CertificateParams::new(names.iter().map(|n| n.to_string()).collect::<Vec<_>>())
                .unwrap();
        let key = KeyPair::generate().unwrap();
        let cert = params.signed_by(&key, &self.ca).unwrap();
        std::fs::write(self.path(&format!("{name}.pem")), cert.pem()).unwrap();
        std::fs::write(self.path(&format!("{name}.key")), key.serialize_pem()).unwrap();
        [cert.pem().into_bytes(), key.serialize_pem().into_bytes()].concat()
    }

    fn path(&self, file: &str) -> std::path::PathBuf {
        self.dir.path().join(file)
    }

    fn settings(&self, client_auth: Option<tls::ClientAuth>) -> ServerSettings {
        let ca = self.path("ca.pem");
        let config = tls::server_config(&tls::TlsFiles {
            cert: &self.path("server.pem"),
            key: &self.path("server.key"),
            client_ca: client_auth.map(|_| ca.as_path()),
            client_auth: client_auth.unwrap_or(tls::ClientAuth::Optional),
        })
        .unwrap();
        ServerSettings {
            tls: Some(config),
            ..ServerSettings::local(0)
        }
    }

    fn client(&self, server: &Server, identity: Option<Vec<u8>>) -> reqwest::ClientBuilder {
        let ca =
            reqwest::Certificate::from_pem(&std::fs::read(self.path("ca.pem")).unwrap()).unwrap();
        let builder = reqwest::Client::builder()
            .add_root_certificate(ca)
            .resolve("localhost", server.local_addr());
        match identity {
            Some(pem) => builder.identity(reqwest::Identity::from_pem(&pem).unwrap()),
            None => builder,
        }
    }
}

fn https(server: &Server, path: &str) -> String {
    format!("https://localhost:{}{path}", server.local_addr().port())
}

#[tokio::test]
async fn serves_http1_with_health_and_readiness() {
    let ready = Arc::new(AtomicBool::new(false));
    let probe = ready.clone();
    let server = Server::start(
        ServerSettings::local(0),
        routes(),
        Arc::new(move || probe.load(Ordering::SeqCst)),
    )
    .await
    .unwrap();
    let client = reqwest::Client::new();
    let get = |path: &str| client.get(url(&server, path)).send();
    assert_eq!(get("/healthz").await.unwrap().text().await.unwrap(), "ok");
    assert_eq!(get("/readyz").await.unwrap().status(), 503);
    ready.store(true, Ordering::SeqCst);
    assert_eq!(get("/readyz").await.unwrap().status(), 200);
    let echo = get("/echo").await.unwrap().text().await.unwrap();
    assert_eq!(echo, "|false|HTTP/1.1");
    assert_eq!(get("/nope").await.unwrap().status(), 404);
}

#[tokio::test]
async fn h2c_only_when_enabled() {
    let prior_knowledge = reqwest::Client::builder()
        .http2_prior_knowledge()
        .build()
        .unwrap();
    let server = start(ServerSettings::local(0)).await;
    assert!(
        prior_knowledge
            .get(url(&server, "/echo"))
            .send()
            .await
            .is_err()
    );
    let server = start(ServerSettings {
        h2c: true,
        ..ServerSettings::local(0)
    })
    .await;
    let echo = prior_knowledge
        .get(url(&server, "/echo"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!(echo, "|false|HTTP/2.0");
    // HTTP/1.1 still works next to it.
    let echo = reqwest::get(url(&server, "/echo"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!(echo, "|false|HTTP/1.1");
}

#[tokio::test]
async fn tls_negotiates_http2_or_http1() {
    let pki = Pki::new();
    let server = start(pki.settings(None)).await;
    let h2 = pki.client(&server, None).build().unwrap();
    let echo = h2
        .get(https(&server, "/echo"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!(echo, "|true|HTTP/2.0");
    let h1 = pki.client(&server, None).http1_only().build().unwrap();
    let echo = h1
        .get(https(&server, "/echo"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!(echo, "|true|HTTP/1.1");
}

#[tokio::test]
async fn mtls_names_the_principal_and_checks_the_allowlist() {
    let pki = Pki::new();
    let dev1 = pki.leaf("dev-1", &["dev-1"]);
    let dev2 = pki.leaf("dev-2", &["dev-2"]);
    let mut settings = pki.settings(Some(tls::ClientAuth::Required));
    settings.auth = Arc::new(Auth::Mtls {
        fingerprints: vec![],
        names: vec!["dev-1".into()],
    });
    let server = start(settings).await;
    let echo = pki
        .client(&server, Some(dev1))
        .build()
        .unwrap()
        .get(https(&server, "/echo"))
        .send()
        .await
        .unwrap();
    assert_eq!(echo.text().await.unwrap(), "dev-1|true|HTTP/2.0");
    let refused = pki
        .client(&server, Some(dev2))
        .build()
        .unwrap()
        .get(https(&server, "/echo"))
        .send()
        .await
        .unwrap();
    assert_eq!(refused.status(), 403);
    // Required: no certificate, no connection.
    assert!(
        pki.client(&server, None)
            .build()
            .unwrap()
            .get(https(&server, "/echo"))
            .send()
            .await
            .is_err()
    );
}

#[tokio::test]
async fn bearer_tokens_guard_routes_but_not_health() {
    let tokens = auth::Secrets::new([("ci".to_string(), "t0ken".to_string())].into());
    let server = start(ServerSettings {
        auth: Arc::new(Auth::Bearer(tokens)),
        ..ServerSettings::local(0)
    })
    .await;
    let client = reqwest::Client::new();
    let refused = client.get(url(&server, "/echo")).send().await.unwrap();
    assert_eq!(refused.status(), 401);
    assert_eq!(refused.headers()["www-authenticate"], "Bearer");
    let wrong = client
        .get(url(&server, "/echo"))
        .bearer_auth("nope")
        .send()
        .await
        .unwrap();
    assert_eq!(wrong.status(), 401);
    let ok = client
        .get(url(&server, "/echo"))
        .bearer_auth("t0ken")
        .send()
        .await
        .unwrap();
    assert_eq!(ok.text().await.unwrap(), "ci|false|HTTP/1.1");
    assert_eq!(
        client
            .get(url(&server, "/healthz"))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(
        client
            .get(url(&server, "/readyz"))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
}

#[tokio::test]
async fn limits_answer_413_and_503() {
    let limits = Limits {
        max_body_bytes: 100,
        max_concurrent_requests: 1,
        request_timeout_ms: 300,
        ..Limits::default()
    };
    let server = start(ServerSettings {
        limits,
        ..ServerSettings::local(0)
    })
    .await;
    let client = reqwest::Client::new();
    let post = |n: usize| {
        client
            .post(url(&server, "/body"))
            .body(vec![b'x'; n])
            .send()
    };
    assert_eq!(post(100).await.unwrap().text().await.unwrap(), "100");
    assert_eq!(post(101).await.unwrap().status(), 413);
    // Too slow: 503 + Retry-After.
    let slow = client.get(url(&server, "/slow/1000")).send().await.unwrap();
    assert_eq!(slow.status(), 503);
    assert_eq!(slow.headers()["retry-after"], "1");
    // One at a time: a second request while one runs is refused.
    let first = tokio::spawn(client.get(url(&server, "/slow/200")).send());
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        client
            .get(url(&server, "/echo"))
            .send()
            .await
            .unwrap()
            .status(),
        503
    );
    assert_eq!(first.await.unwrap().unwrap().status(), 200);
}

#[tokio::test]
async fn connections_beyond_the_limit_wait() {
    let limits = Limits {
        max_connections: 1,
        ..Limits::default()
    };
    let server = start(ServerSettings {
        limits,
        ..ServerSettings::local(0)
    })
    .await;
    let idle = tokio::net::TcpStream::connect(server.local_addr())
        .await
        .unwrap();
    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(300))
        .build()
        .unwrap();
    assert!(
        client.get(url(&server, "/echo")).send().await.is_err(),
        "waits for the slot"
    );
    drop(idle);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        client
            .get(url(&server, "/echo"))
            .send()
            .await
            .unwrap()
            .status()
            .is_success()
    );
}

#[tokio::test]
async fn slow_headers_are_cut_off() {
    let limits = Limits {
        header_read_timeout_ms: 200,
        ..Limits::default()
    };
    let server = start(ServerSettings {
        limits,
        ..ServerSettings::local(0)
    })
    .await;
    let mut tcp = tokio::net::TcpStream::connect(server.local_addr())
        .await
        .unwrap();
    tcp.write_all(b"GET /echo HTTP/1.1\r\nhost: x\r\n")
        .await
        .unwrap();
    let mut buf = vec![];
    let read = tokio::time::timeout(Duration::from_secs(2), tcp.read_to_end(&mut buf)).await;
    assert!(read.is_ok(), "the server closed the connection");
}

#[tokio::test]
async fn websocket_upgrades_go_through() {
    let server = start(ServerSettings::local(0)).await;
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{}/ws", server.local_addr()))
        .await
        .unwrap();
    ws.send(tokio_tungstenite::tungstenite::Message::text("hi"))
        .await
        .unwrap();
    let echoed = ws.next().await.unwrap().unwrap();
    assert_eq!(echoed.into_text().unwrap().as_str(), "hi");
}

#[tokio::test]
async fn shutdown_finishes_requests_within_the_grace() {
    let server = start(ServerSettings::local(0)).await;
    let addr = server.local_addr();
    let mut stopping = server.stopping();
    assert!(!*stopping.borrow());
    // A connection first (kept alive), so the slow request is surely in flight.
    let client = reqwest::Client::new();
    client
        .get(format!("http://{addr}/healthz"))
        .send()
        .await
        .unwrap();
    let in_flight = tokio::spawn(client.get(format!("http://{addr}/slow/300")).send());
    tokio::time::sleep(Duration::from_millis(100)).await;
    server.shutdown(Duration::from_secs(2)).await;
    assert_eq!(
        in_flight.await.unwrap().unwrap().text().await.unwrap(),
        "done"
    );
    assert!(stopping.wait_for(|s| *s).await.is_ok());
    assert!(
        tokio::net::TcpStream::connect(addr).await.is_err(),
        "no longer accepting"
    );
}

#[tokio::test]
async fn shutdown_closes_what_outlives_the_grace() {
    let server = start(ServerSettings::local(0)).await;
    let addr = server.local_addr();
    let in_flight = tokio::spawn(reqwest::get(format!("http://{addr}/slow/5000")));
    tokio::time::sleep(Duration::from_millis(50)).await;
    let started = Instant::now();
    server.shutdown(Duration::from_millis(200)).await;
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(in_flight.await.unwrap().is_err());
}
