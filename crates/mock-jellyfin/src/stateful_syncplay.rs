//! Stateful SyncPlay loopback server for multi-client transport tests.
//!
//! This fixture models only the SyncPlay REST routes and session socket messages used by the
//! Rust client. It is not a Jellyfin server and cannot establish compatibility with one.
//! HTTP and WebSocket use separate loopback ports to reuse the existing synchronous transport.

use std::{
    collections::HashMap,
    io::{self, ErrorKind},
    net::SocketAddr,
    net::{TcpListener as SyncListener, TcpStream},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{SyncSender, TryRecvError, sync_channel},
    },
    thread::{self, JoinHandle as ThreadJoinHandle},
    time::Duration,
};

use axum::{
    Router,
    body::Bytes,
    extract::{DefaultBodyLimit, Json, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::{
    net::TcpListener,
    sync::{Mutex, Notify, oneshot},
    task::JoinHandle,
    time::timeout,
};
use tungstenite::{
    Error as WebSocketError, Message as WebSocketMessage, accept_hdr_with_config,
    handshake::server::{Request as WebSocketRequest, Response as WebSocketResponse},
    protocol::WebSocketConfig,
};

const GROUP_ID: &str = "3f2504e0-4f89-11d3-9a0c-0305e82c3301";
const LAST_UPDATED: &str = "2026-11-14T20:00:00.0000000Z";
const MAX_BODY_BYTES: usize = 64 * 1024;
const MAX_GROUP_NAME_UTF16_UNITS: usize = 200;
const MAX_GROUP_PARTICIPANTS: usize = 8;
const MAX_QUEUE_ITEMS: usize = 32;
const MAX_EVENT_HISTORY: usize = 512;
const MAX_ACTIVE_SOCKET_CONNECTIONS: usize = 16;
const MAX_UPGRADE_DELAY: Duration = Duration::from_secs(2);

/// Ordered operations observed over loopback HTTP or the session WebSocket.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncPlayTransportOperation {
    SocketConnected,
    SocketDisconnected,
    GroupListed,
    GroupCreated,
    GroupJoined,
    QueueChanged,
    Ready,
    Buffering,
    Paused,
    Unpaused,
    Seeked,
    Left,
}

/// Secret-free record of one transport operation accepted by this fixture.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyncPlayTransportEvent {
    pub sequence: u64,
    pub client_id: String,
    pub operation: SyncPlayTransportOperation,
    pub detail: Option<i64>,
}

/// A stateful mock that binds only to loopback and supports one shared SyncPlay group.
pub struct StatefulSyncPlayServer {
    addr: SocketAddr,
    socket_addr: SocketAddr,
    state: Arc<AppState>,
    socket_stop: Arc<AtomicBool>,
    socket_task: Option<ThreadJoinHandle<()>>,
    shutdown_tx: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<()>>,
}

impl StatefulSyncPlayServer {
    /// Start the fixture with an immediate WebSocket upgrade.
    pub async fn spawn() -> io::Result<Self> {
        Self::spawn_with_upgrade_delay(Duration::ZERO).await
    }

    /// Start the fixture with a bounded delay before the WebSocket upgrade response.
    /// This lets tests exercise cancellation while the client is still negotiating.
    pub async fn spawn_with_upgrade_delay(delay: Duration) -> io::Result<Self> {
        if delay > MAX_UPGRADE_DELAY {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "WebSocket upgrade delay exceeds the fixture limit",
            ));
        }

        let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
        let addr = listener.local_addr()?;
        let socket_listener = SyncListener::bind(("127.0.0.1", 0))?;
        socket_listener.set_nonblocking(true)?;
        let socket_addr = socket_listener.local_addr()?;
        let state = Arc::new(AppState {
            data: Mutex::new(FixtureData::default()),
            events_changed: Notify::new(),
            upgrade_delay: delay,
        });
        let socket_stop = Arc::new(AtomicBool::new(false));
        let socket_task = spawn_socket_server(
            socket_listener,
            Arc::clone(&state),
            Arc::clone(&socket_stop),
        )?;
        let app = router(Arc::clone(&state));
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                })
                .await;
        });

        Ok(Self {
            addr,
            socket_addr,
            state,
            socket_stop,
            socket_task: Some(socket_task),
            shutdown_tx: Some(shutdown_tx),
            task: Some(task),
        })
    }

    /// Loopback HTTP base URL without a trailing slash.
    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Loopback URL used by the client's session-socket transport; it shares this fixture's state.
    pub fn socket_base_url(&self) -> String {
        format!("http://{}", self.socket_addr)
    }

    /// A snapshot of the bounded, secret-free event history.
    pub async fn events(&self) -> Vec<SyncPlayTransportEvent> {
        self.state.data.lock().await.events.clone()
    }

    /// Wait until at least `minimum` events have been recorded, with a caller-supplied bound.
    pub async fn wait_for_events(&self, minimum: usize, bound: Duration) -> bool {
        timeout(bound, async {
            loop {
                let changed = self.state.events_changed.notified();
                if self.state.data.lock().await.events.len() >= minimum {
                    return;
                }
                changed.await;
            }
        })
        .await
        .is_ok()
    }

    /// Request graceful shutdown and wait for the loopback listener to exit.
    pub async fn shutdown(mut self) {
        self.socket_stop.store(true, Ordering::Release);
        if let Some(task) = self.socket_task.take() {
            let _ = tokio::task::spawn_blocking(move || task.join()).await;
        }
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

impl Drop for StatefulSyncPlayServer {
    fn drop(&mut self) {
        self.socket_stop.store(true, Ordering::Release);
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
    }
}

fn spawn_socket_server(
    listener: SyncListener,
    state: Arc<AppState>,
    stopping: Arc<AtomicBool>,
) -> io::Result<ThreadJoinHandle<()>> {
    thread::Builder::new()
        .name("mock-syncplay-socket".to_owned())
        .spawn(move || socket_accept_loop(listener, state, stopping))
}

fn socket_accept_loop(listener: SyncListener, state: Arc<AppState>, stopping: Arc<AtomicBool>) {
    let mut clients = Vec::<ThreadJoinHandle<()>>::new();
    while !stopping.load(Ordering::Acquire) {
        let mut index = 0;
        while index < clients.len() {
            if clients[index].is_finished() {
                let finished = clients.swap_remove(index);
                let _ = finished.join();
            } else {
                index += 1;
            }
        }

        match listener.accept() {
            Ok((stream, _)) if clients.len() < MAX_ACTIVE_SOCKET_CONNECTIONS => {
                let client_state = Arc::clone(&state);
                let client_stopping = Arc::clone(&stopping);
                if let Ok(client) = thread::Builder::new()
                    .name("mock-syncplay-client".to_owned())
                    .spawn(move || socket_connection(stream, client_state, client_stopping))
                {
                    clients.push(client);
                }
            }
            Ok((_stream, _)) => {}
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(10));
            }
            Err(_) => break,
        }
    }

    for client in clients {
        let _ = client.join();
    }
}

fn socket_connection(stream: TcpStream, state: Arc<AppState>, stopping: Arc<AtomicBool>) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
    let _ = stream.set_write_timeout(Some(Duration::from_millis(200)));
    if !state.upgrade_delay.is_zero() {
        thread::sleep(state.upgrade_delay);
    }
    if stopping.load(Ordering::Acquire) {
        return;
    }

    let mut client_id = None;
    // Tungstenite fixes the callback error to an HTTP response; this closure cannot shrink it.
    #[allow(clippy::result_large_err)]
    let callback = |request: &WebSocketRequest, response: WebSocketResponse| {
        if request.uri().path() == "/socket" {
            client_id = request
                .headers()
                .get("authorization")
                .and_then(|value| value.to_str().ok())
                .and_then(parse_client_authorization);
        }
        Ok(response)
    };
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_BODY_BYTES))
        .max_frame_size(Some(MAX_BODY_BYTES));
    let Ok(mut socket) = accept_hdr_with_config(stream, callback, Some(config)) else {
        return;
    };
    let Some(client_id) = client_id else {
        let _ = socket.close(None);
        return;
    };

    if socket
        .get_mut()
        .set_read_timeout(Some(Duration::from_millis(20)))
        .is_err()
    {
        return;
    }

    let (connection_id, sender, receiver, snapshot) = {
        let mut data = state.data.blocking_lock();
        data.next_socket_id = data.next_socket_id.saturating_add(1);
        let connection_id = data.next_socket_id;
        let (sender, receiver) = sync_channel(32);
        data.sockets.insert(
            client_id.clone(),
            SocketConnection {
                id: connection_id,
                sender: sender.clone(),
            },
        );
        record_event(
            &state,
            &mut data,
            &client_id,
            SyncPlayTransportOperation::SocketConnected,
            None,
        );
        let snapshot = data
            .group
            .as_ref()
            .filter(|group| group.participants.iter().any(|id| id == &client_id))
            .map(|group| group_update("GroupUpdate", group.dto()));
        (connection_id, sender, receiver, snapshot)
    };
    if let Some(snapshot) = snapshot {
        let _ = sender.try_send(snapshot);
    }

    'connected: loop {
        if stopping.load(Ordering::Acquire) {
            break;
        }
        loop {
            match receiver.try_recv() {
                Ok(outbound) => {
                    if socket
                        .send(WebSocketMessage::Text(outbound.into()))
                        .is_err()
                    {
                        break 'connected;
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => break 'connected,
            }
        }
        match socket.read() {
            Ok(WebSocketMessage::Close(frame)) => {
                let _ = socket.close(frame);
                break;
            }
            Ok(WebSocketMessage::Ping(_)) => {
                if socket.flush().is_err() {
                    break;
                }
            }
            Ok(_) => {}
            Err(WebSocketError::Io(error))
                if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(WebSocketError::ConnectionClosed | WebSocketError::AlreadyClosed) => break,
            Err(_) => break,
        }
    }

    let mut data = state.data.blocking_lock();
    if data
        .sockets
        .get(&client_id)
        .is_some_and(|connection| connection.id == connection_id)
    {
        data.sockets.remove(&client_id);
    }
    record_event(
        &state,
        &mut data,
        &client_id,
        SyncPlayTransportOperation::SocketDisconnected,
        None,
    );
}

struct AppState {
    data: Mutex<FixtureData>,
    events_changed: Notify,
    upgrade_delay: Duration,
}

#[derive(Default)]
struct FixtureData {
    group: Option<Group>,
    sockets: HashMap<String, SocketConnection>,
    next_socket_id: u64,
    next_event_sequence: u64,
    events: Vec<SyncPlayTransportEvent>,
}

struct SocketConnection {
    id: u64,
    sender: SyncSender<String>,
}

struct Group {
    name: String,
    participants: Vec<String>,
    state: &'static str,
    queue: Vec<QueueEntry>,
    playing_item_index: usize,
    position_ticks: i64,
    ready: HashMap<String, bool>,
}

struct QueueEntry {
    item_id: String,
    playlist_item_id: String,
}

impl Group {
    fn dto(&self) -> Value {
        json!({
            "GroupId": GROUP_ID,
            "GroupName": &self.name,
            "State": self.state,
            "Participants": &self.participants,
            "LastUpdatedAt": LAST_UPDATED,
        })
    }

    fn current_entry(&self) -> Option<&QueueEntry> {
        self.queue.get(self.playing_item_index)
    }
}

fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/SyncPlay/List", get(list_groups))
        .route("/SyncPlay/New", post(new_group))
        .route("/SyncPlay/Join", post(join_group))
        .route("/SyncPlay/Leave", post(leave_group))
        .route("/SyncPlay/SetNewQueue", post(set_new_queue))
        .route("/SyncPlay/Ready", post(ready))
        .route("/SyncPlay/Buffering", post(buffering))
        .route("/SyncPlay/Pause", post(pause))
        .route("/SyncPlay/Unpause", post(unpause))
        .route("/SyncPlay/Seek", post(seek))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .with_state(state)
}

fn parse_client_id(headers: &HeaderMap) -> Option<String> {
    let authorization = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    parse_client_authorization(authorization)
}

fn parse_client_authorization(authorization: &str) -> Option<String> {
    if !authorization.starts_with("MediaBrowser ") || !authorization.contains("Token=\"") {
        return None;
    }
    let value = authorization
        .split_once("DeviceId=\"")?
        .1
        .split_once('"')?
        .0;
    if value.is_empty() || value.len() > 128 || value.chars().any(char::is_control) {
        return None;
    }
    Some(value.to_owned())
}

fn record_event(
    state: &AppState,
    data: &mut FixtureData,
    client_id: &str,
    operation: SyncPlayTransportOperation,
    detail: Option<i64>,
) {
    data.next_event_sequence = data.next_event_sequence.saturating_add(1);
    data.events.push(SyncPlayTransportEvent {
        sequence: data.next_event_sequence,
        client_id: client_id.to_owned(),
        operation,
        detail,
    });
    if data.events.len() > MAX_EVENT_HISTORY {
        data.events.remove(0);
    }
    state.events_changed.notify_one();
}

fn parse_body(body: &Bytes) -> Option<Value> {
    if body.len() > MAX_BODY_BYTES {
        return None;
    }
    serde_json::from_slice(body).ok()
}

fn is_guid(value: &str) -> bool {
    let bytes = value.as_bytes();
    (bytes.len() == 36
        && bytes.iter().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => *byte == b'-',
            _ => byte.is_ascii_hexdigit(),
        }))
        || (bytes.len() == 32 && bytes.iter().all(u8::is_ascii_hexdigit))
}

fn unauthorized() -> Response {
    StatusCode::UNAUTHORIZED.into_response()
}

fn no_content() -> Response {
    StatusCode::NO_CONTENT.into_response()
}

fn group_update(update_type: &str, data: Value) -> String {
    json!({
        "MessageType": "SyncPlayGroupUpdate",
        "Data": {
            "Type": update_type,
            "GroupId": GROUP_ID,
            "Data": data,
        },
    })
    .to_string()
}

fn broadcast_to_group(data: &FixtureData, message: &str) {
    let Some(group) = data.group.as_ref() else {
        return;
    };
    for participant in &group.participants {
        if let Some(socket) = data.sockets.get(participant) {
            let _ = socket.sender.try_send(message.to_owned());
        }
    }
}

fn broadcast_state(data: &FixtureData, reason: &str) {
    let Some(group) = data.group.as_ref() else {
        return;
    };
    let message = group_update(
        "StateUpdate",
        json!({ "State": group.state, "Reason": reason }),
    );
    broadcast_to_group(data, &message);
}

fn joined_group(data: &FixtureData, client_id: &str) -> bool {
    data.group
        .as_ref()
        .is_some_and(|group| group.participants.iter().any(|id| id == client_id))
}

async fn list_groups(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let Some(client_id) = parse_client_id(&headers) else {
        return unauthorized();
    };
    let mut data = state.data.lock().await;
    record_event(
        &state,
        &mut data,
        &client_id,
        SyncPlayTransportOperation::GroupListed,
        None,
    );
    let groups = data
        .group
        .as_ref()
        .map(Group::dto)
        .into_iter()
        .collect::<Vec<_>>();
    Json(groups).into_response()
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct NewGroupRequest {
    group_name: String,
}

async fn new_group(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some(client_id) = parse_client_id(&headers) else {
        return unauthorized();
    };
    let Some(request) =
        parse_body(&body).and_then(|value| serde_json::from_value::<NewGroupRequest>(value).ok())
    else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    if request.group_name.trim().is_empty()
        || request.group_name.encode_utf16().count() > MAX_GROUP_NAME_UTF16_UNITS
        || request.group_name.chars().any(char::is_control)
    {
        return StatusCode::BAD_REQUEST.into_response();
    }

    let mut data = state.data.lock().await;
    if data.group.is_some() {
        return StatusCode::CONFLICT.into_response();
    }
    data.group = Some(Group {
        name: request.group_name,
        participants: vec![client_id.clone()],
        state: "Idle",
        queue: Vec::new(),
        playing_item_index: 0,
        position_ticks: 0,
        ready: HashMap::new(),
    });
    record_event(
        &state,
        &mut data,
        &client_id,
        SyncPlayTransportOperation::GroupCreated,
        None,
    );
    let response = data.group.as_ref().map(Group::dto);
    if let Some(group) = response.as_ref() {
        let message = group_update("GroupJoined", group.clone());
        broadcast_to_group(&data, &message);
        Json(group.clone()).into_response()
    } else {
        StatusCode::INTERNAL_SERVER_ERROR.into_response()
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct JoinGroupRequest {
    group_id: String,
}

async fn join_group(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some(client_id) = parse_client_id(&headers) else {
        return unauthorized();
    };
    let Some(request) =
        parse_body(&body).and_then(|value| serde_json::from_value::<JoinGroupRequest>(value).ok())
    else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    if request.group_id != GROUP_ID {
        return StatusCode::NOT_FOUND.into_response();
    }

    let mut data = state.data.lock().await;
    let Some(group) = data.group.as_mut() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if !group.participants.iter().any(|id| id == &client_id) {
        if group.participants.len() >= MAX_GROUP_PARTICIPANTS {
            return StatusCode::CONFLICT.into_response();
        }
        group.participants.push(client_id.clone());
    }
    record_event(
        &state,
        &mut data,
        &client_id,
        SyncPlayTransportOperation::GroupJoined,
        None,
    );
    let message = group_update("UserJoined", json!(client_id));
    broadcast_to_group(&data, &message);
    no_content()
}

async fn leave_group(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let Some(client_id) = parse_client_id(&headers) else {
        return unauthorized();
    };
    let mut data = state.data.lock().await;
    let Some(group) = data.group.as_mut() else {
        return no_content();
    };
    if !group.participants.iter().any(|id| id == &client_id) {
        return no_content();
    }
    group.participants.retain(|id| id != &client_id);
    group.ready.remove(&client_id);
    record_event(
        &state,
        &mut data,
        &client_id,
        SyncPlayTransportOperation::Left,
        None,
    );
    let message = group_update("UserLeft", json!(client_id));
    broadcast_to_group(&data, &message);
    no_content()
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct SetQueueRequest {
    playing_queue: Vec<String>,
    playing_item_position: i64,
    start_position_ticks: i64,
}

fn playlist_entry_id(index: usize) -> String {
    format!("00000000-0000-0000-0000-{index:012x}", index = index + 1)
}

async fn set_new_queue(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some(client_id) = parse_client_id(&headers) else {
        return unauthorized();
    };
    let Some(request) =
        parse_body(&body).and_then(|value| serde_json::from_value::<SetQueueRequest>(value).ok())
    else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let Ok(position) = usize::try_from(request.playing_item_position) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    if request.playing_queue.is_empty()
        || request.playing_queue.len() > MAX_QUEUE_ITEMS
        || position >= request.playing_queue.len()
        || request.start_position_ticks < 0
        || request.playing_queue.iter().any(|item| !is_guid(item))
    {
        return StatusCode::BAD_REQUEST.into_response();
    }

    let mut data = state.data.lock().await;
    if !joined_group(&data, &client_id) {
        return StatusCode::CONFLICT.into_response();
    }
    let Some(group) = data.group.as_mut() else {
        return StatusCode::CONFLICT.into_response();
    };
    group.queue = request
        .playing_queue
        .into_iter()
        .enumerate()
        .map(|(index, item_id)| QueueEntry {
            item_id,
            playlist_item_id: playlist_entry_id(index),
        })
        .collect();
    group.playing_item_index = position;
    group.position_ticks = request.start_position_ticks;
    group.ready.clear();
    group.state = "Waiting";
    let detail = i64::try_from(group.queue.len()).ok();
    record_event(
        &state,
        &mut data,
        &client_id,
        SyncPlayTransportOperation::QueueChanged,
        detail,
    );
    let Some(group) = data.group.as_ref() else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let queue_message = group_update(
        "PlayQueue",
        json!({
            "Reason": "NewPlaylist",
            "LastUpdate": LAST_UPDATED,
            "Playlist": group.queue.iter().map(|entry| json!({
                "ItemId": &entry.item_id,
                "PlaylistItemId": &entry.playlist_item_id,
            })).collect::<Vec<_>>(),
            "PlayingItemIndex": group.playing_item_index,
            "StartPositionTicks": group.position_ticks,
            "IsPlaying": false,
        }),
    );
    broadcast_to_group(&data, &queue_message);
    broadcast_state(&data, "NewPlaylist");
    no_content()
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct PlaybackStatusRequest {
    when: String,
    position_ticks: i64,
    is_playing: bool,
    playlist_item_id: String,
}

fn decode_status(body: &Bytes) -> Option<PlaybackStatusRequest> {
    let value = parse_body(body)?;
    let status = serde_json::from_value::<PlaybackStatusRequest>(value).ok()?;
    (status.when.ends_with('Z')
        && !status.when.is_empty()
        && status.when.len() <= 64
        && status.position_ticks >= 0
        && is_guid(&status.playlist_item_id))
    .then_some(status)
}

async fn playback_status(
    state: Arc<AppState>,
    client_id: String,
    body: Bytes,
    buffering: bool,
) -> Response {
    let Some(status) = decode_status(&body) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let mut data = state.data.lock().await;
    if !joined_group(&data, &client_id) {
        return StatusCode::CONFLICT.into_response();
    }
    let Some(group) = data.group.as_mut() else {
        return StatusCode::CONFLICT.into_response();
    };
    let valid_entry = group
        .current_entry()
        .is_some_and(|entry| entry.playlist_item_id == status.playlist_item_id);
    if !valid_entry {
        return StatusCode::CONFLICT.into_response();
    }
    if buffering {
        group.ready.insert(client_id.clone(), false);
        group.state = "Waiting";
    } else {
        group.ready.insert(client_id.clone(), status.is_playing);
        group.state = if group
            .participants
            .iter()
            .all(|participant| group.ready.get(participant).copied().unwrap_or(false))
        {
            "Playing"
        } else {
            "Waiting"
        };
    }
    group.position_ticks = status.position_ticks;
    let position_ticks = status.position_ticks;
    let operation = if buffering {
        SyncPlayTransportOperation::Buffering
    } else {
        SyncPlayTransportOperation::Ready
    };
    record_event(
        &state,
        &mut data,
        &client_id,
        operation,
        Some(position_ticks),
    );
    broadcast_state(&data, if buffering { "Buffering" } else { "Ready" });
    no_content()
}

async fn ready(State(state): State<Arc<AppState>>, headers: HeaderMap, body: Bytes) -> Response {
    let Some(client_id) = parse_client_id(&headers) else {
        return unauthorized();
    };
    playback_status(state, client_id, body, false).await
}

async fn buffering(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some(client_id) = parse_client_id(&headers) else {
        return unauthorized();
    };
    playback_status(state, client_id, body, true).await
}

#[derive(Clone, Copy)]
enum PlaybackCommandKind {
    Pause,
    Unpause,
}

async fn playback_command(
    state: Arc<AppState>,
    client_id: String,
    command: PlaybackCommandKind,
) -> Response {
    let mut data = state.data.lock().await;
    if !joined_group(&data, &client_id) {
        return StatusCode::CONFLICT.into_response();
    }
    let Some(group) = data.group.as_mut() else {
        return StatusCode::CONFLICT.into_response();
    };
    let (operation, reason) = match command {
        PlaybackCommandKind::Pause => {
            group.state = "Paused";
            (SyncPlayTransportOperation::Paused, "Pause")
        }
        PlaybackCommandKind::Unpause => {
            group.state = "Playing";
            (SyncPlayTransportOperation::Unpaused, "Unpause")
        }
    };
    let detail = group.position_ticks;
    record_event(&state, &mut data, &client_id, operation, Some(detail));
    broadcast_state(&data, reason);
    let Some(group) = data.group.as_ref() else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let Some(entry) = group.current_entry() else {
        return no_content();
    };
    let command_name = match command {
        PlaybackCommandKind::Pause => "Pause",
        PlaybackCommandKind::Unpause => "Unpause",
    };
    let message = json!({
        "MessageType": "SyncPlayCommand",
        "Data": {
            "Command": command_name,
            "GroupId": GROUP_ID,
            "PlaylistItemId": &entry.playlist_item_id,
            "PositionTicks": group.position_ticks,
            "When": LAST_UPDATED,
        },
    })
    .to_string();
    broadcast_to_group(&data, &message);
    no_content()
}

async fn pause(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let Some(client_id) = parse_client_id(&headers) else {
        return unauthorized();
    };
    playback_command(state, client_id, PlaybackCommandKind::Pause).await
}

async fn unpause(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let Some(client_id) = parse_client_id(&headers) else {
        return unauthorized();
    };
    playback_command(state, client_id, PlaybackCommandKind::Unpause).await
}

async fn seek(State(state): State<Arc<AppState>>, headers: HeaderMap, body: Bytes) -> Response {
    let Some(client_id) = parse_client_id(&headers) else {
        return unauthorized();
    };
    let Some(position_ticks) = parse_body(&body)
        .and_then(|value| value.get("PositionTicks").and_then(Value::as_i64))
        .filter(|ticks| *ticks >= 0)
    else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let mut data = state.data.lock().await;
    if !joined_group(&data, &client_id) {
        return StatusCode::CONFLICT.into_response();
    }
    let Some(group) = data.group.as_mut() else {
        return StatusCode::CONFLICT.into_response();
    };
    group.position_ticks = position_ticks;
    record_event(
        &state,
        &mut data,
        &client_id,
        SyncPlayTransportOperation::Seeked,
        Some(position_ticks),
    );
    let Some(group) = data.group.as_ref() else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let Some(entry) = group.current_entry() else {
        return no_content();
    };
    let message = json!({
        "MessageType": "SyncPlayCommand",
        "Data": {
            "Command": "Seek",
            "GroupId": GROUP_ID,
            "PlaylistItemId": &entry.playlist_item_id,
            "PositionTicks": group.position_ticks,
            "When": LAST_UPDATED,
        },
    })
    .to_string();
    broadcast_to_group(&data, &message);
    no_content()
}
