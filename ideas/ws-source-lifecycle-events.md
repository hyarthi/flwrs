# Idea: connect and disconnect events in the WebSocket source

Status: idea, deferred (Nov 2026: "still need to think whether they are needed and how to best implement").

## Problem

A workflow may need to know when a client connects or leaves (presence, session start / end, cleanup), not just its messages.

## Options

- An `emit_lifecycle` setting adding a `kind` field (`message` / `connect` / `disconnect`) to the envelope, with `close_code` / `close_reason` for disconnects.
- Or separate event types on the same out schema, distinguished by `message` (`$connect`, `$disconnect`, like API Gateway's routes).

## Open questions

- Delivery: a disconnect event for a connection whose last messages are still waiting for credit must come after them (same order as the connection).
- What a "disconnect" is when the plugin itself stops (1001 to every client): one event per client at shutdown, or none.
- Whether presence belongs in the plugin at all, or in a stateful transform downstream.
