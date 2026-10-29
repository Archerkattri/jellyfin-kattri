//! Apply and rollback for a verified update (M4-16). LOCAL LOGIC ONLY.
//!
//! The filesystem side is behind [`InstallStore`]. Tests use [`memory::MemoryStore`]. The
//! real per-OS file operations (Windows, macOS, Linux) are not implemented here and are
//! NOT verified on any OS.
//!
//! Apply order (ADR 0004 sections 10 and 11):
//! 1. Select the artifact for the platform and arch, and check its size and SHA-256 against
//!    the verified manifest. Nothing is written if this fails.
//! 2. Stage the bytes, read them back, and check the digest again.
//! 3. Write the journal: the pending target, the previous version and its digest. This is
//!    the record of the previous version that rollback needs.
//! 4. Commit: the staged bytes become installed and the old installed build is kept as
//!    previous. The store does this in one call. The real store must make it atomic.
//!
//! Health (ADR section 11): each launch of the target version that starts without a healthy
//! marker counts as one launch. [`mark_healthy`] is called by the app after the main window
//! is shown and 60 seconds have passed. After [`MAX_FAILED_LAUNCHES`] such launches,
//! [`on_launch`] reports a restore offer. [`rollback`] runs only when the user accepts it.
//! ADR section 11 says rollback is not silent, so this module never rolls back on its own.
//!
//! Error messages never include payload or signature content.

use std::fmt::Write as _;

use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::UpdateError;
use crate::manifest::Artifact;
use crate::verify::{VerifiedManifest, verify_artifact_bytes};
use crate::version::parse_version;

/// Launches of the target version without a healthy marker before a restore is offered (ADR 0004 section 11).
pub const MAX_FAILED_LAUNCHES: u32 = 2;

/// One stored build: its version and its exact bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Slot {
    pub version: String,
    pub bytes: Vec<u8>,
}

/// An update that has been committed but not yet confirmed healthy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pending {
    pub target: String,
    pub previous: String,
    /// SHA-256 (lowercase hex) of the previous build, recorded before the switch.
    pub previous_sha256: String,
    /// Launches of the target that started without a healthy marker.
    pub failed_launches: u32,
}

/// The persisted update journal.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Journal {
    pub pending: Option<Pending>,
    /// The version whose healthy marker was last written.
    pub healthy: Option<String>,
    /// Versions whose update was rolled back. They are not offered again automatically.
    pub failed_targets: Vec<String>,
    /// True when the previous launch of the pending target ended cleanly. Such a launch is not
    /// counted as a failed health check, so ordinary quick quits cannot trigger a rollback.
    pub clean_shutdown: bool,
}

/// A failure in the install store. The message describes the operation, never the bytes.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("install store failed: {0}")]
pub struct StoreError(pub String);

/// Errors from apply and rollback.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ApplyError {
    #[error(transparent)]
    Verify(#[from] UpdateError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("the manifest has no artifact for {platform}/{arch}")]
    NoArtifact { platform: String, arch: String },
    #[error("offered version {offered} is not newer than installed version {installed}")]
    NotNewer { installed: String, offered: String },
    #[error("an update to {target} is already pending")]
    AlreadyPending { target: String },
    #[error("version {version} was rolled back earlier and is not offered again")]
    PreviouslyFailed { version: String },
    #[error("no installed build is recorded")]
    NoInstalledArtifact,
    #[error("the staged bytes do not match the manifest after staging")]
    StagedDigestMismatch,
    #[error("no update is pending")]
    NoPendingUpdate,
    #[error("no previous build is kept")]
    NoPrevious,
    #[error(
        "the kept previous build does not match the digest recorded when the update was applied"
    )]
    PreviousMismatch,
    #[error("staging needs {required} free bytes, but {free} bytes are free")]
    InsufficientSpace { required: u64, free: u64 },
}

/// The filesystem side of install, behind a trait so the logic can be tested without touching disk.
///
/// `installed` and `previous` hold the bytes the store keeps. A real store would keep paths and
/// hash the files it reads. `commit_staged` and `swap_with_previous` must each be all-or-nothing.
pub trait InstallStore {
    fn installed(&self) -> Result<Option<Slot>, StoreError>;
    fn previous(&self) -> Result<Option<Slot>, StoreError>;
    fn staged(&self) -> Result<Option<Slot>, StoreError>;
    fn stage(&mut self, slot: Slot) -> Result<(), StoreError>;
    fn discard_staged(&mut self) -> Result<(), StoreError>;
    /// The staged build becomes installed, and the old installed build becomes previous.
    fn commit_staged(&mut self) -> Result<(), StoreError>;
    /// Atomically install the kept previous build. It remains in `previous` until
    /// `drop_previous` after the rollback journal has been committed.
    fn swap_with_previous(&mut self) -> Result<(), StoreError>;
    fn drop_previous(&mut self) -> Result<(), StoreError>;
    fn journal(&self) -> Result<Journal, StoreError>;
    fn set_journal(&mut self, journal: Journal) -> Result<(), StoreError>;
}

/// Apply a verified update whose artifact bytes were downloaded for `platform` and `arch`.
///
/// Returns `Ok` only after the staged build is committed. On any error the installed build is
/// unchanged and the staged bytes are discarded when possible.
pub fn apply_verified<S: InstallStore>(
    store: &mut S,
    verified: &VerifiedManifest,
    platform: &str,
    arch: &str,
    bytes: &[u8],
) -> Result<(), ApplyError> {
    let manifest = verified.manifest();
    let artifact = manifest
        .artifact_for(platform, arch)
        .ok_or_else(|| ApplyError::NoArtifact {
            platform: platform.to_owned(),
            arch: arch.to_owned(),
        })?;

    // Checks before any write: the bytes first, then the state.
    verify_artifact_bytes(artifact, bytes)?;

    let journal = store.journal()?;
    if let Some(pending) = &journal.pending {
        return Err(ApplyError::AlreadyPending {
            target: pending.target.clone(),
        });
    }
    if journal.failed_targets.contains(&manifest.version) {
        return Err(ApplyError::PreviouslyFailed {
            version: manifest.version.clone(),
        });
    }
    let installed = store.installed()?.ok_or(ApplyError::NoInstalledArtifact)?;
    let offered = parse_version(&manifest.version)?;
    let installed_version = parse_version(&installed.version)?;
    if offered.cmp_precedence(&installed_version).is_le() {
        return Err(ApplyError::NotNewer {
            installed: installed.version.clone(),
            offered: manifest.version.clone(),
        });
    }

    if let Err(err) = switch_over(
        store,
        artifact,
        &installed,
        &journal,
        &manifest.version,
        bytes,
    ) {
        // Best effort: the staged bytes are not the installed build, so leaving them is harmless,
        // but discarding keeps the store tidy.
        let _ = store.discard_staged();
        return Err(err);
    }
    Ok(())
}

fn switch_over<S: InstallStore>(
    store: &mut S,
    artifact: &Artifact,
    installed: &Slot,
    journal: &Journal,
    target: &str,
    bytes: &[u8],
) -> Result<(), ApplyError> {
    store.stage(Slot {
        version: target.to_owned(),
        bytes: bytes.to_vec(),
    })?;

    // Read the staged copy back and check it again before the switch.
    let readback_ok = match store.staged()? {
        Some(slot) => verify_artifact_bytes(artifact, &slot.bytes).is_ok(),
        None => false,
    };
    if !readback_ok {
        return Err(ApplyError::StagedDigestMismatch);
    }

    let mut next = journal.clone();
    next.pending = Some(Pending {
        target: target.to_owned(),
        previous: installed.version.clone(),
        previous_sha256: sha256_hex(&installed.bytes),
        failed_launches: 0,
    });
    store.set_journal(next)?;

    if let Err(err) = store.commit_staged() {
        // The switch did not happen, so the journal must not claim an update is pending.
        let _ = store.set_journal(journal.clone());
        return Err(err.into());
    }
    Ok(())
}

/// What the app should do after a launch, from [`on_launch`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaunchOutcome {
    /// No update is pending.
    Settled,
    /// The pending update never reached the commit. The installed build is unchanged and the
    /// pending record was cleared.
    AbandonedUpdate { target: String },
    /// The target is running without a healthy marker so far. Count recorded.
    HealthCheckPending {
        target: String,
        failed_launches: u32,
    },
    /// The target failed to write its healthy marker on enough launches. The app should offer
    /// to restore `restore_to`. It must not restore without the user's choice.
    RollbackOffered { target: String, restore_to: String },
}

/// Check the journal at launch. Call once per launch, before the main window is shown.
pub fn on_launch<S: InstallStore>(
    store: &mut S,
    running: &str,
) -> Result<LaunchOutcome, ApplyError> {
    let mut journal = store.journal()?;
    let Some(pending) = journal.pending.clone() else {
        return Ok(LaunchOutcome::Settled);
    };

    if running != pending.target {
        journal.pending = None;
        store.set_journal(journal)?;
        return Ok(LaunchOutcome::AbandonedUpdate {
            target: pending.target,
        });
    }

    // A launch only counts as a failed health check when the previous launch neither wrote the
    // healthy marker nor recorded a clean shutdown.
    let failed_launches = if journal.clean_shutdown {
        pending.failed_launches
    } else {
        pending.failed_launches.saturating_add(1)
    };
    journal.clean_shutdown = false;
    let target = pending.target.clone();
    journal.pending = Some(Pending {
        failed_launches,
        ..pending.clone()
    });
    store.set_journal(journal)?;

    if failed_launches >= MAX_FAILED_LAUNCHES {
        Ok(LaunchOutcome::RollbackOffered {
            target,
            restore_to: pending.previous,
        })
    } else {
        Ok(LaunchOutcome::HealthCheckPending {
            target,
            failed_launches,
        })
    }
}

/// Record that the running build is shutting down normally.
///
/// The next [`on_launch`] then does not count this launch as a failed health check, so opening
/// and quitting the app before the 60-second marker cannot accumulate into a rollback offer.
/// Returns `false` when no update is pending.
pub fn note_clean_shutdown<S: InstallStore>(store: &mut S) -> Result<bool, ApplyError> {
    let mut journal = store.journal()?;
    if journal.pending.is_none() {
        return Ok(false);
    }
    journal.clean_shutdown = true;
    store.set_journal(journal)?;
    Ok(true)
}

/// Record the healthy marker for `running` and drop the kept previous build.
///
/// Returns `false` when no update is pending for `running`. The app calls this only after the
/// main window is shown and 60 seconds have passed (ADR section 11).
pub fn mark_healthy<S: InstallStore>(store: &mut S, running: &str) -> Result<bool, ApplyError> {
    let mut journal = store.journal()?;
    let Some(pending) = journal.pending.clone() else {
        return Ok(false);
    };
    if running != pending.target {
        return Ok(false);
    }
    journal.pending = None;
    journal.healthy = Some(pending.target);
    store.set_journal(journal)?;
    store.drop_previous()?;
    Ok(true)
}

/// Restore the previous build for a pending update the user chose to roll back.
///
/// The kept previous build must match the digest recorded at apply time. The failed target is
/// added to `failed_targets`, and its bytes are dropped.
pub fn rollback<S: InstallStore>(store: &mut S) -> Result<(), ApplyError> {
    let mut journal = store.journal()?;
    let Some(pending) = journal.pending.clone() else {
        return Err(ApplyError::NoPendingUpdate);
    };
    let previous = store.previous()?.ok_or(ApplyError::NoPrevious)?;
    // Trust boundary: the digest and the slot live in the same user-writable store, so this
    // check detects corruption and partial writes, not an attacker who can already rewrite the
    // installed build. Authenticating a rollback would require the previous build's signed
    // envelope, which the updater does not retain; see docs/BLOCKERS.md (M4-16).
    if previous.version != pending.previous
        || sha256_hex(&previous.bytes) != pending.previous_sha256
    {
        return Err(ApplyError::PreviousMismatch);
    }

    store.swap_with_previous()?;

    journal.pending = None;
    if !journal.failed_targets.contains(&pending.target) {
        journal.failed_targets.push(pending.target);
    }
    store.set_journal(journal)?;
    store.drop_previous()?;
    Ok(())
}

/// Lowercase hex SHA-256 of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .fold(String::with_capacity(64), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

/// In-memory store and fault injection, for tests. Not used by the app.
pub mod memory {
    use super::{InstallStore, Journal, Slot, StoreError};

    /// An operation the double can be told to fail or corrupt.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Op {
        Stage,
        /// Stage the bytes with the first byte flipped, to test the read-back digest check.
        CorruptStage,
        SetJournal,
        Commit,
        SwapPrevious,
        DropPrevious,
    }

    /// An in-memory [`InstallStore`]. Every failing operation changes nothing.
    #[derive(Debug, Clone, Default, PartialEq, Eq)]
    pub struct MemoryStore {
        installed: Option<Slot>,
        previous: Option<Slot>,
        staged: Option<Slot>,
        journal: Journal,
        fail_on: Vec<Op>,
        writes: usize,
    }

    impl MemoryStore {
        /// A store whose installed build is `version` with `bytes`.
        pub fn with_installed(version: &str, bytes: &[u8]) -> Self {
            Self {
                installed: Some(Slot {
                    version: version.to_owned(),
                    bytes: bytes.to_vec(),
                }),
                ..Self::default()
            }
        }

        /// Make every later call to `op` fail (or corrupt, for `CorruptStage`).
        pub fn fail_on(&mut self, op: Op) {
            if !self.fail_on.contains(&op) {
                self.fail_on.push(op);
            }
        }

        /// Stop injecting failures for `op`, allowing a retry after the test removes its fault.
        pub fn clear_failure(&mut self, op: Op) {
            self.fail_on.retain(|failed_op| *failed_op != op);
        }

        /// Number of write calls made so far, including failed ones that got past their check.
        pub fn writes(&self) -> usize {
            self.writes
        }

        pub fn installed_slot(&self) -> Option<&Slot> {
            self.installed.as_ref()
        }

        pub fn previous_slot(&self) -> Option<&Slot> {
            self.previous.as_ref()
        }

        pub fn staged_slot(&self) -> Option<&Slot> {
            self.staged.as_ref()
        }

        /// Flip the first byte of the kept previous build, to test the rollback digest check.
        pub fn tamper_previous(&mut self) {
            if let Some(b) = self.previous.as_mut().and_then(|s| s.bytes.first_mut()) {
                *b ^= 0xff;
            }
        }

        fn check(&self, op: Op) -> Result<(), StoreError> {
            if self.fail_on.contains(&op) {
                Err(StoreError(format!("injected failure at {op:?}")))
            } else {
                Ok(())
            }
        }
    }

    impl InstallStore for MemoryStore {
        fn installed(&self) -> Result<Option<Slot>, StoreError> {
            Ok(self.installed.clone())
        }

        fn previous(&self) -> Result<Option<Slot>, StoreError> {
            Ok(self.previous.clone())
        }

        fn staged(&self) -> Result<Option<Slot>, StoreError> {
            Ok(self.staged.clone())
        }

        fn stage(&mut self, slot: Slot) -> Result<(), StoreError> {
            self.check(Op::Stage)?;
            self.writes += 1;
            let mut slot = slot;
            if self.fail_on.contains(&Op::CorruptStage)
                && let Some(b) = slot.bytes.first_mut()
            {
                *b ^= 0xff;
            }
            self.staged = Some(slot);
            Ok(())
        }

        fn discard_staged(&mut self) -> Result<(), StoreError> {
            self.writes += 1;
            self.staged = None;
            Ok(())
        }

        fn commit_staged(&mut self) -> Result<(), StoreError> {
            self.check(Op::Commit)?;
            let Some(staged) = self.staged.take() else {
                return Err(StoreError("nothing is staged".to_owned()));
            };
            self.writes += 1;
            self.previous = self.installed.replace(staged);
            Ok(())
        }

        fn swap_with_previous(&mut self) -> Result<(), StoreError> {
            self.check(Op::SwapPrevious)?;
            let Some(previous) = self.previous.clone() else {
                return Err(StoreError("no previous build is kept".to_owned()));
            };
            self.writes += 1;
            self.installed = Some(previous);
            Ok(())
        }

        fn drop_previous(&mut self) -> Result<(), StoreError> {
            self.check(Op::DropPrevious)?;
            self.writes += 1;
            self.previous = None;
            Ok(())
        }

        fn journal(&self) -> Result<Journal, StoreError> {
            Ok(self.journal.clone())
        }

        fn set_journal(&mut self, journal: Journal) -> Result<(), StoreError> {
            self.check(Op::SetJournal)?;
            self.writes += 1;
            self.journal = journal;
            Ok(())
        }
    }
}
