// Demo flwrs sink: sends an HTTP request for each event. Not for real use.
//
// Implements the plugin protocol (README.md, "Plugin protocol") with the
// protobuf messages generated from plugin/schema/ (by the Maven build).
package example;

import com.google.protobuf.util.JsonFormat;
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
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.time.Duration;
import java.util.HashMap;
import java.util.Map;
import schema.common.Common;
import schema.schema.Schema;
import schema.sink.Sink;

public final class HttpSink {
    static final String VERSION = "0.0.1"; // must match manifest.json
    static final int SINK_MESSAGE = 3; // Msg-Code of SinkMessage (sink -> runtime)
    static final int RUNTIME_SINK_MESSAGE = 4; // Msg-Code of RuntimeSinkMessage (runtime -> sink)

    static final HttpClient HTTP = HttpClient.newBuilder().connectTimeout(Duration.ofSeconds(10)).build();

    /** One frame: u32 LE packet length, u32 LE header length, header, body. */
    static void send(OutputStream out, Sink.SinkMessage message) throws IOException {
        byte[] header = ("Version: 1.0.0\r\nMsg-Code: " + SINK_MESSAGE + "\r\n").getBytes(StandardCharsets.US_ASCII);
        byte[] body = message.toByteArray();
        ByteBuffer frame = ByteBuffer.allocate(8 + header.length + body.length).order(ByteOrder.LITTLE_ENDIAN);
        frame.putInt(4 + header.length + body.length).putInt(header.length).put(header).put(body);
        out.write(frame.array());
        out.flush();
    }

    static Sink.RuntimeSinkMessage receive(DataInputStream in) throws IOException {
        byte[] lengths = in.readNBytes(4);
        if (lengths.length < 4) throw new IOException("connection closed");
        byte[] packet = in.readNBytes(ByteBuffer.wrap(lengths).order(ByteOrder.LITTLE_ENDIAN).getInt());
        ByteBuffer buffer = ByteBuffer.wrap(packet).order(ByteOrder.LITTLE_ENDIAN);
        int headerLen = buffer.getInt();
        String header = new String(packet, 4, headerLen, StandardCharsets.US_ASCII);
        boolean ours = header.lines().anyMatch(line -> {
            String[] kv = line.split(":", 2);
            return kv.length == 2 && kv[0].trim().equalsIgnoreCase("Msg-Code")
                    && kv[1].trim().equals(String.valueOf(RUNTIME_SINK_MESSAGE));
        });
        if (!ours) throw new IOException("unexpected header " + header);
        return Sink.RuntimeSinkMessage.parseFrom(ByteBuffer.wrap(packet, 4 + headerLen, packet.length - 4 - headerLen));
    }

    static void handle(OutputStream out, String id, Sink.SinkEvent event) throws IOException {
        Map<String, String> fields = new HashMap<>();
        for (Schema.Field f : event.getPayload().getFieldsList()) fields.put(f.getKey(), f.getValue().getString());
        String method = fields.getOrDefault("method", "").isEmpty() ? "POST" : fields.get("method");
        String url = fields.getOrDefault("url", "");
        try {
            HttpRequest request = HttpRequest.newBuilder(URI.create(url))
                    .timeout(Duration.ofSeconds(10))
                    .method(method, HttpRequest.BodyPublishers.ofString(fields.getOrDefault("body", "")))
                    .build();
            HttpResponse<Void> response = HTTP.send(request, HttpResponse.BodyHandlers.discarding());
            System.out.println(method + " " + url + " -> " + response.statusCode());
        } catch (Exception e) { // report and keep going
            send(out, Sink.SinkMessage.newBuilder().setError(Common.ErrorEvent.newBuilder()
                    .setPluginId(id)
                    .setPluginType(Common.PluginType.Enum.SINK)
                    .setMessage(method + " " + url + " failed: " + e)).build());
        }
    }

    /** One connection. Returns true when the plugin should exit. */
    static boolean session(String host, int port, String id, Sink.SinkMessage initialize) throws IOException {
        try (Socket socket = new Socket(host, port)) {
            OutputStream out = socket.getOutputStream();
            DataInputStream in = new DataInputStream(socket.getInputStream());
            send(out, initialize); // always the first frame
            while (true) {
                Sink.RuntimeSinkMessage message = receive(in);
                switch (message.getPayloadCase()) {
                    case INITIALIZE -> System.out.println("accepted by the hub");
                    case EVENT -> handle(out, id, message.getEvent());
                    case SHUTDOWN -> { // also how the hub rejects us
                        send(out, Sink.SinkMessage.newBuilder()
                                .setExit(Common.Exit.newBuilder().setCode(Common.ExitCode.Enum.OK)).build());
                        return true;
                    }
                    default -> {}
                }
            }
        }
    }

    public static void main(String[] argv) throws Exception {
        Map<String, String> args = new HashMap<>();
        for (int i = 0; i + 1 < argv.length; i++) {
            if (argv[i].startsWith("--")) args.put(argv[i].substring(2), argv[++i]);
        }
        String id = args.get("id");
        String token = args.getOrDefault("token", System.getenv().getOrDefault("FLWRS_TOKEN", ""));

        Schema.SchemaDefinition.Builder schema = Schema.SchemaDefinition.newBuilder();
        JsonFormat.parser().merge(Files.readString(Path.of("schema", "in.json")), schema); // cwd = install folder
        Sink.SinkMessage initialize = Sink.SinkMessage.newBuilder().setInitialize(Sink.Initialize.newBuilder()
                .setPluginId(id).setPluginVersion(VERSION).setToken(token).setSchema(schema)).build();

        // Reconnect rule: if the connection drops, retry with the same Initialize.
        long delay = 1_000;
        while (true) {
            try {
                if (session(args.get("host"), Integer.parseInt(args.get("port")), id, initialize)) return;
            } catch (IOException e) {
                System.err.println("disconnected (" + e.getMessage() + "); retrying in " + delay / 1000 + " s");
            }
            Thread.sleep(delay);
            delay = Math.min(delay * 2, 30_000);
        }
    }
}
