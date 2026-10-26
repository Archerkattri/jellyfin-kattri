//! Volume and volume-boost math (M2-16). Pure: no I/O and no mpv calls.
//!
//! The clamp and step rules here decide the value the UI shows. `proofs/mpv-render` sends the
//! result to mpv as `volume` and `volume-max`. Mute is a session value and is not here.

/// Lowest volume, in percent.
pub const VOLUME_MIN_PERCENT: u32 = 0;

/// Volume that means "no boost", in percent.
pub const VOLUME_UNITY_PERCENT: u32 = 100;

/// Largest volume the app allows, in percent.
///
/// Owner decision 2026-10-15: 300. The legacy clamp was 200 (`legacy/qt-neo/src/player/
/// PlayerComponent.cpp:1345`); Neo item #1037 asks for up to 300. `kattri-settings`
/// (`max_volume_percent`) and the `volume-max` allowlist entry in `mpv_options` use the same
/// number and must change with it.
pub const VOLUME_BOOST_MAX_PERCENT: u32 = 300;

/// Values the `audio.max_volume_percent` setting may take (M2-16). Mirrors
/// `kattri_settings::MAX_VOLUME_PERCENT_CHOICES`, which is kept in step by hand.
pub const MAX_VOLUME_PERCENT_CHOICES: [u32; 4] = [100, 150, 200, 300];

/// Change per wheel notch or key press, in percent.
pub const VOLUME_STEP_PERCENT: u32 = 5;

/// Volume above this is a boost. The UI shows the boost indicator and the one-time notice
/// "Above 100% can distort audio".
pub const BOOST_THRESHOLD_PERCENT: u32 = 100;

/// The ceiling that applies for a requested maximum. A request above the boost maximum is cut
/// down to it, so no caller can exceed [`VOLUME_BOOST_MAX_PERCENT`].
pub const fn effective_max(max_percent: u32) -> u32 {
    if max_percent > VOLUME_BOOST_MAX_PERCENT {
        VOLUME_BOOST_MAX_PERCENT
    } else {
        max_percent
    }
}

/// Clamp `volume` into `VOLUME_MIN_PERCENT..=effective_max(max_percent)`.
///
/// Example: a volume of 150 with a maximum of 100 becomes 100. The UI shows this clamped value.
pub fn clamp_volume(volume: u32, max_percent: u32) -> u32 {
    volume.clamp(VOLUME_MIN_PERCENT, effective_max(max_percent))
}

/// Whether a value is one of the allowed `audio.max_volume_percent` choices.
pub fn is_allowed_max(max_percent: u32) -> bool {
    MAX_VOLUME_PERCENT_CHOICES.contains(&max_percent)
}

/// Whether the volume is above 100 percent, so the boost indicator and notice apply.
pub fn is_boost(volume: u32) -> bool {
    volume > BOOST_THRESHOLD_PERCENT
}

/// Apply `notches` volume steps to `volume` and clamp the result.
///
/// Positive notches go up and negative go down. Steps land on multiples of
/// [`VOLUME_STEP_PERCENT`]: from 103, one step up is 105 and one step down is 100. From a value
/// already on a multiple, a step moves by exactly 5. The result never goes below 0 or above
/// `effective_max(max_percent)`. Arithmetic is widened, so very large notch counts do not
/// overflow.
pub fn step(volume: u32, notches: i32, max_percent: u32) -> u32 {
    let limit = effective_max(max_percent);
    let start = volume.min(limit);
    let extra_steps = u64::from(notches.unsigned_abs().saturating_sub(1));
    let step = u64::from(VOLUME_STEP_PERCENT);

    let moved: u64 = match notches.cmp(&0) {
        std::cmp::Ordering::Equal => u64::from(start),
        std::cmp::Ordering::Greater => u64::from(next_multiple_up(start)) + extra_steps * step,
        std::cmp::Ordering::Less => {
            u64::from(next_multiple_down(start)).saturating_sub(extra_steps * step)
        }
    };

    u32::try_from(moved.min(u64::from(limit))).unwrap_or(limit)
}

/// The smallest multiple of the step strictly above `value`.
fn next_multiple_up(value: u32) -> u32 {
    (value / VOLUME_STEP_PERCENT + 1).saturating_mul(VOLUME_STEP_PERCENT)
}

/// The largest multiple of the step strictly below `value`, or 0.
fn next_multiple_down(value: u32) -> u32 {
    if value == 0 {
        return 0;
    }
    if value.is_multiple_of(VOLUME_STEP_PERCENT) {
        value - VOLUME_STEP_PERCENT
    } else {
        value / VOLUME_STEP_PERCENT * VOLUME_STEP_PERCENT
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boost_maximum_is_the_owner_decided_300_percent() {
        // Guards the owner decision of 2026-10-15. Changing this number is a deliberate change.
        assert_eq!(VOLUME_BOOST_MAX_PERCENT, 300);
    }

    #[test]
    fn effective_max_caps_requests_at_the_boost_maximum() {
        assert_eq!(effective_max(100), 100);
        assert_eq!(effective_max(200), 200);
        assert_eq!(effective_max(300), 300);
        assert_eq!(effective_max(301), VOLUME_BOOST_MAX_PERCENT);
        assert_eq!(effective_max(u32::MAX), VOLUME_BOOST_MAX_PERCENT);
    }

    #[test]
    fn clamp_pulls_volume_down_to_the_maximum_and_up_from_zero() {
        assert_eq!(clamp_volume(150, 100), 100, "volume above max is clamped");
        assert_eq!(clamp_volume(150, 200), 150);
        assert_eq!(clamp_volume(250, 300), 250);
        assert_eq!(
            clamp_volume(350, 400),
            300,
            "request above boost max is capped"
        );
        assert_eq!(clamp_volume(0, 200), 0);
        assert_eq!(clamp_volume(u32::MAX, 300), 300);
    }

    #[test]
    fn allowed_max_accepts_only_100_150_200_and_300() {
        assert!(is_allowed_max(100));
        assert!(is_allowed_max(150));
        assert!(is_allowed_max(200));
        assert!(is_allowed_max(300));
        for rejected in [0, 99, 125, 201, 301] {
            assert!(!is_allowed_max(rejected), "{rejected} accepted");
        }
    }

    #[test]
    fn boost_indicator_starts_above_100() {
        assert!(!is_boost(0));
        assert!(!is_boost(100));
        assert!(is_boost(101));
        assert!(is_boost(200));
    }

    #[test]
    fn single_step_up_snaps_to_the_next_multiple_of_five() {
        assert_eq!(step(100, 1, 200), 105);
        assert_eq!(step(103, 1, 200), 105, "off-grid value rounds up");
        assert_eq!(step(105, 1, 200), 110);
        assert_eq!(step(197, 1, 200), 200);
    }

    #[test]
    fn single_step_down_snaps_to_the_previous_multiple_of_five() {
        assert_eq!(step(103, -1, 200), 100, "off-grid value rounds down");
        assert_eq!(step(100, -1, 200), 95);
        assert_eq!(step(5, -1, 200), 0);
        assert_eq!(step(3, -1, 200), 0);
        assert_eq!(step(0, -1, 200), 0);
    }

    #[test]
    fn multiple_notches_move_by_five_each_and_clamp() {
        assert_eq!(step(100, 3, 200), 115);
        assert_eq!(step(100, -3, 200), 85);
        assert_eq!(step(100, 0, 200), 100);
        assert_eq!(step(100, 3, 100), 100, "clamped to the maximum");
        assert_eq!(step(200, 1, 200), 200, "already at the maximum");
    }

    #[test]
    fn huge_notch_counts_do_not_overflow() {
        assert_eq!(step(0, i32::MAX, 200), 200);
        assert_eq!(step(200, i32::MIN, 200), 0);
        assert_eq!(step(u32::MAX, -1, 200), 195);
    }

    #[test]
    fn step_result_is_always_inside_the_range() {
        for volume in (0..=210).step_by(3) {
            for notches in [-7, -1, 0, 1, 7] {
                for max in [100, 150, 200, 300] {
                    let result = step(volume, notches, max);
                    assert!(result <= effective_max(max), "{volume} {notches} {max}");
                }
            }
        }
    }
}
