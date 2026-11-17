use crate::plugin::core::Unconfirmed;
use crate::plugin::error::SourceError;
use crate::plugin::msg_client::HubClient;
use crate::schema::payload::PluginPayload;
use std::sync::Arc;

/// Handle a source uses to emit events to the hub. Cheap to clone.
#[derive(Clone)]
pub struct LocalSink {
    client: Arc<HubClient>,
}

impl LocalSink {
    pub(crate) fn new(client: Arc<HubClient>) -> Self {
        Self { client }
    }

    /// Emits a payload; the hub fills in the event metadata. Fails fast while
    /// the plugin isn't connected to the hub (or the hub hasn't accepted it
    /// yet). Waits while the hub applies backpressure (no credit): that's
    /// flow control, not an error.
    pub async fn emit(&self, payload: PluginPayload) -> Result<(), SourceError> {
        self.client
            .send_event(payload, None, false)
            .await
            .map(|_| ())
            .map_err(SourceError::new)
    }

    /// Waits until [`LocalSink::emit`] would send at once: the plugin is
    /// connected and the hub has granted credit (or doesn't use `CREDITS`).
    /// Sends nothing and is cancel-safe, so a source can race it against its
    /// own clock, e.g. to drop a tick that would go out late rather than
    /// queue it. With one task emitting, the next `emit` then doesn't wait
    /// for credit.
    /// Whether the hub has accepted the plugin on the current connection.
    pub fn is_connected(&self) -> bool {
        self.client.is_connected()
    }

    /// Whether an event could go out now without waiting: connected, and
    /// credit left (or no credit-based flow control). Synchronous, for a
    /// readiness endpoint; [`LocalSink::ready`] waits instead.
    pub fn is_ready(&self) -> bool {
        self.client.is_ready()
    }

    pub async fn ready(&self) -> Result<(), SourceError> {
        self.client.ready().await.map_err(SourceError::new)
    }

    /// Like [`LocalSink::emit`], then waits until the hub has taken the event:
    /// routed it, and stored it where the target's queue is durable. Use it
    /// when the event's origin must be told only once it's safe (e.g. a
    /// message broker's acknowledgement). Fails if the hub rejected the event
    /// or the connection was lost before the hub answered; the event may then
    /// or may not have been routed.
    /// Sends the event (waiting for credit, like [`emit`](Self::emit)) and
    /// returns once it's written, with a [`Confirmation`] that resolves when
    /// the hub has taken it. Unlike [`emit_confirmed`](Self::emit_confirmed),
    /// a source can keep several events in flight, sent in order, and wait for
    /// their confirmations in that order. Without the `ACKS` capability the
    /// confirmation resolves at once.
    pub async fn emit_tracked(&self, payload: PluginPayload) -> Result<Confirmation, SourceError> {
        let (_, confirmation) = self
            .client
            .send_event(payload, None, true)
            .await
            .map_err(SourceError::new)?;
        Ok(Confirmation(confirmation))
    }

    pub async fn emit_confirmed(&self, payload: PluginPayload) -> Result<(), SourceError> {
        let (_, confirmation) = self
            .client
            .send_event(payload, None, true)
            .await
            .map_err(SourceError::new)?;
        match confirmation {
            Some(rx) => match rx.await {
                Ok(Ok(())) => Ok(()),
                Ok(Err(unconfirmed)) => Err(SourceError::new(unconfirmed.to_string())),
                Err(_) => Err(SourceError::new(Unconfirmed::Lost.to_string())),
            },
            None => Ok(()),
        }
    }
}

/// The hub's answer to an event sent with [`LocalSink::emit_tracked`].
#[must_use = "a confirmation does nothing unless awaited"]
pub struct Confirmation(Option<tokio::sync::oneshot::Receiver<Result<(), Unconfirmed>>>);

impl Confirmation {
    /// Waits for the hub: `Ok` once it took the event; [`Unconfirmed`] if it
    /// refused it (NACK) or the connection was lost first (then the event may
    /// or may not have arrived: send it again to be sure).
    pub async fn confirmed(self) -> Result<(), Unconfirmed> {
        match self.0 {
            Some(rx) => rx.await.unwrap_or(Err(Unconfirmed::Lost)),
            None => Ok(()),
        }
    }
}
