//! Mock read-only session list: `GET /Sessions`. This is not a real server.
//! The fixture is fixed: two sessions, one marked active. It contains no user
//! names and no access tokens. The mock has no write route for this path.

use std::sync::Arc;

use axum::{Router, extract::State, http::HeaderMap, response::Response, routing::get};
use serde_json::{Value, json};

use crate::{AppState, is_authorized, json_ok, record_request, unauthorized};

/// Routes for this module. Merge the result into the main mock router.
pub(crate) fn router() -> Router<Arc<AppState>> {
    Router::new().route("/Sessions", get(sessions))
}

async fn sessions(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    record_request(&state, "GET", "/Sessions", None);
    if !is_authorized(&headers) {
        return unauthorized();
    }
    json_ok(fixture_sessions())
}

fn fixture_sessions() -> Value {
    json!([
        {
            "Id": "mock-session-0001",
            "DeviceName": "Mock Living Room",
            "Client": "Mock Client",
            "IsActive": true,
        },
        {
            "Id": "mock-session-0002",
            "DeviceName": "Mock Tablet",
            "Client": "Mock Client",
            "IsActive": false,
        },
    ])
}
