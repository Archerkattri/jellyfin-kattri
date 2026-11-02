//! Bounded IPv4 LAN discovery using Jellyfin's UDP client-discovery protocol.
//!
//! Discovery returns untrusted candidates only. It does not validate that a
//! server is reachable, connect to it, or log its advertised URL. Run
//! [`discover_lan_servers`] away from a UI thread because its receive loop is
//! synchronous.
//!
//! The wire contract follows Jellyfin server revision
//! `5b385862fb97201d954a6ac7c9c745d791a738fa`:
//! `src/Jellyfin.Networking/AutoDiscoveryHost.cs` and
//! `MediaBrowser.Model/ApiClient/ServerDiscoveryInfo.cs`.

use std::{
    fmt, io,
    net::{Ipv4Addr, SocketAddr, UdpSocket},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

use serde::Deserialize;
use url::Url;

/// UDP port used by Jellyfin client discovery.
pub const DISCOVERY_PORT: u16 = 7359;
/// The exact probe recognized by the Jellyfin server discovery listener.
pub const DISCOVERY_REQUEST: &[u8] = b"Who is JellyfinServer?";
/// Largest discovery response accepted, including JSON syntax.
pub const MAX_DISCOVERY_DATAGRAM_BYTES: usize = 4 * 1024;
/// Maximum number of received datagrams examined by one scan.
pub const MAX_DISCOVERY_DATAGRAMS: usize = 128;
/// Maximum number of unique candidates returned by one scan.
pub const MAX_DISCOVERY_CANDIDATES: usize = 32;
/// Largest permitted scan duration.
pub const MAX_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(5);

const DEFAULT_DISCOVERY_TIMEOUT: Duration = Duration::from_millis(1500);
const READ_POLL_INTERVAL: Duration = Duration::from_millis(25);
const MAX_DISCOVERY_URL_BYTES: usize = 2048;
const MAX_SERVER_ID_BYTES: usize = 128;
const MAX_SERVER_NAME_BYTES: usize = 256;

/// Limits and destination for one IPv4 discovery scan.
///
/// The default destination is the IPv4 limited broadcast address on UDP 7359.
/// Custom destinations are restricted to loopback, private, or link-local IPv4
/// addresses. A nonstandard port is accepted only for loopback testing.
#[derive(Clone, Debug)]
pub struct DiscoveryOptions {
    target: SocketAddr,
    timeout: Duration,
    max_datagrams: usize,
    max_candidates: usize,
}

impl DiscoveryOptions {
    /// Set a single local IPv4 destination, useful for directed broadcasts or
    /// a caller-selected interface address.
    pub fn with_target(mut self, target: SocketAddr) -> Self {
        self.target = target;
        self
    }

    /// Set the scan duration. Values are validated against
    /// [`MAX_DISCOVERY_TIMEOUT`] when the scan starts.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Set the receive cap. Values must be between 1 and
    /// [`MAX_DISCOVERY_DATAGRAMS`].
    pub fn with_max_datagrams(mut self, max_datagrams: usize) -> Self {
        self.max_datagrams = max_datagrams;
        self
    }

    /// Set the result cap. Values must be between 1 and
    /// [`MAX_DISCOVERY_CANDIDATES`].
    pub fn with_max_candidates(mut self, max_candidates: usize) -> Self {
        self.max_candidates = max_candidates;
        self
    }

    fn is_valid(&self) -> bool {
        let SocketAddr::V4(target) = self.target else {
            return false;
        };
        let ip = *target.ip();
        let local_target =
            ip == Ipv4Addr::BROADCAST || ip.is_private() || ip.is_loopback() || ip.is_link_local();
        local_target
            && (target.port() == DISCOVERY_PORT || ip.is_loopback())
            && !self.timeout.is_zero()
            && self.timeout <= MAX_DISCOVERY_TIMEOUT
            && (1..=MAX_DISCOVERY_DATAGRAMS).contains(&self.max_datagrams)
            && (1..=MAX_DISCOVERY_CANDIDATES).contains(&self.max_candidates)
    }
}

impl Default for DiscoveryOptions {
    fn default() -> Self {
        Self {
            target: SocketAddr::from((Ipv4Addr::BROADCAST, DISCOVERY_PORT)),
            timeout: DEFAULT_DISCOVERY_TIMEOUT,
            max_datagrams: MAX_DISCOVERY_DATAGRAMS,
            max_candidates: MAX_DISCOVERY_CANDIDATES,
        }
    }
}

/// A server advertised by a UDP discovery response.
///
/// This is an untrusted candidate; callers must require an explicit user
/// choice and apply their normal connection policy before using its URL.
#[derive(Clone, PartialEq, Eq)]
pub struct DiscoveryCandidate {
    address: Url,
    id: String,
    name: String,
    source: SocketAddr,
}

impl DiscoveryCandidate {
    /// The validated HTTP(S) URL announced by the server.
    pub fn address(&self) -> &Url {
        &self.address
    }

    /// The server identifier announced in the response.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The display name announced in the response.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The source address of the UDP response.
    pub fn source(&self) -> SocketAddr {
        self.source
    }
}

impl fmt::Debug for DiscoveryCandidate {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DiscoveryCandidate")
            .field("address", &"[REDACTED]")
            .field("id", &"[REDACTED]")
            .field("name", &"[REDACTED]")
            .field("source", &"[REDACTED]")
            .finish()
    }
}

/// Failure to start or complete a local discovery scan.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DiscoveryError {
    /// Options exceeded a safety bound or selected a nonlocal destination.
    #[error("LAN discovery options are invalid or exceed a safety limit")]
    InvalidOptions,
    /// The caller set its cancellation flag while discovery was in progress.
    #[error("LAN discovery was cancelled")]
    Cancelled,
    /// A local UDP socket operation failed. The OS message and addresses are
    /// intentionally omitted from the public error.
    #[error("LAN discovery socket operation failed ({0:?})")]
    Socket(io::ErrorKind),
}

/// Send one Jellyfin client-discovery probe and collect bounded responses.
///
/// `cancelled` is checked before sending and at least every 25 ms while
/// receiving. A normal timeout returns all valid candidates collected so far.
/// Malformed, oversized, duplicate, or unsafe responses are ignored.
pub fn discover_lan_servers(
    options: DiscoveryOptions,
    cancelled: &AtomicBool,
) -> Result<Vec<DiscoveryCandidate>, DiscoveryError> {
    if !options.is_valid() {
        return Err(DiscoveryError::InvalidOptions);
    }
    if cancelled.load(Ordering::Acquire) {
        return Err(DiscoveryError::Cancelled);
    }

    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).map_err(socket_error)?;
    socket.set_broadcast(true).map_err(socket_error)?;
    let deadline = Instant::now()
        .checked_add(options.timeout)
        .ok_or(DiscoveryError::InvalidOptions)?;
    socket
        .send_to(DISCOVERY_REQUEST, options.target)
        .map_err(socket_error)?;

    let mut candidates = Vec::with_capacity(options.max_candidates);
    let mut datagrams_seen = 0;
    let mut buffer = [0_u8; MAX_DISCOVERY_DATAGRAM_BYTES + 1];

    while datagrams_seen < options.max_datagrams {
        if cancelled.load(Ordering::Acquire) {
            return Err(DiscoveryError::Cancelled);
        }

        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        socket
            .set_read_timeout(Some(
                remaining
                    .min(READ_POLL_INTERVAL)
                    .max(Duration::from_millis(1)),
            ))
            .map_err(socket_error)?;

        match socket.recv_from(&mut buffer) {
            Ok((length, source)) => {
                datagrams_seen += 1;
                if length > MAX_DISCOVERY_DATAGRAM_BYTES {
                    continue;
                }
                let Some(candidate) = parse_candidate(&buffer[..length], source) else {
                    continue;
                };
                if candidates.iter().any(|existing: &DiscoveryCandidate| {
                    existing.id == candidate.id && existing.address == candidate.address
                }) {
                    continue;
                }
                candidates.push(candidate);
                if candidates.len() == options.max_candidates {
                    break;
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) => {}
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(socket_error(error)),
        }
    }

    Ok(candidates)
}

fn socket_error(error: io::Error) -> DiscoveryError {
    DiscoveryError::Socket(error.kind())
}

#[derive(Deserialize)]
struct ServerDiscoveryInfo {
    #[serde(rename = "Address")]
    address: String,
    #[serde(rename = "Id")]
    id: String,
    #[serde(rename = "Name")]
    name: String,
}

fn parse_candidate(bytes: &[u8], source: SocketAddr) -> Option<DiscoveryCandidate> {
    let response: ServerDiscoveryInfo = serde_json::from_slice(bytes).ok()?;
    if !valid_text_field(&response.id, MAX_SERVER_ID_BYTES)
        || !valid_text_field(&response.name, MAX_SERVER_NAME_BYTES)
        || response.address.len() > MAX_DISCOVERY_URL_BYTES
        || response.address.trim() != response.address
        || response
            .address
            .chars()
            .any(|character| character.is_control() || character == '\\')
    {
        return None;
    }

    let address = Url::parse(&response.address).ok()?;
    if !matches!(address.scheme(), "http" | "https")
        || address.host_str().is_none()
        || has_user_info(&response.address)
        || !address.username().is_empty()
        || address.password().is_some()
        || address.query().is_some()
        || address.fragment().is_some()
        || address.port() == Some(0)
    {
        return None;
    }

    Some(DiscoveryCandidate {
        address,
        id: response.id,
        name: response.name,
        source,
    })
}

fn has_user_info(raw_url: &str) -> bool {
    let Some((_, remainder)) = raw_url.split_once("://") else {
        return true;
    };
    let authority_end = remainder.find(['/', '?', '#']).unwrap_or(remainder.len());
    let authority = &remainder[..authority_end];
    authority.is_empty() || authority.contains('@')
}

fn valid_text_field(value: &str, max_bytes: usize) -> bool {
    !value.trim().is_empty() && value.len() <= max_bytes && !value.chars().any(char::is_control)
}
