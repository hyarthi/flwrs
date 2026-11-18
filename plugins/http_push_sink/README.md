# HTTP push sink (`flwrs-http-push-sink`)

An HTTP server that broadcasts events to subscribers: **Server-Sent Events** (`GET /subscribe/{channel}`, HTTP/1.1 and HTTP/2) and **long polling** (`GET /poll/{channel}`). Every subscriber of a channel gets every event sent to it; a ring of recent events per channel lets subscribers resume after a reconnect.

## Events in

| Field | Type | Meaning |
|---|---|---|
| `channel` | STRING | The channel (must match `allowed_channels`); empty: `default_channel`. |
| `event` | STRING | The event type: SSE `event:`, the envelope's `event`. |
| `data` | BYTES | The data. |
| `content_type` | STRING | The data's media type, passed on in envelopes. |

## Settings

| Key | Default | Meaning |
|---|---|---|
| `ack_mode` | `"delivered"` | `delivered`: an event is acked once a subscriber's connection took it (an SSE stream wrote it, or a long poll answered with it). While nobody does, it waits **unacked**, so the hub holds the rest back in the node's queue (durable, if it is); it reaches the first subscriber that comes. `buffered`: acked once it's in the ring (faster; with nobody subscribed it's only kept for replay). |
| `delivery_timeout_ms` | `0` | `delivered`: refuse an event (retryable) nobody took in this long; 0 = wait as long as it takes. A refused event leaves the ring, and the hub delivers it again (up to the node's `delivery.max_attempts`). |
| `replay_events`, `replay_bytes` | `1000`, 8 MiB | The ring, per channel. Larger events are refused. A `delivered` event pushed out before anyone took it is refused (retryable). |
| `slow_policy` | `"disconnect"` | A subscriber that falls behind the ring: its stream ends (after an `flwrs-lagged` event), or (`skip`) it goes on from the oldest kept event after an `flwrs-gap` event. The producer never waits for a subscriber. |
| `allowed_channels` | any | Channel name patterns (`*` within a `/` segment, `**` across), for events and subscribers. Others: the event is refused for good, a subscriber gets 404. |
| `default_channel` | `"default"` | |
| `max_channels`, `max_subscribers`, `max_subscribers_per_channel` | 256, 10 000, 1 000 | Beyond them: 503 for subscribers, a retryable refusal for events on new channels. |
| `base_path` | | A prefix for both routes. |
| `sse_format` | `"raw"` | `raw`: the data as `data:` lines (non-UTF-8 data as base64, with an `encoding: base64` line, which EventSource ignores). `json`: one `data:` line holding the envelope (below), for browsers that need binary data or the content type. |
| `heartbeat_s`, `retry_ms` | 15, 3000 | SSE keep-alive comments, and the `retry:` hint. |
| `poll_max_events`, `poll_max_wait_s` | 100, 25 | Caps for a poll's `max` and `wait_s` (shorter than `limits.request_timeout_ms`). |
| `allowed_origins` | none | CORS: the `Origin`s browsers may subscribe from, or `["*"]`. Preflights are answered without auth (browsers send them without credentials). |
| `max_in_flight` | `64` | Events handled at once (`delivered`: waiting for subscribers at once). |
| `listen`, `auth`, `tls_*`, `limits`, … | | The server core's settings, as for the HTTP source. Browsers' `EventSource` can't set headers: use `api_key` in the query, cookies through a proxy, or mTLS. |

## Subscribing

**SSE:** `GET /subscribe/{channel}` (the channel may contain `/`). Each event is `id: <epoch>-<seq>`, `event:` (if set), `data:`. To resume, send `Last-Event-ID` (EventSource does it when it reconnects) or `?last_event_id=`; the events after it come from the ring. Without one, the stream starts with what comes next (and with `delivered` events still waiting for a first subscriber).

**Long poll:** `GET /poll/{channel}?cursor=<id>&max=<n>&wait_s=<s>` answers at once when there are events after `cursor`, or waits up to `wait_s` for one:

```json
{ "events": [ { "id": "mf3k2-17", "event": "note", "content_type": "text/plain", "data": "hello" } ],
  "next_cursor": "mf3k2-17" }
```

Binary data is base64 with `"encoding": "base64"`. Poll again with `next_cursor`. Without a cursor, a poll starts from now.

**Gaps:** an id from an earlier run of the plugin (the epoch changes each start), or one the ring has dropped, gives a gap first: SSE `event: flwrs-gap` with `data: {"missed": n}` (`null` when unknown), a poll's `"missed"`; then everything the ring has.

## Delivery

`delivered` is at least once to *some* subscriber: the first one that takes an event acks it, so with several subscribers on a channel the others get it too only if they're connected (or resume within the ring). Restarts lose the ring (memory only) but not unacked events: the hub delivers them again. When the node stops, waiting events are refused at once (the SDK's shutdown hint), so a stop isn't held up.

## Footprint

Measured with `cargo test --release -p flwrs-http-push-sink --test bench -- --ignored --nocapture` (`benches/`, `tests/bench.rs`: SSE subscribers on channel `bench` in the test process, raw TCP / TLS readers and hyper HTTP/2 clients; CPU and memory are the plugin's alone) on an Apple M2 (macOS), median of three 5-second runs. At a steady rate, latency counts from when the harness woke for each send (its own timer lateness, ~0.22 ms, is reported apart). CPU per event = CPU time ÷ events. Max load is a closed loop with 64 events in flight; ~130-byte JSON events. TLS is rustls with ring (TLS 1.3, AES-GCM), with a self-signed certificate uploaded as the node's `tls_cert` / `tls_key`; connections stay open, so handshakes aren't in these numbers (each new connection costs one).

| Scenario | Events/s | p50 / p99 | CPU | RSS |
|---|---|---|---|---|
| `delivered`, 1 subscriber, max load | ~120 600 | 0.44 / 0.79 ms | ~127 % | ~10 MiB |
| `buffered`, 1 subscriber, max load | ~109 200 | 0.48 / 0.88 ms | ~138 % | ~10 MiB |
| `delivered`, 1 subscriber, TLS | ~123 700 | 0.42 / 0.82 ms | ~126 % | ~12 MiB |
| `delivered`, 1 subscriber, HTTP/2 (h2c) | ~44 600 | 1.4 / 2.4 ms | ~98 % | ~11 MiB |
| `buffered`, 1 000 subscribers | ~1 230 (≈ 1.2 M writes/s) | | ~275 % | ~41 MiB |
| `buffered`, 1 000 subscribers, TLS | ~1 230 | | ~261 % | ~56 MiB |
| `buffered`, 1 000 subscribers on 8 HTTP/2 connections | ~3 700 | | ~213 % | 56–137 MiB |
| `delivered`, steady 1 000/s | 1 000 | 0.14 / 0.25 ms | ~4 % | ~9 MiB |

- Per subscriber: ~31 KiB over HTTP/1.1, ~46 KiB with TLS, ~50–130 KiB per HTTP/2 stream. HTTP/2 streams queue what their subscriber hasn't read yet up to `limits.h2_max_send_buf_bytes` (4 KiB by default): with hyper's own default (~400 KiB), 1 000 subscribers took up to ~760 MiB at ~1 000 events/s, at 64 KiB about the same; 4 KiB gave ~3 700 events/s at ~10 % less for a single fast stream.
- TLS costs no measurable CPU per event at these sizes (hardware AES; rustls also gathers writes into records).
- HTTP/2 SSE is slower per event: each event is a DATA frame with flow-control bookkeeping (and window updates from the client).
- With 1 000 subscribers the readers in the test process share the CPU with the plugin.

Binary 7.7 MB, package 2.9 MB.

## Tests

`cargo test -p flwrs-http-push-sink`: SSE text, the package files, and `tests/trellis.rs`: the audit (`buffered`), `delivered` (a held event refused after the timeout and gone from the ring, one acked once an SSE subscriber took it, one held until a late subscriber came, heartbeats), and `buffered` with a 3-event ring: auth, a stale cursor (gap + ring), resuming by cursor and by `Last-Event-ID`, a counted gap, an empty wait, binary data, an unknown channel (404), CORS headers and preflights.
