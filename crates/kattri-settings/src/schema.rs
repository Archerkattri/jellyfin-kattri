//! Typed settings schema and defaults.
//!
//! Each field documents its legacy Neo key (`section.key` in
//! `legacy/qt-neo/resources/settings/settings_description.json`) so the M2 parity
//! work can cross-check. Defaults match the legacy defaults unless noted.
//!
//! Serialized names are snake_case TOML keys. Enum variants that carry a legacy
//! string value use explicit `serde(rename)` so the on-disk spelling is stable.

use std::collections::BTreeSet;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::error::SettingsError;

/// Schema version written by this build. Bump together with a migration step in
/// [`crate::migrate::MIGRATIONS`].
pub const CURRENT_SCHEMA_VERSION: u32 = 3;

/// Root settings document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub schema_version: u32,
    /// Future schema version that caused this read-only default snapshot, if any.
    #[serde(skip)]
    pub read_only_schema_version: Option<u32>,
    /// Unknown top-level fields preserved for forward compatibility.
    #[serde(rename = "_unknown", skip_serializing_if = "toml::Table::is_empty")]
    pub unknown: toml::Table,
    pub general: GeneralSettings,
    pub playback: PlaybackSettings,
    pub audio: AudioSettings,
    pub video: VideoSettings,
    pub subtitles: SubtitleSettings,
    pub window: WindowSettings,
    pub input: InputSettings,
    pub plugins: PluginSettings,
    pub updates: UpdateSettings,
    pub diagnostics: DiagnosticsSettings,
    pub parental: ParentalSettings,
    pub admin: AdminSettings,
    /// Per-series language choices (M2-10), never track IDs or paths.
    pub series_languages: Vec<SeriesLanguageSetting>,
}

/// One validated persisted per-series language preference (M2-10).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SeriesLanguageSetting {
    pub series_id: String,
    pub audio_language: Option<String>,
    pub audio_title: Option<String>,
    pub subtitle_language: Option<String>,
    pub subtitle_title: Option<String>,
    pub subtitle_disabled: bool,
}

/// Maximum series-language records kept by the settings schema, matching client-core.
pub const MAX_SERIES_LANGUAGE_ENTRIES: usize = 500;

/// Smallest allowed playback cache, in MiB (M2-06).
///
/// Mirrors `kattri_client_core::cache`. This crate does not depend on client-core, so the
/// numbers are kept in step by hand. Changing one means changing the other.
pub const CACHE_MB_MIN: u32 = 16;
/// Largest allowed playback cache, in MiB (M2-06).
pub const CACHE_MB_MAX: u32 = 4096;
/// Largest allowed demuxer back-buffer, in MiB (M2-06).
pub const BACKBUFFER_MB_MAX: u32 = 1024;
/// Largest allowed read-ahead, in seconds (M2-06).
pub const READAHEAD_SECS_MAX: u32 = 600;
/// Largest allowed outro skip delay, in milliseconds (M2-08).
///
/// Mirrors `kattri_client_core::segments::MAX_OUTRO_DELAY_MS`; keep the two in step by hand.
pub const OUTRO_DELAY_MS_MAX: u32 = 60_000;
/// Allowed values for `audio.max_volume_percent` (M2-16). Mirrors
/// `kattri_client_core::volume::MAX_VOLUME_PERCENT_CHOICES`; keep the two in step by hand.
pub const MAX_VOLUME_PERCENT_CHOICES: [u32; 4] = [100, 150, 200, 300];

impl PlaybackSettings {
    /// Clamp the buffer fields into their M2-06 ranges. Returns the dotted names of the fields
    /// that changed, so the caller can log one warning per field.
    pub fn clamp_buffers(&mut self) -> Vec<&'static str> {
        let mut changed = Vec::new();
        clamp_into(
            &mut self.cache_mb,
            CACHE_MB_MIN,
            CACHE_MB_MAX,
            "playback.cache_mb",
            &mut changed,
        );
        clamp_into(
            &mut self.demuxer_backbuffer_mb,
            0,
            BACKBUFFER_MB_MAX,
            "playback.demuxer_backbuffer_mb",
            &mut changed,
        );
        clamp_into(
            &mut self.demuxer_readahead_secs,
            0,
            READAHEAD_SECS_MAX,
            "playback.demuxer_readahead_secs",
            &mut changed,
        );
        changed
    }
}

fn clamp_into(
    value: &mut u32,
    min: u32,
    max: u32,
    field: &'static str,
    changed: &mut Vec<&'static str>,
) {
    let clamped = (*value).clamp(min, max);
    if clamped != *value {
        *value = clamped;
        changed.push(field);
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            schema_version: CURRENT_SCHEMA_VERSION,
            read_only_schema_version: None,
            unknown: toml::Table::new(),
            general: GeneralSettings::default(),
            playback: PlaybackSettings::default(),
            audio: AudioSettings::default(),
            video: VideoSettings::default(),
            subtitles: SubtitleSettings::default(),
            window: WindowSettings::default(),
            input: InputSettings::default(),
            plugins: PluginSettings::default(),
            updates: UpdateSettings::default(),
            diagnostics: DiagnosticsSettings::default(),
            parental: ParentalSettings::default(),
            admin: AdminSettings::default(),
            series_languages: Vec::new(),
        }
    }
}

/// General presentation and accessibility preferences shown in the Settings screen.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GeneralSettings {
    pub high_contrast: bool,
    pub reduce_motion: bool,
    #[serde(rename = "_unknown", skip_serializing_if = "toml::Table::is_empty")]
    pub unknown: toml::Table,
}

impl Default for GeneralSettings {
    fn default() -> Self {
        Self {
            high_contrast: false,
            reduce_motion: true,
            unknown: toml::Table::new(),
        }
    }
}

impl Settings {
    /// Check value ranges. Returns the first violation, naming the dotted field path.
    ///
    /// Enum-typed fields are already constrained by deserialization, so this covers
    /// numeric ranges and cross-field rules only.
    pub fn validate(&self) -> Result<(), SettingsError> {
        let invalid = |field: &'static str, reason: &str| SettingsError::Invalid {
            field,
            reason: reason.to_owned(),
        };

        let record = &self.parental.pin_record;
        if !record.is_empty()
            && (record.len() > MAX_PIN_RECORD_LEN
                || !record.bytes().all(|byte| byte.is_ascii_graphic()))
        {
            return Err(invalid(
                "parental.pin_record",
                "must be one Argon2id record without spaces",
            ));
        }

        if self.series_languages.len() > MAX_SERIES_LANGUAGE_ENTRIES {
            return Err(invalid(
                "series_languages",
                "must contain at most 500 entries",
            ));
        }
        let mut series_ids = BTreeSet::new();
        for record in &self.series_languages {
            if !valid_series_guid(&record.series_id) || !series_ids.insert(&record.series_id) {
                return Err(invalid(
                    "series_languages.series_id",
                    "must be a unique, non-zero Jellyfin GUID",
                ));
            }
            for (field, language) in [
                (
                    "series_languages.audio_language",
                    record.audio_language.as_deref(),
                ),
                (
                    "series_languages.subtitle_language",
                    record.subtitle_language.as_deref(),
                ),
            ] {
                if language.is_some_and(|value| {
                    !(2..=3).contains(&value.len())
                        || !value.bytes().all(|byte| byte.is_ascii_alphabetic())
                }) {
                    return Err(invalid(
                        field,
                        "must be a two- or three-letter ISO 639 code",
                    ));
                }
            }
            for (field, title) in [
                (
                    "series_languages.audio_title",
                    record.audio_title.as_deref(),
                ),
                (
                    "series_languages.subtitle_title",
                    record.subtitle_title.as_deref(),
                ),
            ] {
                if title.is_some_and(|value| {
                    value.trim().is_empty()
                        || value.chars().count() > 128
                        || value.chars().any(char::is_control)
                }) {
                    return Err(invalid(
                        field,
                        "must be printable text of at most 128 characters",
                    ));
                }
            }
            if record.audio_language.is_none() && record.audio_title.is_some() {
                return Err(invalid(
                    "series_languages.audio_title",
                    "requires an audio language",
                ));
            }
            if record.subtitle_disabled
                && (record.subtitle_language.is_some() || record.subtitle_title.is_some())
            {
                return Err(invalid(
                    "series_languages.subtitle_disabled",
                    "cannot be combined with a remembered subtitle track",
                ));
            }
            if record.subtitle_language.is_none() && record.subtitle_title.is_some() {
                return Err(invalid(
                    "series_languages.subtitle_title",
                    "requires a subtitle language",
                ));
            }
        }

        let speed = self.playback.default_speed;
        if !speed.is_finite() || !(0.25..=4.0).contains(&speed) {
            return Err(invalid(
                "playback.default_speed",
                "must be between 0.25 and 4.0",
            ));
        }
        if !(CACHE_MB_MIN..=CACHE_MB_MAX).contains(&self.playback.cache_mb) {
            return Err(invalid("playback.cache_mb", "must be between 16 and 4096"));
        }
        if self.playback.demuxer_backbuffer_mb > BACKBUFFER_MB_MAX {
            return Err(invalid(
                "playback.demuxer_backbuffer_mb",
                "must be at most 1024",
            ));
        }
        if self.playback.demuxer_readahead_secs > READAHEAD_SECS_MAX {
            return Err(invalid(
                "playback.demuxer_readahead_secs",
                "must be at most 600",
            ));
        }
        if self.playback.outro_delay_ms > OUTRO_DELAY_MS_MAX {
            return Err(invalid(
                "playback.outro_delay_ms",
                "must be between 0 and 60000",
            ));
        }
        if !MAX_VOLUME_PERCENT_CHOICES.contains(&self.audio.max_volume_percent) {
            return Err(invalid(
                "audio.max_volume_percent",
                "must be 100, 150, 200 or 300",
            ));
        }
        let gains = &self.audio.equalizer_gains_db;
        if !gains.is_empty()
            && (gains.len() != EQUALIZER_BANDS
                || gains
                    .iter()
                    .any(|gain| !gain.is_finite() || gain.abs() > EQUALIZER_GAIN_LIMIT_DB))
        {
            return Err(invalid(
                "audio.equalizer_gains_db",
                "must be empty or ten values from -12 to 12",
            ));
        }
        if !LOUDNESS_TARGET_LUFS_RANGE.contains(&self.audio.loudness_target_lufs) {
            return Err(invalid(
                "audio.loudness_target_lufs",
                "must be between -30 and -10",
            ));
        }
        if self.subtitles.secondary_position_percent > 100 {
            return Err(invalid(
                "subtitles.secondary_position_percent",
                "must be at most 100",
            ));
        }
        if self.video.screenshot.template.trim().is_empty() {
            return Err(invalid("video.screenshot.template", "must not be empty"));
        }
        if let Some(geometry) = &self.window.geometry
            && (geometry.width == 0 || geometry.height == 0)
        {
            return Err(invalid(
                "window.geometry",
                "width and height must be non-zero",
            ));
        }
        Ok(())
    }
}

fn valid_series_guid(raw: &str) -> bool {
    let compact: String = match raw.len() {
        32 => raw.to_owned(),
        36 => {
            let bytes = raw.as_bytes();
            if ![8, 13, 18, 23]
                .iter()
                .all(|&index| bytes.get(index) == Some(&b'-'))
            {
                return false;
            }
            raw.chars().filter(|character| *character != '-').collect()
        }
        _ => return false,
    };
    compact.len() == 32
        && compact.bytes().all(|byte| byte.is_ascii_hexdigit())
        && compact.bytes().any(|byte| byte != b'0')
}

// ---------------------------------------------------------------------------
// Playback
// ---------------------------------------------------------------------------

/// Playback behaviour that is not specific to audio or video output.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PlaybackSettings {
    /// Legacy `video.sync_mode`. Default `display-resample` (`audio` stutters on some panels).
    pub sync_mode: SyncMode,
    /// Drop late video frames to keep audio and video aligned, instead of stalling.
    /// Maps to mpv `framedrop`. Default on.
    pub frame_drop: bool,
    /// Legacy `video.default_playback_speed`. Validated to 0.25..=4.0.
    pub default_speed: f32,
    /// Legacy `video.cache`, in MiB (enum 10 / 75 / 150 / 500). Maps to `demuxer-max-bytes`.
    /// Range 16..=4096 (M2-06). Out-of-range values are clamped on load.
    pub cache_mb: u32,
    /// Legacy `video.demuxer_backbuffer`, in MiB (enum 10 / 25 / 50 / 100). Maps to
    /// `demuxer-max-back-bytes`. Range 0..=1024 (M2-06).
    pub demuxer_backbuffer_mb: u32,
    /// Legacy `video.demuxer_readahead` in seconds (enum 1 / 5 / 10 / 20). Maps to
    /// `demuxer-readahead-secs`. Range 0..=600 (M2-06).
    pub demuxer_readahead_secs: u32,
    /// Legacy `video.skip_intro`. Shows a skip button for server intro segments (M2-08).
    /// Segments are requested only when this or `skip_outro` is on.
    pub skip_intro: bool,
    /// Legacy `video.skip_outro`. Shows a skip button for server outro segments (M2-08).
    pub skip_outro: bool,
    /// Seek past an intro without a press. Applies only when `skip_intro` is also on (M2-08).
    pub auto_skip_intro: bool,
    /// Seek past an outro without a press. Applies only when `skip_outro` is also on (M2-08).
    pub auto_skip_outro: bool,
    /// Milliseconds into an outro before its skip is offered. Range 0..=60000 (M2-08).
    /// Replaces the seconds key `skip_outro_delay_secs`; see `migrate::MIGRATIONS` (version 3).
    pub outro_delay_ms: u32,
    /// Legacy `video.external_player`: hand video off to an external player.
    pub external_player: bool,
    /// Legacy `video.max_streaming_bitrate` in bits per second. 0 means no cap.
    pub max_streaming_bitrate: u64,
    #[serde(rename = "_unknown", skip_serializing_if = "toml::Table::is_empty")]
    pub unknown: toml::Table,
}

impl Default for PlaybackSettings {
    fn default() -> Self {
        Self {
            sync_mode: SyncMode::DisplayResample,
            frame_drop: true,
            default_speed: 1.0,
            cache_mb: 75,
            demuxer_backbuffer_mb: 50,
            demuxer_readahead_secs: 5,
            skip_intro: false,
            skip_outro: false,
            auto_skip_intro: false,
            auto_skip_outro: false,
            outro_delay_ms: 0,
            external_player: false,
            max_streaming_bitrate: 0,
            unknown: toml::Table::new(),
        }
    }
}

/// Legacy `video.sync_mode`. The on-disk spellings are the mpv `video-sync` values accepted by
/// the player: `audio`, `display-resample` and `display-adrop`. Any other spelling is refused
/// on load.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SyncMode {
    Audio,
    DisplayResample,
    DisplayAdrop,
}

// ---------------------------------------------------------------------------
// Audio and passthrough
// ---------------------------------------------------------------------------

/// Audio output settings, including bitstream passthrough.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AudioSettings {
    /// Legacy `audio.devicetype`.
    pub output: AudioOutput,
    /// Legacy `audio.channels`.
    pub channels: ChannelLayout,
    /// Legacy `audio.device`. `"auto"` lets the backend choose.
    pub device: String,
    /// Legacy `audio.normalize`.
    pub normalize: bool,
    /// Legacy `audio.gapless` (maps to mpv `gapless-audio`).
    pub gapless: bool,
    /// Legacy `audio.max_volume` (enum 100 / 125 / 150 / 200). M2-16 allows only
    /// 100, 150, 200 or 300 (see [`MAX_VOLUME_PERCENT_CHOICES`]); the legacy 125 is dropped.
    /// The 300 choice was approved by the owner on 2026-10-15.
    /// Maps to mpv `volume-max`.
    pub max_volume_percent: u32,
    /// Legacy `audio.exclusive` (osx and windows only).
    pub exclusive: bool,
    /// Legacy `audio.passthrough.*`.
    pub passthrough: Passthrough,
    /// Ten-band equalizer gains in dB, from the 31 Hz band to the 16 kHz band. Empty means flat
    /// (no filter); otherwise exactly ten values from -12 to 12 (M6-08).
    pub equalizer_gains_db: Vec<f32>,
    /// Even out loudness between titles with mpv's `loudnorm` filter (M6-09).
    pub loudness_normalization: bool,
    /// Loudness target for `loudness_normalization`, in LUFS. Range -30..=-10 (M6-09).
    pub loudness_target_lufs: i32,
    /// Reduce the gap between quiet and loud passages (mpv `acompressor`, M6-09).
    pub dynamic_range_compression: bool,
    /// Keep the pitch when the playback speed changes (mpv `audio-pitch-correction`, M6-09).
    pub pitch_correction: bool,
    #[serde(rename = "_unknown", skip_serializing_if = "toml::Table::is_empty")]
    pub unknown: toml::Table,
}

/// Number of equalizer bands in `audio.equalizer_gains_db`.
pub const EQUALIZER_BANDS: usize = 10;

/// Largest equalizer gain, up or down, in dB.
pub const EQUALIZER_GAIN_LIMIT_DB: f32 = 12.0;

/// Loudness target range for `audio.loudness_target_lufs`, in LUFS.
pub const LOUDNESS_TARGET_LUFS_RANGE: std::ops::RangeInclusive<i32> = -30..=-10;

impl Default for AudioSettings {
    fn default() -> Self {
        Self {
            output: AudioOutput::Basic,
            channels: ChannelLayout::Stereo,
            device: "auto".to_owned(),
            normalize: true,
            gapless: true,
            max_volume_percent: 100,
            exclusive: false,
            passthrough: Passthrough::default(),
            equalizer_gains_db: Vec::new(),
            loudness_normalization: false,
            loudness_target_lufs: -16,
            dynamic_range_compression: false,
            pitch_correction: true,
            unknown: toml::Table::new(),
        }
    }
}

/// Legacy `audio.devicetype`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioOutput {
    Basic,
    Spdif,
    Hdmi,
}

/// Legacy `audio.channels`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChannelLayout {
    #[serde(rename = "auto")]
    Auto,
    #[serde(rename = "2.0")]
    Stereo,
    #[serde(rename = "5.1")]
    Surround51,
    #[serde(rename = "7.1")]
    Surround71,
}

/// Per-codec bitstream passthrough toggles (legacy `audio.passthrough.*`).
/// All default to off. Passthrough falls back to PCM when the device cannot accept the codec.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Passthrough {
    pub ac3: bool,
    pub eac3: bool,
    pub dts: bool,
    pub dts_hd: bool,
    pub truehd: bool,
}

// ---------------------------------------------------------------------------
// Video, zoom and screenshot
// ---------------------------------------------------------------------------

/// Video output, decoding, transcode preferences, zoom and screenshots.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct VideoSettings {
    /// Hardware decode backend (mpv `hwdec`). `auto` lets the player choose, `off` decodes in
    /// software, and the named values pin one backend. Unknown names are refused on load.
    pub decode_mode: DecodeMode,
    /// Legacy `video.deinterlace`.
    pub deinterlace: bool,
    /// Legacy `video.refreshrate.auto_switch`. Not supported on every platform.
    pub refresh_rate_auto_switch: bool,
    /// Legacy `video.refreshrate.avoid_25hz_30hz`.
    pub refresh_rate_avoid_25_30_hz: bool,
    /// Legacy `video.hdr_preference`.
    pub hdr_preference: HdrPreference,
    /// Legacy `video.hdr_passthrough`. HDR output is M6; this is only the setting.
    pub hdr_passthrough: bool,
    /// Legacy `video.aspect`.
    pub aspect: AspectMode,
    /// Legacy `video.allow_zoom`: Ctrl+wheel and pinch video zoom (0.5x to 4x).
    pub allow_zoom: bool,
    /// Transcode preferences (legacy `video.prefer_transcode_to_h265`,
    /// `video.force_transcode_*`, `video.always_force_transcode`, ...).
    pub transcode: TranscodePreferences,
    /// Screenshot output (legacy `video.screenshot.*`).
    pub screenshot: ScreenshotSettings,
    #[serde(rename = "_unknown", skip_serializing_if = "toml::Table::is_empty")]
    pub unknown: toml::Table,
}

impl Default for VideoSettings {
    fn default() -> Self {
        Self {
            decode_mode: DecodeMode::Auto,
            deinterlace: false,
            refresh_rate_auto_switch: false,
            refresh_rate_avoid_25_30_hz: true,
            hdr_preference: HdrPreference::Auto,
            hdr_passthrough: false,
            aspect: AspectMode::Normal,
            allow_zoom: true,
            transcode: TranscodePreferences::default(),
            screenshot: ScreenshotSettings::default(),
            unknown: toml::Table::new(),
        }
    }
}

/// Hardware decode backend selection. `Auto` and `Off` are general modes; the other variants
/// name one platform backend. Spellings are lowercase, so `d3d11va` is written as `"d3d11va"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DecodeMode {
    Auto,
    Off,
    D3d11va,
    Nvdec,
    Vaapi,
    Videotoolbox,
    Vulkan,
}

/// Legacy `video.hdr_preference`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HdrPreference {
    Auto,
    Dovi,
    Hdr10,
}

/// Legacy `video.aspect`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AspectMode {
    #[serde(rename = "custom")]
    Custom,
    #[serde(rename = "normal")]
    Normal,
    #[serde(rename = "zoom")]
    Zoom,
    #[serde(rename = "force_4_3")]
    Force4x3,
    #[serde(rename = "force_16_9")]
    Force16x9,
    #[serde(rename = "force_16_9_if_4_3")]
    Force16x9IfFourByThree,
    #[serde(rename = "stretch")]
    Stretch,
    #[serde(rename = "noscaling")]
    NoScaling,
}

/// Transcode preferences: which client-side conditions make the device profile
/// ask the server to transcode. Defaults follow the legacy defaults.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TranscodePreferences {
    pub prefer_h265: bool,
    pub allow_hevc: bool,
    pub force_hevc: bool,
    pub force_4k: bool,
    pub force_av1: bool,
    pub force_hdr: bool,
    pub force_hi10p: bool,
    pub force_dovi: bool,
    pub always_force: bool,
}

impl Default for TranscodePreferences {
    fn default() -> Self {
        Self {
            prefer_h265: false,
            allow_hevc: false,
            force_hevc: false,
            force_4k: false,
            force_av1: false,
            force_hdr: false,
            force_hi10p: false,
            force_dovi: true,
            always_force: false,
        }
    }
}

/// Screenshot output (legacy `video.screenshot.*`). The key itself is an input binding (`host:screenshot`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ScreenshotSettings {
    /// Empty means the platform pictures folder chosen by the platform crate.
    pub directory: String,
    /// Legacy `screenshot.template`. `%n` is replaced by a counter.
    pub template: String,
    /// Legacy `screenshot.format`.
    pub format: ImageFormat,
}

impl Default for ScreenshotSettings {
    fn default() -> Self {
        Self {
            directory: String::new(),
            template: "jellyfin-shot%n".to_owned(),
            format: ImageFormat::Png,
        }
    }
}

/// Screenshot image format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ImageFormat {
    Png,
    Jpg,
    Webp,
}

// ---------------------------------------------------------------------------
// Subtitles
// ---------------------------------------------------------------------------

/// Subtitle rendering (legacy `subtitles.*`).
///
/// Colour and style fields are `Option` because the legacy UI uses an empty string
/// to mean "player default". `None` is omitted from the file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SubtitleSettings {
    /// Legacy `subtitles.font`.
    pub font: String,
    /// Legacy `subtitles.size`. -1 means player default.
    pub size: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub border_color: Option<String>,
    /// Legacy `subtitles.border_size`. -1 means player default.
    pub border_size: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub background_color: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub background_transparency: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub placement: Option<String>,
    /// Legacy `subtitles.ass_style_override`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ass_style_override: Option<String>,
    /// Legacy `subtitles.ass_scale_border_and_shadow`.
    pub ass_scale_border_and_shadow: bool,
    /// Legacy `subtitles.secondary_pos`, in percent of screen height (enum 5 / 10 / 25 / 50).
    pub secondary_position_percent: u32,
    /// Legacy `subtitles.resync_on_start`.
    pub resync_on_start: bool,
    #[serde(rename = "_unknown", skip_serializing_if = "toml::Table::is_empty")]
    pub unknown: toml::Table,
}

impl Default for SubtitleSettings {
    fn default() -> Self {
        Self {
            font: "sans-serif".to_owned(),
            size: -1,
            color: None,
            border_color: None,
            border_size: -1,
            background_color: None,
            background_transparency: None,
            placement: None,
            ass_style_override: None,
            ass_scale_border_and_shadow: true,
            secondary_position_percent: 10,
            resync_on_start: true,
            unknown: toml::Table::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Window
// ---------------------------------------------------------------------------

/// Window state and behaviour (legacy `main.fullscreen`, `main.alwaysOnTop`,
/// `main.minimizeOnDefocus`, and the hidden `state.geometry`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WindowSettings {
    pub fullscreen: bool,
    /// Also used for picture-in-picture (M2).
    pub always_on_top: bool,
    /// Legacy `main.minimizeOnDefocus` (Windows only).
    pub minimize_on_defocus: bool,
    /// Last known window placement. `None` on first run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub geometry: Option<WindowGeometry>,
    #[serde(rename = "_unknown", skip_serializing_if = "toml::Table::is_empty")]
    pub unknown: toml::Table,
}

/// Remembered window placement, including the monitor it was on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowGeometry {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    /// Monitor identifier as reported by the platform crate. Restores to the primary if absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub monitor: Option<String>,
}

// ---------------------------------------------------------------------------
// Input
// ---------------------------------------------------------------------------

/// Input device settings. Bindings themselves live in the keymap files owned by `kattri-input`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct InputSettings {
    /// Legacy `main.sdlEnabled`.
    pub sdl_gamepad_enabled: bool,
    /// Legacy `main.enableWebGamepad`.
    pub web_gamepad_enabled: bool,
    /// Legacy `main.enableInputRepeat`.
    pub key_repeat: bool,
    /// Optional user keymap file that overrides the bundled defaults.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub keymap_path: Option<PathBuf>,
    #[serde(rename = "_unknown", skip_serializing_if = "toml::Table::is_empty")]
    pub unknown: toml::Table,
}

impl Default for InputSettings {
    fn default() -> Self {
        Self {
            sdl_gamepad_enabled: true,
            web_gamepad_enabled: true,
            key_repeat: true,
            keymap_path: None,
            unknown: toml::Table::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Plugins and updates
// ---------------------------------------------------------------------------

/// Plugin host switch (legacy `plugins.enabled`). Off by default; the runtime is M4.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PluginSettings {
    pub enabled: bool,
    /// Discord Rich Presence (M4-09). Off until the user turns it on, because it shows what
    /// is playing to other people.
    pub discord_presence: bool,
    #[serde(rename = "_unknown", skip_serializing_if = "toml::Table::is_empty")]
    pub unknown: toml::Table,
}

/// Update checks (legacy `main.checkForUpdates`). The updater itself is M4.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UpdateSettings {
    pub check_for_updates: bool,
    #[serde(rename = "_unknown", skip_serializing_if = "toml::Table::is_empty")]
    pub unknown: toml::Table,
}

impl Default for UpdateSettings {
    fn default() -> Self {
        Self {
            check_for_updates: true,
            unknown: toml::Table::new(),
        }
    }
}

/// Longest accepted PIN record. A record from `kattri-parental` is about 150 bytes.
pub const MAX_PIN_RECORD_LEN: usize = 256;

/// Local crash reports (M4-21). Off by default. Nothing is uploaded either way.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct DiagnosticsSettings {
    pub crash_reporting: bool,
    /// How much `kattri.log` records. Applied while the app runs. The `KATTRI_LOG` environment
    /// variable, when set to a level, takes precedence.
    pub log_level: LogDetail,
    #[serde(rename = "_unknown", skip_serializing_if = "toml::Table::is_empty")]
    pub unknown: toml::Table,
}

/// Log detail choices offered in Settings. `Trace` and `Off` are left to `KATTRI_LOG`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogDetail {
    Error,
    Warn,
    #[default]
    Info,
    Debug,
}

/// Admin-lite read-only dashboard (M4-22). Off by default. The server still decides who
/// may read each call; this switch only shows the navigation entry.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AdminSettings {
    pub dashboard: bool,
    #[serde(rename = "_unknown", skip_serializing_if = "toml::Table::is_empty")]
    pub unknown: toml::Table,
}

/// Parental controls (M4-20). The PIN is kept only as a salted Argon2id record, written by
/// `kattri-parental`. A plain PIN is never stored here.
#[derive(Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ParentalSettings {
    /// Empty when no PIN is set.
    pub pin_record: String,
    /// Highest server rating shown. `None` means no limit.
    pub max_rating: Option<u32>,
    #[serde(rename = "_unknown", skip_serializing_if = "toml::Table::is_empty")]
    pub unknown: toml::Table,
}

impl std::fmt::Debug for ParentalSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The record is a hash, but it is still credential material: keep it out of logs.
        f.debug_struct("ParentalSettings")
            .field(
                "pin_record",
                &if self.pin_record.is_empty() {
                    "<unset>"
                } else {
                    "<redacted>"
                },
            )
            .field("max_rating", &self.max_rating)
            .finish_non_exhaustive()
    }
}
