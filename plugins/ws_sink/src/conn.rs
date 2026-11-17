//! Connections: one task per URL owns its WebSocket, dials (with backoff),
//! pings, writes the messages it's handed in order and, in the reply mode,
//! matches incoming messages to waiting requests by id. The [`Pool`] keeps one
//! connection per URL and closes idle ones.

use crate::config::{Reply, Settings};
use crate::json_path;
use futures_util::{SinkExt, StreamExt};
use std::collections::HashMap;
use std::collections::hash_map::RandomState;
use std::hash::BuildHasher;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime};
use tokio::sync::{mpsc, oneshot, watch};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::{Error as WsError, Message};
use tokio_tungstenite::{Connector, connect_async_tls_with_config};

const MIN_DELAY: Duration = Duration::from_secs(1);
const MAX_DELAY: Duration = Duration::from_secs(30);
const STABLE: Duration = Duration::from_secs(10);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// While not connected, a send waits this long for the connection.
const CONNECT_GRACE: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub message: String,
    pub retryable: bool,
}

impl Failure {
    pub fn retry(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            retryable: true,
        }
    }
    pub fn fatal(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            retryable: false,
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

type Replies = Arc<Mutex<HashMap<String, oneshot::Sender<serde_json::Value>>>>;

struct Request {
    message: Message,
    written: oneshot::Sender<Result<(), Failure>>,
}

/// One URL's connection.
pub struct Conn {
    url: String,
    requests: mpsc::Sender<Request>,
    connected: watch::Receiver<bool>,
    replies: Replies,
    last_used: Mutex<Instant>,
    busy: AtomicU64,
    closed: watch::Sender<bool>,
}

impl Conn {
    fn start(url: String, settings: Arc<Settings>) -> Arc<Self> {
        let (requests, rx) = mpsc::channel(64);
        let (connected_tx, connected) = watch::channel(false);
        let (closed, closed_rx) = watch::channel(false);
        let conn = Arc::new(Self {
            url: url.clone(),
            requests,
            connected,
            replies: Arc::default(),
            last_used: Mutex::new(Instant::now()),
            busy: AtomicU64::new(0),
            closed,
        });
        tokio::spawn(run(
            url,
            settings,
            rx,
            connected_tx,
            conn.replies.clone(),
            closed_rx,
        ));
        conn
    }

    fn close(&self) {
        self.closed.send_replace(true);
    }

    fn idle_for(&self) -> Option<Duration> {
        (self.busy.load(Ordering::SeqCst) == 0).then(|| lock(&self.last_used).elapsed())
    }

    /// Sends a message; with `reply`, also waits for the reply with `id`.
    pub async fn send(
        &self,
        message: Message,
        reply: Option<(&Reply, String)>,
    ) -> Result<(), Failure> {
        self.busy.fetch_add(1, Ordering::SeqCst);
        let result = self.send_inner(message, reply).await;
        self.busy.fetch_sub(1, Ordering::SeqCst);
        *lock(&self.last_used) = Instant::now();
        result
    }

    async fn send_inner(
        &self,
        message: Message,
        reply: Option<(&Reply, String)>,
    ) -> Result<(), Failure> {
        let mut connected = self.connected.clone();
        let _ = tokio::time::timeout(CONNECT_GRACE, connected.wait_for(|c| *c)).await;
        if !*connected.borrow() {
            return Err(Failure::retry(format!("not connected to {}", self.url)));
        }
        // Registered before sending, so a fast reply can't be missed.
        let waiting = match &reply {
            Some((_, id)) => {
                let (tx, rx) = oneshot::channel();
                let mut replies = lock(&self.replies);
                if replies.contains_key(id) {
                    return Err(Failure::retry(format!(
                        "a request with id {id:?} is already waiting"
                    )));
                }
                replies.insert(id.clone(), tx);
                Some(rx)
            }
            None => None,
        };
        let reply_id = reply.as_ref().map(|(_, id)| id.clone());
        let forget = || {
            if let Some(id) = &reply_id {
                lock(&self.replies).remove(id);
            }
        };
        let (written, rx) = oneshot::channel();
        if self
            .requests
            .send(Request { message, written })
            .await
            .is_err()
        {
            forget();
            return Err(Failure::retry("the connection is closing"));
        }
        let result = rx
            .await
            .unwrap_or_else(|_| Err(Failure::retry("the connection was lost")));
        if let Err(e) = result {
            forget();
            return Err(e);
        }
        let (Some((settings, id)), Some(waiting)) = (reply, waiting) else {
            return Ok(());
        };
        match tokio::time::timeout(settings.timeout, waiting).await {
            Err(_) => {
                forget();
                Err(Failure::retry(format!(
                    "no reply for {id:?} within {} ms",
                    settings.timeout.as_millis()
                )))
            }
            Ok(Err(_)) => Err(Failure::retry("the connection was lost before the reply")),
            Ok(Ok(answer)) => match settings
                .reply_error_path
                .as_ref()
                .and_then(|p| json_path::get(&answer, p))
            {
                None | Some(serde_json::Value::Null | serde_json::Value::Bool(false)) => Ok(()),
                Some(error) => Err(Failure::fatal(format!(
                    "the server replied with an error: {error}"
                ))),
            },
        }
    }
}

/// Spreads `delay` by up to ±10 %.
fn jitter(delay: Duration) -> Duration {
    let random = RandomState::new().hash_one(SystemTime::now());
    delay.mul_f64(0.9 + (random % 2001) as f64 / 10_000.0)
}

/// A refusal that needs a config change: retried, but only at the longest delay.
fn needs_config_change(e: &WsError) -> bool {
    matches!(e, WsError::Http(r) if matches!(r.status().as_u16(), 400 | 401 | 403 | 404))
}

async fn run(
    url: String,
    settings: Arc<Settings>,
    mut requests: mpsc::Receiver<Request>,
    connected: watch::Sender<bool>,
    replies: Replies,
    mut closed: watch::Receiver<bool>,
) {
    let mut delay = MIN_DELAY;
    let mut warned_at: Option<Instant> = None;
    let mut ignored: u64 = 0;
    loop {
        if *closed.borrow() {
            return;
        }
        let started = Instant::now();
        let reason = match dial(&url, &settings).await {
            Err(e) => {
                if needs_config_change(&e) {
                    delay = MAX_DELAY;
                }
                format!("connecting failed: {e}")
            }
            Ok(mut ws) => {
                connected.send_replace(true);
                warned_at = None;
                log::info!("connected to {url}");
                let mut ping = tokio::time::interval(settings.ping_interval);
                ping.tick().await;
                let mut awaiting_pong = false;
                let reason = loop {
                    tokio::select! {
                        request = requests.recv() => {
                            let Some(Request { message, written }) = request else {
                                let _ = ws.close(None).await;
                                return;
                            };
                            match ws.send(message).await {
                                Ok(()) => drop(written.send(Ok(()))),
                                Err(WsError::Capacity(e)) => drop(written.send(Err(Failure::fatal(format!("the message is too large: {e}"))))),
                                Err(e) => {
                                    let _ = written.send(Err(Failure::retry(format!("sending failed: {e}"))));
                                    break format!("sending failed: {e}");
                                }
                            }
                        }
                        incoming = ws.next() => match incoming {
                            Some(Ok(Message::Text(text))) => received(text.as_bytes(), &settings, &replies, &mut ignored),
                            Some(Ok(Message::Binary(data))) => received(&data, &settings, &replies, &mut ignored),
                            Some(Ok(Message::Pong(_))) => awaiting_pong = false,
                            Some(Ok(Message::Close(frame))) => {
                                if let Some(f) = &frame
                                    && matches!(u16::from(f.code), 1003 | 1008)
                                {
                                    delay = MAX_DELAY;
                                }
                                break match frame {
                                    Some(f) => format!("the server closed the connection ({}: {})", u16::from(f.code), f.reason),
                                    None => "the server closed the connection".into(),
                                };
                            }
                            // Pings are answered by tungstenite (on the next write or flush).
                            Some(Ok(_)) => drop(ws.flush().await),
                            Some(Err(e)) => break format!("the connection failed: {e}"),
                            None => break "the connection ended".into(),
                        },
                        _ = ping.tick() => {
                            if awaiting_pong {
                                break "no answer to a ping".into();
                            }
                            awaiting_pong = true;
                            if let Err(e) = ws.send(Message::Ping(Default::default())).await {
                                break format!("sending a ping failed: {e}");
                            }
                        }
                        _ = closed.changed() => {
                            let _ = ws.close(None).await;
                            return;
                        }
                    }
                };
                connected.send_replace(false);
                // Replies can't come on another connection.
                lock(&replies).clear();
                reason
            }
        };
        if started.elapsed() >= STABLE {
            delay = MIN_DELAY;
        }
        if warned_at.is_none_or(|t| t.elapsed() >= Duration::from_secs(60)) {
            log::warn!("{url}: {reason}; retrying");
            warned_at = Some(Instant::now());
        } else {
            log::debug!("{url}: {reason}");
        }
        // Requests that arrive while down fail at once (the hub's queue is the buffer).
        let wait = tokio::time::sleep(jitter(delay));
        tokio::pin!(wait);
        loop {
            tokio::select! {
                _ = &mut wait => break,
                request = requests.recv() => match request {
                    Some(r) => drop(r.written.send(Err(Failure::retry(format!("not connected to {url}"))))),
                    None => return,
                },
                _ = closed.changed() => return,
            }
        }
        delay = (delay * 2).min(MAX_DELAY);
    }
}

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn dial(url: &str, settings: &Settings) -> Result<Ws, WsError> {
    let mut request = url.into_client_request()?;
    let headers = request.headers_mut();
    for (name, value) in &settings.headers {
        headers.insert(name.clone(), value.clone());
    }
    if !settings.subprotocols.is_empty()
        && let Ok(value) = HeaderValue::try_from(settings.subprotocols.join(", "))
    {
        headers.insert("sec-websocket-protocol", value);
    }
    let config = WebSocketConfig::default()
        .max_message_size(Some(settings.max_message_bytes))
        .max_frame_size(Some(settings.max_message_bytes));
    let connector = settings.tls.clone().map(Connector::Rustls);
    let dialing = connect_async_tls_with_config(request, Some(config), true, connector);
    match tokio::time::timeout(CONNECT_TIMEOUT, dialing).await {
        Ok(result) => result.map(|(ws, _)| ws),
        Err(_) => Err(WsError::Io(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "connecting timed out",
        ))),
    }
}

/// An incoming message: a reply to a waiting request, or ignored.
fn received(data: &[u8], settings: &Settings, replies: &Replies, ignored: &mut u64) {
    if let Some(reply) = &settings.reply
        && let Ok(json) = serde_json::from_slice::<serde_json::Value>(data)
        && let Some(id) = json_path::get(&json, &reply.reply_id_path).and_then(json_path::id_of)
        && let Some(waiting) = lock(replies).remove(&id)
    {
        let _ = waiting.send(json);
        return;
    }
    *ignored += 1;
    log::debug!("an incoming message that isn't a reply was ignored ({ignored} so far)");
}

/// One connection per URL.
pub struct Pool {
    settings: Arc<Settings>,
    conns: Mutex<HashMap<String, Arc<Conn>>>,
}

impl Pool {
    pub fn start(settings: Arc<Settings>) -> Arc<Self> {
        let pool = Arc::new(Self {
            settings,
            conns: Mutex::default(),
        });
        if !pool.settings.lazy_connect {
            let _ = pool.get(&pool.settings.url.clone());
        }
        // Closes connections to events' URLs that weren't used for idle_timeout.
        let weak = Arc::downgrade(&pool);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(5));
            loop {
                tick.tick().await;
                let Some(pool) = weak.upgrade() else { return };
                let mut conns = lock(&pool.conns);
                conns.retain(|url, conn| {
                    let idle = *url != pool.settings.url
                        && conn
                            .idle_for()
                            .is_some_and(|d| d >= pool.settings.idle_timeout);
                    if idle {
                        log::debug!("closing the idle connection to {url}");
                        conn.close();
                    }
                    !idle
                });
            }
        });
        pool
    }

    /// The connection for `url`, opened if needed. A full pool closes its
    /// longest-idle connection first (never the configured url's).
    pub fn get(&self, url: &str) -> Result<Arc<Conn>, Failure> {
        let mut conns = lock(&self.conns);
        if let Some(conn) = conns.get(url) {
            return Ok(conn.clone());
        }
        if conns.len() >= self.settings.max_connections {
            let idlest = conns
                .iter()
                .filter(|(u, _)| **u != self.settings.url)
                .filter_map(|(u, c)| c.idle_for().map(|d| (d, u.clone())))
                .max();
            match idlest {
                Some((_, u)) => {
                    if let Some(c) = conns.remove(&u) {
                        c.close();
                    }
                }
                None => {
                    return Err(Failure::retry(format!(
                        "all {} connections are busy (max_connections)",
                        self.settings.max_connections
                    )));
                }
            }
        }
        let conn = Conn::start(url.to_string(), self.settings.clone());
        conns.insert(url.to_string(), conn.clone());
        Ok(conn)
    }

    pub fn close_all(&self) {
        for conn in lock(&self.conns).values() {
            conn.close();
        }
    }
}

/// A request id: unique within this process.
pub fn new_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let random = RandomState::new().hash_one((SystemTime::now(), n));
    format!("{random:016x}-{n}")
}
