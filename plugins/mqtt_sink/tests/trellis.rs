//! The MQTT sink checked with trellis against an embedded broker (rumqttd):
//! the protocol audit, publishing over MQTT 5 and 3.1.1 (checked by a
//! subscriber), and NACKs while the broker is down.

use flwrs_trellis::{Audit, Outcome, Target, Tester};
use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

fn target() -> Target {
    Target::from_package(Path::new(env!("CARGO_MANIFEST_DIR")))
        .unwrap()
        .with_executable(env!("CARGO_BIN_EXE_flwrs-mqtt-sink"))
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
    start_broker_on(v4, v5);
    (v4, v5)
}

fn start_broker_on(v4: u16, v5: u16) {
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
}

/// A received message: payload, QoS, and the MQTT 5 user properties and content type.
#[derive(Debug, Clone, PartialEq)]
struct Received {
    payload: String,
    qos: u8,
    content_type: Option<String>,
    user_properties: Vec<(String, String)>,
}

/// Subscribes to `flwrs/#` over MQTT 5; returns what arrived, by topic.
async fn subscribe(port: u16) -> std::sync::Arc<std::sync::Mutex<HashMap<String, Received>>> {
    use rumqttc::v5::mqttbytes::QoS;
    use rumqttc::v5::mqttbytes::v5::Packet;
    use rumqttc::v5::{AsyncClient, Event, MqttOptions};
    let received = std::sync::Arc::new(std::sync::Mutex::new(HashMap::new()));
    let (client, mut events) =
        AsyncClient::new(MqttOptions::new("subscriber", "127.0.0.1", port), 16);
    client.subscribe("flwrs/#", QoS::AtLeastOnce).await.unwrap();
    let (subscribed, ready) = tokio::sync::oneshot::channel();
    let mut subscribed = Some(subscribed);
    let into = received.clone();
    tokio::spawn(async move {
        while let Ok(event) = events.poll().await {
            match event {
                Event::Incoming(Packet::SubAck(_)) => {
                    if let Some(s) = subscribed.take() {
                        let _ = s.send(());
                    }
                }
                Event::Incoming(Packet::Publish(p)) => {
                    let props = p.properties.unwrap_or_default();
                    into.lock().unwrap().insert(
                        String::from_utf8_lossy(&p.topic).into_owned(),
                        Received {
                            payload: String::from_utf8_lossy(&p.payload).into_owned(),
                            qos: p.qos as u8,
                            content_type: props.content_type,
                            user_properties: props.user_properties,
                        },
                    );
                }
                _ => {}
            }
        }
    });
    tokio::time::timeout(Duration::from_secs(5), ready)
        .await
        .unwrap()
        .unwrap();
    received
}

async fn run(file: &str, vars: &[(&str, String)]) {
    let mut tester = Tester::new(target());
    for (name, value) in vars {
        tester = tester.var(*name, value.clone());
    }
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

async fn wait_for(
    received: &std::sync::Arc<std::sync::Mutex<HashMap<String, Received>>>,
    n: usize,
) -> HashMap<String, Received> {
    for _ in 0..100 {
        if received.lock().unwrap().len() >= n {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    received.lock().unwrap().clone()
}

#[tokio::test]
async fn follows_the_plugin_protocol() {
    // No broker needed: the audit checks the protocol (events are NACKed, retryably).
    let text = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/trellis/audit.json"),
    )
    .unwrap();
    let target = target()
        .with_launch_args(serde_json::from_str(&text).unwrap())
        .unwrap();
    let results = Audit::new(target).run().await;
    assert!(
        results.iter().all(|r| r.outcome == Outcome::Pass),
        "{}",
        flwrs_trellis::report(&results)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn publishes_over_mqtt_5_and_3_1_1() {
    let (v4, v5) = start_broker();
    let received = subscribe(v5).await;
    run("publish.test.toml", &[("broker", v5.to_string())]).await;
    let got = wait_for(&received, 4).await;
    assert_eq!(got["flwrs/out"].payload, "default topic");
    assert_eq!(got["flwrs/dev/9"].payload, "exactly once");
    assert_eq!(got["flwrs/out"].qos, 1);
    let named = &got["flwrs/dev/7"];
    assert_eq!(named.payload, "named topic");
    assert_eq!(named.content_type.as_deref(), Some("text/plain"));
    assert_eq!(named.user_properties, [("k".to_string(), "v".to_string())]);
    assert_eq!(got["flwrs/dev/8"].payload, "at most once");
    assert!(!got.contains_key("elsewhere"));

    received.lock().unwrap().clear();
    run("publish_v311.test.toml", &[("broker", v4.to_string())]).await;
    let got = wait_for(&received, 4).await;
    assert_eq!(got["flwrs/dev/7"].payload, "named topic");
    // MQTT 5 properties don't exist in 3.1.1: they're left out.
    assert_eq!(got["flwrs/dev/7"].content_type, None);
    assert_eq!(got.len(), 4);
}

#[tokio::test]
async fn nacks_retryably_while_the_broker_is_down() {
    run("down.test.toml", &[("down", free_port().to_string())]).await;
}

#[tokio::test]
async fn recovers_when_the_broker_comes_up() {
    let (v4, v5) = (free_port(), free_port());
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(2));
        start_broker_on(v4, v5);
    });
    run("recover.test.toml", &[("broker", v5.to_string())]).await;
}
