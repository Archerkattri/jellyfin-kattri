//! Mock remote subtitle search and download (`GET`/`POST /Items/{id}/RemoteSearch/Subtitles/..`),
//! the cultures list (`GET /Localization/Cultures`), downloaded subtitle files
//! (`GET /Videos/{id}/{sourceId}/Subtitles/{index}/Stream.srt`) and trickplay tile sheets
//! (`GET /Videos/{id}/Trickplay/{width}/{index}.jpg`). A download adds an external `srt`
//! stream to the item's media source, which `GET /Items/{id}` then lists. Nothing is fetched
//! from a real provider.

use std::{collections::HashMap, sync::Arc, sync::Mutex};

use axum::{
    Router,
    extract::{Path, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use serde_json::{Value, json};

use crate::{AppState, is_authorized, json_ok, png, record_request, unauthorized};

/// Interval between two mock trickplay thumbnails, in milliseconds.
const TRICKPLAY_INTERVAL_MS: i64 = 10_000;
/// Mock thumbnail size and sheet layout. Small, because the placeholder PNG is uncompressed.
const THUMB_WIDTH: u32 = 64;
const THUMB_HEIGHT: u32 = 36;
const SHEET_COLUMNS: u32 = 10;
const SHEET_ROWS: u32 = 10;
/// First stream index used for downloaded subtitles; the seeded streams use 0 and 1.
const FIRST_DOWNLOADED_INDEX: i64 = 10;

type DownloadedSubtitle = (i64, String, String);

/// Subtitles downloaded per item: `(index, language, subtitle id)`.
#[derive(Default)]
pub(crate) struct SubtitleStore {
    downloaded: Mutex<HashMap<String, Vec<DownloadedSubtitle>>>,
}

impl SubtitleStore {
    /// External subtitle streams added to `item_id` by downloads, as media stream DTOs.
    pub(crate) fn streams(&self, item_id: &str) -> Vec<Value> {
        let Ok(downloaded) = self.downloaded.lock() else {
            return Vec::new();
        };
        downloaded
            .get(item_id)
            .into_iter()
            .flatten()
            .map(|(index, language, _)| {
                json!({
                    "Type": "Subtitle", "Codec": "srt", "Index": index, "Language": language,
                    "DisplayTitle": format!("{} - SRT - External", language.to_uppercase()),
                    "IsExternal": true, "IsTextSubtitleStream": true,
                    "SupportsExternalStream": true, "DeliveryMethod": "External",
                })
            })
            .collect()
    }
}

pub(crate) fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route(
            "/Items/{id}/RemoteSearch/Subtitles/{key}",
            get(search).post(download),
        )
        .route("/Localization/Cultures", get(cultures))
        .route(
            "/Videos/{id}/{source}/Subtitles/{index}/{file}",
            get(subtitle_file),
        )
        .route("/Videos/{id}/Trickplay/{width}/{file}", get(trickplay_tile))
}

/// The item's `Trickplay` manifest: one 64 px resolution for its media source, or `None` for
/// an item that is not a video or has no runtime.
pub(crate) fn trickplay_manifest(item: &crate::seed::Item) -> Option<Value> {
    if item.kind.media_type() != Some("Video") {
        return None;
    }
    let runtime_ms = item.run_time_ticks? / 10_000;
    let count = (runtime_ms / TRICKPLAY_INTERVAL_MS).max(1);
    Some(json!({
        item.id.clone(): {
            THUMB_WIDTH.to_string(): {
                "Width": THUMB_WIDTH, "Height": THUMB_HEIGHT,
                "TileWidth": SHEET_COLUMNS, "TileHeight": SHEET_ROWS,
                "ThumbnailCount": count, "Interval": TRICKPLAY_INTERVAL_MS, "Bandwidth": 10_000,
            }
        }
    }))
}

/// Two results for English, one for any other language, none for `zxx` (no linguistic
/// content), so the empty state can be exercised.
fn results_for(item_id: &str, language: &str) -> Vec<Value> {
    if language == "zxx" {
        return Vec::new();
    }
    let mut results = vec![json!({
        "Id": format!("mock-{language}-1"),
        "ProviderName": "Mock Subtitles",
        "Name": format!("{item_id}.{language}.srt"),
        "Format": "srt",
        "ThreeLetterISOLanguageName": language,
        "CommunityRating": 8.5,
        "DownloadCount": 1200,
        "IsHashMatch": true,
        "HearingImpaired": false,
    })];
    if language == "eng" {
        results.push(json!({
            "Id": "mock-eng-2",
            "ProviderName": "Other Mock Provider",
            "Name": format!("{item_id}.eng.sdh.srt"),
            "Format": "srt",
            "ThreeLetterISOLanguageName": "eng",
            "Comment": "Hearing impaired",
            "DownloadCount": 40,
            "HearingImpaired": true,
        }));
    }
    results
}

async fn search(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((id, language)): Path<(String, String)>,
) -> Response {
    record_request(
        &state,
        "GET",
        &format!("/Items/{id}/RemoteSearch/Subtitles/{language}"),
        None,
    );
    if !is_authorized(&headers) {
        return unauthorized();
    }
    if state.catalog.get(&id).is_none() {
        return StatusCode::NOT_FOUND.into_response();
    }
    json_ok(Value::Array(results_for(&id, &language)))
}

async fn download(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((id, subtitle_id)): Path<(String, String)>,
) -> Response {
    record_request(
        &state,
        "POST",
        &format!("/Items/{id}/RemoteSearch/Subtitles/{subtitle_id}"),
        None,
    );
    if !is_authorized(&headers) {
        return unauthorized();
    }
    if state.catalog.get(&id).is_none() {
        return StatusCode::NOT_FOUND.into_response();
    }
    let Some(language) = subtitle_id
        .strip_prefix("mock-")
        .and_then(|rest| rest.split('-').next())
        .filter(|language| {
            results_for(&id, language)
                .iter()
                .any(|result| result["Id"] == subtitle_id.as_str())
        })
        .map(str::to_owned)
    else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Ok(mut downloaded) = state.subtitle_store.downloaded.lock() else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let entries = downloaded.entry(id).or_default();
    if !entries
        .iter()
        .any(|(_, _, existing)| *existing == subtitle_id)
    {
        let index = FIRST_DOWNLOADED_INDEX + entries.len() as i64;
        entries.push((index, language, subtitle_id));
    }
    StatusCode::NO_CONTENT.into_response()
}

async fn cultures(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    record_request(&state, "GET", "/Localization/Cultures", None);
    if !is_authorized(&headers) {
        return unauthorized();
    }
    json_ok(json!([
        {"Name": "en", "DisplayName": "English", "TwoLetterISOLanguageName": "en",
         "ThreeLetterISOLanguageName": "eng", "ThreeLetterISOLanguageNames": ["eng"]},
        {"Name": "fr", "DisplayName": "French", "TwoLetterISOLanguageName": "fr",
         "ThreeLetterISOLanguageName": "fre", "ThreeLetterISOLanguageNames": ["fre", "fra"]},
        {"Name": "de", "DisplayName": "German", "TwoLetterISOLanguageName": "de",
         "ThreeLetterISOLanguageName": "ger", "ThreeLetterISOLanguageNames": ["ger", "deu"]},
    ]))
}

async fn subtitle_file(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((id, source, index, file)): Path<(String, String, i64, String)>,
) -> Response {
    record_request(
        &state,
        "GET",
        &format!("/Videos/{id}/{source}/Subtitles/{index}/{file}"),
        None,
    );
    if !is_authorized(&headers) {
        return unauthorized();
    }
    let known = state
        .subtitle_store
        .downloaded
        .lock()
        .ok()
        .and_then(|downloaded| {
            downloaded
                .get(&id)
                .and_then(|entries| entries.iter().find(|(i, _, _)| *i == index).cloned())
        });
    let Some((_, language, _)) = known.filter(|_| source == id && file == "Stream.srt") else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let body = format!("1\n00:00:01,000 --> 00:00:04,000\nMock {language} subtitle\n");
    (
        StatusCode::OK,
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/plain; charset=utf-8"),
        )],
        body,
    )
        .into_response()
}

async fn trickplay_tile(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((id, width, file)): Path<(String, u32, String)>,
) -> Response {
    record_request(
        &state,
        "GET",
        &format!("/Videos/{id}/Trickplay/{width}/{file}"),
        None,
    );
    if !is_authorized(&headers) {
        return unauthorized();
    }
    let has_manifest = state
        .catalog
        .get(&id)
        .and_then(trickplay_manifest)
        .is_some();
    let sheet = file
        .strip_suffix(".jpg")
        .and_then(|index| index.parse::<u32>().ok());
    let (true, Some(sheet), THUMB_WIDTH) = (has_manifest, sheet, width) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    // A solid sheet per index; the client accepts PNG or JPEG bytes.
    let shade = u8::try_from(40 + (sheet * 50) % 200).unwrap_or(40);
    let body = png::solid_png(
        THUMB_WIDTH * SHEET_COLUMNS,
        THUMB_HEIGHT * SHEET_ROWS,
        [shade, 90, 160],
    );
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, HeaderValue::from_static("image/jpeg"))],
        body,
    )
        .into_response()
}
