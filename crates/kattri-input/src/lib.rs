//! Input model for Jellyfin Kattri: actions, bindings, keymap files, focus
//! navigation and the gamepad backend contract.
//!
//! Status:
//! - Implemented and tested: the action vocabulary (legacy names), legacy JSON keymap
//!   loading (comments, key-pattern expansion, short/long and array targets), chord
//!   parsing, the pure 2D focus rule, and the `NullGamepad` backend.
//! - Implemented and tested (M2-17, pure): the v1 per-profile map format
//!   (`input_map`), canonical binding strings, the legacy keyboard defaults, conflict
//!   detection, essential-action and binding-cap rules, and keep-last-good reload.
//! - Implemented and tested (gamepad, feature `gilrs-backend`, on by default): the gilrs
//!   backend (`gilrs_backend`) with the `pad:` name table. Tested with recorded inputs
//!   only; no real controller has been exercised.
//! - Implemented and tested (pure): gamepad navigation mapping from button events to
//!   navigation commands, and the auto-repeat schedule for a held direction
//!   (`navigation`). Tested with recorded events only.
//! - Implemented and tested (M2-17, files): per-profile map files under
//!   `<profile>/inputmaps/` (`input_store`), atomic saves, fallback to the defaults for a
//!   corrupt file, and moving an unparseable file aside before a save. Load and save are
//!   tested in a temp folder; nothing here reads the platform config directory.
//! - Implemented and tested (M2-17, pure): the rebind flow (`rebind`): capture, conflict
//!   refusal, Escape and focus-loss cancel, clearing and restoring bindings.
//! - Skeleton: SDL or web-gamepad backends, the folder watcher, the egui Input page, and
//!   wiring into the egui and web front ends. The `inputmaps` folder is passed in by the
//!   caller; the profile store in `kattri-settings` supplies it.
//!
//! Layering: this crate depends on nothing from the UI, platform or player crates.
//! Keymap files are read from the legacy format so the bundled defaults can be reused.

pub mod action;
pub mod focus;
pub mod gamepad;
#[cfg(feature = "gilrs-backend")]
pub mod gilrs_backend;
pub mod grid_focus;
pub mod input_map;
pub mod input_store;
mod json_keys;
pub mod keymap;
pub mod navigation;
pub mod rebind;

pub use action::{ALL_ACTIONS, Action, ActionParseError, HostAction};
pub use focus::{Direction, FocusItem, Rect, move_focus};
pub use gamepad::{GamepadBackend, GamepadError, GamepadEvent, NullGamepad};
#[cfg(feature = "gilrs-backend")]
pub use gilrs_backend::GilrsGamepad;
pub use grid_focus::FocusGrid;
pub use input_map::{
    BindingSpec, Conflict, InputMap, InputMapError, Loaded, MAX_USER_BINDINGS, normalize_binding,
};
pub use keymap::{
    Binding, DeviceKind, KeyChord, Keymap, KeymapError, PatternError, Response, Skipped, Trigger,
    expand_pattern, parse_keymap, strip_comments,
};
pub use navigation::{
    DirectionRepeat, NavigationCommand, apply_navigation, command_for_button, navigation_command,
    repeat_steps_due,
};

#[cfg(test)]
pub(crate) mod test_support {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    /// A unique, empty folder under the system temp directory. It is removed on drop.
    pub(crate) struct Scratch(pub PathBuf);

    impl Scratch {
        pub(crate) fn new(tag: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos());
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "kattri-input-{tag}-{}-{nanos}-{n}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).expect("create scratch folder");
            Self(path)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn m2_every_action_has_a_keyboard_binding() {
        // PiP toggle is the only action without a keyboard default (owner decision).
        // The full reachability checks live in tests/keyboard_reachability.rs.
        let unbound =
            crate::input_map::InputMap::legacy_defaults().actions_without_keyboard_binding();
        assert_eq!(
            unbound,
            vec![crate::action::Action::Host(
                crate::action::HostAction::TogglePip
            )]
        );
    }

    #[test]
    fn m2_rebinding_persists_across_restart() {
        use crate::input_map::InputMap;
        use crate::input_store::{MapName, MapSource, input_maps_dir, load_map, save_map};
        use crate::rebind::{CaptureOutcome, Rebinder};

        let profile = crate::test_support::Scratch::new("restart");
        let maps = input_maps_dir(&profile.0);
        let name = MapName::new("keyboard").expect("valid name");

        let mut session = Rebinder::new(InputMap::legacy_defaults());
        session.start_capture(crate::action::Action::Pause, Some("key:Ctrl+P"));
        assert!(matches!(session.accept("key:F9"), CaptureOutcome::Bound));
        save_map(&maps, &name, session.map()).expect("save");
        drop(session);

        // A new session starts from the file only, as after a restart.
        let loaded = load_map(&maps, &name);
        assert!(matches!(loaded.source, MapSource::File));
        let pause = loaded.map.bindings(crate::action::Action::Pause);
        assert!(pause.contains(&"key:F9".to_owned()));
        assert!(!pause.contains(&"key:Ctrl+P".to_owned()));
    }
}
