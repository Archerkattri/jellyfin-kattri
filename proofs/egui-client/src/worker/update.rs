//! Apply and rollback of a verified update (M4-16).
//!
//! The pure steps run on the UI thread: the manifest is verified, the playback gate is checked,
//! and the job is built. Verification reads at most one 64 KiB envelope, so this stays cheap.
//! The file work (open the store, read the staged build, probe free space, commit or swap)
//! blocks, so it runs on the worker's blocking pool and never on the UI thread.
//!
//! There is no fetch here. The update host is not decided (ADR 0004 section 7), so the caller
//! supplies a signed envelope and a staged build that is already on disk.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use kattri_updater::InstallState;
use kattri_updater::apply::{ApplyError, InstallStore};
use kattri_updater::gate::{PlaybackState, UpdateGate, playback_gate};
use kattri_updater::space::{apply_verified_with_space, check_staging_space};
use kattri_updater::{Channel, Keyring, VerifiedManifest, open_envelope};
use kattri_updater::{FsInstallStore, FsReplayStateStore, verify_artifact_bytes};

/// Bytes kept free beyond the staged build and the kept previous build (journal, filesystem
/// overhead). The updater crate takes the margin from its caller, so the app picks it here.
pub(crate) const SPACE_MARGIN_BYTES: u64 = 64 * 1024 * 1024;

/// Free bytes for the volume that holds a path. Returns the reason when it cannot be read.
pub(crate) type FreeSpaceProbe = fn(&Path) -> Result<u64, String>;

/// Inputs for one apply, as the app receives them.
pub struct ApplyRequest<'a> {
    pub envelope: &'a str,
    pub keyring: &'a Keyring,
    pub channel: Channel,
    /// The build that runs now. Used for the downgrade check.
    pub installed_version: &'a str,
    /// The build the caller staged on disk. Its size and digest are checked before any write.
    pub staged_path: PathBuf,
    /// The update store folder: the installed build, the kept previous build and the journal.
    pub store_dir: PathBuf,
    pub playback: PlaybackState,
}

/// A verified update that is ready to run on the worker.
#[derive(Debug)]
pub(crate) struct ApplyJob {
    verified: VerifiedManifest,
    installed_version: String,
    platform: &'static str,
    arch: &'static str,
    staged_path: PathBuf,
    store_dir: PathBuf,
}

impl ApplyJob {
    pub(crate) fn version(&self) -> &str {
        &self.verified.manifest().version
    }

    pub(crate) fn store_dir(&self) -> &Path {
        &self.store_dir
    }
}

/// What the worker reports after an apply or rollback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateOutcome {
    Applied { version: String },
    Refused { version: String, reason: String },
    Failed { version: String, reason: String },
    RolledBack { version: String },
    RollbackFailed { reason: String },
}

/// What the update panel shows.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum UpdateStatus {
    #[default]
    Idle,
    /// Playback is open, so the update was not applied. Nothing was written.
    Deferred {
        version: String,
    },
    /// The envelope failed verification, or the store refused the write for space or policy.
    Refused {
        version: Option<String>,
        reason: String,
    },
    Applying {
        version: String,
        store_dir: PathBuf,
    },
    /// The new build is committed in the update store and the previous build is kept.
    Applied {
        version: String,
        store_dir: PathBuf,
    },
    Failed {
        version: Option<String>,
        reason: String,
    },
    RollingBack {
        version: String,
        store_dir: PathBuf,
    },
    RolledBack {
        version: String,
    },
    RollbackFailed {
        reason: String,
    },
}

/// The app's update state. Only the UI thread changes it.
#[derive(Debug, Default)]
pub struct UpdateState {
    pub(crate) status: UpdateStatus,
}

impl UpdateState {
    /// True while the worker owns a file step. A second request must wait.
    pub(crate) fn is_busy(&self) -> bool {
        matches!(
            self.status,
            UpdateStatus::Applying { .. } | UpdateStatus::RollingBack { .. }
        )
    }

    pub(crate) fn finish(&mut self, outcome: UpdateOutcome) {
        self.status = match outcome {
            UpdateOutcome::Applied { version } => match &self.status {
                UpdateStatus::Applying { store_dir, .. } => UpdateStatus::Applied {
                    version,
                    store_dir: store_dir.clone(),
                },
                _ => UpdateStatus::Failed {
                    version: Some(version),
                    reason: "The update finished without a pending request.".to_owned(),
                },
            },
            UpdateOutcome::Refused { version, reason } => UpdateStatus::Refused {
                version: Some(version),
                reason,
            },
            UpdateOutcome::Failed { version, reason } => UpdateStatus::Failed {
                version: Some(version),
                reason,
            },
            UpdateOutcome::RolledBack { version } => UpdateStatus::RolledBack { version },
            UpdateOutcome::RollbackFailed { reason } => UpdateStatus::RollbackFailed { reason },
        };
    }
}

/// Verify the envelope, check the playback gate, and build the job. Writes nothing.
pub(crate) fn prepare(request: &ApplyRequest<'_>) -> Result<ApplyJob, UpdateStatus> {
    let state =
        InstallState::new(request.installed_version).map_err(|e| UpdateStatus::Refused {
            version: None,
            reason: e.to_string(),
        })?;
    let verified = open_envelope(request.keyring, request.envelope, request.channel, &state)
        .map_err(|e| UpdateStatus::Refused {
            version: None,
            reason: e.to_string(),
        })?;
    let version = verified.manifest().version.clone();

    // Checked before any apply step, and before the store is opened.
    if playback_gate(request.playback) == UpdateGate::Deferred {
        return Err(UpdateStatus::Deferred { version });
    }

    let Some((platform, arch)) = host_target() else {
        return Err(UpdateStatus::Refused {
            version: Some(version),
            reason: "No update build is published for this OS and CPU.".to_owned(),
        });
    };
    Ok(ApplyJob {
        verified,
        installed_version: request.installed_version.to_owned(),
        platform,
        arch,
        staged_path: request.staged_path.clone(),
        store_dir: request.store_dir.clone(),
    })
}

/// The manifest platform and arch names for the running build, or `None` if none is published.
pub(crate) fn host_target() -> Option<(&'static str, &'static str)> {
    let platform = match std::env::consts::OS {
        "windows" => "windows",
        "macos" => "macos",
        "linux" => "linux",
        _ => return None,
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        _ => return None,
    };
    Some((platform, arch))
}

/// Apply a verified job against the store. Runs on the blocking pool, never on the UI thread.
///
/// Preflight failures leave installed bytes and replay floors unchanged. Valid signed
/// acceptance is durable before slot commit; a failed install can retry the identical manifest.
pub(crate) fn apply_blocking(job: &ApplyJob, probe: FreeSpaceProbe) -> UpdateOutcome {
    let version = job.version().to_owned();
    let refused = |reason: String| UpdateOutcome::Refused {
        version: version.clone(),
        reason,
    };
    let failed = |reason: String| UpdateOutcome::Failed {
        version: version.clone(),
        reason,
    };

    let mut store = match FsInstallStore::open(&job.store_dir) {
        Ok(store) => store,
        Err(error) => return failed(error.to_string()),
    };
    // Keep replay validation, durable acceptance and the slot transaction serialized across
    // processes. The OS lock is released on crash; a stale filename never blocks recovery.
    let replay_store = FsReplayStateStore::new(job.store_dir.join("replay-state.json"));
    let _replay_lock = match replay_store.try_lock() {
        Ok(guard) => guard,
        Err(error) => return refused(error.to_string()),
    };
    let mut replay_state = match replay_store.load(&job.installed_version) {
        Ok(state) => state,
        Err(error) => return refused(error.to_string()),
    };
    if let Err(error) = replay_state.check_verified(&job.verified) {
        return refused(error.to_string());
    }
    let Some(artifact) = job.verified.manifest().artifact_for(job.platform, job.arch) else {
        return refused("The update manifest has no build for this OS and CPU.".to_owned());
    };
    let bytes = match read_staged(&job.staged_path, artifact.size_bytes) {
        Ok(bytes) => bytes,
        Err(reason) => return refused(reason),
    };

    // The kept previous build is a copy of the installed one, so the margin covers it too.
    let installed_len = match store.installed() {
        Ok(Some(slot)) => u64::try_from(slot.bytes.len()).unwrap_or(u64::MAX),
        Ok(None) => 0,
        Err(error) => return failed(error.to_string()),
    };
    let margin = installed_len.saturating_add(SPACE_MARGIN_BYTES);
    let free = match probe(&job.store_dir) {
        Ok(free) => free,
        Err(reason) => {
            return refused(format!("Free disk space could not be checked: {reason}"));
        }
    };

    // Do not advance the replay floor for invalid bytes or a refused free-space preflight.
    if let Err(error) = verify_artifact_bytes(artifact, &bytes) {
        return refused(error.to_string());
    }
    if let Err(error) = check_staging_space(free, artifact.size_bytes, margin) {
        return refused(error.to_string());
    }
    replay_state.record(&job.verified);
    if let Err(error) = replay_store.save(&replay_state) {
        return refused(error.to_string());
    }
    match apply_verified_with_space(
        &mut store,
        &job.verified,
        job.platform,
        job.arch,
        &bytes,
        free,
        margin,
    ) {
        Ok(()) => UpdateOutcome::Applied {
            version: version.clone(),
        },
        Err(error) if is_refusal(&error) => refused(error.to_string()),
        Err(error) => failed(error.to_string()),
    }
}

/// Errors that mean the update was not allowed, as opposed to the store failing.
fn is_refusal(error: &ApplyError) -> bool {
    matches!(
        error,
        ApplyError::Verify(_)
            | ApplyError::InsufficientSpace { .. }
            | ApplyError::NoArtifact { .. }
            | ApplyError::NotNewer { .. }
            | ApplyError::PreviouslyFailed { .. }
            | ApplyError::AlreadyPending { .. }
    )
}

/// Read the staged build only if its size matches the manifest. The size bounds the read.
fn read_staged(path: &Path, expected: u64) -> Result<Vec<u8>, String> {
    let file =
        fs::File::open(path).map_err(|_| "The staged build could not be read.".to_owned())?;
    let meta = file
        .metadata()
        .map_err(|_| "The staged build could not be read.".to_owned())?;
    if meta.len() != expected {
        return Err(format!(
            "The staged build is {} bytes, but the manifest says {expected}.",
            meta.len()
        ));
    }
    let mut bytes = Vec::new();
    file.take(expected.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|_| "The staged build could not be read.".to_owned())?;
    if bytes.len() as u64 != expected {
        return Err("The staged build changed while it was read.".to_owned());
    }
    Ok(bytes)
}

/// Restore the kept previous build. Runs only when the user asks for it.
pub(crate) fn rollback_blocking(store_dir: &Path) -> UpdateOutcome {
    let mut store = match FsInstallStore::open(store_dir) {
        Ok(store) => store,
        Err(error) => {
            return UpdateOutcome::RollbackFailed {
                reason: error.to_string(),
            };
        }
    };
    let replay_store = FsReplayStateStore::new(store_dir.join("replay-state.json"));
    let _replay_lock = match replay_store.try_lock() {
        Ok(guard) => guard,
        Err(error) => {
            return UpdateOutcome::RollbackFailed {
                reason: error.to_string(),
            };
        }
    };
    let pending = match store.journal() {
        Ok(journal) => journal.pending,
        Err(error) => {
            return UpdateOutcome::RollbackFailed {
                reason: error.to_string(),
            };
        }
    };
    let Some(pending) = pending else {
        return UpdateOutcome::RollbackFailed {
            reason: "No update is pending, so there is nothing to roll back.".to_owned(),
        };
    };
    match kattri_updater::apply::rollback(&mut store) {
        Ok(()) => UpdateOutcome::RolledBack {
            version: pending.target,
        },
        Err(error) => UpdateOutcome::RollbackFailed {
            reason: error.to_string(),
        },
    }
}

/// Run an apply job on the blocking pool and return its outcome.
pub(crate) async fn run_apply(job: ApplyJob) -> UpdateOutcome {
    let version = job.version().to_owned();
    match tokio::task::spawn_blocking(move || apply_blocking(&job, system_free_space)).await {
        Ok(outcome) => outcome,
        Err(_) => UpdateOutcome::Failed {
            version,
            reason: "The update stopped before it finished.".to_owned(),
        },
    }
}

/// Run a rollback on the blocking pool and return its outcome.
pub(crate) async fn run_rollback(store_dir: PathBuf) -> UpdateOutcome {
    match tokio::task::spawn_blocking(move || rollback_blocking(&store_dir)).await {
        Ok(outcome) => outcome,
        Err(_) => UpdateOutcome::RollbackFailed {
            reason: "The rollback stopped before it finished.".to_owned(),
        },
    }
}

#[cfg(windows)]
fn system_free_space(path: &Path) -> Result<u64, String> {
    use kattri_platform::DiskSpace as _;
    kattri_platform::WindowsDiskSpace
        .available_bytes(path)
        .map_err(|error| error.to_string())
}

/// No free-space probe exists for this OS yet. Refusing is the safe default, since a write
/// without a space check could fill the disk.
#[cfg(not(windows))]
fn system_free_space(_path: &Path) -> Result<u64, String> {
    Err("it is only checked on Windows so far".to_owned())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;
    use std::process;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD;
    use ed25519_dalek::{Signer, SigningKey};
    use kattri_updater::apply::{InstallStore, Slot, sha256_hex};
    use kattri_updater::gate::PlaybackState;
    use kattri_updater::{Channel, FsInstallStore, FsReplayStateStore, KeyState, Keyring};
    use serde_json::{Value, json};

    use super::{
        ApplyRequest, UpdateOutcome, UpdateState, UpdateStatus, apply_blocking, host_target,
        prepare, rollback_blocking,
    };

    const KID: &str = "egui-client-update-test-2026a";
    const OLD_VERSION: &str = "1.0.0";
    const NEW_VERSION: &str = "1.1.0";

    /// A scratch folder that is removed on drop.
    struct Scratch(std::path::PathBuf);

    impl Scratch {
        fn new(label: &str) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "kattri-update-{label}-{}-{nanos}-{n}",
                process::id()
            ));
            std::fs::create_dir_all(&path).expect("scratch folder");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A test signing key derived from a label, so no private key is stored as a literal.
    fn signer(label: &str) -> SigningKey {
        let hex = sha256_hex(label.as_bytes());
        let mut seed = [0u8; 32];
        for (i, byte) in seed.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).expect("hex digit");
        }
        SigningKey::from_bytes(&seed)
    }

    fn keyring(key: &SigningKey) -> Keyring {
        Keyring::new(vec![(
            KID.to_owned(),
            key.verifying_key().to_bytes(),
            KeyState::Active,
        )])
        .expect("test keyring")
    }

    /// A signed envelope whose manifest lists `artifact` for the host platform.
    fn envelope_signed_by(key: &SigningKey, version: &str, artifact: &[u8]) -> String {
        let (platform, arch) = host_target().expect("host platform is published");
        let payload: Value = json!({
            "schema_version": 1,
            "channel": "stable",
            "version": version,
            "sequence": 1,
            "published": "2026-10-22",
            "artifacts": [{
                "platform": platform,
                "arch": arch,
                "format": if platform == "windows" { "msi" } else if platform == "macos" { "dmg" } else { "appimage" },
                "url": "https://updates.example.org/dl/kattri-test.bin",
                "sha256": sha256_hex(artifact),
                "size_bytes": artifact.len()
            }]
        });
        let payload = payload.to_string();
        let sig = key.sign(payload.as_bytes()).to_bytes();
        json!({
            "envelope_version": 1,
            "alg": "ed25519",
            "kid": KID,
            "payload": STANDARD.encode(payload.as_bytes()),
            "sig": STANDARD.encode(sig),
        })
        .to_string()
    }

    fn request<'a>(
        envelope: &'a str,
        keyring: &'a Keyring,
        scratch: &Scratch,
        playback: PlaybackState,
    ) -> ApplyRequest<'a> {
        ApplyRequest {
            envelope,
            keyring,
            channel: Channel::Stable,
            installed_version: OLD_VERSION,
            staged_path: scratch.path().join("staged-update.bin"),
            store_dir: scratch.path().join("store"),
            playback,
        }
    }

    /// Open the store in `dir` with `bytes` installed as `OLD_VERSION`.
    fn seed_installed(dir: &Path, bytes: &[u8]) {
        let mut store = FsInstallStore::open(dir).expect("open store");
        store
            .stage(Slot {
                version: OLD_VERSION.to_owned(),
                bytes: bytes.to_vec(),
            })
            .expect("stage old build");
        store.commit_staged().expect("commit old build");
    }

    fn ample(_: &Path) -> Result<u64, String> {
        Ok(1 << 40)
    }

    fn none(_: &Path) -> Result<u64, String> {
        Ok(0)
    }

    #[test]
    fn playing_defers_the_update_and_writes_nothing() {
        let scratch = Scratch::new("deferred");
        let key = signer("deferred");
        let keys = keyring(&key);
        let new_build = b"new build bytes";
        let envelope = envelope_signed_by(&key, NEW_VERSION, new_build);
        std::fs::write(scratch.path().join("staged-update.bin"), new_build).expect("stage");

        let req = request(&envelope, &keys, &scratch, PlaybackState::Playing);
        let status = match prepare(&req) {
            Err(status) => status,
            Ok(_) => panic!("playback must defer the update"),
        };
        assert_eq!(
            status,
            UpdateStatus::Deferred {
                version: NEW_VERSION.to_owned()
            }
        );
        assert!(!scratch.path().join("store").exists());
    }

    #[test]
    fn paused_and_loading_playback_also_defer() {
        let scratch = Scratch::new("deferred-paused");
        let key = signer("deferred-paused");
        let keys = keyring(&key);
        let envelope = envelope_signed_by(&key, NEW_VERSION, b"new");
        for playback in [PlaybackState::Paused, PlaybackState::Loading] {
            let req = request(&envelope, &keys, &scratch, playback);
            assert!(matches!(prepare(&req), Err(UpdateStatus::Deferred { .. })));
        }
    }

    #[test]
    fn bad_signature_is_refused_before_any_write() {
        let scratch = Scratch::new("bad-sig");
        let trusted = signer("trusted");
        let forger = signer("forger");
        let keys = keyring(&trusted);
        // Signed by a key the same kid does not match, so verification must fail.
        let envelope = envelope_signed_by(&forger, NEW_VERSION, b"new");

        let req = request(&envelope, &keys, &scratch, PlaybackState::Stopped);
        match prepare(&req) {
            Err(UpdateStatus::Refused { version, reason }) => {
                assert_eq!(version, None);
                assert!(reason.contains("not accepted"), "reason was: {reason}");
            }
            other => panic!("expected a signature refusal, got {other:?}"),
        }
        assert!(!scratch.path().join("store").exists());
    }

    #[test]
    fn applied_build_is_rolled_back_on_a_temp_store() {
        let scratch = Scratch::new("apply-rollback");
        let key = signer("apply-rollback");
        let keys = keyring(&key);
        let old_build = b"old build bytes";
        let new_build = b"new build bytes, longer than the old";
        let store_dir = scratch.path().join("store");
        seed_installed(&store_dir, old_build);
        let staged = scratch.path().join("staged-update.bin");
        std::fs::write(&staged, new_build).expect("stage");

        let envelope = envelope_signed_by(&key, NEW_VERSION, new_build);
        let req = request(&envelope, &keys, &scratch, PlaybackState::Stopped);
        let job = prepare(&req).expect("verified job");

        let outcome = apply_blocking(&job, ample);
        assert_eq!(
            outcome,
            UpdateOutcome::Applied {
                version: NEW_VERSION.to_owned()
            }
        );
        let store = FsInstallStore::open(&store_dir).expect("reopen");
        assert_eq!(
            store.installed().expect("read").expect("installed").bytes,
            new_build
        );
        assert_eq!(
            store.previous().expect("read").expect("previous").bytes,
            old_build
        );

        let rolled = rollback_blocking(&store_dir);
        assert_eq!(
            rolled,
            UpdateOutcome::RolledBack {
                version: NEW_VERSION.to_owned()
            }
        );
        let store = FsInstallStore::open(&store_dir).expect("reopen");
        let installed = store.installed().expect("read").expect("installed");
        assert_eq!(installed.version, OLD_VERSION);
        assert_eq!(installed.bytes, old_build);
    }

    #[test]
    fn fresh_worker_jobs_reject_conflicting_sequences_after_rollback() {
        let scratch = Scratch::new("durable-replay");
        let key = signer("durable-replay");
        let keys = keyring(&key);
        let store_dir = scratch.path().join("store");
        seed_installed(&store_dir, b"old");
        let staged = scratch.path().join("staged-update.bin");
        fs::write(&staged, b"first").unwrap();
        let first = envelope_signed_by(&key, NEW_VERSION, b"first");
        let first_job = prepare(&request(&first, &keys, &scratch, PlaybackState::Stopped)).unwrap();
        assert!(matches!(
            apply_blocking(&first_job, ample),
            UpdateOutcome::Applied { .. }
        ));
        assert!(matches!(
            rollback_blocking(&store_dir),
            UpdateOutcome::RolledBack { .. }
        ));
        let accepted = fs::read(store_dir.join("replay-state.json")).unwrap();

        // prepare deliberately does no disk I/O. The worker must recheck persisted state,
        // including when the UI prepared this signed manifest against a fresh in-memory state.
        fs::write(&staged, b"second").unwrap();
        let second = envelope_signed_by(&key, "2.2.0", b"second");
        let second_job =
            prepare(&request(&second, &keys, &scratch, PlaybackState::Stopped)).unwrap();
        assert!(
            matches!(apply_blocking(&second_job, ample), UpdateOutcome::Refused { reason, .. }
            if reason.contains("different contents"))
        );
        assert_eq!(
            fs::read(store_dir.join("replay-state.json")).unwrap(),
            accepted
        );
        assert_eq!(
            FsInstallStore::open(&store_dir)
                .unwrap()
                .installed()
                .unwrap()
                .unwrap()
                .bytes,
            b"old"
        );
    }

    #[test]
    fn corrupt_replay_state_refuses_install_without_overwriting_evidence() {
        let scratch = Scratch::new("corrupt-replay");
        let key = signer("corrupt-replay");
        let keys = keyring(&key);
        let store_dir = scratch.path().join("store");
        seed_installed(&store_dir, b"old");
        fs::write(store_dir.join("replay-state.json"), b"not json").unwrap();
        fs::write(scratch.path().join("staged-update.bin"), b"new").unwrap();
        let envelope = envelope_signed_by(&key, NEW_VERSION, b"new");
        let job = prepare(&request(&envelope, &keys, &scratch, PlaybackState::Stopped)).unwrap();
        assert!(matches!(
            apply_blocking(&job, ample),
            UpdateOutcome::Refused { .. }
        ));
        assert_eq!(
            fs::read(store_dir.join("replay-state.json")).unwrap(),
            b"not json"
        );
        assert_eq!(
            FsInstallStore::open(&store_dir)
                .unwrap()
                .installed()
                .unwrap()
                .unwrap()
                .bytes,
            b"old"
        );
    }

    #[test]
    fn concurrent_updater_job_is_refused_before_applying_or_advancing_replay_state() {
        let scratch = Scratch::new("busy-replay");
        let key = signer("busy-replay");
        let keys = keyring(&key);
        let store_dir = scratch.path().join("store");
        seed_installed(&store_dir, b"old");
        fs::write(scratch.path().join("staged-update.bin"), b"new").unwrap();
        let envelope = envelope_signed_by(&key, NEW_VERSION, b"new");
        let job = prepare(&request(&envelope, &keys, &scratch, PlaybackState::Stopped)).unwrap();
        let replay = FsReplayStateStore::new(store_dir.join("replay-state.json"));
        let _guard = replay.try_lock().unwrap();
        assert!(
            matches!(apply_blocking(&job, ample), UpdateOutcome::Refused { reason, .. }
            if reason.contains("lock"))
        );
        assert!(!replay.path().exists());
        assert_eq!(
            FsInstallStore::open(&store_dir)
                .unwrap()
                .installed()
                .unwrap()
                .unwrap()
                .bytes,
            b"old"
        );
    }

    #[test]
    fn low_disk_space_is_refused_and_the_installed_build_is_unchanged() {
        let scratch = Scratch::new("no-space");
        let key = signer("no-space");
        let keys = keyring(&key);
        let old_build = b"old build bytes";
        let new_build = b"new build";
        let store_dir = scratch.path().join("store");
        seed_installed(&store_dir, old_build);
        std::fs::write(scratch.path().join("staged-update.bin"), new_build).expect("stage");

        let envelope = envelope_signed_by(&key, NEW_VERSION, new_build);
        let req = request(&envelope, &keys, &scratch, PlaybackState::Stopped);
        let job = prepare(&req).expect("verified job");

        let outcome = apply_blocking(&job, none);
        assert!(
            matches!(&outcome, UpdateOutcome::Refused { version, reason }
                if version == NEW_VERSION && reason.contains("bytes")),
            "outcome was {outcome:?}"
        );
        let store = FsInstallStore::open(&store_dir).expect("reopen");
        assert_eq!(
            store.installed().expect("read").expect("installed").bytes,
            old_build
        );
        assert!(store.previous().expect("read").is_none());
    }

    #[test]
    fn unreadable_free_space_is_refused_not_guessed() {
        fn broken(_: &Path) -> Result<u64, String> {
            Err("probe unavailable".to_owned())
        }
        let scratch = Scratch::new("probe-broken");
        let key = signer("probe-broken");
        let keys = keyring(&key);
        let store_dir = scratch.path().join("store");
        seed_installed(&store_dir, b"old");
        std::fs::write(scratch.path().join("staged-update.bin"), b"new").expect("stage");

        let envelope = envelope_signed_by(&key, NEW_VERSION, b"new");
        let req = request(&envelope, &keys, &scratch, PlaybackState::Stopped);
        let job = prepare(&req).expect("verified job");

        assert!(matches!(
            apply_blocking(&job, broken),
            UpdateOutcome::Refused { .. }
        ));
    }

    #[test]
    fn rollback_without_a_pending_update_is_reported() {
        let scratch = Scratch::new("no-pending");
        let store_dir = scratch.path().join("store");
        seed_installed(&store_dir, b"old");
        assert!(matches!(
            rollback_blocking(&store_dir),
            UpdateOutcome::RollbackFailed { .. }
        ));
    }

    #[test]
    fn app_apply_while_playing_shows_deferred_and_writes_nothing() {
        let scratch = Scratch::new("app-deferred");
        let key = signer("app-deferred");
        let keys = keyring(&key);
        let envelope = envelope_signed_by(&key, NEW_VERSION, b"new");
        let mut app = crate::app::KattriApp::new(egui::Context::default());

        app.apply_update(request(&envelope, &keys, &scratch, PlaybackState::Playing));

        assert_eq!(
            app.update.status,
            UpdateStatus::Deferred {
                version: NEW_VERSION.to_owned()
            }
        );
        assert!(!app.update.is_busy());
        assert!(!scratch.path().join("store").exists());
    }

    #[test]
    fn state_moves_from_applying_to_applied_and_back_to_rolled_back() {
        let mut state = UpdateState::default();
        let store_dir = std::path::PathBuf::from("store");
        state.status = UpdateStatus::Applying {
            version: NEW_VERSION.to_owned(),
            store_dir: store_dir.clone(),
        };
        assert!(state.is_busy());

        state.finish(UpdateOutcome::Applied {
            version: NEW_VERSION.to_owned(),
        });
        assert_eq!(
            state.status,
            UpdateStatus::Applied {
                version: NEW_VERSION.to_owned(),
                store_dir: store_dir.clone(),
            }
        );
        assert!(!state.is_busy());

        state.status = UpdateStatus::RollingBack {
            version: NEW_VERSION.to_owned(),
            store_dir,
        };
        state.finish(UpdateOutcome::RolledBack {
            version: NEW_VERSION.to_owned(),
        });
        assert_eq!(
            state.status,
            UpdateStatus::RolledBack {
                version: NEW_VERSION.to_owned()
            }
        );
    }
}
