//! Button-driven subtitle synchronization calibration.
//!
//! Capture the media position when the viewer hears the spoken line and when
//! its subtitle appears. The resulting signed adjustment is intended to be
//! added to mpv's `sub-delay` property.

use std::time::Duration;

use crate::mpv_options::{MpvOptionError, OptionValue, validate};

/// The sign of a signed duration.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum DurationSign {
    Negative,
    Zero,
    Positive,
}

/// A non-overflowing signed duration represented by a sign and magnitude.
///
/// The fields are private so zero is always represented with [`DurationSign::Zero`]
/// and a zero magnitude. No fixed subtitle-delay range is imposed.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct SignedDuration {
    sign: DurationSign,
    magnitude: Duration,
}

impl SignedDuration {
    /// Construct a signed duration from two nonnegative media timestamps.
    /// The result is `positive - negative`, without converting through a
    /// potentially overflowing integer or floating-point representation.
    pub fn difference(positive: Duration, negative: Duration) -> Self {
        match positive.cmp(&negative) {
            std::cmp::Ordering::Less => Self {
                sign: DurationSign::Negative,
                magnitude: negative - positive,
            },
            std::cmp::Ordering::Equal => Self::ZERO,
            std::cmp::Ordering::Greater => Self {
                sign: DurationSign::Positive,
                magnitude: positive - negative,
            },
        }
    }

    pub const ZERO: Self = Self {
        sign: DurationSign::Zero,
        magnitude: Duration::ZERO,
    };

    pub const fn sign(self) -> DurationSign {
        self.sign
    }

    pub const fn magnitude(self) -> Duration {
        self.magnitude
    }

    /// Return the signed value in seconds, suitable for mpv's `sub-delay`.
    ///
    /// The conversion is finite for every `Duration`; the magnitude is far
    /// below `f64::MAX` even at the largest representable duration.
    pub fn as_seconds_f64(self) -> f64 {
        let magnitude = self.magnitude.as_secs_f64();
        match self.sign {
            DurationSign::Negative => -magnitude,
            DurationSign::Zero => 0.0,
            DurationSign::Positive => magnitude,
        }
    }
}

/// Which calibration marker the viewer is recording.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum CalibrationMarker {
    VoiceHeard,
    TextSeen,
}

/// Result of recording a marker.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CaptureOutcome {
    /// One marker is stored; tap the other marker to complete the calibration.
    AwaitingOtherMarker {
        captured: CalibrationMarker,
        /// True when this tap replaced an earlier tap of the same marker.
        /// Replacing re-arms the pair around the latest timestamp.
        replaced_previous: bool,
    },
    /// Both markers were captured. The pending pair has been cleared and is
    /// ready for another calibration.
    Calibrated(SignedDuration),
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct PendingMarkers {
    voice_heard: Option<Duration>,
    text_seen: Option<Duration>,
}

/// Collects the two VLC-style “Voice heard” / “Text seen” taps.
///
/// Markers may arrive in either order. A duplicate marker replaces its
/// previous timestamp and keeps the calibration armed for the other marker.
/// Once both are present, the pair is consumed and the model resets itself.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SubtitleSyncCalibration {
    pending: Option<PendingMarkers>,
}

impl SubtitleSyncCalibration {
    pub const fn new() -> Self {
        Self { pending: None }
    }

    /// Record the media time at which the viewer hears the voice.
    pub fn capture_voice_heard(&mut self, media_time: Duration) -> CaptureOutcome {
        self.capture(CalibrationMarker::VoiceHeard, media_time)
    }

    /// Record the media time at which the corresponding text appears.
    pub fn capture_text_seen(&mut self, media_time: Duration) -> CaptureOutcome {
        self.capture(CalibrationMarker::TextSeen, media_time)
    }

    /// Discard any partial pair. Returns true if a pending marker was cleared.
    pub fn reset(&mut self) -> bool {
        self.pending.take().is_some()
    }

    pub const fn is_pending(&self) -> bool {
        self.pending.is_some()
    }

    fn capture(&mut self, marker: CalibrationMarker, media_time: Duration) -> CaptureOutcome {
        let pending = self.pending.get_or_insert_with(PendingMarkers::default);
        let slot = match marker {
            CalibrationMarker::VoiceHeard => &mut pending.voice_heard,
            CalibrationMarker::TextSeen => &mut pending.text_seen,
        };
        let replaced_previous = slot.replace(media_time).is_some();

        if let (Some(voice_time), Some(text_time)) = (pending.voice_heard, pending.text_seen) {
            self.pending = None;
            // mpv's positive sub-delay moves subtitles later. Therefore the
            // correction is voice_time - text_time: text seen after speech
            // produces a negative adjustment (advance the subtitles).
            CaptureOutcome::Calibrated(SignedDuration::difference(voice_time, text_time))
        } else {
            CaptureOutcome::AwaitingOtherMarker {
                captured: marker,
                replaced_previous,
            }
        }
    }
}

/// Largest subtitle offset in either direction, in milliseconds (M2-11). Product choice.
pub const SUB_OFFSET_LIMIT_MS: i32 = 5_000;

/// Size of one `[` / `]` nudge, in milliseconds (M2-11 requirement 2).
pub const FINE_STEP_MS: i32 = 50;

/// Size of one Shift plus `[` / `]` nudge, in milliseconds (M2-11 requirement 2).
pub const COARSE_STEP_MS: i32 = 500;

/// Direction of a nudge. `Later` increases the offset, which moves subtitles later.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum OffsetDirection {
    Earlier,
    Later,
}

/// Step size of a nudge.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum OffsetStep {
    Fine,
    Coarse,
}

/// Subtitle timing offset for the current item, in whole milliseconds.
///
/// Sign convention matches mpv's `sub-delay`: a positive value delays subtitles
/// (they appear later) and a negative value advances them. The value is always
/// inside `-SUB_OFFSET_LIMIT_MS..=SUB_OFFSET_LIMIT_MS`. The offset is in memory
/// only: it is never persisted, and it returns to zero on stop and on item change.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct SubtitleOffset {
    millis: i32,
}

impl SubtitleOffset {
    pub const ZERO: Self = Self { millis: 0 };

    pub const fn millis(self) -> i32 {
        self.millis
    }

    pub const fn is_zero(self) -> bool {
        self.millis == 0
    }

    /// Build an offset from any millisecond value, clamped to the allowed range.
    pub fn from_millis_clamped(millis: i64) -> Self {
        let limit = i64::from(SUB_OFFSET_LIMIT_MS);
        let clamped = millis.clamp(-limit, limit);
        Self {
            millis: i32::try_from(clamped).unwrap_or(SUB_OFFSET_LIMIT_MS),
        }
    }

    /// Move the offset by one step in `direction`. The result is clamped to the limit,
    /// so repeated nudges at the limit are no-ops.
    pub fn nudge(self, direction: OffsetDirection, step: OffsetStep) -> Self {
        let size = match step {
            OffsetStep::Fine => FINE_STEP_MS,
            OffsetStep::Coarse => COARSE_STEP_MS,
        };
        let delta = match direction {
            OffsetDirection::Earlier => -size,
            OffsetDirection::Later => size,
        };
        Self::from_millis_clamped(i64::from(self.millis) + i64::from(delta))
    }

    /// Convert a calibration result into an offset. The fractional millisecond is truncated
    /// toward zero, and the result is clamped to the limit.
    pub fn from_calibration(calibration: SignedDuration) -> Self {
        let magnitude = i64::try_from(calibration.magnitude().as_millis()).unwrap_or(i64::MAX);
        let signed = match calibration.sign() {
            DurationSign::Negative => -magnitude,
            DurationSign::Zero => 0,
            DurationSign::Positive => magnitude,
        };
        Self::from_millis_clamped(signed)
    }

    /// Return to zero. Call on playback stop and before the next item loads.
    pub const fn reset(self) -> Self {
        Self::ZERO
    }

    /// Value for mpv's `sub-delay` property, in seconds.
    ///
    /// The value is checked by `mpv_options::validate` against the `sub-delay` allowlist entry.
    /// Only a value the allowlist accepts is returned, so a caller cannot send an offset that
    /// bypasses the allowlist.
    pub fn as_mpv_seconds(self) -> Result<f64, MpvOptionError> {
        let seconds = f64::from(self.millis) / 1000.0;
        validate("sub-delay", OptionValue::Float(seconds))?;
        Ok(seconds)
    }

    /// Text for the OSD while adjusting, for example `+50 ms`, `-500 ms`, or `0 ms`.
    pub fn osd_text(self) -> String {
        if self.millis == 0 {
            "0 ms".to_owned()
        } else {
            format!("{:+} ms", self.millis)
        }
    }
}

#[cfg(test)]
mod offset_tests {
    use super::*;

    fn ms(value: u64) -> Duration {
        Duration::from_millis(value)
    }

    #[test]
    fn fine_and_coarse_steps_move_by_their_size() {
        let up = SubtitleOffset::ZERO.nudge(OffsetDirection::Later, OffsetStep::Fine);
        assert_eq!(up.millis(), 50);
        let down = SubtitleOffset::ZERO.nudge(OffsetDirection::Earlier, OffsetStep::Fine);
        assert_eq!(down.millis(), -50);
        let coarse = SubtitleOffset::ZERO.nudge(OffsetDirection::Later, OffsetStep::Coarse);
        assert_eq!(coarse.millis(), 500);
        let coarse_down = SubtitleOffset::ZERO.nudge(OffsetDirection::Earlier, OffsetStep::Coarse);
        assert_eq!(coarse_down.millis(), -500);
    }

    #[test]
    fn nudges_clamp_at_the_positive_limit() {
        let near = SubtitleOffset::from_millis_clamped(4_990);
        let at_limit = near.nudge(OffsetDirection::Later, OffsetStep::Fine);
        assert_eq!(at_limit.millis(), SUB_OFFSET_LIMIT_MS);
        let still = at_limit.nudge(OffsetDirection::Later, OffsetStep::Coarse);
        assert_eq!(still.millis(), SUB_OFFSET_LIMIT_MS);
    }

    #[test]
    fn nudges_clamp_at_the_negative_limit() {
        let near = SubtitleOffset::from_millis_clamped(-4_990);
        let at_limit = near.nudge(OffsetDirection::Earlier, OffsetStep::Fine);
        assert_eq!(at_limit.millis(), -SUB_OFFSET_LIMIT_MS);
        let still = at_limit.nudge(OffsetDirection::Earlier, OffsetStep::Coarse);
        assert_eq!(still.millis(), -SUB_OFFSET_LIMIT_MS);
    }

    #[test]
    fn many_nudges_never_leave_the_range() {
        let mut offset = SubtitleOffset::ZERO;
        for index in 0..1_000 {
            let direction = if index % 3 == 0 {
                OffsetDirection::Earlier
            } else {
                OffsetDirection::Later
            };
            let step = if index % 2 == 0 {
                OffsetStep::Coarse
            } else {
                OffsetStep::Fine
            };
            offset = offset.nudge(direction, step);
            assert!((-SUB_OFFSET_LIMIT_MS..=SUB_OFFSET_LIMIT_MS).contains(&offset.millis()));
        }
    }

    #[test]
    fn extreme_inputs_are_clamped_without_overflow() {
        assert_eq!(
            SubtitleOffset::from_millis_clamped(i64::MAX).millis(),
            SUB_OFFSET_LIMIT_MS
        );
        assert_eq!(
            SubtitleOffset::from_millis_clamped(i64::MIN).millis(),
            -SUB_OFFSET_LIMIT_MS
        );
        let calibrated = SubtitleOffset::from_calibration(SignedDuration::difference(
            Duration::MAX,
            Duration::ZERO,
        ));
        assert_eq!(calibrated.millis(), SUB_OFFSET_LIMIT_MS);
    }

    #[test]
    fn calibration_sign_matches_mpv_convention() {
        // Voice at 10.0 s, text appears at 10.5 s: subtitles are late, so advance them.
        let late_text = SignedDuration::difference(ms(10_000), ms(10_500));
        assert_eq!(SubtitleOffset::from_calibration(late_text).millis(), -500);
        // Voice at 10.5 s, text at 10.0 s: subtitles are early, so delay them.
        let early_text = SignedDuration::difference(ms(10_500), ms(10_000));
        assert_eq!(SubtitleOffset::from_calibration(early_text).millis(), 500);
        assert_eq!(
            SubtitleOffset::from_calibration(SignedDuration::ZERO),
            SubtitleOffset::ZERO
        );
    }

    #[test]
    fn calibration_truncates_sub_millisecond_remainder() {
        let fraction = SignedDuration::difference(Duration::from_micros(1_999), Duration::ZERO);
        assert_eq!(SubtitleOffset::from_calibration(fraction).millis(), 1);
    }

    #[test]
    fn reset_returns_to_zero_from_any_value() {
        let offset = SubtitleOffset::from_millis_clamped(-1_234);
        assert!(!offset.is_zero());
        assert_eq!(offset.reset(), SubtitleOffset::ZERO);
        assert!(offset.reset().is_zero());
    }

    #[test]
    fn mpv_seconds_use_the_same_sign_as_the_millisecond_value() {
        assert_eq!(
            SubtitleOffset::from_millis_clamped(1_500)
                .as_mpv_seconds()
                .ok(),
            Some(1.5)
        );
        assert_eq!(
            SubtitleOffset::from_millis_clamped(-250)
                .as_mpv_seconds()
                .ok(),
            Some(-0.25)
        );
        assert_eq!(SubtitleOffset::ZERO.as_mpv_seconds().ok(), Some(0.0));
    }

    #[test]
    fn mpv_seconds_pass_the_sub_delay_allowlist_at_both_limits() {
        let limit = i64::from(SUB_OFFSET_LIMIT_MS);
        assert_eq!(
            SubtitleOffset::from_millis_clamped(limit)
                .as_mpv_seconds()
                .ok(),
            Some(5.0)
        );
        assert_eq!(
            SubtitleOffset::from_millis_clamped(-limit)
                .as_mpv_seconds()
                .ok(),
            Some(-5.0)
        );
        // Every offset the nudge path can reach passes the allowlist.
        let mut offset = SubtitleOffset::ZERO;
        for _ in 0..200 {
            offset = offset.nudge(OffsetDirection::Later, OffsetStep::Coarse);
            assert!(offset.as_mpv_seconds().is_ok());
        }
    }

    #[test]
    fn mpv_seconds_refuse_an_offset_outside_the_allowlist() {
        // The field is private, so this builds the out-of-range value the clamp prevents. It
        // proves the allowlist gate is in the path, not only the clamp.
        let past_limit = SubtitleOffset {
            millis: SUB_OFFSET_LIMIT_MS + 1,
        };
        assert!(matches!(
            past_limit.as_mpv_seconds(),
            Err(MpvOptionError::OutOfRange { name: "sub-delay" })
        ));
    }

    #[test]
    fn osd_text_shows_signed_milliseconds() {
        assert_eq!(SubtitleOffset::ZERO.osd_text(), "0 ms");
        assert_eq!(SubtitleOffset::from_millis_clamped(50).osd_text(), "+50 ms");
        assert_eq!(
            SubtitleOffset::from_millis_clamped(-500).osd_text(),
            "-500 ms"
        );
    }
}
