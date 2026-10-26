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
}

impl DetailPanel {
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
        let height = (width * 9.0 / 16.0).clamp(260.0, 460.0);
        let (rect, _) = ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::hover());
        let painter = ui.painter_at(rect);
        match artwork {
            Some(texture) => {
                let uv = super::row::cover_uv(texture.size_vec2(), rect.size());
                painter.image(texture.id(), rect, uv, Color32::WHITE);
            }
            None => {
                painter.rect_filled(
                    rect,
                    egui::CornerRadius::ZERO,
                    Color32::from_rgb(31, 31, 34),
                );
                painter.text(
                    egui::pos2(rect.right() - 28.0, rect.top() + 48.0),
                    egui::Align2::RIGHT_CENTER,
                    ui_text(ids::DETAIL_ARTWORK_UNAVAILABLE),
                    FontId::proportional(14.0),
                    Color32::from_gray(140),
                );
            }
        }
        // Darken the left (behind the text) and the bottom so the title reads on any artwork.
        let dark = Color32::from_black_alpha(215);
        let clear = Color32::TRANSPARENT;
        let left_band = egui::Rect::from_min_max(
            rect.left_top(),
            egui::pos2(rect.left() + rect.width() * 0.65, rect.bottom()),
        );
        super::row::gradient_quad(&painter, left_band, [dark, clear, clear, dark]);
        let bottom_band = egui::Rect::from_min_max(
            egui::pos2(rect.left(), rect.top() + rect.height() * 0.4),
            rect.right_bottom(),
        );
        super::row::gradient_quad(&painter, bottom_band, [clear, clear, dark, dark]);

        let pad = 28.0;
        let back_rect = egui::Rect::from_min_size(
            egui::pos2(rect.left() + pad, rect.top() + pad),
            egui::vec2(104.0, 40.0),
        );
        let back = ui
            .put(
                back_rect,
                egui::Button::new(
                    RichText::new("←  Back")
                        .size(16.0)
                        .strong()
                        .color(Color32::WHITE),
                )
                .fill(Color32::from_black_alpha(150))
                .corner_radius(egui::CornerRadius::same(20)),
            )
            .clicked();

        let text_width = (width * 0.62).min(700.0);
        let text_rect = egui::Rect::from_min_max(
            egui::pos2(rect.left() + pad, rect.top() + pad + 56.0),
            egui::pos2(rect.left() + pad + text_width, rect.bottom() - pad),
        );
        ui.scope_builder(
            egui::UiBuilder::new()
                .max_rect(text_rect)
                .layout(egui::Layout::bottom_up(egui::Align::Min)),
            |ui| {
                actions(ui);
                if let Some(genres) = visible_genres(item) {
                    ui.add_space(8.0);
                    ui.label(RichText::new(genres).color(Color32::from_gray(205)));
                }
                let metadata = metadata_labels(item);
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
                ui.add_space(8.0);
                ui.add(
                    egui::Label::new(
                        RichText::new(&item.name)
                            .size(42.0)
                            .strong()
                            .color(Color32::WHITE),
                    )
                    .wrap(),
                );
            },
        );

        if let Some(overview) = item
            .overview
            .as_deref()
            .map(str::trim)
            .filter(|overview| !overview.is_empty())
        {
            ui.scope(|ui| {
                ui.set_max_width(width.min(860.0));
                self.show_overview(ui, overview);
            });
        }
        DetailHeaderResponse { back }
    }

    fn show_overview(&mut self, ui: &mut Ui, overview: &str) {
        ui.add_space(14.0);
        let width = ui.available_width().max(1.0);
        let (preview, has_more) = if self.overview_expanded {
            (overview.to_owned(), false)
        } else {
            clamp_overview(ui, overview, width)
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
    let mut labels = Vec::with_capacity(3);
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
    labels
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
}
