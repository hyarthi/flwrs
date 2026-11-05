use crate::plugin::core::InitializeRequest;
use crate::plugin::error::{InitializeError, ShutdownError, TransformError};
use crate::schema::common::log_level::Enum as LogLevel;
use crate::schema::transform::TransformEvent;
use crate::transform::local_sink::LocalSink;

/// A plugin that turns each incoming event into an outgoing event.
pub trait Transform {
    /// Called once after connecting to the hub. Returns the plugin's identity
    /// and its input and output schemas.
    fn initialize(
        &mut self,
        plugin_id: String,
        log_level: LogLevel,
        sink: LocalSink,
    ) -> Result<InitializeRequest, InitializeError>;

    fn shutdown(&mut self) -> Result<(), ShutdownError>;

    fn version(&self) -> String;

    /// Processes one event. The returned event is sent back to the hub;
    /// errors are reported to the hub and the transform keeps running.
    fn process_event(&mut self, event: TransformEvent) -> Result<TransformEvent, TransformError>;
}
