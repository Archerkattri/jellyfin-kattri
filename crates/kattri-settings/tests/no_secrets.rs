//! M2-02 guard: the settings file must never hold credentials or upstream URLs.
//!
//! The denylist lives in `kattri_settings::guard`. Every check here goes through that one
//! module, so a future key is covered by the same rule.
//!
//! Deviation from the M2-02 spec text: the spec asks to put `SENTINEL-*` strings into every
//! field. The schema has no credential fields, and free-text fields such as `font` would then
//! legitimately hold a sentinel and fail the check. The test therefore populates every field
//! with a realistic non-secret value and checks the serialized file structurally (every key
//! and every string value). The negative fixtures prove the checks catch sentinels, URLs and
//! credential keys.

use std::path::PathBuf;

use kattri_settings::guard::{
    SECRET_KEY_MARKERS, SECRET_VALUE_MARKERS, find_secret_like_key, find_secret_like_value,
};
use kattri_settings::{
    AspectMode, AudioOutput, ChannelLayout, DecodeMode, HdrPreference, ImageFormat, InputSettings,
    Passthrough, PlaybackSettings, SeriesLanguageSetting, Settings, SettingsError, SyncMode,
    TranscodePreferences, WindowGeometry, load, load_from_str, save,
};

/// Every field set to a non-default, non-secret value.
#[allow(clippy::field_reassign_with_default)] // field-by-field build is the point of this fixture
fn populated() -> Settings {
    let mut s = Settings::default();
    s.general.high_contrast = true;
    s.general.reduce_motion = false;
    s.playback = PlaybackSettings {
        sync_mode: SyncMode::DisplayAdrop,
        frame_drop: false,
        default_speed: 1.5,
        cache_mb: 500,
        demuxer_backbuffer_mb: 100,
        demuxer_readahead_secs: 20,
        skip_intro: true,
        skip_outro: true,
        auto_skip_intro: true,
        auto_skip_outro: true,
        outro_delay_ms: 30_000,
        external_player: true,
        max_streaming_bitrate: 20_000_000,
        unknown: toml::Table::new(),
    };
    s.audio.output = AudioOutput::Hdmi;
    s.audio.channels = ChannelLayout::Surround71;
    s.audio.device = "wasapi-default".to_owned();
    s.audio.normalize = false;
    s.audio.gapless = false;
    s.audio.max_volume_percent = 200;
    s.audio.exclusive = true;
    s.audio.passthrough = Passthrough {
        ac3: true,
        eac3: true,
        dts: true,
        dts_hd: true,
        truehd: true,
    };
    s.video.decode_mode = DecodeMode::Vulkan;
    s.video.deinterlace = true;
    s.video.refresh_rate_auto_switch = true;
    s.video.refresh_rate_avoid_25_30_hz = false;
    s.video.hdr_preference = HdrPreference::Dovi;
    s.video.hdr_passthrough = true;
    s.video.aspect = AspectMode::Force16x9;
    s.video.allow_zoom = false;
    s.video.transcode = TranscodePreferences {
        prefer_h265: true,
        allow_hevc: true,
        force_hevc: true,
        force_4k: true,
        force_av1: true,
        force_hdr: true,
        force_hi10p: true,
        force_dovi: false,
        always_force: true,
    };
    s.video.screenshot.directory = "D:/Shots".to_owned();
    s.video.screenshot.template = "shot-%n".to_owned();
    s.video.screenshot.format = ImageFormat::Webp;
    s.subtitles.font = "Arial".to_owned();
    s.subtitles.size = 40;
    s.subtitles.color = Some("#FBF93E".to_owned());
    s.subtitles.border_color = Some("#000000".to_owned());
    s.subtitles.border_size = 3;
    s.subtitles.background_color = Some("#101010".to_owned());
    s.subtitles.background_transparency = Some("50".to_owned());
    s.subtitles.placement = Some("bottom".to_owned());
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
    s.input = InputSettings {
        sdl_gamepad_enabled: false,
        web_gamepad_enabled: false,
        key_repeat: false,
        keymap_path: Some(PathBuf::from("C:/Users/me/keys.json")),
        unknown: toml::Table::new(),
    };
    s.plugins.enabled = true;
    s.updates.check_for_updates = false;
    s.series_languages = vec![SeriesLanguageSetting {
        series_id: "0123456789abcdef0123456789abcdef".to_owned(),
        audio_language: Some("jpn".to_owned()),
        audio_title: Some("Original".to_owned()),
        subtitle_disabled: true,
        ..SeriesLanguageSetting::default()
    }];
    s
}

/// All problems found in a serialized settings file. Empty means clean.
fn violations(text: &str) -> Vec<String> {
    let mut found = Vec::new();

    let lower = text.to_ascii_lowercase();
    for marker in SECRET_VALUE_MARKERS {
        if lower.contains(marker) {
            found.push(format!("raw text contains `{marker}`"));
        }
    }

    match text.parse::<toml::Table>() {
        Ok(table) => {
            if let Some(path) = find_secret_like_key(&table) {
                found.push(format!("credential-like key `{path}`"));
            }
            if let Some(path) = find_secret_like_value(&table) {
                found.push(format!("URL or credential value at `{path}`"));
            }
        }
        Err(err) => found.push(format!("file is not valid TOML: {err}")),
    }
    found
}

fn assert_clean(label: &str, text: &str) {
    let found = violations(text);
    assert!(
        found.is_empty(),
        "{label} holds secret-like content: {found:?}\n{text}"
    );
}

fn save_and_read(settings: &Settings) -> String {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("settings.toml");
    save(&path, settings).expect("save settings");
    std::fs::read_to_string(&path).expect("read saved file")
}

#[test]
fn default_settings_file_holds_no_credentials() {
    assert_clean("default settings", &save_and_read(&Settings::default()));
}

#[test]
fn fully_populated_settings_file_holds_no_credentials() {
    let text = save_and_read(&populated());
    assert_clean("populated settings", &text);
    // The check must not pass only because the file is empty or unparsable.
    assert_eq!(load_from_str(&text).expect("reload populated"), populated());
}

#[test]
fn negative_fixture_server_address_with_token_url_is_caught() {
    let text =
        "serverAddress = \"http://upstream.invalid/Videos/1/stream?api_key=SENTINEL-TOKEN\"\n";
    assert!(
        !violations(text).is_empty(),
        "URL with api_key must be caught"
    );
}

#[test]
fn negative_fixture_sentinel_url_in_free_text_is_caught() {
    let text = "[subtitles]\nfont = \"https://upstream.invalid/font?token=SENTINEL-TOKEN\"\n";
    assert!(!violations(text).is_empty());
}

#[test]
fn negative_fixture_credential_keys_are_caught() {
    for key in [
        "password",
        "accessToken",
        "APIKEY",
        "api_key",
        "username",
        "quickConnectSecret",
    ] {
        let table: toml::Table = format!("[playback]\n{key} = \"x\"\n")
            .parse()
            .expect("fixture is TOML");
        assert!(
            find_secret_like_key(&table).is_some(),
            "key `{key}` must be flagged"
        );
    }
}

#[test]
fn every_denylist_marker_flags_a_key() {
    for marker in SECRET_KEY_MARKERS {
        let key = format!("x{marker}");
        let table: toml::Table = format!("{key} = 1\n").parse().expect("fixture is TOML");
        assert!(
            find_secret_like_key(&table).is_some(),
            "marker `{marker}` must flag `{key}`"
        );
    }
}

#[test]
fn load_rejects_a_file_with_a_credential_key() {
    let err = load_from_str("[playback]\napi_key = \"SENTINEL-TOKEN\"\n").unwrap_err();
    match &err {
        SettingsError::SecretLikeKey { key } => assert_eq!(key, "playback.api_key"),
        other => panic!("expected SecretLikeKey, got {other:?}"),
    }
    // The error names the key only, never the value.
    assert!(!err.to_string().contains("SENTINEL"));
}

#[test]
fn load_rejects_root_level_and_nested_credential_keys() {
    assert!(matches!(
        load_from_str("accessToken = \"x\"\n"),
        Err(SettingsError::SecretLikeKey { .. })
    ));
    assert!(matches!(
        load_from_str("[audio.passthrough]\npassword = \"x\"\n"),
        Err(SettingsError::SecretLikeKey { .. })
    ));
}

#[test]
fn load_of_credential_file_is_refused_and_file_is_left_untouched() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("settings.toml");
    let original = "[input]\nauth = \"SENTINEL-PASSWORD\"\n";
    std::fs::write(&path, original).expect("write fixture");

    assert!(matches!(
        load(&path),
        Err(SettingsError::SecretLikeKey { .. })
    ));
    assert_eq!(std::fs::read_to_string(&path).expect("read back"), original);
}

#[test]
fn ordinary_unknown_keys_are_preserved_without_becoming_typed_settings() {
    let settings = load_from_str("future_flag = true\n[playback]\nfuture_number = 3\n")
        .expect("non-secret unknown keys load");
    assert_eq!(settings.general, Settings::default().general);
    assert_eq!(
        settings.playback.skip_intro,
        Settings::default().playback.skip_intro
    );
    assert_eq!(
        settings.unknown.get("future_flag"),
        Some(&toml::Value::Boolean(true))
    );
    assert_eq!(
        settings.playback.unknown.get("future_number"),
        Some(&toml::Value::Integer(3))
    );
    let saved = kattri_settings::to_toml_string(&settings).expect("unknown fields serialize");
    let reloaded = load_from_str(&saved).expect("unknown fields survive round trip");
    assert_eq!(reloaded.unknown, settings.unknown);
    assert_eq!(reloaded.playback.unknown, settings.playback.unknown);
}
