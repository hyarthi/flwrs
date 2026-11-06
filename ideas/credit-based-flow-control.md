# Idea: credit-based flow control

Status: idea, not scheduled. Candidate follow-up to the director's TCP-level backpressure.

## Problem

Today, backpressure works at the TCP level. When a target's queue is full, the hub stops reading the producing plugin's socket, the TCP window fills, and the plugin's next send blocks. It's universal (any language gets it from plain blocking writes) and needs no protocol support, but it has costs:

- **System messages wait behind data.** While the hub isn't reading a socket, that plugin's `LogEvent`, `ErrorEvent` and `Exit` are stuck behind queued data events. Logs arrive late, and `Exit` is noticed late (for local plugins, process exit is still detected from the OS).
- **Backpressure is implicit.** A plugin can't tell "hub is busy" from "network is slow". It can't adapt: batch, sample, coalesce, or drop on its own terms.
- **TCP buffers are extra unbounded slack.** Kernel send and receive buffers (often hundreds of KiB to several MiB per connection) hold events outside the hub's count and byte limits.
- **No fairness control.** The hub can only stop reading a socket entirely. It can't let a producer continue at a reduced rate.

## Idea

The hub grants each producing plugin **credits**: permission to send N data events, optionally limited to B bytes as well. A plugin may only send a data event (`SourceEvent`, or a `TransformEvent` it produces) while it holds credit, and each event uses one credit (plus its size from the byte budget). **System messages never need credit**, so logs, errors and `Exit` always flow.

```
hub ──Credit{events: 64, bytes: 4 MiB}──► plugin
plugin ──event, event, … (≤ 64 events, ≤ 4 MiB)──► hub
hub ──Credit{events: 32}──► plugin   (top-up as the target queues drain)
plugin ──LogEvent / ErrorEvent / Exit──► hub    (always allowed, no credit used)
```

- The hub derives credit from the room left in the target queues of the producer's out-edges: the minimum over all targets with a `block` policy, also bounded by the hub-wide budget. With spill to disk, disk capacity counts as room.
- Credits are **additive grants** (`Credit` adds to the current balance), so grants never need to be acknowledged and can't race.
- A plugin out of credit waits for the next grant, or applies its own policy: buffer, coalesce, sample or drop. Sources that read from the outside world (sensors, sockets) can then choose what to lose, instead of losing whatever the hub happens to drop.

## Protocol sketch (additive)

```proto
// common.proto
message Credit {
  uint32 events = 1;  // number of additional data events allowed
  uint64 bytes  = 2;  // additional payload bytes allowed; 0 = no byte limit
}

// Capability negotiation: the plugin opts in through Initialize.
// <Type>.Initialize gets, at the same field number in all three types:
//   bool supports_credit = N;
// The runtime's message to each type gets a new oneof case:
//   common.Credit credit = M;
```

- **Backward compatible.** A plugin that doesn't set `supports_credit` keeps TCP-level backpressure, so old plugins and simple plugins in any language keep working. The hub sends `Credit` only to plugins that opted in.
- **Initial grant** goes right after `InitializeResponse`.
- **Reconnect** resets the balance to zero, and the hub sends a fresh initial grant after the new `InitializeResponse`. Credit is never carried across connections.

## Optional: the other direction (sink and transform input)

The same mechanism could work in reverse. A sink or transform grants the **hub** credits for events it's ready to accept (`Credit` in its message to the runtime). The hub then knows exactly how much a slow consumer can take, instead of filling its TCP buffers. Its queue for that node absorbs the rest, spilling to disk if enabled.

## Edge cases to think through

- **Cycles:** credit derived from downstream room can circle back on itself (A waits for credit from B's room, B waits on A). It's the same deadlock risk as `block`, with the same answer: nodes in a cycle use drop policies, and the hub always grants at least 1 credit to nodes in a strongly connected component, or excludes them.
- **Fan-out:** credit is limited by the slowest `block` target, just like head-of-line blocking today. The status endpoint should show which target is holding a producer back.
- **Starvation:** with many producers sharing the hub-wide budget, grants should be fair (e.g. round-robin or weighted by node priority) so one busy source can't take everything.
- **Grant size and frequency:** large grants reduce message overhead but weaken control; small ones do the opposite. Start with grants of half the available room, topped up when the balance drops below 25 %.
- **Misbehaving plugins:** a plugin that sends data without credit is a protocol violation. Drop the event, log a rate-limited WARN, and after repeated violations disconnect it (it reconnects and gets a fresh grant).
- **Transforms:** a transform's output uses credit, but its input is driven by the hub. A transform with no output credit should stop taking input. With the reverse direction, it simply stops granting the hub credit.

## Effort and dependencies

- **Hub:** credit accounting per producer, derived from queue room; the grant scheduler; violation handling.
- **Rust SDK:** a credit balance in `HubClient`; `LocalSink::emit` and transform output wait for credit; `supports_credit = true` set automatically.
- **Docs:** the README protocol section (an optional capability, how to implement it in other languages).
- **Tests:** a credit-aware fake hub; a mixed scene with credit-aware and TCP-only plugins; reconnect resets the balance; a violation leads to drop then disconnect; system messages flow while the balance is zero.
- **Builds on:** the director's inbound queues and the `Initialize` handshake (both in place).
