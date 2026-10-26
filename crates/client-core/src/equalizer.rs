//! Ten-band equalizer model. Pure: it validates gains and formats an mpv audio filter string.
//! It does no I/O and makes no mpv calls.
//!
//! Each band holds a gain in decibels within [`GAIN_MIN_DB`]..=[`GAIN_MAX_DB`]. The filter
//! string uses the ffmpeg `equalizer` filter, one peaking band per center frequency, wrapped in
//! `lavfi` so it can be handed to mpv's `af` property.

use std::fmt;

/// Number of equalizer bands.
pub const BAND_COUNT: usize = 10;

/// Center frequency of each band, in hertz, from lowest to highest.
pub const BAND_FREQUENCIES_HZ: [u32; BAND_COUNT] =
    [31, 62, 125, 250, 500, 1_000, 2_000, 4_000, 8_000, 16_000];

/// Lowest gain a band accepts, in decibels.
pub const GAIN_MIN_DB: f64 = -12.0;

/// Highest gain a band accepts, in decibels.
pub const GAIN_MAX_DB: f64 = 12.0;

/// Gains of the flat preset, in band order.
pub const PRESET_FLAT: [f64; BAND_COUNT] = [0.0; BAND_COUNT];

/// Gains of the bass boost preset, in band order.
pub const PRESET_BASS_BOOST: [f64; BAND_COUNT] = [6.0, 5.0, 4.0, 2.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0];

/// Gains of the treble boost preset, in band order.
pub const PRESET_TREBLE_BOOST: [f64; BAND_COUNT] =
    [0.0, 0.0, 0.0, 0.0, 0.0, 1.0, 2.0, 4.0, 5.0, 6.0];

/// Gains of the voice preset, in band order. Lifts the speech range and trims the low end.
pub const PRESET_VOICE: [f64; BAND_COUNT] = [-2.0, -2.0, -1.0, 0.0, 2.0, 3.0, 4.0, 3.0, 1.0, 0.0];

/// A named set of band gains.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Preset {
    Flat,
    BassBoost,
    TrebleBoost,
    Voice,
}

impl Preset {
    /// The band gains this preset applies.
    pub const fn gains(self) -> [f64; BAND_COUNT] {
        match self {
            Self::Flat => PRESET_FLAT,
            Self::BassBoost => PRESET_BASS_BOOST,
            Self::TrebleBoost => PRESET_TREBLE_BOOST,
            Self::Voice => PRESET_VOICE,
        }
    }
}

/// Why a band index or gain was refused. Carries the index only, never the gain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EqualizerError {
    /// The band index is not below [`BAND_COUNT`].
    BandOutOfRange { index: usize },
    /// The gain is not finite or is outside [`GAIN_MIN_DB`]..=[`GAIN_MAX_DB`].
    GainOutOfRange { index: usize },
}

impl fmt::Display for EqualizerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BandOutOfRange { index } => {
                write!(f, "equalizer band {index} does not exist")
            }
            Self::GainOutOfRange { index } => {
                write!(
                    f,
                    "equalizer band {index} gain must be a number from -12 to 12 dB"
                )
            }
        }
    }
}

impl std::error::Error for EqualizerError {}

/// Whether a gain is a finite number inside the accepted range.
pub fn is_valid_gain(gain: f64) -> bool {
    gain.is_finite() && (GAIN_MIN_DB..=GAIN_MAX_DB).contains(&gain)
}

/// Turn negative zero into zero so the filter string never reads `g=-0`.
fn canonical(gain: f64) -> f64 {
    if gain == 0.0 { 0.0 } else { gain }
}

/// A ten-band equalizer. Every stored gain is valid.
#[derive(Clone, Debug, PartialEq)]
pub struct Equalizer {
    gains: [f64; BAND_COUNT],
}

impl Default for Equalizer {
    fn default() -> Self {
        Self::flat()
    }
}

impl Equalizer {
    /// An equalizer with every band at 0 dB.
    pub const fn flat() -> Self {
        Self { gains: PRESET_FLAT }
    }

    /// Build an equalizer from all ten gains. Refuses the whole set when any gain is invalid.
    pub fn from_gains(gains: [f64; BAND_COUNT]) -> Result<Self, EqualizerError> {
        for (index, gain) in gains.iter().enumerate() {
            if !is_valid_gain(*gain) {
                return Err(EqualizerError::GainOutOfRange { index });
            }
        }
        Ok(Self {
            gains: gains.map(canonical),
        })
    }

    /// The gain of one band, or `None` when the index is out of range.
    pub fn gain(&self, index: usize) -> Option<f64> {
        self.gains.get(index).copied()
    }

    /// All ten gains, in band order.
    pub fn gains(&self) -> [f64; BAND_COUNT] {
        self.gains
    }

    /// Set one band. A refused call leaves the equalizer unchanged.
    pub fn set_band(&mut self, index: usize, gain: f64) -> Result<(), EqualizerError> {
        if index >= BAND_COUNT {
            return Err(EqualizerError::BandOutOfRange { index });
        }
        if !is_valid_gain(gain) {
            return Err(EqualizerError::GainOutOfRange { index });
        }
        self.gains[index] = canonical(gain);
        Ok(())
    }

    /// Replace every band with the gains of a preset.
    pub fn apply_preset(&mut self, preset: Preset) {
        self.gains = preset.gains().map(canonical);
    }

    /// Whether every band is at 0 dB.
    pub fn is_flat(&self) -> bool {
        self.gains.iter().all(|gain| *gain == 0.0)
    }

    /// The mpv audio filter string for these gains, for example
    /// `lavfi=[equalizer=f=31:width_type=o:width=1:g=3,...]`.
    ///
    /// Returns `None` when every band is at 0 dB, so the caller can remove the filter instead
    /// of adding a no-op.
    pub fn to_af_filter(&self) -> Option<String> {
        self.to_filter_graph()
            .map(|bands| format!("lavfi=[{bands}]"))
    }

    /// The band filters on their own, without the `lavfi=[...]` wrapper, so a caller can put
    /// them in one graph with other filters. `None` when every band is at 0 dB.
    pub fn to_filter_graph(&self) -> Option<String> {
        if self.is_flat() {
            return None;
        }
        Some(
            BAND_FREQUENCIES_HZ
                .iter()
                .zip(self.gains.iter())
                .map(|(hz, gain)| format!("equalizer=f={hz}:width_type=o:width=1:g={gain}"))
                .collect::<Vec<_>>()
                .join(","),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn band_frequencies_are_the_ten_octave_centers() {
        assert_eq!(BAND_FREQUENCIES_HZ.len(), BAND_COUNT);
        assert_eq!(BAND_FREQUENCIES_HZ[0], 31);
        assert_eq!(BAND_FREQUENCIES_HZ[5], 1_000);
        assert_eq!(BAND_FREQUENCIES_HZ[BAND_COUNT - 1], 16_000);
        assert!(BAND_FREQUENCIES_HZ.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn gain_bounds_are_inclusive() {
        assert!(is_valid_gain(GAIN_MIN_DB));
        assert!(is_valid_gain(GAIN_MAX_DB));
        assert!(is_valid_gain(0.0));
        assert!(is_valid_gain(-2.5));
        assert!(!is_valid_gain(-12.5));
        assert!(!is_valid_gain(12.5));
        assert!(!is_valid_gain(f64::NAN));
        assert!(!is_valid_gain(f64::INFINITY));
        assert!(!is_valid_gain(f64::NEG_INFINITY));
    }

    #[test]
    fn from_gains_accepts_the_limits_and_refuses_one_band_past_them() {
        let at_limits = [
            GAIN_MAX_DB,
            GAIN_MIN_DB,
            0.0,
            0.0,
            0.0,
            0.0,
            0.0,
            0.0,
            0.0,
            GAIN_MAX_DB,
        ];
        assert!(Equalizer::from_gains(at_limits).is_ok());

        let mut too_high = [0.0; BAND_COUNT];
        too_high[7] = 12.5;
        assert_eq!(
            Equalizer::from_gains(too_high),
            Err(EqualizerError::GainOutOfRange { index: 7 })
        );

        let mut not_a_number = [0.0; BAND_COUNT];
        not_a_number[2] = f64::NAN;
        assert_eq!(
            Equalizer::from_gains(not_a_number),
            Err(EqualizerError::GainOutOfRange { index: 2 })
        );
    }

    #[test]
    fn set_band_refuses_a_bad_index_and_leaves_the_state_alone() {
        let mut eq = Equalizer::flat();
        assert_eq!(
            eq.set_band(BAND_COUNT, 3.0),
            Err(EqualizerError::BandOutOfRange { index: BAND_COUNT })
        );
        assert!(eq.is_flat());
    }

    #[test]
    fn set_band_refuses_an_out_of_range_gain_and_keeps_the_old_value() {
        let mut eq = Equalizer::flat();
        eq.set_band(4, 5.0).expect("band 4 accepts 5 dB");
        assert_eq!(
            eq.set_band(4, 13.0),
            Err(EqualizerError::GainOutOfRange { index: 4 })
        );
        assert_eq!(eq.gain(4), Some(5.0));
    }

    #[test]
    fn gain_lookup_returns_none_past_the_last_band() {
        let eq = Equalizer::flat();
        assert_eq!(eq.gain(0), Some(0.0));
        assert_eq!(eq.gain(BAND_COUNT - 1), Some(0.0));
        assert_eq!(eq.gain(BAND_COUNT), None);
    }

    #[test]
    fn preset_values_match_the_documented_curves() {
        assert_eq!(Preset::Flat.gains(), [0.0; BAND_COUNT]);
        assert_eq!(
            Preset::BassBoost.gains(),
            [6.0, 5.0, 4.0, 2.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0]
        );
        assert_eq!(
            Preset::TrebleBoost.gains(),
            [0.0, 0.0, 0.0, 0.0, 0.0, 1.0, 2.0, 4.0, 5.0, 6.0]
        );
        assert_eq!(
            Preset::Voice.gains(),
            [-2.0, -2.0, -1.0, 0.0, 2.0, 3.0, 4.0, 3.0, 1.0, 0.0]
        );
    }

    #[test]
    fn every_preset_gain_is_inside_the_bounds() {
        for preset in [
            Preset::Flat,
            Preset::BassBoost,
            Preset::TrebleBoost,
            Preset::Voice,
        ] {
            assert!(
                Equalizer::from_gains(preset.gains()).is_ok(),
                "{preset:?} has a gain outside the bounds"
            );
        }
    }

    #[test]
    fn apply_preset_replaces_every_band() {
        let mut eq = Equalizer::flat();
        eq.apply_preset(Preset::TrebleBoost);
        assert_eq!(eq.gains(), PRESET_TREBLE_BOOST);
        assert!(!eq.is_flat());
        eq.apply_preset(Preset::Flat);
        assert!(eq.is_flat());
    }

    #[test]
    fn filter_string_for_known_gains_is_exact() {
        let mut gains = [0.0; BAND_COUNT];
        gains[0] = 3.0;
        gains[BAND_COUNT - 1] = -2.5;
        let eq = Equalizer::from_gains(gains).expect("gains are in range");
        assert_eq!(
            eq.to_af_filter().as_deref(),
            Some(
                "lavfi=[equalizer=f=31:width_type=o:width=1:g=3,\
                 equalizer=f=62:width_type=o:width=1:g=0,\
                 equalizer=f=125:width_type=o:width=1:g=0,\
                 equalizer=f=250:width_type=o:width=1:g=0,\
                 equalizer=f=500:width_type=o:width=1:g=0,\
                 equalizer=f=1000:width_type=o:width=1:g=0,\
                 equalizer=f=2000:width_type=o:width=1:g=0,\
                 equalizer=f=4000:width_type=o:width=1:g=0,\
                 equalizer=f=8000:width_type=o:width=1:g=0,\
                 equalizer=f=16000:width_type=o:width=1:g=-2.5]"
            )
        );
    }

    #[test]
    fn filter_string_has_one_band_per_frequency() {
        let mut eq = Equalizer::flat();
        eq.apply_preset(Preset::BassBoost);
        let filter = eq.to_af_filter().expect("bass boost is not flat");
        assert_eq!(filter.matches("equalizer=f=").count(), BAND_COUNT);
        assert!(filter.starts_with("lavfi=[equalizer=f=31:"));
        assert!(filter.ends_with("equalizer=f=16000:width_type=o:width=1:g=0]"));
    }

    #[test]
    fn all_flat_gains_produce_no_filter() {
        assert_eq!(Equalizer::flat().to_af_filter(), None);
        assert_eq!(Equalizer::default().to_af_filter(), None);

        let mut eq = Equalizer::flat();
        eq.apply_preset(Preset::Flat);
        assert_eq!(eq.to_af_filter(), None);

        eq.set_band(4, 4.0).expect("band 4 accepts 4 dB");
        assert!(eq.to_af_filter().is_some());
        eq.set_band(4, 0.0).expect("band 4 accepts 0 dB");
        assert_eq!(eq.to_af_filter(), None);
    }

    #[test]
    fn negative_zero_reads_as_flat_and_never_appears_in_the_filter() {
        let mut gains = [0.0; BAND_COUNT];
        gains[3] = -0.0;
        let eq = Equalizer::from_gains(gains).expect("negative zero is in range");
        assert!(eq.is_flat());
        assert_eq!(eq.to_af_filter(), None);

        let mut eq = Equalizer::flat();
        eq.set_band(0, -0.0).expect("band 0 accepts negative zero");
        eq.set_band(1, 1.0).expect("band 1 accepts 1 dB");
        let filter = eq.to_af_filter().expect("one band is raised");
        assert!(!filter.contains("g=-0,"));
    }
}
