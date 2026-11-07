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

    /// Like [`LocalSink::emit`], then waits until the hub has taken the event:
    /// routed it, and stored it where the target's queue is durable. Use it
    /// when the event's origin must be told only once it's safe (e.g. a
    /// message broker's acknowledgement). Fails if the hub rejected the event
    /// or the connection was lost before the hub answered; the event may then
    /// or may not have been routed.
    pub async fn emit_confirmed(&self, payload: PluginPayload) -> Result<(), SourceError> {
        let (_, confirmation) = self
            .client
            .send_event(payload, None, true)
            .await
            .map_err(SourceError::new)?;
        match confirmation {
            Some(rx) => match rx.await {
                Ok(Ok(())) => Ok(()),
                Ok(Err(reason)) => Err(SourceError::new(reason)),
                Err(_) => Err(SourceError::new("the connection to the hub was lost")),
            },
            None => Ok(()),
        }
    }
}
