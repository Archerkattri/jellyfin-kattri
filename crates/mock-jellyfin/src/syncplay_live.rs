//! Opt-in stateful SyncPlay group simulator for the mock server.
//!
//! The default mock (`syncplay.rs`) is a stateless fixture. This module is enabled only with
//! `MockConfig::syncplay_live` and implements the group protocol the Kattri client parses in
//! `jellyfin-api::websocket`: a session WebSocket at `/socket`, `SyncPlayGroupUpdate` notifications
//! (GroupJoined / UserJoined / StateUpdate / PlayQueue) and typed `SyncPlayCommand` playback commands
//! carrying a playlist-entry identity, a position anchor and a `When` timestamp. It also serves
//! `GET /GetUtcTime` for the client's server-clock sampler.
//!
//! This is a protocol-faithful test double, NOT a real Jellyfin server. The group position is a
//! wall-clock anchor set by playback commands; the simulator does not require client position
//! reports to keep members aligned, because every member receives the same anchor and the client's
//! own drift controller corrects toward it. It is used to measure two-client synchronization in an
//! isolated loopback run with no production-server writes.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::{
    Router,
    body::Bytes,
    extract::{
        RawQuery, State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::{AppState, is_authorized, json_ok, record_request, unauthorized};

/// Jellyfin ticks (100 ns) per millisecond.
const TICKS_PER_MS: i64 = 10_000;
/// Lead time between a command being issued and the moment its position anchor applies. Gives each
/// member a brief window to prepare before the group position takes effect.
const COMMAND_DELAY_MS: i64 = 0;

/// One shared queue entry. `playlist_item_id` is the per-entry identity the client validates.
#[derive(Clone)]
struct QueueEntry {
    item_id: String,
    playlist_item_id: String,
}

/// A simulated group. The position anchor is `position_ticks` at `when_ms`; while `is_playing` the
/// group position advances with wall clock time from that anchor.
struct Group {
    name: String,
    participants: Vec<String>,
    state: &'static str,
    queue: Vec<QueueEntry>,
    playing_index: i32,
    position_ticks: i64,
    when_ms: i64,
    is_playing: bool,
    last_update_ms: i64,
}

/// Shared live state: groups plus one outbound channel per connected session device.
pub(crate) struct LiveState {
    inner: Mutex<Inner>,
    guid_counter: AtomicU64,
}

struct Inner {
    groups: HashMap<String, Group>,
    connections: HashMap<String, mpsc::UnboundedSender<String>>,
}

impl LiveState {
    pub(crate) fn new() -> Self {
        Self {
            inner: Mutex::new(Inner {
                groups: HashMap::new(),
                connections: HashMap::new(),
            }),
            guid_counter: AtomicU64::new(1),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|error| error.into_inner())
    }

    /// A GUID-shaped identifier (8-4-4-4-12 hex). The client validates the shape, not RFC version
    /// bits, so a counter/time mix is sufficient and keeps the simulator dependency-free.
    fn next_guid(&self) -> String {
        let n = self.guid_counter.fetch_add(1, Ordering::Relaxed);
        let t = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        format!(
            "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
            (t >> 32) as u32,
            (t >> 16) as u16,
            t as u16,
            (n >> 48) as u16,
            n & 0xffff_ffff_ffff
        )
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Group position in ticks at `now`, advancing from the anchor while playing.
fn current_position_ticks(group: &Group, now: i64) -> i64 {
    if group.is_playing {
        let elapsed = (now - group.when_ms).max(0);
        group.position_ticks + elapsed * TICKS_PER_MS
    } else {
        group.position_ticks
    }
}

/// Days since the Unix epoch to a civil (year, month, day). Howard Hinnant's algorithm.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Unix milliseconds to the server UTC form the client parses: `yyyy-MM-ddTHH:mm:ss.fffffffZ`.
fn utc_iso(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let frac_ticks = ms.rem_euclid(1000) * 10_000;
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:07}Z",
        y,
        m,
        d,
        tod / 3600,
        (tod % 3600) / 60,
        tod % 60,
        frac_ticks
    )
}

fn group_json(group_id: &str, group: &Group) -> Value {
    json!({
        "GroupId": group_id,
        "GroupName": group.name,
        "State": group.state,
        "Participants": group.participants,
        "LastUpdatedAt": utc_iso(group.last_update_ms),
    })
}

/// A `SyncPlayGroupUpdate` notification envelope.
fn group_update(group_id: &str, update_type: &str, data: Value) -> String {
    json!({
        "MessageType": "SyncPlayGroupUpdate",
        "Data": { "Type": update_type, "GroupId": group_id, "Data": data },
    })
    .to_string()
}

fn play_queue_update(group_id: &str, group: &Group) -> String {
    let playlist: Vec<Value> = group
        .queue
        .iter()
        .map(|entry| json!({ "ItemId": entry.item_id, "PlaylistItemId": entry.playlist_item_id }))
        .collect();
    group_update(
        group_id,
        "PlayQueue",
        json!({
            "Reason": "SetNewQueue",
            "LastUpdate": utc_iso(group.last_update_ms),
            "Playlist": playlist,
            "PlayingItemIndex": group.playing_index,
            "StartPositionTicks": group.position_ticks,
            "IsPlaying": group.is_playing,
        }),
    )
}

fn state_update(group_id: &str, group: &Group) -> String {
    group_update(
        group_id,
        "StateUpdate",
        json!({ "State": group.state, "Reason": "Command" }),
    )
}

fn active_playlist_item_id(group: &Group) -> Option<String> {
    usize::try_from(group.playing_index)
        .ok()
        .and_then(|index| group.queue.get(index))
        .map(|entry| entry.playlist_item_id.clone())
}

/// A typed `SyncPlayCommand` playback command with queue-entry identity and a position anchor.
fn playback_command(group_id: &str, group: &Group, command: &str, when_ms: i64) -> Option<String> {
    let playlist_item_id = active_playlist_item_id(group)?;
    Some(
        json!({
            "MessageType": "SyncPlayCommand",
            "Data": {
                "Command": command,
                "GroupId": group_id,
                "PlaylistItemId": playlist_item_id,
                "PositionTicks": group.position_ticks,
                "When": utc_iso(when_ms),
            },
        })
        .to_string(),
    )
}

fn stop_command(group_id: &str) -> String {
    json!({
        "MessageType": "SyncPlayCommand",
        "Data": { "Command": "Stop", "GroupId": group_id, "PositionTicks": 0, "When": "" },
    })
    .to_string()
}

fn send_to(inner: &Inner, device: &str, message: String) {
    if let Some(tx) = inner.connections.get(device) {
        let _ = tx.send(message);
    }
}

/// Deliver collected `(device, message)` pairs after the group borrow has ended.
fn deliver(inner: &Inner, messages: Vec<(String, String)>) {
    for (device, message) in messages {
        send_to(inner, &device, message);
    }
}

/// Fan one message out to every participant of a group, as collected pairs.
fn fan_out(group: &Group, message: &str) -> Vec<(String, String)> {
    group
        .participants
        .iter()
        .map(|device| (device.clone(), message.to_owned()))
        .collect()
}

/// The session device id from the MediaBrowser Authorization header. Both clients share the mock
/// token, so the device id is what distinguishes their sockets and group membership.
fn device_id(headers: &HeaderMap) -> String {
    for name in ["x-emby-authorization", "authorization"] {
        if let Some(value) = headers.get(name).and_then(|value| value.to_str().ok()) {
            let lower = value.to_lowercase();
            if let Some(start) = lower.find("deviceid=\"") {
                let rest = &value[start + "deviceid=\"".len()..];
                if let Some(end) = rest.find('"')
                    && end > 0
                {
                    return rest[..end].to_owned();
                }
            }
        }
    }
    "default-device".to_owned()
}

fn parse_body(body: &Bytes) -> Option<Value> {
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

/// Routes for the live simulator. Merged instead of the stateless router when live mode is on.
pub(crate) fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/SyncPlay/List", get(list_groups))
        .route("/SyncPlay/New", post(new_group))
        .route("/SyncPlay/Join", post(join_group))
        .route("/SyncPlay/Leave", post(leave_group))
        .route("/SyncPlay/Pause", post(pause))
        .route("/SyncPlay/Unpause", post(unpause))
        .route("/SyncPlay/Stop", post(stop))
        .route("/SyncPlay/Seek", post(seek))
        .route("/SyncPlay/SetNewQueue", post(set_new_queue))
        .route("/SyncPlay/Ready", post(ready))
        .route("/SyncPlay/Buffering", post(buffering))
        .route("/SyncPlay/Ping", post(ping))
        .route("/GetUtcTime", get(utc_time))
        .route("/socket", get(socket_upgrade))
}

fn live(state: &AppState) -> Option<Arc<LiveState>> {
    state.syncplay_live.clone()
}

async fn list_groups(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    RawQuery(raw_query): RawQuery,
) -> Response {
    record_request(&state, "GET", "/SyncPlay/List", raw_query.as_deref());
    if !is_authorized(&headers) {
        return unauthorized();
    }
    let Some(live) = live(&state) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let inner = live.lock();
    let groups: Vec<Value> = inner
        .groups
        .iter()
        .map(|(id, group)| group_json(id, group))
        .collect();
    json_ok(Value::Array(groups))
}

async fn new_group(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    RawQuery(raw_query): RawQuery,
    body: Bytes,
) -> Response {
    record_request(&state, "POST", "/SyncPlay/New", raw_query.as_deref());
    if !is_authorized(&headers) {
        return unauthorized();
    }
    let Some(live) = live(&state) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some(name) = parse_body(&body)
        .as_ref()
        .and_then(|value| value.get("GroupName"))
        .and_then(Value::as_str)
        .map(str::to_owned)
    else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let device = device_id(&headers);
    let now = now_ms();
    let group_id = live.next_guid();
    let mut inner = live.lock();
    let group = Group {
        name,
        participants: vec![device.clone()],
        state: "Idle",
        queue: Vec::new(),
        playing_index: -1,
        position_ticks: 0,
        when_ms: now,
        is_playing: false,
        last_update_ms: now,
    };
    let response = group_json(&group_id, &group);
    let joined = group_update(&group_id, "GroupJoined", response.clone());
    inner.groups.insert(group_id, group);
    send_to(&inner, &device, joined);
    json_ok(response)
}

async fn join_group(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    RawQuery(raw_query): RawQuery,
    body: Bytes,
) -> Response {
    record_request(&state, "POST", "/SyncPlay/Join", raw_query.as_deref());
    if !is_authorized(&headers) {
        return unauthorized();
    }
    let Some(live) = live(&state) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some(group_id) = parse_body(&body)
        .as_ref()
        .and_then(|value| value.get("GroupId"))
        .and_then(Value::as_str)
        .map(str::to_owned)
    else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let device = device_id(&headers);
    let now = now_ms();
    let mut inner = live.lock();
    let messages = {
        let Some(group) = inner.groups.get_mut(&group_id) else {
            return StatusCode::NOT_FOUND.into_response();
        };
        if !group.participants.contains(&device) {
            group.participants.push(device.clone());
        }
        group.last_update_ms = now;
        let mut out = Vec::new();
        let joined_notification = group_update(&group_id, "UserJoined", json!(device));
        for participant in &group.participants {
            if participant != &device {
                out.push((participant.clone(), joined_notification.clone()));
            }
        }
        // Bring the joining member to the current group position: group snapshot, queue, state, and
        // a playing anchor when the group is already playing.
        out.push((
            device.clone(),
            group_update(&group_id, "GroupJoined", group_json(&group_id, group)),
        ));
        if !group.queue.is_empty() {
            out.push((device.clone(), play_queue_update(&group_id, group)));
        }
        out.push((device.clone(), state_update(&group_id, group)));
        if group.is_playing {
            let when = now + COMMAND_DELAY_MS;
            group.position_ticks = current_position_ticks(group, now);
            group.when_ms = when;
            if let Some(command) = playback_command(&group_id, group, "Unpause", when) {
                out.push((device.clone(), command));
            }
        }
        out
    };
    deliver(&inner, messages);
    StatusCode::NO_CONTENT.into_response()
}

async fn leave_group(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    RawQuery(raw_query): RawQuery,
) -> Response {
    record_request(&state, "POST", "/SyncPlay/Leave", raw_query.as_deref());
    if !is_authorized(&headers) {
        return unauthorized();
    }
    let Some(live) = live(&state) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let device = device_id(&headers);
    let mut inner = live.lock();
    let messages = {
        let mut out = Vec::new();
        let mut emptied = Vec::new();
        for (group_id, group) in inner.groups.iter_mut() {
            if let Some(index) = group.participants.iter().position(|p| p == &device) {
                group.participants.remove(index);
                let notification = group_update(group_id, "UserLeft", json!(device));
                out.extend(fan_out(group, &notification));
                out.push((
                    device.clone(),
                    group_update(group_id, "GroupLeft", json!({})),
                ));
                if group.participants.is_empty() {
                    emptied.push(group_id.clone());
                }
            }
        }
        for group_id in emptied {
            inner.groups.remove(&group_id);
        }
        out
    };
    deliver(&inner, messages);
    StatusCode::NO_CONTENT.into_response()
}

/// Shared handler for Pause and Unpause: anchor the group position and broadcast a typed command.
fn pause_unpause(state: &AppState, headers: &HeaderMap, playing: bool) -> Response {
    let Some(live) = live(state) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let device = device_id(headers);
    let now = now_ms();
    let mut inner = live.lock();
    let messages = {
        let Some((group_id, group)) = inner
            .groups
            .iter_mut()
            .find(|(_, group)| group.participants.contains(&device))
        else {
            return StatusCode::NOT_FOUND.into_response();
        };
        let when = now + COMMAND_DELAY_MS;
        group.position_ticks = current_position_ticks(group, now);
        group.when_ms = when;
        group.is_playing = playing;
        group.state = if playing { "Playing" } else { "Paused" };
        group.last_update_ms = now;
        let command_name = if playing { "Unpause" } else { "Pause" };
        let mut out = Vec::new();
        if let Some(command) = playback_command(group_id, group, command_name, when) {
            out.extend(fan_out(group, &command));
        }
        out.extend(fan_out(group, &state_update(group_id, group)));
        out
    };
    deliver(&inner, messages);
    StatusCode::NO_CONTENT.into_response()
}

async fn pause(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    RawQuery(raw_query): RawQuery,
) -> Response {
    record_request(&state, "POST", "/SyncPlay/Pause", raw_query.as_deref());
    if !is_authorized(&headers) {
        return unauthorized();
    }
    pause_unpause(&state, &headers, false)
}

async fn unpause(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    RawQuery(raw_query): RawQuery,
) -> Response {
    record_request(&state, "POST", "/SyncPlay/Unpause", raw_query.as_deref());
    if !is_authorized(&headers) {
        return unauthorized();
    }
    pause_unpause(&state, &headers, true)
}

async fn stop(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    RawQuery(raw_query): RawQuery,
) -> Response {
    record_request(&state, "POST", "/SyncPlay/Stop", raw_query.as_deref());
    if !is_authorized(&headers) {
        return unauthorized();
    }
    let Some(live) = live(&state) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let device = device_id(&headers);
    let mut inner = live.lock();
    let messages = {
        let Some((group_id, group)) = inner
            .groups
            .iter_mut()
            .find(|(_, group)| group.participants.contains(&device))
        else {
            return StatusCode::NOT_FOUND.into_response();
        };
        group.is_playing = false;
        group.state = "Idle";
        group.position_ticks = 0;
        group.playing_index = -1;
        group.last_update_ms = now_ms();
        let mut out = fan_out(group, &stop_command(group_id));
        out.extend(fan_out(group, &state_update(group_id, group)));
        out
    };
    deliver(&inner, messages);
    StatusCode::NO_CONTENT.into_response()
}

async fn seek(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    RawQuery(raw_query): RawQuery,
    body: Bytes,
) -> Response {
    record_request(&state, "POST", "/SyncPlay/Seek", raw_query.as_deref());
    if !is_authorized(&headers) {
        return unauthorized();
    }
    let Some(live) = live(&state) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some(ticks) = parse_body(&body)
        .as_ref()
        .and_then(|value| value.get("PositionTicks"))
        .and_then(Value::as_i64)
        .filter(|ticks| *ticks >= 0)
    else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let device = device_id(&headers);
    let now = now_ms();
    let mut inner = live.lock();
    let messages = {
        let Some((group_id, group)) = inner
            .groups
            .iter_mut()
            .find(|(_, group)| group.participants.contains(&device))
        else {
            return StatusCode::NOT_FOUND.into_response();
        };
        let when = now + COMMAND_DELAY_MS;
        group.position_ticks = ticks;
        group.when_ms = when;
        group.last_update_ms = now;
        match playback_command(group_id, group, "Seek", when) {
            Some(command) => fan_out(group, &command),
            None => Vec::new(),
        }
    };
    deliver(&inner, messages);
    StatusCode::NO_CONTENT.into_response()
}

async fn set_new_queue(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    RawQuery(raw_query): RawQuery,
    body: Bytes,
) -> Response {
    record_request(
        &state,
        "POST",
        "/SyncPlay/SetNewQueue",
        raw_query.as_deref(),
    );
    if !is_authorized(&headers) {
        return unauthorized();
    }
    let Some(live) = live(&state) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some(value) = parse_body(&body) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let Some(queue) = value.get("PlayingQueue").and_then(Value::as_array) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let Some(position) = value
        .get("PlayingItemPosition")
        .and_then(Value::as_i64)
        .and_then(|index| usize::try_from(index).ok())
    else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let valid_items = !queue.is_empty()
        && position < queue.len()
        && queue.iter().all(|item| item.as_str().is_some_and(is_guid));
    let Some(start_ticks) = value
        .get("StartPositionTicks")
        .and_then(Value::as_i64)
        .filter(|ticks| *ticks >= 0)
    else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    if !valid_items {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let device = device_id(&headers);
    let now = now_ms();
    let mut inner = live.lock();
    let messages = {
        let Some((group_id, group)) = inner
            .groups
            .iter_mut()
            .find(|(_, group)| group.participants.contains(&device))
        else {
            return StatusCode::NOT_FOUND.into_response();
        };
        group.queue = queue
            .iter()
            .filter_map(|item| item.as_str())
            .map(|item_id| QueueEntry {
                item_id: item_id.to_owned(),
                playlist_item_id: live.next_guid(),
            })
            .collect();
        group.playing_index = position as i32;
        group.position_ticks = start_ticks;
        group.when_ms = now;
        group.is_playing = false;
        group.last_update_ms = now;
        fan_out(group, &play_queue_update(group_id, group))
    };
    deliver(&inner, messages);
    StatusCode::NO_CONTENT.into_response()
}

/// Ready and Buffering carry the same bounded playback-status body; the simulator accepts them so
/// the client's readiness handshake completes. Alignment is driven by the shared command anchors.
async fn playback_status(
    state: &AppState,
    headers: &HeaderMap,
    raw_query: Option<&str>,
    body: &Bytes,
    path: &'static str,
) -> Response {
    record_request(state, "POST", path, raw_query);
    if !is_authorized(headers) {
        return unauthorized();
    }
    let Some(value) = parse_body(body) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let valid_when = value
        .get("When")
        .and_then(Value::as_str)
        .is_some_and(|when| !when.is_empty() && when.len() <= 64 && when.ends_with('Z'));
    let valid_position = value
        .get("PositionTicks")
        .and_then(Value::as_i64)
        .is_some_and(|ticks| ticks >= 0);
    let valid_is_playing = value.get("IsPlaying").and_then(Value::as_bool).is_some();
    let valid_playlist_item = value
        .get("PlaylistItemId")
        .and_then(Value::as_str)
        .is_some_and(is_guid);
    if !(valid_when && valid_position && valid_is_playing && valid_playlist_item) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    StatusCode::NO_CONTENT.into_response()
}

async fn ready(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    RawQuery(raw_query): RawQuery,
    body: Bytes,
) -> Response {
    playback_status(
        &state,
        &headers,
        raw_query.as_deref(),
        &body,
        "/SyncPlay/Ready",
    )
    .await
}

async fn buffering(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    RawQuery(raw_query): RawQuery,
    body: Bytes,
) -> Response {
    playback_status(
        &state,
        &headers,
        raw_query.as_deref(),
        &body,
        "/SyncPlay/Buffering",
    )
    .await
}

async fn ping(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    RawQuery(raw_query): RawQuery,
    body: Bytes,
) -> Response {
    record_request(&state, "POST", "/SyncPlay/Ping", raw_query.as_deref());
    if !is_authorized(&headers) {
        return unauthorized();
    }
    let valid = parse_body(&body)
        .as_ref()
        .and_then(|value| value.get("Ping"))
        .and_then(Value::as_i64)
        .is_some();
    if !valid {
        return StatusCode::BAD_REQUEST.into_response();
    }
    StatusCode::NO_CONTENT.into_response()
}

async fn utc_time(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    record_request(&state, "GET", "/GetUtcTime", None);
    if !is_authorized(&headers) {
        return unauthorized();
    }
    let now = utc_iso(now_ms());
    json_ok(json!({ "RequestReceptionTime": now, "ResponseTransmissionTime": now }))
}

async fn socket_upgrade(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    if !is_authorized(&headers) {
        return unauthorized();
    }
    let device = device_id(&headers);
    ws.on_upgrade(move |socket| handle_socket(socket, state, device))
}

async fn handle_socket(socket: WebSocket, state: Arc<AppState>, device: String) {
    let Some(live) = live(&state) else {
        return;
    };
    let (mut sink, mut stream) = socket.split();
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    {
        let mut inner = live.lock();
        inner.connections.insert(device.clone(), tx);
    }
    let send_task = tokio::spawn(async move {
        while let Some(message) = rx.recv().await {
            if sink.send(Message::Text(message.into())).await.is_err() {
                break;
            }
        }
    });
    while let Some(incoming) = stream.next().await {
        match incoming {
            Ok(Message::Close(_)) | Err(_) => break,
            Ok(_) => {}
        }
    }
    let messages = {
        let mut inner = live.lock();
        inner.connections.remove(&device);
        let mut out = Vec::new();
        for (group_id, group) in inner.groups.iter_mut() {
            if let Some(index) = group.participants.iter().position(|p| p == &device) {
                group.participants.remove(index);
                let notification = group_update(group_id, "UserLeft", json!(device));
                out.extend(fan_out(group, &notification));
            }
        }
        inner
            .groups
            .retain(|_, group| !group.participants.is_empty());
        out
    };
    {
        let inner = live.lock();
        deliver(&inner, messages);
    }
    send_task.abort();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_group() -> Group {
        Group {
            name: "g".into(),
            participants: vec!["a".into()],
            state: "Playing",
            queue: vec![QueueEntry {
                item_id: "3f2504e0-4f89-11d3-9a0c-0305e82c3302".into(),
                playlist_item_id: "3f2504e0-4f89-11d3-9a0c-0305e82c3303".into(),
            }],
            playing_index: 0,
            position_ticks: 5_000_000,
            when_ms: 1_000,
            is_playing: true,
            last_update_ms: 1_000,
        }
    }

    #[test]
    fn guid_shape_is_valid() {
        let live = LiveState::new();
        for _ in 0..8 {
            let guid = live.next_guid();
            assert!(is_guid(&guid), "invalid guid shape: {guid}");
        }
    }

    #[test]
    fn utc_iso_matches_the_server_form() {
        assert_eq!(utc_iso(0), "1970-01-01T00:00:00.0000000Z");
        assert_eq!(utc_iso(2_500), "1970-01-01T00:00:02.5000000Z");
        assert_eq!(utc_iso(86_400_000), "1970-01-02T00:00:00.0000000Z");
    }

    #[test]
    fn position_advances_only_while_playing() {
        let mut group = sample_group();
        group.position_ticks = 1_000_000;
        group.when_ms = 1_000;
        assert_eq!(
            current_position_ticks(&group, 2_000),
            1_000_000 + 1_000 * TICKS_PER_MS
        );
        group.is_playing = false;
        assert_eq!(current_position_ticks(&group, 9_000), 1_000_000);
    }

    #[test]
    fn device_id_is_read_from_the_authorization_header() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "authorization",
            "MediaBrowser Client=\"kattri\", Device=\"d\", DeviceId=\"abc-123\", Version=\"1\", Token=\"t\""
                .parse()
                .unwrap(),
        );
        assert_eq!(device_id(&headers), "abc-123");
    }

    #[test]
    fn command_envelope_carries_identity_and_anchor() {
        let group = sample_group();
        let message = playback_command(
            "3f2504e0-4f89-11d3-9a0c-0305e82c3301",
            &group,
            "Unpause",
            1_500,
        )
        .expect("active entry present");
        let value: Value = serde_json::from_str(&message).unwrap();
        assert_eq!(value["MessageType"], "SyncPlayCommand");
        assert_eq!(value["Data"]["Command"], "Unpause");
        assert_eq!(
            value["Data"]["PlaylistItemId"],
            "3f2504e0-4f89-11d3-9a0c-0305e82c3303"
        );
        assert_eq!(value["Data"]["PositionTicks"], 5_000_000);
        assert_eq!(value["Data"]["When"], "1970-01-01T00:00:01.5000000Z");
    }

    #[test]
    fn play_queue_update_lists_entry_identities() {
        let group = sample_group();
        let message = play_queue_update("3f2504e0-4f89-11d3-9a0c-0305e82c3301", &group);
        let value: Value = serde_json::from_str(&message).unwrap();
        assert_eq!(value["MessageType"], "SyncPlayGroupUpdate");
        assert_eq!(value["Data"]["Type"], "PlayQueue");
        assert_eq!(value["Data"]["Data"]["PlayingItemIndex"], 0);
        assert_eq!(
            value["Data"]["Data"]["Playlist"][0]["PlaylistItemId"],
            "3f2504e0-4f89-11d3-9a0c-0305e82c3303"
        );
    }
}
