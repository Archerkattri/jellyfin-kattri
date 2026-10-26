//! Rebind flow for the Input settings page (M2-17), as a pure model. It draws nothing and
//! does no I/O.
//!
//! The page calls [`Rebinder::start_capture`] when a row is selected, then passes the next
//! key to [`Rebinder::accept`] as a binding string (`key:...`), or the next controller
//! press to [`Rebinder::accept_gamepad`] (stored as `pad:<button>`). Both paths share the
//! same conflict and cancel rules. The page handles Escape itself and calls
//! [`Rebinder::cancel_capture`], so Escape never becomes a binding during capture. It also
//! cancels on focus loss.
//!
//! Conflicts are refused, not resolved. The map is unchanged and capture stays open. To move
//! a binding, the page calls [`Rebinder::clear_binding`] on the other action and accepts the
//! key again. Keeping one binding on two actions is not offered here.
//!
//! The rules of [`InputMap`] apply: at most three bindings per action, and the last binding
//! of an essential action cannot be cleared.

use crate::action::Action;
use crate::gamepad::GamepadEvent;
use crate::input_map::{BindingSpec, InputMap, InputMapError, normalize_binding};

/// A capture in progress.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Capture {
    action: Action,
    /// Canonical binding to replace. `None` adds a new binding.
    replacing: Option<String>,
}

/// The result of one [`Rebinder::accept`] call.
#[derive(Debug)]
pub enum CaptureOutcome {
    /// The binding is set. Capture has ended.
    Bound,
    /// The action already has this binding. Nothing changed. Capture has ended.
    Unchanged,
    /// Another action uses this binding. Nothing changed. Capture stays open.
    Conflict {
        binding: String,
        also_bound_to: Vec<Action>,
    },
    /// The map rules refused the binding: not a valid key or button, the three-binding
    /// cap, or a duplicate on this action. Nothing changed. Capture stays open.
    Refused(InputMapError),
    /// No capture is in progress. Nothing changed.
    NotCapturing,
}

/// A working copy of one profile's map, edited through the rebind flow. Save it with
/// [`crate::input_store::save_map`] when the page is closed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rebinder {
    map: InputMap,
    capture: Option<Capture>,
}

impl Rebinder {
    pub fn new(map: InputMap) -> Self {
        Self { map, capture: None }
    }

    pub fn map(&self) -> &InputMap {
        &self.map
    }

    pub fn into_map(self) -> InputMap {
        self.map
    }

    /// The action being captured, if any.
    pub fn capturing(&self) -> Option<Action> {
        self.capture.as_ref().map(|c| c.action)
    }

    /// Start capture for `action`. With `replacing`, the accepted binding takes that
    /// binding's slot. Without it, the accepted binding is added.
    pub fn start_capture(&mut self, action: Action, replacing: Option<&str>) {
        self.capture = Some(Capture {
            action,
            replacing: replacing.map(normalize_binding),
        });
    }

    /// End capture without a change. Used for Escape and for focus loss.
    pub fn cancel_capture(&mut self) {
        self.capture = None;
    }

    /// Offer a controller press to the capture in progress, stored as `pad:<button>`.
    ///
    /// Only a press counts. A release returns `None` and changes nothing, so the press that
    /// started a capture cannot end it on release. Any other press goes through
    /// [`Rebinder::accept`], with the same conflict and refusal rules as a key.
    pub fn accept_gamepad(&mut self, event: &GamepadEvent) -> Option<CaptureOutcome> {
        if !event.pressed {
            return None;
        }
        Some(self.accept(&format!("pad:{}", event.button)))
    }

    /// Offer the next key to the capture in progress.
    pub fn accept(&mut self, binding: &str) -> CaptureOutcome {
        let Some(capture) = self.capture.clone() else {
            return CaptureOutcome::NotCapturing;
        };
        let spec = BindingSpec::parse(binding);
        if let BindingSpec::Unknown(raw) = &spec {
            return CaptureOutcome::Refused(InputMapError::InvalidBinding(raw.clone()));
        }
        let canonical = spec.canonical();

        let already_set = match &capture.replacing {
            Some(old) => *old == canonical,
            None => self.map.bindings(capture.action).contains(&canonical),
        };
        if already_set {
            self.capture = None;
            return CaptureOutcome::Unchanged;
        }

        let also_bound_to = self.map.also_bound_to(capture.action, &canonical);
        if !also_bound_to.is_empty() {
            return CaptureOutcome::Conflict {
                binding: canonical,
                also_bound_to,
            };
        }

        let result = match &capture.replacing {
            Some(old) => self.map.replace_binding(capture.action, old, &canonical),
            None => self.map.add_binding(capture.action, &canonical),
        };
        match result {
            Ok(()) => {
                self.capture = None;
                CaptureOutcome::Bound
            }
            Err(err) => CaptureOutcome::Refused(err),
        }
    }

    /// Remove one binding from an action. Removing a binding that is not set does nothing.
    /// Refused for the last binding of an essential action.
    pub fn clear_binding(&mut self, action: Action, binding: &str) -> Result<(), InputMapError> {
        self.map.remove_binding(action, binding)
    }

    /// Restore one action to its default bindings.
    pub fn restore_action(&mut self, action: Action) {
        self.map.reset_action(action);
    }

    /// Restore every action to its default bindings.
    pub fn restore_all(&mut self) {
        self.map.reset_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::HostAction;

    fn rebinder() -> Rebinder {
        Rebinder::new(InputMap::legacy_defaults())
    }

    #[test]
    fn accept_without_a_capture_changes_nothing() {
        let mut r = rebinder();
        let before = r.map().clone();
        assert!(matches!(r.accept("key:F7"), CaptureOutcome::NotCapturing));
        assert_eq!(r.map(), &before);
    }

    #[test]
    fn a_free_key_is_added_and_ends_the_capture() {
        let mut r = rebinder();
        r.start_capture(Action::Home, None);
        assert_eq!(r.capturing(), Some(Action::Home));
        assert!(matches!(r.accept("key:F7"), CaptureOutcome::Bound));
        assert!(
            r.map()
                .bindings(Action::Home)
                .contains(&"key:F7".to_owned())
        );
        assert_eq!(r.capturing(), None);
        assert!(matches!(r.accept("key:F8"), CaptureOutcome::NotCapturing));
    }

    #[test]
    fn a_key_used_by_another_action_is_refused_and_the_map_is_unchanged() {
        let mut r = rebinder();
        let before = r.map().clone();
        r.start_capture(Action::Stop, None);
        match r.accept("key:P") {
            CaptureOutcome::Conflict {
                binding,
                also_bound_to,
            } => {
                assert_eq!(binding, "key:P");
                assert_eq!(also_bound_to, vec![Action::PlayPause]);
            }
            other => panic!("expected a conflict, got {other:?}"),
        }
        assert_eq!(r.map(), &before);
        assert_eq!(r.capturing(), Some(Action::Stop), "capture stays open");
    }

    #[test]
    fn escape_cancels_the_capture_without_a_change() {
        let mut r = rebinder();
        let before = r.map().clone();
        r.start_capture(Action::Home, None);
        r.cancel_capture();
        assert_eq!(r.capturing(), None);
        assert_eq!(r.map(), &before);
    }

    #[test]
    fn focus_loss_cancels_the_capture() {
        let mut r = rebinder();
        r.start_capture(Action::Home, None);
        r.cancel_capture();
        assert!(matches!(r.accept("key:F7"), CaptureOutcome::NotCapturing));
        assert!(
            !r.map()
                .bindings(Action::Home)
                .contains(&"key:F7".to_owned())
        );
    }

    #[test]
    fn replacing_moves_the_chosen_slot_to_the_new_key() {
        let mut r = rebinder();
        r.start_capture(Action::Pause, Some("key:Ctrl+P"));
        assert!(matches!(r.accept("key:F9"), CaptureOutcome::Bound));
        assert_eq!(
            r.map().bindings(Action::Pause),
            &["key:F9".to_owned(), "key:Media Pause".to_owned()]
        );
    }

    #[test]
    fn replacing_with_the_same_key_is_unchanged() {
        let mut r = rebinder();
        let before = r.map().clone();
        r.start_capture(Action::Pause, Some("key:Ctrl+P"));
        assert!(matches!(r.accept("key:Ctrl+P"), CaptureOutcome::Unchanged));
        assert_eq!(r.map(), &before);
        assert_eq!(r.capturing(), None);
    }

    #[test]
    fn adding_a_key_the_action_already_has_is_unchanged() {
        let mut r = rebinder();
        r.start_capture(Action::Home, None);
        assert!(matches!(r.accept("key:H"), CaptureOutcome::Unchanged));
    }

    #[test]
    fn the_three_binding_cap_refuses_a_fourth_and_keeps_capture_open() {
        let mut r = rebinder();
        r.start_capture(Action::Home, None);
        assert!(matches!(r.accept("key:F1"), CaptureOutcome::Bound));
        r.start_capture(Action::Home, None);
        assert!(matches!(r.accept("key:F2"), CaptureOutcome::Bound));
        r.start_capture(Action::Home, None);
        assert!(matches!(
            r.accept("key:F3"),
            CaptureOutcome::Refused(InputMapError::TooManyBindings)
        ));
        assert_eq!(r.capturing(), Some(Action::Home));
        // Replacing an existing slot does not change the count, so it still works.
        assert!(matches!(
            r.accept("key:F3"),
            CaptureOutcome::Refused(InputMapError::TooManyBindings)
        ));
        r.start_capture(Action::Home, Some("key:F1"));
        assert!(matches!(r.accept("key:F3"), CaptureOutcome::Bound));
        assert_eq!(r.map().bindings(Action::Home).len(), 3);
    }

    #[test]
    fn an_invalid_binding_is_refused_and_capture_stays_open() {
        let mut r = rebinder();
        r.start_capture(Action::Home, None);
        assert!(matches!(
            r.accept("key:"),
            CaptureOutcome::Refused(InputMapError::InvalidBinding(_))
        ));
        assert_eq!(r.capturing(), Some(Action::Home));
    }

    #[test]
    fn a_controller_button_can_be_captured() {
        let mut r = rebinder();
        r.start_capture(Action::Pause, None);
        assert!(matches!(r.accept("pad:South"), CaptureOutcome::Bound));
        assert!(
            r.map()
                .bindings(Action::Pause)
                .contains(&"pad:South".to_owned())
        );
    }

    fn press(button: &str) -> GamepadEvent {
        GamepadEvent {
            device: "test-pad".to_owned(),
            button: button.to_owned(),
            pressed: true,
        }
    }

    fn release(button: &str) -> GamepadEvent {
        GamepadEvent {
            pressed: false,
            ..press(button)
        }
    }

    #[test]
    fn a_controller_press_is_captured_as_a_pad_binding() {
        // Home has one default binding, so the three-binding cap does not apply.
        let mut r = rebinder();
        r.start_capture(Action::Home, None);
        assert!(matches!(
            r.accept_gamepad(&press("South")),
            Some(CaptureOutcome::Bound)
        ));
        assert!(
            r.map()
                .bindings(Action::Home)
                .contains(&"pad:South".to_owned())
        );
        assert_eq!(r.capturing(), None);
    }

    #[test]
    fn a_controller_release_is_ignored_and_capture_stays_open() {
        let mut r = rebinder();
        let before = r.map().clone();
        r.start_capture(Action::Stop, None);
        assert!(r.accept_gamepad(&release("South")).is_none());
        assert_eq!(r.capturing(), Some(Action::Stop));
        assert_eq!(r.map(), &before);
    }

    #[test]
    fn a_controller_press_with_no_capture_changes_nothing() {
        let mut r = rebinder();
        let before = r.map().clone();
        assert!(matches!(
            r.accept_gamepad(&press("South")),
            Some(CaptureOutcome::NotCapturing)
        ));
        assert_eq!(r.map(), &before);
    }

    #[test]
    fn a_controller_button_used_by_another_action_is_a_conflict() {
        let mut map = InputMap::legacy_defaults();
        map.set_bindings(Action::PlayPause, &["key:P", "pad:South"])
            .expect("valid bindings");
        let mut r = Rebinder::new(map);
        let before = r.map().clone();
        r.start_capture(Action::Home, None);
        match r.accept_gamepad(&press("South")) {
            Some(CaptureOutcome::Conflict {
                binding,
                also_bound_to,
            }) => {
                assert_eq!(binding, "pad:South");
                assert_eq!(also_bound_to, vec![Action::PlayPause]);
            }
            other => panic!("expected a conflict, got {other:?}"),
        }
        assert_eq!(r.map(), &before);
        assert_eq!(r.capturing(), Some(Action::Home), "capture stays open");

        // The same move as for a key: clear the other action, then press again.
        r.clear_binding(Action::PlayPause, "pad:South")
            .expect("not essential");
        assert!(matches!(
            r.accept_gamepad(&press("South")),
            Some(CaptureOutcome::Bound)
        ));
        assert!(r.map().conflicts().iter().all(|c| c.binding != "pad:South"));
    }

    #[test]
    fn cancel_ends_a_controller_capture_without_a_change() {
        let mut r = rebinder();
        let before = r.map().clone();
        r.start_capture(Action::Home, None);
        r.cancel_capture();
        assert!(matches!(
            r.accept_gamepad(&press("South")),
            Some(CaptureOutcome::NotCapturing)
        ));
        assert_eq!(r.map(), &before);
    }

    #[test]
    fn an_invalid_controller_name_is_refused_and_capture_stays_open() {
        let mut r = rebinder();
        r.start_capture(Action::Home, None);
        assert!(matches!(
            r.accept_gamepad(&press("Dpad Up")),
            Some(CaptureOutcome::Refused(InputMapError::InvalidBinding(_)))
        ));
        assert_eq!(r.capturing(), Some(Action::Home));
    }

    #[test]
    fn moving_a_binding_is_clear_then_accept() {
        // Home has one binding, so the cap does not get in the way of the move.
        let mut r = rebinder();
        r.start_capture(Action::Home, None);
        assert!(matches!(r.accept("key:P"), CaptureOutcome::Conflict { .. }));
        r.clear_binding(Action::PlayPause, "key:P")
            .expect("not essential");
        assert!(matches!(r.accept("key:P"), CaptureOutcome::Bound));
        assert!(r.map().bindings(Action::Home).contains(&"key:P".to_owned()));
        assert!(r.map().conflicts().iter().all(|c| c.binding != "key:P"));
    }

    #[test]
    fn clearing_the_last_binding_of_an_essential_action_is_refused() {
        let mut map = InputMap::legacy_defaults();
        map.set_bindings(Action::Back, &["key:Esc"]).expect("valid");
        let mut r = Rebinder::new(map);
        let err = r.clear_binding(Action::Back, "key:Esc").unwrap_err();
        assert_eq!(err.to_string(), "Keep at least one binding for Back");
        assert_eq!(r.map().bindings(Action::Back), &["key:Esc".to_owned()]);
    }

    #[test]
    fn restore_action_and_restore_all_return_to_defaults() {
        let defaults = InputMap::legacy_defaults();
        let mut r = rebinder();
        r.clear_binding(Action::Pause, "key:Ctrl+P").expect("ok");
        r.restore_action(Action::Pause);
        assert_eq!(r.map(), &defaults);

        r.clear_binding(Action::Home, "key:H").expect("ok");
        r.restore_all();
        assert_eq!(r.map(), &defaults);
    }

    #[test]
    fn pip_can_be_captured_on_a_free_key() {
        let mut r = rebinder();
        let pip = Action::Host(HostAction::TogglePip);
        r.start_capture(pip, None);
        assert!(matches!(r.accept("key:F12"), CaptureOutcome::Bound));
        assert!(
            r.map()
                .actions_without_keyboard_binding()
                .iter()
                .all(|a| *a != pip)
        );
    }
}
