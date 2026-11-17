# Idea: byte-based credits

Status: idea, not scheduled. Extends the `CREDITS` capability (protocol v2). This is open question 1 in `docs/protocol.md`.

## Problem

Credits count events only (`Credit { events }`). The hub's real limits are in bytes, though: `max_inflight_bytes` (512 MiB hub-wide), the queues' byte bounds, the spool's disk limits. A single event may be as large as `max_event_bytes` (16 MiB). So the event window says little about memory:

- A producer with the default window of 64 may have up to 64 × 16 MiB = 1 GiB in flight, twice the hub-wide budget. The hub has to read each frame into memory before it can decide anything. Today it copes by routing slower (credit comes back later) and by skipping oversized bodies (`read_frame_limited`, NACK). But by then the bytes are already on the wire or in its buffers.
- Producers whose event sizes vary widely (a camera source: small status events, then large frames) get the same window for both.
- There's no fairness by volume: one producer of large events uses far more of the shared byte budget than a producer of small ones with the same credit.

## Idea

An optional byte budget next to the event count:

```proto
message Credit {
  uint32 events = 1;
  uint64 bytes  = 2;  // additional body bytes allowed; only with CREDIT_BYTES
}
```

- **Negotiated** as a new capability (`CREDIT_BYTES`, which needs `CREDITS`). Old plugins and plugins in other languages that don't offer it keep counting events only.
- **What counts:** the length of the encoded `Event` body (what the frame carries, and what the hub measures for its queues), not the payload's logical size.
- **Rule:** a producer may send while it holds at least one event credit and a positive byte balance. One event may overdraw the byte balance, which then goes negative and must be refilled before the next send. Without that rule, an event larger than any single grant could never be sent.
- **Grants:** the hub derives byte grants from the room it has: the targets' byte bounds and the hub-wide budget, at least `max_event_bytes` at the start so any legal event can go. Additive, like event credit.
- **Violation:** a send with a non-positive byte balance is a protocol violation, as sending without event credit is.

## Benefits and costs

- **Benefits:** the hub never receives more than it has room for; memory bounds follow from the grants; byte fairness across producers becomes possible.
- **Costs:** one more balance in every SDK, and a rule (the overdraft) every implementer must get right. The conformance suite (trellis) needs cases for it. The grant scheduler in the hub gets more complex.

## Cheaper alternatives to try first

- **A smaller credit window per node** (a node runtime setting) for producers of large events. That's no protocol change.
- **Lower `max_event_bytes`** where large events aren't expected.

If those cover the real cases (camera frames, file contents), byte credits may not be worth the protocol surface.

## Open points

- Whether consumers need the same: `max_unacked` is a count too. A consumer window in bytes would cap what the hub sends to a slow sink.
- How grants interact with spilling queues (disk room as room, as event credits do today).
