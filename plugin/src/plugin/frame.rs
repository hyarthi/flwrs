//! Wire framing shared by plugins and the hub (protocol version 2; the full
//! specification is `docs/protocol.md`).
//!
//! Frame layout (integers little-endian):
//!
//! ```text
//! packet_len: u32 | version: u8 (= 2) | header_len: u32 | header | body
//! ```
//!
//! `packet_len` counts everything after itself. The header is a protobuf
//! [`FrameHeader`]; its `code` says which message type the body holds.

use crate::schema::protocol::{
    Ack, Credit, ErrorEvent, Event, Exit, FrameHeader, Initialize, InitializeResponse, LogEvent,
    Nack, Ping, Pong, Shutdown, msg_code::Enum as MsgCode,
};
use bytes::Bytes;
use prost::Message;
use std::io;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// The framing version byte: the layout of a frame.
pub const FRAMING_VERSION: u8 = 2;

/// The protocol version this crate implements (negotiated in the handshake).
pub const PROTOCOL_VERSION: u32 = 2;

/// Upper bound for a single frame (`packet_len`).
pub const MAX_PACKET_LEN: usize = 256 * 1024 * 1024;

/// Upper bound for the header.
pub const MAX_HEADER_LEN: usize = 4 * 1024;

/// Most levels of nesting allowed in payload values and schemas.
pub const MAX_NESTING_DEPTH: usize = 32;

/// `packet_len` counts the version byte and `header_len` too.
const FIXED_LEN: usize = 1 + 4;

/// One frame: its header and the undecoded body.
#[derive(Debug, Clone, PartialEq)]
pub struct Frame {
    pub header: FrameHeader,
    pub body: Bytes,
}

impl Frame {
    /// The message type of the body.
    pub fn code(&self) -> MsgCode {
        self.header.code()
    }

    /// Decodes the body as `M`, checking that the frame's code is `M`'s code.
    pub fn decode<M: WireMessage>(&self) -> Result<M, FrameError> {
        if self.code() != M::CODE {
            return Err(FrameError::UnexpectedCode {
                expected: M::CODE,
                actual: self.code(),
            });
        }
        M::decode(self.body.clone()).map_err(FrameError::Decode)
    }
}

/// Why a frame body couldn't be turned into the expected message.
#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("unexpected message code {actual:?}, expected {expected:?}")]
    UnexpectedCode { expected: MsgCode, actual: MsgCode },
    #[error("invalid message body: {0}")]
    Decode(prost::DecodeError),
}

/// A protocol message, with its code.
pub trait WireMessage: Message + Default {
    const CODE: MsgCode;
}

macro_rules! wire_message {
    ($($ty:ty => $code:ident),* $(,)?) => {
        $(impl WireMessage for $ty {
            const CODE: MsgCode = MsgCode::$code;
        })*
    };
}

wire_message! {
    Initialize => Initialize,
    InitializeResponse => InitializeResponse,
    Event => Event,
    LogEvent => Log,
    ErrorEvent => Error,
    Shutdown => Shutdown,
    Exit => Exit,
    Ack => Ack,
    Nack => Nack,
    Credit => Credit,
    Ping => Ping,
    Pong => Pong,
}

/// A header with only a code (every frame except data events).
pub fn header(code: MsgCode) -> FrameHeader {
    FrameHeader {
        code: code as i32,
        ..Default::default()
    }
}

/// Encodes a complete frame.
pub fn encode_frame(header: &FrameHeader, body: &[u8]) -> Vec<u8> {
    let header_len = header.encoded_len();
    let packet_len = FIXED_LEN + header_len + body.len();
    let mut packet = Vec::with_capacity(4 + packet_len);
    packet.extend_from_slice(&(packet_len as u32).to_le_bytes());
    packet.push(FRAMING_VERSION);
    packet.extend_from_slice(&(header_len as u32).to_le_bytes());
    header
        .encode(&mut packet)
        .expect("a Vec grows as needed");
    packet.extend_from_slice(body);
    packet
}

/// Encodes `msg` as a complete frame with a code-only header.
pub fn encode_message<M: WireMessage>(msg: &M) -> Vec<u8> {
    encode_frame(&header(M::CODE), &msg.encode_to_vec())
}

/// Writes one frame.
pub async fn write_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    header: &FrameHeader,
    body: &[u8],
) -> io::Result<()> {
    writer.write_all(&encode_frame(header, body)).await?;
    writer.flush().await
}

/// Encodes and writes one message with a code-only header.
pub async fn write_message<W: AsyncWrite + Unpin, M: WireMessage>(
    writer: &mut W,
    msg: &M,
) -> io::Result<()> {
    writer.write_all(&encode_message(msg)).await?;
    writer.flush().await
}

/// Reads one frame.
///
/// Returns `Ok(None)` when the peer closed the connection between frames.
/// A malformed frame is an `InvalidData` error; the connection should then be
/// closed, since the stream position can't be trusted.
pub async fn read_frame<R: AsyncRead + Unpin>(reader: &mut R) -> io::Result<Option<Frame>> {
    match read_frame_limited(reader, MAX_PACKET_LEN).await? {
        None => Ok(None),
        Some(LimitedFrame::Frame(frame)) => Ok(Some(frame)),
        // A body can't exceed MAX_PACKET_LEN, which the length check enforces.
        Some(LimitedFrame::Oversized { .. }) => unreachable!("body within the packet cap"),
    }
}

/// Result of [`read_frame_limited`].
#[derive(Debug, Clone, PartialEq)]
pub enum LimitedFrame {
    Frame(Frame),
    /// The body was larger than the limit. It was read and discarded without
    /// being buffered, so the stream stays in sync.
    Oversized { header: FrameHeader, body_len: usize },
}

/// Reads one frame, but skips bodies larger than `max_body` bytes instead of
/// buffering them. Use it where the peer isn't trusted with large
/// allocations, e.g. on the hub. Errors are as for [`read_frame`].
pub async fn read_frame_limited<R: AsyncRead + Unpin>(
    reader: &mut R,
    max_body: usize,
) -> io::Result<Option<LimitedFrame>> {
    let mut len_buf = [0u8; 4];
    match reader.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let packet_len = u32::from_le_bytes(len_buf) as usize;
    if !(FIXED_LEN..=MAX_PACKET_LEN).contains(&packet_len) {
        return Err(invalid_data(format!("invalid packet length {packet_len}")));
    }

    let version = reader.read_u8().await?;
    if version != FRAMING_VERSION {
        return Err(invalid_data(format!(
            "unsupported framing version {version} (this side speaks {FRAMING_VERSION})"
        )));
    }

    reader.read_exact(&mut len_buf).await?;
    let header_len = u32::from_le_bytes(len_buf) as usize;
    if header_len > MAX_HEADER_LEN {
        return Err(invalid_data(format!(
            "header too long ({header_len} bytes, max {MAX_HEADER_LEN})"
        )));
    }
    if FIXED_LEN + header_len > packet_len {
        return Err(invalid_data("header length exceeds packet length"));
    }
    let mut header = vec![0u8; header_len];
    reader.read_exact(&mut header).await?;
    let header = parse_header(&header).map_err(invalid_data)?;

    let body_len = packet_len - FIXED_LEN - header_len;
    if body_len > max_body {
        let skipped = tokio::io::copy(
            &mut (&mut *reader).take(body_len as u64),
            &mut tokio::io::sink(),
        )
        .await?;
        if skipped < body_len as u64 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        return Ok(Some(LimitedFrame::Oversized { header, body_len }));
    }
    let mut body = vec![0u8; body_len];
    reader.read_exact(&mut body).await?;
    Ok(Some(LimitedFrame::Frame(Frame {
        header,
        body: Bytes::from(body),
    })))
}

/// Decodes and checks a header: a known code, and no flags (none are defined).
fn parse_header(bytes: &[u8]) -> Result<FrameHeader, String> {
    let header = FrameHeader::decode(bytes).map_err(|e| format!("invalid frame header: {e}"))?;
    match MsgCode::try_from(header.code) {
        Ok(MsgCode::Undefined) => return Err("frame without a message code".into()),
        Err(_) => return Err(format!("unknown message code {}", header.code)),
        Ok(_) => {}
    }
    if header.flags != 0 {
        return Err(format!("unknown frame flags {:#x}", header.flags));
    }
    Ok(header)
}

fn invalid_data(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::protocol::EventMetadata;

    /// Builds a raw frame with an arbitrary version byte and header bytes.
    fn raw_frame(version: u8, header: &[u8], body: &[u8]) -> Vec<u8> {
        let packet_len = FIXED_LEN + header.len() + body.len();
        let mut packet = Vec::new();
        packet.extend_from_slice(&(packet_len as u32).to_le_bytes());
        packet.push(version);
        packet.extend_from_slice(&(header.len() as u32).to_le_bytes());
        packet.extend_from_slice(header);
        packet.extend_from_slice(body);
        packet
    }

    async fn rejected(bytes: &[u8]) -> String {
        let err = read_frame(&mut &bytes[..]).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        err.to_string()
    }

    #[test]
    fn ping_on_the_wire_matches_the_spec_example() {
        let bytes = encode_message(&Ping { nonce: 1 });
        assert_eq!(
            bytes,
            [0x09, 0, 0, 0, 0x02, 0x02, 0, 0, 0, 0x08, 0x0b, 0x08, 0x01]
        );
    }

    #[tokio::test]
    async fn round_trip() {
        let (mut a, mut b) = tokio::io::duplex(1024);
        let event_header = FrameHeader {
            code: MsgCode::Event as i32,
            seq: 7,
            input_seq: 3,
            metadata: Some(EventMetadata {
                event_id: "e".into(),
                hops: 2,
                ..Default::default()
            }),
            attempt: 2,
            flags: 0,
        };
        write_frame(&mut a, &event_header, b"hello").await.unwrap();
        write_message(&mut a, &Shutdown::default()).await.unwrap();
        write_frame(&mut a, &header(MsgCode::Log), b"world")
            .await
            .unwrap();
        drop(a);

        let f = read_frame(&mut b).await.unwrap().unwrap();
        assert_eq!((&f.header, &f.body[..]), (&event_header, &b"hello"[..]));
        let f = read_frame(&mut b).await.unwrap().unwrap();
        assert_eq!((f.code(), &f.body[..]), (MsgCode::Shutdown, &b""[..]));
        let f = read_frame(&mut b).await.unwrap().unwrap();
        assert_eq!((f.code(), &f.body[..]), (MsgCode::Log, &b"world"[..]));
        assert!(read_frame(&mut b).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn message_round_trip_checks_code() {
        let msg = Exit {
            code: 1,
            message: "bye".into(),
        };
        let bytes = encode_message(&msg);
        let frame = read_frame(&mut bytes.as_slice()).await.unwrap().unwrap();
        assert_eq!(frame.decode::<Exit>().unwrap(), msg);
        assert!(matches!(
            frame.decode::<Shutdown>(),
            Err(FrameError::UnexpectedCode {
                expected: MsgCode::Shutdown,
                actual: MsgCode::Exit
            })
        ));
    }

    #[tokio::test]
    async fn frame_split_across_writes() {
        let (mut a, mut b) = tokio::io::duplex(1024);
        let frame = encode_frame(&header(MsgCode::Event), b"split message");
        let reader = tokio::spawn(async move { read_frame(&mut b).await });
        for chunk in frame.chunks(3) {
            a.write_all(chunk).await.unwrap();
            a.flush().await.unwrap();
        }
        let frame = reader.await.unwrap().unwrap().unwrap();
        assert_eq!(&frame.body[..], b"split message");
    }

    #[tokio::test]
    async fn unknown_header_fields_are_ignored() {
        // FrameHeader { code: PING } plus field 15 (varint 1): a future field.
        let header = [0x08, 0x0b, 0x78, 0x01];
        let bytes = raw_frame(FRAMING_VERSION, &header, b"");
        let frame = read_frame(&mut bytes.as_slice()).await.unwrap().unwrap();
        assert_eq!(frame.code(), MsgCode::Ping);
    }

    #[tokio::test]
    async fn rejects_invalid_frames() {
        let flags = FrameHeader {
            code: MsgCode::Event as i32,
            flags: 1,
            ..Default::default()
        }
        .encode_to_vec();
        let cases: Vec<(Vec<u8>, &str)> = vec![
            (raw_frame(1, &[0x08, 0x0b], b""), "unsupported framing version 1"),
            (raw_frame(3, &[0x08, 0x0b], b""), "unsupported framing version 3"),
            (raw_frame(FRAMING_VERSION, &[], b""), "without a message code"),
            (raw_frame(FRAMING_VERSION, &[0x08, 0x00], b""), "without a message code"),
            (raw_frame(FRAMING_VERSION, &[0x08, 0x63], b""), "unknown message code 99"),
            (raw_frame(FRAMING_VERSION, &[0xff, 0xff], b""), "invalid frame header"),
            (raw_frame(FRAMING_VERSION, &flags, b""), "unknown frame flags"),
        ];
        for (bytes, expected) in cases {
            let err = rejected(&bytes).await;
            assert!(err.contains(expected), "expected {expected:?}, got {err:?}");
        }
    }

    #[tokio::test]
    async fn rejects_the_old_text_header_framing() {
        // packet_len, header_len, "Version: 1.0.0\r\nMsg-Code: 3\r\n": the
        // second u32's low byte (the header length) lands on the version byte.
        let header = b"Version: 1.0.0\r\nMsg-Code: 3\r\n";
        let mut bytes = ((4 + header.len()) as u32).to_le_bytes().to_vec();
        bytes.extend_from_slice(&(header.len() as u32).to_le_bytes());
        bytes.extend_from_slice(header);
        assert!(rejected(&bytes).await.contains("unsupported framing version"));
    }

    #[tokio::test]
    async fn rejects_oversized_header() {
        let header = vec![0u8; MAX_HEADER_LEN + 1];
        let bytes = raw_frame(FRAMING_VERSION, &header, b"");
        assert!(rejected(&bytes).await.contains("header too long"));
    }

    #[tokio::test]
    async fn rejects_bad_lengths() {
        // header_len larger than the packet
        let mut frame = encode_frame(&header(MsgCode::Event), b"x");
        frame[5..9].copy_from_slice(&1000u32.to_le_bytes());
        assert!(rejected(&frame).await.contains("exceeds packet length"));
        // packet_len below the fixed part
        let mut frame = encode_frame(&header(MsgCode::Event), b"x");
        frame[0..4].copy_from_slice(&4u32.to_le_bytes());
        assert!(rejected(&frame).await.contains("invalid packet length"));
        // packet_len above the cap
        let mut frame = encode_frame(&header(MsgCode::Event), b"x");
        frame[0..4].copy_from_slice(&((MAX_PACKET_LEN + 1) as u32).to_le_bytes());
        assert!(rejected(&frame).await.contains("invalid packet length"));
    }

    #[tokio::test]
    async fn limited_read_skips_oversized_bodies_and_stays_in_sync() {
        let big = FrameHeader {
            code: MsgCode::Event as i32,
            seq: 1,
            ..Default::default()
        };
        let mut bytes = encode_frame(&big, &[7u8; 100]);
        bytes.extend(encode_frame(&header(MsgCode::Event), b"small"));
        let mut reader = bytes.as_slice();
        assert_eq!(
            read_frame_limited(&mut reader, 10).await.unwrap(),
            Some(LimitedFrame::Oversized {
                header: big,
                body_len: 100
            })
        );
        match read_frame_limited(&mut reader, 10).await.unwrap() {
            Some(LimitedFrame::Frame(frame)) => assert_eq!(&frame.body[..], b"small"),
            other => panic!("expected a frame, got {other:?}"),
        }
        assert_eq!(read_frame_limited(&mut reader, 10).await.unwrap(), None);
    }

    #[tokio::test]
    async fn truncated_frame_is_an_error_not_a_clean_close() {
        let frame = encode_frame(&header(MsgCode::Event), b"body");
        let err = read_frame(&mut &frame[..frame.len() - 1]).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }
}
