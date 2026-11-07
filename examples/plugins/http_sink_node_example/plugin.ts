#!/usr/bin/env node
// Demo flwrs sink: sends an HTTP request for each event. Not for real use.
//
// Implements plugin protocol version 2 (docs/protocol.md) with the ACKS
// capability and a window of one event. protobufjs loads the messages from the
// .proto files at run time (plugin/schema/; a copy in proto/ in the package),
// so there's no code generation.

import { existsSync, readFileSync } from "node:fs";
import { connect, type Socket } from "node:net";
import { dirname, join } from "node:path";
import { parseArgs } from "node:util";
import protobuf from "protobufjs";

const VERSION = "0.0.1"; // must match manifest.json
const FRAMING_VERSION = 2;
const PROTOCOL_VERSION = 2;
const MAX_PACKET = 256 * 1024 * 1024;
const MAX_HEADER = 4 * 1024;

const here = dirname(new URL(import.meta.url).pathname);
const protoRoot = existsSync(join(here, "proto")) ? join(here, "proto") : join(here, "../../../plugin");
const root = new protobuf.Root();
root.resolvePath = (_origin, target) => join(protoRoot, target); // google/protobuf/* are built in
root.loadSync(["schema/protocol.proto"], { keepCase: true });
const type = (name: string) => root.lookupType(`flwrs.protocol.${name}`);
const FrameHeader = type("FrameHeader");
const Code = root.lookupEnum("flwrs.protocol.MsgCode.Enum").values;
const ACKS = root.lookupEnum("flwrs.protocol.Capability.Enum").values.ACKS;

// Body type per code: what we send, and what a sink may receive.
const sendTypes: Record<number, protobuf.Type> = {
  [Code.INITIALIZE]: type("Initialize"),
  [Code.ERROR]: type("ErrorEvent"),
  [Code.EXIT]: type("Exit"),
  [Code.ACK]: type("Ack"),
  [Code.NACK]: type("Nack"),
  [Code.PONG]: type("Pong"),
};
const receiveTypes: Record<number, protobuf.Type> = {
  [Code.INITIALIZE_RESPONSE]: type("InitializeResponse"),
  [Code.SHUTDOWN]: type("Shutdown"),
  [Code.PING]: type("Ping"),
  [Code.PONG]: type("Pong"),
  [Code.EVENT]: type("Event"),
};

const { values: args } = parseArgs({
  strict: false,
  options: {
    id: { type: "string" },
    host: { type: "string" },
    port: { type: "string" },
    "log-level": { type: "string" },
    token: { type: "string" },
  },
});
// The same Initialize on every connection.
const initialize = {
  instance_id: String(args.id),
  plugin_version: VERSION,
  plugin_type: "SINK",
  token: String(args.token ?? process.env.FLWRS_TOKEN ?? ""),
  in_schema: JSON.parse(readFileSync(join(here, "schema", "in.json"), "utf8")),
  min_protocol_version: PROTOCOL_VERSION,
  max_protocol_version: PROTOCOL_VERSION,
  capabilities: ["ACKS"],
  max_unacked: 1, // one request at a time
};

/** One frame: u32 LE packet_len, u8 framing version, u32 LE header_len, FrameHeader, body. */
function send(socket: Socket, code: number, message: object): void {
  if (socket.destroyed) return; // the connection is gone; the hub redelivers
  const header = FrameHeader.encode(FrameHeader.fromObject({ code })).finish();
  const body = sendTypes[code].encode(sendTypes[code].fromObject(message)).finish();
  const prefix = Buffer.alloc(9);
  prefix.writeUInt32LE(5 + header.length + body.length, 0);
  prefix.writeUInt8(FRAMING_VERSION, 4);
  prefix.writeUInt32LE(header.length, 5);
  socket.write(Buffer.concat([prefix, header, body]));
}

type Payload = { fields?: { key: string; value?: { string?: string } }[] };

/** Sends the request for one event; throws on failure. */
async function request(payload: Payload): Promise<void> {
  const fields = Object.fromEntries((payload.fields ?? []).map((f) => [f.key, f.value?.string ?? ""]));
  const method = fields.method || "POST";
  try {
    const response = await fetch(fields.url, { method, body: fields.body, signal: AbortSignal.timeout(10_000) });
    console.log(`${method} ${fields.url} -> ${response.status}`);
  } catch (e) {
    throw new Error(`${method} ${fields.url} failed: ${e instanceof Error && e.cause ? e.cause : e}`);
  }
}

/** One connection. Resolves true when the plugin should exit. */
function session(onAccepted: () => void): Promise<boolean> {
  return new Promise((resolve) => {
    let buffered = Buffer.alloc(0);
    let accepted = false;
    let acks = false; // ACKS enabled by the hub
    let lastSeq = 0n;
    let queue = Promise.resolve(); // events are handled one at a time, in order
    let done = false;
    const socket = connect(Number(args.port), String(args.host), () => send(socket, Code.INITIALIZE, initialize));
    const violation = (why: string): void => void socket.destroy(new Error(`protocol violation: ${why}`));

    function handle(code: number, header: protobuf.Message, body: Record<string, any>): void {
      if (code === Code.INITIALIZE_RESPONSE && !accepted) {
        accepted = true;
        acks = (body.capabilities ?? []).includes(ACKS);
        console.log(`accepted by the hub (protocol ${body.protocol_version}, acks ${acks})`);
        onAccepted();
      } else if (code === Code.SHUTDOWN) {
        // Before acceptance, this is a rejection: stop, don't reconnect.
        console.log(`shutdown: ${body.reason || "(no reason)"}`);
        done = true;
        queue.then(() => {
          send(socket, Code.EXIT, { code: "OK" });
          socket.end(() => socket.destroy()); // close once EXIT is flushed
        });
      } else if (!accepted) {
        violation(`code ${code} before INITIALIZE_RESPONSE`);
      } else if (code === Code.PING) {
        send(socket, Code.PONG, { nonce: body.nonce });
      } else if (code === Code.EVENT) {
        const seq = BigInt(String((header as any).seq ?? 0)); // uint64: a Long or a number
        if (seq !== lastSeq + 1n) return violation(`event seq ${seq} after ${lastSeq}`);
        lastSeq = seq;
        queue = queue.then(() =>
          request(body.payload ?? {}).then(
            () => {
              if (acks) send(socket, Code.ACK, { up_to: seq.toString() });
            },
            (e: Error) => {
              console.error(e.message);
              send(socket, Code.ERROR, { message: e.message });
              if (acks) send(socket, Code.NACK, { seq: seq.toString(), reason: e.message, retry: false });
            },
          ),
        );
      } else if (code !== Code.PONG) {
        violation(`unexpected code ${code}`); // e.g. a second INITIALIZE_RESPONSE
      }
    }

    socket.on("data", (chunk: Buffer) => {
      buffered = Buffer.concat([buffered, chunk]);
      while (!socket.destroyed && buffered.length >= 4) {
        const len = buffered.readUInt32LE(0);
        if (len < 5 || len > MAX_PACKET) return violation(`packet_len ${len}`);
        if (buffered.length < 4 + len) return; // wait for the rest
        const packet = buffered.subarray(4, 4 + len);
        buffered = buffered.subarray(4 + len);
        if (packet[0] !== FRAMING_VERSION) return violation(`framing version ${packet[0]}`);
        const headerLen = packet.readUInt32LE(1);
        if (headerLen > MAX_HEADER || 5 + headerLen > len) return violation(`header_len ${headerLen}`);
        try {
          const header = FrameHeader.decode(packet.subarray(5, 5 + headerLen));
          const { code, flags } = header as unknown as { code: number; flags: number };
          if (flags) return violation(`flags ${flags}`);
          const bodyType = receiveTypes[code];
          if (!bodyType) return violation(`code ${code} not allowed for a sink`);
          const body = bodyType.toObject(bodyType.decode(packet.subarray(5 + headerLen)), { longs: String });
          handle(code, header, body);
        } catch (e) {
          return violation(`undecodable frame: ${e}`);
        }
      }
    });
    socket.on("error", (e) => console.error(`connection error: ${e.message}`));
    socket.on("close", () => resolve(done));
  });
}

// Reconnect rule: if the connection drops, retry with the same Initialize,
// 1 s doubling to 30 s, with jitter; back to 1 s once accepted again.
let delay = 1_000;
while (!(await session(() => (delay = 1_000)))) {
  const wait = Math.min(delay * (0.8 + Math.random() * 0.4), 30_000);
  console.error(`disconnected; retrying in ${(wait / 1000).toFixed(1)} s`);
  await new Promise((r) => setTimeout(r, wait));
  delay = Math.min(delay * 2, 30_000);
}
