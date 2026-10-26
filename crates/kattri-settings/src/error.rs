//! Error type for loading, validating, migrating and saving settings.

use std::io;

/// Everything that can go wrong while reading or writing the settings file.
#[derive(Debug, thiserror::Error)]
pub enum SettingsError {
    /// The file could not be read or written.
    #[error("settings file I/O failed: {0}")]
    Io(#[from] io::Error),

    /// The file is not valid TOML, or a value has the wrong type or an unknown enum spelling.
    #[error("settings file could not be parsed: {0}")]
    Parse(#[from] toml::de::Error),

    /// The settings document exceeds the parser's bounded input size.
    #[error("settings file exceeds the maximum size of {max_bytes} bytes")]
    TooLarge { max_bytes: usize },

    /// The in-memory settings could not be serialized to TOML.
    #[error("settings could not be serialized: {0}")]
    Serialize(#[from] toml::ser::Error),

    /// The file was written by a newer build. It is refused rather than downgraded.
    #[error("settings schema version {found} is newer than supported version {supported}")]
    FutureVersion { found: u32, supported: u32 },

    /// A migration chain has a gap; the schema was bumped without adding a step.
    #[error("no settings migration from schema version {from}")]
    MissingMigration { from: u32 },

    /// A value is outside its allowed range or otherwise inconsistent.
    #[error("invalid setting `{field}`: {reason}")]
    Invalid { field: &'static str, reason: String },

    /// A text field holds a value that looks like a URL or credential. Save refuses it, so
    /// the value is never written. Only the field path is reported, never the value.
    #[error(
        "settings value `{field}` looks like a URL or credential and is not stored in settings"
    )]
    SecretLikeValue { field: String },

    /// The file has a key whose name looks like a credential. The file is refused and
    /// never rewritten. Only the key path is reported, never its value.
    #[error(
        "settings file contains credential-like key `{key}`; credentials are not stored in settings"
    )]
    SecretLikeKey { key: String },
}
