//! Picture-in-picture window state (M2-12). Pure: no windowing calls and no I/O.
//!
//! The host owns the window. It applies the returned [`PipTransition`] (always-on-top, resize,
//! fullscreen, and placement) and then reports back. Every transition is synchronous here, so the
//! state never sits half-applied. A second toggle requested while the host is still applying the
//! first must wait for the host to finish. That queueing belongs to the host.
//!
//! Product choices where the spec leaves room:
//! - PiP is entered only while a video item is active: playing, paused, buffering, seeking, or
//!   switching streams. An audio-only item, or no item, is refused.
//! - The normal-state geometry is captured on enter and kept in the state. The PiP state therefore
//!   cannot exist without saved geometry.
//! - A fullscreen request made inside PiP is recorded for the restore. The PiP window stays
//!   windowed.
//! - Leaving PiP places the window with [`plan_placement`], so a monitor that has gone away is
//!   handled the same way as at startup (M2-14).
//! - If the video stops while PiP is on, the controller leaves PiP on its own.

use crate::PlaybackPhase;
use crate::window_state::{MonitorInfo, Placement, SavedWindow, plan_placement};

/// Default PiP size in logical pixels. Settings keys `window.pipWidth` and `window.pipHeight`.
pub const DEFAULT_PIP_WIDTH: u32 = 480;
pub const DEFAULT_PIP_HEIGHT: u32 = 270;
const MIN_PIP_WIDTH: u32 = 160;
const MIN_PIP_HEIGHT: u32 = 90;
const MAX_PIP_WIDTH: u32 = 3840;
const MAX_PIP_HEIGHT: u32 = 2160;

/// PiP size in logical pixels, within fixed bounds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PipSize {
    width: u32,
    height: u32,
}

impl PipSize {
    /// Returns `None` when a side is outside the allowed range.
    pub fn new(width: u32, height: u32) -> Option<Self> {
        let width_ok = (MIN_PIP_WIDTH..=MAX_PIP_WIDTH).contains(&width);
        let height_ok = (MIN_PIP_HEIGHT..=MAX_PIP_HEIGHT).contains(&height);
        (width_ok && height_ok).then_some(Self { width, height })
    }

    pub fn default_size() -> Self {
        Self {
            width: DEFAULT_PIP_WIDTH,
            height: DEFAULT_PIP_HEIGHT,
        }
    }

    pub fn width(self) -> u32 {
        self.width
    }

    pub fn height(self) -> u32 {
        self.height
    }
}

/// PiP state. The `Pip` variant always carries the geometry to restore on exit.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum PipState {
    #[default]
    Normal,
    Pip {
        restore: SavedWindow,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PipError {
    AlreadyInPip,
    NotInPip,
    /// No video item is active, so PiP is refused.
    NoVideoPlaying,
}

/// What the host should do after a transition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PipTransition {
    /// Set always-on-top, leave fullscreen when `leave_fullscreen` is set, and resize to `size`.
    Enter {
        size: PipSize,
        leave_fullscreen: bool,
    },
    /// Clear always-on-top and apply `placement` (the saved normal-state window).
    Leave { placement: Placement },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FullscreenOutcome {
    /// The window is in normal state. The host applies the fullscreen change now.
    Applied,
    /// The window is in PiP. The host must keep the PiP window windowed. The choice is saved for
    /// the restore on exit.
    Deferred,
}

/// Whether PiP may be entered or kept for this playback state.
pub fn can_show_video(phase: Option<PlaybackPhase>, has_video: bool) -> bool {
    has_video
        && matches!(
            phase,
            Some(
                PlaybackPhase::Playing
                    | PlaybackPhase::Paused
                    | PlaybackPhase::Buffering
                    | PlaybackPhase::Seeking
                    | PlaybackPhase::SwitchingStream
            )
        )
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PipController {
    state: PipState,
}

impl PipController {
    /// Normal state. PiP is off on start (M2-12).
    pub fn new() -> Self {
        Self::default()
    }

    pub fn state(&self) -> &PipState {
        &self.state
    }

    pub fn is_pip(&self) -> bool {
        matches!(self.state, PipState::Pip { .. })
    }

    /// Enter PiP. `normal` is the current normal-state window, captured by the host before it
    /// changes anything. On error the state is unchanged.
    pub fn enter(
        &mut self,
        normal: SavedWindow,
        size: PipSize,
        phase: Option<PlaybackPhase>,
        has_video: bool,
    ) -> Result<PipTransition, PipError> {
        if self.is_pip() {
            return Err(PipError::AlreadyInPip);
        }
        if !can_show_video(phase, has_video) {
            return Err(PipError::NoVideoPlaying);
        }
        let leave_fullscreen = normal.fullscreen;
        self.state = PipState::Pip { restore: normal };
        Ok(PipTransition::Enter {
            size,
            leave_fullscreen,
        })
    }

    /// Leave PiP and return the saved window placement. `monitors` are the monitors the host sees
    /// now. On error the state is unchanged.
    pub fn leave(&mut self, monitors: &[MonitorInfo]) -> Result<PipTransition, PipError> {
        match std::mem::take(&mut self.state) {
            PipState::Normal => Err(PipError::NotInPip),
            PipState::Pip { restore } => Ok(PipTransition::Leave {
                placement: plan_placement(Some(&restore), monitors),
            }),
        }
    }

    /// Record a fullscreen request from the user. In PiP it is saved for the restore and the
    /// PiP window stays windowed. In normal state the host applies it now.
    pub fn set_fullscreen_requested(&mut self, on: bool) -> FullscreenOutcome {
        match &mut self.state {
            PipState::Normal => FullscreenOutcome::Applied,
            PipState::Pip { restore } => {
                restore.fullscreen = on;
                FullscreenOutcome::Deferred
            }
        }
    }

    /// Report a playback change while PiP may be on. If the video is no longer active, PiP is
    /// left and the transition is returned. Otherwise `None`.
    pub fn playback_changed(
        &mut self,
        phase: Option<PlaybackPhase>,
        has_video: bool,
        monitors: &[MonitorInfo],
    ) -> Option<PipTransition> {
        if !self.is_pip() || can_show_video(phase, has_video) {
            return None;
        }
        self.leave(monitors).ok()
    }
}

#[cfg(test)]
mod tests {
    use crate::PlaybackPhase;
    use crate::window_state::{MonitorInfo, PixelRect, Placement, SavedWindow};

    use super::{
        FullscreenOutcome, PipController, PipError, PipSize, PipState, PipTransition,
        can_show_video,
    };

    fn normal_window() -> SavedWindow {
        SavedWindow {
            x: 100,
            y: 80,
            width: 1280,
            height: 720,
            monitor_name: "Primary".to_owned(),
            maximized: false,
            fullscreen: false,
        }
    }

    fn monitors() -> Vec<MonitorInfo> {
        vec![MonitorInfo {
            name: "Primary".to_owned(),
            bounds: PixelRect {
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
            },
            scale_factor: 1.0,
            is_primary: true,
        }]
    }

    fn size() -> PipSize {
        PipSize::default_size()
    }

    fn enter_playing(controller: &mut PipController, window: SavedWindow) {
        controller
            .enter(window, size(), Some(PlaybackPhase::Playing), true)
            .expect("enter");
    }

    #[test]
    fn enter_and_leave_round_trip_restores_the_saved_placement() {
        let mut controller = PipController::new();
        assert!(!controller.is_pip());
        enter_playing(&mut controller, normal_window());
        assert!(controller.is_pip());

        match controller.leave(&monitors()).expect("leave") {
            PipTransition::Leave {
                placement:
                    Placement::Window {
                        monitor_name,
                        rect,
                        maximized,
                        fullscreen,
                    },
            } => {
                assert_eq!(monitor_name, "Primary");
                assert_eq!(
                    rect,
                    PixelRect {
                        x: 100,
                        y: 80,
                        width: 1280,
                        height: 720,
                    }
                );
                assert!(!maximized);
                assert!(!fullscreen);
            }
            other => panic!("unexpected transition {other:?}"),
        }
        assert_eq!(controller.state(), &PipState::Normal);
    }

    #[test]
    fn pip_is_never_entered_without_an_active_video() {
        let mut controller = PipController::new();
        assert_eq!(
            controller.enter(normal_window(), size(), None, true),
            Err(PipError::NoVideoPlaying)
        );
        for phase in [
            PlaybackPhase::Resolving,
            PlaybackPhase::Loading,
            PlaybackPhase::Ended,
            PlaybackPhase::Stopped,
            PlaybackPhase::Failed,
        ] {
            assert_eq!(
                controller.enter(normal_window(), size(), Some(phase), true),
                Err(PipError::NoVideoPlaying),
                "{phase:?}"
            );
        }
        assert_eq!(
            controller.enter(normal_window(), size(), Some(PlaybackPhase::Playing), false),
            Err(PipError::NoVideoPlaying),
            "audio-only item"
        );
        assert_eq!(controller.state(), &PipState::Normal);
    }

    #[test]
    fn active_video_phases_allow_pip() {
        for phase in [
            PlaybackPhase::Playing,
            PlaybackPhase::Paused,
            PlaybackPhase::Buffering,
            PlaybackPhase::Seeking,
            PlaybackPhase::SwitchingStream,
        ] {
            assert!(can_show_video(Some(phase), true), "{phase:?}");
        }
        assert!(!can_show_video(Some(PlaybackPhase::Playing), false));
    }

    #[test]
    fn entering_twice_is_refused_and_keeps_the_first_saved_window() {
        let mut controller = PipController::new();
        enter_playing(&mut controller, normal_window());
        let mut other = normal_window();
        other.x = 999;
        assert_eq!(
            controller.enter(other, size(), Some(PlaybackPhase::Playing), true),
            Err(PipError::AlreadyInPip)
        );
        match controller.state() {
            PipState::Pip { restore } => assert_eq!(restore.x, 100),
            PipState::Normal => panic!("still in PiP"),
        }
    }

    #[test]
    fn leaving_when_not_in_pip_is_refused() {
        let mut controller = PipController::new();
        assert_eq!(controller.leave(&monitors()), Err(PipError::NotInPip));
    }

    #[test]
    fn enter_leaves_fullscreen_when_it_was_on() {
        let mut controller = PipController::new();
        let mut fullscreen = normal_window();
        fullscreen.fullscreen = true;
        let transition = controller
            .enter(fullscreen, size(), Some(PlaybackPhase::Playing), true)
            .expect("enter");
        assert_eq!(
            transition,
            PipTransition::Enter {
                size: size(),
                leave_fullscreen: true,
            }
        );
    }

    #[test]
    fn fullscreen_inside_pip_is_saved_for_the_restore_and_stays_windowed() {
        let mut controller = PipController::new();
        enter_playing(&mut controller, normal_window());
        assert_eq!(
            controller.set_fullscreen_requested(true),
            FullscreenOutcome::Deferred
        );
        assert!(controller.is_pip());
        match controller.leave(&monitors()).expect("leave") {
            PipTransition::Leave {
                placement: Placement::Window { fullscreen, .. },
            } => assert!(fullscreen),
            other => panic!("unexpected transition {other:?}"),
        }
    }

    #[test]
    fn fullscreen_in_normal_state_is_applied_now() {
        let mut controller = PipController::new();
        assert_eq!(
            controller.set_fullscreen_requested(true),
            FullscreenOutcome::Applied
        );
        assert_eq!(controller.state(), &PipState::Normal);
    }

    #[test]
    fn maximized_state_is_restored_on_exit() {
        let mut controller = PipController::new();
        let mut maximized = normal_window();
        maximized.maximized = true;
        enter_playing(&mut controller, maximized);
        match controller.leave(&monitors()).expect("leave") {
            PipTransition::Leave {
                placement: Placement::Window { maximized, .. },
            } => assert!(maximized),
            other => panic!("unexpected transition {other:?}"),
        }
    }

    #[test]
    fn missing_monitor_on_exit_falls_back_without_panicking() {
        let mut controller = PipController::new();
        enter_playing(&mut controller, normal_window());
        match controller.leave(&[]).expect("leave") {
            PipTransition::Leave {
                placement: Placement::Unplaced { .. },
            } => {}
            other => panic!("unexpected transition {other:?}"),
        }
    }

    #[test]
    fn video_stopping_while_in_pip_leaves_pip() {
        let mut controller = PipController::new();
        enter_playing(&mut controller, normal_window());
        assert_eq!(
            controller.playback_changed(Some(PlaybackPhase::Playing), true, &monitors()),
            None
        );
        let transition = controller
            .playback_changed(Some(PlaybackPhase::Ended), true, &monitors())
            .expect("left PiP");
        assert!(matches!(transition, PipTransition::Leave { .. }));
        assert_eq!(controller.state(), &PipState::Normal);
        assert_eq!(
            controller.playback_changed(None, false, &monitors()),
            None,
            "nothing to leave in normal state"
        );
    }

    #[test]
    fn ten_toggles_return_to_the_same_placement_each_time() {
        let mut controller = PipController::new();
        let original = normal_window();
        for _ in 0..10 {
            enter_playing(&mut controller, original.clone());
            match controller.leave(&monitors()).expect("leave") {
                PipTransition::Leave {
                    placement: Placement::Window { rect, .. },
                } => assert_eq!(
                    rect,
                    PixelRect {
                        x: 100,
                        y: 80,
                        width: 1280,
                        height: 720,
                    }
                ),
                other => panic!("unexpected transition {other:?}"),
            }
        }
        assert_eq!(controller.state(), &PipState::Normal);
    }

    #[test]
    fn pip_size_is_bounded() {
        assert_eq!(PipSize::new(480, 270), Some(PipSize::default_size()));
        assert_eq!(PipSize::new(159, 270), None);
        assert_eq!(PipSize::new(480, 89), None);
        assert_eq!(PipSize::new(3841, 270), None);
        assert_eq!(PipSize::new(480, 2161), None);
    }
}
