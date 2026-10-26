//! gilrs gamepad backend (owner decision, 2026-10-15). Compiled only with the
//! `gilrs-backend` feature; this is the only module that names gilrs types.
//!
//! `poll` drains gilrs events without blocking and turns them into
//! [`GamepadEvent`]s named with the `pad:` table below. Stick and D-pad axes become
//! press/release transitions with hysteresis. A name is reported only when its state
//! changes, and disconnecting a device releases everything that device still holds,
//! so an unplugged pad cannot leave an action stuck on.
//!
//! Tests use recorded inputs and the translator only. No test opens a controller.

use std::collections::{BTreeSet, HashMap};

use gilrs::{Axis, Button, EventType, Gilrs};

use crate::gamepad::{GamepadBackend, GamepadError, GamepadEvent};

/// Axis deflection at or beyond which a direction counts as pressed.
const AXIS_PRESS: f32 = 0.5;
/// Axis deflection at or below which a pressed direction is released again.
/// Between the two thresholds the previous state holds (hysteresis).
const AXIS_RELEASE: f32 = 0.3;

/// The `pad:` name for a gilrs button, or `None` for [`Button::Unknown`].
///
/// Names follow the gilrs variant names, which already match the spec examples
/// (`pad:South`, `pad:DPadUp`) and the `pad:Start` used in the map tests.
pub fn button_pad_name(button: Button) -> Option<&'static str> {
    Some(match button {
        Button::South => "South",
        Button::East => "East",
        Button::North => "North",
        Button::West => "West",
        Button::C => "C",
        Button::Z => "Z",
        Button::LeftTrigger => "LeftTrigger",
        Button::LeftTrigger2 => "LeftTrigger2",
        Button::RightTrigger => "RightTrigger",
        Button::RightTrigger2 => "RightTrigger2",
        Button::Select => "Select",
        Button::Start => "Start",
        Button::Mode => "Mode",
        Button::LeftThumb => "LeftThumb",
        Button::RightThumb => "RightThumb",
        Button::DPadUp => "DPadUp",
        Button::DPadDown => "DPadDown",
        Button::DPadLeft => "DPadLeft",
        Button::DPadRight => "DPadRight",
        Button::Unknown => return None,
    })
}

/// The `(negative, positive)` pad names for the two directions of a gilrs axis.
/// `None` means that direction has no name.
///
/// Trigger axes (`LeftZ`, `RightZ`) only report the positive, pressed direction, as
/// `LeftTrigger2` and `RightTrigger2`.
pub fn axis_pad_names(axis: Axis) -> (Option<&'static str>, Option<&'static str>) {
    match axis {
        Axis::LeftStickX => (Some("LeftStickLeft"), Some("LeftStickRight")),
        Axis::LeftStickY => (Some("LeftStickDown"), Some("LeftStickUp")),
        Axis::RightStickX => (Some("RightStickLeft"), Some("RightStickRight")),
        Axis::RightStickY => (Some("RightStickDown"), Some("RightStickUp")),
        Axis::DPadX => (Some("DPadLeft"), Some("DPadRight")),
        Axis::DPadY => (Some("DPadDown"), Some("DPadUp")),
        Axis::LeftZ => (None, Some("LeftTrigger2")),
        Axis::RightZ => (None, Some("RightTrigger2")),
        Axis::Unknown => (None, None),
    }
}

/// One gilrs event, reduced to the parts this backend uses.
#[derive(Debug, Clone, Copy, PartialEq)]
enum RawInput {
    Pressed(Button),
    Released(Button),
    Axis(Axis, f32),
    Disconnected,
}

fn device_name(device: usize) -> String {
    format!("gilrs-{device}")
}

fn pad_event(device: usize, button: &'static str, pressed: bool) -> GamepadEvent {
    GamepadEvent {
        device: device_name(device),
        button: button.to_owned(),
        pressed,
    }
}

/// Turns raw inputs into named transitions. Keeps the state needed to report each
/// name only when it changes.
#[derive(Debug, Default)]
struct Translator {
    /// Names currently pressed, per device index. A repeated press or release of a
    /// name that is already in the requested state produces no event.
    held: BTreeSet<(usize, &'static str)>,
    /// The name each axis direction currently holds, per device index and axis.
    axis_held: HashMap<(usize, Axis), &'static str>,
}

impl Translator {
    fn press(&mut self, device: usize, name: &'static str, out: &mut Vec<GamepadEvent>) {
        if self.held.insert((device, name)) {
            out.push(pad_event(device, name, true));
        }
    }

    fn release(&mut self, device: usize, name: &'static str, out: &mut Vec<GamepadEvent>) {
        if self.held.remove(&(device, name)) {
            out.push(pad_event(device, name, false));
        }
    }

    fn apply(&mut self, device: usize, input: RawInput, out: &mut Vec<GamepadEvent>) {
        match input {
            RawInput::Pressed(button) => {
                if let Some(name) = button_pad_name(button) {
                    self.press(device, name, out);
                }
            }
            RawInput::Released(button) => {
                if let Some(name) = button_pad_name(button) {
                    self.release(device, name, out);
                }
            }
            RawInput::Axis(axis, value) => self.axis(device, axis, value, out),
            RawInput::Disconnected => {
                self.axis_held.retain(|(dev, _), _| *dev != device);
                let stale: Vec<&'static str> = self
                    .held
                    .iter()
                    .filter(|(dev, _)| *dev == device)
                    .map(|(_, name)| *name)
                    .collect();
                for name in stale {
                    self.release(device, name, out);
                }
            }
        }
    }

    fn axis(&mut self, device: usize, axis: Axis, value: f32, out: &mut Vec<GamepadEvent>) {
        let (negative, positive) = axis_pad_names(axis);
        let current = self.axis_held.get(&(device, axis)).copied();
        let target = if value >= AXIS_PRESS {
            positive
        } else if value <= -AXIS_PRESS {
            negative
        } else if value.abs() <= AXIS_RELEASE {
            None
        } else {
            current
        };
        if target == current {
            return;
        }
        if let Some(name) = current {
            self.release(device, name, out);
        }
        match target {
            Some(name) => {
                self.axis_held.insert((device, axis), name);
                self.press(device, name, out);
            }
            None => {
                self.axis_held.remove(&(device, axis));
            }
        }
    }
}

/// Gamepad backend over a gilrs context.
pub struct GilrsGamepad {
    gilrs: Gilrs,
    translator: Translator,
}

impl GilrsGamepad {
    /// Open the gilrs context. Returns [`GamepadError::Unavailable`] when gilrs has no
    /// support for this platform, so the caller can fall back to `NullGamepad`.
    pub fn new() -> Result<Self, GamepadError> {
        match Gilrs::new() {
            Ok(gilrs) => Ok(Self {
                gilrs,
                translator: Translator::default(),
            }),
            Err(gilrs::Error::NotImplemented(_)) => Err(GamepadError::Unavailable("gilrs")),
            Err(err) => Err(GamepadError::Backend(err.to_string())),
        }
    }
}

impl GamepadBackend for GilrsGamepad {
    fn name(&self) -> &'static str {
        "gilrs"
    }

    fn poll(&mut self) -> Result<Vec<GamepadEvent>, GamepadError> {
        let mut out = Vec::new();
        // `next_event` does not block, so this drains only what is already queued.
        while let Some(event) = self.gilrs.next_event() {
            let device = usize::from(event.id);
            let raw = match event.event {
                EventType::ButtonPressed(button, _) => RawInput::Pressed(button),
                EventType::ButtonReleased(button, _) => RawInput::Released(button),
                EventType::AxisChanged(axis, value, _) => RawInput::Axis(axis, value),
                EventType::Disconnected => RawInput::Disconnected,
                _ => continue,
            };
            self.translator.apply(device, raw, &mut out);
        }
        Ok(out)
    }

    fn connected_controllers(&self) -> usize {
        self.gilrs.gamepads().count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(events: &[GamepadEvent]) -> Vec<(String, bool)> {
        events
            .iter()
            .map(|e| (e.button.clone(), e.pressed))
            .collect()
    }

    fn feed(translator: &mut Translator, device: usize, inputs: &[RawInput]) -> Vec<GamepadEvent> {
        let mut out = Vec::new();
        for input in inputs {
            translator.apply(device, *input, &mut out);
        }
        out
    }

    #[test]
    fn named_buttons_map_to_the_pad_table() {
        assert_eq!(button_pad_name(Button::South), Some("South"));
        assert_eq!(button_pad_name(Button::DPadUp), Some("DPadUp"));
        assert_eq!(button_pad_name(Button::Start), Some("Start"));
        assert_eq!(button_pad_name(Button::LeftThumb), Some("LeftThumb"));
        assert_eq!(
            button_pad_name(Button::RightTrigger2),
            Some("RightTrigger2")
        );
        assert_eq!(button_pad_name(Button::Mode), Some("Mode"));
    }

    #[test]
    fn every_named_button_has_a_distinct_pad_name() {
        let all = [
            Button::South,
            Button::East,
            Button::North,
            Button::West,
            Button::C,
            Button::Z,
            Button::LeftTrigger,
            Button::LeftTrigger2,
            Button::RightTrigger,
            Button::RightTrigger2,
            Button::Select,
            Button::Start,
            Button::Mode,
            Button::LeftThumb,
            Button::RightThumb,
            Button::DPadUp,
            Button::DPadDown,
            Button::DPadLeft,
            Button::DPadRight,
        ];
        let mut seen = BTreeSet::new();
        for button in all {
            let name = button_pad_name(button).expect("named button");
            assert!(!name.contains(char::is_whitespace), "{name}");
            assert!(seen.insert(name), "duplicate pad name {name}");
        }
        assert_eq!(seen.len(), 19);
    }

    #[test]
    fn unknown_button_has_no_pad_name() {
        assert_eq!(button_pad_name(Button::Unknown), None);
    }

    #[test]
    fn stick_and_dpad_axes_map_to_both_directions() {
        assert_eq!(
            axis_pad_names(Axis::LeftStickY),
            (Some("LeftStickDown"), Some("LeftStickUp"))
        );
        assert_eq!(
            axis_pad_names(Axis::RightStickX),
            (Some("RightStickLeft"), Some("RightStickRight"))
        );
        assert_eq!(
            axis_pad_names(Axis::DPadX),
            (Some("DPadLeft"), Some("DPadRight"))
        );
        assert_eq!(
            axis_pad_names(Axis::DPadY),
            (Some("DPadDown"), Some("DPadUp"))
        );
    }

    #[test]
    fn trigger_axes_report_only_the_pressed_direction() {
        assert_eq!(axis_pad_names(Axis::LeftZ), (None, Some("LeftTrigger2")));
        assert_eq!(axis_pad_names(Axis::RightZ), (None, Some("RightTrigger2")));
        assert_eq!(axis_pad_names(Axis::Unknown), (None, None));
    }

    #[test]
    fn button_press_and_release_are_reported_once_each() {
        let mut t = Translator::default();
        let events = feed(
            &mut t,
            0,
            &[
                RawInput::Pressed(Button::South),
                RawInput::Pressed(Button::South),
                RawInput::Released(Button::South),
                RawInput::Released(Button::South),
            ],
        );
        assert_eq!(
            names(&events),
            vec![("South".to_owned(), true), ("South".to_owned(), false)]
        );
        assert_eq!(events[0].device, "gilrs-0");
    }

    #[test]
    fn unknown_buttons_produce_nothing() {
        let mut t = Translator::default();
        let events = feed(&mut t, 0, &[RawInput::Pressed(Button::Unknown)]);
        assert!(events.is_empty());
    }

    #[test]
    fn stick_press_holds_through_the_dead_band_then_releases() {
        let mut t = Translator::default();
        let events = feed(
            &mut t,
            1,
            &[
                RawInput::Axis(Axis::LeftStickX, 0.6),
                RawInput::Axis(Axis::LeftStickX, 0.9),
                RawInput::Axis(Axis::LeftStickX, 0.4),
                RawInput::Axis(Axis::LeftStickX, 0.1),
            ],
        );
        assert_eq!(
            names(&events),
            vec![
                ("LeftStickRight".to_owned(), true),
                ("LeftStickRight".to_owned(), false),
            ]
        );
    }

    #[test]
    fn stick_flick_across_centre_releases_one_direction_and_presses_the_other() {
        let mut t = Translator::default();
        let events = feed(
            &mut t,
            0,
            &[
                RawInput::Axis(Axis::LeftStickX, 0.9),
                RawInput::Axis(Axis::LeftStickX, -0.9),
            ],
        );
        assert_eq!(
            names(&events),
            vec![
                ("LeftStickRight".to_owned(), true),
                ("LeftStickRight".to_owned(), false),
                ("LeftStickLeft".to_owned(), true),
            ]
        );
    }

    #[test]
    fn small_stick_noise_inside_the_dead_band_is_silent() {
        let mut t = Translator::default();
        let events = feed(
            &mut t,
            0,
            &[
                RawInput::Axis(Axis::LeftStickY, 0.05),
                RawInput::Axis(Axis::LeftStickY, -0.2),
                RawInput::Axis(Axis::LeftStickY, 0.25),
            ],
        );
        assert!(events.is_empty());
    }

    #[test]
    fn dpad_axis_and_dpad_button_do_not_double_report() {
        // Some devices send both the hat axis and the D-pad button for one press.
        let mut t = Translator::default();
        let events = feed(
            &mut t,
            0,
            &[
                RawInput::Pressed(Button::DPadUp),
                RawInput::Axis(Axis::DPadY, 1.0),
                RawInput::Released(Button::DPadUp),
                RawInput::Axis(Axis::DPadY, 0.0),
            ],
        );
        assert_eq!(
            names(&events),
            vec![("DPadUp".to_owned(), true), ("DPadUp".to_owned(), false)]
        );
    }

    #[test]
    fn disconnect_releases_only_that_devices_held_names() {
        let mut t = Translator::default();
        let _ = feed(
            &mut t,
            0,
            &[
                RawInput::Pressed(Button::Start),
                RawInput::Axis(Axis::LeftStickX, -0.9),
            ],
        );
        let _ = feed(&mut t, 1, &[RawInput::Pressed(Button::Start)]);

        let events = feed(&mut t, 0, &[RawInput::Disconnected]);
        let mut released = names(&events);
        released.sort();
        assert_eq!(
            released,
            vec![
                ("LeftStickLeft".to_owned(), false),
                ("Start".to_owned(), false),
            ]
        );
        assert!(events.iter().all(|e| e.device == "gilrs-0"));

        // Device 1 still holds Start, so its release is still reported later.
        let later = feed(&mut t, 1, &[RawInput::Released(Button::Start)]);
        assert_eq!(names(&later), vec![("Start".to_owned(), false)]);
    }

    #[test]
    fn devices_do_not_share_held_state() {
        let mut t = Translator::default();
        let a = feed(&mut t, 0, &[RawInput::Pressed(Button::South)]);
        let b = feed(&mut t, 1, &[RawInput::Pressed(Button::South)]);
        assert_eq!(a.len(), 1);
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].device, "gilrs-1");
    }
}
