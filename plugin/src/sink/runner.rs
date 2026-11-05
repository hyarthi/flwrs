use crate::plugin::core::ConnectionConfig;
use crate::plugin::error::Error;
use crate::plugin::logger::PluginLogger;
use crate::plugin::msg_client::{HubClient, Session, Stage};
use crate::plugin::runtime::{Backoff, SessionEnd, accept, error_event, finish, open_session};
use crate::schema::common::{log_level::Enum as LogLevel, plugin_type::Enum as PluginType};
use crate::schema::sink::{
    Initialize, RuntimeSinkMessage, SinkMessage, runtime_sink_message::Payload as RuntimePayload,
    sink_message::Payload,
};
use crate::sink::plugin::Sink;
use std::sync::Arc;

pub struct SinkRunnerConfig {
    pub plugin_id: String,
    pub log_level: LogLevel,
    pub hub_connection: ConnectionConfig,
}

pub struct SinkRunner<T: Sink> {
    plugin: T,
    plugin_id: String,
    log_level: LogLevel,
    connection: ConnectionConfig,
    client: Arc<HubClient>,
}

impl<T: Sink> SinkRunner<T> {
    /// Installs the plugin logger. Connecting happens in [`SinkRunner::run`].
    pub async fn initialize(plugin: T, config: SinkRunnerConfig) -> Result<Self, Error> {
        let client = HubClient::new(config.hub_connection.addr());
        PluginLogger::initialize(
            &config.plugin_id,
            PluginType::Sink,
            config.log_level.into(),
            client.clone(),
        )?;
        Ok(Self {
            plugin,
            plugin_id: config.plugin_id,
            log_level: config.log_level,
            connection: config.hub_connection,
            client,
        })
    }

    /// Connects to the hub, sends `Initialize` and handles events until the
    /// hub sends `Shutdown`. If the connection is lost, it reconnects with
    /// backoff and re-sends the same `Initialize`.
    ///
    /// Returns [`Error::Rejected`] if the hub answers `Initialize` with
    /// `Shutdown` (unknown instance id or wrong token).
    pub async fn run(&mut self) -> Result<(), Error> {
        let mut init: Initialize = self
            .plugin
            .initialize(self.plugin_id.clone(), self.log_level)
            .inspect_err(|err| log::error!("Error initializing plugin: {err}"))?
            .into();
        init.token = self.connection.resolved_token();
        let init = SinkMessage {
            payload: Some(Payload::Initialize(init)),
        };

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
        finish(&self.client, &self.plugin_id, end, shutdown, |exit| {
            SinkMessage {
                payload: Some(Payload::Exit(exit)),
            }
        })
        .await
    }

    async fn serve(
        &mut self,
        session: &mut Session<RuntimeSinkMessage>,
        backoff: &mut Backoff,
    ) -> SessionEnd {
        loop {
            let Some(msg) = session.next().await else {
                return SessionEnd::Disconnected;
            };
            let Some(payload) = msg.payload else {
                log::error!("Message payload is missing");
                continue;
            };
            match payload {
                RuntimePayload::Initialize(_) => accept(&self.client, session, backoff).await,
                RuntimePayload::Event(event) => {
                    log::debug!("Received event for: {}", event.plugin_id);
                    if let Err(err) = self.plugin.consume_event(event) {
                        log::error!("Error processing event: {err}");
                        let msg = SinkMessage {
                            payload: Some(Payload::Error(error_event(
                                &self.plugin_id,
                                PluginType::Sink,
                                err.to_string(),
                            ))),
                        };
                        if self.client.send(&msg, Stage::Initializing).await.is_err() {
                            return SessionEnd::Disconnected;
                        }
                    }
                }
                RuntimePayload::Shutdown(_) => {
                    log::debug!("Received shutdown message");
                    return SessionEnd::Shutdown {
                        accepted: session.accepted,
                    };
                }
            }
        }
    }
}
