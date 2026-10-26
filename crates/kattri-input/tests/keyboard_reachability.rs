//! M2-18: every action has a keyboard path in the default map, and the model can rebind
//! every action from keyboard input. Pure checks only; the egui Input page and the
//! screen-by-screen walk are checked by hand (see the PR notes).

use kattri_input::input_map::{ESSENTIAL_ACTIONS, InputMap};
use kattri_input::{ALL_ACTIONS, Action, BindingSpec, HostAction};

/// Actions with no keyboard default, and why. Anything not listed here must have a `key:`
/// default. PiP toggle is unbound because `Ctrl+Shift+P` (the spec's proposed default) is
/// the legacy play/pause alias, which is an owner decision (M2-17 and M2-18 open points).
const KEYBOARD_UNBOUND_BY_DESIGN: &[Action] = &[Action::Host(HostAction::TogglePip)];

/// Chords the desktop shell or the OS reserves. A default must never be one of these.
const OS_RESERVED_CHORDS: &[&str] = &[
    "key:Alt+Tab",
    "key:Alt+F4",
    "key:Alt+Esc",
    "key:Ctrl+Esc",
    "key:Ctrl+Alt+Delete",
    "key:Ctrl+Shift+Esc", // gitleaks:allow -- an OS-reserved keyboard chord, not a credential.
    "key:Meta+Tab",
    "key:Meta+L",
    "key:Meta+D",
    "key:Meta+R",
];

#[test]
fn every_action_except_the_documented_ones_has_a_keyboard_default() {
    let unbound = InputMap::legacy_defaults().actions_without_keyboard_binding();
    assert_eq!(
        unbound, KEYBOARD_UNBOUND_BY_DESIGN,
        "actions without a keyboard default in the default map"
    );
}

#[test]
fn documented_exceptions_are_real_actions() {
    for action in KEYBOARD_UNBOUND_BY_DESIGN {
        assert!(
            ALL_ACTIONS.contains(action),
            "{action} is not in ALL_ACTIONS"
        );
    }
}

#[test]
fn essential_actions_have_a_keyboard_default() {
    let defaults = InputMap::legacy_defaults();
    for action in ESSENTIAL_ACTIONS {
        assert!(
            defaults
                .bindings(*action)
                .iter()
                .any(|b| matches!(BindingSpec::parse(b), BindingSpec::Key(_))),
            "{action} has no keyboard default"
        );
    }
}

#[test]
fn every_default_is_a_single_key_chord_not_a_modifier_or_button() {
    let defaults = InputMap::legacy_defaults();
    for action in ALL_ACTIONS {
        for binding in defaults.bindings(*action) {
            assert!(
                matches!(BindingSpec::parse(binding), BindingSpec::Key(_)),
                "{action}: default `{binding}` is not a key chord"
            );
        }
    }
}

#[test]
fn no_default_takes_an_os_reserved_chord() {
    let defaults = InputMap::legacy_defaults();
    for action in ALL_ACTIONS {
        for binding in defaults.bindings(*action) {
            assert!(
                !OS_RESERVED_CHORDS.contains(&binding.as_str()),
                "{action}: default `{binding}` is reserved by the OS or shell"
            );
        }
    }
}

#[test]
fn default_map_has_no_two_actions_on_one_key() {
    assert!(InputMap::legacy_defaults().conflicts().is_empty());
}

#[test]
fn every_action_can_be_rebound_to_a_keyboard_key() {
    // Starting from the defaults, each action can take a free key by itself. Keyboard
    // rebinding is the same model call the Input page makes after a key press.
    for action in ALL_ACTIONS {
        let mut map = InputMap::legacy_defaults();
        map.set_bindings(*action, &["key:F10"])
            .unwrap_or_else(|err| panic!("{action} cannot take F10: {err}"));
        assert_eq!(map.bindings(*action), &["key:F10".to_owned()], "{action}");
        assert!(
            map.actions_without_keyboard_binding()
                .iter()
                .all(|a| KEYBOARD_UNBOUND_BY_DESIGN.contains(a)),
            "{action}: rebinding left another action without a keyboard key"
        );
    }
}

#[test]
fn keyboard_rebinding_of_an_essential_action_cannot_remove_its_last_key() {
    let mut map = InputMap::legacy_defaults();
    for action in ESSENTIAL_ACTIONS {
        assert!(map.set_bindings(*action, &[]).is_err(), "{action}");
    }
}

/// Every action in [`ALL_ACTIONS`] whose default bindings include `binding`.
fn default_actions_for_key(binding: &str) -> Vec<Action> {
    let defaults = InputMap::legacy_defaults();
    ALL_ACTIONS
        .iter()
        .copied()
        .filter(|action| defaults.bindings(*action).iter().any(|b| b == binding))
        .collect()
}

#[test]
fn wasd_keys_each_map_to_their_navigation_action_only() {
    for (key, action) in [
        ("key:W", Action::Up),
        ("key:A", Action::Left),
        ("key:S", Action::Down),
        ("key:D", Action::Right),
    ] {
        assert_eq!(default_actions_for_key(key), vec![action], "{key}");
    }
}

#[test]
fn m_maps_to_settings_only() {
    assert_eq!(
        default_actions_for_key("key:M"),
        vec![Action::Settings],
        "M must open Settings and nothing else"
    );
}

#[test]
fn keys_moved_off_wasd_and_m_keep_their_actions() {
    for (key, action) in [
        ("key:E", Action::CycleAudio),
        ("key:C", Action::ToggleSubtitles),
        ("key:N", Action::Mute),
    ] {
        assert_eq!(default_actions_for_key(key), vec![action], "{key}");
    }
}

#[test]
fn space_is_play_pause_only() {
    assert_eq!(
        default_actions_for_key("key:Space"),
        vec![Action::PlayPause],
        "Space must not also select"
    );
}
