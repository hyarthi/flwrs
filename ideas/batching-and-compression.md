# Idea: batching and compression on the wire

Status: idea, not scheduled. **Compression is contentious:** it's unclear whether it helps (smaller messages) or hurts (CPU and time to compress and decompress) for flwrs' typical traffic. Measure before building. This is open question 3 in `docs/protocol.md`. `FrameHeader.flags` is reserved for both: today a sender must leave it 0, and an unknown bit is a protocol violation, so either feature must be negotiated first.

## Batching

**Problem.** Every event is its own frame, written and flushed on its own: with `nodelay` sockets that's about one syscall and one TCP segment per event. At high rates, per-event overheads dominate: the frame header, syscalls, and the hub's per-frame work. (Benchmarks: the hub-side calibration peaks at ~85–170k events/s, and plugins at 30–110k events/s.)

**Step 1, no protocol change: write coalescing.** A connection's writer drains every frame that's ready and writes them together (one vectored write, or a buffer flushed when the outgoing queue is empty: "corking"). Frames stay individual on the wire; the receiver doesn't notice. This alone may capture most of the gain, in the SDK and in the hub. Try it first.

**Step 2, if needed: batch frames.** Several events in one frame, negotiated (a capability, plus a `flags` bit on the frame). Each event keeps what it has in the header today: `seq`, `input_seq`, metadata, `attempt`. So a batch body would be a list of (per-event header, body) pairs, or a contiguous `seq` range with the per-event fields repeated. Things to settle:
- Acks stay per event (by `seq`), so delivery semantics don't change.
- Credit and windows count events inside batches, as today.
- A batch's size counts against the frame limit, so batches have to be cut at a byte size.
- Latency: the sender must never wait to fill a batch. Batch only what's already queued (the same rule as write coalescing; see `adaptive-queue-batching.md` for the storage-side version of this trade-off).

## Compression

**Where it may help:**
- Remote plugins over slow or metered links (edge to data centre, cellular).
- Large, repetitive text payloads (JSON, logs, CSV).
- The hub's queues: bodies are opaque to the hub (it decodes them only for an edge with a mapping), so it could keep them compressed in memory, in spool files and in durable queues. That means less memory and disk per queued event, and fewer bytes written to flash at the edge.

**Where it likely hurts:**
- Local plugins on loopback: there's no bandwidth to save, only CPU spent on both sides.
- Small events (under a few hundred bytes): general-purpose compressors gain little without a shared dictionary, and the framing overhead stays.
- Payloads that are already compressed or dense (images, audio, protobuf binary, packed numeric arrays).
- Constrained edge CPUs (a Raspberry Pi), where compression time may cost more latency than the bytes saved.

**Shape, if adopted:**
- **Negotiated per connection:** the plugin lists the algorithms it supports in `Initialize`, and the hub picks one (or none) in `InitializeResponse`. Candidates: `lz4` (fast, modest ratio; `lz4_flex` is pure Rust), `zstd` (better ratio, tunable level, dictionaries; the common crate builds C code, while pure-Rust encoders are limited).
- **Per frame, not per connection:** a `flags` bit says "this body is compressed", so a sender can skip small or incompressible bodies (a size threshold, or skip when an attempt didn't shrink the body).
- **A node setting** (`runtime.wire.compression`: `off` | `auto` | an algorithm). `off` for local plugins by default; `auto` might be a sensible default for remote ones.
- **The hub keeps compressed bodies as they are:** it stores and forwards them, and decompresses only for a mapping edge or a consumer that didn't negotiate the same algorithm. Then compression costs the hub almost nothing on the common path, and the queues benefit for free.
- Size limits (`max_event_bytes`, queue byte bounds) need a decision: compressed or uncompressed size. The uncompressed size protects the consumer, which must decompress the body; the compressed size reflects memory and disk. Probably both: a cap on the decompressed size against decompression bombs.

## Measure first

- **Payload corpus:** typical events (small sensor readings, JSON documents of 1–100 KiB, binary blobs).
- **Compression:** ratio and CPU per MiB for lz4 and zstd (levels 1–3), on an M-series / x86 machine and on a Raspberry Pi.
- **End to end with `trellis bench`:** throughput, latency and CPU with and without compression, on loopback and over a shaped link (e.g. 10 Mbit/s, 50 ms RTT, using `tc netem` on Linux).
- **Write coalescing alone:** throughput and CPU, before anything else, to see how much of batching's gain needs no protocol change.

Decide per use case from those numbers. It's quite possible that the answer is "coalescing yes, batch frames no, compression only for remote nodes and queue storage".
