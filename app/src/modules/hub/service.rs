//! The hub: the endpoint plugins connect to (protocol v2, `docs/protocol.md`).
//!
//! The director tells the hub which plugin instances (nodes) to expect with
//! [`Hub::expect`], and gets a [`NodeLink`] back: a stream of [`HubEvent`]s
//! from that node's plugin, and a way to send it messages. For each incoming
//! connection the hub runs the handshake (instance id, token, protocol
//! version, type, plugin version and payload schemas; capabilities), then one
//! reader and one writer task. The newest accepted connection for a node wins.
//!
//! On a running connection the hub enforces the protocol (allowed codes per
//! plugin type, sequence numbers, credit), answers and sends heartbeats,
//! remembers the metadata of events it sent to transforms (so their outputs
//! continue the right trace), and forwards data events and acknowledgements
//! to the director in the order they arrived.

use super::config::HubConfig;
use super::wire::{self, DataEvent, InitInfo, Negotiated};
use crate::modules::plugin::manifest::PluginType;
use crate::modules::rate_limit::RateLimit;
use flwrs_plugin::plugin::frame::{
    Frame, LimitedFrame, PROTOCOL_VERSION, WireMessage, encode_frame, encode_message,
    read_frame_limited,
};
use flwrs_plugin::schema::payload::{Field, SchemaDefinition, field_value::Value};
use flwrs_plugin::schema::protocol::{
    Ack, Credit, ErrorEvent, EventMetadata, Exit, FrameHeader, Initialize, InitializeResponse,
    LogEvent, Nack, Ping, Pong, Shutdown, capability::Enum as Capability,
    log_level::Enum as LogLevel, msg_code::Enum as MsgCode,
};
use flwrs_plugin::schema::rules;
use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Frames queued for one connection's writer.
const OUTBOUND_CAPACITY: usize = 64;
/// Events queued from one node's plugin to the director, beyond the credit
/// window. When full, the reader stops reading the socket: TCP backpressure
/// on plugins without credits.
const EVENTS_CAPACITY: usize = 64;
/// At most one WARN/ERROR per this interval for repeated handshake failures.
const REJECT_LOG_INTERVAL: Duration = Duration::from_secs(60);
/// A connection silent for this many heartbeat intervals is dead.
const MISSED_HEARTBEATS: u32 = 3;
/// Inputs a transform's outputs can still refer to after they're acked.
const INPUT_MEMORY: usize = 1024;

pub(crate) type BoxedReader = Box<dyn AsyncRead + Send + Unpin>;
pub(crate) type BoxedWriter = Box<dyn AsyncWrite + Send + Unpin>;

/// A plugin instance the hub should accept.
#[derive(Debug, Clone)]
pub(crate) struct ExpectedNode {
    pub scene_id: String,
    /// The node; unique.
    pub node_id: String,
    /// The id the plugin must send in `Initialize`: the node id for local
    /// plugins, the `remote_id` for remote ones. Not unique: several nodes
    /// (e.g. in two copies of an imported scene) may expect the same one, and
    /// the token decides which node a connection belongs to.
    pub instance_id: String,
    /// The installed plugin package.
    pub plugin_id: String,
    pub plugin_type: PluginType,
    pub plugin_version: String,
    /// From the manifest: what the plugin must declare.
    pub in_schema: Option<SchemaDefinition>,
    pub out_schema: Option<SchemaDefinition>,
    /// How the plugin proves it's this node.
    pub auth: NodeAuth,
    /// The node's own event size limit (`runtime.queue.max_event_bytes`); the
    /// hub applies the lower of this and its own.
    pub max_event_bytes: Option<usize>,
}

/// How a node's plugin authenticates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum NodeAuth {
    /// The per-launch token (local) or the pre-shared token (remote). Never
    /// empty. With `require_tls`, only over the TLS listener.
    Token { token: String, require_tls: bool },
    /// A client certificate on the TLS listener: with the pinned SHA-256
    /// fingerprint (lower-case hex), or else chaining to `[hub.tls]
    /// client_ca` and naming the instance id (SAN or CN).
    Mtls { pin: Option<String> },
}

impl NodeAuth {
    /// A local node's: a token, over any listener (local plugins use loopback).
    pub(crate) fn local(token: String) -> Self {
        Self::Token {
            token,
            require_tls: false,
        }
    }
}

/// How a connection reached the hub.
#[derive(Debug, Clone, Default)]
pub(crate) struct Transport {
    /// Through the TLS listener.
    pub tls: bool,
    /// The client certificate it presented, if any.
    pub client_cert: Option<ClientCert>,
}

/// What the hub knows about a client certificate.
#[derive(Debug, Clone)]
pub(crate) struct ClientCert {
    /// SHA-256 of the certificate (DER), lower-case hex.
    pub sha256: String,
    /// SAN DNS names and URIs, and the subject CN.
    pub names: Vec<String>,
    /// It chains to `[hub.tls] client_ca`.
    pub ca_verified: bool,
}

/// What the hub reports about a node's plugin.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum HubEvent {
    /// The plugin passed the handshake and got `InitializeResponse`.
    Connected {
        conn_id: u64,
        peer: SocketAddr,
        negotiated: Negotiated,
    },
    /// The plugin authenticated, but declared the wrong type, version or
    /// schemas. It got `Shutdown`. Retrying the same executable won't help.
    Rejected { reasons: Vec<String> },
    /// A data event from a source or transform. `event.metadata` is the
    /// metadata of the transform input it belongs to, if any.
    Event {
        conn_id: u64,
        seq: u64,
        event: DataEvent,
        size: usize,
    },
    /// A consumer acknowledged events the hub sent on `conn_id`.
    Acked {
        conn_id: u64,
        up_to: u64,
        seqs: Vec<u64>,
    },
    /// A consumer rejected an event the hub sent on `conn_id`.
    Nacked {
        conn_id: u64,
        seq: u64,
        reason: String,
        retry: bool,
    },
    /// The plugin sent `Exit`.
    Exit { conn_id: u64, exit: Exit },
    /// The current connection was lost (closed by the plugin, an I/O error,
    /// silence or a protocol violation). Not sent when the hub closed it.
    Disconnected { conn_id: u64, reason: String },
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub(crate) enum HubError {
    #[error("the plugin isn't connected")]
    NotConnected,
    #[error("can't send this message: {0}")]
    Invalid(String),
}

/// How the hub finished with an event a producer sent.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Routed {
    /// In every target's queue, or legitimately not forwarded.
    Accepted,
    /// Not (fully) taken: dropped somewhere. `retry` = sending it again may
    /// succeed.
    Rejected { reason: String, retry: bool },
}

/// The hub. Cheap to clone.
#[derive(Clone)]
pub(crate) struct Hub {
    inner: Arc<Inner>,
}

struct Inner {
    config: HubConfig,
    local_addr: SocketAddr,
    listener: Mutex<Option<TcpListener>>,
    /// The TLS listener's address, if `[hub.tls]` is set.
    tls_addr: Option<SocketAddr>,
    #[cfg(feature = "remote")]
    tls: Mutex<Option<Arc<super::tls::TlsListener>>>,
    nodes: Mutex<HashMap<String, NodeEntry>>,
    next_id: AtomicU64,
    /// Cancelled when the hub stops; every connection's token is a child.
    shutdown: CancellationToken,
    /// For handshake failures that aren't tied to an expected node.
    stranger_log: RateLimit,
}

struct NodeEntry {
    link_id: u64,
    expected: Arc<ExpectedNode>,
    events: mpsc::Sender<HubEvent>,
    conn: Option<ConnHandle>,
    reject_log: RateLimit,
}

#[derive(Clone)]
struct ConnHandle {
    id: u64,
    out: mpsc::Sender<Vec<u8>>,
    /// Cancelled when the hub closes the connection on purpose.
    close: CancellationToken,
    negotiated: Negotiated,
    shared: Arc<ConnShared>,
}

/// State of one connection shared by its reader and the senders.
struct ConnShared {
    plugin_type: PluginType,
    /// Next seq of an event the hub sends; held while the frame is queued, so
    /// frames go out in seq order.
    next_seq: tokio::sync::Mutex<u64>,
    /// Transforms: metadata of the inputs the hub sent.
    memory: Option<Mutex<InputMemory>>,
    /// Producers with CREDITS.
    credit: Option<Credits>,
}

/// Metadata of the inputs sent to a transform: every unacked one, plus the
/// most recent ones (an accumulator may emit for an input it acked earlier).
#[derive(Default)]
struct InputMemory {
    recent: VecDeque<(u64, EventMetadata)>,
    unacked: HashMap<u64, EventMetadata>,
}

impl InputMemory {
    fn insert(&mut self, seq: u64, metadata: EventMetadata, acks: bool) {
        if acks {
            self.unacked.insert(seq, metadata.clone());
        }
        if self.recent.len() == INPUT_MEMORY {
            self.recent.pop_front();
        }
        self.recent.push_back((seq, metadata));
    }

    fn get(&self, seq: u64) -> Option<EventMetadata> {
        if let Some(meta) = self.unacked.get(&seq) {
            return Some(meta.clone());
        }
        // seqs are increasing, so the ring is sorted.
        self.recent
            .binary_search_by_key(&seq, |(s, _)| *s)
            .ok()
            .map(|i| self.recent[i].1.clone())
    }

    fn acked(&mut self, up_to: u64, seqs: &[u64]) {
        self.unacked
            .retain(|seq, _| *seq > up_to && !seqs.contains(seq));
    }
}

/// Credit given to a producer, and what it has used.
struct Credits {
    window: u64,
    granted: AtomicU64,
    received: AtomicU64,
    /// Returned by the director, not yet granted again.
    pending: AtomicU64,
}

impl Credits {
    /// Credit to grant now, if any: when a quarter of the window has come
    /// back, or the producer has none left.
    fn take_grant(&self) -> Option<u32> {
        let pending = self.pending.load(Ordering::Acquire);
        let balance = self
            .granted
            .load(Ordering::Acquire)
            .saturating_sub(self.received.load(Ordering::Acquire));
        if pending == 0 || (pending < (self.window / 4).max(1) && balance > 0) {
            return None;
        }
        let n = self.pending.swap(0, Ordering::AcqRel);
        self.granted.fetch_add(n, Ordering::AcqRel);
        u32::try_from(n).ok()
    }
}

impl Hub {
    /// Binds the listener. Call [`Hub::run`] to start accepting.
    pub(crate) async fn bind(config: HubConfig) -> std::io::Result<Self> {
        let listener = TcpListener::bind((config.host.as_str(), config.port)).await?;
        let local_addr = listener.local_addr()?;
        #[cfg(feature = "remote")]
        let tls = match &config.tls {
            Some(tls) => Some(Arc::new(super::tls::bind(tls).await?)),
            None => None,
        };
        #[cfg(feature = "remote")]
        let tls_addr = match &tls {
            Some(tls) => Some(tls.listener.local_addr()?),
            None => None,
        };
        #[cfg(not(feature = "remote"))]
        let tls_addr = {
            if config.tls.is_some() {
                log::warn!(
                    "Hub: [hub.tls] is set, but this server was built without the remote feature; no TLS listener"
                );
            }
            None
        };
        Ok(Self {
            inner: Arc::new(Inner {
                config,
                local_addr,
                listener: Mutex::new(Some(listener)),
                tls_addr,
                #[cfg(feature = "remote")]
                tls: Mutex::new(tls),
                nodes: Mutex::new(HashMap::new()),
                next_id: AtomicU64::new(1),
                shutdown: CancellationToken::new(),
                stranger_log: RateLimit::new(Duration::from_secs(10)),
            }),
        })
    }

    pub(crate) fn config(&self) -> &HubConfig {
        &self.inner.config
    }

    /// The address plugins connect to (with the actual port if `port = 0`).
    pub(crate) fn local_addr(&self) -> SocketAddr {
        self.inner.local_addr
    }

    /// The TLS listener's address, if there is one.
    pub(crate) fn tls_addr(&self) -> Option<SocketAddr> {
        self.inner.tls_addr
    }

    /// Starts expecting `node`. Replaces an earlier expectation for the same
    /// node id (its link stops receiving events and its connection is closed).
    pub(crate) fn expect(&self, node: ExpectedNode) -> NodeLink {
        let capacity = EVENTS_CAPACITY + self.inner.config.credit_window as usize;
        let (tx, rx) = mpsc::channel(capacity);
        let link_id = self.inner.next_id();
        let node_id = node.node_id.clone();
        let old = self.inner.nodes().insert(
            node_id.clone(),
            NodeEntry {
                link_id,
                expected: Arc::new(node),
                events: tx,
                conn: None,
                reject_log: RateLimit::new(REJECT_LOG_INTERVAL),
            },
        );
        if let Some(conn) = old.and_then(|old| old.conn) {
            conn.close.cancel();
        }
        NodeLink {
            sender: LinkSender {
                node_id: node_id.into(),
                link_id,
                inner: self.inner.clone(),
            },
            events: rx,
        }
    }

    /// Accepts connections until `shutdown` is cancelled, then closes every
    /// connection.
    pub(crate) async fn run(&self, shutdown: CancellationToken) -> std::io::Result<()> {
        let listener = self
            .inner
            .listener
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
            .ok_or_else(|| std::io::Error::other("the hub is already running"))?;
        log::info!("Hub: listening for plugins on {}", self.inner.local_addr);
        #[cfg(feature = "remote")]
        let tls = self
            .inner
            .tls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        #[cfg(feature = "remote")]
        if tls.is_some() {
            log::info!(
                "Hub: listening for remote plugins over TLS on {}",
                self.inner.tls_addr.expect("bound with the listener")
            );
        }
        loop {
            #[cfg(feature = "remote")]
            let tls_accept = async {
                match &tls {
                    Some(tls) => tls.listener.accept().await.map(|a| (a, tls.clone())),
                    None => std::future::pending().await,
                }
            };
            #[cfg(not(feature = "remote"))]
            let tls_accept = std::future::pending::<std::io::Result<((TcpStream, SocketAddr), ())>>();
            tokio::select! {
                _ = shutdown.cancelled() => break,
                accepted = listener.accept() => match accepted {
                    Ok((stream, peer)) => {
                        tokio::spawn(serve_tcp(self.inner.clone(), stream, peer));
                    }
                    // E.g. out of file descriptors: keep going after a pause.
                    Err(err) => {
                        log::error!("Hub: accept failed: {err}");
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                },
                accepted = tls_accept => match accepted {
                    #[cfg(feature = "remote")]
                    Ok(((stream, peer), tls)) => {
                        tokio::spawn(serve_tls(self.inner.clone(), tls, stream, peer));
                    }
                    #[cfg(not(feature = "remote"))]
                    Ok(_) => {}
                    Err(err) => {
                        log::error!("Hub: TLS accept failed: {err}");
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                },
            }
        }
        self.inner.shutdown.cancel();
        log::info!("Hub: stopped");
        Ok(())
    }
}

/// The director's side of one expected node. Dropping it makes the hub stop
/// expecting the node and close its connection.
pub(crate) struct NodeLink {
    sender: LinkSender,
    events: mpsc::Receiver<HubEvent>,
}

/// Sends to an expected node's current connection. Cheap to clone; it stays
/// valid across reconnects and token renewals, until the link is dropped.
#[derive(Clone)]
pub(crate) struct LinkSender {
    node_id: Arc<str>,
    link_id: u64,
    inner: Arc<Inner>,
}

impl LinkSender {
    fn with_entry<R>(&self, f: impl FnOnce(&mut NodeEntry) -> R) -> Option<R> {
        self.inner
            .nodes()
            .get_mut(&*self.node_id)
            .filter(|entry| entry.link_id == self.link_id)
            .map(f)
    }

    fn current_conn(&self) -> Option<ConnHandle> {
        self.with_entry(|entry| entry.conn.clone()).flatten()
    }

    pub(crate) fn is_connected(&self) -> bool {
        self.current_conn().is_some()
    }

    /// The current connection's id and what it negotiated.
    pub(crate) fn connection(&self) -> Option<(u64, Negotiated)> {
        self.current_conn().map(|c| (c.id, c.negotiated))
    }

    /// Sends a data event to the plugin's current connection (sinks and
    /// transforms). Waits while the connection's outbound queue is full.
    /// Returns the connection id and the event's seq on it.
    pub(crate) async fn send_event(&self, event: &DataEvent) -> Result<(u64, u64), HubError> {
        let conn = self.current_conn().ok_or(HubError::NotConnected)?;
        if conn.shared.plugin_type == PluginType::Source {
            return Err(HubError::Invalid("a source doesn't receive events".into()));
        }
        let mut next_seq = conn.shared.next_seq.lock().await;
        let seq = *next_seq;
        let header = FrameHeader {
            code: MsgCode::Event as i32,
            seq,
            metadata: event.metadata.clone(),
            attempt: event.attempt.max(1),
            ..Default::default()
        };
        let frame = encode_frame(&header, &event.body);
        if let (Some(memory), Some(metadata)) = (&conn.shared.memory, &event.metadata) {
            lock(memory).insert(seq, metadata.clone(), conn.negotiated.acks);
        }
        conn.out
            .send(frame)
            .await
            .map_err(|_| HubError::NotConnected)?;
        *next_seq += 1;
        Ok((conn.id, seq))
    }

    /// Asks the plugin to stop (`Shutdown`).
    pub(crate) async fn shutdown(&self, reason: &str) -> Result<(), HubError> {
        let conn = self.current_conn().ok_or(HubError::NotConnected)?;
        let frame = encode_message(&Shutdown {
            reason: reason.into(),
        });
        conn.out
            .send(frame)
            .await
            .map_err(|_| HubError::NotConnected)
    }

    /// Tells a producer how the hub finished with its event `seq` on
    /// `conn_id`: an ACK / NACK (with ACKS), and its credit back (with
    /// CREDITS). Does nothing if that connection is gone.
    pub(crate) async fn routed(&self, conn_id: u64, seq: u64, outcome: Routed) {
        if let Some(conn) = self.current_conn().filter(|c| c.id == conn_id) {
            send_routed(&conn, seq, outcome).await;
        }
    }
}

impl NodeLink {
    /// The next event from the node's plugin. `None` if the expectation was
    /// replaced by a newer [`Hub::expect`] for the same node.
    pub(crate) async fn recv(&mut self) -> Option<HubEvent> {
        self.events.recv().await
    }

    /// A cloneable handle for sending to the plugin.
    pub(crate) fn sender(&self) -> LinkSender {
        self.sender.clone()
    }

    pub(crate) fn is_connected(&self) -> bool {
        self.sender.is_connected()
    }

    pub(crate) async fn shutdown(&self, reason: &str) -> Result<(), HubError> {
        self.sender.shutdown(reason).await
    }

    /// Closes the current connection, if any (no `Disconnected` event follows).
    /// A plugin that's still running will reconnect.
    #[cfg(test)]
    pub(crate) fn disconnect(&self) {
        self.sender.with_entry(|entry| {
            if let Some(conn) = entry.conn.take() {
                conn.close.cancel();
            }
        });
    }

    /// Expects a new token from now on (e.g. for a relaunch), and closes the
    /// current connection: the old process's token no longer works.
    pub(crate) fn renew_token(&self, token: String) {
        self.sender.with_entry(|entry| {
            let mut expected = (*entry.expected).clone();
            expected.auth = NodeAuth::local(token);
            entry.expected = Arc::new(expected);
            if let Some(conn) = entry.conn.take() {
                conn.close.cancel();
            }
        });
    }
}

impl Drop for NodeLink {
    fn drop(&mut self) {
        let mut nodes = self.sender.inner.nodes();
        let sender = &self.sender;
        if nodes
            .get(&*sender.node_id)
            .is_some_and(|entry| entry.link_id == sender.link_id)
            && let Some(entry) = nodes.remove(&*sender.node_id)
            && let Some(conn) = entry.conn
        {
            conn.close.cancel();
        }
    }
}

impl Inner {
    fn nodes(&self) -> std::sync::MutexGuard<'_, HashMap<String, NodeEntry>> {
        self.nodes.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// How the handshake went.
enum Verdict {
    /// Unknown instance id or wrong token: reply `Shutdown`, tell nobody.
    Refused(String),
    /// Wrong protocol version, type, version or schemas: reply `Shutdown`,
    /// tell the director.
    Mismatch {
        reasons: Vec<String>,
        events: mpsc::Sender<HubEvent>,
        log: bool,
    },
    Accepted {
        expected: Arc<ExpectedNode>,
        events: mpsc::Sender<HubEvent>,
        conn: ConnHandle,
        superseded: bool,
    },
}

/// What the hub enables for a plugin of `plugin_type` that offered `init`.
fn negotiate(config: &HubConfig, plugin_type: PluginType, init: &InitInfo) -> Negotiated {
    let acks = init.capabilities.contains(&Capability::Acks);
    let credits =
        init.capabilities.contains(&Capability::Credits) && plugin_type != PluginType::Sink;
    let consumer = plugin_type != PluginType::Source;
    let max_unacked = if acks && consumer {
        let asked = if init.max_unacked == 0 {
            config.default_max_unacked
        } else {
            init.max_unacked
        };
        asked.clamp(1, config.max_unacked.max(1))
    } else {
        0
    };
    Negotiated {
        acks,
        credits,
        max_unacked,
    }
}

/// Whether a connection's credentials (and transport) prove it's `expected`.
fn credentials_match(expected: &ExpectedNode, init: &InitInfo, transport: &Transport) -> bool {
    match &expected.auth {
        NodeAuth::Token { token, require_tls } => {
            (transport.tls || !require_tls) && tokens_match(token, &init.token)
        }
        NodeAuth::Mtls { pin } => transport.client_cert.as_ref().is_some_and(|cert| match pin {
            Some(pin) => tokens_match(pin, &cert.sha256),
            None => cert.ca_verified && cert.names.contains(&expected.instance_id),
        }),
    }
}

/// Checks `init` and, if it passes, registers the connection as the node's
/// current one.
fn authenticate(
    inner: &Inner,
    init: &InitInfo,
    transport: &Transport,
    out: mpsc::Sender<Vec<u8>>,
) -> Verdict {
    let mut nodes = inner.nodes();
    // Every expected node with this instance id; the credentials pick among
    // them. If several match them too, prefer one that isn't connected yet.
    let mut candidates = 0;
    let mut chosen: Option<(&String, bool)> = None;
    for (node_id, entry) in nodes.iter() {
        if entry.expected.instance_id != init.instance_id {
            continue;
        }
        candidates += 1;
        if !credentials_match(&entry.expected, init, transport) {
            continue;
        }
        let free = entry.conn.is_none();
        if chosen.is_none_or(|(_, chosen_free)| free && !chosen_free) {
            chosen = Some((node_id, free));
        }
    }
    let Some((node_id, _)) = chosen else {
        return Verdict::Refused(if candidates == 0 {
            format!("unknown instance id {:?}", init.instance_id)
        } else {
            format!(
                "wrong credentials for instance [{}] ({})",
                init.instance_id,
                describe(transport)
            )
        });
    };
    let node_id = node_id.clone();
    let entry = nodes.get_mut(&node_id).expect("found above");
    let reasons = check_declaration(&entry.expected, init);
    if !reasons.is_empty() {
        return Verdict::Mismatch {
            reasons,
            events: entry.events.clone(),
            log: entry.reject_log.allow(),
        };
    }
    let plugin_type = entry.expected.plugin_type;
    let negotiated = negotiate(&inner.config, plugin_type, init);
    let credit = negotiated.credits.then(|| {
        let window = u64::from(inner.config.credit_window.max(1));
        Credits {
            window,
            granted: AtomicU64::new(window),
            received: AtomicU64::new(0),
            pending: AtomicU64::new(0),
        }
    });
    let conn = ConnHandle {
        id: inner.next_id(),
        out,
        close: inner.shutdown.child_token(),
        negotiated,
        shared: Arc::new(ConnShared {
            plugin_type,
            next_seq: tokio::sync::Mutex::new(1),
            memory: (plugin_type == PluginType::Transform)
                .then(|| Mutex::new(InputMemory::default())),
            credit,
        }),
    };
    let superseded = match entry.conn.replace(conn.clone()) {
        Some(old) => {
            old.close.cancel();
            true
        }
        None => false,
    };
    Verdict::Accepted {
        expected: entry.expected.clone(),
        events: entry.events.clone(),
        conn,
        superseded,
    }
}

/// Protocol version, type, version and schema checks. Returns every difference.
fn check_declaration(expected: &ExpectedNode, init: &InitInfo) -> Vec<String> {
    let mut reasons = vec![];
    if !(init.min_protocol_version..=init.max_protocol_version).contains(&PROTOCOL_VERSION) {
        reasons.push(format!(
            "no common protocol version: the hub speaks {PROTOCOL_VERSION}..{PROTOCOL_VERSION}, the plugin {}..{}",
            init.min_protocol_version, init.max_protocol_version
        ));
        return reasons;
    }
    let Some(declared) = init.plugin_type else {
        reasons.push("type: UNDEFINED from the plugin".into());
        return reasons;
    };
    if declared != expected.plugin_type {
        reasons.push(format!(
            "type: {} in the registry, {} from the plugin",
            expected.plugin_type.as_str(),
            declared.as_str()
        ));
        return reasons; // the schemas can't be compared meaningfully
    }
    if init.plugin_version != expected.plugin_version {
        reasons.push(format!(
            "version: {} in the registry, {:?} from the plugin",
            expected.plugin_version, init.plugin_version
        ));
    }
    let sides: &[(&str, &Option<SchemaDefinition>, &Option<SchemaDefinition>)] =
        match expected.plugin_type {
            PluginType::Source => &[("out", &expected.out_schema, &init.out_schema)],
            PluginType::Sink => &[("in", &expected.in_schema, &init.in_schema)],
            PluginType::Transform => &[
                ("in", &expected.in_schema, &init.in_schema),
                ("out", &expected.out_schema, &init.out_schema),
            ],
        };
    for (side, manifest, declared) in sides {
        match (manifest, declared) {
            (None, _) => reasons.push(format!("{side}: no schema in the manifest")),
            (Some(_), None) => reasons.push(format!("{side}: no schema from the plugin")),
            (Some(manifest), Some(declared)) => {
                let problems = rules::check_well_formed(declared);
                if problems.is_empty() {
                    reasons.extend(
                        rules::diff(manifest, declared)
                            .into_iter()
                            .map(|d| format!("{side}.{d}")),
                    );
                } else {
                    reasons.extend(problems.into_iter().map(|p| format!("{side}: {p}")));
                }
            }
        }
    }
    reasons
}

/// Compares tokens in time independent of where they differ. An empty
/// expected token never matches.
fn tokens_match(expected: &str, actual: &str) -> bool {
    let (a, b) = (expected.as_bytes(), actual.as_bytes());
    let mut diff = u8::from(a.len() != b.len() || a.is_empty());
    for i in 0..a.len().max(b.len()) {
        diff |= a.get(i).copied().unwrap_or(0) ^ b.get(i).copied().unwrap_or(0);
    }
    diff == 0
}

/// For log lines: how a refused connection came in.
fn describe(transport: &Transport) -> String {
    match (&transport.tls, &transport.client_cert) {
        (false, _) => "plain TCP".into(),
        (true, None) => "TLS, no client certificate".into(),
        (true, Some(cert)) => format!(
            "TLS, client certificate {} for {:?}{}",
            cert.sha256,
            cert.names,
            if cert.ca_verified { "" } else { ", not from the client CA" }
        ),
    }
}

async fn serve_tcp(inner: Arc<Inner>, stream: TcpStream, peer: SocketAddr) {
    let _ = stream.set_nodelay(true);
    let (reader, writer) = stream.into_split();
    serve_connection(
        inner,
        Box::new(reader),
        Box::new(writer),
        peer,
        Transport::default(),
    )
    .await;
}

#[cfg(feature = "remote")]
async fn serve_tls(
    inner: Arc<Inner>,
    tls: Arc<super::tls::TlsListener>,
    stream: TcpStream,
    peer: SocketAddr,
) {
    let _ = stream.set_nodelay(true);
    let timeout = Duration::from_millis(inner.config.handshake_timeout_ms);
    let stream = match tokio::time::timeout(timeout, tls.acceptor.accept(stream)).await {
        Ok(Ok(stream)) => stream,
        Ok(Err(err)) => return stranger(&inner, peer, format!("TLS handshake failed: {err}")),
        Err(_) => return stranger(&inner, peer, "TLS handshake timed out".into()),
    };
    let client_cert = tls.client_cert(stream.get_ref().1.peer_certificates());
    let (reader, writer) = tokio::io::split(stream);
    let transport = Transport {
        tls: true,
        client_cert,
    };
    serve_connection(inner, Box::new(reader), Box::new(writer), peer, transport).await;
}

async fn serve_connection(
    inner: Arc<Inner>,
    mut reader: BoxedReader,
    mut writer: BoxedWriter,
    peer: SocketAddr,
    transport: Transport,
) {
    // --- handshake: the first frame must be an Initialize ---
    let timeout = Duration::from_millis(inner.config.handshake_timeout_ms);
    let first = tokio::select! {
        _ = inner.shutdown.cancelled() => return,
        first = tokio::time::timeout(
            timeout,
            read_frame_limited(&mut reader, inner.config.max_handshake_bytes),
        ) => first,
    };
    let frame = match first {
        Ok(Ok(Some(LimitedFrame::Frame(frame)))) => frame,
        Ok(Ok(Some(LimitedFrame::Oversized { body_len, .. }))) => {
            return stranger(
                &inner,
                peer,
                format!("first frame too large ({body_len} bytes)"),
            );
        }
        Ok(Ok(None)) => return log::debug!("Hub: {peer} closed the connection before Initialize"),
        Ok(Err(err)) => return stranger(&inner, peer, format!("invalid first frame: {err}")),
        Err(_) => {
            return stranger(
                &inner,
                peer,
                format!("no Initialize within {} ms", timeout.as_millis()),
            );
        }
    };
    if frame.code() != MsgCode::Initialize {
        return stranger(
            &inner,
            peer,
            format!("first frame is {:?}, not Initialize", frame.code()),
        );
    }
    let init: InitInfo = match frame.decode::<Initialize>() {
        Ok(init) => init.into(),
        Err(err) => return stranger(&inner, peer, format!("invalid Initialize: {err}")),
    };

    let (out_tx, out_rx) = mpsc::channel(OUTBOUND_CAPACITY);
    let (expected, events, conn, superseded) = match authenticate(&inner, &init, &transport, out_tx)
    {
        Verdict::Refused(reason) => {
            stranger(&inner, peer, reason);
            // The plugin learns only that it's not welcome here.
            return reply_and_close(writer, "unknown instance id or wrong credentials").await;
        }
        Verdict::Mismatch {
            reasons,
            events,
            log,
        } => {
            let message = format!(
                "Hub: rejected plugin instance [{}] from {peer}: {}",
                init.instance_id,
                reasons.join("; ")
            );
            if log {
                log::error!("{message}");
            } else {
                log::debug!("{message}");
            }
            let reason = reasons.join("; ");
            let _ = events.send(HubEvent::Rejected { reasons }).await;
            return reply_and_close(writer, &reason).await;
        }
        Verdict::Accepted {
            expected,
            events,
            conn,
            superseded,
        } => (expected, events, conn, superseded),
    };
    let tag = format!("[{}/{}]", expected.scene_id, expected.node_id);
    if superseded {
        log::warn!("Hub: {tag} new connection from {peer} replaces the previous one");
    }

    // InitializeResponse (and the initial credit) go out before anything the
    // director queues.
    let heartbeat_ms = inner.config.heartbeat_interval_ms;
    let mut accept = encode_message(&InitializeResponse {
        protocol_version: PROTOCOL_VERSION,
        capabilities: conn.negotiated.capabilities(),
        heartbeat_interval_ms: heartbeat_ms,
        max_unacked: conn.negotiated.max_unacked,
    });
    if let Some(credit) = &conn.shared.credit {
        accept.extend(encode_message(&Credit {
            events: u32::try_from(credit.window).unwrap_or(u32::MAX),
        }));
    }
    if let Err(err) = write_and_flush(&mut writer, &accept).await {
        log::warn!("Hub: {tag} couldn't send InitializeResponse: {err}");
        unregister(&inner, &expected.node_id, conn.id);
        return;
    }
    log::info!(
        "Hub: {tag} plugin [{}] {} ({}) connected from {peer} (capabilities {:?})",
        expected.plugin_id,
        expected.plugin_version,
        expected.plugin_type.as_str(),
        conn.negotiated
    );
    let heartbeat = (heartbeat_ms > 0).then(|| Duration::from_millis(u64::from(heartbeat_ms)));
    let write_failed = CancellationToken::new();
    tokio::spawn(write_frames(
        writer,
        out_rx,
        conn.close.clone(),
        write_failed.clone(),
        heartbeat,
    ));
    let _ = events
        .send(HubEvent::Connected {
            conn_id: conn.id,
            peer,
            negotiated: conn.negotiated,
        })
        .await;

    // --- running ---
    let max_event_bytes = expected
        .max_event_bytes
        .map_or(inner.config.max_event_bytes, |n| {
            n.min(inner.config.max_event_bytes)
        });
    let mut session = Running {
        tag: tag.clone(),
        plugin_type: expected.plugin_type,
        conn: conn.clone(),
        events: events.clone(),
        next_seq: 1,
        exited: false,
        max_event_bytes,
    };
    let lost = loop {
        // Only time spent waiting to read counts as silence; time spent
        // waiting for the director (backpressure) doesn't.
        let read = async {
            match heartbeat {
                Some(interval) => {
                    let limit = interval * MISSED_HEARTBEATS;
                    tokio::time::timeout(limit, read_frame_limited(&mut reader, max_event_bytes))
                        .await
                        .map_err(|_| format!("nothing from the plugin for {} ms", limit.as_millis()))
                }
                None => Ok(read_frame_limited(&mut reader, max_event_bytes).await),
            }
        };
        let read = tokio::select! {
            _ = conn.close.cancelled() => break None,
            _ = write_failed.cancelled() => break Some("writing to the plugin failed".to_string()),
            read = read => read,
        };
        let frame = match read {
            Err(silence) => break Some(silence),
            Ok(Ok(Some(LimitedFrame::Frame(frame)))) => Ok(frame),
            Ok(Ok(Some(LimitedFrame::Oversized { header, body_len }))) => Err((header, body_len)),
            Ok(Ok(None)) => break Some("closed by the plugin".into()),
            Ok(Err(err)) => break Some(format!("read error: {err}")),
        };
        let handled = match frame {
            Ok(frame) => session.handle(frame).await,
            Err((header, body_len)) => session.oversized(header, body_len).await,
        };
        match handled {
            Ok(true) => {}
            Ok(false) => break None, // nobody's listening any more
            Err(violation) => {
                log::error!("Hub: {tag} protocol violation: {violation}; closing the connection");
                break Some(format!("protocol violation: {violation}"));
            }
        }
    };

    conn.close.cancel(); // stops the writer
    let was_current = unregister(&inner, &expected.node_id, conn.id);
    if let Some(reason) = lost
        && was_current
    {
        if session.exited {
            log::debug!("Hub: {tag} connection closed after Exit: {reason}");
        } else {
            log::warn!("Hub: {tag} connection lost: {reason}");
        }
        let _ = events
            .send(HubEvent::Disconnected {
                conn_id: conn.id,
                reason,
            })
            .await;
    }
}

/// A connection after the handshake.
struct Running {
    tag: String,
    plugin_type: PluginType,
    conn: ConnHandle,
    events: mpsc::Sender<HubEvent>,
    /// The seq the plugin's next event must have.
    next_seq: u64,
    /// After Exit, the plugin closing the connection is expected.
    exited: bool,
    max_event_bytes: usize,
}

impl Running {
    /// Handles one frame. `Ok(false)` = the director stopped listening;
    /// `Err` = a protocol violation.
    async fn handle(&mut self, frame: Frame) -> Result<bool, String> {
        let code = frame.code();
        if !wire::plugin_may_send(self.plugin_type, code) {
            return Err(format!("{code:?} from a {}", self.plugin_type.as_str()));
        }
        let event = match code {
            MsgCode::Event => {
                let seq = self.data_frame(&frame.header)?;
                let metadata = match (&self.conn.shared.memory, frame.header.input_seq) {
                    (Some(memory), input) if input > 0 => {
                        let found = lock(memory).get(input);
                        if found.is_none() {
                            log::debug!(
                                "Hub: {} output for unknown input {input}: a new trace",
                                self.tag
                            );
                        }
                        found
                    }
                    _ => None,
                };
                let size = frame.body.len();
                HubEvent::Event {
                    conn_id: self.conn.id,
                    seq,
                    event: DataEvent {
                        body: frame.body,
                        metadata,
                        attempt: 0,
                    },
                    size,
                }
            }
            MsgCode::Ack => {
                self.require_acks(code)?;
                let ack: Ack = decode(&frame)?;
                if let Some(memory) = &self.conn.shared.memory {
                    lock(memory).acked(ack.up_to, &ack.seqs);
                }
                HubEvent::Acked {
                    conn_id: self.conn.id,
                    up_to: ack.up_to,
                    seqs: ack.seqs,
                }
            }
            MsgCode::Nack => {
                self.require_acks(code)?;
                let nack: Nack = decode(&frame)?;
                if let Some(memory) = &self.conn.shared.memory {
                    lock(memory).acked(0, &[nack.seq]);
                }
                HubEvent::Nacked {
                    conn_id: self.conn.id,
                    seq: nack.seq,
                    reason: nack.reason,
                    retry: nack.retry,
                }
            }
            MsgCode::Exit => {
                self.exited = true;
                HubEvent::Exit {
                    conn_id: self.conn.id,
                    exit: decode(&frame)?,
                }
            }
            MsgCode::Log => {
                log_plugin_record(&self.tag, decode(&frame)?);
                return Ok(true);
            }
            MsgCode::Error => {
                log_plugin_error(&self.tag, decode(&frame)?);
                return Ok(true);
            }
            MsgCode::Ping => {
                let ping: Ping = decode(&frame)?;
                let _ = self
                    .conn
                    .out
                    .send(encode_message(&Pong { nonce: ping.nonce }))
                    .await;
                return Ok(true);
            }
            MsgCode::Pong => {
                let _: Pong = decode(&frame)?;
                return Ok(true);
            }
            _ => unreachable!("checked by plugin_may_send"),
        };
        Ok(self.events.send(event).await.is_ok())
    }

    /// A data frame larger than the event size limit: its body was skipped.
    async fn oversized(&mut self, header: FrameHeader, body_len: usize) -> Result<bool, String> {
        if header.code() != MsgCode::Event || !wire::plugin_may_send(self.plugin_type, MsgCode::Event)
        {
            return Err(format!(
                "{:?} of {body_len} bytes (limit {})",
                header.code(),
                self.max_event_bytes
            ));
        }
        let seq = self.data_frame(&header)?;
        log::error!(
            "Hub: {} dropped an event of {body_len} bytes (limit {})",
            self.tag,
            self.max_event_bytes
        );
        let reason = format!(
            "event too large: {body_len} bytes, limit {}",
            self.max_event_bytes
        );
        send_routed(&self.conn, seq, Routed::Rejected { reason, retry: false }).await;
        Ok(true)
    }

    /// Checks an EVENT frame's seq and credit. Returns its seq.
    fn data_frame(&mut self, header: &FrameHeader) -> Result<u64, String> {
        if header.seq != self.next_seq {
            return Err(format!(
                "event seq {}, expected {}",
                header.seq, self.next_seq
            ));
        }
        self.next_seq += 1;
        if let Some(credit) = &self.conn.shared.credit {
            let received = credit.received.fetch_add(1, Ordering::AcqRel) + 1;
            if received > credit.granted.load(Ordering::Acquire) {
                return Err("event sent without credit".into());
            }
        }
        Ok(header.seq)
    }

    fn require_acks(&self, code: MsgCode) -> Result<(), String> {
        if self.conn.negotiated.acks {
            Ok(())
        } else {
            Err(format!("{code:?} without the ACKS capability"))
        }
    }
}

/// The ACK / NACK (with ACKS) and the returned credit (with CREDITS) for a
/// producer's event.
async fn send_routed(conn: &ConnHandle, seq: u64, outcome: Routed) {
    if conn.negotiated.acks {
        let frame = match outcome {
            // By seq, not cumulative: answers can finish out of order (a
            // durable target's commit).
            Routed::Accepted => encode_message(&Ack {
                up_to: 0,
                seqs: vec![seq],
            }),
            Routed::Rejected { reason, retry } => encode_message(&Nack { seq, reason, retry }),
        };
        if conn.out.send(frame).await.is_err() {
            return;
        }
    }
    if let Some(credit) = &conn.shared.credit {
        credit.pending.fetch_add(1, Ordering::AcqRel);
        if let Some(events) = credit.take_grant() {
            let _ = conn.out.send(encode_message(&Credit { events })).await;
        }
    }
}

fn decode<M: WireMessage>(frame: &Frame) -> Result<M, String> {
    frame
        .decode::<M>()
        .map_err(|err| format!("undecodable {:?}: {err}", frame.code()))
}

/// Clears the node's connection if it's still `conn_id`. Returns whether it was.
fn unregister(inner: &Inner, node_id: &str, conn_id: u64) -> bool {
    let mut nodes = inner.nodes();
    match nodes.get_mut(node_id) {
        Some(entry) if entry.conn.as_ref().is_some_and(|c| c.id == conn_id) => {
            entry.conn = None;
            true
        }
        _ => false,
    }
}

/// Logs a failed handshake from a connection that isn't (yet) tied to a node.
fn stranger(inner: &Inner, peer: SocketAddr, reason: String) {
    let message = format!("Hub: refused connection from {peer}: {reason}");
    if inner.stranger_log.allow() {
        log::warn!("{message}");
    } else {
        log::debug!("{message}");
    }
}

async fn reply_and_close(mut writer: BoxedWriter, reason: &str) {
    let frame = encode_message(&Shutdown {
        reason: reason.into(),
    });
    let _ = write_and_flush(&mut writer, &frame).await;
    let _ = writer.shutdown().await;
}

async fn write_and_flush(writer: &mut BoxedWriter, frame: &[u8]) -> std::io::Result<()> {
    writer.write_all(frame).await?;
    writer.flush().await
}

/// Writes queued frames, and a PING every heartbeat interval.
async fn write_frames(
    mut writer: BoxedWriter,
    mut frames: mpsc::Receiver<Vec<u8>>,
    close: CancellationToken,
    failed: CancellationToken,
    heartbeat: Option<Duration>,
) {
    let mut ticker = heartbeat.map(|interval| {
        let mut ticker = tokio::time::interval_at(tokio::time::Instant::now() + interval, interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticker
    });
    let mut nonce = 0u64;
    loop {
        let tick = async {
            match ticker.as_mut() {
                Some(ticker) => ticker.tick().await,
                None => std::future::pending().await,
            }
        };
        let frame = tokio::select! {
            _ = close.cancelled() => break,
            frame = frames.recv() => match frame {
                Some(frame) => frame,
                None => break,
            },
            _ = tick => {
                nonce += 1;
                encode_message(&Ping { nonce })
            }
        };
        if write_and_flush(&mut writer, &frame).await.is_err() {
            failed.cancel();
            return;
        }
    }
    let _ = writer.shutdown().await;
}

fn log_plugin_record(tag: &str, event: LogEvent) {
    let details = format_details(&event.details);
    let level = match event.level() {
        LogLevel::Trace => log::Level::Trace,
        LogLevel::Debug => log::Level::Debug,
        LogLevel::Info => log::Level::Info,
        LogLevel::Warn => log::Level::Warn,
        LogLevel::Error => log::Level::Error,
        LogLevel::Undefined => {
            log::warn!("{tag} (log level missing) {}{details}", event.message);
            return;
        }
    };
    log::log!(level, "{tag} {}{details}", event.message);
}

fn log_plugin_error(tag: &str, event: ErrorEvent) {
    log::error!(
        "{tag} plugin error: {}{}",
        event.message,
        format_details(&event.details)
    );
}

/// ` {k=v, …}`, or nothing without details.
fn format_details(fields: &[Field]) -> String {
    if fields.is_empty() {
        return String::new();
    }
    let parts: Vec<String> = fields
        .iter()
        .map(|f| match f.value.as_ref().and_then(|v| v.value.as_ref()) {
            Some(Value::String(s)) => format!("{}={s}", f.key),
            Some(other) => format!("{}={other:?}", f.key),
            None => format!("{}=null", f.key),
        })
        .collect();
    format!(" {{{}}}", parts.join(", "))
}

#[cfg(test)]
mod unit_tests {
    use super::*;

    #[test]
    fn token_comparison() {
        assert!(tokens_match("secret-token", "secret-token"));
        assert!(!tokens_match("secret-token", "secret-tokeX"));
        assert!(!tokens_match("secret-token", "secret-token-longer"));
        assert!(!tokens_match("secret-token", ""));
        assert!(!tokens_match("", ""));
    }

    #[test]
    fn input_memory_keeps_unacked_and_recent_inputs() {
        let meta = |id: &str| EventMetadata {
            event_id: id.into(),
            ..Default::default()
        };
        let mut memory = InputMemory::default();
        for seq in 1..=(INPUT_MEMORY as u64 + 10) {
            memory.insert(seq, meta(&seq.to_string()), true);
        }
        // The oldest fell out of the ring but is still unacked.
        assert_eq!(memory.get(1).unwrap().event_id, "1");
        memory.acked(5, &[]);
        assert!(memory.get(1).is_none());
        assert_eq!(memory.get(11).unwrap().event_id, "11"); // still recent
        assert!(memory.get(INPUT_MEMORY as u64 + 11).is_none());
    }

    #[test]
    fn credit_is_granted_back_in_batches() {
        let credit = Credits {
            window: 8,
            granted: AtomicU64::new(8),
            received: AtomicU64::new(1),
            pending: AtomicU64::new(1),
        };
        // 1 of 8 back, 7 left: wait for more.
        assert_eq!(credit.take_grant(), None);
        credit.pending.store(2, Ordering::Release);
        assert_eq!(credit.take_grant(), Some(2));
        // None left: grant even a single one.
        credit.received.store(10, Ordering::Release);
        credit.pending.store(1, Ordering::Release);
        assert_eq!(credit.take_grant(), Some(1));
    }

    #[test]
    fn negotiation() {
        let config = HubConfig::default();
        let init = |caps: Vec<Capability>, max_unacked| InitInfo {
            instance_id: String::new(),
            plugin_version: String::new(),
            plugin_type: None,
            token: String::new(),
            in_schema: None,
            out_schema: None,
            min_protocol_version: 2,
            max_protocol_version: 2,
            capabilities: caps,
            max_unacked,
        };
        let both = vec![Capability::Acks, Capability::Credits];
        let sink = negotiate(&config, PluginType::Sink, &init(both.clone(), 0));
        assert_eq!((sink.acks, sink.credits, sink.max_unacked), (true, false, 64));
        let source = negotiate(&config, PluginType::Source, &init(both.clone(), 5));
        assert_eq!((source.acks, source.credits, source.max_unacked), (true, true, 0));
        let transform = negotiate(&config, PluginType::Transform, &init(both, 100_000));
        assert_eq!(transform.max_unacked, 1024);
        let plain = negotiate(&config, PluginType::Transform, &init(vec![], 8));
        assert_eq!(plain, Negotiated::default());
    }
}
