use crate::plugin::core::InitializeRequest;
use crate::plugin::error::{InitializeError, ShutdownError, TransformError};
use crate::schema::transform::TransformEvent;
use tokio::sync::Mutex;

pub trait Transform<'a> {
    fn initialize(
        &mut self,
        plugin_id: String,
        log_level: crate::schema::common::log_level::Enum,
        sink: &'a Mutex<crate::transform::local_sink::LocalSink>,
    ) -> Result<InitializeRequest, InitializeError>;

    fn shutdown(&mut self) -> Result<(), ShutdownError>;

    fn version(&self) -> String;

    fn process_event(&mut self, event: TransformEvent) -> Result<TransformEvent, TransformError>;
}
