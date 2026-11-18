//! The SDK's `Source`. The WebSocket server runs on its own (started before
//! the hub connection, so a bad port shows at once); messages reach the hub
//! through the sink `initialize` hands over. `run` only waits: a hub
//! reconnect restarts it without touching the server or its connections.

use flwrs_plugin::plugin::core::{InitializeRequest, SchemaDefinition as Schema};
use flwrs_plugin::plugin::error::{InitializeError, ShutdownError, SourceError};
use flwrs_plugin::schema::payload::SchemaDefinition;
use flwrs_plugin::schema::protocol::log_level::Enum as LogLevel;
use flwrs_plugin::source::local_sink::LocalSink;
use flwrs_plugin::source::plugin::Source;
use flwrs_plugin_kit::server::Server;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Connections get this long to close when the plugin stops.
pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

pub struct WsSource {
    pub sink: Arc<OnceLock<LocalSink>>,
    pub server: Option<Server>,
    pub out_schema: SchemaDefinition,
}

impl Source for WsSource {
    fn initialize(
        &mut self,
        plugin_id: String,
        _: LogLevel,
        sink: LocalSink,
    ) -> Result<InitializeRequest, InitializeError> {
        let _ = self.sink.set(sink);
        let out: Schema = self.out_schema.clone().into();
        Ok(InitializeRequest::new()
            .with_id(plugin_id)
            .with_version(VERSION.to_string())
            .with_out_schema(out))
    }

    async fn drain(&mut self) {
        if let Some(server) = self.server.take() {
            // Connections see `stopping` and close with 1001.
            server.shutdown(SHUTDOWN_GRACE).await;
        }
    }

    fn shutdown(&mut self) -> Result<(), ShutdownError> {
        Ok(())
    }

    fn version(&self) -> String {
        VERSION.to_string()
    }

    async fn run(&self) -> Result<(), SourceError> {
        std::future::pending().await
    }
}
