#!/usr/bin/env python3
"""Demo flwrs sink: sends an HTTP request for each event. Not for real use.

Implements the plugin protocol (README.md, "Plugin protocol") with the
protobuf messages generated from plugin/schema/ (see build.sh).
"""

import argparse
import os
import socket
import struct
import sys
import time
import urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(HERE, "gen"))

from google.protobuf import json_format  # noqa: E402
from schema import common_pb2, schema_pb2, sink_pb2  # noqa: E402

VERSION = "0.0.1"  # must match manifest.json
SINK_MESSAGE = 3  # Msg-Code of SinkMessage (sink -> runtime)
RUNTIME_SINK_MESSAGE = 4  # Msg-Code of RuntimeSinkMessage (runtime -> sink)


def send(sock, message):
    """One frame: u32 LE packet length, u32 LE header length, header, body."""
    header = f"Version: 1.0.0\r\nMsg-Code: {SINK_MESSAGE}\r\n".encode("ascii")
    body = message.SerializeToString()
    sock.sendall(struct.pack("<II", 4 + len(header) + len(body), len(header)) + header + body)


def read_exact(sock, n):
    data = b""
    while len(data) < n:
        chunk = sock.recv(n - len(data))
        if not chunk:
            raise ConnectionError("connection closed")
        data += chunk
    return data


def receive(sock):
    (packet_len,) = struct.unpack("<I", read_exact(sock, 4))
    packet = read_exact(sock, packet_len)
    (header_len,) = struct.unpack("<I", packet[:4])
    header = packet[4 : 4 + header_len].decode("ascii")
    names = dict(line.split(":", 1) for line in header.splitlines() if line)
    names = {k.strip().lower(): v.strip() for k, v in names.items()}
    if names.get("msg-code") != str(RUNTIME_SINK_MESSAGE):
        raise ConnectionError(f"unexpected header: {header!r}")
    message = sink_pb2.RuntimeSinkMessage()
    message.ParseFromString(packet[4 + header_len :])
    return message


def handle(sock, args, event):
    fields = {f.key: f.value.string for f in event.payload.fields}
    request = urllib.request.Request(
        fields.get("url", ""),
        data=fields.get("body", "").encode(),
        method=fields.get("method") or "POST",
    )
    try:
        with urllib.request.urlopen(request, timeout=10) as response:
            print(f"{request.method} {request.full_url} -> {response.status}", flush=True)
    except Exception as e:  # report and keep going
        error = sink_pb2.SinkMessage()
        error.error.plugin_id = args.id
        error.error.plugin_type = common_pb2.PluginType.SINK
        error.error.message = f"{request.method} {request.full_url} failed: {e}"
        send(sock, error)


def session(args, initialize):
    """One connection. Returns True when the plugin should exit."""
    with socket.create_connection((args.host, args.port)) as sock:
        send(sock, initialize)  # always the first frame
        while True:
            message = receive(sock)
            kind = message.WhichOneof("payload")
            if kind == "initialize":
                print("accepted by the hub", flush=True)
            elif kind == "event":
                handle(sock, args, message.event)
            elif kind == "shutdown":  # also how the hub rejects us
                bye = sink_pb2.SinkMessage()
                bye.exit.code = common_pb2.ExitCode.OK
                send(sock, bye)
                return True


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--id", required=True)
    parser.add_argument("--host", required=True)
    parser.add_argument("--port", type=int, required=True)
    parser.add_argument("--log-level", default="info")
    parser.add_argument("--token", default=os.environ.get("FLWRS_TOKEN", ""))
    args, _ = parser.parse_known_args()

    initialize = sink_pb2.SinkMessage()
    initialize.initialize.plugin_id = args.id
    initialize.initialize.plugin_version = VERSION
    initialize.initialize.token = args.token
    with open(os.path.join(HERE, "schema", "in.json")) as f:
        json_format.Parse(f.read(), initialize.initialize.schema)

    # Reconnect rule: if the connection drops, retry with the same Initialize.
    delay = 1
    while True:
        try:
            if session(args, initialize):
                return
        except (ConnectionError, OSError) as e:
            print(f"disconnected ({e}); retrying in {delay} s", file=sys.stderr, flush=True)
            time.sleep(delay)
            delay = min(delay * 2, 30)


if __name__ == "__main__":
    main()
