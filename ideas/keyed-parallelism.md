# Idea: keyed parallelism in the script transform and accumulator

Status: idea, not scheduled. Both transforms handle one event at a time (the SDK runner's default), ~12–15k events/s per node on an M2.

## Problem

One node is one core's worth of Lua. Scaling out today means splitting the stream by hand into several nodes with filters on edges.

## Sketch

- A `parallelism: N` setting: N Lua runtimes (each its own state, limits and chunks) in the plugin; events are routed to one by a hash of the `key` path, so events of one key stay in order and on one runtime.
- The state store and the outbox already work per input; commits from N workers need a single writer (a commit queue) or one SQLite file per worker (simpler: `state.<n>.db`, the key hash decides).
- The runner needs `max_concurrent > 1` with acks still per input (the sink runner already does this; the transform runner would need it).

## Open questions

- Changing N later reshuffles keys between workers: migrate state on start, or refuse a change while state exists.
- Whether the hub-side alternative (several nodes behind a keyed fan-out edge) is the better general mechanism.
