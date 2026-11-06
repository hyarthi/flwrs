#!/usr/bin/env node
// Demo flwrs sink: sends an HTTP request for each event. Not for real use.
//
// Implements the plugin protocol (README.md, "Plugin protocol"). protobufjs
// loads the messages from the .proto files at run time (plugin/schema/; a
// copy in proto/ in the package), so there's no code generation.

import { existsSync, readFileSync } from "node:fs";
import { connect, type Socket } from "node:net";
import { dirname, join } from "node:path";
import { parseArgs } from "node:util";
import protobuf from "protobufjs";

const VERSION = "0.0.1"; // must match manifest.json
const SINK_MESSAGE = 3; // Msg-Code of SinkMessage (sink -> runtime)
const RUNTIME_SINK_MESSAGE = 4; // Msg-Code of RuntimeSinkMessage (runtime -> sink)

const here = dirname(new URL(import.meta.url).pathname);
const protoRoot = existsSync(join(here, "proto")) ? join(here, "proto") : join(here, "../../../plugin");
const root = new protobuf.Root();
root.resolvePath = (_origin, target) => join(protoRoot, target);
root.loadSync(["schema/sink.proto"], { keepCase: true });
const SinkMessage = root.lookupType("schema.sink.SinkMessage");
const RuntimeSinkMessage = root.lookupType("schema.sink.RuntimeSinkMessage");

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
const id = String(args.id);
const schema = JSON.parse(readFileSync(join(here, "schema", "in.json"), "utf8"));
const initialize = {
  initialize: {
    plugin_id: id,
    plugin_version: VERSION,
    token: String(args.token ?? process.env.FLWRS_TOKEN ?? ""),
    schema,
  },
};

/** One frame: u32 LE packet length, u32 LE header length, header, body. */
function send(socket: Socket, message: object): void {
  const header = Buffer.from(`Version: 1.0.0\r\nMsg-Code: ${SINK_MESSAGE}\r\n`, "ascii");
  const body = SinkMessage.encode(SinkMessage.fromObject(message)).finish();
  const lengths = Buffer.alloc(8);
  lengths.writeUInt32LE(4 + header.length + body.length, 0);
  lengths.writeUInt32LE(header.length, 4);
  socket.write(Buffer.concat([lengths, header, body]));
}

type Fields = { fields?: { key: string; value?: { string?: string } }[] };

async function handle(socket: Socket, payload: Fields): Promise<void> {
  const fields = Object.fromEntries((payload.fields ?? []).map((f) => [f.key, f.value?.string ?? ""]));
  const method = fields.method || "POST";
  try {
    const response = await fetch(fields.url, { method, body: fields.body, signal: AbortSignal.timeout(10_000) });
    console.log(`${method} ${fields.url} -> ${response.status}`);
  } catch (e) {
    // Report and keep going.
    send(socket, {
      error: { plugin_id: id, plugin_type: "SINK", message: `${method} ${fields.url} failed: ${e}` },
    });
  }
}

/** One connection. Resolves true when the plugin should exit. */
function session(): Promise<boolean> {
  return new Promise((resolve) => {
    let buffered = Buffer.alloc(0);
    let queue = Promise.resolve(); // events are handled one at a time, in order
    let done = false;
    const socket = connect(Number(args.port), String(args.host), () => send(socket, initialize));
    socket.on("data", (chunk: Buffer) => {
      buffered = Buffer.concat([buffered, chunk]);
      while (buffered.length >= 4 && buffered.length >= 4 + buffered.readUInt32LE(0)) {
        const packet = buffered.subarray(4, 4 + buffered.readUInt32LE(0));
        buffered = buffered.subarray(4 + packet.length);
        const headerLen = packet.readUInt32LE(0);
        const header = packet.subarray(4, 4 + headerLen).toString("ascii");
        if (!new RegExp(`^msg-code:\\s*${RUNTIME_SINK_MESSAGE}\\s*$`, "im").test(header)) {
          socket.destroy(new Error(`unexpected header ${JSON.stringify(header)}`));
          return;
        }
        const message = RuntimeSinkMessage.toObject(RuntimeSinkMessage.decode(packet.subarray(4 + headerLen)), {
          oneofs: true,
        });
        if (message.payload === "initialize") {
          console.log("accepted by the hub");
        } else if (message.payload === "event") {
          socket.pause(); // backpressure: don't read more while a request runs
          queue = queue.then(() => handle(socket, message.event.payload ?? {})).then(() => void socket.resume());
        } else if (message.payload === "shutdown") {
          // Also how the hub rejects us.
          done = true;
          queue.then(() => {
            send(socket, { exit: { code: "OK" } });
            socket.end();
          });
        }
      }
    });
    socket.on("error", (e) => console.error(`connection error: ${e.message}`));
    socket.on("close", () => resolve(done));
  });
}

// Reconnect rule: if the connection drops, retry with the same Initialize.
let delay = 1_000;
while (!(await session())) {
  console.error(`disconnected; retrying in ${delay / 1000} s`);
  await new Promise((r) => setTimeout(r, delay));
  delay = Math.min(delay * 2, 30_000);
}
