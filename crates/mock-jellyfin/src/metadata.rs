//! Mock administrator metadata routes. This is not a real server.
//!
//! - `POST /Items/{id}` (`UpdateItem`) stores the editable fields; item lookup returns them.
//! - `POST /Items/RemoteSearch/{type}` returns two fixed matches for the searched name, and
//!   `POST /Items/RemoteSearch/Apply/{id}` writes a match's name and provider ids.
//! - `GET /Items/{id}/Images`, `GET /Items/{id}/RemoteImages`,
//!   `POST /Items/{id}/RemoteImages/Download`, `GET` and `DELETE /Items/{id}/Images/{type}/{index}`
//!   keep a per-item image list in memory, seeded from the catalog.
//! - `GET /__mock/remote-image/{name}` serves the placeholder "provider" images without
//!   authentication, like a third-party image host.

use std::{collections::HashMap, sync::Arc, sync::Mutex};

use axum::{
    Router,
    body::Bytes,
    extract::{Path, Query, RawQuery, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::{AppState, color_for, is_authorized, json_ok, parse_body, png, record_request};

/// Fields `POST /Items/{id}` may change in the mock.
const EDITABLE_KEYS: &[&str] = &[
    "Name",
    "ForcedSortName",
    "OriginalTitle",
    "Overview",
    "ProductionYear",
    "PremiereDate",
    "OfficialRating",
    "CommunityRating",
    "Genres",
    "Tags",
    "Studios",
    "LockData",
    "LockedFields",
    "ProviderIds",
];

/// Image types with a single slot; downloading one replaces the current image.
const SINGLE_SLOT_TYPES: &[&str] = &[
    "Primary", "Art", "Banner", "Logo", "Thumb", "Disc", "Box", "Menu", "BoxRear",
];

#[derive(Default)]
pub(crate) struct MetadataStore {
    /// Edited fields keyed by item id.
    edits: Mutex<HashMap<String, Map<String, Value>>>,
    /// Image lists `(type, count)` for items whose images changed.
    images: Mutex<HashMap<String, Vec<(String, u32)>>>,
}

/// Routes for this module. Merge the result into the main mock router.
pub(crate) fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/Items/RemoteSearch/{item_type}", post(remote_search))
        .route("/Items/RemoteSearch/Apply/{id}", post(apply_search_result))
        .route("/Items/{id}/Images", get(list_images))
        .route(
            "/Items/{id}/Images/{image_type}/{index}",
            get(indexed_image).delete(delete_image),
        )
        .route("/Items/{id}/RemoteImages", get(remote_images))
        .route("/Items/{id}/RemoteImages/Download", post(download_image))
        .route("/__mock/remote-image/{name}", get(remote_image_bytes))
}

/// Writes stored edits over a catalog item's JSON.
pub(crate) fn apply_edits(state: &AppState, id: &str, body: &mut Value) {
    let Ok(edits) = state.metadata.edits.lock() else {
        return;
    };
    if let (Some(edit), Some(object)) = (edits.get(id), body.as_object_mut()) {
        for (key, value) in edit {
            object.insert(key.clone(), value.clone());
        }
    }
}

fn store_edit(state: &AppState, id: &str, fields: Map<String, Value>) {
    if let Ok(mut edits) = state.metadata.edits.lock() {
        edits.entry(id.to_owned()).or_default().extend(fields);
    }
}

/// `POST /Items/{id}`: accepts a full `BaseItemDto` and keeps its editable fields.
pub(crate) async fn update_item(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Bytes,
) -> Response {
    record_request(&state, "POST", &format!("/Items/{id}"), None);
    if !is_authorized(&headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if state.catalog.get(&id).is_none() {
        return StatusCode::NOT_FOUND.into_response();
    }
    let Ok(Value::Object(body)) = parse_body(&body) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let named = body
        .get("Name")
        .and_then(Value::as_str)
        .is_some_and(|name| !name.trim().is_empty());
    if !named || body.get("Id").and_then(Value::as_str) != Some(id.as_str()) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let fields = body
        .into_iter()
        .filter(|(key, _)| EDITABLE_KEYS.contains(&key.as_str()))
        .collect();
    store_edit(&state, &id, fields);
    StatusCode::NO_CONTENT.into_response()
}

fn base_url(headers: &HeaderMap) -> String {
    let host = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("127.0.0.1");
    format!("http://{host}")
}

async fn remote_search(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(item_type): Path<String>,
    body: Bytes,
) -> Response {
    record_request(
        &state,
        "POST",
        &format!("/Items/RemoteSearch/{item_type}"),
        None,
    );
    if !is_authorized(&headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let Ok(body) = parse_body(&body) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let info = &body["SearchInfo"];
    let name = info["Name"].as_str().unwrap_or_default();
    let has_ids = info["ProviderIds"]
        .as_object()
        .is_some_and(|ids| !ids.is_empty());
    if name.is_empty() && !has_ids {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let year = info["Year"].as_i64().unwrap_or(2001);
    let base = base_url(&headers);
    let label = if name.is_empty() { "Match" } else { name };
    json_ok(json!([
        {
            "Name": label,
            "ProductionYear": year,
            "SearchProviderName": "MockDb",
            "Overview": format!("{label}, the first mock match."),
            "ProviderIds": { "MockDb": "mock-1" },
            "ImageUrl": format!("{base}/__mock/remote-image/match-1.png"),
        },
        {
            "Name": format!("{label} (remake)"),
            "ProductionYear": year + 10,
            "SearchProviderName": "MockDb",
            "ProviderIds": { "MockDb": "mock-2" },
        },
    ]))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ApplyQuery {
    replace_all_images: Option<bool>,
}

async fn apply_search_result(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    RawQuery(raw_query): RawQuery,
    Query(query): Query<ApplyQuery>,
    body: Bytes,
) -> Response {
    record_request(
        &state,
        "POST",
        &format!("/Items/RemoteSearch/Apply/{id}"),
        raw_query.as_deref(),
    );
    if !is_authorized(&headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if state.catalog.get(&id).is_none() {
        return StatusCode::NOT_FOUND.into_response();
    }
    let Ok(body) = parse_body(&body) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let (Some(name), Some(ids)) = (body["Name"].as_str(), body["ProviderIds"].as_object()) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let mut fields = Map::new();
    fields.insert("Name".into(), json!(name));
    fields.insert("ProviderIds".into(), Value::Object(ids.clone()));
    if let Some(year) = body["ProductionYear"].as_i64() {
        fields.insert("ProductionYear".into(), json!(year));
    }
    store_edit(&state, &id, fields);
    if query.replace_all_images == Some(true)
        && let Ok(mut images) = state.metadata.images.lock()
    {
        images.insert(id, vec![("Primary".into(), 1)]);
    }
    StatusCode::NO_CONTENT.into_response()
}

/// The item's current images as `(type, count)`, seeded from the catalog.
fn current_images(state: &AppState, id: &str) -> Option<Vec<(String, u32)>> {
    let item = state.catalog.get(id)?;
    if let Ok(images) = state.metadata.images.lock()
        && let Some(list) = images.get(id)
    {
        return Some(list.clone());
    }
    let mut list = vec![("Primary".to_owned(), 1)];
    if item.has_backdrop {
        list.push(("Backdrop".to_owned(), 1));
    }
    Some(list)
}

fn set_images(state: &AppState, id: &str, list: Vec<(String, u32)>) {
    if let Ok(mut images) = state.metadata.images.lock() {
        images.insert(
            id.to_owned(),
            list.into_iter().filter(|(_, count)| *count > 0).collect(),
        );
    }
}

async fn list_images(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    record_request(&state, "GET", &format!("/Items/{id}/Images"), None);
    if !is_authorized(&headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let Some(list) = current_images(&state, &id) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let infos: Vec<Value> = list
        .iter()
        .flat_map(|(kind, count)| {
            (0..*count).map(move |index| {
                json!({
                    "ImageType": kind,
                    "ImageIndex": index,
                    "ImageTag": format!("{kind}-{index}"),
                    "Width": 32,
                    "Height": 32,
                })
            })
        })
        .collect();
    json_ok(Value::Array(infos))
}

fn png_response(seed: &str) -> Response {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, HeaderValue::from_static("image/png"))],
        png::solid_png(32, 32, color_for(seed)),
    )
        .into_response()
}

async fn indexed_image(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((id, image_type, index)): Path<(String, String, u32)>,
) -> Response {
    record_request(
        &state,
        "GET",
        &format!("/Items/{id}/Images/{image_type}/{index}"),
        None,
    );
    if !is_authorized(&headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let exists = current_images(&state, &id).is_some_and(|list| {
        list.iter()
            .any(|(kind, count)| *kind == image_type && index < *count)
    });
    if !exists {
        return StatusCode::NOT_FOUND.into_response();
    }
    png_response(&format!("{id}:{image_type}:{index}"))
}

async fn delete_image(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((id, image_type, index)): Path<(String, String, u32)>,
) -> Response {
    record_request(
        &state,
        "DELETE",
        &format!("/Items/{id}/Images/{image_type}/{index}"),
        None,
    );
    if !is_authorized(&headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let Some(mut list) = current_images(&state, &id) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some(entry) = list
        .iter_mut()
        .find(|(kind, count)| *kind == image_type && index < *count)
    else {
        return StatusCode::NOT_FOUND.into_response();
    };
    entry.1 -= 1;
    set_images(&state, &id, list);
    StatusCode::NO_CONTENT.into_response()
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RemoteImagesQuery {
    #[serde(rename = "type")]
    image_type: Option<String>,
}

async fn remote_images(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    RawQuery(raw_query): RawQuery,
    Query(query): Query<RemoteImagesQuery>,
) -> Response {
    record_request(
        &state,
        "GET",
        &format!("/Items/{id}/RemoteImages"),
        raw_query.as_deref(),
    );
    if !is_authorized(&headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if state.catalog.get(&id).is_none() {
        return StatusCode::NOT_FOUND.into_response();
    }
    let kind = query.image_type.unwrap_or_else(|| "Primary".into());
    let base = base_url(&headers);
    let images: Vec<Value> = (1..=3)
        .map(|number| {
            json!({
                "ProviderName": "MockDb",
                "Url": format!("{base}/__mock/remote-image/{kind}-{number}.png"),
                "ThumbnailUrl": format!("{base}/__mock/remote-image/{kind}-{number}-thumb.png"),
                "Width": 1000,
                "Height": 1500,
                "CommunityRating": 8.0 - f64::from(number),
                "VoteCount": 10 * number,
                "Language": "en",
                "Type": kind,
            })
        })
        .collect();
    json_ok(json!({
        "Images": images,
        "TotalRecordCount": images.len(),
        "Providers": ["MockDb"],
    }))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DownloadQuery {
    #[serde(rename = "type")]
    image_type: Option<String>,
    image_url: Option<String>,
}

async fn download_image(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    RawQuery(raw_query): RawQuery,
    Query(query): Query<DownloadQuery>,
) -> Response {
    record_request(
        &state,
        "POST",
        &format!("/Items/{id}/RemoteImages/Download"),
        raw_query.as_deref(),
    );
    if !is_authorized(&headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let (Some(kind), Some(url)) = (query.image_type, query.image_url) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    if url.is_empty() {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let Some(mut list) = current_images(&state, &id) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let single = SINGLE_SLOT_TYPES.contains(&kind.as_str());
    match list.iter_mut().find(|(existing, _)| *existing == kind) {
        Some(entry) if single => entry.1 = 1,
        Some(entry) => entry.1 += 1,
        None => list.push((kind, 1)),
    }
    set_images(&state, &id, list);
    StatusCode::NO_CONTENT.into_response()
}

async fn remote_image_bytes(Path(name): Path<String>) -> Response {
    png_response(&name)
}
