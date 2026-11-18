# Idea: persistent replay rings in the push sinks

Status: idea, not scheduled (phase 8, Nov 2026: rings are in memory; after a restart a client resuming with an old id gets a gap).

## Problem

A push sink restart (an upgrade, a crash, a scene reload that changes the node) loses every channel's ring, so reconnecting subscribers miss what was sent shortly before.

## Sketch

- `replay_persist: true`: each channel's ring in the node's folder (SQLite, like the script transform's state: one table per channel or one table with a channel column, rows dropped as the ring rotates), written in batches.
- The epoch then survives restarts (stored with the ring), so ids stay valid and resuming clients get no gap.
- With `ack_mode: buffered`, an event would be acked once it's in the persisted ring, a stronger guarantee than today's.

## Open questions

- Write cost at high rates (batching like durable queues' 5 ms window; see `adaptive-queue-batching.md`).
- A size limit for the file as a whole across channels (`replay_bytes` × channels may be large).
