# Idea: event-time windows in the accumulator

Status: idea, not scheduled. The accumulator's windows are processing time only (decided for v1, Nov 2026).

## Problem

A window closes by the plugin's clock, so late or replayed events land in the window that's open when they arrive, not the one their own timestamp belongs to. Sensors that buffer offline and then flush, or a durable queue replaying after a restart, skew every window.

## Sketch

- A `time` setting: a dot path to a DATETIME (or epoch-ms integer) field; windows are assigned by that value.
- **Watermarks**: the window `[start, end)` closes once the watermark passes `end + allowed_lateness`; the watermark is the highest event time seen minus `max_out_of_order` (per key or global), and advances on an idle timer too, so a quiet stream still closes its windows.
- Late events after a close: `late: drop` (counted) or `late: update` (re-run `finish` and emit a correction, marked as such).
- Everything stays in the state store as today (timers keyed by event-time end), so exactly-once and restarts work the same.

## Open questions

- Watermark per key or per node (per node is simpler and the common default).
- How a correction is marked in the output (a field the out schema must declare?).
