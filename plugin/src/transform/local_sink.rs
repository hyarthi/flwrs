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
