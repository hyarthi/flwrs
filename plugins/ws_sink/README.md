# WebSocket sink (`flwrs-ws-sink`)

Sends each event as a WebSocket message, over one long-lived connection per server. An event is acknowledged once its message is written (`ack: write`), or once the server answered it (`ack: reply`).

## Settings

| Key | Default | Meaning |
|---|---|---|
| `url` | (required) | `ws://…` or `wss://…` (TLS). |
| `allowed_urls` | | URL prefixes that events may name their own `url` within. A prefix matches itself and what follows it after a `/` or `?` (so `ws://a.com` doesn't allow `ws://a.com.evil.net`). Without them, an event that names a URL is rejected. |
| `ack` | `"write"` | `write` or `reply`, see *Delivery*. |
| `message_type` | `"text"` | For events that don't set one: `text` (must be UTF-8) or `binary`. |
| `request_id_path`, `reply_id_path`, `reply_error_path` | | `ack: reply`: JSON paths (`id`, `meta.request_id`, `items[0].id`). `reply_id_path` is required. |
| `reply_timeout_ms` | `10000` | `ack: reply`: no reply in time is a retryable NACK. |
| `authorization` | | The `Authorization` header's value (e.g. `Bearer …`), a secret (delivered in `WS_AUTHORIZATION`). Sent to every URL. |
| `headers` | | Extra handshake headers (not secret). The handshake's own (`Host`, `Upgrade`, `Sec-WebSocket-*`, …) can't be set. |
| `subprotocols`, `origin` | | Offered subprotocols; the `Origin` header. |
| `tls_ca`, `tls_cert`, `tls_key` | | Uploaded PEM files: the server's CA (default: the bundled Mozilla roots, not the OS store), and a client certificate and its key (a secret) for mTLS. |
| `insecure_skip_verify` | `false` | Development only. |
| `lazy_connect` | `false` | Connect with the first event instead of at start (where a wrong URL or credentials show in the log at once). |
| `ping_interval_s` | `20` | A ping unanswered by the next one means a dead connection, which is redialled. |
| `max_message_bytes` | 16 MiB | Sent or received; a larger event is rejected (not retried). |
| `max_connections` | `16` | One per URL. When full, the longest-idle connection is closed for a new one; if all are busy, the event is NACKed retryably. |
| `idle_timeout_s` | `300` | A connection to an event's URL that wasn't used this long is closed (never `url`'s). |
| `max_concurrent` | `1` | Events handled at once. `1` keeps the order end to end; more is for `ack: reply` with a server that answers out of order. |

## Events

The input schema (`schema/in.json`): `url`, `data` (bytes, or a string), `message_type` and `correlation_id`. Every field is optional.

## Delivery

WebSocket has no acknowledgement per message, so:

- **`ack: write`** (default): the event is ACKed once its frame is written to the connection. A message still in the kernel's buffers when the connection drops is lost: **at most once over the wire**.
- **`ack: reply`**: for servers that answer each message. The request's id is the event's `correlation_id`, or generated; with `request_id_path` it's put into the outgoing JSON at that path (the data must then be a JSON object), without it the data must already carry the event's `correlation_id`. The event is ACKed when a message comes back whose `reply_id_path` holds the same id (a string, or a number as written), or NACKed: not retried if `reply_error_path` holds a value (not `null` or `false`), retried after `reply_timeout_ms` without a reply. **At least once**, end to end.
- Incoming messages that aren't a reply are ignored (counted at DEBUG).
- **Disconnected:** an event waits up to a second for the connection, then it's NACKed retryably; the plugin doesn't buffer while offline (the node's queue is the buffer). A send in flight when the connection drops is NACKed retryably too, and so is a reply that can no longer come.
- **Reconnects** use a backoff (1 s, doubling to 30 s, with jitter). A refusal that needs a config change (the handshake answered 400, 401, 403 or 404; the server closing with 1003 or 1008) is retried at the longest delay. A WARN when a connection drops, then at most once a minute while it stays down.
- **Rejected, not retried:** a URL outside `allowed_urls`, a wrong field type, text that isn't UTF-8, data that isn't JSON when the reply mode must insert an id, a message over `max_message_bytes`.

HTTP/1.1 upgrade only (no WebSocket over HTTP/2), and no `permessage-deflate` (tungstenite doesn't support it yet). No proxy support yet.

## Footprint

Measured with `cargo test --release -p flwrs-ws-sink --test bench -- --ignored --nocapture` (`benches/`, `tests/bench.rs`: trellis's bench against a WebSocket server in the test process; CPU and memory are the plugin's alone) on an Apple M2 (macOS), median of three 5-second runs. "Max load" is a closed loop with 64 events in flight. At a steady rate, latency counts from when the harness woke for each send (its own timer lateness, ~0.23 ms, is reported apart since trellis 0.0.5).

| Scenario | Events/s | p50 / p99 latency | CPU | RSS |
|---|---|---|---|---|
| `ack: write`, `max_concurrent` 1, max load | ~42 900 | 0.4 / 0.5 ms | ~40 % | ~9 MiB |
| `ack: write`, `max_concurrent` 16, max load | ~113 000 | 0.2 / 0.4 ms | ~120 % | ~9 MiB |
| `ack: reply`, `max_concurrent` 16, max load | ~94 900 | 0.3 / 0.5 ms | ~113 % | ~9 MiB |
| `ack: write`, steady 1 000/s | 1 000 | 0.16 / 0.33 ms | ~6 % | ~8 MiB |

Binary 4.8 MB, package 2.0 MB. ~120-byte JSON text messages.

## Tests

`cargo test -p flwrs-ws-sink`: JSON paths, the settings (URLs and the prefix rule, headers, every problem reported), building messages (URLs, types, reply ids), the package files against the CLI, and `tests/trellis.rs` against an in-test server: the trellis audit, `ack: write` (text and binary, a second connection for an allowed URL, the handshake's Authorization, Origin and subprotocol as the server saw them, a URL outside `allowed_urls` never sent), `ack: reply` (a generated id put into the JSON, a given one used as is, an error reply, a timeout, an unrelated message ignored), a refused handshake, and redialling after the server closes.
