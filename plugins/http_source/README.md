# HTTP source (`flwrs-http-source`)

An HTTP server that turns requests into events. Routes, parameters and bodies come from an **OpenAPI 3.1** document: requests that don't match it are answered with 4xx and never become events, and the ones that do become events whose fields are typed by the document. Without a document the node only answers health and readiness checks.

## Settings

| Key | Default | Meaning |
|---|---|---|
| `openapi` | | The API: an OpenAPI 3.1 document, as JSON or YAML text, or as an object. It must be self-contained: `$ref`s only inside the document (`#/components/…`). |
| `base_path` | | A prefix for every route (`/v1`). The document's `servers` aren't used for routing. |
| `ack_mode` | `"confirmed"` | `confirmed`: a request gets its 2xx once the hub has its event (committed, with a durable queue), so a client that retries on 5xx gets at-least-once delivery. `accepted`: once the event is sent (faster; a crash can lose it). |
| `enqueue_timeout_ms` | `5000` | How long a request waits for the workflow to take its event (backpressure); then 503 with `Retry-After: 1`. |
| `hide_error_details` | `false` | Answer 400s without the validation messages. |
| `swagger_ui` | off | `{"enabled": true, "path": "/docs", "public": false}`: Swagger UI for the document (and the document at `<path>/openapi.json`), behind the node's auth unless `public`. "Try it out" sends real requests, which become real events. |
| `out_schema` | (derived) | The node's out schema, set by the plugin's configurator from `openapi`; read-only. |
| `listen`, `auth`, `tls_*`, `limits`, … | | The server core's settings (shared with flwrs' other HTTP plugins): see below. |

**Template:** in the web UI, an empty `openapi` starts with a sample document ([`templates/openapi.yaml`](templates/openapi.yaml): one `POST` route with a path parameter, a query parameter and a JSON body). It works as it is; edit it into your API.

### The server

From `flwrs-plugin-kit`'s server core:

- `listen` (`127.0.0.1:8080`). **A listener on anything but loopback refuses to start without `auth`**, unless `auth: none` is set explicitly (then it starts with a WARN).
- `auth`: `none`, `bearer` (`Authorization: Bearer <token>`), `api_key` (`api_key_name` in a header or the query, `api_key_in`) or `mtls` (client certificates from `tls_client_ca`, optionally limited by `mtls_allow` fingerprints or names). Tokens and keys are `auth_secrets` (a secret): `{"label": "secret"}`; a request's event gets the label as its `principal`, never the secret.
- TLS: `tls_cert`, `tls_key` (a secret), `tls_client_ca`, `tls_client_auth` (uploaded PEM files). HTTP/2 and HTTP/1.1 by ALPN; `h2c` for cleartext HTTP/2.
- `limits`: `max_body_bytes` (1 MiB, 413), `max_header_bytes`, `request_timeout_ms` (30 s), `header_read_timeout_ms` (10 s), `max_connections`, `max_concurrent_requests` (503), `h2_max_streams`, `h2_max_send_buf_bytes` (4 KiB: what one HTTP/2 stream may have queued before its writer waits; hyper's own default, ~400 KiB, is per slow SSE subscriber).
- `/healthz` (200 while it runs) and `/readyz` (200 while it's connected to the hub with credit to send, else 503), without auth; `health_path` / `ready_path` move them.
- `header_allowlist` / `header_denylist`: which request headers events carry (by default everything but credentials and hop-by-hop headers).

## Requests

In this order:

1. Routing: an unknown path is 404, a method the path doesn't have 405.
2. Auth (401, 403) and size (413), from the server core.
3. The body's content type against the operation's `requestBody` (415). `application/json` and `…+json` bodies are parsed; `application/x-www-form-urlencoded` ones too (values converted by their property's type); any other media type (`application/octet-stream`, `image/*`, `multipart/*`, …) passes through as bytes.
4. Path and query parameters: converted from text by their schema's type (`?n=3` is an integer, `?tag=a&tag=b` an array; `explode: false` takes `?tag=a,b`), then checked against their schemas; a missing required one is an error (400).
5. The body against its schema (400). Errors are `application/problem+json`, with the messages unless `hide_error_details`.
6. The event, emitted by `ack_mode`; then the operation's lowest 2xx response (202 without one), with an empty body.

Not connected to the hub: 503 at once. The hub refusing the event for good (e.g. too large for it): 500.

## Events

Every event has the common fields `method`, `path` (the matched template, `/devices/{id}`), `uri` (path and query as sent), `headers` (a map of string arrays, lowercase names), `content_type`, `body` (the raw bytes, also for typed bodies: e.g. to check a webhook's signature), `remote_addr`, `http_version`, `principal` and `receive_time`. Then, typed by the document:

```
paths["/devices/{id}"].post.path_params.id     string
paths["/devices/{id}"].post.query.verbose      bool
paths["/devices/{id}"].post.body.temp          f64
```

Only the matching operation's part is set. Edge mappings use these paths (quoted keys for the templates), e.g. a filter on `method` and a copy from `paths["/devices/{id}"].post.body.temp`.

### From JSON Schema to flwrs types

| JSON Schema | flwrs |
|---|---|
| `string` | STRING; `format: date-time` → DATETIME; `format: binary` / `byte`, `contentEncoding: base64` → BYTES |
| `integer` | I64; `format: int32` → I32 |
| `number` | F64; `format: float` → F32 |
| `boolean` | BOOL |
| `array` with `items` | ARRAY |
| `object` with `properties` | OBJECT (properties in key order) |
| `object` with an `additionalProperties` schema only | MAP |
| `allOf` of objects | one OBJECT (merged) |
| `type: [T, "null"]`, `enum` of strings | T, STRING |

Everything else is refused when the node is saved, naming its place in the document: `oneOf` / `anyOf` / `not` / `if`, free-form objects (no properties), several types, arrays without `items`, recursive schemas, parameters with `content`, object parameters, query styles other than `form`. The out schema comes from the plugin's **configurator** (`bin/flwrs-http-source-config`, see the plugin specification): the server runs it when the node's settings change, so the out schema is known before the plugin runs, and the plugin checks at start that it still matches.

Ignored, with a warning: webhooks and callbacks (requests the API would send), security requirements (the node's `auth` applies), `servers` paths (use `base_path`), cookie parameters.

## Delivery and backpressure

- `confirmed` (default): 2xx after the hub took the event (for a durable queue: committed). A client that retries 5xx and timeouts gets at-least-once delivery; duplicates are possible (a retry after a lost answer).
- Backpressure is HTTP: when the workflow is slow, requests wait up to `enqueue_timeout_ms`, then get 503 with `Retry-After`; `/readyz` turns 503 too, so a load balancer can steer away.
- Shutdown: the server stops accepting, requests in progress get 5 s to finish (their events still go out).

## Footprint

Measured with `cargo test --release -p flwrs-http-source --test bench -- --ignored --nocapture` (`benches/`, `tests/bench.rs`: keep-alive HTTP/1.1 and hyper HTTP/2 clients in the test process; CPU and memory are the plugin's alone) on an Apple M2 (macOS), median of three 5-second runs. At a steady rate, latency counts from when the harness woke for each send (its own timer lateness, ~0.22 ms, is reported apart). CPU per event = CPU time ÷ events. Max load: 32 requests in flight (typed JSON bodies on `/devices/{id}`, validated), counted against a credit window of 64. TLS is rustls with ring (TLS 1.3, AES-GCM), with a self-signed certificate uploaded as the node's `tls_cert` / `tls_key`; connections stay open, so handshakes aren't in these numbers (each new connection costs one).

| Scenario | Events/s | CPU | CPU per event | RSS |
|---|---|---|---|---|
| `confirmed`, HTTP/1.1 (32 connections), max load | ~41 900 | ~158 % | ~38 µs | ~18 MiB |
| `accepted`, HTTP/1.1, max load | ~35 400 | ~132 % | ~37 µs | ~18 MiB |
| `confirmed`, HTTP/1.1 + TLS, max load | ~44 500 | ~166 % | ~37 µs | ~20 MiB |
| `confirmed`, HTTP/2 h2c (1 connection, 32 streams) | ~29 100 | ~131 % | ~45 µs | ~19 MiB |
| `accepted`, HTTP/2 h2c (1 connection, 32 streams) | ~30 900 | ~135 % | ~44 µs | ~19 MiB |
| `confirmed`, HTTP/2 h2c (8 connections × 4 streams) | ~32 600 | ~150 % | ~46 µs | ~19 MiB |
| `confirmed`, HTTP/2 + TLS (1 connection, 32 streams) | ~31 000 | ~146 % | ~47 µs | ~20 MiB |
| `confirmed`, steady 1 000/s (request → event): p50 / p99 0.15 / 0.23 ms | 1 000 | ~5 % | | ~15 MiB |

- **TLS** costs no measurable CPU per event at these sizes (hardware AES; rustls also gathers small writes into records, saving syscalls), ~2 MiB more memory.
- **HTTP/2** does more work per request than HTTP/1.1 (frames, HPACK, flow control, stream state: ~45 vs ~38 µs), and one connection's requests go through one connection task on each side. Its gain is fewer connections, not speed per request: keep-alive HTTP/1.1 already avoids reconnects.
- `accepted` is below `confirmed`: its clients outrun the credit window and take turns at it (since flwrs-plugin 0.0.8 in order, longest-waiting first; before, every grant woke every waiting request: ~20 100/s).

Binary 22 MB (jsonschema, the OpenAPI parser and Swagger UI's assets), package 8.3 MB (with the configurator).

## Tests

`cargo test -p flwrs-http-source`: reading documents (YAML, references, what can't be served), the conversion to flwrs types (and what's refused), the package files against the CLI, and `tests/trellis.rs`: the trellis audit (requests as its stimulus), requests becoming typed events and the ones refused (400, 415, 404) with their statuses, backpressure (503 without credit, then 201), Swagger UI's document and readiness, and the configurator (`*.configure.toml`: the out schema for a document, a YAML document, `oneOf` refused).
