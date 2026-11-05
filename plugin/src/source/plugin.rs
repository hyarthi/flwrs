use crate::plugin::core::InitializeRequest;
use crate::plugin::error::{InitializeError, ShutdownError, SourceError};
use crate::schema::common::log_level::Enum as LogLevel;
use crate::source::local_sink::LocalSink;

/// A plugin that produces events, e.g. a trigger, a poller or a listener.
pub trait Source {
    /// Called once after connecting to the hub. Keep `sink` to emit events
    /// from [`Source::run`]. Returns the plugin's identity and output schema.
    fn initialize(
        &mut self,
        plugin_id: String,
        log_level: LogLevel,
        sink: LocalSink,
    ) -> Result<InitializeRequest, InitializeError>;

    /// Called after `run` has been stopped.
    fn shutdown(&mut self) -> Result<(), ShutdownError>;

    fn version(&self) -> String;

    /// Produces events. The future is dropped when the hub asks for shutdown.
    /// Return `Ok` when the source is exhausted; an `Err` is reported to the
    /// hub and `run` is restarted after a short delay.
    #[allow(async_fn_in_trait)]
    async fn run(&self) -> Result<(), SourceError>;
}
