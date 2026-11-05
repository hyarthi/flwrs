use crate::plugin::msg_client::{HubClient, Stage};
use crate::schema::common::{
    LogEvent, log_level::Enum as LogLevel, plugin_type::Enum as PluginType,
};
use crate::schema::schema::{Field, FieldValue, field_value::Value};
use crate::schema::{sink, source, transform};
use log::{Level, Metadata, Record, SetLoggerError};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::Duration;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

/// Records waiting to be sent. When full, the oldest record is dropped, and
/// the number dropped is reported with the next record sent.
const LOG_QUEUE_CAPACITY: usize = 10_000;

static LOGGER: RwLock<Option<PluginLogger>> = RwLock::new(None);
static LOG_WRAPPER: LogWrapper = LogWrapper;

/// Forwards plugin log records to the hub as `LogEvent`s.
///
/// Records go into a bounded queue and are sent in order by one background
/// task, so logging never blocks and never needs an async context. While the
/// hub is unreachable they wait in the queue (logs may only be sent after
/// `Initialize`), and are delivered after the reconnect.
pub(crate) struct PluginLogger {
    plugin_id: String,
    plugin_type: PluginType,
    level: Level,
    shared: Arc<Shared>,
}

struct Shared {
    queue: Mutex<LogQueue>,
    /// A record was queued.
    queued: Notify,
    /// The queue became empty and nothing is being sent.
    idle: Notify,
    /// The runner finished; the forwarder stops and records go to stderr.
    closed: CancellationToken,
}

#[derive(Default)]
struct LogQueue {
    events: VecDeque<LogEvent>,
    dropped: u64,
    sending: bool,
}

impl LogQueue {
    fn push(&mut self, event: LogEvent, capacity: usize) {
        if self.events.len() >= capacity {
            self.events.pop_front();
            self.dropped += 1;
        }
        self.events.push_back(event);
    }

    /// The next record, preceded by a WARN about dropped records if any.
    fn pop(&mut self, template: &LogEvent) -> Vec<LogEvent> {
        let Some(event) = self.events.pop_front() else {
            return vec![];
        };
        let mut out = vec![];
        if self.dropped > 0 {
            out.push(LogEvent {
                log_level: LogLevel::Warn as i32,
                message: format!(
                    "{} log records were dropped because the log queue was full",
                    self.dropped
                ),
                details: vec![],
                ..template.clone()
            });
            self.dropped = 0;
        }
        out.push(event);
        out
    }

    fn is_idle(&self) -> bool {
        self.events.is_empty() && !self.sending
    }
}

impl PluginLogger {
    /// Installs the plugin logger, sending through `client`. Must be called
    /// from within a tokio runtime. Calling it again replaces the previous
    /// settings and connection; installing it while another `log` logger is
    /// set fails.
    pub(crate) fn initialize(
        id: &str,
        plugin_type: PluginType,
        log_level: Level,
        client: Arc<HubClient>,
    ) -> Result<(), SetLoggerError> {
        let mut installed = LOGGER.write().unwrap_or_else(PoisonError::into_inner);
        match installed.take() {
            Some(old) => old.shared.closed.cancel(),
            None => log::set_logger(&LOG_WRAPPER)?,
        }
        log::set_max_level(log_level.to_level_filter());

        let shared = Arc::new(Shared {
            queue: Mutex::new(LogQueue::default()),
            queued: Notify::new(),
            idle: Notify::new(),
            closed: CancellationToken::new(),
        });
        let template = LogEvent {
            plugin_id: id.to_string(),
            plugin_type: plugin_type as i32,
            ..Default::default()
        };
        tokio::spawn(forward_logs(shared.clone(), client, plugin_type, template));
        *installed = Some(PluginLogger {
            plugin_id: id.to_string(),
            plugin_type,
            level: log_level,
            shared,
        });
        Ok(())
    }

    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= self.level
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let message = record.args().to_string();
        if self.shared.closed.is_cancelled() {
            eprintln!("[{}] {}: {message}", self.plugin_id, record.level());
            return;
        }
        let mut details = FieldCollector(vec![]);
        let _ = record.key_values().visit(&mut details);
        let log_level: LogLevel = record.level().into();
        let event = LogEvent {
            plugin_id: self.plugin_id.clone(),
            plugin_type: self.plugin_type as i32,
            log_level: log_level as i32,
            message,
            details: details.0,
        };
        lock(&self.shared.queue).push(event, LOG_QUEUE_CAPACITY);
        self.shared.queued.notify_one();
    }
}

fn lock(queue: &Mutex<LogQueue>) -> std::sync::MutexGuard<'_, LogQueue> {
    queue.lock().unwrap_or_else(PoisonError::into_inner)
}

fn current_shared() -> Option<Arc<Shared>> {
    LOGGER
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .as_ref()
        .map(|logger| logger.shared.clone())
}

/// Waits (up to `timeout`) until every queued record has been sent.
pub(crate) async fn flush(timeout: Duration) {
    let Some(shared) = current_shared() else {
        return;
    };
    let _ = tokio::time::timeout(timeout, async {
        loop {
            let idle = shared.idle.notified();
            tokio::pin!(idle);
            idle.as_mut().enable();
            if lock(&shared.queue).is_idle() {
                return;
            }
            idle.await;
        }
    })
    .await;
}

/// Stops forwarding: records still queued, and every later record, go to stderr.
pub(crate) fn close() {
    let Some(shared) = current_shared() else {
        return;
    };
    shared.closed.cancel();
    let mut queue = lock(&shared.queue);
    for event in queue.events.drain(..) {
        eprintln!("[{}] {}", event.plugin_id, event.message);
    }
}

async fn forward_logs(
    shared: Arc<Shared>,
    client: Arc<HubClient>,
    plugin_type: PluginType,
    template: LogEvent,
) {
    loop {
        // Logs may only be sent once `Initialize` is on the wire.
        tokio::select! {
            _ = shared.closed.cancelled() => return,
            _ = client.wait_for(Stage::Initializing) => {}
        }
        let batch = {
            let mut queue = lock(&shared.queue);
            let batch = queue.pop(&template);
            queue.sending = !batch.is_empty();
            batch
        };
        if batch.is_empty() {
            shared.idle.notify_waiters();
            tokio::select! {
                _ = shared.closed.cancelled() => return,
                _ = shared.queued.notified() => continue,
            }
        }
        for event in batch {
            if let Err(e) = send_log(&client, plugin_type, event.clone()).await {
                // Can't use `log` here: that would loop back into this logger.
                eprintln!(
                    "Failed to send log message to hub: {e}; message: {}",
                    event.message
                );
            }
        }
        lock(&shared.queue).sending = false;
    }
}

async fn send_log(
    client: &HubClient,
    plugin_type: PluginType,
    event: LogEvent,
) -> std::io::Result<()> {
    match plugin_type {
        PluginType::Source => {
            let msg = source::SourceMessage {
                payload: Some(source::source_message::Payload::Log(event)),
            };
            client.send(&msg, Stage::Initializing).await
        }
        PluginType::Transform => {
            let msg = transform::TransformMessage {
                payload: Some(transform::transform_message::Payload::Log(event)),
            };
            client.send(&msg, Stage::Initializing).await
        }
        PluginType::Sink | PluginType::Undefined => {
            let msg = sink::SinkMessage {
                payload: Some(sink::sink_message::Payload::Log(event)),
            };
            client.send(&msg, Stage::Initializing).await
        }
    }
}

/// Collects structured `log` key-values into payload fields.
struct FieldCollector(Vec<Field>);

impl<'kvs> log::kv::VisitSource<'kvs> for FieldCollector {
    fn visit_pair(
        &mut self,
        key: log::kv::Key<'kvs>,
        value: log::kv::Value<'kvs>,
    ) -> Result<(), log::kv::Error> {
        self.0.push(Field {
            key: key.to_string(),
            value: Some(FieldValue {
                value: Some(Value::String(value.to_string())),
            }),
        });
        Ok(())
    }
}

impl From<Level> for LogLevel {
    fn from(level: Level) -> Self {
        match level {
            Level::Error => LogLevel::Error,
            Level::Warn => LogLevel::Warn,
            Level::Info => LogLevel::Info,
            Level::Debug => LogLevel::Debug,
            Level::Trace => LogLevel::Trace,
        }
    }
}

impl From<LogLevel> for Level {
    fn from(value: LogLevel) -> Self {
        match value {
            LogLevel::Undefined => Self::Warn,
            LogLevel::Trace => Self::Trace,
            LogLevel::Debug => Self::Debug,
            LogLevel::Info => Self::Info,
            LogLevel::Warn => Self::Warn,
            LogLevel::Error => Self::Error,
        }
    }
}

struct LogWrapper;

impl log::Log for LogWrapper {
    fn enabled(&self, metadata: &Metadata) -> bool {
        LOGGER
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .is_some_and(|logger| logger.enabled(metadata))
    }

    fn log(&self, record: &Record) {
        if let Some(logger) = LOGGER.read().unwrap_or_else(PoisonError::into_inner).as_ref() {
            logger.log(record);
        }
    }

    fn flush(&self) {
        // noop: records are sent as soon as the forwarding task picks them up
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(message: &str) -> LogEvent {
        LogEvent {
            message: message.into(),
            ..Default::default()
        }
    }

    #[test]
    fn full_queue_drops_oldest_and_reports_the_count() {
        let mut queue = LogQueue::default();
        for i in 0..5 {
            queue.push(event(&i.to_string()), 3);
        }
        let template = event("");
        let first = queue.pop(&template);
        assert_eq!(first.len(), 2);
        assert_eq!(first[0].log_level, LogLevel::Warn as i32);
        assert!(first[0].message.starts_with("2 log records were dropped"));
        assert_eq!(first[1].message, "2");
        let rest: Vec<String> = [queue.pop(&template), queue.pop(&template)]
            .concat()
            .into_iter()
            .map(|e| e.message)
            .collect();
        assert_eq!(rest, ["3", "4"]);
        assert!(queue.pop(&template).is_empty());
        assert!(queue.is_idle());
    }
}
