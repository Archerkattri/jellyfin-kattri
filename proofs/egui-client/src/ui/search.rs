//! Presentation-only grouped search results.
//!
//! Search requests and navigation remain owned by the caller. This module only
//! groups the supplied Jellyfin DTOs, draws visible groups, and returns an
//! activated item's ID.

use super::{ui_text, ui_text_args};
use egui::{self, Response, Ui};
use jellyfin_api::BaseItemDto;
use kattri_i18n::ids;

pub const SEARCH_GROUP_RESULT_LIMIT: usize = 10;

/// Search-result categories in their stable presentation order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearchGroup {
    Movies,
    Series,
    Episodes,
    Music,
    Playlists,
    Photos,
    People,
    Channels,
}

fn show_all_button(ui: &mut Ui, group: SearchGroup) -> Response {
    ui.push_id(("search-show-all", group.label()), |ui| {
        ui.button(ui_text_args(
            ids::SEARCH_BUTTON_SHOW_ALL,
            &[("group", group.label())],
        ))
    })
    .inner
}

impl SearchGroup {
    pub const fn item_types(self) -> &'static [&'static str] {
        match self {
            Self::Movies => &["Movie"],
            Self::Series => &["Series"],
            Self::Episodes => &["Episode"],
            Self::Music => &["Audio", "MusicAlbum"],
            Self::Playlists => &["Playlist"],
            Self::Photos => &["Photo"],
            Self::People => &["Person"],
            Self::Channels => &["TvChannel", "LiveTvChannel"],
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SearchAction {
    SelectItem(String),
    ShowAll(SearchGroup),
}

impl SearchGroup {
    pub fn label(self) -> &'static str {
        match self {
            Self::Movies => ui_text(ids::SEARCH_CATEGORY_MOVIES),
            Self::Series => ui_text(ids::SEARCH_CATEGORY_SERIES),
            Self::Episodes => ui_text(ids::SEARCH_CATEGORY_EPISODES),
            Self::Music => ui_text(ids::SEARCH_CATEGORY_MUSIC),
            Self::Playlists => ui_text(ids::SEARCH_CATEGORY_PLAYLISTS),
            Self::Photos => ui_text(ids::SEARCH_CATEGORY_PHOTOS),
            Self::People => ui_text(ids::SEARCH_CATEGORY_PEOPLE),
            Self::Channels => ui_text(ids::SEARCH_CATEGORY_CHANNELS),
        }
    }
}

/// Test adapter for presentation assertions that do not inspect the focus graph.
#[cfg(test)]
fn draw_with_channel_total(
    ui: &mut Ui,
    results: &[BaseItemDto],
    channel_total: Option<u32>,
) -> Option<SearchAction> {
    draw_with_channel_total_and_focus(ui, results, channel_total, |_, _, _| {})
}

/// Draw the search results and report each visible result button with its focus row. The caller
/// uses these responses in the same directional-navigation graph as catalog cards.
#[cfg(test)]
pub(super) fn draw_with_channel_total_and_focus(
    ui: &mut Ui,
    results: &[BaseItemDto],
    channel_total: Option<u32>,
    result_focus: impl FnMut(usize, &BaseItemDto, egui::Response),
) -> Option<SearchAction> {
    draw_with_focus_targets(ui, results, &[], channel_total, result_focus, |_, _, _| {})
}

/// Draw results and report both media cards and the Show All controls as focus targets.
/// `extra_results` are drawn as further matches in the same groups, so the caller can pass a
/// second request's results without cloning them into one vector.
pub(super) fn draw_with_focus_targets(
    ui: &mut Ui,
    results: &[BaseItemDto],
    extra_results: &[BaseItemDto],
    channel_total: Option<u32>,
    mut result_focus: impl FnMut(usize, &BaseItemDto, egui::Response),
    mut show_all_focus: impl FnMut(usize, SearchGroup, egui::Response),
) -> Option<SearchAction> {
    let groups = group_results_with(results, extra_results);
    let mut selected_id = None;
    let mut focus_row = 0;
    let mut last_result_center_y: Option<f32> = None;

    for (group, items) in groups {
        if items.is_empty() {
            continue;
        }

        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new(group.label()).size(21.0).strong());
            ui.label(
                egui::RichText::new(format!("{}", items.len()))
                    .size(13.0)
                    .color(egui::Color32::from_gray(164)),
            );
        });
        ui.add_space(6.0);
        let card_width = ((ui.available_width() - 28.0) / 3.0).clamp(112.0, 220.0);
        ui.horizontal_wrapped(|ui| {
            for item in items.iter().take(SEARCH_GROUP_RESULT_LIMIT) {
                let label = egui::RichText::new(&item.name).strong();
                let response = ui.add_sized(egui::vec2(card_width, 54.0), egui::Button::new(label));
                // The same context menu as a library card: a search result is a server DTO, so
                // the menu converts it on demand.
                super::item_menu::attach_dto(&response, item);
                if response.clicked() {
                    selected_id = Some(SearchAction::SelectItem(item.id.clone()));
                }
                let center_y = response.rect.center().y;
                if last_result_center_y.is_some_and(|last_y| center_y > last_y + 1.0) {
                    focus_row += 1;
                }
                last_result_center_y = Some(center_y);
                result_focus(focus_row, item, response);
            }
        });
        let has_more = items.len() > SEARCH_GROUP_RESULT_LIMIT
            || (group == SearchGroup::Channels
                && channel_total.is_some_and(|total| total > SEARCH_GROUP_RESULT_LIMIT as u32));
        if has_more {
            let response = show_all_button(ui, group);
            if let Some(last_y) = last_result_center_y
                && response.rect.center().y > last_y + 1.0
            {
                focus_row += 1;
            }
            last_result_center_y = Some(response.rect.center().y);
            show_all_focus(focus_row, group, response.clone());
            if response.clicked() {
                selected_id = Some(SearchAction::ShowAll(group));
            }
        }
        ui.add_space(12.0);
    }

    selected_id
}

/// Groups recognized item types while preserving result order within a group.
/// Unknown or missing Jellyfin item types are intentionally omitted.
#[cfg(test)]
pub fn group_results(results: &[BaseItemDto]) -> Vec<(SearchGroup, Vec<&BaseItemDto>)> {
    group_results_with(results, &[])
}

/// The same grouping over two result lists, in order.
pub fn group_results_with<'a>(
    results: &'a [BaseItemDto],
    extra: &'a [BaseItemDto],
) -> Vec<(SearchGroup, Vec<&'a BaseItemDto>)> {
    kattri_client_core::search::group_search_results_with(results, extra)
        .into_iter()
        .map(|group| {
            let kind = match group.kind {
                kattri_client_core::search::SearchResultKind::Movies => SearchGroup::Movies,
                kattri_client_core::search::SearchResultKind::Series => SearchGroup::Series,
                kattri_client_core::search::SearchResultKind::Episodes => SearchGroup::Episodes,
                kattri_client_core::search::SearchResultKind::Music => SearchGroup::Music,
                kattri_client_core::search::SearchResultKind::Playlists => SearchGroup::Playlists,
                kattri_client_core::search::SearchResultKind::Photos => SearchGroup::Photos,
                kattri_client_core::search::SearchResultKind::People => SearchGroup::People,
                kattri_client_core::search::SearchResultKind::Channels => SearchGroup::Channels,
            };
            (kind, group.items)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{
        SEARCH_GROUP_RESULT_LIMIT, SearchAction, SearchGroup, draw_with_channel_total,
        draw_with_channel_total_and_focus, draw_with_focus_targets, group_results,
    };
    use egui::{Event, FullOutput, Pos2, RawInput, Rect, Shape, Vec2};
    use jellyfin_api::BaseItemDto;

    fn item(id: &str, name: &str, item_type: &str) -> BaseItemDto {
        serde_json::from_value(serde_json::json!({
            "Id": id,
            "Name": name,
            "Type": item_type,
        }))
        .expect("minimal BaseItemDto fixture deserializes")
    }

    fn output_text(output: &FullOutput) -> Vec<String> {
        fn collect(shape: &Shape, text: &mut Vec<String>) {
            match shape {
                Shape::Text(shape) => text.push(shape.galley.job.text.clone()),
                Shape::Vec(shapes) => {
                    for shape in shapes {
                        collect(shape, text);
                    }
                }
                _ => {}
            }
        }

        let mut text = Vec::new();
        for clipped in &output.shapes {
            collect(&clipped.shape, &mut text);
        }
        text
    }

    fn frame(events: Vec<Event>) -> RawInput {
        frame_sized(events, Vec2::new(800.0, 600.0))
    }

    fn frame_sized(events: Vec<Event>, size: Vec2) -> RawInput {
        RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, size)),
            events,
            ..Default::default()
        }
    }

    #[test]
    fn groups_are_ordered_and_preserve_item_order() {
        let results = [
            item("p", "A Person", "Person"),
            item("c2", "Second Channel", "LiveTvChannel"),
            item("photo", "A Photo", "Photo"),
            item("playlist", "A Playlist", "Playlist"),
            item("e", "An Episode", "Episode"),
            item("m1", "First Movie", "Movie"),
            item("m2", "Second Movie", "Movie"),
            item("s", "A Series", "Series"),
            item("a", "A Track", "Audio"),
            item("c1", "First Channel", "TvChannel"),
        ];

        let groups = group_results(&results);
        assert_eq!(
            groups.iter().map(|(group, _)| *group).collect::<Vec<_>>(),
            [
                SearchGroup::Movies,
                SearchGroup::Series,
                SearchGroup::Episodes,
                SearchGroup::Music,
                SearchGroup::Playlists,
                SearchGroup::Photos,
                SearchGroup::People,
                SearchGroup::Channels,
            ]
        );
        assert_eq!(
            groups[0]
                .1
                .iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>(),
            ["m1", "m2"]
        );
        assert_eq!(groups[1].1[0].id, "s");
        assert_eq!(groups[2].1[0].id, "e");
        assert_eq!(groups[3].1[0].id, "a");
        assert_eq!(groups[4].1[0].id, "playlist");
        assert_eq!(groups[5].1[0].id, "photo");
        assert_eq!(groups[6].1[0].id, "p");
        assert_eq!(
            groups[7]
                .1
                .iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>(),
            ["c2", "c1"]
        );
    }

    #[test]
    fn headless_ui_shows_only_nonempty_group_labels() {
        let context = egui::Context::default();
        let results = [
            item("e", "An Episode", "Episode"),
            item("m", "A Movie", "Movie"),
        ];
        let mut output = context.run_ui(frame(Vec::new()), |ui| {
            assert_eq!(draw_with_channel_total(ui, &results, None), None);
        });
        let labels = output_text(&output);
        output.textures_delta.clear();

        assert!(labels.iter().any(|text| text == "Movies"));
        assert!(labels.iter().any(|text| text == "Episodes"));
        assert!(!labels.iter().any(|text| text == "Series"));
        assert!(!labels.iter().any(|text| text == "Music"));
        assert!(!labels.iter().any(|text| text == "Playlists"));
        assert!(!labels.iter().any(|text| text == "Photos"));
        assert!(!labels.iter().any(|text| text == "People"));
        assert!(!labels.iter().any(|text| text == "Channels"));
    }

    #[test]
    fn headless_ui_presents_new_groups_and_caps_each_at_ten_items() {
        let context = egui::Context::default();
        let mut results = (0..SEARCH_GROUP_RESULT_LIMIT + 2)
            .map(|index| {
                item(
                    &format!("photo-{index}"),
                    &format!("Photo {index}"),
                    "Photo",
                )
            })
            .collect::<Vec<_>>();
        results.extend([
            item("playlist", "Saved Playlist", "Playlist"),
            item("channel", "Local Channel", "LiveTvChannel"),
        ]);

        let mut output = context.run_ui(frame(Vec::new()), |ui| {
            assert_eq!(draw_with_channel_total(ui, &results, None), None);
        });
        let labels = output_text(&output);
        output.textures_delta.clear();

        for expected in ["Playlists", "Photos", "Channels", "Photo 0", "Photo 9"] {
            assert!(
                labels.iter().any(|text| text == expected),
                "missing {expected}"
            );
        }
        assert!(!labels.iter().any(|text| text == "Photo 10"));
        assert!(!labels.iter().any(|text| text == "Photo 11"));
    }

    #[test]
    fn headless_ui_returns_activated_item_id() {
        let context = egui::Context::default();
        let results = [item("movie-id", "A Movie", "Movie")];
        let selected = std::cell::RefCell::new(None);
        let mut initial = context.run_ui(frame(Vec::new()), |ui| {
            draw_with_channel_total(ui, &results, None);
        });
        let button_center = initial
            .shapes
            .iter()
            .find_map(|clipped| match &clipped.shape {
                Shape::Text(text) if text.galley.job.text == "A Movie" => {
                    Some(text.pos + text.galley.size() * 0.5)
                }
                _ => None,
            })
            .expect("movie button text is laid out");
        initial.textures_delta.clear();
        let mut clicked = context.run_ui(
            frame(vec![
                Event::PointerMoved(button_center),
                Event::PointerButton {
                    pos: button_center,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::default(),
                },
                Event::PointerButton {
                    pos: button_center,
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: egui::Modifiers::default(),
                },
            ]),
            |ui| *selected.borrow_mut() = draw_with_channel_total(ui, &results, None),
        );
        clicked.textures_delta.clear();

        assert_eq!(
            selected.borrow().as_ref(),
            Some(&SearchAction::SelectItem("movie-id".into()))
        );
    }

    #[test]
    fn show_all_is_only_offered_when_a_group_exceeds_ten_results() {
        let context = egui::Context::default();
        let results = (0..SEARCH_GROUP_RESULT_LIMIT + 1)
            .map(|index| {
                item(
                    &format!("photo-{index}"),
                    &format!("Photo {index}"),
                    "Photo",
                )
            })
            .collect::<Vec<_>>();
        let mut output = context.run_ui(frame(Vec::new()), |ui| {
            assert_eq!(draw_with_channel_total(ui, &results, None), None);
        });
        let labels = output_text(&output);
        output.textures_delta.clear();
        assert!(labels.iter().any(|text| text == "Show all Photos"));

        let short = [item("one", "One photo", "Photo")];
        let mut short_output = context.run_ui(frame(Vec::new()), |ui| {
            assert_eq!(draw_with_channel_total(ui, &short, None), None);
        });
        let short_labels = output_text(&short_output);
        short_output.textures_delta.clear();
        assert!(!short_labels.iter().any(|text| text == "Show all Photos"));
    }

    #[test]
    fn clicking_show_all_returns_the_selected_group() {
        let context = egui::Context::default();
        let results = (0..SEARCH_GROUP_RESULT_LIMIT + 1)
            .map(|index| {
                item(
                    &format!("channel-{index}"),
                    &format!("Channel {index}"),
                    "TvChannel",
                )
            })
            .collect::<Vec<_>>();
        let mut initial = context.run_ui(frame(Vec::new()), |ui| {
            draw_with_channel_total(ui, &results, None);
        });
        let button_center = initial
            .shapes
            .iter()
            .find_map(|clipped| match &clipped.shape {
                Shape::Text(text) if text.galley.job.text == "Show all Channels" => {
                    Some(text.pos + text.galley.size() * 0.5)
                }
                _ => None,
            })
            .expect("show-all button is laid out");
        initial.textures_delta.clear();
        let mut selected = None;
        let mut clicked = context.run_ui(
            frame(vec![
                Event::PointerMoved(button_center),
                Event::PointerButton {
                    pos: button_center,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::default(),
                },
                Event::PointerButton {
                    pos: button_center,
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: egui::Modifiers::default(),
                },
            ]),
            |ui| selected = draw_with_channel_total(ui, &results, None),
        );
        clicked.textures_delta.clear();
        assert_eq!(selected, Some(SearchAction::ShowAll(SearchGroup::Channels)));
    }

    #[test]
    fn channel_total_offers_show_all_at_exactly_ten_returned_results() {
        let context = egui::Context::default();
        let results = (0..SEARCH_GROUP_RESULT_LIMIT)
            .map(|index| {
                item(
                    &format!("channel-{index}"),
                    &format!("Channel {index}"),
                    "TvChannel",
                )
            })
            .collect::<Vec<_>>();
        let mut output = context.run_ui(frame(Vec::new()), |ui| {
            assert_eq!(draw_with_channel_total(ui, &results, Some(47)), None);
        });
        let labels = output_text(&output);
        output.textures_delta.clear();
        assert!(labels.iter().any(|text| text == "Show all Channels"));
    }

    #[test]
    fn show_all_is_reported_as_a_focus_target_on_its_own_visual_row() {
        let context = egui::Context::default();
        let results = (0..SEARCH_GROUP_RESULT_LIMIT + 1)
            .map(|index| {
                item(
                    &format!("photo-{index}"),
                    &format!("Photo {index}"),
                    "Photo",
                )
            })
            .collect::<Vec<_>>();
        let mut targets = Vec::new();
        let mut show_all_targets = Vec::new();
        let mut output = context.run_ui(frame(Vec::new()), |ui| {
            draw_with_focus_targets(
                ui,
                &results,
                &[],
                None,
                |row, item, response| {
                    targets.push((row, Some(item.id.clone()), response.rect));
                },
                |row, group, response| {
                    show_all_targets.push((row, None::<String>, response.rect));
                    assert_eq!(group, SearchGroup::Photos);
                },
            );
        });
        output.textures_delta.clear();

        let show_all = show_all_targets
            .iter()
            .find(|(_, item_id, _)| item_id.is_none())
            .expect("Show All is registered as a focus target");
        let last_card = targets
            .iter()
            .filter(|(_, item_id, _)| item_id.is_some())
            .max_by_key(|(_, _, rect)| rect.center().y as i32)
            .expect("visible cards are registered");
        assert!(show_all.0 > last_card.0);
        assert!(show_all.2.is_positive());
    }

    #[test]
    fn visible_results_report_distinct_focus_rows_in_display_order() {
        let context = egui::Context::default();
        let results = [
            item("movie", "A Movie", "Movie"),
            item("episode", "An Episode", "Episode"),
            item("movie-2", "Another Movie", "Movie"),
        ];
        let mut focus = Vec::new();
        let mut output = context.run_ui(frame(Vec::new()), |ui| {
            let action =
                draw_with_channel_total_and_focus(ui, &results, None, |row, item, response| {
                    focus.push((row, item.id.clone(), response.rect));
                });
            assert_eq!(action, None);
        });
        output.textures_delta.clear();

        assert_eq!(
            focus
                .iter()
                .map(|(row, id, _)| (*row, id.as_str()))
                .collect::<Vec<_>>(),
            [(0, "movie"), (0, "movie-2"), (1, "episode")]
        );
        assert!(focus.iter().all(|(_, _, rect)| rect.is_positive()));
    }

    #[test]
    fn narrow_wrapped_results_get_focus_rows_matching_their_visual_rows() {
        let context = egui::Context::default();
        let mut results = (0..6)
            .map(|index| {
                item(
                    &format!("movie-{index}"),
                    &format!("Movie {index}"),
                    "Movie",
                )
            })
            .collect::<Vec<_>>();
        results.push(item("episode", "Episode", "Episode"));

        let mut focus = Vec::new();
        let mut output = context.run_ui(frame_sized(Vec::new(), Vec2::new(320.0, 1000.0)), |ui| {
            let action =
                draw_with_channel_total_and_focus(ui, &results, None, |row, item, response| {
                    focus.push((row, item.id.clone(), response.rect));
                });
            assert_eq!(action, None);
        });
        output.textures_delta.clear();

        let movie_rows: Vec<_> = focus
            .iter()
            .filter(|(_, id, _)| id.starts_with("movie-"))
            .map(|(row, _, _)| *row)
            .collect();
        assert!(
            movie_rows
                .iter()
                .copied()
                .collect::<std::collections::BTreeSet<_>>()
                .len()
                > 1,
            "the narrow viewport wraps movies onto multiple focus rows: {focus:?}"
        );
        for (index, (_, _, rect)) in focus.iter().enumerate() {
            for (other_row, _, other_rect) in &focus[index + 1..] {
                if (rect.center().y - other_rect.center().y).abs() < 1.0 {
                    assert_eq!(focus[index].0, *other_row, "same visual row: {focus:?}");
                }
            }
        }
        assert!(
            focus.windows(2).all(|pair| pair[0].0 <= pair[1].0),
            "focus rows stay monotonic across category groups: {focus:?}"
        );
        assert!(
            focus.last().is_some_and(
                |(row, id, _)| id == "episode" && *row > *movie_rows.last().unwrap_or(&0)
            )
        );
    }
}
