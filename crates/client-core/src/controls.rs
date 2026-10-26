//! UI-independent playback control math and state.

use std::time::Duration;

/// A seek request expressed relative to the current position or from the start.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SeekRequest {
    /// Move by a signed whole number of seconds.
    RelativeSeconds(i64),
    /// Seek to an absolute position.
    Absolute(Duration),
}

/// Calculate a seek destination, clamped before the final second of the item.
///
/// Unknown durations disable seeking. For media shorter than one second, the
/// only valid destination is zero.
pub fn seek_target(
    current: Duration,
    duration: Option<Duration>,
    request: SeekRequest,
) -> Option<Duration> {
    let duration = duration?;
    let upper_bound = duration.saturating_sub(Duration::from_secs(1));
    let target = match request {
        SeekRequest::RelativeSeconds(seconds) if seconds < 0 => {
            current.saturating_sub(Duration::from_secs(seconds.unsigned_abs()))
        }
        SeekRequest::RelativeSeconds(seconds) => {
            current.saturating_add(Duration::from_secs(seconds as u64))
        }
        SeekRequest::Absolute(position) => position,
    };

    Some(target.min(upper_bound))
}

/// Supported playback speed choices, in ascending order.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum PlaybackSpeed {
    Half = 0,
    ThreeQuarters = 1,
    Normal = 2,
    FiveQuarters = 3,
    ThreeHalves = 4,
    Double = 5,
}

impl PlaybackSpeed {
    /// All supported speed choices in display order.
    pub const ALL: [Self; 6] = [
        Self::Half,
        Self::ThreeQuarters,
        Self::Normal,
        Self::FiveQuarters,
        Self::ThreeHalves,
        Self::Double,
    ];

    /// The playback rate multiplier.
    pub const fn multiplier(self) -> f64 {
        match self {
            Self::Half => 0.5,
            Self::ThreeQuarters => 0.75,
            Self::Normal => 1.0,
            Self::FiveQuarters => 1.25,
            Self::ThreeHalves => 1.5,
            Self::Double => 2.0,
        }
    }

    /// Select an adjacent speed; values beyond either end stay at that end.
    pub fn step(self, direction: i32) -> Self {
        let index = i32::from(self as u8);
        let last = Self::ALL.len() as i32 - 1;
        let next = (index + direction.signum()).clamp(0, last) as usize;
        Self::ALL[next]
    }
}

/// Inactivity duration before playback controls hide.
pub const OSD_INACTIVITY_TIMEOUT: Duration = Duration::from_secs(3);

/// Fake-clock-friendly on-screen-display visibility state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OsdVisibility {
    last_input: Option<Duration>,
    paused: bool,
}

impl OsdVisibility {
    /// Create a hidden OSD state.
    pub const fn new() -> Self {
        Self {
            last_input: None,
            paused: false,
        }
    }

    /// Record user input and restart the inactivity timer.
    pub fn on_input(&mut self, now: Duration) {
        self.last_input = Some(now);
    }

    /// Update whether playback is paused.
    pub fn set_paused(&mut self, paused: bool) {
        self.paused = paused;
    }

    /// Whether controls should be visible at the supplied monotonic time.
    pub fn is_visible_at(self, now: Duration) -> bool {
        self.paused
            || self
                .last_input
                .is_some_and(|last_input| now.saturating_sub(last_input) < OSD_INACTIVITY_TIMEOUT)
    }

    /// Return when the playing OSD should next repaint to hide, if it is visible.
    pub fn time_until_hidden(self, now: Duration) -> Option<Duration> {
        if self.paused {
            return None;
        }

        self.last_input
            .map(|last_input| OSD_INACTIVITY_TIMEOUT.saturating_sub(now.saturating_sub(last_input)))
            .filter(|remaining| !remaining.is_zero())
    }
}

impl Default for OsdVisibility {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::{OsdVisibility, PlaybackSpeed, SeekRequest, seek_target};
    use std::time::Duration;

    #[test]
    fn relative_seek_clamps_at_zero_and_before_end() {
        let duration = Duration::from_secs(100);
        assert_eq!(
            seek_target(
                Duration::from_secs(5),
                Some(duration),
                SeekRequest::RelativeSeconds(-10)
            ),
            Some(Duration::ZERO)
        );
        assert_eq!(
            seek_target(
                Duration::from_secs(95),
                Some(duration),
                SeekRequest::RelativeSeconds(20)
            ),
            Some(Duration::from_secs(99))
        );
    }

    #[test]
    fn absolute_seek_clamps_at_both_ends() {
        let duration = Duration::from_secs(30);
        assert_eq!(
            seek_target(
                Duration::ZERO,
                Some(duration),
                SeekRequest::Absolute(Duration::MAX)
            ),
            Some(Duration::from_secs(29))
        );
        assert_eq!(
            seek_target(
                Duration::ZERO,
                Some(duration),
                SeekRequest::Absolute(Duration::ZERO)
            ),
            Some(Duration::ZERO)
        );
    }

    #[test]
    fn unknown_duration_disables_seeking_and_short_duration_clamps_to_zero() {
        assert_eq!(
            seek_target(
                Duration::from_secs(4),
                None,
                SeekRequest::RelativeSeconds(10)
            ),
            None
        );
        assert_eq!(
            seek_target(
                Duration::ZERO,
                Some(Duration::from_millis(500)),
                SeekRequest::Absolute(Duration::from_millis(250))
            ),
            Some(Duration::ZERO)
        );
    }

    #[test]
    fn speed_choices_and_steps_match_the_spec() {
        assert_eq!(
            PlaybackSpeed::ALL.map(PlaybackSpeed::multiplier),
            [0.5, 0.75, 1.0, 1.25, 1.5, 2.0]
        );
        assert_eq!(PlaybackSpeed::Normal.step(-1), PlaybackSpeed::ThreeQuarters);
        assert_eq!(PlaybackSpeed::Normal.step(1), PlaybackSpeed::FiveQuarters);
        assert_eq!(PlaybackSpeed::Half.step(-1), PlaybackSpeed::Half);
        assert_eq!(PlaybackSpeed::Double.step(1), PlaybackSpeed::Double);
        assert_eq!(PlaybackSpeed::Normal.step(0), PlaybackSpeed::Normal);
    }

    #[test]
    fn osd_hides_after_three_seconds_without_input() {
        let mut osd = OsdVisibility::new();
        osd.on_input(Duration::from_secs(10));
        assert!(osd.is_visible_at(Duration::from_millis(12_999)));
        assert!(!osd.is_visible_at(Duration::from_secs(13)));
    }

    #[test]
    fn osd_stays_visible_while_paused_and_uses_elapsed_time_after_resume() {
        let mut osd = OsdVisibility::new();
        osd.on_input(Duration::from_secs(1));
        osd.set_paused(true);
        assert!(osd.is_visible_at(Duration::from_secs(60)));
        osd.set_paused(false);
        assert!(!osd.is_visible_at(Duration::from_secs(60)));

        osd.on_input(Duration::from_secs(60));
        assert!(osd.is_visible_at(Duration::from_secs(62)));
        osd.set_paused(true);
        assert!(osd.is_visible_at(Duration::from_secs(600)));
    }

    #[test]
    fn osd_exposes_only_the_remaining_hide_delay_while_playing() {
        let mut osd = OsdVisibility::new();
        assert_eq!(osd.time_until_hidden(Duration::ZERO), None);
        osd.on_input(Duration::from_secs(10));
        assert_eq!(
            osd.time_until_hidden(Duration::from_millis(12_500)),
            Some(Duration::from_millis(500))
        );
        osd.set_paused(true);
        assert_eq!(osd.time_until_hidden(Duration::from_millis(12_500)), None);
        osd.set_paused(false);
        assert_eq!(osd.time_until_hidden(Duration::from_secs(13)), None);
    }
}
