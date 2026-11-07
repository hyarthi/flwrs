// Demo flwrs sink: sends an HTTP request for each event. Not for real use.
//
// Implements the plugin protocol, version 2 (docs/protocol.md) with the
// protobuf messages generated from plugin/schema/ (by the Maven build).
// Offers the ACKS capability with a window of one event.
package example;

import com.google.protobuf.Message;
import com.google.protobuf.util.JsonFormat;
import flwrs.payload.Payload;
import flwrs.protocol.Protocol.*;
import java.io.DataInputStream;
import java.io.IOException;
import java.io.OutputStream;
import java.net.Socket;
import java.net.URI;
import java.net.http.HttpClient;
import java.net.http.HttpRequest;
import java.net.http.HttpResponse;
import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.nio.file.Files;
import java.nio.file.Path;
import java.time.Duration;
import java.util.HashMap;
import java.util.Map;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.concurrent.TimeUnit;

public final class HttpSink {
    static final String VERSION = "0.0.1"; // must match manifest.json
    static final int FRAMING_VERSION = 2;

    static long delay = 1_000; // reconnect backoff: 1 s doubling to 30 s, reset once accepted

    static final HttpClient HTTP = HttpClient.newBuilder().connectTimeout(Duration.ofSeconds(10)).build();

    /** A received frame: its header and raw body. */
    record Frame(FrameHeader header, byte[] body) {}

    /** One connection: the socket's output is shared by the reader and the HTTP worker. */
    static final class Connection {
        final OutputStream out;
        Connection(OutputStream out) { this.out = out; }

        /** u32 LE packet_len | u8 version | u32 LE header_len | FrameHeader | body. */
        synchronized void send(MsgCode.Enum code, Message body) throws IOException {
            byte[] header = FrameHeader.newBuilder().setCode(code).build().toByteArray();
            byte[] bytes = body.toByteArray();
            ByteBuffer frame = ByteBuffer.allocate(4 + 1 + 4 + header.length + bytes.length).order(ByteOrder.LITTLE_ENDIAN);
            frame.putInt(1 + 4 + header.length + bytes.length).put((byte) FRAMING_VERSION)
                    .putInt(header.length).put(header).put(bytes);
            out.write(frame.array());
            out.flush();
        }
    }

    /** Reads one frame; any frame error is an IOException (close and reconnect). */
    static Frame receive(DataInputStream in) throws IOException {
        int packetLen = Integer.reverseBytes(in.readInt()); // EOFException when closed
        if (packetLen < 5 || packetLen > 256 << 20) throw new IOException("bad packet_len " + packetLen);
        byte[] packet = in.readNBytes(packetLen);
        if (packet.length < packetLen) throw new IOException("connection closed");
        ByteBuffer buffer = ByteBuffer.wrap(packet).order(ByteOrder.LITTLE_ENDIAN);
        if (buffer.get() != FRAMING_VERSION) throw new IOException("unknown framing version");
        int headerLen = buffer.getInt();
        if (headerLen < 0 || headerLen > 4096 || headerLen > packetLen - 5) throw new IOException("bad header_len");
        FrameHeader header = FrameHeader.parseFrom(ByteBuffer.wrap(packet, 5, headerLen));
        if (header.getFlags() != 0) throw new IOException("unknown flags " + header.getFlags());
        byte[] body = new byte[packetLen - 5 - headerLen];
        System.arraycopy(packet, 5 + headerLen, body, 0, body.length);
        return new Frame(header, body);
    }

    /** Sends the request for one event, then ACKs it, or reports an ERROR and NACKs it. */
    static void handle(Connection conn, long seq, Event event) {
        Map<String, String> fields = new HashMap<>();
        for (Payload.Field f : event.getPayload().getFieldsList()) fields.put(f.getKey(), f.getValue().getString());
        String method = fields.getOrDefault("method", "").isEmpty() ? "POST" : fields.get("method");
        String url = fields.getOrDefault("url", "");
        try {
            try {
                HttpRequest request = HttpRequest.newBuilder(URI.create(url))
                        .timeout(Duration.ofSeconds(10))
                        .method(method, HttpRequest.BodyPublishers.ofString(fields.getOrDefault("body", "")))
                        .build();
                HttpResponse<Void> response = HTTP.send(request, HttpResponse.BodyHandlers.discarding());
                System.out.println(method + " " + url + " -> " + response.statusCode());
                conn.send(MsgCode.Enum.ACK, Ack.newBuilder().setUpTo(seq).build());
            } catch (IOException | InterruptedException | RuntimeException e) { // report and keep going
                String reason = method + " " + url + " failed: " + e;
                conn.send(MsgCode.Enum.ERROR, ErrorEvent.newBuilder().setMessage(reason).build());
                conn.send(MsgCode.Enum.NACK, Nack.newBuilder().setSeq(seq).setReason(reason).setRetry(false).build());
            }
        } catch (IOException e) { // the connection is gone; the hub redelivers the event
            System.err.println("could not answer event " + seq + ": " + e.getMessage());
        }
    }

    /** One connection. Returns true when the plugin should exit. */
    static boolean session(String host, int port, Initialize initialize) throws IOException, InterruptedException {
        // One HTTP request at a time (max_unacked = 1), off the reader so PINGs are answered meanwhile.
        ExecutorService worker = Executors.newSingleThreadExecutor(r -> { Thread t = new Thread(r); t.setDaemon(true); return t; });
        try (Socket socket = new Socket(host, port)) {
            Connection conn = new Connection(socket.getOutputStream());
            DataInputStream in = new DataInputStream(socket.getInputStream());
            conn.send(MsgCode.Enum.INITIALIZE, initialize); // always the first frame
            boolean accepted = false;
            long lastSeq = 0; // events are numbered 1, 2, 3, … per connection
            while (true) {
                Frame frame = receive(in);
                MsgCode.Enum code = frame.header().getCode();
                if (!accepted && code != MsgCode.Enum.INITIALIZE_RESPONSE && code != MsgCode.Enum.SHUTDOWN)
                    throw new IOException("protocol violation: " + code + " before INITIALIZE_RESPONSE");
                switch (code) {
                    case INITIALIZE_RESPONSE -> {
                        if (accepted) throw new IOException("protocol violation: second INITIALIZE_RESPONSE");
                        accepted = true;
                        delay = 1_000;
                        System.out.println("accepted by the hub: " + InitializeResponse.parseFrom(frame.body()));
                    }
                    case EVENT -> {
                        long seq = frame.header().getSeq();
                        if (seq != lastSeq + 1) throw new IOException("protocol violation: seq " + seq + " after " + lastSeq);
                        lastSeq = seq;
                        Event event = Event.parseFrom(frame.body());
                        worker.execute(() -> handle(conn, seq, event));
                    }
                    case PING -> conn.send(MsgCode.Enum.PONG,
                            Pong.newBuilder().setNonce(Ping.parseFrom(frame.body()).getNonce()).build());
                    case PONG -> {} // we never ping, but a PONG is allowed
                    case SHUTDOWN -> { // ends the run, or rejects us (before INITIALIZE_RESPONSE)
                        System.out.println("shutdown: " + Shutdown.parseFrom(frame.body()).getReason());
                        worker.shutdown(); // finish (and ack) the request in hand
                        worker.awaitTermination(15, TimeUnit.SECONDS);
                        conn.send(MsgCode.Enum.EXIT, Exit.newBuilder().setCode(ExitCode.Enum.OK).build());
                        return true;
                    }
                    default -> throw new IOException("protocol violation: unexpected code " + code);
                }
            }
        } finally {
            worker.shutdownNow();
        }
    }

    public static void main(String[] argv) throws Exception {
        Map<String, String> args = new HashMap<>();
        for (int i = 0; i + 1 < argv.length; i++) {
            if (argv[i].startsWith("--")) args.put(argv[i].substring(2), argv[++i]);
        }
        String token = args.getOrDefault("token", System.getenv().getOrDefault("FLWRS_TOKEN", ""));

        Payload.SchemaDefinition.Builder schema = Payload.SchemaDefinition.newBuilder();
        JsonFormat.parser().merge(Files.readString(Path.of("schema", "in.json")), schema); // cwd = install folder
        Initialize initialize = Initialize.newBuilder()
                .setInstanceId(args.get("id")).setPluginVersion(VERSION).setPluginType(PluginType.Enum.SINK)
                .setToken(token).setInSchema(schema)
                .setMinProtocolVersion(2).setMaxProtocolVersion(2)
                .addCapabilities(Capability.Enum.ACKS).setMaxUnacked(1)
                .build();

        // Reconnect rule: if the connection drops, retry with the same Initialize.
        while (true) {
            try {
                if (session(args.get("host"), Integer.parseInt(args.get("port")), initialize)) return;
            } catch (IOException e) {
                System.err.println("disconnected (" + e.getMessage() + "); retrying in " + delay / 1000 + " s");
            }
            Thread.sleep(delay + (long) (Math.random() * 250)); // a little jitter
            delay = Math.min(delay * 2, 30_000);
        }
    }
}
