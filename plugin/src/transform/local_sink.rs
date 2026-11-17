use crate::plugin::core::InputRef;
use crate::plugin::error::TransformError;
use crate::plugin::msg_client::HubClient;
use crate::schema::payload::PluginPayload;
use std::sync::Arc;

/// Handle a transform uses to emit events outside `process_event`, e.g. a
/// stateful accumulator that emits on a timer or an external signal. Cheap
/// to clone.
#[derive(Clone)]
pub struct LocalSink {
    client: Arc<HubClient>,
}

impl LocalSink {
    pub(crate) fn new(client: Arc<HubClient>) -> Self {
        Self { client }
    }

    /// Sends an event that isn't tied to an input: it starts a new trace.
    /// Fails fast while the plugin isn't connected to the hub (or the hub
    /// hasn't accepted it yet). Waits while the hub applies backpressure (no
    /// credit): that's flow control, not an error.
    pub async fn emit(&self, payload: PluginPayload) -> Result<(), TransformError> {
        self.client
            .send_event(payload, None, false)
            .await
            .map(|_| ())
            .map_err(TransformError::new)
    }

    /// Like [`Self::emit`], and also waits for the hub's ACK (with `ACKS`):
    /// `Ok` means the hub took the event (into a durable queue, if the target
    /// has one). A NACK or a lost connection is an error; the caller may send
    /// it again (so a receiver may see it twice).
    pub async fn emit_confirmed(&self, payload: PluginPayload) -> Result<(), TransformError> {
        let (_, confirmation) = self
            .client
            .send_event(payload, None, true)
            .await
            .map_err(TransformError::new)?;
        match confirmation {
            Some(rx) => match rx.await {
                Ok(Ok(())) => Ok(()),
                Ok(Err(unconfirmed)) => Err(TransformError::new(unconfirmed.to_string())),
                Err(_) => Err(TransformError::new("the connection to the hub was lost")),
            },
            None => Ok(()),
        }
    }

    /// Sends an event tied to an earlier input (from
    /// [`InboundEvent::input_ref`](crate::plugin::core::InboundEvent::input_ref)),
    /// so it continues that input's trace, even if the input was acknowledged
    /// long ago. If the input came on an earlier connection, or the hub no
    /// longer remembers it, the event starts a new trace instead.
    pub async fn emit_linked(
        &self,
        input: InputRef,
        payload: PluginPayload,
    ) -> Result<(), TransformError> {
        self.client
            .send_event(payload, Some(input), false)
            .await
            .map(|_| ())
            .map_err(TransformError::new)
    }
}
