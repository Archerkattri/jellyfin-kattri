//! The sample presence plugin under `examples/sample-presence` must pass the
//! crate's manifest parser and entry resolution.

use std::path::{Path, PathBuf};

use kattri_plugins::permission::ALLOWED_WIRE_NAMES;
use kattri_plugins::{PLUGIN_API_VERSION, PluginManifest, resolve_entry};

const MANIFEST: &[u8] = include_bytes!("../examples/sample-presence/plugin.json");

fn sample_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("examples")
        .join("sample-presence")
}

#[test]
fn sample_manifest_parses_and_validates() {
    let manifest = PluginManifest::from_bytes(MANIFEST).unwrap();
    assert_eq!(manifest.id, "org.kattri.sample-presence");
    assert_eq!(manifest.name, "Sample Presence");
    assert_eq!(manifest.version, "0.1.0");
    assert_eq!(manifest.api_version, PLUGIN_API_VERSION);
    manifest.validate().unwrap();
}

#[test]
fn sample_entry_resolves_to_a_file_inside_its_folder() {
    let manifest = PluginManifest::from_bytes(MANIFEST).unwrap();
    let path = resolve_entry(&sample_dir(), &manifest).unwrap();
    assert!(
        path.ends_with("lib.rs"),
        "unexpected entry {}",
        path.display()
    );
}

#[test]
fn sample_permissions_are_all_in_the_allowed_list() {
    let manifest = PluginManifest::from_bytes(MANIFEST).unwrap();
    assert!(!manifest.permissions.is_empty());
    for permission in &manifest.permissions {
        assert!(
            ALLOWED_WIRE_NAMES.contains(&permission.wire_name()),
            "{} is not in the allowed permission list",
            permission.wire_name()
        );
    }
}

/// The crate's parser reports errors only, not warnings, so this checks the
/// things a warning would normally flag: validation passes, no permission is
/// repeated, the description is present, and the manifest survives a round trip
/// through its serialized form unchanged.
#[test]
fn sample_manifest_validates_without_warnings() {
    let manifest = PluginManifest::from_bytes(MANIFEST).unwrap();
    manifest.validate().unwrap();

    let mut seen = std::collections::HashSet::new();
    for permission in &manifest.permissions {
        assert!(seen.insert(permission.wire_name()), "duplicate permission");
    }
    assert!(!manifest.description.trim().is_empty());

    let serialized = serde_json::to_string(&manifest).unwrap();
    let reparsed = PluginManifest::from_json(&serialized).unwrap();
    assert_eq!(reparsed, manifest);
}
