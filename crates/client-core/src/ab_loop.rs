//! A-B loop and frame-step state (M6-12). Pure: no playback, mpv, or clock calls.
//!
//! The owner maps the returned values to player commands. Loop points are session-only: the owner
//! drops the [`AbLoop`] when the item changes, so a loop never carries over to another item.
//!
//! Product choices where the spec leaves room:
//! - A point that breaks the order is rejected, not swapped. The existing points are kept.
//! - A loop must be at least [`MIN_LOOP_LENGTH`] long.
//! - Wrap rule: at or past B, the owner seeks to A. Before A, nothing wraps, so the user can seek
//!   to an earlier position and play into the loop.
//! - Chapter boundaries are not known here. The owner decides whether a loop may cross a chapter.

use std::time::Duration;

/// Shortest loop accepted, so that a loop cannot seek on every frame. Product choice.
pub const MIN_LOOP_LENGTH: Duration = Duration::from_millis(500);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AbLoopError {
    /// B was set while A is not set.
    MissingA,
    /// B was set at or before A.
    NotAfterA,
    /// A was set at or after B.
    NotBeforeB,
    /// The loop would be shorter than [`MIN_LOOP_LENGTH`].
    TooShort,
}

/// Loop points. Both are set, or neither. Every setter keeps `a < b` and
/// `b - a >= MIN_LOOP_LENGTH`, so a set pair is always a valid loop.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AbLoop {
    a: Option<Duration>,
    b: Option<Duration>,
}

impl AbLoop {
    /// No points set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set A to `position`. If B is set, A must lie before B by at least the minimum length.
    /// On error the state is unchanged.
    pub fn set_a(&mut self, position: Duration) -> Result<(), AbLoopError> {
        if let Some(b) = self.b {
            if position >= b {
                return Err(AbLoopError::NotBeforeB);
            }
            if b.saturating_sub(position) < MIN_LOOP_LENGTH {
                return Err(AbLoopError::TooShort);
            }
        }
        self.a = Some(position);
        Ok(())
    }

    /// Set B to `position`. A must already be set, and B must lie after A by at least the
    /// minimum length. On error the state is unchanged.
    pub fn set_b(&mut self, position: Duration) -> Result<(), AbLoopError> {
        let a = self.a.ok_or(AbLoopError::MissingA)?;
        if position <= a {
            return Err(AbLoopError::NotAfterA);
        }
        if position.saturating_sub(a) < MIN_LOOP_LENGTH {
            return Err(AbLoopError::TooShort);
        }
        self.b = Some(position);
        Ok(())
    }

    /// Clear both points with one action.
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    pub fn a(&self) -> Option<Duration> {
        self.a
    }

    pub fn b(&self) -> Option<Duration> {
        self.b
    }

    /// True only when both points are set.
    pub fn is_enabled(&self) -> bool {
        self.a.is_some() && self.b.is_some()
    }

    /// Where playback should seek when `position` has reached the loop end. Returns `Some(A)` when
    /// the loop is enabled and `position` is at or past B. Otherwise `None`.
    pub fn wrap_target(&self, position: Duration) -> Option<Duration> {
        match (self.a, self.b) {
            (Some(a), Some(b)) if position >= b => Some(a),
            _ => None,
        }
    }

    /// One press of the A-B control: with no points it sets A, with A only it sets B, with both it
    /// clears them. Both the key binding and the on-screen control call this, so the displayed
    /// state and the applied loop points cannot drift apart.
    pub fn cycle(&mut self, position: Duration) -> AbLoopCycle {
        match (self.a, self.b) {
            (Some(_), Some(_)) => {
                self.clear();
                AbLoopCycle::Cleared
            }
            (Some(_), None) => match self.set_b(position) {
                Ok(()) => AbLoopCycle::SetB(position),
                Err(error) => AbLoopCycle::Rejected(error),
            },
            (None, _) => match self.set_a(position) {
                Ok(()) => AbLoopCycle::SetA(position),
                Err(error) => AbLoopCycle::Rejected(error),
            },
        }
    }
}

/// What one press of the A-B control did. The host maps this to the player's loop points and to
/// the feedback it shows.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AbLoopCycle {
    /// A was set here. B is still unset, so the loop is not enabled yet.
    SetA(Duration),
    /// B was set here. The loop is now enabled.
    SetB(Duration),
    /// Both points were cleared.
    Cleared,
    /// The press was refused; the points are unchanged.
    Rejected(AbLoopError),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StepDirection {
    Forward,
    Back,
}

/// Frame index after one step, or `None` when the step is already at a bound.
///
/// `last_frame` is the final frame index when the owner knows the total. `None` means the total
/// is unknown, so only the lower bound (0) and the integer limit apply. A `current` past
/// `last_frame` cannot step forward.
pub fn frame_after_step(
    current: u64,
    direction: StepDirection,
    last_frame: Option<u64>,
) -> Option<u64> {
    match direction {
        StepDirection::Back => current.checked_sub(1),
        StepDirection::Forward => {
            if last_frame.is_some_and(|last| current >= last) {
                return None;
            }
            current.checked_add(1)
        }
    }
}

/// How a frame step is carried out. A step while playing pauses first, then steps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameStepPlan {
    pub pause_first: bool,
}

pub fn plan_frame_step(is_playing: bool) -> FrameStepPlan {
    FrameStepPlan {
        pause_first: is_playing,
    }
}

/// A command produced from the [`AbLoop`] state for the playback host to apply.
/// Only seek commands are produced; the host decides when to observe position.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LoopCommand {
    /// Seek back to A because playback reached or passed B.
    SeekToA(Duration),
    /// No loop action required at the current position.
    None,
}

/// Evaluate the loop against the current playback position and return the command
/// the host should apply. When `wrap_target` is `Some(A)`, the host should seek to A.
/// The host remains responsible for comparing `position` and calling this function
/// at the playback update rate; no clock call is made here.
pub fn loop_command(loop_state: &AbLoop, position: Duration) -> LoopCommand {
    match loop_state.wrap_target(position) {
        Some(a) => LoopCommand::SeekToA(a),
        None => LoopCommand::None,
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{
        AbLoop, AbLoopError, MIN_LOOP_LENGTH, StepDirection, frame_after_step, plan_frame_step,
    };

    fn secs(value: u64) -> Duration {
        Duration::from_secs(value)
    }

    fn loop_from(a: u64, b: u64) -> AbLoop {
        let mut loop_state = AbLoop::new();
        loop_state.set_a(secs(a)).expect("set a");
        loop_state.set_b(secs(b)).expect("set b");
        loop_state
    }

    #[test]
    fn a_then_b_enables_the_loop() {
        let loop_state = loop_from(10, 15);
        assert!(loop_state.is_enabled());
        assert_eq!(loop_state.a(), Some(secs(10)));
        assert_eq!(loop_state.b(), Some(secs(15)));
    }

    #[test]
    fn b_without_a_is_refused() {
        let mut loop_state = AbLoop::new();
        assert_eq!(loop_state.set_b(secs(5)), Err(AbLoopError::MissingA));
        assert!(!loop_state.is_enabled());
    }

    #[test]
    fn b_at_or_before_a_is_refused_and_state_is_kept() {
        let mut loop_state = AbLoop::new();
        loop_state.set_a(secs(10)).expect("set a");
        assert_eq!(loop_state.set_b(secs(10)), Err(AbLoopError::NotAfterA));
        assert_eq!(loop_state.set_b(secs(4)), Err(AbLoopError::NotAfterA));
        assert_eq!(loop_state.a(), Some(secs(10)));
        assert_eq!(loop_state.b(), None);
    }

    #[test]
    fn a_at_or_after_b_is_refused_and_state_is_kept() {
        let mut loop_state = loop_from(10, 15);
        assert_eq!(loop_state.set_a(secs(15)), Err(AbLoopError::NotBeforeB));
        assert_eq!(loop_state.set_a(secs(20)), Err(AbLoopError::NotBeforeB));
        assert_eq!(loop_state.a(), Some(secs(10)));
        assert_eq!(loop_state.b(), Some(secs(15)));
    }

    #[test]
    fn loops_shorter_than_the_minimum_are_refused() {
        let mut loop_state = AbLoop::new();
        loop_state.set_a(secs(10)).expect("set a");
        let too_close = secs(10) + MIN_LOOP_LENGTH - Duration::from_millis(1);
        assert_eq!(loop_state.set_b(too_close), Err(AbLoopError::TooShort));
        assert_eq!(loop_state.b(), None);

        loop_state
            .set_b(secs(10) + MIN_LOOP_LENGTH)
            .expect("minimum length is allowed");
        assert!(loop_state.is_enabled());

        let mut moved = loop_from(10, 20);
        assert_eq!(
            moved.set_a(secs(20) - Duration::from_millis(100)),
            Err(AbLoopError::TooShort)
        );
        assert_eq!(moved.a(), Some(secs(10)));
    }

    #[test]
    fn moving_a_earlier_is_allowed() {
        let mut loop_state = loop_from(10, 15);
        loop_state.set_a(secs(5)).expect("earlier A is valid");
        assert_eq!(loop_state.a(), Some(secs(5)));
        assert!(loop_state.is_enabled());
    }

    #[test]
    fn clear_drops_both_points() {
        let mut loop_state = loop_from(10, 15);
        loop_state.clear();
        assert_eq!(loop_state, AbLoop::new());
        assert!(!loop_state.is_enabled());
        assert_eq!(loop_state.wrap_target(secs(20)), None);
    }

    #[test]
    fn a_only_never_wraps() {
        let mut loop_state = AbLoop::new();
        loop_state.set_a(secs(10)).expect("set a");
        assert_eq!(loop_state.wrap_target(secs(99)), None);
    }

    #[test]
    fn wrap_happens_at_and_after_b_and_returns_a() {
        let loop_state = loop_from(10, 15);
        assert_eq!(loop_state.wrap_target(secs(14)), None);
        assert_eq!(loop_state.wrap_target(secs(15)), Some(secs(10)));
        assert_eq!(loop_state.wrap_target(secs(16)), Some(secs(10)));
    }

    #[test]
    fn positions_before_a_do_not_wrap() {
        let loop_state = loop_from(10, 15);
        assert_eq!(loop_state.wrap_target(secs(0)), None);
        assert_eq!(loop_state.wrap_target(secs(9)), None);
    }

    #[test]
    fn repeated_wraps_return_to_the_same_a() {
        let loop_state = loop_from(5, 10);
        let mut position = secs(5);
        for _ in 0..10 {
            while loop_state.wrap_target(position).is_none() {
                position += Duration::from_millis(100);
            }
            position = loop_state.wrap_target(position).expect("wrap");
            assert_eq!(position, secs(5));
        }
    }

    #[test]
    fn forward_step_moves_one_frame_and_stops_at_the_last_frame() {
        assert_eq!(
            frame_after_step(0, StepDirection::Forward, Some(3)),
            Some(1)
        );
        assert_eq!(
            frame_after_step(2, StepDirection::Forward, Some(3)),
            Some(3)
        );
        assert_eq!(frame_after_step(3, StepDirection::Forward, Some(3)), None);
    }

    #[test]
    fn back_step_stops_at_the_first_frame() {
        assert_eq!(frame_after_step(3, StepDirection::Back, Some(3)), Some(2));
        assert_eq!(frame_after_step(1, StepDirection::Back, None), Some(0));
        assert_eq!(frame_after_step(0, StepDirection::Back, Some(3)), None);
    }

    #[test]
    fn loop_command_produces_seek_when_at_or_past_b() {
        use super::{LoopCommand, loop_command};
        let loop_state = loop_from(10, 15);
        assert_eq!(loop_command(&loop_state, secs(14)), LoopCommand::None);
        assert_eq!(
            loop_command(&loop_state, secs(15)),
            LoopCommand::SeekToA(secs(10))
        );
        assert_eq!(
            loop_command(&loop_state, secs(20)),
            LoopCommand::SeekToA(secs(10))
        );
    }

    #[test]
    fn unknown_total_steps_forward_without_an_upper_bound() {
        assert_eq!(
            frame_after_step(1_000_000, StepDirection::Forward, None),
            Some(1_000_001)
        );
        assert_eq!(
            frame_after_step(u64::MAX, StepDirection::Forward, None),
            None
        );
    }

    #[test]
    fn index_past_the_known_last_frame_cannot_step_forward() {
        assert_eq!(frame_after_step(9, StepDirection::Forward, Some(3)), None);
    }

    #[test]
    fn stepping_while_playing_pauses_first() {
        assert!(plan_frame_step(true).pause_first);
        assert!(!plan_frame_step(false).pause_first);
    }

    #[test]
    fn cycle_sets_a_then_b_then_clears() {
        use super::AbLoopCycle;
        let mut loop_state = AbLoop::new();

        assert_eq!(loop_state.cycle(secs(10)), AbLoopCycle::SetA(secs(10)));
        assert_eq!(loop_state.a(), Some(secs(10)));
        assert!(!loop_state.is_enabled());

        assert_eq!(loop_state.cycle(secs(15)), AbLoopCycle::SetB(secs(15)));
        assert!(loop_state.is_enabled());

        assert_eq!(loop_state.cycle(secs(30)), AbLoopCycle::Cleared);
        assert_eq!(loop_state.a(), None);
        assert_eq!(loop_state.b(), None);
    }

    #[test]
    fn cycle_refuses_a_b_point_that_is_not_after_a_and_keeps_a() {
        use super::AbLoopCycle;
        let mut loop_state = AbLoop::new();
        loop_state.cycle(secs(10));

        assert_eq!(
            loop_state.cycle(secs(9)),
            AbLoopCycle::Rejected(AbLoopError::NotAfterA)
        );
        assert_eq!(loop_state.a(), Some(secs(10)));
        assert_eq!(loop_state.b(), None);
        assert!(!loop_state.is_enabled());
    }

    #[test]
    fn cycle_refuses_a_b_point_too_close_to_a_and_still_clears_next() {
        use super::AbLoopCycle;
        let mut loop_state = AbLoop::new();
        loop_state.cycle(secs(10));
        let too_close = secs(10) + MIN_LOOP_LENGTH - Duration::from_millis(1);

        assert_eq!(
            loop_state.cycle(too_close),
            AbLoopCycle::Rejected(AbLoopError::TooShort)
        );
        assert!(!loop_state.is_enabled());

        // A refused B press must not lose A, so the next press attempts B again.
        assert_eq!(loop_state.cycle(secs(12)), AbLoopCycle::SetB(secs(12)));
        assert!(loop_state.is_enabled());
    }
}
