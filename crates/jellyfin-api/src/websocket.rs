//! Session WebSocket for remote control (M4-13).
//!
//! The API worker opens one [`SessionSocket`] per signed-in session and calls
//! [`SessionSocket::receive`] from its own thread. Reads block for at most
//! [`RECEIVE_POLL_INTERVAL`], so the worker can check its shutdown channel between frames.
//! The UI thread never touches the socket.
//!
//! Route: `/socket` under the configured base path, with the MediaBrowser `Authorization`
//! header as the only credential. The token is never put in the URL. The route and header
//! auth are **unconfirmed** against a live server: the OpenAPI at api.jellyfin.org (12.2.0)
//! does not list the socket (see `remote.rs`).
//!
//! Frames and messages are capped at [`MAX_MESSAGE_BYTES`] by the protocol layer, before
//! they are buffered, so an oversized message is refused without being allocated.

use std::{
    collections::HashSet,
    io::{self, Read, Write},
    net::{SocketAddr, TcpStream, ToSocketAddrs},
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError},
    },
    thread,
    time::{Duration, Instant},
};

use serde::Deserialize;
use serde_json::Value;
use tungstenite::{
    Connector, Message, WebSocket,
    client::IntoClientRequest,
    handshake::{HandshakeError, HandshakeRole},
    http::{HeaderValue, header::AUTHORIZATION},
    protocol::WebSocketConfig,
    stream::MaybeTlsStream,
};
use url::Url;

use crate::remote::parse_session_message;
use crate::remote_command::{RemoteCommand, RemoteCommandError};
use crate::{GroupState, JellyfinClient, Session, SyncPlayGroup};

/// Largest inbound message or frame, in bytes. A session command is a few hundred bytes.
pub const MAX_MESSAGE_BYTES: usize = 16 * 1024;
/// Queue updates must fit in one bounded session-socket message.
pub const MAX_SYNCPLAY_QUEUE_ITEMS: usize = 128;
const MAX_SYNCPLAY_ID_LENGTH: usize = 36;
const MAX_SYNCPLAY_TEXT_LENGTH: usize = 64;

/// Total time allowed for DNS, TCP, TLS, and the WebSocket handshake.
const SETUP_TIMEOUT: Duration = Duration::from_secs(10);

/// Read timeout after the handshake. [`SessionSocket::receive`] returns
/// [`Inbound::Idle`] when it expires, so the worker can check for shutdown.
pub const RECEIVE_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Why the session socket could not be opened or read.
#[derive(Debug, thiserror::Error)]
pub enum WebSocketError {
    #[error("the server URL cannot be turned into a session socket URL")]
    InvalidConfiguration,
    #[error("the session socket could not connect to the server")]
    Connect(#[source] io::Error),
    #[error("the session socket setup deadline expired")]
    SetupTimeout,
    #[error("the session socket setup was cancelled")]
    SetupCancelled,
    #[error("the session socket resolver is busy")]
    ResolverBusy,
    #[error("the session socket resolver is unavailable")]
    ResolverUnavailable,
    #[error("the server refused the session socket credentials")]
    Unauthorized,
    #[error("the session socket handshake failed")]
    Handshake,
    #[error("an inbound session message is larger than the limit")]
    MessageTooLarge,
    #[error("the session socket failed")]
    Transport(#[source] Box<tungstenite::Error>),
}

/// Legacy SyncPlay command shape used by the core's pause/unpause/seek mapper. The session
/// socket emits [`SyncPlayPlaybackCommand`] for commands so the app can validate queue identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncPlayMessage {
    pub command: SyncPlayWireCommand,
    /// Group position in Jellyfin ticks (100 ns). Never negative.
    pub position_ticks: i64,
    /// Server UTC time at which the group applies the command, as the server wrote it.
    pub when: String,
}

/// The SyncPlay commands this client maps to playback. Other command types are unsupported
/// and are never guessed from these.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncPlayWireCommand {
    Pause,
    Unpause,
    Seek,
}

/// SyncPlay commands carried with their queue-entry identity. This is separate from
/// `SyncPlayMessage`, whose older core mapping deliberately covers only pause/unpause/seek.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncPlayPlaybackAction {
    Pause,
    Unpause,
    Seek,
    Stop,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncPlayPlaybackCommand {
    pub action: SyncPlayPlaybackAction,
    pub group_id: Option<String>,
    /// `None` is accepted only for Stop, which applies regardless of the current entry.
    pub playlist_item_id: Option<String>,
    pub position_ticks: i64,
    pub when: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncPlayQueueEntry {
    pub item_id: String,
    pub playlist_item_id: String,
}

/// One bounded shared queue snapshot from Jellyfin's `PlayQueue` update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncPlayPlayQueueUpdate {
    pub reason: String,
    pub last_update: String,
    pub playlist: Vec<SyncPlayQueueEntry>,
    /// `-1` means there is no active playlist entry.
    pub playing_item_index: i32,
    pub start_position_ticks: i64,
    pub is_playing: bool,
}

/// A typed SyncPlay group, queue, or playback update received on the session WebSocket.
///
/// Participant names and group identifiers are data for the caller; this parser never logs
/// either value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncPlayGroupUpdate {
    Joined(SyncPlayGroup),
    Left {
        group_id: Option<String>,
    },
    NotInGroup {
        group_id: Option<String>,
    },
    Updated(SyncPlayGroup),
    UserJoined {
        group_id: Option<String>,
        user_name: String,
    },
    UserLeft {
        group_id: Option<String>,
        user_name: String,
    },
    StateUpdate {
        group_id: Option<String>,
        state: GroupState,
        reason: String,
    },
    PlayQueue {
        group_id: Option<String>,
        update: SyncPlayPlayQueueUpdate,
    },
    /// The socket bridge already forwards typed group updates, so command identity and Stop
    /// travel through that bounded event path without changing the legacy core DTO.
    PlaybackCommand(SyncPlayPlaybackCommand),
    /// A SyncPlay group update type this client does not yet interpret.
    Other,
}

/// One result of [`SessionSocket::receive`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Inbound {
    /// A command this client executes. The caller maps it to a player action.
    Command(RemoteCommand),
    /// A SyncPlay playback command for the joined group.
    SyncPlay(SyncPlayMessage),
    /// A SyncPlay group membership or state update.
    SyncPlayGroupUpdate(SyncPlayGroupUpdate),
    /// A message this client does not act on: another message type, an unsupported or
    /// malformed command, or a binary frame.
    Ignored,
    /// No frame arrived within [`RECEIVE_POLL_INTERVAL`]. Check for shutdown and receive again.
    Idle,
    /// The server closed the socket. Reconnecting is the caller's decision.
    Closed,
}

/// An open session socket. Blocking: create and use it on the worker thread.
pub struct SessionSocket {
    socket: WebSocket<MaybeTlsStream<DeadlineStream<SystemClock>>>,
}

impl SessionSocket {
    /// Open the session socket for `session` on the server behind `client`.
    ///
    /// The `ws`/`wss` scheme follows the client's `http`/`https` base URL. Plain `ws` to a
    /// remote host is already refused by [`JellyfinClient::new`] unless the caller opted
    /// in, so this does not repeat that check.
    pub fn connect(client: &JellyfinClient, session: &Session) -> Result<Self, WebSocketError> {
        Self::connect_with_cancel(client, session, Arc::new(AtomicBool::new(false)))
    }

    /// Open a session socket while observing `cancelled` during setup. OS DNS itself is
    /// uninterruptible, but abandoning its waiter lets the worker shut down promptly.
    pub fn connect_with_cancel(
        client: &JellyfinClient,
        session: &Session,
        cancelled: Arc<AtomicBool>,
    ) -> Result<Self, WebSocketError> {
        let clock = SystemClock;
        let deadline = clock
            .now()
            .checked_add(SETUP_TIMEOUT)
            .ok_or(WebSocketError::SetupTimeout)?;
        let url = socket_url(client)?;
        let auth = client.session_authorization(session);
        let mut token = HeaderValue::from_str(auth.header_value())
            .map_err(|_| WebSocketError::InvalidConfiguration)?;
        // Keeps the header value out of any debug output that a proxy or logger might add.
        token.set_sensitive(true);

        let mut request = url
            .as_str()
            .into_client_request()
            .map_err(|_| WebSocketError::InvalidConfiguration)?;
        request.headers_mut().insert(AUTHORIZATION, token);

        let plain = url.scheme() == "ws";
        let stream = DeadlineStream::new(
            connect_tcp(&url, deadline, &cancelled)?,
            deadline,
            clock,
            Arc::clone(&cancelled),
        );

        let config = WebSocketConfig::default()
            .max_message_size(Some(MAX_MESSAGE_BYTES))
            .max_frame_size(Some(MAX_MESSAGE_BYTES));
        // `Connector::Plain` keeps `ws` unencrypted. For `wss` the default rustls connector
        // (native roots) is used.
        let connector = plain.then_some(Connector::Plain);
        let (mut socket, _response) =
            tungstenite::client_tls_with_config(request, stream, Some(config), connector)
                .map_err(|error| handshake_error(error, deadline, &clock))?;

        if remaining_until(deadline, clock.now()).is_none() {
            return Err(WebSocketError::SetupTimeout);
        }
        if cancelled.load(Ordering::Acquire) {
            return Err(WebSocketError::SetupCancelled);
        }
        set_poll_interval(socket.get_mut()).map_err(WebSocketError::Connect)?;
        if remaining_until(deadline, clock.now()).is_none() {
            return Err(WebSocketError::SetupTimeout);
        }
        Ok(Self { socket })
    }

    /// Wait for the next inbound message, up to [`RECEIVE_POLL_INTERVAL`].
    ///
    /// Ping frames are answered by the protocol layer. A message that does not parse as a
    /// session command is logged at debug level and returned as [`Inbound::Ignored`].
    pub fn receive(&mut self) -> Result<Inbound, WebSocketError> {
        match self.socket.read() {
            Ok(message) => Ok(classify(message)),
            Err(tungstenite::Error::Io(error))
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                Ok(Inbound::Idle)
            }
            Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => {
                Ok(Inbound::Closed)
            }
            Err(tungstenite::Error::Capacity(_)) => Err(WebSocketError::MessageTooLarge),
            Err(error) => Err(WebSocketError::Transport(Box::new(error))),
        }
    }

    /// Send a close frame and wait for the server's reply. Errors after the close frame
    /// is written are returned, but the socket is unusable either way.
    pub fn close(&mut self) -> Result<(), WebSocketError> {
        match self.socket.close(None) {
            Ok(())
            | Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => {
                Ok(())
            }
            Err(error) => Err(WebSocketError::Transport(Box::new(error))),
        }
    }
}

/// Map one protocol message to the inbound result. Only text frames carry session
/// messages; anything else is ignored.
fn classify(message: Message) -> Inbound {
    match message {
        Message::Text(text) => classify_text(text.as_str()),
        Message::Close(_) => Inbound::Closed,
        _ => Inbound::Ignored,
    }
}

fn classify_text(text: &str) -> Inbound {
    if let Some(update) = parse_sync_play_group_update(text) {
        return match update {
            Ok(update) => Inbound::SyncPlayGroupUpdate(update),
            Err(()) => Inbound::Ignored,
        };
    }
    if let Some(command) = parse_sync_play_playback_command(text) {
        return match command {
            Ok(command) => {
                Inbound::SyncPlayGroupUpdate(SyncPlayGroupUpdate::PlaybackCommand(command))
            }
            Err(_) => Inbound::Ignored,
        };
    }
    if let Some(sync_play) = parse_sync_play(text) {
        return match sync_play {
            Ok(message) => Inbound::SyncPlay(message),
            Err(error) => {
                tracing::debug!(%error, "ignored inbound SyncPlay command");
                Inbound::Ignored
            }
        };
    }
    match parse_session_message(text) {
        Ok(command) => Inbound::Command(command),
        Err(error) => {
            tracing::debug!(%error, "ignored inbound session message");
            Inbound::Ignored
        }
    }
}

/// A session message envelope, read only far enough to pick the parser.
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct SessionEnvelope {
    message_type: String,
    data: Value,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct SyncPlayGroupUpdateData {
    #[serde(rename = "Type")]
    update_type: String,
    #[serde(default)]
    data: Value,
    #[serde(default)]
    group_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct SyncPlayStateUpdateData {
    state: GroupState,
    reason: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct SyncPlayQueueEntryData {
    item_id: String,
    playlist_item_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct SyncPlayPlayQueueData {
    reason: String,
    last_update: String,
    playlist: Vec<SyncPlayQueueEntryData>,
    playing_item_index: i32,
    start_position_ticks: i64,
    is_playing: bool,
}

/// Parse a `SyncPlayGroupUpdate` notification. Unknown update types are preserved as `Other`;
/// malformed known update payloads are ignored by the caller.
fn parse_sync_play_group_update(text: &str) -> Option<Result<SyncPlayGroupUpdate, ()>> {
    if text.len() > MAX_MESSAGE_BYTES {
        return Some(Err(()));
    }
    let envelope: SessionEnvelope = serde_json::from_str(text).ok()?;
    if envelope.message_type != "SyncPlayGroupUpdate" {
        return None;
    }

    let update: SyncPlayGroupUpdateData = serde_json::from_value(envelope.data).ok()?;
    let group_id = update.group_id.or_else(|| {
        update
            .data
            .get("GroupId")
            .and_then(Value::as_str)
            .map(str::to_owned)
    });

    let result = match update.update_type.as_str() {
        "GroupJoined" => serde_json::from_value(update.data)
            .map(SyncPlayGroupUpdate::Joined)
            .map_err(|_| ()),
        "GroupLeft" => Ok(SyncPlayGroupUpdate::Left { group_id }),
        "NotInGroup" => Ok(SyncPlayGroupUpdate::NotInGroup { group_id }),
        "GroupUpdate" => serde_json::from_value(update.data)
            .map(SyncPlayGroupUpdate::Updated)
            .map_err(|_| ()),
        "UserJoined" | "UserLeft" => {
            update
                .data
                .as_str()
                .map(str::to_owned)
                .ok_or(())
                .map(|user_name| {
                    if update.update_type == "UserJoined" {
                        SyncPlayGroupUpdate::UserJoined {
                            group_id,
                            user_name,
                        }
                    } else {
                        SyncPlayGroupUpdate::UserLeft {
                            group_id,
                            user_name,
                        }
                    }
                })
        }
        "StateUpdate" => serde_json::from_value::<SyncPlayStateUpdateData>(update.data)
            .map(|state| SyncPlayGroupUpdate::StateUpdate {
                group_id,
                state: state.state,
                reason: state.reason,
            })
            .map_err(|_| ()),
        "PlayQueue" => {
            if group_id.as_deref().is_some_and(|id| !is_valid_guid(id)) {
                Err(())
            } else {
                serde_json::from_value::<SyncPlayPlayQueueData>(update.data)
                    .map_err(|_| ())
                    .and_then(validate_sync_play_queue_update)
                    .map(|update| SyncPlayGroupUpdate::PlayQueue { group_id, update })
            }
        }
        _ => Ok(SyncPlayGroupUpdate::Other),
    };
    Some(result)
}

fn validate_sync_play_queue_update(
    update: SyncPlayPlayQueueData,
) -> Result<SyncPlayPlayQueueUpdate, ()> {
    if update.playlist.len() > MAX_SYNCPLAY_QUEUE_ITEMS
        || update.reason.is_empty()
        || update.reason.len() > MAX_SYNCPLAY_TEXT_LENGTH
        || !is_utc_timestamp(&update.last_update)
        || update.start_position_ticks < 0
        || update.playing_item_index < -1
        || (update.playlist.is_empty() && update.playing_item_index != -1)
        || usize::try_from(update.playing_item_index)
            .is_ok_and(|index| index >= update.playlist.len())
    {
        return Err(());
    }

    let mut playlist_item_ids = HashSet::with_capacity(update.playlist.len());
    let mut playlist = Vec::with_capacity(update.playlist.len());
    for entry in update.playlist {
        if !is_valid_guid(&entry.item_id)
            || !is_valid_guid(&entry.playlist_item_id)
            || !playlist_item_ids.insert(entry.playlist_item_id.clone())
        {
            return Err(());
        }
        playlist.push(SyncPlayQueueEntry {
            item_id: entry.item_id,
            playlist_item_id: entry.playlist_item_id,
        });
    }

    Ok(SyncPlayPlayQueueUpdate {
        reason: update.reason,
        last_update: update.last_update,
        playlist,
        playing_item_index: update.playing_item_index,
        start_position_ticks: update.start_position_ticks,
        is_playing: update.is_playing,
    })
}

fn parse_sync_play_playback_command(
    text: &str,
) -> Option<Result<SyncPlayPlaybackCommand, RemoteCommandError>> {
    if text.len() > MAX_MESSAGE_BYTES {
        return Some(Err(RemoteCommandError::InvalidPayload));
    }
    let envelope: SessionEnvelope = serde_json::from_str(text).ok()?;
    if envelope.message_type != "SyncPlayCommand" {
        return None;
    }
    if envelope.data.get("PlaylistItemId").is_none()
        && envelope.data.get("Command").and_then(Value::as_str) != Some("Stop")
    {
        // Preserve the public legacy decoder contract. The app refuses these identity-free
        // events for native playback; malformed typed entry IDs must never take this path.
        return None;
    }
    Some(sync_play_playback_command_from_data(envelope.data))
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct SyncPlayPlaybackCommandData {
    command: String,
    #[serde(default)]
    group_id: Option<String>,
    #[serde(default)]
    playlist_item_id: Option<String>,
    #[serde(default)]
    position_ticks: Option<i64>,
    #[serde(default)]
    when: Option<String>,
}

fn sync_play_playback_command_from_data(
    data: Value,
) -> Result<SyncPlayPlaybackCommand, RemoteCommandError> {
    let data: SyncPlayPlaybackCommandData =
        serde_json::from_value(data).map_err(|_| RemoteCommandError::InvalidPayload)?;
    let action = match data.command.as_str() {
        "Pause" => SyncPlayPlaybackAction::Pause,
        "Unpause" => SyncPlayPlaybackAction::Unpause,
        "Seek" => SyncPlayPlaybackAction::Seek,
        "Stop" => SyncPlayPlaybackAction::Stop,
        _ => return Err(RemoteCommandError::UnsupportedCommand),
    };
    let playlist_item_id = match data.playlist_item_id {
        Some(id) if is_valid_guid(&id) => Some(id),
        Some(_) => return Err(RemoteCommandError::InvalidPayload),
        None if action == SyncPlayPlaybackAction::Stop => None,
        None => return Err(RemoteCommandError::InvalidPayload),
    };
    let position_ticks = match data.position_ticks {
        Some(ticks) if ticks >= 0 => ticks,
        Some(_) => return Err(RemoteCommandError::InvalidPayload),
        None if action == SyncPlayPlaybackAction::Stop => 0,
        None => return Err(RemoteCommandError::InvalidPayload),
    };
    let when = match data.when {
        Some(when)
            if when.len() <= MAX_SYNCPLAY_TEXT_LENGTH
                && (action == SyncPlayPlaybackAction::Stop || is_utc_timestamp(&when)) =>
        {
            when
        }
        None if action == SyncPlayPlaybackAction::Stop => String::new(),
        _ => return Err(RemoteCommandError::InvalidPayload),
    };
    if data.group_id.as_ref().is_some_and(|id| !is_valid_guid(id)) {
        return Err(RemoteCommandError::InvalidPayload);
    }
    Ok(SyncPlayPlaybackCommand {
        action,
        group_id: data.group_id,
        playlist_item_id,
        position_ticks,
        when,
    })
}

fn is_valid_guid(value: &str) -> bool {
    if value.len() > MAX_SYNCPLAY_ID_LENGTH {
        return false;
    }
    let bytes = value.as_bytes();
    let hyphenated = bytes.len() == 36
        && bytes.iter().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => *byte == b'-',
            _ => byte.is_ascii_hexdigit(),
        });
    let simple = bytes.len() == 32 && bytes.iter().all(u8::is_ascii_hexdigit);
    hyphenated || simple
}

fn is_utc_timestamp(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() < 20 || bytes.len() > MAX_SYNCPLAY_TEXT_LENGTH || bytes.last() != Some(&b'Z') {
        return false;
    }
    if !(*b"--T::")
        .into_iter()
        .zip([4, 7, 10, 13, 16])
        .all(|(expected, index)| bytes.get(index) == Some(&expected))
    {
        return false;
    }
    if ![0..4, 5..7, 8..10, 11..13, 14..16, 17..19]
        .into_iter()
        .all(|range| bytes[range].iter().all(u8::is_ascii_digit))
    {
        return false;
    }
    let Ok(year) = value[0..4].parse::<u32>() else {
        return false;
    };
    let Ok(month) = value[5..7].parse::<u32>() else {
        return false;
    };
    let Ok(day) = value[8..10].parse::<u32>() else {
        return false;
    };
    let Ok(hour) = value[11..13].parse::<u32>() else {
        return false;
    };
    let Ok(minute) = value[14..16].parse::<u32>() else {
        return false;
    };
    let Ok(second) = value[17..19].parse::<u32>() else {
        return false;
    };
    let fraction_ok = match bytes.get(19) {
        Some(b'Z') => bytes.len() == 20,
        Some(b'.') => bytes.len() > 21 && bytes[20..bytes.len() - 1].iter().all(u8::is_ascii_digit),
        _ => false,
    };
    if !fraction_ok || !(1..=12).contains(&month) || hour > 23 || minute > 59 || second > 59 {
        return false;
    }
    let leap_year = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days_in_month = match month {
        2 if leap_year => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    };
    (1..=days_in_month).contains(&day)
}

/// The `Data` of a `SyncPlayCommand` message. The position and time are read as `Option` and
/// checked in [`sync_play_from_data`], so a missing field and a null one get the same refusal.
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct SyncPlayData {
    command: String,
    #[serde(default)]
    position_ticks: Option<i64>,
    #[serde(default)]
    when: Option<String>,
}

/// Parse a `SyncPlayCommand` message. Returns `None` for any other message type or for text
/// that is not an envelope, so the caller can try the remote-control parser.
fn parse_sync_play(text: &str) -> Option<Result<SyncPlayMessage, RemoteCommandError>> {
    let envelope: SessionEnvelope = serde_json::from_str(text).ok()?;
    if envelope.message_type != "SyncPlayCommand" {
        return None;
    }
    Some(sync_play_from_data(envelope.data))
}

fn sync_play_from_data(data: Value) -> Result<SyncPlayMessage, RemoteCommandError> {
    let data: SyncPlayData =
        serde_json::from_value(data).map_err(|_| RemoteCommandError::InvalidPayload)?;
    let command = match data.command.as_str() {
        "Pause" => SyncPlayWireCommand::Pause,
        "Unpause" => SyncPlayWireCommand::Unpause,
        "Seek" => SyncPlayWireCommand::Seek,
        _ => return Err(RemoteCommandError::UnsupportedCommand),
    };
    let position_ticks = data
        .position_ticks
        .filter(|ticks| *ticks >= 0)
        .ok_or(RemoteCommandError::InvalidPayload)?;
    let when = data
        .when
        .filter(|when| !when.is_empty())
        .ok_or(RemoteCommandError::InvalidPayload)?;
    Ok(SyncPlayMessage {
        command,
        position_ticks,
        when,
    })
}

/// The socket URL: the client's base URL with `ws`/`wss` in place of `http`/`https`, and
/// the `socket` path segment. It holds no credentials, because [`ClientConfig::new`]
/// refuses base URLs with userinfo, query, or fragment.
///
/// [`ClientConfig::new`]: crate::ClientConfig::new
fn socket_url(client: &JellyfinClient) -> Result<Url, WebSocketError> {
    let mut url = client.endpoint(&["socket"]);
    let scheme = match url.scheme() {
        "http" => "ws",
        "https" => "wss",
        _ => return Err(WebSocketError::InvalidConfiguration),
    };
    url.set_scheme(scheme)
        .map_err(|()| WebSocketError::InvalidConfiguration)?;
    Ok(url)
}

trait Clock: Send + Sync {
    fn now(&self) -> Instant;
}

#[derive(Clone, Copy)]
struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

fn remaining_until(deadline: Instant, now: Instant) -> Option<Duration> {
    deadline
        .checked_duration_since(now)
        .filter(|remaining| !remaining.is_zero())
}

fn setup_timeout_io_error() -> io::Error {
    io::Error::new(
        io::ErrorKind::TimedOut,
        "session socket setup deadline expired",
    )
}

/// Carries the absolute setup deadline through both TLS and the WebSocket handshake.
struct DeadlineStream<C> {
    stream: TcpStream,
    deadline: Option<Instant>,
    clock: C,
    cancelled: Option<Arc<AtomicBool>>,
}

impl<C: Clock> DeadlineStream<C> {
    fn new(stream: TcpStream, deadline: Instant, clock: C, cancelled: Arc<AtomicBool>) -> Self {
        Self {
            stream,
            deadline: Some(deadline),
            clock,
            cancelled: Some(cancelled),
        }
    }

    fn remaining(&self) -> io::Result<Option<Duration>> {
        self.deadline
            .map(|deadline| {
                remaining_until(deadline, self.clock.now()).ok_or_else(setup_timeout_io_error)
            })
            .transpose()
    }

    fn check_deadline(&self) -> io::Result<()> {
        if self
            .cancelled
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::Acquire))
        {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "session socket setup cancelled",
            ));
        }
        if self.remaining()?.is_some() || self.deadline.is_none() {
            Ok(())
        } else {
            Err(setup_timeout_io_error())
        }
    }

    fn disarm(&mut self) {
        self.deadline = None;
        self.cancelled = None;
    }
}

impl<C: Clock> Read for DeadlineStream<C> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        loop {
            if let Some(remaining) = self.remaining()? {
                self.stream
                    .set_read_timeout(Some(remaining.min(Duration::from_millis(100))))?;
                match self.stream.read(buffer) {
                    Err(error)
                        if matches!(
                            error.kind(),
                            io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                        ) =>
                    {
                        self.check_deadline()?;
                    }
                    result => {
                        self.check_deadline()?;
                        return result;
                    }
                }
            } else {
                return self.stream.read(buffer);
            }
        }
    }
}

impl<C: Clock> Write for DeadlineStream<C> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        loop {
            if let Some(remaining) = self.remaining()? {
                self.stream
                    .set_write_timeout(Some(remaining.min(Duration::from_millis(100))))?;
                match self.stream.write(buffer) {
                    Err(error)
                        if matches!(
                            error.kind(),
                            io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                        ) =>
                    {
                        self.check_deadline()?;
                    }
                    result => {
                        self.check_deadline()?;
                        return result;
                    }
                }
            } else {
                return self.stream.write(buffer);
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        loop {
            if let Some(remaining) = self.remaining()? {
                self.stream
                    .set_write_timeout(Some(remaining.min(Duration::from_millis(100))))?;
                match self.stream.flush() {
                    Err(error)
                        if matches!(
                            error.kind(),
                            io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                        ) =>
                    {
                        self.check_deadline()?;
                    }
                    result => {
                        self.check_deadline()?;
                        return result;
                    }
                }
            } else {
                return self.stream.flush();
            }
        }
    }
}

type Resolver = Box<dyn FnOnce() -> io::Result<Vec<SocketAddr>> + Send + 'static>;

struct ResolverJob {
    deadline: Instant,
    clock: Arc<dyn Clock>,
    cancelled: Arc<AtomicBool>,
    resolve: Resolver,
    response: SyncSender<io::Result<Vec<SocketAddr>>>,
}

struct ResolverWaiter {
    deadline: Instant,
    clock: Arc<dyn Clock>,
    cancelled: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    response: Receiver<io::Result<Vec<SocketAddr>>>,
}

impl ResolverWaiter {
    fn wait(&self) -> Result<Vec<SocketAddr>, WebSocketError> {
        let addresses = loop {
            if self.stop.load(Ordering::Acquire) {
                self.cancelled.store(true, Ordering::Release);
                return Err(WebSocketError::SetupCancelled);
            }
            let Some(remaining) = remaining_until(self.deadline, self.clock.now()) else {
                self.cancelled.store(true, Ordering::Release);
                return Err(WebSocketError::SetupTimeout);
            };
            match self
                .response
                .recv_timeout(remaining.min(Duration::from_millis(50)))
            {
                Ok(addresses) => break addresses,
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => {
                    self.cancelled.store(true, Ordering::Release);
                    return Err(WebSocketError::ResolverUnavailable);
                }
            }
        };
        if remaining_until(self.deadline, self.clock.now()).is_none() {
            self.cancelled.store(true, Ordering::Release);
            return Err(WebSocketError::SetupTimeout);
        }
        addresses.map_err(WebSocketError::Connect)
    }
}

impl Drop for ResolverWaiter {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
    }
}

// OS DNS cannot be interrupted, so keep at most one lookup active and one queued process-wide.
static RESOLVER_WORKER: OnceLock<Result<SyncSender<ResolverJob>, io::ErrorKind>> = OnceLock::new();

fn resolver_queue() -> Result<&'static SyncSender<ResolverJob>, WebSocketError> {
    let result = RESOLVER_WORKER.get_or_init(|| {
        let (sender, receiver) = mpsc::sync_channel(1);
        match thread::Builder::new()
            .name("jellyfin-session-dns".into())
            .spawn(move || resolver_worker(receiver))
        {
            Ok(_) => Ok(sender),
            Err(error) => Err(error.kind()),
        }
    });
    result
        .as_ref()
        .map_err(|_| WebSocketError::ResolverUnavailable)
}

fn resolver_worker(receiver: Receiver<ResolverJob>) {
    while let Ok(job) = receiver.recv() {
        if job.cancelled.load(Ordering::Acquire)
            || remaining_until(job.deadline, job.clock.now()).is_none()
        {
            continue;
        }
        let addresses = (job.resolve)();
        if job.cancelled.load(Ordering::Acquire)
            || remaining_until(job.deadline, job.clock.now()).is_none()
        {
            continue;
        }
        let _ = job.response.send(addresses);
    }
}

fn submit_resolution(
    sender: &SyncSender<ResolverJob>,
    deadline: Instant,
    clock: Arc<dyn Clock>,
    stop: Arc<AtomicBool>,
    resolve: Resolver,
) -> Result<ResolverWaiter, WebSocketError> {
    if remaining_until(deadline, clock.now()).is_none() {
        return Err(WebSocketError::SetupTimeout);
    }
    let (response_sender, response) = mpsc::sync_channel(1);
    let cancelled = Arc::new(AtomicBool::new(false));
    let job = ResolverJob {
        deadline,
        clock: Arc::clone(&clock),
        cancelled: Arc::clone(&cancelled),
        resolve,
        response: response_sender,
    };
    match sender.try_send(job) {
        Ok(()) => Ok(ResolverWaiter {
            deadline,
            clock,
            cancelled,
            stop,
            response,
        }),
        Err(TrySendError::Full(_)) => Err(WebSocketError::ResolverBusy),
        Err(TrySendError::Disconnected(_)) => Err(WebSocketError::ResolverUnavailable),
    }
}

fn resolve_addresses(
    host: String,
    port: u16,
    deadline: Instant,
    stop: &Arc<AtomicBool>,
) -> Result<Vec<SocketAddr>, WebSocketError> {
    let resolver = resolver_queue()?;
    let host_for_lookup = host;
    let waiter = submit_resolution(
        resolver,
        deadline,
        Arc::new(SystemClock),
        Arc::clone(stop),
        Box::new(move || {
            (host_for_lookup.as_str(), port)
                .to_socket_addrs()
                .map(|addresses| addresses.collect::<Vec<_>>())
        }),
    )?;
    waiter.wait()
}

/// Resolve once, then try each address with only the budget remaining from the same deadline.
fn connect_tcp(
    url: &Url,
    deadline: Instant,
    cancelled: &Arc<AtomicBool>,
) -> Result<TcpStream, WebSocketError> {
    let host = url
        .host_str()
        .ok_or(WebSocketError::InvalidConfiguration)?
        .to_owned();
    let port = url
        .port_or_known_default()
        .ok_or(WebSocketError::InvalidConfiguration)?;
    let addresses = resolve_addresses(host, port, deadline, cancelled)?;
    // std exposes only a blocking connect_timeout, not a cancellable in-flight socket connect.
    // Use the whole remaining setup budget once so slow but valid TCP handshakes can complete.
    connect_addresses_with(addresses, deadline, &SystemClock, |address, remaining| {
        if cancelled.load(Ordering::Acquire) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "session socket setup cancelled",
            ));
        }
        TcpStream::connect_timeout(&address, remaining)
    })
}

fn connect_addresses_with<C, F, S>(
    addresses: impl IntoIterator<Item = SocketAddr>,
    deadline: Instant,
    clock: &C,
    mut connect: F,
) -> Result<S, WebSocketError>
where
    C: Clock,
    F: FnMut(SocketAddr, Duration) -> io::Result<S>,
{
    let mut last_error = None;
    for address in addresses {
        let remaining =
            remaining_until(deadline, clock.now()).ok_or(WebSocketError::SetupTimeout)?;
        match connect(address, remaining) {
            Ok(stream) => {
                if remaining_until(deadline, clock.now()).is_none() {
                    return Err(WebSocketError::SetupTimeout);
                }
                return Ok(stream);
            }
            Err(error) => {
                if remaining_until(deadline, clock.now()).is_none() {
                    return Err(WebSocketError::SetupTimeout);
                }
                last_error = Some(error);
            }
        }
    }
    if let Some(error) = last_error {
        Err(WebSocketError::Connect(error))
    } else {
        Err(WebSocketError::Connect(io::Error::new(
            io::ErrorKind::NotFound,
            "the server host has no address",
        )))
    }
}

fn set_poll_interval(stream: &mut MaybeTlsStream<DeadlineStream<SystemClock>>) -> io::Result<()> {
    let tcp = match stream {
        MaybeTlsStream::Plain(tcp) => tcp,
        MaybeTlsStream::Rustls(tls) => tls.get_mut(),
        _ => return Ok(()),
    };
    tcp.disarm();
    tcp.stream
        .set_read_timeout(Some(RECEIVE_POLL_INTERVAL))
        .and_then(|()| tcp.stream.set_write_timeout(Some(SETUP_TIMEOUT)))
}

fn handshake_error<R: HandshakeRole>(
    error: HandshakeError<R>,
    deadline: Instant,
    clock: &impl Clock,
) -> WebSocketError {
    if remaining_until(deadline, clock.now()).is_none()
        || matches!(
            &error,
            HandshakeError::Failure(tungstenite::Error::Io(source))
                if source.kind() == io::ErrorKind::TimedOut
        )
    {
        return WebSocketError::SetupTimeout;
    }
    if matches!(
        &error,
        HandshakeError::Failure(tungstenite::Error::Io(source))
            if source.kind() == io::ErrorKind::Interrupted
    ) {
        return WebSocketError::SetupCancelled;
    }
    match error {
        HandshakeError::Failure(tungstenite::Error::Http(response))
            if response.status() == tungstenite::http::StatusCode::UNAUTHORIZED =>
        {
            WebSocketError::Unauthorized
        }
        HandshakeError::Failure(_) | HandshakeError::Interrupted(_) => WebSocketError::Handshake,
    }
}

#[cfg(test)]
mod tests {
    use std::{
        net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream},
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, Ordering},
            mpsc,
        },
        thread::{self, JoinHandle},
        time::{Duration, Instant},
    };

    use super::{
        Clock, DeadlineStream, Inbound, ResolverJob, SyncPlayGroupUpdate, SyncPlayMessage,
        SyncPlayPlayQueueUpdate, SyncPlayPlaybackAction, SyncPlayPlaybackCommand,
        SyncPlayQueueEntry, SyncPlayWireCommand, SystemClock, WebSocketError,
        connect_addresses_with, parse_sync_play, parse_sync_play_group_update,
        parse_sync_play_playback_command, resolver_worker, submit_resolution,
    };
    use crate::remote_command::RemoteCommandError;
    use crate::{GroupState, SyncPlayGroup};
    use serde_json::{Value, json};

    struct TestClock(Mutex<Instant>);

    impl TestClock {
        fn new(now: Instant) -> Self {
            Self(Mutex::new(now))
        }

        fn advance(&self, duration: Duration) {
            let mut now = self.0.lock().expect("test clock lock");
            *now = now.checked_add(duration).expect("test clock duration");
        }
    }

    impl Clock for TestClock {
        fn now(&self) -> Instant {
            *self.0.lock().expect("test clock lock")
        }
    }

    fn test_address(port: u16) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)
    }

    fn local_resolver_worker() -> (mpsc::SyncSender<ResolverJob>, JoinHandle<()>) {
        let (sender, receiver) = mpsc::sync_channel(1);
        let worker = thread::spawn(move || resolver_worker(receiver));
        (sender, worker)
    }

    fn clock_handle(clock: &Arc<TestClock>) -> Arc<dyn Clock> {
        let clock: Arc<dyn Clock> = clock.clone();
        clock
    }

    fn sync_play(data: &str) -> String {
        format!(r#"{{"MessageType":"SyncPlayCommand","Data":{data}}}"#)
    }

    fn group_update(update_type: &str, data: &str) -> String {
        format!(
            r#"{{"MessageType":"SyncPlayGroupUpdate","Data":{{"Type":"{update_type}","Data":{data}}}}}"#
        )
    }

    fn play_queue_update(data: Value) -> String {
        json!({
            "MessageType": "SyncPlayGroupUpdate",
            "Data": {
                "GroupId": "3f2504e0-4f89-11d3-9a0c-0305e82c3301",
                "Type": "PlayQueue",
                "Data": data,
            }
        })
        .to_string()
    }

    #[test]
    fn parses_each_known_sync_play_group_update_shape() {
        let group = SyncPlayGroup {
            group_id: "group-1".into(),
            group_name: "Example group".into(),
            state: GroupState::Playing,
            participants: vec!["alice".into(), "bob".into()],
            last_updated_at: "2026-10-24T12:00:00Z".into(),
        };
        let group_json = r#"{"GroupId":"group-1","GroupName":"Example group","State":"Playing","Participants":["alice","bob"],"LastUpdatedAt":"2026-10-24T12:00:00Z"}"#;

        assert_eq!(
            parse_sync_play_group_update(&group_update("GroupJoined", group_json)),
            Some(Ok(SyncPlayGroupUpdate::Joined(group.clone())))
        );
        assert_eq!(
            parse_sync_play_group_update(&group_update("GroupUpdate", group_json)),
            Some(Ok(SyncPlayGroupUpdate::Updated(group)))
        );
        assert_eq!(
            parse_sync_play_group_update(&group_update("GroupLeft", r#"{"GroupId":"group-1"}"#)),
            Some(Ok(SyncPlayGroupUpdate::Left {
                group_id: Some("group-1".into())
            }))
        );
        assert_eq!(
            parse_sync_play_group_update(&group_update("NotInGroup", "null")),
            Some(Ok(SyncPlayGroupUpdate::NotInGroup { group_id: None }))
        );
        assert_eq!(
            parse_sync_play_group_update(&group_update("UserJoined", r#""alice""#)),
            Some(Ok(SyncPlayGroupUpdate::UserJoined {
                group_id: None,
                user_name: "alice".into()
            }))
        );
        assert_eq!(
            parse_sync_play_group_update(&group_update("UserLeft", r#""bob""#)),
            Some(Ok(SyncPlayGroupUpdate::UserLeft {
                group_id: None,
                user_name: "bob".into()
            }))
        );
        assert_eq!(
            parse_sync_play_group_update(&group_update(
                "StateUpdate",
                r#"{"State":"Paused","Reason":"Buffering"}"#
            )),
            Some(Ok(SyncPlayGroupUpdate::StateUpdate {
                group_id: None,
                state: GroupState::Paused,
                reason: "Buffering".into()
            }))
        );
    }

    #[test]
    fn unknown_sync_play_group_updates_are_other_and_known_malformed_updates_are_ignored() {
        assert_eq!(
            parse_sync_play_group_update(&group_update("FutureUpdate", r#"{"Value":1}"#)),
            Some(Ok(SyncPlayGroupUpdate::Other))
        );
        for text in [
            group_update("GroupJoined", r#"{"GroupId":[]}"#),
            group_update("GroupUpdate", "null"),
            group_update("UserJoined", r#"{"UserName":"alice"}"#),
            group_update("UserLeft", "null"),
            group_update("StateUpdate", r#"{"State":"Paused"}"#),
            r#"{"MessageType":"SyncPlayGroupUpdate","Data":{"Type":"StateUpdate","Data":[]} }"#
                .into(),
        ] {
            assert_eq!(parse_sync_play_group_update(&text), Some(Err(())));
        }
        assert_eq!(parse_sync_play_group_update("not json"), None);
        assert_eq!(parse_sync_play_group_update(&sync_play("{}")), None);
    }

    #[test]
    fn play_queue_update_keeps_duplicate_media_entries_and_entry_identity() {
        const ITEM: &str = "3f2504e0-4f89-11d3-9a0c-0305e82c3302";
        const ENTRY_A: &str = "3f2504e0-4f89-11d3-9a0c-0305e82c3303";
        const ENTRY_B: &str = "3f2504e0-4f89-11d3-9a0c-0305e82c3304";
        let text = play_queue_update(json!({
            "Reason": "NewPlaylist",
            "LastUpdate": "2026-11-01T12:00:00.1250000Z",
            "Playlist": [
                { "ItemId": ITEM, "PlaylistItemId": ENTRY_A },
                { "ItemId": ITEM, "PlaylistItemId": ENTRY_B },
            ],
            "PlayingItemIndex": 1,
            "StartPositionTicks": 12_345,
            "IsPlaying": true,
        }));

        assert!(matches!(
            super::classify_text(&text),
            Inbound::SyncPlayGroupUpdate(SyncPlayGroupUpdate::PlayQueue { .. })
        ));

        assert_eq!(
            parse_sync_play_group_update(&text),
            Some(Ok(SyncPlayGroupUpdate::PlayQueue {
                group_id: Some("3f2504e0-4f89-11d3-9a0c-0305e82c3301".into()),
                update: SyncPlayPlayQueueUpdate {
                    reason: "NewPlaylist".into(),
                    last_update: "2026-11-01T12:00:00.1250000Z".into(),
                    playlist: vec![
                        SyncPlayQueueEntry {
                            item_id: ITEM.into(),
                            playlist_item_id: ENTRY_A.into(),
                        },
                        SyncPlayQueueEntry {
                            item_id: ITEM.into(),
                            playlist_item_id: ENTRY_B.into(),
                        },
                    ],
                    playing_item_index: 1,
                    start_position_ticks: 12_345,
                    is_playing: true,
                },
            }))
        );
    }

    #[test]
    fn play_queue_update_rejects_malformed_duplicate_and_unbounded_entries() {
        const ITEM: &str = "3f2504e0-4f89-11d3-9a0c-0305e82c3302";
        const ENTRY: &str = "3f2504e0-4f89-11d3-9a0c-0305e82c3303";
        let base = json!({
            "Reason": "NewPlaylist",
            "LastUpdate": "2026-11-01T12:00:00Z",
            "Playlist": [{ "ItemId": ITEM, "PlaylistItemId": ENTRY }],
            "PlayingItemIndex": 0,
            "StartPositionTicks": 0,
            "IsPlaying": false,
        });
        for (label, mutate) in [
            ("bad time", json!({ "LastUpdate": "yesterdayZ" })),
            ("negative ticks", json!({ "StartPositionTicks": -1 })),
            ("out-of-range index", json!({ "PlayingItemIndex": 1 })),
            (
                "duplicate playlist IDs",
                json!({
                    "Playlist": [
                        { "ItemId": ITEM, "PlaylistItemId": ENTRY },
                        { "ItemId": ITEM, "PlaylistItemId": ENTRY },
                    ],
                    "PlayingItemIndex": 1,
                }),
            ),
        ] {
            let mut data = base.clone();
            let object = data.as_object_mut().expect("base fixture is an object");
            for (key, value) in mutate.as_object().expect("object").iter() {
                object.insert(key.clone(), value.clone());
            }
            assert_eq!(
                parse_sync_play_group_update(&play_queue_update(data)),
                Some(Err(())),
                "{label}"
            );
        }

        let oversized = json!({
            "Reason": "NewPlaylist",
            "LastUpdate": "2026-11-01T12:00:00Z",
            "Playlist": (0..=super::MAX_SYNCPLAY_QUEUE_ITEMS)
                .map(|_| json!({ "ItemId": ITEM, "PlaylistItemId": ENTRY }))
                .collect::<Vec<_>>(),
            "PlayingItemIndex": 0,
            "StartPositionTicks": 0,
            "IsPlaying": false,
        });
        assert_eq!(
            parse_sync_play_group_update(&play_queue_update(oversized)),
            Some(Err(()))
        );
    }

    #[test]
    fn typed_playback_commands_retain_entry_identity_and_support_stop() {
        let pause = sync_play(
            r#"{"GroupId":"3f2504e0-4f89-11d3-9a0c-0305e82c3301","Command":"Pause","PlaylistItemId":"3f2504e0-4f89-11d3-9a0c-0305e82c3302","PositionTicks":600000000,"When":"1970-01-01T00:16:40Z"}"#,
        );
        assert_eq!(
            parse_sync_play_playback_command(&pause),
            Some(Ok(SyncPlayPlaybackCommand {
                action: SyncPlayPlaybackAction::Pause,
                group_id: Some("3f2504e0-4f89-11d3-9a0c-0305e82c3301".into()),
                playlist_item_id: Some("3f2504e0-4f89-11d3-9a0c-0305e82c3302".into()),
                position_ticks: 600_000_000,
                when: "1970-01-01T00:16:40Z".into(),
            }))
        );

        let stop = sync_play(r#"{"Command":"Stop"}"#);
        assert!(matches!(
            super::classify_text(&stop),
            Inbound::SyncPlayGroupUpdate(SyncPlayGroupUpdate::PlaybackCommand(
                SyncPlayPlaybackCommand {
                    action: SyncPlayPlaybackAction::Stop,
                    playlist_item_id: None,
                    position_ticks: 0,
                    when,
                    ..
                }
            )) if when.is_empty()
        ));

        let legacy =
            sync_play(r#"{"Command":"Pause","PositionTicks":0,"When":"1970-01-01T00:16:40Z"}"#);
        assert!(matches!(
            super::classify_text(&legacy),
            Inbound::SyncPlay(_)
        ));
        for malformed in [
            sync_play(
                r#"{"Command":"Pause","PlaylistItemId":"invalid","PositionTicks":0,"When":"1970-01-01T00:16:40Z"}"#,
            ),
            sync_play(
                r#"{"Command":"Stop","PlaylistItemId":"bad","PositionTicks":0,"When":"1970-01-01T00:16:40Z"}"#,
            ),
            sync_play(
                r#"{"Command":"Pause","PlaylistItemId":"3f2504e0-4f89-11d3-9a0c-0305e82c3302","PositionTicks":-1,"When":"1970-01-01T00:16:40Z"}"#,
            ),
        ] {
            assert!(matches!(super::classify_text(&malformed), Inbound::Ignored));
        }
    }

    #[test]
    fn pause_carries_its_position_and_server_time() {
        let text = sync_play(
            r#"{"GroupId":"g1","Command":"Pause","PositionTicks":600000000,"When":"1970-01-01T00:16:40.0000000Z"}"#,
        );
        assert_eq!(
            parse_sync_play(&text),
            Some(Ok(SyncPlayMessage {
                command: SyncPlayWireCommand::Pause,
                position_ticks: 600_000_000,
                when: "1970-01-01T00:16:40.0000000Z".into(),
            }))
        );
    }

    #[test]
    fn unpause_and_seek_are_read_the_same_way() {
        let unpause = sync_play(r#"{"Command":"Unpause","PositionTicks":0,"When":"x"}"#);
        assert!(matches!(
            parse_sync_play(&unpause),
            Some(Ok(SyncPlayMessage {
                command: SyncPlayWireCommand::Unpause,
                ..
            }))
        ));
        let seek = sync_play(r#"{"Command":"Seek","PositionTicks":10000,"When":"x"}"#);
        assert!(matches!(
            parse_sync_play(&seek),
            Some(Ok(SyncPlayMessage {
                command: SyncPlayWireCommand::Seek,
                position_ticks: 10_000,
                ..
            }))
        ));
    }

    #[test]
    fn other_message_types_are_not_sync_play() {
        let playstate = r#"{"MessageType":"Playstate","Data":{"Command":"Pause"}}"#;
        assert_eq!(parse_sync_play(playstate), None);
        assert_eq!(parse_sync_play("not json"), None);
    }

    #[test]
    fn unsupported_sync_play_commands_are_refused_not_guessed() {
        let stop = sync_play(r#"{"Command":"Stop","PositionTicks":0,"When":"x"}"#);
        assert_eq!(
            parse_sync_play(&stop),
            Some(Err(RemoteCommandError::UnsupportedCommand))
        );
    }

    #[test]
    fn missing_or_negative_position_or_time_is_an_invalid_payload() {
        for data in [
            r#"{"Command":"Pause","When":"x"}"#,
            r#"{"Command":"Pause","PositionTicks":null,"When":"x"}"#,
            r#"{"Command":"Pause","PositionTicks":-1,"When":"x"}"#,
            r#"{"Command":"Pause","PositionTicks":0}"#,
            r#"{"Command":"Pause","PositionTicks":0,"When":""}"#,
            r#"{"PositionTicks":0,"When":"x"}"#,
        ] {
            assert_eq!(
                parse_sync_play(&sync_play(data)),
                Some(Err(RemoteCommandError::InvalidPayload)),
                "{data}"
            );
        }
    }

    #[test]
    fn address_attempts_get_the_whole_remaining_shared_deadline_budget() {
        let start = Instant::now();
        let clock = TestClock::new(start);
        let deadline = start + Duration::from_secs(10);
        let mut budgets = Vec::new();

        let result = connect_addresses_with::<_, _, ()>(
            [test_address(1), test_address(2)],
            deadline,
            &clock,
            |_address, remaining| {
                budgets.push(remaining);
                let elapsed = if budgets.len() == 1 {
                    Duration::from_secs(7)
                } else {
                    remaining
                };
                clock.advance(elapsed);
                Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "injected connect timeout",
                ))
            },
        );

        assert!(matches!(result, Err(WebSocketError::SetupTimeout)));
        assert_eq!(budgets, [Duration::from_secs(10), Duration::from_secs(3)]);
    }

    #[test]
    fn a_connection_returned_after_the_deadline_is_dropped() {
        struct DropSignal(Arc<AtomicBool>);

        impl Drop for DropSignal {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Release);
            }
        }

        let start = Instant::now();
        let clock = TestClock::new(start);
        let deadline = start + Duration::from_secs(10);
        let dropped = Arc::new(AtomicBool::new(false));
        let result = connect_addresses_with::<_, _, DropSignal>(
            [test_address(1)],
            deadline,
            &clock,
            |_address, _remaining| {
                clock.advance(Duration::from_secs(10));
                Ok(DropSignal(Arc::clone(&dropped)))
            },
        );

        assert!(matches!(result, Err(WebSocketError::SetupTimeout)));
        assert!(dropped.load(Ordering::Acquire));
    }

    #[test]
    fn deadline_stream_reapplies_the_remaining_timeout_before_each_read() {
        use std::io::{Read, Write};

        let listener = TcpListener::bind(test_address(0)).expect("bind loopback");
        let address = listener.local_addr().expect("loopback address");
        let client = TcpStream::connect(address).expect("connect loopback");
        let (mut server, _) = listener.accept().expect("accept loopback");
        server.write_all(b"ab").expect("write loopback data");

        let start = Instant::now();
        let clock = TestClock::new(start);
        let deadline = start + Duration::from_secs(10);
        let mut stream =
            DeadlineStream::new(client, deadline, clock, Arc::new(AtomicBool::new(false)));
        let mut byte = [0_u8; 1];

        stream.read_exact(&mut byte).expect("first byte");
        assert_eq!(byte, [b'a']);
        assert_eq!(
            stream.stream.read_timeout().expect("read timeout"),
            Some(Duration::from_millis(100))
        );

        stream.clock.advance(Duration::from_secs(4));
        stream.read_exact(&mut byte).expect("second byte");
        assert_eq!(byte, [b'b']);
        assert_eq!(
            stream.stream.read_timeout().expect("read timeout"),
            Some(Duration::from_millis(100))
        );

        stream.clock.advance(Duration::from_secs(6));
        assert_eq!(
            stream.read(&mut byte).expect_err("expired read").kind(),
            std::io::ErrorKind::TimedOut
        );
    }

    #[test]
    fn resolver_has_one_active_job_one_queued_job_and_a_typed_busy_error() {
        let (sender, worker) = local_resolver_worker();
        let clock = Arc::new(TestClock::new(Instant::now()));
        let deadline = clock.now() + Duration::from_secs(30);
        let address = test_address(1234);
        let (started_sender, started_receiver) = mpsc::channel();
        let (release_sender, release_receiver) = mpsc::channel();

        let active = submit_resolution(
            &sender,
            deadline,
            clock_handle(&clock),
            Arc::new(AtomicBool::new(false)),
            Box::new(move || {
                started_sender
                    .send(())
                    .map_err(|_| std::io::Error::other("start signal closed"))?;
                release_receiver
                    .recv()
                    .map_err(|_| std::io::Error::other("release signal closed"))?;
                Ok(vec![address])
            }),
        )
        .expect("submit active resolver");
        started_receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("resolver started");

        let queued = submit_resolution(
            &sender,
            deadline,
            clock_handle(&clock),
            Arc::new(AtomicBool::new(false)),
            Box::new(|| Ok(vec![test_address(2345)])),
        )
        .expect("one queued resolver");
        let busy = submit_resolution(
            &sender,
            deadline,
            clock_handle(&clock),
            Arc::new(AtomicBool::new(false)),
            Box::new(|| Ok(Vec::new())),
        );
        assert!(matches!(busy, Err(WebSocketError::ResolverBusy)));

        release_sender.send(()).expect("release resolver");
        assert_eq!(active.wait().expect("active result"), vec![address]);
        assert_eq!(
            queued.wait().expect("queued result"),
            vec![test_address(2345)]
        );
        drop(active);
        drop(queued);
        drop(sender);
        worker.join().expect("resolver worker exit");
    }

    #[test]
    fn resolver_waiter_returns_promptly_when_session_setup_is_cancelled() {
        let (sender, worker) = local_resolver_worker();
        let deadline = Instant::now() + Duration::from_secs(30);
        let stop = Arc::new(AtomicBool::new(false));
        let (started_sender, started_receiver) = mpsc::channel();
        let (release_sender, release_receiver) = mpsc::channel();
        let waiter = submit_resolution(
            &sender,
            deadline,
            Arc::new(SystemClock),
            Arc::clone(&stop),
            Box::new(move || {
                started_sender
                    .send(())
                    .map_err(|_| std::io::Error::other("start signal closed"))?;
                release_receiver
                    .recv()
                    .map_err(|_| std::io::Error::other("release signal closed"))?;
                Ok(vec![test_address(5678)])
            }),
        )
        .expect("submit resolver");
        started_receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("resolver started");

        stop.store(true, Ordering::Release);
        assert!(matches!(waiter.wait(), Err(WebSocketError::SetupCancelled)));
        release_sender.send(()).expect("release resolver");
        drop(waiter);
        drop(sender);
        worker.join().expect("resolver worker exit");
    }

    #[test]
    fn resolver_discards_late_results_and_skips_expired_queued_jobs() {
        let (sender, worker) = local_resolver_worker();
        let start = Instant::now();
        let clock = Arc::new(TestClock::new(start));
        let active_deadline = start + Duration::from_secs(5);
        let queued_deadline = start + Duration::from_secs(3);
        let (started_sender, started_receiver) = mpsc::channel();
        let (release_sender, release_receiver) = mpsc::channel();
        let queued_resolver_called = Arc::new(AtomicBool::new(false));
        let active = submit_resolution(
            &sender,
            active_deadline,
            clock_handle(&clock),
            Arc::new(AtomicBool::new(false)),
            Box::new(move || {
                started_sender
                    .send(())
                    .map_err(|_| std::io::Error::other("start signal closed"))?;
                release_receiver
                    .recv()
                    .map_err(|_| std::io::Error::other("release signal closed"))?;
                Ok(vec![test_address(3456)])
            }),
        )
        .expect("submit active resolver");
        started_receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("resolver started");

        let queued_called = Arc::clone(&queued_resolver_called);
        let queued = submit_resolution(
            &sender,
            queued_deadline,
            clock_handle(&clock),
            Arc::new(AtomicBool::new(false)),
            Box::new(move || {
                queued_called.store(true, Ordering::Release);
                Ok(vec![test_address(4567)])
            }),
        )
        .expect("queue resolver");

        clock.advance(Duration::from_secs(6));
        assert!(matches!(active.wait(), Err(WebSocketError::SetupTimeout)));
        drop(active);
        release_sender.send(()).expect("release resolver");
        drop(sender);
        worker.join().expect("resolver worker exit");

        assert!(!queued_resolver_called.load(Ordering::Acquire));
        drop(queued);
    }
}
