//! Mock remote-command harness: `POST /__mock/remote/commands` queues a session message and
//! `GET /__mock/remote/next` returns the oldest queued one.
//!
//! The real server delivers session commands over a WebSocket, and the transport the target
//! server version uses is unconfirmed (M4-13). This pair is a loopback test hook, not the
//! Jellyfin transport. Messages keep the WebSocket envelope shape `{"MessageType": ..., "Data": ...}`
//! so the client parser sees the same fields it would see on the wire. Only the envelope is
//! checked here; the client validates `Data`.

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use axum::{
    Router,
    body::Bytes,
    extract::Extension,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde_json::Value;

use crate::{AppState, is_authorized, json_ok, unauthorized};

/// Message types the harness accepts, as the OpenAPI `MessageType` enum spells them.
const ACCEPTED_MESSAGE_TYPES: [&str; 2] = ["GeneralCommand", "Playstate"];

/// Queue of envelopes waiting for the client. One queue per running mock server.
type Queue = Arc<Mutex<VecDeque<Value>>>;

/// Routes for this module. Merge the result into the main mock router.
pub(crate) fn router() -> Router<Arc<AppState>> {
    let queue: Queue = Arc::new(Mutex::new(VecDeque::new()));
    Router::new()
        .route("/__mock/remote/commands", post(enqueue))
        .route("/__mock/remote/next", get(next))
        .layer(Extension(queue))
}

/// Accept an envelope with a known `MessageType` and an object `Data`. Anything else is 400,
/// so a harness that sends the wrong shape fails at the sender.
async fn enqueue(Extension(queue): Extension<Queue>, body: Bytes) -> Response {
    let Ok(envelope) = serde_json::from_slice::<Value>(&body) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let message_type = envelope.get("MessageType").and_then(Value::as_str);
    let has_data = envelope.get("Data").is_some_and(Value::is_object);
    let known = message_type.is_some_and(|name| ACCEPTED_MESSAGE_TYPES.contains(&name));
    if !known || !has_data {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let Ok(mut queue) = queue.lock() else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    queue.push_back(envelope);
    StatusCode::NO_CONTENT.into_response()
}

/// Return the oldest queued envelope, or 204 when none is waiting. Requires the same token as
/// the other session routes.
async fn next(Extension(queue): Extension<Queue>, headers: HeaderMap) -> Response {
    if !is_authorized(&headers) {
        return unauthorized();
    }
    let Ok(mut queue) = queue.lock() else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    match queue.pop_front() {
        Some(envelope) => json_ok(envelope),
        None => StatusCode::NO_CONTENT.into_response(),
    }
}
