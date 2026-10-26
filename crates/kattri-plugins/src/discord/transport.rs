//! The connection the presence plugin writes frames to.
//!
//! The plugin depends only on [`PresenceTransport`]. The real transport, which opens the
//! Discord pipe or socket, is [`super::IpcTransport`] and needs the `discord-ipc` feature.
//! [`MockTransport`] keeps frames in memory and is the only transport the tests use.

use std::sync::{Arc, Mutex, MutexGuard};

/// Why a transport call failed. Carries no endpoint path and no payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum TransportError {
    #[error("Discord IPC connection could not be opened")]
    Connect,
    #[error("Discord IPC write failed")]
    Write,
}

/// A connection that carries encoded presence frames.
pub trait PresenceTransport {
    /// Open the connection and perform the handshake for `client_id`.
    fn connect(&mut self, client_id: &str) -> Result<(), TransportError>;

    /// Write one encoded frame in full.
    fn write_frame(&mut self, frame: &[u8]) -> Result<(), TransportError>;

    /// Drop the connection. Must not fail, because it runs on app exit.
    fn close(&mut self);
}

#[derive(Debug, Default)]
struct MockState {
    connected: bool,
    frames: Vec<Vec<u8>>,
    connect_calls: usize,
    failing_connects: usize,
    failing_writes: usize,
}

/// An in-memory transport. Clones share state, so a test keeps one clone to inspect and
/// gives another to the plugin.
#[derive(Debug, Clone, Default)]
pub struct MockTransport {
    state: Arc<Mutex<MockState>>,
}

impl MockTransport {
    pub fn new() -> Self {
        Self::default()
    }

    /// Every frame written so far, in order, with headers.
    pub fn frames(&self) -> Vec<Vec<u8>> {
        self.locked().frames.clone()
    }

    /// How many times `connect` was called, including failed attempts.
    pub fn connect_calls(&self) -> usize {
        self.locked().connect_calls
    }

    pub fn is_connected(&self) -> bool {
        self.locked().connected
    }

    /// The next `count` connect calls fail.
    pub fn fail_next_connects(&self, count: usize) {
        self.locked().failing_connects = count;
    }

    /// The next `count` writes fail, and each failed write drops the connection.
    pub fn fail_next_writes(&self, count: usize) {
        self.locked().failing_writes = count;
    }

    fn locked(&self) -> MutexGuard<'_, MockState> {
        // A panic in another test thread cannot leave the counters unsafe to read.
        match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

impl PresenceTransport for MockTransport {
    fn connect(&mut self, _client_id: &str) -> Result<(), TransportError> {
        let mut state = self.locked();
        state.connect_calls += 1;
        if state.failing_connects > 0 {
            state.failing_connects -= 1;
            return Err(TransportError::Connect);
        }
        state.connected = true;
        Ok(())
    }

    fn write_frame(&mut self, frame: &[u8]) -> Result<(), TransportError> {
        let mut state = self.locked();
        if !state.connected {
            return Err(TransportError::Write);
        }
        if state.failing_writes > 0 {
            state.failing_writes -= 1;
            state.connected = false;
            return Err(TransportError::Write);
        }
        state.frames.push(frame.to_vec());
        Ok(())
    }

    fn close(&mut self) {
        self.locked().connected = false;
    }
}
