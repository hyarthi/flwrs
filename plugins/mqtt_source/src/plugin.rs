//! The SDK's `Source`: messages from the inbox become events, and each is
//! acknowledged to the broker once the hub has it (`confirmed`) or once it's
//! sent (`on_emit`), in the order the broker sent them (MQTT requires it).

use crate::config::{AckMode, Settings};
use crate::conn::Connection;
use flwrs_plugin::plugin::core::{InitializeRequest, SchemaDefinition, Unconfirmed};
use flwrs_plugin::plugin::error::{InitializeError, ShutdownError, SourceError};
use flwrs_plugin::schema::protocol::log_level::Enum as LogLevel;
use flwrs_plugin::source::local_sink::{Confirmation, LocalSink};
use flwrs_plugin::source::plugin::Source;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::mpsc;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// The output schema, as packaged.
pub const OUT_SCHEMA: &str = include_str!("../schema/out.json");

pub fn out_schema() -> SchemaDefinition {
    flwrs_plugin::schema::rules::parse_json(OUT_SCHEMA)
        .expect("schema/out.json is valid (tested)")
        .into()
}

pub struct MqttSource {
    settings: Arc<Settings>,
    connection: Option<Connection>,
    sink: Option<LocalSink>,
    /// Events the hub refused for good (their messages acknowledged anyway).
    refused: AtomicU64,
}

impl MqttSource {
    pub fn new(settings: Settings) -> Self {
        Self {
            settings: Arc::new(settings),
            connection: None,
            sink: None,
            refused: AtomicU64::new(0),
        }
    }

    /// `on_emit`: acknowledged as soon as the event is sent.
    async fn run_on_emit(&self, conn: &Connection, sink: &LocalSink) -> Result<(), SourceError> {
        loop {
            let payload = conn.inbox.next().await;
            sink.emit(payload).await?;
            if let Some(item) = conn.inbox.take() {
                conn.ack(&item).await;
            }
        }
    }

    /// `confirmed`: events go out in order, several in flight; each message
    /// is acknowledged after its event's confirmation, in the same order.
    async fn run_confirmed(&self, conn: &Connection, sink: &LocalSink) -> Result<(), SourceError> {
        let (confirmations, mut pending) = mpsc::unbounded_channel::<Confirmation>();
        let send = async {
            loop {
                let payload = conn.inbox.next().await;
                let confirmation = sink.emit_tracked(payload).await?;
                conn.inbox.emitted();
                if confirmations.send(confirmation).is_err() {
                    return Ok::<(), SourceError>(());
                }
            }
        };
        let confirm = async {
            while let Some(confirmation) = pending.recv().await {
                match confirmation.confirmed().await {
                    Ok(()) => {}
                    Err(Unconfirmed::Refused {
                        reason,
                        retry: false,
                    }) => {
                        // The hub won't take it (e.g. a queue that drops): the
                        // message is acknowledged anyway, or it would block the
                        // broker's window forever.
                        let n = self.refused.fetch_add(1, Ordering::Relaxed) + 1;
                        log::warn!(
                            "the hub refused an event ({reason}); its message is dropped ({n} so far)"
                        );
                    }
                    // Not taken yet: `run` restarts and sends it again.
                    Err(e) => return Err(SourceError::new(e.to_string())),
                }
                if let Some(item) = conn.inbox.confirmed() {
                    conn.ack(&item).await;
                }
            }
            Ok(())
        };
        tokio::try_join!(send, confirm).map(|_| ())
    }
}

impl Source for MqttSource {
    fn initialize(
        &mut self,
        plugin_id: String,
        _: LogLevel,
        sink: LocalSink,
    ) -> Result<InitializeRequest, InitializeError> {
        self.sink = Some(sink);
        // Connect at once, so a bad broker URL or credentials show right away.
        // The connection outlives hub reconnects (and `run` restarts).
        if self.connection.is_none() {
            self.connection = Some(Connection::start(self.settings.clone()));
        }
        Ok(InitializeRequest::new()
            .with_id(plugin_id)
            .with_version(VERSION.to_string())
            .with_out_schema(out_schema()))
    }

    fn shutdown(&mut self) -> Result<(), ShutdownError> {
        if let Some(connection) = &self.connection {
            connection.disconnect();
        }
        Ok(())
    }

    fn version(&self) -> String {
        VERSION.to_string()
    }

    async fn run(&self) -> Result<(), SourceError> {
        let (Some(conn), Some(sink)) = (&self.connection, &self.sink) else {
            return Err(SourceError::new("not initialized"));
        };
        // A previous run's events that weren't confirmed are sent again first.
        conn.inbox.requeue_unconfirmed();
        match self.settings.ack_mode {
            AckMode::Confirmed => self.run_confirmed(conn, sink).await,
            AckMode::OnEmit => self.run_on_emit(conn, sink).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_schema_file_is_valid() {
        let schema = flwrs_plugin::schema::rules::parse_json(OUT_SCHEMA).unwrap();
        assert!(flwrs_plugin::schema::rules::check_well_formed(&schema).is_empty());
    }
}
