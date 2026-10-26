//! Semver comparison and the update decision.
//!
//! Uses the `semver` crate's precedence rules: `1.0.0-beta.1 < 1.0.0 < 1.0.1`, and
//! build metadata is ignored. Versions that fail to parse are errors, never guesses.

use semver::Version;

use crate::UpdateError;
use crate::manifest::UpdateManifest;

/// Parse a semver string, mapping failure to [`UpdateError::Version`].
pub fn parse_version(text: &str) -> Result<Version, UpdateError> {
    Version::parse(text).map_err(|_| UpdateError::Version {
        text: text.to_owned(),
    })
}

/// What the client should do after comparing its version with a manifest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateDecision {
    /// The offered version is not newer than the running one.
    UpToDate,
    /// A newer version is offered. The user may defer it.
    Available,
    /// The running version is below `minimum_version`. The update must be applied.
    Required,
}

/// Compare the running version against a manifest.
///
/// `Required` takes precedence over `Available`. A manifest whose version is not newer
/// than the running build is `UpToDate`, even if it carries a `minimum_version` the
/// running build fails to meet. That combination is malformed and should not be
/// published, so it is reported as `Required` only when the running version is below the minimum.
pub fn decide(current: &str, manifest: &UpdateManifest) -> Result<UpdateDecision, UpdateError> {
    let current = parse_version(current)?;
    let offered = parse_version(&manifest.version)?;
    if let Some(minimum) = &manifest.minimum_version {
        let minimum = parse_version(minimum)?;
        if current.cmp_precedence(&minimum).is_lt() {
            return Ok(UpdateDecision::Required);
        }
    }
    if offered.cmp_precedence(&current).is_gt() {
        Ok(UpdateDecision::Available)
    } else {
        Ok(UpdateDecision::UpToDate)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn order(a: &str, b: &str) -> std::cmp::Ordering {
        parse_version(a)
            .unwrap()
            .cmp_precedence(&parse_version(b).unwrap())
    }

    #[test]
    fn prerelease_sorts_before_release() {
        assert_eq!(order("1.0.0-beta.1", "1.0.0"), std::cmp::Ordering::Less);
        assert_eq!(order("1.0.0", "1.0.1"), std::cmp::Ordering::Less);
    }

    #[test]
    fn numeric_components_compare_numerically_not_lexically() {
        assert_eq!(order("1.10.0", "1.9.0"), std::cmp::Ordering::Greater);
        assert_eq!(order("2.0.0", "10.0.0"), std::cmp::Ordering::Less);
    }

    #[test]
    fn build_metadata_does_not_affect_precedence() {
        assert_eq!(order("1.0.0+abc", "1.0.0+xyz"), std::cmp::Ordering::Equal);
    }

    #[test]
    fn malformed_versions_are_errors() {
        assert_eq!(
            parse_version("1.2"),
            Err(UpdateError::Version {
                text: "1.2".to_owned()
            })
        );
        assert!(parse_version("v1.2.3").is_err());
    }

    fn manifest(version: &str, minimum: Option<&str>) -> UpdateManifest {
        let min = minimum.map_or(String::new(), |m| format!(r#""minimum_version":"{m}","#));
        let text = format!(
            r#"{{"schema_version":1,"channel":"stable","version":"{version}","sequence":1,
                "published":"2026-10-13",{min}
                "artifacts":[{{"platform":"windows","arch":"x64","format":"exe",
                "url":"https://dl.example.org/x.exe",
                "sha256":"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
                "size_bytes":1}}]}}"#
        );
        UpdateManifest::from_json(&text).unwrap()
    }

    #[test]
    fn decision_available_when_newer() {
        assert_eq!(
            decide("2.0.0", &manifest("2.1.0", None)).unwrap(),
            UpdateDecision::Available
        );
    }

    #[test]
    fn decision_up_to_date_when_equal_or_older() {
        assert_eq!(
            decide("2.1.0", &manifest("2.1.0", None)).unwrap(),
            UpdateDecision::UpToDate
        );
        assert_eq!(
            decide("2.2.0", &manifest("2.1.0", None)).unwrap(),
            UpdateDecision::UpToDate
        );
    }

    #[test]
    fn decision_required_below_minimum_version() {
        assert_eq!(
            decide("1.9.9", &manifest("2.1.0", Some("2.0.0"))).unwrap(),
            UpdateDecision::Required
        );
    }

    #[test]
    fn prerelease_running_build_is_older_than_its_release() {
        assert_eq!(
            decide("2.1.0-beta.2", &manifest("2.1.0", None)).unwrap(),
            UpdateDecision::Available
        );
    }

    #[test]
    fn unparseable_current_version_is_an_error() {
        assert!(decide("dev build", &manifest("2.1.0", None)).is_err());
    }
}
