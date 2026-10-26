//! Verification of signed update manifests and downloaded artifact bytes (M4-15).
//!
//! Order of operations for an update (M4-16 installer must follow it):
//! 1. [`open_envelope`]: size cap, envelope shape, key id (unknown or retired refused),
//!    Ed25519 `verify_strict` over the exact payload bytes, then manifest validation,
//!    channel match, anti-downgrade and per-channel sequence (replay) checks.
//! 2. Download the artifact over HTTPS (M4-16, not in this crate).
//! 3. [`verify_artifact_bytes`]: size and SHA-256 must match the verified manifest.
//!    Apply must not run unless this returns `Ok`.
//! 4. [`InstallState::record`] after a successful verification, to advance the sequence.
//!
//! Error messages never include signature bytes or payload content.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use ed25519_dalek::Signature;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::UpdateError;
use crate::keyring::Keyring;
use crate::manifest::{Artifact, Channel, MAX_ARTIFACT_BYTES, UpdateManifest};
use crate::state::InstallState;
use crate::version::parse_version;

/// Largest signed envelope accepted, in bytes of JSON text. The manifest is smaller.
pub const MAX_ENVELOPE_BYTES: usize = 64 * 1024;

const ENVELOPE_VERSION: u32 = 1;
const ALG_ED25519: &str = "ed25519";
const SIGNATURE_LEN: usize = 64;

/// The signed envelope the update server publishes at the manifest URL.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    envelope_version: u32,
    alg: String,
    kid: String,
    /// Base64 of the exact manifest JSON bytes that were signed.
    payload: String,
    /// Base64 of the 64-byte Ed25519 signature over the decoded payload.
    sig: String,
}

/// A manifest whose signature and state checks have passed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedManifest {
    manifest: UpdateManifest,
    kid: String,
    payload_sha256: [u8; 32],
}

impl VerifiedManifest {
    /// The verified manifest.
    pub fn manifest(&self) -> &UpdateManifest {
        &self.manifest
    }

    /// The key id that signed it.
    pub fn kid(&self) -> &str {
        &self.kid
    }

    /// SHA-256 of the signed payload bytes.
    pub fn payload_sha256(&self) -> &[u8; 32] {
        &self.payload_sha256
    }
}

/// Verify a signed manifest envelope and check it against the install state.
///
/// `state` is read only. Nothing is recorded here: call [`InstallState::record`] after the
/// manifest is accepted.
pub fn open_envelope(
    keyring: &Keyring,
    envelope_text: &str,
    expected_channel: Channel,
    state: &InstallState,
) -> Result<VerifiedManifest, UpdateError> {
    if envelope_text.len() > MAX_ENVELOPE_BYTES {
        return Err(UpdateError::TooLarge {
            what: "update envelope",
            limit: MAX_ENVELOPE_BYTES as u64,
        });
    }
    let envelope: Envelope = serde_json::from_str(envelope_text)
        .map_err(|e| malformed(&format!("envelope is not valid JSON: {e}")))?;
    if envelope.envelope_version != ENVELOPE_VERSION {
        return Err(malformed("unsupported envelope_version"));
    }
    if envelope.alg != ALG_ED25519 {
        return Err(malformed("unsupported signature algorithm"));
    }

    let key = keyring.lookup(&envelope.kid)?;

    let sig_bytes = STANDARD
        .decode(envelope.sig.as_bytes())
        .map_err(|_| malformed("sig is not canonical base64"))?;
    let sig_array: [u8; SIGNATURE_LEN] = sig_bytes
        .as_slice()
        .try_into()
        .map_err(|_| malformed("signature must be exactly 64 bytes"))?;
    let payload = STANDARD
        .decode(envelope.payload.as_bytes())
        .map_err(|_| malformed("payload is not canonical base64"))?;

    let signature = Signature::from_bytes(&sig_array);
    key.verify_strict(&payload, &signature)
        .map_err(|_| UpdateError::VerificationFailed("signature not accepted".to_owned()))?;

    // Only now is the payload trusted enough to parse.
    let text = std::str::from_utf8(&payload)
        .map_err(|_| UpdateError::Json("manifest is not UTF-8".to_owned()))?;
    let manifest = UpdateManifest::from_json(text)?;

    if manifest.channel != expected_channel {
        return Err(UpdateError::ChannelMismatch {
            expected: channel_name(expected_channel).to_owned(),
            found: channel_name(manifest.channel).to_owned(),
        });
    }

    let payload_sha256: [u8; 32] = Sha256::digest(&payload).into();
    let verified = VerifiedManifest {
        manifest,
        kid: envelope.kid,
        payload_sha256,
    };
    check_verified_state(state, &verified)?;
    Ok(verified)
}

/// Revalidate downgrade and replay rules for a previously signature-verified manifest.
pub(crate) fn check_verified_state(
    state: &InstallState,
    verified: &VerifiedManifest,
) -> Result<(), UpdateError> {
    let manifest = verified.manifest();
    let installed = state.installed_version();
    let offered = parse_version(&manifest.version)?;
    if offered.cmp_precedence(installed).is_lt() {
        return Err(UpdateError::Downgrade {
            installed: installed.to_string(),
            offered: manifest.version.clone(),
        });
    }
    check_replay(state, manifest, verified.payload_sha256())
}

/// Apply the replay-floor rule shared by envelope verification and durable-state rechecks.
pub(crate) fn check_replay(
    state: &InstallState,
    manifest: &UpdateManifest,
    payload_sha256: &[u8; 32],
) -> Result<(), UpdateError> {
    if let Some(last) = state.accepted(manifest.channel) {
        if manifest.sequence < last.sequence {
            return Err(UpdateError::Replay {
                sequence: manifest.sequence,
                last_accepted: last.sequence,
            });
        }
        // Same sequence is accepted only as an identical re-check of the same bytes.
        if manifest.sequence == last.sequence && &last.payload_sha256 != payload_sha256 {
            return Err(UpdateError::SequenceConflict {
                sequence: manifest.sequence,
            });
        }
    }
    Ok(())
}

/// Check downloaded artifact bytes against the verified manifest entry.
///
/// Checks the exact byte count and the SHA-256 digest. Apply must not run unless this is `Ok`.
pub fn verify_artifact_bytes(artifact: &Artifact, bytes: &[u8]) -> Result<(), UpdateError> {
    let actual = u64::try_from(bytes.len()).map_err(|_| UpdateError::TooLarge {
        what: "artifact",
        limit: MAX_ARTIFACT_BYTES,
    })?;
    if actual > MAX_ARTIFACT_BYTES {
        return Err(UpdateError::TooLarge {
            what: "artifact",
            limit: MAX_ARTIFACT_BYTES,
        });
    }
    if actual != artifact.size_bytes {
        return Err(UpdateError::SizeMismatch {
            expected: artifact.size_bytes,
            actual,
        });
    }
    let digest = to_hex(&Sha256::digest(bytes));
    if digest != artifact.sha256 {
        return Err(UpdateError::DigestMismatch {
            platform: artifact.platform.clone(),
            arch: artifact.arch.clone(),
        });
    }
    Ok(())
}

fn malformed(reason: &str) -> UpdateError {
    UpdateError::Envelope(reason.to_owned())
}

fn channel_name(channel: Channel) -> &'static str {
    match channel {
        Channel::Stable => "stable",
        Channel::Nightly => "nightly",
    }
}

fn to_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}
