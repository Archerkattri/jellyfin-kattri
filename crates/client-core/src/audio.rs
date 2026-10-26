//! Pure audio passthrough configuration and fallback decisions.

use kattri_settings::{AudioOutput, AudioSettings};

use crate::mpv_options::{MpvOptionError, OptionValue, ValidatedOption, validate};

type CodecSetting = (&'static str, fn(&AudioSettings) -> bool);

const SPDIF_CODECS: &[CodecSetting] = &[
    ("ac3", |settings| settings.passthrough.ac3),
    ("dts", |settings| settings.passthrough.dts),
];

const HDMI_CODECS: &[CodecSetting] = &[
    ("ac3", |settings| settings.passthrough.ac3),
    ("dts", |settings| settings.passthrough.dts),
    ("eac3", |settings| settings.passthrough.eac3),
    ("dts-hd", |settings| settings.passthrough.dts_hd),
    ("truehd", |settings| settings.passthrough.truehd),
];

/// Build the ordered mpv codec list from the persisted output and passthrough settings.
/// The returned value is intended to pass through the `audio-spdif` option allowlist.
pub fn audio_spdif_value(settings: &AudioSettings) -> String {
    let codecs = match settings.output {
        AudioOutput::Basic => return String::new(),
        AudioOutput::Spdif => SPDIF_CODECS,
        AudioOutput::Hdmi => HDMI_CODECS,
    };

    let dts_hd_enabled = settings.output == AudioOutput::Hdmi && settings.passthrough.dts_hd;
    codecs
        .iter()
        .filter(|(codec, enabled)| enabled(settings) && !(*codec == "dts" && dts_hd_enabled))
        .map(|(codec, _)| *codec)
        .collect::<Vec<_>>()
        .join(",")
}

/// Build and validate the settings-owned `audio-spdif` option.
pub fn audio_spdif_option(settings: &AudioSettings) -> Result<ValidatedOption, MpvOptionError> {
    let value = audio_spdif_value(settings);
    validate("audio-spdif", OptionValue::Text(&value))
}

/// Build the allowlisted `gapless-audio` value for the persisted setting.
pub fn gapless_audio_option(enabled: bool) -> Result<ValidatedOption, MpvOptionError> {
    validate("gapless-audio", OptionValue::Flag(enabled))
}

/// Whether an observed audio codec is one of the codecs this client can request as passthrough.
pub fn is_passthrough_codec(codec: &str) -> bool {
    matches!(codec, "ac3" | "eac3" | "dts" | "dts-hd" | "truehd")
}

/// The UI notice shown when a requested bitstream is not actually being passed through.
pub const PCM_FALLBACK_NOTICE: &str = "Passthrough not available for this device; using PCM";

/// Decide whether the current item should show the non-blocking PCM fallback notice.
/// Unknown observations do not trigger a false warning; a known empty device list does.
pub fn pcm_fallback_notice(
    passthrough_enabled_for_codec: bool,
    codec: Option<&str>,
    output_format: Option<&str>,
    has_audio_device: Option<bool>,
) -> Option<&'static str> {
    if !passthrough_enabled_for_codec || !codec.is_some_and(is_passthrough_codec) {
        return None;
    }

    if output_format.is_some_and(|format| format.starts_with("spdif-")) {
        return None;
    }

    match has_audio_device {
        Some(false) => Some(PCM_FALLBACK_NOTICE),
        Some(true) if output_format.is_some() => Some(PCM_FALLBACK_NOTICE),
        Some(true) | None => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kattri_settings::{Passthrough, Settings};

    #[test]
    fn output_and_enabled_codec_table_builds_the_expected_ordered_allowlisted_value() {
        let cases = [
            (AudioOutput::Basic, Passthrough::default(), ""),
            (
                AudioOutput::Spdif,
                Passthrough {
                    ac3: true,
                    dts: true,
                    eac3: true,
                    dts_hd: true,
                    truehd: true,
                },
                "ac3,dts",
            ),
            (
                AudioOutput::Hdmi,
                Passthrough {
                    ac3: true,
                    dts: true,
                    eac3: true,
                    dts_hd: false,
                    truehd: true,
                },
                "ac3,dts,eac3,truehd",
            ),
            (
                AudioOutput::Hdmi,
                Passthrough {
                    ac3: true,
                    dts: true,
                    eac3: true,
                    dts_hd: true,
                    truehd: true,
                },
                "ac3,eac3,dts-hd,truehd",
            ),
            (AudioOutput::Hdmi, Passthrough::default(), ""),
        ];

        for (output, passthrough, expected) in cases {
            let settings = AudioSettings {
                output,
                passthrough,
                ..AudioSettings::default()
            };
            assert_eq!(audio_spdif_value(&settings), expected);
            let option = audio_spdif_option(&settings).expect("value is allowlisted");
            assert_eq!(option.name, "audio-spdif");
            assert_eq!(option.value, expected);
        }
    }

    #[test]
    fn all_persisted_passthrough_flags_round_trip_with_output_choice() {
        let mut settings = Settings::default();
        settings.audio.output = AudioOutput::Hdmi;
        settings.audio.passthrough = Passthrough {
            ac3: true,
            eac3: true,
            dts: true,
            dts_hd: true,
            truehd: true,
        };
        let serialized = kattri_settings::to_toml_string(&settings).expect("settings serialize");
        let restored = kattri_settings::load_from_str(&serialized).expect("settings load");
        assert_eq!(restored.audio.output, AudioOutput::Hdmi);
        assert_eq!(restored.audio.passthrough, settings.audio.passthrough);
    }

    #[test]
    fn gapless_setting_maps_to_yes_and_no_through_the_allowlist() {
        for (enabled, expected) in [(true, "yes"), (false, "no")] {
            let option = gapless_audio_option(enabled).expect("flag is allowlisted");
            assert_eq!(option.name, "gapless-audio");
            assert_eq!(option.value, expected);
        }
    }

    #[test]
    fn pcm_fallback_is_only_reported_for_enabled_codec_without_spdif_output() {
        let cases = [
            (false, Some("dts"), Some("pcm"), Some(true), None),
            (true, Some("flac"), Some("pcm"), Some(true), None),
            (true, Some("dts"), Some("spdif-dts"), Some(true), None),
            (
                true,
                Some("dts"),
                Some("pcm"),
                Some(true),
                Some(PCM_FALLBACK_NOTICE),
            ),
            (
                true,
                Some("truehd"),
                None,
                Some(false),
                Some(PCM_FALLBACK_NOTICE),
            ),
            (true, Some("dts"), None, Some(true), None),
            (true, None, Some("pcm"), Some(true), None),
            (true, Some("dts"), None, None, None),
        ];

        for (enabled, codec, format, device, expected) in cases {
            assert_eq!(
                pcm_fallback_notice(enabled, codec, format, device),
                expected,
                "enabled={enabled}, codec={codec:?}, format={format:?}, device={device:?}"
            );
        }
    }
}
