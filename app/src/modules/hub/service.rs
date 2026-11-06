//! The hub: the TCP endpoint plugins connect to.
//!
//! The director tells the hub which plugin instances (nodes) to expect with
//! [`Hub::expect`], and gets a [`NodeLink`] back: a stream of [`HubEvent`]s
//! from that node's plugin, and a way to send it messages. For each incoming
//! connection the hub runs the handshake (instance id, token, type, version
//! and payload schemas), then one reader and one writer task. The newest
//! accepted connection for a node wins.

use super::config::HubConfig;
use super::wire::{self, DataEvent, InitInfo, PluginMsg, RuntimeMsg};
use crate::modules::plugin::manifest::PluginType;
use crate::modules::rate_limit::RateLimit;
use flwrs_plugin::plugin::frame::{LimitedFrame, read_frame_limited};
use flwrs_plugin::schema::common::{
    ErrorEvent, Exit, LogEvent, log_level::Enum as LogLevel, msg_code::Enum as MsgCode,
};
use flwrs_plugin::schema::rules;
use flwrs_plugin::schema::schema::{Field, SchemaDefinition, field_value::Value};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::net::tcp::OwnedWriteHalf;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Frames queued for one connection's writer.
const OUTBOUND_CAPACITY: usize = 64;
/// Events queued from one node's plugin to the director. When full, the
/// reader stops reading the socket: TCP backpressure on the plugin.
const EVENTS_CAPACITY: usize = 64;
/// At most one WARN/ERROR per this interval for repeated handshake failures.
const REJECT_LOG_INTERVAL: Duration = Duration::from_secs(60);

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
    /// Per-launch token (local) or pre-shared token (remote). Never empty.
    pub token: String,
    /// The node's own event size limit (`runtime.queue.max_event_bytes`); the
    /// hub applies the lower of this and its own.
    pub max_event_bytes: Option<usize>,
}

/// What the hub reports about a node's plugin.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum HubEvent {
    /// The plugin passed the handshake and got `InitializeResponse`.
    Connected { conn_id: u64, peer: SocketAddr },
    /// The plugin authenticated, but declared the wrong type, version or
    /// schemas. It got `Shutdown`. Retrying the same executable won't help.
    Rejected { reasons: Vec<String> },
    /// A data event (sources and transforms). `size` is the encoded body size.
    Event { event: DataEvent, size: usize },
    /// The plugin sent `Exit`.
    Exit { conn_id: u64, exit: Exit },
    /// The current connection was lost (closed by the plugin, an I/O error or
    /// a protocol violation). Not sent when the hub itself closed it.
    Disconnected { conn_id: u64, reason: String },
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub(crate) enum HubError {
    #[error("the plugin isn't connected")]
    NotConnected,
    #[error("can't send this message: {0}")]
    Invalid(String),
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
}

impl Hub {
    /// Binds the listener. Call [`Hub::run`] (or register the hub as a
    /// service) to start accepting.
    pub(crate) async fn bind(config: HubConfig) -> std::io::Result<Self> {
        let listener = TcpListener::bind((config.host.as_str(), config.port)).await?;
        let local_addr = listener.local_addr()?;
        Ok(Self {
            inner: Arc::new(Inner {
                config,
                local_addr,
                listener: Mutex::new(Some(listener)),
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

    /// Starts expecting `node`. Replaces an earlier expectation for the same
    /// node id (its link stops receiving events and its connection is closed).
    pub(crate) fn expect(&self, node: ExpectedNode) -> NodeLink {
        let (tx, rx) = mpsc::channel(EVENTS_CAPACITY);
        let link_id = self.inner.next_id();
        let node_id = node.node_id.clone();
        let plugin_type = node.plugin_type;
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
                plugin_type,
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
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => break,
                accepted = listener.accept() => match accepted {
                    Ok((stream, peer)) => {
                        tokio::spawn(serve_connection(self.inner.clone(), stream, peer));
                    }
                    // E.g. out of file descriptors: keep going after a pause.
                    Err(err) => {
                        log::error!("Hub: accept failed: {err}");
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
    plugin_type: PluginType,
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

    /// Sends a message to the plugin's current connection. Waits while the
    /// connection's outbound queue is full.
    pub(crate) async fn send(&self, msg: RuntimeMsg) -> Result<(), HubError> {
        let frame = wire::encode(self.plugin_type, msg).map_err(HubError::Invalid)?;
        let conn = self.current_conn().ok_or(HubError::NotConnected)?;
        conn.out
            .send(frame)
            .await
            .map_err(|_| HubError::NotConnected)
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

    pub(crate) async fn send(&self, msg: RuntimeMsg) -> Result<(), HubError> {
        self.sender.send(msg).await
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
            expected.token = token;
            entry.expected = Arc::new(expected);
            if let Some(conn) = entry.conn.take() {
                conn.close.cancel();
            }
        });
    }
}

impl Drop for NodeLink {
    fn drop(&mut self) {
        let mut nodes = self.inner().nodes();
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

impl NodeLink {
    fn inner(&self) -> &Inner {
        &self.sender.inner
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

/// How the handshake went.
enum Verdict {
    /// Unknown instance id or wrong token: reply `Shutdown`, tell nobody.
    Refused(String),
    /// Wrong type, version or schemas: reply `Shutdown`, tell the director.
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

/// Checks `init` (sent by a plugin of `sender`) and, if it passes, registers
/// the connection as the node's current one.
fn authenticate(
    inner: &Inner,
    init: &InitInfo,
    sender: PluginType,
    out: mpsc::Sender<Vec<u8>>,
) -> Verdict {
    let mut nodes = inner.nodes();
    // Every expected node with this instance id; the token picks among them.
    // If several match it too, prefer one that isn't connected yet.
    let mut candidates = 0;
    let mut chosen: Option<(&String, bool)> = None;
    for (node_id, entry) in nodes.iter() {
        if entry.expected.instance_id != init.instance_id {
            continue;
        }
        candidates += 1;
        if !tokens_match(&entry.expected.token, &init.token) {
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
            format!("wrong token for instance [{}]", init.instance_id)
        });
    };
    let node_id = node_id.clone();
    let entry = nodes.get_mut(&node_id).expect("found above");
    let reasons = check_declaration(&entry.expected, init, sender);
    if !reasons.is_empty() {
        return Verdict::Mismatch {
            reasons,
            events: entry.events.clone(),
            log: entry.reject_log.allow(),
        };
    }
    let conn = ConnHandle {
        id: inner.next_id(),
        out,
        close: inner.shutdown.child_token(),
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

/// Type, version and schema checks. Returns every difference.
fn check_declaration(expected: &ExpectedNode, init: &InitInfo, sender: PluginType) -> Vec<String> {
    let mut reasons = vec![];
    if sender != expected.plugin_type {
        reasons.push(format!(
            "type: {} in the registry, {} from the plugin",
            expected.plugin_type.as_str(),
            sender.as_str()
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

async fn serve_connection(inner: Arc<Inner>, stream: TcpStream, peer: SocketAddr) {
    let _ = stream.set_nodelay(true);
    let (mut reader, mut writer) = stream.into_split();

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
    let Some(sender) = wire::sender_type(frame.code) else {
        return stranger(
            &inner,
            peer,
            format!(
                "first frame has Msg-Code {:?}, not a plugin message",
                frame.code
            ),
        );
    };
    let init = match wire::decode(sender, frame) {
        Ok(PluginMsg::Initialize(init)) => init,
        Ok(_) => return stranger(&inner, peer, "first frame isn't Initialize".into()),
        Err(err) => return stranger(&inner, peer, format!("invalid Initialize: {err}")),
    };

    let (out_tx, out_rx) = mpsc::channel(OUTBOUND_CAPACITY);
    let (expected, events, conn, superseded) = match authenticate(&inner, &init, sender, out_tx) {
        Verdict::Refused(reason) => {
            stranger(&inner, peer, reason);
            return reply_and_close(writer, sender, RuntimeMsg::Shutdown).await;
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
            let _ = events.send(HubEvent::Rejected { reasons }).await;
            return reply_and_close(writer, sender, RuntimeMsg::Shutdown).await;
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

    // InitializeResponse goes out before anything the director queues.
    let accept = wire::encode(sender, RuntimeMsg::Accept).expect("accept encodes for every type");
    if let Err(err) = write_and_flush(&mut writer, &accept).await {
        log::warn!("Hub: {tag} couldn't send InitializeResponse: {err}");
        unregister(&inner, &expected.node_id, conn.id);
        return;
    }
    log::info!(
        "Hub: {tag} plugin [{}] {} ({}) connected from {peer}",
        expected.plugin_id,
        expected.plugin_version,
        expected.plugin_type.as_str()
    );
    let write_failed = CancellationToken::new();
    tokio::spawn(write_frames(
        writer,
        out_rx,
        conn.close.clone(),
        write_failed.clone(),
    ));
    let _ = events
        .send(HubEvent::Connected {
            conn_id: conn.id,
            peer,
        })
        .await;

    // --- running ---
    let max_event_bytes = expected
        .max_event_bytes
        .map_or(inner.config.max_event_bytes, |n| {
            n.min(inner.config.max_event_bytes)
        });
    let code = wire::plugin_code(expected.plugin_type);
    // After Exit, the plugin closing the connection is expected.
    let mut exited = false;
    let lost = loop {
        let read = tokio::select! {
            _ = conn.close.cancelled() => break None,
            _ = write_failed.cancelled() => break Some("writing to the plugin failed".to_string()),
            read = read_frame_limited(&mut reader, max_event_bytes) => read,
        };
        let frame = match read {
            Ok(Some(LimitedFrame::Frame(frame))) => frame,
            Ok(Some(LimitedFrame::Oversized { code: c, body_len })) if c == code => {
                log::error!(
                    "Hub: {tag} dropped a message of {body_len} bytes (limit {max_event_bytes})"
                );
                continue;
            }
            Ok(Some(LimitedFrame::Oversized { code: c, .. })) => {
                break Some(violation(&tag, c, code));
            }
            Ok(None) => break Some("closed by the plugin".into()),
            Err(err) => break Some(format!("read error: {err}")),
        };
        if frame.code != code {
            break Some(violation(&tag, frame.code, code));
        }
        let size = frame.body.len();
        match wire::decode(expected.plugin_type, frame) {
            Err(err) => log::error!("Hub: {tag} skipped an undecodable message: {err}"),
            Ok(PluginMsg::Event(event)) => {
                if events.send(HubEvent::Event { event, size }).await.is_err() {
                    break None; // nobody's listening any more
                }
            }
            Ok(PluginMsg::Exit(exit)) => {
                exited = true;
                let _ = events
                    .send(HubEvent::Exit {
                        conn_id: conn.id,
                        exit,
                    })
                    .await;
            }
            Ok(PluginMsg::Log(log)) => log_plugin_record(&tag, log),
            Ok(PluginMsg::Error(error)) => log_plugin_error(&tag, error),
            Ok(PluginMsg::Initialize(_)) => {
                log::warn!("Hub: {tag} ignored a repeated Initialize on the same connection");
            }
            Ok(PluginMsg::Empty) => log::debug!("Hub: {tag} ignored a message without payload"),
        }
    };

    conn.close.cancel(); // stops the writer
    let was_current = unregister(&inner, &expected.node_id, conn.id);
    if let Some(reason) = lost
        && was_current
    {
        if exited {
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

fn violation(tag: &str, actual: MsgCode, expected: MsgCode) -> String {
    let reason = format!("protocol violation: Msg-Code {actual:?}, expected {expected:?}");
    log::error!("Hub: {tag} {reason}; closing the connection");
    reason
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

async fn reply_and_close(mut writer: OwnedWriteHalf, sender: PluginType, msg: RuntimeMsg) {
    if let Ok(frame) = wire::encode(sender, msg) {
        let _ = write_and_flush(&mut writer, &frame).await;
    }
    let _ = writer.shutdown().await;
}

async fn write_and_flush(writer: &mut OwnedWriteHalf, frame: &[u8]) -> std::io::Result<()> {
    writer.write_all(frame).await?;
    writer.flush().await
}

async fn write_frames(
    mut writer: OwnedWriteHalf,
    mut frames: mpsc::Receiver<Vec<u8>>,
    close: CancellationToken,
    failed: CancellationToken,
) {
    loop {
        let frame = tokio::select! {
            _ = close.cancelled() => break,
            frame = frames.recv() => match frame {
                Some(frame) => frame,
                None => break,
            },
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
    let level = match event.log_level() {
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
}
