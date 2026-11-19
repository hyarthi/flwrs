# Idea: adaptive batching for durable and spill queues

Status: idea, not scheduled. An optimisation to try when durable-queue latency matters; durable queues trade speed for guarantees, so it isn't pressing.

## Problem

The durable queue's writer (`app/src/modules/director/queue/durable.rs`, `BATCH_WINDOW`) waits a fixed 5 ms whenever fewer events are pending than one batch (`write_batch`), so a burst can fill the batch before the commit. The spill queue does the same for events going to disk (`spill.rs`).

Under load the batch fills and nothing waits. At light load, though, a lone event waits the whole window on every durable hop: about 6 ms, because tokio's timer rounds up to the next millisecond. Then comes the commit's fsync. A pipeline of three nodes with durable queues picks up ~18 ms it doesn't need. (Measured context: a plugin round trip over loopback is 0.15–0.25 ms at the median; see the plugins' *Footprint* sections.)

## Why not just drop the window

Committing at once whenever the writer is idle (plain group commit) removes the wait, but at moderate rates it turns into many small commits: one fsync and WAL append each. That's more write amplification on the storage the queue is meant to protect. On SD cards and cheap flash at the edge, it means faster wear. The window exists to amortise that, and it does so even when latency would allow less.

## Sketch: time the window from the traffic

Keep batching, but size the wait from what the queue has seen instead of a constant:

- **Track the arrival rate** per queue: an exponentially weighted moving average of inter-arrival times. Maybe also the recent commit duration (fsync cost), which differs a lot between NVMe and an SD card.
- **Predict whether waiting pays off.** If the next event is likely within a short time (high rate), wait about as long as it takes to fill the batch, capped. If arrivals are sparse (the expected gap is longer than the cap), commit at once: waiting would add latency without saving a commit.
- **Bound it by the storage's cost.** Never commit more often than a minimum interval derived from the measured commit time (e.g. no more than one commit per `k × commit_duration`). Slow storage then batches more and fast storage less, and the fsync rate has an upper bound whatever the traffic does.
- **Optional: settings per node** (`runtime.queue.batch`: `max_wait_ms`, a `min_commit_interval_ms` or a commits-per-second budget). Make it adaptive by default, with the current fixed 5 ms as a mode for predictability.

A naive model is probably enough: the goal is "don't wait when nothing's coming, don't commit too often when a lot is", not optimal batching.

## Things to measure first

- Latency per durable hop at 10 / 100 / 1 000 / 10 000 events/s, fixed window vs adaptive.
- Commits (fsyncs) per second and bytes written per event, on an SSD and on an SD card.
- Behaviour around the rate where the model switches (no flapping: hysteresis on the EWMA).

## Related

- The `trellis bench` open loop now paces with a thread and reports the harness's own lateness apart, so latency per hop at low rates can be measured cleanly.
- The spill queue's window affects only events going to disk (memory over its soft limit): same idea, lower priority.
