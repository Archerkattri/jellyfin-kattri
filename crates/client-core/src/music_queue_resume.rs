//! The saved-queue resume position as one value with two stages.
//!
//! The app kept this in two fields, `music_queue_resume_position_ms: Option<u64>` and
//! `music_queue_resume_position_ticks: Option<i64>`. They were not redundant in time:
//!
//! 1. restore sets the **milliseconds** the queue was saved at; the Resume button is shown while
//!    that value exists;
//! 2. pressing Resume converts it to **ticks** for playback and clears the milliseconds value, which
//!    hides the button while the stream negotiates;
//! 3. the ticks value is consumed when playback starts.
//!
//! Two fields meant every clear site had to remember both. This type holds one value whose variant
//! says which stage it is in, so a clear is one assignment and the stages cannot disagree.
//!
//! Pure: no I/O, no clock.

/// 100 ns ticks per millisecond. Playback positions are ticks; the queue store saves milliseconds.
pub const TICKS_PER_MS: u64 = 10_000;

/// Where the resume position is in its short life.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ResumePosition {
    /// No saved position, or it was already used.
    #[default]
    None,
    /// A saved position in milliseconds. The Resume button is shown while this is set.
    Available { ms: u64 },
    /// The viewer pressed Resume; the position is parked in ticks until playback starts. The button
    /// is hidden in this stage, exactly as it was when the old code cleared the milliseconds field.
    Parked { ticks: i64 },
}

impl ResumePosition {
    /// Record a saved position from the queue store.
    pub fn available(ms: u64) -> Self {
        Self::Available { ms }
    }

    /// Whether the Resume button should be offered.
    pub fn is_available(&self) -> bool {
        matches!(self, Self::Available { .. })
    }

    /// Whether a position is parked for the next playback start.
    pub fn is_parked(&self) -> bool {
        matches!(self, Self::Parked { .. })
    }

    /// The saved position in milliseconds, while it is still available.
    pub fn ms(&self) -> Option<u64> {
        match self {
            Self::Available { ms } => Some(*ms),
            Self::None | Self::Parked { .. } => None,
        }
    }

    /// Move from "available" to "parked": this is what pressing Resume does. The milliseconds value
    /// is consumed, so the button disappears immediately.
    ///
    /// A value that cannot be represented in ticks is dropped rather than clamped, matching the
    /// previous `i64::try_from` behavior: an absurd saved position starts the track at the beginning
    /// instead of at a wrong offset.
    pub fn park(&mut self) {
        if let Self::Available { ms } = *self {
            *self = match i64::try_from(ms.saturating_mul(TICKS_PER_MS)) {
                Ok(ticks) => Self::Parked { ticks },
                Err(_) => Self::None,
            };
        }
    }

    /// Take the parked ticks for a playback start, leaving nothing behind.
    pub fn take_ticks(&mut self) -> Option<i64> {
        match core::mem::take(self) {
            Self::Parked { ticks } => Some(ticks),
            other => {
                *self = other;
                None
            }
        }
    }

    /// Forget any position. Every clear site uses this.
    pub fn clear(&mut self) {
        *self = Self::None;
    }
}

#[cfg(test)]
mod tests {
    use super::{ResumePosition, TICKS_PER_MS};

    #[test]
    fn a_fresh_value_is_none_and_offers_no_button() {
        let value = ResumePosition::default();
        assert_eq!(value, ResumePosition::None);
        assert!(!value.is_available());
        assert!(!value.is_parked());
        assert_eq!(value.ms(), None);
    }

    #[test]
    fn a_saved_position_is_available_and_exposes_milliseconds() {
        let value = ResumePosition::available(12_345);
        assert!(value.is_available());
        assert_eq!(value.ms(), Some(12_345));
        assert!(!value.is_parked());
    }

    #[test]
    fn parking_converts_to_ticks_and_hides_the_button() {
        let mut value = ResumePosition::available(12_345);
        value.park();
        assert!(value.is_parked());
        assert!(
            !value.is_available(),
            "the button hides once Resume is pressed"
        );
        assert_eq!(value.ms(), None);
        assert_eq!(
            value,
            ResumePosition::Parked {
                ticks: 12_345 * TICKS_PER_MS as i64
            }
        );
    }

    #[test]
    fn parked_ticks_are_taken_once() {
        let mut value = ResumePosition::available(1_000);
        value.park();
        assert_eq!(value.take_ticks(), Some(1_000 * TICKS_PER_MS as i64));
        assert_eq!(value, ResumePosition::None);
        assert_eq!(value.take_ticks(), None, "a second start gets no position");
    }

    #[test]
    fn taking_ticks_from_an_available_value_returns_nothing_and_keeps_it() {
        let mut value = ResumePosition::available(500);
        assert_eq!(value.take_ticks(), None);
        assert!(value.is_available(), "an unpressed Resume is not consumed");
    }

    #[test]
    fn clearing_works_from_every_stage() {
        for mut value in [
            ResumePosition::None,
            ResumePosition::available(10),
            ResumePosition::Parked { ticks: 10 },
        ] {
            value.clear();
            assert_eq!(value, ResumePosition::None);
        }
    }

    #[test]
    fn an_unrepresentable_position_is_dropped_rather_than_clamped() {
        let mut value = ResumePosition::available(u64::MAX);
        value.park();
        assert_eq!(
            value,
            ResumePosition::None,
            "an absurd saved offset starts at the beginning instead of a wrong one"
        );
    }

    #[test]
    fn parking_twice_does_not_double_convert() {
        let mut value = ResumePosition::available(2_000);
        value.park();
        let parked = value;
        value.park();
        assert_eq!(value, parked, "parking a parked value changes nothing");
    }
}
