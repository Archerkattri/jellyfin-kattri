//! Durable snapshot of a download job.
//!
//! The record is versioned JSON written atomically: a sibling `.tmp` file is written and
//! fsynced, then renamed over the target. A reader sees either the previous record or the
//! new one, never a mix.
//!
//! Contents: job state, validators (ETag and Last-Modified), total length, durable offset,
//! and the random temp file name from [`crate::fs`]. It never holds the item title, item
//! ID, URLs or credentials. Writes and reads refuse any string the credential guard flags.
//!
//! In-flight states (`Negotiating`, `Downloading`) are saved as `Queued`: the HTTP
//! response does not survive a restart, so the restored job re-negotiates from its
//! durable offset.
//!
//! Durability: the file contents are fsynced before the rename. On Unix the parent
//! directory is fsynced after the rename. On Windows std offers no directory fsync, so
//! the rename is atomic but is not synced to disk after it happens.

use kattri_client_core::downloads::{
    DownloadJob, DownloadSnapshot, DownloadState, DownloadTransitionError, Validators,
};
use kattri_settings::guard::is_secret_like_value;
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

/// The only format version this build reads or writes.
pub const FORMAT_VERSION: u32 = 1;
/// A real record is a few hundred bytes. Larger files are refused before parsing.
pub const MAX_RECORD_BYTES: u64 = 16 * 1024;
/// Temp file names are 16 hex characters plus `.part` (see `fs::random_name`).
const TEMP_NAME_HEX_LEN: usize = 16;
const TEMP_NAME_SUFFIX: &str = ".part";

#[derive(Debug, thiserror::Error)]
pub enum PersistError {
    #[error("snapshot file could not be read or written: {0}")]
    Io(#[from] io::Error),
    #[error("snapshot is not a valid format {FORMAT_VERSION} record")]
    Malformed,
    #[error("snapshot format version {0} is not supported")]
    UnsupportedVersion(u64),
    #[error("snapshot is larger than {MAX_RECORD_BYTES} bytes")]
    TooLarge,
    #[error("snapshot field contains URL or credential text")]
    SecretLikeField,
    #[error("snapshot is inconsistent: {0}")]
    Inconsistent(&'static str),
    #[error("snapshot cannot be restored: {0:?}")]
    Restore(DownloadTransitionError),
}

/// Serialized form of [`DownloadState`]. Kept separate so the file format does not depend
/// on the in-memory enum.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PersistedState {
    Queued,
    Paused,
    NeedsAuth,
    Verifying,
    Committed,
    Cancelled,
    Failed,
}

/// The persisted snapshot record (format version 1).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PersistedDownload {
    pub version: u32,
    pub state: PersistedState,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub expected_total_length: Option<u64>,
    pub durable_offset: u64,
    pub temp_name: Option<String>,
}

impl PersistedDownload {
    /// Captures a job snapshot and the temp file name it refers to.
    pub fn capture(snapshot: &DownloadSnapshot, temp_name: Option<&str>) -> Self {
        Self {
            version: FORMAT_VERSION,
            state: to_persisted_state(snapshot.state),
            etag: snapshot.validators.etag.clone(),
            last_modified: snapshot.validators.last_modified.clone(),
            expected_total_length: snapshot.expected_total_length,
            durable_offset: snapshot.durable_offset,
            temp_name: temp_name.map(str::to_owned),
        }
    }

    /// Checks the record before it is written or after it is read.
    pub fn validate(&self) -> Result<(), PersistError> {
        if self.version != FORMAT_VERSION {
            return Err(PersistError::UnsupportedVersion(u64::from(self.version)));
        }
        let strings = [
            self.etag.as_deref(),
            self.last_modified.as_deref(),
            self.temp_name.as_deref(),
        ];
        if strings.into_iter().flatten().any(is_secret_like_value) {
            return Err(PersistError::SecretLikeField);
        }
        if let Some(name) = self.temp_name.as_deref()
            && !is_valid_temp_name(name)
        {
            return Err(PersistError::Inconsistent(
                "temp file name has an unexpected shape",
            ));
        }
        if self.durable_offset > 0 && self.temp_name.is_none() {
            return Err(PersistError::Inconsistent(
                "durable bytes without a temp file",
            ));
        }
        Ok(())
    }

    /// Rebuilds the job and its temp file name. The job's own transition rules reject
    /// impossible offsets and totals.
    pub fn into_job(self) -> Result<(DownloadJob, Option<String>), PersistError> {
        self.validate()?;
        let snapshot = DownloadSnapshot {
            state: from_persisted_state(self.state),
            validators: Validators {
                etag: self.etag,
                last_modified: self.last_modified,
            },
            expected_total_length: self.expected_total_length,
            durable_offset: self.durable_offset,
        };
        let job = DownloadJob::from_snapshot(snapshot).map_err(PersistError::Restore)?;
        Ok((job, self.temp_name))
    }
}

/// Writes `record` to `path` atomically: temp sibling, fsync, rename, directory fsync.
pub fn save(path: &Path, record: &PersistedDownload) -> Result<(), PersistError> {
    record.validate()?;
    let bytes = serde_json::to_vec(record).map_err(|_| PersistError::Malformed)?;
    if bytes.len() as u64 > MAX_RECORD_BYTES {
        return Err(PersistError::TooLarge);
    }
    let tmp = tmp_sibling(path);
    {
        let mut file = File::create(&tmp)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    sync_parent(path)?;
    Ok(())
}

/// Reads and validates the record at `path`.
pub fn load(path: &Path) -> Result<PersistedDownload, PersistError> {
    let mut bytes = Vec::new();
    File::open(path)?
        .take(MAX_RECORD_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_RECORD_BYTES {
        return Err(PersistError::TooLarge);
    }
    parse(&bytes)
}

/// Parses and validates record bytes. The version is checked before the full shape, so a
/// future format reports `UnsupportedVersion` rather than `Malformed`.
pub fn parse(bytes: &[u8]) -> Result<PersistedDownload, PersistError> {
    let value: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|_| PersistError::Malformed)?;
    let version = value
        .get("version")
        .and_then(serde_json::Value::as_u64)
        .ok_or(PersistError::Malformed)?;
    if version != u64::from(FORMAT_VERSION) {
        return Err(PersistError::UnsupportedVersion(version));
    }
    let record: PersistedDownload =
        serde_json::from_value(value).map_err(|_| PersistError::Malformed)?;
    record.validate()?;
    Ok(record)
}

/// Removes the record at `path`. A missing file is not an error.
pub fn clear(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn tmp_sibling(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".tmp");
    PathBuf::from(name)
}

#[cfg(unix)]
fn sync_parent(path: &Path) -> io::Result<()> {
    match path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        Some(parent) => File::open(parent)?.sync_all(),
        None => Ok(()),
    }
}

#[cfg(not(unix))]
fn sync_parent(_path: &Path) -> io::Result<()> {
    // std cannot fsync a directory on Windows. The rename replaces the target atomically.
    Ok(())
}

fn is_valid_temp_name(name: &str) -> bool {
    name.len() == TEMP_NAME_HEX_LEN + TEMP_NAME_SUFFIX.len()
        && name.ends_with(TEMP_NAME_SUFFIX)
        && name[..TEMP_NAME_HEX_LEN]
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
}

fn to_persisted_state(state: DownloadState) -> PersistedState {
    match state {
        DownloadState::Queued | DownloadState::Negotiating | DownloadState::Downloading => {
            PersistedState::Queued
        }
        DownloadState::Paused => PersistedState::Paused,
        DownloadState::Verifying => PersistedState::Verifying,
        DownloadState::Committed => PersistedState::Committed,
        DownloadState::Cancelled => PersistedState::Cancelled,
        DownloadState::Failed => PersistedState::Failed,
        DownloadState::NeedsAuth => PersistedState::NeedsAuth,
    }
}

fn from_persisted_state(state: PersistedState) -> DownloadState {
    match state {
        PersistedState::Queued => DownloadState::Queued,
        PersistedState::Paused => DownloadState::Paused,
        PersistedState::Verifying => DownloadState::Verifying,
        PersistedState::Committed => DownloadState::Committed,
        PersistedState::Cancelled => DownloadState::Cancelled,
        PersistedState::Failed => DownloadState::Failed,
        PersistedState::NeedsAuth => DownloadState::NeedsAuth,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NAME: &str = "0123456789abcdef.part";

    fn snapshot(state: DownloadState, offset: u64, total: Option<u64>) -> DownloadSnapshot {
        DownloadSnapshot {
            state,
            validators: Validators {
                etag: Some("\"v1\"".to_owned()),
                last_modified: Some("Wed, 01 Jan 2025 00:00:00 GMT".to_owned()),
            },
            expected_total_length: total,
            durable_offset: offset,
        }
    }

    fn record(state: DownloadState, offset: u64, total: Option<u64>) -> PersistedDownload {
        PersistedDownload::capture(&snapshot(state, offset, total), Some(NAME))
    }

    #[test]
    fn snapshot_round_trip_preserves_every_field() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("snapshot.json");
        let original = record(DownloadState::Paused, 524_288, Some(1_048_576));

        save(&path, &original).expect("save");
        let loaded = load(&path).expect("load");

        assert_eq!(loaded, original);
        let (job, temp) = loaded.into_job().expect("restore");
        assert_eq!(
            job.snapshot(),
            &snapshot(DownloadState::Paused, 524_288, Some(1_048_576))
        );
        assert_eq!(temp.as_deref(), Some(NAME));
    }

    #[test]
    fn save_replaces_previous_record_and_leaves_no_temp_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("snapshot.json");
        save(&path, &record(DownloadState::Paused, 1, Some(10))).expect("first");
        save(&path, &record(DownloadState::Paused, 2, Some(10))).expect("second");

        assert_eq!(load(&path).expect("load").durable_offset, 2);
        assert!(!tmp_sibling(&path).exists(), "tmp file is renamed away");
    }

    #[test]
    fn corrupt_bytes_are_refused() {
        assert!(matches!(
            parse(b"\x00\xffnot json"),
            Err(PersistError::Malformed)
        ));
    }

    #[test]
    fn truncated_json_is_refused() {
        let bytes = serde_json::to_vec(&record(DownloadState::Paused, 4, Some(10))).expect("json");
        let half = &bytes[..bytes.len() / 2];
        assert!(matches!(parse(half), Err(PersistError::Malformed)));
    }

    #[test]
    fn unknown_field_is_refused() {
        let mut value =
            serde_json::to_value(record(DownloadState::Paused, 4, Some(10))).expect("value");
        value["extra"] = serde_json::json!("x");
        let bytes = serde_json::to_vec(&value).expect("json");
        assert!(matches!(parse(&bytes), Err(PersistError::Malformed)));
    }

    #[test]
    fn future_version_is_refused_as_unsupported() {
        let mut value =
            serde_json::to_value(record(DownloadState::Paused, 4, Some(10))).expect("value");
        value["version"] = serde_json::json!(2);
        let bytes = serde_json::to_vec(&value).expect("json");
        assert!(matches!(
            parse(&bytes),
            Err(PersistError::UnsupportedVersion(2))
        ));
    }

    #[test]
    fn url_in_validator_is_refused_on_save_and_load() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("snapshot.json");
        let mut bad = record(DownloadState::Paused, 4, Some(10));
        bad.etag = Some("\"https://srv/Items/abc\"".to_owned());

        assert!(matches!(
            save(&path, &bad),
            Err(PersistError::SecretLikeField)
        ));
        assert!(!path.exists(), "nothing is written for a refused record");

        let bytes = serde_json::to_vec(&bad).expect("json");
        assert!(matches!(parse(&bytes), Err(PersistError::SecretLikeField)));
    }

    #[test]
    fn temp_name_with_path_text_is_refused() {
        let mut bad = record(DownloadState::Paused, 4, Some(10));
        bad.temp_name = Some("../../etc/passwd".to_owned());
        assert!(matches!(bad.validate(), Err(PersistError::Inconsistent(_))));
    }

    #[test]
    fn durable_bytes_without_temp_file_are_refused() {
        let mut bad = record(DownloadState::Paused, 4, Some(10));
        bad.temp_name = None;
        assert!(matches!(bad.validate(), Err(PersistError::Inconsistent(_))));
    }

    #[test]
    fn in_flight_states_are_saved_as_queued() {
        assert_eq!(
            record(DownloadState::Downloading, 4, Some(10)).state,
            PersistedState::Queued
        );
        assert_eq!(
            record(DownloadState::Negotiating, 0, None).state,
            PersistedState::Queued
        );
    }

    #[test]
    fn impossible_offset_is_refused_on_restore() {
        let bad = record(DownloadState::Paused, 11, Some(10));
        assert!(matches!(bad.into_job(), Err(PersistError::Restore(_))));
    }

    #[test]
    fn record_holds_no_title_id_or_url_text() {
        let text =
            serde_json::to_string(&record(DownloadState::Paused, 4, Some(10))).expect("json");
        assert!(!text.contains("http"));
        assert!(!text.contains("Items"));
        assert!(!text.contains("title"));
    }

    #[test]
    fn clear_of_missing_record_is_not_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(clear(&dir.path().join("absent.json")).is_ok());
    }
}
