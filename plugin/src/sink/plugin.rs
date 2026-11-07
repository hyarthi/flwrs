use crate::plugin::core::{InboundEvent, InitializeRequest};
use crate::plugin::error::{InitializeError, ShutdownError, SinkError};
use crate::schema::protocol::log_level::Enum as LogLevel;

/// A plugin that consumes events, e.g. writes them somewhere.
pub trait Sink {
    /// Called once before connecting to the hub. Returns the plugin's
    /// identity and the schema of payloads it accepts.
    fn initialize(
        &mut self,
        plugin_id: String,
        log_level: LogLevel,
    ) -> Result<InitializeRequest, InitializeError>;

    fn shutdown(&mut self) -> Result<(), ShutdownError>;

    fn version(&self) -> String;

    /// Handles one event. The event is acknowledged to the hub when the
    /// future returns `Ok`, so finish the work (e.g. wait for the response)
    /// before returning. An error is logged and the event is rejected: the
    /// hub delivers it again later if the error is [`SinkError::retryable`],
    /// and gives up on it otherwise. The sink keeps running either way.
    ///
    /// The runner handles up to
    /// [`SinkRunnerConfig::max_concurrent`](crate::sink::runner::SinkRunnerConfig::max_concurrent)
    /// events at once (1 by default: one at a time, in order), so this takes
    /// `&self`; keep mutable state behind a `Mutex` or similar. If the
    /// connection drops, unfinished calls are cancelled (the future is
    /// dropped) and the hub delivers those events again.
    ///
    /// The hub may deliver an event more than once (after a crash, a
    /// reconnect or a retry): `event.attempt > 1` says it may be a duplicate,
    /// and `event.metadata.event_id` identifies it.
    #[allow(async_fn_in_trait)]
    async fn consume_event(&self, event: InboundEvent) -> Result<(), SinkError>;
}
