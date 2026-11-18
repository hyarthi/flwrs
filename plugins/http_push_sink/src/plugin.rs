//! The SDK's `Sink`: each event goes into its channel on the broadcast core,
//! and is answered by the ack mode (`delivered`: once a subscriber's
//! connection took it). The HTTP server runs on its own; the shutdown hint
//! closes the core, so held events are refused (retryable) at once.

use flwrs_plugin::plugin::core::{InboundEvent, InitializeRequest};
use flwrs_plugin::plugin::error::{InitializeError, ShutdownError, SinkError};
use flwrs_plugin::schema::protocol::log_level::Enum as LogLevel;
use flwrs_plugin::sink::plugin::Sink;
use flwrs_plugin_kit::broadcast::{AckMode, Broadcaster, in_schema, publish};
use flwrs_plugin_kit::server::Server;
use std::sync::Arc;
use std::time::Duration;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Subscribers get this long to see their streams end when the plugin stops.
pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

pub struct PushSink {
    pub broadcaster: Arc<Broadcaster>,
    pub ack_mode: AckMode,
    pub server: Option<Server>,
}

impl Sink for PushSink {
    fn initialize(
        &mut self,
        plugin_id: String,
        _: LogLevel,
    ) -> Result<InitializeRequest, InitializeError> {
        Ok(InitializeRequest::new()
            .with_id(plugin_id)
            .with_version(VERSION.to_string())
            .with_in_schema(in_schema().into()))
    }

    fn shutdown_started(&self) {
        self.broadcaster.close();
    }

    async fn drain(&mut self) {
        self.broadcaster.close();
        if let Some(server) = self.server.take() {
            server.shutdown(SHUTDOWN_GRACE).await;
        }
    }

    fn shutdown(&mut self) -> Result<(), ShutdownError> {
        self.broadcaster.close();
        Ok(())
    }

    fn version(&self) -> String {
        VERSION.to_string()
    }

    async fn consume_event(&self, event: InboundEvent) -> Result<(), SinkError> {
        publish(&self.broadcaster, self.ack_mode, &event).await
    }
}
