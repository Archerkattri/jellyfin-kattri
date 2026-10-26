//! Presentation-only series seasons and episodes for the egui client.
//!
//! This component owns the selected season and rendering state, but leaves data
//! loading, errors, and playback actions to its caller.

use super::{ui_text, ui_text_args};
use egui::{Color32, ProgressBar, RichText, Ui};
use jellyfin_api::BaseItemDto;
use kattri_i18n::ids;

const TICKS_PER_MINUTE: i64 = 10_000_000 * 60;

/// Stateful season selector. The selected season is retained by item ID when
/// refreshed data arrives, and otherwise falls back to the first ordered season.
#[derive(Debug, Default)]
pub struct SeriesPresentation {
    selected_season_id: Option<String>,
}

/// User-visible result of drawing the series presentation.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SeriesPresentationResponse {
    pub selected_season_id: Option<String>,
    pub activated_episode_id: Option<String>,
}

impl SeriesPresentation {
    /// Draws season tabs and the episodes belonging to the selected season.
    pub fn show(
        &mut self,
        ui: &mut Ui,
        seasons: &[BaseItemDto],
        episodes: &[BaseItemDto],
    ) -> SeriesPresentationResponse {
        let ordered = ordered_seasons(seasons);
        if ordered.is_empty() {
            self.selected_season_id = None;
            ui.label(RichText::new(ui_text(ids::SERIES_EMPTY_NO_SEASONS)).weak());
            return SeriesPresentationResponse::default();
        }

        if !ordered.iter().any(|season| {
            self.selected_season_id
                .as_deref()
                .is_some_and(|selected| selected == season.id)
        }) {
            self.selected_season_id = Some(ordered[0].id.clone());
        }

        ui.add_space(6.0);
        let season_tab_width = (ui.available_width() / 4.0).clamp(112.0, 170.0);
        ui.horizontal_wrapped(|ui| {
            for season in &ordered {
                let label = season_label(season);
                let selected = self.selected_season_id.as_deref() == Some(season.id.as_str());
                let response = ui.add_sized(
                    egui::vec2(season_tab_width, 42.0),
                    egui::Button::selectable(selected, label),
                );
                if response.clicked() {
                    self.selected_season_id = Some(season.id.clone());
                }
            }
        });

        let selected_season = ordered
            .iter()
            .find(|season| self.selected_season_id.as_deref() == Some(season.id.as_str()));
        let mut activated_episode_id = None;
        if let Some(season) = selected_season {
            let season_index = season.index_number;
            let matching_episodes = episodes
                .iter()
                .filter(|episode| episode.parent_index_number == season_index)
                .collect::<Vec<_>>();
            let matching_episodes = ordered_episode_refs(&matching_episodes);

            ui.add_space(8.0);
            if matching_episodes.is_empty() {
                egui::Frame::new()
                    .fill(Color32::from_rgb(31, 31, 34))
                    .corner_radius(egui::CornerRadius::same(12))
                    .inner_margin(egui::Margin::symmetric(18, 16))
                    .show(ui, |ui| {
                        ui.label(
                            RichText::new(ui_text(ids::SERIES_EMPTY_NO_EPISODES))
                                .color(Color32::from_gray(164)),
                        );
                    });
            } else {
                ui.add_space(4.0);
                for episode in matching_episodes {
                    if episode_row(ui, episode).clicked() {
                        activated_episode_id = Some(episode.id.clone());
                    }
                }
            }
        }

        SeriesPresentationResponse {
            selected_season_id: self.selected_season_id.clone(),
            activated_episode_id,
        }
    }
}

/// Orders numbered seasons numerically, then Specials (season zero), then
/// seasons without an index. IDs make ties deterministic.
pub fn ordered_seasons(seasons: &[BaseItemDto]) -> Vec<&BaseItemDto> {
    let mut ordered = seasons.iter().collect::<Vec<_>>();
    ordered.sort_by_key(|season| season_order(season));
    ordered
}

fn season_order(season: &BaseItemDto) -> (u8, i32, &str) {
    match season.index_number {
        Some(0) => (1, 0, &season.id),
        Some(number) => (0, number, &season.id),
        None => (2, 0, &season.id),
    }
}

fn ordered_episode_refs<'a>(episodes: &[&'a BaseItemDto]) -> Vec<&'a BaseItemDto> {
    let mut ordered = episodes.to_vec();
    ordered.sort_by(
        |left, right| match (left.index_number, right.index_number) {
            (Some(left_number), Some(right_number)) => left_number
                .cmp(&right_number)
                .then_with(|| left.id.cmp(&right.id)),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => left
                .premiere_date
                .is_none()
                .cmp(&right.premiere_date.is_none())
                .then_with(|| left.premiere_date.cmp(&right.premiere_date))
                .then_with(|| left.id.cmp(&right.id)),
        },
    );
    ordered
}

fn season_label(season: &BaseItemDto) -> String {
    let name = season.name.trim();
    if !name.is_empty() {
        name.to_owned()
    } else if season.index_number == Some(0) {
        ui_text(ids::SERIES_SEASON_SPECIALS).to_owned()
    } else {
        season.index_number.map_or_else(
            || ui_text(ids::SERIES_SEASON_DEFAULT).to_owned(),
            |number| {
                ui_text_args(
                    ids::SERIES_SEASON_NUMBERED,
                    &[("number", &number.to_string())],
                )
            },
        )
    }
}

fn episode_row(ui: &mut Ui, episode: &BaseItemDto) -> egui::Response {
    ui.push_id(&episode.id, |ui| {
        egui::Frame::new()
            .fill(Color32::from_rgb(21, 21, 23))
            .corner_radius(egui::CornerRadius::same(12))
            .inner_margin(egui::Margin::symmetric(14, 10))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let response = ui.add_sized(
                        egui::vec2((ui.available_width() - 190.0).max(140.0), 42.0),
                        egui::Button::new(RichText::new(episode_label(episode)).strong()),
                    );
                    if let Some(runtime) = format_runtime(episode.run_time_ticks) {
                        ui.label(RichText::new(runtime).color(Color32::from_gray(164)));
                    }
                    if let Some(progress) = progress_fraction(episode) {
                        ui.add(
                            ProgressBar::new(progress)
                                .desired_width(ui.available_width().min(112.0))
                                .desired_height(7.0)
                                .show_percentage(),
                        );
                    }
                    response
                })
                .inner
            })
            .inner
    })
    .inner
}

fn episode_label(episode: &BaseItemDto) -> String {
    match episode.index_number {
        Some(index) => format!("E{index}  {}", episode.name),
        None => episode.name.clone(),
    }
}

fn format_runtime(runtime_ticks: Option<i64>) -> Option<String> {
    let total_minutes = runtime_ticks.filter(|ticks| *ticks > 0)? / TICKS_PER_MINUTE;
    if total_minutes == 0 {
        return None;
    }
    let hours = total_minutes / 60;
    let minutes = total_minutes % 60;
    Some(if hours > 0 {
        ui_text_args(
            ids::TIME_HOURS_MINUTES,
            &[
                ("hours", &hours.to_string()),
                ("minutes", &minutes.to_string()),
            ],
        )
    } else {
        ui_text_args(ids::TIME_MINUTES, &[("minutes", &minutes.to_string())])
    })
}

fn progress_fraction(episode: &BaseItemDto) -> Option<f32> {
    let user_data = episode.user_data.as_ref()?;
    if let Some(percentage) = user_data
        .played_percentage
        .filter(|percentage| percentage.is_finite() && *percentage > 0.0)
    {
        return Some((percentage / 100.0).clamp(0.0, 1.0) as f32);
    }

    let position = user_data.playback_position_ticks?;
    let runtime = episode.run_time_ticks?;
    if position <= 0 || runtime <= 0 {
        return None;
    }
    Some((position as f64 / runtime as f64).clamp(0.0, 1.0) as f32)
}

#[cfg(test)]
mod tests {
    use super::{
        SeriesPresentation, episode_label, format_runtime, ordered_episode_refs, ordered_seasons,
        progress_fraction,
    };
    use egui::{Pos2, RawInput, Rect, Shape, Vec2};
    use jellyfin_api::BaseItemDto;
    use serde_json::json;

    const TICKS_PER_MINUTE: i64 = 10_000_000 * 60;

    fn item(id: &str, name: &str, index: Option<i32>) -> BaseItemDto {
        let mut value = json!({ "Id": id, "Name": name, "Type": "Season" });
        if let Some(index) = index {
            value["IndexNumber"] = json!(index);
        }
        serde_json::from_value(value).expect("valid Jellyfin BaseItemDto fixture")
    }

    fn text_shapes(output: &egui::FullOutput) -> Vec<String> {
        fn collect(shape: &Shape, labels: &mut Vec<String>) {
            match shape {
                Shape::Text(text) => labels.push(text.galley.job.text.clone()),
                Shape::Vec(shapes) => {
                    for shape in shapes {
                        collect(shape, labels);
                    }
                }
                _ => {}
            }
        }

        let mut labels = Vec::new();
        for clipped in &output.shapes {
            collect(&clipped.shape, &mut labels);
        }
        labels
    }

    #[test]
    fn regular_seasons_are_numbered_with_zero_last() {
        let seasons = [
            item("s0", "Specials", Some(0)),
            item("s2", "Season 2", Some(2)),
            item("s1", "Season 1", Some(1)),
        ];
        let ordered = ordered_seasons(&seasons);
        assert_eq!(
            ordered
                .iter()
                .map(|season| season.id.as_str())
                .collect::<Vec<_>>(),
            ["s1", "s2", "s0"]
        );
    }

    #[test]
    fn missing_index_season_sorts_after_numbered_seasons_and_specials() {
        let seasons = [
            item("missing", "Unnumbered", None),
            item("s0", "Specials", Some(0)),
            item("s3", "Season 3", Some(3)),
        ];
        let ordered = ordered_seasons(&seasons);
        assert_eq!(
            ordered
                .iter()
                .map(|season| season.id.as_str())
                .collect::<Vec<_>>(),
            ["s3", "s0", "missing"]
        );
    }

    #[test]
    fn duplicate_numbers_are_stable_by_item_id() {
        let seasons = [
            item("z-season", "Duplicate", Some(2)),
            item("a-season", "Duplicate", Some(2)),
        ];
        let ordered = ordered_seasons(&seasons);
        assert_eq!(ordered[0].id, "a-season");
        assert_eq!(ordered[1].id, "z-season");
    }

    #[test]
    fn unnumbered_episodes_follow_numbered_episodes_by_date_then_id() {
        let make_episode = |id: &str, index: Option<i32>, date: Option<&str>| {
            let mut value = json!({ "Id": id, "Name": id, "Type": "Episode" });
            if let Some(index) = index {
                value["IndexNumber"] = json!(index);
            }
            if let Some(date) = date {
                value["PremiereDate"] = json!(date);
            }
            serde_json::from_value::<BaseItemDto>(value).expect("valid episode fixture")
        };
        let episodes = [
            make_episode("later", None, Some("2022-01-01")),
            make_episode("number-2", Some(2), None),
            make_episode("undated-z", None, None),
            make_episode("number-1", Some(1), None),
            make_episode("earlier", None, Some("2021-01-01")),
            make_episode("undated-a", None, None),
        ];
        let ordered = ordered_episode_refs(&episodes.iter().collect::<Vec<_>>());
        assert_eq!(
            ordered
                .iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>(),
            [
                "number-1",
                "number-2",
                "earlier",
                "later",
                "undated-a",
                "undated-z"
            ]
        );
    }

    #[test]
    fn selection_defaults_to_first_ordered_season_and_preserves_valid_choice() {
        let seasons = [
            item("s2", "Season 2", Some(2)),
            item("s1", "Season 1", Some(1)),
        ];
        let mut component = SeriesPresentation::default();
        let context = egui::Context::default();
        let mut output = context.run_ui(RawInput::default(), |ui| {
            component.show(ui, &seasons, &[]);
        });
        output.textures_delta.clear();
        assert_eq!(component.selected_season_id.as_deref(), Some("s1"));

        component.selected_season_id = Some("s2".to_owned());
        let mut output = context.run_ui(RawInput::default(), |ui| {
            component.show(ui, &seasons, &[]);
        });
        output.textures_delta.clear();
        assert_eq!(component.selected_season_id.as_deref(), Some("s2"));
    }

    #[test]
    fn episode_row_renders_index_runtime_and_user_progress() {
        let episode: BaseItemDto = serde_json::from_value(json!({
            "Id": "e2",
            "Name": "The Long Night",
            "Type": "Episode",
            "IndexNumber": 2,
            "ParentIndexNumber": 1,
            "RunTimeTicks": 47 * TICKS_PER_MINUTE,
            "UserData": { "PlayedPercentage": 50.0 }
        }))
        .expect("valid Jellyfin episode fixture");
        let season = item("s1", "Season 1", Some(1));
        let mut component = SeriesPresentation::default();
        let context = egui::Context::default();
        let mut output = context.run_ui(
            RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(800.0, 500.0))),
                ..Default::default()
            },
            |ui| {
                component.show(
                    ui,
                    std::slice::from_ref(&season),
                    std::slice::from_ref(&episode),
                );
            },
        );
        output.textures_delta.clear();
        let labels = text_shapes(&output);
        assert!(labels.iter().any(|label| label == "E2  The Long Night"));
        assert!(labels.iter().any(|label| label == "47m"));
        assert!(labels.iter().any(|label| label == "50%"));
    }

    #[test]
    fn progress_prefers_percentage_then_uses_position_and_runtime() {
        let episode: BaseItemDto = serde_json::from_value(json!({
            "Id": "e1", "Name": "Episode", "Type": "Episode",
            "RunTimeTicks": 1000,
            "UserData": { "PlayedPercentage": 25.0, "PlaybackPositionTicks": 500 }
        }))
        .expect("valid Jellyfin episode fixture");
        assert_eq!(progress_fraction(&episode), Some(0.25));

        let position_only: BaseItemDto = serde_json::from_value(json!({
            "Id": "e2", "Name": "Episode", "Type": "Episode",
            "RunTimeTicks": 1000,
            "UserData": { "PlaybackPositionTicks": 500 }
        }))
        .expect("valid Jellyfin episode fixture");
        assert_eq!(progress_fraction(&position_only), Some(0.5));
        assert_eq!(episode_label(&position_only), "Episode");
    }

    #[test]
    fn runtime_formatter_hides_unusable_values() {
        assert_eq!(format_runtime(None), None);
        assert_eq!(format_runtime(Some(0)), None);
        assert_eq!(
            format_runtime(Some(65 * TICKS_PER_MINUTE)),
            Some("1h 5m".into())
        );
    }
}
