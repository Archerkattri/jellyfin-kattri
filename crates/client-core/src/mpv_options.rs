//! Allowlist of mpv options that settings may drive (M2-20).
//!
//! Every setting that reaches libmpv goes through [`validate`]. The table [`ALLOWLIST`] is the
//! only list of properties the product may set from settings. A name that is not in the table,
//! or a value that does not fit its [`ValueKind`], is refused with a typed [`MpvOptionError`].
//! Writes that are not settings, such as the one-shot `sid` and `aid` track selectors, live in
//! [`ONE_SHOT_ALLOWLIST`]. [`validate`] refuses them with `OneShotOnly`; only
//! [`validate_one_shot`], [`validate_audio_track`], and [`validate_subtitle_track`] accept them.
//! Errors carry the option name and a reason, never the value, so a path or font name cannot
//! reach a log line through an error.
//!
//! Scope: settings-driven properties from M2-04 to M2-22 whose names already appear in the repo
//! (`legacy/qt-neo/src/player/PlayerComponent.cpp` or `proofs/mpv-render/src`). Left out on
//! purpose:
//! - Fixed initialization options in `proofs/mpv-render/src/session.rs` (`config`,
//!   `load-scripts`, `vo`, `ao`, redirect limits). They are app constants, not settings.
//! - Runtime playback commands (`pause`, `audio-delay`). They are not settings. `speed` is
//!   listed only for the saved default speed that is written when a file opens.
//!   `sub-delay` is the exception: M2-11 routes the subtitle offset through this table
//!   (`SubtitleOffset::as_mpv_seconds` in subtitle_sync.rs).
//! - Legacy settings whose M2 task is not in M2-04..M2-22 (`tls-verify`, `tls-ca-file`,
//!   `ytdl`, `audio-normalize-downmix`, `video-unscaled`, `ass` overrides).
//!
//! Names are the ones the repo uses. Any name not confirmed against the pinned libmpv is marked
//! [UNCONFIRMED] in its table comment. This module is pure: it validates and formats, and it
//! does no I/O.

use std::fmt;

use kattri_settings::schema::{AspectMode, AudioSettings, ChannelLayout, DecodeMode, SyncMode};

use crate::audio_filters::{
    AudioFilterSettings, DEFAULT_LOUDNESS_TARGET_LUFS, Normalization, filter_graph,
};
use crate::equalizer::{BAND_COUNT, Equalizer};

use crate::cache::{
    BACKBUFFER_MB_MAX, CACHE_MB_MAX, CACHE_MB_MIN, READAHEAD_SECS_MAX, mib_to_bytes,
};
use crate::subtitle_style::{FONT_SIZE_PT_MAX, FONT_SIZE_PT_MIN, size_and_color_options};
use crate::subtitle_sync::SUB_OFFSET_LIMIT_MS;
use crate::volume::VOLUME_BOOST_MAX_PERCENT;

/// Longest text value the allowlist accepts, in bytes.
pub const MAX_TEXT_LEN: usize = 1024;

/// Largest width or height accepted in a `video-aspect-override` ratio.
pub const ASPECT_RATIO_PART_MAX: u32 = 10_000;

/// `sub-pos` for a bottom placement, as legacy writes it (PlayerComponent.cpp:1493).
const SUB_POS_BOTTOM: i64 = 100;
/// `sub-pos` for a top placement, as legacy writes it (PlayerComponent.cpp:1493).
const SUB_POS_TOP: i64 = 10;

/// The value shape an option accepts.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ValueKind {
    /// mpv flag. Sent as `yes` or `no`.
    Flag,
    /// Integer in the inclusive range `min..=max`.
    Int { min: i64, max: i64 },
    /// Finite float in the inclusive range `min..=max`.
    Float { min: f64, max: f64 },
    /// One of a fixed set of spellings.
    Enum(&'static [&'static str]),
    /// Free text: font names, colors, paths, templates, codec lists. Rejected when it is longer
    /// than [`MAX_TEXT_LEN`], starts with `-`, or contains a control character (including
    /// newlines). The leading-dash rule stops a value from being read as an option.
    Text,
    /// A subtitle or audio track selector: a positive integer track id, or the text `no` to
    /// turn the track off. Track ids in mpv start at 1, so 0 and negative ids are refused.
    TrackId,
    /// An audio track selector: `auto`, `no`, or a positive track id. The id must also be in the
    /// current inventory; [`validate_audio_track`] checks that, because this check has no inventory.
    AudioTrack,
    /// A subtitle track selector for `sid`: `auto`, `no`, or a positive track id. The id is
    /// checked against the inventory by [`validate_subtitle_track`]. `auto` is mpv's default
    /// selection, and the menu offers it as "Automatic".
    SubtitleTrack,
    /// The `video-aspect-override` value: `-1` for off, or a ratio `W:H` of two whole numbers,
    /// each from 1 to [`ASPECT_RATIO_PART_MAX`]. Decimal ratios are refused.
    AspectRatio,
    /// The `af` value: empty (no filters) or `lavfi=[...]` holding only the filters this client
    /// builds ([`AUDIO_FILTER_NAMES`]) with plain `key=value` parameters. Anything that could
    /// open a file, a source, or another graph is refused.
    AudioFilterChain,
    /// The `audio-device` value: `auto`, or `<backend>/<id>` as mpv lists it (for example
    /// `wasapi/{GUID}` or `alsa/default`).
    AudioDevice,
}

/// Filters that [`ValueKind::AudioFilterChain`] accepts: the loudness normalizer, the dynamic
/// range compressor, and the equalizer bands.
pub const AUDIO_FILTER_NAMES: &[&str] = &["loudnorm", "acompressor", "equalizer"];

/// Most filters one chain may hold. The ten equalizer bands plus the two processing filters.
pub const AUDIO_FILTER_COUNT_MAX: usize = 12;

/// Longest `audio-device` value, in bytes.
pub const AUDIO_DEVICE_LEN_MAX: usize = 200;

/// A typed value offered for an option. The variant must match the option's [`ValueKind`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum OptionValue<'a> {
    Flag(bool),
    Int(i64),
    Float(f64),
    Text(&'a str),
}

/// One allowlist entry.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OptionSpec {
    /// The mpv property name, as the repo spells it.
    pub name: &'static str,
    /// The value shape this option accepts.
    pub kind: ValueKind,
    /// The setting key (or the M2 task) that drives this option.
    pub owner: &'static str,
    /// A value the entry must accept. The guard test checks it against `kind`.
    pub default: OptionValue<'static>,
}

/// A value that passed validation, already in the spelling mpv takes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidatedOption {
    pub name: &'static str,
    pub value: String,
}

/// Why a name or value was refused. None of the variants carries the value itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MpvOptionError {
    /// The name is not in [`ALLOWLIST`].
    UnknownOption { name: String },
    /// The value variant does not match the option kind (for example a string for a flag).
    TypeMismatch {
        name: &'static str,
        expected: &'static str,
    },
    /// A number is outside the allowed range, or a float is not finite.
    OutOfRange { name: &'static str },
    /// A text value is not one of the allowed spellings.
    NotAllowedValue { name: &'static str },
    /// A text value failed the safety rules. `reason` is a fixed phrase.
    UnsafeText {
        name: &'static str,
        reason: &'static str,
    },
    /// The name is a one-shot write in [`ONE_SHOT_ALLOWLIST`]. It cannot come from settings.
    OneShotOnly { name: &'static str },
    /// A track id is well formed but is not in the current track inventory.
    NotInInventory { name: &'static str },
}

impl fmt::Display for MpvOptionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownOption { name } => {
                write!(f, "mpv option `{name}` is not on the allowlist")
            }
            Self::TypeMismatch { name, expected } => {
                write!(f, "mpv option `{name}` expects a {expected} value")
            }
            Self::OutOfRange { name } => {
                write!(f, "mpv option `{name}` is outside its allowed range")
            }
            Self::NotAllowedValue { name } => {
                write!(f, "mpv option `{name}` is not an allowed value")
            }
            Self::UnsafeText { name, reason } => {
                write!(f, "mpv option `{name}` value rejected: {reason}")
            }
            Self::OneShotOnly { name } => {
                write!(
                    f,
                    "mpv option `{name}` is a one-shot write and cannot come from settings"
                )
            }
            Self::NotInInventory { name } => {
                write!(
                    f,
                    "mpv option `{name}` names a track that is not in the current inventory"
                )
            }
        }
    }
}

impl std::error::Error for MpvOptionError {}

/// Allowed mpv properties. Each entry needs a test-visible range or enum, a setting owner, and
/// a default that passes its own kind. Adding a property means adding an entry here first.
pub const ALLOWLIST: &[OptionSpec] = &[
    // --- Audio (M2-04, M2-07, M2-16) -----------------------------------------------------
    OptionSpec {
        name: "audio-spdif",
        kind: ValueKind::Text,
        owner: "audio.passthrough (M2-04)",
        default: OptionValue::Text(""),
    },
    OptionSpec {
        name: "audio-exclusive",
        kind: ValueKind::Flag,
        owner: "audio.exclusive",
        default: OptionValue::Flag(false),
    },
    OptionSpec {
        name: "gapless-audio",
        kind: ValueKind::Flag,
        owner: "audio.gapless (M2-07)",
        default: OptionValue::Flag(true),
    },
    OptionSpec {
        // Legacy `audio.normalize` is this flag (PlayerComponent.cpp:1356). It evens out the
        // level when multichannel audio is mixed down. It is not the loudnorm filter.
        name: "audio-normalize-downmix",
        kind: ValueKind::Flag,
        owner: "audio.normalize",
        default: OptionValue::Flag(true),
    },
    OptionSpec {
        // The spellings mpv takes for the four layouts in the Settings choice. `auto-safe` is
        // mpv's own default and stands for "Automatic". Names checked with `mpv --list-options`
        // (mpv 0.37); the pinned build is [UNCONFIRMED].
        name: "audio-channels",
        kind: ValueKind::Enum(&["auto-safe", "stereo", "5.1", "7.1"]),
        owner: "audio.channels",
        default: OptionValue::Text("auto-safe"),
    },
    OptionSpec {
        // Keeps the pitch when the speed changes. mpv's own default is on, so turning it off has
        // to be written explicitly (see `crate::audio_filters`).
        name: "audio-pitch-correction",
        kind: ValueKind::Flag,
        owner: "audio.pitch_correction (M6-09)",
        default: OptionValue::Flag(true),
    },
    OptionSpec {
        // Written as `auto` or as a device id chosen from mpv's own `audio-device-list`.
        name: "audio-device",
        kind: ValueKind::AudioDevice,
        owner: "audio.device",
        default: OptionValue::Text("auto"),
    },
    OptionSpec {
        // The whole chain in one value, because mpv replaces the chain on each write. Built by
        // `crate::audio_filters` and `crate::equalizer`; see `playback_policy::audio_filter_value`.
        name: "af",
        kind: ValueKind::AudioFilterChain,
        owner: "audio.normalize and the audio filters (M6-08, M6-09)",
        default: OptionValue::Text(""),
    },
    OptionSpec {
        // Owner decision 2026-10-15: maximum 300 (M2-16); the legacy clamp was 100..=200
        // (PlayerComponent.cpp:1345). The ceiling comes from crate::volume, so it is changed
        // in one place.
        name: "volume-max",
        kind: ValueKind::Int {
            min: 100,
            max: VOLUME_BOOST_MAX_PERCENT as i64,
        },
        owner: "audio.max_volume_percent (M2-16)",
        default: OptionValue::Int(100),
    },
    OptionSpec {
        name: "volume",
        kind: ValueKind::Int {
            min: 0,
            max: VOLUME_BOOST_MAX_PERCENT as i64,
        },
        owner: "M2-16 volume control",
        default: OptionValue::Int(100),
    },
    OptionSpec {
        name: "mute",
        kind: ValueKind::Flag,
        owner: "M2-16 session mute (not persisted)",
        default: OptionValue::Flag(false),
    },
    // --- Playback buffers and cache (M2-06) ----------------------------------------------
    OptionSpec {
        // Bytes. Settings hold MiB, 16..=4096 (legacy PlayerComponent.cpp:1638). The range
        // comes from crate::cache so the limits live in one place.
        name: "demuxer-max-bytes",
        kind: ValueKind::Int {
            min: mib_to_bytes(CACHE_MB_MIN) as i64,
            max: mib_to_bytes(CACHE_MB_MAX) as i64,
        },
        owner: "playback.cache_mb (M2-06)",
        default: OptionValue::Int(mib_to_bytes(75) as i64),
    },
    OptionSpec {
        // Bytes. Settings hold MiB, 0..=1024 (legacy PlayerComponent.cpp:1641).
        name: "demuxer-max-back-bytes",
        kind: ValueKind::Int {
            min: 0,
            max: mib_to_bytes(BACKBUFFER_MB_MAX) as i64,
        },
        owner: "playback.demuxer_backbuffer_mb (M2-06)",
        default: OptionValue::Int(mib_to_bytes(50) as i64),
    },
    OptionSpec {
        name: "demuxer-readahead-secs",
        kind: ValueKind::Int {
            min: 0,
            max: READAHEAD_SECS_MAX as i64,
        },
        owner: "playback.demuxer_readahead_secs (M2-06)",
        default: OptionValue::Int(5),
    },
    // --- Subtitles (M2-09, M2-11) --------------------------------------------------------
    OptionSpec {
        // Secondary track selector: a track id, or "no" for off. Legacy writes it through
        // streamIdPropertyName (PlayerComponent.cpp:1040). [UNCONFIRMED against the pinned mpv
        // track-id type]
        name: "secondary-sid",
        kind: ValueKind::TrackId,
        owner: "subtitles.secondary_track (M2-09)",
        default: OptionValue::Text("no"),
    },
    OptionSpec {
        name: "secondary-sub-pos",
        kind: ValueKind::Int { min: 0, max: 100 },
        owner: "subtitles.secondary_position_percent (M2-09)",
        default: OptionValue::Int(10),
    },
    OptionSpec {
        name: "sub-pos",
        kind: ValueKind::Int { min: 0, max: 100 },
        owner: "subtitles.placement",
        default: OptionValue::Int(100),
    },
    OptionSpec {
        // Legacy placement strings are left, center, or right (settings_description.json).
        name: "sub-align-x",
        kind: ValueKind::Enum(&["left", "center", "right"]),
        owner: "subtitles.placement",
        default: OptionValue::Text("center"),
    },
    OptionSpec {
        // Scale factor. Legacy uses font size / 32 (PlayerComponent.cpp:1452). The range is a
        // choice made here, not from the spec. [UNCONFIRMED against the pinned mpv range]
        name: "sub-scale",
        kind: ValueKind::Float { min: 0.1, max: 4.0 },
        owner: "subtitles.size",
        default: OptionValue::Float(1.0),
    },
    OptionSpec {
        name: "sub-font",
        kind: ValueKind::Text,
        owner: "subtitles.font",
        default: OptionValue::Text("sans-serif"),
    },
    OptionSpec {
        name: "sub-color",
        kind: ValueKind::Text,
        owner: "subtitles.color",
        default: OptionValue::Text("#FFFFFF"),
    },
    OptionSpec {
        name: "sub-border-color",
        kind: ValueKind::Text,
        owner: "subtitles.border_color",
        default: OptionValue::Text("#000000"),
    },
    OptionSpec {
        name: "sub-border-size",
        kind: ValueKind::Int { min: 0, max: 10 },
        owner: "subtitles.border_size",
        default: OptionValue::Int(2),
    },
    OptionSpec {
        name: "sub-back-color",
        kind: ValueKind::Text,
        owner: "subtitles.background_color",
        default: OptionValue::Text("#00000000"),
    },
    OptionSpec {
        // Legacy writes `ScaledBorderAndShadow=yes|no` from `subtitles.ass_scale_border_and_shadow`
        // (PlayerComponent.cpp:1441). Checked with `mpv --list-options` and a runtime `set` on
        // mpv 0.37.
        name: "sub-ass-style-overrides",
        kind: ValueKind::Enum(&["ScaledBorderAndShadow=yes", "ScaledBorderAndShadow=no"]),
        owner: "subtitles.ass_scale_border_and_shadow",
        default: OptionValue::Text("ScaledBorderAndShadow=yes"),
    },
    OptionSpec {
        // Legacy writes the stored string when it is not empty (PlayerComponent.cpp:1446). Only the
        // five spellings mpv documents are accepted.
        name: "sub-ass-override",
        kind: ValueKind::Enum(&["no", "yes", "force", "scale", "strip"]),
        owner: "subtitles.ass_style_override",
        default: OptionValue::Text("yes"),
    },
    OptionSpec {
        // Signed seconds. A positive value delays subtitles. The ±5 s range is the product choice
        // made in subtitle_sync.rs (SubtitleOffset clamps to ±5000 ms, SUB_OFFSET_LIMIT_MS), not an
        // mpv limit. The bound is taken from that constant so the two cannot drift apart.
        name: "sub-delay",
        kind: ValueKind::Float {
            min: -(SUB_OFFSET_LIMIT_MS as f64) / 1000.0,
            max: SUB_OFFSET_LIMIT_MS as f64 / 1000.0,
        },
        owner: "subtitles.offset (M2-11)",
        default: OptionValue::Float(0.0),
    },
    // --- Video decode and sync (M2-22) and video layout (M2-15, M2-17 area) -------------
    OptionSpec {
        // Legacy values: no, auto, auto-copy (PlayerComponent.cpp:1600-1613). The named backends
        // are the M2-22 settings values (docs/spec/M2.md, `hwdec` enum). Vulkan has no entry here
        // because the spec does not name it; see hwdec_for_decode_mode.
        name: "hwdec",
        kind: ValueKind::Enum(&[
            "no",
            "auto",
            "auto-copy",
            "d3d11va",
            "dxva2",
            "nvdec",
            "vaapi",
            "videotoolbox",
        ]),
        owner: "video.hardware_decoding (M2-22)",
        default: OptionValue::Text("auto-copy"),
    },
    OptionSpec {
        // Comma-separated codec list. Legacy default is "all" (PlayerComponent.cpp:177).
        name: "hwdec-codecs",
        kind: ValueKind::Text,
        owner: "video.hardware_decoding",
        default: OptionValue::Text("all"),
    },
    OptionSpec {
        // Legacy values: audio, display-resample, display-adrop (settings_description.json).
        name: "video-sync",
        kind: ValueKind::Enum(&["audio", "display-resample", "display-adrop"]),
        owner: "playback.sync_mode (M2-22)",
        default: OptionValue::Text("display-resample"),
    },
    OptionSpec {
        // `vo` drops late frames at output (mpv's default); `no` keeps every frame. The other
        // choices (`decoder`, `decoder+vo`) are not offered in Settings.
        name: "framedrop",
        kind: ValueKind::Enum(&["vo", "no"]),
        owner: "playback.frame_drop",
        default: OptionValue::Text("vo"),
    },
    OptionSpec {
        // The saved default speed, written when a file opens. The range is the Settings range
        // (kattri-settings validates 0.25..=4.0). Speed changes during playback do not use this.
        name: "speed",
        kind: ValueKind::Float {
            min: 0.25,
            max: 4.0,
        },
        owner: "playback.default_speed",
        default: OptionValue::Float(1.0),
    },
    OptionSpec {
        name: "deinterlace",
        kind: ValueKind::Flag,
        owner: "video.deinterlace (M2-22)",
        default: OptionValue::Flag(false),
    },
    OptionSpec {
        // 0 is taken to mean "no override". [UNCONFIRMED against the pinned mpv]
        name: "display-fps-override",
        kind: ValueKind::Float {
            min: 0.0,
            max: 500.0,
        },
        owner: "video.refresh_rate_auto_switch (M2-22)",
        default: OptionValue::Float(0.0),
    },
    // --- Screenshots (M2-15) -------------------------------------------------------------
    OptionSpec {
        name: "screenshot-directory",
        kind: ValueKind::Text,
        owner: "video.screenshot.directory (M2-15)",
        default: OptionValue::Text(""),
    },
    OptionSpec {
        name: "screenshot-template",
        kind: ValueKind::Text,
        owner: "video.screenshot.template (M2-15)",
        default: OptionValue::Text("jellyfin-shot%n"),
    },
    OptionSpec {
        // Legacy accepts png, jpg, webp; anything else falls back to png (PlayerComponent.cpp:1571).
        name: "screenshot-format",
        kind: ValueKind::Enum(&["png", "jpg", "webp"]),
        owner: "video.screenshot.format (M2-15)",
        default: OptionValue::Text("png"),
    },
    // --- Aspect and zoom (M2-12 area, legacy video mode) ---------------------------------
    OptionSpec {
        name: "keepaspect",
        kind: ValueKind::Flag,
        owner: "video.aspect",
        default: OptionValue::Flag(true),
    },
    OptionSpec {
        name: "panscan",
        kind: ValueKind::Float { min: 0.0, max: 1.0 },
        owner: "video.aspect",
        default: OptionValue::Float(0.0),
    },
    OptionSpec {
        // Log2 scale in mpv: -1.0 is 0.5x and 2.0 is 4x, the range the zoom setting describes.
        // [UNCONFIRMED against the pinned mpv]
        name: "video-zoom",
        kind: ValueKind::Float {
            min: -1.0,
            max: 2.0,
        },
        owner: "video.allow_zoom (M2 zoom)",
        default: OptionValue::Float(0.0),
    },
    OptionSpec {
        // -1 turns the override off; otherwise a ratio W:H. Legacy wrote "no" for off
        // (PlayerComponent.cpp:1545); -1 is the M2-20 decision.
        name: "video-aspect-override",
        kind: ValueKind::AspectRatio,
        owner: "video.aspect",
        default: OptionValue::Text("-1"),
    },
];

/// One-shot writes. These are performed once on an explicit product action and are never
/// settings, so they must not persist or come from a settings value. They are kept out of
/// [`ALLOWLIST`] on purpose: [`validate`] refuses them with `OneShotOnly`, and only
/// [`validate_one_shot`] checks them. The entries use the same shape as [`ALLOWLIST`].
pub const ONE_SHOT_ALLOWLIST: &[OptionSpec] = &[
    OptionSpec {
        // Primary track selector. The M2-21 re-select workaround writes "no", then the track id,
        // once per item (see subtitles.rs). The track menu writes "auto" for Automatic. Legacy
        // writes it through streamIdPropertyName (PlayerComponent.cpp:1035). [UNCONFIRMED against
        // the pinned mpv track-id type]
        name: SUBTITLE_TRACK_NAME,
        kind: ValueKind::SubtitleTrack,
        owner: "subtitles.primary_track (M2-09, track menu) and M2-21 re-select (one-shot)",
        default: OptionValue::Text("no"),
    },
    OptionSpec {
        // Audio track selector, written by the track menu. It is a user action, not a setting,
        // so it is one-shot. Checked by validate_audio_track against the current inventory.
        name: AUDIO_TRACK_NAME,
        kind: ValueKind::AudioTrack,
        owner: "audio.track_menu (one-shot)",
        default: OptionValue::Text("auto"),
    },
];

/// Name of the audio track selector in [`ONE_SHOT_ALLOWLIST`].
pub const AUDIO_TRACK_NAME: &str = "aid";

/// Name of the primary subtitle track selector in [`ONE_SHOT_ALLOWLIST`].
pub const SUBTITLE_TRACK_NAME: &str = "sid";

/// Look up an allowlist entry by its exact mpv name.
pub fn spec_for(name: &str) -> Option<&'static OptionSpec> {
    ALLOWLIST.iter().find(|spec| spec.name == name)
}

/// Look up a one-shot entry by its exact mpv name.
pub fn one_shot_spec_for(name: &str) -> Option<&'static OptionSpec> {
    ONE_SHOT_ALLOWLIST.iter().find(|spec| spec.name == name)
}

/// Check one option name and value against the allowlist.
///
/// Returns the value in the spelling mpv takes (`yes`/`no` for flags, decimal for numbers).
/// Floats are formatted with a dot separator, so the result does not depend on locale.
/// A one-shot name (`sid`) is refused with [`MpvOptionError::OneShotOnly`], so settings can
/// never reach it.
pub fn validate(name: &str, value: OptionValue<'_>) -> Result<ValidatedOption, MpvOptionError> {
    let Some(spec) = spec_for(name) else {
        return Err(refuse_unlisted(name));
    };
    check_spec(spec, value)
}

/// Check one one-shot write. Only names in [`ONE_SHOT_ALLOWLIST`] pass; every other name is
/// refused as unknown, so this path cannot widen the settings table.
pub fn validate_one_shot(
    name: &str,
    value: OptionValue<'_>,
) -> Result<ValidatedOption, MpvOptionError> {
    let spec = one_shot_spec_for(name).ok_or_else(|| MpvOptionError::UnknownOption {
        name: name.to_owned(),
    })?;
    check_spec(spec, value)
}

/// Check an audio track choice for `aid`: `auto`, `no`, or a track id from `audio_track_ids`.
///
/// `audio_track_ids` is the current inventory's audio track ids. An id that is not in it is refused
/// with [`MpvOptionError::NotInInventory`], so a stale or foreign id never reaches mpv.
pub fn validate_audio_track(
    value: OptionValue<'_>,
    audio_track_ids: &[i64],
) -> Result<ValidatedOption, MpvOptionError> {
    let checked = validate_one_shot(AUDIO_TRACK_NAME, value)?;
    if let OptionValue::Int(id) = value
        && !audio_track_ids.contains(&id)
    {
        return Err(MpvOptionError::NotInInventory {
            name: AUDIO_TRACK_NAME,
        });
    }
    Ok(checked)
}

/// Check a subtitle track choice for `sid`: `auto`, `no`, or a track id from `subtitle_track_ids`.
///
/// `subtitle_track_ids` is the current inventory's subtitle track ids. An id that is not in it is
/// refused with [`MpvOptionError::NotInInventory`]. The M2-21 re-select uses [`validate_one_shot`]
/// directly, because it writes an id it took from the inventory itself.
pub fn validate_subtitle_track(
    value: OptionValue<'_>,
    subtitle_track_ids: &[i64],
) -> Result<ValidatedOption, MpvOptionError> {
    let checked = validate_one_shot(SUBTITLE_TRACK_NAME, value)?;
    if let OptionValue::Int(id) = value
        && !subtitle_track_ids.contains(&id)
    {
        return Err(MpvOptionError::NotInInventory {
            name: SUBTITLE_TRACK_NAME,
        });
    }
    Ok(checked)
}

/// The audio filter chain and pitch correction writes for the equalizer and the loudness,
/// compression and pitch settings (M6-08, M6-09).
///
/// The equalizer bands and the enabled filters go into one `lavfi=[...]` graph, because mpv
/// replaces the whole `af` chain on each write. With nothing enabled `af` is written empty, which
/// clears an earlier chain. Equalizer gains that are not ten values inside the limit (the
/// settings loader refuses them, but a caller may hold an unchecked copy) count as flat.
pub fn audio_filter_options(audio: &AudioSettings) -> Result<Vec<ValidatedOption>, MpvOptionError> {
    let equalizer = <[f64; BAND_COUNT]>::try_from(
        audio
            .equalizer_gains_db
            .iter()
            .map(|gain| f64::from(*gain))
            .collect::<Vec<_>>(),
    )
    .ok()
    .and_then(|gains| Equalizer::from_gains(gains).ok())
    .unwrap_or_else(Equalizer::flat);
    let normalization = Normalization::new(
        audio.loudness_normalization,
        f64::from(audio.loudness_target_lufs),
    )
    .or_else(|_| Normalization::new(audio.loudness_normalization, DEFAULT_LOUDNESS_TARGET_LUFS))
    .unwrap_or_default();
    let filters = AudioFilterSettings {
        normalization,
        pitch_correction: audio.pitch_correction,
        compression: audio.dynamic_range_compression,
    };
    let graph = [equalizer.to_filter_graph(), filter_graph(&filters)]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(",");
    let chain = if graph.is_empty() {
        String::new()
    } else {
        format!("lavfi=[{graph}]")
    };
    Ok(vec![
        validate("af", OptionValue::Text(&chain))?,
        validate(
            "audio-pitch-correction",
            OptionValue::Flag(filters.pitch_correction),
        )?,
    ])
}

/// The mpv `audio-channels` value for an `audio.channels` setting. `Auto` is mpv's own
/// `auto-safe`. Every result passes the `audio-channels` allowlist entry.
pub const fn audio_channels_for_layout(layout: ChannelLayout) -> &'static str {
    match layout {
        ChannelLayout::Auto => "auto-safe",
        ChannelLayout::Stereo => "stereo",
        ChannelLayout::Surround51 => "5.1",
        ChannelLayout::Surround71 => "7.1",
    }
}

/// The mpv `framedrop` value for the `playback.frame_drop` setting.
pub const fn framedrop_for_setting(frame_drop: bool) -> &'static str {
    if frame_drop { "vo" } else { "no" }
}

/// The saved default speed as an mpv `speed` value: the setting is an `f32`, so it is rounded to
/// two decimals (0.01 steps) before it is written. Values that cannot be rounded (NaN) come back
/// as `1.0`; the allowlist range check still applies afterwards.
pub fn default_speed_for_setting(speed: f32) -> f64 {
    if !speed.is_finite() {
        return 1.0;
    }
    (f64::from(speed) * 100.0).round() / 100.0
}

/// The mpv `hwdec` value for a `video.decode_mode` setting (M2-22).
///
/// `auto` and `off` map to `auto` and `no`. The named backends keep their names. Vulkan returns
/// `None`: the M2-22 settings enum does not name it, so the caller uses the setting default and
/// logs a warning instead of inventing a value. Every `Some` result is in the `hwdec` enum.
pub const fn hwdec_for_decode_mode(mode: DecodeMode) -> Option<&'static str> {
    match mode {
        DecodeMode::Auto => Some("auto"),
        DecodeMode::Off => Some("no"),
        DecodeMode::D3d11va => Some("d3d11va"),
        DecodeMode::Nvdec => Some("nvdec"),
        DecodeMode::Vaapi => Some("vaapi"),
        DecodeMode::Videotoolbox => Some("videotoolbox"),
        DecodeMode::Vulkan => None,
    }
}

/// The mpv `video-sync` value for a `playback.sync_mode` setting (M2-22). The spellings are the
/// ones the settings schema already uses, so this is a direct mapping.
pub const fn video_sync_for_sync_mode(mode: SyncMode) -> &'static str {
    match mode {
        SyncMode::Audio => "audio",
        SyncMode::DisplayResample => "display-resample",
        SyncMode::DisplayAdrop => "display-adrop",
    }
}

/// Aspect writes for `video.aspect` (M2-20). Legacy `updateVideoAspectSettings` sets the same three
/// properties from the same mode (PlayerComponent.cpp:1509-1547).
///
/// `Custom` returns nothing, so the user's own mpv configuration is left alone, as legacy does.
/// `NoScaling` also needs `video-unscaled`, which is not on the allowlist, so it is not written
/// here. `Force16x9IfFourByThree` depends on the decoded aspect, which is not known at load, so
/// it writes the off value.
pub fn aspect_options(mode: AspectMode) -> Result<Vec<ValidatedOption>, MpvOptionError> {
    let (keep_aspect, pan_scan, aspect_override) = match mode {
        AspectMode::Custom => return Ok(Vec::new()),
        AspectMode::Normal | AspectMode::NoScaling | AspectMode::Force16x9IfFourByThree => {
            (true, 0.0, "-1")
        }
        AspectMode::Zoom => (true, 1.0, "-1"),
        AspectMode::Force4x3 => (true, 0.0, "4:3"),
        AspectMode::Force16x9 => (true, 0.0, "16:9"),
        AspectMode::Stretch => (false, 0.0, "-1"),
    };
    Ok(vec![
        validate("keepaspect", OptionValue::Flag(keep_aspect))?,
        validate("panscan", OptionValue::Float(pan_scan))?,
        validate("video-aspect-override", OptionValue::Text(aspect_override))?,
    ])
}

/// Load-time write for `video.allow_zoom` (M2-20). The schema holds the permission for
/// interactive zoom, not a zoom level, so the only load-time value is the 1x reset that legacy
/// also writes (PlayerComponent.cpp:888). The range the setting describes, 0.5x to 4x, is
/// `video-zoom` -1.0 to 2.0 on mpv's log2 scale.
pub fn zoom_reset_options() -> Result<Vec<ValidatedOption>, MpvOptionError> {
    Ok(vec![validate("video-zoom", OptionValue::Float(0.0))?])
}

/// Subtitle position writes for `subtitles.placement`, a `horizontal,vertical` pair such as
/// `left,bottom` (settings_description.json). An unset or empty value writes nothing, so mpv keeps
/// its default. The vertical part maps to `sub-pos` as legacy does (PlayerComponent.cpp:1488-1494).
pub fn placement_options(placement: Option<&str>) -> Result<Vec<ValidatedOption>, MpvOptionError> {
    let Some(raw) = placement.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(Vec::new());
    };
    let Some((horizontal, vertical)) = raw.split_once(',') else {
        return Err(MpvOptionError::NotAllowedValue {
            name: "sub-align-x",
        });
    };
    let position = match vertical {
        "bottom" => SUB_POS_BOTTOM,
        "top" => SUB_POS_TOP,
        _ => return Err(MpvOptionError::NotAllowedValue { name: "sub-pos" }),
    };
    Ok(vec![
        validate("sub-align-x", OptionValue::Text(horizontal))?,
        validate("sub-pos", OptionValue::Int(position))?,
    ])
}

/// Subtitle size and colour writes for `subtitles.size` and `subtitles.color` (M2-20). `size` is in
/// points with -1 for the player default, as the legacy key is. `sub-scale` is size / 32
/// (PlayerComponent.cpp:1452). A size of -1 or an unset colour writes nothing for that field.
pub fn subtitle_size_and_color_options(
    size: i32,
    color: Option<&str>,
) -> Result<Vec<ValidatedOption>, MpvOptionError> {
    let size_pt = if size == -1 {
        None
    } else {
        let points = u8::try_from(size)
            .ok()
            .filter(|points| (FONT_SIZE_PT_MIN..=FONT_SIZE_PT_MAX).contains(points))
            .ok_or(MpvOptionError::OutOfRange { name: "sub-scale" })?;
        Some(points)
    };
    size_and_color_options(size_pt, color)
}

/// Subtitle font and outline writes for `subtitles.font` and `subtitles.border_size` (legacy
/// `sub-font` and `sub-border-size`). An empty font writes nothing; `border_size` is whole
/// points with -1 for the player default, which also writes nothing.
pub fn subtitle_font_and_border_options(
    font: &str,
    border_size: i32,
) -> Result<Vec<ValidatedOption>, MpvOptionError> {
    let mut options = Vec::with_capacity(2);
    let font = font.trim();
    if !font.is_empty() {
        options.push(validate("sub-font", OptionValue::Text(font))?);
    }
    if border_size != -1 {
        options.push(validate(
            "sub-border-size",
            OptionValue::Int(i64::from(border_size)),
        )?);
    }
    Ok(options)
}

/// ASS style writes for `subtitles.ass_scale_border_and_shadow` and `subtitles.ass_style_override`,
/// as the legacy client sends them. The scaling choice is always written; the override only when
/// it is set. A value that is not one of the five mpv spellings is refused.
pub fn subtitle_ass_options(
    scale_border_and_shadow: bool,
    ass_override: Option<&str>,
) -> Result<Vec<ValidatedOption>, MpvOptionError> {
    let scaled = if scale_border_and_shadow {
        "ScaledBorderAndShadow=yes"
    } else {
        "ScaledBorderAndShadow=no"
    };
    let mut options = vec![validate(
        "sub-ass-style-overrides",
        OptionValue::Text(scaled),
    )?];
    if let Some(value) = ass_override
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        options.push(validate("sub-ass-override", OptionValue::Text(value))?);
    }
    Ok(options)
}

/// True for `-1`, or for a `W:H` ratio whose two parts are whole numbers from 1 to
/// [`ASPECT_RATIO_PART_MAX`]. Signs, spaces, and decimal points are refused.
fn is_aspect_override(text: &str) -> bool {
    if text == "-1" {
        return true;
    }
    let Some((width, height)) = text.split_once(':') else {
        return false;
    };
    [width, height].iter().all(|part| {
        part.bytes().all(|byte| byte.is_ascii_digit())
            && part
                .parse::<u32>()
                .is_ok_and(|value| (1..=ASPECT_RATIO_PART_MAX).contains(&value))
    })
}

fn refuse_unlisted(name: &str) -> MpvOptionError {
    match one_shot_spec_for(name) {
        Some(spec) => MpvOptionError::OneShotOnly { name: spec.name },
        None => MpvOptionError::UnknownOption {
            name: name.to_owned(),
        },
    }
}

fn check_spec(
    spec: &OptionSpec,
    value: OptionValue<'_>,
) -> Result<ValidatedOption, MpvOptionError> {
    let formatted = match (spec.kind, value) {
        (ValueKind::Flag, OptionValue::Flag(on)) => {
            let text = if on { "yes" } else { "no" };
            text.to_owned()
        }
        (ValueKind::Int { min, max }, OptionValue::Int(number)) => {
            if number < min || number > max {
                return Err(MpvOptionError::OutOfRange { name: spec.name });
            }
            number.to_string()
        }
        (ValueKind::Float { min, max }, OptionValue::Float(number)) => {
            if !number.is_finite() || number < min || number > max {
                return Err(MpvOptionError::OutOfRange { name: spec.name });
            }
            format!("{number}")
        }
        (ValueKind::Enum(allowed), OptionValue::Text(text)) => {
            if !allowed.contains(&text) {
                return Err(MpvOptionError::NotAllowedValue { name: spec.name });
            }
            text.to_owned()
        }
        (ValueKind::Text, OptionValue::Text(text)) => {
            check_text(spec.name, text)?;
            text.to_owned()
        }
        (ValueKind::TrackId, OptionValue::Int(id)) => {
            if id < 1 {
                return Err(MpvOptionError::OutOfRange { name: spec.name });
            }
            id.to_string()
        }
        (ValueKind::TrackId, OptionValue::Text(text)) => {
            if text != "no" {
                return Err(MpvOptionError::NotAllowedValue { name: spec.name });
            }
            text.to_owned()
        }
        (ValueKind::AudioTrack | ValueKind::SubtitleTrack, OptionValue::Int(id)) => {
            if id < 1 {
                return Err(MpvOptionError::OutOfRange { name: spec.name });
            }
            id.to_string()
        }
        (ValueKind::AudioTrack | ValueKind::SubtitleTrack, OptionValue::Text(text)) => {
            if text != "auto" && text != "no" {
                return Err(MpvOptionError::NotAllowedValue { name: spec.name });
            }
            text.to_owned()
        }
        (ValueKind::AspectRatio, OptionValue::Text(text)) => {
            if !is_aspect_override(text) {
                return Err(MpvOptionError::NotAllowedValue { name: spec.name });
            }
            text.to_owned()
        }
        (ValueKind::AudioFilterChain, OptionValue::Text(text)) => {
            if !is_audio_filter_chain(text) {
                return Err(MpvOptionError::NotAllowedValue { name: spec.name });
            }
            text.to_owned()
        }
        (ValueKind::AudioDevice, OptionValue::Text(text)) => {
            if !is_audio_device(text) {
                return Err(MpvOptionError::NotAllowedValue { name: spec.name });
            }
            text.to_owned()
        }
        (kind, _) => {
            return Err(MpvOptionError::TypeMismatch {
                name: spec.name,
                expected: expected_label(kind),
            });
        }
    };

    Ok(ValidatedOption {
        name: spec.name,
        value: formatted,
    })
}

fn check_text(name: &'static str, text: &str) -> Result<(), MpvOptionError> {
    if text.len() > MAX_TEXT_LEN {
        return Err(MpvOptionError::UnsafeText {
            name,
            reason: "longer than the allowed length",
        });
    }
    if text.starts_with('-') {
        return Err(MpvOptionError::UnsafeText {
            name,
            reason: "starts with '-'",
        });
    }
    if text.chars().any(char::is_control) {
        return Err(MpvOptionError::UnsafeText {
            name,
            reason: "contains a control character",
        });
    }
    Ok(())
}

const fn expected_label(kind: ValueKind) -> &'static str {
    match kind {
        ValueKind::Flag => "flag",
        ValueKind::Int { .. } => "integer",
        ValueKind::Float { .. } => "float",
        ValueKind::Enum(_) | ValueKind::Text => "text",
        ValueKind::TrackId => "track id",
        ValueKind::AudioTrack => "audio track",
        ValueKind::SubtitleTrack => "subtitle track",
        ValueKind::AspectRatio => "aspect ratio",
        ValueKind::AudioFilterChain => "audio filter chain",
        ValueKind::AudioDevice => "audio device",
    }
}

/// True for an empty chain, or `lavfi=[f1,f2,...]` where every `f` is `name=key=value:...` with
/// `name` in [`AUDIO_FILTER_NAMES`] and parameters made only of letters, digits and `_ . - = :`.
fn is_audio_filter_chain(text: &str) -> bool {
    if text.is_empty() {
        return true;
    }
    if text.len() > MAX_TEXT_LEN {
        return false;
    }
    let Some(inner) = text
        .strip_prefix("lavfi=[")
        .and_then(|rest| rest.strip_suffix(']'))
    else {
        return false;
    };
    let filters: Vec<&str> = inner.split(',').collect();
    if filters.is_empty() || filters.len() > AUDIO_FILTER_COUNT_MAX {
        return false;
    }
    filters.into_iter().all(is_audio_filter)
}

fn is_audio_filter(filter: &str) -> bool {
    let Some((name, params)) = filter.split_once('=') else {
        return false;
    };
    AUDIO_FILTER_NAMES.contains(&name)
        && !params.is_empty()
        && params.split(':').all(|pair| {
            pair.split_once('=').is_some_and(|(key, value)| {
                !key.is_empty()
                    && !value.is_empty()
                    && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                    && value
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
            })
        })
}

/// True for `auto`, or `<backend>/<id>` with a lowercase backend name and an id of letters,
/// digits, spaces and `_ { } . : @ , = + - /`. No quotes, backslashes or control characters.
fn is_audio_device(text: &str) -> bool {
    if text == "auto" {
        return true;
    }
    if text.len() > AUDIO_DEVICE_LEN_MAX {
        return false;
    }
    let Some((backend, id)) = text.split_once('/') else {
        return false;
    };
    !backend.is_empty()
        && backend.len() <= 24
        && backend
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        && !id.is_empty()
        && id.chars().all(|c| {
            c.is_ascii_alphanumeric()
                || matches!(
                    c,
                    ' ' | '_' | '{' | '}' | '.' | ':' | '@' | ',' | '=' | '+' | '-'
                )
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::{Path, PathBuf};

    fn manifest_path(relative: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join(relative)
    }

    /// Concatenate every file under `dir` whose extension is in `exts`. Missing dirs add nothing.
    fn read_tree(dir: &Path, exts: &[&str], out: &mut String) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                read_tree(&path, exts, out);
                continue;
            }
            let matches_ext = path
                .extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| exts.contains(&ext));
            if matches_ext && let Ok(text) = fs::read_to_string(&path) {
                out.push_str(&text);
                out.push('\n');
            }
        }
    }

    fn is_well_formed_name(name: &str) -> bool {
        !name.is_empty()
            && !name.starts_with('-')
            && !name.ends_with('-')
            && name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    }

    #[test]
    fn table_names_are_well_formed_and_unique() {
        let mut seen = std::collections::BTreeSet::new();
        for spec in ALLOWLIST.iter().chain(ONE_SHOT_ALLOWLIST) {
            assert!(
                is_well_formed_name(spec.name),
                "malformed name {}",
                spec.name
            );
            assert!(seen.insert(spec.name), "duplicate name {}", spec.name);
        }
        assert!(!ALLOWLIST.is_empty());
    }

    #[test]
    fn every_table_name_appears_in_the_embedded_libmpv_runtime_fixture() {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/mpv-runtime-options.json"))
                .expect("the captured libmpv runtime-options fixture is valid JSON");
        assert_eq!(
            fixture
                .get("mpv_version")
                .and_then(serde_json::Value::as_str),
            Some("mpv v0.41.0-1286-gfa0534a72"),
            "the fixture must identify the queried libmpv version"
        );
        assert_eq!(
            fixture
                .get("runtime_sha256")
                .and_then(serde_json::Value::as_str),
            Some("c949759df898b8587c8e51e36cbef96c6a027727cf299f3071e849d555d1929a"),
            "the fixture must identify the queried runtime by checksum"
        );
        assert_eq!(
            fixture
                .get("source_url")
                .and_then(serde_json::Value::as_str),
            Some("https://github.com/zhongfly/mpv-winbuild/releases/tag/2026-10-23-fa0534a729"),
            "the fixture must identify the runtime source"
        );
        let fixture_options = fixture
            .get("options")
            .and_then(serde_json::Value::as_array)
            .expect("the runtime fixture must contain an options array");
        assert_eq!(
            fixture_options.len(),
            861,
            "unexpected option inventory size"
        );
        let option_names: std::collections::BTreeSet<&str> = fixture_options
            .iter()
            .filter_map(serde_json::Value::as_str)
            .collect();
        assert_eq!(
            option_names.len(),
            fixture_options.len(),
            "runtime fixture options must all be strings and unique"
        );
        for spec in ALLOWLIST.iter().chain(ONE_SHOT_ALLOWLIST) {
            assert!(
                option_names.contains(spec.name),
                "allowlisted name {} is absent from the captured libmpv runtime",
                spec.name
            );
        }
    }

    #[test]
    fn audio_filter_chains_accept_exactly_what_the_client_builds() {
        use crate::audio_filters::{AudioFilterSettings, Normalization, filter_chain};
        use crate::equalizer::Equalizer;

        let settings = AudioFilterSettings {
            normalization: Normalization::new(true, -16.0).expect("target"),
            pitch_correction: true,
            compression: true,
        };
        let chain = filter_chain(&settings).expect("chain");
        assert!(validate("af", OptionValue::Text(&chain)).is_ok(), "{chain}");

        let eq = Equalizer::from_gains([3.0, -2.5, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 12.0])
            .expect("gains")
            .to_af_filter()
            .expect("not flat");
        assert!(validate("af", OptionValue::Text(&eq)).is_ok(), "{eq}");
        assert!(
            validate("af", OptionValue::Text("")).is_ok(),
            "empty clears the chain"
        );
    }

    #[test]
    fn audio_filter_chains_refuse_anything_that_could_reach_files_or_other_graphs() {
        for bad in [
            "lavfi=[movie=/etc/passwd]",
            "lavfi=[amovie=file.wav]",
            "lavfi=[anullsrc]",
            "lavfi=[loudnorm=I=-16;movie=x]",
            "lavfi=[loudnorm=I=-16,movie=x]",
            "lavfi=[loudnorm=I=-16][out]",
            "lavfi=[loudnorm=I=[1]]",
            "lavfi=[loudnorm]",
            "lavfi=[loudnorm=]",
            "lavfi=[loudnorm=I]",
            "lavfi=[loudnorm=I=-16:]",
            "lavfi=[loudnorm=I=-16 ]",
            "lavfi=[loudnorm=I='-16']",
            "lavfi=[loudnorm=I=-16\\]",
            "lavfi=[]",
            "lavfi=[,]",
            "loudnorm=I=-16",
            "lavfi=loudnorm=I=-16",
            "lavfi=[loudnorm=I=-16",
            "@name:lavfi=[loudnorm=I=-16]",
            "-lavfi=[loudnorm=I=-16]",
            "lavfi=[loudnorm=I=-16]\n",
            "lavfi=[loudnorm=I=-16:file=/etc/passwd]",
        ] {
            let result = validate("af", OptionValue::Text(bad));
            // `file=/etc/passwd` has a slash in the value, `[1]` has brackets, and so on.
            assert!(result.is_err(), "{bad:?} must be refused");
        }
        let too_many = format!(
            "lavfi=[{}]",
            ["acompressor=ratio=4"; AUDIO_FILTER_COUNT_MAX + 1].join(",")
        );
        assert!(validate("af", OptionValue::Text(&too_many)).is_err());
        let at_limit = format!(
            "lavfi=[{}]",
            ["acompressor=ratio=4"; AUDIO_FILTER_COUNT_MAX].join(",")
        );
        assert!(validate("af", OptionValue::Text(&at_limit)).is_ok());
    }

    #[test]
    fn audio_devices_accept_mpv_spellings_and_refuse_injection() {
        for good in [
            "auto",
            "wasapi/{0.0.0.00000000}.{2f1b7a54-1c3d-4e5a-9b2c-0d1e2f3a4b5c}",
            "coreaudio/AppleUSBAudioEngine:Vendor:Dock:0001:1,2",
            "alsa/hdmi:CARD=NVidia,DEV=3",
            "alsa/default",
            "pulse/alsa_output.pci-0000_00_1f.3.analog-stereo",
            "pipewire/alsa_output.usb-Logitech_G-00.analog-stereo",
            "sdl/Built-in Audio Analog Stereo",
        ] {
            assert!(
                validate("audio-device", OptionValue::Text(good)).is_ok(),
                "{good}"
            );
        }
        for bad in [
            "",
            "default",
            "/alsa",
            "alsa/",
            "-alsa/default",
            "ALSA/default",
            "alsa/de\"fault",
            "alsa/de'fault",
            "alsa/default\n",
            "alsa/back\\slash",
            "alsa/a;b",
            "alsa/a|b",
            "file:///etc/passwd",
        ] {
            assert!(
                validate("audio-device", OptionValue::Text(bad)).is_err(),
                "{bad:?}"
            );
        }
        let long = format!("alsa/{}", "x".repeat(AUDIO_DEVICE_LEN_MAX));
        assert!(validate("audio-device", OptionValue::Text(&long)).is_err());
    }

    #[test]
    fn channel_framedrop_and_speed_values_are_the_ones_settings_can_produce() {
        for ok in ["auto-safe", "stereo", "5.1", "7.1"] {
            assert!(
                validate("audio-channels", OptionValue::Text(ok)).is_ok(),
                "{ok}"
            );
        }
        for bad in ["auto", "2.0", "mono", "5.1(side)", "", "stereo "] {
            assert!(
                validate("audio-channels", OptionValue::Text(bad)).is_err(),
                "{bad:?}"
            );
        }
        assert!(validate("framedrop", OptionValue::Text("vo")).is_ok());
        assert!(validate("framedrop", OptionValue::Text("no")).is_ok());
        assert!(validate("framedrop", OptionValue::Text("decoder")).is_err());
        for ok in [0.25, 1.0, 1.5, 4.0] {
            assert!(validate("speed", OptionValue::Float(ok)).is_ok(), "{ok}");
        }
        for bad in [0.0, 0.24, 4.01, -1.0, f64::NAN, f64::INFINITY] {
            assert!(validate("speed", OptionValue::Float(bad)).is_err(), "{bad}");
        }
    }

    #[test]
    fn setting_values_map_to_allowlisted_mpv_values() {
        for layout in [
            ChannelLayout::Auto,
            ChannelLayout::Stereo,
            ChannelLayout::Surround51,
            ChannelLayout::Surround71,
        ] {
            let value = audio_channels_for_layout(layout);
            assert!(
                validate("audio-channels", OptionValue::Text(value)).is_ok(),
                "{layout:?}"
            );
        }
        assert_eq!(framedrop_for_setting(true), "vo");
        assert_eq!(framedrop_for_setting(false), "no");
        assert_eq!(default_speed_for_setting(1.0), 1.0);
        assert_eq!(default_speed_for_setting(1.5), 1.5);
        assert_eq!(default_speed_for_setting(0.1), 0.1);
        assert_eq!(default_speed_for_setting(1.25), 1.25);
        assert_eq!(default_speed_for_setting(f32::NAN), 1.0);
        assert_eq!(default_speed_for_setting(f32::INFINITY), 1.0);
        for speed in [0.25_f32, 0.3, 1.1, 2.75, 4.0] {
            let written = default_speed_for_setting(speed);
            assert!(
                validate("speed", OptionValue::Float(written)).is_ok(),
                "{speed}"
            );
        }
    }

    #[test]
    fn subtitle_font_and_border_follow_the_setting_defaults() {
        let written = |font: &str, border: i32| {
            subtitle_font_and_border_options(font, border)
                .expect("valid")
                .into_iter()
                .map(|option| (option.name, option.value))
                .collect::<Vec<_>>()
        };
        // The defaults: the font is written, the outline is left to the player.
        assert_eq!(
            written("sans-serif", -1),
            [("sub-font", "sans-serif".to_owned())]
        );
        assert_eq!(
            written(" Noto Sans ", 3),
            [
                ("sub-font", "Noto Sans".to_owned()),
                ("sub-border-size", "3".to_owned())
            ]
        );
        assert_eq!(written("", -1), []);
        assert_eq!(written("", 0), [("sub-border-size", "0".to_owned())]);
        assert!(subtitle_font_and_border_options("Arial", 11).is_err());
        assert!(subtitle_font_and_border_options("Arial", -2).is_err());
        assert!(subtitle_font_and_border_options("-Arial", -1).is_err());
        assert!(subtitle_font_and_border_options("A\nB", -1).is_err());
    }

    #[test]
    fn ass_options_follow_the_legacy_client() {
        let written = |scale: bool, value: Option<&str>| {
            subtitle_ass_options(scale, value)
                .expect("valid")
                .into_iter()
                .map(|option| (option.name, option.value))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            written(true, None),
            [(
                "sub-ass-style-overrides",
                "ScaledBorderAndShadow=yes".to_owned()
            )]
        );
        assert_eq!(
            written(false, Some("")),
            [(
                "sub-ass-style-overrides",
                "ScaledBorderAndShadow=no".to_owned()
            )]
        );
        assert_eq!(
            written(true, Some(" scale ")),
            [
                (
                    "sub-ass-style-overrides",
                    "ScaledBorderAndShadow=yes".to_owned()
                ),
                ("sub-ass-override", "scale".to_owned())
            ]
        );
        for bad in ["bogus", "Force", "yes;no", "-1"] {
            assert!(subtitle_ass_options(true, Some(bad)).is_err(), "{bad}");
        }
    }

    #[test]
    fn audio_filter_options_default_to_an_empty_chain_and_pitch_correction_on() {
        let written = audio_filter_options(&AudioSettings::default()).expect("valid");
        let pairs: Vec<_> = written.iter().map(|o| (o.name, o.value.as_str())).collect();
        assert_eq!(pairs, [("af", ""), ("audio-pitch-correction", "yes")]);
    }

    #[test]
    fn audio_filter_options_put_the_equalizer_and_filters_in_one_graph() {
        let audio = AudioSettings {
            equalizer_gains_db: vec![3.0, 2.5, 0.0, -1.0, 0.0, 0.5, 1.0, 2.0, 4.0, 6.0],
            loudness_normalization: true,
            loudness_target_lufs: -23,
            dynamic_range_compression: true,
            pitch_correction: false,
            ..AudioSettings::default()
        };
        let written = audio_filter_options(&audio).expect("valid");
        let af = &written[0];
        assert_eq!(af.name, "af");
        assert_eq!(af.value.matches("lavfi=").count(), 1, "{}", af.value);
        assert!(
            af.value.starts_with("lavfi=[equalizer=f=31:"),
            "{}",
            af.value
        );
        assert!(
            af.value
                .contains("equalizer=f=16000:width_type=o:width=1:g=6")
        );
        assert!(af.value.contains(",loudnorm=I=-23:"), "{}", af.value);
        assert!(
            af.value
                .ends_with(",acompressor=threshold=0.125:ratio=4:attack=20:release=250]")
        );
        assert_eq!(written[1].value, "no");
    }

    #[test]
    fn audio_filter_options_treat_unusable_gains_as_flat_and_still_validate() {
        for gains in [vec![1.0], vec![0.0; 11], vec![13.0; 10], vec![f32::NAN; 10]] {
            let audio = AudioSettings {
                equalizer_gains_db: gains,
                loudness_normalization: true,
                ..AudioSettings::default()
            };
            let written = audio_filter_options(&audio).expect("valid");
            assert!(
                !written[0].value.contains("equalizer="),
                "{}",
                written[0].value
            );
            assert!(written[0].value.contains("loudnorm="));
        }
        // An out-of-range target (the loader refuses it) falls back to the default target.
        let audio = AudioSettings {
            loudness_normalization: true,
            loudness_target_lufs: 5,
            ..AudioSettings::default()
        };
        assert!(
            audio_filter_options(&audio).expect("valid")[0]
                .value
                .contains("loudnorm=I=-16:")
        );
    }

    #[test]
    fn every_default_is_accepted_by_its_own_kind() {
        for spec in ALLOWLIST {
            let result = validate(spec.name, spec.default);
            assert!(
                result.is_ok(),
                "default for {} rejected: {result:?}",
                spec.name
            );
        }
        for spec in ONE_SHOT_ALLOWLIST {
            let result = validate_one_shot(spec.name, spec.default);
            assert!(
                result.is_ok(),
                "one-shot default for {} rejected: {result:?}",
                spec.name
            );
        }
    }

    #[test]
    fn unknown_and_dangerous_names_are_rejected() {
        let names = [
            "not-an-option",
            "",
            "VOLUME-MAX",
            "volume-max ",
            "volume-max=100",
            "config",
            "load-scripts",
            "vo",
            "ao",
            "ytdl",
            "tls-verify",
            "tls-ca-file",
            "input-conf",
            "script-opts",
            "include",
        ];
        for name in names {
            let result = validate(name, OptionValue::Flag(true));
            assert!(
                matches!(result, Err(MpvOptionError::UnknownOption { .. })),
                "name {name:?} was not refused as unknown: {result:?}"
            );
        }
    }

    #[test]
    fn integer_and_float_boundaries_are_accepted_and_one_past_is_refused() {
        for spec in ALLOWLIST {
            match spec.kind {
                ValueKind::Int { min, max } => {
                    assert!(
                        validate(spec.name, OptionValue::Int(min)).is_ok(),
                        "{}",
                        spec.name
                    );
                    assert!(
                        validate(spec.name, OptionValue::Int(max)).is_ok(),
                        "{}",
                        spec.name
                    );
                    assert!(
                        matches!(
                            validate(spec.name, OptionValue::Int(min - 1)),
                            Err(MpvOptionError::OutOfRange { .. })
                        ),
                        "{} accepted min - 1",
                        spec.name
                    );
                    assert!(
                        matches!(
                            validate(spec.name, OptionValue::Int(max + 1)),
                            Err(MpvOptionError::OutOfRange { .. })
                        ),
                        "{} accepted max + 1",
                        spec.name
                    );
                }
                ValueKind::Float { min, max } => {
                    assert!(
                        validate(spec.name, OptionValue::Float(min)).is_ok(),
                        "{}",
                        spec.name
                    );
                    assert!(
                        validate(spec.name, OptionValue::Float(max)).is_ok(),
                        "{}",
                        spec.name
                    );
                    assert!(
                        matches!(
                            validate(spec.name, OptionValue::Float(max + 0.5)),
                            Err(MpvOptionError::OutOfRange { .. })
                        ),
                        "{} accepted above max",
                        spec.name
                    );
                }
                _ => {}
            }
        }
    }

    #[test]
    fn non_finite_floats_are_refused() {
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let result = validate("sub-scale", OptionValue::Float(bad));
            assert!(matches!(result, Err(MpvOptionError::OutOfRange { .. })));
        }
    }

    #[test]
    fn sub_delay_accepts_the_signed_five_second_range() {
        for seconds in [-5.0, -0.25, 0.0, 0.05, 5.0] {
            let result = validate("sub-delay", OptionValue::Float(seconds));
            assert!(result.is_ok(), "{seconds} refused: {result:?}");
        }
        let negative = validate("sub-delay", OptionValue::Float(-0.25));
        assert_eq!(negative.map(|v| v.value).ok().as_deref(), Some("-0.25"));
    }

    #[test]
    fn sub_delay_refuses_values_past_either_end_and_non_finite() {
        for bad in [5.001, -5.001, 60.0, f64::NAN, f64::INFINITY] {
            assert!(
                matches!(
                    validate("sub-delay", OptionValue::Float(bad)),
                    Err(MpvOptionError::OutOfRange { name: "sub-delay" })
                ),
                "sub-delay accepted {bad}"
            );
        }
        assert!(matches!(
            validate("sub-delay", OptionValue::Int(1)),
            Err(MpvOptionError::TypeMismatch { .. })
        ));
    }

    #[test]
    fn type_mismatch_is_refused() {
        assert!(matches!(
            validate("volume-max", OptionValue::Flag(true)),
            Err(MpvOptionError::TypeMismatch { .. })
        ));
        assert!(matches!(
            validate("keepaspect", OptionValue::Text("yes")),
            Err(MpvOptionError::TypeMismatch { .. })
        ));
        assert!(matches!(
            validate("sub-font", OptionValue::Int(3)),
            Err(MpvOptionError::TypeMismatch { .. })
        ));
    }

    #[test]
    fn flags_format_as_yes_and_no() {
        let on = validate("keepaspect", OptionValue::Flag(true));
        let off = validate("keepaspect", OptionValue::Flag(false));
        assert_eq!(on.map(|v| v.value).ok().as_deref(), Some("yes"));
        assert_eq!(off.map(|v| v.value).ok().as_deref(), Some("no"));
    }

    #[test]
    fn enum_rejects_values_outside_the_list() {
        assert!(validate("hwdec", OptionValue::Text("auto-copy")).is_ok());
        assert!(matches!(
            validate("hwdec", OptionValue::Text("vaapi-egl")),
            Err(MpvOptionError::NotAllowedValue { .. })
        ));
        assert!(matches!(
            validate("video-aspect-override", OptionValue::Text("no")),
            Err(MpvOptionError::NotAllowedValue { .. })
        ));
    }

    #[test]
    fn text_rejects_leading_dash_newline_and_overlong_values() {
        assert!(matches!(
            validate("screenshot-directory", OptionValue::Text("-rf")),
            Err(MpvOptionError::UnsafeText { .. })
        ));
        assert!(matches!(
            validate("sub-font", OptionValue::Text("Arial\nvo=null")),
            Err(MpvOptionError::UnsafeText { .. })
        ));
        let long = "a".repeat(MAX_TEXT_LEN + 1);
        assert!(matches!(
            validate("screenshot-template", OptionValue::Text(&long)),
            Err(MpvOptionError::UnsafeText { .. })
        ));
        let at_limit = "a".repeat(MAX_TEXT_LEN);
        assert!(validate("screenshot-template", OptionValue::Text(&at_limit)).is_ok());
    }

    #[test]
    fn error_text_never_includes_the_rejected_value() {
        let secret_path = "-/secret/path";
        let err = validate("screenshot-directory", OptionValue::Text(secret_path))
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert!(!err.contains("secret"), "error leaked the value: {err}");
        assert!(err.contains("screenshot-directory"));
    }

    #[test]
    fn track_selector_accepts_positive_ids_and_no() {
        let id = validate("secondary-sid", OptionValue::Int(42));
        assert_eq!(id.map(|v| v.value).ok().as_deref(), Some("42"));
        assert!(validate("secondary-sid", OptionValue::Int(1)).is_ok());
        let off = validate("secondary-sid", OptionValue::Text("no"));
        assert_eq!(off.map(|v| v.value).ok().as_deref(), Some("no"));
    }

    #[test]
    fn track_selector_refuses_zero_negative_and_other_text() {
        for bad in [0, -1, i64::MIN] {
            assert!(
                matches!(
                    validate("secondary-sid", OptionValue::Int(bad)),
                    Err(MpvOptionError::OutOfRange {
                        name: "secondary-sid"
                    })
                ),
                "secondary-sid accepted {bad}"
            );
        }
        for bad in ["yes", "1", "", "auto"] {
            assert!(
                matches!(
                    validate("secondary-sid", OptionValue::Text(bad)),
                    Err(MpvOptionError::NotAllowedValue {
                        name: "secondary-sid"
                    })
                ),
                "secondary-sid accepted {bad:?}"
            );
        }
        assert!(matches!(
            validate("secondary-sid", OptionValue::Flag(true)),
            Err(MpvOptionError::TypeMismatch { .. })
        ));
    }

    #[test]
    fn sid_is_refused_by_the_settings_path_for_every_value_shape() {
        let shapes = [
            OptionValue::Int(1),
            OptionValue::Text("no"),
            OptionValue::Flag(false),
            OptionValue::Float(1.0),
        ];
        for value in shapes {
            assert!(
                matches!(
                    validate("sid", value),
                    Err(MpvOptionError::OneShotOnly { name: "sid" })
                ),
                "settings path accepted sid as {value:?}"
            );
        }
        assert!(spec_for("sid").is_none());
    }

    #[test]
    fn one_shot_path_accepts_sid_and_no_other_name() {
        let off = validate_one_shot("sid", OptionValue::Text("no"));
        assert_eq!(off.map(|v| v.value).ok().as_deref(), Some("no"));
        let track = validate_one_shot("sid", OptionValue::Int(3));
        assert_eq!(track.map(|v| v.value).ok().as_deref(), Some("3"));
        assert!(matches!(
            validate_one_shot("sid", OptionValue::Int(0)),
            Err(MpvOptionError::OutOfRange { name: "sid" })
        ));
        for name in ["secondary-sid", "volume", "sub-delay", "sid-extra"] {
            assert!(
                matches!(
                    validate_one_shot(name, OptionValue::Int(1)),
                    Err(MpvOptionError::UnknownOption { .. })
                ),
                "one-shot path accepted {name}"
            );
        }
    }

    #[test]
    fn subtitle_track_accepts_auto_no_and_inventory_ids() {
        let inventory = [1, 4];
        let auto = validate_subtitle_track(OptionValue::Text("auto"), &inventory);
        assert_eq!(auto.map(|v| v.value).ok().as_deref(), Some("auto"));
        let off = validate_subtitle_track(OptionValue::Text("no"), &inventory);
        assert_eq!(off.map(|v| v.value).ok().as_deref(), Some("no"));
        let track = validate_subtitle_track(OptionValue::Int(4), &inventory);
        assert_eq!(track.map(|v| v.value).ok().as_deref(), Some("4"));
    }

    #[test]
    fn subtitle_track_refuses_ids_outside_the_inventory() {
        let inventory = [1, 2];
        for id in [3, 0, -1] {
            let result = validate_subtitle_track(OptionValue::Int(id), &inventory);
            let expected = if id < 1 {
                matches!(result, Err(MpvOptionError::OutOfRange { name: "sid" }))
            } else {
                matches!(result, Err(MpvOptionError::NotInInventory { name: "sid" }))
            };
            assert!(expected, "sid accepted {id}: {result:?}");
        }
        // An empty inventory accepts no numeric id, but auto and no still pass.
        assert!(matches!(
            validate_subtitle_track(OptionValue::Int(1), &[]),
            Err(MpvOptionError::NotInInventory { name: "sid" })
        ));
        assert!(validate_subtitle_track(OptionValue::Text("auto"), &[]).is_ok());
    }

    #[test]
    fn subtitle_track_refuses_other_text_and_flags() {
        let inventory = [1];
        for bad in ["yes", "1", "", "auto ", "AUTO", "aid"] {
            assert!(
                matches!(
                    validate_subtitle_track(OptionValue::Text(bad), &inventory),
                    Err(MpvOptionError::NotAllowedValue { name: "sid" })
                ),
                "sid accepted {bad:?}"
            );
        }
        assert!(matches!(
            validate_subtitle_track(OptionValue::Flag(true), &inventory),
            Err(MpvOptionError::TypeMismatch { name: "sid", .. })
        ));
    }

    #[test]
    fn sid_auto_is_refused_by_the_settings_path() {
        assert!(matches!(
            validate("sid", OptionValue::Text("auto")),
            Err(MpvOptionError::OneShotOnly { name: "sid" })
        ));
    }

    #[test]
    fn audio_track_accepts_auto_no_and_inventory_ids() {
        let inventory = [1, 2, 7];
        let auto = validate_audio_track(OptionValue::Text("auto"), &inventory);
        assert_eq!(auto.map(|v| v.value).ok().as_deref(), Some("auto"));
        let off = validate_audio_track(OptionValue::Text("no"), &inventory);
        assert_eq!(off.map(|v| v.value).ok().as_deref(), Some("no"));
        let track = validate_audio_track(OptionValue::Int(7), &inventory);
        assert_eq!(track.map(|v| v.value).ok().as_deref(), Some("7"));
    }

    #[test]
    fn audio_track_refuses_ids_outside_the_inventory() {
        let inventory = [1, 2];
        for id in [3, 0, -1] {
            let result = validate_audio_track(OptionValue::Int(id), &inventory);
            let expected = if id < 1 {
                matches!(result, Err(MpvOptionError::OutOfRange { name: "aid" }))
            } else {
                matches!(result, Err(MpvOptionError::NotInInventory { name: "aid" }))
            };
            assert!(expected, "aid accepted {id}: {result:?}");
        }
        // An empty inventory accepts no numeric id, but auto and no still pass.
        assert!(matches!(
            validate_audio_track(OptionValue::Int(1), &[]),
            Err(MpvOptionError::NotInInventory { name: "aid" })
        ));
        assert!(validate_audio_track(OptionValue::Text("auto"), &[]).is_ok());
    }

    #[test]
    fn audio_track_refuses_other_text_and_flags() {
        let inventory = [1];
        for bad in ["yes", "1", "", "auto ", "AUTO", "sid"] {
            assert!(
                matches!(
                    validate_audio_track(OptionValue::Text(bad), &inventory),
                    Err(MpvOptionError::NotAllowedValue { name: "aid" })
                ),
                "aid accepted {bad:?}"
            );
        }
        assert!(matches!(
            validate_audio_track(OptionValue::Flag(true), &inventory),
            Err(MpvOptionError::TypeMismatch { name: "aid", .. })
        ));
    }

    #[test]
    fn aid_is_one_shot_and_never_a_settings_option() {
        assert!(spec_for("aid").is_none());
        assert!(one_shot_spec_for("aid").is_some());
        for value in [
            OptionValue::Int(1),
            OptionValue::Text("auto"),
            OptionValue::Text("no"),
        ] {
            assert!(
                matches!(
                    validate("aid", value),
                    Err(MpvOptionError::OneShotOnly { name: "aid" })
                ),
                "settings path accepted aid as {value:?}"
            );
        }
    }

    #[test]
    fn decode_modes_map_to_their_hwdec_values() {
        let expected = [
            (DecodeMode::Auto, Some("auto")),
            (DecodeMode::Off, Some("no")),
            (DecodeMode::D3d11va, Some("d3d11va")),
            (DecodeMode::Nvdec, Some("nvdec")),
            (DecodeMode::Vaapi, Some("vaapi")),
            (DecodeMode::Videotoolbox, Some("videotoolbox")),
            (DecodeMode::Vulkan, None),
        ];
        for (mode, value) in expected {
            assert_eq!(hwdec_for_decode_mode(mode), value, "{mode:?}");
        }
    }

    #[test]
    fn every_mapped_decode_mode_passes_the_hwdec_allowlist() {
        let modes = [
            DecodeMode::Auto,
            DecodeMode::Off,
            DecodeMode::D3d11va,
            DecodeMode::Nvdec,
            DecodeMode::Vaapi,
            DecodeMode::Videotoolbox,
            DecodeMode::Vulkan,
        ];
        for mode in modes {
            let Some(value) = hwdec_for_decode_mode(mode) else {
                continue;
            };
            let checked = validate("hwdec", OptionValue::Text(value));
            assert_eq!(
                checked.map(|v| v.value).ok().as_deref(),
                Some(value),
                "{mode:?} was not accepted as hwdec"
            );
        }
    }

    #[test]
    fn sync_modes_map_to_the_video_sync_allowlist() {
        for mode in [
            SyncMode::Audio,
            SyncMode::DisplayResample,
            SyncMode::DisplayAdrop,
        ] {
            let value = video_sync_for_sync_mode(mode);
            assert!(
                validate("video-sync", OptionValue::Text(value)).is_ok(),
                "{mode:?} -> {value} refused"
            );
        }
        assert_eq!(
            video_sync_for_sync_mode(SyncMode::DisplayResample),
            "display-resample"
        );
    }

    /// The `(name, value)` pairs a builder writes, or a panic naming the refusal.
    fn written(
        result: Result<Vec<ValidatedOption>, MpvOptionError>,
    ) -> Vec<(&'static str, String)> {
        match result {
            Ok(options) => options.into_iter().map(|o| (o.name, o.value)).collect(),
            Err(error) => panic!("expected writes, got {error}"),
        }
    }

    #[test]
    fn aspect_modes_write_the_legacy_properties() {
        let expect = |mode: AspectMode, keep: &str, pan: &str, ratio: &str| {
            assert_eq!(
                written(aspect_options(mode)),
                vec![
                    ("keepaspect", keep.to_owned()),
                    ("panscan", pan.to_owned()),
                    ("video-aspect-override", ratio.to_owned()),
                ],
                "{mode:?}"
            );
        };
        expect(AspectMode::Normal, "yes", "0", "-1");
        expect(AspectMode::NoScaling, "yes", "0", "-1");
        expect(AspectMode::Zoom, "yes", "1", "-1");
        expect(AspectMode::Force4x3, "yes", "0", "4:3");
        expect(AspectMode::Force16x9, "yes", "0", "16:9");
        expect(AspectMode::Force16x9IfFourByThree, "yes", "0", "-1");
        expect(AspectMode::Stretch, "no", "0", "-1");
    }

    #[test]
    fn custom_aspect_writes_nothing() {
        assert!(matches!(
            aspect_options(AspectMode::Custom),
            Ok(options) if options.is_empty()
        ));
    }

    #[test]
    fn aspect_override_accepts_off_and_whole_ratios() {
        for value in ["-1", "4:3", "16:9", "21:9", "1:1", "10000:1"] {
            assert!(
                validate("video-aspect-override", OptionValue::Text(value)).is_ok(),
                "{value} refused"
            );
        }
    }

    #[test]
    fn aspect_override_refuses_malformed_and_out_of_range_ratios() {
        for value in [
            "no", "", " -1", "-1 ", "-2", "0:9", "4:0", "4", "4:3:2", "+4:3", "-4:3", "2.35:1",
            "4.0:3", "a:b", "4:", ":3", "10001:1", "4 :3",
        ] {
            assert!(
                matches!(
                    validate("video-aspect-override", OptionValue::Text(value)),
                    Err(MpvOptionError::NotAllowedValue {
                        name: "video-aspect-override"
                    })
                ),
                "video-aspect-override accepted {value:?}"
            );
        }
        assert!(matches!(
            validate("video-aspect-override", OptionValue::Int(-1)),
            Err(MpvOptionError::TypeMismatch { .. })
        ));
    }

    #[test]
    fn zoom_reset_and_the_zoom_range_match_zero_point_five_to_four_times() {
        assert_eq!(
            written(zoom_reset_options()),
            vec![("video-zoom", "0".to_owned())]
        );
        assert!(validate("video-zoom", OptionValue::Float(-1.0)).is_ok());
        assert!(validate("video-zoom", OptionValue::Float(2.0)).is_ok());
        for bad in [-1.01, 2.01] {
            assert!(
                matches!(
                    validate("video-zoom", OptionValue::Float(bad)),
                    Err(MpvOptionError::OutOfRange { name: "video-zoom" })
                ),
                "video-zoom accepted {bad}"
            );
        }
    }

    #[test]
    fn placement_maps_the_horizontal_and_vertical_parts() {
        assert!(written(placement_options(None)).is_empty());
        assert!(written(placement_options(Some(""))).is_empty());
        assert_eq!(
            written(placement_options(Some("left,bottom"))),
            vec![
                ("sub-align-x", "left".to_owned()),
                ("sub-pos", "100".to_owned())
            ]
        );
        assert_eq!(
            written(placement_options(Some("center,top"))),
            vec![
                ("sub-align-x", "center".to_owned()),
                ("sub-pos", "10".to_owned())
            ]
        );
        assert_eq!(
            written(placement_options(Some("right,top"))),
            vec![
                ("sub-align-x", "right".to_owned()),
                ("sub-pos", "10".to_owned())
            ]
        );
    }

    #[test]
    fn placement_refuses_unknown_parts_and_extra_fields() {
        for value in [
            "middle,top",
            "left,middle",
            "left",
            "left,bottom,x",
            "-left,top",
            "left, bottom",
            "left,BOTTOM",
            ",bottom",
        ] {
            assert!(
                placement_options(Some(value)).is_err(),
                "placement accepted {value:?}"
            );
        }
    }

    #[test]
    fn subtitle_size_accepts_the_setting_range_and_the_default() {
        assert!(written(subtitle_size_and_color_options(-1, None)).is_empty());
        assert_eq!(
            written(subtitle_size_and_color_options(32, None)),
            vec![("sub-scale", "1".to_owned())]
        );
        assert_eq!(
            written(subtitle_size_and_color_options(8, None)),
            vec![("sub-scale", "0.25".to_owned())]
        );
        assert_eq!(
            written(subtitle_size_and_color_options(72, None)),
            vec![("sub-scale", "2.25".to_owned())]
        );
    }

    #[test]
    fn subtitle_size_refuses_values_outside_the_range() {
        for size in [-2, 0, 7, 73, 1000, i32::MIN, i32::MAX] {
            assert!(
                matches!(
                    subtitle_size_and_color_options(size, None),
                    Err(MpvOptionError::OutOfRange { name: "sub-scale" })
                ),
                "subtitle size accepted {size}"
            );
        }
    }

    #[test]
    fn subtitle_color_is_written_in_mpv_argb_order() {
        assert_eq!(
            written(subtitle_size_and_color_options(-1, Some("#FFFFFF"))),
            vec![("sub-color", "#FFFFFFFF".to_owned())]
        );
        assert_eq!(
            written(subtitle_size_and_color_options(-1, Some("#10203040"))),
            vec![("sub-color", "#40102030".to_owned())]
        );
    }

    #[test]
    fn subtitle_color_refuses_anything_but_hex() {
        for value in ["white", "#FFF", "FFFFFF", "#GGGGGG", "#FFFFFFF", ""] {
            assert!(
                matches!(
                    subtitle_size_and_color_options(-1, Some(value)),
                    Err(MpvOptionError::NotAllowedValue { name: "sub-color" })
                ),
                "sub-color accepted {value:?}"
            );
        }
    }

    #[test]
    fn no_free_form_option_string_building_in_crates() {
        // Needles are assembled at run time so this file does not match itself.
        let needles = [concat!("extra", "Args"), concat!("format!(\"{}", "={}\"")];
        let mut haystack = String::new();
        read_tree(&manifest_path("../"), &["rs"], &mut haystack);
        for needle in needles {
            assert!(
                !haystack.contains(needle),
                "free-form option building found: {needle}"
            );
        }
    }
}
