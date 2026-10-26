//! M4-20 and M4-21 settings: defaults, TOML round trip, and the PIN record rules.
//! Text in and text out, no file I/O.

use kattri_settings::guard::{find_secret_like_key, find_secret_like_value};
use kattri_settings::schema::{DiagnosticsSettings, MAX_PIN_RECORD_LEN, ParentalSettings};
use kattri_settings::{Settings, SettingsError, load_from_str, to_toml_string};

/// Shaped like a `kattri-parental` record. The digest is fake and is not a real PIN hash.
const FAKE_RECORD: &str = "argon2id$v=19$m=19456,t=2,p=1$00112233445566778899aabbccddeeff$0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

#[test]
fn crash_reporting_is_off_by_default() {
    assert!(!Settings::default().diagnostics.crash_reporting);
    assert!(!DiagnosticsSettings::default().crash_reporting);
    let loaded = load_from_str("schema_version = 3\n").expect("minimal file loads");
    assert!(!loaded.diagnostics.crash_reporting);
}

#[test]
fn crash_reporting_round_trips_through_toml() {
    let mut settings = Settings::default();
    settings.diagnostics.crash_reporting = true;
    let text = to_toml_string(&settings).expect("serialize");
    assert!(text.contains("[diagnostics]"), "{text}");
    let reloaded = load_from_str(&text).expect("reload");
    assert!(reloaded.diagnostics.crash_reporting, "{text}");
}

#[test]
fn unknown_keys_inside_diagnostics_are_kept() {
    let loaded = load_from_str(
        "schema_version = 3\n[diagnostics]\ncrash_reporting = true\nfuture_key = 1\n",
    )
    .expect("loads");
    assert!(loaded.diagnostics.crash_reporting);
    assert_eq!(
        loaded.diagnostics.unknown.get("future_key"),
        Some(&toml::Value::Integer(1))
    );
}

#[test]
fn pin_record_and_rating_ceiling_round_trip() {
    let settings = Settings {
        parental: ParentalSettings {
            pin_record: FAKE_RECORD.to_owned(),
            max_rating: Some(13),
            unknown: toml::Table::new(),
        },
        ..Settings::default()
    };
    let text = to_toml_string(&settings).expect("serialize");
    let reloaded = load_from_str(&text).expect("reload");
    assert_eq!(reloaded.parental.pin_record, FAKE_RECORD);
    assert_eq!(reloaded.parental.max_rating, Some(13));
}

#[test]
fn unset_parental_settings_mean_no_pin_and_no_limit() {
    let defaults = Settings::default().parental;
    assert!(defaults.pin_record.is_empty());
    assert_eq!(defaults.max_rating, None);
    let text = to_toml_string(&Settings::default()).expect("serialize");
    let reloaded = load_from_str(&text).expect("reload");
    assert_eq!(reloaded.parental.max_rating, None);
}

#[test]
fn a_record_with_spaces_or_control_text_is_refused() {
    let mut settings = Settings::default();
    settings.parental.pin_record = "argon2id$v=19 injected".to_owned();
    assert!(matches!(
        settings.validate(),
        Err(SettingsError::Invalid {
            field: "parental.pin_record",
            ..
        })
    ));
}

#[test]
fn an_overlong_record_is_refused() {
    let mut settings = Settings::default();
    settings.parental.pin_record = "a".repeat(MAX_PIN_RECORD_LEN + 1);
    assert!(matches!(
        settings.validate(),
        Err(SettingsError::Invalid {
            field: "parental.pin_record",
            ..
        })
    ));
}

#[test]
fn debug_output_never_shows_the_pin_record() {
    let parental = ParentalSettings {
        pin_record: FAKE_RECORD.to_owned(),
        max_rating: None,
        unknown: toml::Table::new(),
    };
    let shown = format!("{parental:?}");
    assert!(!shown.contains(FAKE_RECORD), "{shown}");
    assert!(shown.contains("<redacted>"), "{shown}");
}

#[test]
fn default_file_has_no_credential_keys_or_urls() {
    let mut settings = Settings::default();
    settings.parental.max_rating = Some(10);
    settings.diagnostics.crash_reporting = true;
    let text = to_toml_string(&settings).expect("serialize");
    let table: toml::Table = text.parse().expect("parses as TOML");
    assert_eq!(find_secret_like_key(&table), None, "{text}");
    assert_eq!(find_secret_like_value(&table), None, "{text}");
}
