//! Per-download manifest on disk.
//!
//! The manifest holds only identifiers, offsets, names and status. It has no URL, header,
//! token, `api_key`, or server origin. The struct has no such fields, and the sanitizer
//! and the test in `tests/manifest_no_secrets.rs` back that up.
//!
//! The denylist is shared with the settings crate (`kattri_settings::guard`), so the
//! two places that keep secrets out of disk files use the same markers.
//!
//! Atomic write: write a sibling temp file, fsync it, then rename over the
//! target. A crash leaves either the old manifest or the new one.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::Path;

use kattri_client_core::downloads::{DownloadSnapshot, DownloadState};
use kattri_settings::guard::is_secret_like_value;
use serde::{Deserialize, Serialize};

use crate::verify::Verification;

pub const MANIFEST_SCHEMA_VERSION: u32 = 1;
/// Manifests are a few hundred bytes; a larger file is refused rather than allocated.
const MAX_MANIFEST_READ_BYTES: u64 = 1024 * 1024;
/// Longest title kept for display (characters, not bytes).
pub const MAX_TITLE_CHARS: usize = 200;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ManifestVerification {
    Verified,
    Unverified,
    Failed,
}

impl From<Verification> for ManifestVerification {
    fn from(value: Verification) -> Self {
        match value {
            Verification::Verified => Self::Verified,
            Verification::Unverified => Self::Unverified,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Manifest {
    pub schema_version: u32,
    /// Server association by ID, never by URL.
    pub server_id: String,
    pub item_id: String,
    /// Display only. Sanitized; omitted when the raw title looks like a URL or credential.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub state: String,
    pub durable_offset: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_length: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temp_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub final_name: Option<String>,
    /// `None` until the file is committed or failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verification: Option<ManifestVerification>,
    /// Unix seconds when the download was added.
    pub added_at: u64,
    /// External text subtitle files. Names and languages only, no URLs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub subtitles: Vec<SubtitleEntry>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SubtitleStatus {
    Committed,
    Failed,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubtitleEntry {
    pub index: i32,
    pub format: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// `<index>.<format>` under the item's `subtitles` folder, when committed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_name: Option<String>,
    pub status: SubtitleStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verification: Option<ManifestVerification>,
}

/// Inputs for a manifest, taken from a job snapshot and the caller's metadata.
pub struct ManifestInput<'a> {
    pub server_id: &'a str,
    pub item_id: &'a str,
    pub raw_title: Option<&'a str>,
    pub snapshot: &'a DownloadSnapshot,
    pub temp_name: Option<&'a str>,
    pub final_name: Option<&'a str>,
    pub verification: Option<Verification>,
    pub failed: bool,
    pub added_at: u64,
}

impl Manifest {
    pub fn build(input: &ManifestInput<'_>) -> Self {
        let verification = if input.failed {
            Some(ManifestVerification::Failed)
        } else {
            input.verification.map(ManifestVerification::from)
        };
        Self {
            schema_version: MANIFEST_SCHEMA_VERSION,
            server_id: input.server_id.to_owned(),
            item_id: input.item_id.to_owned(),
            title: input.raw_title.and_then(sanitize_title),
            state: state_name(input.snapshot.state).to_owned(),
            durable_offset: input.snapshot.durable_offset,
            total_length: input.snapshot.expected_total_length,
            temp_name: input.temp_name.map(str::to_owned),
            final_name: input.final_name.map(str::to_owned),
            verification,
            added_at: input.added_at,
            subtitles: Vec::new(),
        }
    }
}

fn state_name(state: DownloadState) -> &'static str {
    match state {
        DownloadState::Queued => "queued",
        DownloadState::Negotiating => "negotiating",
        DownloadState::Downloading => "downloading",
        DownloadState::Verifying => "verifying",
        DownloadState::Committed => "committed",
        DownloadState::Paused => "paused",
        DownloadState::Cancelled => "cancelled",
        DownloadState::Failed => "failed",
        DownloadState::NeedsAuth => "needsAuth",
    }
}

/// Title for display, or `None`.
///
/// Control characters are removed, whitespace is trimmed, and the result is cut to
/// [`MAX_TITLE_CHARS`]. A title that matches the settings denylist (a URL, `api_key=`,
/// `token=`) is refused entirely (`None`), not partly kept.
pub fn sanitize_title(raw: &str) -> Option<String> {
    let cleaned: String = raw.chars().filter(|c| !c.is_control()).collect();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() || is_secret_like_value(trimmed) {
        return None;
    }
    Some(trimmed.chars().take(MAX_TITLE_CHARS).collect())
}

/// Writes the manifest atomically: temp sibling, fsync, rename.
pub fn write_atomic(path: &Path, manifest: &Manifest) -> io::Result<()> {
    let bytes = serde_json::to_vec_pretty(manifest).map_err(io::Error::other)?;
    let tmp = path.with_extension("json.tmp");
    {
        let mut file = File::create(&tmp)?;
        file.write_all(&bytes)?;
        file.flush()?;
        file.sync_all()?;
    }
    fs::rename(&tmp, path)
}

/// Reads a manifest back. The read is bounded so a damaged or replaced file cannot be fully
/// allocated.
pub fn read(path: &Path) -> io::Result<Manifest> {
    let mut bytes = Vec::new();
    File::open(path)?
        .take(MAX_MANIFEST_READ_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_MANIFEST_READ_BYTES {
        return Err(io::Error::other(
            "download manifest is larger than the limit",
        ));
    }
    serde_json::from_slice(&bytes).map_err(io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_are_cleaned_and_bounded() {
        assert_eq!(
            sanitize_title("  Episode\u{0007} 1\n"),
            Some("Episode 1".to_owned())
        );
        let long: String = "a".repeat(MAX_TITLE_CHARS + 50);
        assert_eq!(
            sanitize_title(&long).map(|t| t.chars().count()),
            Some(MAX_TITLE_CHARS)
        );
        assert_eq!(sanitize_title("   \t"), None);
    }

    #[test]
    fn titles_that_look_like_urls_or_credentials_are_refused() {
        assert_eq!(sanitize_title("https://host/Videos/1?api_key=abc"), None);
        assert_eq!(sanitize_title("Movie token=abc"), None);
        assert_eq!(sanitize_title("see http://x"), None);
    }

    #[test]
    fn manifest_build_takes_no_url_and_maps_status() {
        let snapshot = DownloadSnapshot {
            state: DownloadState::Failed,
            validators: Default::default(),
            expected_total_length: Some(10),
            durable_offset: 4,
        };
        let manifest = Manifest::build(&ManifestInput {
            server_id: "srv-1",
            item_id: "item-9",
            raw_title: Some("A Film"),
            snapshot: &snapshot,
            temp_name: Some("0123456789abcdef.part"),
            final_name: None,
            verification: None,
            failed: true,
            added_at: 1_700_000_000,
        });
        assert_eq!(manifest.state, "failed");
        assert_eq!(manifest.verification, Some(ManifestVerification::Failed));
        assert_eq!(manifest.title.as_deref(), Some("A Film"));
        assert_eq!(manifest.durable_offset, 4);
    }
}
