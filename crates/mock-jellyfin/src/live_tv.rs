//! Mock Live TV routes: `GET /LiveTv/Channels` (OpenAPI `GetLiveTvChannels`)
//! and `GET /LiveTv/Programs` (OpenAPI `GetLiveTvPrograms`). This is not a real
//! server.
//!
//! The mock serves a fixed lineup of 150 numbered channels in server order, so
//! paging can be tested across two 100-item pages. Requests that start past the
//! end get an empty page with the full total.
//!
//! Each channel has one-hour programs from 2026-10-14T00:00Z to
//! 2026-10-17T00:00Z. A `minStartDate`/`maxStartDate` window keeps programs
//! whose start is at least the minimum and before the maximum. The maximum is
//! exclusive here, which is [UNCONFIRMED] against the real server. `channelIds`
//! is read as comma-separated values or repeated keys.

use std::{collections::HashMap, sync::Arc};

use axum::{
    Router,
    extract::{RawQuery, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
};
use serde_json::{Value, json};

use crate::{AppState, is_authorized, json_ok, record_request, unauthorized};

/// Number of channels in the mock lineup.
pub(crate) const CHANNEL_COUNT: usize = 150;

/// Whether `id` is one of the deterministic channel IDs in the mock lineup.
pub(crate) fn channel_id_index(id: &str) -> Option<usize> {
    let value = u128::from_str_radix(id, 16).ok()?;
    let index = usize::try_from(value).ok()?.checked_sub(1)?;
    (index < CHANNEL_COUNT && channel_id(index) == id).then_some(index)
}

/// Every other channel has a seeded primary image; the rest exercise 404 fallback.
pub(crate) fn channel_has_primary_image(id: &str) -> bool {
    channel_id_index(id).is_some_and(|index| index % 2 == 0)
}

/// One-hour program slots per channel: 2026-10-14T00:00Z through 2026-10-17T00:00Z.
const PROGRAM_SLOTS: usize = 72;
const FIRST_PROGRAM_DAY: usize = 14;

/// Routes for this module. Merge the result into the main mock router.
pub(crate) fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/LiveTv/Channels", get(channels))
        .route("/LiveTv/Programs", get(programs))
}

async fn channels(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    RawQuery(raw_query): RawQuery,
) -> Response {
    record_request(&state, "GET", "/LiveTv/Channels", raw_query.as_deref());
    if !is_authorized(&headers) {
        return unauthorized();
    }
    let params = query_values(raw_query.as_deref());
    let (Ok(start), Ok(limit)) = (
        usize_param(&params, "startIndex"),
        usize_param(&params, "limit"),
    ) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let start = start.unwrap_or(0).min(CHANNEL_COUNT);
    let end = limit.map_or(CHANNEL_COUNT, |limit| {
        start.saturating_add(limit).min(CHANNEL_COUNT)
    });
    let items: Vec<Value> = (start..end).map(channel_json).collect();
    json_ok(json!({
        "Items": items,
        "TotalRecordCount": CHANNEL_COUNT,
        "StartIndex": start,
    }))
}

async fn programs(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    RawQuery(raw_query): RawQuery,
) -> Response {
    record_request(&state, "GET", "/LiveTv/Programs", raw_query.as_deref());
    if !is_authorized(&headers) {
        return unauthorized();
    }
    let params = query_values(raw_query.as_deref());
    let (Ok(start_index), Ok(limit)) = (
        usize_param(&params, "startIndex"),
        usize_param(&params, "limit"),
    ) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let min_start = first_value(&params, "minStartDate");
    let max_start = first_value(&params, "maxStartDate");
    let channel_filter = channel_ids(&params);

    let mut matching: Vec<Value> = Vec::new();
    for channel in 0..CHANNEL_COUNT {
        if !channel_filter.is_empty() && !channel_filter.contains(&channel_id(channel)) {
            continue;
        }
        for slot in 0..PROGRAM_SLOTS {
            let start = slot_start(slot);
            if min_start.as_deref().is_some_and(|min| start.as_str() < min) {
                continue;
            }
            if max_start
                .as_deref()
                .is_some_and(|max| start.as_str() >= max)
            {
                continue;
            }
            matching.push(program_json(channel, slot));
        }
    }
    let total = matching.len();
    let start = start_index.unwrap_or(0).min(total);
    let end = limit.map_or(total, |limit| start.saturating_add(limit).min(total));
    json_ok(json!({
        "Items": matching[start..end].to_vec(),
        "TotalRecordCount": total,
        "StartIndex": start,
    }))
}

/// Channel ID for `index` (0-based). A 32-hex-digit GUID-shaped string.
fn channel_id(index: usize) -> String {
    format!("{:032x}", index + 1)
}

/// Channel `index` (0-based) of the lineup. Channel numbers start at 1.
fn channel_json(index: usize) -> Value {
    json!({
        "Id": channel_id(index),
        "Name": format!("Mock Channel {:03}", index + 1),
        "Type": "TvChannel",
        "ChannelType": "TV",
        "Number": (index + 1).to_string(),
        "MediaType": "Video",
        "UserData": {"IsFavorite": index.is_multiple_of(10)},
        "ImageTags": if index.is_multiple_of(2) {
            json!({"Primary": format!("channel-image-{:03}", index + 1)})
        } else {
            json!({})
        },
    })
}

/// UTC start of program slot `slot`, as `YYYY-MM-DDTHH:MM:SSZ`. Fixed width, so
/// string order is time order. Slot `PROGRAM_SLOTS` is the end of the last slot.
fn slot_start(slot: usize) -> String {
    format!(
        "2026-10-{:02}T{:02}:00:00Z",
        FIRST_PROGRAM_DAY + slot / 24,
        slot % 24
    )
}

/// One-hour program in slot `slot` on channel `channel` (both 0-based).
fn program_json(channel: usize, slot: usize) -> Value {
    json!({
        "Id": format!("{:016x}{:016x}", channel + 1, slot + 1),
        "Name": format!("Mock Program {:03} {:02}:00", channel + 1, slot % 24),
        "Type": "Program",
        "ChannelId": channel_id(channel),
        "ChannelType": "TV",
        "StartDate": slot_start(slot),
        "EndDate": slot_start(slot + 1),
    })
}

fn query_values(raw_query: Option<&str>) -> HashMap<String, Vec<String>> {
    let mut values: HashMap<String, Vec<String>> = HashMap::new();
    for (key, value) in form_urlencoded::parse(raw_query.unwrap_or_default().as_bytes()) {
        values
            .entry(key.into_owned())
            .or_default()
            .push(value.into_owned());
    }
    values
}

fn first_value(params: &HashMap<String, Vec<String>>, key: &str) -> Option<String> {
    params.get(key).and_then(|values| values.first()).cloned()
}

/// Every `channelIds` value, split on commas. Empty means no channel filter.
fn channel_ids(params: &HashMap<String, Vec<String>>) -> Vec<String> {
    params
        .get("channelIds")
        .into_iter()
        .flatten()
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
        .collect()
}

/// First value of `key` as a `usize`. `Err` means the value is present but not a number.
fn usize_param(params: &HashMap<String, Vec<String>>, key: &str) -> Result<Option<usize>, ()> {
    params
        .get(key)
        .and_then(|values| values.first())
        .map(|value| value.parse::<usize>().map_err(|_| ()))
        .transpose()
}
