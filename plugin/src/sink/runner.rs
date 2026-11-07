use crate::plugin::core::{ConnectionConfig, InboundEvent};
use crate::plugin::error::{Error, SinkError};
use crate::plugin::logger::PluginLogger;
use crate::plugin::msg_client::{HubClient, Incoming, Session};
use crate::plugin::runtime::{Backoff, SessionEnd, accept, finish, open_session};
use crate::schema::protocol::{log_level::Enum as LogLevel, plugin_type::Enum as PluginType};
use crate::sink::plugin::Sink;
use futures_util::StreamExt;
use futures_util::stream::FuturesUnordered;
use std::collections::VecDeque;
use std::sync::Arc;

/// Events the hub may send ahead of the sink's acks, by default.
pub const DEFAULT_MAX_UNACKED: u32 = 16;

pub struct SinkRunnerConfig {
    pub plugin_id: String,
    pub log_level: LogLevel,
    pub hub_connection: ConnectionConfig,
    /// The most events the hub may have sent that the sink hasn't handled
    /// yet; a window larger than `max_concurrent` lets the hub send ahead.
    /// See [`DEFAULT_MAX_UNACKED`].
    pub max_unacked: u32,
    /// How many [`Sink::consume_event`] calls may run at once (at least 1).
    /// With 1 (the default) events are handled one at a time, in the order
    /// the hub sent them; with more, they may finish (and be acked) in any
    /// order. More than `max_unacked` has no effect.
    pub max_concurrent: usize,
}

impl SinkRunnerConfig {
    pub fn new(plugin_id: String, log_level: LogLevel, hub_connection: ConnectionConfig) -> Self {
        Self {
            plugin_id,
            log_level,
            hub_connection,
            max_unacked: DEFAULT_MAX_UNACKED,
            max_concurrent: 1,
        }
    }

    /// Sets [`Self::max_unacked`].
    pub fn with_max_unacked(mut self, max_unacked: u32) -> Self {
        self.max_unacked = max_unacked;
        self
    }

    /// Sets [`Self::max_concurrent`].
    pub fn with_max_concurrent(mut self, max_concurrent: usize) -> Self {
        self.max_concurrent = max_concurrent;
        self
    }
}

pub struct SinkRunner<T: Sink> {
    plugin: T,
    plugin_id: String,
    log_level: LogLevel,
    connection: ConnectionConfig,
    max_unacked: u32,
    max_concurrent: usize,
    client: Arc<HubClient>,
}

impl<T: Sink> SinkRunner<T> {
    /// Installs the plugin logger. Connecting happens in [`SinkRunner::run`].
    pub async fn initialize(plugin: T, config: SinkRunnerConfig) -> Result<Self, Error> {
        let client = config.hub_connection.client(PluginType::Sink)?;
        PluginLogger::initialize(&config.plugin_id, config.log_level.into(), client.clone())?;
        Ok(Self {
            plugin,
            plugin_id: config.plugin_id,
            log_level: config.log_level,
            connection: config.hub_connection,
            max_unacked: config.max_unacked,
            max_concurrent: config.max_concurrent.max(1),
            client,
        })
    }

    /// Connects to the hub, sends `Initialize` and handles events until the
    /// hub sends `Shutdown`. Each event is acknowledged once the sink has
    /// handled it (its `consume_event` future finished). On `Shutdown`, the
    /// events already received are still handled and answered first. If the connection is lost, it reconnects with backoff and
    /// re-sends the same `Initialize`; the hub redelivers unacknowledged events.
    ///
    /// Returns [`Error::Rejected`] if the hub answers `Initialize` with
    /// `Shutdown` (unknown instance id, wrong token, a mismatch).
    pub async fn run(&mut self) -> Result<(), Error> {
        let mut init = self
            .plugin
            .initialize(self.plugin_id.clone(), self.log_level)
            .inspect_err(|err| log::error!("Error initializing plugin: {err}"))?
            .into_initialize(PluginType::Sink);
        init.token = self.connection.resolved_token();
        init.max_unacked = self.max_unacked;

        let mut backoff = Backoff::new(self.connection.reconnect);
        let end = loop {
            let mut session = open_session(&self.client, &init, &mut backoff).await;
            let served = serve(
                &self.plugin,
                &self.client,
                self.max_concurrent,
                &mut session,
                &mut backoff,
            );
            match served.await {
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
}

/// Handles one connection's events, up to `max_concurrent` at a time, and
/// answers each one when it's done.
async fn serve<T: Sink>(
    plugin: &T,
    client: &HubClient,
    max_concurrent: usize,
    session: &mut Session,
    backoff: &mut Backoff,
) -> SessionEnd {
    let mut running = FuturesUnordered::new();
    // Received but not started yet; the hub's window bounds it.
    let mut waiting = VecDeque::new();
    loop {
        while running.len() < max_concurrent
            && let Some(event) = waiting.pop_front()
        {
            running.push(consume(plugin, event));
        }
        tokio::select! {
            Some((seq, result)) = running.next(), if !running.is_empty() => {
                if answer(client, session.id, seq, result).await.is_err() {
                    return SessionEnd::Disconnected;
                }
            }
            msg = session.next() => match msg {
                None => return SessionEnd::Disconnected,
                Some(Incoming::Accepted(response)) => accept(session, &response, backoff),
                Some(Incoming::Event(event)) => waiting.push_back(event),
                Some(Incoming::Shutdown(shutdown)) => {
                    log::debug!("Received shutdown message");
                    // Finish what was received before the shutdown.
                    loop {
                        while running.len() < max_concurrent
                            && let Some(event) = waiting.pop_front()
                        {
                            running.push(consume(plugin, event));
                        }
                        let Some((seq, result)) = running.next().await else {
                            break;
                        };
                        if answer(client, session.id, seq, result).await.is_err() {
                            break;
                        }
                    }
                    return SessionEnd::Shutdown {
                        accepted: session.accepted,
                        reason: shutdown.reason,
                    };
                }
            },
        }
    }
}

async fn consume<T: Sink>(plugin: &T, event: InboundEvent) -> (u64, Result<(), SinkError>) {
    let seq = event.seq;
    (seq, plugin.consume_event(event).await)
}

/// ACKs a handled event, or NACKs a failed one.
async fn answer(
    client: &HubClient,
    conn_id: u64,
    seq: u64,
    result: Result<(), SinkError>,
) -> std::io::Result<()> {
    match result {
        Ok(()) => client.ack(conn_id, seq).await,
        Err(err) => {
            log::error!("Error processing event: {err}");
            client.nack(conn_id, seq, err.to_string(), err.retry).await
        }
    }
}
