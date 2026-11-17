//! The MQTT source checked with trellis against an embedded broker (rumqttd):
//! the protocol audit, messages as events over MQTT 5 and 3.1.1, and a message
//! kept by the broker across a crash because it was never confirmed.

use flwrs_trellis::{Audit, Outcome, StimulusCtx, Target, Tester};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

fn target() -> Target {
    Target::from_package(Path::new(env!("CARGO_MANIFEST_DIR")))
        .unwrap()
        .with_executable(env!("CARGO_BIN_EXE_flwrs-mqtt-source"))
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Starts a broker with an MQTT 3.1.1 and an MQTT 5 listener; returns their ports.
fn start_broker() -> (u16, u16) {
    let (v4, v5) = (free_port(), free_port());
    let listener = |name: &str, port: u16| {
        serde_json::json!({ "1": {
            "name": name,
            "listen": format!("127.0.0.1:{port}"),
            "next_connection_delay_ms": 1,
            "connections": {
                "connection_timeout_ms": 60000, "max_payload_size": 1048576,
                "max_inflight_count": 100, "dynamic_filters": true
            }
        }})
    };
    let config: rumqttd::Config = serde_json::from_value(serde_json::json!({
        "id": 0,
        "router": {
            "max_connections": 100, "max_outgoing_packet_count": 200,
            "max_segment_size": 1048576, "max_segment_count": 10
        },
        "v4": listener("v4", v4),
        "v5": listener("v5", v5),
    }))
    .unwrap();
    std::thread::spawn(move || rumqttd::Broker::new(config).start().unwrap());
    for port in [v4, v5] {
        for _ in 0..100 {
            if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    (v4, v5)
}

/// Publishes one QoS 1 message over MQTT 5 (with properties, if asked) and
/// waits for the broker's PUBACK.
async fn publish(
    port: u16,
    topic: String,
    payload: String,
    with_properties: bool,
) -> Result<(), String> {
    use rumqttc::v5::mqttbytes::QoS;
    use rumqttc::v5::mqttbytes::v5::{Packet, PublishProperties};
    use rumqttc::v5::{AsyncClient, Event, MqttOptions};
    let id = format!("publisher-{}", free_port());
    let (client, mut events) = AsyncClient::new(MqttOptions::new(id, "127.0.0.1", port), 8);
    let sent = if with_properties {
        let properties = PublishProperties {
            content_type: Some("text/plain".into()),
            user_properties: vec![("k".into(), "v".into())],
            ..Default::default()
        };
        client
            .publish_with_properties(topic, QoS::AtLeastOnce, false, payload, properties)
            .await
    } else {
        client
            .publish(topic, QoS::AtLeastOnce, false, payload)
            .await
    };
    sent.map_err(|e| e.to_string())?;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match events.poll().await {
                Ok(Event::Incoming(Packet::PubAck(_))) => return Ok(()),
                Ok(_) => {}
                Err(e) => return Err(e.to_string()),
            }
        }
    })
    .await
    .map_err(|_| "no PUBACK".to_string())?
}

/// A stimulus publishing sensors/<n> with payload m<n> (n = 1, 2, …) to `port`.
/// Without properties for an MQTT 3.1.1 subscriber: rumqttd 0.20 panics
/// forwarding MQTT 5 properties to one.
fn publisher(port: u16, with_properties: bool) -> impl flwrs_trellis::stimulus::Stimulus {
    let n = Arc::new(AtomicU32::new(0));
    move |_: StimulusCtx| {
        let n = n.fetch_add(1, Ordering::SeqCst) + 1;
        Box::pin(async move {
            // The audit fires its first stimulus right after the handshake:
            // give the plugin time to subscribe (messages before that aren't delivered).
            if n == 1 {
                tokio::time::sleep(Duration::from_millis(1000)).await;
            }
            publish(
                port,
                format!("sensors/{n}"),
                format!("m{n}"),
                with_properties,
            )
            .await?;
            Ok(None)
        })
    }
}

async fn run(file: &str, publish_to: u16, broker: u16) {
    let mut tester = Tester::new(target())
        .var("broker", broker.to_string())
        .var("client", file.replace('.', "-"))
        .stimulus(publisher(publish_to, publish_to == broker));
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/trellis")
        .join(file);
    let result = tester.run_file(&path).await;
    assert_eq!(
        result.outcome,
        Outcome::Pass,
        "{}",
        flwrs_trellis::testing::report(std::slice::from_ref(&result))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn follows_the_plugin_protocol() {
    let (_, v5) = start_broker();
    let args = serde_json::json!({
        "broker_url": format!("mqtt://127.0.0.1:{v5}"),
        "client_id": "flwrs-test-audit",
        "subscriptions": [{ "filter": "sensors/#" }]
    });
    let target = target().with_launch_args(args).unwrap();
    let results = Audit::new(target).stimulus(publisher(v5, true)).run().await;
    assert!(
        results.iter().all(|r| r.outcome == Outcome::Pass),
        "{}",
        flwrs_trellis::report(&results)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn receives_messages_over_mqtt_5_and_3_1_1() {
    let (v4, v5) = start_broker();
    run("receive.test.toml", v5, v5).await;
    // Published over MQTT 5 (no properties), received over 3.1.1.
    run("receive_v311.test.toml", v5, v4).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unconfirmed_message_survives_a_crash() {
    let (_, v5) = start_broker();
    run("held.test.toml", v5, v5).await;
}
