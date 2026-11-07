mod common;

use common::{FakeHub, connection, listen};
use flwrs_plugin::plugin::core::{InboundEvent, InitializeRequest};
use flwrs_plugin::plugin::error::{InitializeError, ShutdownError, SinkError};
use flwrs_plugin::schema::payload::field_value::Value;
use flwrs_plugin::schema::protocol::{LogEvent, log_level::Enum as LogLevel};
use flwrs_plugin::sink::plugin::Sink;
use flwrs_plugin::sink::runner::{SinkRunner, SinkRunnerConfig};

struct NoopSink;

impl Sink for NoopSink {
    fn initialize(&mut self, _: String, _: LogLevel) -> Result<InitializeRequest, InitializeError> {
        // Logged before Initialize is sent: must still arrive after it.
        log::warn!(attempt = 3; "retrying {} of {}", 3, 5);
        Ok(InitializeRequest::new())
    }
    fn shutdown(&mut self) -> Result<(), ShutdownError> {
        Ok(())
    }
    fn version(&self) -> String {
        "1".into()
    }
    async fn consume_event(&self, _: InboundEvent) -> Result<(), SinkError> {
        Ok(())
    }
}

#[tokio::test]
async fn forwards_formatted_logs_with_record_level_after_initialize() {
    let (listener, port) = listen().await;
    let _runner = tokio::spawn(async move {
        let mut runner = SinkRunner::initialize(
            NoopSink,
            SinkRunnerConfig::new("sink-log".into(), LogLevel::Info, connection(port)),
        )
        .await
        .unwrap();
        log::debug!("filtered out by level");
        runner.run().await
    });
    // Initialize must be the first frame.
    let (mut hub, _) = FakeHub::handshake(&listener).await;
    let event: LogEvent = hub.frame().await.decode().unwrap();
    assert_eq!(event.level, LogLevel::Warn as i32);
    assert_eq!(event.message, "retrying 3 of 5");
    assert_eq!(event.details.len(), 1);
    assert_eq!(event.details[0].key, "attempt");
    assert_eq!(
        event.details[0].value.as_ref().unwrap().value,
        Some(Value::String("3".into()))
    );
}
