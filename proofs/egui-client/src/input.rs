//! egui keyboard and gamepad adapters for the UI-independent navigation model.
//!
//! Key presses are read against the active profile's keymap (`inputmaps/keyboard.json`, M2-17)
//! once [`reload_keymap`] has installed it. When the keymap cannot be loaded, the fixed keys in
//! [`key_to_action`] stay in effect. Gamepad buttons still use the fixed names: controller
//! capture and per-device gamepad maps are not implemented.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use egui::{Context, Event, Id, Key, Modifiers};
use kattri_client_core::focus::NavAction;
use kattri_input::action::Action;
use kattri_input::input_map::InputMap;
use kattri_input::input_store::{MapName, MapSource, StoreError, load_map, save_map};
use kattri_input::{GamepadBackend, GamepadError, GamepadEvent};

/// File stem of the keyboard map inside the profile's `inputmaps` folder.
pub(crate) const KEYBOARD_MAP: &str = "keyboard";

/// egui temporary-memory key for the installed keymap.
const INSTALLED_KEYMAP_ID: &str = "kattri_installed_keymap";

/// Keymap actions that drive navigation, and the navigation action each one sends. Keymap
/// actions without an entry (playback, volume, screenshot) are not handled here.
const NAV_BINDINGS: [(Action, NavAction); 8] = [
    (Action::Up, NavAction::Up),
    (Action::Down, NavAction::Down),
    (Action::Left, NavAction::Left),
    (Action::Right, NavAction::Right),
    (Action::Enter, NavAction::Select),
    (Action::Back, NavAction::Back),
    (Action::PlayPause, NavAction::PlayPause),
    // The shell opens Settings on `Menu` (ui/mod.rs `handle_shell_action`), so the keymap's
    // `settings` action reuses it rather than adding a second route.
    (Action::Settings, NavAction::Menu),
];

/// The keymap key presses are read against. `None` keeps the fixed keys in effect.
#[derive(Clone, Default)]
struct InstalledKeymap(Option<Arc<InputMap>>);

/// Load the keyboard map from `maps_dir` and make it the one key presses are read against.
///
/// A missing file gives the defaults. A file that cannot be used keeps the previously installed
/// keymap if there is one; otherwise the fixed keys stay in effect. That case logs one warning
/// with the fault kind only, never the path.
pub fn reload_keymap(ctx: &Context, maps_dir: Option<&Path>) {
    let installed = match (maps_dir, MapName::new(KEYBOARD_MAP)) {
        (Some(dir), Ok(name)) => {
            let loaded = load_map(dir, &name);
            match loaded.source {
                MapSource::File | MapSource::Missing => Some(Arc::new(loaded.map)),
                MapSource::Fallback(fault) => {
                    let kept = installed_keymap(ctx);
                    tracing::warn!(
                        %fault,
                        kept_last_good = kept.is_some(),
                        "keymap could not be loaded"
                    );
                    kept
                }
            }
        }
        _ => {
            tracing::warn!("keymap could not be loaded; fixed keys are in use");
            None
        }
    };
    ctx.data_mut(|data| {
        data.insert_temp(Id::unique(INSTALLED_KEYMAP_ID), InstalledKeymap(installed));
    });
}

/// Write a keyboard map and read it back into the installed keymap, so the next key press uses
/// the saved bindings without a restart. The read-back proves what reached the disk.
pub(crate) fn save_keymap(
    ctx: &Context,
    maps_dir: &Path,
    map: &InputMap,
) -> Result<Option<PathBuf>, StoreError> {
    let name = MapName::new(KEYBOARD_MAP)?;
    let report = save_map(maps_dir, &name, map)?;
    reload_keymap(ctx, Some(maps_dir));
    Ok(report.quarantined)
}

/// The keymap installed by the last [`reload_keymap`], if any.
fn installed_keymap(ctx: &Context) -> Option<Arc<InputMap>> {
    ctx.data(|data| data.get_temp::<InstalledKeymap>(Id::unique(INSTALLED_KEYMAP_ID)))
        .and_then(|installed| installed.0)
}

/// Map a keyboard key to navigation with the fixed keys, ignoring Ctrl/Alt/system-shortcut
/// chords. Used only when no keymap is installed.
pub fn key_to_action(key: Key, modifiers: Modifiers) -> Option<NavAction> {
    if modifiers.ctrl || modifiers.alt || modifiers.command || modifiers.mac_cmd {
        return None;
    }
    Some(match key {
        Key::ArrowUp | Key::W => NavAction::Up,
        Key::ArrowDown | Key::S => NavAction::Down,
        Key::ArrowLeft | Key::A => NavAction::Left,
        Key::ArrowRight | Key::D => NavAction::Right,
        Key::Enter | Key::Space => NavAction::Select,
        Key::Escape | Key::Backspace => NavAction::Back,
        Key::P => NavAction::PlayPause,
        Key::M => NavAction::Menu,
        _ => return None,
    })
}

/// The binding text for a key press, in the form keymap files use (`key:Ctrl+Shift+P`). Used
/// by both runtime lookup and the Input page's capture, so the two always agree. `None` for keys
/// the format cannot name, and for Cmd chords, which it cannot express.
pub(crate) fn key_binding(key: Key, modifiers: Modifiers) -> Option<String> {
    if modifiers.mac_cmd {
        return None;
    }
    let name = key_binding_name(key)?;
    let mut out = String::from("key:");
    if modifiers.ctrl {
        out.push_str("Ctrl+");
    }
    if modifiers.shift {
        out.push_str("Shift+");
    }
    if modifiers.alt {
        out.push_str("Alt+");
    }
    out.push_str(name);
    Some(out)
}

/// The legacy spelling of a key, as `key:` bindings use it. `+` is absent because it
/// separates the parts of a chord.
fn key_binding_name(key: Key) -> Option<&'static str> {
    let named = match key {
        Key::Escape => "Esc",
        Key::ArrowUp => "Up",
        Key::ArrowDown => "Down",
        Key::ArrowLeft => "Left",
        Key::ArrowRight => "Right",
        Key::Enter => "Return",
        Key::Space => "Space",
        Key::Backspace => "Backspace",
        Key::Tab => "Tab",
        Key::Insert => "Insert",
        Key::Delete => "Delete",
        Key::Home => "Home",
        Key::End => "End",
        Key::PageUp => "PgUp",
        Key::PageDown => "PgDown",
        Key::Minus => "-",
        Key::Equals => "=",
        Key::OpenBracket => "[",
        Key::CloseBracket => "]",
        Key::Backslash => "\\",
        Key::Comma => ",",
        Key::Period => ".",
        Key::Semicolon => ";",
        Key::Slash => "/",
        Key::Quote => "'",
        Key::Backtick => "`",
        _ => return plain_key_name(key),
    };
    Some(named)
}

/// Letters, digits and function keys use their egui names unchanged.
fn plain_key_name(key: Key) -> Option<&'static str> {
    let name = key.name();
    let plain = match name.as_bytes() {
        [only] => only.is_ascii_alphanumeric(),
        [b'F', digits @ ..] => !digits.is_empty() && digits.iter().all(u8::is_ascii_digit),
        _ => false,
    };
    plain.then_some(name)
}

/// Navigation actions for a key press. With a keymap, the keymap decides and the fixed keys are
/// not consulted. Without one, the fixed keys apply.
fn nav_actions_for_key(
    keymap: Option<&InputMap>,
    key: Key,
    modifiers: Modifiers,
) -> Vec<NavAction> {
    let Some(map) = keymap else {
        return key_to_action(key, modifiers).into_iter().collect();
    };
    let Some(binding) = key_binding(key, modifiers) else {
        return Vec::new();
    };
    NAV_BINDINGS
        .iter()
        .filter(|(action, _)| map.bindings(*action).contains(&binding))
        .map(|(_, nav)| *nav)
        .collect()
}

/// Consume the next keyboard action from the requested set, if any.
pub fn take_action(
    ctx: &Context,
    accepted: &[NavAction],
    allow_key_repeat: bool,
) -> Option<NavAction> {
    let keymap = installed_keymap(ctx);
    let events = ctx.input(|input| {
        input
            .events
            .iter()
            .filter_map(|event| match event {
                Event::Key {
                    key,
                    pressed: true,
                    repeat,
                    modifiers,
                    ..
                } if allow_key_repeat || !repeat => Some((
                    *key,
                    *modifiers,
                    nav_actions_for_key(keymap.as_deref(), *key, *modifiers),
                )),
                _ => None,
            })
            .collect::<Vec<_>>()
    });

    events.into_iter().find_map(|(key, modifiers, actions)| {
        let action = actions
            .into_iter()
            .find(|action| accepted.contains(action))?;
        ctx.input_mut(|input| input.consume_key(modifiers, key))
            .then_some(action)
    })
}

/// Translate conventional gamepad names to the same navigation vocabulary.
/// Analog backends may report the left stick as directional transitions.
pub fn gamepad_button_to_action(button: &str) -> Option<NavAction> {
    match button.trim().to_ascii_uppercase().as_str() {
        "DPAD_UP" | "D_PAD_UP" | "DPADUP" | "LEFT_STICK_UP" | "LEFTSTICKUP" => Some(NavAction::Up),
        "DPAD_DOWN" | "D_PAD_DOWN" | "DPADDOWN" | "LEFT_STICK_DOWN" | "LEFTSTICKDOWN" => {
            Some(NavAction::Down)
        }
        "DPAD_LEFT" | "D_PAD_LEFT" | "DPADLEFT" | "LEFT_STICK_LEFT" | "LEFTSTICKLEFT" => {
            Some(NavAction::Left)
        }
        "DPAD_RIGHT" | "D_PAD_RIGHT" | "DPADRIGHT" | "LEFT_STICK_RIGHT" | "LEFTSTICKRIGHT" => {
            Some(NavAction::Right)
        }
        "A" | "BUTTON_A" | "SOUTH" => Some(NavAction::Select),
        "B" | "BUTTON_B" | "EAST" => Some(NavAction::Back),
        "START" | "BUTTON_START" => Some(NavAction::Menu),
        _ => None,
    }
}

pub fn gamepad_events_to_actions(events: &[GamepadEvent]) -> Vec<NavAction> {
    events
        .iter()
        .filter(|event| event.pressed)
        .filter_map(|event| gamepad_button_to_action(&event.button))
        .collect()
}

/// Poll one non-blocking backend frame and translate button transitions.
pub fn poll_gamepad(backend: &mut dyn GamepadBackend) -> Result<Vec<NavAction>, GamepadError> {
    backend
        .poll()
        .map(|events| gamepad_events_to_actions(&events))
}

#[cfg(test)]
mod tests {
    use super::*;
    use kattri_input::NullGamepad;

    const NONE: Modifiers = Modifiers::NONE;

    #[test]
    fn default_keyboard_map_covers_navigation_and_shell_actions() {
        for (key, expected) in [
            (Key::ArrowUp, NavAction::Up),
            (Key::W, NavAction::Up),
            (Key::ArrowDown, NavAction::Down),
            (Key::S, NavAction::Down),
            (Key::ArrowLeft, NavAction::Left),
            (Key::A, NavAction::Left),
            (Key::ArrowRight, NavAction::Right),
            (Key::D, NavAction::Right),
            (Key::Enter, NavAction::Select),
            (Key::Space, NavAction::Select),
            (Key::Escape, NavAction::Back),
            (Key::Backspace, NavAction::Back),
            (Key::P, NavAction::PlayPause),
            (Key::M, NavAction::Menu),
        ] {
            assert_eq!(key_to_action(key, Modifiers::NONE), Some(expected));
        }
    }

    #[test]
    fn default_keymap_sends_wasd_m_and_space_to_their_actions() {
        let map = InputMap::legacy_defaults();
        for (key, expected) in [
            (Key::W, NavAction::Up),
            (Key::A, NavAction::Left),
            (Key::S, NavAction::Down),
            (Key::D, NavAction::Right),
            (Key::M, NavAction::Menu),
        ] {
            assert_eq!(
                nav_actions_for_key(Some(&map), key, NONE),
                [expected],
                "{key:?}"
            );
        }
        assert_eq!(
            nav_actions_for_key(Some(&map), Key::Space, NONE),
            [NavAction::PlayPause],
            "Space must stay play/pause"
        );
    }

    #[test]
    fn control_alt_and_command_chords_are_not_navigation() {
        let ctrl = Modifiers {
            ctrl: true,
            command: true,
            ..Modifiers::NONE
        };
        let alt = Modifiers {
            alt: true,
            ..Modifiers::NONE
        };
        assert_eq!(key_to_action(Key::ArrowLeft, ctrl), None);
        assert_eq!(key_to_action(Key::ArrowUp, alt), None);
        assert_eq!(
            key_to_action(Key::M, Modifiers::NONE),
            Some(NavAction::Menu)
        );
    }

    #[test]
    fn default_gamepad_map_uses_dpad_left_stick_a_b_and_start() {
        let events = [
            GamepadEvent {
                device: "synthetic".into(),
                button: "DPAD_UP".into(),
                pressed: true,
            },
            GamepadEvent {
                device: "synthetic".into(),
                button: "D_PAD_DOWN".into(),
                pressed: true,
            },
            GamepadEvent {
                device: "synthetic".into(),
                button: "DPAD_LEFT".into(),
                pressed: true,
            },
            GamepadEvent {
                device: "synthetic".into(),
                button: "LEFT_STICK_RIGHT".into(),
                pressed: true,
            },
            GamepadEvent {
                device: "synthetic".into(),
                button: "A".into(),
                pressed: true,
            },
            GamepadEvent {
                device: "synthetic".into(),
                button: "B".into(),
                pressed: true,
            },
            GamepadEvent {
                device: "synthetic".into(),
                button: "START".into(),
                pressed: true,
            },
            GamepadEvent {
                device: "synthetic".into(),
                button: "A".into(),
                pressed: false,
            },
        ];
        assert_eq!(
            gamepad_events_to_actions(&events),
            [
                NavAction::Up,
                NavAction::Down,
                NavAction::Left,
                NavAction::Right,
                NavAction::Select,
                NavAction::Back,
                NavAction::Menu,
            ]
        );
    }

    #[test]
    fn gilrs_button_names_map_to_the_conventional_navigation_actions() {
        for (button, action) in [
            ("DPadUp", NavAction::Up),
            ("DPadDown", NavAction::Down),
            ("DPadLeft", NavAction::Left),
            ("DPadRight", NavAction::Right),
            ("LeftStickUp", NavAction::Up),
            ("LeftStickDown", NavAction::Down),
            ("LeftStickLeft", NavAction::Left),
            ("LeftStickRight", NavAction::Right),
            ("South", NavAction::Select),
            ("East", NavAction::Back),
            ("Start", NavAction::Menu),
        ] {
            assert_eq!(gamepad_button_to_action(button), Some(action), "{button}");
        }
    }

    #[test]
    fn null_gamepad_backend_returns_no_navigation_events() {
        let mut backend = NullGamepad;
        assert!(
            poll_gamepad(&mut backend)
                .expect("null backend poll")
                .is_empty()
        );
    }

    #[test]
    fn defaults_read_escape_and_ctrl_shift_p_under_their_legacy_names() {
        let map = InputMap::legacy_defaults();
        assert_eq!(
            nav_actions_for_key(Some(&map), Key::Escape, NONE),
            [NavAction::Back]
        );
        let ctrl_shift = Modifiers {
            ctrl: true,
            shift: true,
            ..Modifiers::NONE
        };
        assert_eq!(
            nav_actions_for_key(Some(&map), Key::P, ctrl_shift),
            [NavAction::PlayPause]
        );
        assert_eq!(
            nav_actions_for_key(Some(&map), Key::P, NONE),
            [NavAction::PlayPause]
        );
    }

    #[test]
    fn rebound_key_drives_the_action_and_the_old_key_stops_working() {
        let mut map = InputMap::legacy_defaults();
        map.set_bindings(Action::Enter, &["key:F9"])
            .expect("valid bindings");
        assert_eq!(
            nav_actions_for_key(Some(&map), Key::F9, NONE),
            [NavAction::Select]
        );
        assert!(nav_actions_for_key(Some(&map), Key::Enter, NONE).is_empty());
        // Back keeps its default bindings, so Escape still goes back.
        assert_eq!(
            nav_actions_for_key(Some(&map), Key::Escape, NONE),
            [NavAction::Back]
        );
    }

    #[test]
    fn fixed_keys_apply_only_when_no_keymap_is_installed() {
        assert_eq!(nav_actions_for_key(None, Key::W, NONE), [NavAction::Up]);
        assert_eq!(
            nav_actions_for_key(None, Key::Enter, NONE),
            [NavAction::Select]
        );
        assert!(nav_actions_for_key(None, Key::F9, NONE).is_empty());
    }

    #[test]
    fn a_saved_keymap_is_used_by_the_next_key_press_after_reload() {
        let dir = std::env::temp_dir().join(format!("kattri-egui-keymap-{}", uuid::Uuid::new_v4()));
        let ctx = Context::default();

        reload_keymap(&ctx, Some(&dir));
        assert!(
            nav_actions_for_key(installed_keymap(&ctx).as_deref(), Key::F9, NONE).is_empty(),
            "no file yet: defaults apply"
        );

        let mut map = InputMap::legacy_defaults();
        map.set_bindings(Action::Enter, &["key:F9"])
            .expect("valid bindings");
        let name = MapName::new(KEYBOARD_MAP).expect("valid name");
        save_map(&dir, &name, &map).expect("save");
        reload_keymap(&ctx, Some(&dir));
        assert_eq!(
            nav_actions_for_key(installed_keymap(&ctx).as_deref(), Key::F9, NONE),
            [NavAction::Select]
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
