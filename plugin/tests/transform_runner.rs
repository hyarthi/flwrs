mod common;

use common::{FakeHub, connection, listen, payload, text};
use flwrs_plugin::plugin::core::{InboundEvent, InitializeRequest};
use flwrs_plugin::plugin::error::{InitializeError, ShutdownError, TransformError};
use flwrs_plugin::schema::payload::{FieldValue, PluginPayload, field_value::Value};
use flwrs_plugin::schema::protocol::{
    Ack, Credit, Event, Exit, Nack, capability::Enum as Capability, log_level::Enum as LogLevel,
    msg_code::Enum as MsgCode, plugin_type::Enum as PluginType,
};
use flwrs_plugin::transform::local_sink::LocalSink;
use flwrs_plugin::transform::plugin::Transform;
use flwrs_plugin::transform::runner::{TransformRunner, TransformRunnerConfig};

/// Upper-cases every string field. "split" gives two outputs, "drop" none,
/// an event without fields fails.
struct Upper;

impl Transform for Upper {
    fn initialize(
        &mut self,
        id: String,
        _: LogLevel,
        _: LocalSink,
    ) -> Result<InitializeRequest, InitializeError> {
        Ok(InitializeRequest::new().with_id(id))
    }
    fn shutdown(&mut self) -> Result<(), ShutdownError> {
        Ok(())
    }
    fn version(&self) -> String {
        "1".into()
    }
    fn process_event(
        &mut self,
        mut event: InboundEvent,
    ) -> Result<Vec<PluginPayload>, TransformError> {
        if event.payload.fields.is_empty() {
            return Err(TransformError::new("no fields"));
        }
        let mut word = String::new();
        for field in &mut event.payload.fields {
            if let Some(FieldValue {
                value: Some(Value::String(s)),
            }) = &mut field.value
            {
                word = s.clone();
                *s = s.to_uppercase();
            }
        }
        Ok(match word.as_str() {
            "drop" => vec![],
            "split" => vec![event.payload.clone(), event.payload],
            _ => vec![event.payload],
        })
    }
}

fn output(frame: &flwrs_plugin::plugin::frame::Frame) -> (u64, u64, PluginPayload) {
    assert_eq!(frame.code(), MsgCode::Event, "{frame:?}");
    let event: Event = frame.decode().unwrap();
    (frame.header.seq, frame.header.input_seq, event.payload.unwrap())
}

#[tokio::test]
async fn outputs_are_tied_to_their_input_and_followed_by_its_ack() {
    let (listener, port) = listen().await;
    let runner = tokio::spawn(async move {
        let mut runner = TransformRunner::initialize(
            Upper,
            TransformRunnerConfig::new("upper".into(), LogLevel::Warn, connection(port)),
        )
        .await
        .unwrap();
        runner.run().await
    });
    let (mut hub, init) = FakeHub::handshake(&listener).await;
    assert_eq!(init.plugin_type, PluginType::Transform as i32);
    assert!(init.in_schema.is_some() && init.out_schema.is_some());
    hub.accept_plugin(&[Capability::Acks, Capability::Credits], 0).await;
    hub.send(&Credit { events: 100 }).await;

    // One output: tied to input 1, then input 1 is acked.
    hub.send_event(payload(vec![text("text", "hello")]), None).await;
    let (seq, input, out) = output(&hub.next().await);
    assert_eq!((seq, input), (1, 1));
    assert_eq!(out, payload(vec![text("text", "HELLO")]));
    let ack: Ack = hub.expect().await;
    assert_eq!((ack.up_to, ack.seqs), (0, vec![1]));

    // Two outputs for input 2, none for input 3.
    hub.send_event(payload(vec![text("text", "split")]), None).await;
    hub.send_event(payload(vec![text("text", "drop")]), None).await;
    for expected_seq in [2, 3] {
        let (seq, input, out) = output(&hub.next().await);
        assert_eq!((seq, input), (expected_seq, 2));
        assert_eq!(out, payload(vec![text("text", "SPLIT")]));
    }
    let ack: Ack = hub.expect().await;
    assert_eq!((ack.up_to, ack.seqs), (0, vec![2]));
    let ack: Ack = hub.expect().await;
    assert_eq!((ack.up_to, ack.seqs), (0, vec![3]));

    // A failure: NACK, not retryable.
    hub.send_event(payload(vec![]), None).await;
    let nack: Nack = hub.expect().await;
    assert_eq!((nack.seq, nack.retry, nack.reason.as_str()), (4, false, "no fields"));

    hub.shutdown("").await;
    let _: Exit = hub.expect().await;
    common::finished(runner).await.unwrap();
}
