//! Reconnectable connection from a plugin to the hub (protocol v2).
//!
//! The writer half is shared (runner, `LocalSink`, logger); the reader half
//! belongs to the runner's current [`Session`]. Every connection goes through
//! three stages, and each kind of message needs a minimum stage:
//!
//! - `Down`: no connection, or one on which `Initialize` hasn't been sent.
//!   Nothing else may be sent (`Initialize` must be the first frame).
//! - `Initializing`: `Initialize` sent. Logs, errors and `Exit` may be sent.
//! - `Ready`: the hub answered with `InitializeResponse`. Data events, acks
//!   and pings may be sent.
//!
//! Sends below their required stage fail fast with `NotConnected`.
//!
//! The reader task handles the protocol's own traffic itself: it answers
//! `PING`, adds `CREDIT`, resolves the hub's `ACK` / `NACK` of emitted events,
//! checks the hub's sequence numbers and the liveness timeout. The runner only
//! sees acceptance, `Shutdown` and incoming data events.

use crate::plugin::frame::{
    Frame, FrameError, WireMessage, encode_frame, encode_message, read_frame,
};
use crate::plugin::core::{InboundEvent, InputRef};
use crate::schema::payload::PluginPayload;
use crate::schema::protocol::{
    Ack, Credit, Event, FrameHeader, InitializeResponse, Nack, Ping, Pong,
    Shutdown, capability::Enum as Capability, msg_code::Enum as MsgCode,
    plugin_type::Enum as PluginType,
};
use prost::Message;
use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{Mutex, Notify, mpsc, oneshot, watch};
use tokio::task::JoinHandle;

pub(crate) type BoxedReader = Box<dyn AsyncRead + Send + Unpin>;
pub(crate) type BoxedWriter = Box<dyn AsyncWrite + Send + Unpin>;

/// A silent hub is given up on after this many heartbeat intervals.
const MISSED_HEARTBEATS: u32 = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Stage {
    Down,
    Initializing,
    Ready,
}

/// What the runner gets from a session.
#[derive(Debug)]
pub(crate) enum Incoming {
    Accepted(InitializeResponse),
    Shutdown(Shutdown),
    Event(InboundEvent),
}

/// The hub's answer to an emitted event.
pub(crate) type Confirmation = Result<(), String>;

struct Conn {
    id: u64,
    writer: BoxedWriter,
    /// Next seq for an outgoing EVENT.
    next_seq: u64,
    /// Remaining credit, if the hub enabled CREDITS on this connection.
    credit: Option<u64>,
    /// The hub enabled ACKS on this connection.
    acks: bool,
    /// Emitted events waiting for the hub's ACK / NACK.
    confirms: HashMap<u64, oneshot::Sender<Confirmation>>,
}

impl Conn {
    async fn write(&mut self, frame: &[u8]) -> io::Result<()> {
        self.writer.write_all(frame).await?;
        self.writer.flush().await
    }
}

pub(crate) struct HubClient {
    addr: String,
    plugin_type: PluginType,
    connector: Connector,
    // Stage changes happen only while this lock is held, so a send that
    // checks the stage under the lock can't write to a newer connection
    // before its `Initialize`.
    conn: Mutex<Option<Conn>>,
    stage: watch::Sender<Stage>,
    /// Credit arrived (or the connection changed).
    credit: Notify,
    next_id: AtomicU64,
    next_nonce: AtomicU64,
}

/// How to open the byte stream to the hub.
#[derive(Clone)]
pub(crate) enum Connector {
    Plain,
    #[cfg(feature = "tls")]
    Tls(Arc<crate::plugin::tls::ClientTls>),
}

impl HubClient {
    pub(crate) fn new(addr: String, plugin_type: PluginType, connector: Connector) -> Arc<Self> {
        Arc::new(Self {
            addr,
            plugin_type,
            connector,
            conn: Mutex::new(None),
            stage: watch::Sender::new(Stage::Down),
            credit: Notify::new(),
            next_id: AtomicU64::new(1),
            next_nonce: AtomicU64::new(1),
        })
    }

    pub(crate) fn addr(&self) -> &str {
        &self.addr
    }

    pub(crate) fn stage(&self) -> Stage {
        *self.stage.borrow()
    }

    /// Opens a new connection (replacing any old one). Its stage is `Down`
    /// until [`HubClient::send_initialize`] succeeds.
    pub(crate) async fn connect(&self) -> io::Result<(u64, BoxedReader)> {
        let stream = TcpStream::connect(&self.addr).await?;
        stream.set_nodelay(true)?;
        let (reader, writer): (BoxedReader, BoxedWriter) = match &self.connector {
            Connector::Plain => {
                let (r, w) = stream.into_split();
                (Box::new(r), Box::new(w))
            }
            #[cfg(feature = "tls")]
            Connector::Tls(tls) => {
                let stream = tls.connect(stream).await?;
                let (r, w) = tokio::io::split(stream);
                (Box::new(r), Box::new(w))
            }
        };
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let mut conn = self.conn.lock().await;
        if let Some(old) = conn.take() {
            fail_confirms(old.confirms);
        }
        *conn = Some(Conn {
            id,
            writer,
            next_seq: 1,
            credit: None,
            acks: false,
            confirms: HashMap::new(),
        });
        self.stage.send_replace(Stage::Down);
        self.credit.notify_waiters();
        Ok((id, reader))
    }

    /// Sends `Initialize` as the first frame of the current connection.
    pub(crate) async fn send_initialize<M: WireMessage>(&self, msg: &M) -> io::Result<()> {
        let mut guard = self.conn.lock().await;
        let conn = guard.as_mut().ok_or_else(not_connected)?;
        match conn.write(&encode_message(msg)).await {
            Ok(()) => {
                self.stage.send_replace(Stage::Initializing);
                Ok(())
            }
            Err(err) => {
                self.drop_conn(&mut guard);
                Err(err)
            }
        }
    }

    /// Sends a system message if the connection has reached `min`. A failed
    /// write drops the connection.
    pub(crate) async fn send<M: WireMessage>(&self, msg: &M, min: Stage) -> io::Result<()> {
        self.send_frame(&encode_message(msg), min, None).await
    }

    async fn send_frame(&self, frame: &[u8], min: Stage, conn_id: Option<u64>) -> io::Result<()> {
        self.send_frame_if(frame, min, conn_id, |_| true).await
    }

    /// Sends `frame` if `wanted` says so for the current connection (e.g.
    /// acks only where the hub enabled them); otherwise does nothing.
    async fn send_frame_if(
        &self,
        frame: &[u8],
        min: Stage,
        conn_id: Option<u64>,
        wanted: impl Fn(&Conn) -> bool,
    ) -> io::Result<()> {
        let mut guard = self.conn.lock().await;
        if self.stage() < min {
            return Err(not_connected());
        }
        let conn = guard.as_mut().ok_or_else(not_connected)?;
        if conn_id.is_some_and(|id| id != conn.id) {
            return Err(not_connected());
        }
        if !wanted(conn) {
            return Ok(());
        }
        let result = conn.write(frame).await;
        if result.is_err() {
            self.drop_conn(&mut guard);
        }
        result
    }

    /// Sends a data event: waits for credit if the hub uses credits, and
    /// returns its seq. With `confirm`, also returns a receiver for the hub's
    /// ACK / NACK (resolved at once if the hub doesn't use acks).
    pub(crate) async fn send_event(
        &self,
        payload: PluginPayload,
        input: Option<InputRef>,
        confirm: bool,
    ) -> io::Result<(u64, Option<oneshot::Receiver<Confirmation>>)> {
        let body = Event {
            payload: Some(payload),
        }
        .encode_to_vec();
        loop {
            let credit = self.credit.notified();
            tokio::pin!(credit);
            credit.as_mut().enable();
            {
                let mut guard = self.conn.lock().await;
                if self.stage() < Stage::Ready {
                    return Err(not_connected());
                }
                let conn = guard.as_mut().ok_or_else(not_connected)?;
                if conn.credit != Some(0) {
                    let seq = conn.next_seq;
                    // seq numbers are per connection: an input from an
                    // earlier connection can't be referred to any more.
                    let input_seq = input.filter(|i| i.conn_id == conn.id).map_or(0, |i| i.seq);
                    let header = FrameHeader {
                        code: MsgCode::Event as i32,
                        seq,
                        input_seq,
                        ..Default::default()
                    };
                    let result = conn.write(&encode_frame(&header, &body)).await;
                    if let Err(err) = result {
                        self.drop_conn(&mut guard);
                        return Err(err);
                    }
                    conn.next_seq += 1;
                    if let Some(credit) = conn.credit.as_mut() {
                        *credit -= 1;
                    }
                    let receiver = confirm.then(|| {
                        let (tx, rx) = oneshot::channel();
                        if conn.acks {
                            conn.confirms.insert(seq, tx);
                        } else {
                            let _ = tx.send(Ok(()));
                        }
                        rx
                    });
                    return Ok((seq, receiver));
                }
            }
            // Out of credit: wait for a grant (or a new connection).
            let mut stage = self.stage.subscribe();
            tokio::select! {
                _ = &mut credit => {}
                _ = stage.wait_for(|s| *s < Stage::Ready) => return Err(not_connected()),
            }
        }
    }

    /// Acknowledges an event received from the hub, if the hub uses acks.
    /// The runners handle events in order, so the ack is cumulative.
    /// ACKs one event by its seq (not cumulatively: a sink may finish
    /// events out of order).
    pub(crate) async fn ack(&self, conn_id: u64, seq: u64) -> io::Result<()> {
        let frame = encode_message(&Ack {
            up_to: 0,
            seqs: vec![seq],
        });
        self.send_frame_if(&frame, Stage::Ready, Some(conn_id), |c| c.acks)
            .await
    }

    pub(crate) async fn nack(
        &self,
        conn_id: u64,
        seq: u64,
        reason: String,
        retry: bool,
    ) -> io::Result<()> {
        let frame = encode_message(&Nack { seq, reason, retry });
        self.send_frame_if(&frame, Stage::Ready, Some(conn_id), |c| c.acks)
            .await
    }

    /// Marks connection `id` as accepted by the hub, with what it negotiated.
    pub(crate) async fn set_ready(&self, id: u64, response: &InitializeResponse) {
        let mut guard = self.conn.lock().await;
        if let Some(conn) = guard.as_mut().filter(|c| c.id == id)
            && self.stage() == Stage::Initializing
        {
            let has = |cap: Capability| response.capabilities.contains(&(cap as i32));
            conn.credit = has(Capability::Credits).then_some(0);
            conn.acks = has(Capability::Acks);
            self.stage.send_replace(Stage::Ready);
        }
    }

    async fn add_credit(&self, id: u64, events: u32) {
        let mut guard = self.conn.lock().await;
        if let Some(conn) = guard.as_mut().filter(|c| c.id == id)
            && let Some(credit) = conn.credit.as_mut()
        {
            *credit = credit.saturating_add(u64::from(events));
            drop(guard);
            self.credit.notify_waiters();
        }
    }

    /// The hub's ACK / NACK of emitted events.
    async fn confirm(&self, id: u64, acked: impl Fn(u64) -> bool, nack: Option<&Nack>) {
        let mut guard = self.conn.lock().await;
        let Some(conn) = guard.as_mut().filter(|c| c.id == id) else {
            return;
        };
        let done: Vec<u64> = conn.confirms.keys().copied().filter(|s| acked(*s)).collect();
        for seq in done {
            if let Some(tx) = conn.confirms.remove(&seq) {
                let _ = tx.send(Ok(()));
            }
        }
        if let Some(nack) = nack
            && let Some(tx) = conn.confirms.remove(&nack.seq)
        {
            let _ = tx.send(Err(nack.reason.clone()));
        }
    }

    /// Drops connection `id` (or whatever connection is open, for `None`).
    pub(crate) async fn disconnect(&self, id: Option<u64>) {
        let mut guard = self.conn.lock().await;
        if guard
            .as_ref()
            .is_some_and(|c| id.is_none_or(|id| c.id == id))
        {
            self.drop_conn(&mut guard);
        }
    }

    fn drop_conn(&self, guard: &mut Option<Conn>) {
        if let Some(conn) = guard.take() {
            fail_confirms(conn.confirms);
        }
        self.stage.send_replace(Stage::Down);
        self.credit.notify_waiters();
    }

    /// Waits until the stage is at least `min`.
    pub(crate) async fn wait_for(&self, min: Stage) {
        let mut rx = self.stage.subscribe();
        let _ = rx.wait_for(|stage| *stage >= min).await;
    }

    pub(crate) fn subscribe(&self) -> watch::Receiver<Stage> {
        self.stage.subscribe()
    }

    /// Sends a `PING` (e.g. to test the connection).
    #[allow(dead_code)]
    pub(crate) async fn ping(&self) -> io::Result<()> {
        let nonce = self.next_nonce.fetch_add(1, Ordering::Relaxed);
        self.send(&Ping { nonce }, Stage::Ready).await
    }
}

fn fail_confirms(confirms: HashMap<u64, oneshot::Sender<Confirmation>>) {
    for (_, tx) in confirms {
        let _ = tx.send(Err("the connection to the hub was lost".into()));
    }
}

fn not_connected() -> io::Error {
    io::Error::new(io::ErrorKind::NotConnected, "not connected to the hub")
}

/// One connection's receiving side. Frames are read on their own task, so
/// waiting for the next one is cancel-safe (a half-read frame is never lost).
/// Dropping the session stops the reader.
pub(crate) struct Session {
    pub(crate) id: u64,
    rx: mpsc::Receiver<Incoming>,
    stage: watch::Receiver<Stage>,
    reader: JoinHandle<()>,
    pub(crate) accepted: bool,
}

impl Session {
    pub(crate) fn new(client: &Arc<HubClient>, id: u64, reader: BoxedReader) -> Self {
        let (tx, rx) = mpsc::channel(16);
        Self {
            id,
            rx,
            stage: client.subscribe(),
            reader: tokio::spawn(read_messages(client.clone(), id, reader, tx)),
            accepted: false,
        }
    }

    /// The next message from the hub, or `None` once the connection is gone
    /// (closed by the hub, a read error, a protocol violation, a silent hub,
    /// or a failed write on our side).
    pub(crate) async fn next(&mut self) -> Option<Incoming> {
        tokio::select! {
            msg = self.rx.recv() => msg,
            _ = self.stage.wait_for(|stage| *stage == Stage::Down) => None,
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

/// What the hub may send to this plugin type (after the handshake).
fn allowed(plugin_type: PluginType, code: MsgCode) -> bool {
    match code {
        MsgCode::InitializeResponse | MsgCode::Shutdown | MsgCode::Ping | MsgCode::Pong => true,
        MsgCode::Event => plugin_type != PluginType::Source,
        MsgCode::Credit | MsgCode::Ack | MsgCode::Nack => plugin_type != PluginType::Sink,
        _ => false,
    }
}

async fn read_messages(
    client: Arc<HubClient>,
    conn_id: u64,
    mut reader: BoxedReader,
    tx: mpsc::Sender<Incoming>,
) {
    let end = read_loop(&client, conn_id, &mut reader, &tx).await;
    match end {
        Ok(()) => log::warn!("The hub closed the connection"),
        Err(reason) => log::error!("Connection to the hub: {reason}; closing it"),
    }
    client.disconnect(Some(conn_id)).await;
}

async fn read_loop(
    client: &Arc<HubClient>,
    conn_id: u64,
    reader: &mut BoxedReader,
    tx: &mpsc::Sender<Incoming>,
) -> Result<(), String> {
    let mut heartbeat: Option<Duration> = None;
    let mut next_seq = 1u64;
    let mut accepted = false;
    loop {
        // Only time spent waiting to read counts as silence; time spent
        // waiting for the runner (backpressure) doesn't.
        let read = match heartbeat {
            Some(interval) => {
                let limit = interval * MISSED_HEARTBEATS;
                match tokio::time::timeout(limit, read_frame(reader)).await {
                    Ok(read) => read,
                    Err(_) => {
                        return Err(format!(
                            "nothing from the hub for {} ms",
                            limit.as_millis()
                        ));
                    }
                }
            }
            None => read_frame(reader).await,
        };
        let frame = match read {
            Ok(Some(frame)) => frame,
            Ok(None) => return Ok(()),
            Err(err) => return Err(format!("read error: {err}")),
        };
        let code = frame.code();
        if !allowed(client.plugin_type, code) {
            return Err(format!("protocol violation by the hub: unexpected {code:?}"));
        }
        if !accepted && !matches!(code, MsgCode::InitializeResponse | MsgCode::Shutdown) {
            return Err(format!(
                "protocol violation by the hub: {code:?} before InitializeResponse"
            ));
        }
        let incoming = match code {
            MsgCode::InitializeResponse => {
                let response: InitializeResponse = decode(&frame)?;
                accepted = true;
                heartbeat = (response.heartbeat_interval_ms > 0)
                    .then(|| Duration::from_millis(u64::from(response.heartbeat_interval_ms)));
                client.set_ready(conn_id, &response).await;
                Some(Incoming::Accepted(response))
            }
            MsgCode::Shutdown => Some(Incoming::Shutdown(decode(&frame)?)),
            MsgCode::Ping => {
                let ping: Ping = decode(&frame)?;
                let pong = encode_message(&Pong { nonce: ping.nonce });
                if client
                    .send_frame(&pong, Stage::Initializing, Some(conn_id))
                    .await
                    .is_err()
                {
                    return Err("couldn't answer a PING".into());
                }
                None
            }
            MsgCode::Pong => None,
            MsgCode::Credit => {
                let credit: Credit = decode(&frame)?;
                client.add_credit(conn_id, credit.events).await;
                None
            }
            MsgCode::Ack => {
                let ack: Ack = decode(&frame)?;
                client
                    .confirm(conn_id, |s| s <= ack.up_to || ack.seqs.contains(&s), None)
                    .await;
                None
            }
            MsgCode::Nack => {
                let nack: Nack = decode(&frame)?;
                client.confirm(conn_id, |_| false, Some(&nack)).await;
                None
            }
            MsgCode::Event => {
                if frame.header.seq != next_seq {
                    return Err(format!(
                        "protocol violation by the hub: event seq {}, expected {next_seq}",
                        frame.header.seq
                    ));
                }
                next_seq += 1;
                let event: Event = decode(&frame)?;
                Some(Incoming::Event(InboundEvent {
                    seq: frame.header.seq,
                    attempt: frame.header.attempt.max(1),
                    metadata: frame.header.metadata.clone(),
                    payload: event.payload.unwrap_or_default(),
                    conn_id,
                }))
            }
            _ => unreachable!("checked by allowed()"),
        };
        if let Some(incoming) = incoming
            && tx.send(incoming).await.is_err()
        {
            return Ok(()); // the session is gone
        }
    }
}

fn decode<M: WireMessage>(frame: &Frame) -> Result<M, String> {
    frame.decode::<M>().map_err(|err| match err {
        FrameError::Decode(e) => {
            format!("protocol violation by the hub: undecodable {:?}: {e}", frame.code())
        }
        other => other.to_string(),
    })
}
