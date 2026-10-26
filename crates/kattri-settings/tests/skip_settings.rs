//! M2-08 settings: the skip keys round-trip through TOML, and the legacy
//! `skip_outro_delay_secs` key migrates to `outro_delay_ms`. Text in and text out, no file I/O.

use kattri_settings::{Settings, SettingsError, load_from_str, to_toml_string};

const LEGACY_KEY: &str = "skip_outro_delay_secs";

#[test]
fn skip_defaults_are_off_and_zero() {
    let playback = Settings::default().playback;
    assert!(!playback.skip_intro);
    assert!(!playback.skip_outro);
    assert!(!playback.auto_skip_intro);
    assert!(!playback.auto_skip_outro);
    assert_eq!(playback.outro_delay_ms, 0);
}

#[test]
fn default_settings_round_trip_through_toml() {
    let settings = Settings::default();
    let text = to_toml_string(&settings).expect("serialize defaults");
    let reloaded = load_from_str(&text).expect("reload defaults");
    assert_eq!(reloaded, settings);
}

#[test]
fn populated_skip_keys_round_trip_through_toml() {
    let mut settings = Settings::default();
    settings.playback.skip_intro = true;
    settings.playback.skip_outro = true;
    settings.playback.auto_skip_intro = true;
    settings.playback.auto_skip_outro = true;
    settings.playback.outro_delay_ms = 45_000;

    let text = to_toml_string(&settings).expect("serialize");
    assert!(
        !text.contains(LEGACY_KEY),
        "legacy key must not be written: {text}"
    );
    assert!(
        !text.contains("_unknown"),
        "skip keys must be known, not kept as unknown: {text}"
    );

    let reloaded = load_from_str(&text).expect("reload");
    assert_eq!(reloaded, settings);
    assert!(reloaded.playback.skip_intro);
    assert!(reloaded.playback.skip_outro);
    assert!(reloaded.playback.auto_skip_intro);
    assert!(reloaded.playback.auto_skip_outro);
    assert_eq!(reloaded.playback.outro_delay_ms, 45_000);
    assert!(reloaded.playback.unknown.is_empty());
}

#[test]
fn skip_keys_load_from_hand_written_toml() {
    let text = "[playback]\n\
                skip_intro = true\n\
                auto_skip_intro = true\n\
                skip_outro = false\n\
                auto_skip_outro = false\n\
                outro_delay_ms = 2500\n";
    let settings = load_from_str(text).expect("load");
    assert!(settings.playback.skip_intro);
    assert!(settings.playback.auto_skip_intro);
    assert!(!settings.playback.skip_outro);
    assert!(!settings.playback.auto_skip_outro);
    assert_eq!(settings.playback.outro_delay_ms, 2_500);
}

/// A version-2 file, as written before M2-08, goes through the version 2 to 3 step.
#[test]
fn version_two_file_migrates_seconds_to_milliseconds_and_drops_the_key() {
    let cases: [(&str, u32); 4] = [("5", 5_000), ("60", 60_000), ("61", 60_000), ("-3", 0)];
    for (seconds, expected) in cases {
        let text = format!("schema_version = 2\n[playback]\n{LEGACY_KEY} = {seconds}\n");
        let settings = load_from_str(&text).expect("version 2 migrates");
        assert_eq!(settings.schema_version, 3, "stamped at the current version");
        assert_eq!(
            settings.playback.outro_delay_ms, expected,
            "{LEGACY_KEY} = {seconds}"
        );
        assert!(!settings.playback.unknown.contains_key(LEGACY_KEY));
        let saved = to_toml_string(&settings).expect("serialize");
        assert!(!saved.contains(LEGACY_KEY), "{saved}");
    }
}

#[test]
fn legacy_seconds_key_migrates_to_milliseconds() {
    let text = "[playback]\nskip_outro_delay_secs = 5\n";
    let settings = load_from_str(text).expect("migrate");
    assert_eq!(settings.playback.outro_delay_ms, 5_000);
    assert!(
        !settings.playback.unknown.contains_key(LEGACY_KEY),
        "migrated legacy key must not stay in _unknown"
    );

    // The migrated file saves without the legacy key and reloads to the same value.
    let saved = to_toml_string(&settings).expect("serialize migrated");
    assert!(!saved.contains(LEGACY_KEY), "{saved}");
    assert_eq!(load_from_str(&saved).expect("reload"), settings);
}

#[test]
fn legacy_seconds_are_multiplied_and_clamped() {
    let cases: [(&str, u32); 10] = [
        ("0", 0),
        ("3", 3_000),
        ("5", 5_000),
        ("10", 10_000),
        ("15", 15_000),
        ("30", 30_000),
        ("60", 60_000),
        ("61", 60_000),
        ("-3", 0),
        ("9223372036854775807", 60_000),
    ];
    for (seconds, expected) in cases {
        let text = format!("[playback]\n{LEGACY_KEY} = {seconds}\n");
        let settings = load_from_str(&text).expect("migrate");
        assert_eq!(
            settings.playback.outro_delay_ms, expected,
            "{LEGACY_KEY} = {seconds}"
        );
    }
}

#[test]
fn legacy_key_migrates_without_a_schema_version_and_with_version_two() {
    let unversioned = load_from_str("[playback]\nskip_outro_delay_secs = 7\n").expect("v1");
    assert_eq!(unversioned.playback.outro_delay_ms, 7_000);

    let versioned =
        load_from_str("schema_version = 2\n[playback]\nskip_outro_delay_secs = 7\n").expect("v2");
    assert_eq!(versioned.playback.outro_delay_ms, 7_000);
}

#[test]
fn explicit_outro_delay_ms_wins_over_legacy_key() {
    let text = "[playback]\nskip_outro_delay_secs = 5\noutro_delay_ms = 1500\n";
    let settings = load_from_str(text).expect("load");
    assert_eq!(settings.playback.outro_delay_ms, 1_500);
    assert!(!settings.playback.unknown.contains_key(LEGACY_KEY));
}

#[test]
fn non_integer_legacy_seconds_are_refused() {
    let err = load_from_str("[playback]\nskip_outro_delay_secs = \"5\"\n").unwrap_err();
    assert!(
        matches!(
            err,
            SettingsError::Invalid {
                field: "playback.skip_outro_delay_secs",
                ..
            }
        ),
        "got {err:?}"
    );
}

#[test]
fn outro_delay_ms_is_validated_on_load() {
    let max = load_from_str("[playback]\noutro_delay_ms = 60000\n").expect("max is allowed");
    assert_eq!(max.playback.outro_delay_ms, 60_000);

    let err = load_from_str("[playback]\noutro_delay_ms = 60001\n").unwrap_err();
    assert!(
        matches!(
            err,
            SettingsError::Invalid {
                field: "playback.outro_delay_ms",
                ..
            }
        ),
        "got {err:?}"
    );
}
