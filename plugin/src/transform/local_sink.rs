use crate::plugin::error::TransformError;
use crate::plugin::msg_client::{HubClient, Stage};
use crate::schema::transform::{TransformEvent, TransformMessage, transform_message::Payload};
use std::sync::Arc;

/// Handle a transform can use to emit extra events (beyond the one returned
/// from `process_event`), e.g. to fan out. Cheap to clone.
///
/// Events sent here start a new trace unless you set `metadata` yourself
/// (for example, a copy of the input event's metadata).
#[derive(Clone)]
pub struct LocalSink {
    client: Arc<HubClient>,
}

impl LocalSink {
    pub(crate) fn new(client: Arc<HubClient>) -> Self {
        Self { client }
    }

    /// Sends an event. Fails fast while the plugin isn't connected to the hub
    /// (or the hub hasn't accepted it yet). May wait while the hub applies
    /// backpressure.
    pub async fn event(&self, evt: TransformEvent) -> Result<(), TransformError> {
        let msg = TransformMessage {
            payload: Some(Payload::Event(evt)),
        };
        self.client
            .send(&msg, Stage::Ready)
            .await
            .map_err(TransformError::new)
    }
}
