//! The fake hub: starts the plugin, accepts its connections and speaks the
//! hub's side of the protocol. Shared by `audit`, `test` and `bench`.

use crate::target::Target;
use flwrs_plugin::plugin::frame::{
    Frame, FrameError, PROTOCOL_VERSION, WireMessage, encode_frame, encode_message, read_frame,
};
use flwrs_plugin::schema::payload::PluginPayload;
use flwrs_plugin::schema::protocol::{
    Event, EventMetadata, FrameHeader, Initialize, InitializeResponse,
    capability::Enum as Capability, msg_code::Enum as MsgCode,
};
use prost::Message;
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::process::{Child, Command};

/// The instance id the plugin is started with.
pub const INSTANCE_ID: &str = "trellis-node";

/// The fake hub with the plugin running against it.
pub(crate) struct Fixture {
    pub target: Target,
    pub listener: TcpListener,
    /// `None` for an in-process peer (bench calibration).
    pub child: Option<Child>,
    pub token: String,
    /// When the current process was started.
    pub spawned: Instant,
}

impl Fixture {
    pub async fn start(target: &Target) -> Result<Self, String> {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|e| format!("fake hub: {e}"))?;
        let token = random_token();
        let child = spawn(target, &listener, &token)?;
        Ok(Self {
            target: target.clone(),
            listener,
            child: Some(child),
            token,
            spawned: Instant::now(),
        })
    }

    /// The fake hub alone: something in this process connects to [`Self::port`].
    pub async fn without_process(target: &Target) -> Result<Self, String> {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|e| format!("fake hub: {e}"))?;
        Ok(Self {
            target: target.clone(),
            listener,
            child: None,
            token: String::new(),
            spawned: Instant::now(),
        })
    }

    pub fn port(&self) -> u16 {
        self.listener.local_addr().map(|a| a.port()).unwrap_or(0)
    }

    /// Kills the plugin and starts it again (same hub, same token).
    pub async fn restart(&mut self) -> Result<(), String> {
        if let Some(child) = &mut self.child {
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
        self.child = Some(spawn(&self.target, &self.listener, &self.token)?);
        self.spawned = Instant::now();
        Ok(())
    }

    pub fn pid(&self) -> Option<u32> {
        self.child.as_ref().and_then(Child::id)
    }

    /// The plugin's next connection.
    pub async fn accept(&self, wait: Duration) -> Result<Conn, String> {
        match tokio::time::timeout(wait, self.listener.accept()).await {
            Ok(Ok((stream, _))) => {
                let _ = stream.set_nodelay(true);
                let (reader, writer) = stream.into_split();
                Ok(Conn {
                    reader: ConnReader { stream: reader },
                    writer: ConnWriter {
                        stream: writer,
                        next_seq: 1,
                    },
                })
            }
            Ok(Err(e)) => Err(format!("accept: {e}")),
            Err(_) => Err(format!(
                "the plugin didn't connect within {} s",
                wait.as_secs()
            )),
        }
    }

    /// Accepts a connection and reads its Initialize (the first frame).
    pub async fn handshake(&self) -> Result<(Conn, Initialize), String> {
        let mut conn = self.accept(Duration::from_secs(10)).await?;
        let first = conn
            .reader
            .frame(Duration::from_secs(10))
            .await?
            .ok_or("the plugin closed the connection before Initialize")?;
        if first.code() != MsgCode::Initialize {
            return Err(format!(
                "the first frame is {:?}, not Initialize",
                first.code()
            ));
        }
        let init = decode::<Initialize>(&first)?;
        Ok((conn, init))
    }

    /// Handshake, then accept it with every capability it offered.
    pub async fn accepted(&self) -> Result<(Conn, Initialize, Negotiated), String> {
        let (mut conn, init) = self.handshake().await?;
        let has = |c: Capability| init.capabilities.contains(&(c as i32));
        let negotiated = Negotiated {
            acks: has(Capability::Acks),
            credits: has(Capability::Credits) && self.target.produces(),
            max_unacked: if init.max_unacked == 0 {
                64
            } else {
                init.max_unacked.min(1024)
            },
        };
        let mut caps = vec![];
        if negotiated.acks {
            caps.push(Capability::Acks as i32);
        }
        if negotiated.credits {
            caps.push(Capability::Credits as i32);
        }
        conn.writer
            .send(&InitializeResponse {
                protocol_version: PROTOCOL_VERSION,
                capabilities: caps,
                heartbeat_interval_ms: 0,
                max_unacked: if negotiated.acks && self.target.consumes() {
                    negotiated.max_unacked
                } else {
                    0
                },
            })
            .await?;
        Ok((conn, init, negotiated))
    }

    pub async fn exits(&mut self, wait: Duration) -> bool {
        match &mut self.child {
            Some(child) => tokio::time::timeout(wait, child.wait()).await.is_ok(),
            None => true,
        }
    }

    pub async fn stop(mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
    }
}

fn spawn(target: &Target, listener: &TcpListener, token: &str) -> Result<Child, String> {
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();
    let output = || {
        if target.show_output {
            Stdio::inherit()
        } else {
            Stdio::null()
        }
    };
    Command::new(&target.program)
        .args([
            "--id",
            INSTANCE_ID,
            "--host",
            "127.0.0.1",
            "--port",
            &port.to_string(),
            "--log-level",
            "debug",
        ])
        .args(target.argv())
        .current_dir(&target.cwd)
        .env("FLWRS_TOKEN", token)
        .envs(target.env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::null())
        .stdout(output())
        .stderr(output())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("couldn't start {}: {e}", target.program.display()))
}

/// What the handshake settled.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Negotiated {
    pub acks: bool,
    pub credits: bool,
    pub max_unacked: u32,
}

/// A connection from the plugin, as two halves (`bench` reads and writes
/// from separate tasks).
pub(crate) struct Conn {
    pub reader: ConnReader,
    pub writer: ConnWriter,
}

pub(crate) struct ConnReader {
    stream: OwnedReadHalf,
}

pub(crate) struct ConnWriter {
    stream: OwnedWriteHalf,
    /// Next seq for an event the fake hub sends.
    next_seq: u64,
}

impl ConnWriter {
    pub async fn send<M: WireMessage>(&mut self, msg: &M) -> Result<(), String> {
        self.raw(&encode_message(msg)).await
    }

    pub async fn raw(&mut self, bytes: &[u8]) -> Result<(), String> {
        self.stream
            .write_all(bytes)
            .await
            .map_err(|e| format!("writing to the plugin: {e}"))
    }

    /// Closes the connection from the hub's side (the plugin sees it end).
    pub async fn close(&mut self) {
        let _ = self.stream.shutdown().await;
    }

    /// The seq of the last event sent (0: none yet).
    pub fn last_seq(&self) -> u64 {
        self.next_seq - 1
    }

    /// Sends an event; returns its seq.
    pub async fn send_event(
        &mut self,
        payload: PluginPayload,
        event_id: &str,
        attempt: u32,
    ) -> Result<u64, String> {
        let frame = self.event_frame(payload, event_id, attempt);
        self.raw(&frame.1).await?;
        Ok(frame.0)
    }

    /// An encoded event frame with the next seq, for sending later.
    pub fn event_frame(
        &mut self,
        payload: PluginPayload,
        event_id: &str,
        attempt: u32,
    ) -> (u64, Vec<u8>) {
        let seq = self.next_seq;
        self.next_seq += 1;
        let header = FrameHeader {
            code: MsgCode::Event as i32,
            seq,
            metadata: Some(EventMetadata {
                event_id: event_id.to_string(),
                trace_id: format!("trace-{event_id}"),
                hops: 1,
                origin_node_id: "trellis-source".into(),
                create_time: None,
            }),
            attempt,
            ..Default::default()
        };
        let body = Event {
            payload: Some(payload),
        }
        .encode_to_vec();
        (seq, encode_frame(&header, &body))
    }
}

impl ConnReader {
    /// The next frame; `None` if the plugin closed the connection.
    pub async fn frame(&mut self, wait: Duration) -> Result<Option<Frame>, String> {
        match tokio::time::timeout(wait, read_frame(&mut self.stream)).await {
            Ok(Ok(frame)) => Ok(frame),
            Ok(Err(e)) => Err(format!("invalid frame from the plugin: {e}")),
            Err(_) => Err(format!(
                "nothing from the plugin within {} ms",
                wait.as_millis()
            )),
        }
    }

    /// The next frame that isn't a log or error record.
    pub async fn next(&mut self, wait: Duration) -> Result<Option<Frame>, String> {
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            let Some(frame) = self.frame(left).await? else {
                return Ok(None);
            };
            if !matches!(frame.code(), MsgCode::Log | MsgCode::Error) {
                return Ok(Some(frame));
            }
        }
    }

    /// Reads until the plugin closes the connection.
    pub async fn closed(&mut self, wait: Duration) -> Result<(), String> {
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            match tokio::time::timeout(left, read_frame(&mut self.stream)).await {
                Ok(Ok(None)) | Ok(Err(_)) => return Ok(()),
                Ok(Ok(Some(_))) => continue,
                Err(_) => {
                    return Err(format!(
                        "the plugin kept the connection open for {} s",
                        wait.as_secs()
                    ));
                }
            }
        }
    }
}

pub(crate) fn decode<M: WireMessage>(frame: &Frame) -> Result<M, String> {
    frame.decode::<M>().map_err(|e| match e {
        FrameError::Decode(e) => format!("undecodable {:?}: {e}", frame.code()),
        other => other.to_string(),
    })
}

fn random_token() -> String {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).expect("the OS has random numbers");
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
