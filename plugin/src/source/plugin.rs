use tokio::sync::Mutex;
use crate::plugin::core::InitializeRequest;
use crate::plugin::error::{InitializeError, ShutdownError, SourceError};
use crate::schema::common::log_level::Enum as LogLevel;
use crate::source::local_sink::LocalSink;

pub trait Source<'a> {
    fn initialize(
        &mut self,
        plugin_id: String,
        log_level: LogLevel,
        sink: &'a Mutex<LocalSink>,
    ) -> Result<InitializeRequest, InitializeError>;

    fn shutdown(&mut self) -> Result<(), ShutdownError>;

    fn version(&self) -> String;

    fn run(&self) -> Result<(), SourceError>;
}
