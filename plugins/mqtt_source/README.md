# MQTT source (`flwrs-mqtt-source`)

Subscribes to an MQTT broker, over MQTT 5 (default) or 3.1.1, and turns each message into an event. By default a message is acknowledged to the broker only once the hub has its event, so a message is never lost between the broker and the scene: if the plugin, the hub or the connection fails first, the broker sends it again.

## Settings

| Key | Default | Meaning |
|---|---|---|
| `broker_url` | (required) | `mqtt://host[:1883]` or `mqtts://host[:8883]` (TLS). |
| `protocol_version` | `"5"` | `"5"` or `"3.1.1"`. |
| `subscriptions` | (required) | `[{"filter", "qos", "no_local", "retain_as_published", "retain_handling"}]`: topic filters (`+`, `#`) with their QoS (default 1). `$share/<group>/<filter>` is a **shared subscription**: the broker spreads the messages over the group's clients, so several nodes (or servers) can split one stream. `no_local`, `retain_as_published` and `retain_handling` (`send` / `new_only` / `never`) are MQTT 5 only. |
| `ack_mode` | `"confirmed"` | `confirmed`: a message is acknowledged once the hub has the event (at least once, end to end). `on_emit`: once the event is sent (faster; events in flight can be lost if the plugin or the server crashes). QoS 0 messages are never acknowledged (MQTT has no acks for them). |
| `username`, `password` | | Broker credentials. The password is a secret (delivered in `MQTT_PASSWORD`). |
| `tls_ca`, `tls_cert`, `tls_key` | | Uploaded PEM files: the broker's CA (default: the bundled Mozilla roots), a client certificate and its key (a secret) for mTLS. |
| `insecure_skip_verify` | `false` | Development only. |
| `client_id` | `flwrs-<node id>` | Must be unique per broker, and stable: the broker keeps the session by it. |
| `clean_start` | `false` | Off: the broker keeps the session (subscriptions and unacknowledged messages) while the plugin is away. On: every connection starts empty, and what was published meanwhile is lost. |
| `session_expiry_s` | 3600 | MQTT 5: how long the broker keeps the session after a disconnect. |
| `keepalive_s` | `30` | Seconds between pings. |
| `max_inflight` | `64` | QoS 1 / 2 messages the broker may send before they're acknowledged (MQTT 5 Receive Maximum). |
| `max_qos0_buffered` | `1024` | QoS 0 messages kept while the hub isn't taking events; more are dropped (a WARN counts them, at most once a minute). |
| `max_packet_bytes` | 1 MiB | Largest packet the broker may send. |
| `last_will` | | `{"topic", "payload", "qos", "retain"}`. |

## Events

The output schema (`schema/out.json`): `topic`, `payload` (bytes), `qos`, `retain`, `dup` (the broker may have sent it before), the MQTT 5 properties `content_type`, `response_topic`, `correlation_data`, `user_properties` (a map of string arrays) and `message_expiry_s`, and `receive_time`. Every event starts a new trace.

## Delivery and flow control

- **Confirmed** (the default): events go out in the order the messages came, several at a time; each message is acknowledged after its event's confirmation, in the same order (as MQTT requires). Events the hub hasn't confirmed when its connection drops are sent again after the reconnect.
- **Duplicates:** at least once means a message can arrive twice: after a crash or restart (the broker sends again what wasn't acknowledged, with `dup` set) or after a hub reconnect. Make downstream steps idempotent, or use the script transform's exactly-once state with a message id.
- **A refused event** (the hub NACKs it without retry, e.g. a full queue that drops): its message is acknowledged anyway, or it would block the broker's window forever. A WARN counts them.
- **Backpressure:** when the hub grants no credit, events wait; QoS 1 / 2 messages stay unacknowledged, so the broker stops at `max_inflight` (MQTT 5) or its own limit (3.1.1) and keeps the rest. The connection stays alive meanwhile (pings go on). QoS 0 messages keep coming and are buffered up to `max_qos0_buffered`, then dropped.
- **Before the subscription:** as always in MQTT, a message published before the subscription exists isn't delivered. With a persistent session (the default) that only matters at the very first start; after that the broker keeps the subscription and queues messages while the plugin is away.
- **Reconnects** to the broker use a backoff (1 s, doubling to 30 s, with jitter); the plugin subscribes again on every connection. A message received on an earlier connection can't be acknowledged on the new one: with a persistent session the broker sends it again (a duplicate). A shutdown disconnects cleanly, so the broker drops the Last Will.

## Footprint

Measured with `cargo test --release -p flwrs-mqtt-source --test bench -- --ignored --nocapture` (`benches/`, `tests/bench.rs`: trellis's bench against an embedded rumqttd broker and a publisher in the test process; CPU and memory are the plugin's alone) on an Apple M2 (macOS), median of three 5-second runs. "Max load" is a closed loop with 64 events in flight. At a steady rate, latency counts from when the harness woke for each send (its own timer lateness, ~0.23 ms, is reported apart since trellis 0.0.5). Max load: a publisher floods the broker and the bench counts events against a credit window of 64.

| Scenario | Events/s | p50 / p99 latency | CPU | RSS |
|---|---|---|---|---|
| QoS 1, `confirmed`, max load | ~32 700 | | ~82 % | 8–22 MiB |
| QoS 1, `on_emit`, max load | ~31 100 | | ~77 % | 8–22 MiB |
| QoS 0, max load | ~31 800 | | ~94 % | 9–24 MiB |
| QoS 1, `confirmed`, steady 1 000/s (publish → event) | 1 000 | 0.22 / 0.34 ms | ~6 % | ~22 MiB |

Binary 5.1 MB, package 2.1 MB. The broker (in the same process as the publisher) is likely the limit at max load: `confirmed` and `on_emit` come out the same. Memory varies between runs of the same scenario (8–24 MiB); not looked into yet.

## Tests

`cargo test -p flwrs-mqtt-source`: subscription filters (shared ones too), the settings, the inbox (messages kept until confirmed, requeued in order, the QoS 0 cap), the package files against the CLI, and `tests/trellis.rs` against an embedded broker (rumqttd): the trellis audit (with a publisher as its stimulus), messages as events over MQTT 5 (topic, payload, QoS, content type, user properties) and 3.1.1, and **a message that survives a crash**: with no credit it can't be emitted, the plugin is killed, and after the restart the broker delivers it again. TLS isn't tested end to end yet.
