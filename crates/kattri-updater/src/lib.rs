//! Update manifest model, version decisions and signed-manifest verification.
//!
//! Status (M4 auto-update):
//! - Implemented and tested (M4-15): the manifest schema and validation, semver decision,
//!   Ed25519 verification of the signed envelope over the exact payload bytes, a keyring
//!   with active/pending/retired keys, anti-downgrade and per-channel replay checks, and
//!   SHA-256 and size checks for artifact bytes.
//! - Implemented (M4-16): [`apply`] checks the artifact before any write, stages and re-checks
//!   it, journals the previous version, commits, tracks the healthy marker and offers rollback.
//!   It runs against an [`apply::InstallStore`] trait. [`FsInstallStore`] is the on-disk store
//!   (staging folder, sibling temp file, sync, rename, previous build kept). Its tests use temp
//!   directories and run on any OS. Installing a platform package is not part of this crate.
//! - Implemented (M4-16, pure): [`gate::playback_gate`] defers an update while media is open;
//!   [`space::check_staging_space`] refuses the staging write when free space is short. The
//!   caller supplies the playback state and the free space; this crate reads neither.
//! - Not here yet: fetching the manifest and downloading artifacts (network). The release
//!   keyring is empty until the owner's release key exists (D3), so a release build refuses
//!   every manifest.
//!
//! Trust rule: an artifact is installed only after its manifest envelope passes
//! [`open_envelope`] and the downloaded bytes pass [`verify_artifact_bytes`].

pub mod apply;
pub mod gate;
pub mod install_store;
pub mod keyring;
pub mod manifest;
pub mod space;
pub mod state;
pub mod state_store;
pub mod verify;
pub mod version;

pub use install_store::FsInstallStore;
pub use keyring::{KeyState, Keyring, StaticKey};
pub use manifest::{Artifact, Channel, UpdateManifest};
pub use state::{Accepted, InstallState};
pub use state_store::{FsReplayStateStore, ReplayStateStoreError};
pub use verify::{VerifiedManifest, open_envelope, verify_artifact_bytes};
pub use version::{UpdateDecision, decide, parse_version};

use thiserror::Error;

/// Errors from manifest parsing, version handling and verification.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum UpdateError {
    #[error("update manifest could not be parsed: {0}")]
    Json(String),
    #[error("invalid update manifest field `{field}`: {reason}")]
    Invalid { field: &'static str, reason: String },
    #[error("`{text}` is not a valid semver version")]
    Version { text: String },
    #[error("update envelope is malformed: {0}")]
    Envelope(String),
    #[error("update signing key `{kid}` is not recognized")]
    UnknownKey { kid: String },
    #[error("update signing key `{kid}` is retired; install the current release manually")]
    RetiredKey { kid: String },
    #[error(
        "update signing key `{kid}` has been revoked; download the current release manually from the project site"
    )]
    RevokedKey { kid: String },
    #[error("update keyring is invalid: {0}")]
    Keyring(String),
    #[error("update signature was not accepted")]
    VerificationFailed(String),
    #[error("update channel is `{found}`, expected `{expected}`")]
    ChannelMismatch { expected: String, found: String },
    #[error("update version {offered} is older than installed version {installed}")]
    Downgrade { installed: String, offered: String },
    #[error("update sequence {sequence} is older than accepted sequence {last_accepted} (replay)")]
    Replay { sequence: u64, last_accepted: u64 },
    #[error("update sequence {sequence} was already accepted with different contents")]
    SequenceConflict { sequence: u64 },
    #[error("{what} is larger than the {limit}-byte limit")]
    TooLarge { what: &'static str, limit: u64 },
    #[error("artifact size is {actual} bytes, manifest says {expected}")]
    SizeMismatch { expected: u64, actual: u64 },
    #[error("artifact digest does not match the manifest for {platform}/{arch}")]
    DigestMismatch { platform: String, arch: String },
    #[error("{0} is not implemented yet")]
    NotImplemented(&'static str),
}
