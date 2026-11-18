# Idea: application-level acks in the WebSocket source

Status: idea, deferred (phase 8, Nov 2026): a configuration-time feature, with modes per integration (and "none", today's behaviour).

## Problem

WebSocket has no acknowledgement of its own: a message the client sent just before a crash or a lost connection may be lost (at most once). Clients that need at-least-once want a reply once the hub has the event.

## Sketch

- `ack: none` (default) / `reply`: once the event is confirmed (`emit_tracked` + `confirmed()`, as the HTTP source's `confirmed` mode), the source sends a reply built from a template, e.g. `{"ack": "${id}"}`, with the id taken from the message at a JSON pointer (`ack_id_path`); a NACK replies `{"nack": "${id}", "reason": …}` (also templated).
- Messages without an id at that path: no reply (or an error reply, configurable).
- AsyncAPI documents can describe the reply message (`reply` in operations, 3.0): the template could come from there.

## Open questions

- Several messages in flight per connection (today one at a time): needed for throughput once replies exist.
- Integrations differ (JSON-RPC ids, STOMP receipts, custom envelopes): templates may not cover them all.
