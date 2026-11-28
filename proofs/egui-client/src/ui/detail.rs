//! Read-only metadata panel for a Jellyfin library item.

use super::{ui_text, ui_text_args};
use egui::{Color32, FontId, RichText, Ui};
use jellyfin_api::{BaseItemDto, UserItemDataDto};
use kattri_i18n::ids;

const TICKS_PER_MINUTE: i64 = 10_000_000 * 60;
const OVERVIEW_MAX_LINES: usize = 6;
const OVERVIEW_FONT_SIZE: f32 = 16.0;
const OVERVIEW_SEARCH_CHAR_BUDGET: usize = 1024;

/// Watched-toggle state for one item. The flag flips as soon as a change is
/// requested, further changes are refused while that request is in flight, and
/// a failed request restores the value from before the change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WatchedToggle {
    played: bool,
    /// The played value to restore if the in-flight request fails.
    revert_to: Option<bool>,
}

impl WatchedToggle {
    /// Missing `UserData` or a missing `Played` flag both mean unplayed.
    pub fn from_user_data(user_data: Option<&UserItemDataDto>) -> Self {
        Self {
            played: user_data.and_then(|data| data.played).unwrap_or(false),
            revert_to: None,
        }
    }

    pub fn played(&self) -> bool {
        self.played
    }

    pub fn is_pending(&self) -> bool {
        self.revert_to.is_some()
    }

    /// Flips the flag and returns the played value to send, or `None` when a
    /// request is already in flight.
    pub fn begin(&mut self) -> Option<bool> {
        if self.is_pending() {
            return None;
        }
        self.revert_to = Some(self.played);
        self.played = !self.played;
        Some(self.played)
    }

    /// Accepts the server's confirmed value and clears the in-flight state.
    pub fn succeed(&mut self, played: bool) {
        self.played = played;
        self.revert_to = None;
    }

    /// Restores the value from before the failed request.
    pub fn fail(&mut self) {
        if let Some(previous) = self.revert_to.take() {
            self.played = previous;
        }
    }
}

/// What the details header reports back to its caller.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DetailHeaderResponse {
    /// The Back pill was pressed.
    pub back: bool,
}

/// Local presentation state for the expandable overview. It deliberately owns
/// no item actions; callers decide how selection and playback are handled.
#[derive(Debug, Default)]
pub struct DetailPanel {
    overview_expanded: bool,
    cached_overview: Option<CachedClampedOverview>,
    requested_chapter_ticks: Option<u64>,
}

#[derive(Debug)]
struct CachedClampedOverview {
    item_id: String,
    width_bucket_px: u32,
    overview_len: usize,
    clamped_text: String,
    has_more: bool,
}

impl DetailPanel {
    /// Returns and clears any chapter seek requested by clicking a chapter card.
    pub fn take_requested_chapter_ticks(&mut self) -> Option<u64> {
        self.requested_chapter_ticks.take()
    }

    /// Draws the header and the overview. The header is the backdrop with the title, metadata
    /// and genres over it and a Back pill at its top left. `actions` draws the buttons at the
    /// bottom of the header's text block.
    pub fn show(
        &mut self,
        ui: &mut Ui,
        item: &BaseItemDto,
        artwork: Option<&egui::TextureHandle>,
        actions: impl FnOnce(&mut Ui),
    ) -> DetailHeaderResponse {
        let width = ui.available_width().max(1.0);
        let min_hero_height = (width * 9.0 / 16.0).clamp(320.0, 460.0);
        let inset = 20.0;
        let text_width = (width * 0.62)
            .max((width - 2.0 * inset).min(360.0))
            .min(700.0)
            .min((width - 2.0 * inset).max(1.0));
        let metadata = metadata_labels(item);
        let genres = visible_genres(item);
        let title_height = ui
            .fonts_mut(|fonts| {
                fonts.layout(
                    item.name.clone(),
                    FontId::proportional(42.0),
                    Color32::WHITE,
                    text_width,
                )
            })
            .size()
            .y;
        let metadata_height = if metadata.is_empty() { 0.0 } else { 8.0 + 22.0 };
        let genres_height = genres.as_ref().map_or(0.0, |text| {
            let h = ui
                .fonts_mut(|fonts| {
                    fonts.layout(
                        text.clone(),
                        FontId::proportional(14.0),
                        Color32::WHITE,
                        text_width,
                    )
                })
                .size()
                .y;
            8.0 + h
        });
        let estimated_block_height =
            title_height + metadata_height + genres_height + 8.0 + 48.0 + inset;
        let top_row_height = inset + 36.0;
        let spacer = (min_hero_height - top_row_height - estimated_block_height).max(16.0);

        let bg_idx = ui.painter().add(egui::Shape::Noop);
        let mut back = false;
        let hero_response = ui.scope(|ui| {
            ui.set_min_width(width);
            ui.set_max_width(width);
            ui.set_min_height(min_hero_height);
            ui.add_space(inset);
            ui.horizontal(|ui| {
                ui.add_space(inset);
                back = ui
                    .add_sized(
                        egui::vec2(104.0, 36.0),
                        egui::Button::new(
                            RichText::new("←  Back")
                                .size(16.0)
                                .strong()
                                .color(Color32::WHITE),
                        )
                        .fill(Color32::from_black_alpha(150))
                        .corner_radius(egui::CornerRadius::same(18)),
                    )
                    .clicked();
            });
            ui.add_space(spacer);
            ui.horizontal(|ui| {
                ui.add_space(inset);
                ui.vertical(|ui| {
                    ui.set_max_width(text_width);
                    ui.add(
                        egui::Label::new(
                            RichText::new(&item.name)
                                .size(42.0)
                                .strong()
                                .color(Color32::WHITE),
                        )
                        .wrap(),
                    );
                    if !metadata.is_empty() {
                        ui.add_space(8.0);
                        ui.horizontal_wrapped(|ui| {
                            ui.spacing_mut().item_spacing.x = 9.0;
                            for (index, label) in metadata.iter().enumerate() {
                                if index > 0 {
                                    ui.label(RichText::new("·").color(Color32::from_gray(160)));
                                }
                                ui.label(
                                    RichText::new(label)
                                        .strong()
                                        .size(15.0)
                                        .color(Color32::WHITE),
                                );
                            }
                        });
                    }
                    if let Some(genres) = genres {
                        ui.add_space(8.0);
                        ui.add(
                            egui::Label::new(RichText::new(genres).color(Color32::from_gray(205)))
                                .wrap(),
                        );
                    }
                    ui.add_space(8.0);
                    actions(ui);
                    show_external_links(ui, item);
                });
            });
            ui.add_space(inset);
        });

        let hero_rect = egui::Rect::from_min_size(
            hero_response.response.rect.min,
            egui::vec2(
                width,
                hero_response.response.rect.height().max(min_hero_height),
            ),
        );
        let mut bg_shapes = Vec::with_capacity(4);
        match artwork {
            Some(texture) => {
                let uv = super::row::cover_uv(texture.size_vec2(), hero_rect.size());
                let mut mesh = egui::Mesh::with_texture(texture.id());
                mesh.add_rect_with_uv(hero_rect, uv, Color32::WHITE);
                bg_shapes.push(egui::Shape::mesh(mesh));
            }
            None => {
                bg_shapes.push(egui::Shape::rect_filled(
                    hero_rect,
                    egui::CornerRadius::ZERO,
                    Color32::from_rgb(31, 31, 34),
                ));
                let unavailable = ui.fonts_mut(|fonts| {
                    fonts.layout_no_wrap(
                        ui_text(ids::DETAIL_ARTWORK_UNAVAILABLE).to_owned(),
                        FontId::proportional(14.0),
                        Color32::from_gray(140),
                    )
                });
                let text_pos = egui::pos2(
                    hero_rect.right() - 28.0 - unavailable.size().x,
                    hero_rect.top() + inset + 18.0 - unavailable.size().y * 0.5,
                );
                bg_shapes.push(egui::Shape::galley(
                    text_pos,
                    unavailable,
                    Color32::from_gray(140),
                ));
            }
        }
        // Darken the left (behind the text) and the bottom so the title reads on any artwork.
        let dark = Color32::from_black_alpha(215);
        let clear = Color32::TRANSPARENT;
        let left_band = egui::Rect::from_min_max(
            hero_rect.left_top(),
            egui::pos2(
                hero_rect.left() + hero_rect.width() * 0.65,
                hero_rect.bottom(),
            ),
        );
        bg_shapes.push(gradient_quad_shape(left_band, [dark, clear, clear, dark]));
        let bottom_band = egui::Rect::from_min_max(
            egui::pos2(hero_rect.left(), hero_rect.top() + hero_rect.height() * 0.4),
            hero_rect.right_bottom(),
        );
        bg_shapes.push(gradient_quad_shape(bottom_band, [clear, clear, dark, dark]));
        ui.painter().set(bg_idx, egui::Shape::Vec(bg_shapes));

        if let Some(tagline) = item
            .taglines
            .as_deref()
            .and_then(|list| list.iter().map(|s| s.trim()).find(|s| !s.is_empty()))
        {
            ui.scope(|ui| {
                ui.set_max_width(width.min(860.0));
                ui.add_space(10.0);
                ui.add(
                    egui::Label::new(
                        RichText::new(tagline)
                            .italics()
                            .size(15.0)
                            .color(Color32::from_gray(190)),
                    )
                    .wrap(),
                );
            });
        }

        if let Some(overview) = item
            .overview
            .as_deref()
            .map(str::trim)
            .filter(|overview| !overview.is_empty())
        {
            ui.scope(|ui| {
                ui.set_max_width(width.min(860.0));
                self.show_overview_for(ui, &item.id, overview);
            });
        }
        self.show_chapters_strip(ui, item);
        DetailHeaderResponse { back }
    }

    #[cfg(test)]
    fn show_overview(&mut self, ui: &mut Ui, overview: &str) {
        self.show_overview_for(ui, "", overview);
    }

    fn show_overview_for(&mut self, ui: &mut Ui, item_id: &str, overview: &str) {
        ui.add_space(14.0);
        let width = ui.available_width().max(1.0);
        let (preview, has_more) = if self.overview_expanded {
            (overview.to_owned(), false)
        } else {
            let width_bucket_px = width.round().max(1.0) as u32;
            let cache_hit = self.cached_overview.as_ref().filter(|cached| {
                cached.item_id == item_id
                    && cached.width_bucket_px == width_bucket_px
                    && cached.overview_len == overview.len()
            });
            if let Some(cached) = cache_hit {
                (cached.clamped_text.clone(), cached.has_more)
            } else {
                let (clamped_text, has_more) = clamp_overview(ui, overview, width);
                self.cached_overview = Some(CachedClampedOverview {
                    item_id: item_id.to_owned(),
                    width_bucket_px,
                    overview_len: overview.len(),
                    clamped_text: clamped_text.clone(),
                    has_more,
                });
                (clamped_text, has_more)
            }
        };
        ui.add(egui::Label::new(RichText::new(preview).size(OVERVIEW_FONT_SIZE)).wrap());
        ui.add_space(4.0);

        if !self.overview_expanded && has_more {
            if ui.button(ui_text(ids::DETAIL_BUTTON_MORE)).clicked() {
                self.overview_expanded = true;
            }
        } else if self.overview_expanded && ui.button(ui_text(ids::DETAIL_BUTTON_LESS)).clicked() {
            self.overview_expanded = false;
        }
    }

    fn show_chapters_strip(&mut self, ui: &mut Ui, item: &BaseItemDto) {
        let Some(chapters) = item.chapters.as_deref().filter(|list| !list.is_empty()) else {
            return;
        };
        ui.add_space(18.0);
        ui.heading("Scenes");
        ui.add_space(8.0);
        egui::ScrollArea::horizontal()
            .id_salt(("detail_chapters", &item.id))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 10.0;
                    for (index, chapter) in chapters.iter().take(64).enumerate() {
                        let name = chapter
                            .name
                            .as_deref()
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .map(str::to_owned)
                            .unwrap_or_else(|| format!("Chapter {}", index + 1));
                        let ticks = chapter.start_position_ticks.unwrap_or(0);
                        let timestamp = format_chapter_timestamp(ticks);
                        let card = egui::Frame::group(ui.style())
                            .inner_margin(egui::Margin::symmetric(12, 8))
                            .show(ui, |ui| {
                                ui.set_min_width(140.0);
                                ui.vertical(|ui| {
                                    ui.label(RichText::new(&name).strong().size(13.0));
                                    ui.label(
                                        RichText::new(&timestamp)
                                            .small()
                                            .color(Color32::from_gray(170)),
                                    );
                                });
                            });
                        let response = card
                            .response
                            .interact(egui::Sense::click())
                            .on_hover_cursor(egui::CursorIcon::PointingHand);
                        response.widget_info(|| {
                            egui::WidgetInfo::labeled(
                                egui::Role::Button,
                                true,
                                format!("{name} {timestamp}"),
                            )
                        });
                        if response.clicked() {
                            self.requested_chapter_ticks = Some(ticks);
                        }
                    }
                });
            });
    }
}

fn valid_external_provider_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn external_links_for_item(item: &BaseItemDto) -> Vec<(&'static str, String)> {
    let Some(provider_ids) = item.provider_ids.as_ref() else {
        return Vec::new();
    };
    let tmdb_kind = if item.item_type.as_deref().is_some_and(|kind| {
        kind.eq_ignore_ascii_case("Series")
            || kind.eq_ignore_ascii_case("Season")
            || kind.eq_ignore_ascii_case("Episode")
    }) {
        "tv"
    } else {
        "movie"
    };
    let mut links = Vec::with_capacity(3);
    if let Some(id) = provider_ids
        .get("Imdb")
        .map(String::as_str)
        .map(str::trim)
        .filter(|id| valid_external_provider_id(id))
    {
        links.push(("IMDb", format!("https://www.imdb.com/title/{id}")));
    }
    if let Some(id) = provider_ids
        .get("Tmdb")
        .map(String::as_str)
        .map(str::trim)
        .filter(|id| valid_external_provider_id(id))
    {
        links.push((
            "TMDB",
            format!("https://www.themoviedb.org/{tmdb_kind}/{id}"),
        ));
    }
    if let Some(id) = provider_ids
        .get("Tvdb")
        .map(String::as_str)
        .map(str::trim)
        .filter(|id| valid_external_provider_id(id))
    {
        links.push(("TVDb", format!("https://thetvdb.com/?tab=series&id={id}")));
    }
    links
}

fn show_external_links(ui: &mut Ui, item: &BaseItemDto) {
    let links = external_links_for_item(item);
    if links.is_empty() {
        return;
    }
    ui.add_space(6.0);
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        for (label, url) in links {
            if ui
                .add(
                    egui::Button::new(RichText::new(label).size(12.0))
                        .fill(Color32::from_black_alpha(130))
                        .corner_radius(egui::CornerRadius::same(12)),
                )
                .clicked()
            {
                ui.ctx().open_url(egui::OpenUrl::new_tab(url));
            }
        }
    });
}

fn format_chapter_timestamp(ticks: u64) -> String {
    const TICKS_PER_SECOND: u64 = 10_000_000;
    let total_seconds = ticks / TICKS_PER_SECOND;
    let hours = total_seconds / 3600;
    let minutes = (total_seconds % 3600) / 60;
    let seconds = total_seconds % 60;
    if hours > 0 {
        format!("{hours:02}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes:02}:{seconds:02}")
    }
}

fn gradient_quad_shape(rect: egui::Rect, corners: [Color32; 4]) -> egui::Shape {
    let mut mesh = egui::Mesh::default();
    mesh.colored_vertex(rect.left_top(), corners[0]);
    mesh.colored_vertex(rect.right_top(), corners[1]);
    mesh.colored_vertex(rect.right_bottom(), corners[2]);
    mesh.colored_vertex(rect.left_bottom(), corners[3]);
    mesh.add_triangle(0, 1, 2);
    mesh.add_triangle(0, 2, 3);
    egui::Shape::mesh(mesh)
}

/// Formats Jellyfin's 100-nanosecond runtime ticks as a compact `h m` label.
pub fn format_runtime(runtime_ticks: Option<i64>) -> Option<String> {
    let ticks = runtime_ticks.filter(|ticks| *ticks > 0)?;
    let total_minutes = ticks / TICKS_PER_MINUTE;
    if total_minutes == 0 {
        return None;
    }

    let hours = total_minutes / 60;
    let minutes = total_minutes % 60;
    if hours > 0 {
        Some(ui_text_args(
            ids::TIME_HOURS_MINUTES,
            &[
                ("hours", &hours.to_string()),
                ("minutes", &minutes.to_string()),
            ],
        ))
    } else {
        Some(ui_text_args(
            ids::TIME_MINUTES,
            &[("minutes", &minutes.to_string())],
        ))
    }
}

fn metadata_labels(item: &BaseItemDto) -> Vec<String> {
    let mut labels = Vec::with_capacity(6);
    if let Some(year) = item.production_year {
        labels.push(year.to_string());
    }
    if let Some(runtime) = format_runtime(item.run_time_ticks) {
        labels.push(runtime);
    }
    if let Some(rating) = item
        .official_rating
        .as_deref()
        .map(str::trim)
        .filter(|rating| !rating.is_empty())
    {
        labels.push(rating.to_owned());
    }
    if let Some(rating) = item
        .community_rating
        .filter(|rating| rating.is_finite() && *rating > 0.0)
    {
        labels.push(format!("★ {rating:.1}"));
    }
    if let Some(rating) = item
        .critic_rating
        .filter(|rating| rating.is_finite() && *rating > 0.0)
    {
        labels.push(format!("{}%", rating.round() as i32));
    }
    let position_ticks = item
        .user_data
        .as_ref()
        .and_then(|user_data| user_data.playback_position_ticks);
    if let Some(ends_at) = format_ends_at(item.run_time_ticks, position_ticks) {
        labels.push(ends_at);
    }
    labels
}

fn format_ends_at(run_time_ticks: Option<i64>, position_ticks: Option<i64>) -> Option<String> {
    const TICKS_PER_SECOND: i64 = 10_000_000;
    let total_ticks = run_time_ticks.filter(|ticks| *ticks > 0)?;
    let pos_ticks = position_ticks.unwrap_or(0).max(0);
    let remaining_ticks = total_ticks.saturating_sub(pos_ticks);
    if remaining_ticks <= 0 {
        return None;
    }
    let remaining_secs = remaining_ticks / TICKS_PER_SECOND;
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs() as i64;
    let end_secs = now_secs.checked_add(remaining_secs)?;
    let hh_mm = local_hh_mm(end_secs)?;
    Some(format!("Ends at {hh_mm}"))
}

#[cfg(unix)]
fn local_hh_mm(unix_seconds: i64) -> Option<String> {
    use std::ffi::CStr;

    let timestamp = libc::time_t::try_from(unix_seconds).ok()?;
    let mut broken_down: libc::tm = unsafe { std::mem::zeroed() };
    let mut output = [0 as libc::c_char; 16];
    let format = c"%H:%M";
    unsafe {
        if libc::localtime_r(&timestamp, &mut broken_down).is_null() {
            return None;
        }
        if libc::strftime(
            output.as_mut_ptr(),
            output.len(),
            format.as_ptr(),
            &broken_down,
        ) == 0
        {
            return None;
        }
        CStr::from_ptr(output.as_ptr())
            .to_str()
            .ok()
            .map(str::to_owned)
    }
}

#[cfg(windows)]
fn local_hh_mm(unix_seconds: i64) -> Option<String> {
    use std::ffi::{CStr, c_char};

    unsafe extern "C" {
        fn strftime(
            output: *mut c_char,
            capacity: usize,
            format: *const c_char,
            time: *const libc::tm,
        ) -> usize;
    }

    let timestamp: libc::time_t = unix_seconds;
    let mut broken_down: libc::tm = unsafe { std::mem::zeroed() };
    let mut output = [0 as libc::c_char; 16];
    let format = c"%H:%M";
    unsafe {
        if libc::localtime_s(&mut broken_down, &timestamp) != 0 {
            return None;
        }
        if strftime(
            output.as_mut_ptr(),
            output.len(),
            format.as_ptr(),
            &broken_down,
        ) == 0
        {
            return None;
        }
        CStr::from_ptr(output.as_ptr())
            .to_str()
            .ok()
            .map(str::to_owned)
    }
}

#[cfg(not(any(unix, windows)))]
fn local_hh_mm(_unix_seconds: i64) -> Option<String> {
    None
}

fn visible_genres(item: &BaseItemDto) -> Option<String> {
    let genres = item
        .genres
        .as_deref()?
        .iter()
        .map(|genre| genre.trim())
        .filter(|genre| !genre.is_empty())
        .collect::<Vec<_>>();
    (!genres.is_empty()).then(|| genres.join(" · "))
}

fn clamp_overview(ui: &Ui, text: &str, width: f32) -> (String, bool) {
    let (bounded_text, has_unmeasured_tail) = bounded_prefix(text);
    if !has_unmeasured_tail
        && layout_overview(ui, &bounded_text, width).rows.len() <= OVERVIEW_MAX_LINES
    {
        return (bounded_text, false);
    }

    let char_count = bounded_text.chars().count();
    let mut low = 0;
    let mut high = char_count;
    while low < high {
        let middle = low + (high - low).div_ceil(2);
        let candidate = format!("{}…", prefix_chars(&bounded_text, middle).trim_end());
        if layout_overview(ui, &candidate, width).rows.len() <= OVERVIEW_MAX_LINES {
            low = middle;
        } else {
            high = middle - 1;
        }
    }
    (
        format!("{}…", prefix_chars(&bounded_text, low).trim_end()),
        true,
    )
}

/// Inspects at most the preview budget plus one character to detect a tail.
fn bounded_prefix(text: &str) -> (String, bool) {
    let mut chars = text.chars();
    let prefix = chars
        .by_ref()
        .take(OVERVIEW_SEARCH_CHAR_BUDGET)
        .collect::<String>();
    (prefix, chars.next().is_some())
}

fn prefix_chars(text: &str, count: usize) -> &str {
    text.char_indices()
        .nth(count)
        .map_or(text, |(byte_index, _)| &text[..byte_index])
}

fn layout_overview(ui: &Ui, text: &str, width: f32) -> std::sync::Arc<egui::Galley> {
    let font = FontId::proportional(OVERVIEW_FONT_SIZE);
    ui.fonts_mut(|fonts| fonts.layout(text.to_owned(), font, Color32::WHITE, width))
}

#[cfg(test)]
mod tests {
    use super::{
        DetailPanel, OVERVIEW_SEARCH_CHAR_BUDGET, WatchedToggle, bounded_prefix, format_runtime,
        metadata_labels,
    };
    use egui::{Event, Modifiers, PointerButton, Pos2, RawInput, Rect, Shape, Vec2};
    use jellyfin_api::{BaseItemDto, UserItemDataDto};
    use serde_json::json;

    const TICKS_PER_MINUTE: i64 = 10_000_000 * 60;

    #[test]
    fn watched_toggle_flips_and_blocks_a_second_change_while_pending() {
        let mut toggle = WatchedToggle::from_user_data(None);
        assert!(!toggle.played());

        assert_eq!(toggle.begin(), Some(true));
        assert!(toggle.played());
        assert!(toggle.is_pending());
        assert_eq!(toggle.begin(), None);
        assert!(toggle.played());
    }

    #[test]
    fn watched_toggle_takes_the_server_value_on_success() {
        let mut toggle = WatchedToggle::from_user_data(None);
        toggle.begin();
        toggle.succeed(true);
        assert!(toggle.played());
        assert!(!toggle.is_pending());

        assert_eq!(toggle.begin(), Some(false));
        toggle.succeed(false);
        assert!(!toggle.played());
        assert!(!toggle.is_pending());
    }

    #[test]
    fn watched_toggle_reverts_on_failure() {
        let mut toggle = WatchedToggle::from_user_data(Some(&played_user_data(true)));
        assert!(toggle.played());

        assert_eq!(toggle.begin(), Some(false));
        toggle.fail();
        assert!(toggle.played());
        assert!(!toggle.is_pending());

        toggle.fail();
        assert!(toggle.played());
    }

    #[test]
    fn watched_toggle_reads_played_flag_and_treats_missing_as_unplayed() {
        assert!(WatchedToggle::from_user_data(Some(&played_user_data(true))).played());
        assert!(!WatchedToggle::from_user_data(Some(&played_user_data(false))).played());
        assert!(!WatchedToggle::from_user_data(Some(&UserItemDataDto::default())).played());
    }

    fn played_user_data(played: bool) -> UserItemDataDto {
        UserItemDataDto {
            played: Some(played),
            ..Default::default()
        }
    }

    #[test]
    fn runtime_is_hidden_when_missing_or_zero() {
        assert_eq!(format_runtime(None), None);
        assert_eq!(format_runtime(Some(0)), None);
    }

    #[test]
    fn runtime_under_one_hour_uses_minutes() {
        assert_eq!(
            format_runtime(Some(42 * TICKS_PER_MINUTE)),
            Some("42m".into())
        );
    }

    #[test]
    fn runtime_over_one_hour_uses_hours_and_minutes() {
        assert_eq!(
            format_runtime(Some(2 * 60 * TICKS_PER_MINUTE + 7 * TICKS_PER_MINUTE)),
            Some("2h 7m".into())
        );
    }

    #[test]
    fn very_long_overview_is_bounded_before_layout() {
        let long_text = "x".repeat(100_000);
        let (prefix, has_more) = bounded_prefix(&long_text);

        assert_eq!(prefix.chars().count(), OVERVIEW_SEARCH_CHAR_BUDGET);
        assert!(has_more);
    }

    #[test]
    fn metadata_includes_non_empty_official_rating_only() {
        let item: BaseItemDto = serde_json::from_value(json!({
            "Id": "item-1",
            "Name": "Example",
            "OfficialRating": " PG-13 "
        }))
        .expect("fixture parses");
        assert_eq!(metadata_labels(&item), vec!["PG-13".to_owned()]);

        let unrated: BaseItemDto = serde_json::from_value(json!({
            "Id": "item-2",
            "Name": "Unrated",
            "OfficialRating": "  "
        }))
        .expect("fixture parses");
        assert!(metadata_labels(&unrated).is_empty());
    }

    #[test]
    fn overview_is_six_lines_until_more_expands_it() {
        let overview = format!(
            "{}.",
            "A distinctive overview sentence with enough words to wrap across lines".repeat(40)
        );
        let context = egui::Context::default();
        let mut panel = DetailPanel::default();

        let collapsed = show_overview(&context, &mut panel, &overview, Vec::new());
        let preview = text_shape_ending_with(&collapsed, '…')
            .expect("collapsed overview preview is rendered");
        assert!(preview.galley.rows.len() <= 6);
        assert_ne!(preview.galley.job.text, overview);
        assert!(text_shape(&collapsed, "More").is_some());
        assert!(text_shape(&collapsed, &overview).is_none());

        let more = text_shape(&collapsed, "More").expect("More button is rendered");
        let click_position = more.pos + more.galley.rect.size() / 2.0;
        let pointer_events = |pressed| {
            vec![
                Event::PointerMoved(click_position),
                Event::PointerButton {
                    pos: click_position,
                    button: PointerButton::Primary,
                    pressed,
                    modifiers: Modifiers::NONE,
                },
            ]
        };
        let _ = show_overview(&context, &mut panel, &overview, pointer_events(true));
        let _ = show_overview(&context, &mut panel, &overview, pointer_events(false));

        let expanded = show_overview(&context, &mut panel, &overview, Vec::new());
        let full_overview = text_shape(&expanded, &overview).expect("full overview is rendered");
        assert!(full_overview.galley.rows.len() > 6);
        assert!(text_shape(&expanded, "Less").is_some());
        assert!(text_shape(&expanded, "More").is_none());
    }

    fn show_overview(
        context: &egui::Context,
        panel: &mut DetailPanel,
        overview: &str,
        events: Vec<Event>,
    ) -> egui::FullOutput {
        let mut output = context.run_ui(
            RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(800.0, 600.0))),
                events,
                ..Default::default()
            },
            |ui| {
                ui.allocate_ui(Vec2::new(800.0, 600.0), |ui| {
                    panel.show_overview(ui, overview);
                });
            },
        );
        output.textures_delta.clear();
        output
    }

    fn text_shape<'a>(
        output: &'a egui::FullOutput,
        text: &str,
    ) -> Option<&'a egui::epaint::TextShape> {
        fn find<'a>(shape: &'a Shape, text: &str) -> Option<&'a egui::epaint::TextShape> {
            match shape {
                Shape::Text(shape) if shape.galley.job.text == text => Some(shape),
                Shape::Vec(shapes) => shapes.iter().find_map(|shape| find(shape, text)),
                _ => None,
            }
        }

        output
            .shapes
            .iter()
            .find_map(|clipped| find(&clipped.shape, text))
    }

    fn text_shape_ending_with(
        output: &egui::FullOutput,
        suffix: char,
    ) -> Option<&egui::epaint::TextShape> {
        fn find(shape: &Shape, suffix: char) -> Option<&egui::epaint::TextShape> {
            match shape {
                Shape::Text(shape) if shape.galley.job.text.ends_with(suffix) => Some(shape),
                Shape::Vec(shapes) => shapes.iter().find_map(|shape| find(shape, suffix)),
                _ => None,
            }
        }

        output
            .shapes
            .iter()
            .find_map(|clipped| find(&clipped.shape, suffix))
    }

    #[test]
    fn long_title_and_wrapped_actions_expand_hero_without_overlapping_back_button() {
        let item: BaseItemDto = serde_json::from_value(json!({
            "Id": "item-long",
            "Name": "An Extraordinarily Long Movie Title That Wraps Across Several Lines In Narrower Heroes",
            "ProductionYear": 2024,
            "RunTimeTicks": 72000000000_i64,
            "OfficialRating": "PG-13",
            "Genres": ["Science Fiction", "Adventure", "Mystery", "Drama"],
            "Overview": "Overview text rendered below the hero."
        }))
        .expect("fixture parses");

        let context = egui::Context::default();
        let mut panel = DetailPanel::default();
        let mut output = context.run_ui(
            RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(640.0, 1000.0))),
                ..Default::default()
            },
            |ui| {
                panel.show(ui, &item, None, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        for label in [
                            "Play",
                            "Trailer",
                            "Mark watched",
                            "Add to favorites",
                            "Download",
                            "More options",
                        ] {
                            let _ = ui.add_sized(Vec2::new(140.0, 44.0), egui::Button::new(label));
                        }
                    });
                });
            },
        );
        output.textures_delta.clear();

        let back = text_shape(&output, "←  Back").expect("Back button is rendered");
        let title = text_shape(&output, &item.name).expect("Title is rendered");
        let overview = text_shape(&output, "Overview text rendered below the hero.")
            .expect("Overview rendered");
        let more_options =
            text_shape(&output, "More options").expect("Last wrapped action rendered");

        assert!(
            back.pos.y >= 16.0,
            "Back button should stay near top: {back:?}"
        );
        let back_bottom = back.pos.y + back.galley.size().y;
        assert!(
            title.pos.y > back_bottom + 8.0,
            "Title top ({}) must stay below Back button bottom ({back_bottom})",
            title.pos.y
        );
        assert!(
            overview.pos.y > more_options.pos.y + more_options.galley.size().y,
            "Overview ({}) must render below wrapped hero actions ({})",
            overview.pos.y,
            more_options.pos.y
        );
    }

    #[test]
    fn chapter_timestamp_formats_minutes_and_hours() {
        assert_eq!(super::format_chapter_timestamp(0), "00:00");
        assert_eq!(super::format_chapter_timestamp(125 * 10_000_000), "02:05");
        assert_eq!(
            super::format_chapter_timestamp(3_661 * 10_000_000),
            "01:01:01"
        );
    }

    #[test]
    fn external_links_build_expected_urls_for_movie_and_series() {
        let movie: BaseItemDto = serde_json::from_value(json!({
            "Id": "m1",
            "Name": "Movie",
            "Type": "Movie",
            "ProviderIds": {
                "Imdb": "tt0133093",
                "Tmdb": "603",
                "Tvdb": ""
            }
        }))
        .expect("movie fixture parses");
        assert_eq!(
            super::external_links_for_item(&movie),
            vec![
                ("IMDb", "https://www.imdb.com/title/tt0133093".to_owned()),
                ("TMDB", "https://www.themoviedb.org/movie/603".to_owned()),
            ]
        );

        let series: BaseItemDto = serde_json::from_value(json!({
            "Id": "s1",
            "Name": "Series",
            "Type": "Series",
            "ProviderIds": {
                "Tmdb": "1399",
                "Tvdb": "121361"
            }
        }))
        .expect("series fixture parses");
        assert_eq!(
            super::external_links_for_item(&series),
            vec![
                ("TMDB", "https://www.themoviedb.org/tv/1399".to_owned()),
                (
                    "TVDb",
                    "https://thetvdb.com/?tab=series&id=121361".to_owned()
                ),
            ]
        );
    }

    #[test]
    fn chapters_strip_renders_and_clicking_chapter_records_ticks() {
        let item: BaseItemDto = serde_json::from_value(json!({
            "Id": "item-chapters",
            "Name": "Chaptered Movie",
            "Chapters": [
                { "StartPositionTicks": 0_u64, "Name": "Opening" },
                { "StartPositionTicks": 900_000_000_u64, "Name": "" }
            ]
        }))
        .expect("fixture parses");

        let context = egui::Context::default();
        let mut panel = DetailPanel::default();
        let mut output = context.run_ui(
            RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(800.0, 900.0))),
                ..Default::default()
            },
            |ui| {
                panel.show(ui, &item, None, |_| {});
            },
        );
        output.textures_delta.clear();

        assert!(text_shape(&output, "Scenes").is_some());
        assert!(text_shape(&output, "Opening").is_some());
        assert!(text_shape(&output, "00:00").is_some());
        assert!(text_shape(&output, "Chapter 2").is_some());
        assert!(text_shape(&output, "01:30").is_some());

        let chapter_two = text_shape(&output, "Chapter 2").expect("Chapter 2 rendered");
        let click_pos = chapter_two.pos + chapter_two.galley.rect.size() / 2.0;
        let click_events = |pressed| {
            vec![
                Event::PointerMoved(click_pos),
                Event::PointerButton {
                    pos: click_pos,
                    button: PointerButton::Primary,
                    pressed,
                    modifiers: Modifiers::NONE,
                },
            ]
        };
        let _ = context.run_ui(
            RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(800.0, 900.0))),
                events: click_events(true),
                ..Default::default()
            },
            |ui| {
                panel.show(ui, &item, None, |_| {});
            },
        );
        let _ = context.run_ui(
            RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(800.0, 900.0))),
                events: click_events(false),
                ..Default::default()
            },
            |ui| {
                panel.show(ui, &item, None, |_| {});
            },
        );
        assert_eq!(panel.take_requested_chapter_ticks(), Some(900_000_000));
        assert_eq!(panel.take_requested_chapter_ticks(), None);
    }
}
