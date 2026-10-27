//! What Kattri does with HDR and Dolby Vision video (M6-05, with M6-03 for the SDR fallback).
//! Pure: a table from the stream's signalling, the display and the viewer's preference to one
//! action. Nothing here talks to the player or the display.
//!
//! Rules this module keeps:
//! - A Dolby Vision output path is never claimed unless the caller says it was measured
//!   ([`Display::dolby_vision_output_proven`]). Nothing sets that today, so every Dolby Vision
//!   stream resolves to its HDR10, HLG or SDR base layer, or to tone mapping.
//! - HDR is never left half-applied: either the display shows the signal, or it is tone mapped
//!   to SDR. There is no "HDR signal on an SDR display" result.

use kattri_settings::HdrPreference;

/// The transfer function and primaries class of the stream, as the player reports them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Signal {
    /// Ordinary SDR (BT.709 or similar).
    Sdr,
    /// HDR10: PQ transfer, BT.2020 primaries.
    Hdr10,
    /// Hybrid log-gamma.
    Hlg,
}

impl Signal {
    /// Classify mpv's `video-params/gamma` string.
    pub fn from_mpv_transfer(transfer: &str) -> Option<Self> {
        match transfer.to_ascii_lowercase().as_str() {
            "pq" => Some(Self::Hdr10),
            "hlg" => Some(Self::Hlg),
            "bt.1886" | "srgb" | "gamma2.2" | "gamma2.4" | "gamma2.6" | "gamma2.8" => {
                Some(Self::Sdr)
            }
            _ => None,
        }
    }
}

/// Dolby Vision signalling read from the stream's configuration record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DolbyVision {
    /// Profile number: 4, 5, 7, 8 or 9.
    pub profile: u8,
    /// `dv_bl_signal_compatibility_id`: 0 none, 1 HDR10, 2 SDR, 4 HLG.
    pub compatibility_id: u8,
    /// False for a file that declares Dolby Vision but carries no RPU metadata.
    pub has_rpu: bool,
}

impl DolbyVision {
    /// The signal of the base layer a player without Dolby Vision support would show, if the
    /// profile has one. Profile 5 has none: its pictures are only correct after RPU reshaping.
    pub fn base_layer(self) -> Option<Signal> {
        match (self.profile, self.compatibility_id) {
            (5, _) | (_, 0) => None,
            (_, 1) => Some(Signal::Hdr10),
            (_, 2) => Some(Signal::Sdr),
            (_, 4) => Some(Signal::Hlg),
            _ => None,
        }
    }
}

/// What the display can show.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Display {
    /// The display and output path present HDR10 and HLG signals as HDR.
    pub hdr: bool,
    /// A Dolby Vision output path was measured on this display (a recorded readout). Never
    /// guessed from the display's name or its EDID.
    pub dolby_vision_output_proven: bool,
}

/// The decision for one stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// SDR video on any display: no tone mapping.
    PlaySdr,
    /// The display shows this HDR signal as HDR.
    PresentHdr(Signal),
    /// Map the HDR signal down to the SDR display (mpv `tone-mapping`).
    ToneMapToSdr(Signal),
    /// Dolby Vision on a display with a proven Dolby Vision output path.
    PresentDolbyVision,
    /// Dolby Vision with no usable base layer (profile 5) on a display that cannot show Dolby
    /// Vision: reshape with the RPU and tone map to SDR.
    ReshapeAndToneMap,
}

/// Choose the action. `dolby_vision` is `None` for a stream that does not declare it.
pub fn decide(
    signal: Signal,
    dolby_vision: Option<DolbyVision>,
    display: Display,
    preference: HdrPreference,
) -> Action {
    // A declaration without metadata is only a claim: play the stream as its base signal.
    let dolby_vision = dolby_vision.filter(|dv| dv.has_rpu);
    if let Some(dv) = dolby_vision {
        let dv_allowed = preference != HdrPreference::Hdr10;
        if display.dolby_vision_output_proven && dv_allowed {
            return Action::PresentDolbyVision;
        }
        return match dv.base_layer() {
            Some(base) => present_or_tone_map(base, display),
            None => Action::ReshapeAndToneMap,
        };
    }
    present_or_tone_map(signal, display)
}

fn present_or_tone_map(signal: Signal, display: Display) -> Action {
    match (signal, display.hdr) {
        (Signal::Sdr, _) => Action::PlaySdr,
        (hdr, true) => Action::PresentHdr(hdr),
        (hdr, false) => Action::ToneMapToSdr(hdr),
    }
}

/// Whether the viewer's choice is offered. "Prefer Dolby Vision" appears only on a display where
/// a Dolby Vision output path was proven.
pub fn dolby_vision_preference_offered(display: Display) -> bool {
    display.dolby_vision_output_proven
}

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
    fn plain_streams_follow_the_display() {
        for preference in [
            HdrPreference::Auto,
            HdrPreference::Dovi,
            HdrPreference::Hdr10,
        ] {
            assert_eq!(
                decide(Signal::Sdr, None, SDR_SCREEN, preference),
                Action::PlaySdr
            );
            assert_eq!(
                decide(Signal::Sdr, None, HDR_SCREEN, preference),
                Action::PlaySdr
            );
            assert_eq!(
                decide(Signal::Hdr10, None, HDR_SCREEN, preference),
                Action::PresentHdr(Signal::Hdr10)
            );
            assert_eq!(
                decide(Signal::Hlg, None, HDR_SCREEN, preference),
                Action::PresentHdr(Signal::Hlg)
            );
            assert_eq!(
                decide(Signal::Hdr10, None, SDR_SCREEN, preference),
                Action::ToneMapToSdr(Signal::Hdr10)
            );
            assert_eq!(
                decide(Signal::Hlg, None, SDR_SCREEN, preference),
                Action::ToneMapToSdr(Signal::Hlg)
            );
        }
    }

    #[test]
    fn dolby_vision_is_never_presented_without_a_proven_output_path() {
        for profile_id in [(5, 0), (7, 1), (8, 1), (8, 2), (8, 4)] {
            for preference in [
                HdrPreference::Auto,
                HdrPreference::Dovi,
                HdrPreference::Hdr10,
            ] {
                for display in [SDR_SCREEN, HDR_SCREEN] {
                    let action = decide(
                        Signal::Hdr10,
                        dv(profile_id.0, profile_id.1),
                        display,
                        preference,
                    );
                    assert_ne!(action, Action::PresentDolbyVision, "{profile_id:?}");
                }
            }
        }
    }

    #[test]
    fn a_hdr10_compatible_dolby_vision_stream_plays_its_base_layer() {
        for (profile, id) in [(7, 1), (8, 1)] {
            assert_eq!(
                decide(
                    Signal::Hdr10,
                    dv(profile, id),
                    HDR_SCREEN,
                    HdrPreference::Auto
                ),
                Action::PresentHdr(Signal::Hdr10)
            );
            assert_eq!(
                decide(
                    Signal::Hdr10,
                    dv(profile, id),
                    SDR_SCREEN,
                    HdrPreference::Auto
                ),
                Action::ToneMapToSdr(Signal::Hdr10)
            );
        }
    }

    #[test]
    fn profile_8_with_an_sdr_or_hlg_base_follows_that_base() {
        assert_eq!(
            decide(Signal::Sdr, dv(8, 2), HDR_SCREEN, HdrPreference::Auto),
            Action::PlaySdr
        );
        assert_eq!(
            decide(Signal::Hlg, dv(8, 4), HDR_SCREEN, HdrPreference::Auto),
            Action::PresentHdr(Signal::Hlg)
        );
        assert_eq!(
            decide(Signal::Hlg, dv(8, 4), SDR_SCREEN, HdrPreference::Auto),
            Action::ToneMapToSdr(Signal::Hlg)
        );
    }

    #[test]
    fn profile_5_has_no_base_layer_and_needs_reshaping_on_every_non_dolby_display() {
        for display in [SDR_SCREEN, HDR_SCREEN] {
            assert_eq!(
                decide(Signal::Hdr10, dv(5, 0), display, HdrPreference::Auto),
                Action::ReshapeAndToneMap
            );
        }
    }

    #[test]
    fn a_proven_dolby_vision_display_presents_it_unless_hdr10_is_preferred() {
        for (profile, id) in [(5, 0), (7, 1), (8, 1)] {
            assert_eq!(
                decide(
                    Signal::Hdr10,
                    dv(profile, id),
                    DV_SCREEN,
                    HdrPreference::Auto
                ),
                Action::PresentDolbyVision
            );
            assert_eq!(
                decide(
                    Signal::Hdr10,
                    dv(profile, id),
                    DV_SCREEN,
                    HdrPreference::Dovi
                ),
                Action::PresentDolbyVision
            );
        }
        assert_eq!(
            decide(Signal::Hdr10, dv(8, 1), DV_SCREEN, HdrPreference::Hdr10),
            Action::PresentHdr(Signal::Hdr10)
        );
        // Profile 5 has no HDR10 layer to prefer.
        assert_eq!(
            decide(Signal::Hdr10, dv(5, 0), DV_SCREEN, HdrPreference::Hdr10),
            Action::ReshapeAndToneMap
        );
    }

    #[test]
    fn a_declaration_without_rpu_metadata_is_treated_as_its_base_signal() {
        let claim = Some(DolbyVision {
            profile: 8,
            compatibility_id: 1,
            has_rpu: false,
        });
        assert_eq!(
            decide(Signal::Hdr10, claim, DV_SCREEN, HdrPreference::Dovi),
            Action::PresentHdr(Signal::Hdr10)
        );
        assert_eq!(
            decide(Signal::Hdr10, claim, SDR_SCREEN, HdrPreference::Auto),
            Action::ToneMapToSdr(Signal::Hdr10)
        );
    }

    #[test]
    fn mpv_transfer_names_classify_the_signal() {
        assert_eq!(Signal::from_mpv_transfer("pq"), Some(Signal::Hdr10));
        assert_eq!(Signal::from_mpv_transfer("HLG"), Some(Signal::Hlg));
        assert_eq!(Signal::from_mpv_transfer("bt.1886"), Some(Signal::Sdr));
        assert_eq!(Signal::from_mpv_transfer("srgb"), Some(Signal::Sdr));
        assert_eq!(Signal::from_mpv_transfer("v-log"), None);
        assert_eq!(Signal::from_mpv_transfer(""), None);
    }

    #[test]
    fn prefer_dolby_vision_is_offered_only_where_it_was_proven() {
        assert!(!dolby_vision_preference_offered(SDR_SCREEN));
        assert!(!dolby_vision_preference_offered(HDR_SCREEN));
        assert!(dolby_vision_preference_offered(DV_SCREEN));
    }
}
