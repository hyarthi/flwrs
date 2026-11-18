# WebSocket push sink (`flwrs-ws-push-sink`)

A WebSocket server that broadcasts events to connected clients: each client connects to `GET /ws/{channel}` and gets every event sent to that channel as a message. It's the HTTP push sink's broadcast core (channels, replay ring, acks) with a WebSocket transport.

## Events in

The same as the HTTP push sink: `channel`, `event`, `data` (BYTES), `content_type`.

## Settings

The broadcast settings are the HTTP push sink's (`ack_mode`, `delivery_timeout_ms`, `replay_events`, `replay_bytes`, `slow_policy`, `allowed_channels`, `default_channel`, `max_channels`, `max_subscribers`, `max_subscribers_per_channel`): see its README. The server settings (`listen`, `auth`, `tls_*`, `limits`, …) are the server core's. Its own:

| Key | Default | Meaning |
|---|---|---|
| `base_path` | | A prefix for the route. |
| `frame` | `"auto"` | How an event becomes a message: `auto` (text if the data is UTF-8, else binary), `text`, `binary`, or `envelope`: JSON text `{id, event, content_type, data, encoding?}` (binary data as base64), with `{"type": "gap", "missed": n}` markers. Clients that resume need `envelope` (the other frames carry no ids, and gaps aren't shown). |
| `subprotocols` | | The subprotocols the server speaks, in order of preference. |
| `allowed_origins` | any | The `Origin`s browsers may connect from, or `["*"]` (403 otherwise). Clients without an `Origin` aren't affected. |
| `ping_interval_s` | `30` | A client that has sent nothing (not even a pong) since the last ping is closed. |
| `write_timeout_ms` | `10000` | A message not written in this long (a stuck client) closes the connection. |
| `max_client_message_bytes` | `4096` | Client messages are ignored; a larger one closes the connection (1009). |
| `max_in_flight` | `64` | Events handled at once (`delivered`: waiting for clients at once). |

Browsers can't set headers on a WebSocket: use `api_key` in the query, cookies through a proxy, or mTLS for them.

## Connecting

`GET /ws/{channel}` (the channel may contain `/`), `?after=<id>` to resume after an envelope's `id`. Before the upgrade: auth (401, 403), `Origin` (403), the channel (404 if it isn't allowed) and the limits (503). Then messages flow until the client closes, misses a ping, stalls a write, falls behind the ring with `slow_policy: disconnect` (closed with 1008), or the node stops (1001).

An event counts as delivered (for `ack_mode: delivered`) once its message is written to the connection.

## Footprint

Measured with `cargo test --release -p flwrs-ws-push-sink --test bench -- --ignored --nocapture` (`benches/`, `tests/bench.rs`: tokio-tungstenite clients on channel `bench` in the test process; CPU and memory are the plugin's alone) on an Apple M2 (macOS), median of three 5-second runs. At a steady rate, latency counts from when the harness woke for each send (its own timer lateness, ~0.22 ms, is reported apart). CPU per event = CPU time ÷ events. Max load is a closed loop with 64 events in flight; ~130-byte JSON events. TLS is rustls with ring (TLS 1.3, AES-GCM), with a self-signed certificate uploaded as the node's `tls_cert` / `tls_key`; connections stay open, so handshakes aren't in these numbers (each new connection costs one).

| Scenario | Events/s | p50 / p99 | CPU | RSS |
|---|---|---|---|---|
| `delivered`, 1 client, max load | ~103 900 | 0.52 / 0.85 ms | ~132 % | ~10 MiB |
| `buffered`, 1 client, max load | ~105 100 | 0.51 / 0.89 ms | ~140 % | ~10 MiB |
| `delivered`, 1 client, `wss` (TLS) | ~114 900 | 0.49 / 0.88 ms | ~130 % | ~11 MiB |
| `buffered`, 1 000 clients | ~505 (≈ 505 k messages/s) | | ~184 % | ~21 MiB |
| `buffered`, 1 000 clients, `wss` | ~535 | | ~211 % | ~36 MiB |
| `delivered`, steady 1 000/s | 1 000 | 0.16 / 0.30 ms | ~4 % | ~10 MiB |

- Per client: ~11 KiB (`read_buffer_bytes` 4 KiB, `write_buffer_bytes` 16 KiB; with tungstenite's 128 KiB defaults it was ~130 KiB), ~26 KiB with TLS.
- TLS costs no measurable CPU per event at these sizes.
- With many clients, each message is written and flushed on its own (the SSE sink's body is batched by hyper): see `ideas/batching-and-compression.md`. The 1 000 tungstenite clients in the test process also cost more than the SSE test's raw readers.

Binary 7.9 MB, package 3.0 MB.

## Tests

`cargo test -p flwrs-ws-push-sink`: frames, the package files, and `tests/trellis.rs` (tokio-tungstenite clients): the audit (`buffered`), `delivered` (a held event refused after the timeout and gone from the ring, one acked once a client got it, one held until a late client came), and `envelope` frames with a 3-event ring: auth (401), a stale id (gap + ring), resuming after an id, a counted gap, binary data as base64, a channel not allowed (404), Origins (403 / allowed).
