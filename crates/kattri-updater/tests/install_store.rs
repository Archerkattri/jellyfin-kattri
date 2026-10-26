//! M4-16 tests for the on-disk install store (`FsInstallStore`).
//!
//! Each test works in its own temp directory and removes it afterwards. The tests use only
//! `std::fs`, so they run the same way on Windows, macOS and Linux. Keys are derived at
//! runtime from labels in this file. They are TEST KEYS, never release keys.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use ed25519_dalek::{Signer, SigningKey};
use kattri_updater::apply::{
    ApplyError, InstallStore, Journal, LaunchOutcome, Pending, Slot, apply_verified, mark_healthy,
    on_launch, rollback, sha256_hex,
};
use kattri_updater::{
    Channel, FsInstallStore, InstallState, KeyState, Keyring, VerifiedManifest, open_envelope,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const OLD_VERSION: &str = "2.0.0";
const NEW_VERSION: &str = "2.1.0";
const OLD_BYTES: &[u8] = b"fake 2.0.0 build bytes for the on-disk store tests";
const NEW_BYTES: &[u8] = b"fake 2.1.0 build bytes for the on-disk store tests";

/// A temp directory that is removed when the test ends.
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
            "kattri-updater-{label}-{}-{nanos}-{n}",
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

/// A test-only signing key derived from a label. Never a release key.
fn test_key(label: &str) -> SigningKey {
    let seed: [u8; 32] =
        Sha256::digest(format!("kattri-updater fs-store TEST KEY: {label}").as_bytes()).into();
    SigningKey::from_bytes(&seed)
}

fn manifest_value(version: &str, sequence: u64, artifact: &[u8]) -> Value {
    json!({
        "schema_version": 1,
        "channel": "stable",
        "version": version,
        "sequence": sequence,
        "published": "2026-10-16",
        "minimum_version": "1.0.0",
        "artifacts": [{
            "platform": "windows",
            "arch": "x64",
            "format": "msi",
            "url": "https://updates.example.org/dl/kattri-x64.msi",
            "sha256": sha256_hex(artifact),
            "size_bytes": artifact.len()
        }]
    })
}

/// Sign a manifest for `version` and open it against an install state at `OLD_VERSION`.
fn verified(version: &str, sequence: u64, artifact: &[u8]) -> VerifiedManifest {
    let key = test_key("fs-signer");
    let ring = Keyring::new(vec![(
        "test-fs-2026a".to_owned(),
        key.verifying_key().to_bytes(),
        KeyState::Active,
    )])
    .expect("test keyring");
    let payload = manifest_value(version, sequence, artifact).to_string();
    let sig = key.sign(payload.as_bytes()).to_bytes();
    let envelope = json!({
        "envelope_version": 1,
        "alg": "ed25519",
        "kid": "test-fs-2026a",
        "payload": STANDARD.encode(payload.as_bytes()),
        "sig": STANDARD.encode(sig),
    })
    .to_string();
    open_envelope(
        &ring,
        &envelope,
        Channel::Stable,
        &InstallState::new(OLD_VERSION).expect("state"),
    )
    .expect("verified manifest")
}

fn slot(version: &str, bytes: &[u8]) -> Slot {
    Slot {
        version: version.to_owned(),
        bytes: bytes.to_vec(),
    }
}

/// Open a store in `dir` that already has `OLD_VERSION` installed, as a first install would.
fn store_with_old_build(dir: &Path) -> FsInstallStore {
    let mut store = FsInstallStore::open(dir).expect("open store");
    store
        .stage(slot(OLD_VERSION, OLD_BYTES))
        .expect("stage old");
    store.commit_staged().expect("commit old");
    store
}

#[test]
fn open_creates_the_directory_and_reports_an_empty_store() {
    let root = TempDir::new("empty");
    let dir = root.path().join("nested").join("install");
    let store = FsInstallStore::open(&dir).expect("open");
    assert!(dir.is_dir());
    assert_eq!(store.dir(), dir.as_path());
    assert_eq!(store.installed().expect("read"), None);
    assert_eq!(store.previous().expect("read"), None);
    assert_eq!(store.staged().expect("read"), None);
    assert_eq!(store.journal().expect("read"), Journal::default());
}

#[test]
fn apply_survives_a_restart_and_keeps_the_previous_build() {
    let root = TempDir::new("apply");
    let mut store = store_with_old_build(root.path());
    apply_verified(
        &mut store,
        &verified(NEW_VERSION, 7, NEW_BYTES),
        "windows",
        "x64",
        NEW_BYTES,
    )
    .expect("apply");
    drop(store);

    let reopened = FsInstallStore::open(root.path()).expect("reopen");
    assert_eq!(
        reopened.installed().expect("read"),
        Some(slot(NEW_VERSION, NEW_BYTES))
    );
    assert_eq!(
        reopened.previous().expect("read"),
        Some(slot(OLD_VERSION, OLD_BYTES))
    );
    assert_eq!(reopened.staged().expect("read"), None);
    let journal = reopened.journal().expect("journal");
    assert_eq!(
        journal.pending,
        Some(Pending {
            target: NEW_VERSION.to_owned(),
            previous: OLD_VERSION.to_owned(),
            previous_sha256: sha256_hex(OLD_BYTES),
            failed_launches: 0,
        })
    );
}

#[test]
fn healthy_marker_after_restart_drops_the_previous_build() {
    let root = TempDir::new("healthy");
    let mut store = store_with_old_build(root.path());
    apply_verified(
        &mut store,
        &verified(NEW_VERSION, 7, NEW_BYTES),
        "windows",
        "x64",
        NEW_BYTES,
    )
    .expect("apply");
    assert_eq!(
        on_launch(&mut store, NEW_VERSION).expect("launch"),
        LaunchOutcome::HealthCheckPending {
            target: NEW_VERSION.to_owned(),
            failed_launches: 1,
        }
    );
    assert!(mark_healthy(&mut store, NEW_VERSION).expect("healthy"));
    drop(store);

    let reopened = FsInstallStore::open(root.path()).expect("reopen");
    assert_eq!(reopened.previous().expect("read"), None);
    let journal = reopened.journal().expect("journal");
    assert_eq!(journal.pending, None);
    assert_eq!(journal.healthy.as_deref(), Some(NEW_VERSION));
}

#[test]
fn rollback_after_two_failed_launches_restores_the_previous_build() {
    let root = TempDir::new("rollback");
    let mut store = store_with_old_build(root.path());
    apply_verified(
        &mut store,
        &verified(NEW_VERSION, 7, NEW_BYTES),
        "windows",
        "x64",
        NEW_BYTES,
    )
    .expect("apply");
    on_launch(&mut store, NEW_VERSION).expect("first launch");
    assert!(matches!(
        on_launch(&mut store, NEW_VERSION).expect("second launch"),
        LaunchOutcome::RollbackOffered { .. }
    ));
    rollback(&mut store).expect("rollback");
    drop(store);

    let reopened = FsInstallStore::open(root.path()).expect("reopen");
    assert_eq!(
        reopened.installed().expect("read"),
        Some(slot(OLD_VERSION, OLD_BYTES))
    );
    assert_eq!(reopened.previous().expect("read"), None);
    let journal = reopened.journal().expect("journal");
    assert_eq!(journal.pending, None);
    assert_eq!(journal.failed_targets, vec![NEW_VERSION.to_owned()]);
}

#[test]
fn crash_after_staging_leaves_the_installed_build_unchanged() {
    let root = TempDir::new("crash-stage");
    let mut store = store_with_old_build(root.path());
    // Stop after staging, as if the process died before the switch.
    store.stage(slot(NEW_VERSION, NEW_BYTES)).expect("stage");
    drop(store);

    let mut reopened = FsInstallStore::open(root.path()).expect("reopen");
    assert_eq!(
        reopened.installed().expect("read"),
        Some(slot(OLD_VERSION, OLD_BYTES))
    );
    assert_eq!(reopened.previous().expect("read"), None);
    assert_eq!(
        reopened.staged().expect("read"),
        Some(slot(NEW_VERSION, NEW_BYTES))
    );
    assert_eq!(reopened.journal().expect("journal").pending, None);

    // The next update replaces the leftover staged bytes and completes normally.
    apply_verified(
        &mut reopened,
        &verified(NEW_VERSION, 7, NEW_BYTES),
        "windows",
        "x64",
        NEW_BYTES,
    )
    .expect("apply after crash");
    assert_eq!(
        reopened.installed().expect("read"),
        Some(slot(NEW_VERSION, NEW_BYTES))
    );
    assert_eq!(reopened.staged().expect("read"), None);
}

#[test]
fn crash_after_journal_and_staging_is_abandoned_on_next_launch() {
    let root = TempDir::new("crash-journal");
    let mut store = store_with_old_build(root.path());
    store.stage(slot(NEW_VERSION, NEW_BYTES)).expect("stage");
    store
        .set_journal(Journal {
            pending: Some(Pending {
                target: NEW_VERSION.to_owned(),
                previous: OLD_VERSION.to_owned(),
                previous_sha256: sha256_hex(OLD_BYTES),
                failed_launches: 0,
            }),
            ..Journal::default()
        })
        .expect("journal");
    drop(store);

    let mut reopened = FsInstallStore::open(root.path()).expect("reopen");
    assert_eq!(
        on_launch(&mut reopened, OLD_VERSION).expect("launch"),
        LaunchOutcome::AbandonedUpdate {
            target: NEW_VERSION.to_owned()
        }
    );
    assert_eq!(
        reopened.installed().expect("read"),
        Some(slot(OLD_VERSION, OLD_BYTES))
    );
    assert_eq!(reopened.journal().expect("journal").pending, None);
}

#[test]
fn commit_without_staged_bytes_fails_and_changes_nothing() {
    let root = TempDir::new("no-stage");
    let mut store = store_with_old_build(root.path());
    assert!(store.commit_staged().is_err());
    assert_eq!(
        store.installed().expect("read"),
        Some(slot(OLD_VERSION, OLD_BYTES))
    );
    assert_eq!(store.previous().expect("read"), None);
}

#[test]
fn tampered_artifact_is_refused_before_anything_is_written() {
    let root = TempDir::new("tamper");
    let mut store = store_with_old_build(root.path());
    let mut bad = NEW_BYTES.to_vec();
    bad[0] ^= 0xff;
    let result = apply_verified(
        &mut store,
        &verified(NEW_VERSION, 7, NEW_BYTES),
        "windows",
        "x64",
        &bad,
    );
    assert!(matches!(result, Err(ApplyError::Verify(_))));
    drop(store);

    let reopened = FsInstallStore::open(root.path()).expect("reopen");
    assert_eq!(
        reopened.installed().expect("read"),
        Some(slot(OLD_VERSION, OLD_BYTES))
    );
    assert_eq!(reopened.staged().expect("read"), None);
    assert_eq!(reopened.journal().expect("journal"), Journal::default());
}

#[test]
fn rollback_refuses_a_kept_build_that_changed_on_disk() {
    let root = TempDir::new("tampered-previous");
    let mut store = store_with_old_build(root.path());
    apply_verified(
        &mut store,
        &verified(NEW_VERSION, 7, NEW_BYTES),
        "windows",
        "x64",
        NEW_BYTES,
    )
    .expect("apply");

    // Flip the last byte of the kept previous build on disk.
    let previous_path = root.path().join("previous.slot");
    let mut raw = fs::read(&previous_path).expect("read previous");
    let last = raw.len() - 1;
    raw[last] ^= 0xff;
    fs::write(&previous_path, &raw).expect("write previous");

    assert!(matches!(
        rollback(&mut store),
        Err(ApplyError::PreviousMismatch)
    ));
    assert_eq!(
        store.installed().expect("read"),
        Some(slot(NEW_VERSION, NEW_BYTES))
    );
}

#[test]
fn damaged_installed_file_is_reported_not_treated_as_empty() {
    let root = TempDir::new("damaged");
    let store = store_with_old_build(root.path());
    fs::write(root.path().join("installed.slot"), b"no header here").expect("write");
    assert!(store.installed().is_err());
}

#[test]
fn leftover_temp_file_does_not_change_what_is_read() {
    let root = TempDir::new("leftover-tmp");
    let store = store_with_old_build(root.path());
    fs::write(root.path().join("installed.slot.tmp"), b"partial write").expect("write");
    assert_eq!(
        store.installed().expect("read"),
        Some(slot(OLD_VERSION, OLD_BYTES))
    );
}

#[test]
fn unsupported_journal_version_is_refused() {
    let root = TempDir::new("journal-version");
    let store = FsInstallStore::open(root.path()).expect("open");
    fs::write(root.path().join("journal.json"), br#"{"version": 2}"#).expect("write");
    assert!(store.journal().is_err());
}
