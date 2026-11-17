# HTTP sink (`flwrs-http-sink`)

A sink that sends an HTTP request for each event. Use it to call webhooks and REST APIs at the end of a workflow, or to hand events from an edge scene to a central one (with an HTTP source there).

## Input

| Field | Type | |
|---|---|---|
| `url` | STRING | Where to send the request. |
| `method` | STRING | `GET`, `POST`, `PUT`, … |
| `headers` | MAP of ARRAY of STRING | Request headers; a header can have several values. |
| `body` | BYTES | The request body (from a mapping, a string becomes its UTF-8 bytes). |

An edge mapping usually builds these from the producer's fields, e.g. `{"rules": [{"target": "url", "value": "https://example.com/hook"}, {"target": "method", "value": "POST"}, {"target": "body", "source": "text"}]}`.

## Settings

`launch_args`, all optional:

| Key | Default | Meaning |
|---|---|---|
| `http_timeout_seconds` | `60` | Total time for one request. |
| `http_read_timeout_seconds` | `30` | Time to wait for the response. |
| `http_connect_timeout_seconds` | `30` | Time to connect. |
| `max_concurrent_requests` | `16` | Requests in flight at once (1–1024). Use `1` to keep the events' order. |
| `http_verbose_logging` | `false` | Log connection details. |

## Delivery

Each event is acknowledged once its response arrives with a success status. A network error, a timeout, `408`, `429` or a `5xx` answer is a retryable NACK: the server delivers the event again after the node's `retry_delay_ms`, up to `max_attempts`. An invalid request or another `4xx` is a NACK without retry (the event is dead-lettered). So delivery is at-least-once: a request that succeeded but whose answer was lost is sent again.

## Tests

`cargo test -p flwrs-http-sink`: unit tests, checks that the CLI flags and the package files agree, and `tests/trellis.rs` (the trellis audit, and `tests/trellis/delivery.test.toml` against a mock HTTP server: 200 → ACK, 503 → retryable NACK, 400 → NACK).
