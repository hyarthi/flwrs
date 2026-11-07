//! Hub tests: a hand-rolled fake plugin for exact protocol cases, and the real
//! Rust SDK (in a child process, like a real plugin) for the end-to-end ones.

use super::HubConfig;
use super::service::{ExpectedNode, Hub, HubError, HubEvent, NodeAuth, NodeLink, Routed};
use super::wire::DataEvent;
use crate::modules::plugin::manifest::PluginType;
use flwrs_plugin::plugin::core::InboundEvent;
use flwrs_plugin::plugin::frame::{
    Frame, PROTOCOL_VERSION, WireMessage, encode_frame, header, read_frame, write_frame,
    write_message,
};
use flwrs_plugin::schema::payload::{
    Field, FieldDefinition, FieldValue, PluginPayload, SchemaDefinition, field_type,
    field_value::Value,
};
use flwrs_plugin::schema::protocol::{
    Ack, Credit, Event, EventMetadata, Exit, FrameHeader, Initialize, InitializeResponse,
    LogEvent, Nack, Ping, Pong, Shutdown, capability::Enum as Capability, exit_code,
    msg_code::Enum as MsgCode, plugin_type::Enum as PbPluginType,
};
use prost::Message;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio_util::sync::CancellationToken;

pub(super) const TOKEN: &str = "a-sufficiently-long-token";
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

pub(super) fn expected(node_id: &str, plugin_type: PluginType) -> ExpectedNode {
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
        auth: NodeAuth::local(TOKEN.into()),
        max_event_bytes: None,
    }
}

pub(super) struct TestHub {
    pub hub: Hub,
    pub stop: CancellationToken,
}

impl Drop for TestHub {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

pub(super) async fn start_hub(config: HubConfig) -> TestHub {
    let hub = Hub::bind(config).await.unwrap();
    let stop = CancellationToken::new();
    let running = hub.clone();
    let token = stop.clone();
    tokio::spawn(async move { running.run(token).await });
    TestHub { hub, stop }
}

pub(super) fn config() -> HubConfig {
    HubConfig {
        handshake_timeout_ms: 300,
        // No pings unless a test wants them (they'd interleave with frames).
        heartbeat_interval_ms: 0,
        credit_window: 4,
        ..HubConfig::default()
    }
}

pub(super) async fn next(link: &mut NodeLink) -> HubEvent {
    tokio::time::timeout(WAIT, link.recv())
        .await
        .expect("timed out waiting for a hub event")
        .expect("link closed")
}

pub(super) async fn assert_quiet(link: &mut NodeLink) {
    let quiet = tokio::time::timeout(Duration::from_millis(200), link.recv()).await;
    assert!(quiet.is_err(), "unexpected event: {quiet:?}");
}

pub(super) fn init(plugin_type: PbPluginType, id: &str, token: &str) -> Initialize {
    let schema = Some(text_schema("text"));
    let (in_schema, out_schema) = match plugin_type {
        PbPluginType::Source => (None, schema),
        PbPluginType::Sink => (schema, None),
        _ => (schema.clone(), schema),
    };
    Initialize {
        instance_id: id.into(),
        plugin_version: "1.0.0".into(),
        plugin_type: plugin_type as i32,
        token: token.into(),
        in_schema,
        out_schema,
        min_protocol_version: PROTOCOL_VERSION,
        max_protocol_version: PROTOCOL_VERSION,
        capabilities: vec![],
        max_unacked: 0,
    }
}

fn sink_init(id: &str, token: &str) -> Initialize {
    init(PbPluginType::Sink, id, token)
}

fn with_caps(mut init: Initialize, caps: &[Capability]) -> Initialize {
    init.capabilities = caps.iter().map(|c| *c as i32).collect();
    init
}

/// A byte stream to the hub: plain TCP, or TLS.
pub(super) trait Io: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send> Io for T {}

/// A plugin speaking raw frames.
pub(super) struct FakePlugin {
    pub stream: Box<dyn Io>,
    next_seq: u64,
}

impl FakePlugin {
    pub(super) async fn connect(hub: &Hub) -> Self {
        Self::over(Box::new(TcpStream::connect(hub.local_addr()).await.unwrap()))
    }

    pub(super) fn over(stream: Box<dyn Io>) -> Self {
        Self {
            stream,
            next_seq: 1,
        }
    }

    pub(super) async fn send<M: WireMessage>(&mut self, msg: &M) {
        write_message(&mut self.stream, msg).await.unwrap();
    }

    pub(super) async fn send_raw(&mut self, header: &FrameHeader, body: &[u8]) {
        write_frame(&mut self.stream, header, body).await.unwrap();
    }

    /// An EVENT with the next seq.
    pub(super) async fn send_event(&mut self, payload: PluginPayload, input_seq: u64) -> u64 {
        let seq = self.next_seq;
        self.next_seq += 1;
        let header = FrameHeader {
            code: MsgCode::Event as i32,
            seq,
            input_seq,
            ..Default::default()
        };
        let body = Event {
            payload: Some(payload),
        }
        .encode_to_vec();
        self.send_raw(&header, &body).await;
        seq
    }

    pub(super) async fn recv(&mut self) -> Option<Frame> {
        tokio::time::timeout(WAIT, read_frame(&mut self.stream))
            .await
            .expect("timed out waiting for the hub")
            .ok()
            .flatten()
    }

    pub(super) async fn recv_msg<M: WireMessage>(&mut self) -> M {
        let frame = self.recv().await.expect("hub closed the connection");
        frame
            .decode()
            .unwrap_or_else(|e| panic!("expected {:?}: {e} ({frame:?})", M::CODE))
    }

    pub(super) async fn expect_closed(&mut self) {
        while self.recv().await.is_some() {}
    }

    pub(super) async fn expect_shutdown(&mut self) -> String {
        let shutdown: Shutdown = self.recv_msg().await;
        self.expect_closed().await;
        shutdown.reason
    }
}

/// Connects a plugin with valid credentials and checks it's accepted.
async fn connected(
    hub: &Hub,
    link: &mut NodeLink,
    init: Initialize,
) -> (FakePlugin, u64, InitializeResponse) {
    let mut plugin = FakePlugin::connect(hub).await;
    plugin.send(&init).await;
    let response: InitializeResponse = plugin.recv_msg().await;
    assert_eq!(response.protocol_version, PROTOCOL_VERSION);
    match next(link).await {
        HubEvent::Connected { conn_id, .. } => (plugin, conn_id, response),
        other => panic!("expected Connected, got {other:?}"),
    }
}

async fn connected_sink(hub: &Hub, link: &mut NodeLink) -> (FakePlugin, u64) {
    let (plugin, conn_id, _) = connected(hub, link, sink_init("sink", TOKEN)).await;
    (plugin, conn_id)
}

fn data(text: &str, metadata: Option<EventMetadata>) -> DataEvent {
    DataEvent {
        metadata,
        attempt: 1,
        ..DataEvent::from_payload(text_payload(text))
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

    let metadata = EventMetadata {
        trace_id: "t".into(),
        hops: 1,
        ..Default::default()
    };
    let sender = link.sender();
    let (id, seq) = sender
        .send_event(&data("hi", Some(metadata.clone())))
        .await
        .unwrap();
    assert_eq!((id, seq), (conn_id, 1));
    let frame = plugin.recv().await.unwrap();
    assert_eq!(frame.code(), MsgCode::Event);
    assert_eq!(
        (frame.header.seq, frame.header.attempt, frame.header.metadata.clone()),
        (1, 1, Some(metadata))
    );
    let event: Event = frame.decode().unwrap();
    assert_eq!(event.payload, Some(text_payload("hi")));
    let (_, seq) = sender.send_event(&data("again", None)).await.unwrap();
    assert_eq!(seq, 2, "seqs count up per connection");
    plugin.recv().await.unwrap();

    // Logs and errors are only written to the server log.
    plugin
        .send(&LogEvent {
            message: "hello from the plugin".into(),
            ..Default::default()
        })
        .await;

    link.shutdown("done").await.unwrap();
    let shutdown: Shutdown = plugin.recv_msg().await;
    assert_eq!(shutdown.reason, "done");
    let exit = Exit {
        code: exit_code::Enum::Ok as i32,
        message: String::new(),
    };
    plugin.send(&exit).await;
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
    assert_eq!(link.shutdown("").await, Err(HubError::NotConnected));
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
        plugin.send(&sink_init(id, token)).await;
        let reason = plugin.expect_shutdown().await;
        // Strangers learn nothing about which part was wrong.
        assert_eq!(reason, "unknown instance id or wrong credentials", "{id}/{token}");
    }
    assert_quiet(&mut link).await;
}

#[tokio::test]
async fn mismatched_declarations_are_rejected_with_reasons() {
    let hub = start_hub(config()).await;
    let mut link = hub.hub.expect(expected("sink", PluginType::Sink));

    let mut wrong_version = sink_init("sink", TOKEN);
    wrong_version.plugin_version = "2.0.0".into();
    let mut wrong_schema = sink_init("sink", TOKEN);
    wrong_schema.in_schema = Some(text_schema("body"));
    let mut malformed = sink_init("sink", TOKEN);
    malformed.in_schema = Some(SchemaDefinition {
        fields: vec![FieldDefinition {
            key: "text".into(),
            ..Default::default()
        }],
    });
    let mut old_protocol = sink_init("sink", TOKEN);
    old_protocol.min_protocol_version = 1;
    old_protocol.max_protocol_version = 1;
    let mut no_type = sink_init("sink", TOKEN);
    no_type.plugin_type = 0;
    let cases = [
        (wrong_version, "version: 1.0.0 in the registry"),
        (wrong_schema, "in.body: not in manifest"),
        (malformed, "in: "),
        (old_protocol, "no common protocol version: the hub speaks 2..2, the plugin 1..1"),
        (no_type, "type: UNDEFINED"),
        (
            init(PbPluginType::Source, "sink", TOKEN),
            "type: SINK in the registry, SOURCE from the plugin",
        ),
    ];
    for (init, expected_reason) in cases {
        let mut plugin = FakePlugin::connect(&hub.hub).await;
        plugin.send(&init).await;
        let reason = plugin.expect_shutdown().await;
        assert!(reason.contains(expected_reason), "{expected_reason:?} not in {reason:?}");
        match next(&mut link).await {
            HubEvent::Rejected { reasons } => assert!(
                reasons.iter().any(|r| r.contains(expected_reason)),
                "{expected_reason:?} not in {reasons:?}"
            ),
            other => panic!("expected Rejected, got {other:?}"),
        }
    }
    assert!(!link.is_connected());
}

#[tokio::test]
async fn negotiates_capabilities_by_plugin_type() {
    let hub = start_hub(HubConfig {
        heartbeat_interval_ms: 1_000,
        ..config()
    })
    .await;
    let both = [Capability::Acks, Capability::Credits];

    let mut link = hub.hub.expect(expected("sink", PluginType::Sink));
    let mut asked = with_caps(sink_init("sink", TOKEN), &both);
    asked.max_unacked = 5;
    let (_sink, _, response) = connected(&hub.hub, &mut link, asked).await;
    // A sink produces nothing, so no credits; its window as asked.
    assert_eq!(response.capabilities, [Capability::Acks as i32]);
    assert_eq!((response.max_unacked, response.heartbeat_interval_ms), (5, 1_000));

    let mut link = hub.hub.expect(expected("src", PluginType::Source));
    let (mut source, _, response) = connected(
        &hub.hub,
        &mut link,
        with_caps(init(PbPluginType::Source, "src", TOKEN), &both),
    )
    .await;
    assert_eq!(
        response.capabilities,
        [Capability::Acks as i32, Capability::Credits as i32]
    );
    assert_eq!(response.max_unacked, 0, "a source doesn't consume");
    // The initial credit follows InitializeResponse.
    let credit: Credit = source.recv_msg().await;
    assert_eq!(credit.events, 4);

    let mut link = hub.hub.expect(expected("plain", PluginType::Transform));
    let (_plain, _, response) =
        connected(&hub.hub, &mut link, init(PbPluginType::Transform, "plain", TOKEN)).await;
    assert!(response.capabilities.is_empty());
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
    plugin.send(&LogEvent::default()).await;
    assert!(plugin.recv().await.is_none());

    // A hub -> plugin code.
    let mut plugin = FakePlugin::connect(&hub.hub).await;
    plugin.send_raw(&header(MsgCode::InitializeResponse), &[]).await;
    assert!(plugin.recv().await.is_none());

    // Garbage instead of a frame (and the old text-header framing).
    for garbage in [
        &[10u8, 0, 0, 0, 2, 0, 0, 0, 1, 2, 3, 4, 5, 6][..],
        &b"\x21\0\0\0\x1d\0\0\0Version: 1.0.0\r\nMsg-Code: 3\r\n"[..],
    ] {
        let mut plugin = FakePlugin::connect(&hub.hub).await;
        plugin.stream.write_all(garbage).await.unwrap();
        assert!(plugin.recv().await.is_none());
    }

    // Too large for a handshake.
    let mut plugin = FakePlugin::connect(&hub.hub).await;
    let mut body = sink_init("sink", TOKEN).encode_to_vec();
    body.resize(4096, 0);
    plugin.send_raw(&header(MsgCode::Initialize), &body).await;
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
async fn source_events_reach_the_link_and_oversized_ones_are_nacked() {
    let hub = start_hub(config()).await;
    let mut node = expected("src", PluginType::Source);
    node.max_event_bytes = Some(64);
    let mut link = hub.hub.expect(node);
    let (mut plugin, conn_id, _) = connected(
        &hub.hub,
        &mut link,
        with_caps(init(PbPluginType::Source, "src", TOKEN), &[Capability::Acks]),
    )
    .await;

    plugin.send_event(text_payload(&"x".repeat(200)), 0).await; // over the node's limit
    let nack: Nack = plugin.recv_msg().await;
    assert_eq!((nack.seq, nack.retry), (1, false));
    assert!(nack.reason.contains("too large"), "{}", nack.reason);

    plugin.send_event(text_payload("small"), 0).await;
    let body_len = Event {
        payload: Some(text_payload("small")),
    }
    .encoded_len();
    match next(&mut link).await {
        HubEvent::Event {
            conn_id: id,
            seq,
            event,
            size,
        } => {
            assert_eq!((id, seq, size), (conn_id, 2, body_len));
            assert_eq!(event.payload().unwrap(), text_payload("small"));
            assert_eq!(event.metadata, None, "a source's event starts a trace");
        }
        other => panic!("expected the small event, got {other:?}"),
    }
    // The director's verdict goes back as ACK / NACK.
    let sender = link.sender();
    sender.routed(conn_id, 2, Routed::Accepted).await;
    let ack: Ack = plugin.recv_msg().await;
    assert_eq!((ack.up_to, ack.seqs), (0, vec![2]));

    // Sources don't receive events.
    assert!(matches!(
        sender.send_event(&data("x", None)).await,
        Err(HubError::Invalid(_))
    ));
}

#[tokio::test]
async fn credits_are_enforced_and_returned() {
    let hub = start_hub(config()).await;
    let mut link = hub.hub.expect(expected("src", PluginType::Source));
    let (mut plugin, conn_id, _) = connected(
        &hub.hub,
        &mut link,
        with_caps(init(PbPluginType::Source, "src", TOKEN), &[Capability::Credits]),
    )
    .await;
    let credit: Credit = plugin.recv_msg().await;
    assert_eq!(credit.events, 4);
    for _ in 0..4 {
        plugin.send_event(text_payload("e"), 0).await;
    }
    let sender = link.sender();
    for seq in 1..=4 {
        assert!(matches!(next(&mut link).await, HubEvent::Event { .. }));
        // Routing done: the credit comes back (in batches of a quarter).
        sender.routed(conn_id, seq, Routed::Accepted).await;
        let credit: Credit = plugin.recv_msg().await;
        assert_eq!(credit.events, 1, "the window is 4, so one at a time");
    }
    // 4 granted on top of the first 4: 8 events in total are fine, a 9th isn't.
    for _ in 0..4 {
        plugin.send_event(text_payload("e"), 0).await;
    }
    plugin.send_event(text_payload("one too many"), 0).await;
    plugin.expect_closed().await;
    for _ in 0..4 {
        assert!(matches!(next(&mut link).await, HubEvent::Event { .. }));
    }
    match next(&mut link).await {
        HubEvent::Disconnected { reason, .. } => {
            assert!(reason.contains("without credit"), "{reason}")
        }
        other => panic!("expected Disconnected, got {other:?}"),
    }
}

#[tokio::test]
async fn consumer_acks_reach_the_link() {
    let hub = start_hub(config()).await;
    let mut link = hub.hub.expect(expected("sink", PluginType::Sink));
    let (mut plugin, conn_id, _) = connected(
        &hub.hub,
        &mut link,
        with_caps(sink_init("sink", TOKEN), &[Capability::Acks]),
    )
    .await;
    plugin.send(&Ack { up_to: 3, seqs: vec![5] }).await;
    plugin
        .send(&Nack {
            seq: 4,
            reason: "busy".into(),
            retry: true,
        })
        .await;
    assert_eq!(
        next(&mut link).await,
        HubEvent::Acked {
            conn_id,
            up_to: 3,
            seqs: vec![5]
        }
    );
    assert_eq!(
        next(&mut link).await,
        HubEvent::Nacked {
            conn_id,
            seq: 4,
            reason: "busy".into(),
            retry: true
        }
    );
}

#[tokio::test]
async fn protocol_violations_close_the_connection() {
    let hub = start_hub(config()).await;
    let mut link = hub.hub.expect(expected("sink", PluginType::Sink));

    let mut seq_gap = FrameHeader {
        code: MsgCode::Event as i32,
        seq: 3,
        ..Default::default()
    };
    let cases: Vec<(&str, FrameHeader, Vec<u8>)> = vec![
        // A sink never sends events.
        ("Event from a SINK", header(MsgCode::Event), vec![]),
        ("Credit from a SINK", header(MsgCode::Credit), vec![]),
        ("Initialize from a SINK", header(MsgCode::Initialize), vec![]),
        // Acks need the capability.
        ("Ack without the ACKS capability", header(MsgCode::Ack), vec![]),
        // A body that doesn't decode.
        ("undecodable Exit", header(MsgCode::Exit), vec![0xff, 0xff]),
    ];
    for (expected_reason, frame_header, body) in cases {
        let (mut plugin, conn_id) = connected_sink(&hub.hub, &mut link).await;
        plugin.send_raw(&frame_header, &body).await;
        plugin.expect_closed().await;
        match next(&mut link).await {
            HubEvent::Disconnected {
                conn_id: id,
                reason,
            } => {
                assert_eq!(id, conn_id);
                assert!(reason.contains(expected_reason), "{expected_reason:?}: {reason}");
            }
            other => panic!("expected Disconnected, got {other:?}"),
        }
    }

    // A producer's seqs must count up from 1.
    let mut link = hub.hub.expect(expected("src", PluginType::Source));
    let (mut plugin, _, _) =
        connected(&hub.hub, &mut link, init(PbPluginType::Source, "src", TOKEN)).await;
    seq_gap.seq = 2;
    plugin.send_raw(&seq_gap, &[]).await;
    plugin.expect_closed().await;
    match next(&mut link).await {
        HubEvent::Disconnected { reason, .. } => {
            assert!(reason.contains("event seq 2, expected 1"), "{reason}")
        }
        other => panic!("expected Disconnected, got {other:?}"),
    }
}

#[tokio::test]
async fn heartbeats_ping_answer_and_time_out() {
    let hub = start_hub(HubConfig {
        heartbeat_interval_ms: 100,
        ..config()
    })
    .await;
    let mut link = hub.hub.expect(expected("sink", PluginType::Sink));
    let (mut plugin, _) = connected_sink(&hub.hub, &mut link).await;

    // The plugin's ping is answered.
    plugin.send(&Ping { nonce: 7 }).await;
    loop {
        let frame = plugin.recv().await.unwrap();
        match frame.code() {
            MsgCode::Pong => {
                assert_eq!(frame.decode::<Pong>().unwrap().nonce, 7);
                break;
            }
            MsgCode::Ping => {} // the hub's own
            other => panic!("unexpected {other:?}"),
        }
    }
    // The hub pings; answering keeps the connection alive.
    let started = std::time::Instant::now();
    while started.elapsed() < Duration::from_millis(500) {
        let ping: Ping = plugin.recv_msg().await;
        plugin.send(&Pong { nonce: ping.nonce }).await;
    }
    assert!(link.is_connected());
    // Silence for three intervals: the hub gives up.
    let silent = std::time::Instant::now();
    match next(&mut link).await {
        HubEvent::Disconnected { reason, .. } => {
            assert!(reason.contains("nothing from the plugin for 300 ms"), "{reason}");
            assert!(silent.elapsed() >= Duration::from_millis(250));
        }
        other => panic!("expected Disconnected, got {other:?}"),
    }
}

#[tokio::test]
async fn transform_outputs_carry_their_inputs_metadata() {
    let hub = start_hub(config()).await;
    let mut link = hub.hub.expect(expected("t", PluginType::Transform));
    let (mut plugin, _, _) = connected(
        &hub.hub,
        &mut link,
        with_caps(init(PbPluginType::Transform, "t", TOKEN), &[Capability::Acks]),
    )
    .await;
    let meta = |id: &str| EventMetadata {
        event_id: id.into(),
        trace_id: format!("trace-{id}"),
        hops: 2,
        ..Default::default()
    };
    let sender = link.sender();
    for id in ["a", "b"] {
        sender.send_event(&data(id, Some(meta(id)))).await.unwrap();
        plugin.recv().await.unwrap();
    }
    // Output for input 2, then 1 (after acking it: an accumulator), then for
    // none, then for an input that was never sent.
    plugin.send_event(text_payload("B"), 2).await;
    plugin.send(&Ack { up_to: 1, seqs: vec![] }).await;
    plugin.send_event(text_payload("A"), 1).await;
    plugin.send_event(text_payload("new"), 0).await;
    plugin.send_event(text_payload("unknown"), 99).await;
    let mut seen = vec![];
    while seen.len() < 4 {
        match next(&mut link).await {
            HubEvent::Event { event, .. } => seen.push(event.metadata.map(|m| m.event_id)),
            HubEvent::Acked { .. } => {}
            other => panic!("unexpected {other:?}"),
        }
    }
    assert_eq!(seen, [Some("b".into()), Some("a".into()), None, None]);
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
    assert_eq!(link.sender().connection().map(|c| c.0), Some(second_id));
    link.shutdown("").await.unwrap();
    let _: Shutdown = second.recv_msg().await;
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
    plugin.send(&sink_init("sink", TOKEN)).await;
    plugin.expect_shutdown().await;

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
    stale.send(&sink_init("sink", TOKEN)).await;
    stale.expect_shutdown().await;

    let (mut fresh, _, _) = connected(
        &hub.hub,
        &mut link,
        sink_init("sink", "the-next-launch-token"),
    )
    .await;
    // The sender cloned before the renewal still reaches the plugin.
    sender.shutdown("").await.unwrap();
    let _: Shutdown = fresh.recv_msg().await;
}

#[tokio::test]
async fn nodes_sharing_a_remote_id_are_told_apart_by_token() {
    let hub = start_hub(config()).await;
    let shared = |node_id: &str, token: &str| ExpectedNode {
        instance_id: "gateway-1".into(),
        auth: NodeAuth::local(token.into()),
        ..expected(node_id, PluginType::Sink)
    };
    let mut first = hub.hub.expect(shared("node-a", "token-for-node-a-0123"));
    let mut second = hub.hub.expect(shared("node-b", "token-for-node-b-0123"));

    let mut open = vec![]; // keeps the connections up for the test
    for (token, link) in [
        ("token-for-node-b-0123", &mut second),
        ("token-for-node-a-0123", &mut first),
    ] {
        let (plugin, _, _) = connected(&hub.hub, link, sink_init("gateway-1", token)).await;
        open.push(plugin);
    }
    assert!(first.is_connected() && second.is_connected());

    // The node id itself isn't what a remote plugin sends.
    let mut by_node_id = FakePlugin::connect(&hub.hub).await;
    by_node_id
        .send(&sink_init("node-a", "token-for-node-a-0123"))
        .await;
    by_node_id.expect_shutdown().await;
    let mut wrong_token = FakePlugin::connect(&hub.hub).await;
    wrong_token
        .send(&sink_init("gateway-1", "token-for-nobody-0123"))
        .await;
    wrong_token.expect_shutdown().await;
}

#[tokio::test]
async fn stopping_the_hub_closes_connections() {
    let hub = start_hub(config()).await;
    let mut link = hub.hub.expect(expected("sink", PluginType::Sink));
    let (mut plugin, _) = connected_sink(&hub.hub, &mut link).await;
    hub.stop.cancel();
    plugin.expect_closed().await;
}

#[test]
fn spec_example_frame_is_what_the_hub_writes() {
    // docs/protocol.md, section 4: a Ping with nonce 1.
    let frame = encode_frame(&header(MsgCode::Ping), &Ping { nonce: 1 }.encode_to_vec());
    assert_eq!(frame, [9, 0, 0, 0, 2, 2, 0, 0, 0, 0x08, 0x0b, 0x08, 0x01]);
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
        _: flwrs_plugin::schema::protocol::log_level::Enum,
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
        mut event: InboundEvent,
    ) -> Result<Vec<PluginPayload>, flwrs_plugin::plugin::error::TransformError> {
        for field in &mut event.payload.fields {
            if let Some(FieldValue {
                value: Some(Value::String(s)),
            }) = &mut field.value
            {
                *s = s.to_uppercase();
            }
        }
        Ok(vec![event.payload])
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
        TransformRunnerConfig::new(
            "upper".into(),
            flwrs_plugin::schema::protocol::log_level::Enum::Info,
            // The token comes from FLWRS_TOKEN.
            flwrs_plugin::plugin::core::ConnectionConfig::new("127.0.0.1", port.parse().unwrap())
                .with_reconnect(flwrs_plugin::plugin::core::ReconnectPolicy {
                    initial_delay: Duration::from_millis(50),
                    max_delay: Duration::from_millis(200),
                }),
        ),
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

pub(super) fn spawn_child(hub: &Hub, token: &str, expect: &str) -> tokio::process::Child {
    spawn_child_with(hub.local_addr().port(), token, expect, &[])
}

/// The SDK child, connecting to `port`, with extra environment variables.
pub(super) fn spawn_child_with(
    port: u16,
    token: &str,
    expect: &str,
    env: &[(&str, String)],
) -> tokio::process::Child {
    tokio::process::Command::new(std::env::current_exe().unwrap())
        .args([CHILD_TEST, "--exact", "--ignored", "--test-threads=1"])
        .envs(env.iter().map(|(k, v)| (*k, v.as_str())))
        .env(PORT_ENV, port.to_string())
        .env(EXPECT_ENV, expect)
        .env("FLWRS_TOKEN", token)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap()
}

pub(super) async fn child_succeeded(mut child: tokio::process::Child) {
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

    let HubEvent::Connected {
        conn_id: first,
        negotiated,
        ..
    } = next(&mut link).await
    else {
        panic!("expected Connected");
    };
    assert!(negotiated.acks && negotiated.credits);
    let metadata = EventMetadata {
        event_id: "e1".into(),
        trace_id: "t1".into(),
        hops: 3,
        origin_node_id: "src".into(),
        create_time: None,
    };
    let sender = link.sender();
    sender
        .send_event(&data("hello", Some(metadata.clone())))
        .await
        .unwrap();
    // The output (with the input's metadata, from the hub's memory), then
    // the input's ack.
    match next(&mut link).await {
        HubEvent::Event {
            conn_id,
            seq,
            event,
            ..
        } => {
            assert_eq!(event.payload().unwrap(), text_payload("HELLO"));
            assert_eq!(event.metadata, Some(metadata.clone()), "the input's metadata");
            sender.routed(conn_id, seq, Routed::Accepted).await;
        }
        other => panic!("expected the transformed event, got {other:?}"),
    }
    assert_eq!(
        next(&mut link).await,
        HubEvent::Acked {
            conn_id: first,
            up_to: 0,
            seqs: vec![1]
        }
    );

    // The hub drops the connection; the plugin reconnects and re-initializes.
    link.disconnect();
    let HubEvent::Connected {
        conn_id: second, ..
    } = next(&mut link).await
    else {
        panic!("expected a second Connected");
    };
    assert_ne!(first, second);
    sender.send_event(&data("again", Some(metadata))).await.unwrap();
    assert!(matches!(next(&mut link).await, HubEvent::Event { .. }));
    assert!(matches!(next(&mut link).await, HubEvent::Acked { .. }));

    link.shutdown("").await.unwrap();
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
