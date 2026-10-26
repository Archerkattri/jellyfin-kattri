//! M4-20 tests for the PIN attempt delay and its persisted state.
//!
//! The clock is a plain `u64` of whole seconds that each test advances by hand, so no test sleeps
//! and no test reads the real time.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use kattri_parental::{
    ATTEMPT_FILE_NAME, ATTEMPT_FILE_VERSION, AttemptFileError, AttemptState, HashParams, Pin,
    PinLock, StoredPin, Unlock, backoff_secs, decode_attempts, delete_attempts, encode_attempts,
    load_attempts, save_attempts,
};

/// A temp directory removed when the test ends.
struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let path = std::env::temp_dir().join(format!(
            "kattri-parental-{label}-{}-{nanos}-{n}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("create temp dir");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn stored(digits: &str) -> StoredPin {
    let params = HashParams::new(8, 1, 1).expect("test params");
    let pin = Pin::parse(digits).expect("test pin");
    StoredPin::create(&pin, params, [5u8; 16]).expect("stored pin")
}

fn locked_lock(digits: &str) -> PinLock {
    PinLock::new(Some(stored(digits)))
}

#[test]
fn backoff_is_one_second_per_failure_capped_at_thirty() {
    assert_eq!(backoff_secs(0), 0);
    assert_eq!(backoff_secs(1), 1);
    assert_eq!(backoff_secs(2), 2);
    assert_eq!(backoff_secs(29), 29);
    assert_eq!(backoff_secs(30), 30);
    assert_eq!(backoff_secs(31), 30);
    assert_eq!(backoff_secs(u32::MAX), 30);
}

#[test]
fn encoded_document_is_versioned_and_exact() {
    assert_eq!(ATTEMPT_FILE_VERSION, 1);
    assert_eq!(ATTEMPT_FILE_NAME, "parental-lock.json");
    let with_delay = AttemptState {
        failures: 3,
        locked_until: Some(1_700_000_042),
    };
    assert_eq!(
        encode_attempts(with_delay),
        "{\"version\":1,\"failures\":3,\"locked_until\":1700000042}\n"
    );
    let idle = AttemptState::default();
    assert_eq!(
        encode_attempts(idle),
        "{\"version\":1,\"failures\":0,\"locked_until\":null}\n"
    );
}

#[test]
fn document_round_trips() {
    let state = AttemptState {
        failures: 7,
        locked_until: Some(99),
    };
    assert_eq!(decode_attempts(&encode_attempts(state)), Ok(state));
}

#[test]
fn unknown_version_is_refused_with_its_number() {
    let text = "{\"version\":2,\"failures\":0,\"locked_until\":null}";
    assert_eq!(
        decode_attempts(text),
        Err(AttemptFileError::UnsupportedVersion { found: 2 })
    );
}

#[test]
fn damaged_documents_are_refused() {
    for text in [
        "",
        "not json",
        "{\"version\":1,\"failures\":0}",
        "{\"version\":1,\"failures\":0,\"locked_until\":null,\"extra\":true}",
        "{\"version\":1,\"failures\":-1,\"locked_until\":null}",
    ] {
        assert_eq!(
            decode_attempts(text),
            Err(AttemptFileError::Malformed),
            "{text:?}"
        );
    }
}

#[test]
fn missing_file_means_no_failures() {
    let root = TempDir::new("missing");
    let state = load_attempts(&root.path().join(ATTEMPT_FILE_NAME)).expect("load");
    assert_eq!(state, AttemptState::default());
}

#[test]
fn save_then_load_round_trips_and_leaves_no_temp_file() {
    let root = TempDir::new("save");
    let path = root.path().join(ATTEMPT_FILE_NAME);
    let state = AttemptState {
        failures: 4,
        locked_until: Some(1_000),
    };
    save_attempts(&path, state).expect("save");
    assert_eq!(load_attempts(&path), Ok(state));
    assert!(fs::read_dir(root.path()).expect("read dir").all(|entry| {
        !entry
            .expect("dir entry")
            .file_name()
            .to_string_lossy()
            .starts_with(&format!("{ATTEMPT_FILE_NAME}.tmp-"))
    }));

    save_attempts(&path, AttemptState::default()).expect("overwrite");
    assert_eq!(load_attempts(&path), Ok(AttemptState::default()));
}

#[test]
fn damaged_file_is_an_error_and_is_not_replaced_by_defaults() {
    let root = TempDir::new("damaged");
    let path = root.path().join(ATTEMPT_FILE_NAME);
    fs::write(&path, "{\"version\":1,\"failures\":").expect("write");
    assert_eq!(load_attempts(&path), Err(AttemptFileError::Malformed));
    // Loading does not rewrite the file.
    assert_eq!(
        fs::read_to_string(&path).expect("read"),
        "{\"version\":1,\"failures\":"
    );
}

#[test]
fn delete_removes_the_file_and_tolerates_a_missing_one() {
    let root = TempDir::new("delete");
    let path = root.path().join(ATTEMPT_FILE_NAME);
    assert_eq!(delete_attempts(&path), Ok(()));
    save_attempts(
        &path,
        AttemptState {
            failures: 2,
            locked_until: None,
        },
    )
    .expect("save");
    assert_eq!(delete_attempts(&path), Ok(()));
    assert!(!path.exists());
    assert_eq!(load_attempts(&path), Ok(AttemptState::default()));
}

#[test]
fn delay_survives_a_restart_with_a_fake_clock() {
    let root = TempDir::new("restart");
    let path = root.path().join(ATTEMPT_FILE_NAME);

    // Session one: a wrong PIN at t=1000 starts a one second delay.
    {
        let mut first = locked_lock("4821");
        assert_eq!(
            first.try_unlock("0000", 1_000),
            Ok(Unlock::WrongPin {
                failures: 1,
                retry_after_secs: 1
            })
        );
        save_attempts(&path, first.attempt_state()).expect("persist");
    }
    // Session two starts at t=1000 and still refuses the correct PIN.
    let state = load_attempts(&path).expect("load");
    let mut second = PinLock::restore(Some(stored("4821")), state);
    assert_eq!(
        second.try_unlock("4821", 1_000),
        Ok(Unlock::LockedOut {
            retry_after_secs: 1
        })
    );

    // At t=1001 the delay has ended. A wrong PIN is now the second failure, with a two second delay.
    assert_eq!(
        second.try_unlock("0000", 1_001),
        Ok(Unlock::WrongPin {
            failures: 2,
            retry_after_secs: 2
        })
    );
}

#[test]
fn success_after_restart_persists_a_cleared_state() {
    let root = TempDir::new("success");
    let path = root.path().join(ATTEMPT_FILE_NAME);
    let mut lock = PinLock::restore(
        Some(stored("4821")),
        AttemptState {
            failures: 6,
            locked_until: Some(50),
        },
    );
    assert_eq!(lock.try_unlock("4821", 50), Ok(Unlock::Unlocked));
    save_attempts(&path, lock.attempt_state()).expect("persist");
    assert_eq!(load_attempts(&path), Ok(AttemptState::default()));
}

#[test]
fn restoring_an_unprotected_profile_ignores_stored_failures() {
    let lock = PinLock::restore(
        None,
        AttemptState {
            failures: 9,
            locked_until: Some(u64::MAX),
        },
    );
    assert!(!lock.is_protected());
    assert_eq!(lock.failures(), 0);
    assert_eq!(lock.attempt_state(), AttemptState::default());
}

#[test]
fn restored_expiry_in_the_past_does_not_delay() {
    let mut lock = PinLock::restore(
        Some(stored("4821")),
        AttemptState {
            failures: 3,
            locked_until: Some(10),
        },
    );
    assert_eq!(lock.try_unlock("4821", 11), Ok(Unlock::Unlocked));
}
