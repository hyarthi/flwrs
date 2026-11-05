#![allow(dead_code)] // not every test binary uses every helper

//! Minimal fake hub for runner tests.

use flwrs_plugin::plugin::core::{ConnectionConfig, ReconnectPolicy};
use flwrs_plugin::plugin::frame::{WireMessage, read_frame, write_frame, write_message};
use flwrs_plugin::schema::common::msg_code::Enum as MsgCode;
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};

pub const TOKEN: &str = "test-token-0123456789";

pub struct FakeHub {
    pub stream: TcpStream,
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

impl FakeHub {
    pub async fn accept(listener: &TcpListener) -> Self {
        let (stream, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
            .await
            .expect("timed out waiting for the plugin to connect")
            .unwrap();
        Self { stream }
    }

    pub async fn send<M: WireMessage>(&mut self, msg: &M) {
        write_message(&mut self.stream, msg).await.unwrap();
    }

    pub async fn send_raw(&mut self, code: MsgCode, body: &[u8]) {
        write_frame(&mut self.stream, code, body).await.unwrap();
    }

    /// Next message of type `M`, skipping frames for which `skip` returns true
    /// (e.g. log messages, whose timing is not deterministic).
    pub async fn recv<M: WireMessage>(&mut self, skip: impl Fn(&M) -> bool) -> M {
        loop {
            let frame = tokio::time::timeout(Duration::from_secs(5), read_frame(&mut self.stream))
                .await
                .expect("timed out waiting for plugin message")
                .unwrap()
                .expect("plugin closed connection");
            let msg = frame.decode::<M>().unwrap();
            if !skip(&msg) {
                return msg;
            }
        }
    }

    /// Reads (and discards) frames until the plugin closes the connection.
    pub async fn expect_closed(&mut self) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while let Ok(Some(_)) = read_frame(&mut self.stream).await {}
        })
        .await
        .expect("plugin didn't close the connection");
    }
}
