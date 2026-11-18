# Idea: idempotency keys for inbound requests

Status: idea, not scheduled (plugins plan §4.2).

## Problem

With `ack_mode: confirmed`, an HTTP client that retries after a lost answer sends the same request twice, and the HTTP source emits two events. Delivery is at least once; downstream can't tell a retry from a new request.

## Sketch

- An `idempotency_key_header` setting (e.g. `Idempotency-Key`): its value is copied into the event (a field in the envelope), so downstream can dedupe.
- Optionally, the source dedupes itself: a bounded window of recent keys (count and age) in memory, or in the node's folder for restarts; a repeated key within the window gets the first request's answer without a new event.
- The same idea on the hub: a dedup window on the producer's event identity (the design notes' "dedup window on event_id").

## Open questions

- In-plugin dedup only covers one node; a key shared across several HTTP source nodes (behind a load balancer) needs the hub-side version.
