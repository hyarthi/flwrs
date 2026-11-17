# MQTT sink (`flwrs-mqtt-sink`)

Publishes each event to an MQTT broker, over MQTT 5 (default) or 3.1.1. An event is acknowledged to the hub only once the broker has the message, so with a durable queue in front of the node nothing is lost while the broker is away.

## Settings

| Key | Default | Meaning |
|---|---|---|
| `broker_url` | (required) | `mqtt://host[:1883]` or `mqtts://host[:8883]` (TLS). |
| `protocol_version` | `"5"` | `"5"` or `"3.1.1"`. Over 3.1.1, the MQTT 5 fields of an event are left out (a WARN once). |
| `default_topic` | | Where events go that don't name a topic. |
| `allowed_topics` | | Topic filters (`+`, `#`) that events may name their own `topic` within. **Without them, an event that names a topic is rejected**, so a mapping mistake can't publish anywhere. |
| `default_qos` | `1` | For events that don't set `qos`. |
| `username`, `password` | | Broker credentials. The password is a secret (delivered in `MQTT_PASSWORD`, never on the command line). |
| `tls_ca`, `tls_cert`, `tls_key` | | Files uploaded to the node (PEM): the CA of the broker's certificate (default: the bundled Mozilla roots, not the OS store), and a client certificate and its key for mTLS. The key is a secret. Uploads travel with the scene in a `.far` export. |
| `insecure_skip_verify` | `false` | Development only: any broker certificate is accepted (a warning at start). |
| `client_id` | `flwrs-<node id>` | Must be unique per broker. |
| `keep_session` | `false` | Keep the broker session across connections (clean start off). `session_expiry_s`: MQTT 5 only. |
| `keepalive_s` | `30` | Seconds between pings. |
| `max_inflight` | `16` | QoS 1 / 2 messages awaiting the broker at once; also how many events the plugin works on at once. `1` keeps the event order strictly. |
| `max_packet_bytes` | 1 MiB | A larger message is rejected (not retried). |
| `last_will` | | `{"topic", "payload", "qos", "retain"}`: published by the broker if the connection drops without a clean disconnect. |

At least one of `default_topic` and `allowed_topics` is required.

## Events

The input schema (`schema/in.json`): `topic`, `payload` (bytes, or a string as UTF-8), `qos` (0–2), `retain`, and the MQTT 5 properties `content_type`, `response_topic`, `correlation_data`, `user_properties` (a map of string arrays: a name may repeat) and `message_expiry_s`. Every field is optional.

## Delivery

- **QoS 1 / 2:** the event is ACKed after the broker's PUBACK / PUBCOMP. An MQTT 5 refusal is a NACK: not retried for `NotAuthorized`, `TopicNameInvalid` and `PayloadFormatInvalid` (they need a config change), retried otherwise.
- **QoS 0:** ACKed once the message is written to the connection; it can still be lost on the way.
- **Disconnected:** an event waits up to a second for the connection (so a start or a brief reconnect costs no retry), then it's NACKed, retryably; the hub redelivers them after its retry delay. The plugin keeps nothing in memory while offline: the node's queue (durable if configured) is the buffer. Messages in flight when the connection drops are NACKed too, so the broker may get one twice (at least once).
- **Rejected, not retried:** a topic outside `allowed_topics` (or none at all), wildcards in a topic, a wrong field type, `qos` above 2, a message over `max_packet_bytes`.
- **Order:** messages go out in the order the plugin receives the events, but with `max_inflight` above 1 a retried event comes later than the ones after it. Use `max_inflight: 1` for strict order.

The plugin connects when it starts, not with the first event, so a wrong URL or credentials show in the log at once. It reconnects with a backoff (1 s, doubling to 30 s, with jitter) and logs a WARN when a connection drops, then at most once a minute while the broker stays unreachable. On shutdown it disconnects cleanly, so the broker drops the Last Will.

## Footprint

Measured with `cargo test --release -p flwrs-mqtt-sink --test bench -- --ignored --nocapture` (`benches/`, `tests/bench.rs`: trellis's bench against an embedded rumqttd broker (no subscribers) in the test process; CPU and memory are the plugin's alone) on an Apple M2 (macOS), median of three 5-second runs. "Max load" is a closed loop with 64 events in flight. At a steady rate, latency counts from when the harness woke for each send (its own timer lateness, ~0.23 ms, is reported apart since trellis 0.0.5).

| Scenario | Events/s | p50 / p99 latency | CPU | RSS |
|---|---|---|---|---|
| QoS 1 (`max_inflight` 64), max load | ~46 500 | 1.4 / 2.2 ms | ~97 % | ~9 MiB |
| QoS 0, max load | ~48 200 | 1.4 / 1.9 ms | ~85 % | ~9 MiB |
| QoS 1 over MQTT 3.1.1, max load | ~50 600 | 1.3 / 1.9 ms | ~90 % | ~9 MiB |
| QoS 1, steady 1 000/s | 1 000 | 0.25 / 0.43 ms | ~6 % | ~8 MiB |

Binary 5.0 MB, package 2.0 MB. The broker shares the machine, so a real broker elsewhere changes the numbers (the network's round trip comes on top for QoS 1 / 2; `max_inflight` hides it under load).

## Tests

`cargo test -p flwrs-mqtt-sink`: topic names and filters, the settings (broker URLs, every problem reported), building messages (the topic guard, sizes, MQTT 5 fields over 3.1.1), the package files against the CLI, and `tests/trellis.rs` against an embedded broker (rumqttd): the trellis audit, publishing over MQTT 5 and 3.1.1 at QoS 0, 1 and 2 (checked by a subscriber: payloads, QoS, content type, user properties, the guarded topic never arriving), NACKs while the broker is down, and recovering when it comes up. TLS isn't tested end to end yet (the test broker is built without TLS).
