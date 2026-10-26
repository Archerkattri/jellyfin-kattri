//! The real Discord IPC transport. Built only with the `discord-ipc` feature.
//!
//! Windows opens the first `\\?\pipe\discord-ipc-N` that accepts. Other platforms connect to
//! the first `discord-ipc-N` socket under `$XDG_RUNTIME_DIR`, or `/tmp`. The default build
//! and the test suite never call [`IpcTransport::connect`].
//!
//! Discord answers every frame. [`IpcTransport`] reads one answer after each write, before
//! it returns. The answers are discarded. Reading them in step with the writes keeps the
//! pipe from filling, and it avoids a reader thread. A reader thread blocked on a Windows
//! pipe handle could hold up writes on the same pipe, which was not tested here.

use std::io::{self, Read, Write};
#[cfg(unix)]
use std::time::Duration;

use super::transport::{PresenceTransport, TransportError};
use super::{HEADER_LEN, Opcode, decode_header, handshake_payload};

#[cfg(windows)]
type Endpoint = std::fs::File;
#[cfg(unix)]
type Endpoint = std::os::unix::net::UnixStream;
#[cfg(not(any(windows, unix)))]
compile_error!("the discord-ipc feature needs a Windows or Unix target");

/// How long a Unix write or answer may take before the connection is treated as failed.
/// Windows std pipes have no timeout, so a silent Discord blocks that caller there.
#[cfg(unix)]
const ANSWER_TIMEOUT: Duration = Duration::from_secs(2);

/// One Discord IPC endpoint, opened by [`PresenceTransport::connect`].
#[derive(Debug, Default)]
pub struct IpcTransport {
    endpoint: Option<Endpoint>,
}

impl IpcTransport {
    pub fn new() -> Self {
        Self::default()
    }
}

impl PresenceTransport for IpcTransport {
    fn connect(&mut self, client_id: &str) -> Result<(), TransportError> {
        self.endpoint = None;
        let payload = handshake_payload(client_id).map_err(|_| TransportError::Connect)?;
        let mut endpoint = open_endpoint().map_err(|_| TransportError::Connect)?;
        super::write_frame(&mut endpoint, Opcode::Handshake, &payload)
            .map_err(|_| TransportError::Connect)?;
        read_answer(&mut endpoint).map_err(|_| TransportError::Connect)?;
        self.endpoint = Some(endpoint);
        Ok(())
    }

    fn write_frame(&mut self, frame: &[u8]) -> Result<(), TransportError> {
        let Some(endpoint) = self.endpoint.as_mut() else {
            return Err(TransportError::Write);
        };
        if send_and_wait(endpoint, frame).is_err() {
            // A broken or silent endpoint is dropped. The plugin retries after its backoff.
            self.endpoint = None;
            return Err(TransportError::Write);
        }
        Ok(())
    }

    fn close(&mut self) {
        if let Some(mut endpoint) = self.endpoint.take() {
            // Best effort: Discord clears the activity when the client goes away.
            let _ = super::write_frame(&mut endpoint, Opcode::Close, b"{}");
        }
    }
}

fn send_and_wait(endpoint: &mut Endpoint, frame: &[u8]) -> io::Result<()> {
    endpoint.write_all(frame)?;
    endpoint.flush()?;
    read_answer(endpoint)
}

/// Read and discard one answer frame. A short read, an unknown opcode or an oversize length
/// is an error.
fn read_answer<R: Read>(reader: &mut R) -> io::Result<()> {
    let mut header = [0_u8; HEADER_LEN];
    reader.read_exact(&mut header)?;
    let (_, length) = decode_header(header)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err.to_string()))?;
    let mut body = vec![0_u8; length];
    reader.read_exact(&mut body)
}

#[cfg(windows)]
fn open_endpoint() -> io::Result<Endpoint> {
    for path in super::windows_pipe_paths() {
        let opened = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path);
        if let Ok(file) = opened {
            return Ok(file);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "no Discord IPC pipe accepted a connection",
    ))
}

#[cfg(unix)]
fn open_endpoint() -> io::Result<Endpoint> {
    let xdg = std::env::var("XDG_RUNTIME_DIR").ok();
    let dir = super::unix_runtime_dir(xdg.as_deref());
    for index in 0..=super::MAX_IPC_INDEX {
        let path = super::unix_socket_path(&dir, index);
        if let Ok(stream) = Endpoint::connect(&path) {
            stream.set_read_timeout(Some(ANSWER_TIMEOUT))?;
            stream.set_write_timeout(Some(ANSWER_TIMEOUT))?;
            return Ok(stream);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "no Discord IPC socket accepted a connection",
    ))
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;
    use crate::discord::encode_frame;

    #[test]
    fn one_answer_frame_is_consumed_completely() {
        let frame = encode_frame(Opcode::Frame, br#"{"evt":null}"#).expect("frame");
        let mut reader = Cursor::new(frame.clone());
        read_answer(&mut reader).expect("answer");
        assert_eq!(reader.position() as usize, frame.len());
    }

    #[test]
    fn short_body_is_an_error() {
        let mut bytes = vec![1, 0, 0, 0, 5, 0, 0, 0];
        bytes.extend_from_slice(b"ab");
        assert!(read_answer(&mut Cursor::new(bytes)).is_err());
    }

    #[test]
    fn oversize_length_is_an_error_before_any_body_is_read() {
        let mut bytes = vec![1, 0, 0, 0];
        bytes.extend_from_slice(&u32::MAX.to_le_bytes());
        assert!(read_answer(&mut Cursor::new(bytes)).is_err());
    }

    #[test]
    fn writing_before_connect_fails_without_touching_the_os() {
        let mut transport = IpcTransport::new();
        assert_eq!(transport.write_frame(b"x"), Err(TransportError::Write));
        transport.close();
        assert!(transport.endpoint.is_none());
    }
}
