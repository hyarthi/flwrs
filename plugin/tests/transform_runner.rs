mod common;

use common::{FakeHub, connection, listen};
use flwrs_plugin::plugin::core::InitializeRequest;
use flwrs_plugin::plugin::error::{InitializeError, ShutdownError, TransformError};
use flwrs_plugin::schema::common::{EventMetadata, InitializeResponse, log_level::Enum as LogLevel};
use flwrs_plugin::schema::schema::{Field, FieldValue, PluginPayload, field_value::Value};
use flwrs_plugin::schema::transform::{
    RuntimeTransformMessage, TransformEvent, TransformMessage, runtime_transform_message,
    transform_message::Payload,
};
use flwrs_plugin::transform::local_sink::LocalSink;
use flwrs_plugin::transform::plugin::Transform;
use flwrs_plugin::transform::runner::{TransformRunner, TransformRunnerConfig};

/// Upper-cases every string field.
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
        mut event: TransformEvent,
    ) -> Result<TransformEvent, TransformError> {
        let payload = event
            .payload
            .as_mut()
            .ok_or(TransformError::new("no payload"))?;
        for field in &mut payload.fields {
            if let Some(FieldValue {
                value: Some(Value::String(s)),
            }) = &mut field.value
            {
                *s = s.to_uppercase();
            }
        }
        // A fresh output event: the runner must copy the input's metadata.
        Ok(TransformEvent {
            metadata: None,
            ..event
        })
    }
}

fn is_log(msg: &TransformMessage) -> bool {
    matches!(msg.payload, Some(Payload::Log(_)))
}

#[tokio::test]
async fn transform_returns_processed_event() {
    let (listener, port) = listen().await;
    let runner = tokio::spawn(async move {
        let mut runner = TransformRunner::initialize(
            Upper,
            TransformRunnerConfig {
                plugin_id: "upper".into(),
                log_level: LogLevel::Warn,
                hub_connection: connection(port),
            },
        )
        .await
        .unwrap();
        runner.run().await
    });
    let mut hub = FakeHub::accept(&listener).await;
    let init: TransformMessage = hub.recv(is_log).await;
    assert!(matches!(init.payload, Some(Payload::Initialize(_))));
    hub.send(&RuntimeTransformMessage {
        payload: Some(runtime_transform_message::Payload::Initialize(
            InitializeResponse {},
        )),
    })
    .await;

    let field = |s: &str| Field {
        key: "text".into(),
        value: Some(FieldValue {
            value: Some(Value::String(s.into())),
        }),
    };
    let metadata = EventMetadata {
        event_id: "e1".into(),
        trace_id: "t1".into(),
        hops: 2,
        origin_node_id: "src".into(),
        create_time: None,
    };
    hub.send(&RuntimeTransformMessage {
        payload: Some(runtime_transform_message::Payload::Event(TransformEvent {
            plugin_id: "upper".into(),
            plugin_version: "1".into(),
            payload: Some(PluginPayload {
                fields: vec![field("hello")],
            }),
            metadata: Some(metadata.clone()),
        })),
    })
    .await;
    let out: TransformMessage = hub.recv(is_log).await;
    match out.payload {
        Some(Payload::Event(e)) => {
            assert_eq!(e.payload.unwrap().fields, vec![field("HELLO")]);
            // Metadata is carried over from the input event.
            assert_eq!(e.metadata, Some(metadata));
        }
        other => panic!("expected Event, got {other:?}"),
    }

    hub.send(&RuntimeTransformMessage {
        payload: Some(runtime_transform_message::Payload::Event(
            TransformEvent::default(),
        )),
    })
    .await;
    let err: TransformMessage = hub.recv(is_log).await;
    assert!(matches!(err.payload, Some(Payload::Error(e)) if e.message == "no payload"));

    hub.send(&RuntimeTransformMessage {
        payload: Some(runtime_transform_message::Payload::Shutdown(
            flwrs_plugin::schema::common::Shutdown {},
        )),
    })
    .await;
    let exit: TransformMessage = hub.recv(is_log).await;
    assert!(matches!(exit.payload, Some(Payload::Exit(_))));
    runner.await.unwrap().unwrap();
}
