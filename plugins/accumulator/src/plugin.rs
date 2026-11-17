//! The SDK's `Transform` around the engine, and the timer task: it closes the
//! windows that ended and sends their outputs, each confirmed by the hub
//! before it leaves the queue (at-least-once).

use crate::engine::Accumulator;
use flwrs_plugin::plugin::core::{InboundEvent, InitializeRequest};
use flwrs_plugin::plugin::error::{InitializeError, ShutdownError, TransformError};
use flwrs_plugin::schema::payload::{PluginPayload, SchemaDefinition};
use flwrs_plugin::schema::protocol::log_level::Enum as LogLevel;
use flwrs_plugin::transform::local_sink::LocalSink;
use flwrs_plugin::transform::plugin::Transform;
use flwrs_script_transform::config::OnError;
use flwrs_script_transform::store::now_ms;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// How often windows are checked for closing.
pub const TICK: Duration = Duration::from_millis(100);
/// Window outputs sent per tick at most.
const SEND_BATCH: usize = 100;

pub type Shared = Arc<Mutex<Accumulator>>;

fn lock(engine: &Shared) -> std::sync::MutexGuard<'_, Accumulator> {
    engine.lock().unwrap_or_else(PoisonError::into_inner)
}

pub struct AccumulatorTransform {
    pub engine: Shared,
    pub input: SchemaDefinition,
    pub output: SchemaDefinition,
    pub on_error: OnError,
    timers: bool,
}

impl AccumulatorTransform {
    pub fn new(
        engine: Accumulator,
        input: SchemaDefinition,
        output: SchemaDefinition,
        on_error: OnError,
    ) -> Self {
        Self {
            engine: Arc::new(Mutex::new(engine)),
            input,
            output,
            on_error,
            timers: false,
        }
    }
}

impl Transform for AccumulatorTransform {
    fn initialize(
        &mut self,
        plugin_id: String,
        _: LogLevel,
        sink: LocalSink,
    ) -> Result<InitializeRequest, InitializeError> {
        if !self.timers {
            self.timers = true;
            tokio::spawn(run_timers(self.engine.clone(), sink));
        }
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
        let result =
            lock(&self.engine).process_at(&event.payload, event.metadata.as_ref(), now_ms());
        match result {
            Ok(outputs) => Ok(outputs),
            Err(e) => match self.on_error {
                OnError::Nack => Err(TransformError::new(e)),
                OnError::NackRetry => Err(TransformError::retryable(e)),
                OnError::Drop => {
                    log::warn!("dropped an event the step failed on: {e}");
                    Ok(vec![])
                }
            },
        }
    }
}

/// Closes due windows and sends queued outputs, every [`TICK`]. A send that
/// fails (disconnected, a NACK) stays queued for the next tick.
async fn run_timers(engine: Shared, sink: LocalSink) {
    loop {
        tokio::time::sleep(TICK).await;
        let batch = {
            let mut engine = lock(&engine);
            if let Err(e) = engine.close_due(now_ms()) {
                log::warn!("closing windows failed (retried): {e}");
            }
            engine.pending(SEND_BATCH)
        };
        let batch = match batch {
            Ok(batch) => batch,
            Err(e) => {
                log::warn!("reading queued window outputs failed: {e}");
                continue;
            }
        };
        let mut sent = vec![];
        for (key, payload) in batch {
            match sink.emit_confirmed(payload).await {
                Ok(()) => sent.push(key),
                Err(e) => {
                    log::debug!("a window output wasn't taken yet ({e}); retried");
                    break;
                }
            }
        }
        if !sent.is_empty()
            && let Err(e) = lock(&engine).sent(&sent)
        {
            log::warn!("removing sent window outputs failed (they may be sent again): {e}");
        }
    }
}
