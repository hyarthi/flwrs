# WebSocket source (`flwrs-ws-source`)

A WebSocket server that turns messages into events. Its paths and messages come from an **AsyncAPI 3.0 / 3.1** document: the channels of the document's `receive` operations are the paths clients connect to (their `address`), and their messages' JSON Schema payloads type and validate what clients send. Without a document, the node takes connections on `paths` (default `/ws`) and every message is an event, as bytes.

## Settings

| Key | Default | Meaning |
|---|---|---|
| `asyncapi` | | The API: an AsyncAPI 3.0 or 3.1 document, as JSON or YAML text, or as an object. It must be self-contained: `$ref`s only inside the document. |
| `paths` | `["/ws"]` | Without a document: the paths clients connect to. |
| `subprotocols` | | The subprotocols the server speaks (`Sec-WebSocket-Protocol`), in order of preference. A client offering none of them still connects, without one. |
| `allowed_origins` | any | The `Origin`s browsers may connect from (403 otherwise). Clients that send no `Origin` (not browsers) aren't affected. |
| `max_message_bytes` | `1048576` | A larger message or frame closes the connection (1009). |
| `ping_interval_s` | `30` | The server pings each client this often; a client that has sent nothing (not even a pong) since the last ping is closed. |
| `enqueue_timeout_ms` | `5000` | How long a message waits for the workflow to take it; then the connection closes with 1013 (try again later). |
| `invalid_messages` | `"drop"` | A message no definition of its channel takes: `drop` (a WARN once a minute with the count) or `close` (1008). |
| `out_schema` | (derived) | The node's out schema, set by the plugin's configurator from `asyncapi`; read-only. |
| `listen`, `auth`, `tls_*`, `limits`, … | | The server core's settings, as for the HTTP source: auth (`none`, `bearer`, `api_key` in a header or the query, `mtls`; a non-loopback listener needs one, or `auth: none` explicitly), TLS (HTTP/1.1 upgrades; HTTP/2 extended CONNECT is untested), `limits` (`max_connections` also bounds open WebSockets: 503 beyond it), `/healthz` and `/readyz`, the header policy. |

Browsers can't set headers on a WebSocket: use `api_key` with `api_key_in: query` (or cookies through a proxy, or mTLS) for them.

## Connections

The upgrade request (`GET` on a channel's address) is checked in order: routing (404), auth (401, 403), `Origin` (403), the hub connection (503 while the node isn't connected to its workflow), a free connection slot (503), then the subprotocol. A connection stays open until the client closes it, misses a ping, sends something too large (1009), or the node stops (1001).

## Messages

With a document, each text or binary message is matched against its channel's messages, in the document's order: a JSON message (`contentType` `application/json` or `…+json`, the document's `defaultContentType`, else JSON) takes a message that parses as JSON and passes its payload schema (no schema: any JSON); a non-JSON message definition takes anything the JSON ones didn't, as bytes. A message nothing takes is dropped or closes the connection (`invalid_messages`).

Every event has the common fields `connection_id` (a ULID per connection), `path` (the channel's address, `/devices/{deviceId}`), `uri` (path and query as requested), `message_type` (`text` / `binary`), `message` (the name of the message definition that took it), `data` (the message as sent), `headers` (the upgrade request's, lowercase names, credentials left out), `remote_addr`, `principal` and `receive_time`. Then, typed by the document:

```
channels["/devices/{deviceId}"].path_params.deviceId     string
channels["/devices/{deviceId}"].messages.reading.temp    f64
```

Only the matching message's part is set. JSON Schema becomes flwrs types as in the HTTP source (the same converter: `flwrs-plugin-kit`'s `schema`); what flwrs can't type (`oneOf`, free-form objects, …) is refused when the node is saved. Payloads in other schema formats (Avro, …) are refused. The out schema comes from the plugin's **configurator** (`bin/flwrs-ws-source-config`); the plugin checks at start that it still matches the document.

Ignored, with a warning: `send` operations (what the server would send: a push sink's job). Channel parameters must be whole path segments (`/devices/{id}`, not `/devices/{id}.json`); the document's `servers` and security schemes aren't used (the node's `listen` and `auth` apply).

## Delivery and backpressure

Messages are read one at a time per connection, and each waits until it's sent to the hub, so a slow workflow slows its clients down through TCP; one not taken within `enqueue_timeout_ms` closes the connection with 1013. WebSocket has no acknowledgements of its own: a message the client sent just before the node or the connection failed can be lost (at most once). Application-level acks (a reply once the hub confirmed the event) are a possible later setting; connect / disconnect events too.

## Footprint

Measured with `cargo test --release -p flwrs-ws-source --test bench -- --ignored --nocapture` (`benches/`, `tests/bench.rs`: tokio-tungstenite clients in the test process; CPU and memory are the plugin's alone) on an Apple M2 (macOS), median of three 5-second runs. At a steady rate, latency counts from when the harness woke for each send (its own timer lateness, ~0.22 ms, is reported apart). CPU per event = CPU time ÷ events. Max load: 32 connections send ~50-byte JSON readings back to back, counted against a credit window of 64. TLS is rustls with ring (TLS 1.3, AES-GCM), with a self-signed certificate uploaded as the node's `tls_cert` / `tls_key`; connections stay open, so handshakes aren't in these numbers (each new connection costs one).

| Scenario | Events/s | CPU | CPU per event | RSS |
|---|---|---|---|---|
| typed by AsyncAPI (matched, validated), max load | ~50 100 | ~73 % | ~15 µs | 22–32 MiB |
| no document (bytes), max load | ~64 500 | ~82 % | ~13 µs | 18–31 MiB |
| typed, `wss` (TLS), max load | ~51 300 | ~75 % | ~15 µs | 24–34 MiB |
| typed, steady 1 000/s (send → event): p50 / p99 0.10–0.15 / 0.2–0.5 ms | 1 000 | ~2 % | | ~17 MiB |

TLS costs no measurable CPU per event at these sizes, ~2 MiB more memory. The credit queue of flwrs-plugin 0.0.8 (senders served in order, longest-waiting first) raised max load from ~37 000 (typed) and ~41 000 (bytes).

Binary 17.9 MB (jsonschema and the AsyncAPI parser), package 7.4 MB (with the configurator).

## Tests

`cargo test -p flwrs-ws-source`: reading documents (receive channels, references, what can't be served), the out schema, the package files against the CLI, and `tests/trellis.rs` (a WebSocket client as the stimulus): the trellis audit (no document), typed messages over text and binary frames, dropped messages, a refused upgrade (404), `invalid_messages: close` (1008), backpressure (1013 without credit, then delivered), and the configurator (`readings.configure.toml`).
