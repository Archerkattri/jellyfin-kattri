//! Install state the verifier checks against: the installed version and, per channel,
//! the highest manifest sequence accepted so far.
//!
//! [`crate::verify::open_envelope`] takes this state by shared reference and never
//! changes it. Only [`InstallState::record`], called with a verified manifest, advances
//! the stored sequence. The installed version is set for the running build on creation or reload.

use std::collections::HashMap;

use semver::Version;

use crate::UpdateError;
use crate::manifest::Channel;
use crate::verify::VerifiedManifest;
use crate::version::parse_version;

/// The last manifest accepted on one channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Accepted {
    pub sequence: u64,
    /// SHA-256 of the signed payload bytes, used for the identical re-check.
    pub payload_sha256: [u8; 32],
}

/// Installed version and per-channel replay state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallState {
    installed: Version,
    accepted: HashMap<Channel, Accepted>,
}

impl InstallState {
    /// A fresh state for a build whose installed version is `installed_version`.
    pub fn new(installed_version: &str) -> Result<Self, UpdateError> {
        Ok(Self {
            installed: parse_version(installed_version)?,
            accepted: HashMap::new(),
        })
    }

    /// The installed version.
    pub fn installed_version(&self) -> &Version {
        &self.installed
    }

    /// The last accepted manifest on `channel`, if any.
    pub fn accepted(&self, channel: Channel) -> Option<&Accepted> {
        self.accepted.get(&channel)
    }

    /// Recheck a previously verified manifest against this state's durable replay floor.
    pub fn check_verified(&self, verified: &VerifiedManifest) -> Result<(), UpdateError> {
        crate::verify::check_verified_state(self, verified)
    }

    /// Advance the stored sequence for the manifest's channel.
    ///
    /// Only a [`VerifiedManifest`] can reach this, so the stored value moves only after
    /// signature and field checks pass. A lower sequence never overwrites a higher one.
    pub fn record(&mut self, verified: &VerifiedManifest) {
        let manifest = verified.manifest();
        let next = Accepted {
            sequence: manifest.sequence,
            payload_sha256: *verified.payload_sha256(),
        };
        let advance = self
            .accepted
            .get(&manifest.channel)
            .is_none_or(|last| next.sequence > last.sequence);
        if advance {
            self.accepted.insert(manifest.channel, next);
        }
    }

    pub(crate) fn from_accepted(
        installed_version: &str,
        accepted: HashMap<Channel, Accepted>,
    ) -> Result<Self, UpdateError> {
        Ok(Self {
            installed: parse_version(installed_version)?,
            accepted,
        })
    }

    pub(crate) fn accepted_entries(&self) -> &HashMap<Channel, Accepted> {
        &self.accepted
    }
}
