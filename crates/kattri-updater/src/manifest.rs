//! Update manifest: the payload the update server signs (M4-15).
//!
//! The payload is only trusted after [`crate::verify::open_envelope`] has checked its
//! Ed25519 signature over the exact bytes. [`UpdateManifest::from_json`] parses unsigned
//! JSON and must not be used as a trust path on its own.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::UpdateError;
use crate::version::parse_version;

/// The only manifest schema this client understands. Higher values are refused.
pub const SCHEMA_VERSION: u32 = 1;

/// Largest artifact the client will accept (M4-15 cap).
pub const MAX_ARTIFACT_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// Release channel. A client only accepts manifests for its own channel.
///
/// The wire value of `Nightly` is `nightly`. The earlier wire value `beta` is still accepted
/// when reading and maps to `Nightly`. Writing always uses `nightly`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Channel {
    Stable,
    #[serde(alias = "beta")]
    Nightly,
}

/// One downloadable build for one platform and architecture.
///
/// There is no per-artifact signature (D9): the signed manifest covers `sha256` and `size_bytes`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Artifact {
    /// `windows`, `macos` or `linux`.
    pub platform: String,
    /// `x64`, `arm64`, or `universal` (macOS only).
    pub arch: String,
    /// Informational package format: `msi`, `exe`, `dmg`, `appimage`, `deb`, `rpm` or `flatpak_ref`.
    pub format: String,
    /// Must be `https://`.
    pub url: String,
    /// Lowercase hex SHA-256 of the artifact file (64 characters).
    pub sha256: String,
    /// Exact byte count of the artifact file. At most [`MAX_ARTIFACT_BYTES`].
    pub size_bytes: u64,
    /// Version embedded in the artifact, to catch mislabelled files. Optional.
    #[serde(default)]
    pub product_version: Option<String>,
}

/// The payload served for one channel, inside the signed envelope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateManifest {
    /// Must equal [`SCHEMA_VERSION`].
    pub schema_version: u32,
    pub channel: Channel,
    /// Release version, semver.
    pub version: String,
    /// Monotonic per channel. Used for replay defense.
    pub sequence: u64,
    /// Release date as `YYYY-MM-DD`. Informational only; never used to decide expiry.
    pub published: String,
    /// Versions older than this must update before continuing. Advisory (D10 is open).
    #[serde(default)]
    pub minimum_version: Option<String>,
    #[serde(default)]
    pub notes_url: Option<String>,
    pub artifacts: Vec<Artifact>,
}

impl UpdateManifest {
    /// Parse and validate a manifest from JSON.
    ///
    /// This does not check any signature. Use [`crate::verify::open_envelope`] for input
    /// that came from the network.
    pub fn from_json(text: &str) -> Result<Self, UpdateError> {
        let manifest: Self =
            serde_json::from_str(text).map_err(|e| UpdateError::Json(e.to_string()))?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// Check every field. Returns the first violation.
    pub fn validate(&self) -> Result<(), UpdateError> {
        if self.schema_version != SCHEMA_VERSION {
            return Err(invalid("schema_version", "unsupported schema version"));
        }
        parse_version(&self.version)?;
        if let Some(minimum) = &self.minimum_version {
            parse_version(minimum)?;
        }
        if !is_iso_date(&self.published) {
            return Err(invalid("published", "must be YYYY-MM-DD"));
        }
        if let Some(url) = &self.notes_url
            && !url.starts_with("https://")
        {
            return Err(invalid("notes_url", "must be an https:// URL"));
        }
        if self.artifacts.is_empty() {
            return Err(invalid("artifacts", "must list at least one artifact"));
        }

        let mut seen = HashSet::new();
        for artifact in &self.artifacts {
            artifact.validate()?;
            if !seen.insert((artifact.platform.as_str(), artifact.arch.as_str())) {
                return Err(invalid(
                    "artifacts",
                    "has two entries for the same platform and arch",
                ));
            }
        }
        Ok(())
    }

    /// Find the artifact for a platform and architecture, if this release ships one.
    pub fn artifact_for(&self, platform: &str, arch: &str) -> Option<&Artifact> {
        self.artifacts
            .iter()
            .find(|a| a.platform == platform && a.arch == arch)
    }
}

impl Artifact {
    fn validate(&self) -> Result<(), UpdateError> {
        let platform_ok = matches!(self.platform.as_str(), "windows" | "macos" | "linux");
        if !platform_ok {
            return Err(invalid(
                "artifacts.platform",
                "must be windows, macos or linux",
            ));
        }
        let arch_ok = match self.arch.as_str() {
            "x64" | "arm64" => true,
            "universal" => self.platform == "macos",
            _ => false,
        };
        if !arch_ok {
            return Err(invalid(
                "artifacts.arch",
                "must be x64 or arm64 (universal for macos only)",
            ));
        }
        let format_ok = matches!(
            self.format.as_str(),
            "msi" | "exe" | "dmg" | "appimage" | "deb" | "rpm" | "flatpak_ref"
        );
        if !format_ok {
            return Err(invalid("artifacts.format", "is not a known package format"));
        }
        if !self.url.starts_with("https://") {
            return Err(invalid("artifacts.url", "must be an https:// URL"));
        }
        let digest_ok = self.sha256.len() == 64
            && self
                .sha256
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        if !digest_ok {
            return Err(invalid(
                "artifacts.sha256",
                "must be 64 lowercase hex characters",
            ));
        }
        if self.size_bytes == 0 || self.size_bytes > MAX_ARTIFACT_BYTES {
            return Err(invalid(
                "artifacts.size_bytes",
                "must be between 1 and 2 GiB",
            ));
        }
        if let Some(product) = &self.product_version {
            parse_version(product)?;
        }
        Ok(())
    }
}

fn invalid(field: &'static str, reason: &str) -> UpdateError {
    UpdateError::Invalid {
        field,
        reason: reason.to_owned(),
    }
}

/// `YYYY-MM-DD` shape check. Does not validate the calendar.
fn is_iso_date(text: &str) -> bool {
    let b = text.as_bytes();
    b.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && b.iter()
            .enumerate()
            .all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIGEST: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn manifest(extra_artifact: &str) -> String {
        format!(
            r#"{{
                "schema_version": 1,
                "channel": "stable",
                "version": "2.1.0",
                "sequence": 7,
                "published": "2026-10-13",
                "minimum_version": "2.0.0",
                "artifacts": [
                    {{"platform":"windows","arch":"x64","format":"msi",
                      "url":"https://dl.example.org/k-2.1.0-win-x64.msi",
                      "sha256":"{DIGEST}","size_bytes":1024}}
                    {extra_artifact}
                ]
            }}"#
        )
    }

    #[test]
    fn valid_manifest_parses_and_selects_artifact() {
        let m = UpdateManifest::from_json(&manifest("")).unwrap();
        assert_eq!(m.channel, Channel::Stable);
        assert_eq!(m.sequence, 7);
        assert!(m.artifact_for("windows", "x64").is_some());
        assert!(m.artifact_for("windows", "arm64").is_none());
    }

    #[test]
    fn nightly_wire_value_is_nightly_and_legacy_beta_maps_to_nightly() {
        let nightly = manifest("").replace("\"stable\"", "\"nightly\"");
        let parsed = UpdateManifest::from_json(&nightly).unwrap();
        assert_eq!(parsed.channel, Channel::Nightly);

        let legacy = manifest("").replace("\"stable\"", "\"beta\"");
        let parsed = UpdateManifest::from_json(&legacy).unwrap();
        assert_eq!(parsed.channel, Channel::Nightly);
    }

    #[test]
    fn channel_serializes_with_the_nightly_wire_value() {
        assert_eq!(
            serde_json::to_string(&Channel::Nightly).unwrap(),
            "\"nightly\""
        );
        assert_eq!(
            serde_json::to_string(&Channel::Stable).unwrap(),
            "\"stable\""
        );
    }

    #[test]
    fn unknown_channel_is_rejected() {
        let text = manifest("").replace("\"stable\"", "\"canary\"");
        assert!(matches!(
            UpdateManifest::from_json(&text),
            Err(UpdateError::Json(_))
        ));
    }

    #[test]
    fn unsupported_schema_version_is_rejected() {
        let text = manifest("").replace("\"schema_version\": 1", "\"schema_version\": 2");
        assert!(matches!(
            UpdateManifest::from_json(&text),
            Err(UpdateError::Invalid {
                field: "schema_version",
                ..
            })
        ));
    }

    #[test]
    fn http_artifact_urls_are_rejected() {
        let text = manifest("").replace("https://dl.example.org", "http://dl.example.org");
        assert!(matches!(
            UpdateManifest::from_json(&text),
            Err(UpdateError::Invalid {
                field: "artifacts.url",
                ..
            })
        ));
    }

    #[test]
    fn digest_must_be_64_lowercase_hex() {
        for bad in ["ABCDEF", &DIGEST.to_uppercase(), &DIGEST[..63]] {
            let text = manifest("").replace(DIGEST, bad);
            assert!(matches!(
                UpdateManifest::from_json(&text),
                Err(UpdateError::Invalid {
                    field: "artifacts.sha256",
                    ..
                })
            ));
        }
    }

    #[test]
    fn size_over_the_cap_is_rejected() {
        let text = manifest("").replace("1024", &(MAX_ARTIFACT_BYTES + 1).to_string());
        assert!(matches!(
            UpdateManifest::from_json(&text),
            Err(UpdateError::Invalid {
                field: "artifacts.size_bytes",
                ..
            })
        ));
    }

    #[test]
    fn universal_arch_is_only_for_macos() {
        let text = manifest("").replace(
            "\"windows\",\"arch\":\"x64\"",
            "\"windows\",\"arch\":\"universal\"",
        );
        assert!(matches!(
            UpdateManifest::from_json(&text),
            Err(UpdateError::Invalid {
                field: "artifacts.arch",
                ..
            })
        ));
    }

    #[test]
    fn duplicate_platform_arch_is_rejected() {
        let dup = format!(
            r#",{{"platform":"windows","arch":"x64","format":"exe","url":"https://dl.example.org/b.exe","sha256":"{DIGEST}","size_bytes":1}}"#
        );
        assert!(matches!(
            UpdateManifest::from_json(&manifest(&dup)),
            Err(UpdateError::Invalid {
                field: "artifacts",
                ..
            })
        ));
    }

    #[test]
    fn unknown_platform_is_rejected() {
        let text = manifest("").replace("\"windows\"", "\"plan9\"");
        assert!(matches!(
            UpdateManifest::from_json(&text),
            Err(UpdateError::Invalid {
                field: "artifacts.platform",
                ..
            })
        ));
    }

    #[test]
    fn bad_published_date_is_rejected() {
        let text = manifest("").replace("2026-10-13", "13/10/2026");
        assert!(matches!(
            UpdateManifest::from_json(&text),
            Err(UpdateError::Invalid {
                field: "published",
                ..
            })
        ));
    }

    #[test]
    fn non_semver_minimum_version_is_rejected() {
        let text = manifest("").replace("\"2.0.0\"", "\"two\"");
        assert!(matches!(
            UpdateManifest::from_json(&text),
            Err(UpdateError::Version { .. })
        ));
    }
}
