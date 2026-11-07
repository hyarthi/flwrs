use crate::plugin::core::{InboundEvent, InitializeRequest};
use crate::plugin::error::{InitializeError, ShutdownError, TransformError};
use crate::schema::payload::PluginPayload;
use crate::schema::protocol::log_level::Enum as LogLevel;
use crate::transform::local_sink::LocalSink;

/// A plugin that turns incoming events into outgoing ones.
pub trait Transform {
    /// Called once before connecting to the hub. Returns the plugin's
    /// identity and its input and output schemas.
    fn initialize(
        &mut self,
        plugin_id: String,
        log_level: LogLevel,
        sink: LocalSink,
    ) -> Result<InitializeRequest, InitializeError>;

    fn shutdown(&mut self) -> Result<(), ShutdownError>;

    fn version(&self) -> String;

    /// Processes one event and returns its outputs: none (to filter it out),
    /// one, or several. The runner sends them to the hub tied to this input,
    /// so they continue its trace, then acknowledges the input.
    ///
    /// An error is logged and the input is rejected: the hub delivers it
    /// again later if the error is [`TransformError::retryable`], and gives
    /// up on it otherwise. The transform keeps running either way.
    fn process_event(&mut self, event: InboundEvent)
    -> Result<Vec<PluginPayload>, TransformError>;
}
