//! M4-16 tests for apply, health and rollback, run against the in-memory store double.
//!
//! The filesystem swap on real disks is NOT tested here and is NOT verified on Windows or macOS.
//! Keys are derived at runtime from labels in this file. They are TEST KEYS, never release keys.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use ed25519_dalek::{Signer, SigningKey};
use kattri_updater::apply::memory::{MemoryStore, Op};
use kattri_updater::apply::{
    ApplyError, InstallStore, LaunchOutcome, Pending, Slot, apply_verified, mark_healthy,
    note_clean_shutdown, on_launch, rollback, sha256_hex,
};
use kattri_updater::{Channel, InstallState, KeyState, Keyring, VerifiedManifest, open_envelope};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const OLD_VERSION: &str = "2.0.0";
const NEW_VERSION: &str = "2.1.0";
const OLD_BYTES: &[u8] = b"fake 2.0.0 build bytes for the M4-16 apply tests";
const NEW_BYTES: &[u8] = b"fake 2.1.0 build bytes for the M4-16 apply tests";

/// A test-only signing key derived from a label. Never a release key.
fn test_key(label: &str) -> SigningKey {
    let seed: [u8; 32] =
        Sha256::digest(format!("kattri-updater apply TEST KEY: {label}").as_bytes()).into();
    SigningKey::from_bytes(&seed)
}

fn manifest_value(version: &str, sequence: u64, artifact: &[u8]) -> Value {
    json!({
        "schema_version": 1,
        "channel": "stable",
        "version": version,
        "sequence": sequence,
        "published": "2026-10-15",
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

/// Sign `manifest` with a test key and open it against an install state at `OLD_VERSION`.
fn verified(version: &str, sequence: u64, artifact: &[u8]) -> VerifiedManifest {
    let key = test_key("apply-signer");
    let ring = Keyring::new(vec![(
        "test-apply-2026a".to_owned(),
        key.verifying_key().to_bytes(),
        KeyState::Active,
    )])
    .unwrap();
    let payload = manifest_value(version, sequence, artifact).to_string();
    let sig = key.sign(payload.as_bytes()).to_bytes();
    let envelope = json!({
        "envelope_version": 1,
        "alg": "ed25519",
        "kid": "test-apply-2026a",
        "payload": STANDARD.encode(payload.as_bytes()),
        "sig": STANDARD.encode(sig),
    })
    .to_string();
    open_envelope(
        &ring,
        &envelope,
        Channel::Stable,
        &InstallState::new(OLD_VERSION).unwrap(),
    )
    .unwrap()
}

fn store_at_old() -> MemoryStore {
    MemoryStore::with_installed(OLD_VERSION, OLD_BYTES)
}

fn installed_version(store: &MemoryStore) -> String {
    store.installed_slot().unwrap().version.clone()
}

#[test]
fn apply_succeeds_and_keeps_previous_build_for_rollback() {
    let v = verified(NEW_VERSION, 7, NEW_BYTES);
    let mut store = store_at_old();

    apply_verified(&mut store, &v, "windows", "x64", NEW_BYTES).unwrap();

    assert_eq!(
        store.installed_slot(),
        Some(&Slot {
            version: NEW_VERSION.to_owned(),
            bytes: NEW_BYTES.to_vec()
        })
    );
    assert_eq!(
        store.previous_slot(),
        Some(&Slot {
            version: OLD_VERSION.to_owned(),
            bytes: OLD_BYTES.to_vec()
        })
    );
    assert!(
        store.staged_slot().is_none(),
        "staging is cleared after commit"
    );
    let journal = store.journal().unwrap();
    let pending = journal.pending.unwrap();
    assert_eq!(pending.target, NEW_VERSION);
    assert_eq!(pending.previous, OLD_VERSION);
    assert_eq!(pending.previous_sha256, sha256_hex(OLD_BYTES));
    assert_eq!(pending.failed_launches, 0);
}

#[test]
fn tampered_artifact_is_refused_before_any_write() {
    let v = verified(NEW_VERSION, 7, NEW_BYTES);

    // One flipped byte: digest mismatch.
    let mut flipped = NEW_BYTES.to_vec();
    flipped[0] ^= 0x01;
    let mut store = store_at_old();
    let err = apply_verified(&mut store, &v, "windows", "x64", &flipped).unwrap_err();
    assert!(matches!(err, ApplyError::Verify(_)), "got {err:?}");
    assert_eq!(store.writes(), 0, "no write may happen before the check");
    assert_eq!(installed_version(&store), OLD_VERSION);

    // Truncated: size mismatch, still before any write.
    let mut store = store_at_old();
    let err = apply_verified(&mut store, &v, "windows", "x64", &NEW_BYTES[..10]).unwrap_err();
    assert!(matches!(err, ApplyError::Verify(_)), "got {err:?}");
    assert_eq!(store.writes(), 0);
    assert_eq!(installed_version(&store), OLD_VERSION);
}

#[test]
fn failed_health_check_twice_rolls_back() {
    let v = verified(NEW_VERSION, 7, NEW_BYTES);
    let mut store = store_at_old();
    apply_verified(&mut store, &v, "windows", "x64", NEW_BYTES).unwrap();

    // First launch of the target, no healthy marker: counted, no offer yet.
    assert_eq!(
        on_launch(&mut store, NEW_VERSION).unwrap(),
        LaunchOutcome::HealthCheckPending {
            target: NEW_VERSION.to_owned(),
            failed_launches: 1
        }
    );
    // Second launch of the target without a healthy marker: restore is offered.
    assert_eq!(
        on_launch(&mut store, NEW_VERSION).unwrap(),
        LaunchOutcome::RollbackOffered {
            target: NEW_VERSION.to_owned(),
            restore_to: OLD_VERSION.to_owned()
        }
    );
    // The user accepts the offer.
    rollback(&mut store).unwrap();

    assert_eq!(installed_version(&store), OLD_VERSION);
    assert_eq!(store.installed_slot().unwrap().bytes, OLD_BYTES);
    assert!(
        store.previous_slot().is_none(),
        "the failed build is dropped"
    );
    let journal = store.journal().unwrap();
    assert!(journal.pending.is_none());
    assert_eq!(journal.failed_targets, vec![NEW_VERSION.to_owned()]);

    // The rolled-back version is not offered again, and the old version settles.
    let err = apply_verified(&mut store, &v, "windows", "x64", NEW_BYTES).unwrap_err();
    assert!(
        matches!(err, ApplyError::PreviouslyFailed { .. }),
        "got {err:?}"
    );
    assert_eq!(
        on_launch(&mut store, OLD_VERSION).unwrap(),
        LaunchOutcome::Settled
    );
}

#[test]
fn rollback_refuses_when_kept_build_fails_its_digest() {
    let v = verified(NEW_VERSION, 7, NEW_BYTES);
    let mut store = store_at_old();
    apply_verified(&mut store, &v, "windows", "x64", NEW_BYTES).unwrap();
    store.tamper_previous();

    let err = rollback(&mut store).unwrap_err();
    assert_eq!(err, ApplyError::PreviousMismatch);
    assert_eq!(
        installed_version(&store),
        NEW_VERSION,
        "installed build is unchanged"
    );
}

#[test]
fn rollback_retry_after_journal_failure_keeps_the_restored_build_installed() {
    let v = verified(NEW_VERSION, 7, NEW_BYTES);
    let mut store = store_at_old();
    apply_verified(&mut store, &v, "windows", "x64", NEW_BYTES).unwrap();
    store.fail_on(Op::SetJournal);

    let err = rollback(&mut store).expect_err("journal failure is injected");
    assert!(matches!(err, ApplyError::Store(_)), "got {err:?}");
    assert_eq!(installed_version(&store), OLD_VERSION);
    assert_eq!(store.installed_slot().unwrap().bytes, OLD_BYTES);
    assert_eq!(
        store.previous_slot(),
        Some(&Slot {
            version: OLD_VERSION.to_owned(),
            bytes: OLD_BYTES.to_vec(),
        })
    );
    assert_eq!(
        store
            .journal()
            .unwrap()
            .pending
            .as_ref()
            .map(|pending| pending.target.as_str()),
        Some(NEW_VERSION),
        "failed journal write leaves the pending record available for retry"
    );

    store.clear_failure(Op::SetJournal);
    rollback(&mut store).expect("retry completes the rollback");
    assert_eq!(installed_version(&store), OLD_VERSION);
    assert_eq!(store.installed_slot().unwrap().bytes, OLD_BYTES);
    assert!(store.previous_slot().is_none());
    let journal = store.journal().unwrap();
    assert!(journal.pending.is_none());
    assert_eq!(journal.failed_targets, [NEW_VERSION.to_owned()]);
}

#[test]
fn healthy_marker_clears_pending_and_drops_previous_build() {
    let v = verified(NEW_VERSION, 7, NEW_BYTES);
    let mut store = store_at_old();
    apply_verified(&mut store, &v, "windows", "x64", NEW_BYTES).unwrap();

    on_launch(&mut store, NEW_VERSION).unwrap();
    assert!(mark_healthy(&mut store, NEW_VERSION).unwrap());

    let journal = store.journal().unwrap();
    assert!(journal.pending.is_none());
    assert_eq!(journal.healthy.as_deref(), Some(NEW_VERSION));
    assert!(
        store.previous_slot().is_none(),
        "kept build is deleted once healthy"
    );
    assert_eq!(
        on_launch(&mut store, NEW_VERSION).unwrap(),
        LaunchOutcome::Settled
    );
    assert!(
        !mark_healthy(&mut store, NEW_VERSION).unwrap(),
        "nothing left to mark"
    );
}

#[test]
fn clean_quits_before_the_healthy_marker_do_not_accumulate_failed_launches() {
    let v = verified(NEW_VERSION, 7, NEW_BYTES);
    let mut store = store_at_old();
    apply_verified(&mut store, &v, "windows", "x64", NEW_BYTES).unwrap();

    // Three ordinary open/quit cycles: each launch is followed by a clean shutdown.
    for expected in [1, 1, 1] {
        let outcome = on_launch(&mut store, NEW_VERSION).unwrap();
        match outcome {
            LaunchOutcome::HealthCheckPending {
                failed_launches, ..
            } => {
                assert_eq!(failed_launches, expected);
            }
            other => panic!("expected a pending health check, got {other:?}"),
        }
        assert!(note_clean_shutdown(&mut store).unwrap());
    }
    assert!(store.journal().unwrap().pending.is_some());

    // Two launches without a clean shutdown in between do count and reach the rollback offer.
    let outcome = on_launch(&mut store, NEW_VERSION).unwrap();
    assert!(
        matches!(
            outcome,
            LaunchOutcome::HealthCheckPending {
                failed_launches: 1,
                ..
            }
        ),
        "got {outcome:?}"
    );
    let outcome = on_launch(&mut store, NEW_VERSION).unwrap();
    assert!(
        matches!(outcome, LaunchOutcome::RollbackOffered { .. }),
        "got {outcome:?}"
    );
}

#[test]
fn commit_failure_between_stage_and_swap_leaves_installed_build_unchanged() {
    let v = verified(NEW_VERSION, 7, NEW_BYTES);
    let mut store = store_at_old();
    store.fail_on(Op::Commit);

    let err = apply_verified(&mut store, &v, "windows", "x64", NEW_BYTES).unwrap_err();
    assert!(matches!(err, ApplyError::Store(_)), "got {err:?}");
    assert_eq!(installed_version(&store), OLD_VERSION);
    assert_eq!(store.installed_slot().unwrap().bytes, OLD_BYTES);
    assert!(store.previous_slot().is_none());
    assert!(store.staged_slot().is_none(), "staged bytes are discarded");
    assert!(
        store.journal().unwrap().pending.is_none(),
        "no pending claim without a switch"
    );
}

#[test]
fn crash_after_stage_and_journal_is_abandoned_on_next_launch() {
    // Simulate a process that died after staging and journalling, before the commit.
    let mut store = store_at_old();
    store
        .stage(Slot {
            version: NEW_VERSION.to_owned(),
            bytes: NEW_BYTES.to_vec(),
        })
        .unwrap();
    store
        .set_journal(kattri_updater::apply::Journal {
            pending: Some(Pending {
                target: NEW_VERSION.to_owned(),
                previous: OLD_VERSION.to_owned(),
                previous_sha256: sha256_hex(OLD_BYTES),
                failed_launches: 0,
            }),
            ..Default::default()
        })
        .unwrap();

    assert_eq!(
        on_launch(&mut store, OLD_VERSION).unwrap(),
        LaunchOutcome::AbandonedUpdate {
            target: NEW_VERSION.to_owned()
        }
    );
    assert_eq!(
        installed_version(&store),
        OLD_VERSION,
        "installed build is unchanged"
    );
    assert_eq!(store.installed_slot().unwrap().bytes, OLD_BYTES);
    assert!(store.journal().unwrap().pending.is_none());
}

#[test]
fn staged_bytes_that_fail_the_readback_check_are_discarded() {
    let v = verified(NEW_VERSION, 7, NEW_BYTES);
    let mut store = store_at_old();
    store.fail_on(Op::CorruptStage);

    let err = apply_verified(&mut store, &v, "windows", "x64", NEW_BYTES).unwrap_err();
    assert_eq!(err, ApplyError::StagedDigestMismatch);
    assert_eq!(installed_version(&store), OLD_VERSION);
    assert!(store.staged_slot().is_none());
    assert!(store.journal().unwrap().pending.is_none());
}

#[test]
fn not_newer_and_second_pending_update_are_refused() {
    let same = verified(OLD_VERSION, 3, OLD_BYTES);
    let mut store = store_at_old();
    let err = apply_verified(&mut store, &same, "windows", "x64", OLD_BYTES).unwrap_err();
    assert!(matches!(err, ApplyError::NotNewer { .. }), "got {err:?}");

    let next = verified(NEW_VERSION, 7, NEW_BYTES);
    apply_verified(&mut store, &next, "windows", "x64", NEW_BYTES).unwrap();
    let later = verified("2.2.0", 8, b"fake 2.2.0 bytes");
    let err =
        apply_verified(&mut store, &later, "windows", "x64", b"fake 2.2.0 bytes").unwrap_err();
    assert!(
        matches!(err, ApplyError::AlreadyPending { .. }),
        "got {err:?}"
    );
    assert_eq!(installed_version(&store), NEW_VERSION);
}

#[test]
fn missing_platform_artifact_is_refused_before_any_write() {
    let v = verified(NEW_VERSION, 7, NEW_BYTES);
    let mut store = store_at_old();
    let err = apply_verified(&mut store, &v, "linux", "x64", NEW_BYTES).unwrap_err();
    assert!(matches!(err, ApplyError::NoArtifact { .. }), "got {err:?}");
    assert_eq!(store.writes(), 0);
}
