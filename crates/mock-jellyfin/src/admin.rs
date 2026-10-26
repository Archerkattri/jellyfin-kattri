//! Mock read-only admin routes: `GET /System/Info` (OpenAPI `GetSystemInfo`) and
//! `GET /Library/VirtualFolders` (OpenAPI `GetVirtualFolders`). This is not a
//! real server. The mock serves the seed libraries to the single demo session
//! and has no write routes for these paths.

use std::sync::Arc;

use axum::{Router, extract::State, http::HeaderMap, response::Response, routing::get};
use serde_json::{Value, json};

use crate::{AppState, is_authorized, json_ok, record_request, seed, unauthorized};

/// Routes for this module. Merge the result into the main mock router.
pub(crate) fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/System/Info", get(system_info))
        .route("/Library/VirtualFolders", get(virtual_folders))
}

async fn system_info(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    record_request(&state, "GET", "/System/Info", None);
    if !is_authorized(&headers) {
        return unauthorized();
    }
    json_ok(json!({
        "ServerName": "Mock Jellyfin",
        "Version": "0.0.0-mock",
        "ProductName": "Jellyfin Server",
        "Id": seed::SERVER_ID,
        "OperatingSystemDisplayName": "mock-os",
        "StartupWizardCompleted": true,
        "LocalAddress": "http://127.0.0.1",
    }))
}

async fn virtual_folders(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    record_request(&state, "GET", "/Library/VirtualFolders", None);
    if !is_authorized(&headers) {
        return unauthorized();
    }
    let folders: Vec<Value> = state
        .catalog
        .libraries
        .iter()
        .map(|library| {
            json!({
                "Name": library.name,
                "CollectionType": library.collection_type,
                "ItemId": library.id,
                "Locations": [format!("/mock/media/{}", library.name)],
            })
        })
        .collect();
    json_ok(Value::Array(folders))
}
