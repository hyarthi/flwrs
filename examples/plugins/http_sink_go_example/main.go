// Demo flwrs sink: sends an HTTP request for each event. Not for real use.
//
// Implements the plugin protocol, version 2 (docs/protocol.md), with the
// protobuf messages generated from plugin/schema/ (see build.sh). Offers the
// ACKS capability with a window of one event.
package main

import (
	"bufio"
	"encoding/binary"
	"errors"
	"flag"
	"fmt"
	"io"
	"math/rand/v2"
	"net"
	"net/http"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"sync"
	"time"

	"flwrs.example/http-sink-go/pb"
	"google.golang.org/protobuf/encoding/protojson"
	"google.golang.org/protobuf/proto"
)

const version = "0.0.1" // must match manifest.json

// conn is one connection to the hub. Writes come from the reader loop and the
// request goroutine, so they're serialized.
type conn struct {
	net.Conn
	mu sync.Mutex
}

// send writes one frame: u32 LE packet_len, u8 framing version 2,
// u32 LE header_len, FrameHeader, body.
func (c *conn) send(code pb.MsgCode_Enum, body proto.Message) error {
	header, _ := proto.Marshal(&pb.FrameHeader{Code: code})
	payload, err := proto.Marshal(body)
	if err != nil {
		return err
	}
	frame := binary.LittleEndian.AppendUint32(nil, uint32(5+len(header)+len(payload)))
	frame = append(frame, 2)
	frame = binary.LittleEndian.AppendUint32(frame, uint32(len(header)))
	frame = append(append(frame, header...), payload...)
	c.mu.Lock()
	defer c.mu.Unlock()
	_, err = c.Write(frame)
	return err
}

// receive reads one frame and checks its layout (section 4).
func receive(r *bufio.Reader) (*pb.FrameHeader, []byte, error) {
	var b [4]byte
	if _, err := io.ReadFull(r, b[:]); err != nil {
		return nil, nil, err
	}
	n := binary.LittleEndian.Uint32(b[:])
	if n < 5 || n > 256<<20 {
		return nil, nil, fmt.Errorf("bad packet length %d", n)
	}
	packet := make([]byte, n)
	if _, err := io.ReadFull(r, packet); err != nil {
		return nil, nil, err
	}
	if packet[0] != 2 {
		return nil, nil, fmt.Errorf("unknown framing version %d", packet[0])
	}
	hlen := binary.LittleEndian.Uint32(packet[1:5])
	if hlen > 4096 || hlen > n-5 {
		return nil, nil, fmt.Errorf("bad header length %d", hlen)
	}
	header := &pb.FrameHeader{}
	if err := proto.Unmarshal(packet[5:5+hlen], header); err != nil {
		return nil, nil, err
	}
	if header.Flags != 0 {
		return nil, nil, fmt.Errorf("unknown flags %#x", header.Flags)
	}
	return header, packet[5+hlen:], nil
}

// request sends the HTTP request for one event; nil means it succeeded.
func request(event *pb.Event) error {
	fields := map[string]string{}
	for _, f := range event.GetPayload().GetFields() {
		fields[f.GetKey()] = f.GetValue().GetString_()
	}
	method := fields["method"]
	if method == "" {
		method = "POST"
	}
	req, err := http.NewRequest(method, fields["url"], strings.NewReader(fields["body"]))
	if err != nil {
		return err
	}
	resp, err := (&http.Client{Timeout: 10 * time.Second}).Do(req)
	if err != nil {
		return fmt.Errorf("%s %s failed: %w", method, fields["url"], err)
	}
	resp.Body.Close()
	fmt.Printf("%s %s -> %d\n", method, fields["url"], resp.StatusCode)
	return nil
}

// errRejected: SHUTDOWN came before INITIALIZE_RESPONSE. Don't reconnect.
var errRejected = errors.New("rejected by the hub")

// session runs one connection. It returns nil after a SHUTDOWN once accepted.
func session(addr string, initialize *pb.Initialize) error {
	nc, err := net.Dial("tcp", addr)
	if err != nil {
		return err
	}
	var inflight sync.WaitGroup // the request in progress (at most one)
	defer inflight.Wait()       // runs after Close, so a failed write ends it
	c := &conn{Conn: nc}
	defer c.Close()
	if err := c.send(pb.MsgCode_INITIALIZE, initialize); err != nil { // always the first frame
		return err
	}
	accepted, acks, lastSeq := false, false, uint64(0)
	r := bufio.NewReader(c)
	for {
		header, body, err := receive(r)
		if err != nil {
			return err
		}
		code := header.Code
		if !accepted && code != pb.MsgCode_INITIALIZE_RESPONSE && code != pb.MsgCode_SHUTDOWN {
			return fmt.Errorf("protocol violation: %v before INITIALIZE_RESPONSE", code)
		}
		switch code {
		case pb.MsgCode_INITIALIZE_RESPONSE:
			resp := &pb.InitializeResponse{}
			if accepted || proto.Unmarshal(body, resp) != nil {
				return errors.New("protocol violation: bad INITIALIZE_RESPONSE")
			}
			accepted = true
			for _, enabled := range resp.Capabilities {
				acks = acks || enabled == pb.Capability_ACKS
			}
			fmt.Println("accepted by the hub")
		case pb.MsgCode_SHUTDOWN:
			inflight.Wait() // finish (and ack) the request in hand
			c.send(pb.MsgCode_EXIT, &pb.Exit{Code: pb.ExitCode_OK})
			if !accepted {
				return errRejected
			}
			return nil
		case pb.MsgCode_PING:
			ping := &pb.Ping{}
			if err := proto.Unmarshal(body, ping); err != nil {
				return fmt.Errorf("protocol violation: %v", err)
			}
			if err := c.send(pb.MsgCode_PONG, &pb.Pong{Nonce: ping.Nonce}); err != nil {
				return err
			}
		case pb.MsgCode_PONG: // we don't send pings; nothing to do
		case pb.MsgCode_EVENT:
			event := &pb.Event{}
			if header.Seq != lastSeq+1 || proto.Unmarshal(body, event) != nil {
				return fmt.Errorf("protocol violation: bad EVENT (seq %d after %d)", header.Seq, lastSeq)
			}
			lastSeq = header.Seq
			inflight.Add(1)
			go func(seq uint64) { // off the reader, so PINGs are answered meanwhile
				defer inflight.Done()
				if err := request(event); err == nil {
					if acks {
						c.send(pb.MsgCode_ACK, &pb.Ack{UpTo: seq})
					}
				} else {
					c.send(pb.MsgCode_ERROR, &pb.ErrorEvent{Message: err.Error()})
					if acks {
						c.send(pb.MsgCode_NACK, &pb.Nack{Seq: seq, Reason: err.Error()})
					}
				}
			}(header.Seq)
		default:
			return fmt.Errorf("protocol violation: unexpected %v", code)
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
	initialize := &pb.Initialize{
		InstanceId: *id, PluginVersion: version, PluginType: pb.PluginType_SINK,
		Token: *token, InSchema: schema,
		MinProtocolVersion: 2, MaxProtocolVersion: 2,
		Capabilities: []pb.Capability_Enum{pb.Capability_ACKS}, MaxUnacked: 1,
	}

	// Reconnect rule: if the connection drops, retry with the same Initialize.
	addr := net.JoinHostPort(*host, strconv.Itoa(*port))
	delay := time.Second
	for {
		err := session(addr, initialize)
		if err == nil {
			return
		}
		if err == errRejected {
			fmt.Fprintln(os.Stderr, err)
			os.Exit(1)
		}
		fmt.Fprintf(os.Stderr, "disconnected (%v); retrying in %v\n", err, delay)
		time.Sleep(delay + rand.N(delay/4)) // with some jitter
		delay = min(delay*2, 30*time.Second)
	}
}
