use crate::plugin::core::ConnectionConfig;
use crate::plugin::error::Error;
use crate::plugin::logger::PluginLogger;
use crate::plugin::msg_client::{HubClient, Session, Stage};
use crate::plugin::runtime::{
    Backoff, RESTART_BACKOFF, SessionEnd, accept, error_event, finish, open_session,
};
use crate::schema::common::{log_level::Enum as LogLevel, plugin_type::Enum as PluginType};
use crate::schema::source::{
    Initialize, RuntimeSourceMessage, SourceMessage,
    runtime_source_message::Payload as RuntimePayload, source_message::Payload,
};
use crate::source::local_sink::LocalSink;
use crate::source::plugin::Source;
use std::sync::Arc;

pub struct SourceRunnerConfig {
    pub plugin_id: String,
    pub log_level: LogLevel,
    pub hub_connection: ConnectionConfig,
}

pub struct SourceRunner<T: Source> {
    plugin: T,
    plugin_id: String,
    log_level: LogLevel,
    connection: ConnectionConfig,
    client: Arc<HubClient>,
}

/// What a control message means for the session.
enum Control {
    Continue,
    End(SessionEnd),
}

impl<T: Source> SourceRunner<T> {
    /// Installs the plugin logger. Connecting happens in [`SourceRunner::run`].
    pub async fn initialize(plugin: T, config: SourceRunnerConfig) -> Result<Self, Error> {
        let client = HubClient::new(config.hub_connection.addr());
        PluginLogger::initialize(
            &config.plugin_id,
            PluginType::Source,
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

    /// Connects to the hub and sends `Initialize`. Once the hub accepts the
    /// plugin (`InitializeResponse`), runs the source until the hub sends
    /// `Shutdown`. A failing `run` is restarted after a short delay.
    ///
    /// If the connection is lost, `run` is cancelled; the runner reconnects
    /// with backoff, re-sends the same `Initialize` and restarts `run` once
    /// accepted again. A `run` that already finished isn't restarted.
    ///
    /// Returns [`Error::Rejected`] if the hub answers `Initialize` with
    /// `Shutdown` (unknown instance id or wrong token).
    pub async fn run(&mut self) -> Result<(), Error> {
        let sink = LocalSink::new(
            self.plugin_id.clone(),
            self.plugin.version(),
            self.client.clone(),
        );
        let mut init: Initialize = self
            .plugin
            .initialize(self.plugin_id.clone(), self.log_level, sink)
            .inspect_err(|err| log::error!("Error initializing plugin: {err}"))?
            .into();
        init.token = self.connection.resolved_token();
        let init = SourceMessage {
            payload: Some(Payload::Initialize(init)),
        };

        let mut backoff = Backoff::new(self.connection.reconnect);
        let mut finished = false;
        let end = loop {
            let mut session = open_session(&self.client, &init, &mut backoff).await;
            let mut end = await_accept(&self.client, &mut session, &mut backoff).await;
            if end.is_none() {
                end = Some(
                    run_source(
                        &self.plugin,
                        &self.plugin_id,
                        &self.client,
                        &mut session,
                        &mut finished,
                    )
                    .await,
                );
            }
            match end {
                Some(SessionEnd::Disconnected) | None => {
                    self.client.disconnect(Some(session.id)).await;
                    log::warn!("Lost the connection to the hub; reconnecting");
                }
                Some(end) => break end,
            }
        };

        let shutdown = self.plugin.shutdown();
        finish(&self.client, &self.plugin_id, end, shutdown, |exit| {
            SourceMessage {
                payload: Some(Payload::Exit(exit)),
            }
        })
        .await
    }
}

/// Waits for `InitializeResponse`. Returns `None` once accepted, or how the
/// session ended before that.
async fn await_accept(
    client: &HubClient,
    session: &mut Session<RuntimeSourceMessage>,
    backoff: &mut Backoff,
) -> Option<SessionEnd> {
    loop {
        let Some(msg) = session.next().await else {
            return Some(SessionEnd::Disconnected);
        };
        match msg.payload {
            None => log::error!("Message payload is missing"),
            Some(RuntimePayload::Initialize(_)) => {
                accept(client, session, backoff).await;
                return None;
            }
            Some(RuntimePayload::Shutdown(_)) => {
                log::debug!("Received shutdown message");
                return Some(SessionEnd::Shutdown { accepted: false });
            }
        }
    }
}

fn control(msg: RuntimeSourceMessage) -> Control {
    match msg.payload {
        None => {
            log::error!("Message payload is missing");
            Control::Continue
        }
        Some(RuntimePayload::Initialize(_)) => Control::Continue,
        Some(RuntimePayload::Shutdown(_)) => {
            log::debug!("Received shutdown message");
            Control::End(SessionEnd::Shutdown { accepted: true })
        }
    }
}

/// Runs the source (restarting it on failure) while handling control
/// messages, until `Shutdown` or a disconnect.
async fn run_source<T: Source>(
    plugin: &T,
    plugin_id: &str,
    client: &HubClient,
    session: &mut Session<RuntimeSourceMessage>,
    finished: &mut bool,
) -> SessionEnd {
    loop {
        if *finished {
            // Nothing left to run: just wait for Shutdown or a disconnect.
            loop {
                match session.next().await {
                    None => return SessionEnd::Disconnected,
                    Some(msg) => {
                        if let Control::End(end) = control(msg) {
                            return end;
                        }
                    }
                }
            }
        }

        let result = {
            let run = plugin.run();
            tokio::pin!(run);
            loop {
                tokio::select! {
                    result = &mut run => break result,
                    msg = session.next() => match msg {
                        None => return SessionEnd::Disconnected,
                        Some(msg) => if let Control::End(end) = control(msg) {
                            return end;
                        },
                    },
                }
            }
        };

        match result {
            Ok(()) => {
                log::info!("Source finished, waiting for shutdown");
                *finished = true;
            }
            Err(_) if client.stage() == Stage::Down => return SessionEnd::Disconnected,
            Err(err) => {
                log::error!("Plugin crashed: {err}");
                let msg = SourceMessage {
                    payload: Some(Payload::Error(error_event(
                        plugin_id,
                        PluginType::Source,
                        err.to_string(),
                    ))),
                };
                if client.send(&msg, Stage::Initializing).await.is_err() {
                    return SessionEnd::Disconnected;
                }
                let sleep = tokio::time::sleep(RESTART_BACKOFF);
                tokio::pin!(sleep);
                loop {
                    tokio::select! {
                        _ = &mut sleep => break,
                        msg = session.next() => match msg {
                            None => return SessionEnd::Disconnected,
                            Some(msg) => if let Control::End(end) = control(msg) {
                                return end;
                            },
                        },
                    }
                }
            }
        }
    }
}
