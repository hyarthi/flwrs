use crate::plugin::core::ConnectionConfig;
use crate::plugin::error::Error;
use crate::plugin::logger::PluginLogger;
use crate::plugin::msg_client::{HubClient, Session, Stage};
use crate::plugin::runtime::{Backoff, SessionEnd, accept, error_event, finish, open_session};
use crate::schema::common::{log_level::Enum as LogLevel, plugin_type::Enum as PluginType};
use crate::schema::transform::{
    Initialize, RuntimeTransformMessage, TransformMessage,
    runtime_transform_message::Payload as RuntimePayload, transform_message::Payload,
};
use crate::transform::local_sink::LocalSink;
use crate::transform::plugin::Transform;
use std::sync::Arc;

pub struct TransformRunnerConfig {
    pub plugin_id: String,
    pub log_level: LogLevel,
    pub hub_connection: ConnectionConfig,
}

pub struct TransformRunner<T: Transform> {
    plugin: T,
    plugin_id: String,
    log_level: LogLevel,
    connection: ConnectionConfig,
    client: Arc<HubClient>,
}

impl<T: Transform> TransformRunner<T> {
    /// Installs the plugin logger. Connecting happens in [`TransformRunner::run`].
    pub async fn initialize(plugin: T, config: TransformRunnerConfig) -> Result<Self, Error> {
        let client = HubClient::new(config.hub_connection.addr());
        PluginLogger::initialize(
            &config.plugin_id,
            PluginType::Transform,
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

    /// Connects to the hub, sends `Initialize` and processes events until the
    /// hub sends `Shutdown`. If the connection is lost, it reconnects with
    /// backoff and re-sends the same `Initialize`.
    ///
    /// Each output event gets the input event's `metadata` unless the plugin
    /// set its own (protocol rule: transforms propagate metadata).
    ///
    /// Returns [`Error::Rejected`] if the hub answers `Initialize` with
    /// `Shutdown` (unknown instance id or wrong token).
    pub async fn run(&mut self) -> Result<(), Error> {
        let sink = LocalSink::new(self.client.clone());
        let mut init: Initialize = self
            .plugin
            .initialize(self.plugin_id.clone(), self.log_level, sink)
            .inspect_err(|err| log::error!("Error initializing plugin: {err}"))?
            .into();
        init.token = self.connection.resolved_token();
        let init = TransformMessage {
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
            TransformMessage {
                payload: Some(Payload::Exit(exit)),
            }
        })
        .await
    }

    async fn serve(
        &mut self,
        session: &mut Session<RuntimeTransformMessage>,
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
                    let metadata = event.metadata.clone();
                    let (reply, stage) = match self.plugin.process_event(event) {
                        Ok(mut out) => {
                            if out.metadata.is_none() {
                                out.metadata = metadata;
                            }
                            (Payload::Event(out), Stage::Ready)
                        }
                        Err(err) => {
                            log::error!("Error processing event: {err}");
                            let error =
                                error_event(&self.plugin_id, PluginType::Transform, err.to_string());
                            (Payload::Error(error), Stage::Initializing)
                        }
                    };
                    let msg = TransformMessage {
                        payload: Some(reply),
                    };
                    if self.client.send(&msg, stage).await.is_err() {
                        return SessionEnd::Disconnected;
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
