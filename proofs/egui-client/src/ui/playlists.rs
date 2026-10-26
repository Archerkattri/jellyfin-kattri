//! Bounded, presentation-only playlist controls.
//!
//! Call [`show`] with playlist cards, an optional ordered playlist, and the
//! media choices that may be added. The component performs no I/O: it returns
//! typed [`PlaylistIntent`] values for its caller to handle.

use super::{ui_text, ui_text_args};
use egui::{self, Color32, Role};
use kattri_i18n::ids;
use std::collections::BTreeSet;

/// Maximum number of playlist cards laid out by one call to [`show`].
pub const MAX_PLAYLIST_CARDS: usize = 200;
/// Maximum number of ordered entries laid out by one call to [`show`].
pub const MAX_PLAYLIST_ENTRIES: usize = kattri_client_core::playlist::MAX_ENTRIES;
/// Maximum number of candidate media items laid out by one call to [`show`].
pub const MAX_ADDABLE_MEDIA: usize = 500;
/// Jellyfin playlist names are accepted locally only when they contain 1–100
/// non-whitespace Unicode characters.
pub const MAX_PLAYLIST_NAME_CHARS: usize = 100;

const ROW_HEIGHT: f32 = 36.0;
const MAX_LABEL_CHARS: usize = 512;
const MAX_IDENTIFIER_CHARS: usize = 256;

/// A playlist shown in the card/list portion of the component.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlaylistCard {
    /// Stable server playlist identifier.
    pub playlist_id: String,
    /// Display name.
    pub name: String,
    /// Number of entries reported by the caller.
    pub entry_count: Option<usize>,
}

/// One ordered playlist entry. Repeated media is represented by distinct
/// `playlist_entry_id` values; entries are never deduplicated by `media_id`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlaylistEntry {
    /// Stable identifier of this playlist entry, distinct from the media ID.
    pub playlist_entry_id: String,
    /// ID of the media item referenced by this entry.
    pub media_id: String,
    /// Display title for the referenced item.
    pub title: String,
}

/// A media item that can be selected for an add intent.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AddableMedia {
    /// Stable media identifier.
    pub media_id: String,
    /// Display title.
    pub title: String,
}

/// An opened playlist and its entries in the order supplied by the caller.
#[derive(Clone, Copy, Debug)]
pub struct OpenPlaylist<'a> {
    /// Playlist identifier.
    pub playlist_id: &'a str,
    /// Playlist display name.
    pub name: &'a str,
    /// Ordered entries, including repeated media items.
    pub entries: &'a [PlaylistEntry],
}

/// Immutable data presented by the component. `error` is rendered verbatim so
/// callers can supply suitable messages for unavailable or forbidden playlists.
#[derive(Clone, Copy, Debug)]
pub struct PlaylistInput<'a> {
    /// Available playlist cards, in caller-selected order.
    pub playlists: &'a [PlaylistCard],
    /// The currently opened playlist, if any.
    pub opened_playlist: Option<OpenPlaylist<'a>>,
    /// Media candidates for the add-selection list.
    pub addable_media: &'a [AddableMedia],
    /// Caller-provided error text, displayed without rewriting it.
    pub error: Option<&'a str>,
}

/// Ephemeral component state only; it contains no server or persistence logic.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PlaylistUiState {
    /// Current create-playlist text field value.
    pub create_name: String,
    /// Selected media IDs from the current candidate list.
    pub selected_media_ids: BTreeSet<String>,
    name_error: Option<PlaylistNameError>,
}

/// Direction for a one-position playlist-entry move intent.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MoveDirection {
    /// Move the selected entry one position toward the beginning.
    Up,
    /// Move the selected entry one position toward the end.
    Down,
}

/// User actions requested by the playlist component. These are descriptions
/// for the caller to handle, not claims that a server operation succeeded.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PlaylistIntent {
    /// Open the selected playlist card.
    OpenPlaylist { playlist_id: String },
    /// Submit a locally validated, trimmed playlist name for creation.
    SubmitCreateName { name: String },
    /// Start playback of the opened playlist.
    PlayPlaylist { playlist_id: String },
    /// Add the selected media IDs to the opened playlist, in candidate order.
    AddSelectedMedia {
        playlist_id: String,
        media_ids: Vec<String>,
    },
    /// Remove one entry, addressed by its playlist-entry ID rather than media ID.
    RemoveEntry {
        playlist_id: String,
        playlist_entry_id: String,
    },
    /// Move one entry by one position, addressed by playlist-entry ID.
    MoveEntry {
        playlist_id: String,
        playlist_entry_id: String,
        direction: MoveDirection,
    },
}

/// Actions emitted during a single egui frame.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PlaylistResponse {
    /// Typed intents in the order the user activated them.
    pub intents: Vec<PlaylistIntent>,
}

/// Local validation failure for the create-name field.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlaylistNameError {
    /// Name is empty or contains only whitespace.
    Blank,
    /// Trimmed name is longer than [`MAX_PLAYLIST_NAME_CHARS`].
    TooLong,
}

impl PlaylistNameError {
    fn message(self) -> &'static str {
        match self {
            Self::Blank => ui_text(ids::PLAYLISTS_ERROR_BLANK),
            Self::TooLong => ui_text(ids::PLAYLISTS_ERROR_TOO_LONG),
        }
    }
}

/// Validate and normalize a name without contacting a server.
pub fn validate_playlist_name(name: &str) -> Result<String, PlaylistNameError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(PlaylistNameError::Blank);
    }
    if name.chars().count() > MAX_PLAYLIST_NAME_CHARS {
        return Err(PlaylistNameError::TooLong);
    }
    Ok(name.to_owned())
}

/// Draw a bounded playlist component and return presentation intents.
pub fn show(
    ui: &mut egui::Ui,
    input: PlaylistInput<'_>,
    state: &mut PlaylistUiState,
) -> PlaylistResponse {
    let mut response = PlaylistResponse::default();
    ui.heading(
        egui::RichText::new(ui_text(ids::PLAYLISTS_HEADING_TITLE))
            .size(28.0)
            .strong(),
    );
    ui.add_space(4.0);
    if let Some(error) = input.error {
        ui.colored_label(Color32::LIGHT_RED, error);
    }

    create_playlist_controls(ui, state, &mut response);
    ui.separator();
    playlist_cards(ui, input.playlists, &mut response);

    if let Some(opened) = input.opened_playlist {
        ui.separator();
        opened_playlist(ui, opened, input.addable_media, state, &mut response);
    } else {
        ui.label(ui_text(ids::PLAYLISTS_EMPTY_CHOOSE));
    }
    response
}

fn create_playlist_controls(
    ui: &mut egui::Ui,
    state: &mut PlaylistUiState,
    response: &mut PlaylistResponse,
) {
    ui.label(ui_text(ids::PLAYLISTS_HEADING_CREATE));
    ui.horizontal_wrapped(|ui| {
        let name_input = ui.add(
            egui::TextEdit::singleline(&mut state.create_name)
                .char_limit(MAX_PLAYLIST_NAME_CHARS)
                .hint_text(ui_text(ids::PLAYLISTS_HINT_NAME)),
        );
        name_input.widget_info(|| {
            egui::WidgetInfo::labeled(
                Role::TextInput,
                true,
                ui_text(ids::PLAYLISTS_LABEL_NAME_HELP).to_owned(),
            )
        });
        if name_input.changed() {
            state.name_error = None;
        }

        let submit = ui.button(ui_text(ids::PLAYLISTS_BUTTON_CREATE));
        submit.widget_info(|| {
            egui::WidgetInfo::labeled(
                Role::Button,
                true,
                ui_text(ids::PLAYLISTS_HEADING_CREATE).to_owned(),
            )
        });
        if submit.clicked() {
            match validate_playlist_name(&state.create_name) {
                Ok(name) => {
                    state.name_error = None;
                    response
                        .intents
                        .push(PlaylistIntent::SubmitCreateName { name });
                }
                Err(error) => state.name_error = Some(error),
            }
        }
    });
    if let Some(error) = state.name_error {
        ui.colored_label(Color32::LIGHT_RED, error.message());
    }
}

fn playlist_cards(ui: &mut egui::Ui, playlists: &[PlaylistCard], response: &mut PlaylistResponse) {
    ui.heading(ui_text(ids::PLAYLISTS_HEADING_YOURS));
    let shown = playlists.len().min(MAX_PLAYLIST_CARDS);
    if shown == 0 {
        ui.label(ui_text(ids::PLAYLISTS_EMPTY_NONE));
        return;
    }

    egui::ScrollArea::vertical()
        .id_salt("playlist_cards")
        .max_height(220.0)
        .show_rows(ui, ROW_HEIGHT, shown, |ui, rows| {
            for index in rows {
                let card = &playlists[index];
                let label = match card.entry_count {
                    Some(count) => ui_text_args(
                        ids::PLAYLISTS_CARD_COUNT,
                        &[
                            ("name", bounded_label(&card.name).as_str()),
                            ("count", &count.to_string()),
                        ],
                    ),
                    None => bounded_label(&card.name),
                };
                let enabled = valid_id(&card.playlist_id);
                let button = ui.add_enabled(enabled, egui::Button::new(label));
                button.widget_info(|| {
                    egui::WidgetInfo::labeled(
                        Role::Button,
                        enabled,
                        ui_text_args(
                            ids::PLAYLISTS_A11Y_OPEN,
                            &[("name", bounded_label(&card.name).as_str())],
                        ),
                    )
                });
                if button.clicked() {
                    response.intents.push(PlaylistIntent::OpenPlaylist {
                        playlist_id: card.playlist_id.clone(),
                    });
                }
            }
        });
    show_omitted(
        ui,
        playlists.len() - shown,
        ui_text(ids::PLAYLISTS_NOUN_PLAYLISTS),
    );
}

fn opened_playlist(
    ui: &mut egui::Ui,
    opened: OpenPlaylist<'_>,
    addable_media: &[AddableMedia],
    state: &mut PlaylistUiState,
    response: &mut PlaylistResponse,
) {
    ui.heading(
        egui::RichText::new(bounded_label(opened.name))
            .size(24.0)
            .strong(),
    );
    ui.add_space(4.0);
    let playlist_id_valid = valid_id(opened.playlist_id);
    ui.horizontal_wrapped(|ui| {
        let play = ui.add_enabled(
            playlist_id_valid,
            egui::Button::new(ui_text(ids::PLAYLISTS_BUTTON_PLAY)),
        );
        play.widget_info(|| {
            egui::WidgetInfo::labeled(
                Role::Button,
                playlist_id_valid,
                ui_text(ids::PLAYLISTS_BUTTON_PLAY),
            )
        });
        if play.clicked() {
            response.intents.push(PlaylistIntent::PlayPlaylist {
                playlist_id: opened.playlist_id.to_owned(),
            });
        }
    });

    ui.label(ui_text(ids::PLAYLISTS_HEADING_ADD_MEDIA));
    addable_media_list(ui, addable_media, state);
    let selected_ids = selected_media_in_candidate_order(addable_media, state);
    let can_add = playlist_id_valid && !selected_ids.is_empty();
    let add = ui.add_enabled(
        can_add,
        egui::Button::new(ui_text(ids::PLAYLISTS_BUTTON_ADD_SELECTED)),
    );
    add.widget_info(|| {
        egui::WidgetInfo::labeled(
            Role::Button,
            can_add,
            ui_text(ids::PLAYLISTS_BUTTON_ADD_SELECTED),
        )
    });
    if add.clicked() {
        response.intents.push(PlaylistIntent::AddSelectedMedia {
            playlist_id: opened.playlist_id.to_owned(),
            media_ids: selected_ids,
        });
    }

    ui.label(ui_text(ids::PLAYLISTS_HEADING_ENTRIES));
    playlist_entries(ui, opened, response);
}

fn addable_media_list(ui: &mut egui::Ui, media: &[AddableMedia], state: &mut PlaylistUiState) {
    let shown = media.len().min(MAX_ADDABLE_MEDIA);
    if shown == 0 {
        ui.label(ui_text(ids::PLAYLISTS_EMPTY_NO_MEDIA));
        return;
    }
    egui::ScrollArea::vertical()
        .id_salt("playlist_addable_media")
        .max_height(180.0)
        .show_rows(ui, ROW_HEIGHT, shown, |ui, rows| {
            for index in rows {
                let item = &media[index];
                let enabled = valid_id(&item.media_id);
                let mut selected = state.selected_media_ids.contains(&item.media_id);
                let checkbox = ui.add_enabled(
                    enabled,
                    egui::Checkbox::new(&mut selected, bounded_label(&item.title)),
                );
                checkbox.widget_info(|| {
                    egui::WidgetInfo::labeled(
                        Role::CheckBox,
                        enabled,
                        ui_text_args(
                            ids::PLAYLISTS_A11Y_SELECT_TO_ADD,
                            &[("title", bounded_label(&item.title).as_str())],
                        ),
                    )
                });
                if checkbox.changed() {
                    if selected {
                        state.selected_media_ids.insert(item.media_id.clone());
                    } else {
                        state.selected_media_ids.remove(&item.media_id);
                    }
                }
            }
        });
    show_omitted(
        ui,
        media.len() - shown,
        ui_text(ids::PLAYLISTS_NOUN_MEDIA_ITEMS),
    );
}

fn playlist_entries(ui: &mut egui::Ui, opened: OpenPlaylist<'_>, response: &mut PlaylistResponse) {
    let entries = opened.entries;
    let shown = entries.len().min(MAX_PLAYLIST_ENTRIES);
    if shown == 0 {
        ui.label(ui_text(ids::PLAYLISTS_EMPTY_ENTRIES));
        return;
    }
    egui::ScrollArea::vertical()
        .id_salt("playlist_ordered_entries")
        .max_height(320.0)
        .show_rows(ui, ROW_HEIGHT, shown, |ui, rows| {
            for index in rows {
                let entry = &entries[index];
                let entry_id_valid = valid_id(&entry.playlist_entry_id);
                ui.push_id(("playlist_entry", index, &entry.playlist_entry_id), |ui| {
                    ui.horizontal(|ui| {
                        ui.label(format!("{}. {}", index + 1, bounded_label(&entry.title)));

                        let move_up = ui.add_enabled(
                            entry_id_valid && index > 0,
                            egui::Button::new(ui_text(ids::PLAYLISTS_BUTTON_MOVE_UP)),
                        );
                        move_up.widget_info(|| {
                            egui::WidgetInfo::labeled(
                                Role::Button,
                                entry_id_valid && index > 0,
                                ui_text_args(
                                    ids::PLAYLISTS_A11Y_MOVE_UP,
                                    &[("title", bounded_label(&entry.title).as_str())],
                                ),
                            )
                        });
                        if move_up.clicked() {
                            response.intents.push(PlaylistIntent::MoveEntry {
                                playlist_id: opened.playlist_id.to_owned(),
                                playlist_entry_id: entry.playlist_entry_id.clone(),
                                direction: MoveDirection::Up,
                            });
                        }

                        let move_down = ui.add_enabled(
                            entry_id_valid && index + 1 < shown,
                            egui::Button::new(ui_text(ids::PLAYLISTS_BUTTON_MOVE_DOWN)),
                        );
                        move_down.widget_info(|| {
                            egui::WidgetInfo::labeled(
                                Role::Button,
                                entry_id_valid && index + 1 < shown,
                                ui_text_args(
                                    ids::PLAYLISTS_A11Y_MOVE_DOWN,
                                    &[("title", bounded_label(&entry.title).as_str())],
                                ),
                            )
                        });
                        if move_down.clicked() {
                            response.intents.push(PlaylistIntent::MoveEntry {
                                playlist_id: opened.playlist_id.to_owned(),
                                playlist_entry_id: entry.playlist_entry_id.clone(),
                                direction: MoveDirection::Down,
                            });
                        }

                        let remove = ui.add_enabled(
                            entry_id_valid,
                            egui::Button::new(ui_text(ids::PLAYLISTS_BUTTON_REMOVE)),
                        );
                        remove.widget_info(|| {
                            egui::WidgetInfo::labeled(
                                Role::Button,
                                entry_id_valid,
                                ui_text_args(
                                    ids::PLAYLISTS_A11Y_REMOVE,
                                    &[("title", bounded_label(&entry.title).as_str())],
                                ),
                            )
                        });
                        if remove.clicked() {
                            response.intents.push(PlaylistIntent::RemoveEntry {
                                playlist_id: opened.playlist_id.to_owned(),
                                playlist_entry_id: entry.playlist_entry_id.clone(),
                            });
                        }
                    });
                });
            }
        });
    show_omitted(
        ui,
        entries.len() - shown,
        ui_text(ids::PLAYLISTS_NOUN_ENTRIES),
    );
}

fn selected_media_in_candidate_order(
    media: &[AddableMedia],
    state: &PlaylistUiState,
) -> Vec<String> {
    let mut seen = BTreeSet::new();
    media
        .iter()
        .take(MAX_ADDABLE_MEDIA)
        .filter(|item| {
            valid_id(&item.media_id) && state.selected_media_ids.contains(&item.media_id)
        })
        .filter(|item| seen.insert(item.media_id.as_str()))
        .map(|item| item.media_id.clone())
        .collect()
}

fn valid_id(id: &str) -> bool {
    !id.trim().is_empty() && id.chars().count() <= MAX_IDENTIFIER_CHARS
}

fn bounded_label(label: &str) -> String {
    let mut chars = label.chars();
    let visible: String = chars.by_ref().take(MAX_LABEL_CHARS).collect();
    if chars.next().is_some() {
        format!("{visible}…")
    } else {
        visible
    }
}

fn show_omitted(ui: &mut egui::Ui, count: usize, noun: &str) {
    if count > 0 {
        ui.label(ui_text_args(
            ids::PLAYLISTS_LIMIT_OMITTED,
            &[("count", &count.to_string()), ("noun", noun)],
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AddableMedia, MAX_PLAYLIST_NAME_CHARS, MoveDirection, OpenPlaylist, PlaylistCard,
        PlaylistEntry, PlaylistInput, PlaylistIntent, PlaylistNameError, PlaylistUiState, show,
        validate_playlist_name,
    };
    use egui::{FullOutput, Pos2, RawInput, Rect, Shape, Vec2};

    fn playlist() -> PlaylistCard {
        PlaylistCard {
            playlist_id: "playlist-1".into(),
            name: "Road trip".into(),
            entry_count: Some(2),
        }
    }

    fn entry(entry_id: &str, media_id: &str, title: &str) -> PlaylistEntry {
        PlaylistEntry {
            playlist_entry_id: entry_id.into(),
            media_id: media_id.into(),
            title: title.into(),
        }
    }

    fn opened<'a>(entries: &'a [PlaylistEntry]) -> OpenPlaylist<'a> {
        OpenPlaylist {
            playlist_id: "playlist-1",
            name: "Road trip",
            entries,
        }
    }

    fn frame(
        context: &egui::Context,
        cards: &[PlaylistCard],
        opened: Option<OpenPlaylist<'_>>,
        media: &[AddableMedia],
        state: &mut PlaylistUiState,
        click: Option<Pos2>,
    ) -> (FullOutput, super::PlaylistResponse) {
        let events = click.map_or_else(Vec::new, |position| {
            vec![
                egui::Event::PointerMoved(position),
                egui::Event::PointerButton {
                    pos: position,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: Default::default(),
                },
                egui::Event::PointerButton {
                    pos: position,
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: Default::default(),
                },
            ]
        });
        let mut response = super::PlaylistResponse::default();
        let mut output = context.run_ui(
            RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1600.0, 1200.0))),
                events,
                ..Default::default()
            },
            |ui| {
                response = show(
                    ui,
                    PlaylistInput {
                        playlists: cards,
                        opened_playlist: opened,
                        addable_media: media,
                        error: None,
                    },
                    state,
                )
            },
        );
        output.textures_delta.clear();
        (output, response)
    }

    fn text_positions(output: &FullOutput, needle: &str) -> Vec<Pos2> {
        fn visit(shape: &Shape, needle: &str, positions: &mut Vec<Pos2>) {
            match shape {
                Shape::Text(text) if text.galley.job.text.contains(needle) => {
                    positions.push(text.pos + text.galley.size() * 0.5);
                }
                Shape::Vec(shapes) => {
                    for shape in shapes {
                        visit(shape, needle, positions);
                    }
                }
                _ => {}
            }
        }
        let mut positions = Vec::new();
        for clipped in &output.shapes {
            visit(&clipped.shape, needle, &mut positions);
        }
        positions
    }

    fn playlist_input<'a>(
        cards: &'a [PlaylistCard],
        opened: Option<OpenPlaylist<'a>>,
        media: &'a [AddableMedia],
    ) -> PlaylistInput<'a> {
        PlaylistInput {
            playlists: cards,
            opened_playlist: opened,
            addable_media: media,
            error: None,
        }
    }

    fn click_text(
        context: &egui::Context,
        input: PlaylistInput<'_>,
        state: &mut PlaylistUiState,
        output: &FullOutput,
        needle: &str,
        occurrence: usize,
    ) -> super::PlaylistResponse {
        let position = *text_positions(output, needle)
            .get(occurrence)
            .expect("expected a matching widget label");
        frame(
            context,
            input.playlists,
            input.opened_playlist,
            input.addable_media,
            state,
            Some(position),
        )
        .1
    }

    fn has_text(output: &FullOutput, needle: &str) -> bool {
        !text_positions(output, needle).is_empty()
    }

    #[test]
    fn duplicate_media_entries_render_as_distinct_rows_and_remove_by_entry_id() {
        let entries = [
            entry("entry-a", "same-media", "Twin track"),
            entry("entry-b", "same-media", "Twin track"),
        ];
        let context = egui::Context::default();
        let cards = [];
        let media = [];
        let mut state = PlaylistUiState::default();
        let (output, _) = frame(
            &context,
            &cards,
            Some(opened(&entries)),
            &media,
            &mut state,
            None,
        );
        assert_eq!(text_positions(&output, "Twin track").len(), 2);

        let removed = click_text(
            &context,
            playlist_input(&cards, Some(opened(&entries)), &media),
            &mut state,
            &output,
            "Remove",
            1,
        );
        assert_eq!(
            removed.intents,
            [PlaylistIntent::RemoveEntry {
                playlist_id: "playlist-1".into(),
                playlist_entry_id: "entry-b".into(),
            }]
        );
    }

    #[test]
    fn component_emits_typed_intents_for_playlist_actions() {
        let cards = [playlist()];
        let entries = [
            entry("entry-a", "media-a", "First"),
            entry("entry-b", "media-b", "Second"),
        ];
        let media = [
            AddableMedia {
                media_id: "media-z".into(),
                title: "Z track".into(),
            },
            AddableMedia {
                media_id: "media-a".into(),
                title: "A track".into(),
            },
        ];
        let context = egui::Context::default();

        let mut state = PlaylistUiState::default();
        let (output, _) = frame(&context, &cards, None, &[], &mut state, None);
        assert_eq!(
            click_text(
                &context,
                playlist_input(&cards, None, &[]),
                &mut state,
                &output,
                "Road trip",
                0
            )
            .intents,
            [PlaylistIntent::OpenPlaylist {
                playlist_id: "playlist-1".into()
            }]
        );

        let mut state = PlaylistUiState {
            create_name: "  Night drive  ".into(),
            ..Default::default()
        };
        let (output, _) = frame(&context, &[], None, &[], &mut state, None);
        assert_eq!(
            click_text(
                &context,
                playlist_input(&[], None, &[]),
                &mut state,
                &output,
                "Create",
                1,
            )
            .intents,
            [PlaylistIntent::SubmitCreateName {
                name: "Night drive".into()
            }]
        );

        let mut state = PlaylistUiState::default();
        let (output, _) = frame(
            &context,
            &[],
            Some(opened(&entries)),
            &media,
            &mut state,
            None,
        );
        assert_eq!(
            click_text(
                &context,
                playlist_input(&[], Some(opened(&entries)), &media),
                &mut state,
                &output,
                "Play playlist",
                0,
            )
            .intents,
            [PlaylistIntent::PlayPlaylist {
                playlist_id: "playlist-1".into()
            }]
        );

        let mut state = PlaylistUiState {
            selected_media_ids: ["media-a".into(), "media-z".into()].into(),
            ..Default::default()
        };
        let (output, _) = frame(
            &context,
            &[],
            Some(opened(&entries)),
            &media,
            &mut state,
            None,
        );
        assert_eq!(
            click_text(
                &context,
                playlist_input(&[], Some(opened(&entries)), &media),
                &mut state,
                &output,
                "Add selected media",
                0,
            )
            .intents,
            [PlaylistIntent::AddSelectedMedia {
                playlist_id: "playlist-1".into(),
                media_ids: vec!["media-z".into(), "media-a".into()],
            }]
        );

        let mut state = PlaylistUiState::default();
        let (output, _) = frame(&context, &[], Some(opened(&entries)), &[], &mut state, None);
        let moved_up = click_text(
            &context,
            playlist_input(&[], Some(opened(&entries)), &[]),
            &mut state,
            &output,
            "Move up",
            1,
        );
        assert_eq!(
            moved_up.intents,
            [PlaylistIntent::MoveEntry {
                playlist_id: "playlist-1".into(),
                playlist_entry_id: "entry-b".into(),
                direction: MoveDirection::Up,
            }]
        );

        let mut state = PlaylistUiState::default();
        let (output, _) = frame(&context, &[], Some(opened(&entries)), &[], &mut state, None);
        let moved_down = click_text(
            &context,
            playlist_input(&[], Some(opened(&entries)), &[]),
            &mut state,
            &output,
            "Move down",
            0,
        );
        assert_eq!(
            moved_down.intents,
            [PlaylistIntent::MoveEntry {
                playlist_id: "playlist-1".into(),
                playlist_entry_id: "entry-a".into(),
                direction: MoveDirection::Down,
            }]
        );
    }

    #[test]
    fn invalid_blank_or_overlength_names_emit_no_create_intent() {
        assert_eq!(
            validate_playlist_name(" \t "),
            Err(PlaylistNameError::Blank)
        );
        assert_eq!(
            validate_playlist_name(&"n".repeat(MAX_PLAYLIST_NAME_CHARS + 1)),
            Err(PlaylistNameError::TooLong)
        );

        for (name, expected_error) in [
            ("  ".to_owned(), "Enter a playlist name."),
            (
                "x".repeat(MAX_PLAYLIST_NAME_CHARS + 1),
                "Playlist names must be 100 characters or fewer.",
            ),
        ] {
            let context = egui::Context::default();
            let mut state = PlaylistUiState {
                create_name: name,
                ..Default::default()
            };
            let (output, _) = frame(&context, &[], None, &[], &mut state, None);
            assert_eq!(
                click_text(
                    &context,
                    playlist_input(&[], None, &[]),
                    &mut state,
                    &output,
                    "Create",
                    1,
                )
                .intents,
                []
            );
            let (output, _) = frame(&context, &[], None, &[], &mut state, None);
            assert!(has_text(&output, expected_error));
        }
    }

    #[test]
    fn caller_error_is_presented_verbatim_and_lists_are_bounded() {
        let context = egui::Context::default();
        let cards: Vec<_> = (0..super::MAX_PLAYLIST_CARDS + 1)
            .map(|index| PlaylistCard {
                playlist_id: format!("playlist-{index}"),
                name: format!("Playlist {index}"),
                entry_count: None,
            })
            .collect();
        let mut state = PlaylistUiState::default();
        let mut response = super::PlaylistResponse::default();
        let mut output = context.run_ui(
            RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1200.0, 1000.0))),
                ..Default::default()
            },
            |ui| {
                response = show(
                    ui,
                    PlaylistInput {
                        playlists: &cards,
                        opened_playlist: None,
                        addable_media: &[],
                        error: Some("This playlist no longer exists (404)."),
                    },
                    &mut state,
                )
            },
        );
        output.textures_delta.clear();
        assert!(has_text(&output, "This playlist no longer exists (404)."));
        assert_eq!(response.intents.len(), 0);
        assert!(has_text(&output, "1 additional playlists omitted"));
    }
}
