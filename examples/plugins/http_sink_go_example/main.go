// Demo flwrs sink: sends an HTTP request for each event. Not for real use.
//
// Implements the plugin protocol (README.md, "Plugin protocol") with the
// protobuf messages generated from plugin/schema/ (see build.sh).
package main

import (
	"bufio"
	"encoding/binary"
	"flag"
	"fmt"
	"io"
	"net"
	"net/http"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"time"

	"flwrs.example/http-sink-go/pb"
	"google.golang.org/protobuf/encoding/protojson"
	"google.golang.org/protobuf/proto"
)

const (
	version            = "0.0.1" // must match manifest.json
	sinkMessage        = 3       // Msg-Code of SinkMessage (sink -> runtime)
	runtimeSinkMessage = 4       // Msg-Code of RuntimeSinkMessage (runtime -> sink)
)

// send writes one frame: u32 LE packet length, u32 LE header length, header, body.
func send(conn net.Conn, msg *pb.SinkMessage) error {
	header := fmt.Sprintf("Version: 1.0.0\r\nMsg-Code: %d\r\n", sinkMessage)
	body, err := proto.Marshal(msg)
	if err != nil {
		return err
	}
	frame := make([]byte, 8, 8+len(header)+len(body))
	binary.LittleEndian.PutUint32(frame[0:], uint32(4+len(header)+len(body)))
	binary.LittleEndian.PutUint32(frame[4:], uint32(len(header)))
	frame = append(append(frame, header...), body...)
	_, err = conn.Write(frame)
	return err
}

func receive(r *bufio.Reader) (*pb.RuntimeSinkMessage, error) {
	var lengths [4]byte
	if _, err := io.ReadFull(r, lengths[:]); err != nil {
		return nil, err
	}
	packet := make([]byte, binary.LittleEndian.Uint32(lengths[:]))
	if _, err := io.ReadFull(r, packet); err != nil {
		return nil, err
	}
	headerLen := binary.LittleEndian.Uint32(packet[:4])
	header := string(packet[4 : 4+headerLen])
	code := ""
	for _, line := range strings.Split(header, "\n") {
		if name, value, ok := strings.Cut(line, ":"); ok && strings.EqualFold(strings.TrimSpace(name), "Msg-Code") {
			code = strings.TrimSpace(value)
		}
	}
	if code != strconv.Itoa(runtimeSinkMessage) {
		return nil, fmt.Errorf("unexpected header %q", header)
	}
	msg := &pb.RuntimeSinkMessage{}
	return msg, proto.Unmarshal(packet[4+headerLen:], msg)
}

func handle(conn net.Conn, id string, event *pb.SinkEvent) error {
	fields := map[string]string{}
	for _, f := range event.GetPayload().GetFields() {
		fields[f.GetKey()] = f.GetValue().GetString_()
	}
	method := fields["method"]
	if method == "" {
		method = "POST"
	}
	client := http.Client{Timeout: 10 * time.Second}
	req, err := http.NewRequest(method, fields["url"], strings.NewReader(fields["body"]))
	if err == nil {
		var resp *http.Response
		if resp, err = client.Do(req); err == nil {
			resp.Body.Close()
			fmt.Printf("%s %s -> %d\n", method, fields["url"], resp.StatusCode)
			return nil
		}
	}
	// Report and keep going.
	return send(conn, &pb.SinkMessage{Payload: &pb.SinkMessage_Error{Error: &pb.ErrorEvent{
		PluginId:   id,
		PluginType: pb.PluginType_SINK,
		Message:    fmt.Sprintf("%s %s failed: %v", method, fields["url"], err),
	}}})
}

// session runs one connection. It returns true when the plugin should exit.
func session(addr, id string, initialize *pb.SinkMessage) (bool, error) {
	conn, err := net.Dial("tcp", addr)
	if err != nil {
		return false, err
	}
	defer conn.Close()
	if err := send(conn, initialize); err != nil { // always the first frame
		return false, err
	}
	r := bufio.NewReader(conn)
	for {
		msg, err := receive(r)
		if err != nil {
			return false, err
		}
		switch p := msg.Payload.(type) {
		case *pb.RuntimeSinkMessage_Initialize:
			fmt.Println("accepted by the hub")
		case *pb.RuntimeSinkMessage_Event:
			if err := handle(conn, id, p.Event); err != nil {
				return false, err
			}
		case *pb.RuntimeSinkMessage_Shutdown: // also how the hub rejects us
			bye := &pb.SinkMessage{Payload: &pb.SinkMessage_Exit{Exit: &pb.Exit{Code: pb.ExitCode_OK}}}
			return true, send(conn, bye)
		}
	}
}

func main() {
	id := flag.String("id", "", "instance id")
	host := flag.String("host", "", "hub host")
	port := flag.Int("port", 0, "hub port")
	flag.String("log-level", "info", "log level")
	token := flag.String("token", os.Getenv("FLWRS_TOKEN"), "token")
	flag.Parse()

	exe, _ := os.Executable()
	text, err := os.ReadFile(filepath.Join(filepath.Dir(exe), "..", "schema", "in.json"))
	if err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
	schema := &pb.SchemaDefinition{}
	if err := protojson.Unmarshal(text, schema); err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
	initialize := &pb.SinkMessage{Payload: &pb.SinkMessage_Initialize{Initialize: &pb.Initialize{
		PluginId: *id, PluginVersion: version, Schema: schema, Token: *token,
	}}}

	// Reconnect rule: if the connection drops, retry with the same Initialize.
	addr := net.JoinHostPort(*host, strconv.Itoa(*port))
	delay := time.Second
	for {
		done, err := session(addr, *id, initialize)
		if done {
			return
		}
		fmt.Fprintf(os.Stderr, "disconnected (%v); retrying in %v\n", err, delay)
		time.Sleep(delay)
		delay = min(delay*2, 30*time.Second)
	}
}
