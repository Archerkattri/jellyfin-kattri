//! Input page of the settings screen: key repeat, controller support, and the keyboard
//! binding list (M2-17).
//!
//! Bindings are edited through `kattri_input::rebind::Rebinder` and saved to the active
//! profile's `inputmaps/keyboard.json`. The working copy lives in egui temporary memory. It
//! is read from disk again each time the Input category opens (see [`reset`]).
//!
//! Capture takes the next key press or controller press. Escape cancels it, and so does losing
//! window focus or pressing East (B on Xbox-style pads). Controller presses are the raw
//! transitions that `app.rs` records on each poll, because navigation maps them to actions and
//! drops the button names. [`capture_frame`] runs before any page control is drawn, so the
//! press is not also used to activate a button or to leave the page.

use super::{ui_text, ui_text_args};
use kattri_i18n::ids;
use std::path::{Path, PathBuf};

use egui::{Context, Event, Key, Modifiers, RichText, Ui};
use kattri_client_core::focus::NavAction;
use kattri_input::GamepadEvent;
use kattri_input::action::{ALL_ACTIONS, Action, HostAction};
use kattri_input::input_map::{InputMap, MAX_USER_BINDINGS};
use kattri_input::input_store::{MapName, load_map};
use kattri_input::rebind::{CaptureOutcome, Rebinder};

use crate::app::KattriApp;
use crate::input::{KEYBOARD_MAP, key_binding, save_keymap};
use crate::theme::Palette;
use crate::ui::settings::{finish_page, muted, settings_card, subsection_heading};

const STATE_ID: &str = "kattri_input_bindings_state";

/// The keyboard bindings page state, kept between frames in egui temporary memory.
#[derive(Clone, Debug, Default)]
struct BindingsState {
    /// Folder the working copy was loaded from. A different folder reloads the copy.
    maps_dir: Option<PathBuf>,
    rebinder: Option<Rebinder>,
    /// Action whose bindings are expanded below its row.
    selected: Option<Action>,
    /// Message for the user: a refused change, a conflict, a load or save problem.
    notice: Option<String>,
}

/// A button press on the binding list, applied after the list is drawn.
enum Intent {
    Select(Action),
    Change { action: Action, binding: String },
    Add(Action),
    Clear { action: Action, binding: String },
}

pub(super) fn input_page(ui: &mut Ui, app: &mut KattriApp, colors: Palette) {
    let before = app.settings.settings().clone();
    settings_card(ui, |ui| {
        let input = &mut app.settings.settings_mut().input;
        ui.checkbox(
            &mut input.key_repeat,
            ui_text(ids::SETTINGS_INPUT_KEY_REPEAT),
        );
        ui.checkbox(
            &mut input.sdl_gamepad_enabled,
            ui_text(ids::SETTINGS_INPUT_GAMEPAD),
        );
        muted(ui, colors, ui_text(ids::SETTINGS_INPUT_GAMEPAD_HELP), 12.0);
    });
    ui.add_space(12.0);
    settings_card(ui, |ui| {
        subsection_heading(ui, ui_text(ids::SETTINGS_INPUT_KEYBOARD_HEADING), colors);
        match app.settings.keymap_dir() {
            Some(dir) => bindings_editor(ui, colors, &dir),
            None => muted(ui, colors, ui_text(ids::SETTINGS_INPUT_SAVED_HELP), 12.0),
        }
    });
    finish_page(ui, app, colors, &before);
}

/// Forget the working copy so the next visit reads the saved map again.
pub(super) fn reset(ctx: &Context) {
    ctx.data_mut(|data| data.remove_temp::<BindingsState>(egui::Id::unique(STATE_ID)));
}

/// Runs first in each settings frame. While a capture is open it takes the key presses and the
/// controller presses, and returns true so the caller skips page navigation for this frame.
pub(super) fn capture_frame(ctx: &Context, gamepad_actions: &mut Vec<NavAction>) -> bool {
    let pad_presses = crate::app::take_pad_presses(ctx);
    let mut state = load_state(ctx);
    let Some(rebinder) = state.rebinder.as_mut() else {
        return false;
    };
    if rebinder.capturing().is_none() {
        return false;
    }
    // A press the capture takes must not also move focus or activate a control.
    gamepad_actions.clear();
    let press = take_key_presses(ctx).into_iter().next();
    let focused = ctx.input(|input| input.focused);
    let effect = if !focused {
        rebinder.cancel_capture();
        PressEffect::default()
    } else {
        let pad_effect = apply_pad_presses(rebinder, &pad_presses);
        let key_effect = match press {
            Some((key, modifiers)) if rebinder.capturing().is_some() => {
                apply_press(rebinder, key, modifiers)
            }
            _ => PressEffect::default(),
        };
        merge(pad_effect, key_effect)
    };
    state.notice = if effect.changed {
        persist_state(ctx, &state)
    } else {
        effect.notice
    };
    save_state(ctx, state);
    true
}

/// What one key press did to the capture. The caller saves when `changed` is set.
#[derive(Debug, Default, PartialEq, Eq)]
struct PressEffect {
    changed: bool,
    notice: Option<String>,
}

fn apply_press(rebinder: &mut Rebinder, key: Key, modifiers: Modifiers) -> PressEffect {
    if key == Key::Escape {
        rebinder.cancel_capture();
        return PressEffect::default();
    }
    let Some(binding) = key_binding(key, modifiers) else {
        return PressEffect {
            changed: false,
            notice: Some(ui_text(ids::SETTINGS_INPUT_UNBINDABLE_KEY).to_owned()),
        };
    };
    outcome_effect(rebinder.accept(&binding))
}

/// Offers the controller presses of one frame to the open capture. East cancels the capture and
/// is never stored. Any other press is stored as `pad:<button>` under the same rules as a key.
/// Presses after the capture has closed are ignored.
fn apply_pad_presses(rebinder: &mut Rebinder, presses: &[GamepadEvent]) -> PressEffect {
    let mut effect = PressEffect::default();
    for press in presses.iter().filter(|event| event.pressed) {
        if is_east(&press.button) {
            rebinder.cancel_capture();
            break;
        }
        let Some(outcome) = rebinder.accept_gamepad(press) else {
            continue;
        };
        effect = merge(effect, outcome_effect(outcome));
        if rebinder.capturing().is_none() {
            break;
        }
    }
    effect
}

fn outcome_effect(outcome: CaptureOutcome) -> PressEffect {
    match outcome {
        CaptureOutcome::Bound => PressEffect {
            changed: true,
            notice: None,
        },
        CaptureOutcome::Unchanged | CaptureOutcome::NotCapturing => PressEffect::default(),
        CaptureOutcome::Conflict {
            binding,
            also_bound_to,
        } => PressEffect {
            changed: false,
            notice: Some(conflict_message(&binding, &also_bound_to)),
        },
        CaptureOutcome::Refused(error) => PressEffect {
            changed: false,
            notice: Some(error.to_string()),
        },
    }
}

fn merge(first: PressEffect, second: PressEffect) -> PressEffect {
    PressEffect {
        changed: first.changed || second.changed,
        notice: second.notice.or(first.notice),
    }
}

/// East is the B button on Xbox-style pads. These are the names the navigation mapping reads as
/// Back, so the two never disagree about which button is East.
fn is_east(button: &str) -> bool {
    matches!(
        button.trim().to_ascii_uppercase().as_str(),
        "EAST" | "B" | "BUTTON_B"
    )
}

/// Keys pressed this frame. Every key event is consumed, so buttons drawn later in the same
/// frame do not also fire on Enter or Space.
fn take_key_presses(ctx: &Context) -> Vec<(Key, Modifiers)> {
    let presses = ctx.input(|input| {
        input
            .events
            .iter()
            .filter_map(|event| match event {
                Event::Key {
                    key,
                    pressed: true,
                    modifiers,
                    ..
                } => Some((*key, *modifiers)),
                _ => None,
            })
            .collect()
    });
    ctx.input_mut(|input| {
        input
            .events
            .retain(|event| !matches!(event, Event::Key { .. }))
    });
    presses
}

fn conflict_message(binding: &str, also_bound_to: &[Action]) -> String {
    let names: Vec<&str> = also_bound_to
        .iter()
        .map(|action| action_label(*action))
        .collect();
    ui_text_args(
        ids::SETTINGS_INPUT_ALSO_BOUND,
        &[
            ("key", display_binding(binding)),
            ("actions", names.join(", ").as_str()),
        ],
    )
}

/// Binding text for people: `key:Ctrl+P` shows as `Ctrl+P`. Controller bindings keep their
/// prefix so the two kinds stay distinguishable.
fn display_binding(binding: &str) -> &str {
    binding.strip_prefix("key:").unwrap_or(binding)
}

fn bindings_text(bindings: &[String]) -> String {
    if bindings.is_empty() {
        ui_text(ids::SETTINGS_INPUT_UNBOUND).to_owned()
    } else {
        bindings
            .iter()
            .map(|binding| display_binding(binding))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// Label for one action in the list. Exhaustive, so a new action does not compile until it
/// has a label.
fn action_label(action: Action) -> &'static str {
    match action {
        Action::Up => ui_text(ids::SETTINGS_ACTION_MOVE_UP),
        Action::Down => ui_text(ids::SETTINGS_ACTION_MOVE_DOWN),
        Action::Left => ui_text(ids::SETTINGS_ACTION_MOVE_LEFT),
        Action::Right => ui_text(ids::SETTINGS_ACTION_MOVE_RIGHT),
        Action::Enter => ui_text(ids::SETTINGS_ACTION_SELECT),
        Action::Back => ui_text(ids::SETTINGS_ACTION_BACK),
        Action::Exit => ui_text(ids::SETTINGS_ACTION_EXIT),
        Action::Home => ui_text(ids::SETTINGS_ACTION_HOME),
        Action::Search => ui_text(ids::SETTINGS_ACTION_SEARCH),
        Action::Settings => ui_text(ids::SETTINGS_ACTION_SETTINGS),
        Action::PlayPause => ui_text(ids::SETTINGS_ACTION_PLAY_PAUSE),
        Action::Pause => ui_text(ids::SETTINGS_ACTION_PAUSE),
        Action::Stop => ui_text(ids::SETTINGS_ACTION_STOP),
        Action::SeekBackward => ui_text(ids::SETTINGS_ACTION_SEEK_BACK),
        Action::SeekForward => ui_text(ids::SETTINGS_ACTION_SEEK_FORWARD),
        Action::StepBackward => ui_text(ids::SETTINGS_ACTION_STEP_BACK),
        Action::StepForward => ui_text(ids::SETTINGS_ACTION_STEP_FORWARD),
        Action::CycleAudio => ui_text(ids::SETTINGS_ACTION_NEXT_AUDIO),
        Action::CycleAudioBack => ui_text(ids::SETTINGS_ACTION_PREVIOUS_AUDIO),
        Action::CycleSubtitles => ui_text(ids::SETTINGS_ACTION_NEXT_SUBTITLE),
        Action::CycleSubtitlesBack => ui_text(ids::SETTINGS_ACTION_PREVIOUS_SUBTITLE),
        Action::ToggleSubtitles => ui_text(ids::SETTINGS_ACTION_SUBTITLES_TOGGLE),
        Action::IncreaseAudioDelay => ui_text(ids::SETTINGS_ACTION_AUDIO_DELAY_LATER),
        Action::DecreaseAudioDelay => ui_text(ids::SETTINGS_ACTION_AUDIO_DELAY_EARLIER),
        Action::IncreaseSubtitleDelay => ui_text(ids::SETTINGS_ACTION_SUBTITLE_DELAY_LATER),
        Action::DecreaseSubtitleDelay => ui_text(ids::SETTINGS_ACTION_SUBTITLE_DELAY_EARLIER),
        Action::ResetSubtitleDelay => ui_text(ids::SETTINGS_ACTION_RESET_SUBTITLE_DELAY),
        Action::IncreaseVolume => ui_text(ids::SETTINGS_ACTION_VOLUME_UP),
        Action::DecreaseVolume => ui_text(ids::SETTINGS_ACTION_VOLUME_DOWN),
        Action::Mute => ui_text(ids::SETTINGS_ACTION_MUTE),
        Action::SpeedUp => ui_text(ids::SETTINGS_ACTION_SPEED_UP),
        Action::SpeedDown => ui_text(ids::SETTINGS_ACTION_SPEED_DOWN),
        Action::ChapterNext => ui_text(ids::SETTINGS_ACTION_NEXT_CHAPTER),
        Action::ChapterPrevious => ui_text(ids::SETTINGS_ACTION_PREVIOUS_CHAPTER),
        Action::Host(HostAction::Screenshot) => ui_text(ids::SETTINGS_ACTION_SCREENSHOT),
        Action::Host(HostAction::Minimize) => ui_text(ids::SETTINGS_ACTION_MINIMIZE_WINDOW),
        Action::Host(HostAction::Fullscreen) => ui_text(ids::SETTINGS_ACTION_FULLSCREEN),
        Action::Host(HostAction::ToggleWebMode) => ui_text(ids::SETTINGS_ACTION_WEB_MODE),
        Action::Host(HostAction::Quit) => ui_text(ids::SETTINGS_ACTION_QUIT),
        Action::Host(HostAction::Close) => ui_text(ids::SETTINGS_ACTION_CLOSE_WINDOW),
        Action::Host(HostAction::ToggleDebug) => ui_text(ids::SETTINGS_ACTION_DEBUG_OVERLAY),
        Action::Host(HostAction::TogglePip) => ui_text(ids::SETTINGS_ACTION_PICTURE_IN_PICTURE),
    }
}

fn keyboard_map_name() -> Option<MapName> {
    MapName::new(KEYBOARD_MAP).ok()
}

/// Write the working copy and make it the runtime keymap. Returns the notice to show, or `None`
/// when the save went through cleanly.
fn persist(ctx: &Context, dir: &Path, map: &InputMap) -> Option<String> {
    if keyboard_map_name().is_none() {
        return Some(ui_text(ids::SETTINGS_INPUT_SAVE_FAILED).to_owned());
    }
    match save_keymap(ctx, dir, map) {
        Ok(quarantined) => {
            quarantined.map(|_| ui_text(ids::SETTINGS_INPUT_KEYMAP_REPLACED).to_owned())
        }
        Err(error) => {
            tracing::warn!(%error, "keymap could not be saved");
            Some(ui_text(ids::SETTINGS_INPUT_SAVE_FAILED).to_owned())
        }
    }
}

fn persist_state(ctx: &Context, state: &BindingsState) -> Option<String> {
    let (Some(dir), Some(rebinder)) = (state.maps_dir.as_deref(), state.rebinder.as_ref()) else {
        return Some(ui_text(ids::SETTINGS_INPUT_SAVE_FAILED).to_owned());
    };
    persist(ctx, dir, rebinder.map())
}

fn bindings_editor(ui: &mut Ui, colors: Palette, dir: &Path) {
    let ctx = ui.ctx().clone();
    let mut state = load_state(&ctx);
    if state.maps_dir.as_deref() != Some(dir) || state.rebinder.is_none() {
        let Some(name) = keyboard_map_name() else {
            muted(ui, colors, ui_text(ids::SETTINGS_INPUT_SAVE_FAILED), 12.0);
            return;
        };
        let loaded = load_map(dir, &name);
        state.maps_dir = Some(dir.to_path_buf());
        state.rebinder = Some(Rebinder::new(loaded.map));
        state.selected = None;
        state.notice = (!loaded.notices.is_empty()).then(|| loaded.notices.join(" "));
    }
    muted(ui, colors, ui_text(ids::SETTINGS_INPUT_EDIT_HELP), 12.0);
    ui.add_space(8.0);
    if let Some(notice) = state.notice.clone() {
        ui.label(RichText::new(notice).size(12.0).color(colors.text));
        ui.add_space(6.0);
    }

    let mut intent = None;
    if let Some(rebinder) = state.rebinder.as_ref() {
        if let Some(action) = rebinder.capturing() {
            ui.label(
                RichText::new(ui_text_args(
                    ids::SETTINGS_INPUT_PRESS_NEW_KEY,
                    &[("action", action_label(action))],
                ))
                .strong()
                .color(colors.text),
            );
            ui.add_space(6.0);
        }
        for (index, action) in ALL_ACTIONS.iter().copied().enumerate() {
            let bindings = rebinder.map().bindings(action);
            let selected = state.selected == Some(action);
            let row_text = format!("{}: {}", action_label(action), bindings_text(bindings));
            ui.push_id(("binding_row", index), |ui| {
                if ui
                    .add(egui::Button::selectable(selected, row_text))
                    .clicked()
                {
                    intent = Some(Intent::Select(action));
                }
                if !selected {
                    return;
                }
                ui.indent(("binding_detail", index), |ui| {
                    for binding in bindings {
                        ui.horizontal(|ui| {
                            ui.label(display_binding(binding));
                            if ui
                                .button(ui_text(ids::SETTINGS_INPUT_BUTTON_CHANGE))
                                .clicked()
                            {
                                intent = Some(Intent::Change {
                                    action,
                                    binding: binding.clone(),
                                });
                            }
                            if ui
                                .button(ui_text(ids::SETTINGS_INPUT_BUTTON_CLEAR))
                                .clicked()
                            {
                                intent = Some(Intent::Clear {
                                    action,
                                    binding: binding.clone(),
                                });
                            }
                        });
                    }
                    if bindings.len() < MAX_USER_BINDINGS
                        && ui.button(ui_text(ids::SETTINGS_INPUT_BUTTON_ADD)).clicked()
                    {
                        intent = Some(Intent::Add(action));
                    }
                });
            });
            ui.add_space(2.0);
        }
    }

    match intent {
        None => {}
        Some(Intent::Select(action)) => state.selected = Some(action),
        Some(Intent::Change { action, binding }) => {
            if let Some(rebinder) = state.rebinder.as_mut() {
                rebinder.start_capture(action, Some(&binding));
            }
            state.notice = None;
        }
        Some(Intent::Add(action)) => {
            if let Some(rebinder) = state.rebinder.as_mut() {
                rebinder.start_capture(action, None);
            }
            state.notice = None;
        }
        Some(Intent::Clear { action, binding }) => {
            let result = state
                .rebinder
                .as_mut()
                .map(|rebinder| rebinder.clear_binding(action, &binding));
            state.notice = match result {
                Some(Ok(())) => persist_state(&ctx, &state),
                Some(Err(error)) => Some(error.to_string()),
                None => None,
            };
        }
    }
    save_state(&ctx, state);
}

fn load_state(ctx: &Context) -> BindingsState {
    ctx.data(|data| data.get_temp::<BindingsState>(egui::Id::unique(STATE_ID)))
        .unwrap_or_default()
}

fn save_state(ctx: &Context, state: BindingsState) {
    ctx.data_mut(|data| data.insert_temp(egui::Id::unique(STATE_ID), state));
}

#[cfg(test)]
mod tests {
    use super::{
        Action, GamepadEvent, InputMap, PressEffect, Rebinder, action_label, apply_pad_presses,
        apply_press, bindings_text, display_binding, is_east, key_binding,
    };
    use egui::{Key, Modifiers};
    use kattri_input::action::{ALL_ACTIONS, HostAction};
    use kattri_input::input_map::normalize_binding;
    use std::collections::BTreeSet;

    fn rebinder() -> Rebinder {
        Rebinder::new(InputMap::legacy_defaults())
    }

    fn pad(button: &str) -> GamepadEvent {
        GamepadEvent {
            device: "test-pad".to_owned(),
            button: button.to_owned(),
            pressed: true,
        }
    }

    fn pad_release(button: &str) -> GamepadEvent {
        GamepadEvent {
            pressed: false,
            ..pad(button)
        }
    }

    #[test]
    fn east_cancels_a_capture_and_stores_nothing() {
        let mut r = rebinder();
        let before = r.map().clone();
        r.start_capture(Action::Home, None);
        let effect = apply_pad_presses(&mut r, &[pad("East")]);
        assert_eq!(effect, PressEffect::default());
        assert_eq!(r.capturing(), None);
        assert_eq!(r.map(), &before);
    }

    #[test]
    fn east_release_leaves_the_capture_open() {
        let mut r = rebinder();
        r.start_capture(Action::Home, None);
        apply_pad_presses(&mut r, &[pad_release("East")]);
        assert_eq!(r.capturing(), Some(Action::Home));
    }

    #[test]
    fn other_controller_presses_are_stored_as_pad_bindings() {
        let mut r = rebinder();
        r.start_capture(Action::Home, None);
        let effect = apply_pad_presses(&mut r, &[pad("South")]);
        assert!(effect.changed);
        assert_eq!(effect.notice, None);
        assert!(
            r.map()
                .bindings(Action::Home)
                .contains(&"pad:South".to_owned())
        );
        assert_eq!(r.capturing(), None);
    }

    #[test]
    fn presses_after_east_in_the_same_frame_are_not_stored() {
        let mut r = rebinder();
        r.start_capture(Action::Home, None);
        apply_pad_presses(&mut r, &[pad("East"), pad("South")]);
        assert!(
            !r.map()
                .bindings(Action::Home)
                .contains(&"pad:South".to_owned())
        );
        assert_eq!(r.capturing(), None);
    }

    #[test]
    fn a_controller_button_bound_elsewhere_keeps_the_capture_open() {
        let mut map = InputMap::legacy_defaults();
        map.set_bindings(Action::PlayPause, &["key:P", "pad:South"])
            .expect("valid bindings");
        let mut r = Rebinder::new(map);
        let before = r.map().clone();
        r.start_capture(Action::Home, None);
        let effect = apply_pad_presses(&mut r, &[pad("South")]);
        assert!(!effect.changed);
        assert!(
            effect
                .notice
                .as_deref()
                .is_some_and(|notice| notice.contains("also bound to Play or pause")),
            "{effect:?}"
        );
        assert_eq!(r.capturing(), Some(Action::Home));
        assert_eq!(r.map(), &before);
    }

    #[test]
    fn east_is_recognised_under_the_names_navigation_uses() {
        assert!(is_east("East"));
        assert!(is_east("B"));
        assert!(!is_east("South"));
        assert!(!is_east("North"));
    }

    #[test]
    fn key_presses_become_canonical_bindings() {
        let ctrl_shift = Modifiers {
            ctrl: true,
            shift: true,
            ..Modifiers::NONE
        };
        assert_eq!(
            key_binding(Key::P, ctrl_shift).as_deref(),
            Some("key:Ctrl+Shift+P")
        );
        assert_eq!(
            key_binding(Key::F9, Modifiers::NONE).as_deref(),
            Some("key:F9")
        );
        assert_eq!(
            key_binding(Key::Enter, Modifiers::NONE).as_deref(),
            Some("key:Return")
        );
        let built = key_binding(Key::OpenBracket, Modifiers::NONE).expect("named key");
        assert_eq!(normalize_binding(&built), built);
    }

    #[test]
    fn keys_the_format_cannot_name_are_refused() {
        // `+` separates chord parts, so it cannot be a key name.
        assert_eq!(key_binding(Key::Plus, Modifiers::NONE), None);
    }

    #[test]
    fn display_drops_only_the_key_prefix() {
        assert_eq!(display_binding("key:Ctrl+P"), "Ctrl+P");
        assert_eq!(display_binding("pad:South"), "pad:South");
        assert_eq!(bindings_text(&[]), "Unbound");
        assert_eq!(
            bindings_text(&["key:P".to_owned(), "key:Space".to_owned()]),
            "P, Space"
        );
    }

    #[test]
    fn escape_cancels_capture_without_a_change() {
        let mut r = rebinder();
        let before = r.map().clone();
        r.start_capture(Action::Home, None);
        let effect = apply_press(&mut r, Key::Escape, Modifiers::NONE);
        assert_eq!(effect, PressEffect::default());
        assert_eq!(r.capturing(), None);
        assert_eq!(r.map(), &before);
    }

    #[test]
    fn a_free_key_is_bound_and_reported_as_a_change() {
        let mut r = rebinder();
        r.start_capture(Action::Home, None);
        let effect = apply_press(&mut r, Key::F7, Modifiers::NONE);
        assert!(effect.changed);
        assert_eq!(effect.notice, None);
        assert!(
            r.map()
                .bindings(Action::Home)
                .contains(&"key:F7".to_owned())
        );
    }

    #[test]
    fn a_conflict_names_the_other_action_and_keeps_capture_open() {
        let mut r = rebinder();
        let before = r.map().clone();
        r.start_capture(Action::Stop, None);
        let effect = apply_press(&mut r, Key::P, Modifiers::NONE);
        assert!(!effect.changed);
        assert_eq!(
            effect.notice.as_deref(),
            Some(
                "P is also bound to Play or pause. Clear it there first, then press the key again."
            )
        );
        assert_eq!(r.capturing(), Some(Action::Stop));
        assert_eq!(r.map(), &before);
    }

    #[test]
    fn an_unbindable_key_keeps_capture_open_with_a_notice() {
        let mut r = rebinder();
        r.start_capture(Action::Home, None);
        let effect = apply_press(&mut r, Key::Plus, Modifiers::NONE);
        assert!(!effect.changed);
        assert!(effect.notice.is_some());
        assert_eq!(r.capturing(), Some(Action::Home));
    }

    #[test]
    fn clearing_the_last_back_binding_keeps_the_action_bound() {
        let mut map = InputMap::legacy_defaults();
        map.set_bindings(Action::Back, &["key:Esc"]).expect("valid");
        let mut r = Rebinder::new(map);
        let error = r.clear_binding(Action::Back, "key:Esc").unwrap_err();
        assert_eq!(error.to_string(), "Keep at least one binding for Back");
        assert_eq!(r.map().bindings(Action::Back), &["key:Esc".to_owned()]);
    }

    #[test]
    fn every_action_has_a_distinct_label() {
        let labels: BTreeSet<&str> = ALL_ACTIONS.iter().map(|a| action_label(*a)).collect();
        assert_eq!(labels.len(), ALL_ACTIONS.len());
        assert!(labels.iter().all(|label| !label.is_empty()));
        assert_eq!(
            action_label(Action::Host(HostAction::TogglePip)),
            "Picture-in-picture"
        );
    }
}
