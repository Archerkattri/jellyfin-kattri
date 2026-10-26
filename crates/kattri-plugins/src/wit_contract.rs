//! Capability mapping for the host imports declared in the plugin WIT contract.
//!
//! The WIT file at `wit/kattri-plugin.wit` is the source of truth for the import
//! names. This module records which capability each import requires, using the
//! wire names from [`crate::permission::Permission`]. No bindings are generated
//! from the WIT file here.

/// Host imports and the capability wire name each one requires, as
/// `(import name, capability)`.
pub const IMPORT_CAPABILITIES: &[(&str, &str)] = &[
    ("presence-write", "presence.write"),
    ("settings-get", "settings.own"),
    ("settings-set", "settings.own"),
    ("log-write", "log.write"),
];

/// The capability wire name required by a host import, or `None` if the import is
/// not part of the contract.
pub fn capability_for_import(import: &str) -> Option<&'static str> {
    IMPORT_CAPABILITIES
        .iter()
        .find(|(name, _)| *name == import)
        .map(|(_, capability)| *capability)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permission::{ALLOWED_WIRE_NAMES, Permission};

    const WIT_SOURCE: &str = include_str!("../wit/kattri-plugin.wit");

    /// Names of the `import` declarations in the WIT source.
    fn wit_imports() -> Vec<&'static str> {
        WIT_SOURCE
            .lines()
            .map(str::trim)
            .filter_map(|line| line.strip_prefix("import "))
            .filter_map(|rest| rest.split(':').next())
            .map(str::trim)
            .collect()
    }

    #[test]
    fn wit_declares_the_expected_imports() {
        let mut imports = wit_imports();
        imports.sort_unstable();
        assert_eq!(
            imports,
            vec![
                "log-write",
                "presence-write",
                "settings-get",
                "settings-set"
            ]
        );
    }

    #[test]
    fn every_wit_import_has_a_capability() {
        for import in wit_imports() {
            assert!(
                capability_for_import(import).is_some(),
                "WIT import {import} has no capability"
            );
        }
    }

    #[test]
    fn every_mapping_names_a_wit_import() {
        let imports = wit_imports();
        assert_eq!(IMPORT_CAPABILITIES.len(), imports.len());
        for (name, _) in IMPORT_CAPABILITIES {
            assert!(imports.contains(name), "mapping {name} is not a WIT import");
        }
    }

    #[test]
    fn every_capability_named_by_the_wit_exists_in_the_permission_list() {
        for (import, capability) in IMPORT_CAPABILITIES {
            assert!(
                ALLOWED_WIRE_NAMES.contains(capability),
                "capability {capability} for {import} is not in the permission list"
            );
        }
    }

    #[test]
    fn imports_map_to_the_matching_permission_variant() {
        assert_eq!(
            capability_for_import("presence-write"),
            Some(Permission::PresenceWrite.wire_name())
        );
        assert_eq!(
            capability_for_import("settings-get"),
            Some(Permission::SettingsOwn.wire_name())
        );
        assert_eq!(
            capability_for_import("settings-set"),
            Some(Permission::SettingsOwn.wire_name())
        );
        assert_eq!(
            capability_for_import("log-write"),
            Some(Permission::LogWrite.wire_name())
        );
        assert_eq!(capability_for_import("not-in-contract"), None);
    }
}
