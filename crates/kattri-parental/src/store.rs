//! Persistence for the PIN attempt state (M4-20).
//!
//! The attempt state is a small versioned JSON document in the profile's folder:
//! `{"version": 1, "failures": <n>, "locked_until": <unix seconds or null>}`. Only the failure
//! count and the delay end are written. The PIN record stays in the settings file, as before.
//!
//! Writes go to a uniquely created `*.tmp-*` sibling, are synced, and are renamed over the target,
//! so a crash leaves either the old or the new document. A damaged file is reported as an error
//! and is never replaced by a default. Replacing it would reset the failure count.

use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::Deserialize;
use serde_json::Value;

use crate::lock::AttemptState;

/// File name of the attempt state inside each profile's folder.
pub const ATTEMPT_FILE_NAME: &str = "parental-lock.json";
/// Format version written to and required from the attempt file.
pub const ATTEMPT_FILE_VERSION: u32 = 1;

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Failure to read or write the attempt file. Messages name the operation only, never the
/// contents of the file.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AttemptFileError {
    #[error("the parental lock file could not be {operation}: {reason}")]
    Io {
        operation: &'static str,
        reason: String,
    },
    #[error("the parental lock file is damaged or not recognized")]
    Malformed,
    #[error("the parental lock file version {found} is not supported")]
    UnsupportedVersion { found: u32 },
}

#[derive(Deserialize)]
struct VersionOnly {
    version: u32,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AttemptFile {
    #[serde(rename = "version")]
    _version: u32,
    failures: u32,
    locked_until: Value,
}

/// Encodes the attempt state as the versioned document, with a trailing newline.
pub fn encode_attempts(state: AttemptState) -> String {
    let locked_until = match state.locked_until {
        Some(until) => until.to_string(),
        None => "null".to_owned(),
    };
    format!(
        "{{\"version\":{ATTEMPT_FILE_VERSION},\"failures\":{},\"locked_until\":{locked_until}}}\n",
        state.failures
    )
}

/// Decodes an attempt document. Unknown versions are refused before the rest is parsed.
pub fn decode_attempts(text: &str) -> Result<AttemptState, AttemptFileError> {
    let version_only: VersionOnly =
        serde_json::from_str(text).map_err(|_| AttemptFileError::Malformed)?;
    if version_only.version != ATTEMPT_FILE_VERSION {
        return Err(AttemptFileError::UnsupportedVersion {
            found: version_only.version,
        });
    }
    let file: AttemptFile = serde_json::from_str(text).map_err(|_| AttemptFileError::Malformed)?;
    let locked_until = match file.locked_until {
        Value::Null => None,
        Value::Number(number) => Some(number.as_u64().ok_or(AttemptFileError::Malformed)?),
        _ => return Err(AttemptFileError::Malformed),
    };
    Ok(AttemptState {
        failures: file.failures,
        locked_until,
    })
}

/// Reads the attempt state from `path`. A missing file means no failures and no delay.
pub fn load_attempts(path: &Path) -> Result<AttemptState, AttemptFileError> {
    match fs::read_to_string(path) {
        Ok(text) => decode_attempts(&text),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(AttemptState::default()),
        Err(e) => Err(io_error("read", &e)),
    }
}

/// Writes the attempt state to `path` atomically: sibling temp file, sync, rename.
pub fn save_attempts(path: &Path, state: AttemptState) -> Result<(), AttemptFileError> {
    let encoded = encode_attempts(state);
    let (tmp, mut file) = create_temp(path).map_err(|e| io_error("created", &e))?;
    let result = (|| -> Result<(), AttemptFileError> {
        file.write_all(encoded.as_bytes())
            .map_err(|e| io_error("written", &e))?;
        file.sync_all().map_err(|e| io_error("synced", &e))?;
        drop(file);
        fs::rename(&tmp, path).map_err(|e| io_error("replaced", &e))?;
        sync_parent(path)
    })();
    if result.is_err() {
        // Best effort: a leftover temp file is clutter. The target may already have been
        // replaced if syncing the parent directory failed after the rename.
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// Removes the attempt file. A missing file is not an error.
pub fn delete_attempts(path: &Path) -> Result<(), AttemptFileError> {
    match fs::remove_file(path) {
        Ok(()) => sync_parent(path),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
        Err(e) => Err(io_error("removed", &e)),
    }
}

fn create_temp(path: &Path) -> std::io::Result<(std::path::PathBuf, File)> {
    let mut name = path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(format!(".tmp-{}-", std::process::id()));
    for _ in 0..100 {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let mut candidate_name = name.clone();
        candidate_name.push(sequence.to_string());
        let candidate = path.with_file_name(candidate_name);
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(file) => return Ok((candidate, file)),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::new(
        ErrorKind::AlreadyExists,
        "could not allocate a unique temporary file",
    ))
}

#[cfg(unix)]
fn sync_parent(path: &Path) -> Result<(), AttemptFileError> {
    match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => File::open(dir)
            .and_then(|d| d.sync_all())
            .map_err(|e| io_error("synced", &e)),
        _ => Ok(()),
    }
}

#[cfg(not(unix))]
fn sync_parent(_path: &Path) -> Result<(), AttemptFileError> {
    Ok(())
}

fn io_error(operation: &'static str, err: &std::io::Error) -> AttemptFileError {
    AttemptFileError::Io {
        operation,
        reason: err.to_string(),
    }
}
