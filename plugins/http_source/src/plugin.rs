//! The SDK's `Source`. The HTTP server runs on its own (started before the
//! hub connection, so a bad port shows at once); requests reach the hub
//! through the sink `initialize` hands over. `run` only waits: a hub
//! reconnect restarts it without touching the server.

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
/// Requests in progress get this long to finish when the plugin stops.
pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

pub struct HttpSource {
    pub sink: Arc<OnceLock<LocalSink>>,
    pub server: Option<Server>,
    pub out_schema: SchemaDefinition,
}

impl Source for HttpSource {
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

    fn shutdown(&mut self) -> Result<(), ShutdownError> {
        if let Some(server) = self.server.take() {
            // Finish the requests in progress (their events still go out).
            tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(server.shutdown(SHUTDOWN_GRACE))
            });
        }
        Ok(())
    }

    fn version(&self) -> String {
        VERSION.to_string()
    }

    async fn run(&self) -> Result<(), SourceError> {
        std::future::pending().await
    }
}
