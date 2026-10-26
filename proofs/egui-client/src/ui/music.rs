//! Reusable, presentation-only music queue controls.
//!
//! The caller owns the queue and applies returned intents. This module does not
//! mutate queue state, perform network requests, or issue player commands.

use super::{ui_text, ui_text_args};
use kattri_i18n::ids;
use std::collections::HashMap;

use egui::{self, RichText, Role, WidgetInfo};
use jellyfin_api::session_reporting::{QueueItem, RepeatMode};
use kattri_client_core::{focus::NavAction, queue::Queue};

/// State borrowed by the music queue presentation.
///
/// `ordered_tracks` must be in the same list order as `queue.items()`. The
/// separate slice lets a caller reuse its already-resolved track list; IDs are
/// used to join it to the name map and the queue's current item.
#[derive(Clone, Copy)]
pub struct MusicQueueInput<'a> {
    pub queue: &'a Queue,
    pub ordered_tracks: &'a [QueueItem],
    pub track_names: &'a HashMap<String, String>,
    /// Direction/select transitions from the app's keyboard/gamepad adapter.
    /// egui buttons also remain in normal keyboard tab order.
    pub navigation: &'a [NavAction],
}

/// User intent emitted by this presentation. The caller owns all effects.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MusicAction {
    PlayNow { item_id: String },
    PlayNext { item_id: String },
    AddToQueue { item_id: String },
    Remove { queue_index: usize, item_id: String },
    SetShuffle(bool),
    SetRepeat(RepeatMode),
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MusicQueueResponse {
    pub actions: Vec<MusicAction>,
    /// Stable ID of the currently keyboard/gamepad-focused control, if any.
    pub focused_control: Option<String>,
}

struct FocusTarget {
    id: String,
    response: egui::Response,
    action: MusicAction,
}

/// Draw an accessible queue view and return actions selected in this frame.
pub fn show(ui: &mut egui::Ui, input: MusicQueueInput<'_>) -> MusicQueueResponse {
    let mut output = MusicQueueResponse::default();
    let mut focus_targets = Vec::new();

    ui.heading(
        RichText::new(ui_text(ids::MUSIC_QUEUE_HEADING))
            .size(28.0)
            .strong(),
    );
    ui.add_space(4.0);
    ui.horizontal_wrapped(|ui| {
        let shuffle_label = if input.queue.is_shuffled() {
            ui_text(ids::MUSIC_SHUFFLE_ON)
        } else {
            ui_text(ids::MUSIC_SHUFFLE_OFF)
        };
        let shuffle_action = MusicAction::SetShuffle(!input.queue.is_shuffled());
        let shuffle = ui
            .add(egui::Button::new(shuffle_label).selected(input.queue.is_shuffled()))
            .on_hover_text(ui_text(ids::MUSIC_SHUFFLE_HOVER));
        shuffle.widget_info(|| {
            WidgetInfo::selected(
                Role::Button,
                true,
                input.queue.is_shuffled(),
                ui_text_args(
                    ids::MUSIC_SHUFFLE_A11Y,
                    &[(
                        "state",
                        if input.queue.is_shuffled() {
                            ui_text(ids::MUSIC_STATE_ON)
                        } else {
                            ui_text(ids::MUSIC_STATE_OFF)
                        },
                    )],
                ),
            )
        });
        collect_target(&mut focus_targets, "shuffle", shuffle, shuffle_action);

        let (repeat_label, next_repeat, selected) = match input.queue.repeat() {
            RepeatMode::None => (ui_text(ids::MUSIC_REPEAT_OFF), RepeatMode::All, false),
            RepeatMode::All => (ui_text(ids::MUSIC_REPEAT_ALL), RepeatMode::One, true),
            RepeatMode::One => (ui_text(ids::MUSIC_REPEAT_ONE), RepeatMode::None, true),
        };
        let repeat = ui
            .add(egui::Button::new(repeat_label).selected(selected))
            .on_hover_text(ui_text(ids::MUSIC_REPEAT_HOVER));
        repeat.widget_info(|| {
            WidgetInfo::selected(
                Role::Button,
                true,
                selected,
                ui_text_args(ids::MUSIC_REPEAT_MODE, &[("label", repeat_label)]),
            )
        });
        collect_target(
            &mut focus_targets,
            "repeat",
            repeat,
            MusicAction::SetRepeat(next_repeat),
        );
    });

    if input.ordered_tracks.is_empty() {
        let response = ui.label(ui_text(ids::MUSIC_QUEUE_EMPTY));
        response.widget_info(|| {
            WidgetInfo::labeled(
                Role::Label,
                true,
                ui_text(ids::MUSIC_QUEUE_EMPTY).to_owned(),
            )
        });
        apply_focus_navigation(ui, input.navigation, &mut focus_targets, &mut output);
        return output;
    }

    let current_id = input
        .queue
        .current()
        .or_else(|| input.queue.removed_current())
        .map(|item| item.id.as_str());
    if let Some(current_id) = current_id {
        let current_name = display_name(input.track_names, current_id);
        let response = ui.label(
            RichText::new(ui_text_args(
                ids::MUSIC_QUEUE_NOW_PLAYING,
                &[("name", current_name)],
            ))
            .strong(),
        );
        response.widget_info(|| {
            WidgetInfo::labeled(
                Role::Label,
                true,
                ui_text_args(ids::MUSIC_QUEUE_NOW_PLAYING, &[("name", current_name)]),
            )
        });
    } else {
        ui.label(ui_text(ids::MUSIC_QUEUE_NOTHING_PLAYING));
    }

    ui.add_space(8.0);
    egui::ScrollArea::vertical()
        .id_salt("music_queue_tracks")
        .show(ui, |ui| {
            for (index, track) in input.ordered_tracks.iter().enumerate() {
                let current = current_id == Some(track.id.as_str());
                let title = display_name(input.track_names, &track.id).to_owned();
                let accessible_title = if current {
                    ui_text_args(ids::MUSIC_QUEUE_CURRENT_TRACK, &[("title", title.as_str())])
                } else {
                    title.clone()
                };
                egui::Frame::group(ui.style()).show(ui, |ui| {
                    ui.set_min_width(ui.available_width());
                    ui.horizontal_wrapped(|ui| {
                        let title_response = ui.label(if current {
                            RichText::new(&title).strong().size(17.0)
                        } else {
                            RichText::new(&title).size(16.0)
                        });
                        title_response.widget_info(|| {
                            WidgetInfo::labeled(Role::Label, true, accessible_title.clone())
                        });

                        let item_id = track.id.clone();
                        let play = labeled_button(
                            ui,
                            ui_text(ids::MUSIC_QUEUE_ACTION_PLAY_NOW),
                            &ui_text_args(
                                ids::MUSIC_QUEUE_ACTION_PLAY_NOW_TITLE,
                                &[("title", title.as_str())],
                            ),
                        );
                        collect_target(
                            &mut focus_targets,
                            &format!("play:{index}"),
                            play,
                            MusicAction::PlayNow {
                                item_id: item_id.clone(),
                            },
                        );

                        let play_next = labeled_button(
                            ui,
                            ui_text(ids::MUSIC_QUEUE_ACTION_PLAY_NEXT),
                            &ui_text_args(
                                ids::MUSIC_QUEUE_ACTION_PLAY_NEXT_TITLE,
                                &[("title", title.as_str())],
                            ),
                        );
                        collect_target(
                            &mut focus_targets,
                            &format!("next:{index}"),
                            play_next,
                            MusicAction::PlayNext {
                                item_id: item_id.clone(),
                            },
                        );

                        let add = labeled_button(
                            ui,
                            ui_text(ids::MUSIC_QUEUE_ACTION_ADD),
                            &ui_text_args(
                                ids::MUSIC_QUEUE_ACTION_ADD_TITLE,
                                &[("title", title.as_str())],
                            ),
                        );
                        collect_target(
                            &mut focus_targets,
                            &format!("add:{index}"),
                            add,
                            MusicAction::AddToQueue {
                                item_id: item_id.clone(),
                            },
                        );

                        let remove = labeled_button(
                            ui,
                            ui_text(ids::MUSIC_QUEUE_ACTION_REMOVE),
                            &ui_text_args(
                                ids::MUSIC_QUEUE_ACTION_REMOVE_TITLE,
                                &[("title", title.as_str())],
                            ),
                        );
                        collect_target(
                            &mut focus_targets,
                            &format!("remove:{index}"),
                            remove,
                            MusicAction::Remove {
                                queue_index: index,
                                item_id,
                            },
                        );
                    });
                });
            }
        });

    apply_focus_navigation(ui, input.navigation, &mut focus_targets, &mut output);
    output
}

fn display_name<'a>(names: &'a HashMap<String, String>, item_id: &str) -> &'a str {
    names
        .get(item_id)
        .map(String::as_str)
        .filter(|name| !name.trim().is_empty())
        .unwrap_or(ui_text(ids::MUSIC_TRACK_UNTITLED))
}

fn labeled_button(ui: &mut egui::Ui, text: &str, accessible_name: &str) -> egui::Response {
    let response = ui.button(text).on_hover_text(accessible_name);
    response.widget_info(|| WidgetInfo::labeled(Role::Button, true, accessible_name.to_owned()));
    response
}

fn collect_target(
    targets: &mut Vec<FocusTarget>,
    id: &str,
    response: egui::Response,
    action: MusicAction,
) {
    targets.push(FocusTarget {
        id: id.to_owned(),
        response,
        action,
    });
}

fn apply_focus_navigation(
    ui: &mut egui::Ui,
    gamepad_navigation: &[NavAction],
    targets: &mut [FocusTarget],
    output: &mut MusicQueueResponse,
) {
    let keyboard_navigation = crate::input::take_action(
        ui.ctx(),
        &[
            NavAction::Up,
            NavAction::Down,
            NavAction::Left,
            NavAction::Right,
            NavAction::Select,
        ],
        false,
    );
    let navigation = gamepad_navigation
        .iter()
        .copied()
        .find(|action| {
            matches!(
                action,
                NavAction::Up
                    | NavAction::Down
                    | NavAction::Left
                    | NavAction::Right
                    | NavAction::Select
            )
        })
        .or(keyboard_navigation);

    let clicked = targets.iter().find(|target| target.response.clicked());
    if let Some(clicked) = clicked {
        output.actions.push(clicked.action.clone());
    }

    if targets.is_empty() {
        return;
    }

    let focused = targets
        .iter()
        .position(|target| target.response.has_focus());
    match navigation {
        Some(NavAction::Up | NavAction::Left) => {
            let next = focused.map_or(targets.len() - 1, |index| index.saturating_sub(1));
            targets[next].response.request_focus();
        }
        Some(NavAction::Down | NavAction::Right) => {
            let next = focused.map_or(0, |index| (index + 1).min(targets.len() - 1));
            targets[next].response.request_focus();
        }
        Some(NavAction::Select) if clicked.is_none() => {
            let selected = focused.unwrap_or(0);
            targets[selected].response.request_focus();
            output.actions.push(targets[selected].action.clone());
        }
        _ => {}
    }
    output.focused_control = targets
        .iter()
        .find(|target| target.response.has_focus())
        .map(|target| target.id.clone());
}

#[cfg(test)]
mod tests {
    use super::{MusicAction, MusicQueueInput, display_name, show};
    use egui::{FullOutput, Pos2, RawInput, Rect, Shape, Vec2};
    use jellyfin_api::session_reporting::{QueueItem, RepeatMode};
    use kattri_client_core::{focus::NavAction, queue::Queue};
    use std::collections::HashMap;

    fn tracks() -> Vec<QueueItem> {
        vec![QueueItem::new("track-a"), QueueItem::new("track-b")]
    }

    fn names() -> HashMap<String, String> {
        HashMap::from([
            ("track-a".to_owned(), "First song".to_owned()),
            ("track-b".to_owned(), "Second song".to_owned()),
        ])
    }

    fn frame(
        context: &egui::Context,
        queue: &Queue,
        tracks: &[QueueItem],
        names: &HashMap<String, String>,
        navigation: &[NavAction],
        click: Option<Pos2>,
    ) -> (FullOutput, super::MusicQueueResponse) {
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
        let mut response = super::MusicQueueResponse::default();
        let mut output = context.run_ui(
            RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1600.0, 1000.0))),
                events,
                ..Default::default()
            },
            |ui| {
                response = show(
                    ui,
                    MusicQueueInput {
                        queue,
                        ordered_tracks: tracks,
                        track_names: names,
                        navigation,
                    },
                )
            },
        );
        output.textures_delta.clear();
        (output, response)
    }

    fn text_center(output: &FullOutput, expected: &str) -> Pos2 {
        output
            .shapes
            .iter()
            .find_map(|clipped| match &clipped.shape {
                Shape::Text(text) if text.galley.job.text.contains(expected) => {
                    Some(text.pos + text.galley.size() * 0.5)
                }
                Shape::Vec(shapes) => shapes.iter().find_map(|shape| match shape {
                    Shape::Text(text) if text.galley.job.text.contains(expected) => {
                        Some(text.pos + text.galley.size() * 0.5)
                    }
                    _ => None,
                }),
                _ => None,
            })
            .expect("expected text in headless egui output")
    }

    fn has_text(output: &FullOutput, expected: &str) -> bool {
        fn contains(shape: &Shape, expected: &str) -> bool {
            match shape {
                Shape::Text(text) => text.galley.job.text.contains(expected),
                Shape::Vec(shapes) => shapes.iter().any(|shape| contains(shape, expected)),
                _ => false,
            }
        }
        output
            .shapes
            .iter()
            .any(|clipped| contains(&clipped.shape, expected))
    }

    #[test]
    fn empty_state_and_default_repeat_shuffle_are_presented() {
        let queue = Queue::new();
        let empty = [];
        let names = names();
        let context = egui::Context::default();
        let (output, response) = frame(&context, &queue, &empty, &names, &[], None);
        assert!(has_text(&output, "Queue is empty"));
        assert!(has_text(&output, "Shuffle off"));
        assert!(has_text(&output, "Repeat off"));
        assert!(response.actions.is_empty());
    }

    #[test]
    fn clicking_track_actions_emits_intents_without_mutating_queue() {
        let mut queue = Queue::new();
        queue.add(tracks()[0].clone()).expect("queue item");
        queue.add(tracks()[1].clone()).expect("queue item");
        queue.next_track();
        let before_current = queue.current().map(|item| item.id.clone());
        let before_shuffle = queue.is_shuffled();
        let before_repeat = queue.repeat();
        let track_list = tracks();
        let names = names();
        let context = egui::Context::default();
        let (mut initial, _) = frame(&context, &queue, &track_list, &names, &[], None);
        let position = text_center(&initial, "Play next");
        initial.textures_delta.clear();
        let (_, clicked) = frame(&context, &queue, &track_list, &names, &[], Some(position));
        assert!(clicked.actions.contains(&MusicAction::PlayNext {
            item_id: "track-a".to_owned()
        }));
        assert_eq!(queue.current().map(|item| item.id.clone()), before_current);
        assert_eq!(queue.is_shuffled(), before_shuffle);
        assert_eq!(queue.repeat(), before_repeat);
    }

    #[test]
    fn every_track_control_returns_the_matching_intent() {
        let mut queue = Queue::new();
        for track in tracks() {
            queue.add(track).expect("queue item");
        }
        let track_list = tracks();
        let names = names();
        for (label, expected) in [
            (
                "Play now",
                MusicAction::PlayNow {
                    item_id: "track-a".to_owned(),
                },
            ),
            (
                "Play next",
                MusicAction::PlayNext {
                    item_id: "track-a".to_owned(),
                },
            ),
            (
                "Add to queue",
                MusicAction::AddToQueue {
                    item_id: "track-a".to_owned(),
                },
            ),
            (
                "Remove",
                MusicAction::Remove {
                    queue_index: 0,
                    item_id: "track-a".to_owned(),
                },
            ),
        ] {
            let context = egui::Context::default();
            let (initial, _) = frame(&context, &queue, &track_list, &names, &[], None);
            let position = text_center(&initial, label);
            let (_, clicked) = frame(&context, &queue, &track_list, &names, &[], Some(position));
            assert_eq!(clicked.actions, vec![expected], "{label}");
        }
    }

    #[test]
    fn shuffle_and_repeat_controls_return_new_values_as_intents() {
        let queue = Queue::new();
        let track_list = [];
        let names = names();
        for (label, expected) in [
            ("Shuffle off", MusicAction::SetShuffle(true)),
            ("Repeat off", MusicAction::SetRepeat(RepeatMode::All)),
        ] {
            let context = egui::Context::default();
            let (initial, _) = frame(&context, &queue, &track_list, &names, &[], None);
            let position = text_center(&initial, label);
            let (_, clicked) = frame(&context, &queue, &track_list, &names, &[], Some(position));
            assert_eq!(clicked.actions, vec![expected], "{label}");
        }
    }

    #[test]
    fn presentation_shows_current_track_and_repeat_shuffle_state() {
        let mut queue = Queue::new();
        queue.add(tracks()[0].clone()).expect("queue item");
        queue.add(tracks()[1].clone()).expect("queue item");
        queue.next_track();
        queue.set_shuffle(true);
        queue.set_repeat(RepeatMode::All);
        let track_list = tracks();
        let names = names();
        let context = egui::Context::default();
        let (output, _) = frame(&context, &queue, &track_list, &names, &[], None);
        assert!(has_text(&output, "Now playing: First song"));
        assert!(has_text(&output, "Shuffle on"));
        assert!(has_text(&output, "Repeat all"));
    }

    #[test]
    fn gamepad_navigation_and_select_emit_the_focused_control_intent() {
        let queue = Queue::new();
        let track_list = tracks();
        let names = names();
        let context = egui::Context::default();
        let (_, focused) = frame(
            &context,
            &queue,
            &track_list,
            &names,
            &[NavAction::Down],
            None,
        );
        assert_eq!(focused.focused_control.as_deref(), Some("shuffle"));
        let (_, selected) = frame(
            &context,
            &queue,
            &track_list,
            &names,
            &[NavAction::Select],
            None,
        );
        assert_eq!(selected.actions, vec![MusicAction::SetShuffle(true)]);
    }

    #[test]
    fn missing_track_name_uses_a_safe_placeholder() {
        let names = HashMap::new();
        assert_eq!(display_name(&names, "unknown"), "Untitled track");
    }
}
