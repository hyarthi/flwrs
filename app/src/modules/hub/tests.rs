//! Hub tests: a hand-rolled fake plugin for exact protocol cases, and the real
//! Rust SDK (in a child process, like a real plugin) for the end-to-end ones.

use super::HubConfig;
use super::service::{ExpectedNode, Hub, HubError, HubEvent, NodeLink};
use super::wire::{DataEvent, RuntimeMsg};
use crate::modules::plugin::manifest::PluginType;
use flwrs_plugin::plugin::frame::{Frame, WireMessage, read_frame, write_frame, write_message};
use flwrs_plugin::schema::common::{
    EventMetadata, Exit, LogEvent, exit_code, msg_code::Enum as MsgCode,
};
use flwrs_plugin::schema::schema::{
    Field, FieldDefinition, FieldValue, PluginPayload, SchemaDefinition, field_type,
    field_value::Value,
};
use flwrs_plugin::schema::{sink, source, transform};
use prost::Message;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio_util::sync::CancellationToken;

const TOKEN: &str = "a-sufficiently-long-token";
const WAIT: Duration = Duration::from_secs(5);

fn text_schema(key: &str) -> SchemaDefinition {
    SchemaDefinition {
        fields: vec![FieldDefinition {
            key: key.into(),
            r#type: field_type::Enum::String as i32,
            ..Default::default()
        }],
    }
}

fn text_payload(text: &str) -> PluginPayload {
    PluginPayload {
        fields: vec![Field {
            key: "text".into(),
            value: Some(FieldValue {
                value: Some(Value::String(text.into())),
            }),
        }],
    }
}

fn expected(node_id: &str, plugin_type: PluginType) -> ExpectedNode {
    let (in_schema, out_schema) = match plugin_type {
        PluginType::Source => (None, Some(text_schema("text"))),
        PluginType::Sink => (Some(text_schema("text")), None),
        PluginType::Transform => (Some(text_schema("text")), Some(text_schema("text"))),
    };
    ExpectedNode {
        scene_id: "scene-1".into(),
        node_id: node_id.into(),
        instance_id: node_id.into(),
        plugin_id: "test-plugin".into(),
        plugin_type,
        plugin_version: "1.0.0".into(),
        in_schema,
        out_schema,
        token: TOKEN.into(),
        max_event_bytes: None,
    }
}

struct TestHub {
    hub: Hub,
    stop: CancellationToken,
}

impl Drop for TestHub {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

async fn start_hub(config: HubConfig) -> TestHub {
    let hub = Hub::bind(config).await.unwrap();
    let stop = CancellationToken::new();
    let running = hub.clone();
    let token = stop.clone();
    tokio::spawn(async move { running.run(token).await });
    TestHub { hub, stop }
}

fn config() -> HubConfig {
    HubConfig {
        handshake_timeout_ms: 300,
        ..HubConfig::default()
    }
}

async fn next(link: &mut NodeLink) -> HubEvent {
    tokio::time::timeout(WAIT, link.recv())
        .await
        .expect("timed out waiting for a hub event")
        .expect("link closed")
}

async fn assert_quiet(link: &mut NodeLink) {
    let quiet = tokio::time::timeout(Duration::from_millis(200), link.recv()).await;
    assert!(quiet.is_err(), "unexpected event: {quiet:?}");
}

/// A plugin speaking raw frames.
struct FakePlugin {
    stream: TcpStream,
}

impl FakePlugin {
    async fn connect(hub: &Hub) -> Self {
        Self {
            stream: TcpStream::connect(hub.local_addr()).await.unwrap(),
        }
    }

    async fn send<M: WireMessage>(&mut self, msg: &M) {
        write_message(&mut self.stream, msg).await.unwrap();
    }

    async fn send_raw(&mut self, code: MsgCode, body: &[u8]) {
        write_frame(&mut self.stream, code, body).await.unwrap();
    }

    async fn recv(&mut self) -> Option<Frame> {
        tokio::time::timeout(WAIT, read_frame(&mut self.stream))
            .await
            .expect("timed out waiting for the hub")
            .ok()
            .flatten()
    }

    async fn recv_msg<M: WireMessage>(&mut self) -> M {
        self.recv()
            .await
            .expect("hub closed the connection")
            .decode()
            .unwrap()
    }

    async fn expect_closed(&mut self) {
        while self.recv().await.is_some() {}
    }
}

fn sink_init(id: &str, version: &str, token: &str, schema: SchemaDefinition) -> sink::SinkMessage {
    sink::SinkMessage {
        payload: Some(sink::sink_message::Payload::Initialize(sink::Initialize {
            plugin_id: id.into(),
            plugin_version: version.into(),
            schema: Some(schema),
            token: token.into(),
        })),
    }
}

fn source_init(id: &str) -> source::SourceMessage {
    source::SourceMessage {
        payload: Some(source::source_message::Payload::Initialize(
            source::Initialize {
                plugin_id: id.into(),
                plugin_version: "1.0.0".into(),
                schema: Some(text_schema("text")),
                token: TOKEN.into(),
            },
        )),
    }
}

fn source_event(text: &str) -> source::SourceMessage {
    source::SourceMessage {
        payload: Some(source::source_message::Payload::Event(
            source::SourceEvent {
                plugin_id: "src".into(),
                plugin_version: "1.0.0".into(),
                payload: Some(text_payload(text)),
                metadata: None,
            },
        )),
    }
}

fn is_accept(msg: &sink::RuntimeSinkMessage) -> bool {
    matches!(
        msg.payload,
        Some(sink::runtime_sink_message::Payload::Initialize(_))
    )
}

fn is_shutdown(msg: &sink::RuntimeSinkMessage) -> bool {
    matches!(
        msg.payload,
        Some(sink::runtime_sink_message::Payload::Shutdown(_))
    )
}

/// Connects a sink with valid credentials and checks it's accepted.
async fn connected_sink(hub: &Hub, link: &mut NodeLink) -> (FakePlugin, u64) {
    let mut plugin = FakePlugin::connect(hub).await;
    plugin
        .send(&sink_init("sink", "1.0.0", TOKEN, text_schema("text")))
        .await;
    assert!(is_accept(&plugin.recv_msg().await));
    match next(link).await {
        HubEvent::Connected { conn_id, .. } => (plugin, conn_id),
        other => panic!("expected Connected, got {other:?}"),
    }
}

// --- handshake ---

#[tokio::test]
async fn accepts_a_valid_plugin_and_relays_both_directions() {
    let hub = start_hub(config()).await;
    let mut link = hub.hub.expect(expected("sink", PluginType::Sink));
    assert!(!link.is_connected());
    let (mut plugin, conn_id) = connected_sink(&hub.hub, &mut link).await;
    assert!(link.is_connected());

    let event = DataEvent {
        instance_id: "sink".into(),
        plugin_version: "1.0.0".into(),
        payload: Some(text_payload("hi")),
        metadata: Some(EventMetadata {
            trace_id: "t".into(),
            hops: 1,
            ..Default::default()
        }),
    };
    link.send(RuntimeMsg::Event(event.clone())).await.unwrap();
    let got: sink::RuntimeSinkMessage = plugin.recv_msg().await;
    match got.payload {
        Some(sink::runtime_sink_message::Payload::Event(e)) => {
            assert_eq!(e.payload, event.payload);
            assert_eq!(e.metadata, event.metadata);
        }
        other => panic!("expected an event, got {other:?}"),
    }

    // Logs and errors are only written to the server log.
    plugin
        .send(&sink::SinkMessage {
            payload: Some(sink::sink_message::Payload::Log(LogEvent {
                message: "hello from the plugin".into(),
                ..Default::default()
            })),
        })
        .await;

    link.send(RuntimeMsg::Shutdown).await.unwrap();
    assert!(is_shutdown(&plugin.recv_msg().await));
    let exit = Exit {
        code: exit_code::Enum::Ok as i32,
        message: String::new(),
    };
    plugin
        .send(&sink::SinkMessage {
            payload: Some(sink::sink_message::Payload::Exit(exit.clone())),
        })
        .await;
    assert_eq!(next(&mut link).await, HubEvent::Exit { conn_id, exit });

    drop(plugin);
    match next(&mut link).await {
        HubEvent::Disconnected {
            conn_id: id,
            reason,
        } => {
            assert_eq!(id, conn_id);
            assert!(reason.contains("closed by the plugin"), "{reason}");
        }
        other => panic!("expected Disconnected, got {other:?}"),
    }
    assert!(!link.is_connected());
    assert_eq!(
        link.send(RuntimeMsg::Shutdown).await,
        Err(HubError::NotConnected)
    );
}

#[tokio::test]
async fn unknown_instance_and_wrong_token_get_shutdown_and_no_event() {
    let hub = start_hub(config()).await;
    let mut link = hub.hub.expect(expected("sink", PluginType::Sink));

    for (id, token) in [
        ("nobody", TOKEN),
        ("sink", "wrong-token-wrong-token"),
        ("sink", ""),
    ] {
        let mut plugin = FakePlugin::connect(&hub.hub).await;
        plugin
            .send(&sink_init(id, "1.0.0", token, text_schema("text")))
            .await;
        assert!(is_shutdown(&plugin.recv_msg().await), "{id}/{token}");
        plugin.expect_closed().await;
    }
    assert_quiet(&mut link).await;
}

#[tokio::test]
async fn mismatched_declarations_are_rejected_with_reasons() {
    let hub = start_hub(config()).await;
    let mut link = hub.hub.expect(expected("sink", PluginType::Sink));

    let cases: Vec<(sink::SinkMessage, &str)> = vec![
        (
            sink_init("sink", "2.0.0", TOKEN, text_schema("text")),
            "version: 1.0.0 in the registry",
        ),
        (
            sink_init("sink", "1.0.0", TOKEN, text_schema("body")),
            "in.body: not in manifest",
        ),
        (
            sink_init(
                "sink",
                "1.0.0",
                TOKEN,
                SchemaDefinition {
                    fields: vec![FieldDefinition {
                        key: "text".into(),
                        ..Default::default()
                    }],
                },
            ),
            "in: ",
        ),
    ];
    for (init, expected_reason) in cases {
        let mut plugin = FakePlugin::connect(&hub.hub).await;
        plugin.send(&init).await;
        assert!(is_shutdown(&plugin.recv_msg().await));
        plugin.expect_closed().await;
        match next(&mut link).await {
            HubEvent::Rejected { reasons } => assert!(
                reasons.iter().any(|r| r.contains(expected_reason)),
                "{expected_reason:?} not in {reasons:?}"
            ),
            other => panic!("expected Rejected, got {other:?}"),
        }
    }

    // A source pretending to be the sink node: wrong type, answered in its own type.
    let mut plugin = FakePlugin::connect(&hub.hub).await;
    plugin.send(&source_init("sink")).await;
    let reply: source::RuntimeSourceMessage = plugin.recv_msg().await;
    assert!(matches!(
        reply.payload,
        Some(source::runtime_source_message::Payload::Shutdown(_))
    ));
    match next(&mut link).await {
        HubEvent::Rejected { reasons } => {
            assert_eq!(
                reasons,
                ["type: SINK in the registry, SOURCE from the plugin"]
            )
        }
        other => panic!("expected Rejected, got {other:?}"),
    }
    assert!(!link.is_connected());
}

#[tokio::test]
async fn bad_first_frames_are_closed_without_reply() {
    let hub = start_hub(HubConfig {
        max_handshake_bytes: 1024,
        ..config()
    })
    .await;
    let mut link = hub.hub.expect(expected("sink", PluginType::Sink));

    // Not an Initialize.
    let mut plugin = FakePlugin::connect(&hub.hub).await;
    plugin
        .send(&sink::SinkMessage {
            payload: Some(sink::sink_message::Payload::Log(LogEvent::default())),
        })
        .await;
    assert!(plugin.recv().await.is_none());

    // A runtime -> plugin code.
    let mut plugin = FakePlugin::connect(&hub.hub).await;
    plugin.send_raw(MsgCode::RuntimeSinkMessage, &[]).await;
    assert!(plugin.recv().await.is_none());

    // Garbage instead of a frame header.
    let mut plugin = FakePlugin::connect(&hub.hub).await;
    tokio::io::AsyncWriteExt::write_all(
        &mut plugin.stream,
        &[10, 0, 0, 0, 2, 0, 0, 0, 1, 2, 3, 4, 5, 6],
    )
    .await
    .unwrap();
    assert!(plugin.recv().await.is_none());

    // Too large for a handshake.
    let mut plugin = FakePlugin::connect(&hub.hub).await;
    let mut init = sink_init("sink", "1.0.0", TOKEN, text_schema("text")).encode_to_vec();
    init.resize(4096, 0);
    plugin.send_raw(MsgCode::SinkMessage, &init).await;
    assert!(plugin.recv().await.is_none());

    // Silent: closed after the handshake timeout.
    let mut plugin = FakePlugin::connect(&hub.hub).await;
    let started = std::time::Instant::now();
    assert!(plugin.recv().await.is_none());
    assert!(started.elapsed() >= Duration::from_millis(250));

    assert_quiet(&mut link).await;
}

// --- running ---

#[tokio::test]
async fn source_events_reach_the_link_and_oversized_ones_are_dropped() {
    let hub = start_hub(config()).await;
    let mut node = expected("src", PluginType::Source);
    node.max_event_bytes = Some(64);
    let mut link = hub.hub.expect(node);

    let mut plugin = FakePlugin::connect(&hub.hub).await;
    plugin.send(&source_init("src")).await;
    let accept: source::RuntimeSourceMessage = plugin.recv_msg().await;
    assert!(matches!(
        accept.payload,
        Some(source::runtime_source_message::Payload::Initialize(_))
    ));
    assert!(matches!(next(&mut link).await, HubEvent::Connected { .. }));

    plugin.send(&source_event(&"x".repeat(200))).await; // over the node's limit
    plugin.send(&source_event("small")).await;
    match next(&mut link).await {
        HubEvent::Event { event, size } => {
            assert_eq!(event.payload, Some(text_payload("small")));
            assert_eq!(size, source_event("small").encoded_len());
        }
        other => panic!("expected the small event, got {other:?}"),
    }

    // Sources don't receive events.
    assert!(matches!(
        link.send(RuntimeMsg::Event(DataEvent::default())).await,
        Err(HubError::Invalid(_))
    ));
}

#[tokio::test]
async fn wrong_msg_code_after_handshake_closes_the_connection() {
    let hub = start_hub(config()).await;
    let mut link = hub.hub.expect(expected("sink", PluginType::Sink));
    let (mut plugin, conn_id) = connected_sink(&hub.hub, &mut link).await;

    plugin
        .send(&transform::TransformMessage { payload: None })
        .await;
    plugin.expect_closed().await;
    match next(&mut link).await {
        HubEvent::Disconnected {
            conn_id: id,
            reason,
        } => {
            assert_eq!(id, conn_id);
            assert!(reason.contains("protocol violation"), "{reason}");
        }
        other => panic!("expected Disconnected, got {other:?}"),
    }
}

#[tokio::test]
async fn newest_connection_wins() {
    let hub = start_hub(config()).await;
    let mut link = hub.hub.expect(expected("sink", PluginType::Sink));
    let (mut first, first_id) = connected_sink(&hub.hub, &mut link).await;
    let (mut second, second_id) = connected_sink(&hub.hub, &mut link).await;
    assert_ne!(first_id, second_id);

    first.expect_closed().await;
    // The hub closed the old one itself: no Disconnected for it.
    assert_quiet(&mut link).await;
    link.send(RuntimeMsg::Shutdown).await.unwrap();
    assert!(is_shutdown(&second.recv_msg().await));
}

#[tokio::test]
async fn dropping_or_replacing_the_link_closes_the_connection() {
    let hub = start_hub(config()).await;
    let mut link = hub.hub.expect(expected("sink", PluginType::Sink));
    let (mut plugin, _) = connected_sink(&hub.hub, &mut link).await;
    drop(link);
    plugin.expect_closed().await;

    // No longer expected.
    let mut plugin = FakePlugin::connect(&hub.hub).await;
    plugin
        .send(&sink_init("sink", "1.0.0", TOKEN, text_schema("text")))
        .await;
    assert!(is_shutdown(&plugin.recv_msg().await));

    let mut old = hub.hub.expect(expected("sink", PluginType::Sink));
    let (mut plugin, _) = connected_sink(&hub.hub, &mut old).await;
    let mut new = hub.hub.expect(expected("sink", PluginType::Sink));
    plugin.expect_closed().await;
    assert!(old.recv().await.is_none(), "the replaced link is closed");
    // Dropping the replaced link must not remove the new expectation.
    drop(old);
    connected_sink(&hub.hub, &mut new).await;
}

#[tokio::test]
async fn renewing_the_token_closes_the_connection_and_refuses_the_old_token() {
    let hub = start_hub(config()).await;
    let mut link = hub.hub.expect(expected("sink", PluginType::Sink));
    let sender = link.sender();
    let (mut plugin, _) = connected_sink(&hub.hub, &mut link).await;

    link.renew_token("the-next-launch-token".into());
    plugin.expect_closed().await;
    assert!(!sender.is_connected());

    let mut stale = FakePlugin::connect(&hub.hub).await;
    stale
        .send(&sink_init("sink", "1.0.0", TOKEN, text_schema("text")))
        .await;
    assert!(is_shutdown(&stale.recv_msg().await));

    let mut fresh = FakePlugin::connect(&hub.hub).await;
    fresh
        .send(&sink_init(
            "sink",
            "1.0.0",
            "the-next-launch-token",
            text_schema("text"),
        ))
        .await;
    assert!(is_accept(&fresh.recv_msg().await));
    assert!(matches!(next(&mut link).await, HubEvent::Connected { .. }));
    // The sender cloned before the renewal still reaches the plugin.
    sender.send(RuntimeMsg::Shutdown).await.unwrap();
    assert!(is_shutdown(&fresh.recv_msg().await));
}

#[tokio::test]
async fn nodes_sharing_a_remote_id_are_told_apart_by_token() {
    let hub = start_hub(config()).await;
    let shared = |node_id: &str, token: &str| ExpectedNode {
        instance_id: "gateway-1".into(),
        token: token.into(),
        ..expected(node_id, PluginType::Sink)
    };
    let mut first = hub.hub.expect(shared("node-a", "token-for-node-a-0123"));
    let mut second = hub.hub.expect(shared("node-b", "token-for-node-b-0123"));

    let mut open = vec![]; // keeps the connections up for the test
    for (token, link) in [
        ("token-for-node-b-0123", &mut second),
        ("token-for-node-a-0123", &mut first),
    ] {
        let mut plugin = FakePlugin::connect(&hub.hub).await;
        plugin
            .send(&sink_init("gateway-1", "1.0.0", token, text_schema("text")))
            .await;
        assert!(is_accept(&plugin.recv_msg().await));
        assert!(matches!(next(link).await, HubEvent::Connected { .. }));
        open.push(plugin);
    }
    assert!(first.is_connected() && second.is_connected());

    // The node id itself isn't what a remote plugin sends.
    let mut by_node_id = FakePlugin::connect(&hub.hub).await;
    by_node_id
        .send(&sink_init(
            "node-a",
            "1.0.0",
            "token-for-node-a-0123",
            text_schema("text"),
        ))
        .await;
    assert!(is_shutdown(&by_node_id.recv_msg().await));
    let mut wrong_token = FakePlugin::connect(&hub.hub).await;
    wrong_token
        .send(&sink_init(
            "gateway-1",
            "1.0.0",
            "token-for-nobody-0123",
            text_schema("text"),
        ))
        .await;
    assert!(is_shutdown(&wrong_token.recv_msg().await));
}

#[tokio::test]
async fn stopping_the_hub_closes_connections() {
    let hub = start_hub(config()).await;
    let mut link = hub.hub.expect(expected("sink", PluginType::Sink));
    let (mut plugin, _) = connected_sink(&hub.hub, &mut link).await;
    hub.stop.cancel();
    plugin.expect_closed().await;
}

// --- the real SDK, in a child process ---

const CHILD_TEST: &str = "modules::hub::tests::sdk_transform_child";
const PORT_ENV: &str = "FLWRS_HUB_TEST_PORT";
const EXPECT_ENV: &str = "FLWRS_HUB_TEST_EXPECT";

/// Upper-cases the `text` field.
struct Upper;

impl flwrs_plugin::transform::plugin::Transform for Upper {
    fn initialize(
        &mut self,
        id: String,
        _: flwrs_plugin::schema::common::log_level::Enum,
        _: flwrs_plugin::transform::local_sink::LocalSink,
    ) -> Result<
        flwrs_plugin::plugin::core::InitializeRequest,
        flwrs_plugin::plugin::error::InitializeError,
    > {
        use flwrs_plugin::plugin::core::{FieldDefinition, InitializeRequest, SchemaDefinition};
        let schema = SchemaDefinition::new().add_field(
            FieldDefinition::new()
                .with_key("text".into())
                .with_type(field_type::Enum::String),
        );
        Ok(InitializeRequest::new()
            .with_id(id)
            .with_version("1.0.0".into())
            .with_in_schema(schema.clone())
            .with_out_schema(schema))
    }

    fn shutdown(&mut self) -> Result<(), flwrs_plugin::plugin::error::ShutdownError> {
        Ok(())
    }

    fn version(&self) -> String {
        "1.0.0".into()
    }

    fn process_event(
        &mut self,
        event: transform::TransformEvent,
    ) -> Result<transform::TransformEvent, flwrs_plugin::plugin::error::TransformError> {
        let mut payload = event.payload.unwrap_or_default();
        for field in &mut payload.fields {
            if let Some(FieldValue {
                value: Some(Value::String(s)),
            }) = &mut field.value
            {
                *s = s.to_uppercase();
            }
        }
        Ok(transform::TransformEvent {
            plugin_id: event.plugin_id,
            plugin_version: event.plugin_version,
            payload: Some(payload),
            metadata: None, // the runner copies the input's
        })
    }
}

/// Runs only as a child of the tests below (it's ignored otherwise): a
/// transform plugin built on the SDK, connecting to the parent's hub.
#[tokio::test]
#[ignore = "runs as a child process of the hub SDK tests"]
async fn sdk_transform_child() {
    let (Ok(port), Ok(expect)) = (std::env::var(PORT_ENV), std::env::var(EXPECT_ENV)) else {
        return;
    };
    use flwrs_plugin::transform::runner::{TransformRunner, TransformRunnerConfig};
    let mut runner = TransformRunner::initialize(
        Upper,
        TransformRunnerConfig {
            plugin_id: "upper".into(),
            log_level: flwrs_plugin::schema::common::log_level::Enum::Info,
            // The token comes from FLWRS_TOKEN.
            hub_connection: flwrs_plugin::plugin::core::ConnectionConfig::new(
                "127.0.0.1",
                port.parse().unwrap(),
            )
            .with_reconnect(flwrs_plugin::plugin::core::ReconnectPolicy {
                initial_delay: Duration::from_millis(50),
                max_delay: Duration::from_millis(200),
            }),
        },
    )
    .await
    .unwrap();
    let result = runner.run().await;
    match expect.as_str() {
        "ok" => result.unwrap(),
        "rejected" => assert!(matches!(
            result,
            Err(flwrs_plugin::plugin::error::Error::Rejected)
        )),
        other => panic!("unknown expectation {other}"),
    }
}

fn spawn_child(hub: &Hub, token: &str, expect: &str) -> tokio::process::Child {
    tokio::process::Command::new(std::env::current_exe().unwrap())
        .args([CHILD_TEST, "--exact", "--ignored", "--test-threads=1"])
        .env(PORT_ENV, hub.local_addr().port().to_string())
        .env(EXPECT_ENV, expect)
        .env("FLWRS_TOKEN", token)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap()
}

async fn child_succeeded(mut child: tokio::process::Child) {
    let status = tokio::time::timeout(WAIT, child.wait())
        .await
        .expect("plugin process didn't exit")
        .unwrap();
    assert!(status.success(), "plugin process failed: {status}");
}

#[tokio::test]
async fn sdk_plugin_round_trip_reconnect_and_shutdown() {
    let hub = start_hub(config()).await;
    let mut link = hub.hub.expect(expected("upper", PluginType::Transform));
    let child = spawn_child(&hub.hub, TOKEN, "ok");

    let HubEvent::Connected { conn_id: first, .. } = next(&mut link).await else {
        panic!("expected Connected");
    };
    let metadata = EventMetadata {
        event_id: "e1".into(),
        trace_id: "t1".into(),
        hops: 3,
        origin_node_id: "src".into(),
        create_time: None,
    };
    let input = DataEvent {
        instance_id: "upper".into(),
        plugin_version: "1.0.0".into(),
        payload: Some(text_payload("hello")),
        metadata: Some(metadata.clone()),
    };
    link.send(RuntimeMsg::Event(input.clone())).await.unwrap();
    match next(&mut link).await {
        HubEvent::Event { event, .. } => {
            assert_eq!(event.payload, Some(text_payload("HELLO")));
            assert_eq!(event.metadata, Some(metadata), "metadata propagated");
        }
        other => panic!("expected the transformed event, got {other:?}"),
    }

    // The hub drops the connection; the plugin reconnects and re-initializes.
    link.disconnect();
    let HubEvent::Connected {
        conn_id: second, ..
    } = next(&mut link).await
    else {
        panic!("expected a second Connected");
    };
    assert_ne!(first, second);
    link.send(RuntimeMsg::Event(input)).await.unwrap();
    assert!(matches!(next(&mut link).await, HubEvent::Event { .. }));

    link.send(RuntimeMsg::Shutdown).await.unwrap();
    match next(&mut link).await {
        HubEvent::Exit { conn_id, exit } => {
            assert_eq!(conn_id, second);
            assert_eq!(exit.code, exit_code::Enum::Ok as i32);
        }
        other => panic!("expected Exit, got {other:?}"),
    }
    child_succeeded(child).await;
}

#[tokio::test]
async fn sdk_plugin_with_wrong_token_is_rejected_and_stops() {
    let hub = start_hub(config()).await;
    let mut link = hub.hub.expect(expected("upper", PluginType::Transform));
    let child = spawn_child(&hub.hub, "not-the-right-token", "rejected");
    child_succeeded(child).await;
    assert_quiet(&mut link).await;
}

#[tokio::test]
async fn sdk_plugin_with_wrong_schema_is_rejected_and_stops() {
    let hub = start_hub(config()).await;
    let mut node = expected("upper", PluginType::Transform);
    node.out_schema = Some(text_schema("other"));
    let mut link = hub.hub.expect(node);
    let child = spawn_child(&hub.hub, TOKEN, "rejected");
    match next(&mut link).await {
        HubEvent::Rejected { reasons } => {
            assert_eq!(
                reasons,
                [
                    "out.other: missing from plugin",
                    "out.text: not in manifest"
                ]
            )
        }
        other => panic!("expected Rejected, got {other:?}"),
    }
    child_succeeded(child).await;
}
