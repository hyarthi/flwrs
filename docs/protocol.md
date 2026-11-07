# flwrs plugin protocol, version 2

Status: **draft for review**. Feedback welcome, especially on the open questions at the end.

[flwrs](https://github.com/hyarthi/flwrs) runs workflows ("scenes") made of plugins: **sources** produce events, **transforms** change them, **sinks** consume them. Every plugin is a separate process, written in any language. It talks to the flwrs server (the **hub**) over one TCP connection, using the framing and messages described here.

This document is the contract. The Rust SDK (`flwrs-plugin`) is one implementation of it. A plugin in another language needs only this document, the two `.proto` files in [`plugin/schema/`](../plugin/schema) and a protobuf library.

Contents:

1. [Design goals](#1-design-goals)
2. [Terms](#2-terms)
3. [Processes and connections](#3-processes-and-connections)
4. [Framing](#4-framing)
5. [Messages](#5-messages)
6. [Connection lifecycle](#6-connection-lifecycle)
7. [Data events](#7-data-events)
8. [Flow control](#8-flow-control)
9. [Acknowledgements and delivery](#9-acknowledgements-and-delivery)
10. [Heartbeats](#10-heartbeats)
11. [Payloads and schemas](#11-payloads-and-schemas)
12. [Limits](#12-limits)
13. [Errors and protocol violations](#13-errors-and-protocol-violations)
14. [Security](#14-security)
15. [Versioning and compatibility](#15-versioning-and-compatibility)
16. [Writing a minimal plugin](#16-writing-a-minimal-plugin)
17. [Open questions](#17-open-questions)

The key words MUST, MUST NOT, SHOULD and MAY are used as in RFC 2119.

## 1. Design goals

- **Language-agnostic and small.** A minimal plugin in a new language is a few dozen lines on top of a protobuf library: plain TCP, a length-prefixed frame, a handful of messages. Everything beyond that is an optional capability.
- **Light enough for the edge.** The same runtime runs in a data centre and on a Raspberry Pi. No RPC framework, no HTTP/2: the useful parts of those (flow control, keepalive, negotiation) are built into the protocol directly.
- **The hub doesn't decode what it routes.** Routing metadata is in a small fixed header, so the hub can route, queue and forward event bodies as opaque bytes. It decodes a body only when an edge maps its fields.
- **Bounded everything.** Frame sizes, queues and in-flight events all have limits, and a slow consumer slows its producers down instead of filling memory.

Out of scope here: how scenes are configured, and how the hub queues events internally. Those are server features, described in the [README](../README.md).

## 2. Terms

| Term | Meaning |
|---|---|
| **Hub** | The flwrs server side that plugins connect to. |
| **Plugin** | An executable that speaks this protocol. Its **type** is SOURCE, TRANSFORM or SINK. |
| **Node** | One use of a plugin in a scene. Every connection belongs to exactly one node. |
| **Instance id** | How a plugin identifies its node: the node id (local plugins, passed as `--id`) or the node's `remote_id` (remote plugins). |
| **Producer / consumer** | A plugin that sends data events to the hub (sources, transforms) / receives them from the hub (sinks, transforms). A transform is both. |
| **Data event** | An `Event` frame: the only thing routed along a scene's edges. Everything else is a **system message** between one plugin and the hub. |
| **Frame** | One message on the wire: length, framing version, header, body. |

## 3. Processes and connections

A node runs its plugin in one of two ways.

**Local** (the default). The hub starts the plugin executable as a child process, with the plugin's install folder as its working directory, and these arguments, followed by the node's own launch arguments:

```
--id <instance id> --host <hub host> --port <hub port> --log-level <trace|debug|info|warn|error>
```

A fresh random **token** for this launch is in the environment variable `FLWRS_TOKEN`. It's in the environment rather than the arguments because arguments are visible to other users in `ps`. Standard input is closed. Standard output and standard error go to the server log, tagged with the scene and node.

**Remote.** An operator starts the plugin, possibly on another machine, with the same arguments pointing at the hub's reachable address, and authenticates it with the node's pre-shared token (`FLWRS_TOKEN`, or `--token`) or with a client certificate ([section 14](#14-security)).

In both cases the plugin opens **one TCP connection** to the hub and keeps it for its whole run. Messages flow in both directions on it. If the connection drops, the plugin reconnects ([section 6.5](#65-reconnecting)).

Stopping a local plugin: the hub sends `Shutdown` and waits for the process to exit (10 s by default). Then it asks the OS to stop the process and everything it started (SIGTERM to its process group on Unix, Ctrl-Break to its console group on Windows), waits again (2 s by default) and kills it. A plugin SHOULD exit on `Shutdown`, and SHOULD treat SIGTERM or Ctrl-Break as a last request to stop.

## 4. Framing

Every message is one frame. Integers are little-endian.

```
+------------+---------+------------+------------------------+----------------------+
| packet_len | version | header_len | header                 | body                 |
| u32 LE     | u8 = 2  | u32 LE     | FrameHeader (protobuf) | protobuf, per code   |
+------------+---------+------------+------------------------+----------------------+
             |<---------------------------- packet_len bytes -------------------------->|
```

- **`packet_len`** counts every byte after itself: `1 + 4 + header_len + body length`. It's at least 5.
- **`version`** is the framing version: the layout of this frame. It's `2` for everything in this document. A receiver that gets any other value MUST close the connection: it can't know where the frame ends. This byte lets the frame layout itself change in a later version. The **protocol** version (which messages exist, and the rules for them) is negotiated separately, in the handshake ([section 6.1](#61-handshake)).
- **`header_len`** is the length of the header, at most 4 KiB. It may be 0, since a header with every field at its default encodes to nothing, but a frame without a `code` is invalid.
- **`header`** is a `flwrs.protocol.FrameHeader`. Its `code` says which message the body holds.
- **`body`** is the protobuf encoding of the message type for `code`. It may be empty (a message with every field at its default).

Example: a `Ping` with nonce 1.

```
09 00 00 00   packet_len = 9
02            framing version 2
02 00 00 00   header_len = 2
08 0b         FrameHeader { code: PING (11) }
08 01         Ping { nonce: 1 }
```

### 4.1 Frame header

```proto
message FrameHeader {
  MsgCode.Enum code = 1;
  uint64 seq = 2;              // EVENT: sequence number (section 7.1)
  uint64 input_seq = 3;        // EVENT from a transform: its input's seq (section 7.3)
  EventMetadata metadata = 4;  // EVENT from the hub: routing metadata (section 7.2)
  uint32 attempt = 5;          // EVENT from the hub: delivery attempt (section 9.3)
  uint32 flags = 6;            // reserved, must be 0
}
```

Only `code` is used by every frame. The other fields are for `EVENT` frames, and a receiver ignores them on other codes. `flags` is reserved for features negotiated in a later version (batching, compression). A sender MUST leave it 0, and a receiver MUST treat any bit it doesn't know as a protocol violation, since it can't decode the body correctly without knowing what the bit means. Field numbers 7 and 8 are planned for clustering (slot id and ownership epoch).

### 4.2 Message codes

Each code names exactly one body type. The table also says who may send it.

| Code | Name | Body | Sender | Notes |
|---|---|---|---|---|
| 1 | `INITIALIZE` | `Initialize` | plugin | First frame on every connection. |
| 2 | `INITIALIZE_RESPONSE` | `InitializeResponse` | hub | The plugin is accepted. |
| 3 | `EVENT` | `Event` | both | Producers send, consumers receive (section 7). |
| 4 | `LOG` | `LogEvent` | plugin | |
| 5 | `ERROR` | `ErrorEvent` | plugin | |
| 6 | `SHUTDOWN` | `Shutdown` | hub | |
| 7 | `EXIT` | `Exit` | plugin | |
| 8 | `ACK` | `Ack` | both | Only with capability `ACKS`. |
| 9 | `NACK` | `Nack` | both | Only with capability `ACKS`. |
| 10 | `CREDIT` | `Credit` | hub | Only with capability `CREDITS`, to producers. |
| 11 | `PING` | `Ping` | both | |
| 12 | `PONG` | `Pong` | both | |

Code `0` (`UNDEFINED`) is never valid. What each plugin type may send and receive after the handshake:

| | Source | Transform | Sink |
|---|---|---|---|
| plugin → hub | `EVENT`, `LOG`, `ERROR`, `EXIT`, `PING`, `PONG` | all of the source's, plus `ACK`, `NACK` | `LOG`, `ERROR`, `EXIT`, `PING`, `PONG`, `ACK`, `NACK` |
| hub → plugin | `SHUTDOWN`, `PING`, `PONG`, `CREDIT`, `ACK`, `NACK` | all of the source's, plus `EVENT` | `SHUTDOWN`, `PING`, `PONG`, `EVENT` |

`ACK` and `NACK` go from consumers to the hub (for events the hub delivered), and from the hub to producers (for events they sent). A code outside its row is a protocol violation ([section 13](#13-errors-and-protocol-violations)).

## 5. Messages

The definitions are in [`plugin/schema/protocol.proto`](../plugin/schema/protocol.proto) (package `flwrs.protocol`) and [`plugin/schema/payload.proto`](../plugin/schema/payload.proto) (package `flwrs.payload`). Generate code with `protoc` or `buf`. In summary:

| Message | Fields |
|---|---|
| `Initialize` | `instance_id`, `plugin_version`, `plugin_type`, `token`, `in_schema`, `out_schema`, `min_protocol_version`, `max_protocol_version`, `capabilities`, `max_unacked` |
| `InitializeResponse` | `protocol_version`, `capabilities`, `heartbeat_interval_ms`, `max_unacked` |
| `Event` | `payload` (a `PluginPayload`) |
| `Ack` | `up_to`, `seqs` |
| `Nack` | `seq`, `reason`, `retry` |
| `Credit` | `events` |
| `Ping`, `Pong` | `nonce` |
| `LogEvent` | `level`, `message`, `details` |
| `ErrorEvent` | `message`, `details` |
| `Shutdown` | `reason` |
| `Exit` | `code` (`OK`, `INTERRUPTED`, `ERROR`), `message` |

Every enum's zero value is `UNDEFINED`, so a field that was never set can't be mistaken for a real value. Enums are wrapped in a message (`MsgCode.Enum`, `PluginType.Enum`, …) to keep their value names scoped.

## 6. Connection lifecycle

```
plugin                                   hub
  | --- INITIALIZE ------------------------> |  first frame, within 10 s
  | <-- INITIALIZE_RESPONSE ---------------- |  accepted (or SHUTDOWN: rejected)
  | <-- CREDIT (with CREDITS) -------------- |
  | <== EVENT / ACK / NACK / LOG / ... ====> |  running
  | <-- PING / PONG -----------------------> |  heartbeats, either way
  | <-- SHUTDOWN --------------------------- |
  | --- EXIT ------------------------------> |
  close
```

### 6.1 Handshake

The plugin's first frame MUST be `INITIALIZE`, within 10 seconds of connecting and at most 1 MiB in size. Otherwise the hub closes the connection without a reply.

`Initialize` carries:

- `instance_id`, `plugin_version`, `plugin_type` and `token` (empty when the node uses a client certificate instead, [section 14](#14-security));
- the payload schemas: `in_schema` for sinks and transforms, `out_schema` for sources and transforms ([section 11](#11-payloads-and-schemas));
- the protocol versions the plugin speaks, `min_protocol_version` ..= `max_protocol_version`. Version 2 is the one described here;
- the optional `capabilities` it supports (`ACKS`, `CREDITS`), and, for a consumer with `ACKS`, its `max_unacked` window.

The hub then checks, in order:

1. **Identity.** A node with this instance id is expected, and the credentials match: the token (compared in constant time), or the client certificate. Otherwise the reply is `Shutdown` with a short `reason`, and the attempt is logged (rate-limited). A plugin left over from an earlier server run gets the same answer, since its token is no longer valid.
2. **Protocol version.** The hub picks the highest version both sides speak. If there's none, the reply is `Shutdown` ("no common protocol version: hub speaks 2..2").
3. **Declaration.** `plugin_type` and `plugin_version` equal the installed plugin's manifest, and the declared schemas are well formed and structurally equal to the manifest's ([section 11.4](#114-schema-rules)). Otherwise the reply is `Shutdown` with every difference in `reason`. The hub doesn't relaunch a local plugin rejected this way: the same executable would fail the same way.

If all pass, the hub answers `InitializeResponse` with the protocol version in use, the capabilities enabled (a subset of those offered), its heartbeat interval and, with `ACKS`, the consumer window it will use. If the node already had a connection, the new one replaces it and the old one is closed ("newest connection wins"). This recovers from half-open connections.

### 6.2 Before and after acceptance

Between `INITIALIZE` and `INITIALIZE_RESPONSE`, a plugin MAY send `LOG`, `ERROR` and `EXIT`, but MUST NOT send `EVENT`, `ACK`, `NACK`, `PING` or `PONG`. The hub sends nothing but `INITIALIZE_RESPONSE` or `SHUTDOWN` until then, and anything else from it in that time is a protocol violation.

A plugin MUST honour `SHUTDOWN` at any time. When it comes **before** `INITIALIZE_RESPONSE`, the hub has rejected the plugin. The plugin MUST then stop and MUST NOT reconnect: retrying with the same token or declaration would fail again. It MAY send `EXIT` (code `ERROR`), and SHOULD log the `reason` and exit with a non-zero status, so an operator can tell a rejection from a normal stop.

### 6.3 Running

After `INITIALIZE_RESPONSE`, data flows ([sections 7 to 9](#7-data-events)), and either side may send `PING` ([section 10](#10-heartbeats)). The plugin reports logs with `LOG` and failures with `ERROR`. The hub writes both to the server log, tagged with the scene and node. An `ErrorEvent` is only logged: the plugin keeps running and isn't restarted. A record with level `UNDEFINED` is logged as a warning.

### 6.4 Shutdown

The hub sends `SHUTDOWN` to end a run. After that it sends no more events. The plugin SHOULD finish what it has in hand: a transform sends its remaining outputs, and a consumer with `ACKS` acks what it has finished. It SHOULD take no more than a few seconds for that: the hub waits 10 s by default (configurable per node) before it stops the process. Unacked events are delivered again later anyway, so it's better to stop than to overrun. Then it sends `EXIT` with a code (`OK`, `INTERRUPTED` or `ERROR`) and exits. The hub keeps reading the connection until the plugin closes it or its grace period runs out, and events that arrive meanwhile are still routed.

An `EXIT` the hub didn't ask for means the plugin is stopping on its own. The hub relaunches a local plugin after a delay, and waits for a remote one to reconnect.

### 6.5 Reconnecting

If the connection drops without a `SHUTDOWN`, the plugin keeps running and reconnects:

- with backoff, for example 1 s doubling to at most **30 s**, with some jitter. Jitter MUST NOT take a delay past 30 s. The hub relies on that maximum: it gives a local plugin 60 s by default to come back before it treats it as unhealthy and relaunches it;
- re-sending the **same** `Initialize` (same token);
- and waiting for `INITIALIZE_RESPONSE` again before sending data.

Everything tied to the old connection ends with it. Sequence numbers start again at 1, credit is 0 until the hub grants more, and the hub redelivers the events the plugin hadn't acked ([section 9](#9-acknowledgements-and-delivery)). A transform MUST NOT send outputs that refer to an input received on an earlier connection.

## 7. Data events

An `EVENT` frame carries an `Event { payload }` body. Its routing information is in the frame header.

### 7.1 Sequence numbers

Every `EVENT` frame has a `seq` in its header: the sender numbers its own events on each connection 1, 2, 3, … in the order it sends them, with no gaps. The two directions are numbered independently. `ACK`, `NACK` and `input_seq` refer to events by `seq`, and a receiver MUST treat a `seq` that isn't the previous one plus 1 as a protocol violation. Numbering starts at 1 again on every new connection.

### 7.2 Metadata

The hub owns each event's `EventMetadata`, and puts it in the header of every event it delivers:

| Field | Meaning |
|---|---|
| `event_id` | Unique per handover (a ULID). Kept on redelivery, so consumers can detect duplicates. |
| `trace_id` | Shared by every event derived from the same original source event. |
| `hops` | Handovers since the original source event. The hub drops events over its hop limit (32 by default, configurable per scene), which stops runaway loops in cyclic scenes. |
| `origin_node_id` | The node that produced the trace's original event. |
| `create_time` | When the trace's original event entered the hub. |

Producers never set `metadata`, and the hub ignores it on events it receives. A source's events start new traces. A transform's outputs continue the trace of their input (section 7.3).

### 7.3 Transforms: correlating outputs with inputs

A transform's outputs may or may not follow its inputs one to one:

- **Pipes** turn each input into outputs right away: none (a filter), one, or several (a split).
- **Stateful transforms** (accumulators, windows, joins) take inputs into their state, and emit when their own criteria are met. That can be on an input ("every 100th event"), or on anything else: a timer, an external signal.

Every output may name the input it belongs to in `header.input_seq`. The hub then gives the output that input's trace, origin, creation time and hop count, so the trace continues across the transform without the plugin copying anything. A pipe names the input it's answering. An accumulator names whichever input it considers the cause, for example the one that completed a window. `input_seq = 0` means the output isn't tied to an input (a timer, say), and it starts a new trace.

The hub remembers the metadata of every input it delivered that hasn't been acked, and of the most recent 1024 inputs it delivered, acked or not. An output may refer to any of those. If it refers to an input the hub no longer remembers, or to a `seq` from an earlier connection, it starts a new trace. That's not an error.

With `ACKS`, the transform's `ACK` of an input means "done with this input": its outputs are sent, or it's been taken into the transform's state. Two rules follow:

- **Outputs before the ACK.** Whatever outputs a transform sends for an input before acking it are routed before the hub treats the input as done, so a crash in between can't lose them.
- **Ack when you absorb.** A stateful transform MUST ack an input once it has taken it into its state, not hold the ack until it emits. Otherwise the consumer window (section 8.3) fills with inputs waiting for an output that's waiting for more inputs, and both sides wait forever. Keeping that state across a crash is up to the plugin (for example, a checkpoint file). After an ack the hub won't deliver the input again.

The ack also gives the hub the transform's processing time per event.

## 8. Flow control

The hub bounds the events in flight at every step. A producer that's faster than its consumers is slowed down, not buffered without limit. There are two levels, and plugins choose with capabilities.

### 8.1 Baseline: TCP backpressure

Without `CREDITS`, flow control rides on TCP. When a producer's events can't move on (a target queue is full, with the `block` policy), the hub stops reading that plugin's socket. The plugin's TCP send buffer fills and its next write blocks. **Sends may block. That's flow control, not an error.** This needs no code in the plugin, but while the hub isn't reading, the plugin's logs, errors and `EXIT` wait behind its data.

On the receiving side, a consumer without `ACKS` gets events as fast as TCP lets the hub write them. A consumer that's slow simply reads slowly.

### 8.2 Credits (capability `CREDITS`)

With `CREDITS`, a producer may send an `EVENT` only while it holds credit:

- The balance starts at 0 on every connection. Right after `INITIALIZE_RESPONSE`, the hub grants an initial window (64 events by default).
- Each `EVENT` uses one credit. `Credit { events }` adds to the balance. Grants are additive and are never acknowledged.
- The hub returns credit as it finishes routing events: once an event is in every target's queue, or a target with a drop policy has dropped it. So credit comes back at the pace of the slowest `block` target, which is the same pace TCP backpressure would set. The difference is that the hub keeps reading the connection, so system messages always flow.
- An `EVENT` sent without credit is a protocol violation.

A producer out of credit waits, or applies its own policy: buffer, coalesce, sample or drop. Sources that read from the outside world (sensors, sockets) can then choose what to lose, instead of losing whatever the hub would drop.

### 8.3 Consumer window (capability `ACKS`)

A consumer with `ACKS` declares `max_unacked` in `Initialize`: the most events it's willing to have received but not yet acked or nacked. The hub caps it (1024 by default; 0 means the hub's default of 64) and reports the value in effect in `InitializeResponse`. It never has more than that many events outstanding to the plugin. A consumer that processes one event at a time can declare 1. One that pipelines (an HTTP sink with concurrent requests, a batching ML transform) declares more.

## 9. Acknowledgements and delivery

### 9.1 Capability `ACKS`

`ACKS` is in use only if the hub lists it in `InitializeResponse.capabilities`: a plugin that offered it but doesn't see it there MUST NOT send `ACK` or `NACK`, and the hub won't either. The same goes for `CREDITS`, and for the consumer window: the one in `InitializeResponse.max_unacked` applies, which may be smaller than the plugin asked for.

With `ACKS`, both directions acknowledge data events by `seq`:

- **Consumer → hub.** A sink acks an event when it's done with it, in whatever sense its destination needs: sent, written, committed. A transform acks an input once all its outputs are sent (section 7.3). Every event received MUST eventually be acked or nacked.
- **Hub → producer.** The hub acks an event once it's routed: placed in every target's queue (committed to disk where the target's queue is durable, see 9.4), or legitimately not forwarded (a mapping filtered it out, or the node has no out-edges). It nacks an event:
  - with `retry = true` when a target's queue dropped it (full, a block timeout, the memory budget) or a durable queue couldn't store it. Sending it again may succeed, but targets that did take it would then get it twice;
  - with `retry = false` when it's too large, over the hop limit, or no out-edge's mapping could map it.

  A producer that needs to know its event is safe before letting go of it (for example, a source acknowledging a message broker) waits for this answer.

`Ack { up_to, seqs }` acknowledges every `seq` up to and including `up_to` (0 = none) that wasn't NACKed already, plus each one listed in `seqs`. A consumer that handles events in order can send just `up_to`. Acks may come out of order. Acking a `seq` twice, or one already covered, is harmless. Acking a `seq` that was never sent is ignored. The hub acks producers' events by `seqs`, one at a time, because its answers can finish out of order: an event for a durable queue is answered only once it's committed.

`Nack { seq, reason, retry }` rejects one event. From a consumer, `retry = true` asks the hub to deliver it again later, and `retry = false` gives up on it. From the hub, `retry` says whether sending the same event again might succeed.

### 9.2 Without `ACKS`

A plugin that doesn't offer `ACKS` sends no acks and gets none, and delivery on its hops is **at-most-once**. An event the hub has written to the plugin's connection counts as delivered, and is lost if the plugin crashes before handling it. That's fine for many plugins, and it keeps the minimal plugin minimal.

### 9.3 Redelivery

For a consumer with `ACKS`, the hub keeps each delivered event until the consumer acks it. It delivers the event again, with the same `event_id` and `header.attempt` increased by one:

- after the connection drops, or the plugin crashes or restarts, with every unacked event, in their original order;
- after a `Nack` with `retry = true`, following a delay (1 s by default).

After a configurable number of attempts (5 by default), the event is dead-lettered: dropped, logged and counted. A `Nack` with `retry = false` dead-letters it at once. This bounds the damage of a "poison" event that crashes its consumer every time.

Delivery with `ACKS` is therefore **at-least-once**, and a consumer MAY see an event more than once. `attempt > 1` means the event may have been delivered before, and `event_id` lets a consumer recognise it. Exactly-once isn't a protocol guarantee: consumers that need it deduplicate on `event_id`, or write idempotently.

### 9.4 What the hub guarantees in between

The hub queues events for each target node. By default these queues are in memory, so at-least-once holds while the hub runs: a plugin crash, a reconnect or a NACK doesn't lose events, but a hub restart does. A scene (or a node) can make its queues **durable**: events are then committed to a local file before the producer gets its ack, kept until the consumer acks them, and delivered again after a hub restart. Queue policies (`block`, `drop_newest`, `drop_oldest`) still apply when a queue is full. A drop policy trades the guarantee for not slowing producers down, and its drops are counted. See the README for the queue settings.

## 10. Heartbeats

Either side MAY send `PING { nonce }`. The receiver MUST answer promptly with `PONG` carrying the same nonce. Answering is mandatory for every plugin, while sending pings is optional.

The hub sends a `PING` at least every `heartbeat_interval_ms` (10 s by default; announced in `InitializeResponse`, 0 = none). **Any** frame received counts as a sign of life, not just `PONG`. If a side has received nothing for three intervals while it was waiting to read, it MAY consider the connection dead and close it. The plugin then reconnects (section 6.5), and the hub treats it as a lost connection. Time a side spends *not* reading, because it's applying backpressure itself, doesn't count. Otherwise TCP backpressure would look like a dead peer.

Heartbeats matter most for remote plugins, where a network partition can leave a connection half-open for a long time.

## 11. Payloads and schemas

### 11.1 Payloads

An event's payload is a list of named, typed fields:

```proto
message PluginPayload { repeated Field fields = 1; }
message Field { string key = 1; FieldValue value = 2; }
```

`FieldValue` is a `oneof` with one variant per type. A `FieldValue` with no variant set means **null**, which is different from an empty string or zero.

| Type | Value | Notes |
|---|---|---|
| `STRING` | `string` | UTF-8. |
| `I32`, `I64`, `U32`, `U64` | `i32`, `i64`, `u32`, `u64` | |
| `F32`, `F64` | `f32`, `f64` | |
| `BOOL` | `bool` | |
| `DATETIME` | `date_time` | `google.protobuf.Timestamp`. |
| `BYTES` | `bytes` | The schema may name a `content_type` (section 11.3). |
| `ARRAY` | `array` | Elements share one type (`nested_type_definition`). |
| `MAP` | `map` | String keys; values share one type (`nested_type_definition`). |
| `OBJECT` | `object` | Named fields with their own types (`object_fields`). |
| `PACKED_F32`, `PACKED_F64`, `PACKED_I32`, `PACKED_I64`, `PACKED_U32`, `PACKED_U64` | `packed_f32`, … | Packed numeric arrays (section 11.2). |

### 11.2 Packed numeric arrays

An `ARRAY` of `F32` encodes every element as its own `FieldValue` message: several bytes of overhead and an allocation per element. For numeric vectors such as embeddings, the packed types send one protobuf packed repeated field instead: 4 bytes per `f32`, decoded in one go. A packed field has no nested type definition. Mappings can convert between a packed array and an `ARRAY` of the same element type.

### 11.3 Content types

A `BYTES` field definition MAY name a `content_type`: a media type such as `image/jpeg` or `application/json`. It tells consumers how to read the bytes, and it's part of the field's contract: two schemas that differ only in a field's content type don't match.

### 11.4 Schema rules

A plugin declares the shape of what it accepts and emits as a `SchemaDefinition`: a list of `FieldDefinition`s (`key`, `type`, `nested_type_definition` for `ARRAY` and `MAP`, `object_fields` for `OBJECT`, `content_type`, `description`). The same schemas are in the plugin's package (as proto3 JSON files), and the hub compares what the plugin declares in `Initialize` against them.

**Well formed:**
- keys are non-empty and unique within their level;
- no field has type `UNDEFINED`;
- `ARRAY` and `MAP` have a `nested_type_definition`, and other types don't;
- `OBJECT` has `object_fields` (checked recursively), and other types don't;
- `content_type` is set only on `BYTES`;
- nesting is at most 32 levels deep.

**Matching** (declared against the manifest): structural equality. Both have the same set of keys at every level, with the same type, nested definition, object fields and content type for each key. Field order and descriptions are ignored.

**Values** in a payload SHOULD follow the declared schema. The hub checks payloads only where an edge maps them: it converts losslessly to the target's schema where it can, and counts and logs a mapping error where it can't. Undeclared fields are dropped on mapped edges, and passed through unchanged on unmapped ones.

## 12. Limits

Every implementation enforces the same limits, so a frame accepted by one side is never too large for the other:

| Limit | Value |
|---|---|
| Frame (`packet_len`) | at most 256 MiB |
| Header (`header_len`) | at most 4 KiB |
| First frame (`Initialize`) | at most 1 MiB, within 10 s of connecting |
| Event body, as accepted by the hub | 16 MiB by default (configurable, and lower per node). Larger events are skipped without being buffered, and NACKed (`retry = false`) to producers with `ACKS`. |
| Nesting depth of payload values and schemas | at most 32 levels |

Protobuf itself allows messages up to 2 GiB, and most libraries limit decoding to 100 levels of nesting, so these limits are below every library default. Keep events small: results, labels, detections, small crops. For anything over about 1 MiB, pass a reference (a key in an object store, a file name in a shared folder) instead of the data.

## 13. Errors and protocol violations

- **Frame errors**: an unknown framing version, `packet_len` out of range, `header_len` over 4 KiB or past the end of the packet, a header that doesn't decode. The receiver closes the connection, since the stream position can't be trusted.
- **Protocol violations**: a second `INITIALIZE` or `INITIALIZE_RESPONSE` on a connection, an unknown or `UNDEFINED` code, a code the sender may not send or the receiver may not receive ([section 4.2](#42-message-codes); for example `ACK` to a sink, or `EVENT` from one), a body that doesn't decode as its code's type, data before acceptance, a `seq` out of order, an `EVENT` without credit, unknown `flags` bits. The receiver logs it and closes the connection. A plugin then reconnects (section 6.5). An exception: the hub doesn't decode event bodies it only forwards, so it notices a broken event body only where an edge maps it, and counts it as a mapping error.
- **Failures inside the plugin** aren't protocol errors. Report them with `ERROR` (logged), or with a `NACK` for the event concerned.

## 14. Security

**Local plugins** connect to the hub's plain listener, which binds to loopback by default. They authenticate with their per-launch token. A new token is generated for every launch, so a process left over from an earlier launch can't take a node's place.

**Remote plugins** authenticate in one of two ways, configured per scene with per-node overrides:

- **Pre-shared token** (`token`): the node's secret, given to the plugin in `FLWRS_TOKEN` (or `--token`) and sent in `Initialize`. Over the plain listener the token travels unencrypted, so a node can require TLS, which is the default once the hub has a TLS listener.
- **Client certificate** (`mtls`): the plugin connects to the hub's **TLS listener** with a client certificate, and leaves `token` empty. The certificate belongs to the node if it chains to the hub's configured client CA and its subject alternative name (DNS or URI) or common name equals the node's `remote_id`, or if its SHA-256 fingerprint equals the one pinned in the node's configuration (for self-signed certificates).

The TLS listener is a separate port. Plugins connect to it with TLS 1.2 or 1.3, then speak exactly the same framing inside. The plugin verifies the hub's certificate against a CA it's given.

SDKs SHOULD take their TLS settings from these environment variables, so an operator can enable TLS for any plugin the same way (the Rust SDK does):

| Variable | Meaning |
|---|---|
| `FLWRS_TLS_CA` | PEM file with the CA certificate(s) the hub's certificate must chain to. Setting it turns TLS on. |
| `FLWRS_TLS_SERVER_NAME` | The name to check the hub's certificate against (default: the `--host`). |
| `FLWRS_TLS_CERT`, `FLWRS_TLS_KEY` | PEM files of the plugin's client certificate (chain) and private key, for `mtls` nodes. |

Either way, the hub checks `instance_id` together with the credentials: several nodes may share a `remote_id` (for example, two copies of an imported scene), and the credentials decide which node a connection belongs to.

## 15. Versioning and compatibility

- **Framing version** (the byte in every frame): changes only if the frame layout itself changes.
- **Protocol version** (negotiated in `Initialize`): the set of messages and rules. This document is version 2. Version 1 was an unreleased draft with text headers, and isn't supported. A hub keeps accepting older protocol versions after it adds newer ones, and a plugin offers the range it speaks.
- **Capabilities**: optional features within a version. A new capability needs no new version, since peers that don't know it simply don't enable it.
- **The `.proto` files evolve additively**: fields, messages, enum values and message codes are only ever added. Field numbers are never reused or changed, and removed fields are marked `reserved`. A new message code always comes with a new capability or protocol version, so a peer never receives a code it doesn't know.
- **Unknown fields** in a known message are ignored, as protobuf does by default.

## 16. Writing a minimal plugin

What every plugin MUST do:

1. Read `--id`, `--host`, `--port`, `--log-level` (and its own arguments) and the token from `FLWRS_TOKEN` (or `--token`).
2. Connect, and send `INITIALIZE` with `min_protocol_version = max_protocol_version = 2` and no capabilities.
3. Read frames. On `INITIALIZE_RESPONSE`, start working. On `SHUTDOWN`, send `EXIT` and stop (and don't reconnect if it came before `INITIALIZE_RESPONSE`). On `PING`, answer `PONG`.
4. Producers: send `EVENT` frames with `seq` 1, 2, 3, …. Transforms also set `input_seq`.
5. Consumers: handle each `EVENT`.
6. If the connection drops, reconnect with backoff (at most 30 s) and start again at step 2.

That's a few dozen lines in most languages: see the demo plugins in [`examples/plugins/`](../examples/plugins) (Python, Go, TypeScript, Java). Add `ACKS` for at-least-once delivery, and `CREDITS` to control your own backpressure.

## 17. Open questions

Feedback is especially welcome on these:

1. **Credit units.** Credits count events only. Should they also count bytes (`Credit.bytes`), for producers whose event sizes vary a lot?
2. **Acks from the hub to producers** mean "routed". Is that the right point, or should there be an option for "delivered to (and acked by) every consumer", end to end?
3. **Batching and compression** are reserved in `flags` but not specified. Is per-frame compression worth it for the expected event sizes, or is batching (several events per frame) the better first step?
4. **Heartbeat defaults**: a 10 s interval with three missed intervals, about 30 s to notice a dead link. Too slow for some deployments, too chatty for others?
5. **How far back a transform's output can refer**: the hub remembers every unacked input and the last 1024 delivered. Should that window be negotiable, for accumulators with long windows? Or should an output be able to name several inputs (span links in tracing terms)?
