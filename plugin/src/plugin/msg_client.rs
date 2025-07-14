use byteorder::{LittleEndian, ReadBytesExt, WriteBytesExt};
use bytes::Bytes;
use lazy_static::lazy_static;
use std::io;
use std::io::Cursor;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt, ReadHalf, WriteHalf};
use tokio::net::TcpStream;
use tokio::sync::{Mutex, RwLock};

lazy_static! {
    pub(crate) static ref MSG_CLIENT: Arc<RwLock<MessagingClient>> =
        Arc::new(RwLock::new(MessagingClient::default()));
    pub(crate) static ref PROTOCOL_VERSION: Bytes = Bytes::from("1.0.0");
}

pub(crate) struct MessagingClient {
    socket_in: Option<Mutex<ReadHalf<TcpStream>>>,
    socket_out: Option<Mutex<WriteHalf<TcpStream>>>,
}

impl MessagingClient {
    pub(crate) fn new(socket: Option<TcpStream>) -> Self {
        let (socket_in, socket_out) = match socket {
            Some(socket) => {
                let (socket_in, socket_out) = tokio::io::split(socket);
                (Some(Mutex::new(socket_in)), Some(Mutex::new(socket_out)))
            }
            None => (None, None),
        };
        Self {
            socket_in,
            socket_out,
        }
    }

    pub(crate) async fn send(&self, msg: &[u8]) -> io::Result<()> {
        match &self.socket_out {
            None => Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "MessagingClient not connected",
            )),
            Some(socket) => {
                let mut header = vec![];
                WriteBytesExt::write_u32::<LittleEndian>(
                    &mut header,
                    PROTOCOL_VERSION.len() as u32,
                )?;
                std::io::Write::write(&mut header, &PROTOCOL_VERSION)?;

                let total_len = header.len() + msg.len();

                let mut packet_len = vec![];
                WriteBytesExt::write_u32::<LittleEndian>(&mut packet_len, total_len as u32)?;

                let packet = [&packet_len, &header, msg].concat();

                let mut sock = socket.lock().await;
                match sock.write(packet.as_slice()).await {
                    Ok(_) => Ok(()),
                    Err(e) => Err(e),
                }
            }
        }
    }

    pub(crate) async fn receive(&self) -> io::Result<Option<Bytes>> {
        match &self.socket_in {
            None => Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "MessagingClient not connected",
            )),
            Some(socket) => {
                let mut sock = socket.lock().await;
                let mut packet_len_bytes = vec![0u8; 4];
                let bytes_read = sock.read(&mut packet_len_bytes).await?;
                if bytes_read < 4 {
                    return Ok(None);
                }
                let mut packet_len_reader = Cursor::new(&packet_len_bytes);
                let packet_len =
                    match ReadBytesExt::read_u32::<LittleEndian>(&mut packet_len_reader) {
                        Ok(len) => Ok(len),
                        Err(e) => Err(e),
                    }?;

                let mut buf = vec![0; packet_len as usize];
                sock.read_exact(&mut buf).await?;
                let buf_bytes = Bytes::from(buf);

                let mut reader = Cursor::new(&buf_bytes);
                let header_len = ReadBytesExt::read_u32::<LittleEndian>(&mut reader)?;
                // will support more protocol versions/headers if necessary in the future
                let mut header_buf = vec![0u8; header_len as usize];
                let bytes_read = std::io::Read::read(&mut reader, &mut header_buf)?;
                if bytes_read < header_len as usize {
                    return Err(io::Error::new(
                        io::ErrorKind::Other,
                        "Header length mismatch",
                    ));
                }
                let header = Bytes::from(header_buf);
                if PROTOCOL_VERSION.ne(&header) {
                    return Err(io::Error::new(
                        io::ErrorKind::Other,
                        "Protocol version mismatch",
                    ));
                }

                let msg_len = packet_len - header_len - 4;
                let mut msg_buf = vec![0u8; msg_len as usize];
                let bytes_read = std::io::Read::read(&mut reader, &mut msg_buf)?;
                if bytes_read < msg_len as usize {
                    return Err(io::Error::new(
                        io::ErrorKind::Other,
                        "Message length mismatch",
                    ));
                }

                Ok(Some(Bytes::from(msg_buf)))
            }
        }
    }

    pub(crate) async fn connect(&mut self, addr: &str) -> io::Result<()> {
        if self.socket_in.is_some() && self.socket_out.is_some() {
            return Ok(());
        }
        let socket = TcpStream::connect(addr).await?;
        let (socket_in, socket_out) = tokio::io::split(socket);
        self.socket_in = Some(Mutex::new(socket_in));
        self.socket_out = Some(Mutex::new(socket_out));
        Ok(())
    }
}

impl Default for MessagingClient {
    fn default() -> Self {
        Self::new(None)
    }
}
