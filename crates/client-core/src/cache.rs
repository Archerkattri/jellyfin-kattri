//! Playback buffer limits and their mapping to mpv options (M2-06).
//!
//! Pure logic: clamping, MiB-to-byte conversion, and the mapping onto allowlisted options
//! from [`crate::mpv_options`]. No I/O and no mpv calls. The settings file stores the values;
//! `proofs/mpv-render` applies the result.
//!
//! The persisted settings schema owns the limits so validation and option mapping cannot drift.

use crate::mpv_options::{MpvOptionError, OptionValue, ValidatedOption, validate};
pub use kattri_settings::{BACKBUFFER_MB_MAX, CACHE_MB_MAX, CACHE_MB_MIN, READAHEAD_SECS_MAX};

const BYTES_PER_MIB: u64 = 1024 * 1024;

/// The three buffer values as the settings file stores them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BufferSettings {
    /// MiB, mapped to `demuxer-max-bytes`.
    pub cache_mb: u32,
    /// MiB, mapped to `demuxer-max-back-bytes`.
    pub backbuffer_mb: u32,
    /// Seconds, mapped to `demuxer-readahead-secs`.
    pub readahead_secs: u32,
}

/// Clamp a cache size into `CACHE_MB_MIN..=CACHE_MB_MAX`.
pub fn clamp_cache_mb(value: u32) -> u32 {
    value.clamp(CACHE_MB_MIN, CACHE_MB_MAX)
}

/// Clamp a back-buffer size into `0..=BACKBUFFER_MB_MAX`.
pub fn clamp_backbuffer_mb(value: u32) -> u32 {
    value.min(BACKBUFFER_MB_MAX)
}

/// Clamp a read-ahead into `0..=READAHEAD_SECS_MAX`.
pub fn clamp_readahead_secs(value: u32) -> u32 {
    value.min(READAHEAD_SECS_MAX)
}

/// Convert MiB to bytes. Exact for every `u32` input, because `u32::MAX * 2^20` fits in `u64`.
pub const fn mib_to_bytes(mib: u32) -> u64 {
    mib as u64 * BYTES_PER_MIB
}

/// Map the buffer settings onto the three allowlisted demuxer options.
///
/// Values are clamped first, so the result is always inside the allowlist ranges. The function
/// still returns `Result` because it goes through [`validate`], and a value that somehow fell
/// outside the range is reported rather than sent.
pub fn demuxer_options(settings: BufferSettings) -> Result<Vec<ValidatedOption>, MpvOptionError> {
    let cache_bytes = mib_to_bytes(clamp_cache_mb(settings.cache_mb));
    let backbuffer_bytes = mib_to_bytes(clamp_backbuffer_mb(settings.backbuffer_mb));
    let readahead = clamp_readahead_secs(settings.readahead_secs);

    Ok(vec![
        validate(
            "demuxer-max-bytes",
            OptionValue::Int(i64::try_from(cache_bytes).unwrap_or(i64::MAX)),
        )?,
        validate(
            "demuxer-max-back-bytes",
            OptionValue::Int(i64::try_from(backbuffer_bytes).unwrap_or(i64::MAX)),
        )?,
        validate(
            "demuxer-readahead-secs",
            OptionValue::Int(i64::from(readahead)),
        )?,
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mpv_options::{ValueKind, spec_for};

    #[test]
    fn clamp_pulls_each_value_into_its_range_at_both_ends() {
        assert_eq!(clamp_cache_mb(0), CACHE_MB_MIN);
        assert_eq!(clamp_cache_mb(10), 16, "legacy enum floor is raised to 16");
        assert_eq!(clamp_cache_mb(16), 16);
        assert_eq!(clamp_cache_mb(75), 75);
        assert_eq!(clamp_cache_mb(4096), CACHE_MB_MAX);
        assert_eq!(clamp_cache_mb(4097), CACHE_MB_MAX);
        assert_eq!(clamp_cache_mb(u32::MAX), CACHE_MB_MAX);

        assert_eq!(clamp_backbuffer_mb(0), 0);
        assert_eq!(clamp_backbuffer_mb(1024), 1024);
        assert_eq!(clamp_backbuffer_mb(1025), BACKBUFFER_MB_MAX);

        assert_eq!(clamp_readahead_secs(0), 0);
        assert_eq!(clamp_readahead_secs(600), 600);
        assert_eq!(clamp_readahead_secs(601), READAHEAD_SECS_MAX);
    }

    #[test]
    fn mib_to_bytes_is_exact_for_the_legacy_example_values() {
        assert_eq!(mib_to_bytes(0), 0);
        assert_eq!(mib_to_bytes(16), 16_777_216);
        assert_eq!(mib_to_bytes(50), 52_428_800);
        assert_eq!(mib_to_bytes(75), 78_643_200);
        assert_eq!(mib_to_bytes(4096), 4_294_967_296);
    }

    #[test]
    fn mib_to_bytes_does_not_overflow_for_any_u32() {
        assert_eq!(mib_to_bytes(u32::MAX), u64::from(u32::MAX) * 1_048_576);
    }

    #[test]
    fn demuxer_options_map_legacy_example_values_exactly() {
        let options = demuxer_options(BufferSettings {
            cache_mb: 75,
            backbuffer_mb: 50,
            readahead_secs: 5,
        })
        .unwrap();
        let pairs: Vec<(&str, &str)> = options.iter().map(|o| (o.name, o.value.as_str())).collect();
        assert_eq!(
            pairs,
            vec![
                ("demuxer-max-bytes", "78643200"),
                ("demuxer-max-back-bytes", "52428800"),
                ("demuxer-readahead-secs", "5"),
            ]
        );
    }

    #[test]
    fn demuxer_options_clamp_before_mapping() {
        let options = demuxer_options(BufferSettings {
            cache_mb: 0,
            backbuffer_mb: 5000,
            readahead_secs: 9999,
        })
        .unwrap();
        let values: Vec<&str> = options.iter().map(|o| o.value.as_str()).collect();
        assert_eq!(values, vec!["16777216", "1073741824", "600"]);
    }

    #[test]
    fn demuxer_options_accept_every_bound() {
        for settings in [
            BufferSettings {
                cache_mb: CACHE_MB_MIN,
                backbuffer_mb: 0,
                readahead_secs: 0,
            },
            BufferSettings {
                cache_mb: CACHE_MB_MAX,
                backbuffer_mb: BACKBUFFER_MB_MAX,
                readahead_secs: READAHEAD_SECS_MAX,
            },
        ] {
            assert!(demuxer_options(settings).is_ok(), "{settings:?}");
        }
    }

    #[test]
    fn limits_match_the_allowlist_ranges() {
        let check = |name: &str, min: i64, max: i64| match spec_for(name).map(|s| s.kind) {
            Some(ValueKind::Int {
                min: got_min,
                max: got_max,
            }) => {
                assert_eq!((got_min, got_max), (min, max), "{name}");
            }
            other => panic!("{name} has unexpected kind {other:?}"),
        };
        let mib = |v: u32| i64::try_from(mib_to_bytes(v)).unwrap_or(i64::MAX);
        check("demuxer-max-bytes", mib(CACHE_MB_MIN), mib(CACHE_MB_MAX));
        check("demuxer-max-back-bytes", 0, mib(BACKBUFFER_MB_MAX));
        check("demuxer-readahead-secs", 0, i64::from(READAHEAD_SECS_MAX));
    }

    #[test]
    fn one_mib_past_the_cache_ceiling_is_refused_by_the_allowlist() {
        let over = i64::try_from(mib_to_bytes(CACHE_MB_MAX + 1)).unwrap_or(i64::MAX);
        assert!(matches!(
            validate("demuxer-max-bytes", OptionValue::Int(over)),
            Err(MpvOptionError::OutOfRange { .. })
        ));
    }
}
