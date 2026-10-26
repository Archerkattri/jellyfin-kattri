//! Gamepad navigation: maps controller button transitions to navigation commands and
//! schedules auto-repeat for a held direction.
//!
//! The mapping works on [`GamepadEvent`]s named with the `pad:` table of the gilrs
//! backend. Only press transitions produce a command, so releasing a button never
//! triggers anything. The repeat schedule is pure: callers pass the press time and the
//! current time in milliseconds, and no clock is read here.
//!
//! Tests use recorded events and pure models. No controller or display is opened.

use crate::focus::Direction;
use crate::gamepad::GamepadEvent;
use crate::grid_focus::FocusGrid;

/// Time between the first press of a held direction and its first repeat step.
pub const REPEAT_DELAY_MS: u64 = 400;

/// Time between consecutive repeat steps once the repeat has started.
pub const REPEAT_INTERVAL_MS: u64 = 100;

/// A navigation intent produced by a controller button.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NavigationCommand {
    MoveUp,
    MoveDown,
    MoveLeft,
    MoveRight,
    /// Confirm the focused item.
    Activate,
    /// Leave the current screen or level.
    Back,
    /// Shoulder or trigger that moves to the next page of content.
    PageNext,
    /// Shoulder or trigger that moves to the previous page of content.
    PagePrevious,
    /// Open the menu for the current screen.
    Menu,
}

impl NavigationCommand {
    /// The focus direction for a directional command, or `None` for the others.
    pub const fn direction(self) -> Option<Direction> {
        match self {
            Self::MoveUp => Some(Direction::Up),
            Self::MoveDown => Some(Direction::Down),
            Self::MoveLeft => Some(Direction::Left),
            Self::MoveRight => Some(Direction::Right),
            Self::Activate | Self::Back | Self::PageNext | Self::PagePrevious | Self::Menu => None,
        }
    }
}

/// The navigation command for a `pad:` button name, or `None` when the button has no
/// navigation role.
///
/// Both D-pad and stick directions move focus. South activates, East goes back, Start
/// opens the menu, and the left and right shoulder or trigger pairs page backward and
/// forward. Face buttons North and West, Select, Mode, the stick clicks and the
/// auxiliary buttons C and Z have no navigation command.
pub fn command_for_button(button: &str) -> Option<NavigationCommand> {
    Some(match button {
        "DPadUp" | "LeftStickUp" | "RightStickUp" => NavigationCommand::MoveUp,
        "DPadDown" | "LeftStickDown" | "RightStickDown" => NavigationCommand::MoveDown,
        "DPadLeft" | "LeftStickLeft" | "RightStickLeft" => NavigationCommand::MoveLeft,
        "DPadRight" | "LeftStickRight" | "RightStickRight" => NavigationCommand::MoveRight,
        "South" => NavigationCommand::Activate,
        "East" => NavigationCommand::Back,
        "Start" => NavigationCommand::Menu,
        "LeftTrigger" | "LeftTrigger2" => NavigationCommand::PagePrevious,
        "RightTrigger" | "RightTrigger2" => NavigationCommand::PageNext,
        _ => return None,
    })
}

/// The navigation command for a gamepad event.
///
/// Button releases (`pressed == false`) map to `None`. The device identifier does not
/// affect the result.
pub fn navigation_command(event: &GamepadEvent) -> Option<NavigationCommand> {
    if !event.pressed {
        return None;
    }
    command_for_button(&event.button)
}

/// Apply a navigation command to a focus model and return the updated focus.
///
/// Directional commands move focus as [`FocusGrid::step`] describes. `PageNext` and
/// `PagePrevious` move to the next or previous non-empty row, the same as `MoveDown`
/// and `MoveUp`. `Activate`, `Back` and `Menu` leave focus unchanged, since the caller
/// acts on the focused item or the screen for those commands.
pub fn apply_navigation(mut focus: FocusGrid, command: NavigationCommand) -> FocusGrid {
    let direction = match command {
        NavigationCommand::PageNext => Some(Direction::Down),
        NavigationCommand::PagePrevious => Some(Direction::Up),
        other => other.direction(),
    };
    if let Some(direction) = direction {
        focus.step(direction);
    }
    focus
}

/// Number of repeat steps due at or before `now_ms` for a direction first pressed at
/// `pressed_at_ms`.
///
/// The first step is due `REPEAT_DELAY_MS` after the press and each later step is due
/// `REPEAT_INTERVAL_MS` after the previous one. The press itself is not counted. The
/// result is 0 before the first step, including when `now_ms` is not after the press.
pub fn repeat_steps_due(pressed_at_ms: u64, now_ms: u64) -> u64 {
    let elapsed = match now_ms.checked_sub(pressed_at_ms) {
        Some(elapsed) => elapsed,
        None => return 0,
    };
    match elapsed.checked_sub(REPEAT_DELAY_MS) {
        Some(past_delay) => 1 + past_delay / REPEAT_INTERVAL_MS,
        None => 0,
    }
}

/// Tracks one held direction and reports when a repeat step is due.
///
/// Create it when the direction is pressed, then call [`DirectionRepeat::poll`] once
/// per frame with the current time. Several steps that came due since the previous
/// poll are reported as a single `true`, so a long frame produces one move rather than
/// a burst of moves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DirectionRepeat {
    pressed_at_ms: u64,
    steps_reported: u64,
}

impl DirectionRepeat {
    /// Start tracking a direction pressed at `pressed_at_ms`.
    pub const fn new(pressed_at_ms: u64) -> Self {
        Self {
            pressed_at_ms,
            steps_reported: 0,
        }
    }

    /// Report whether a repeat step has come due since the last poll.
    pub fn poll(&mut self, now_ms: u64) -> bool {
        let due = repeat_steps_due(self.pressed_at_ms, now_ms);
        if due > self.steps_reported {
            self.steps_reported = due;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(button: &str, pressed: bool) -> GamepadEvent {
        GamepadEvent {
            device: "recorded-pad".to_owned(),
            button: button.to_owned(),
            pressed,
        }
    }

    #[test]
    fn dpad_presses_map_to_directional_commands() {
        assert_eq!(
            navigation_command(&event("DPadUp", true)),
            Some(NavigationCommand::MoveUp)
        );
        assert_eq!(
            navigation_command(&event("DPadDown", true)),
            Some(NavigationCommand::MoveDown)
        );
        assert_eq!(
            navigation_command(&event("DPadLeft", true)),
            Some(NavigationCommand::MoveLeft)
        );
        assert_eq!(
            navigation_command(&event("DPadRight", true)),
            Some(NavigationCommand::MoveRight)
        );
    }

    #[test]
    fn stick_presses_map_to_the_same_directions_as_the_dpad() {
        for (stick, expected) in [
            ("LeftStickUp", NavigationCommand::MoveUp),
            ("LeftStickDown", NavigationCommand::MoveDown),
            ("LeftStickLeft", NavigationCommand::MoveLeft),
            ("LeftStickRight", NavigationCommand::MoveRight),
            ("RightStickUp", NavigationCommand::MoveUp),
            ("RightStickDown", NavigationCommand::MoveDown),
            ("RightStickLeft", NavigationCommand::MoveLeft),
            ("RightStickRight", NavigationCommand::MoveRight),
        ] {
            assert_eq!(
                navigation_command(&event(stick, true)),
                Some(expected),
                "{stick}"
            );
        }
    }

    #[test]
    fn south_activates_and_east_goes_back() {
        assert_eq!(
            navigation_command(&event("South", true)),
            Some(NavigationCommand::Activate)
        );
        assert_eq!(
            navigation_command(&event("East", true)),
            Some(NavigationCommand::Back)
        );
    }

    #[test]
    fn start_opens_the_menu() {
        assert_eq!(
            navigation_command(&event("Start", true)),
            Some(NavigationCommand::Menu)
        );
    }

    #[test]
    fn both_triggers_on_each_side_page_in_their_direction() {
        for name in ["LeftTrigger", "LeftTrigger2"] {
            assert_eq!(
                navigation_command(&event(name, true)),
                Some(NavigationCommand::PagePrevious),
                "{name}"
            );
        }
        for name in ["RightTrigger", "RightTrigger2"] {
            assert_eq!(
                navigation_command(&event(name, true)),
                Some(NavigationCommand::PageNext),
                "{name}"
            );
        }
    }

    #[test]
    fn unassigned_buttons_map_to_no_command() {
        for name in [
            "North",
            "West",
            "C",
            "Z",
            "Select",
            "Mode",
            "LeftThumb",
            "RightThumb",
            "KEY_BUTTON_0",
            "",
        ] {
            assert_eq!(command_for_button(name), None, "{name}");
            assert_eq!(navigation_command(&event(name, true)), None, "{name}");
        }
    }

    #[test]
    fn every_mapped_button_produces_nothing_on_release() {
        for name in [
            "DPadUp",
            "DPadDown",
            "DPadLeft",
            "DPadRight",
            "LeftStickUp",
            "RightStickRight",
            "South",
            "East",
            "Start",
            "LeftTrigger",
            "LeftTrigger2",
            "RightTrigger",
            "RightTrigger2",
        ] {
            assert!(command_for_button(name).is_some(), "{name} is not mapped");
            assert_eq!(navigation_command(&event(name, false)), None, "{name}");
        }
    }

    #[test]
    fn device_identifier_does_not_change_the_command() {
        let other_pad = GamepadEvent {
            device: "second-pad".to_owned(),
            button: "South".to_owned(),
            pressed: true,
        };
        assert_eq!(
            navigation_command(&other_pad),
            Some(NavigationCommand::Activate)
        );
    }

    #[test]
    fn directions_expose_their_focus_direction() {
        assert_eq!(NavigationCommand::MoveUp.direction(), Some(Direction::Up));
        assert_eq!(
            NavigationCommand::MoveDown.direction(),
            Some(Direction::Down)
        );
        assert_eq!(
            NavigationCommand::MoveLeft.direction(),
            Some(Direction::Left)
        );
        assert_eq!(
            NavigationCommand::MoveRight.direction(),
            Some(Direction::Right)
        );
        for command in [
            NavigationCommand::Activate,
            NavigationCommand::Back,
            NavigationCommand::PageNext,
            NavigationCommand::PagePrevious,
            NavigationCommand::Menu,
        ] {
            assert_eq!(command.direction(), None, "{command:?}");
        }
    }

    #[test]
    fn no_repeat_step_before_the_initial_delay() {
        assert_eq!(repeat_steps_due(1_000, 1_000), 0);
        assert_eq!(repeat_steps_due(1_000, 1_399), 0);
    }

    #[test]
    fn first_repeat_step_is_due_exactly_at_the_initial_delay() {
        assert_eq!(repeat_steps_due(1_000, 1_400), 1);
        assert_eq!(repeat_steps_due(1_000, 1_499), 1);
    }

    #[test]
    fn repeat_steps_follow_the_interval_after_the_delay() {
        assert_eq!(repeat_steps_due(1_000, 1_500), 2);
        assert_eq!(repeat_steps_due(1_000, 1_599), 2);
        assert_eq!(repeat_steps_due(1_000, 1_600), 3);
        assert_eq!(repeat_steps_due(1_000, 2_000), 7);
    }

    #[test]
    fn repeat_schedule_is_relative_to_the_press_time() {
        assert_eq!(repeat_steps_due(10_000, 10_399), 0);
        assert_eq!(repeat_steps_due(10_000, 10_400), 1);
    }

    #[test]
    fn a_clock_earlier_than_the_press_yields_no_steps() {
        assert_eq!(repeat_steps_due(1_000, 500), 0);
        assert_eq!(repeat_steps_due(u64::MAX, 0), 0);
    }

    #[test]
    fn repeat_count_does_not_overflow_at_the_largest_time() {
        let expected = 1 + (u64::MAX - REPEAT_DELAY_MS) / REPEAT_INTERVAL_MS;
        assert_eq!(repeat_steps_due(0, u64::MAX), expected);
    }

    #[test]
    fn direction_repeat_reports_each_step_once() {
        let mut repeat = DirectionRepeat::new(0);
        assert!(!repeat.poll(0));
        assert!(!repeat.poll(399));
        assert!(repeat.poll(400));
        assert!(!repeat.poll(401));
        assert!(!repeat.poll(499));
        assert!(repeat.poll(500));
        assert!(!repeat.poll(550));
    }

    #[test]
    fn direction_repeat_coalesces_steps_missed_between_polls() {
        let mut repeat = DirectionRepeat::new(0);
        assert!(repeat.poll(400));
        // Steps at 500 through 1000 were missed. One report covers them all.
        assert!(repeat.poll(1_000));
        assert!(!repeat.poll(1_000));
        assert!(!repeat.poll(1_050));
        assert!(repeat.poll(1_100));
    }

    #[test]
    fn direction_repeat_never_fires_before_the_delay_when_polled_early() {
        let mut repeat = DirectionRepeat::new(5_000);
        assert!(!repeat.poll(0));
        assert!(!repeat.poll(5_399));
        assert!(repeat.poll(5_400));
    }

    /// Two rows: three items on top, two below. Focus starts at (0, 0).
    fn two_rows() -> FocusGrid {
        FocusGrid::new(vec![3, 2])
    }

    #[test]
    fn directional_commands_move_focus_within_the_grid() {
        let focus = apply_navigation(two_rows(), NavigationCommand::MoveRight);
        assert_eq!(focus.current(), Some((0, 1)));
        let focus = apply_navigation(focus, NavigationCommand::MoveDown);
        assert_eq!(focus.current(), Some((1, 1)));
        let focus = apply_navigation(focus, NavigationCommand::MoveUp);
        assert_eq!(focus.current(), Some((0, 1)));
        let focus = apply_navigation(focus, NavigationCommand::MoveLeft);
        assert_eq!(focus.current(), Some((0, 0)));
    }

    #[test]
    fn directional_commands_clamp_at_the_edges() {
        let focus = apply_navigation(two_rows(), NavigationCommand::MoveLeft);
        assert_eq!(focus.current(), Some((0, 0)));
        let focus = apply_navigation(focus, NavigationCommand::MoveUp);
        assert_eq!(focus.current(), Some((0, 0)));

        let focus = FocusGrid::new(vec![3, 2]);
        let focus = apply_navigation(focus, NavigationCommand::MoveRight);
        let focus = apply_navigation(focus, NavigationCommand::MoveRight);
        let focus = apply_navigation(focus, NavigationCommand::MoveRight);
        assert_eq!(focus.current(), Some((0, 2)));
    }

    #[test]
    fn move_down_into_a_shorter_row_clamps_the_column() {
        let focus = apply_navigation(two_rows(), NavigationCommand::MoveRight);
        let focus = apply_navigation(focus, NavigationCommand::MoveRight);
        let focus = apply_navigation(focus, NavigationCommand::MoveDown);
        assert_eq!(focus.current(), Some((1, 1)));
    }

    #[test]
    fn page_commands_move_one_row_like_up_and_down() {
        let focus = FocusGrid::new(vec![3, 2]);
        let focus = apply_navigation(focus, NavigationCommand::MoveRight);
        let focus = apply_navigation(focus, NavigationCommand::MoveRight);
        let focus = apply_navigation(focus, NavigationCommand::PageNext);
        assert_eq!(focus.current(), Some((1, 1)));
        let focus = apply_navigation(focus, NavigationCommand::PagePrevious);
        assert_eq!(focus.current(), Some((0, 1)));
    }

    #[test]
    fn page_commands_stop_at_the_first_and_last_row() {
        let focus = apply_navigation(two_rows(), NavigationCommand::PagePrevious);
        assert_eq!(focus.current(), Some((0, 0)));
        let focus = apply_navigation(focus, NavigationCommand::PageNext);
        let focus = apply_navigation(focus, NavigationCommand::PageNext);
        assert_eq!(focus.current(), Some((1, 0)));
    }

    #[test]
    fn activate_back_and_menu_leave_focus_unchanged() {
        let focus = apply_navigation(two_rows(), NavigationCommand::MoveDown);
        let before = focus.clone();
        for command in [
            NavigationCommand::Activate,
            NavigationCommand::Back,
            NavigationCommand::Menu,
        ] {
            assert_eq!(
                apply_navigation(focus.clone(), command),
                before,
                "{command:?}"
            );
        }
    }

    #[test]
    fn every_command_is_a_no_op_on_an_empty_model() {
        let commands = [
            NavigationCommand::MoveUp,
            NavigationCommand::MoveDown,
            NavigationCommand::MoveLeft,
            NavigationCommand::MoveRight,
            NavigationCommand::Activate,
            NavigationCommand::Back,
            NavigationCommand::PageNext,
            NavigationCommand::PagePrevious,
            NavigationCommand::Menu,
        ];
        for command in commands {
            let focus = apply_navigation(FocusGrid::new(Vec::new()), command);
            assert_eq!(focus.current(), None, "{command:?}");
        }
    }

    #[test]
    fn recorded_pad_events_drive_focus_through_the_grid() {
        let recorded = [
            event("DPadRight", true),
            event("DPadRight", false),
            event("DPadRight", true),
            event("DPadDown", true),
            event("South", true),
            event("RightTrigger2", true),
            event("LeftTrigger", true),
            event("LeftStickUp", true),
        ];
        let mut focus = two_rows();
        let mut activations = 0;
        for recorded_event in &recorded {
            let Some(command) = navigation_command(recorded_event) else {
                continue;
            };
            if command == NavigationCommand::Activate {
                activations += 1;
            }
            focus = apply_navigation(focus, command);
        }
        assert_eq!(activations, 1);
        assert_eq!(focus.current(), Some((0, 1)));
    }
}
