use crate::plugin::core::InitializeRequest;
use crate::plugin::error::{InitializeError, ShutdownError, SinkError};
use crate::schema::common::log_level::Enum as LogLevel;
use crate::schema::sink::SinkEvent;

/// A plugin that consumes events, e.g. writes them somewhere.
pub trait Sink {
    /// Called once after connecting to the hub. Returns the plugin's
    /// identity and the schema of payloads it accepts.
    fn initialize(
        &mut self,
        plugin_id: String,
        log_level: LogLevel,
    ) -> Result<InitializeRequest, InitializeError>;

    fn shutdown(&mut self) -> Result<(), ShutdownError>;

    fn version(&self) -> String;

    /// Handles one event. Errors are reported to the hub; the sink keeps running.
    fn consume_event(&mut self, event: SinkEvent) -> Result<(), SinkError>;
}
