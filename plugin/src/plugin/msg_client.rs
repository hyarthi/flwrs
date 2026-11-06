//! Reconnectable connection from a plugin to the hub.
//!
//! The writer half is shared (runner, `LocalSink`, logger); the reader half
//! belongs to the runner's current session. Every connection goes through
//! three stages, and each kind of message needs a minimum stage:
//!
//! - `Down`: no connection, or one on which `Initialize` hasn't been sent.
//!   Nothing else may be sent (`Initialize` must be the first frame).
//! - `Initializing`: `Initialize` sent. System messages (logs, errors, `Exit`)
//!   may be sent.
//! - `Ready`: the hub answered with `InitializeResponse`. Data events may be
//!   sent.
//!
//! Sends below their required stage fail fast with `NotConnected`.

use crate::plugin::frame::{FrameError, WireMessage, read_frame, write_message};
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::{Mutex, mpsc, watch};
use tokio::task::JoinHandle;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Stage {
    Down,
    Initializing,
    Ready,
}

struct Conn {
    id: u64,
    writer: OwnedWriteHalf,
}

pub(crate) struct HubClient {
    addr: String,
    // Stage changes happen only while this lock is held, so a send that
    // checks the stage under the lock can't write to a newer connection
    // before its `Initialize`.
    conn: Mutex<Option<Conn>>,
    stage: watch::Sender<Stage>,
    next_id: AtomicU64,
}

impl HubClient {
    pub(crate) fn new(addr: String) -> Arc<Self> {
        Arc::new(Self {
            addr,
            conn: Mutex::new(None),
            stage: watch::Sender::new(Stage::Down),
            next_id: AtomicU64::new(1),
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
    pub(crate) async fn connect(&self) -> io::Result<(u64, OwnedReadHalf)> {
        let stream = TcpStream::connect(&self.addr).await?;
        stream.set_nodelay(true)?;
        let (reader, writer) = stream.into_split();
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let mut conn = self.conn.lock().await;
        *conn = Some(Conn { id, writer });
        self.stage.send_replace(Stage::Down);
        Ok((id, reader))
    }

    /// Sends `Initialize` as the first frame of the current connection.
    pub(crate) async fn send_initialize<M: WireMessage>(&self, msg: &M) -> io::Result<()> {
        let mut guard = self.conn.lock().await;
        let conn = guard.as_mut().ok_or_else(not_connected)?;
        match write_message(&mut conn.writer, msg).await {
            Ok(()) => {
                self.stage.send_replace(Stage::Initializing);
                Ok(())
            }
            Err(err) => {
                *guard = None;
                self.stage.send_replace(Stage::Down);
                Err(err)
            }
        }
    }

    /// Sends a message if the connection has reached `min`. A failed write
    /// drops the connection.
    pub(crate) async fn send<M: WireMessage>(&self, msg: &M, min: Stage) -> io::Result<()> {
        let mut guard = self.conn.lock().await;
        if self.stage() < min {
            return Err(not_connected());
        }
        let conn = guard.as_mut().ok_or_else(not_connected)?;
        let result = write_message(&mut conn.writer, msg).await;
        if result.is_err() {
            *guard = None;
            self.stage.send_replace(Stage::Down);
        }
        result
    }

    /// Marks connection `id` as accepted by the hub.
    pub(crate) async fn set_ready(&self, id: u64) {
        let guard = self.conn.lock().await;
        if guard.as_ref().is_some_and(|c| c.id == id) && self.stage() == Stage::Initializing {
            self.stage.send_replace(Stage::Ready);
        }
    }

    /// Drops connection `id` (or whatever connection is open, for `None`).
    pub(crate) async fn disconnect(&self, id: Option<u64>) {
        let mut guard = self.conn.lock().await;
        if guard
            .as_ref()
            .is_some_and(|c| id.is_none_or(|id| c.id == id))
        {
            *guard = None;
            self.stage.send_replace(Stage::Down);
        }
    }

    /// Waits until the stage is at least `min`.
    pub(crate) async fn wait_for(&self, min: Stage) {
        let mut rx = self.stage.subscribe();
        let _ = rx.wait_for(|stage| *stage >= min).await;
    }

    pub(crate) fn subscribe(&self) -> watch::Receiver<Stage> {
        self.stage.subscribe()
    }
}

fn not_connected() -> io::Error {
    io::Error::new(io::ErrorKind::NotConnected, "not connected to the hub")
}

/// One connection's receiving side. Messages are read on their own task, so
/// waiting for the next one is cancel-safe (a half-read frame is never lost).
/// Dropping the session stops the reader.
pub(crate) struct Session<M> {
    pub(crate) id: u64,
    rx: mpsc::Receiver<M>,
    stage: watch::Receiver<Stage>,
    reader: JoinHandle<()>,
    pub(crate) accepted: bool,
}

impl<M: WireMessage + Send + 'static> Session<M> {
    pub(crate) fn new(client: &HubClient, id: u64, reader: OwnedReadHalf) -> Self {
        let (tx, rx) = mpsc::channel(16);
        Self {
            id,
            rx,
            stage: client.subscribe(),
            reader: tokio::spawn(read_messages(reader, tx)),
            accepted: false,
        }
    }

    /// The next message from the hub, or `None` once the connection is gone
    /// (closed by the hub, a read error, a protocol violation, or a failed
    /// write on our side).
    pub(crate) async fn next(&mut self) -> Option<M> {
        tokio::select! {
            msg = self.rx.recv() => msg,
            _ = self.stage.wait_for(|stage| *stage == Stage::Down) => None,
        }
    }
}

impl<M> Drop for Session<M> {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

async fn read_messages<M: WireMessage>(mut reader: OwnedReadHalf, tx: mpsc::Sender<M>) {
    loop {
        let frame = match read_frame(&mut reader).await {
            Ok(Some(frame)) => frame,
            Ok(None) => {
                log::warn!("The hub closed the connection");
                return;
            }
            Err(err) => {
                log::error!("Error reading from the hub: {err}");
                return;
            }
        };
        match frame.decode::<M>() {
            Ok(msg) => {
                if tx.send(msg).await.is_err() {
                    return;
                }
            }
            Err(err @ FrameError::UnexpectedCode { .. }) => {
                log::error!("Protocol violation by the hub: {err}; closing the connection");
                return;
            }
            Err(err) => log::error!("Skipping a message from the hub: {err}"),
        }
    }
}
