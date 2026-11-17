//! The SDK's `Transform`, around the engine.

use crate::config::OnError;
use crate::engine::Engine;
use flwrs_plugin::plugin::core::{InboundEvent, InitializeRequest};
use flwrs_plugin::plugin::error::{InitializeError, ShutdownError, TransformError};
use flwrs_plugin::schema::payload::{PluginPayload, SchemaDefinition};
use flwrs_plugin::schema::protocol::log_level::Enum as LogLevel;
use flwrs_plugin::transform::local_sink::LocalSink;
use flwrs_plugin::transform::plugin::Transform;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub struct ScriptTransform {
    pub engine: Engine,
    pub input: SchemaDefinition,
    pub output: SchemaDefinition,
    pub on_error: OnError,
}

impl Transform for ScriptTransform {
    fn initialize(
        &mut self,
        plugin_id: String,
        _: LogLevel,
        _: LocalSink,
    ) -> Result<InitializeRequest, InitializeError> {
        Ok(InitializeRequest::new()
            .with_id(plugin_id)
            .with_version(VERSION.to_string())
            .with_in_schema(self.input.clone().into())
            .with_out_schema(self.output.clone().into()))
    }

    fn shutdown(&mut self) -> Result<(), ShutdownError> {
        Ok(())
    }

    fn version(&self) -> String {
        VERSION.to_string()
    }

    fn process_event(&mut self, event: InboundEvent) -> Result<Vec<PluginPayload>, TransformError> {
        match self.engine.process(&event.payload, event.metadata.as_ref()) {
            Ok(outputs) => Ok(outputs),
            Err(e) => match self.on_error {
                OnError::Nack => Err(TransformError::new(e)),
                OnError::NackRetry => Err(TransformError::retryable(e)),
                OnError::Drop => {
                    log::warn!("dropped an event the script failed on: {e}");
                    Ok(vec![])
                }
            },
        }
    }
}
