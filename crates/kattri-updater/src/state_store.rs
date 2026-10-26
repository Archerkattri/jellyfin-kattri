//! Bounded durable storage for per-channel manifest replay state.
//!
//! The installed version is snapshot metadata; valid replay floors survive version changes.
//! Malformed or unreadable snapshots fail closed.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::manifest::Channel;
use crate::state::{Accepted, InstallState};

/// Maximum serialized replay-state file size.
pub const MAX_REPLAY_STATE_BYTES: usize = 4 * 1024;
const FILE_VERSION: u32 = 1;
const MAX_CHANNELS: usize = 2;
const MAX_TEMP_ATTEMPTS: usize = 64;
static TEMP_ID: AtomicU64 = AtomicU64::new(0);

/// Errors loading or saving durable replay state.
#[derive(Debug, Error)]
pub enum ReplayStateStoreError {
    #[error("could not {operation} replay state: {source}")]
    Io {
        operation: &'static str,
        #[source]
        source: io::Error,
    },
    #[error("replay state is corrupt or has an unsupported version")]
    Corrupt,
    #[error("replay state exceeds the {limit}-byte limit")]
    TooLarge { limit: usize },
    #[error("another updater job holds the replay-state lock")]
    Busy,
    #[error("could not reserve a replay-state temporary file after repeated name collisions")]
    TemporaryCollision,
    #[error("installed version is invalid: {0}")]
    InstalledVersion(#[from] crate::UpdateError),
}

/// On-disk replay state with the installed version retained as metadata.
///
/// Callers must hold [`FsReplayStateStore::try_lock`] from before `load` through manifest
/// acceptance, `save`, and the associated install operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsReplayStateStore {
    path: PathBuf,
}

/// Holds the store's OS-backed exclusive lock until this guard is dropped.
#[derive(Debug)]
pub struct ReplayStateLockGuard {
    _file: File,
}

impl FsReplayStateStore {
    /// Create a store for `path`. Its parent directory must already exist.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// The state file path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Try to acquire the per-store exclusive lock without waiting.
    ///
    /// The sibling lock file is persistent; the operating system releases the lock when the
    /// guard's file handle closes, including when the owning process exits unexpectedly.
    pub fn try_lock(&self) -> Result<ReplayStateLockGuard, ReplayStateStoreError> {
        let lock_path = self.lock_path();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(lock_path)
            .map_err(|source| io_error("open lock file", source))?;
        match file.try_lock() {
            Ok(()) => Ok(ReplayStateLockGuard { _file: file }),
            Err(TryLockError::WouldBlock) => Err(ReplayStateStoreError::Busy),
            Err(TryLockError::Error(source)) => Err(io_error("lock replay state", source)),
        }
    }

    /// Load replay floors for `installed_version`.
    ///
    /// A missing file is fresh. A valid file's floors are retained across installed-version
    /// changes, and the returned state is associated with the current installed version.
    /// Corrupt state is an error so callers cannot accidentally bypass replay protection.
    /// Hold the store lock across this call and the subsequent acceptance/install operation.
    pub fn load(&self, installed_version: &str) -> Result<InstallState, ReplayStateStoreError> {
        let file = match File::open(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(InstallState::new(installed_version)?);
            }
            Err(source) => return Err(io_error("open", source)),
        };
        let mut raw = Vec::with_capacity(MAX_REPLAY_STATE_BYTES + 1);
        file.take((MAX_REPLAY_STATE_BYTES + 1) as u64)
            .read_to_end(&mut raw)
            .map_err(|source| io_error("read", source))?;
        if raw.len() > MAX_REPLAY_STATE_BYTES {
            return Err(ReplayStateStoreError::TooLarge {
                limit: MAX_REPLAY_STATE_BYTES,
            });
        }

        let snapshot: ReplayStateFile =
            serde_json::from_slice(&raw).map_err(|_| ReplayStateStoreError::Corrupt)?;
        if snapshot.version != FILE_VERSION || snapshot.accepted.len() > MAX_CHANNELS {
            return Err(ReplayStateStoreError::Corrupt);
        }
        crate::version::parse_version(&snapshot.installed_version)
            .map_err(|_| ReplayStateStoreError::Corrupt)?;
        let mut accepted = HashMap::with_capacity(snapshot.accepted.len());
        for entry in snapshot.accepted {
            let digest = decode_digest(&entry.payload_sha256)?;
            if accepted
                .insert(
                    entry.channel,
                    Accepted {
                        sequence: entry.sequence,
                        payload_sha256: digest,
                    },
                )
                .is_some()
            {
                return Err(ReplayStateStoreError::Corrupt);
            }
        }
        InstallState::from_accepted(installed_version, accepted).map_err(Into::into)
    }

    /// Persist a snapshot through an exclusively created sibling temporary file and atomic rename.
    /// The caller must hold this store's lock.
    pub fn save(&self, state: &InstallState) -> Result<(), ReplayStateStoreError> {
        let mut accepted: Vec<_> = state
            .accepted_entries()
            .iter()
            .map(|(channel, accepted)| AcceptedFile {
                channel: *channel,
                sequence: accepted.sequence,
                payload_sha256: encode_digest(&accepted.payload_sha256),
            })
            .collect();
        accepted.sort_by_key(|entry| match entry.channel {
            Channel::Stable => 0_u8,
            Channel::Nightly => 1_u8,
        });
        let snapshot = ReplayStateFile {
            version: FILE_VERSION,
            installed_version: state.installed_version().to_string(),
            accepted,
        };
        if snapshot.accepted.len() > MAX_CHANNELS {
            return Err(ReplayStateStoreError::Corrupt);
        }
        let raw = serde_json::to_vec(&snapshot).map_err(|_| ReplayStateStoreError::Corrupt)?;
        if raw.len() > MAX_REPLAY_STATE_BYTES {
            return Err(ReplayStateStoreError::TooLarge {
                limit: MAX_REPLAY_STATE_BYTES,
            });
        }

        let mut temp_path = None;
        let mut created_temp = false;
        let result = (|| {
            let (path, mut temp) = create_temporary(|| self.temp_path())?;
            temp_path = Some(path.clone());
            created_temp = true;
            temp.write_all(&raw)
                .map_err(|source| io_error("write temporary", source))?;
            temp.sync_all()
                .map_err(|source| io_error("sync temporary", source))?;
            drop(temp);
            fs::rename(&path, &self.path).map_err(|source| io_error("replace", source))?;
            temp_path = None;
            sync_parent(&self.path)
        })();
        if result.is_err()
            && created_temp
            && let Some(temp_path) = temp_path
        {
            let _ = fs::remove_file(temp_path);
        }
        result
    }

    fn temp_path(&self) -> PathBuf {
        let id = TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let mut name = self
            .path
            .file_name()
            .unwrap_or_else(|| std::ffi::OsStr::new("replay-state"))
            .to_os_string();
        name.push(format!(".{}.{}.tmp", std::process::id(), id));
        self.path.with_file_name(name)
    }

    fn lock_path(&self) -> PathBuf {
        let mut name = self
            .path
            .file_name()
            .unwrap_or_else(|| std::ffi::OsStr::new("replay-state"))
            .to_os_string();
        name.push(".lock");
        self.path.with_file_name(name)
    }
}

fn create_temporary(
    mut next_path: impl FnMut() -> PathBuf,
) -> Result<(PathBuf, File), ReplayStateStoreError> {
    for _ in 0..MAX_TEMP_ATTEMPTS {
        let path = next_path();
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(source) => return Err(io_error("create temporary", source)),
        }
    }
    Err(ReplayStateStoreError::TemporaryCollision)
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplayStateFile {
    version: u32,
    installed_version: String,
    accepted: Vec<AcceptedFile>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AcceptedFile {
    channel: Channel,
    sequence: u64,
    payload_sha256: String,
}

fn decode_digest(hex: &str) -> Result<[u8; 32], ReplayStateStoreError> {
    if hex.len() != 64
        || !hex
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(ReplayStateStoreError::Corrupt);
    }
    let mut digest = [0_u8; 32];
    for (index, pair) in hex.as_bytes().as_chunks::<2>().0.iter().enumerate() {
        let high = nibble(pair[0]).ok_or(ReplayStateStoreError::Corrupt)?;
        let low = nibble(pair[1]).ok_or(ReplayStateStoreError::Corrupt)?;
        digest[index] = (high << 4) | low;
    }
    Ok(digest)
}

fn nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

fn encode_digest(digest: &[u8; 32]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(64);
    for byte in digest {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    encoded
}

fn io_error(operation: &'static str, source: io::Error) -> ReplayStateStoreError {
    ReplayStateStoreError::Io { operation, source }
}

#[cfg(unix)]
fn sync_parent(path: &Path) -> Result<(), ReplayStateStoreError> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|source| io_error("sync parent directory", source))
}

#[cfg(not(unix))]
fn sync_parent(_path: &Path) -> Result<(), ReplayStateStoreError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD;
    use ed25519_dalek::{Signer, SigningKey};
    use serde_json::{Value, json};
    use sha2::{Digest, Sha256};

    use crate::{KeyState, Keyring, UpdateError, open_envelope};

    const INSTALLED: &str = "2.0.0";

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            let id = TEMP_ID.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("kattri-replay-test-{}-{id}", std::process::id()));
            fs::create_dir(&path).expect("create test directory");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn store(dir: &TestDir) -> FsReplayStateStore {
        FsReplayStateStore::new(dir.path().join("replay.json"))
    }

    fn test_key() -> SigningKey {
        let seed: [u8; 32] = Sha256::digest(b"kattri updater TEST KEY; never a release key").into();
        SigningKey::from_bytes(&seed)
    }

    fn envelope(key: &SigningKey, sequence: u64) -> String {
        envelope_at(key, sequence, "2026-10-15")
    }

    fn envelope_at(key: &SigningKey, sequence: u64, published: &str) -> String {
        envelope_for(key, sequence, "stable", published)
    }

    fn envelope_for(key: &SigningKey, sequence: u64, channel: &str, published: &str) -> String {
        let manifest = json!({
            "schema_version": 1,
            "channel": channel,
            "version": "2.1.0",
            "sequence": sequence,
            "published": published,
            "artifacts": [{
                "platform": "windows", "arch": "x64", "format": "msi",
                "url": "https://updates.example.org/test.msi",
                "sha256": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
                "size_bytes": 1
            }]
        });
        let payload = manifest.to_string();
        json!({
            "envelope_version": 1,
            "alg": "ed25519",
            "kid": "test-key",
            "payload": STANDARD.encode(payload.as_bytes()),
            "sig": STANDARD.encode(key.sign(payload.as_bytes()).to_bytes())
        })
        .to_string()
    }

    fn keyring(key: &SigningKey) -> Keyring {
        Keyring::new(vec![(
            "test-key".to_owned(),
            key.verifying_key().to_bytes(),
            KeyState::Active,
        )])
        .expect("valid test keyring")
    }

    #[test]
    fn restart_preserves_highest_sequence_and_rejects_replay() {
        let dir = TestDir::new();
        let store = store(&dir);
        let key = test_key();
        let ring = keyring(&key);
        let mut state = InstallState::new(INSTALLED).expect("valid installed version");
        let accepted = open_envelope(&ring, &envelope(&key, 10), Channel::Stable, &state)
            .expect("initial manifest accepted");
        state.record(&accepted);
        store.save(&state).expect("save replay state");

        let restarted = store.load(INSTALLED).expect("reload replay state");
        assert_eq!(
            restarted.accepted(Channel::Stable),
            state.accepted(Channel::Stable)
        );
        assert!(matches!(
            open_envelope(&ring, &envelope(&key, 9), Channel::Stable, &restarted),
            Err(UpdateError::Replay {
                sequence: 9,
                last_accepted: 10
            })
        ));
    }

    #[test]
    fn equal_sequence_requires_identical_payload_after_restart() {
        let dir = TestDir::new();
        let store = store(&dir);
        let key = test_key();
        let ring = keyring(&key);
        let mut state = InstallState::new(INSTALLED).expect("valid installed version");
        let accepted = open_envelope(&ring, &envelope(&key, 10), Channel::Stable, &state)
            .expect("initial manifest accepted");
        state.record(&accepted);
        store.save(&state).expect("save replay state");
        let restarted = store.load(INSTALLED).expect("reload replay state");
        assert!(open_envelope(&ring, &envelope(&key, 10), Channel::Stable, &restarted).is_ok());

        let mut different: Value =
            serde_json::from_str(&envelope(&key, 10)).expect("valid envelope");
        let payload = STANDARD
            .decode(different["payload"].as_str().expect("payload"))
            .expect("base64");
        let changed = String::from_utf8(payload)
            .expect("utf8")
            .replace("2.1.0", "2.1.1");
        different["payload"] = Value::String(STANDARD.encode(changed.as_bytes()));
        different["sig"] = Value::String(STANDARD.encode(key.sign(changed.as_bytes()).to_bytes()));
        assert!(matches!(
            open_envelope(&ring, &different.to_string(), Channel::Stable, &restarted),
            Err(UpdateError::SequenceConflict { sequence: 10 })
        ));
    }

    #[test]
    fn corrupt_state_fails_closed() {
        let dir = TestDir::new();
        let store = store(&dir);
        fs::write(
            store.path(),
            br#"{"version":99,"installed_version":"2.0.0","accepted":[]}"#,
        )
        .expect("write corrupt file");
        assert!(matches!(
            store.load(INSTALLED),
            Err(ReplayStateStoreError::Corrupt)
        ));
    }

    #[test]
    fn oversized_state_is_rejected_before_full_read() {
        let dir = TestDir::new();
        let store = store(&dir);
        fs::write(store.path(), vec![b' '; MAX_REPLAY_STATE_BYTES + 1])
            .expect("write oversized file");
        assert!(matches!(
            store.load(INSTALLED),
            Err(ReplayStateStoreError::TooLarge {
                limit: MAX_REPLAY_STATE_BYTES
            })
        ));
    }

    #[test]
    fn installed_version_change_preserves_replay_floors_for_rechecks() {
        let dir = TestDir::new();
        let store = store(&dir);
        let mut state = InstallState::new(INSTALLED).expect("valid installed version");
        let key = test_key();
        let ring = keyring(&key);
        let lower = open_envelope(&ring, &envelope(&key, 9), Channel::Stable, &state)
            .expect("lower candidate is valid before a floor is recorded");
        let same_sequence_conflict = open_envelope(
            &ring,
            &envelope_at(&key, 10, "2026-10-16"),
            Channel::Stable,
            &state,
        )
        .expect("conflicting candidate is valid before a floor is recorded");
        let accepted = open_envelope(&ring, &envelope(&key, 10), Channel::Stable, &state)
            .expect("accepted manifest");
        state.record(&accepted);
        store.save(&state).expect("save replay state");
        let next = store.load("2.1.0").expect("new installed version");
        assert_eq!(next.accepted(Channel::Stable).unwrap().sequence, 10);
        assert_eq!(next.installed_version().to_string(), "2.1.0");
        assert!(matches!(
            next.check_verified(&lower),
            Err(UpdateError::Replay {
                sequence: 9,
                last_accepted: 10
            })
        ));
        assert!(matches!(
            next.check_verified(&same_sequence_conflict),
            Err(UpdateError::SequenceConflict { sequence: 10 })
        ));
    }

    #[test]
    fn saved_channels_are_sorted_deterministically() {
        let dir = TestDir::new();
        let store = store(&dir);
        let key = test_key();
        let ring = keyring(&key);
        let mut state = InstallState::new(INSTALLED).expect("valid installed version");
        let nightly = open_envelope(
            &ring,
            &envelope_for(&key, 3, "nightly", "2026-10-15"),
            Channel::Nightly,
            &state,
        )
        .expect("nightly manifest accepted");
        let stable = open_envelope(
            &ring,
            &envelope_for(&key, 7, "stable", "2026-10-15"),
            Channel::Stable,
            &state,
        )
        .expect("stable manifest accepted");
        state.record(&nightly);
        state.record(&stable);

        store.save(&state).expect("save replay state");
        let first = fs::read(store.path()).expect("read saved state");
        store.save(&state).expect("save replay state again");
        let second = fs::read(store.path()).expect("read saved state again");
        assert_eq!(first, second);

        let saved: Value = serde_json::from_slice(&first).expect("valid saved JSON");
        assert_eq!(saved["accepted"][0]["channel"], "stable");
        assert_eq!(saved["accepted"][1]["channel"], "nightly");
    }

    #[test]
    fn save_failure_does_not_replace_existing_state_or_leave_temp_file() {
        let dir = TestDir::new();
        let target = dir.path().join("replay.json");
        fs::create_dir(&target).expect("create target directory");
        let store = FsReplayStateStore::new(&target);
        let state = InstallState::new(INSTALLED).expect("valid installed version");
        assert!(matches!(
            store.save(&state),
            Err(ReplayStateStoreError::Io { .. })
        ));
        assert!(target.is_dir());
        let temps: Vec<_> = fs::read_dir(dir.path())
            .expect("read test directory")
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(temps.is_empty());
    }

    #[test]
    fn temporary_creation_skips_a_stale_file_without_modifying_it() {
        let dir = TestDir::new();
        let store = store(&dir);
        let stale_path = store.path().with_file_name("stale.tmp");
        let fresh_path = store.path().with_file_name("fresh.tmp");
        fs::write(&stale_path, b"partial snapshot").expect("create stale temp file");
        let mut attempts = 0;
        let (path, file) = create_temporary(|| {
            attempts += 1;
            if attempts == 1 {
                stale_path.clone()
            } else {
                fresh_path.clone()
            }
        })
        .expect("skip stale file");
        assert_eq!(attempts, 2);
        assert_eq!(path, fresh_path);
        assert_eq!(fs::read(&stale_path).unwrap(), b"partial snapshot");
        drop(file);
        assert!(matches!(
            create_temporary(|| stale_path.clone()),
            Err(ReplayStateStoreError::TemporaryCollision)
        ));
    }

    #[test]
    fn replay_store_lock_is_exclusive_and_released_with_guard() {
        let dir = TestDir::new();
        let store = store(&dir);
        let guard = store.try_lock().expect("acquire lock");
        assert!(matches!(store.try_lock(), Err(ReplayStateStoreError::Busy)));
        drop(guard);
        assert!(store.try_lock().is_ok());
    }
}
