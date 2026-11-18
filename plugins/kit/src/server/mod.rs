//! The HTTP server core for flwrs' HTTP and WebSocket plugins: a plugin hands
//! over an axum [`Router`] and gets
//!
//! - its own accept loop: TLS (rustls / ring, ALPN `h2` + `http/1.1`),
//!   optional client certificates, HTTP/1.1 and HTTP/2 (h2c behind a flag),
//!   upgrades (WebSockets), at most `max_connections` at once;
//! - per request: [`ConnInfo`] (remote address, client certificate) and,
//!   past auth, the [`Principal`];
//! - `GET` health and readiness endpoints (no auth);
//! - limits: body size (413), concurrent requests and request timeout (503 +
//!   `Retry-After`), header size and read timeout, HTTP/2 streams;
//! - an access log at DEBUG (never headers), and a rate-limited WARN that
//!   sums up refused requests;
//! - shutdown: stop accepting, let connections finish within a grace, then
//!   close them. Streaming handlers (SSE, WebSockets) watch [`Server::stopping`].
//!
//! [`ServerArgs`] are the settings a plugin flattens into its command line;
//! [`HeaderPolicy`] decides which request headers an event may carry.

pub mod args;
pub mod auth;
pub mod headers;
#[cfg(test)]
mod tests;
pub mod tls;

pub use args::{Limits, ServerArgs, ServerSettings, schema_properties};
pub use auth::{Auth, Principal, is_preflight, query_pairs};
pub use headers::{Filtered, HeaderPolicy};
pub use tls::ClientCert;

use axum::Router;
use axum::extract::{DefaultBodyLimit, MatchedPath, Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use hyper_util::server::conn::auto;
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpListener;
use tokio::sync::{Semaphore, watch};
use tokio::task::JoinSet;
use tower::ServiceExt;

/// Whether the plugin can make progress (credit for a source, accepting for
/// a sink): `/readyz` answers 200 or 503.
pub type Readiness = Arc<dyn Fn() -> bool + Send + Sync>;

/// The connection a request came on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnInfo {
    pub remote_addr: SocketAddr,
    pub tls: bool,
    /// A client certificate the TLS layer verified (mTLS).
    pub client_cert: Option<ClientCert>,
}

#[cfg(test)]
impl ConnInfo {
    pub(crate) fn test() -> Self {
        Self {
            remote_addr: SocketAddr::from(([127, 0, 0, 1], 1)),
            tls: true,
            client_cert: None,
        }
    }
}

/// Refused requests by reason, summed up in a WARN at most once a minute.
static REFUSALS: Mutex<Option<(Instant, BTreeMap<&'static str, u64>)>> = Mutex::new(None);

pub(crate) fn note_refusal(reason: &'static str) {
    let mut guard = REFUSALS.lock().unwrap_or_else(|e| e.into_inner());
    let (since, counts) = guard.get_or_insert_with(|| (Instant::now(), BTreeMap::new()));
    *counts.entry(reason).or_default() += 1;
    if since.elapsed() >= Duration::from_secs(60) || counts.values().sum::<u64>() == 1 {
        let summary: Vec<String> = counts.iter().map(|(r, n)| format!("{r} {n}")).collect();
        log::warn!("refused requests: {}", summary.join(", "));
        *since = Instant::now();
        counts.clear();
    }
}

fn unavailable(why: &'static str) -> Response {
    note_refusal(why);
    let mut response = StatusCode::SERVICE_UNAVAILABLE.into_response();
    response
        .headers_mut()
        .insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
    response
}

struct LimitState {
    requests: Arc<Semaphore>,
    timeout: Duration,
    max_body: u64,
}

async fn limits_layer(State(s): State<Arc<LimitState>>, request: Request, next: Next) -> Response {
    let length = request
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    if length.is_some_and(|l| l > s.max_body) {
        note_refusal("too large");
        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
    }
    let Ok(_permit) = s.requests.clone().try_acquire_owned() else {
        return unavailable("too many requests at once");
    };
    match tokio::time::timeout(s.timeout, next.run(request)).await {
        Ok(response) => response,
        Err(_) => unavailable("timed out"),
    }
}

async fn access_log(request: Request, next: Next) -> Response {
    let started = Instant::now();
    let method = request.method().clone();
    let path = request
        .extensions()
        .get::<MatchedPath>()
        .map(|p| p.as_str().to_string())
        .unwrap_or_else(|| "-".into());
    let version = request.version();
    let response = next.run(request).await;
    if log::log_enabled!(log::Level::Debug) {
        let size = response
            .headers()
            .get(header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("-")
            .to_string();
        log::debug!(
            "{method} {path} {version:?} {} {size} {:.1} ms",
            response.status().as_u16(),
            started.elapsed().as_secs_f64() * 1000.0
        );
    }
    response
}

/// The whole app: health and readiness, the plugin's routes behind auth,
/// and the limits around everything.
fn app(settings: &ServerSettings, routes: Router, public: Router, ready: Readiness) -> Router {
    let auth = settings.auth.clone();
    let protected = routes.layer(middleware::from_fn(move |request, next| {
        auth::layer(auth.clone(), request, next)
    }));
    let limits = Arc::new(LimitState {
        requests: Arc::new(Semaphore::new(settings.limits.max_concurrent_requests)),
        timeout: settings.request_timeout(),
        max_body: settings.limits.max_body_bytes as u64,
    });
    Router::new()
        .route(&settings.health_path, get(|| async { "ok" }))
        .route(
            &settings.ready_path,
            get(move || {
                let ready = ready();
                async move {
                    if ready {
                        (StatusCode::OK, "ready")
                    } else {
                        (StatusCode::SERVICE_UNAVAILABLE, "not ready")
                    }
                }
            }),
        )
        .merge(public)
        .merge(protected)
        .layer(DefaultBodyLimit::max(settings.limits.max_body_bytes))
        .layer(middleware::from_fn_with_state(limits, limits_layer))
        .layer(middleware::from_fn(access_log))
}

/// How connections are served: HTTP/1.1 only (plain, no h2c), or HTTP/1.1
/// and HTTP/2 (TLS with ALPN, or h2c). hyper-util's `http1_only` doesn't
/// apply to connections with upgrades, so the HTTP/1.1-only case uses
/// hyper's own HTTP/1 builder.
enum Protocols {
    Http1(hyper::server::conn::http1::Builder),
    Auto(auto::Builder<TokioExecutor>),
}

fn protocols(settings: &ServerSettings) -> Protocols {
    if settings.tls.is_none() && !settings.h2c {
        let mut http1 = hyper::server::conn::http1::Builder::new();
        http1
            .timer(TokioTimer::new())
            .header_read_timeout(settings.header_read_timeout())
            .max_buf_size(settings.limits.max_header_bytes.max(8192));
        return Protocols::Http1(http1);
    }
    let mut builder = auto::Builder::new(TokioExecutor::new());
    builder
        .http1()
        .timer(TokioTimer::new())
        .header_read_timeout(settings.header_read_timeout())
        .max_buf_size(settings.limits.max_header_bytes.max(8192));
    builder
        .http2()
        .timer(TokioTimer::new())
        .max_concurrent_streams(settings.limits.h2_max_streams)
        .max_send_buf_size(settings.limits.h2_max_send_buf_bytes)
        .max_header_list_size(settings.limits.max_header_bytes as u32)
        .keep_alive_interval(Some(Duration::from_secs(30)))
        .keep_alive_timeout(Duration::from_secs(20))
        // WebSockets over HTTP/2 (RFC 8441) for the routes that accept them.
        .enable_connect_protocol();
    Protocols::Auto(builder)
}

/// A running server.
pub struct Server {
    local_addr: SocketAddr,
    /// `None` while running, then the grace for open connections.
    stop: watch::Sender<Option<Duration>>,
    task: tokio::task::JoinHandle<()>,
}

impl Server {
    /// Binds and starts serving `routes` (their paths must not clash with
    /// the health and readiness paths).
    pub async fn start(
        settings: ServerSettings,
        routes: Router,
        ready: Readiness,
    ) -> std::io::Result<Server> {
        Self::start_with_public(settings, routes, Router::new(), ready).await
    }

    /// Like [`Server::start`], with `public` routes that need no auth (e.g.
    /// API docs a plugin opens on purpose). They still get the limits.
    pub async fn start_with_public(
        settings: ServerSettings,
        routes: Router,
        public: Router,
        ready: Readiness,
    ) -> std::io::Result<Server> {
        for warning in &settings.warnings {
            log::warn!("{warning}");
        }
        let listener = TcpListener::bind(settings.listen).await?;
        let local_addr = listener.local_addr()?;
        let (stop, _) = watch::channel(None);
        let app = app(&settings, routes, public, ready);
        let protocols = Arc::new(protocols(&settings));
        let tls = settings.tls.clone().map(tokio_rustls::TlsAcceptor::from);
        let task = tokio::spawn(accept_loop(
            listener,
            app,
            protocols,
            tls,
            settings,
            stop.subscribe(),
        ));
        log::info!("listening on {local_addr}");
        Ok(Server {
            local_addr,
            stop,
            task,
        })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Becomes `true` when the server stops: streaming handlers (SSE,
    /// WebSockets) end their streams then.
    pub fn stopping(&self) -> watch::Receiver<bool> {
        let mut source = self.stop.subscribe();
        let (tx, rx) = watch::channel(source.borrow().is_some());
        tokio::spawn(async move {
            if source.wait_for(Option::is_some).await.is_ok() {
                let _ = tx.send(true);
            }
        });
        rx
    }

    /// Stops accepting, lets open connections finish their requests within
    /// `grace`, then closes what's left.
    pub async fn shutdown(self, grace: Duration) {
        let _ = self.stop.send(Some(grace));
        let _ = self.task.await;
    }
}

/// Waits until the server stops (dropping the watch's guard at once, so the
/// caller's future stays `Send`).
async fn stopped(stop: &mut watch::Receiver<Option<Duration>>) {
    let _ = stop.wait_for(Option::is_some).await;
}

async fn accept_loop(
    listener: TcpListener,
    app: Router,
    protocols: Arc<Protocols>,
    tls: Option<tokio_rustls::TlsAcceptor>,
    settings: ServerSettings,
    mut stop: watch::Receiver<Option<Duration>>,
) {
    let connections = Arc::new(Semaphore::new(settings.limits.max_connections));
    let handshake_timeout = settings.header_read_timeout();
    let mut open = JoinSet::new();
    loop {
        // A free slot first: beyond max_connections, new ones wait in the backlog.
        let permit = tokio::select! {
            permit = connections.clone().acquire_owned() => permit.expect("never closed"),
            _ = stopped(&mut stop) => break,
            Some(_) = open.join_next(), if !open.is_empty() => continue,
        };
        let (tcp, remote_addr) = tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok(a) => a,
                Err(e) => {
                    // E.g. out of file descriptors: back off instead of spinning.
                    log::warn!("accepting a connection failed: {e}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            },
            _ = stopped(&mut stop) => break,
        };
        let _ = tcp.set_nodelay(true);
        let (app, protocols, tls, stop) =
            (app.clone(), protocols.clone(), tls.clone(), stop.clone());
        open.spawn(async move {
            let _permit = permit;
            match tls {
                None => {
                    let info = ConnInfo {
                        remote_addr,
                        tls: false,
                        client_cert: None,
                    };
                    serve(tcp, info, app, &protocols, stop).await;
                }
                Some(acceptor) => {
                    match tokio::time::timeout(handshake_timeout, acceptor.accept(tcp)).await {
                        Ok(Ok(stream)) => {
                            let client_cert = stream
                                .get_ref()
                                .1
                                .peer_certificates()
                                .and_then(|c| c.first())
                                .map(ClientCert::of);
                            let info = ConnInfo {
                                remote_addr,
                                tls: true,
                                client_cert,
                            };
                            serve(stream, info, app, &protocols, stop).await;
                        }
                        Ok(Err(e)) => log::debug!("TLS handshake with {remote_addr} failed: {e}"),
                        Err(_) => log::debug!("TLS handshake with {remote_addr} timed out"),
                    }
                }
            }
        });
    }
    drop(listener);
    let grace = stop.borrow().unwrap_or_default();
    if tokio::time::timeout(grace, async { while open.join_next().await.is_some() {} })
        .await
        .is_err()
    {
        log::debug!(
            "closing {} connection(s) still open after the grace",
            open.len()
        );
        open.abort_all();
    }
}

async fn serve<I>(
    io: I,
    info: ConnInfo,
    app: Router,
    protocols: &Protocols,
    mut stop: watch::Receiver<Option<Duration>>,
) where
    I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let remote = info.remote_addr;
    let service = app.map_request(move |mut request: Request<hyper::body::Incoming>| {
        request.extensions_mut().insert(info.clone());
        request
    });
    let service = hyper_util::service::TowerToHyperService::new(service);
    let io = TokioIo::new(io);
    // On stop: finish what's in progress, take nothing new.
    let result: Result<(), Box<dyn std::error::Error + Send + Sync>> = match protocols {
        Protocols::Http1(http1) => {
            let connection = http1.serve_connection(io, service).with_upgrades();
            tokio::pin!(connection);
            tokio::select! {
                result = connection.as_mut() => result.map_err(Into::into),
                _ = stopped(&mut stop) => {
                    connection.as_mut().graceful_shutdown();
                    connection.await.map_err(Into::into)
                }
            }
        }
        Protocols::Auto(auto) => {
            let connection = auto.serve_connection_with_upgrades(io, service);
            tokio::pin!(connection);
            tokio::select! {
                result = connection.as_mut() => result,
                _ = stopped(&mut stop) => {
                    connection.as_mut().graceful_shutdown();
                    connection.await
                }
            }
        }
    };
    if let Err(e) = result {
        log::debug!("connection from {remote}: {e}");
    }
}
