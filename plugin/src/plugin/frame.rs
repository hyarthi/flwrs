//! Wire framing shared by plugins and the hub.
//!
//! Frame layout (all integers little-endian `u32`):
//! `packet_len | header_len | header | protobuf message`,
//! where `packet_len` counts everything after itself.
//!
//! The header is ASCII `Name: value` lines, each ending in CRLF (readers also
//! accept a bare LF), for example:
//!
//! ```text
//! Version: 1.0.0\r\n
//! Msg-Code: 1\r\n
//! ```
//!
//! `Version` and `Msg-Code` are required. Names are case-insensitive, values
//! are trimmed, unknown names are ignored, and duplicate names are an error.
//! `Msg-Code` is a [`MsgCode`] value naming the body's message type.

use crate::schema::common::msg_code::Enum as MsgCode;
use crate::schema::{sink, source, transform};
use bytes::Bytes;
use prost::Message;
use std::io;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Protocol version sent in the `Version` header.
pub const PROTOCOL_VERSION: &str = "1.0.0";

/// Upper bound for a single frame, to avoid allocating on a corrupted length.
pub const MAX_PACKET_LEN: usize = 256 * 1024 * 1024;

/// Upper bound for the header block.
pub const MAX_HEADER_LEN: usize = 4 * 1024;

/// One decoded frame: the message type and the undecoded protobuf body.
#[derive(Debug, Clone, PartialEq)]
pub struct Frame {
    pub code: MsgCode,
    pub body: Bytes,
}

impl Frame {
    /// Decodes the body as `M`, checking that the frame's code is `M`'s code.
    pub fn decode<M: WireMessage>(self) -> Result<M, FrameError> {
        if self.code != M::CODE {
            return Err(FrameError::UnexpectedCode {
                expected: M::CODE,
                actual: self.code,
            });
        }
        M::decode(self.body).map_err(FrameError::Decode)
    }
}

/// Why a frame body couldn't be turned into the expected message.
#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("unexpected Msg-Code {actual:?}, expected {expected:?}")]
    UnexpectedCode { expected: MsgCode, actual: MsgCode },
    #[error("invalid message body: {0}")]
    Decode(prost::DecodeError),
}

/// A top-level protocol message, with its `Msg-Code`.
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
    source::SourceMessage => SourceMessage,
    source::RuntimeSourceMessage => RuntimeSourceMessage,
    sink::SinkMessage => SinkMessage,
    sink::RuntimeSinkMessage => RuntimeSinkMessage,
    transform::TransformMessage => TransformMessage,
    transform::RuntimeTransformMessage => RuntimeTransformMessage,
}

/// Encodes a complete frame with the given code and body.
pub fn encode_frame(code: MsgCode, body: &[u8]) -> Vec<u8> {
    let header = format!(
        "Version: {PROTOCOL_VERSION}\r\nMsg-Code: {}\r\n",
        code as i32
    );
    let packet_len = 4 + header.len() + body.len();
    let mut packet = Vec::with_capacity(4 + packet_len);
    packet.extend_from_slice(&(packet_len as u32).to_le_bytes());
    packet.extend_from_slice(&(header.len() as u32).to_le_bytes());
    packet.extend_from_slice(header.as_bytes());
    packet.extend_from_slice(body);
    packet
}

/// Encodes `msg` as a complete frame.
pub fn encode_message<M: WireMessage>(msg: &M) -> Vec<u8> {
    encode_frame(M::CODE, &msg.encode_to_vec())
}

/// Writes one frame.
pub async fn write_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    code: MsgCode,
    body: &[u8],
) -> io::Result<()> {
    writer.write_all(&encode_frame(code, body)).await?;
    writer.flush().await
}

/// Encodes and writes one message.
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
    Oversized {
        code: MsgCode,
        body_len: usize,
    },
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
    if !(4..=MAX_PACKET_LEN).contains(&packet_len) {
        return Err(invalid_data(format!("invalid packet length {packet_len}")));
    }

    reader.read_exact(&mut len_buf).await?;
    let header_len = u32::from_le_bytes(len_buf) as usize;
    if header_len > MAX_HEADER_LEN {
        return Err(invalid_data(format!(
            "header too long ({header_len} bytes, max {MAX_HEADER_LEN})"
        )));
    }
    if 4 + header_len > packet_len {
        return Err(invalid_data("header length exceeds packet length"));
    }
    let mut header = vec![0u8; header_len];
    reader.read_exact(&mut header).await?;
    let code = parse_header(&header).map_err(invalid_data)?;

    let body_len = packet_len - 4 - header_len;
    if body_len > max_body {
        let skipped = tokio::io::copy(
            &mut (&mut *reader).take(body_len as u64),
            &mut tokio::io::sink(),
        )
        .await?;
        if skipped < body_len as u64 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        return Ok(Some(LimitedFrame::Oversized { code, body_len }));
    }
    let mut body = vec![0u8; body_len];
    reader.read_exact(&mut body).await?;
    Ok(Some(LimitedFrame::Frame(Frame {
        code,
        body: Bytes::from(body),
    })))
}

/// Parses the header block and returns its `Msg-Code`.
fn parse_header(header: &[u8]) -> Result<MsgCode, String> {
    if !header.is_ascii() {
        return Err("header contains non-ASCII bytes".into());
    }
    // ASCII was checked, so this can't fail.
    let text = std::str::from_utf8(header).map_err(|e| e.to_string())?;
    if !text.is_empty() && !text.ends_with('\n') {
        return Err("header must end with a line break".into());
    }

    let mut seen: Vec<String> = Vec::new();
    let mut version = None;
    let mut code = None;
    for line in text.split_terminator('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| format!("invalid header line {line:?}"))?;
        if name.is_empty()
            || name
                .bytes()
                .any(|b| b.is_ascii_whitespace() || b.is_ascii_control())
        {
            return Err(format!("invalid header name {name:?}"));
        }
        let name = name.to_ascii_lowercase();
        if seen.contains(&name) {
            return Err(format!("duplicate header {name:?}"));
        }
        let value = value.trim();
        match name.as_str() {
            "version" => version = Some(value),
            "msg-code" => code = Some(value),
            _ => {} // unknown headers are ignored
        }
        seen.push(name);
    }

    match version {
        None => return Err("missing Version header".into()),
        Some(PROTOCOL_VERSION) => {}
        Some(other) => return Err(format!("unsupported protocol version {other:?}")),
    }
    let code = code.ok_or("missing Msg-Code header")?;
    if code.is_empty() || !code.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!("invalid Msg-Code {code:?}"));
    }
    match code.parse::<i32>().ok().map(MsgCode::try_from) {
        Some(Ok(MsgCode::Undefined)) | Some(Err(_)) | None => {
            Err(format!("unknown Msg-Code {code:?}"))
        }
        Some(Ok(code)) => Ok(code),
    }
}

fn invalid_data(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::sink::{RuntimeSinkMessage, SinkMessage};

    /// Builds a raw frame with an arbitrary header.
    fn raw_frame(header: &[u8], body: &[u8]) -> Vec<u8> {
        let packet_len = 4 + header.len() + body.len();
        let mut packet = Vec::new();
        packet.extend_from_slice(&(packet_len as u32).to_le_bytes());
        packet.extend_from_slice(&(header.len() as u32).to_le_bytes());
        packet.extend_from_slice(header);
        packet.extend_from_slice(body);
        packet
    }

    async fn read_raw(header: &[u8]) -> io::Result<Option<Frame>> {
        let frame = raw_frame(header, b"body");
        read_frame(&mut frame.as_slice()).await
    }

    async fn rejected(header: &[u8]) -> String {
        let err = read_raw(header).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        err.to_string()
    }

    #[tokio::test]
    async fn round_trip() {
        let (mut a, mut b) = tokio::io::duplex(1024);
        write_frame(&mut a, MsgCode::SinkMessage, b"hello")
            .await
            .unwrap();
        write_frame(&mut a, MsgCode::RuntimeSourceMessage, b"")
            .await
            .unwrap();
        write_frame(&mut a, MsgCode::TransformMessage, b"world")
            .await
            .unwrap();
        drop(a);

        let f = read_frame(&mut b).await.unwrap().unwrap();
        assert_eq!((f.code, &f.body[..]), (MsgCode::SinkMessage, &b"hello"[..]));
        let f = read_frame(&mut b).await.unwrap().unwrap();
        assert_eq!(
            (f.code, &f.body[..]),
            (MsgCode::RuntimeSourceMessage, &b""[..])
        );
        let f = read_frame(&mut b).await.unwrap().unwrap();
        assert_eq!(
            (f.code, &f.body[..]),
            (MsgCode::TransformMessage, &b"world"[..])
        );
        assert!(read_frame(&mut b).await.unwrap().is_none());
    }

    #[test]
    fn header_on_the_wire() {
        let frame = encode_frame(MsgCode::SinkMessage, b"");
        assert_eq!(&frame[8..], b"Version: 1.0.0\r\nMsg-Code: 3\r\n");
    }

    #[tokio::test]
    async fn message_round_trip_checks_code() {
        let msg = SinkMessage { payload: None };
        let bytes = encode_message(&msg);
        let frame = read_frame(&mut bytes.as_slice()).await.unwrap().unwrap();
        assert_eq!(frame.clone().decode::<SinkMessage>().unwrap(), msg);
        assert!(matches!(
            frame.decode::<RuntimeSinkMessage>(),
            Err(FrameError::UnexpectedCode {
                expected: MsgCode::RuntimeSinkMessage,
                actual: MsgCode::SinkMessage
            })
        ));
    }

    #[tokio::test]
    async fn frame_split_across_writes() {
        let (mut a, mut b) = tokio::io::duplex(1024);
        let frame = encode_frame(MsgCode::SinkMessage, b"split message");
        let reader = tokio::spawn(async move { read_frame(&mut b).await });
        for chunk in frame.chunks(3) {
            a.write_all(chunk).await.unwrap();
            a.flush().await.unwrap();
        }
        let frame = reader.await.unwrap().unwrap().unwrap();
        assert_eq!(&frame.body[..], b"split message");
    }

    #[tokio::test]
    async fn accepts_case_insensitive_names_lf_and_unknown_headers() {
        let frame = read_raw(b"msg-CODE:  5 \nX-Future: whatever\r\nversion:1.0.0\n")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(frame.code, MsgCode::TransformMessage);
        assert_eq!(&frame.body[..], b"body");
    }

    #[tokio::test]
    async fn rejects_invalid_headers() {
        let cases: &[(&[u8], &str)] = &[
            (b"Version: 1.0.0\r\n", "missing Msg-Code"),
            (b"Msg-Code: 1\r\n", "missing Version"),
            (
                b"Version: 2.0.0\r\nMsg-Code: 1\r\n",
                "unsupported protocol version",
            ),
            (b"1.0.0", "header must end with a line break"),
            (b"Version: 1.0.0\r\nMsg-Code: 1", "header must end"),
            (
                b"Version: 1.0.0\r\nMsg-Code: 1\r\nMsg-Code: 1\r\n",
                "duplicate",
            ),
            (
                b"Version: 1.0.0\r\nversion: 1.0.0\r\nMsg-Code: 1\r\n",
                "duplicate",
            ),
            (b"Version: 1.0.0\r\nMsg-Code: 0\r\n", "unknown Msg-Code"),
            (b"Version: 1.0.0\r\nMsg-Code: 99\r\n", "unknown Msg-Code"),
            (b"Version: 1.0.0\r\nMsg-Code: -1\r\n", "invalid Msg-Code"),
            (b"Version: 1.0.0\r\nMsg-Code: one\r\n", "invalid Msg-Code"),
            (b"Version: 1.0.0\r\nMsg-Code: \r\n", "invalid Msg-Code"),
            (
                b"Version: 1.0.0\r\nMsg-Code: 99999999999\r\n",
                "unknown Msg-Code",
            ),
            (
                b"Version: 1.0.0\r\n\r\nMsg-Code: 1\r\n",
                "invalid header line",
            ),
            (b"Version 1.0.0\r\nMsg-Code: 1\r\n", "invalid header line"),
            (b"Msg Code: 1\r\nVersion: 1.0.0\r\n", "invalid header name"),
            (b": 1\r\nVersion: 1.0.0\r\n", "invalid header name"),
            (
                "Version: 1.0.0\r\nMsg-Code: 1\r\nX: \u{e9}\r\n".as_bytes(),
                "non-ASCII",
            ),
            (b"", "missing Version"),
        ];
        for (header, expected) in cases {
            let err = rejected(header).await;
            assert!(
                err.contains(expected),
                "header {:?}: expected {expected:?}, got {err:?}",
                String::from_utf8_lossy(header)
            );
        }
    }

    #[tokio::test]
    async fn rejects_oversized_header() {
        let mut header = b"Version: 1.0.0\r\nMsg-Code: 1\r\n".to_vec();
        header.extend_from_slice(format!("X-Pad: {}\r\n", "a".repeat(MAX_HEADER_LEN)).as_bytes());
        assert!(rejected(&header).await.contains("header too long"));
    }

    #[tokio::test]
    async fn limited_read_skips_oversized_bodies_and_stays_in_sync() {
        let mut bytes = encode_frame(MsgCode::SinkMessage, &[7u8; 100]);
        bytes.extend(encode_frame(MsgCode::SinkMessage, b"small"));
        let mut reader = bytes.as_slice();
        assert_eq!(
            read_frame_limited(&mut reader, 10).await.unwrap(),
            Some(LimitedFrame::Oversized {
                code: MsgCode::SinkMessage,
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
    async fn rejects_bad_lengths() {
        let mut frame = encode_frame(MsgCode::SinkMessage, b"x");
        frame[4..8].copy_from_slice(&1000u32.to_le_bytes());
        assert!(read_frame(&mut frame.as_slice()).await.is_err());

        let mut frame = encode_frame(MsgCode::SinkMessage, b"x");
        frame[0..4].copy_from_slice(&2u32.to_le_bytes());
        assert!(read_frame(&mut frame.as_slice()).await.is_err());
    }
}
