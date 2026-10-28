//! Seek-bar preview thumbnails from the server's trickplay sheets. The manifest is read once per
//! played item; sheets are fetched only when the viewer hovers or drags over a position they
//! cover, and only a few decoded sheets are kept (a 10x10 sheet of 320 px thumbnails is about
//! 23 MB of RGBA). An item without trickplay data shows no thumbnail.

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::Duration;

use jellyfin_api::trickplay::{TrickplayInfo, pick_resolution};

/// Decoded sheets kept as textures; the least recently shown one is dropped first.
pub(crate) const MAX_SHEETS: usize = 4;
/// Sheet requests in flight at once, so a fast drag does not queue the whole video.
const MAX_PENDING: usize = 2;
/// A decoded sheet normally uses about 23 MiB; reject unexpectedly large textures before upload.
const MAX_DECODED_SHEET_BYTES: usize = 64 * 1024 * 1024;

/// One thumbnail ready to paint: a texture and the part of it that holds the thumbnail.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SeekThumbnail {
    pub texture: egui::TextureId,
    pub uv: egui::Rect,
    /// Width over height of one thumbnail.
    pub aspect: f32,
}

struct Sheet {
    index: u32,
    texture: egui::TextureHandle,
    size: [usize; 2],
}

/// Trickplay state of the item now playing.
#[derive(Default)]
pub(crate) struct TrickplayState {
    item_id: Option<String>,
    media_source_id: Option<String>,
    manifest_request: Option<u64>,
    info: Option<TrickplayInfo>,
    /// Least recently shown first.
    sheets: VecDeque<Sheet>,
    pending: HashMap<u64, u32>,
    failed: HashSet<u32>,
}

/// What [`TrickplayState::thumbnail`] needs from the caller: a sheet to fetch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SheetRequest {
    pub width: u32,
    pub sheet: u32,
}

impl TrickplayState {
    /// Start over for a newly played item; the manifest request is `request_id`.
    pub(crate) fn start(
        &mut self,
        item_id: String,
        media_source_id: Option<String>,
        request_id: u64,
    ) {
        *self = Self {
            item_id: Some(item_id),
            media_source_id,
            manifest_request: Some(request_id),
            ..Self::default()
        };
    }

    pub(crate) fn clear(&mut self) {
        *self = Self::default();
    }

    pub(crate) fn item_id(&self) -> Option<&str> {
        self.item_id.as_deref()
    }

    pub(crate) fn media_source_id(&self) -> Option<&str> {
        self.media_source_id.as_deref()
    }

    /// Keeps the resolution Jellyfin Web would pick for a screen `screen_width_px` wide: the
    /// widest one up to a fifth of it.
    pub(crate) fn resolutions_loaded(
        &mut self,
        request_id: u64,
        item_id: &str,
        resolutions: &[TrickplayInfo],
        screen_width_px: f32,
    ) {
        if self.manifest_request != Some(request_id) || self.item_id.as_deref() != Some(item_id) {
            return;
        }
        self.manifest_request = None;
        let max_width = (screen_width_px.max(0.0) * 0.2) as u32;
        self.info = pick_resolution(resolutions, max_width);
    }

    /// The thumbnail at `position`, if its sheet is loaded. Otherwise the sheet to fetch, when
    /// it has not failed and not too many fetches are running.
    pub(crate) fn thumbnail(
        &mut self,
        position: Duration,
    ) -> (Option<SeekThumbnail>, Option<SheetRequest>) {
        let Some(info) = self.info else {
            return (None, None);
        };
        let Some(cell) = info.locate(position) else {
            return (None, None);
        };
        if let Some(found) = self
            .sheets
            .iter()
            .position(|sheet| sheet.index == cell.sheet)
        {
            // Most recently shown last, so eviction drops the sheet viewed longest ago.
            let Some(sheet) = self.sheets.remove(found) else {
                return (None, None);
            };
            let thumbnail = thumbnail_in(&info, cell.column, cell.row, &sheet);
            self.sheets.push_back(sheet);
            return (thumbnail, None);
        }
        let requested = self.pending.values().any(|sheet| *sheet == cell.sheet);
        if requested || self.failed.contains(&cell.sheet) || self.pending.len() >= MAX_PENDING {
            return (None, None);
        }
        (
            None,
            Some(SheetRequest {
                width: info.width,
                sheet: cell.sheet,
            }),
        )
    }

    pub(crate) fn sheet_requested(&mut self, request_id: u64, sheet: u32) {
        self.pending.insert(request_id, sheet);
    }

    /// Stores a fetched sheet as a texture. A failed sheet is not asked for again for this item.
    pub(crate) fn sheet_loaded(
        &mut self,
        ctx: &egui::Context,
        request_id: u64,
        item_id: &str,
        image: Option<(u32, u32, Vec<u8>)>,
    ) {
        let Some(index) = self.pending.remove(&request_id) else {
            return;
        };
        if self.item_id.as_deref() != Some(item_id) {
            return;
        }
        let Some((width, height, rgba)) = image else {
            self.failed.insert(index);
            return;
        };
        let size = [width as usize, height as usize];
        let Some(decoded_bytes) = size[0]
            .checked_mul(size[1])
            .and_then(|pixels| pixels.checked_mul(4))
        else {
            self.failed.insert(index);
            return;
        };
        if size.contains(&0)
            || decoded_bytes > MAX_DECODED_SHEET_BYTES
            || decoded_bytes != rgba.len()
        {
            self.failed.insert(index);
            return;
        }
        let texture = ctx.load_texture(
            format!("kattri-trickplay:{item_id}:{index}"),
            egui::ColorImage::from_rgba_unmultiplied(size, &rgba),
            egui::TextureOptions::LINEAR,
        );
        self.sheets.retain(|sheet| sheet.index != index);
        self.sheets.push_back(Sheet {
            index,
            texture,
            size,
        });
        while self.sheets.len() > MAX_SHEETS {
            self.sheets.pop_front();
        }
    }

    #[cfg(test)]
    fn loaded_sheets(&self) -> Vec<u32> {
        self.sheets.iter().map(|sheet| sheet.index).collect()
    }
}

/// The thumbnail at `column`, `row` of `sheet`. The last sheet can have fewer rows, so the UV
/// rectangle is taken from the decoded size, not from the full sheet layout.
fn thumbnail_in(
    info: &TrickplayInfo,
    column: u32,
    row: u32,
    sheet: &Sheet,
) -> Option<SeekThumbnail> {
    let [sheet_width, sheet_height] = sheet.size;
    let (sheet_width, sheet_height) = (sheet_width as f32, sheet_height as f32);
    let left = (column * info.width) as f32;
    let top = (row * info.height) as f32;
    let right = left + info.width as f32;
    let bottom = top + info.height as f32;
    if right > sheet_width + 0.5 || bottom > sheet_height + 0.5 {
        return None;
    }
    Some(SeekThumbnail {
        texture: sheet.texture.id(),
        uv: egui::Rect::from_min_max(
            egui::pos2(left / sheet_width, top / sheet_height),
            egui::pos2(
                (right / sheet_width).min(1.0),
                (bottom / sheet_height).min(1.0),
            ),
        ),
        aspect: info.width as f32 / info.height as f32,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info() -> TrickplayInfo {
        TrickplayInfo {
            width: 4,
            height: 2,
            tile_width: 2,
            tile_height: 2,
            thumbnail_count: 30,
            interval: 1_000,
        }
    }

    fn sheet_pixels() -> Option<(u32, u32, Vec<u8>)> {
        Some((8, 4, vec![255; 8 * 4 * 4]))
    }

    fn started() -> TrickplayState {
        let mut state = TrickplayState::default();
        state.start("ep".into(), Some("src".into()), 1);
        state.resolutions_loaded(1, "ep", &[info()], 1920.0);
        state
    }

    #[test]
    fn no_manifest_or_a_stale_one_shows_nothing() {
        let mut state = TrickplayState::default();
        state.start("ep".into(), None, 1);
        state.resolutions_loaded(2, "ep", &[info()], 1920.0);
        assert_eq!(state.thumbnail(Duration::ZERO), (None, None));
        state.resolutions_loaded(1, "ep", &[], 1920.0);
        assert_eq!(state.thumbnail(Duration::ZERO), (None, None));
    }

    #[test]
    fn a_missing_sheet_is_requested_once_then_painted_from_its_texture() {
        let ctx = egui::Context::default();
        let mut state = started();
        // Thumbnail 5 at 5.5 s: sheet 1 (4 per sheet), column 1, row 0.
        let (thumbnail, request) = state.thumbnail(Duration::from_millis(5_500));
        assert!(thumbnail.is_none());
        assert_eq!(request, Some(SheetRequest { width: 4, sheet: 1 }));
        state.sheet_requested(10, 1);
        assert_eq!(state.thumbnail(Duration::from_millis(5_500)), (None, None));
        state.sheet_loaded(&ctx, 10, "ep", sheet_pixels());
        let (thumbnail, request) = state.thumbnail(Duration::from_millis(5_500));
        assert!(request.is_none());
        let thumbnail = thumbnail.expect("thumbnail");
        assert_eq!(
            thumbnail.uv,
            egui::Rect::from_min_max(egui::pos2(0.5, 0.0), egui::pos2(1.0, 0.5))
        );
        assert_eq!(thumbnail.aspect, 2.0);
    }

    #[test]
    fn failed_sheets_are_not_retried_and_fetches_are_bounded() {
        let ctx = egui::Context::default();
        let mut state = started();
        state.sheet_requested(10, 0);
        state.sheet_loaded(&ctx, 10, "ep", None);
        assert_eq!(state.thumbnail(Duration::ZERO), (None, None));
        state.sheet_requested(11, 1);
        state.sheet_requested(12, 2);
        // Two fetches are running, so sheet 3 waits.
        assert_eq!(state.thumbnail(Duration::from_secs(13)), (None, None));
    }

    #[test]
    fn only_the_most_recently_shown_sheets_are_kept() {
        let ctx = egui::Context::default();
        let mut state = started();
        for sheet in 0..MAX_SHEETS as u32 {
            state.sheet_requested(u64::from(sheet) + 10, sheet);
            state.sheet_loaded(&ctx, u64::from(sheet) + 10, "ep", sheet_pixels());
        }
        // Showing sheet 0 again makes sheet 1 the oldest, so loading another drops it.
        assert!(state.thumbnail(Duration::ZERO).0.is_some());
        state.sheet_requested(20, 6);
        state.sheet_loaded(&ctx, 20, "ep", sheet_pixels());
        assert_eq!(state.loaded_sheets(), [2, 3, 0, 6]);
    }

    #[test]
    fn a_sheet_for_a_previous_item_is_dropped() {
        let ctx = egui::Context::default();
        let mut state = started();
        state.sheet_requested(10, 0);
        state.start("next".into(), None, 2);
        state.sheet_loaded(&ctx, 10, "ep", sheet_pixels());
        assert!(state.loaded_sheets().is_empty());
    }

    #[test]
    fn empty_and_oversized_sheets_are_rejected_before_texture_creation() {
        let ctx = egui::Context::default();
        let mut state = started();
        state.sheet_requested(10, 0);
        state.sheet_loaded(&ctx, 10, "ep", Some((0, 0, Vec::new())));
        assert!(state.loaded_sheets().is_empty());
        assert!(state.failed.contains(&0));

        state.sheet_requested(11, 1);
        state.sheet_loaded(&ctx, 11, "ep", Some((4096, 4096, vec![0])));
        assert!(state.loaded_sheets().is_empty());
        assert!(state.failed.contains(&1));
    }
}
