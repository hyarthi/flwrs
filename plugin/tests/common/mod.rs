#![allow(dead_code)] // not every test binary uses every helper

//! Minimal fake hub for runner tests (protocol v2).

use flwrs_plugin::plugin::core::{ConnectionConfig, ReconnectPolicy};
use flwrs_plugin::plugin::frame::{
    Frame, PROTOCOL_VERSION, WireMessage, header, read_frame, write_frame, write_message,
};
use flwrs_plugin::schema::payload::{Field, FieldValue, PluginPayload, field_value::Value};
use flwrs_plugin::schema::protocol::{
    Event, EventMetadata, FrameHeader, Initialize, InitializeResponse, Shutdown,
    capability::Enum as Capability, msg_code::Enum as MsgCode,
};
use prost::Message;
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};

pub const TOKEN: &str = "test-token-0123456789";

pub struct FakeHub {
    pub stream: TcpStream,
    /// Next seq for an event the hub sends.
    next_seq: u64,
}

pub async fn listen() -> (TcpListener, u16) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    (listener, port)
}

/// Loopback connection with a token and fast reconnects.
pub fn connection(port: u16) -> ConnectionConfig {
    ConnectionConfig::new("127.0.0.1", port)
        .with_token(Some(TOKEN.into()))
        .with_reconnect(ReconnectPolicy {
            initial_delay: Duration::from_millis(20),
            max_delay: Duration::from_millis(100),
        })
}

pub fn text(key: &str, s: &str) -> Field {
    Field {
        key: key.into(),
        value: Some(FieldValue {
            value: Some(Value::String(s.into())),
        }),
    }
}

pub fn payload(fields: Vec<Field>) -> PluginPayload {
    PluginPayload { fields }
}

impl FakeHub {
    pub async fn accept(listener: &TcpListener) -> Self {
        let (stream, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
            .await
            .expect("timed out waiting for the plugin to connect")
            .unwrap();
        Self {
            stream,
            next_seq: 1,
        }
    }

    /// Accepts a connection and reads its first frame, which must be
    /// `Initialize` (logs aren't skipped: nothing may come before it).
    pub async fn handshake(listener: &TcpListener) -> (Self, Initialize) {
        let mut hub = Self::accept(listener).await;
        let first = hub.frame().await;
        assert_eq!(first.code(), MsgCode::Initialize, "first frame: {first:?}");
        let init: Initialize = first.decode().unwrap();
        assert_eq!(init.token, TOKEN);
        assert_eq!(init.min_protocol_version, PROTOCOL_VERSION);
        assert_eq!(init.max_protocol_version, PROTOCOL_VERSION);
        (hub, init)
    }

    pub async fn send<M: WireMessage>(&mut self, msg: &M) {
        write_message(&mut self.stream, msg).await.unwrap();
    }

    pub async fn send_raw(&mut self, header: &FrameHeader, body: &[u8]) {
        write_frame(&mut self.stream, header, body).await.unwrap();
    }

    /// `InitializeResponse` enabling `capabilities`.
    pub async fn accept_plugin(&mut self, capabilities: &[Capability], heartbeat_ms: u32) {
        self.send(&InitializeResponse {
            protocol_version: PROTOCOL_VERSION,
            capabilities: capabilities.iter().map(|c| *c as i32).collect(),
            heartbeat_interval_ms: heartbeat_ms,
            max_unacked: 16,
        })
        .await;
    }

    pub async fn shutdown(&mut self, reason: &str) {
        self.send(&Shutdown {
            reason: reason.into(),
        })
        .await;
    }

    /// Sends a data event with the next seq; returns the seq.
    pub async fn send_event(&mut self, payload: PluginPayload, metadata: Option<EventMetadata>) -> u64 {
        let seq = self.next_seq;
        self.next_seq += 1;
        let header = FrameHeader {
            code: MsgCode::Event as i32,
            seq,
            metadata,
            attempt: 1,
            ..Default::default()
        };
        let body = Event {
            payload: Some(payload),
        }
        .encode_to_vec();
        self.send_raw(&header, &body).await;
        seq
    }

    /// The next frame, whatever it is.
    pub async fn frame(&mut self) -> Frame {
        tokio::time::timeout(Duration::from_secs(5), read_frame(&mut self.stream))
            .await
            .expect("timed out waiting for a plugin message")
            .unwrap()
            .expect("plugin closed connection")
    }

    /// The next frame that isn't a log record (their timing isn't deterministic).
    pub async fn next(&mut self) -> Frame {
        loop {
            let frame = self.frame().await;
            if frame.code() != MsgCode::Log {
                return frame;
            }
        }
    }

    /// The next non-log frame, which must be an `M`.
    pub async fn expect<M: WireMessage>(&mut self) -> M {
        let frame = self.next().await;
        frame
            .decode::<M>()
            .unwrap_or_else(|e| panic!("expected {:?}: {e} ({frame:?})", M::CODE))
    }

    /// Reads (and discards) frames until the plugin closes the connection.
    pub async fn expect_closed(&mut self) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while let Ok(Some(_)) = read_frame(&mut self.stream).await {}
        })
        .await
        .expect("plugin didn't close the connection");
    }

    /// Nothing but logs arrives within `wait`.
    pub async fn expect_quiet(&mut self, wait: Duration) {
        let quiet = tokio::time::timeout(wait, self.next()).await;
        assert!(quiet.is_err(), "unexpected frame: {:?}", quiet.unwrap());
    }

    pub fn header(code: MsgCode) -> FrameHeader {
        header(code)
    }
}

/// Waits (at most 5 s) for a runner task to finish, and returns its result.
pub async fn finished<T>(runner: tokio::task::JoinHandle<T>) -> T {
    tokio::time::timeout(Duration::from_secs(5), runner)
        .await
        .expect("the runner didn't finish")
        .unwrap()
}
