//! Trickplay (seek-bar preview) data for a video: the per-source manifest from the item's
//! `Trickplay` field (`GET /Items/{id}?fields=Trickplay`) and its JPEG tile sheets
//! (`GET /Videos/{id}/Trickplay/{width}/{index}.jpg`). Each sheet holds `TileWidth` x
//! `TileHeight` thumbnails taken every `Interval` milliseconds.

use std::collections::HashMap;
use std::time::Duration;

use reqwest::{Method, StatusCode, header};
use serde::Deserialize;

use crate::{ApiError, JellyfinClient, Session, home};

/// Largest tile sheet accepted on the wire. A 10x10 sheet of 320 px thumbnails is well under
/// one MiB; the cap only guards against a misbehaving server.
pub const MAX_TRICKPLAY_TILE_BYTES: usize = 8 * 1024 * 1024;

/// Most resolutions kept per media source.
const MAX_RESOLUTIONS: usize = 16;

/// One trickplay resolution of one media source.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "PascalCase", default)]
pub struct TrickplayInfo {
    /// Width of one thumbnail in pixels; also the `{width}` of the tile URL.
    pub width: u32,
    /// Height of one thumbnail in pixels.
    pub height: u32,
    /// Thumbnails per row of a sheet.
    pub tile_width: u32,
    /// Thumbnails per column of a sheet.
    pub tile_height: u32,
    /// Thumbnails over the whole video.
    pub thumbnail_count: u32,
    /// Milliseconds between two thumbnails.
    pub interval: u32,
}

impl TrickplayInfo {
    /// Whether every value is usable: no zero sizes, and a sheet no larger than 64x64 thumbnails
    /// of at most 1920x1080 each.
    pub fn is_valid(&self) -> bool {
        (1..=1920).contains(&self.width)
            && (1..=1080).contains(&self.height)
            && (1..=64).contains(&self.tile_width)
            && (1..=64).contains(&self.tile_height)
            && self.thumbnail_count > 0
            && self.interval > 0
    }

    /// The thumbnail shown at `position`: the sheet index and the thumbnail's column and row in
    /// it. Positions past the last thumbnail show the last one.
    pub fn locate(&self, position: Duration) -> Option<TrickplayCell> {
        if !self.is_valid() {
            return None;
        }
        let per_sheet = self.tile_width * self.tile_height;
        let thumbnail = u32::try_from(position.as_millis() / u128::from(self.interval))
            .unwrap_or(u32::MAX)
            .min(self.thumbnail_count - 1);
        let offset = thumbnail % per_sheet;
        Some(TrickplayCell {
            sheet: thumbnail / per_sheet,
            column: offset % self.tile_width,
            row: offset / self.tile_width,
        })
    }

    /// How many sheets the video has.
    pub fn sheet_count(&self) -> u32 {
        let per_sheet = self.tile_width.saturating_mul(self.tile_height).max(1);
        self.thumbnail_count.div_ceil(per_sheet)
    }
}

/// Where one thumbnail sits: sheet `sheet`, at `column` and `row` in thumbnails.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TrickplayCell {
    pub sheet: u32,
    pub column: u32,
    pub row: u32,
}

/// The resolution Jellyfin Web picks: the widest one no wider than `max_width`, or the narrowest
/// one when every resolution is wider.
pub fn pick_resolution(resolutions: &[TrickplayInfo], max_width: u32) -> Option<TrickplayInfo> {
    let usable = resolutions.iter().filter(|info| info.is_valid());
    let fitting = usable
        .clone()
        .filter(|info| info.width <= max_width)
        .max_by_key(|info| info.width);
    fitting
        .or_else(|| usable.min_by_key(|info| info.width))
        .copied()
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "PascalCase", default)]
struct TrickplayItem {
    trickplay: Option<HashMap<String, HashMap<String, TrickplayInfo>>>,
}

impl TrickplayItem {
    /// Valid resolutions of `media_source_id`, or of the item's only source when no id is known.
    fn resolutions(self, media_source_id: Option<&str>) -> Vec<TrickplayInfo> {
        let Some(mut sources) = self.trickplay else {
            return Vec::new();
        };
        let source = match media_source_id {
            Some(id) => sources.remove(id),
            None if sources.len() == 1 => sources.into_values().next(),
            None => None,
        };
        let mut resolutions: Vec<TrickplayInfo> = source
            .into_iter()
            .flat_map(HashMap::into_values)
            .filter(TrickplayInfo::is_valid)
            .collect();
        resolutions.sort_by_key(|info| info.width);
        resolutions.truncate(MAX_RESOLUTIONS);
        resolutions
    }
}

impl JellyfinClient {
    /// Trickplay resolutions of one media source of `item_id`, narrowest first. Empty when the
    /// server has generated none.
    pub async fn trickplay_resolutions(
        &self,
        session: &Session,
        item_id: &str,
        media_source_id: Option<&str>,
    ) -> Result<Vec<TrickplayInfo>, ApiError> {
        home::validate_id(item_id)?;
        if let Some(id) = media_source_id {
            home::validate_id(id)?;
        }
        let request = self
            .http
            .request(Method::GET, self.endpoint(&["Items", item_id]))
            .query(&[
                ("userId", session.user.id.as_str()),
                ("fields", "Trickplay"),
            ]);
        let request = self.add_metadata(request, Some(&session.access_token));
        let response = request.send().await.map_err(crate::transport_error)?;
        let item: TrickplayItem = crate::decode_response(response).await?;
        Ok(item.resolutions(media_source_id))
    }

    /// One JPEG tile sheet. `index` is [`TrickplayCell::sheet`].
    pub async fn trickplay_tile(
        &self,
        session: &Session,
        item_id: &str,
        media_source_id: Option<&str>,
        width: u32,
        index: u32,
    ) -> Result<Vec<u8>, ApiError> {
        home::validate_id(item_id)?;
        // Jellyfin binds both route values as signed 32-bit integers.
        if width > i32::MAX as u32 || index > i32::MAX as u32 {
            return Err(ApiError::InvalidResponse);
        }
        let width = width.to_string();
        let file = format!("{index}.jpg");
        let mut request = self
            .http
            .request(
                Method::GET,
                self.endpoint(&["Videos", item_id, "Trickplay", &width, &file]),
            )
            .timeout(Duration::from_secs(15));
        if let Some(id) = media_source_id {
            home::validate_id(id)?;
            request = request.query(&[("mediaSourceId", id)]);
        }
        let request = self
            .add_metadata(request, Some(&session.access_token))
            .header(header::ACCEPT, "image/jpeg");
        let response = request.send().await.map_err(crate::transport_error)?;
        read_bounded(response, MAX_TRICKPLAY_TILE_BYTES, || {
            ApiError::ImageTooLarge
        })
        .await
    }
}

/// The body of a successful response. An empty body is invalid; one larger than `limit` is
/// refused with `too_large`.
pub(crate) async fn read_bounded(
    mut response: reqwest::Response,
    limit: usize,
    too_large: fn() -> ApiError,
) -> Result<Vec<u8>, ApiError> {
    let status = response.status();
    if status == StatusCode::UNAUTHORIZED {
        return Err(ApiError::ReauthenticationRequired);
    }
    if status == StatusCode::FORBIDDEN {
        return Err(ApiError::Forbidden);
    }
    if !status.is_success() {
        return Err(crate::status_error(status, response.headers()));
    }
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err(too_large());
    }
    let mut body =
        Vec::with_capacity(response.content_length().unwrap_or(0).min(limit as u64) as usize);
    while let Some(chunk) = response.chunk().await.map_err(crate::transport_error)? {
        if body.len().saturating_add(chunk.len()) > limit {
            return Err(too_large());
        }
        body.extend_from_slice(&chunk);
    }
    if body.is_empty() {
        return Err(ApiError::InvalidResponse);
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(width: u32) -> TrickplayInfo {
        TrickplayInfo {
            width,
            height: width * 9 / 16,
            tile_width: 10,
            tile_height: 10,
            thumbnail_count: 369,
            interval: 10_000,
        }
    }

    #[test]
    fn the_manifest_is_read_for_the_playing_source() {
        let item: TrickplayItem = serde_json::from_str(
            r#"{"Id":"ep","Trickplay":{"src":{"320":{"Width":320,"Height":180,"TileWidth":10,
            "TileHeight":10,"ThumbnailCount":369,"Interval":10000,"Bandwidth":10015},
            "160":{"Width":160,"Height":90,"TileWidth":10,"TileHeight":10,"ThumbnailCount":369,
            "Interval":10000}},"other":{"320":{"Width":0}}}}"#,
        )
        .expect("item");
        let resolutions = item.resolutions(Some("src"));
        assert_eq!(
            resolutions
                .iter()
                .map(|info| info.width)
                .collect::<Vec<_>>(),
            [160, 320]
        );
    }

    #[test]
    fn without_a_source_id_only_a_single_source_is_used_and_invalid_entries_are_dropped() {
        let one: TrickplayItem = serde_json::from_str(
            r#"{"Trickplay":{"a":{"320":{"Width":320,"Height":180,"TileWidth":10,
            "TileHeight":10,"ThumbnailCount":5,"Interval":1000}}}}"#,
        )
        .expect("item");
        assert_eq!(one.resolutions(None).len(), 1);
        let two: TrickplayItem = serde_json::from_str(
            r#"{"Trickplay":{"a":{"320":{"Width":320,"Height":180,"TileWidth":10,
            "TileHeight":10,"ThumbnailCount":5,"Interval":1000}},"b":{}}}"#,
        )
        .expect("item");
        assert!(two.resolutions(None).is_empty());
        let invalid: TrickplayItem =
            serde_json::from_str(r#"{"Trickplay":{"a":{"320":{"Width":320}}}}"#).expect("item");
        assert!(invalid.resolutions(Some("a")).is_empty());
        let none: TrickplayItem = serde_json::from_str(r#"{"Id":"x"}"#).expect("item");
        assert!(none.resolutions(Some("a")).is_empty());
    }

    #[test]
    fn positions_map_to_the_sheet_column_and_row() {
        let info = info(320);
        assert_eq!(
            info.locate(Duration::ZERO),
            Some(TrickplayCell {
                sheet: 0,
                column: 0,
                row: 0
            })
        );
        // Thumbnail 123 (at 1230 s): sheet 1, offset 23, so column 3 of row 2.
        assert_eq!(
            info.locate(Duration::from_secs(1_235)),
            Some(TrickplayCell {
                sheet: 1,
                column: 3,
                row: 2
            })
        );
        // Past the end the last thumbnail (368) is shown.
        assert_eq!(
            info.locate(Duration::from_secs(100_000)),
            Some(TrickplayCell {
                sheet: 3,
                column: 8,
                row: 6
            })
        );
        assert_eq!(info.sheet_count(), 4);
        assert_eq!(TrickplayInfo::default().locate(Duration::ZERO), None);
    }

    #[test]
    fn the_widest_fitting_resolution_is_picked_like_jellyfin_web() {
        let all = [info(480), info(160), info(320)];
        assert_eq!(pick_resolution(&all, 384).map(|i| i.width), Some(320));
        assert_eq!(pick_resolution(&all, 1000).map(|i| i.width), Some(480));
        assert_eq!(pick_resolution(&all, 100).map(|i| i.width), Some(160));
        assert_eq!(pick_resolution(&[], 384), None);
    }
}
