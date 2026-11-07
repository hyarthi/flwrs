#!/usr/bin/env python3
"""Demo flwrs sink: sends an HTTP request for each event. Not for real use.

Implements the plugin protocol, version 2 (docs/protocol.md), with the
protobuf messages generated from plugin/schema/ (see build.sh). Offers the
ACKS capability with a window of one event: each event is ACKed after its
request finished, or NACKed (no retry) if it failed.
"""

import argparse
import os
import random
import socket
import struct
import sys
import threading
import time
import urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(HERE, "gen"))

from google.protobuf import json_format  # noqa: E402
from schema import protocol_pb2 as pb  # noqa: E402

VERSION = "0.0.1"  # must match manifest.json
FRAMING = 2  # framing version byte
MAX_PACKET = 256 * 1024 * 1024
MAX_HEADER = 4096
Code = pb.MsgCode
BODIES = {  # what a sink may receive, with the body type of each code
    Code.INITIALIZE_RESPONSE: pb.InitializeResponse,
    Code.SHUTDOWN: pb.Shutdown,
    Code.PING: pb.Ping,
    Code.PONG: pb.Pong,
    Code.EVENT: pb.Event,
}


class Violation(Exception):
    """A protocol violation: close the connection and reconnect."""


class Rejected(Exception):
    """Shutdown before InitializeResponse: stop, don't reconnect."""


def read_exact(sock, n):
    data = b""
    while len(data) < n:
        chunk = sock.recv(n - len(data))
        if not chunk:
            raise ConnectionError("connection closed")
        data += chunk
    return data


def receive(sock):
    """One frame: u32 LE packet_len, u8 version, u32 LE header_len, header, body."""
    (packet_len,) = struct.unpack("<I", read_exact(sock, 4))
    if not 5 <= packet_len <= MAX_PACKET:
        raise Violation(f"bad packet length {packet_len}")
    packet = read_exact(sock, packet_len)
    version, header_len = struct.unpack("<BI", packet[:5])
    if version != FRAMING or header_len > min(MAX_HEADER, packet_len - 5):
        raise Violation(f"bad frame (version {version}, header {header_len})")
    try:
        header = pb.FrameHeader.FromString(packet[5 : 5 + header_len])
        if header.flags or header.code not in BODIES:
            raise Violation(f"unexpected code {header.code} / flags {header.flags}")
        return header, BODIES[header.code].FromString(packet[5 + header_len :])
    except Violation:
        raise
    except Exception as e:  # header or body doesn't decode
        raise Violation(f"undecodable frame: {e}") from e


class Connection:
    """A socket whose writes may come from several threads."""

    def __init__(self, sock):
        self.sock, self.lock = sock, threading.Lock()

    def send(self, code, message):
        header = pb.FrameHeader(code=code).SerializeToString()
        body = message.SerializeToString()
        frame = struct.pack("<IBI", 5 + len(header) + len(body), FRAMING, len(header))
        with self.lock:
            self.sock.sendall(frame + header + body)


def handle(conn, seq, event):
    """Sends the request, then ACKs or NACKs the event. Runs on its own thread."""
    fields = {f.key: f.value.string for f in event.payload.fields}
    request = urllib.request.Request(
        fields.get("url", ""),
        data=fields.get("body", "").encode(),
        method=fields.get("method") or "POST",
    )
    try:
        try:
            with urllib.request.urlopen(request, timeout=10) as response:
                print(f"{request.method} {request.full_url} -> {response.status}", flush=True)
            conn.send(Code.ACK, pb.Ack(up_to=seq))
        except Exception as e:  # report, give up on this event, keep going
            reason = f"{request.method} {request.full_url} failed: {e}"
            conn.send(Code.ERROR, pb.ErrorEvent(message=reason))
            conn.send(Code.NACK, pb.Nack(seq=seq, reason=reason, retry=False))
    except OSError:
        pass  # the connection is gone; the hub redelivers the event


def session(args, initialize, accepted):
    """One connection. Returns when the hub sent Shutdown."""
    with socket.create_connection((args.host, args.port)) as sock:
        conn = Connection(sock)
        conn.send(Code.INITIALIZE, initialize)  # always the first frame
        last_seq, worker = 0, None
        while True:
            header, message = receive(sock)
            code = header.code
            if code == Code.SHUTDOWN:
                if not accepted.is_set():
                    raise Rejected(message.reason)
                if worker:  # finish (and ack) the event in hand
                    worker.join(timeout=10)
                conn.send(Code.EXIT, pb.Exit(code=pb.ExitCode.OK))
                return
            if not accepted.is_set():
                if code != Code.INITIALIZE_RESPONSE:
                    raise Violation(f"code {code} before InitializeResponse")
                accepted.set()
                print("accepted by the hub", flush=True)
            elif code == Code.PING:
                conn.send(Code.PONG, pb.Pong(nonce=message.nonce))
            elif code == Code.EVENT:
                if header.seq != last_seq + 1:
                    raise Violation(f"seq {header.seq} after {last_seq}")
                last_seq = header.seq
                worker = threading.Thread(target=handle, args=(conn, header.seq, message), daemon=True)
                worker.start()
            elif code == Code.INITIALIZE_RESPONSE:
                raise Violation("second InitializeResponse")
            # PONG: nothing to do (we don't send pings)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--id", required=True)
    parser.add_argument("--host", required=True)
    parser.add_argument("--port", type=int, required=True)
    parser.add_argument("--log-level", default="info")
    parser.add_argument("--token", default=os.environ.get("FLWRS_TOKEN", ""))
    args, _ = parser.parse_known_args()

    initialize = pb.Initialize(
        instance_id=args.id,
        plugin_version=VERSION,
        plugin_type=pb.PluginType.SINK,
        token=args.token,
        min_protocol_version=2,
        max_protocol_version=2,
        capabilities=[pb.Capability.ACKS],
        max_unacked=1,  # one event at a time
    )
    with open(os.path.join(HERE, "schema", "in.json")) as f:
        json_format.Parse(f.read(), initialize.in_schema)

    # Reconnect rule: if the connection drops, retry with the same Initialize,
    # backing off 1 s doubling to 30 s, with jitter.
    delay = 1
    while True:
        accepted = threading.Event()
        try:
            session(args, initialize, accepted)
            return
        except Rejected as e:
            print(f"rejected by the hub: {e}", file=sys.stderr, flush=True)
            sys.exit(1)
        except (Violation, ConnectionError, OSError) as e:
            if accepted.is_set():
                delay = 1  # the last connection worked: start over
            print(f"disconnected ({e}); retrying in {delay} s", file=sys.stderr, flush=True)
            time.sleep(min(delay * random.uniform(0.8, 1.2), 30))
            delay = min(delay * 2, 30)


if __name__ == "__main__":
    main()
