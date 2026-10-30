//! Load, parse, validate and atomically save the settings file.
//!
//! Load path: read TOML -> read `schema_version` -> migrate -> deserialize -> validate.
//! Save path: validate -> serialize -> write a unique sibling temp file -> fsync -> rename over the target.
//!
//! A missing file is not an error: [`load`] returns [`Settings::default`].
//!
//! The settings file is TOML, not JSON or a platform registry. It lives under the application
//! config directory (`kattri_platform::AppDirs::config_dir`). Credentials never go in this file.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use toml::Value;

use crate::error::SettingsError;
use crate::guard;
use crate::migrate;
use crate::schema::Settings;

/// Maximum settings document size accepted from disk or by the string parser.
pub const MAX_SETTINGS_BYTES: usize = 1 << 20;

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// One entry in the report returned by the load functions. An entry never carries the value
/// of a setting, only the dotted field path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadWarning {
    /// A buffer field was outside its range and was clamped into it (M2-06).
    ClampedField(&'static str),
    /// A text field held a value that looks like a URL or credential. The value was dropped,
    /// so the field has its default, and the file is not refused.
    SecretLikeValue(String),
    /// The file was malformed, moved aside, and defaults were loaded. Carries only its filename.
    CorruptFile(String),
    /// A future schema is left untouched and exposes defaults as a read-only snapshot.
    FutureSchema(u32),
}

/// Load settings from `path`. A missing file yields defaults.
pub fn load(path: &Path) -> Result<Settings, SettingsError> {
    load_with_report(path).map(|(settings, _)| settings)
}

/// Like [`load`], and also returns the load report: clamped fields, dropped credential-like
/// values, corrupt-file recovery, and future-schema read-only fallback. The caller logs each
/// warning without logging setting values.
pub fn load_with_report(path: &Path) -> Result<(Settings, Vec<LoadWarning>), SettingsError> {
    match File::open(path) {
        Ok(file) => {
            let mut bytes = Vec::new();
            file.take((MAX_SETTINGS_BYTES + 1) as u64)
                .read_to_end(&mut bytes)?;
            if bytes.len() > MAX_SETTINGS_BYTES {
                return move_corrupt_aside(path);
            }
            let text = match String::from_utf8(bytes) {
                Ok(text) => text,
                Err(_) => return move_corrupt_aside(path),
            };
            match load_from_str_with_report(&text) {
                Ok(result) => Ok(result),
                // Only a document that cannot be parsed as TOML at all is unrecoverable.
                // A well-formed file with one bad value is kept so the user can fix it; the
                // app shows the error and refuses to overwrite it.
                Err(SettingsError::Syntax(_) | SettingsError::TooLarge { .. }) => {
                    move_corrupt_aside(path)
                }
                Err(error) => Err(error),
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            Ok((Settings::default(), Vec::new()))
        }
        Err(err) => Err(SettingsError::Io(err)),
    }
}

/// Parse, migrate, deserialize and validate settings from TOML text.
///
/// Unknown keys are retained in the model for forward compatibility, except a key whose name
/// looks like a credential (see [`crate::guard`]): that file is refused with
/// [`SettingsError::SecretLikeKey`]. The caller must not overwrite the file in that case.
/// A value that looks like a URL or credential is dropped and reported; see
/// [`load_from_str_with_report`].
pub fn load_from_str(text: &str) -> Result<Settings, SettingsError> {
    load_from_str_with_report(text).map(|(settings, _)| settings)
}

/// Like [`load_from_str`], and also returns the load report.
///
/// Buffer fields are clamped rather than rejected on load, so a hand-edited value never blocks
/// startup; each clamped field is reported. Other out-of-range fields are still rejected.
/// A string value that looks like a URL or credential is removed before deserialization, so
/// the field takes its default and the value is never kept. It is reported by field path
/// only, and the file is still loaded.
pub fn load_from_str_with_report(
    text: &str,
) -> Result<(Settings, Vec<LoadWarning>), SettingsError> {
    if text.len() > MAX_SETTINGS_BYTES {
        return Err(SettingsError::TooLarge {
            max_bytes: MAX_SETTINGS_BYTES,
        });
    }
    let table: toml::Table = text
        .parse()
        .map_err(|error: toml::de::Error| SettingsError::Syntax(error.to_string()))?;
    if let Some(key) = guard::find_secret_like_key(&table) {
        return Err(SettingsError::SecretLikeKey { key });
    }
    let file_version = migrate::read_version(&table)?;
    if file_version > crate::schema::CURRENT_SCHEMA_VERSION {
        let settings = Settings {
            read_only_schema_version: Some(file_version),
            ..Settings::default()
        };
        return Ok((settings, vec![LoadWarning::FutureSchema(file_version)]));
    }
    let mut table = migrate::migrate(table)?;
    preserve_unknown_fields(&mut table);
    let mut warnings: Vec<LoadWarning> = guard::remove_secret_like_values(&mut table)
        .into_iter()
        .map(LoadWarning::SecretLikeValue)
        .collect();
    let mut settings: Settings = Value::Table(table)
        .try_into()
        .map_err(SettingsError::Parse)?;
    warnings.extend(
        settings
            .playback
            .clamp_buffers()
            .into_iter()
            .map(LoadWarning::ClampedField),
    );
    settings.validate()?;
    Ok((settings, warnings))
}

fn preserve_unknown_fields(table: &mut toml::Table) {
    move_unknown_fields(
        table,
        &[
            "schema_version",
            "general",
            "playback",
            "audio",
            "video",
            "subtitles",
            "window",
            "input",
            "plugins",
            "updates",
            "diagnostics",
            "parental",
            "admin",
            "series_languages",
        ],
    );
    for (section, known) in [
        (
            "general",
            &[
                "high_contrast",
                "reduce_motion",
                "backdrop",
                "custom_backdrop_path",
                "interface_size",
                "home_continue_watching",
                "home_next_up",
                "home_latest",
                "home_my_media",
                "home_collections",
            ][..],
        ),
        (
            "playback",
            &[
                "sync_mode",
                "frame_drop",
                "default_speed",
                "cache_mb",
                "demuxer_backbuffer_mb",
                "demuxer_readahead_secs",
                "skip_intro",
                "skip_outro",
                "auto_skip_intro",
                "auto_skip_outro",
                "outro_delay_ms",
                "resume_rewind_seconds",
                "auto_play_next_episode",
                "external_player",
                "max_streaming_bitrate",
            ][..],
        ),
        (
            "audio",
            &[
                "output",
                "channels",
                "device",
                "normalize",
                "gapless",
                "max_volume_percent",
                "exclusive",
                "passthrough",
                "equalizer_gains_db",
                "loudness_normalization",
                "loudness_target_lufs",
                "dynamic_range_compression",
                "pitch_correction",
            ][..],
        ),
        (
            "video",
            &[
                "decode_mode",
                "deinterlace",
                "refresh_rate_auto_switch",
                "refresh_rate_avoid_25_30_hz",
                "hdr_preference",
                "hdr_passthrough",
                "tone_mapping",
                "aspect",
                "allow_zoom",
                "transcode",
                "screenshot",
            ][..],
        ),
        (
            "subtitles",
            &[
                "font",
                "bold",
                "size",
                "color",
                "border_color",
                "border_size",
                "background_color",
                "background_transparency",
                "placement",
                "ass_style_override",
                "ass_scale_border_and_shadow",
                "secondary_position_percent",
                "resync_on_start",
            ][..],
        ),
        (
            "window",
            &[
                "fullscreen",
                "always_on_top",
                "minimize_on_defocus",
                "geometry",
            ][..],
        ),
        (
            "input",
            &[
                "sdl_gamepad_enabled",
                "web_gamepad_enabled",
                "key_repeat",
                "keymap_path",
            ][..],
        ),
        ("plugins", &["enabled", "discord_presence"][..]),
        ("admin", &["dashboard"][..]),
        ("updates", &["check_for_updates"][..]),
        ("diagnostics", &["crash_reporting", "log_level"][..]),
        ("parental", &["pin_record", "max_rating"][..]),
    ] {
        if let Some(toml::Value::Table(section_table)) = table.get_mut(section) {
            move_unknown_fields(section_table, known);
        }
    }
}

fn move_unknown_fields(table: &mut toml::Table, known: &[&str]) {
    let mut unknown = match table.get("_unknown") {
        Some(toml::Value::Table(existing)) => existing.clone(),
        _ => toml::Table::new(),
    };
    let unknown_keys: Vec<_> = table
        .keys()
        .filter(|key| key.as_str() != "_unknown" && !known.contains(&key.as_str()))
        .cloned()
        .collect();
    for key in unknown_keys {
        if let Some(value) = table.remove(&key) {
            unknown.insert(key, value);
        }
    }
    if !unknown.is_empty() || table.contains_key("_unknown") {
        table.insert("_unknown".to_owned(), toml::Value::Table(unknown));
    }
}

fn move_corrupt_aside(path: &Path) -> Result<(Settings, Vec<LoadWarning>), SettingsError> {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let filename = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "settings".to_owned());
    let extension = path
        .extension()
        .map(|extension| extension.to_string_lossy().into_owned())
        .unwrap_or_default();
    let base_extension = if extension.is_empty() {
        format!("corrupt-{timestamp}")
    } else {
        format!("{extension}.corrupt-{timestamp}")
    };
    let mut aside = path.with_extension(&base_extension);
    let mut suffix = 1_u32;
    while aside.exists() {
        aside = path.with_extension(format!("{base_extension}-{suffix}"));
        suffix = suffix.saturating_add(1);
    }
    fs::rename(path, &aside)?;
    Ok((
        Settings::default(),
        vec![LoadWarning::CorruptFile(filename)],
    ))
}

/// Serialize settings to pretty TOML. Validates first, and refuses any string value that
/// looks like a URL or credential, so such a value never reaches the file.
pub fn to_toml_string(settings: &Settings) -> Result<String, SettingsError> {
    if let Some(found) = settings.read_only_schema_version {
        return Err(SettingsError::FutureVersion {
            found,
            supported: crate::schema::CURRENT_SCHEMA_VERSION,
        });
    }
    settings.validate()?;
    let text = toml::to_string_pretty(settings)?;
    let table: toml::Table = text
        .parse()
        .map_err(|error: toml::de::Error| SettingsError::Syntax(error.to_string()))?;
    if let Some(field) = guard::find_secret_like_value(&table) {
        return Err(SettingsError::SecretLikeValue { field });
    }
    Ok(text)
}

/// Validate and atomically write settings to `path`.
///
/// The data goes to a temporary file in the same directory, is flushed to disk,
/// and is then renamed over `path`. A crash leaves either the old file or the new
/// file, never a partial one. Stale temp files are removed on failure.
pub fn save(path: &Path, settings: &Settings) -> Result<(), SettingsError> {
    let text = to_toml_string(settings)?;
    if text.len() > MAX_SETTINGS_BYTES {
        return Err(SettingsError::TooLarge {
            max_bytes: MAX_SETTINGS_BYTES,
        });
    }

    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(dir)?;

    let (tmp, mut file) = create_temp(path)?;
    let write_result = (|| -> Result<(), std::io::Error> {
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        drop(file);
        Ok(())
    })();
    if let Err(err) = write_result {
        let _ = fs::remove_file(&tmp);
        return Err(SettingsError::Io(err));
    }

    // std::fs::rename replaces an existing destination on Windows and Unix.
    if let Err(err) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(SettingsError::Io(err));
    }
    sync_parent(path)?;
    Ok(())
}

fn create_temp(path: &Path) -> Result<(std::path::PathBuf, File), SettingsError> {
    for _ in 0..100 {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let tmp = path.with_extension(format!("tmp-{}-{sequence}", std::process::id()));
        match OpenOptions::new().write(true).create_new(true).open(&tmp) {
            Ok(file) => return Ok((tmp, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(SettingsError::Io(error)),
        }
    }
    Err(SettingsError::Io(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "could not allocate a unique temporary settings file",
    )))
}

#[cfg(unix)]
fn sync_parent(path: &Path) -> Result<(), SettingsError> {
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    File::open(dir)?.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
fn sync_parent(_path: &Path) -> Result<(), SettingsError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::LogDetail;
    use crate::schema::{
        AspectMode, AudioOutput, BACKBUFFER_MB_MAX, CACHE_MB_MAX, CACHE_MB_MIN,
        CURRENT_SCHEMA_VERSION, ChannelLayout, DecodeMode, READAHEAD_SECS_MAX,
        SeriesLanguageSetting, SyncMode, WindowGeometry,
    };
    use std::path::PathBuf;

    fn populated() -> Settings {
        let mut s = Settings::default();
        s.general.high_contrast = true;
        s.general.reduce_motion = false;
        s.playback.sync_mode = SyncMode::Audio;
        s.playback.frame_drop = false;
        s.playback.default_speed = 1.5;
        s.playback.cache_mb = 128;
        s.playback.demuxer_backbuffer_mb = 100;
        s.playback.demuxer_readahead_secs = 12;
        s.playback.skip_intro = true;
        s.playback.skip_outro = true;
        s.playback.auto_skip_intro = true;
        s.playback.auto_skip_outro = true;
        s.playback.outro_delay_ms = 10_000;
        s.playback.external_player = true;
        s.playback.max_streaming_bitrate = 20_000_000;
        s.video.decode_mode = DecodeMode::Vaapi;
        s.audio.output = AudioOutput::Spdif;
        s.audio.channels = ChannelLayout::Surround51;
        s.audio.device = "alsa/test-device".to_owned();
        s.audio.normalize = false;
        s.audio.gapless = false;
        s.audio.max_volume_percent = 150;
        s.audio.exclusive = true;
        s.diagnostics.log_level = LogDetail::Debug;
        s.audio.equalizer_gains_db = vec![3.0, 2.5, 0.0, -1.0, 0.0, 0.5, 1.0, 2.0, 4.0, 6.0];
        s.audio.loudness_normalization = true;
        s.audio.loudness_target_lufs = -23;
        s.audio.dynamic_range_compression = true;
        s.audio.pitch_correction = false;
        s.audio.passthrough.ac3 = true;
        s.audio.passthrough.eac3 = true;
        s.audio.passthrough.dts = true;
        s.audio.passthrough.dts_hd = true;
        s.audio.passthrough.truehd = true;
        s.video.aspect = AspectMode::Force16x9;
        s.video.deinterlace = true;
        s.video.refresh_rate_auto_switch = true;
        s.video.refresh_rate_avoid_25_30_hz = false;
        s.video.hdr_preference = crate::schema::HdrPreference::Hdr10;
        s.video.hdr_passthrough = true;
        s.video.allow_zoom = false;
        s.video.transcode.prefer_h265 = true;
        s.video.transcode.allow_hevc = true;
        s.video.transcode.force_hevc = true;
        s.video.transcode.force_4k = true;
        s.video.transcode.force_av1 = true;
        s.video.transcode.force_hdr = true;
        s.video.transcode.force_hi10p = true;
        s.video.transcode.force_dovi = false;
        s.video.transcode.always_force = true;
        s.video.screenshot.directory = "Screenshots".to_owned();
        s.video.screenshot.template = "capture-%n".to_owned();
        s.video.screenshot.format = crate::schema::ImageFormat::Webp;
        s.subtitles.font = "Noto Sans".to_owned();
        s.subtitles.size = 24;
        s.subtitles.color = Some("#FBF93E".to_owned());
        s.subtitles.border_color = Some("#010203".to_owned());
        s.subtitles.border_size = 2;
        s.subtitles.background_color = Some("#202020".to_owned());
        s.subtitles.background_transparency = Some("25%".to_owned());
        s.subtitles.placement = Some("top".to_owned());
        s.subtitles.ass_style_override = Some("force".to_owned());
        s.subtitles.ass_scale_border_and_shadow = false;
        s.subtitles.secondary_position_percent = 25;
        s.subtitles.resync_on_start = false;
        s.window.fullscreen = true;
        s.window.always_on_top = true;
        s.window.minimize_on_defocus = true;
        s.window.geometry = Some(WindowGeometry {
            x: 40,
            y: -1080,
            width: 1280,
            height: 720,
            monitor: Some("DISPLAY2".to_owned()),
        });
        s.input.sdl_gamepad_enabled = false;
        s.input.web_gamepad_enabled = false;
        s.input.key_repeat = false;
        s.input.keymap_path = Some(PathBuf::from("C:/Users/me/keys.json"));
        s.plugins.enabled = true;
        s.updates.check_for_updates = false;
        s.series_languages = vec![
            SeriesLanguageSetting {
                series_id: "0123456789abcdef0123456789abcdef".to_owned(),
                audio_language: Some("jpn".to_owned()),
                audio_title: Some("Original".to_owned()),
                subtitle_language: Some("eng".to_owned()),
                subtitle_title: Some("English".to_owned()),
                subtitle_disabled: false,
            },
            SeriesLanguageSetting {
                series_id: "1123456789abcdef0123456789abcdef".to_owned(),
                subtitle_disabled: true,
                ..SeriesLanguageSetting::default()
            },
        ];
        s
    }

    #[test]
    fn settings_parser_rejects_documents_over_the_input_limit() {
        let text = format!("#{}", "x".repeat(MAX_SETTINGS_BYTES));
        assert!(matches!(
            load_from_str(&text),
            Err(SettingsError::TooLarge {
                max_bytes: MAX_SETTINGS_BYTES
            })
        ));
    }

    #[test]
    fn default_settings_round_trip_through_toml() {
        let text = to_toml_string(&Settings::default()).unwrap();
        assert_eq!(load_from_str(&text).unwrap(), Settings::default());
    }

    #[test]
    fn subtitle_bold_defaults_off_and_round_trips() {
        assert!(!Settings::default().subtitles.bold);
        let loaded = load_from_str("[subtitles]\nbold = true\n").expect("load");
        assert!(loaded.subtitles.bold);
        let saved = to_toml_string(&loaded).expect("save");
        assert!(load_from_str(&saved).expect("reload").subtitles.bold);
    }

    #[test]
    fn backdrop_preferences_default_and_round_trip() {
        use crate::BackdropPreference;
        let defaults = Settings::default();
        assert_eq!(defaults.general.backdrop, BackdropPreference::Server);
        assert_eq!(defaults.general.custom_backdrop_path, None);
        for (preference, spelling) in [
            (BackdropPreference::Server, "server"),
            (BackdropPreference::Custom, "custom"),
            (BackdropPreference::Off, "off"),
        ] {
            let mut settings = Settings::default();
            settings.general.backdrop = preference;
            settings.general.custom_backdrop_path = Some("art/backdrop.jpg".into());
            let text = to_toml_string(&settings).unwrap();
            assert!(text.contains(&format!("backdrop = \"{spelling}\"")));
            assert_eq!(load_from_str(&text).unwrap().general, settings.general);
        }
        let legacy = to_toml_string(&defaults)
            .unwrap()
            .lines()
            .filter(|line| !line.starts_with("backdrop = "))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(
            load_from_str(&legacy).unwrap().general.backdrop,
            BackdropPreference::Server
        );
    }

    #[test]
    fn every_tone_mapping_round_trips_and_names_its_mpv_value() {
        use crate::ToneMapping;
        assert_eq!(Settings::default().video.tone_mapping, ToneMapping::Auto);
        let mpv_names: Vec<_> = ToneMapping::ALL.map(ToneMapping::mpv_value).into();
        assert_eq!(
            mpv_names,
            ["auto", "bt.2390", "mobius", "hable", "reinhard", "clip"]
        );
        for curve in ToneMapping::ALL {
            let mut settings = Settings::default();
            settings.video.tone_mapping = curve;
            let text = to_toml_string(&settings).unwrap();
            assert_eq!(
                load_from_str(&text).unwrap().video.tone_mapping,
                curve,
                "{text}"
            );
        }
        assert!(load_from_str("[video]\ntone_mapping = \"banana\"\n").is_err());
    }

    #[test]
    fn the_interface_size_round_trips_and_defaults_to_standard() {
        use crate::InterfaceSize;
        assert_eq!(
            Settings::default().general.interface_size,
            InterfaceSize::Standard
        );
        for size in InterfaceSize::ALL {
            let mut settings = Settings::default();
            settings.general.interface_size = size;
            let text = to_toml_string(&settings).unwrap();
            assert_eq!(
                load_from_str(&text).unwrap().general.interface_size,
                size,
                "{text}"
            );
        }
        let text = to_toml_string(&Settings::default()).unwrap().replace(
            "interface_size = \"standard\"",
            "interface_size = \"ten-foot\"",
        );
        assert_eq!(
            load_from_str(&text).unwrap().general.interface_size,
            InterfaceSize::TenFoot
        );
    }

    #[test]
    fn an_unknown_interface_size_is_refused_not_guessed() {
        let text = to_toml_string(&Settings::default())
            .unwrap()
            .replace("interface_size = \"standard\"", "interface_size = \"huge\"");
        assert!(load_from_str(&text).is_err());
    }

    #[test]
    fn larger_sizes_scale_the_interface_up_in_order() {
        use crate::InterfaceSize;
        let factors = InterfaceSize::ALL.map(InterfaceSize::zoom_factor);
        assert_eq!(factors[0], 1.0);
        assert!(factors.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn populated_settings_round_trip_through_toml() {
        let text = to_toml_string(&populated()).unwrap();
        assert_eq!(load_from_str(&text).unwrap(), populated());
    }

    #[test]
    fn dashboard_and_presence_switches_are_off_by_default_and_round_trip() {
        let defaults = Settings::default();
        assert!(!defaults.admin.dashboard);
        assert!(!defaults.plugins.discord_presence);

        let mut on = Settings::default();
        on.admin.dashboard = true;
        on.plugins.discord_presence = true;
        let loaded = load_from_str(&to_toml_string(&on).unwrap()).unwrap();
        assert!(loaded.admin.dashboard);
        assert!(loaded.plugins.discord_presence);
        // A known key must stay out of the forward-compatibility table.
        assert!(loaded.admin.unknown.is_empty());
        assert!(loaded.plugins.unknown.is_empty());
    }

    #[test]
    fn series_language_records_validate_ids_languages_and_duplicate_series() {
        let valid = SeriesLanguageSetting {
            series_id: "0123456789abcdef0123456789abcdef".to_owned(),
            audio_language: Some("jpn".to_owned()),
            audio_title: Some("Original".to_owned()),
            ..SeriesLanguageSetting::default()
        };
        let settings = Settings {
            series_languages: vec![valid.clone(), valid.clone()],
            ..Settings::default()
        };
        assert!(matches!(
            settings.validate(),
            Err(SettingsError::Invalid {
                field: "series_languages.series_id",
                ..
            })
        ));

        let settings = Settings {
            series_languages: vec![SeriesLanguageSetting {
                series_id: "not-a-guid".to_owned(),
                ..SeriesLanguageSetting::default()
            }],
            ..Settings::default()
        };
        assert!(matches!(
            settings.validate(),
            Err(SettingsError::Invalid {
                field: "series_languages.series_id",
                ..
            })
        ));

        let settings = Settings {
            series_languages: vec![SeriesLanguageSetting {
                series_id: valid.series_id,
                audio_language: Some("japanese".to_owned()),
                ..SeriesLanguageSetting::default()
            }],
            ..Settings::default()
        };
        assert!(matches!(
            settings.validate(),
            Err(SettingsError::Invalid {
                field: "series_languages.audio_language",
                ..
            })
        ));
    }

    #[test]
    fn empty_document_loads_defaults() {
        assert_eq!(load_from_str("").unwrap(), Settings::default());
    }

    #[test]
    fn partial_document_fills_remaining_defaults() {
        let s = load_from_str("[audio]\ngapless = false\n").unwrap();
        assert!(!s.audio.gapless);
        assert_eq!(s.audio.device, "auto");
        assert_eq!(s.video.decode_mode, DecodeMode::Auto);
    }

    #[test]
    fn unknown_keys_are_preserved_for_forward_compatibility() {
        let s =
            load_from_str("future_top = 1\n[playback]\nfuture_flag = true\nskip_intro = true\n")
                .unwrap();
        assert!(s.playback.skip_intro);
        assert_eq!(s.unknown.get("future_top"), Some(&toml::Value::Integer(1)));
        assert_eq!(
            s.playback.unknown.get("future_flag"),
            Some(&toml::Value::Boolean(true))
        );
        let saved = to_toml_string(&s).unwrap();
        assert!(saved.contains("[_unknown]"), "{saved}");
        assert!(saved.contains("[playback._unknown]"), "{saved}");
        assert!(saved.contains("future_top = 1"));
        assert!(saved.contains("future_flag = true"));
        let reloaded = load_from_str(&saved).unwrap();
        assert_eq!(reloaded.unknown, s.unknown);
        assert_eq!(reloaded.playback.unknown, s.playback.unknown);
    }

    #[test]
    fn unknown_keys_are_preserved_in_every_settings_section() {
        for section in [
            "general",
            "playback",
            "audio",
            "video",
            "subtitles",
            "window",
            "input",
            "plugins",
            "updates",
        ] {
            let input = format!(
                "schema_version = {CURRENT_SCHEMA_VERSION}\n[{section}]\nfuture_section_key = \"{section}-value\"\n"
            );
            let loaded = load_from_str(&input).unwrap_or_else(|error| {
                panic!("failed to load unknown key for [{section}]: {error}")
            });
            let saved = to_toml_string(&loaded).unwrap_or_else(|error| {
                panic!("failed to save settings with unknown key for [{section}]: {error}")
            });
            let saved_value: toml::Table = saved.parse().unwrap_or_else(|error| {
                panic!("failed to parse saved settings for [{section}]: {error}")
            });

            assert_eq!(
                saved_value
                    .get(section)
                    .and_then(|value| value.get("_unknown"))
                    .and_then(|value| value.get("future_section_key")),
                Some(&toml::Value::String(format!("{section}-value"))),
                "unknown field should be saved in [{section}._unknown]; saved TOML:\n{saved}"
            );

            let reloaded = load_from_str(&saved).unwrap_or_else(|error| {
                panic!("failed to reload settings with unknown key for [{section}]: {error}")
            });
            let resaved = to_toml_string(&reloaded).unwrap_or_else(|error| {
                panic!("failed to resave settings with unknown key for [{section}]: {error}")
            });
            let resaved_value: toml::Table = resaved.parse().unwrap_or_else(|error| {
                panic!("failed to parse resaved settings for [{section}]: {error}")
            });
            assert_eq!(
                resaved_value
                    .get(section)
                    .and_then(|value| value.get("_unknown"))
                    .and_then(|value| value.get("future_section_key")),
                Some(&toml::Value::String(format!("{section}-value"))),
                "unknown field should survive a second load/save for [{section}._unknown]"
            );
        }
    }

    #[test]
    fn future_schema_version_is_refused() {
        let (settings, warnings) = load_from_str_with_report("schema_version = 99\n").unwrap();
        assert_eq!(
            settings,
            Settings {
                read_only_schema_version: Some(99),
                ..Settings::default()
            }
        );
        assert_eq!(warnings, vec![LoadWarning::FutureSchema(99)]);
        assert!(matches!(
            to_toml_string(&settings).unwrap_err(),
            SettingsError::FutureVersion {
                found: 99,
                supported: CURRENT_SCHEMA_VERSION
            }
        ));
    }

    #[test]
    fn future_schema_file_is_left_byte_for_byte_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");
        let original = b"schema_version = 99\n[future_section]\nfuture_value = \"keep me\"\n";
        fs::write(&path, original).unwrap();

        let (settings, warnings) = load_with_report(&path).unwrap();

        assert_eq!(warnings, vec![LoadWarning::FutureSchema(99)]);
        assert_eq!(settings.read_only_schema_version, Some(99));
        assert!(matches!(
            save(&path, &settings),
            Err(SettingsError::FutureVersion {
                found: 99,
                supported: CURRENT_SCHEMA_VERSION
            })
        ));
        assert_eq!(fs::read(&path).unwrap(), original);
    }

    #[test]
    fn a_bad_value_keeps_the_file_and_a_syntax_error_moves_it_aside() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");

        // Well-formed TOML with an unknown enum spelling: kept, error returned.
        let original = b"schema_version = 3\n[diagnostics]\nlog_level = \"loud\"\n";
        fs::write(&path, original).unwrap();
        assert!(matches!(
            load_with_report(&path),
            Err(SettingsError::Parse(_))
        ));
        assert_eq!(fs::read(&path).unwrap(), original);
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);

        // Invalid TOML: moved aside, defaults loaded.
        fs::write(&path, b"this is not toml = = =\n").unwrap();
        let (settings, warnings) = load_with_report(&path).unwrap();
        assert_eq!(settings, Settings::default());
        assert!(matches!(warnings.as_slice(), [LoadWarning::CorruptFile(_)]));
        assert!(!path.exists());
        let aside = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .find(|name| name.starts_with("settings.toml.corrupt-"))
            .expect("the invalid file was moved aside");
        assert!(aside.len() > "settings.toml.corrupt-".len());
    }

    #[test]
    fn log_level_defaults_to_info_and_accepts_the_four_names_only() {
        assert_eq!(
            load_from_str("").unwrap().diagnostics.log_level,
            LogDetail::Info
        );
        for (name, level) in [
            ("error", LogDetail::Error),
            ("warn", LogDetail::Warn),
            ("info", LogDetail::Info),
            ("debug", LogDetail::Debug),
        ] {
            let text = format!("[diagnostics]\nlog_level = \"{name}\"\n");
            assert_eq!(load_from_str(&text).unwrap().diagnostics.log_level, level);
        }
        for bad in ["trace", "off", "loud", "Info", ""] {
            let text = format!("[diagnostics]\nlog_level = \"{bad}\"\n");
            assert!(load_from_str(&text).is_err(), "{bad:?} must be refused");
        }
    }

    #[test]
    fn equalizer_and_loudness_defaults_are_off_and_a_missing_key_keeps_them() {
        let loaded = load_from_str("[audio]\ngapless = false\n").unwrap();
        assert!(loaded.audio.equalizer_gains_db.is_empty());
        assert!(!loaded.audio.loudness_normalization);
        assert_eq!(loaded.audio.loudness_target_lufs, -16);
        assert!(!loaded.audio.dynamic_range_compression);
        assert!(loaded.audio.pitch_correction);
    }

    #[test]
    fn equalizer_gains_must_be_empty_or_ten_values_within_the_limit() {
        let ten = "[3.0, 2.5, 0.0, -1.0, 0.0, 0.5, 1.0, 2.0, 4.0, 12.0]";
        let loaded = load_from_str(&format!("[audio]\nequalizer_gains_db = {ten}\n")).unwrap();
        assert_eq!(loaded.audio.equalizer_gains_db.len(), 10);
        assert!(load_from_str("[audio]\nequalizer_gains_db = []\n").is_ok());
        for bad in [
            "[1.0]",
            "[0,0,0,0,0,0,0,0,0,0,0]",
            "[0,0,0,0,0,0,0,0,0,12.5]",
            "[0,0,0,0,0,0,0,0,0,-13]",
            "[0,0,0,0,0,0,0,0,0,nan]",
        ] {
            let err = load_from_str(&format!("[audio]\nequalizer_gains_db = {bad}\n"));
            assert!(
                matches!(
                    &err,
                    Err(SettingsError::Invalid {
                        field: "audio.equalizer_gains_db",
                        ..
                    })
                ),
                "{bad}: {err:?}"
            );
        }
    }

    #[test]
    fn loudness_target_is_limited_to_minus_30_through_minus_10() {
        for ok in [-30, -16, -10] {
            let text = format!("[audio]\nloudness_target_lufs = {ok}\n");
            assert_eq!(load_from_str(&text).unwrap().audio.loudness_target_lufs, ok);
        }
        for bad in [-31, -9, 0, 100] {
            let err = load_from_str(&format!("[audio]\nloudness_target_lufs = {bad}\n"));
            assert!(
                matches!(
                    &err,
                    Err(SettingsError::Invalid {
                        field: "audio.loudness_target_lufs",
                        ..
                    })
                ),
                "{bad}: {err:?}"
            );
        }
    }

    #[test]
    fn max_volume_accepts_only_the_allowed_choices() {
        for ok in [100, 150, 200, 300] {
            let text = format!("[audio]\nmax_volume_percent = {ok}\n");
            assert_eq!(load_from_str(&text).unwrap().audio.max_volume_percent, ok);
        }
        for bad in [125, 175, 201, 301] {
            let text = format!("[audio]\nmax_volume_percent = {bad}\n");
            assert!(
                matches!(
                    load_from_str(&text).unwrap_err(),
                    SettingsError::Invalid {
                        field: "audio.max_volume_percent",
                        ..
                    }
                ),
                "{bad} accepted"
            );
        }
    }

    #[test]
    fn out_of_range_value_is_rejected_with_field_name() {
        let err = load_from_str("[audio]\nmax_volume_percent = 301\n").unwrap_err();
        assert!(matches!(
            err,
            SettingsError::Invalid {
                field: "audio.max_volume_percent",
                ..
            }
        ));
    }

    #[test]
    fn max_volume_300_is_accepted_on_load_and_save_and_301_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");

        let loaded = load_from_str("[audio]\nmax_volume_percent = 300\n").unwrap();
        assert_eq!(loaded.audio.max_volume_percent, 300);

        let mut s = Settings::default();
        s.audio.max_volume_percent = 300;
        save(&path, &s).unwrap();
        assert_eq!(load(&path).unwrap().audio.max_volume_percent, 300);

        let mut over = Settings::default();
        over.audio.max_volume_percent = 301;
        assert!(matches!(
            save(&path, &over).unwrap_err(),
            SettingsError::Invalid {
                field: "audio.max_volume_percent",
                ..
            }
        ));
        assert_eq!(load(&path).unwrap().audio.max_volume_percent, 300);
    }

    #[test]
    fn buffer_fields_at_their_bounds_round_trip() {
        for (cache, backbuffer, readahead) in [
            (CACHE_MB_MIN, 0, 0),
            (CACHE_MB_MAX, BACKBUFFER_MB_MAX, READAHEAD_SECS_MAX),
        ] {
            let mut s = Settings::default();
            s.playback.cache_mb = cache;
            s.playback.demuxer_backbuffer_mb = backbuffer;
            s.playback.demuxer_readahead_secs = readahead;

            let text = to_toml_string(&s).unwrap();
            let (loaded, clamped) = load_from_str_with_report(&text).unwrap();
            assert_eq!(loaded, s);
            assert!(
                clamped.is_empty(),
                "bounds must not be clamped: {clamped:?}"
            );
        }
    }

    #[test]
    fn out_of_range_buffer_fields_are_clamped_on_load_and_reported() {
        let text = "[playback]\ncache_mb = 5\ndemuxer_backbuffer_mb = 4000\ndemuxer_readahead_secs = 9999\n";
        let (s, clamped) = load_from_str_with_report(text).unwrap();
        assert_eq!(s.playback.cache_mb, CACHE_MB_MIN);
        assert_eq!(s.playback.demuxer_backbuffer_mb, BACKBUFFER_MB_MAX);
        assert_eq!(s.playback.demuxer_readahead_secs, READAHEAD_SECS_MAX);
        assert_eq!(
            clamped,
            vec![
                LoadWarning::ClampedField("playback.cache_mb"),
                LoadWarning::ClampedField("playback.demuxer_backbuffer_mb"),
                LoadWarning::ClampedField("playback.demuxer_readahead_secs"),
            ]
        );
    }

    #[test]
    fn cache_clamp_at_the_legacy_enum_floor_and_above_the_ceiling() {
        // Legacy enum value 10 is below the M2-06 floor of 16 and is clamped up.
        let (low, low_report) = load_from_str_with_report("[playback]\ncache_mb = 10\n").unwrap();
        assert_eq!(low.playback.cache_mb, 16);
        assert_eq!(
            low_report,
            vec![LoadWarning::ClampedField("playback.cache_mb")]
        );

        let (high, high_report) =
            load_from_str_with_report("[playback]\ncache_mb = 4097\n").unwrap();
        assert_eq!(high.playback.cache_mb, 4096);
        assert_eq!(
            high_report,
            vec![LoadWarning::ClampedField("playback.cache_mb")]
        );
    }

    /// Text fields that can hold a free-form string, each with a value that looks like a URL
    /// or credential, and the dotted path that must be reported.
    fn secret_text_fixtures() -> Vec<(&'static str, &'static str)> {
        vec![
            (
                "[subtitles]\nfont = \"https://upstream.invalid/font?token=SENTINEL\"\n",
                "subtitles.font",
            ),
            (
                "[audio]\ndevice = \"https://upstream.invalid/out\"\n",
                "audio.device",
            ),
            (
                "[video.screenshot]\ndirectory = \"Authorization: SENTINEL\"\n",
                "video.screenshot.directory",
            ),
            (
                "[video.screenshot]\ntemplate = \"api_key=SENTINEL-%n\"\n",
                "video.screenshot.template",
            ),
            (
                "[subtitles]\ncolor = \"password=SENTINEL\"\n",
                "subtitles.color",
            ),
            (
                "[subtitles]\nass_style_override = \"Bearer SENTINEL\"\n",
                "subtitles.ass_style_override",
            ),
            (
                "[window.geometry]\nx = 0\ny = 0\nwidth = 800\nheight = 600\nmonitor = \"secret=SENTINEL\"\n",
                "window.geometry.monitor",
            ),
        ]
    }

    #[test]
    fn url_or_credential_in_a_text_field_is_reported_on_load_without_its_value() {
        for (text, path) in secret_text_fixtures() {
            let (loaded, warnings) = load_from_str_with_report(text)
                .unwrap_or_else(|err| panic!("{path} must load: {err}"));
            assert_eq!(
                warnings,
                vec![LoadWarning::SecretLikeValue(path.to_owned())],
                "{path}"
            );
            assert!(
                !format!("{warnings:?}").contains("SENTINEL"),
                "{path}: the report must not echo the value"
            );
            assert_eq!(
                loaded.subtitles.font, "sans-serif",
                "the dropped value must not reach the typed model"
            );
            assert_eq!(loaded.audio.device, "auto");
            assert_eq!(loaded.video.screenshot.directory, "");
            assert_eq!(loaded.subtitles.color, None);
            assert_eq!(
                loaded
                    .window
                    .geometry
                    .as_ref()
                    .and_then(|g| g.monitor.clone()),
                None
            );
        }
    }

    #[test]
    fn url_in_a_text_field_is_not_written_back_on_save() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.toml");
        let target = dir.path().join("target.toml");
        std::fs::write(
            &source,
            "[subtitles]\nfont = \"https://upstream.invalid/font?token=SENTINEL\"\n",
        )
        .unwrap();

        let (loaded, warnings) = load_with_report(&source).unwrap();
        assert_eq!(
            warnings,
            vec![LoadWarning::SecretLikeValue("subtitles.font".to_owned())]
        );
        save(&target, &loaded).unwrap();

        let written = std::fs::read_to_string(&target).unwrap();
        assert!(!written.contains("https://"), "{written}");
        assert!(!written.contains("SENTINEL"), "{written}");
        assert_eq!(load(&target).unwrap().subtitles.font, "sans-serif");
    }

    #[test]
    fn save_refuses_a_url_or_credential_in_a_text_field() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");
        let mut s = Settings::default();
        s.subtitles.font = "https://upstream.invalid/font".to_owned();
        match save(&path, &s).unwrap_err() {
            SettingsError::SecretLikeValue { field } => assert_eq!(field, "subtitles.font"),
            other => panic!("expected SecretLikeValue, got {other:?}"),
        }
        assert!(!path.exists());
    }

    #[test]
    fn ordinary_text_fields_load_without_warnings() {
        let (s, warnings) =
            load_from_str_with_report("[subtitles]\nfont = \"Arial\"\ncolor = \"#FBF93E\"\n")
                .unwrap();
        assert_eq!(s.subtitles.font, "Arial");
        assert!(warnings.is_empty());
    }

    #[test]
    fn in_range_buffer_values_are_not_reported() {
        let (s, clamped) = load_from_str_with_report("[playback]\ncache_mb = 150\n").unwrap();
        assert_eq!(s.playback.cache_mb, 150);
        assert!(clamped.is_empty());
    }

    #[test]
    fn zero_readahead_is_allowed() {
        let s = load_from_str("[playback]\ndemuxer_readahead_secs = 0\n").unwrap();
        assert_eq!(s.playback.demuxer_readahead_secs, 0);
    }

    #[test]
    fn save_rejects_buffer_values_outside_their_range() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");
        let mut s = Settings::default();
        s.playback.cache_mb = CACHE_MB_MAX + 1;
        let err = save(&path, &s).unwrap_err();
        assert!(matches!(
            err,
            SettingsError::Invalid {
                field: "playback.cache_mb",
                ..
            }
        ));
        assert!(!path.exists());

        let mut s = Settings::default();
        s.playback.demuxer_readahead_secs = READAHEAD_SECS_MAX + 1;
        assert!(matches!(
            save(&path, &s).unwrap_err(),
            SettingsError::Invalid {
                field: "playback.demuxer_readahead_secs",
                ..
            }
        ));
    }

    #[test]
    fn unknown_enum_spelling_is_a_parse_error() {
        let err = load_from_str("[video]\ndecode_mode = \"banana\"\n").unwrap_err();
        assert!(matches!(err, SettingsError::Parse(_)));
    }

    /// Settings file with the given legacy `hardware_decoding` line, in schema version 1.
    fn version_one_with_legacy(line: &str) -> String {
        format!("schema_version = 1\n[video]\n{line}\n")
    }

    #[test]
    fn each_legacy_hardware_decoding_value_migrates_to_decode_mode() {
        let cases = [
            ("enabled", DecodeMode::Auto),
            ("osx_compat", DecodeMode::Videotoolbox),
            ("copy", DecodeMode::Auto),
            ("disabled", DecodeMode::Off),
        ];
        for (legacy, expected) in cases {
            let text = version_one_with_legacy(&format!("hardware_decoding = \"{legacy}\""));
            let (loaded, warnings) = load_from_str_with_report(&text).unwrap();
            assert_eq!(loaded.video.decode_mode, expected, "legacy `{legacy}`");
            assert_eq!(loaded.schema_version, CURRENT_SCHEMA_VERSION);
            assert!(warnings.is_empty(), "legacy `{legacy}`: {warnings:?}");
        }
    }

    #[test]
    fn version_one_file_without_legacy_key_loads_with_default_decode_mode() {
        let loaded = load_from_str("schema_version = 1\n[audio]\ngapless = false\n").unwrap();
        assert_eq!(loaded.video.decode_mode, DecodeMode::Auto);
        assert!(!loaded.audio.gapless);
    }

    #[test]
    fn explicit_decode_mode_in_a_version_one_file_is_kept_over_the_legacy_value() {
        // The version 1 build wrote both keys. A non-default decode_mode is a choice made on
        // the new key, so the default legacy value must not overwrite it.
        let text =
            "schema_version = 1\n[video]\nhardware_decoding = \"copy\"\ndecode_mode = \"vaapi\"\n";
        let loaded = load_from_str(text).unwrap();
        assert_eq!(loaded.video.decode_mode, DecodeMode::Vaapi);

        // With decode_mode at its default, the legacy value still decides.
        let text = "schema_version = 1\n[video]\nhardware_decoding = \"disabled\"\ndecode_mode = \"auto\"\n";
        let loaded = load_from_str(text).unwrap();
        assert_eq!(loaded.video.decode_mode, DecodeMode::Off);
    }

    #[test]
    fn unknown_legacy_hardware_decoding_value_is_refused_with_a_clear_error() {
        for bad in ["", "Enabled", "auto", "vaapi", "d3d11va", "none"] {
            let text = version_one_with_legacy(&format!("hardware_decoding = \"{bad}\""));
            match load_from_str(&text).unwrap_err() {
                SettingsError::Invalid { field, reason } => {
                    assert_eq!(field, "video.hardware_decoding", "legacy `{bad}`");
                    assert!(reason.contains("osx_compat"), "{reason}");
                }
                other => panic!("legacy `{bad}`: expected Invalid, got {other:?}"),
            }
        }
    }

    #[test]
    fn non_string_legacy_hardware_decoding_value_is_refused() {
        let err = load_from_str(&version_one_with_legacy("hardware_decoding = 1")).unwrap_err();
        assert!(matches!(
            err,
            SettingsError::Invalid {
                field: "video.hardware_decoding",
                ..
            }
        ));
    }

    #[test]
    fn unknown_legacy_value_is_refused_even_when_decode_mode_is_set() {
        let text = "schema_version = 1\n[video]\nhardware_decoding = \"banana\"\ndecode_mode = \"vaapi\"\n";
        assert!(matches!(
            load_from_str(text).unwrap_err(),
            SettingsError::Invalid {
                field: "video.hardware_decoding",
                ..
            }
        ));
    }

    #[test]
    fn migrated_settings_save_with_decode_mode_and_no_legacy_key() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("old.toml");
        let target = dir.path().join("new.toml");
        std::fs::write(
            &source,
            version_one_with_legacy("hardware_decoding = \"osx_compat\""),
        )
        .unwrap();

        let migrated = load(&source).unwrap();
        save(&target, &migrated).unwrap();
        let written = std::fs::read_to_string(&target).unwrap();
        assert!(
            written.contains(&format!("schema_version = {CURRENT_SCHEMA_VERSION}")),
            "{written}"
        );
        assert!(
            written.contains("decode_mode = \"videotoolbox\""),
            "{written}"
        );
        assert!(!written.contains("hardware_decoding"), "{written}");
        assert_eq!(load(&target).unwrap(), migrated);
    }

    #[test]
    fn decode_and_sync_defaults() {
        let s = Settings::default();
        assert_eq!(s.video.decode_mode, DecodeMode::Auto);
        assert_eq!(s.playback.sync_mode, SyncMode::DisplayResample);
        assert!(s.playback.frame_drop);

        let loaded = load_from_str("").unwrap();
        assert_eq!(loaded.video.decode_mode, DecodeMode::Auto);
        assert_eq!(loaded.playback.sync_mode, SyncMode::DisplayResample);
        assert!(loaded.playback.frame_drop);
    }

    #[test]
    fn every_decode_mode_round_trips_with_its_on_disk_spelling() {
        let cases = [
            (DecodeMode::Auto, "auto"),
            (DecodeMode::Off, "off"),
            (DecodeMode::D3d11va, "d3d11va"),
            (DecodeMode::Nvdec, "nvdec"),
            (DecodeMode::Vaapi, "vaapi"),
            (DecodeMode::Videotoolbox, "videotoolbox"),
            (DecodeMode::Vulkan, "vulkan"),
        ];
        for (mode, spelling) in cases {
            let mut s = Settings::default();
            s.video.decode_mode = mode;
            let text = to_toml_string(&s).unwrap();
            assert!(
                text.contains(&format!("decode_mode = \"{spelling}\"")),
                "{mode:?} written as {text}"
            );
            assert_eq!(load_from_str(&text).unwrap(), s);
        }
    }

    #[test]
    fn every_sync_mode_and_frame_drop_round_trip() {
        let cases = [
            (SyncMode::Audio, "audio"),
            (SyncMode::DisplayResample, "display-resample"),
            (SyncMode::DisplayAdrop, "display-adrop"),
        ];
        for (mode, spelling) in cases {
            for frame_drop in [true, false] {
                let mut s = Settings::default();
                s.playback.sync_mode = mode;
                s.playback.frame_drop = frame_drop;
                let text = to_toml_string(&s).unwrap();
                assert!(
                    text.contains(&format!("sync_mode = \"{spelling}\"")),
                    "{mode:?} written as {text}"
                );
                assert_eq!(load_from_str(&text).unwrap(), s);
            }
        }
    }

    #[test]
    fn unknown_decode_mode_is_refused_on_load() {
        for bad in ["", "Auto", "dxva2", "d3d11", "cuda", "none"] {
            let text = format!("[video]\ndecode_mode = \"{bad}\"\n");
            assert!(
                matches!(load_from_str(&text).unwrap_err(), SettingsError::Parse(_)),
                "{bad:?} accepted"
            );
        }
    }

    #[test]
    fn unknown_sync_mode_is_refused_on_load() {
        for bad in [
            "",
            "video",
            "display-vdrop",
            "display-resample-vdrop",
            "desync",
        ] {
            let text = format!("[playback]\nsync_mode = \"{bad}\"\n");
            assert!(
                matches!(load_from_str(&text).unwrap_err(), SettingsError::Parse(_)),
                "{bad:?} accepted"
            );
        }
    }

    #[test]
    fn frame_drop_must_be_a_boolean() {
        let err = load_from_str("[playback]\nframe_drop = \"yes\"\n").unwrap_err();
        assert!(matches!(err, SettingsError::Parse(_)));
    }

    #[test]
    fn missing_file_loads_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let s = load(&dir.path().join("settings.toml")).unwrap();
        assert_eq!(s, Settings::default());
    }

    #[test]
    fn malformed_file_is_moved_aside_and_defaults_are_loaded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");
        fs::write(&path, "[playback\ninvalid = true\n").unwrap();

        let (settings, warnings) = load_with_report(&path).unwrap();

        assert_eq!(settings, Settings::default());
        assert_eq!(
            warnings,
            vec![LoadWarning::CorruptFile("settings.toml".to_owned())]
        );
        assert!(!path.exists());
        let files: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(files.len(), 1);
        assert!(files[0].starts_with("settings.toml.corrupt-"));
        assert_eq!(
            fs::read_to_string(dir.path().join(&files[0])).unwrap(),
            "[playback\ninvalid = true\n"
        );
    }

    #[test]
    fn non_utf8_settings_file_is_moved_aside_without_loss() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");
        let invalid = [0xff, 0xfe, 0x00];
        fs::write(&path, invalid).unwrap();

        let (settings, warnings) = load_with_report(&path).unwrap();

        assert_eq!(settings, Settings::default());
        assert_eq!(
            warnings,
            vec![LoadWarning::CorruptFile("settings.toml".to_owned())]
        );
        let aside = fs::read_dir(dir.path())
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        assert_eq!(fs::read(aside).unwrap(), invalid);
    }

    #[test]
    fn save_then_load_round_trips_and_leaves_no_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");
        save(&path, &populated()).unwrap();
        assert_eq!(load(&path).unwrap(), populated());

        // Overwrite with different content: rename must replace the old file.
        save(&path, &Settings::default()).unwrap();
        assert_eq!(load(&path).unwrap(), Settings::default());

        let names: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["settings.toml".to_owned()]);
    }

    #[test]
    fn save_rejects_invalid_settings_without_creating_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");
        let mut s = Settings::default();
        s.playback.default_speed = 0.0;
        assert!(save(&path, &s).is_err());
        assert!(!path.exists());
    }

    #[test]
    fn oversized_save_preserves_existing_target() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");
        let original = b"schema_version = 3\n";
        fs::write(&path, original).unwrap();

        let mut settings = Settings::default();
        settings.unknown.insert(
            "future_payload".to_owned(),
            Value::String("x".repeat(MAX_SETTINGS_BYTES)),
        );

        assert!(matches!(
            save(&path, &settings),
            Err(SettingsError::TooLarge {
                max_bytes: MAX_SETTINGS_BYTES
            })
        ));
        assert_eq!(fs::read(&path).unwrap(), original);
        let names: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["settings.toml"]);
    }

    #[test]
    fn concurrent_saves_use_distinct_temporary_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
        let mut workers = Vec::new();
        for value in ["first", "second"] {
            let barrier = barrier.clone();
            let path = path.clone();
            let mut settings = Settings::default();
            settings
                .unknown
                .insert("writer_marker".to_owned(), Value::String(value.to_owned()));
            workers.push(std::thread::spawn(move || {
                barrier.wait();
                save(&path, &settings)
            }));
        }

        barrier.wait();
        for worker in workers {
            worker.join().unwrap().unwrap();
        }

        let loaded = load(&path).unwrap();
        assert!(matches!(
            loaded.unknown.get("writer_marker"),
            Some(Value::String(value)) if value == "first" || value == "second"
        ));
        let names: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["settings.toml"]);
    }
}
