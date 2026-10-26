//! Pure playback policy. Each function takes already-typed inputs and returns the decision in
//! mpv's spelling. Nothing here calls libmpv, reads a file, or queries hardware.
//!
//! Option names are checked by the caller. Where a name is on the `mpv_options` allowlist, the
//! send path validates it before writing. Where a name is not on the allowlist, the function
//! still returns it so the decision can be tested, and the caller must refuse to send it until
//! the allowlist gains an entry with a documented value range.

use std::collections::{BTreeMap, HashSet};

use kattri_client_core::audio_device::AudioDeviceList;
use kattri_client_core::audio_filters::{AudioFilterSettings, filter_graph};
use kattri_client_core::equalizer::Equalizer;

// ---------------------------------------------------------------------------
// Gapless preload between queue items
// ---------------------------------------------------------------------------

/// Preload the next item once the current item has less than this many seconds left.
pub const PRELOAD_REMAINING_SECONDS: f64 = 15.0;

/// The next queue entry as the playback layer sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NextQueueItem {
    pub id: String,
    /// Album the entry belongs to, when the queue knows it.
    pub album_id: Option<String>,
}

/// Remembers which queue items have already been preloaded, so each item is preloaded at most
/// once while the queue stays the same. Call [`PreloadGate::reset`] when the queue is replaced.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PreloadGate {
    preloaded: HashSet<String>,
}

impl PreloadGate {
    pub fn new() -> Self {
        Self::default()
    }

    /// Return the next item's id when it should be preloaded now, and `None` otherwise.
    ///
    /// The id is returned only when the remaining time is known, is below
    /// [`PRELOAD_REMAINING_SECONDS`], a next item exists, and that item was not returned before.
    pub fn next_to_preload(
        &mut self,
        remaining_seconds: Option<f64>,
        next: Option<&NextQueueItem>,
    ) -> Option<String> {
        let remaining = remaining_seconds.filter(|value| !value.is_nan())?;
        if remaining >= PRELOAD_REMAINING_SECONDS {
            return None;
        }
        let next = next?;
        if self.preloaded.contains(&next.id) {
            return None;
        }
        self.preloaded.insert(next.id.clone());
        Some(next.id.clone())
    }

    pub fn reset(&mut self) {
        self.preloaded.clear();
    }
}

/// The `gapless-audio` write when the next queue item is on the same album as the current one.
///
/// `yes` is the allowlisted spelling for the flag. Returns `None` when the albums differ or are
/// unknown, so the caller keeps the value from settings.
pub fn gapless_audio_command(
    current_album: Option<&str>,
    next: Option<&NextQueueItem>,
) -> Option<[&'static str; 3]> {
    let next_album = next.and_then(|item| item.album_id.as_deref())?;
    let same_album = current_album.is_some_and(|album| album == next_album);
    same_album.then_some(["set", "gapless-audio", "yes"])
}

// ---------------------------------------------------------------------------
// Write ledger
// ---------------------------------------------------------------------------

/// Tracks the last value written for each option, so a settings change writes only the options
/// whose value moved.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OptionWriteLedger {
    applied: BTreeMap<&'static str, String>,
}

impl OptionWriteLedger {
    pub fn new() -> Self {
        Self::default()
    }

    /// Return the pairs whose value differs from the last applied value, in input order, and
    /// record them as applied.
    pub fn changed_writes(
        &mut self,
        pairs: &[(&'static str, String)],
    ) -> Vec<(&'static str, String)> {
        let mut writes = Vec::new();
        for &(name, ref value) in pairs {
            if self.applied.get(name) == Some(value) {
                continue;
            }
            self.applied.insert(name, value.clone());
            writes.push((name, value.clone()));
        }
        writes
    }

    /// The last value recorded for `name`, if any.
    pub fn applied(&self, name: &str) -> Option<&str> {
        self.applied.get(name).map(String::as_str)
    }
}

// ---------------------------------------------------------------------------
// ASS and SSA rendering
// ---------------------------------------------------------------------------

/// Values accepted for `sub-ass-override`, as mpv documents them.
/// [UNCONFIRMED against the pinned libmpv.]
const SUB_ASS_OVERRIDE_VALUES: &[&str] = &["no", "yes", "force", "scale", "strip"];

/// Options that control ASS and SSA subtitle rendering.
///
/// `sub-ass` enables ASS rendering. `sub-ass-style-overrides` carries the scaled border and
/// shadow choice; the spelling follows the legacy client. `sub-ass-override` is written only
/// when the stored override is one of the documented values, so an arbitrary string never
/// reaches mpv. None of these names is on the `mpv_options` allowlist yet.
pub fn ass_rendering_pairs(
    scale_border_and_shadow: bool,
    ass_override: Option<&str>,
) -> Vec<(&'static str, String)> {
    let mut pairs = vec![
        ("sub-ass", "yes".to_owned()),
        (
            "sub-ass-style-overrides",
            if scale_border_and_shadow {
                "ScaledBorderAndShadow=yes".to_owned()
            } else {
                "ScaledBorderAndShadow=no".to_owned()
            },
        ),
    ];
    if let Some(value) = ass_override
        .map(str::trim)
        .filter(|value| SUB_ASS_OVERRIDE_VALUES.contains(value))
    {
        pairs.push(("sub-ass-override", value.to_owned()));
    }
    pairs
}

// ---------------------------------------------------------------------------
// Audio output
// ---------------------------------------------------------------------------

/// The single `af` value for the equalizer and the audio filter chain.
///
/// The equalizer bands and the enabled filters share one `lavfi=[...]` graph, equalizer first.
/// `None` means no filter is enabled; the caller then clears `af`.
pub fn audio_filter_value(equalizer: &Equalizer, filters: &AudioFilterSettings) -> Option<String> {
    let parts: Vec<String> = [equalizer.to_filter_graph(), filter_graph(filters)]
        .into_iter()
        .flatten()
        .filter(|part| !part.is_empty())
        .collect();
    if parts.is_empty() {
        None
    } else {
        Some(format!("lavfi=[{}]", parts.join(",")))
    }
}

/// The `audio-device` pair for the selected output. The value is `auto` when no device is chosen.
pub fn audio_device_pair(devices: &AudioDeviceList) -> (&'static str, String) {
    ("audio-device", devices.mpv_value().to_owned())
}

// ---------------------------------------------------------------------------
// Hardware decode
// ---------------------------------------------------------------------------

/// Map a `video.decode_mode` value, in its settings spelling, to the mpv `hwdec` value.
///
/// `auto` maps to `auto` and `off` maps to `no`. The named backends keep their names. An
/// unknown spelling returns `None`. `vulkan` also returns `None`: the hwdec allowlist has no
/// entry for it, so the caller keeps the default instead of sending a refused value. This
/// matches `kattri_client_core::mpv_options::hwdec_for_decode_mode`.
pub fn hwdec_for_decode_mode(decode_mode: &str) -> Option<&'static str> {
    match decode_mode {
        "auto" => Some("auto"),
        "off" => Some("no"),
        "d3d11va" => Some("d3d11va"),
        "nvdec" => Some("nvdec"),
        "vaapi" => Some("vaapi"),
        "videotoolbox" => Some("videotoolbox"),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// HDR output and tone mapping
// ---------------------------------------------------------------------------

/// What the video output does with an HDR source.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HdrOutputPlan {
    /// The source is SDR. No HDR option is written.
    SdrSource,
    /// HDR source, HDR display, and passthrough enabled: the output signals its colorspace.
    HdrPassthrough,
    /// HDR source, HDR display, passthrough disabled: tone map to the display's peak.
    ToneMapToHdrDisplay,
    /// HDR source on a display that is not HDR: tone map to SDR.
    ToneMapToSdr,
}

/// Choose the HDR output plan from the source, the display, and the passthrough setting.
pub fn hdr_output_plan(
    source_is_hdr: bool,
    display_is_hdr: bool,
    passthrough_enabled: bool,
) -> HdrOutputPlan {
    match (source_is_hdr, display_is_hdr, passthrough_enabled) {
        (false, _, _) => HdrOutputPlan::SdrSource,
        (true, true, true) => HdrOutputPlan::HdrPassthrough,
        (true, true, false) => HdrOutputPlan::ToneMapToHdrDisplay,
        (true, false, _) => HdrOutputPlan::ToneMapToSdr,
    }
}

/// The mpv option pairs for an HDR output plan.
///
/// `target-colorspace-hint` and `hdr-compute-peak` take `yes` or `no`. `tone-mapping` takes a
/// named algorithm, and `auto` lets mpv choose. The values follow the mpv manual.
/// [UNCONFIRMED against the pinned libmpv. None of these names is on the `mpv_options`
/// allowlist yet.]
pub fn hdr_output_pairs(plan: HdrOutputPlan) -> Vec<(&'static str, &'static str)> {
    match plan {
        HdrOutputPlan::SdrSource => Vec::new(),
        HdrOutputPlan::HdrPassthrough => vec![
            ("target-colorspace-hint", "yes"),
            ("hdr-compute-peak", "yes"),
        ],
        HdrOutputPlan::ToneMapToHdrDisplay => vec![
            ("target-colorspace-hint", "no"),
            ("tone-mapping", "auto"),
            ("hdr-compute-peak", "yes"),
        ],
        HdrOutputPlan::ToneMapToSdr => vec![
            ("target-colorspace-hint", "no"),
            ("tone-mapping", "auto"),
            ("hdr-compute-peak", "yes"),
        ],
    }
}

// ---------------------------------------------------------------------------
// Refresh-rate matching
// ---------------------------------------------------------------------------

/// Relative tolerance for a content rate to match a display rate. The NTSC rates differ from
/// their integer neighbours by about 0.1 percent (24000/1001 against 24), so 0.2 percent is
/// used.
pub const RATE_RELATIVE_TOLERANCE: f64 = 0.002;

/// Largest integer multiple of the content rate that is considered a match.
pub const MAX_RATE_MULTIPLE: u32 = 4;

/// What the refresh policy asks mpv to do.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum RefreshPlan {
    /// Leave `video-sync` and `display-fps-override` as they are.
    Keep,
    /// A numeric override from the user. mpv treats the display as this rate.
    DisplayFpsOverride(f64),
    /// Set `video-sync` to this value. `display-resample` matches the display's refresh;
    /// `audio` keeps the audio clock when no display rate fits.
    VideoSync(&'static str),
}

/// Choose the refresh-rate handling for one file.
///
/// Rules, in order:
/// 1. A positive, finite manual override wins and is written as `display-fps-override`.
/// 2. With auto-switch off, or with an unknown content rate or no display rates, nothing changes.
/// 3. The display rate that is an integer multiple of the content rate (1 to
///    [`MAX_RATE_MULTIPLE`]) is preferred, lowest multiple first. A match selects
///    `display-resample`.
/// 4. With no match, `audio` is selected.
///
/// When `avoid_25_30` is set, 25 Hz and 30 Hz display rates are not candidates. 25 fps content
/// then matches 50 Hz and 30 fps content matches 60 Hz.
pub fn refresh_plan(
    video_fps: Option<f64>,
    supported_rates: &[f64],
    auto_switch: bool,
    avoid_25_30: bool,
    manual_override: Option<f64>,
) -> RefreshPlan {
    if let Some(rate) = manual_override.filter(|rate| rate.is_finite() && *rate > 0.0) {
        return RefreshPlan::DisplayFpsOverride(rate);
    }
    if !auto_switch {
        return RefreshPlan::Keep;
    }
    let Some(fps) = video_fps.filter(|fps| fps.is_finite() && *fps > 0.0) else {
        return RefreshPlan::Keep;
    };
    let candidates: Vec<f64> = supported_rates
        .iter()
        .copied()
        .filter(|rate| rate.is_finite() && *rate > 0.0)
        .filter(|rate| !(avoid_25_30 && (is_near(*rate, 25.0) || is_near(*rate, 30.0))))
        .collect();
    if candidates.is_empty() {
        return RefreshPlan::Keep;
    }
    for multiple in 1..=MAX_RATE_MULTIPLE {
        let target = fps * f64::from(multiple);
        if candidates
            .iter()
            .any(|rate| (rate / target - 1.0).abs() <= RATE_RELATIVE_TOLERANCE)
        {
            return RefreshPlan::VideoSync("display-resample");
        }
    }
    RefreshPlan::VideoSync("audio")
}

fn is_near(rate: f64, target: f64) -> bool {
    (rate - target).abs() < 0.01
}

// ---------------------------------------------------------------------------
// Dolby Vision layer choice
// ---------------------------------------------------------------------------

/// The Dolby Vision preference, in the spelling used by `video.hdr_preference`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DvPreference {
    Auto,
    Dovi,
    Hdr10,
}

/// The layer of a Dolby Vision file that is played.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DolbyVisionLayer {
    /// The file carries no Dolby Vision metadata. The normal HDR or SDR path applies.
    NotDolbyVision,
    /// The Dolby Vision enhancement is played. Only chosen when the display is Dolby Vision
    /// capable and the preference asks for it.
    DolbyVision,
    /// The HDR10-compatible base layer is played.
    Hdr10Base,
    /// The file has no HDR10-compatible base layer, so its SDR base layer is played.
    SdrBase,
}

/// Choose the layer to play.
///
/// Default policy: when Dolby Vision is present and the display is not Dolby Vision capable,
/// play the HDR10-compatible base layer if the file has one, and the SDR base layer otherwise.
/// The Dolby Vision enhancement is chosen only on a capable display with the `dovi` preference.
/// `display_dv_capable` must be set only when a Dolby Vision output path has been measured.
pub fn dolby_vision_layer(
    dv_present: bool,
    display_dv_capable: bool,
    hdr10_base_available: bool,
    preference: DvPreference,
) -> DolbyVisionLayer {
    if !dv_present {
        return DolbyVisionLayer::NotDolbyVision;
    }
    if display_dv_capable && preference == DvPreference::Dovi {
        return DolbyVisionLayer::DolbyVision;
    }
    if hdr10_base_available {
        DolbyVisionLayer::Hdr10Base
    } else {
        DolbyVisionLayer::SdrBase
    }
}

// ---------------------------------------------------------------------------
// Frame stepping, chapters, and A-B loop commands
// ---------------------------------------------------------------------------

/// Direction of a chapter jump.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChapterStep {
    Next,
    Previous,
}

/// The mpv command for one frame step. mpv pauses playback when a step is taken.
pub fn frame_step_command(backward: bool) -> &'static str {
    if backward {
        "frame-back-step"
    } else {
        "frame-step"
    }
}

/// The mpv command arguments for a chapter jump, relative to the current chapter.
pub fn chapter_step_arguments(step: ChapterStep) -> [&'static str; 3] {
    match step {
        ChapterStep::Next => ["add", "chapter", "1"],
        ChapterStep::Previous => ["add", "chapter", "-1"],
    }
}

/// The mpv command that cycles the A-B loop: set A, then B, then clear both.
pub const AB_LOOP_CYCLE_ARGUMENTS: [&str; 1] = ["ab-loop"];

#[cfg(test)]
mod tests {
    use super::*;
    use kattri_client_core::audio_device::AudioDevice;
    use kattri_client_core::audio_filters::{Normalization, filter_chain};
    use kattri_client_core::mpv_options::{OptionValue, validate};

    fn item(id: &str, album: Option<&str>) -> NextQueueItem {
        NextQueueItem {
            id: id.to_owned(),
            album_id: album.map(str::to_owned),
        }
    }

    // --- Gapless preload -------------------------------------------------------------

    #[test]
    fn preload_waits_until_fifteen_seconds_remain() {
        let mut gate = PreloadGate::new();
        let next = item("track-2", Some("album-1"));
        assert_eq!(gate.next_to_preload(Some(120.0), Some(&next)), None);
        assert_eq!(gate.next_to_preload(Some(15.0), Some(&next)), None);
        assert_eq!(
            gate.next_to_preload(Some(14.999), Some(&next)),
            Some("track-2".to_owned())
        );
    }

    #[test]
    fn preload_returns_the_same_item_only_once() {
        let mut gate = PreloadGate::new();
        let next = item("track-2", Some("album-1"));
        assert_eq!(
            gate.next_to_preload(Some(10.0), Some(&next)),
            Some("track-2".to_owned())
        );
        for remaining in [9.0, 5.0, 0.5, -1.0] {
            assert_eq!(gate.next_to_preload(Some(remaining), Some(&next)), None);
        }
    }

    #[test]
    fn preload_needs_a_known_remaining_time_and_a_next_item() {
        let mut gate = PreloadGate::new();
        let next = item("track-2", None);
        assert_eq!(gate.next_to_preload(None, Some(&next)), None);
        assert_eq!(gate.next_to_preload(Some(f64::NAN), Some(&next)), None);
        assert_eq!(gate.next_to_preload(Some(3.0), None), None);
    }

    #[test]
    fn preload_reset_allows_a_new_queue_to_preload_again() {
        let mut gate = PreloadGate::new();
        let next = item("track-2", None);
        assert!(gate.next_to_preload(Some(3.0), Some(&next)).is_some());
        gate.reset();
        assert_eq!(
            gate.next_to_preload(Some(3.0), Some(&next)),
            Some("track-2".to_owned())
        );
    }

    #[test]
    fn preload_moves_to_each_new_item_in_turn() {
        let mut gate = PreloadGate::new();
        let first = item("track-2", None);
        let second = item("track-3", None);
        assert_eq!(
            gate.next_to_preload(Some(5.0), Some(&first)),
            Some("track-2".to_owned())
        );
        assert_eq!(
            gate.next_to_preload(Some(5.0), Some(&second)),
            Some("track-3".to_owned())
        );
    }

    #[test]
    fn gapless_is_set_only_for_the_same_album() {
        let same = item("track-2", Some("album-1"));
        let other = item("track-9", Some("album-2"));
        let unknown = item("track-10", None);
        assert_eq!(
            gapless_audio_command(Some("album-1"), Some(&same)),
            Some(["set", "gapless-audio", "yes"])
        );
        assert_eq!(gapless_audio_command(Some("album-1"), Some(&other)), None);
        assert_eq!(gapless_audio_command(Some("album-1"), Some(&unknown)), None);
        assert_eq!(gapless_audio_command(None, Some(&same)), None);
        assert_eq!(gapless_audio_command(Some("album-1"), None), None);
    }

    #[test]
    fn gapless_flag_is_written_as_yes_by_the_allowlist() {
        let checked = validate("gapless-audio", OptionValue::Flag(true)).expect("flag");
        assert_eq!(checked.value, "yes");
    }

    // --- Write ledger ----------------------------------------------------------------

    #[test]
    fn ledger_writes_only_changed_values() {
        let mut ledger = OptionWriteLedger::new();
        let first = vec![
            ("sub-font", "Arial".to_owned()),
            ("sub-scale", "1".to_owned()),
        ];
        assert_eq!(ledger.changed_writes(&first), first);
        let second = vec![
            ("sub-font", "Arial".to_owned()),
            ("sub-scale", "1.5".to_owned()),
        ];
        assert_eq!(
            ledger.changed_writes(&second),
            vec![("sub-scale", "1.5".to_owned())]
        );
        assert_eq!(ledger.applied("sub-scale"), Some("1.5"));
        assert!(ledger.changed_writes(&second).is_empty());
    }

    // --- Subtitle style ----------------------------------------------------------------

    #[test]
    fn subtitle_style_pairs_have_the_documented_names_and_order() {
        use kattri_client_core::subtitle_style::{RgbaColor, SubtitleStyle};
        let style = SubtitleStyle::new(
            "Arial",
            32,
            RgbaColor::new(0xFF, 0xFF, 0xFF, 0xFF),
            RgbaColor::new(0x00, 0x00, 0x00, 0xFF),
            2,
            50,
        )
        .expect("valid style");
        let names: Vec<&str> = style
            .to_mpv_options()
            .iter()
            .map(|(name, _)| *name)
            .collect();
        assert_eq!(
            names,
            [
                "sub-font",
                "sub-scale",
                "sub-color",
                "sub-border-color",
                "sub-border-size",
                "sub-back-color",
            ]
        );
    }

    #[test]
    fn subtitle_style_pairs_pass_the_allowlist() {
        use kattri_client_core::subtitle_style::{RgbaColor, SubtitleStyle};
        let style = SubtitleStyle::new(
            "Arial",
            16,
            RgbaColor::new(0x12, 0x34, 0x56, 0x78),
            RgbaColor::new(0x00, 0x00, 0x00, 0xFF),
            10,
            0,
        )
        .expect("valid style");
        for (name, value) in style.to_mpv_options() {
            let typed = match name {
                "sub-scale" => OptionValue::Float(value.parse().expect("scale")),
                "sub-border-size" => OptionValue::Int(value.parse().expect("border")),
                _ => OptionValue::Text(&value),
            };
            let checked = validate(name, typed);
            assert!(checked.is_ok(), "{name}: {checked:?}");
        }
    }

    #[test]
    fn subtitle_style_scale_is_size_over_reference_and_colors_are_argb() {
        use kattri_client_core::subtitle_style::{RgbaColor, SubtitleStyle};
        let style = SubtitleStyle::new(
            "Arial",
            16,
            RgbaColor::new(0x12, 0x34, 0x56, 0x78),
            RgbaColor::new(0x00, 0x00, 0x00, 0xFF),
            3,
            100,
        )
        .expect("valid style");
        let pairs = style.to_mpv_options();
        let value = |name: &str| {
            pairs
                .iter()
                .find(|(pair_name, _)| *pair_name == name)
                .map(|(_, value)| value.clone())
        };
        assert_eq!(value("sub-scale").as_deref(), Some("0.5"));
        assert_eq!(value("sub-color").as_deref(), Some("#78123456"));
        assert_eq!(value("sub-border-size").as_deref(), Some("3"));
        assert_eq!(value("sub-back-color").as_deref(), Some("#FF000000"));
    }

    // --- ASS rendering -----------------------------------------------------------------

    #[test]
    fn ass_pairs_enable_ass_and_set_the_scale_choice() {
        assert_eq!(
            ass_rendering_pairs(true, None),
            vec![
                ("sub-ass", "yes".to_owned()),
                (
                    "sub-ass-style-overrides",
                    "ScaledBorderAndShadow=yes".to_owned()
                ),
            ]
        );
        assert_eq!(
            ass_rendering_pairs(false, None)[1],
            (
                "sub-ass-style-overrides",
                "ScaledBorderAndShadow=no".to_owned()
            )
        );
    }

    #[test]
    fn ass_override_is_written_only_for_documented_values() {
        for value in ["no", "yes", "force", "scale", "strip"] {
            let pairs = ass_rendering_pairs(true, Some(value));
            assert_eq!(pairs.last(), Some(&("sub-ass-override", value.to_owned())));
        }
        for bad in ["", "--config=/x", "force;vo=null", "FORCE", "banana"] {
            let pairs = ass_rendering_pairs(true, Some(bad));
            assert!(
                pairs.iter().all(|(name, _)| *name != "sub-ass-override"),
                "{bad:?} was written"
            );
        }
    }

    // --- Audio output --------------------------------------------------------------------

    fn flat_eq() -> Equalizer {
        Equalizer::from_gains([0.0; 10]).expect("flat")
    }

    fn boosted_eq() -> Equalizer {
        Equalizer::from_gains([3.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]).expect("gains")
    }

    fn no_filters() -> AudioFilterSettings {
        AudioFilterSettings {
            normalization: Normalization::new(false, -16.0).expect("target"),
            pitch_correction: true,
            compression: false,
        }
    }

    #[test]
    fn audio_filter_value_is_none_when_nothing_is_enabled() {
        assert_eq!(audio_filter_value(&flat_eq(), &no_filters()), None);
    }

    #[test]
    fn audio_filter_value_joins_the_equalizer_and_the_chain() {
        let filters = AudioFilterSettings {
            normalization: Normalization::new(true, -16.0).expect("target"),
            pitch_correction: true,
            compression: false,
        };
        let value = audio_filter_value(&boosted_eq(), &filters).expect("filters");
        let eq = boosted_eq().to_filter_graph().expect("eq");
        let chain = filter_graph(&filters).expect("chain");
        assert_eq!(value, format!("lavfi=[{eq},{chain}]"));
        assert_eq!(
            value.matches("lavfi=").count(),
            1,
            "one graph, not two filters"
        );
    }

    #[test]
    fn audio_filter_value_has_no_empty_segments() {
        let eq_only = audio_filter_value(&boosted_eq(), &no_filters()).expect("eq only");
        assert!(eq_only.starts_with("lavfi=["));
        assert!(!eq_only.contains(",,"));
        assert!(!eq_only.ends_with(','));
        assert!(!eq_only.starts_with(','));
        let compressed = AudioFilterSettings {
            normalization: Normalization::new(false, -16.0).expect("target"),
            pitch_correction: true,
            compression: true,
        };
        let chain_only = audio_filter_value(&flat_eq(), &compressed).expect("chain only");
        assert_eq!(Some(chain_only.clone()), filter_chain(&compressed));
        assert!(!chain_only.contains(",,"));
    }

    #[test]
    fn audio_device_pair_is_auto_without_a_selection() {
        let mut list =
            AudioDeviceList::new(vec![AudioDevice::new("hdmi:0", "HDMI").expect("device")])
                .expect("list");
        assert_eq!(
            audio_device_pair(&list),
            ("audio-device", "auto".to_owned())
        );
        list.select("hdmi:0").expect("select");
        assert_eq!(
            audio_device_pair(&list),
            ("audio-device", "hdmi:0".to_owned())
        );
    }

    // --- Hardware decode -----------------------------------------------------------------

    #[test]
    fn every_decode_mode_maps_to_its_hwdec_value() {
        let table = [
            ("auto", "auto"),
            ("off", "no"),
            ("d3d11va", "d3d11va"),
            ("nvdec", "nvdec"),
            ("vaapi", "vaapi"),
            ("videotoolbox", "videotoolbox"),
        ];
        for (mode, expected) in table {
            assert_eq!(hwdec_for_decode_mode(mode), Some(expected), "{mode}");
        }
    }

    #[test]
    fn unknown_decode_mode_maps_to_nothing() {
        for bad in ["", "AUTO", "copy", "auto-copy", "dxva2", " vaapi"] {
            assert_eq!(hwdec_for_decode_mode(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn vulkan_has_no_hwdec_value_because_the_allowlist_refuses_it() {
        assert_eq!(hwdec_for_decode_mode("vulkan"), None);
        assert!(validate("hwdec", OptionValue::Text("vulkan")).is_err());
    }

    #[test]
    fn every_mapped_decode_mode_passes_the_hwdec_allowlist() {
        for mode in ["auto", "off", "d3d11va", "nvdec", "vaapi", "videotoolbox"] {
            let value = hwdec_for_decode_mode(mode).unwrap_or_default();
            assert!(
                validate("hwdec", OptionValue::Text(value)).is_ok(),
                "{mode} -> {value} refused by the hwdec allowlist"
            );
        }
    }

    // --- HDR output ----------------------------------------------------------------------

    #[test]
    fn hdr_plan_table() {
        let table = [
            (false, false, false, HdrOutputPlan::SdrSource),
            (false, true, true, HdrOutputPlan::SdrSource),
            (true, true, true, HdrOutputPlan::HdrPassthrough),
            (true, true, false, HdrOutputPlan::ToneMapToHdrDisplay),
            (true, false, true, HdrOutputPlan::ToneMapToSdr),
            (true, false, false, HdrOutputPlan::ToneMapToSdr),
        ];
        for (source, display, passthrough, expected) in table {
            assert_eq!(
                hdr_output_plan(source, display, passthrough),
                expected,
                "source={source} display={display} passthrough={passthrough}"
            );
        }
    }

    #[test]
    fn hdr_pairs_for_each_plan() {
        assert!(hdr_output_pairs(HdrOutputPlan::SdrSource).is_empty());
        assert_eq!(
            hdr_output_pairs(HdrOutputPlan::HdrPassthrough),
            vec![
                ("target-colorspace-hint", "yes"),
                ("hdr-compute-peak", "yes")
            ]
        );
        let sdr = hdr_output_pairs(HdrOutputPlan::ToneMapToSdr);
        assert!(sdr.contains(&("tone-mapping", "auto")));
        assert_eq!(sdr, hdr_output_pairs(HdrOutputPlan::ToneMapToHdrDisplay));
    }

    #[test]
    fn tone_mapped_plans_never_request_colorspace_passthrough() {
        for plan in [
            HdrOutputPlan::ToneMapToSdr,
            HdrOutputPlan::ToneMapToHdrDisplay,
        ] {
            assert!(
                hdr_output_pairs(plan)
                    .iter()
                    .all(|pair| *pair != ("target-colorspace-hint", "yes")),
                "{plan:?}"
            );
        }
    }

    // --- Refresh rate --------------------------------------------------------------------

    #[test]
    fn refresh_plan_table_for_common_rates() {
        let displays = [30.0, 50.0, 60.0];
        let cases: [(f64, &[f64], RefreshPlan); 6] = [
            (
                23.976,
                &[24.0, 60.0],
                RefreshPlan::VideoSync("display-resample"),
            ),
            (24.0, &[60.0], RefreshPlan::VideoSync("audio")),
            (25.0, &displays, RefreshPlan::VideoSync("display-resample")),
            (29.97, &[60.0], RefreshPlan::VideoSync("display-resample")),
            (
                50.0,
                &[50.0, 60.0],
                RefreshPlan::VideoSync("display-resample"),
            ),
            (59.94, &[60.0], RefreshPlan::VideoSync("display-resample")),
        ];
        for (fps, rates, expected) in cases {
            assert_eq!(
                refresh_plan(Some(fps), rates, true, false, None),
                expected,
                "fps={fps} rates={rates:?}"
            );
        }
    }

    #[test]
    fn avoid_25_30_hz_moves_those_content_rates_to_their_double() {
        assert_eq!(
            refresh_plan(Some(25.0), &[25.0, 50.0], true, true, None),
            RefreshPlan::VideoSync("display-resample")
        );
        assert_eq!(
            refresh_plan(Some(25.0), &[25.0, 60.0], true, true, None),
            RefreshPlan::VideoSync("audio")
        );
        assert_eq!(
            refresh_plan(Some(25.0), &[25.0, 60.0], true, false, None),
            RefreshPlan::VideoSync("display-resample")
        );
    }

    #[test]
    fn refresh_plan_keeps_settings_when_auto_switch_is_off_or_rates_are_unknown() {
        assert_eq!(
            refresh_plan(Some(24.0), &[24.0], false, true, None),
            RefreshPlan::Keep
        );
        assert_eq!(
            refresh_plan(None, &[24.0], true, true, None),
            RefreshPlan::Keep
        );
        assert_eq!(
            refresh_plan(Some(0.0), &[24.0], true, true, None),
            RefreshPlan::Keep
        );
        assert_eq!(
            refresh_plan(Some(f64::NAN), &[24.0], true, true, None),
            RefreshPlan::Keep
        );
        assert_eq!(
            refresh_plan(Some(24.0), &[], true, true, None),
            RefreshPlan::Keep
        );
    }

    #[test]
    fn manual_override_takes_precedence_and_zero_means_none() {
        assert_eq!(
            refresh_plan(Some(24.0), &[60.0], false, true, Some(48.0)),
            RefreshPlan::DisplayFpsOverride(48.0)
        );
        assert_eq!(
            refresh_plan(Some(24.0), &[24.0], true, true, Some(0.0)),
            RefreshPlan::VideoSync("display-resample")
        );
        assert_eq!(
            refresh_plan(Some(24.0), &[24.0], true, true, Some(f64::INFINITY)),
            RefreshPlan::VideoSync("display-resample")
        );
    }

    #[test]
    fn override_range_is_inside_the_allowlist_range() {
        assert!(validate("display-fps-override", OptionValue::Float(48.0)).is_ok());
    }

    // --- Dolby Vision --------------------------------------------------------------------

    #[test]
    fn dolby_vision_table() {
        let table = [
            (
                false,
                false,
                false,
                DvPreference::Auto,
                DolbyVisionLayer::NotDolbyVision,
            ),
            (
                false,
                true,
                true,
                DvPreference::Dovi,
                DolbyVisionLayer::NotDolbyVision,
            ),
            (
                true,
                false,
                true,
                DvPreference::Auto,
                DolbyVisionLayer::Hdr10Base,
            ),
            (
                true,
                false,
                false,
                DvPreference::Auto,
                DolbyVisionLayer::SdrBase,
            ),
            (
                true,
                false,
                true,
                DvPreference::Dovi,
                DolbyVisionLayer::Hdr10Base,
            ),
            (
                true,
                false,
                false,
                DvPreference::Hdr10,
                DolbyVisionLayer::SdrBase,
            ),
            (
                true,
                true,
                true,
                DvPreference::Dovi,
                DolbyVisionLayer::DolbyVision,
            ),
            (
                true,
                true,
                true,
                DvPreference::Auto,
                DolbyVisionLayer::Hdr10Base,
            ),
            (
                true,
                true,
                false,
                DvPreference::Dovi,
                DolbyVisionLayer::DolbyVision,
            ),
        ];
        for (present, capable, base, preference, expected) in table {
            assert_eq!(
                dolby_vision_layer(present, capable, base, preference),
                expected,
                "present={present} capable={capable} base={base} preference={preference:?}"
            );
        }
    }

    #[test]
    fn dolby_vision_is_never_chosen_on_a_display_that_is_not_capable() {
        for base in [true, false] {
            for preference in [DvPreference::Auto, DvPreference::Dovi, DvPreference::Hdr10] {
                assert_ne!(
                    dolby_vision_layer(true, false, base, preference),
                    DolbyVisionLayer::DolbyVision
                );
            }
        }
    }

    // --- Frame, chapter, and A-B loop commands -----------------------------------------

    #[test]
    fn frame_step_commands() {
        assert_eq!(frame_step_command(false), "frame-step");
        assert_eq!(frame_step_command(true), "frame-back-step");
    }

    #[test]
    fn chapter_step_arguments_are_relative_adds() {
        assert_eq!(
            chapter_step_arguments(ChapterStep::Next),
            ["add", "chapter", "1"]
        );
        assert_eq!(
            chapter_step_arguments(ChapterStep::Previous),
            ["add", "chapter", "-1"]
        );
    }

    #[test]
    fn ab_loop_cycle_is_one_command() {
        assert_eq!(AB_LOOP_CYCLE_ARGUMENTS, ["ab-loop"]);
    }
}
