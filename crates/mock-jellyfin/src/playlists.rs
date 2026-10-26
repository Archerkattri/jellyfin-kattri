//! Mock playlist routes: create, read, rename, list entries, add, remove, move,
//! and delete. Paths and methods follow the Jellyfin OpenAPI `Playlists`
//! operations, but this is not a real server.
//!
//! Playlist state lives in an `Extension` created by [`router`], so each
//! running mock has its own playlists. Playlist IDs and entry IDs are
//! sequential 32-digit hex strings, so runs are deterministic.
//!
//! `GET /Items?includeItemTypes=Playlist` reads the same store as the playlist
//! routes; `DELETE /Items/{id}` deletes only mock playlists.

use std::sync::{Arc, Mutex};

use axum::{
    Extension, Router,
    body::Bytes,
    extract::{Path, Query, RawQuery, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    AppState, is_authorized, json_error, json_ok, parse_body, record_request, seed, unauthorized,
};

/// Longest playlist name accepted, in characters after trimming.
const MAX_NAME_CHARS: usize = 100;
const PLAYLIST_ID_BASE: u128 = 0x7000_0000_0000_0000_0000_0000_0000_0000;
const ENTRY_ID_BASE: u128 = 0x8000_0000_0000_0000_0000_0000_0000_0000;

/// Routes for this module. Merge the result into the main mock router.
pub(crate) fn router(store: Arc<PlaylistStore>) -> Router<Arc<AppState>> {
    Router::new()
        .route("/Playlists", post(create_playlist))
        .route("/Playlists/{id}", get(get_playlist).post(update_playlist))
        .route(
            "/Playlists/{id}/Items",
            get(playlist_items).post(add_items).delete(remove_items),
        )
        .route(
            "/Playlists/{id}/Items/{entry_id}/Move/{new_index}",
            post(move_item),
        )
        .route("/Items/{id}", delete(delete_item))
        .layer(Extension(store))
}

#[derive(Default)]
pub(crate) struct PlaylistStore {
    inner: Mutex<StoreState>,
}

impl PlaylistStore {
    pub(crate) fn list_item_dtos(&self) -> Result<Vec<Value>, StatusCode> {
        let state = self
            .inner
            .lock()
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        Ok(state
            .playlists
            .iter()
            .map(|playlist| {
                json!({
                    "Id": playlist.id,
                    "Name": playlist.name,
                    "Type": "Playlist",
                    "ChildCount": playlist.entries.len()
                })
            })
            .collect())
    }
}

#[derive(Default)]
struct StoreState {
    playlists: Vec<Playlist>,
    next_playlist: u128,
    next_entry: u128,
}

struct Playlist {
    id: String,
    name: String,
    entries: Vec<Entry>,
}

#[derive(Clone)]
struct Entry {
    entry_id: String,
    item_id: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PageQuery {
    start_index: Option<usize>,
    limit: Option<usize>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AddQuery {
    ids: Option<String>,
    position: Option<usize>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RemoveQuery {
    entry_ids: Option<String>,
}

fn validate_name(name: &str) -> Option<&str> {
    let trimmed = name.trim();
    (!trimmed.is_empty() && trimmed.chars().count() <= MAX_NAME_CHARS).then_some(trimmed)
}

/// Map a list of item IDs to catalog entries, or `None` if any is unknown.
fn resolve_items(app: &AppState, item_ids: &[String]) -> Option<Vec<Entry>> {
    let mut store_entries = Vec::with_capacity(item_ids.len());
    for item_id in item_ids {
        app.catalog.get(item_id)?;
        store_entries.push(Entry {
            entry_id: String::new(),
            item_id: item_id.clone(),
        });
    }
    Some(store_entries)
}

fn string_array(value: Option<&Value>) -> Result<Vec<String>, StatusCode> {
    match value {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(values)) => values
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .map(str::to_owned)
                    .ok_or(StatusCode::BAD_REQUEST)
            })
            .collect(),
        Some(_) => Err(StatusCode::BAD_REQUEST),
    }
}

fn split_ids(value: Option<&str>) -> Vec<String> {
    value
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(str::to_owned)
        .collect()
}

impl StoreState {
    fn new_entry(&mut self, item_id: String) -> Entry {
        let entry_id = seed::hex_id(ENTRY_ID_BASE + self.next_entry);
        self.next_entry += 1;
        Entry { entry_id, item_id }
    }

    fn playlist_mut(&mut self, id: &str) -> Option<&mut Playlist> {
        self.playlists.iter_mut().find(|playlist| playlist.id == id)
    }
}

fn with_store<T>(
    store: &PlaylistStore,
    f: impl FnOnce(&mut StoreState) -> T,
) -> Result<T, StatusCode> {
    let mut guard = store
        .inner
        .lock()
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(f(&mut guard))
}

fn not_found_playlist() -> Response {
    json_error(StatusCode::NOT_FOUND, "Playlist not found.")
}

fn authorized_or_log(
    app: &AppState,
    headers: &HeaderMap,
    method: &str,
    path: &str,
    raw: Option<&str>,
) -> Option<Response> {
    record_request(app, method, path, raw);
    (!is_authorized(headers)).then(unauthorized)
}

fn item_entry_json(app: &AppState, entry: &Entry) -> Option<Value> {
    let item = app.catalog.get(&entry.item_id)?;
    let mut value = seed::item_json(item);
    if let Some(object) = value.as_object_mut() {
        object.insert("PlaylistItemId".into(), json!(entry.entry_id));
    }
    Some(value)
}

async fn create_playlist(
    State(app): State<Arc<AppState>>,
    Extension(store): Extension<Arc<PlaylistStore>>,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
    body: Bytes,
) -> Response {
    if let Some(response) = authorized_or_log(&app, &headers, "POST", "/Playlists", raw.as_deref())
    {
        return response;
    }
    let Ok(payload) = parse_body(&body) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let Some(name) = payload
        .get("Name")
        .and_then(Value::as_str)
        .and_then(validate_name)
    else {
        return json_error(StatusCode::BAD_REQUEST, "Playlist name is required.");
    };
    let Ok(item_ids) = string_array(payload.get("Ids")) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let Some(entries) = resolve_items(&app, &item_ids) else {
        return json_error(StatusCode::NOT_FOUND, "Item not found.");
    };
    let name = name.to_owned();
    let result = with_store(&store, |state| {
        let id = seed::hex_id(PLAYLIST_ID_BASE + state.next_playlist);
        state.next_playlist += 1;
        let entries = entries
            .into_iter()
            .map(|entry| state.new_entry(entry.item_id))
            .collect();
        state.playlists.push(Playlist {
            id: id.clone(),
            name,
            entries,
        });
        id
    });
    match result {
        Ok(id) => json_ok(json!({ "Id": id })),
        Err(status) => status.into_response(),
    }
}

async fn get_playlist(
    State(app): State<Arc<AppState>>,
    Extension(store): Extension<Arc<PlaylistStore>>,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
    Path(id): Path<String>,
) -> Response {
    let path = format!("/Playlists/{id}");
    if let Some(response) = authorized_or_log(&app, &headers, "GET", &path, raw.as_deref()) {
        return response;
    }
    let found = with_store(&store, |state| {
        state
            .playlists
            .iter()
            .find(|playlist| playlist.id == id)
            .map(|playlist| {
                playlist
                    .entries
                    .iter()
                    .map(|entry| entry.item_id.clone())
                    .collect::<Vec<_>>()
            })
    });
    match found {
        Ok(Some(item_ids)) => json_ok(json!({
            "ItemIds": item_ids,
            "OpenAccess": false,
            "Shares": [],
        })),
        Ok(None) => not_found_playlist(),
        Err(status) => status.into_response(),
    }
}

async fn update_playlist(
    State(app): State<Arc<AppState>>,
    Extension(store): Extension<Arc<PlaylistStore>>,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
    Path(id): Path<String>,
    body: Bytes,
) -> Response {
    let path = format!("/Playlists/{id}");
    if let Some(response) = authorized_or_log(&app, &headers, "POST", &path, raw.as_deref()) {
        return response;
    }
    let Ok(payload) = parse_body(&body) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let name = match payload.get("Name") {
        None | Some(Value::Null) => None,
        Some(value) => match value.as_str().and_then(validate_name) {
            Some(name) => Some(name.to_owned()),
            None => {
                return json_error(StatusCode::BAD_REQUEST, "Playlist name is required.");
            }
        },
    };
    let ids = match payload.get("Ids") {
        None | Some(Value::Null) => None,
        Some(value) => match string_array(Some(value)) {
            Ok(ids) => Some(ids),
            Err(status) => return status.into_response(),
        },
    };
    let entries = match ids.as_deref().map(|ids| resolve_items(&app, ids)) {
        None => None,
        Some(Some(entries)) => Some(entries),
        Some(None) => return json_error(StatusCode::NOT_FOUND, "Item not found."),
    };
    let result = with_store(&store, |state| {
        let entries = entries.map(|entries| {
            entries
                .into_iter()
                .map(|entry| state.new_entry(entry.item_id))
                .collect::<Vec<_>>()
        });
        let playlist = state.playlist_mut(&id)?;
        if let Some(name) = name {
            playlist.name = name;
        }
        if let Some(entries) = entries {
            playlist.entries = entries;
        }
        Some(())
    });
    match result {
        Ok(Some(())) => StatusCode::NO_CONTENT.into_response(),
        Ok(None) => not_found_playlist(),
        Err(status) => status.into_response(),
    }
}

async fn playlist_items(
    State(app): State<Arc<AppState>>,
    Extension(store): Extension<Arc<PlaylistStore>>,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
    Path(id): Path<String>,
    Query(page): Query<PageQuery>,
) -> Response {
    let path = format!("/Playlists/{id}/Items");
    if let Some(response) = authorized_or_log(&app, &headers, "GET", &path, raw.as_deref()) {
        return response;
    }
    let entries = with_store(&store, |state| {
        state
            .playlists
            .iter()
            .find(|playlist| playlist.id == id)
            .map(|playlist| playlist.entries.clone())
    });
    let entries = match entries {
        Ok(Some(entries)) => entries,
        Ok(None) => return not_found_playlist(),
        Err(status) => return status.into_response(),
    };
    let total = entries.len();
    let start = page.start_index.unwrap_or(0).min(total);
    let limit = page.limit.unwrap_or(total).min(total - start);
    let items: Vec<Value> = entries[start..start + limit]
        .iter()
        .filter_map(|entry| item_entry_json(&app, entry))
        .collect();
    json_ok(json!({
        "Items": items,
        "TotalRecordCount": total,
        "StartIndex": start,
    }))
}

async fn add_items(
    State(app): State<Arc<AppState>>,
    Extension(store): Extension<Arc<PlaylistStore>>,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
    Path(id): Path<String>,
    Query(query): Query<AddQuery>,
) -> Response {
    let path = format!("/Playlists/{id}/Items");
    if let Some(response) = authorized_or_log(&app, &headers, "POST", &path, raw.as_deref()) {
        return response;
    }
    let item_ids = split_ids(query.ids.as_deref());
    if item_ids.is_empty() {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let Some(new_entries) = resolve_items(&app, &item_ids) else {
        return json_error(StatusCode::NOT_FOUND, "Item not found.");
    };
    let result = with_store(&store, |state| {
        let entries: Vec<Entry> = new_entries
            .into_iter()
            .map(|entry| state.new_entry(entry.item_id))
            .collect();
        let playlist = state.playlist_mut(&id)?;
        let position = query.position.unwrap_or(playlist.entries.len());
        if position > playlist.entries.len() {
            return Some(Err(StatusCode::BAD_REQUEST));
        }
        playlist.entries.splice(position..position, entries);
        Some(Ok(()))
    });
    match result {
        Ok(Some(Ok(()))) => StatusCode::NO_CONTENT.into_response(),
        Ok(Some(Err(status))) => status.into_response(),
        Ok(None) => not_found_playlist(),
        Err(status) => status.into_response(),
    }
}

async fn remove_items(
    State(app): State<Arc<AppState>>,
    Extension(store): Extension<Arc<PlaylistStore>>,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
    Path(id): Path<String>,
    Query(query): Query<RemoveQuery>,
) -> Response {
    let path = format!("/Playlists/{id}/Items");
    if let Some(response) = authorized_or_log(&app, &headers, "DELETE", &path, raw.as_deref()) {
        return response;
    }
    let entry_ids = split_ids(query.entry_ids.as_deref());
    let result = with_store(&store, |state| {
        let playlist = state.playlist_mut(&id)?;
        playlist
            .entries
            .retain(|entry| !entry_ids.contains(&entry.entry_id));
        Some(())
    });
    match result {
        Ok(Some(())) => StatusCode::NO_CONTENT.into_response(),
        Ok(None) => not_found_playlist(),
        Err(status) => status.into_response(),
    }
}

async fn move_item(
    State(app): State<Arc<AppState>>,
    Extension(store): Extension<Arc<PlaylistStore>>,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
    Path((id, entry_id, new_index)): Path<(String, String, usize)>,
) -> Response {
    let path = format!("/Playlists/{id}/Items/{entry_id}/Move/{new_index}");
    if let Some(response) = authorized_or_log(&app, &headers, "POST", &path, raw.as_deref()) {
        return response;
    }
    let result = with_store(&store, |state| {
        let playlist = state.playlist_mut(&id)?;
        let Some(from) = playlist
            .entries
            .iter()
            .position(|entry| entry.entry_id == entry_id)
        else {
            return Some(Err(StatusCode::NOT_FOUND));
        };
        if new_index >= playlist.entries.len() {
            return Some(Err(StatusCode::BAD_REQUEST));
        }
        let entry = playlist.entries.remove(from);
        playlist.entries.insert(new_index, entry);
        Some(Ok(()))
    });
    match result {
        Ok(Some(Ok(()))) => StatusCode::NO_CONTENT.into_response(),
        Ok(Some(Err(status))) => status.into_response(),
        Ok(None) => not_found_playlist(),
        Err(status) => status.into_response(),
    }
}

async fn delete_item(
    State(app): State<Arc<AppState>>,
    Extension(store): Extension<Arc<PlaylistStore>>,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
    Path(id): Path<String>,
) -> Response {
    let path = format!("/Items/{id}");
    if let Some(response) = authorized_or_log(&app, &headers, "DELETE", &path, raw.as_deref()) {
        return response;
    }
    let removed = with_store(&store, |state| {
        let before = state.playlists.len();
        state.playlists.retain(|playlist| playlist.id != id);
        before != state.playlists.len()
    });
    match removed {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) if app.catalog.get(&id).is_some() => StatusCode::FORBIDDEN.into_response(),
        Ok(false) => not_found_playlist(),
        Err(status) => status.into_response(),
    }
}
