//! Dolby Vision execution policy (M6-05), on top of the decision table in [`crate::hdr_policy`].
//!
//! [`DvPolicy`] names what the player would do with a Dolby Vision stream. Selecting a policy is
//! pure, and each policy carries a validation rule for the stream and display it is used with.
//! Nothing here talks to libmpv, reads a file, or queries a display.
//!
//! What is known, and from where:
//! - The captured pinned libmpv runtime inventory
//!   (`crates/client-core/tests/fixtures/mpv-runtime-options.json`, mpv v0.41.0-1286-gfa0534a72,
//!   861 options) contains no option whose name selects, strips, or passes through a Dolby Vision
//!   layer. A test below asserts that absence. The only allowlisted HDR option is `tone-mapping`,
//!   which belongs to the M6-03 setting `video.tone_mapping`; it maps a curve, not a DV layer.
//! - The host exposes the transfer function through `video-params/gamma`
//!   (`proofs/mpv-render/src/stream_info.rs`). It does not read the Dolby Vision configuration
//!   record, so no caller in this tree can build a [`crate::hdr_policy::DolbyVision`] value yet.
//! - [`crate::hdr_policy`] never returns [`Action::PresentDolbyVision`] unless the caller sets
//!   [`Display::dolby_vision_output_proven`], and nothing sets it.
//!
//! What is unknown or unvalidated:
//! - Whether the pinned libmpv build can present Dolby Vision at all (M6-01 and M6-02 are open),
//!   and whether these policies behave as described on real profile 5, 7, 8 and 9 files.
//! - No Dolby Vision file, display, or output path was exercised. This module is a model, not a
//!   measurement.
//!
//! Therefore only [`DvPolicy::ToneMapToSdr`] maps to an allowlisted mpv option, and the host must
//! not write it from here until a DV signalling read exists and M6-02 records a DV output path.

use std::fmt;

use kattri_settings::HdrPreference;

use crate::hdr_policy::{Action, Display, DolbyVision, Signal, decide};
use crate::mpv_options::{MpvOptionError, OptionValue, ValidatedOption, validate};

/// What Kattri would do with a Dolby Vision stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DvPolicy {
    /// Ask the output path for the Dolby Vision presentation. Only valid on a display where a
    /// Dolby Vision output path was measured.
    Passthrough,
    /// Tone map the picture down to the SDR range (mpv `tone-mapping`).
    ToneMapToSdr,
    /// Play the HDR10-compatible base layer without the Dolby Vision enhancement.
    StripToHdr10BaseLayer,
    /// Play a base layer that is not HDR10 (the SDR or HLG compatible layer) unchanged.
    PlayNonHdr10BaseLayer,
}

impl DvPolicy {
    /// Every policy, in display order.
    pub const ALL: [Self; 4] = [
        Self::Passthrough,
        Self::ToneMapToSdr,
        Self::StripToHdr10BaseLayer,
        Self::PlayNonHdr10BaseLayer,
    ];

    /// The settings spelling. Stable text: it would be persisted if a setting is added.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Passthrough => "passthrough",
            Self::ToneMapToSdr => "tone_map_to_sdr",
            Self::StripToHdr10BaseLayer => "strip_to_hdr10_base_layer",
            Self::PlayNonHdr10BaseLayer => "play_non_hdr10_base_layer",
        }
    }

    /// Parse the settings spelling. Surrounding whitespace is ignored; every other difference
    /// (case, a different separator) is refused rather than guessed.
    pub fn parse(raw: &str) -> Result<Self, DvPolicyError> {
        match raw.trim() {
            "passthrough" => Ok(Self::Passthrough),
            "tone_map_to_sdr" => Ok(Self::ToneMapToSdr),
            "strip_to_hdr10_base_layer" => Ok(Self::StripToHdr10BaseLayer),
            "play_non_hdr10_base_layer" => Ok(Self::PlayNonHdr10BaseLayer),
            _ => Err(DvPolicyError::UnknownSpelling),
        }
    }

    /// The policy for a signalling/display/preference combination, or `None` when the stream does
    /// not carry usable Dolby Vision metadata (the [`crate::hdr_policy`] table then applies).
    pub fn select(
        signal: Signal,
        dolby_vision: Option<DolbyVision>,
        display: Display,
        preference: HdrPreference,
    ) -> Option<Self> {
        let dolby_vision = dolby_vision.filter(|dv| dv.has_rpu)?;
        match decide(signal, Some(dolby_vision), display, preference) {
            Action::PresentDolbyVision => Some(Self::Passthrough),
            Action::ReshapeAndToneMap | Action::ToneMapToSdr(_) => Some(Self::ToneMapToSdr),
            Action::PresentHdr(Signal::Hdr10) => Some(Self::StripToHdr10BaseLayer),
            Action::PresentHdr(Signal::Hlg | Signal::Sdr) | Action::PlaySdr => {
                Some(Self::PlayNonHdr10BaseLayer)
            }
        }
    }

    /// Check the policy against the stream and display it is about to be used with.
    ///
    /// Tone mapping is always allowed: it is the fallback that needs no DV output path.
    /// Passthrough needs Dolby Vision metadata and a measured DV output path. The two base-layer
    /// policies need a base layer of the matching kind, so a profile 5 stream cannot be "stripped
    /// to HDR10" and an HDR10 base is not played as a non-HDR10 base.
    pub fn validate(
        self,
        dolby_vision: Option<DolbyVision>,
        display: Display,
    ) -> Result<(), DvPolicyError> {
        if self == Self::ToneMapToSdr {
            return Ok(());
        }
        let Some(dv) = dolby_vision.filter(|dv| dv.has_rpu) else {
            return Err(match dolby_vision {
                Some(_) => DvPolicyError::NoDolbyVisionMetadata,
                None => DvPolicyError::NotDolbyVisionStream,
            });
        };
        match self {
            Self::Passthrough => {
                if display.dolby_vision_output_proven {
                    Ok(())
                } else {
                    Err(DvPolicyError::DolbyVisionOutputUnproven)
                }
            }
            Self::StripToHdr10BaseLayer => match dv.base_layer() {
                Some(Signal::Hdr10) => Ok(()),
                _ => Err(DvPolicyError::NoHdr10BaseLayer),
            },
            Self::PlayNonHdr10BaseLayer => match dv.base_layer() {
                Some(Signal::Sdr | Signal::Hlg) => Ok(()),
                _ => Err(DvPolicyError::NotANonHdr10BaseLayer),
            },
            Self::ToneMapToSdr => Ok(()),
        }
    }

    /// The allowlisted mpv property this policy drives, if one exists in the pinned runtime.
    ///
    /// Only tone mapping has an allowlisted option. The base-layer and passthrough policies have
    /// no option in the captured runtime inventory, so they are model-only until the pinned build
    /// and a DV output path are measured.
    pub const fn allowlisted_option(self) -> Option<&'static str> {
        match self {
            Self::ToneMapToSdr => Some("tone-mapping"),
            Self::Passthrough | Self::StripToHdr10BaseLayer | Self::PlayNonHdr10BaseLayer => None,
        }
    }

    /// The checked mpv writes for a policy. Only [`DvPolicy::ToneMapToSdr`] writes anything, and
    /// the value is the user's M6-03 tone-mapping curve; the load path still owns that write.
    /// The other policies return no writes rather than inventing an option.
    pub fn mpv_options(
        self,
        curve: kattri_settings::ToneMapping,
    ) -> Result<Vec<ValidatedOption>, MpvOptionError> {
        match self.allowlisted_option() {
            Some(name) => Ok(vec![validate(name, OptionValue::Text(curve.mpv_value()))?]),
            None => Ok(Vec::new()),
        }
    }
}

/// Why a policy was refused. No variant carries a user value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DvPolicyError {
    /// The text is not one of the stable policy spellings.
    UnknownSpelling,
    /// The stream does not declare Dolby Vision.
    NotDolbyVisionStream,
    /// The stream declares Dolby Vision but carries no RPU metadata.
    NoDolbyVisionMetadata,
    /// No Dolby Vision output path was measured on this display.
    DolbyVisionOutputUnproven,
    /// The stream has no HDR10-compatible base layer (for example profile 5).
    NoHdr10BaseLayer,
    /// The stream's base layer is the HDR10 one, so it is not a non-HDR10 base.
    NotANonHdr10BaseLayer,
}

impl fmt::Display for DvPolicyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownSpelling => f.write_str("unknown Dolby Vision policy spelling"),
            Self::NotDolbyVisionStream => f.write_str("the stream does not declare Dolby Vision"),
            Self::NoDolbyVisionMetadata => {
                f.write_str("the Dolby Vision declaration carries no RPU metadata")
            }
            Self::DolbyVisionOutputUnproven => {
                f.write_str("no Dolby Vision output path was measured on this display")
            }
            Self::NoHdr10BaseLayer => f.write_str("the stream has no HDR10-compatible base layer"),
            Self::NotANonHdr10BaseLayer => {
                f.write_str("the stream's base layer is HDR10, not SDR or HLG")
            }
        }
    }
}

impl std::error::Error for DvPolicyError {}

#[cfg(test)]
mod tests {
    use super::*;

    const SDR_SCREEN: Display = Display {
        hdr: false,
        dolby_vision_output_proven: false,
    };
    const HDR_SCREEN: Display = Display {
        hdr: true,
        dolby_vision_output_proven: false,
    };
    const DV_SCREEN: Display = Display {
        hdr: true,
        dolby_vision_output_proven: true,
    };

    fn dv(profile: u8, compatibility_id: u8) -> Option<DolbyVision> {
        Some(DolbyVision {
            profile,
            compatibility_id,
            has_rpu: true,
        })
    }

    #[test]
    fn every_policy_spelling_round_trips() {
        for policy in DvPolicy::ALL {
            assert_eq!(DvPolicy::parse(policy.as_str()), Ok(policy));
        }
    }

    #[test]
    fn unknown_spellings_are_refused_without_echoing_them() {
        for bad in [
            "",
            " ",
            "auto",
            "prefer_dv",
            "PASSTHROUGH",
            "tone-map",
            " force",
        ] {
            assert_eq!(DvPolicy::parse(bad), Err(DvPolicyError::UnknownSpelling));
        }
    }

    #[test]
    fn streams_without_usable_dolby_vision_metadata_have_no_policy() {
        assert_eq!(
            DvPolicy::select(Signal::Hdr10, None, HDR_SCREEN, HdrPreference::Auto),
            None
        );
        assert_eq!(
            DvPolicy::select(
                Signal::Hdr10,
                Some(DolbyVision {
                    profile: 8,
                    compatibility_id: 1,
                    has_rpu: false,
                }),
                DV_SCREEN,
                HdrPreference::Dovi
            ),
            None
        );
    }

    #[test]
    fn selection_table_over_profiles_and_displays() {
        let cases: [(Option<DolbyVision>, Display, HdrPreference, DvPolicy); 8] = [
            (
                dv(7, 1),
                HDR_SCREEN,
                HdrPreference::Auto,
                DvPolicy::StripToHdr10BaseLayer,
            ),
            (
                dv(8, 1),
                HDR_SCREEN,
                HdrPreference::Auto,
                DvPolicy::StripToHdr10BaseLayer,
            ),
            (
                dv(8, 1),
                SDR_SCREEN,
                HdrPreference::Auto,
                DvPolicy::ToneMapToSdr,
            ),
            (
                dv(5, 0),
                HDR_SCREEN,
                HdrPreference::Auto,
                DvPolicy::ToneMapToSdr,
            ),
            (
                dv(8, 2),
                HDR_SCREEN,
                HdrPreference::Auto,
                DvPolicy::PlayNonHdr10BaseLayer,
            ),
            (
                dv(8, 4),
                HDR_SCREEN,
                HdrPreference::Auto,
                DvPolicy::PlayNonHdr10BaseLayer,
            ),
            (
                dv(7, 1),
                DV_SCREEN,
                HdrPreference::Auto,
                DvPolicy::Passthrough,
            ),
            (
                dv(7, 1),
                DV_SCREEN,
                HdrPreference::Hdr10,
                DvPolicy::StripToHdr10BaseLayer,
            ),
        ];
        for (dolby_vision, display, preference, expected) in cases {
            assert_eq!(
                DvPolicy::select(Signal::Hdr10, dolby_vision, display, preference),
                Some(expected),
                "{dolby_vision:?} {display:?} {preference:?}"
            );
        }
    }

    #[test]
    fn passthrough_needs_a_proven_output_path_and_metadata() {
        assert_eq!(
            DvPolicy::Passthrough.validate(dv(8, 1), HDR_SCREEN),
            Err(DvPolicyError::DolbyVisionOutputUnproven)
        );
        assert_eq!(DvPolicy::Passthrough.validate(dv(8, 1), DV_SCREEN), Ok(()));
        assert_eq!(
            DvPolicy::Passthrough.validate(None, DV_SCREEN),
            Err(DvPolicyError::NotDolbyVisionStream)
        );
        let declaration = Some(DolbyVision {
            profile: 8,
            compatibility_id: 1,
            has_rpu: false,
        });
        assert_eq!(
            DvPolicy::Passthrough.validate(declaration, DV_SCREEN),
            Err(DvPolicyError::NoDolbyVisionMetadata)
        );
    }

    #[test]
    fn base_layer_policies_need_the_matching_base_layer() {
        assert_eq!(
            DvPolicy::StripToHdr10BaseLayer.validate(dv(7, 1), HDR_SCREEN),
            Ok(())
        );
        assert_eq!(
            DvPolicy::StripToHdr10BaseLayer.validate(dv(5, 0), HDR_SCREEN),
            Err(DvPolicyError::NoHdr10BaseLayer)
        );
        assert_eq!(
            DvPolicy::PlayNonHdr10BaseLayer.validate(dv(8, 2), HDR_SCREEN),
            Ok(())
        );
        assert_eq!(
            DvPolicy::PlayNonHdr10BaseLayer.validate(dv(8, 4), HDR_SCREEN),
            Ok(())
        );
        assert_eq!(
            DvPolicy::PlayNonHdr10BaseLayer.validate(dv(8, 1), HDR_SCREEN),
            Err(DvPolicyError::NotANonHdr10BaseLayer)
        );
    }

    #[test]
    fn tone_mapping_is_allowed_without_any_dolby_vision_metadata() {
        for dolby_vision in [None, dv(5, 0), dv(8, 1)] {
            assert_eq!(
                DvPolicy::ToneMapToSdr.validate(dolby_vision, SDR_SCREEN),
                Ok(())
            );
        }
    }

    #[test]
    fn only_tone_mapping_has_an_allowlisted_option() {
        assert_eq!(
            DvPolicy::ToneMapToSdr.allowlisted_option(),
            Some("tone-mapping")
        );
        for policy in [
            DvPolicy::Passthrough,
            DvPolicy::StripToHdr10BaseLayer,
            DvPolicy::PlayNonHdr10BaseLayer,
        ] {
            assert_eq!(policy.allowlisted_option(), None, "{policy:?}");
            assert!(
                policy
                    .mpv_options(kattri_settings::ToneMapping::Auto)
                    .is_ok_and(|options| options.is_empty()),
                "{policy:?}"
            );
        }
    }

    #[test]
    fn tone_mapping_policy_writes_the_m6_03_curve_through_the_allowlist() {
        for curve in kattri_settings::ToneMapping::ALL {
            let options = DvPolicy::ToneMapToSdr
                .mpv_options(curve)
                .expect("every tone-mapping curve is allowlisted");
            assert_eq!(options.len(), 1);
            assert_eq!(options[0].name, "tone-mapping");
            assert_eq!(options[0].value, curve.mpv_value());
        }
    }

    #[test]
    fn the_pinned_runtime_has_no_dolby_vision_option() {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/mpv-runtime-options.json"))
                .expect("the captured runtime-options fixture is valid JSON");
        let options = fixture
            .get("options")
            .and_then(serde_json::Value::as_array)
            .expect("the fixture has an options array");
        for option in options {
            let Some(name) = option.as_str() else {
                continue;
            };
            let lower = name.to_ascii_lowercase();
            assert!(
                !lower.contains("dolby") && !lower.starts_with("dv-"),
                "the pinned runtime gained a Dolby Vision option: {name}"
            );
        }
        assert!(
            options
                .iter()
                .any(|option| option.as_str() == Some("tone-mapping")),
            "the tone-mapping name must be backed by the captured runtime"
        );
    }
}
