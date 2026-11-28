//! Release DoD 2.9 upgrade path, exercised through the real store API.
//!
//! Release 0.7.0 is tag `v0.7.0`. Its `crates/kattri-settings/src/schema.rs` already declared
//! `CURRENT_SCHEMA_VERSION = 3`, and `git diff v0.7.0 HEAD -- crates/kattri-settings/` shows only
//! documentation comments changed, so the frozen [`SETTINGS_0_7_0`] document below is byte-for-byte
//! what the 0.7.0 release writes for the same settings. It was captured from
//! `to_toml_string(&populated_0_7_0())` and is embedded as a literal so a future schema change
//! cannot silently rewrite the historical layout. The older schema versions 2 and 1 are derived
//! from it by substituting only the keys their releases wrote, including the real legacy keys that
//! the shipped [`kattri_settings::migrate::MIGRATIONS`] steps transform.
//!
//! Every row keeps one non-default value for every field the schema holds, so a migration that
//! drops or rewrites any user value fails the full [`Settings`] equality, not just a spot check.
//! Unknown top-level, section-level and per-title-style keys must survive the load and a save.

use std::path::PathBuf;

use kattri_settings::{
    AspectMode, AudioOutput, CURRENT_SCHEMA_VERSION, ChannelLayout, DecodeMode, HdrPreference,
    ImageFormat, InterfaceSize, LibraryArtworkSize, LibraryLayout, LoadWarning, LogDetail,
    SeriesLanguageSetting, Settings, SettingsError, SyncMode, ThemePreset,
    TitleSubtitleStyleSetting, WindowGeometry, load, load_with_report, save, to_toml_string,
};

/// Shaped like a `kattri-parental` record. The digest is fake and is not a real PIN hash.
const FAKE_RECORD: &str = "argon2id$v=19$m=19456,t=2,p=1$00112233445566778899aabbccddeeff$0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// The settings file written by release 0.7.0 (schema version 3), with unknown forward-compatible
/// keys present at the top level, in `[general]`, in `[playback]`, and in one title style.
const SETTINGS_0_7_0: &str = r##"schema_version = 3

[_unknown]
future_top_key = 1

[general]
reduce_motion = false
theme_preset = "warm-dark"
custom_theme_enabled = true
custom_accent = [
    0,
    164,
    220,
]
custom_background = [
    0,
    0,
    0,
]
custom_surface = [
    32,
    32,
    32,
]
library_layout = "list"
library_artwork_size = "large"
backdrop = "custom"
custom_backdrop_path = "art/backdrop.jpg"
custom_ca_path = "certs/ca.pem"
client_cert_path = "certs/client.pem"
client_key_path = "certs/client-key.pem"
interface_size = "ten-foot"
home_continue_watching = false
home_next_up = false
home_combine_continue_next_up = true
home_latest = false
home_my_media = false
home_collections = false
home_section_order = [
    "latest",
    "future_row",
    "continue_watching",
]
screensaver_idle_seconds = 240

[general._unknown]
future_general_key = "keep"

[playback]
sync_mode = "audio"
frame_drop = false
default_speed = 1.25
cache_mb = 256
demuxer_backbuffer_mb = 100
demuxer_readahead_secs = 12
skip_intro = true
skip_outro = true
auto_skip_intro = true
auto_skip_outro = true
outro_delay_ms = 15000
resume_rewind_seconds = 8
auto_play_next_episode = false
external_player = true
max_streaming_bitrate = 20000000
theme_media = false

[playback._unknown]
future_playback_key = 7

[audio]
output = "hdmi"
channels = "7.1"
device = "wasapi-default"
normalize = false
gapless = false
max_volume_percent = 200
exclusive = true
equalizer_gains_db = [
    3.0,
    2.5,
    0.0,
    -1.0,
    0.0,
    0.5,
    1.0,
    2.0,
    4.0,
    6.0,
]
loudness_normalization = true
loudness_target_lufs = -23
dynamic_range_compression = true
pitch_correction = false

[audio.passthrough]
ac3 = true
eac3 = true
dts = true
dts_hd = true
truehd = true

[video]
decode_mode = "vaapi"
deinterlace = true
refresh_rate_auto_switch = true
refresh_rate_avoid_25_30_hz = false
hdr_preference = "hdr10"
hdr_passthrough = true
tone_mapping = "bt-2390"
aspect = "force_16_9"
allow_zoom = false

[video.transcode]
prefer_h265 = true
allow_hevc = true
force_hevc = true
force_4k = true
force_av1 = true
force_hdr = true
force_hi10p = true
force_dovi = false
always_force = true

[video.screenshot]
directory = "art/screens"
template = "shot-%n"
format = "webp"

[subtitles]
font = "Noto Sans"
bold = true
size = 36
color = "#FBF93E"
border_color = "#000000"
border_size = 2
background_color = "#202020"
background_transparency = "25"
placement = "top"
ass_style_override = "force"
ass_scale_border_and_shadow = false
secondary_position_percent = 25
resync_on_start = false
burn_in = true

[[subtitles.title_styles]]
item_id = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
font = "Noto Sans"
bold = true
size = 32
color = "#FFFFFF"
border_color = "#000000"
border_size = 3
background_color = "#202020"
placement = "bottom"

[subtitles.title_styles._unknown]
future_style_key = "style-keep"

[window]
fullscreen = true
always_on_top = true
minimize_on_defocus = true

[window.geometry]
x = 40
y = -1080
width = 1280
height = 720
monitor = "DISPLAY2"

[input]
sdl_gamepad_enabled = false
web_gamepad_enabled = false
global_shortcuts = true
key_repeat = false
keymap_path = "keys.json"

[plugins]
enabled = true
discord_presence = true

[updates]
check_for_updates = false

[diagnostics]
crash_reporting = true
log_level = "debug"

[parental]
pin_record = "argon2id$v=19$m=19456,t=2,p=1$00112233445566778899aabbccddeeff$0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
max_rating = 13

[admin]
dashboard = true

[[series_languages]]
series_id = "0123456789abcdef0123456789abcdef"
audio_language = "jpn"
audio_title = "Original"
subtitle_language = "eng"
subtitle_title = "English"
subtitle_disabled = false

[[series_languages]]
series_id = "1123456789abcdef0123456789abcdef"
subtitle_disabled = true

[[home_hidden_series]]
server_id = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"
profile = "default"
series_id = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb"
"##;

/// Every field set to a non-default, non-secret value: the model the 0.7.0 file encodes.
#[allow(clippy::field_reassign_with_default)] // field-by-field build is the point of the fixture
fn populated_0_7_0() -> Settings {
    let mut s = Settings::default();
    s.general.reduce_motion = false;
    s.general.theme_preset = ThemePreset::WarmDark;
    s.general.custom_theme_enabled = true;
    s.general.custom_accent = Some([0, 164, 220]);
    s.general.custom_background = Some([0, 0, 0]);
    s.general.custom_surface = Some([32, 32, 32]);
    s.general.library_layout = LibraryLayout::List;
    s.general.library_artwork_size = LibraryArtworkSize::Large;
    s.general.backdrop = kattri_settings::BackdropPreference::Custom;
    s.general.custom_backdrop_path = Some(PathBuf::from("art/backdrop.jpg"));
    s.general.custom_ca_path = Some(PathBuf::from("certs/ca.pem"));
    s.general.client_cert_path = Some(PathBuf::from("certs/client.pem"));
    s.general.client_key_path = Some(PathBuf::from("certs/client-key.pem"));
    s.general.interface_size = InterfaceSize::TenFoot;
    s.general.home_continue_watching = false;
    s.general.home_next_up = false;
    s.general.home_combine_continue_next_up = true;
    s.general.home_latest = false;
    s.general.home_my_media = false;
    s.general.home_collections = false;
    s.general.home_section_order = vec![
        "latest".to_owned(),
        "future_row".to_owned(),
        "continue_watching".to_owned(),
    ];
    s.general.screensaver_idle_seconds = 240;
    s.general.unknown.insert(
        "future_general_key".to_owned(),
        toml::Value::String("keep".to_owned()),
    );

    s.playback.sync_mode = SyncMode::Audio;
    s.playback.frame_drop = false;
    s.playback.default_speed = 1.25;
    s.playback.cache_mb = 256;
    s.playback.demuxer_backbuffer_mb = 100;
    s.playback.demuxer_readahead_secs = 12;
    s.playback.skip_intro = true;
    s.playback.skip_outro = true;
    s.playback.auto_skip_intro = true;
    s.playback.auto_skip_outro = true;
    s.playback.outro_delay_ms = 15_000;
    s.playback.resume_rewind_seconds = 8;
    s.playback.auto_play_next_episode = false;
    s.playback.external_player = true;
    s.playback.max_streaming_bitrate = 20_000_000;
    s.playback.theme_media = false;
    s.playback
        .unknown
        .insert("future_playback_key".to_owned(), toml::Value::Integer(7));

    s.audio.output = AudioOutput::Hdmi;
    s.audio.channels = ChannelLayout::Surround71;
    s.audio.device = "wasapi-default".to_owned();
    s.audio.normalize = false;
    s.audio.gapless = false;
    s.audio.max_volume_percent = 200;
    s.audio.exclusive = true;
    s.audio.passthrough.ac3 = true;
    s.audio.passthrough.eac3 = true;
    s.audio.passthrough.dts = true;
    s.audio.passthrough.dts_hd = true;
    s.audio.passthrough.truehd = true;
    s.audio.equalizer_gains_db = vec![3.0, 2.5, 0.0, -1.0, 0.0, 0.5, 1.0, 2.0, 4.0, 6.0];
    s.audio.loudness_normalization = true;
    s.audio.loudness_target_lufs = -23;
    s.audio.dynamic_range_compression = true;
    s.audio.pitch_correction = false;

    s.video.decode_mode = DecodeMode::Vaapi;
    s.video.deinterlace = true;
    s.video.refresh_rate_auto_switch = true;
    s.video.refresh_rate_avoid_25_30_hz = false;
    s.video.hdr_preference = HdrPreference::Hdr10;
    s.video.hdr_passthrough = true;
    s.video.tone_mapping = kattri_settings::ToneMapping::Bt2390;
    s.video.aspect = AspectMode::Force16x9;
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
    s.video.screenshot.directory = "art/screens".to_owned();
    s.video.screenshot.template = "shot-%n".to_owned();
    s.video.screenshot.format = ImageFormat::Webp;

    s.subtitles.font = "Noto Sans".to_owned();
    s.subtitles.bold = true;
    s.subtitles.size = 36;
    s.subtitles.color = Some("#FBF93E".to_owned());
    s.subtitles.border_color = Some("#000000".to_owned());
    s.subtitles.border_size = 2;
    s.subtitles.background_color = Some("#202020".to_owned());
    s.subtitles.background_transparency = Some("25".to_owned());
    s.subtitles.placement = Some("top".to_owned());
    s.subtitles.ass_style_override = Some("force".to_owned());
    s.subtitles.ass_scale_border_and_shadow = false;
    s.subtitles.secondary_position_percent = 25;
    s.subtitles.resync_on_start = false;
    s.subtitles.burn_in = true;
    s.subtitles.title_styles = vec![TitleSubtitleStyleSetting {
        item_id: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned(),
        font: Some("Noto Sans".to_owned()),
        bold: Some(true),
        size: Some(32),
        color: Some("#FFFFFF".to_owned()),
        border_color: Some("#000000".to_owned()),
        border_size: Some(3),
        background_color: Some("#202020".to_owned()),
        placement: Some("bottom".to_owned()),
        unknown: {
            let mut table = toml::Table::new();
            table.insert(
                "future_style_key".to_owned(),
                toml::Value::String("style-keep".to_owned()),
            );
            table
        },
    }];

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
    s.input.global_shortcuts = true;
    s.input.key_repeat = false;
    s.input.keymap_path = Some(PathBuf::from("keys.json"));

    s.plugins.enabled = true;
    s.plugins.discord_presence = true;
    s.updates.check_for_updates = false;
    s.diagnostics.crash_reporting = true;
    s.diagnostics.log_level = LogDetail::Debug;
    s.parental.pin_record = FAKE_RECORD.to_owned();
    s.parental.max_rating = Some(13);

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
    s.home_hidden_series = vec![kattri_settings::schema::HomeSeriesSuppressionSetting {
        server_id: "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa".to_owned(),
        profile: "default".to_owned(),
        series_id: "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb".to_owned(),
    }];

    s.unknown
        .insert("future_top_key".to_owned(), toml::Value::Integer(1));
    s
}

/// Replace one occurrence, failing loudly if the frozen fixture no longer holds it.
fn replace_once(text: &str, from: &str, to: &str) -> String {
    assert_eq!(
        text.matches(from).count(),
        1,
        "the 0.7.0 fixture must contain exactly one `{from}`"
    );
    text.replace(from, to)
}

/// Table rows: label, on-disk document, and the model the current build must expose.
fn upgrade_cases() -> Vec<(&'static str, String, Settings)> {
    let current_expected = populated_0_7_0();

    // Schema version 2 wrote the outro skip delay in seconds; version 3 migrated it to
    // milliseconds. All other settings in the frozen 0.7.0 document were already written by 0.7.0.
    let version_two = replace_once(SETTINGS_0_7_0, "schema_version = 3", "schema_version = 2");
    let version_two = replace_once(
        &version_two,
        "outro_delay_ms = 15000",
        "skip_outro_delay_secs = 12",
    );
    let mut version_two_expected = current_expected.clone();
    version_two_expected.playback.outro_delay_ms = 12_000;

    // Schema version 1 wrote the hardware decoding choice under the legacy key, which the
    // version 1 to 2 step maps to `decode_mode`.
    let version_one = replace_once(&version_two, "schema_version = 2", "schema_version = 1");
    let version_one = replace_once(
        &version_one,
        "decode_mode = \"vaapi\"",
        "hardware_decoding = \"osx_compat\"",
    );
    let mut version_one_expected = version_two_expected.clone();
    version_one_expected.video.decode_mode = DecodeMode::Videotoolbox;

    vec![
        (
            "release 0.7.0 (schema version 3)",
            SETTINGS_0_7_0.to_owned(),
            current_expected,
        ),
        (
            "schema version 2 with the legacy seconds key",
            version_two,
            version_two_expected,
        ),
        (
            "schema version 1 with the legacy hardware decoding key",
            version_one,
            version_one_expected,
        ),
    ]
}

fn assert_unknown_keys_preserved(label: &str, settings: &Settings) {
    assert_eq!(
        settings.unknown.get("future_top_key"),
        Some(&toml::Value::Integer(1)),
        "{label}: top-level unknown key"
    );
    assert_eq!(
        settings.general.unknown.get("future_general_key"),
        Some(&toml::Value::String("keep".to_owned())),
        "{label}: [general] unknown key"
    );
    assert_eq!(
        settings.playback.unknown.get("future_playback_key"),
        Some(&toml::Value::Integer(7)),
        "{label}: [playback] unknown key"
    );
    assert_eq!(
        settings.subtitles.title_styles[0]
            .unknown
            .get("future_style_key"),
        Some(&toml::Value::String("style-keep".to_owned())),
        "{label}: title style unknown key"
    );
}

#[test]
fn previous_release_settings_upgrade_with_every_user_value_preserved() {
    for (label, document, expected) in upgrade_cases() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("settings.toml");
        std::fs::write(&path, &document).expect("write fixture");

        let (loaded, warnings) = load_with_report(&path)
            .unwrap_or_else(|error| panic!("{label}: the old file must load: {error}"));

        assert!(warnings.is_empty(), "{label}: {warnings:?}");
        assert_eq!(
            loaded.schema_version, CURRENT_SCHEMA_VERSION,
            "{label}: the loaded snapshot carries the current schema version"
        );
        assert_eq!(loaded, expected, "{label}: every user value must survive");
        assert_eq!(
            std::fs::read_to_string(&path).expect("read fixture back"),
            document,
            "{label}: loading must not rewrite the old file"
        );
        assert_unknown_keys_preserved(label, &loaded);

        // Saving through the real store writes the current layout, keeps the values and the
        // unknown keys, and drops the legacy keys the migrations consumed.
        let upgraded_path = dir.path().join("upgraded.toml");
        save(&upgraded_path, &loaded)
            .unwrap_or_else(|error| panic!("{label}: upgraded settings must save: {error}"));
        let saved = std::fs::read_to_string(&upgraded_path).expect("read upgraded file");
        assert!(
            saved.contains(&format!("schema_version = {CURRENT_SCHEMA_VERSION}")),
            "{label}: saved document must carry the current version:\n{saved}"
        );
        assert!(!saved.contains("hardware_decoding"), "{label}: {saved}");
        assert!(!saved.contains("skip_outro_delay_secs"), "{label}: {saved}");
        assert!(saved.contains("future_top_key = 1"), "{label}: {saved}");
        assert!(
            saved.contains("future_general_key = \"keep\""),
            "{label}: {saved}"
        );
        assert!(
            saved.contains("future_playback_key = 7"),
            "{label}: {saved}"
        );
        assert!(
            saved.contains("future_style_key = \"style-keep\""),
            "{label}: {saved}"
        );
        assert_eq!(
            load(&upgraded_path)
                .unwrap_or_else(|error| panic!("{label}: the upgraded file must reload: {error}")),
            loaded,
            "{label}: save and reload must be lossless"
        );
    }
}

#[test]
fn release_0_7_0_file_upgrades_in_place_through_the_store() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("settings.toml");
    std::fs::write(&path, SETTINGS_0_7_0).expect("write 0.7.0 file");

    let loaded = load(&path).expect("0.7.0 settings load");
    assert_eq!(loaded, populated_0_7_0());
    assert_eq!(loaded.schema_version, 3);

    save(&path, &loaded).expect("upgrade in place");
    let rewritten = std::fs::read_to_string(&path).expect("read upgraded file");
    assert!(rewritten.contains("schema_version = 3"), "{rewritten}");
    assert!(!rewritten.contains("hardware_decoding"), "{rewritten}");
    assert!(!rewritten.contains("skip_outro_delay_secs"), "{rewritten}");
    assert_eq!(
        load(&path).expect("reload upgraded in place"),
        populated_0_7_0()
    );
}

#[test]
fn a_newer_schema_file_is_loaded_read_only_and_kept_byte_for_byte() {
    let supported = CURRENT_SCHEMA_VERSION;
    let fixtures: [(&str, Vec<u8>); 2] = [
        (
            "one version ahead",
            format!(
                "schema_version = {}\n# written by a newer build\n[future_section]\nfuture_value = \"keep me\"\n",
                supported + 1
            )
            .into_bytes(),
        ),
        (
            "far ahead with CRLF line endings",
            format!(
                "schema_version = {}\r\n[future_section]\r\nfuture_value = \"keep me\"\r\n[general]\r\n",
                supported + 100
            )
            .into_bytes(),
        ),
    ];

    for (label, original) in fixtures {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("settings.toml");
        std::fs::write(&path, &original).expect("write future-version file");

        let version = std::str::from_utf8(&original)
            .expect("fixture is UTF-8")
            .lines()
            .next()
            .and_then(|line| line.strip_prefix("schema_version = "))
            .and_then(|value| value.parse::<u32>().ok())
            .expect("fixture starts with its schema version");

        let (settings, warnings) =
            load_with_report(&path).unwrap_or_else(|error| panic!("{label}: {error}"));
        assert_eq!(
            warnings,
            vec![LoadWarning::FutureSchema(version)],
            "{label}"
        );
        assert_eq!(
            settings,
            Settings {
                read_only_schema_version: Some(version),
                ..Settings::default()
            },
            "{label}: defaults are exposed read-only with no user values guessed"
        );
        assert_eq!(
            std::fs::read(&path).expect("read future file back"),
            original,
            "{label}: loading a future file must not modify a single byte"
        );

        // Both save paths refuse to downgrade the file.
        assert!(
            matches!(
                to_toml_string(&settings),
                Err(SettingsError::FutureVersion {
                    found,
                    supported: CURRENT_SCHEMA_VERSION
                }) if found == version
            ),
            "{label}: serializing a read-only snapshot must be refused"
        );
        assert!(
            matches!(
                save(&path, &settings),
                Err(SettingsError::FutureVersion {
                    found,
                    supported: CURRENT_SCHEMA_VERSION
                }) if found == version
            ),
            "{label}: saving a read-only snapshot must be refused"
        );
        assert_eq!(
            std::fs::read(&path).expect("read future file back after refused save"),
            original,
            "{label}: a refused save must not modify a single byte"
        );

        // No corrupt-aside copy and no temporary file may be left behind.
        let mut names: Vec<String> = std::fs::read_dir(dir.path())
            .expect("list temp dir")
            .map(|entry| {
                entry
                    .expect("read dir entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        names.sort();
        assert_eq!(names, ["settings.toml"], "{label}: unexpected side files");
    }
}
