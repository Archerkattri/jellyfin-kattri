//! Mock SyncPlay routes: `GET /SyncPlay/List`, `POST /SyncPlay/New`, `POST /SyncPlay/Join`,
//! `POST /SyncPlay/Leave`, and the playback messages `POST /SyncPlay/Pause`,
//! `/SyncPlay/Unpause`, `/SyncPlay/Stop`, `/SyncPlay/Seek`, `/SyncPlay/SetNewQueue`,
//! `/SyncPlay/Ready`, `/SyncPlay/Buffering`, and `/SyncPlay/Ping`. This is not a real server.
//!
//! Responses are fixtures. The mock keeps no group state, so a join does not change what
//! the list returns. Only the group in [`FIXTURE_GROUP_ID`] can be joined; other IDs get
//! 404. Malformed bodies get 400, so a client that sends the wrong shape fails in tests.

use std::sync::Arc;

use axum::{
    Router,
    body::Bytes,
    extract::{RawQuery, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde_json::{Value, json};

use crate::{AppState, is_authorized, json_ok, record_request, unauthorized};

/// The only group the mock accepts for join.
const FIXTURE_GROUP_ID: &str = "3f2504e0-4f89-11d3-9a0c-0305e82c3301";
const FIXTURE_GROUP_NAME: &str = "Movie night";
const FIXTURE_LAST_UPDATED: &str = "2026-10-15T20:00:00.0000000Z";

/// Routes for this module. Merge the result into the main mock router.
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

fn fixture_group(group_name: &str) -> Value {
    json!({
        "GroupId": FIXTURE_GROUP_ID,
        "GroupName": group_name,
        "State": "Idle",
        "Participants": ["demo"],
        "LastUpdatedAt": FIXTURE_LAST_UPDATED,
    })
}

/// Parse a request body as JSON. `None` means the body is not valid JSON.
fn parse_body(body: &Bytes) -> Option<Value> {
    serde_json::from_slice(body).ok()
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
    json_ok(json!([fixture_group(FIXTURE_GROUP_NAME)]))
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
    let Some(name) = parse_body(&body)
        .as_ref()
        .and_then(|value| value.get("GroupName"))
        .and_then(Value::as_str)
        .map(str::to_owned)
    else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    json_ok(fixture_group(&name))
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
    let Some(group_id) = parse_body(&body)
        .as_ref()
        .and_then(|value| value.get("GroupId"))
        .and_then(Value::as_str)
        .map(str::to_owned)
    else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    if group_id != FIXTURE_GROUP_ID {
        return StatusCode::NOT_FOUND.into_response();
    }
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
    StatusCode::NO_CONTENT.into_response()
}

/// Shared handler for the routes that take no body: `Pause` and `Unpause`.
fn no_body_command(
    state: &AppState,
    headers: &HeaderMap,
    raw_query: Option<&str>,
    path: &str,
) -> Response {
    record_request(state, "POST", path, raw_query);
    if !is_authorized(headers) {
        return unauthorized();
    }
    StatusCode::NO_CONTENT.into_response()
}

async fn pause(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    RawQuery(raw_query): RawQuery,
) -> Response {
    no_body_command(&state, &headers, raw_query.as_deref(), "/SyncPlay/Pause")
}

async fn unpause(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    RawQuery(raw_query): RawQuery,
) -> Response {
    no_body_command(&state, &headers, raw_query.as_deref(), "/SyncPlay/Unpause")
}

async fn stop(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    RawQuery(raw_query): RawQuery,
) -> Response {
    no_body_command(&state, &headers, raw_query.as_deref(), "/SyncPlay/Stop")
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
    // The client sends 64-bit signed ticks. A value the server cannot read is a 400.
    let valid = parse_body(&body)
        .as_ref()
        .and_then(|value| value.get("PositionTicks"))
        .and_then(Value::as_i64)
        .is_some_and(|ticks| ticks >= 0);
    if !valid {
        return StatusCode::BAD_REQUEST.into_response();
    }
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
    let valid_ticks = value
        .get("StartPositionTicks")
        .and_then(Value::as_i64)
        .is_some_and(|ticks| ticks >= 0);
    if !valid_items || !valid_ticks {
        return StatusCode::BAD_REQUEST.into_response();
    }
    StatusCode::NO_CONTENT.into_response()
}

fn valid_playback_status(body: &Bytes) -> bool {
    let Some(value) = parse_body(body) else {
        return false;
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
    valid_when && valid_position && valid_is_playing && valid_playlist_item
}

async fn playback_status(
    state: Arc<AppState>,
    headers: HeaderMap,
    raw_query: Option<String>,
    body: Bytes,
    path: &'static str,
) -> Response {
    record_request(&state, "POST", path, raw_query.as_deref());
    if !is_authorized(&headers) {
        return unauthorized();
    }
    if !valid_playback_status(&body) {
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
    playback_status(state, headers, raw_query, body, "/SyncPlay/Ready").await
}

async fn buffering(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    RawQuery(raw_query): RawQuery,
    body: Bytes,
) -> Response {
    playback_status(state, headers, raw_query, body, "/SyncPlay/Buffering").await
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
