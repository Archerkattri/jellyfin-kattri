//! Rust-owned, typed application settings for Jellyfin Kattri.
//!
//! This crate replaces the Qt-era `legacy/qt-neo/resources/settings/settings_description.json`
//! with a typed schema. Every category of the Neo settings surface has a section here:
//! playback, audio (including passthrough), subtitles, video (zoom and screenshot),
//! window, input, plugins and updates.
//!
//! Status: the schema, defaults, validation, TOML round-trip, atomic save and the
//! versioned migration framework are implemented and unit-tested. Applying these values
//! to mpv, the UI or the OS is NOT part of this crate; those consumers land in M2.
//!
//! Rules:
//! - No credentials, tokens or server URLs are stored here (M2 acceptance criterion).
//! - Unknown keys are ignored on load so older builds can read newer files.
//! - A file with a schema version newer than [`CURRENT_SCHEMA_VERSION`] is rejected,
//!   never silently downgraded.
//!
//! Modules:
//! - [`schema`]: typed sections and their defaults.
//! - [`migrate`]: ordered, versioned migrations.
//! - [`store`]: load, parse, validate, and atomic save.
//! - [`profile`]: named profiles, each with its own settings file, a default profile, an
//!   active-profile pointer and a versioned index (M2-13).
//! - [`error`]: the error type shared by all of the above.

pub mod error;
pub mod guard;
pub mod migrate;
pub mod profile;
pub mod schema;
pub mod store;

pub use error::SettingsError;
pub use profile::{
    DEFAULT_PROFILE_NAME, PROFILE_INDEX_VERSION, ProfileError, ProfileIndex, ProfileStore,
    parse_index, to_index_string, validate_profile_name,
};
pub use schema::{
    AspectMode, AudioOutput, AudioSettings, BACKBUFFER_MB_MAX, BackdropPreference, CACHE_MB_MAX,
    CACHE_MB_MIN, CURRENT_SCHEMA_VERSION, ChannelLayout, DecodeMode, EQUALIZER_BANDS,
    EQUALIZER_GAIN_LIMIT_DB, GeneralSettings, HdrPreference, ImageFormat, InputSettings,
    InterfaceSize, LOUDNESS_TARGET_LUFS_RANGE, LogDetail, MAX_SERIES_LANGUAGE_ENTRIES, Passthrough,
    PlaybackSettings, PluginSettings, READAHEAD_SECS_MAX, ScreenshotSettings,
    SeriesLanguageSetting, Settings, SubtitleSettings, SyncMode, ToneMapping, TranscodePreferences,
    UpdateSettings, VideoSettings, WindowGeometry, WindowSettings,
};
pub use store::{
    LoadWarning, load, load_from_str, load_from_str_with_report, load_with_report, save,
    to_toml_string,
};
