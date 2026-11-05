mod common;

use common::{FakeHub, connection, listen};
use flwrs_plugin::plugin::core::InitializeRequest;
use flwrs_plugin::plugin::error::{InitializeError, ShutdownError, SinkError};
use flwrs_plugin::schema::common::{log_level::Enum as LogLevel, plugin_type};
use flwrs_plugin::schema::schema::field_value::Value;
use flwrs_plugin::schema::sink::{SinkEvent, SinkMessage, sink_message::Payload};
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
    fn consume_event(&mut self, _: SinkEvent) -> Result<(), SinkError> {
        Ok(())
    }
}

#[tokio::test]
async fn forwards_formatted_logs_with_record_level_after_initialize() {
    let (listener, port) = listen().await;
    let _runner = tokio::spawn(async move {
        let mut runner = SinkRunner::initialize(
            NoopSink,
            SinkRunnerConfig {
                plugin_id: "sink-log".into(),
                log_level: LogLevel::Info,
                hub_connection: connection(port),
            },
        )
        .await
        .unwrap();
        log::debug!("filtered out by level");
        runner.run().await
    });
    let mut hub = FakeHub::accept(&listener).await;

    let first: SinkMessage = hub.recv(|_| false).await;
    assert!(
        matches!(first.payload, Some(Payload::Initialize(_))),
        "Initialize must be the first frame, got {first:?}"
    );
    let msg: SinkMessage = hub.recv(|_| false).await;
    let Some(Payload::Log(event)) = msg.payload else {
        panic!("expected a log message, got {msg:?}");
    };
    assert_eq!(event.plugin_id, "sink-log");
    assert_eq!(event.plugin_type, plugin_type::Enum::Sink as i32);
    assert_eq!(event.log_level, LogLevel::Warn as i32);
    assert_eq!(event.message, "retrying 3 of 5");
    assert_eq!(event.details.len(), 1);
    assert_eq!(event.details[0].key, "attempt");
    assert_eq!(
        event.details[0].value.as_ref().unwrap().value,
        Some(Value::String("3".into()))
    );
}
