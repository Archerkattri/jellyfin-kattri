//! Audio processing options: loudness normalization, pitch correction, and dynamic range
//! compression. Pure: it validates the options and builds the mpv property writes for them. It
//! does no I/O and makes no mpv calls.
//!
//! Normalization and compression are ffmpeg filters, so they travel together in one `af`
//! value. mpv replaces the whole filter chain on each write, so the enabled filters must be
//! sent as a single list. Pitch correction is an mpv property rather than a filter. It is
//! always reported, because mpv's own default is on and turning it off has to be written
//! explicitly.

use std::fmt;

/// Lowest loudness target, in LUFS.
pub const LOUDNESS_TARGET_MIN_LUFS: f64 = -30.0;

/// Highest loudness target, in LUFS.
pub const LOUDNESS_TARGET_MAX_LUFS: f64 = -10.0;

/// Loudness target used when normalization has not been configured, in LUFS.
pub const DEFAULT_LOUDNESS_TARGET_LUFS: f64 = -16.0;

/// Maximum true peak the normalizer may produce, in dBTP.
const TRUE_PEAK_DBTP: f64 = -1.5;

/// Loudness range the normalizer aims for, in LU.
const LOUDNESS_RANGE_LU: f64 = 11.0;

/// Dynamic range compressor. Threshold is linear amplitude, so 0.125 is about -18 dBFS.
const COMPRESSOR_FILTER: &str = "acompressor=threshold=0.125:ratio=4:attack=20:release=250";

/// mpv property that holds the audio filter chain.
pub const AF_PROPERTY: &str = "af";

/// mpv property that keeps pitch fixed when playback speed changes.
pub const PITCH_CORRECTION_PROPERTY: &str = "audio-pitch-correction";

/// Why an audio filter option was refused. Carries no value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioFilterError {
    /// The loudness target is not finite or is outside the accepted range.
    LoudnessTargetOutOfRange,
}

impl fmt::Display for AudioFilterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LoudnessTargetOutOfRange => {
                write!(f, "loudness target must be a number from -30 to -10 LUFS")
            }
        }
    }
}

impl std::error::Error for AudioFilterError {}

/// Loudness normalization. The target is kept even while normalization is off, and it is
/// always inside the accepted range.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Normalization {
    enabled: bool,
    target_lufs: f64,
}

impl Normalization {
    /// Build a normalization setting. Refuses a target that is not finite or is outside
    /// [`LOUDNESS_TARGET_MIN_LUFS`]..=[`LOUDNESS_TARGET_MAX_LUFS`].
    pub fn new(enabled: bool, target_lufs: f64) -> Result<Self, AudioFilterError> {
        let in_range = (LOUDNESS_TARGET_MIN_LUFS..=LOUDNESS_TARGET_MAX_LUFS).contains(&target_lufs);
        if !target_lufs.is_finite() || !in_range {
            return Err(AudioFilterError::LoudnessTargetOutOfRange);
        }
        Ok(Self {
            enabled,
            target_lufs,
        })
    }

    /// Whether normalization is applied.
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// The integrated loudness the normalizer aims for, in LUFS.
    pub fn target_lufs(&self) -> f64 {
        self.target_lufs
    }
}

impl Default for Normalization {
    fn default() -> Self {
        Self {
            enabled: false,
            target_lufs: DEFAULT_LOUDNESS_TARGET_LUFS,
        }
    }
}

/// All audio processing options together.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct AudioFilterSettings {
    pub normalization: Normalization,
    pub pitch_correction: bool,
    pub compression: bool,
}

/// One mpv property write: the property name and the value in mpv's spelling.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FilterPair {
    pub name: &'static str,
    pub value: String,
}

/// The `af` value for the enabled filters, or `None` when neither normalization nor
/// compression is on.
///
/// Normalization comes first, then compression. The result is wrapped in `lavfi=[...]`.
pub fn filter_chain(settings: &AudioFilterSettings) -> Option<String> {
    filter_graph(settings).map(|graph| format!("lavfi=[{graph}]"))
}

/// The enabled filters on their own, without the `lavfi=[...]` wrapper, or `None` when neither
/// normalization nor compression is on. Use it to put them in one graph with other filters.
pub fn filter_graph(settings: &AudioFilterSettings) -> Option<String> {
    let mut filters = Vec::new();
    if settings.normalization.enabled {
        filters.push(format!(
            "loudnorm=I={}:TP={TRUE_PEAK_DBTP}:LRA={LOUDNESS_RANGE_LU}",
            settings.normalization.target_lufs
        ));
    }
    if settings.compression {
        filters.push(COMPRESSOR_FILTER.to_owned());
    }
    if filters.is_empty() {
        None
    } else {
        Some(filters.join(","))
    }
}

/// The mpv property writes for these options.
///
/// The `af` pair appears only when a filter is enabled. The pitch correction pair always
/// appears, with `yes` or `no`.
pub fn filter_pairs(settings: &AudioFilterSettings) -> Vec<FilterPair> {
    let mut pairs = Vec::with_capacity(2);
    if let Some(chain) = filter_chain(settings) {
        pairs.push(FilterPair {
            name: AF_PROPERTY,
            value: chain,
        });
    }
    pairs.push(FilterPair {
        name: PITCH_CORRECTION_PROPERTY,
        value: yes_no(settings.pitch_correction).to_owned(),
    });
    pairs
}

const fn yes_no(on: bool) -> &'static str {
    if on { "yes" } else { "no" }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with(
        normalization: Option<f64>,
        pitch_correction: bool,
        compression: bool,
    ) -> AudioFilterSettings {
        let normalization = match normalization {
            Some(target) => Normalization::new(true, target).expect("target is in range"),
            None => Normalization::default(),
        };
        AudioFilterSettings {
            normalization,
            pitch_correction,
            compression,
        }
    }

    fn pair_value(pairs: &[FilterPair], name: &str) -> Option<String> {
        pairs
            .iter()
            .find(|pair| pair.name == name)
            .map(|pair| pair.value.clone())
    }

    #[test]
    fn defaults_are_normalization_off_pitch_on_compression_off() {
        let settings = AudioFilterSettings::default();
        assert!(!settings.normalization.enabled());
        assert_eq!(
            settings.normalization.target_lufs(),
            DEFAULT_LOUDNESS_TARGET_LUFS
        );
        assert!(!settings.pitch_correction);
        assert!(!settings.compression);

        let standard = AudioFilterSettings {
            pitch_correction: true,
            ..AudioFilterSettings::default()
        };
        assert_eq!(filter_chain(&standard), None);
    }

    #[test]
    fn loudness_target_accepts_the_limits_and_refuses_past_them() {
        assert!(Normalization::new(true, LOUDNESS_TARGET_MIN_LUFS).is_ok());
        assert!(Normalization::new(true, LOUDNESS_TARGET_MAX_LUFS).is_ok());
        assert_eq!(
            Normalization::new(true, -30.5),
            Err(AudioFilterError::LoudnessTargetOutOfRange)
        );
        assert_eq!(
            Normalization::new(true, -9.5),
            Err(AudioFilterError::LoudnessTargetOutOfRange)
        );
        assert_eq!(
            Normalization::new(false, f64::NAN),
            Err(AudioFilterError::LoudnessTargetOutOfRange)
        );
        assert_eq!(
            Normalization::new(false, f64::NEG_INFINITY),
            Err(AudioFilterError::LoudnessTargetOutOfRange)
        );
    }

    #[test]
    fn loudness_target_is_checked_even_when_normalization_is_off() {
        assert!(Normalization::new(false, -40.0).is_err());
        let off = Normalization::new(false, -23.0).expect("target is in range");
        assert!(!off.enabled());
        assert_eq!(off.target_lufs(), -23.0);
    }

    #[test]
    fn normalization_on_builds_a_loudnorm_chain_with_the_target() {
        let settings = with(Some(-16.0), true, false);
        assert_eq!(
            filter_chain(&settings).as_deref(),
            Some("lavfi=[loudnorm=I=-16:TP=-1.5:LRA=11]")
        );

        let quieter = with(Some(-23.5), true, false);
        assert_eq!(
            filter_chain(&quieter).as_deref(),
            Some("lavfi=[loudnorm=I=-23.5:TP=-1.5:LRA=11]")
        );
    }

    #[test]
    fn compression_on_builds_a_compressor_chain() {
        let settings = with(None, true, true);
        assert_eq!(
            filter_chain(&settings).as_deref(),
            Some("lavfi=[acompressor=threshold=0.125:ratio=4:attack=20:release=250]")
        );
    }

    #[test]
    fn normalization_and_compression_share_one_chain_in_order() {
        let settings = with(Some(-14.0), false, true);
        assert_eq!(
            filter_chain(&settings).as_deref(),
            Some(
                "lavfi=[loudnorm=I=-14:TP=-1.5:LRA=11,\
                 acompressor=threshold=0.125:ratio=4:attack=20:release=250]"
            )
        );
    }

    #[test]
    fn pitch_correction_alone_adds_no_filter() {
        assert_eq!(filter_chain(&with(None, true, false)), None);
        assert_eq!(filter_chain(&with(None, false, false)), None);
    }

    #[test]
    fn pitch_correction_is_reported_as_yes_or_no() {
        let on = filter_pairs(&with(None, true, false));
        assert_eq!(
            pair_value(&on, PITCH_CORRECTION_PROPERTY).as_deref(),
            Some("yes")
        );

        let off = filter_pairs(&with(None, false, false));
        assert_eq!(
            pair_value(&off, PITCH_CORRECTION_PROPERTY).as_deref(),
            Some("no")
        );
    }

    #[test]
    fn all_options_off_writes_no_filter_chain_and_pitch_off() {
        let settings = AudioFilterSettings::default();
        let pairs = filter_pairs(&AudioFilterSettings {
            pitch_correction: false,
            ..settings
        });
        assert_eq!(
            pairs,
            vec![FilterPair {
                name: PITCH_CORRECTION_PROPERTY,
                value: "no".to_owned(),
            }]
        );
        assert_eq!(pair_value(&pairs, AF_PROPERTY), None);
    }

    #[test]
    fn af_pair_appears_only_when_a_filter_is_enabled() {
        let normalized = filter_pairs(&with(Some(-16.0), true, false));
        assert_eq!(
            pair_value(&normalized, AF_PROPERTY).as_deref(),
            Some("lavfi=[loudnorm=I=-16:TP=-1.5:LRA=11]")
        );

        let compressed = filter_pairs(&with(None, true, true));
        assert_eq!(
            pair_value(&compressed, AF_PROPERTY).as_deref(),
            Some("lavfi=[acompressor=threshold=0.125:ratio=4:attack=20:release=250]")
        );
        assert_eq!(compressed.len(), 2);
    }
}
