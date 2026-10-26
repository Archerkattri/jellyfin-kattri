//! Named settings profiles (M2-13): one settings file per profile, a default profile,
//! an active-profile pointer, and a versioned, atomically written index.
//!
//! Layout under the profiles directory, which the caller passes to [`ProfileStore::open`]
//! (nothing here reads the platform config directory):
//! - `index.toml`: the versioned index (see [`ProfileIndex`]).
//! - `<profile>/settings.toml`: the profile's settings, read and written through [`crate::store`],
//!   so the same credential-key guard applies as to the main settings file.
//! - `deleted/<profile>-<unix-time>/`: profile folders moved aside by [`ProfileStore::delete`]
//!   or by a rollback. Nothing is hard-deleted.
//!
//! Rules:
//! - A profile name is 1 to 32 characters of lowercase ASCII letters, digits and hyphens. It
//!   must not be a Windows device name (`con`, `nul`, `com0`-`com9`, `lpt0`-`lpt9`, and so on)
//!   or `deleted`, which is reserved for the layout above.
//! - Names are unique case-insensitively. With the lowercase-only charset this is an extra
//!   guard; mixed-case names are refused by the charset rule first.
//! - The default profile cannot be deleted; it can be renamed (the default pointer follows).
//! - The last profile cannot be deleted, and the active profile cannot be deleted until a
//!   switch has made another profile active.
//! - Switching is restart-based (M2-13). [`ProfileStore::set_active`] only persists the pointer;
//!   the caller restarts the process with `--profile <name>`.
//! - No file locking. Two processes writing the same index: last writer wins (M2-01).

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use toml::{Table, Value};

use crate::error::SettingsError;
use crate::migrate::SCHEMA_VERSION_KEY;
use crate::schema::Settings;
use crate::store::{self, LoadWarning};

/// Version of the profile index format written and accepted by this build.
pub const PROFILE_INDEX_VERSION: u32 = 1;
/// File name of the index inside the profiles directory.
pub const PROFILE_INDEX_FILE: &str = "index.toml";
/// File name of each profile's settings inside its folder.
pub const PROFILE_SETTINGS_FILE: &str = "settings.toml";
/// Folder inside the profiles directory that receives removed profile folders.
pub const PROFILE_DELETED_DIR: &str = "deleted";
/// Name of the profile created on first run.
pub const DEFAULT_PROFILE_NAME: &str = "default";
/// Longest allowed profile name, in characters.
pub const PROFILE_NAME_MAX_LEN: usize = 32;

/// Errors from profile names, the index and profile operations.
///
/// Settings-file failures (I/O, parse, the credential guard) arrive as [`ProfileError::Settings`]
/// or [`ProfileError::Io`].
#[derive(Debug, thiserror::Error)]
pub enum ProfileError {
    /// A settings file could not be read, parsed, validated or written.
    #[error(transparent)]
    Settings(#[from] SettingsError),

    /// The profiles directory or the index could not be read or written.
    #[error("profile storage I/O failed: {0}")]
    Io(#[from] io::Error),

    /// The profile name breaks a rule in [`validate_profile_name`].
    #[error("profile name `{name}` is not allowed: {reason}")]
    InvalidName { name: String, reason: &'static str },

    /// The index file is not UTF-8 or TOML, or does not match the index shape. The file is
    /// refused and left as it is, so the profile list is never silently replaced.
    #[error("profile index is corrupt and was left unchanged: {0}")]
    CorruptIndex(String),

    /// The index is structurally wrong (missing key, default or active not listed, duplicate).
    #[error("profile index is invalid: {reason}")]
    InvalidIndex { reason: &'static str },

    /// The index was written by a different format version. It is refused, never rewritten.
    #[error("profile index version {found} is not supported (expected {supported})")]
    UnsupportedIndexVersion { found: i64, supported: u32 },

    /// Another profile already uses this name (compared case-insensitively).
    #[error("a profile named `{name}` already exists")]
    Duplicate { name: String },

    /// No profile with this name is listed in the index.
    #[error("no profile named `{name}`")]
    NotFound { name: String },

    /// The last remaining profile cannot be deleted.
    #[error("the last profile cannot be deleted")]
    LastProfile,

    /// The default profile cannot be deleted. It can be renamed.
    #[error("profile `{name}` is the default profile and cannot be deleted")]
    DefaultProfile { name: String },

    /// The active profile cannot be deleted. Switch to another profile first.
    #[error("profile `{name}` is active; switch to another profile before deleting it")]
    ActiveProfile { name: String },

    /// A folder with the target name already exists on disk.
    #[error("a folder for profile `{name}` already exists on disk")]
    FolderExists { name: String },
}

/// The versioned index: which profiles exist, which one is the default, and which is active.
///
/// TOML example:
/// ```toml
/// schema_version = 1
/// default = "default"
/// active = "kids"
/// profiles = ["default", "kids"]
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileIndex {
    pub schema_version: u32,
    /// Name of the default profile. Always listed in `profiles`.
    pub default: String,
    /// Name of the active profile. Always listed in `profiles`.
    pub active: String,
    /// Profile names in creation order.
    pub profiles: Vec<String>,
}

/// Check a profile name against the rules in the module documentation.
///
/// The rules are checked in this order, so the first failing rule is the one reported:
/// length, path separators, charset, reserved names.
pub fn validate_profile_name(name: &str) -> Result<(), ProfileError> {
    let invalid = |reason: &'static str| ProfileError::InvalidName {
        name: name.to_owned(),
        reason,
    };
    if name.is_empty() || name.len() > PROFILE_NAME_MAX_LEN {
        return Err(invalid("must be 1 to 32 characters"));
    }
    if name.contains(['/', '\\']) {
        return Err(invalid("must not contain a path separator"));
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        return Err(invalid(
            "only lowercase letters, digits and hyphens are allowed",
        ));
    }
    if is_reserved_name(name) {
        return Err(invalid("is a reserved name"));
    }
    Ok(())
}

/// Windows device names and the folder name used for removed profiles.
fn is_reserved_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    if lower == PROFILE_DELETED_DIR || matches!(lower.as_str(), "con" | "prn" | "aux" | "nul") {
        return true;
    }
    ["com", "lpt"].iter().any(|prefix| {
        matches!(
            lower.strip_prefix(prefix),
            Some(digit) if digit.len() == 1 && digit.bytes().all(|b| b.is_ascii_digit())
        )
    })
}

/// Parse an index from TOML text. The version is checked before any field is read.
pub fn parse_index(text: &str) -> Result<ProfileIndex, ProfileError> {
    let table: Table = text
        .parse()
        .map_err(|err: toml::de::Error| ProfileError::CorruptIndex(err.message().to_owned()))?;
    match table.get(SCHEMA_VERSION_KEY) {
        None => {
            return Err(ProfileError::InvalidIndex {
                reason: "schema_version is missing",
            });
        }
        Some(Value::Integer(found)) if *found == i64::from(PROFILE_INDEX_VERSION) => {}
        Some(Value::Integer(found)) => {
            return Err(ProfileError::UnsupportedIndexVersion {
                found: *found,
                supported: PROFILE_INDEX_VERSION,
            });
        }
        Some(_) => {
            return Err(ProfileError::InvalidIndex {
                reason: "schema_version must be an integer",
            });
        }
    }
    let index: ProfileIndex = Value::Table(table)
        .try_into()
        .map_err(|err: toml::de::Error| ProfileError::CorruptIndex(err.message().to_owned()))?;
    validate_index(&index)?;
    Ok(index)
}

/// Serialize an index to pretty TOML. Validates first, so an invalid index is never written.
pub fn to_index_string(index: &ProfileIndex) -> Result<String, ProfileError> {
    validate_index(index)?;
    Ok(toml::to_string_pretty(index).map_err(SettingsError::Serialize)?)
}

fn validate_index(index: &ProfileIndex) -> Result<(), ProfileError> {
    if index.schema_version != PROFILE_INDEX_VERSION {
        return Err(ProfileError::UnsupportedIndexVersion {
            found: i64::from(index.schema_version),
            supported: PROFILE_INDEX_VERSION,
        });
    }
    if index.profiles.is_empty() {
        return Err(ProfileError::InvalidIndex {
            reason: "at least one profile must be listed",
        });
    }
    for (i, name) in index.profiles.iter().enumerate() {
        validate_profile_name(name)?;
        if index.profiles[..i]
            .iter()
            .any(|earlier| earlier.eq_ignore_ascii_case(name))
        {
            return Err(ProfileError::InvalidIndex {
                reason: "profile names must be unique",
            });
        }
    }
    if !index.profiles.contains(&index.default) {
        return Err(ProfileError::InvalidIndex {
            reason: "the default profile is not listed",
        });
    }
    if !index.profiles.contains(&index.active) {
        return Err(ProfileError::InvalidIndex {
            reason: "the active profile is not listed",
        });
    }
    Ok(())
}

/// Owns the profiles directory and its index. All changes go through its methods, which keep the
/// folders and the index in step and roll back the folder change if the index write fails.
#[derive(Debug, Clone)]
pub struct ProfileStore {
    dir: PathBuf,
    index: ProfileIndex,
}

impl ProfileStore {
    /// Open the profiles in `dir`. On first run (no index) the `default` profile is created with
    /// default settings and the index is written.
    ///
    /// A present index that fails to parse, has an unknown version, or is structurally wrong is
    /// refused with an error and is not overwritten.
    pub fn open(dir: impl Into<PathBuf>) -> Result<Self, ProfileError> {
        let dir = dir.into();
        let index_path = dir.join(PROFILE_INDEX_FILE);
        match fs::read_to_string(&index_path) {
            Ok(text) => Ok(Self {
                index: parse_index(&text)?,
                dir,
            }),
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                let index = ProfileIndex {
                    schema_version: PROFILE_INDEX_VERSION,
                    default: DEFAULT_PROFILE_NAME.to_owned(),
                    active: DEFAULT_PROFILE_NAME.to_owned(),
                    profiles: vec![DEFAULT_PROFILE_NAME.to_owned()],
                };
                let settings_path = dir.join(DEFAULT_PROFILE_NAME).join(PROFILE_SETTINGS_FILE);
                if !settings_path.exists() {
                    store::save(&settings_path, &Settings::default())?;
                }
                let text = to_index_string(&index)?;
                write_atomic(&index_path, text.as_bytes())?;
                Ok(Self { dir, index })
            }
            // Non-UTF-8 bytes are a corrupt index, not an I/O failure (M2-01 rule 3).
            Err(err) if err.kind() == io::ErrorKind::InvalidData => Err(
                ProfileError::CorruptIndex("index is not valid UTF-8".to_owned()),
            ),
            Err(err) => Err(ProfileError::Io(err)),
        }
    }

    /// Profile names in creation order.
    pub fn names(&self) -> &[String] {
        &self.index.profiles
    }

    /// Name of the default profile.
    pub fn default_profile(&self) -> &str {
        &self.index.default
    }

    /// Name of the active profile.
    pub fn active_profile(&self) -> &str {
        &self.index.active
    }

    /// The profiles directory this store was opened on.
    pub fn profiles_dir(&self) -> &Path {
        &self.dir
    }

    /// Folder of a listed profile: `<profiles>/<name>`. Validates the name and requires that it
    /// is listed, so a name can never reach a path outside the profiles directory.
    pub fn profile_dir(&self, name: &str) -> Result<PathBuf, ProfileError> {
        validate_profile_name(name)?;
        self.position(name)?;
        Ok(self.dir.join(name))
    }

    /// Settings file of a listed profile: `<profiles>/<name>/settings.toml`.
    pub fn settings_path(&self, name: &str) -> Result<PathBuf, ProfileError> {
        Ok(self.profile_dir(name)?.join(PROFILE_SETTINGS_FILE))
    }

    /// Create a profile with default settings. The name must be valid, unique and not an
    /// existing folder on disk.
    pub fn create(&mut self, name: &str) -> Result<(), ProfileError> {
        validate_profile_name(name)?;
        self.ensure_unique(name, None)?;
        let dir = self.dir.join(name);
        if dir.exists() {
            return Err(ProfileError::FolderExists {
                name: name.to_owned(),
            });
        }
        fs::create_dir_all(&dir)?;
        let result = store::save(&dir.join(PROFILE_SETTINGS_FILE), &Settings::default())
            .map_err(ProfileError::from)
            .and_then(|()| {
                let mut next = self.index.clone();
                next.profiles.push(name.to_owned());
                self.commit(next)
            });
        if let Err(err) = result {
            // The new folder holds only defaults. Move it aside rather than delete it.
            let _ = self.move_to_deleted(&dir, name);
            return Err(err);
        }
        Ok(())
    }

    /// Rename a profile. Renaming the default or the active profile moves that pointer too.
    /// Renaming to the same name is a no-op.
    pub fn rename(&mut self, old: &str, new: &str) -> Result<(), ProfileError> {
        let pos = self.position(old)?;
        if old == new {
            return Ok(());
        }
        validate_profile_name(new)?;
        self.ensure_unique(new, Some(old))?;
        let from = self.dir.join(old);
        let to = self.dir.join(new);
        if to.exists() {
            return Err(ProfileError::FolderExists {
                name: new.to_owned(),
            });
        }
        let moved = from.exists();
        if moved {
            fs::rename(&from, &to)?;
        }
        let mut next = self.index.clone();
        next.profiles[pos] = new.to_owned();
        if next.default == old {
            next.default = new.to_owned();
        }
        if next.active == old {
            next.active = new.to_owned();
        }
        if let Err(err) = self.commit(next) {
            if moved {
                // Put the folder back so the index and the disk agree again.
                let _ = fs::rename(&to, &from);
            }
            return Err(err);
        }
        Ok(())
    }

    /// Remove a profile from the index and move its folder to `deleted/<name>-<unix-time>`.
    ///
    /// Refused for the last profile, the default profile, and the active profile.
    pub fn delete(&mut self, name: &str) -> Result<(), ProfileError> {
        let pos = self.position(name)?;
        if self.index.profiles.len() == 1 {
            return Err(ProfileError::LastProfile);
        }
        if self.index.default == name {
            return Err(ProfileError::DefaultProfile {
                name: name.to_owned(),
            });
        }
        if self.index.active == name {
            return Err(ProfileError::ActiveProfile {
                name: name.to_owned(),
            });
        }
        let from = self.dir.join(name);
        let moved_to = if from.exists() {
            Some(self.move_to_deleted(&from, name)?)
        } else {
            None
        };
        let mut next = self.index.clone();
        next.profiles.remove(pos);
        if let Err(err) = self.commit(next) {
            if let Some(to) = moved_to {
                let _ = fs::rename(&to, &from);
            }
            return Err(err);
        }
        Ok(())
    }

    /// Make `name` the active profile and persist the pointer. The caller restarts the process
    /// with `--profile <name>` (M2-13: restart-based switching).
    ///
    /// The target's settings file must pass the no-credential check, so a switch into a profile
    /// with a credential-like key is refused and the active pointer is unchanged.
    pub fn set_active(&mut self, name: &str) -> Result<(), ProfileError> {
        self.position(name)?;
        self.load_settings(name)?;
        if self.index.active == name {
            return Ok(());
        }
        let mut next = self.index.clone();
        next.active = name.to_owned();
        self.commit(next)
    }

    /// Load a profile's settings through the same guard as the main settings file. A missing
    /// file yields defaults. Returns the load report, as [`store::load_with_report`] does.
    pub fn load_settings(&self, name: &str) -> Result<(Settings, Vec<LoadWarning>), ProfileError> {
        let path = self.settings_path(name)?;
        Ok(store::load_with_report(&path)?)
    }

    /// Validate and atomically write a profile's settings.
    pub fn save_settings(&self, name: &str, settings: &Settings) -> Result<(), ProfileError> {
        let path = self.settings_path(name)?;
        store::save(&path, settings)?;
        Ok(())
    }

    fn position(&self, name: &str) -> Result<usize, ProfileError> {
        self.index
            .profiles
            .iter()
            .position(|listed| listed == name)
            .ok_or_else(|| ProfileError::NotFound {
                name: name.to_owned(),
            })
    }

    /// Refuse a name that matches a listed profile other than `ignore`, case-insensitively.
    fn ensure_unique(&self, name: &str, ignore: Option<&str>) -> Result<(), ProfileError> {
        let clash = self
            .index
            .profiles
            .iter()
            .any(|listed| Some(listed.as_str()) != ignore && listed.eq_ignore_ascii_case(name));
        if clash {
            return Err(ProfileError::Duplicate {
                name: name.to_owned(),
            });
        }
        Ok(())
    }

    /// Write `next` to the index file atomically, then make it the in-memory index.
    fn commit(&mut self, next: ProfileIndex) -> Result<(), ProfileError> {
        let text = to_index_string(&next)?;
        write_atomic(&self.dir.join(PROFILE_INDEX_FILE), text.as_bytes())?;
        self.index = next;
        Ok(())
    }

    /// Move a profile folder into `deleted/` under a name that does not exist yet.
    fn move_to_deleted(&self, from: &Path, name: &str) -> Result<PathBuf, ProfileError> {
        let deleted = self.dir.join(PROFILE_DELETED_DIR);
        fs::create_dir_all(&deleted)?;
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_secs())
            .unwrap_or(0);
        let mut target = deleted.join(format!("{name}-{secs}"));
        let mut suffix = 1_u32;
        while target.exists() {
            target = deleted.join(format!("{name}-{secs}-{suffix}"));
            suffix += 1;
        }
        fs::rename(from, &target)?;
        Ok(target)
    }
}

/// Write `bytes` to `path` through a temporary file in the same folder: write, flush to disk,
/// then rename over the target. A crash leaves the old file or the new one, never a partial one.
fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(dir)?;
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    let result: io::Result<()> = (|| {
        let mut file = File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        // std::fs::rename replaces an existing destination on Windows and Unix.
        fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}
