use crate::plugin::core::InitializeRequest;
use crate::plugin::error::{InitializeError, ShutdownError, SourceError};
use crate::schema::protocol::log_level::Enum as LogLevel;
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

    /// Called once when the run ends (after the hub's `Shutdown`, or a
    /// rejection), before `shutdown` and the `Exit`: finish asynchronous work
    /// here, e.g. stop a server and let its requests end (what they send
    /// still goes out). The default does nothing. Unlike blocking in
    /// `shutdown`, this works on a single-threaded runtime too.
    #[allow(async_fn_in_trait)]
    async fn drain(&mut self) {}

    fn version(&self) -> String;

    /// Produces events. The future is dropped when the hub asks for shutdown.
    /// Return `Ok` when the source is exhausted; an `Err` is reported to the
    /// hub and `run` is restarted after a short delay.
    #[allow(async_fn_in_trait)]
    async fn run(&self) -> Result<(), SourceError>;
}
