//! M4-16 apply and rollback on a real folder (temp dir). Runs on any OS. The platform rename
//! and directory-sync details are not exercised beyond the standard library calls.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use ed25519_dalek::{Signer, SigningKey};
use kattri_updater::apply::{
    ApplyError, InstallStore, LaunchOutcome, Slot, apply_verified, on_launch, rollback,
};
use kattri_updater::space::apply_verified_with_space;
use kattri_updater::{
    Channel, FsInstallStore, InstallState, KeyState, Keyring, UpdateError, VerifiedManifest,
    open_envelope,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const OLD_VERSION: &str = "2.0.0";
const NEW_VERSION: &str = "2.1.0";
const OLD_BYTES: &[u8] = b"fake 2.0.0 build bytes for the apply and rollback folder tests";
const NEW_BYTES: &[u8] = b"fake 2.1.0 build bytes for the apply and rollback folder tests";
const MARGIN: u64 = 1_024;

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
            "kattri-updater-apply-{label}-{}-{nanos}-{n}",
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
fn test_key() -> SigningKey {
    let seed: [u8; 32] = Sha256::digest(b"kattri-updater fs-apply TEST KEY: signer").into();
    SigningKey::from_bytes(&seed)
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn manifest_value(version: &str, sequence: u64, artifact: &[u8]) -> Value {
    json!({
        "schema_version": 1,
        "channel": "stable",
        "version": version,
        "sequence": sequence,
        "published": "2026-10-21",
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

/// A signed envelope for `version`, as text.
fn envelope(version: &str, sequence: u64, artifact: &[u8]) -> String {
    let key = test_key();
    let payload = manifest_value(version, sequence, artifact).to_string();
    let sig = key.sign(payload.as_bytes()).to_bytes();
    json!({
        "envelope_version": 1,
        "alg": "ed25519",
        "kid": "test-fs-apply-2026a",
        "payload": STANDARD.encode(payload.as_bytes()),
        "sig": STANDARD.encode(sig),
    })
    .to_string()
}

/// Run the verifier on `envelope_text` against an install at `OLD_VERSION`.
fn open(envelope_text: &str) -> Result<VerifiedManifest, UpdateError> {
    let ring = Keyring::new(vec![(
        "test-fs-apply-2026a".to_owned(),
        test_key().verifying_key().to_bytes(),
        KeyState::Active,
    )])
    .expect("test keyring");
    open_envelope(
        &ring,
        envelope_text,
        Channel::Stable,
        &InstallState::new(OLD_VERSION).expect("state"),
    )
}

fn verified(version: &str, sequence: u64, artifact: &[u8]) -> VerifiedManifest {
    open(&envelope(version, sequence, artifact)).expect("verified manifest")
}

fn slot(version: &str, bytes: &[u8]) -> Slot {
    Slot {
        version: version.to_owned(),
        bytes: bytes.to_vec(),
    }
}

/// Open a store in `dir` with `OLD_VERSION` installed, as a first install would leave it.
fn store_with_old_build(dir: &Path) -> FsInstallStore {
    let mut store = FsInstallStore::open(dir).expect("open store");
    store
        .stage(slot(OLD_VERSION, OLD_BYTES))
        .expect("stage old");
    store.commit_staged().expect("commit old");
    store
}

#[test]
fn apply_then_rollback_restores_the_previous_bytes_on_disk() {
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

    let mut applied = FsInstallStore::open(root.path()).expect("reopen after apply");
    assert_eq!(
        applied.installed().expect("read"),
        Some(slot(NEW_VERSION, NEW_BYTES))
    );
    assert_eq!(
        applied.previous().expect("read"),
        Some(slot(OLD_VERSION, OLD_BYTES))
    );

    assert!(matches!(
        on_launch(&mut applied, NEW_VERSION).expect("first launch"),
        LaunchOutcome::HealthCheckPending { .. }
    ));
    assert!(matches!(
        on_launch(&mut applied, NEW_VERSION).expect("second launch"),
        LaunchOutcome::RollbackOffered { .. }
    ));
    rollback(&mut applied).expect("rollback");
    drop(applied);

    let restored = FsInstallStore::open(root.path()).expect("reopen after rollback");
    assert_eq!(
        restored.installed().expect("read"),
        Some(slot(OLD_VERSION, OLD_BYTES))
    );
    assert_eq!(restored.previous().expect("read"), None);
}

#[test]
fn rollback_only_needs_one_atomic_slot_replace() {
    let root = TempDir::new("rollback-second-slot-failure");
    let mut store = store_with_old_build(root.path());
    apply_verified(
        &mut store,
        &verified(NEW_VERSION, 7, NEW_BYTES),
        "windows",
        "x64",
        NEW_BYTES,
    )
    .expect("apply");

    // The previous implementation tried to exchange both slot files. This obstruction made
    // the second write fail after installed.slot had already been replaced.
    let blocker = root.path().join("previous.slot.tmp");
    fs::create_dir(&blocker).expect("block obsolete second temp write");
    rollback(&mut store).expect("rollback does not need to rewrite the previous slot");

    assert_eq!(
        store.installed().expect("installed"),
        Some(slot(OLD_VERSION, OLD_BYTES))
    );
    assert_eq!(store.previous().expect("previous"), None);
    let journal = store.journal().expect("journal");
    assert!(journal.pending.is_none());
    assert_eq!(journal.failed_targets, [NEW_VERSION.to_owned()]);
    fs::remove_dir(&blocker).expect("remove obsolete blocker");
}

#[test]
fn failed_swap_leaves_the_installed_build_unchanged_and_apply_can_retry() {
    let root = TempDir::new("failed-swap");
    let mut store = store_with_old_build(root.path());
    // A folder where the temp file for previous.slot must go makes the first write of the swap fail.
    let blocker = root.path().join("previous.slot.tmp");
    fs::create_dir(&blocker).expect("block the swap");

    let result = apply_verified(
        &mut store,
        &verified(NEW_VERSION, 7, NEW_BYTES),
        "windows",
        "x64",
        NEW_BYTES,
    );
    assert!(matches!(result, Err(ApplyError::Store(_))));
    drop(store);

    let mut reopened = FsInstallStore::open(root.path()).expect("reopen after failed swap");
    assert_eq!(
        reopened.installed().expect("read"),
        Some(slot(OLD_VERSION, OLD_BYTES))
    );
    assert_eq!(reopened.previous().expect("read"), None);
    assert_eq!(reopened.staged().expect("read"), None);
    assert_eq!(reopened.journal().expect("journal").pending, None);

    // Once the obstruction is gone, the same verified update applies normally.
    fs::remove_dir(&blocker).expect("unblock");
    apply_verified(
        &mut reopened,
        &verified(NEW_VERSION, 7, NEW_BYTES),
        "windows",
        "x64",
        NEW_BYTES,
    )
    .expect("apply after unblocking");
    assert_eq!(
        reopened.installed().expect("read"),
        Some(slot(NEW_VERSION, NEW_BYTES))
    );
}

#[test]
fn bad_signature_is_refused_and_the_installed_build_is_unchanged() {
    let root = TempDir::new("bad-signature");
    store_with_old_build(root.path());

    // Flip one bit of the signature. The envelope never yields a VerifiedManifest, so apply
    // cannot be called with these bytes.
    let mut value: Value =
        serde_json::from_str(&envelope(NEW_VERSION, 7, NEW_BYTES)).expect("envelope is JSON");
    let sig_text = value["sig"].as_str().expect("sig is a string").to_owned();
    let mut sig = STANDARD.decode(sig_text).expect("sig is base64");
    sig[0] ^= 0x01;
    value["sig"] = json!(STANDARD.encode(sig));

    let refused = open(&value.to_string());
    assert!(matches!(refused, Err(UpdateError::VerificationFailed(_))));

    let reopened = FsInstallStore::open(root.path()).expect("reopen");
    assert_eq!(
        reopened.installed().expect("read"),
        Some(slot(OLD_VERSION, OLD_BYTES))
    );
    assert_eq!(reopened.previous().expect("read"), None);
    assert_eq!(reopened.staged().expect("read"), None);
    assert_eq!(reopened.journal().expect("journal").pending, None);
}

#[test]
fn staged_build_lives_in_the_staging_folder_until_the_switch() {
    let root = TempDir::new("staging-folder");
    let mut store = store_with_old_build(root.path());
    store.stage(slot(NEW_VERSION, NEW_BYTES)).expect("stage");

    let staged_path = root.path().join("staging").join("staged.slot");
    assert!(
        staged_path.is_file(),
        "staged build is not in the staging folder"
    );
    assert!(!root.path().join("staged.slot").exists());

    store.commit_staged().expect("commit");
    assert!(
        !staged_path.exists(),
        "staging folder still holds the build after the switch"
    );
    assert_eq!(
        store.installed().expect("read"),
        Some(slot(NEW_VERSION, NEW_BYTES))
    );
}

#[test]
fn staging_refused_for_lack_of_space_leaves_the_store_untouched() {
    let root = TempDir::new("disk-full");
    let mut store = store_with_old_build(root.path());
    let needed = u64::try_from(NEW_BYTES.len()).expect("length fits") + MARGIN;

    let refused = apply_verified_with_space(
        &mut store,
        &verified(NEW_VERSION, 7, NEW_BYTES),
        "windows",
        "x64",
        NEW_BYTES,
        needed - 1,
        MARGIN,
    );
    assert!(matches!(
        refused,
        Err(ApplyError::InsufficientSpace { required, free })
            if required == needed && free == needed - 1
    ));
    drop(store);

    let mut reopened = FsInstallStore::open(root.path()).expect("reopen");
    assert_eq!(
        reopened.installed().expect("read"),
        Some(slot(OLD_VERSION, OLD_BYTES))
    );
    assert_eq!(reopened.staged().expect("read"), None);
    assert_eq!(reopened.journal().expect("journal").pending, None);

    // Exactly enough space lets the same update through.
    apply_verified_with_space(
        &mut reopened,
        &verified(NEW_VERSION, 7, NEW_BYTES),
        "windows",
        "x64",
        NEW_BYTES,
        needed,
        MARGIN,
    )
    .expect("apply with exact space");
    assert_eq!(
        reopened.installed().expect("read"),
        Some(slot(NEW_VERSION, NEW_BYTES))
    );
}
