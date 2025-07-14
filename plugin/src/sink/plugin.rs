use crate::plugin::error::{InitializeError, ShutdownError, SinkError};
use crate::schema::common::{log_level::Enum as LogLevel};
use crate::schema::sink::SinkEvent;
use crate::plugin::core::InitializeRequest;

pub trait Sink {
    fn initialize(
        &mut self,
        plugin_id: String,
        log_level: LogLevel,
    ) -> Result<InitializeRequest, InitializeError>;

    fn shutdown(&mut self) -> Result<(), ShutdownError>;
    
    fn version(&self) -> String;

    fn consume_event(&mut self, event: SinkEvent) -> Result<(), SinkError>;
}

