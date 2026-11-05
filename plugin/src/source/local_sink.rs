use crate::plugin::error::SourceError;
use crate::plugin::msg_client::{HubClient, Stage};
use crate::schema::schema::PluginPayload;
use crate::schema::source::{SourceEvent, SourceMessage, source_message::Payload};
use std::sync::Arc;

/// Handle a source uses to emit events to the hub. Cheap to clone.
#[derive(Clone)]
pub struct LocalSink {
    plugin_id: String,
    plugin_version: String,
    client: Arc<HubClient>,
}

impl LocalSink {
    pub(crate) fn new(plugin_id: String, plugin_version: String, client: Arc<HubClient>) -> Self {
        Self {
            plugin_id,
            plugin_version,
            client,
        }
    }

    /// Emits a payload, filling in this source's id and version. The hub
    /// fills in the event metadata.
    pub async fn emit(&self, payload: PluginPayload) -> Result<(), SourceError> {
        self.event(SourceEvent {
            plugin_id: self.plugin_id.clone(),
            plugin_version: self.plugin_version.clone(),
            payload: Some(payload),
            metadata: None,
        })
        .await
    }

    /// Emits a fully built event. Fails fast while the plugin isn't connected
    /// to the hub (or the hub hasn't accepted it yet). May wait while the hub
    /// applies backpressure: that's flow control, not an error.
    pub async fn event(&self, evt: SourceEvent) -> Result<(), SourceError> {
        let msg = SourceMessage {
            payload: Some(Payload::Event(evt)),
        };
        self.client
            .send(&msg, Stage::Ready)
            .await
            .map_err(SourceError::new)
    }
}
