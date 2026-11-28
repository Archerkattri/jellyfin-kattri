//! Manual, informational GitHub Releases lookup; this is deliberately separate from updater.
use std::time::Duration;

use serde::Deserialize;

pub(crate) const API_URL: &str =
    "https://api.github.com/repos/Archerkattri/jellyfin-kattri/releases/latest";
pub(crate) const RELEASE_PAGE_URL: &str =
    "https://github.com/Archerkattri/jellyfin-kattri/releases";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(8);
pub(crate) const MAX_RESPONSE_BYTES: usize = 256 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ReleaseCheckError {
    Network,
    HttpStatus,
    TooLarge,
    InvalidResponse,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ReleaseCheckResult {
    Current { latest_version: String },
    Newer { latest_version: String },
}

#[derive(Deserialize)]
struct GithubRelease {
    tag_name: String,
    draft: bool,
    prerelease: bool,
    html_url: String,
}

pub(crate) fn parse_latest_release(
    bytes: &[u8],
    current_version: &str,
) -> Result<ReleaseCheckResult, ReleaseCheckError> {
    if bytes.len() > MAX_RESPONSE_BYTES {
        return Err(ReleaseCheckError::TooLarge);
    }
    let release: GithubRelease =
        serde_json::from_slice(bytes).map_err(|_| ReleaseCheckError::InvalidResponse)?;
    if release.draft
        || release.prerelease
        || release.html_url != RELEASE_PAGE_URL
            && release.html_url != format!("{RELEASE_PAGE_URL}/tag/{}", release.tag_name)
    {
        return Err(ReleaseCheckError::InvalidResponse);
    }
    let Some(version) = release.tag_name.strip_prefix('v') else {
        return Err(ReleaseCheckError::InvalidResponse);
    };
    let parsed = semver::Version::parse(version).map_err(|_| ReleaseCheckError::InvalidResponse)?;
    if !parsed.pre.is_empty() {
        return Err(ReleaseCheckError::InvalidResponse);
    }
    let current =
        semver::Version::parse(current_version).map_err(|_| ReleaseCheckError::InvalidResponse)?;
    let latest_version = parsed.to_string();
    if parsed > current {
        Ok(ReleaseCheckResult::Newer { latest_version })
    } else {
        Ok(ReleaseCheckResult::Current { latest_version })
    }
}

pub(crate) async fn fetch_latest(
    current_version: &str,
) -> Result<ReleaseCheckResult, ReleaseCheckError> {
    let client = reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(concat!("Kattri/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|_| ReleaseCheckError::Network)?;
    let response = client
        .get(API_URL)
        .send()
        .await
        .map_err(|_| ReleaseCheckError::Network)?;
    if response.status() != reqwest::StatusCode::OK {
        return Err(ReleaseCheckError::HttpStatus);
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
    {
        return Err(ReleaseCheckError::TooLarge);
    }
    let mut response = response;
    let mut body = Vec::with_capacity(4096);
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| ReleaseCheckError::Network)?
    {
        if body.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
            return Err(ReleaseCheckError::TooLarge);
        }
        body.extend_from_slice(&chunk);
    }
    parse_latest_release(&body, current_version)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(tag: &str, draft: bool, prerelease: bool, url: &str) -> Vec<u8> {
        serde_json::json!({"tag_name":tag,"draft":draft,"prerelease":prerelease,"html_url":url})
            .to_string()
            .into_bytes()
    }

    #[test]
    fn detects_current_and_newer_release() {
        assert_eq!(
            parse_latest_release(&response("v1.0.1", false, false, RELEASE_PAGE_URL), "1.0.1"),
            Ok(ReleaseCheckResult::Current {
                latest_version: "1.0.1".into()
            })
        );
        assert_eq!(
            parse_latest_release(&response("v1.0.1", false, false, RELEASE_PAGE_URL), "1.0.0"),
            Ok(ReleaseCheckResult::Newer {
                latest_version: "1.0.1".into()
            })
        );
    }

    #[test]
    fn rejects_oversized_invalid_tag_repository_url_draft_and_prerelease() {
        assert_eq!(
            parse_latest_release(&vec![b' '; MAX_RESPONSE_BYTES + 1], "1.0.0"),
            Err(ReleaseCheckError::TooLarge)
        );
        assert_eq!(
            parse_latest_release(b"not json", "1.0.0"),
            Err(ReleaseCheckError::InvalidResponse)
        );
        for bytes in [
            response("1.0.1", false, false, RELEASE_PAGE_URL),
            response("v1.0.1-rc.1", false, false, RELEASE_PAGE_URL),
            response("v1.0.1", true, false, RELEASE_PAGE_URL),
            response("v1.0.1", false, true, RELEASE_PAGE_URL),
            response(
                "v1.0.1",
                false,
                false,
                "https://github.com/other/repo/releases",
            ),
        ] {
            assert_eq!(
                parse_latest_release(&bytes, "1.0.0"),
                Err(ReleaseCheckError::InvalidResponse)
            );
        }
    }
}
