# Idea: a WebSocket client source

Status: idea, not scheduled. The WS source is a server (clients connect to it); the WS sink is a client that sends.

## Problem

Many feeds are WebSocket servers to subscribe to (exchange tickers, vendor event streams, Home Assistant, chat APIs). Today that needs a custom plugin.

## Sketch

- A source that dials out (`url`, headers, the `authorization` secret, TLS uploads, subprotocols, like the WS sink), optionally sends a subscribe message after connecting (a template), and turns every message into an event (the WS source's envelope, typed by an AsyncAPI document's `send` operations when there is one: the server's side of the conversation).
- Reconnects with backoff, re-sending the subscribe message; pings as the WS sink does.
- Backpressure: stop reading while there's no credit (the server's TCP window fills), with a `max_buffered` cap and a drop policy for feeds that can't be slowed down.

## Open questions

- One package (a `mode: server | client` in the WS source) or a separate `ws-client-source`, as the sinks are split.
