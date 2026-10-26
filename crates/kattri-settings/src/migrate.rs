//! Versioned settings migrations.
//!
//! A settings file carries `schema_version`. A file without one is treated as
//! version 1, the first versioned layout. [`migrate`] applies steps in order
//! until the file reaches [`CURRENT_SCHEMA_VERSION`]. Each step moves exactly
//! one version forward (`from` -> `from + 1`).
//!
//! To change the schema:
//! 1. Bump `CURRENT_SCHEMA_VERSION` in `schema.rs`.
//! 2. Add a `Migration { from: <old version>, apply: ... }` to [`MIGRATIONS`].
//! 3. Add a test that feeds an old-layout TOML document through `load_from_str`.
//!
//! Files newer than the build are rejected with `FutureVersion`.

use toml::{Table, Value};

use crate::error::SettingsError;
use crate::schema::{CURRENT_SCHEMA_VERSION, OUTRO_DELAY_MS_MAX};

/// Key that holds the schema version in the TOML document.
pub const SCHEMA_VERSION_KEY: &str = "schema_version";

/// A single migration step that rewrites a document from `from` to `from + 1`.
#[derive(Debug, Clone, Copy)]
pub struct Migration {
    pub from: u32,
    pub apply: fn(&mut Table) -> Result<(), SettingsError>,
}

/// Ordered migration steps for the real schema.
pub const MIGRATIONS: &[Migration] = &[
    Migration {
        from: 1,
        apply: retire_hardware_decoding,
    },
    Migration {
        from: 2,
        apply: outro_delay_seconds_to_milliseconds,
    },
];

/// Legacy key in `[video]` that version 2 replaced with `decode_mode`.
const LEGACY_HARDWARE_DECODING_KEY: &str = "hardware_decoding";

/// Current key in `[video]` for the hardware decode backend.
const DECODE_MODE_KEY: &str = "decode_mode";

/// Legacy key in `[playback]`, in seconds, that version 3 replaced with `outro_delay_ms`.
const LEGACY_OUTRO_DELAY_SECS_KEY: &str = "skip_outro_delay_secs";

/// Current key in `[playback]` for the outro skip delay, in milliseconds.
const OUTRO_DELAY_MS_KEY: &str = "outro_delay_ms";

/// Version 2 to 3: `playback.skip_outro_delay_secs` becomes `playback.outro_delay_ms`.
///
/// The seconds value is multiplied by 1000 and clamped to 0..=[`OUTRO_DELAY_MS_MAX`]. An
/// `outro_delay_ms` that is already present wins, and the legacy key is always removed. A
/// non-integer legacy value is refused, because it has no safe meaning.
fn outro_delay_seconds_to_milliseconds(table: &mut Table) -> Result<(), SettingsError> {
    let Some(Value::Table(playback)) = table.get_mut("playback") else {
        return Ok(());
    };
    let Some(legacy) = playback.remove(LEGACY_OUTRO_DELAY_SECS_KEY) else {
        return Ok(());
    };
    let Value::Integer(seconds) = legacy else {
        return Err(SettingsError::Invalid {
            field: "playback.skip_outro_delay_secs",
            reason: "must be a whole number of seconds".to_owned(),
        });
    };
    if !playback.contains_key(OUTRO_DELAY_MS_KEY) {
        let milliseconds = seconds
            .saturating_mul(1_000)
            .clamp(0, i64::from(OUTRO_DELAY_MS_MAX));
        playback.insert(OUTRO_DELAY_MS_KEY.to_owned(), Value::Integer(milliseconds));
    }
    Ok(())
}

/// Version 1 to 2: `video.hardware_decoding` becomes `video.decode_mode`.
///
/// The legacy values map as follows: `enabled` to `auto`, `osx_compat` to `videotoolbox`,
/// `copy` to `auto` and `disabled` to `off`. Any other value is refused with
/// [`SettingsError::Invalid`], because the old value has no safe meaning.
///
/// The version 1 build wrote both keys, so a version 1 file may hold `decode_mode` as well.
/// A `decode_mode` other than `auto` is an explicit choice on the new key and is kept. When
/// `decode_mode` is absent or `auto`, the legacy value decides. The legacy key is always removed.
fn retire_hardware_decoding(table: &mut Table) -> Result<(), SettingsError> {
    let Some(Value::Table(video)) = table.get_mut("video") else {
        return Ok(());
    };
    let Some(legacy) = video.remove(LEGACY_HARDWARE_DECODING_KEY) else {
        return Ok(());
    };
    let mapped = match &legacy {
        Value::String(text) => map_legacy_hardware_decoding(text)?,
        _ => return Err(legacy_value_error()),
    };

    let explicit_choice = matches!(
        video.get(DECODE_MODE_KEY),
        Some(Value::String(current)) if current != "auto"
    );
    if !explicit_choice {
        video.insert(DECODE_MODE_KEY.to_owned(), Value::String(mapped.to_owned()));
    }
    Ok(())
}

/// Maps one legacy `video.hardwareDecoding` value to its `decode_mode` spelling.
fn map_legacy_hardware_decoding(value: &str) -> Result<&'static str, SettingsError> {
    match value {
        "enabled" | "copy" => Ok("auto"),
        "osx_compat" => Ok("videotoolbox"),
        "disabled" => Ok("off"),
        _ => Err(legacy_value_error()),
    }
}

fn legacy_value_error() -> SettingsError {
    SettingsError::Invalid {
        field: "video.hardware_decoding",
        reason: "must be enabled, osx_compat, copy or disabled".to_owned(),
    }
}

/// Read the schema version from a parsed document. A missing key means version 1.
pub fn read_version(table: &Table) -> Result<u32, SettingsError> {
    match table.get(SCHEMA_VERSION_KEY) {
        None => Ok(1),
        Some(Value::Integer(v)) if *v >= 1 && *v <= i64::from(u32::MAX) => Ok(*v as u32),
        Some(_) => Err(SettingsError::Invalid {
            field: SCHEMA_VERSION_KEY,
            reason: "must be a positive integer".to_owned(),
        }),
    }
}

/// Migrate a document to [`CURRENT_SCHEMA_VERSION`] using the built-in [`MIGRATIONS`].
pub fn migrate(table: Table) -> Result<Table, SettingsError> {
    migrate_with(table, MIGRATIONS, CURRENT_SCHEMA_VERSION)
}

/// Migrate a document to `target` using the given steps.
///
/// Exposed so tests and future tooling can run a chain without touching the
/// shipped [`MIGRATIONS`].
pub fn migrate_with(
    mut table: Table,
    steps: &[Migration],
    target: u32,
) -> Result<Table, SettingsError> {
    let mut version = read_version(&table)?;
    if version > target {
        return Err(SettingsError::FutureVersion {
            found: version,
            supported: target,
        });
    }
    while version < target {
        let step = steps
            .iter()
            .find(|step| step.from == version)
            .ok_or(SettingsError::MissingMigration { from: version })?;
        (step.apply)(&mut table)?;
        version += 1;
        table.insert(
            SCHEMA_VERSION_KEY.to_owned(),
            Value::Integer(i64::from(version)),
        );
    }
    table.insert(
        SCHEMA_VERSION_KEY.to_owned(),
        Value::Integer(i64::from(target)),
    );
    Ok(table)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(text: &str) -> Table {
        text.parse().expect("test document is valid TOML")
    }

    fn rename_volume(table: &mut Table) -> Result<(), SettingsError> {
        // v1 -> v2 (test only): `[audio] volume` becomes `[audio] max_volume_percent`.
        if let Some(Value::Table(audio)) = table.get_mut("audio")
            && let Some(value) = audio.remove("volume")
        {
            audio.insert("max_volume_percent".to_owned(), value);
        }
        Ok(())
    }

    fn add_gapless(table: &mut Table) -> Result<(), SettingsError> {
        // v2 -> v3 (test only): gapless defaults to true when absent.
        let audio = table
            .entry("audio".to_owned())
            .or_insert_with(|| Value::Table(Table::new()));
        if let Value::Table(audio) = audio {
            audio
                .entry("gapless".to_owned())
                .or_insert(Value::Boolean(true));
        }
        Ok(())
    }

    const TEST_STEPS: &[Migration] = &[
        Migration {
            from: 1,
            apply: rename_volume,
        },
        Migration {
            from: 2,
            apply: add_gapless,
        },
    ];

    #[test]
    fn missing_version_is_treated_as_one() {
        assert_eq!(read_version(&doc("[audio]\ngapless = false")).unwrap(), 1);
    }

    #[test]
    fn non_integer_version_is_rejected() {
        let err = read_version(&doc("schema_version = \"two\"")).unwrap_err();
        assert!(matches!(
            err,
            SettingsError::Invalid {
                field: "schema_version",
                ..
            }
        ));
    }

    #[test]
    fn current_version_needs_no_steps() {
        let out = migrate(doc("")).unwrap();
        assert_eq!(read_version(&out).unwrap(), CURRENT_SCHEMA_VERSION);
    }

    #[test]
    fn chain_applies_steps_in_order_and_stamps_target() {
        let input = doc("[audio]\nvolume = 150\n");
        let out = migrate_with(input, TEST_STEPS, 3).unwrap();
        let audio = out["audio"].as_table().unwrap();
        assert_eq!(audio["max_volume_percent"].as_integer(), Some(150));
        assert!(!audio.contains_key("volume"));
        assert_eq!(audio["gapless"].as_bool(), Some(true));
        assert_eq!(read_version(&out).unwrap(), 3);
    }

    #[test]
    fn gap_in_chain_is_reported() {
        let steps = &TEST_STEPS[..1];
        let err = migrate_with(doc(""), steps, 3).unwrap_err();
        assert!(matches!(err, SettingsError::MissingMigration { from: 2 }));
    }

    #[test]
    fn future_version_is_refused_not_downgraded() {
        let err = migrate_with(doc("schema_version = 9"), TEST_STEPS, 3).unwrap_err();
        assert!(matches!(
            err,
            SettingsError::FutureVersion {
                found: 9,
                supported: 3
            }
        ));
    }
}
