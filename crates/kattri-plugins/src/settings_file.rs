//! Plugin settings file: `plugins.toml`, one table per plugin id.
//!
//! [`SettingsFile::save`] writes only the keys a plugin's manifest declares. Everything else
//! already in the file is carried through unchanged: other plugins' tables, undeclared keys
//! inside a plugin's table, and unknown top-level entries.
//!
//! - Atomic write: the new text goes to `plugins.toml.tmp`, is synced, and is then renamed
//!   over `plugins.toml`. A reader sees the old file or the new one, never a partial file.
//! - Lock: `plugins.toml.lock` is created with `create_new`, and a second writer gets
//!   [`PluginError::Locked`] rather than waiting. The lock file is removed when the save
//!   finishes. If the process dies mid-save, the lock file stays and must be removed by hand.
//! - Corrupt file: a file that is not UTF-8 or not valid TOML is renamed to
//!   `plugins.toml.corrupt-<unix seconds>` and loading continues with no stored values.
//!
//! Stored values are checked again on load: a value that fails [`validate_value`], or a key
//! the manifest does not declare, is not loaded.

use std::fs::{self, OpenOptions};
use std::io::{self, ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use toml::{Table, Value};

use crate::error::PluginError;
use crate::manifest::PluginManifest;
use crate::settings::{PluginSettingsStore, validate_value};

/// File name of the settings file inside the settings directory.
pub const SETTINGS_FILE_NAME: &str = "plugins.toml";

/// Upper bound on moved-aside names tried for one corrupt file.
const MAX_ASIDE_ATTEMPTS: u32 = 1000;

/// What [`SettingsFile::load`] produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadOutcome {
    /// The declared values that were stored and passed validation.
    pub store: PluginSettingsStore,
    /// Set when the file was corrupt and has been renamed to this path.
    pub moved_aside: Option<PathBuf>,
}

/// A `plugins.toml` file at a fixed path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingsFile {
    path: PathBuf,
}

/// The result of reading the file once.
enum Document {
    Missing,
    Parsed(Table),
    MovedAside(PathBuf),
}

impl SettingsFile {
    /// A settings file at `path`. The file and its parent directory need not exist yet.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Path of the settings file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Load the declared values of every plugin in `manifests`. A missing file yields an empty
    /// store and does not create anything.
    pub fn load(&self, manifests: &[PluginManifest]) -> Result<LoadOutcome, PluginError> {
        let mut store = PluginSettingsStore::new();
        let table = match self.read_document()? {
            Document::Missing => {
                return Ok(LoadOutcome {
                    store,
                    moved_aside: None,
                });
            }
            Document::MovedAside(path) => {
                return Ok(LoadOutcome {
                    store,
                    moved_aside: Some(path),
                });
            }
            Document::Parsed(table) => table,
        };

        for manifest in manifests {
            let Some(plugin_table) = table.get(&manifest.id).and_then(Value::as_table) else {
                continue;
            };
            for key in &manifest.settings {
                if let Some(Value::String(value)) = plugin_table.get(key)
                    && validate_value(value).is_ok()
                {
                    store.restore(&manifest.id, key, value.clone());
                }
            }
        }
        Ok(LoadOutcome {
            store,
            moved_aside: None,
        })
    }

    /// Write the stored values of every plugin in `manifests`, for its declared keys.
    ///
    /// A declared key with no stored value is removed from the file. Everything else in the
    /// file is kept. Pass every manifest whose values should be persisted: plugins left out
    /// are not written, and their tables are kept as they are.
    pub fn save(
        &self,
        store: &PluginSettingsStore,
        manifests: &[PluginManifest],
    ) -> Result<(), PluginError> {
        let dir = self.directory();
        fs::create_dir_all(dir).map_err(|err| self.storage(err))?;
        let _lock = LockGuard::acquire(self.lock_path())?;

        let mut document = match self.read_document()? {
            Document::Parsed(table) => table,
            Document::Missing | Document::MovedAside(_) => Table::new(),
        };

        for manifest in manifests {
            let stored = store.table(&manifest.id);
            let entry = document
                .entry(manifest.id.clone())
                .or_insert_with(|| Value::Table(Table::new()));
            if !entry.is_table() {
                *entry = Value::Table(Table::new());
            }
            let Value::Table(plugin_table) = entry else {
                continue;
            };
            for key in &manifest.settings {
                match stored.and_then(|values| values.get(key)) {
                    Some(value) => {
                        plugin_table.insert(key.clone(), Value::String(value.clone()));
                    }
                    None => {
                        plugin_table.remove(key);
                    }
                }
            }
            let empty = plugin_table.is_empty();
            if empty {
                document.remove(&manifest.id);
            }
        }

        let text = toml::to_string_pretty(&document).map_err(|err| self.storage(err))?;
        self.write_atomic(&text)
    }

    fn read_document(&self) -> Result<Document, PluginError> {
        let bytes = match fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == ErrorKind::NotFound => return Ok(Document::Missing),
            Err(err) => return Err(self.storage(err)),
        };
        let parsed = std::str::from_utf8(&bytes)
            .ok()
            .and_then(|text| text.parse::<Table>().ok());
        match parsed {
            Some(table) => Ok(Document::Parsed(table)),
            None => Ok(Document::MovedAside(self.move_aside()?)),
        }
    }

    /// Rename a corrupt file to a name that does not exist yet, and return that name.
    fn move_aside(&self) -> Result<PathBuf, PluginError> {
        let stamp = unix_now();
        for attempt in 0..MAX_ASIDE_ATTEMPTS {
            let name = if attempt == 0 {
                format!("{SETTINGS_FILE_NAME}.corrupt-{stamp}")
            } else {
                format!("{SETTINGS_FILE_NAME}.corrupt-{stamp}-{attempt}")
            };
            let target = self.directory().join(name);
            if target.exists() {
                continue;
            }
            fs::rename(&self.path, &target).map_err(|err| self.storage(err))?;
            return Ok(target);
        }
        Err(self.storage("no free name for the moved-aside copy of the corrupt file"))
    }

    fn write_atomic(&self, text: &str) -> Result<(), PluginError> {
        let temp = self.temp_path();
        let result = write_and_sync(&temp, text).and_then(|()| fs::rename(&temp, &self.path));
        if result.is_err() {
            let _ = fs::remove_file(&temp);
        }
        result.map_err(|err| self.storage(err))
    }

    fn directory(&self) -> &Path {
        self.path.parent().unwrap_or_else(|| Path::new("."))
    }

    fn temp_path(&self) -> PathBuf {
        self.with_suffix(".tmp")
    }

    fn lock_path(&self) -> PathBuf {
        self.with_suffix(".lock")
    }

    fn with_suffix(&self, suffix: &str) -> PathBuf {
        let mut name = self
            .path
            .file_name()
            .map(|n| n.to_os_string())
            .unwrap_or_default();
        name.push(suffix);
        self.directory().join(name)
    }

    fn storage(&self, reason: impl std::fmt::Display) -> PluginError {
        PluginError::Storage {
            path: self.path.display().to_string(),
            reason: reason.to_string(),
        }
    }
}

fn write_and_sync(path: &Path, text: &str) -> io::Result<()> {
    let mut file = fs::File::create(path)?;
    file.write_all(text.as_bytes())?;
    file.sync_all()
}

/// Holds the settings lock. The lock file is removed on drop.
struct LockGuard {
    path: PathBuf,
}

impl LockGuard {
    fn acquire(path: PathBuf) -> Result<Self, PluginError> {
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(_file) => Ok(Self { path }),
            Err(err) if err.kind() == ErrorKind::AlreadyExists => Err(PluginError::Locked {
                path: path.display().to_string(),
            }),
            Err(err) => Err(PluginError::Storage {
                path: path.display().to_string(),
                reason: err.to_string(),
            }),
        }
    }
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}
