use crate::plugin::core::ConnectionConfig;
use crate::plugin::error::Error;
use crate::plugin::logger::PluginLogger;
use crate::plugin::msg_client::{HubClient, Incoming, Session};
use crate::plugin::runtime::{Backoff, SessionEnd, accept, finish, open_session};
use crate::schema::protocol::{log_level::Enum as LogLevel, plugin_type::Enum as PluginType};
use crate::transform::local_sink::LocalSink;
use crate::transform::plugin::Transform;
use std::sync::Arc;

/// Inputs the hub may send ahead of the transform's acks, by default.
pub const DEFAULT_MAX_UNACKED: u32 = 16;

pub struct TransformRunnerConfig {
    pub plugin_id: String,
    pub log_level: LogLevel,
    pub hub_connection: ConnectionConfig,
    /// The most inputs the hub may have sent that the transform hasn't
    /// finished yet. See [`DEFAULT_MAX_UNACKED`].
    pub max_unacked: u32,
}

impl TransformRunnerConfig {
    pub fn new(plugin_id: String, log_level: LogLevel, hub_connection: ConnectionConfig) -> Self {
        Self {
            plugin_id,
            log_level,
            hub_connection,
            max_unacked: DEFAULT_MAX_UNACKED,
        }
    }
}

pub struct TransformRunner<T: Transform> {
    plugin: T,
    plugin_id: String,
    log_level: LogLevel,
    connection: ConnectionConfig,
    max_unacked: u32,
    client: Arc<HubClient>,
}

impl<T: Transform> TransformRunner<T> {
    /// Installs the plugin logger. Connecting happens in [`TransformRunner::run`].
    pub async fn initialize(plugin: T, config: TransformRunnerConfig) -> Result<Self, Error> {
        let client = config.hub_connection.client(PluginType::Transform)?;
        PluginLogger::initialize(&config.plugin_id, config.log_level.into(), client.clone())?;
        Ok(Self {
            plugin,
            plugin_id: config.plugin_id,
            log_level: config.log_level,
            connection: config.hub_connection,
            max_unacked: config.max_unacked,
            client,
        })
    }

    /// Connects to the hub, sends `Initialize` and processes events until the
    /// hub sends `Shutdown`. Each input's outputs are sent tied to it (so the
    /// hub continues its trace), then the input is acknowledged. If the
    /// connection is lost, it reconnects with backoff and re-sends the same
    /// `Initialize`; the hub redelivers unacknowledged inputs.
    ///
    /// Returns [`Error::Rejected`] if the hub answers `Initialize` with
    /// `Shutdown` (unknown instance id, wrong token, a mismatch).
    pub async fn run(&mut self) -> Result<(), Error> {
        let sink = LocalSink::new(self.client.clone());
        let mut init = self
            .plugin
            .initialize(self.plugin_id.clone(), self.log_level, sink)
            .inspect_err(|err| log::error!("Error initializing plugin: {err}"))?
            .into_initialize(PluginType::Transform);
        init.token = self.connection.resolved_token();
        init.max_unacked = self.max_unacked;

        let mut backoff = Backoff::new(self.connection.reconnect);
        let end = loop {
            let mut session = open_session(&self.client, &init, &mut backoff).await;
            match self.serve(&mut session, &mut backoff).await {
                SessionEnd::Disconnected => {
                    self.client.disconnect(Some(session.id)).await;
                    log::warn!("Lost the connection to the hub; reconnecting");
                }
                end => break end,
            }
        };

        let shutdown = self.plugin.shutdown();
        finish(&self.client, &self.plugin_id, end, shutdown).await
    }

    async fn serve(&mut self, session: &mut Session, backoff: &mut Backoff) -> SessionEnd {
        loop {
            let Some(msg) = session.next().await else {
                return SessionEnd::Disconnected;
            };
            match msg {
                Incoming::Accepted(response) => accept(session, &response, backoff),
                Incoming::Event(event) => {
                    let seq = event.seq;
                    let input = event.input_ref();
                    let sent = match self.plugin.process_event(event) {
                        Ok(outputs) => self.send_outputs(session.id, input, outputs).await,
                        Err(err) => {
                            log::error!("Error processing event: {err}");
                            self.client
                                .nack(session.id, seq, err.to_string(), err.retry)
                                .await
                        }
                    };
                    if sent.is_err() {
                        return SessionEnd::Disconnected;
                    }
                }
                Incoming::Shutdown(shutdown) => {
                    log::debug!("Received shutdown message");
                    return SessionEnd::Shutdown {
                        accepted: session.accepted,
                        reason: shutdown.reason,
                    };
                }
            }
        }
    }

    /// Outputs first, then the input's ack: "done with this input".
    async fn send_outputs(
        &self,
        conn_id: u64,
        input: crate::plugin::core::InputRef,
        outputs: Vec<crate::schema::payload::PluginPayload>,
    ) -> std::io::Result<()> {
        for output in outputs {
            self.client.send_event(output, Some(input), false).await?;
        }
        self.client.ack(conn_id, input.seq).await
    }
}
