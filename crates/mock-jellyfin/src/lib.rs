//! Deterministic mock of the Jellyfin HTTP API for tests and development.
//!
//! This is NOT a real Jellyfin server. It implements only the endpoints the
//! Rust client uses, with fixed seed data, placeholder media bytes, and an
//! in-memory log of playback reports. Passing tests against it do not
//! validate compatibility with a real Jellyfin server.

mod admin;
mod favorites;
mod live_tv;
mod metadata;
mod playlists;
mod png;
mod remote;
mod seed;
mod subtitles;
mod syncplay;
mod system;

use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use axum::{
    Router,
    body::{Body, Bytes},
    extract::{Path, Query, RawQuery, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::{sync::oneshot, task::JoinHandle};

use seed::{Catalog, Item, Kind};

/// Size of the placeholder media body served by the stream endpoints.
const STREAM_LEN: usize = 32 * 1024;
const DEFAULT_LATEST_LIMIT: usize = 16;

/// Options for [`spawn`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MockConfig {
    /// Port to bind on 127.0.0.1. `0` selects an ephemeral port.
    pub port: u16,
    /// Limit the returned user libraries for empty-state and tile-count tests.
    pub user_view_limit: Option<usize>,
    /// Override the deterministic seed to exercise multi-page catalog browsing.
    pub item_count: Option<usize>,
    /// Seed this many deterministic audio tracks for queue restore tests.
    pub audio_item_count: Option<usize>,
    /// Seed this many deterministic photos in the standard photo folder.
    pub photo_item_count: Option<usize>,
    /// Seed four ordered items and a nested box set in the first movie collection.
    pub collection_children: bool,
    /// Simulate an installation that rejects an unsupported filter for audio.
    pub reject_unplayed_filter_for_audio: bool,
    /// Omit `ImageTags` from item DTOs to exercise untagged-image fallback.
    pub omit_primary_image_tags: bool,
    /// Simulate items whose primary image is unavailable.
    pub primary_images_unavailable: bool,
    /// Simulate an older server without the dedicated seasons endpoint.
    pub seasons_endpoint_not_found: bool,
    /// Return this status for the optional similar-items endpoint.
    pub similar_endpoint_status: Option<u16>,
    /// Fail this many authenticated progress reports before normal behavior resumes.
    pub fail_progress_posts: usize,
    /// Return HTTP 500 for this many public-info requests, then resume normally.
    pub public_info_failures: usize,
    /// Return this status for played-state toggles without changing item state.
    pub played_toggle_status: Option<u16>,
    /// Report Quick Connect as inactive (`Enabled` is false and `Initiate` returns 401).
    pub quick_connect_disabled: bool,
    /// Return this non-success status from the enabled probe, for older-server compatibility tests.
    pub quick_connect_enabled_status: Option<u16>,
    /// Number of `Connect` polls that stay pending before a request is authorized.
    pub quick_connect_pending_polls: usize,
    /// Report video items as transcode-only MKV: `PlaybackInfo` lists an `mkv` source that
    /// cannot be direct played or direct streamed, and the master playlist leads to a media
    /// playlist with segments, as a Jellyfin server does when it must transcode.
    pub transcode_only_mkv: bool,
    /// Add collections, books, home videos, music videos and Live TV libraries to the seed.
    pub extra_libraries: bool,
}

/// Handle to a running mock server. Dropping it requests shutdown; call
/// [`RunningServer::shutdown`] to wait for the server task to finish.
pub struct RunningServer {
    /// The bound loopback address.
    pub addr: SocketAddr,
    shutdown_tx: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<()>>,
}

impl RunningServer {
    /// `http://127.0.0.1:<port>`, with no trailing slash.
    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Request graceful shutdown and wait for the server task to finish.
    pub async fn shutdown(mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

impl Drop for RunningServer {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
    }
}

/// Bind 127.0.0.1 on `config.port` and serve the mock API in the background.
/// Must be called from within a Tokio runtime.
pub async fn spawn(config: MockConfig) -> std::io::Result<RunningServer> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", config.port)).await?;
    let addr = listener.local_addr()?;
    let state = Arc::new(AppState {
        catalog: if let Some(count) = config.audio_item_count {
            seed::catalog_with_audio_track_count(count)
        } else if let Some(count) = config.photo_item_count {
            seed::catalog_with_photo_count(count)
        } else {
            match config.item_count {
                Some(count) => seed::catalog_with_item_count(count),
                None if config.collection_children => seed::catalog_with_collection_children(),
                None if config.extra_libraries => seed::catalog_with_extra_libraries(),
                None => seed::catalog(),
            }
        },
        sessions: Mutex::new(Vec::new()),
        capabilities: Mutex::new(Vec::new()),
        requests: Mutex::new(Vec::new()),
        user_view_limit: config.user_view_limit,
        reject_unplayed_filter_for_audio: config.reject_unplayed_filter_for_audio,
        omit_primary_image_tags: config.omit_primary_image_tags,
        primary_images_unavailable: config.primary_images_unavailable,
        seasons_endpoint_not_found: config.seasons_endpoint_not_found,
        similar_endpoint_status: config.similar_endpoint_status,
        fail_progress_posts_remaining: AtomicUsize::new(config.fail_progress_posts),
        public_info_failures_remaining: AtomicUsize::new(config.public_info_failures),
        played_toggle_status: config.played_toggle_status,
        quick_connect_disabled: config.quick_connect_disabled,
        quick_connect_enabled_status: config.quick_connect_enabled_status,
        quick_connect_pending_polls: config.quick_connect_pending_polls,
        transcode_only_mkv: config.transcode_only_mkv,
        played_overrides: Mutex::new(HashMap::new()),
        favorite_overrides: Mutex::new(HashMap::new()),
        quick_connect: Mutex::new(HashMap::new()),
        quick_connect_counter: AtomicUsize::new(0),
        playlist_store: Arc::new(playlists::PlaylistStore::default()),
        metadata: metadata::MetadataStore::default(),
        subtitle_store: subtitles::SubtitleStore::default(),
    });
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let app = router(state);
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = shutdown_rx.await;
            })
            .await;
    });
    Ok(RunningServer {
        addr,
        shutdown_tx: Some(shutdown_tx),
        task: Some(task),
    })
}

struct AppState {
    catalog: Catalog,
    user_view_limit: Option<usize>,
    reject_unplayed_filter_for_audio: bool,
    omit_primary_image_tags: bool,
    primary_images_unavailable: bool,
    seasons_endpoint_not_found: bool,
    similar_endpoint_status: Option<u16>,
    fail_progress_posts_remaining: AtomicUsize,
    public_info_failures_remaining: AtomicUsize,
    played_toggle_status: Option<u16>,
    quick_connect_disabled: bool,
    quick_connect_enabled_status: Option<u16>,
    quick_connect_pending_polls: usize,
    transcode_only_mkv: bool,
    /// Played overrides set by `POST`/`DELETE /UserPlayedItems/{id}`, keyed by item id.
    played_overrides: Mutex<HashMap<String, bool>>,
    /// Favorite overrides set by `POST`/`DELETE /UserFavoriteItems/{id}`.
    favorite_overrides: Mutex<HashMap<String, bool>>,
    /// Pending Quick Connect requests keyed by secret. Secrets are never logged.
    quick_connect: Mutex<HashMap<String, QuickConnectEntry>>,
    quick_connect_counter: AtomicUsize,
    /// Mock playlist storage shared by playlist routes and `GET /Items`.
    playlist_store: Arc<playlists::PlaylistStore>,
    /// Metadata edits and image lists changed by the administrator routes.
    metadata: metadata::MetadataStore,
    /// Subtitles "downloaded" through the remote subtitle routes, listed by `GET /Items/{id}`.
    subtitle_store: subtitles::SubtitleStore,
    /// Playback reports in arrival order, served at `GET /__mock/sessions`.
    sessions: Mutex<Vec<Value>>,
    /// Sanitized capability advertisements, served at `GET /__mock/capabilities`.
    capabilities: Mutex<Vec<Value>>,
    /// Sanitized request method/path entries; authorization headers are never recorded.
    requests: Mutex<Vec<Value>>,
}

fn router(state: Arc<AppState>) -> Router {
    let playlist_store = state.playlist_store.clone();
    Router::new()
        .route("/System/Info/Public", get(public_info))
        .route("/Users/AuthenticateByName", post(authenticate))
        .route("/Users/Me", get(current_user))
        .route(
            "/Users/AuthenticateWithQuickConnect",
            post(authenticate_quick_connect),
        )
        .route("/QuickConnect/Enabled", get(quick_connect_enabled))
        .route("/QuickConnect/Initiate", post(quick_connect_initiate))
        .route("/QuickConnect/Connect", get(quick_connect_connect))
        .route(
            "/UserPlayedItems/{id}",
            post(mark_played).delete(mark_unplayed),
        )
        .route("/UserViews", get(user_views))
        .route("/UserItems/Resume", get(resume))
        .route("/Shows/NextUp", get(next_up))
        .route("/Shows/{series_id}/Seasons", get(show_seasons))
        .route("/Shows/{series_id}/Episodes", get(show_episodes))
        .route("/Items/{id}/Similar", get(similar_items))
        .route("/Persons/{name}/Images/{image_type}", get(person_image))
        .route("/Items/Latest", get(latest))
        .route("/Items", get(list_items))
        .route("/Items/{id}", get(get_item).post(metadata::update_item))
        .route("/MediaSegments/{id}", get(media_segments))
        .route("/Items/{id}/PlaybackInfo", post(playback_info))
        .route("/Items/{id}/Download", get(download))
        .route("/Items/{id}/Images/{image_type}", get(image))
        .route("/Videos/{id}/master.m3u8", get(master_playlist))
        .route("/Videos/{id}/main.m3u8", get(media_playlist))
        .route("/Videos/{id}/hls1/{playlist}/{segment}", get(hls_segment))
        .route("/Videos/{id}/{stream}", get(stream))
        .route("/Audio/{id}/{stream}", get(stream))
        .route("/Sessions/Playing", post(report_playing))
        .route("/Sessions/Playing/Progress", post(report_progress))
        .route("/Sessions/Playing/Stopped", post(report_stopped))
        .route("/Sessions/Capabilities/Full", post(advertise_capabilities))
        .route("/__mock/sessions", get(session_log))
        .route("/__mock/capabilities", get(capability_log))
        .route("/__mock/requests", get(request_log))
        .merge(playlists::router(playlist_store))
        .merge(favorites::router())
        .merge(live_tv::router())
        .merge(admin::router())
        .merge(metadata::router())
        .merge(system::router())
        .merge(syncplay::router())
        .merge(remote::router())
        .merge(subtitles::router())
        .fallback(not_found)
        .with_state(state)
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ItemsQuery {
    recursive: Option<bool>,
    start_index: Option<usize>,
    limit: Option<usize>,
    parent_id: Option<String>,
    ids: Option<String>,
    search_term: Option<String>,
    include_item_types: Option<String>,
    media_types: Option<String>,
    sort_by: Option<Vec<String>>,
    sort_order: Option<Vec<String>>,
    filters: Option<Vec<String>>,
    genres: Option<Vec<String>>,
    years: Option<Vec<String>>,
    enable_total_record_count: Option<bool>,
    person_ids: Option<Vec<String>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MediaSegmentsQuery {
    include_segment_types: Option<String>,
}

fn parse_items_query(raw_query: Option<&str>) -> Result<ItemsQuery, ()> {
    let mut values = std::collections::HashMap::<String, Vec<String>>::new();
    for (key, value) in form_urlencoded::parse(raw_query.unwrap_or_default().as_bytes()) {
        values
            .entry(key.into_owned())
            .or_default()
            .push(value.into_owned());
    }

    fn scalar<T: std::str::FromStr>(
        values: &std::collections::HashMap<String, Vec<String>>,
        key: &str,
    ) -> Result<Option<T>, ()> {
        values
            .get(key)
            .and_then(|items| items.first())
            .map(|value| value.parse().map_err(|_| ()))
            .transpose()
    }

    fn string(
        values: &std::collections::HashMap<String, Vec<String>>,
        key: &str,
    ) -> Option<String> {
        values.get(key).and_then(|items| items.first()).cloned()
    }

    fn repeated(
        values: &std::collections::HashMap<String, Vec<String>>,
        key: &str,
    ) -> Option<Vec<String>> {
        values.get(key).cloned()
    }

    Ok(ItemsQuery {
        recursive: scalar(&values, "recursive")?,
        start_index: scalar(&values, "startIndex")?,
        limit: scalar(&values, "limit")?,
        parent_id: string(&values, "parentId"),
        ids: string(&values, "ids"),
        search_term: string(&values, "searchTerm"),
        include_item_types: string(&values, "includeItemTypes"),
        media_types: string(&values, "mediaTypes"),
        sort_by: repeated(&values, "sortBy"),
        sort_order: repeated(&values, "sortOrder"),
        filters: repeated(&values, "filters"),
        genres: repeated(&values, "genres"),
        years: repeated(&values, "years"),
        enable_total_record_count: scalar(&values, "enableTotalRecordCount")?,
        person_ids: repeated(&values, "personIds"),
    })
}

fn split_list(value: Option<&str>) -> Vec<String> {
    value
        .unwrap_or("")
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(str::to_owned)
        .collect()
}

fn split_repeated_list(values: Option<&[String]>) -> Vec<String> {
    values
        .unwrap_or_default()
        .iter()
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(str::to_owned)
        .collect()
}

fn token_from_header(value: &str) -> Option<&str> {
    let start = value.find("Token=")? + "Token=".len();
    let rest = &value[start..];
    if let Some(quoted) = rest.strip_prefix('"') {
        quoted.split('"').next()
    } else {
        rest.split(',').next().map(str::trim)
    }
}

fn is_authorized(headers: &HeaderMap) -> bool {
    ["x-emby-authorization", "authorization"]
        .iter()
        .filter_map(|name| headers.get(*name))
        .filter_map(|value| value.to_str().ok())
        .any(|value| token_from_header(value) == Some(seed::MOCK_ACCESS_TOKEN))
}

fn unauthorized() -> Response {
    StatusCode::UNAUTHORIZED.into_response()
}

fn json_ok(value: Value) -> Response {
    (StatusCode::OK, axum::Json(value)).into_response()
}

fn items_response(items: &[&Item], total: usize, start: usize) -> Value {
    json!({
        "Items": items.iter().map(|item| seed::item_json(item)).collect::<Vec<_>>(),
        "TotalRecordCount": total,
        "StartIndex": start,
    })
}

fn parse_body(body: &Bytes) -> Result<Value, StatusCode> {
    if body.iter().all(u8::is_ascii_whitespace) {
        return Ok(Value::Null);
    }
    serde_json::from_slice(body).map_err(|_| StatusCode::BAD_REQUEST)
}

async fn not_found() -> Response {
    StatusCode::NOT_FOUND.into_response()
}

async fn public_info(State(state): State<Arc<AppState>>) -> Response {
    if state
        .public_info_failures_remaining
        .try_update(Ordering::AcqRel, Ordering::Acquire, |remaining| {
            remaining.checked_sub(1)
        })
        .is_ok()
    {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    json_ok(json!({
        "LocalAddress": "http://127.0.0.1",
        "ServerName": "Mock Jellyfin",
        "Version": "10.10.7",
        "ProductName": "Jellyfin Server (mock)",
        "OperatingSystem": "",
        "Id": seed::SERVER_ID,
        "StartupWizardCompleted": true,
    }))
}

async fn authenticate(body: Bytes) -> Response {
    let Ok(value) = parse_body(&body) else {
        return unauthorized();
    };
    let username = value.get("Username").and_then(Value::as_str);
    let password = value.get("Pw").and_then(Value::as_str);
    if username != Some(seed::DEMO_USERNAME) || password != Some(seed::DEMO_PASSWORD) {
        return unauthorized();
    }
    json_ok(authentication_result())
}

/// `GET /Users/Me`: the signed-in user, for a token saved from an earlier sign-in.
async fn current_user(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    record_request(&state, "GET", "/Users/Me", None);
    if !is_authorized(&headers) {
        return unauthorized();
    }
    json_ok(seed::user_json())
}

fn authentication_result() -> Value {
    json!({
        "User": seed::user_json(),
        "SessionInfo": {
            "Id": seed::hex_id(0x5e55),
            "UserId": seed::DEMO_USER_ID,
            "UserName": seed::DEMO_USERNAME,
            "Client": "mock-jellyfin",
            "DeviceName": "mock-jellyfin",
            "DeviceId": "mock-jellyfin-device",
            "ServerId": seed::SERVER_ID,
        },
        "AccessToken": seed::MOCK_ACCESS_TOKEN,
        "ServerId": seed::SERVER_ID,
    })
}

/// One pending Quick Connect request. `polls` counts `Connect` calls so the
/// mock can authorize after a configured number of pending polls.
struct QuickConnectEntry {
    code: String,
    polls: usize,
}

async fn quick_connect_enabled(State(state): State<Arc<AppState>>) -> Response {
    record_request(&state, "GET", "/QuickConnect/Enabled", None);
    if let Some(status) = state.quick_connect_enabled_status
        && let Ok(status) = StatusCode::from_u16(status)
        && !status.is_success()
    {
        return status.into_response();
    }
    json_ok(json!(!state.quick_connect_disabled))
}

async fn quick_connect_initiate(State(state): State<Arc<AppState>>) -> Response {
    record_request(&state, "POST", "/QuickConnect/Initiate", None);
    if state.quick_connect_disabled {
        return json_error(
            StatusCode::UNAUTHORIZED,
            "Quick connect is not active on this server.",
        );
    }
    let sequence = state.quick_connect_counter.fetch_add(1, Ordering::AcqRel) + 1;
    let secret = format!("mock-quick-connect-secret-{sequence:06}");
    let code = format!("{:06}", sequence % 1_000_000);
    let Ok(mut entries) = state.quick_connect.lock() else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    entries.insert(
        secret.clone(),
        QuickConnectEntry {
            code: code.clone(),
            polls: 0,
        },
    );
    json_ok(json!({
        "Authenticated": false,
        "Secret": secret,
        "Code": code,
        "DeviceId": "mock-jellyfin-device",
        "DeviceName": "mock-jellyfin",
        "AppName": "mock-jellyfin",
        "AppVersion": "0.0.0",
        "DateAdded": "2026-01-01T00:00:00Z",
    }))
}

#[derive(Deserialize)]
struct QuickConnectSecretQuery {
    secret: String,
}

async fn quick_connect_connect(
    State(state): State<Arc<AppState>>,
    Query(query): Query<QuickConnectSecretQuery>,
) -> Response {
    // The query carries the secret; the recorded path omits the query entirely.
    record_request(&state, "GET", "/QuickConnect/Connect", None);
    let Ok(mut entries) = state.quick_connect.lock() else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let Some(entry) = entries.get_mut(&query.secret) else {
        return json_error(StatusCode::NOT_FOUND, "Unknown quick connect secret.");
    };
    entry.polls += 1;
    json_ok(json!({
        "Authenticated": entry.polls > state.quick_connect_pending_polls,
        "Code": entry.code,
    }))
}

async fn authenticate_quick_connect(State(state): State<Arc<AppState>>, body: Bytes) -> Response {
    record_request(&state, "POST", "/Users/AuthenticateWithQuickConnect", None);
    let Ok(value) = parse_body(&body) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let Some(secret) = value.get("Secret").and_then(Value::as_str) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let Ok(mut entries) = state.quick_connect.lock() else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let authorized = entries
        .get(secret)
        .is_some_and(|entry| entry.polls > state.quick_connect_pending_polls);
    if !authorized {
        return unauthorized();
    }
    // Quick Connect secrets are single use.
    entries.remove(secret);
    json_ok(authentication_result())
}

fn set_played(
    state: &AppState,
    headers: &HeaderMap,
    raw_query: Option<&str>,
    item_id: &str,
    played: bool,
) -> Response {
    if !is_authorized(headers) {
        return unauthorized();
    }
    let method = if played { "POST" } else { "DELETE" };
    record_request(
        state,
        method,
        &format!("/UserPlayedItems/{item_id}"),
        raw_query,
    );
    if let Some(status) = state.played_toggle_status {
        let status = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        return status.into_response();
    }
    let Some(item) = state.catalog.get(item_id) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Ok(mut overrides) = state.played_overrides.lock() else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    overrides.insert(item_id.to_owned(), played);
    let mut user_data = seed::user_data_json(item);
    if let Some(object) = user_data.as_object_mut() {
        object.insert("Played".into(), json!(played));
        if played {
            object.remove("PlayedPercentage");
        }
    }
    json_ok(user_data)
}

async fn mark_played(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    RawQuery(raw_query): RawQuery,
    Path(id): Path<String>,
) -> Response {
    set_played(&state, &headers, raw_query.as_deref(), &id, true)
}

async fn mark_unplayed(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    RawQuery(raw_query): RawQuery,
    Path(id): Path<String>,
) -> Response {
    set_played(&state, &headers, raw_query.as_deref(), &id, false)
}

async fn user_views(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if !is_authorized(&headers) {
        return unauthorized();
    }
    let views: Vec<Value> = state
        .catalog
        .libraries
        .iter()
        .take(state.user_view_limit.unwrap_or(usize::MAX))
        .map(|library| {
            json!({
                "Name": library.name,
                "ServerId": seed::SERVER_ID,
                "Id": library.id,
                "CollectionType": library.collection_type,
                "Type": "CollectionFolder",
                "IsFolder": true,
            })
        })
        .collect();
    let total = views.len();
    json_ok(json!({ "Items": views, "TotalRecordCount": total, "StartIndex": 0 }))
}

async fn resume(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<ItemsQuery>,
) -> Response {
    if !is_authorized(&headers) {
        return unauthorized();
    }
    let mut items: Vec<&Item> = state
        .catalog
        .items
        .iter()
        .filter(|item| item.kind.media_type() == Some("Video"))
        .filter(|item| item.position_ticks > 0 && !item.played)
        .collect();
    items.sort_by(|a, b| b.last_played.cmp(&a.last_played));
    let total = items.len();
    let limit = query.limit.unwrap_or(total);
    items.truncate(limit);
    json_ok(items_response(&items, total, 0))
}

async fn next_up(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<ItemsQuery>,
) -> Response {
    if !is_authorized(&headers) {
        return unauthorized();
    }
    // For each series: the first unplayed episode after the series' last
    // played episode, or the first episode when nothing has been played.
    let catalog = &state.catalog;
    let mut next: Vec<&Item> = Vec::new();
    for series in catalog
        .items
        .iter()
        .filter(|item| item.kind == Kind::Series)
    {
        let episodes: Vec<&Item> = catalog
            .items
            .iter()
            .filter(|item| {
                item.kind == Kind::Episode && item.series_id.as_deref() == Some(&series.id)
            })
            .collect();
        let last_played = episodes.iter().rposition(|episode| episode.played);
        let candidate = match last_played {
            Some(position) => episodes[position + 1..]
                .iter()
                .find(|episode| !episode.played),
            None => episodes.first(),
        };
        if let Some(episode) = candidate {
            next.push(*episode);
        }
    }
    let total = next.len();
    next.truncate(query.limit.unwrap_or(total));
    json_ok(items_response(&next, total, 0))
}

async fn latest(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<ItemsQuery>,
) -> Response {
    if !is_authorized(&headers) {
        return unauthorized();
    }
    let catalog = &state.catalog;
    let mut items: Vec<&Item> = catalog
        .items
        .iter()
        .filter(|item| item.kind.is_leaf())
        .filter(|item| match &query.parent_id {
            Some(parent) => catalog.is_under(item, parent, true),
            None => true,
        })
        .collect();
    items.sort_by(|a, b| b.date_created.cmp(&a.date_created));
    items.truncate(query.limit.unwrap_or(DEFAULT_LATEST_LIMIT));
    // Jellyfin returns this endpoint as a bare array.
    json_ok(Value::Array(
        items.iter().map(|item| seed::item_json(item)).collect(),
    ))
}

async fn list_items(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    RawQuery(raw_query): RawQuery,
) -> Response {
    record_request(&state, "GET", "/Items", raw_query.as_deref());
    if !is_authorized(&headers) {
        return unauthorized();
    }
    let Ok(query) = parse_items_query(raw_query.as_deref()) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let catalog = &state.catalog;
    let types = split_list(query.include_item_types.as_deref());
    let media_types = split_list(query.media_types.as_deref());
    let filters = split_repeated_list(query.filters.as_deref());
    let favorite_overrides = match state.favorite_overrides.lock() {
        Ok(overrides) => overrides.clone(),
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    let requested_genres = split_repeated_list(query.genres.as_deref());
    let requested_years = split_repeated_list(query.years.as_deref())
        .into_iter()
        .filter_map(|year| year.parse::<i32>().ok())
        .collect::<Vec<_>>();
    let person_ids = split_repeated_list(query.person_ids.as_deref());
    let item_ids = split_list(query.ids.as_deref());
    if !types.is_empty() && types.iter().all(|item_type| item_type == "Playlist") {
        let Ok(mut playlists) = state.playlist_store.list_item_dtos() else {
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        };
        if let Some(search) = query.search_term.as_deref().map(str::to_lowercase) {
            playlists.retain(|item| {
                item["Name"]
                    .as_str()
                    .is_some_and(|name| name.to_lowercase().contains(&search))
            });
        }
        playlists.sort_by_key(|item| item["Name"].as_str().unwrap_or_default().to_lowercase());
        if split_repeated_list(query.sort_order.as_deref())
            .first()
            .is_some_and(|order| order == "Descending")
        {
            playlists.reverse();
        }
        let total = playlists.len();
        let start = query.start_index.unwrap_or(0).min(total);
        let end = query
            .limit
            .map_or(total, |limit| start.saturating_add(limit).min(total));
        let mut response = json!({
            "Items": &playlists[start..end],
            "TotalRecordCount": total,
            "StartIndex": start
        });
        if query.enable_total_record_count == Some(false)
            && let Some(object) = response.as_object_mut()
        {
            object.remove("TotalRecordCount");
        }
        return json_ok(response);
    }
    if state.reject_unplayed_filter_for_audio
        && (media_types.iter().any(|media_type| media_type == "Audio")
            || types
                .iter()
                .any(|media_type| matches!(media_type.as_str(), "Audio" | "MusicAlbum")))
        && filters.iter().any(|filter| filter == "IsUnplayed")
    {
        return json_error(
            StatusCode::BAD_REQUEST,
            "The item filter 'IsUnplayed' is unsupported.",
        );
    }
    let search = query.search_term.as_deref().map(str::to_lowercase);
    let recursive = query.recursive.unwrap_or(false);

    let mut items: Vec<&Item> = catalog
        .items
        .iter()
        .filter(|item| item_ids.is_empty() || item_ids.iter().any(|id| id == &item.id))
        .filter(|item| types.is_empty() || types.iter().any(|t| t == item.kind.as_str()))
        .filter(|item| {
            media_types.is_empty()
                || item
                    .kind
                    .media_type()
                    .is_some_and(|m| media_types.iter().any(|t| t == m))
        })
        .filter(|item| {
            filters.iter().all(|filter| match filter.as_str() {
                "IsPlayed" => item.played,
                "IsUnplayed" => !item.played,
                "IsFavorite" | "IsFavoriteOrLikes" => favorite_overrides
                    .get(&item.id)
                    .copied()
                    .unwrap_or(item.favorite),
                _ => true,
            })
        })
        .filter(|item| {
            requested_genres.is_empty()
                || item
                    .genres
                    .iter()
                    .any(|genre| requested_genres.iter().any(|requested| requested == genre))
        })
        .filter(|item| {
            requested_years.is_empty()
                || item
                    .production_year
                    .is_some_and(|year| requested_years.contains(&year))
        })
        .filter(|item| {
            person_ids.is_empty()
                || seed::people(item).iter().any(|person| {
                    person["Id"]
                        .as_str()
                        .is_some_and(|id| person_ids.iter().any(|requested| requested == id))
                })
        })
        .filter(|item| match &search {
            Some(term) => item.name.to_lowercase().contains(term.as_str()),
            None => true,
        })
        .filter(|item| match &query.parent_id {
            Some(parent) => catalog.is_under(item, parent, recursive),
            None => true,
        })
        .collect();

    let descending = split_repeated_list(query.sort_order.as_deref())
        .first()
        .is_some_and(|order| order == "Descending");
    let sort_key = split_repeated_list(query.sort_by.as_deref())
        .into_iter()
        .next()
        .unwrap_or_else(|| "SortName".to_owned());
    match sort_key.as_str() {
        "PremiereDate" | "ProductionYear" => {
            items.sort_by_key(|item| item.production_year.unwrap_or(0));
        }
        "CommunityRating" => items.sort_by_key(|item| item.community_rating.unwrap_or(0)),
        "RunTimeTicks" => items.sort_by_key(|item| item.run_time_ticks.unwrap_or(0)),
        "DateCreated" => items.sort_by(|a, b| a.date_created.cmp(&b.date_created)),
        _ => items.sort_by_key(|item| item.name.to_lowercase()),
    }
    if descending {
        items.reverse();
    }

    let total = items.len();
    let start = query.start_index.unwrap_or(0).min(total);
    let end = query
        .limit
        .map_or(total, |limit| start.saturating_add(limit).min(total));
    let mut response = items_response(&items[start..end], total, start);
    if let Some(entries) = response.get_mut("Items").and_then(Value::as_array_mut) {
        for entry in entries {
            let Some(id) = entry.get("Id").and_then(Value::as_str) else {
                continue;
            };
            if let Some(favorite) = favorite_overrides.get(id)
                && let Some(user_data) = entry.get_mut("UserData").and_then(Value::as_object_mut)
            {
                user_data.insert("IsFavorite".into(), json!(favorite));
            }
        }
    }
    if query.enable_total_record_count == Some(false)
        && let Value::Object(object) = &mut response
    {
        object.remove("TotalRecordCount");
    }
    json_ok(response)
}

async fn similar_items(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    RawQuery(raw_query): RawQuery,
) -> Response {
    record_request(
        &state,
        "GET",
        &format!("/Items/{id}/Similar"),
        raw_query.as_deref(),
    );
    if !is_authorized(&headers) {
        return unauthorized();
    }
    if let Some(status) = state.similar_endpoint_status
        && let Ok(status) = StatusCode::from_u16(status)
    {
        return status.into_response();
    }
    let Some(item) = state.catalog.get(&id) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Ok(query) = parse_items_query(raw_query.as_deref()) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let limit = query.limit.unwrap_or(10);
    let people = seed::people(item);
    let ids = people
        .iter()
        .filter_map(|person| person["Id"].as_str().map(str::to_owned))
        .collect::<Vec<_>>();
    let mut similar = state
        .catalog
        .items
        .iter()
        .filter(|candidate| {
            candidate.id != id
                && candidate.kind.media_type() == Some("Video")
                && seed::people(candidate).iter().any(|person| {
                    person["Id"]
                        .as_str()
                        .is_some_and(|person_id| ids.iter().any(|id| id == person_id))
                })
        })
        .collect::<Vec<_>>();
    similar.sort_by(|left, right| left.id.cmp(&right.id));
    similar.truncate(limit);
    let total = similar.len();
    json_ok(json!({
        "Items": similar.iter().map(|item| seed::item_json(item)).collect::<Vec<_>>(),
        "TotalRecordCount": total,
        "StartIndex": 0,
    }))
}

async fn person_image(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((name, image_type)): Path<(String, String)>,
) -> Response {
    record_request(
        &state,
        "GET",
        &format!("/Persons/{name}/Images/{image_type}"),
        None,
    );
    if !is_authorized(&headers) {
        return unauthorized();
    }
    if image_type != "Primary" || !seed::is_person_name(&name) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let rgb = color_for(&format!("person:{name}:Primary"));
    let body = png::solid_png(32, 32, rgb);
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, HeaderValue::from_static("image/png"))],
        body,
    )
        .into_response()
}

async fn show_seasons(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(series_id): Path<String>,
    RawQuery(raw_query): RawQuery,
) -> Response {
    record_request(
        &state,
        "GET",
        &format!("/Shows/{series_id}/Seasons"),
        raw_query.as_deref(),
    );
    if !is_authorized(&headers) {
        return unauthorized();
    }
    if state.seasons_endpoint_not_found {
        return StatusCode::NOT_FOUND.into_response();
    }
    let mut seasons: Vec<&Item> = state
        .catalog
        .items
        .iter()
        .filter(|item| {
            item.kind == Kind::Season && item.parent_id.as_deref() == Some(series_id.as_str())
        })
        .collect();
    seasons.sort_by_key(|season| (season.index_number.unwrap_or(i32::MAX), season.id.clone()));
    let total = seasons.len();
    json_ok(items_response(&seasons, total, 0))
}

async fn show_episodes(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(series_id): Path<String>,
    RawQuery(raw_query): RawQuery,
) -> Response {
    record_request(
        &state,
        "GET",
        &format!("/Shows/{series_id}/Episodes"),
        raw_query.as_deref(),
    );
    if !is_authorized(&headers) {
        return unauthorized();
    }
    let season_id = raw_query
        .as_deref()
        .into_iter()
        .flat_map(|query| form_urlencoded::parse(query.as_bytes()))
        .find(|(key, _)| key == "seasonId")
        .map(|(_, value)| value.into_owned());
    let Some(season_id) = season_id else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let mut episodes: Vec<&Item> = state
        .catalog
        .items
        .iter()
        .filter(|item| {
            item.kind == Kind::Episode
                && item.series_id.as_deref() == Some(series_id.as_str())
                && item.season_id.as_deref() == Some(season_id.as_str())
        })
        .collect();
    episodes.sort_by_key(|episode| (episode.index_number.unwrap_or(i32::MAX), episode.id.clone()));
    let total = episodes.len();
    json_ok(items_response(&episodes, total, 0))
}

fn record_request(state: &AppState, method: &str, path: &str, query: Option<&str>) {
    let path = query.map_or_else(
        || path.to_owned(),
        |query| {
            let sanitized = form_urlencoded::parse(query.as_bytes())
                .map(|(key, value)| {
                    let redacted = matches!(
                        key.to_ascii_lowercase().as_str(),
                        "api_key" | "token" | "access_token" | "authorization"
                    );
                    (
                        key.into_owned(),
                        if redacted {
                            "[redacted]".into()
                        } else {
                            value.into_owned()
                        },
                    )
                })
                .collect::<Vec<_>>();
            let query = form_urlencoded::Serializer::new(String::new())
                .extend_pairs(sanitized)
                .finish();
            format!("{path}?{query}")
        },
    );
    if let Ok(mut requests) = state.requests.lock() {
        requests.push(json!({ "Method": method, "Path": path }));
    }
}

async fn request_log(State(state): State<Arc<AppState>>) -> Response {
    let Ok(requests) = state.requests.lock() else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    json_ok(Value::Array(requests.clone()))
}

fn json_error(status: StatusCode, message: &str) -> Response {
    (status, axum::Json(json!({ "Message": message }))).into_response()
}

async fn get_item(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if !is_authorized(&headers) {
        return unauthorized();
    }
    match state.catalog.get(&id) {
        Some(item) => {
            let mut body = seed::item_json(item);
            if state.omit_primary_image_tags
                && let Some(object) = body.as_object_mut()
            {
                object.remove("ImageTags");
            }
            if let Ok(overrides) = state.played_overrides.lock()
                && let Some(played) = overrides.get(&id)
                && let Some(user_data) = body.get_mut("UserData").and_then(Value::as_object_mut)
            {
                user_data.insert("Played".into(), json!(played));
            }
            if let Ok(overrides) = state.favorite_overrides.lock()
                && let Some(favorite) = overrides.get(&id)
                && let Some(user_data) = body.get_mut("UserData").and_then(Value::as_object_mut)
            {
                user_data.insert("IsFavorite".into(), json!(favorite));
            }
            metadata::apply_edits(&state, &id, &mut body);
            if item.kind.media_type() == Some("Video")
                && let Some(object) = body.as_object_mut()
            {
                if let Some(trickplay) = subtitles::trickplay_manifest(item) {
                    object.insert("Trickplay".into(), trickplay);
                }
                let mut streams = media_streams(true).as_array().cloned().unwrap_or_default();
                streams.extend(state.subtitle_store.streams(&id));
                object.insert(
                    "MediaSources".into(),
                    json!([{ "Id": id, "Name": item.name, "MediaStreams": streams }]),
                );
            }
            json_ok(body)
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn media_segments(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(query): Query<MediaSegmentsQuery>,
    RawQuery(raw_query): RawQuery,
) -> Response {
    record_request(
        &state,
        "GET",
        &format!("/MediaSegments/{id}"),
        raw_query.as_deref(),
    );
    if !is_authorized(&headers) {
        return unauthorized();
    }
    let Some(item) = state.catalog.get(&id) else {
        return StatusCode::NOT_FOUND.into_response();
    };

    let mut items = if item.kind == Kind::Episode {
        vec![
            json!({
                "Id": format!("{id}-intro"),
                "ItemId": id.clone(),
                "Type": "Intro",
                "StartTicks": 0,
                "EndTicks": 120 * 10_000_000_i64,
            }),
            json!({
                "Id": format!("{id}-outro"),
                "ItemId": id.clone(),
                "Type": "Outro",
                "StartTicks": 1_200 * 10_000_000_i64,
                "EndTicks": 1_260 * 10_000_000_i64,
            }),
        ]
    } else {
        Vec::new()
    };
    if let Some(include) = query.include_segment_types {
        let include = include
            .split(',')
            .map(str::trim)
            .collect::<std::collections::HashSet<_>>();
        items.retain(|segment| {
            segment["Type"]
                .as_str()
                .is_some_and(|segment_type| include.contains(segment_type))
        });
    }
    let total = items.len();
    json_ok(json!({
        "Items": items,
        "TotalRecordCount": total,
        "StartIndex": 0,
    }))
}

async fn playback_info(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    _body: Bytes,
) -> Response {
    if !is_authorized(&headers) {
        return unauthorized();
    }
    let Some(item) = state.catalog.get(&id) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let play_session_id = format!("mock-play-{}", seed::image_tag(&id, "PlaySession"));
    let is_video = item.kind.media_type() == Some("Video");
    let transcode_only = is_video && state.transcode_only_mkv;
    let container = if transcode_only {
        "mkv"
    } else if is_video {
        "mp4"
    } else {
        "mp3"
    };
    let mut source = json!({
        "Protocol": "File",
        "Id": id,
        "Path": format!("/mock-media/{id}.{container}"),
        "Type": "Default",
        "Container": container,
        "Name": item.name,
        "IsRemote": false,
        "ETag": seed::image_tag(&id, "etag"),
        "RunTimeTicks": item.run_time_ticks,
        "Size": STREAM_LEN,
        "Bitrate": 2_000_000,
        "SupportsDirectPlay": !transcode_only,
        "SupportsDirectStream": !transcode_only,
        "SupportsTranscoding": is_video,
        "IsInfiniteStream": false,
        "RequiresOpening": false,
        "RequiresClosing": false,
        "RequiresLooping": false,
        "SupportsProbing": true,
        "RequiredHttpHeaders": {},
        "MediaStreams": media_streams(is_video),
        "DefaultAudioStreamIndex": 1,
    });
    if is_video {
        source["TranscodingUrl"] = json!(format!(
            "/Videos/{id}/master.m3u8?MediaSourceId={id}&PlaySessionId={play_session_id}&VideoCodec=h264&AudioCodec=aac"
        ));
        source["TranscodingSubProtocol"] = json!("hls");
        source["TranscodingContainer"] = json!("ts");
    }
    json_ok(json!({ "MediaSources": [source], "PlaySessionId": play_session_id }))
}

fn media_streams(is_video: bool) -> Value {
    let audio_codec = if is_video { "aac" } else { "mp3" };
    let audio = json!({
        "Type": "Audio", "Codec": audio_codec, "Index": 1, "Channels": 2,
        "SampleRate": 48000, "Language": "eng", "IsDefault": true,
    });
    if is_video {
        json!([
            {"Type": "Video", "Codec": "h264", "Index": 0, "Width": 1920, "Height": 1080,
             "AspectRatio": "16:9", "IsDefault": true},
            audio,
        ])
    } else {
        json!([audio])
    }
}

async fn image(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((id, image_type)): Path<(String, String)>,
    RawQuery(raw_query): RawQuery,
) -> Response {
    record_request(
        &state,
        "GET",
        &format!("/Items/{id}/Images/{image_type}"),
        raw_query.as_deref(),
    );
    if !is_authorized(&headers) {
        return unauthorized();
    }
    let item = state.catalog.get(&id);
    let is_live_tv_channel = live_tv::channel_id_index(&id).is_some();
    if item.is_none() && !is_live_tv_channel {
        return StatusCode::NOT_FOUND.into_response();
    }
    let available = match image_type.as_str() {
        "Primary" => {
            !state.primary_images_unavailable
                && (item.is_some() || live_tv::channel_has_primary_image(&id))
        }
        "Backdrop" => item.is_some_and(|item| item.has_backdrop),
        _ => false,
    };
    if !available {
        return StatusCode::NOT_FOUND.into_response();
    }
    let rgb = color_for(&format!("{id}:{image_type}"));
    let body = png::solid_png(32, 32, rgb);
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, HeaderValue::from_static("image/png"))],
        body,
    )
        .into_response()
}

fn color_for(seed_text: &str) -> [u8; 3] {
    let hash = seed::fnv1a64(seed_text);
    [
        ((hash >> 16) as u8) / 2 + 64,
        ((hash >> 24) as u8) / 2 + 64,
        ((hash >> 32) as u8) / 2 + 64,
    ]
}

async fn master_playlist(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if !is_authorized(&headers) {
        return unauthorized();
    }
    match state.catalog.get(&id) {
        Some(item) if item.kind.media_type() == Some("Video") => {}
        Some(_) => return StatusCode::BAD_REQUEST.into_response(),
        None => return StatusCode::NOT_FOUND.into_response(),
    }
    // One variant, not a bitrate ladder. By default it points back at the placeholder media
    // body. In the transcode-only scenario it leads to a media playlist with segments.
    let variant = if state.transcode_only_mkv {
        format!("main.m3u8?MediaSourceId={id}")
    } else {
        format!("/Videos/{id}/stream.ts?static=false")
    };
    let body = format!(
        "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-STREAM-INF:BANDWIDTH=2000000,RESOLUTION=1920x1080\n{variant}\n"
    );
    (
        StatusCode::OK,
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/vnd.apple.mpegurl"),
        )],
        body,
    )
        .into_response()
}

/// Number of segments in the transcode-only scenario's media playlist.
const HLS_SEGMENT_COUNT: usize = 3;

async fn media_playlist(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if !is_authorized(&headers) {
        return unauthorized();
    }
    match state.catalog.get(&id) {
        Some(item) if item.kind.media_type() == Some("Video") => {}
        Some(_) => return StatusCode::BAD_REQUEST.into_response(),
        None => return StatusCode::NOT_FOUND.into_response(),
    }
    let mut body = String::from(
        "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:6\n#EXT-X-MEDIA-SEQUENCE:0\n#EXT-X-PLAYLIST-TYPE:VOD\n",
    );
    for index in 0..HLS_SEGMENT_COUNT {
        body.push_str(&format!(
            "#EXTINF:6.000,\nhls1/main/{index}.ts?runtimeTicks={}\n",
            index * 60_000_000
        ));
    }
    body.push_str("#EXT-X-ENDLIST\n");
    (
        StatusCode::OK,
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/vnd.apple.mpegurl"),
        )],
        body,
    )
        .into_response()
}

async fn hls_segment(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((id, playlist, segment)): Path<(String, String, String)>,
) -> Response {
    if !is_authorized(&headers) {
        return unauthorized();
    }
    if state.catalog.get(&id).is_none() || playlist != "main" {
        return StatusCode::NOT_FOUND.into_response();
    }
    let index = segment
        .strip_suffix(".ts")
        .and_then(|digits| digits.parse::<usize>().ok());
    if index.is_none_or(|index| index >= HLS_SEGMENT_COUNT) {
        return StatusCode::NOT_FOUND.into_response();
    }
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, HeaderValue::from_static("video/mp2t"))],
        Body::from(placeholder_media()),
    )
        .into_response()
}

async fn stream(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((id, stream_name)): Path<(String, String)>,
) -> Response {
    if !is_authorized(&headers) {
        return unauthorized();
    }
    let is_stream = stream_name == "stream" || stream_name.starts_with("stream.");
    if !is_stream {
        return StatusCode::NOT_FOUND.into_response();
    }
    media_response(&state, &headers, &id)
}

/// The original-file download route the download manager uses.
async fn download(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if !is_authorized(&headers) {
        return unauthorized();
    }
    media_response(&state, &headers, &id)
}

fn media_response(state: &AppState, headers: &HeaderMap, id: &str) -> Response {
    let Some(item) = state.catalog.get(id) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let body = placeholder_media();
    let total = body.len() as u64;
    let range = headers
        .get(header::RANGE)
        .and_then(|value| value.to_str().ok())
        .map(|value| parse_range(value, total));
    let etag = HeaderValue::from_static("\"mock-media-v1\"");

    let content_type = if item.kind.media_type() == Some("Audio") {
        HeaderValue::from_static("audio/mpeg")
    } else {
        HeaderValue::from_static("video/mp4")
    };
    match range {
        None | Some(RangeOutcome::Ignored) => {
            let mut response = (StatusCode::OK, Body::from(body)).into_response();
            let headers = response.headers_mut();
            headers.insert(header::CONTENT_TYPE, content_type);
            headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
            headers.insert(header::ETAG, etag);
            response
        }
        Some(RangeOutcome::Unsatisfiable) => {
            let mut response = StatusCode::RANGE_NOT_SATISFIABLE.into_response();
            if let Ok(value) = HeaderValue::from_str(&format!("bytes */{total}")) {
                response.headers_mut().insert(header::CONTENT_RANGE, value);
            }
            response
        }
        Some(RangeOutcome::Satisfiable(start, end)) => {
            let slice = body[start as usize..=end as usize].to_vec();
            let mut response = (StatusCode::PARTIAL_CONTENT, Body::from(slice)).into_response();
            let headers = response.headers_mut();
            headers.insert(header::CONTENT_TYPE, content_type);
            headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
            headers.insert(header::ETAG, etag);
            if let Ok(value) = HeaderValue::from_str(&format!("bytes {start}-{end}/{total}")) {
                headers.insert(header::CONTENT_RANGE, value);
            }
            response
        }
    }
}

fn placeholder_media() -> Vec<u8> {
    (0..STREAM_LEN)
        .map(|index| {
            (index as u32)
                .wrapping_mul(31)
                .wrapping_add((index >> 8) as u32) as u8
        })
        .collect()
}

#[derive(Debug, PartialEq, Eq)]
enum RangeOutcome {
    /// Inclusive byte offsets.
    Satisfiable(u64, u64),
    Unsatisfiable,
    /// Malformed or multi-range header: the full body is served instead.
    Ignored,
}

/// Parse a single `bytes=` range (RFC 9110). Multi-range and non-`bytes`
/// headers are ignored, so the caller serves the full body.
fn parse_range(header_value: &str, total: u64) -> RangeOutcome {
    let spec = match header_value.strip_prefix("bytes=") {
        Some(spec) if !spec.contains(',') => spec.trim(),
        _ => return RangeOutcome::Ignored,
    };
    let Some((start_text, end_text)) = spec.split_once('-') else {
        return RangeOutcome::Unsatisfiable;
    };
    let (start, end) = if start_text.is_empty() {
        // Suffix range: last N bytes.
        let Ok(suffix) = end_text.parse::<u64>() else {
            return RangeOutcome::Unsatisfiable;
        };
        if suffix == 0 || total == 0 {
            return RangeOutcome::Unsatisfiable;
        }
        (total.saturating_sub(suffix), total - 1)
    } else {
        let Ok(start) = start_text.parse::<u64>() else {
            return RangeOutcome::Unsatisfiable;
        };
        let end = if end_text.is_empty() {
            total.saturating_sub(1)
        } else {
            match end_text.parse::<u64>() {
                Ok(end) => end.min(total.saturating_sub(1)),
                Err(_) => return RangeOutcome::Unsatisfiable,
            }
        };
        (start, end)
    };
    if total == 0 || start >= total || start > end {
        return RangeOutcome::Unsatisfiable;
    }
    RangeOutcome::Satisfiable(start, end)
}

async fn report_playing(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    record_report(&state, &headers, "Playing", &body)
}

async fn report_progress(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    record_report(&state, &headers, "Progress", &body)
}

async fn report_stopped(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    record_report(&state, &headers, "Stopped", &body)
}

fn record_report(state: &AppState, headers: &HeaderMap, event: &str, body: &Bytes) -> Response {
    if !is_authorized(headers) {
        return unauthorized();
    }
    if event == "Progress"
        && state
            .fail_progress_posts_remaining
            .try_update(Ordering::AcqRel, Ordering::Acquire, |remaining| {
                remaining.checked_sub(1)
            })
            .is_ok()
    {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    let payload = match parse_body(body) {
        Ok(payload) => payload,
        Err(status) => return status.into_response(),
    };
    let Ok(mut sessions) = state.sessions.lock() else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let sequence = sessions.len() + 1;
    sessions.push(json!({ "Sequence": sequence, "Event": event, "Body": payload }));
    StatusCode::NO_CONTENT.into_response()
}

async fn session_log(State(state): State<Arc<AppState>>) -> Response {
    let Ok(sessions) = state.sessions.lock() else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    json_ok(Value::Array(sessions.clone()))
}

async fn capability_log(State(state): State<Arc<AppState>>) -> Response {
    let Ok(capabilities) = state.capabilities.lock() else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    json_ok(Value::Array(capabilities.clone()))
}

async fn advertise_capabilities(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    record_request(&state, "POST", "/Sessions/Capabilities/Full", None);
    if !is_authorized(&headers) {
        return unauthorized();
    }
    let Ok(body) = parse_body(&body) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let Ok(mut capabilities) = state.capabilities.lock() else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    capabilities.push(body);
    StatusCode::NO_CONTENT.into_response()
}

#[cfg(test)]
mod tests {
    use super::{
        MockConfig, RangeOutcome, parse_items_query, parse_range, spawn, token_from_header,
    };
    use reqwest::header::{AUTHORIZATION, HeaderValue};

    const AUTHORIZATION_VALUE: &str = "MediaBrowser Token=mock-access-token-0001";

    #[tokio::test]
    async fn configured_public_info_failures_then_recover() {
        let server = spawn(MockConfig {
            public_info_failures: 1,
            ..MockConfig::default()
        })
        .await
        .expect("mock server starts");
        let endpoint = format!("{}/System/Info/Public", server.base_url());
        let client = reqwest::Client::new();

        let failed = client
            .get(&endpoint)
            .send()
            .await
            .expect("first request completes");
        assert_eq!(failed.status(), reqwest::StatusCode::INTERNAL_SERVER_ERROR);
        let recovered = client
            .get(&endpoint)
            .send()
            .await
            .expect("second request completes");
        assert_eq!(recovered.status(), reqwest::StatusCode::OK);
        server.shutdown().await;
    }

    #[tokio::test]
    async fn configured_progress_failures_are_bounded_and_not_recorded() {
        let server = spawn(MockConfig {
            fail_progress_posts: 2,
            ..MockConfig::default()
        })
        .await
        .expect("mock server starts");
        let client = reqwest::Client::new();
        let endpoint = format!("{}/Sessions/Playing/Progress", server.base_url());

        let unauthorized = client
            .post(&endpoint)
            .json(&serde_json::json!({ "PositionTicks": 1 }))
            .send()
            .await
            .expect("unauthenticated request completes");
        assert_eq!(unauthorized.status(), reqwest::StatusCode::UNAUTHORIZED);

        for expected_status in [
            reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            reqwest::StatusCode::NO_CONTENT,
        ] {
            let response = client
                .post(&endpoint)
                .header(AUTHORIZATION, HeaderValue::from_static(AUTHORIZATION_VALUE))
                .json(&serde_json::json!({ "PositionTicks": 42 }))
                .send()
                .await
                .expect("authenticated request completes");
            assert_eq!(response.status(), expected_status);
        }

        let recorded = client
            .get(format!("{}/__mock/sessions", server.base_url()))
            .send()
            .await
            .expect("session log request completes")
            .json::<serde_json::Value>()
            .await
            .expect("session log is JSON");
        assert_eq!(recorded.as_array().map(Vec::len), Some(1));
        assert_eq!(recorded[0]["Event"], "Progress");
        assert_eq!(recorded[0]["Body"]["PositionTicks"], 42);
        server.shutdown().await;
    }

    #[tokio::test]
    async fn unset_progress_failure_option_preserves_normal_behavior() {
        let server = spawn(MockConfig::default())
            .await
            .expect("mock server starts");
        let response = reqwest::Client::new()
            .post(format!("{}/Sessions/Playing/Progress", server.base_url()))
            .header(AUTHORIZATION, HeaderValue::from_static(AUTHORIZATION_VALUE))
            .json(&serde_json::json!({ "PositionTicks": 99 }))
            .send()
            .await
            .expect("progress request completes");

        assert_eq!(response.status(), reqwest::StatusCode::NO_CONTENT);
        server.shutdown().await;
    }

    #[test]
    fn item_query_parser_preserves_repeated_and_comma_separated_values() {
        let query = parse_items_query(Some(
            "sortBy=DateCreated&sortBy=PremiereDate&filters=IsUnplayed%2CIsFavorite&genres=Drama&genres=Comedy&years=2020&years=2024",
        ))
        .expect("valid Jellyfin query parses");

        assert_eq!(
            query.sort_by,
            Some(vec!["DateCreated".to_owned(), "PremiereDate".to_owned()])
        );
        assert_eq!(
            query.filters,
            Some(vec!["IsUnplayed,IsFavorite".to_owned()])
        );
        assert_eq!(
            query.genres,
            Some(vec!["Drama".to_owned(), "Comedy".to_owned()])
        );
        assert_eq!(
            query.years,
            Some(vec!["2020".to_owned(), "2024".to_owned()])
        );
    }

    #[test]
    fn token_parsing_handles_quoted_and_bare_values() {
        assert_eq!(
            token_from_header("MediaBrowser Client=\"x\", Token=\"abc-123\""),
            Some("abc-123")
        );
        assert_eq!(
            token_from_header("MediaBrowser Token=abc, Client=\"x\""),
            Some("abc")
        );
        assert_eq!(token_from_header("MediaBrowser Client=\"x\""), None);
    }

    #[test]
    fn range_forms_are_parsed() {
        assert_eq!(
            parse_range("bytes=0-9", 100),
            RangeOutcome::Satisfiable(0, 9)
        );
        assert_eq!(
            parse_range("bytes=90-", 100),
            RangeOutcome::Satisfiable(90, 99)
        );
        assert_eq!(
            parse_range("bytes=-10", 100),
            RangeOutcome::Satisfiable(90, 99)
        );
        assert_eq!(
            parse_range("bytes=50-500", 100),
            RangeOutcome::Satisfiable(50, 99)
        );
        assert_eq!(parse_range("bytes=100-", 100), RangeOutcome::Unsatisfiable);
        assert_eq!(parse_range("bytes=9-3", 100), RangeOutcome::Unsatisfiable);
        assert_eq!(parse_range("bytes=0-1,4-5", 100), RangeOutcome::Ignored);
        assert_eq!(parse_range("items=0-1", 100), RangeOutcome::Ignored);
    }
}
