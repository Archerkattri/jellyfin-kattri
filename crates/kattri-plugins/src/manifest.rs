//! Plugin manifest: identity, version, entry point and declared permissions.
//!
//! Validation is strict on purpose. A manifest that fails here is never loaded, so
//! the runtime (M4) only ever sees well-formed, bounded input.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::PluginError;
use crate::permission::Permission;

/// Plugin API version this build implements. A manifest must declare exactly this.
pub const PLUGIN_API_VERSION: u32 = 1;

/// Largest manifest accepted, in bytes. Larger input is rejected before parsing.
pub const MAX_MANIFEST_BYTES: usize = 64 * 1024;

const MIN_ID_LEN: usize = 3;
const MAX_ID_LEN: usize = 64;
const MAX_NAME_LEN: usize = 80;
const MAX_VERSION_LEN: usize = 64;
const MAX_ENTRY_LEN: usize = 255;
const MAX_DESCRIPTION_LEN: usize = 1000;
const MAX_PERMISSIONS: usize = 32;
const MAX_SETTINGS_KEYS: usize = 32;

/// The `plugin.json` document shipped with each plugin.
///
/// Unknown top-level keys are rejected (`deny_unknown_fields`), so a typo in a
/// permission key can never be silently ignored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginManifest {
    /// Reverse-DNS style identifier, for example `org.example.skip-credits`.
    pub id: String,
    pub name: String,
    /// Plugin version, semver (`1.2.3`, `1.0.0-beta.1`).
    pub version: String,
    /// Must equal [`PLUGIN_API_VERSION`].
    pub api_version: u32,
    /// Relative path to the plugin's entry file inside its folder.
    pub entry: String,
    #[serde(default)]
    pub permissions: Vec<Permission>,
    #[serde(default)]
    pub description: String,
    /// Oldest host version the plugin supports, semver. Optional.
    #[serde(default)]
    pub min_host_version: Option<String>,
    /// Keys the plugin may store in its own settings namespace (M4-07). Empty by default.
    #[serde(default)]
    pub settings: Vec<String>,
}

impl PluginManifest {
    /// Parse and validate a manifest from JSON text.
    pub fn from_json(text: &str) -> Result<Self, PluginError> {
        if text.len() > MAX_MANIFEST_BYTES {
            return Err(invalid("manifest", "is larger than 64 KiB"));
        }
        let manifest: Self =
            serde_json::from_str(text).map_err(|e| PluginError::Json(e.to_string()))?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// Parse and validate a manifest from raw file bytes. Rejects non-UTF-8 input.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, PluginError> {
        if bytes.len() > MAX_MANIFEST_BYTES {
            return Err(invalid("manifest", "is larger than 64 KiB"));
        }
        let text =
            std::str::from_utf8(bytes).map_err(|_| invalid("manifest", "is not valid UTF-8"))?;
        Self::from_json(text)
    }

    /// Check every field. Returns the first violation.
    pub fn validate(&self) -> Result<(), PluginError> {
        validate_id(&self.id)?;

        validate_text("name", &self.name, MAX_NAME_LEN, false)?;

        validate_semver("version", &self.version)?;

        if let Some(min) = &self.min_host_version {
            validate_semver("min_host_version", min)?;
        }

        if self.api_version != PLUGIN_API_VERSION {
            return Err(PluginError::UnsupportedApi {
                found: self.api_version,
                supported: PLUGIN_API_VERSION,
            });
        }

        validate_text("description", &self.description, MAX_DESCRIPTION_LEN, true)?;

        validate_entry(&self.entry)?;

        if self.permissions.len() > MAX_PERMISSIONS {
            return Err(invalid("permissions", "lists more than 32 entries"));
        }
        let mut seen = HashSet::new();
        for permission in &self.permissions {
            permission.validate()?;
            if !seen.insert(permission) {
                return Err(invalid("permissions", "contains a duplicate entry"));
            }
        }

        if self.settings.len() > MAX_SETTINGS_KEYS {
            return Err(invalid("settings", "declares more than 32 keys"));
        }
        let mut keys = HashSet::new();
        for key in &self.settings {
            crate::settings::validate_declared_key(key)?;
            if !keys.insert(key) {
                return Err(invalid("settings", "declares a key twice"));
            }
        }
        Ok(())
    }
}

/// Resolve the manifest's entry file under `plugin_dir`.
///
/// The entry is validated as a relative path, joined to `plugin_dir`, canonicalized
/// (following symlinks), and must still lie inside the canonical plugin folder.
/// A symlink or junction that points outside the folder is rejected.
pub fn resolve_entry(plugin_dir: &Path, manifest: &PluginManifest) -> Result<PathBuf, PluginError> {
    validate_entry(&manifest.entry)?;
    let mut candidate = plugin_dir.to_path_buf();
    for segment in manifest.entry.split(['/', '\\']) {
        candidate.push(segment);
    }
    let root = plugin_dir
        .canonicalize()
        .map_err(|_| invalid("entry", "the plugin folder cannot be resolved"))?;
    let target = candidate.canonicalize().map_err(|_| {
        invalid(
            "entry",
            "the entry file does not exist or cannot be resolved",
        )
    })?;
    if !target.starts_with(&root) {
        return Err(invalid(
            "entry",
            "resolves outside the plugin folder (symlink or junction escape)",
        ));
    }
    if !target.is_file() {
        return Err(invalid("entry", "does not name a regular file"));
    }
    Ok(target)
}

/// Reject a manifest whose id is already held by a loaded plugin.
pub fn ensure_id_available(
    loaded: &[PluginManifest],
    candidate: &PluginManifest,
) -> Result<(), PluginError> {
    if loaded.iter().any(|m| m.id == candidate.id) {
        return Err(PluginError::DuplicateId {
            id: candidate.id.clone(),
        });
    }
    Ok(())
}

fn invalid(field: &'static str, reason: &str) -> PluginError {
    PluginError::Invalid {
        field,
        reason: reason.to_owned(),
    }
}

fn validate_semver(field: &'static str, value: &str) -> Result<(), PluginError> {
    if value.len() > MAX_VERSION_LEN {
        return Err(invalid(field, "is longer than 64 characters"));
    }
    semver::Version::parse(value)
        .map(|_| ())
        .map_err(|_| invalid(field, "must be a semver version such as 1.2.3"))
}

/// Free text: non-empty after trimming (unless `allow_empty`), bounded in characters,
/// and free of control characters. Newlines and tabs are allowed only in descriptions.
fn validate_text(
    field: &'static str,
    value: &str,
    max_chars: usize,
    allow_empty: bool,
) -> Result<(), PluginError> {
    if !allow_empty && value.trim().is_empty() {
        return Err(invalid(field, "must not be empty"));
    }
    if value.chars().count() > max_chars {
        return Err(invalid(field, "is longer than the allowed length"));
    }
    let bad_control = value
        .chars()
        .any(|c| c.is_control() && !(allow_empty && (c == '\n' || c == '\t')));
    if bad_control {
        return Err(invalid(field, "contains control characters"));
    }
    Ok(())
}

/// Lowercase segments of `[a-z0-9-]` joined by single dots, at least two segments.
fn validate_id(id: &str) -> Result<(), PluginError> {
    if id.len() < MIN_ID_LEN {
        return Err(invalid("id", "is shorter than 3 characters"));
    }
    if id.len() > MAX_ID_LEN {
        return Err(invalid("id", "is longer than 64 characters"));
    }
    let segments: Vec<&str> = id.split('.').collect();
    if segments.len() < 2 {
        return Err(invalid(
            "id",
            "must have at least two dot-separated segments",
        ));
    }
    for segment in segments {
        if segment.is_empty()
            || segment.starts_with('-')
            || segment.ends_with('-')
            || !segment
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        {
            return Err(invalid("id", "segments must be lowercase a-z, 0-9 or '-'"));
        }
    }
    Ok(())
}

/// The entry must be a non-empty relative path made only of normal segments.
///
/// Backslashes are normalized to `/` first, so `..\x.wasm` and `..\\x` are rejected
/// on every platform. Rejected: absolute paths and roots, drive prefixes (any `:`),
/// NUL and control characters, empty segments, `.` and `..`.
fn validate_entry(entry: &str) -> Result<(), PluginError> {
    if entry.is_empty() {
        return Err(invalid("entry", "must not be empty"));
    }
    if entry.len() > MAX_ENTRY_LEN {
        return Err(invalid("entry", "is longer than 255 characters"));
    }
    if entry.chars().any(|c| c == ':' || c.is_control()) {
        return Err(invalid(
            "entry",
            "must not contain ':' or control characters (no drive prefixes)",
        ));
    }
    let normalized = entry.replace('\\', "/");
    if normalized.starts_with('/') {
        return Err(invalid("entry", "must be a relative path, not absolute"));
    }
    for segment in normalized.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." {
            return Err(invalid(
                "entry",
                "must be a relative path inside the plugin folder (no '..', '.', or empty segments)",
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> String {
        r#"{
            "id": "org.example.skip-credits",
            "name": "Skip Credits",
            "version": "1.2.3",
            "api_version": 1,
            "entry": "dist/main.js",
            "permissions": ["read_playback", {"network": {"host": "api.example.org"}}]
        }"#
        .to_owned()
    }

    #[test]
    fn valid_manifest_parses() {
        let m = PluginManifest::from_json(&base()).unwrap();
        assert_eq!(m.version, "1.2.3");
        assert_eq!(m.permissions.len(), 2);
    }

    #[test]
    fn bad_ids_are_rejected() {
        for bad in [
            "Org.Example",
            "single",
            "org..example",
            ".org.example",
            "org.-bad",
            "org.exa mple",
            "org.Example",
        ] {
            let text = base().replace("org.example.skip-credits", bad);
            assert!(
                matches!(
                    PluginManifest::from_json(&text),
                    Err(PluginError::Invalid { field: "id", .. })
                ),
                "id {bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn non_semver_version_is_rejected() {
        let text = base().replace("1.2.3", "v1");
        assert!(matches!(
            PluginManifest::from_json(&text),
            Err(PluginError::Invalid {
                field: "version",
                ..
            })
        ));
    }

    #[test]
    fn other_api_versions_are_refused() {
        let text = base().replace("\"api_version\": 1", "\"api_version\": 2");
        assert_eq!(
            PluginManifest::from_json(&text),
            Err(PluginError::UnsupportedApi {
                found: 2,
                supported: 1
            })
        );
    }

    #[test]
    fn entry_traversal_and_absolute_paths_are_rejected() {
        for bad in [
            "../evil.js",
            "/etc/passwd.js",
            "C:evil.js",
            "a/../../b.js",
            "./main.js",
            "",
        ] {
            let text = base().replace("dist/main.js", bad);
            assert!(
                matches!(
                    PluginManifest::from_json(&text),
                    Err(PluginError::Invalid { field: "entry", .. })
                ),
                "entry {bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn unknown_permission_names_fail_to_parse() {
        let text = base().replace("\"read_playback\"", "\"root_access\"");
        assert!(matches!(
            PluginManifest::from_json(&text),
            Err(PluginError::Json(_))
        ));
    }

    #[test]
    fn duplicate_permissions_are_rejected() {
        let text = base().replace(
            "\"permissions\": [\"read_playback\", {\"network\": {\"host\": \"api.example.org\"}}]",
            "\"permissions\": [\"read_playback\", \"read_playback\"]",
        );
        assert!(matches!(
            PluginManifest::from_json(&text),
            Err(PluginError::Invalid {
                field: "permissions",
                ..
            })
        ));
    }

    #[test]
    fn missing_permissions_default_to_none() {
        let text = base().replace(
            ",\n            \"permissions\": [\"read_playback\", {\"network\": {\"host\": \"api.example.org\"}}]",
            "",
        );
        let m = PluginManifest::from_json(&text).unwrap();
        assert!(m.permissions.is_empty());
    }
}
