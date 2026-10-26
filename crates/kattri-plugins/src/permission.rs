//! Permission model: what a plugin may ask for, and what the user has granted.
//!
//! Two gates apply to every request:
//! 1. The permission must be declared in the plugin's manifest.
//! 2. If the permission needs user consent (see [`Permission::requires_consent`]),
//!    the user must have granted it in [`PermissionGrants`].
//!
//! Both checks live in [`authorize`]. The runtime (M4) calls it before every host call.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::error::PluginError;
use crate::manifest::PluginManifest;

/// One capability a plugin can request.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Permission {
    /// Read library metadata (titles, artwork URLs). No media bytes.
    ReadLibrary,
    /// Read the current item and playback position.
    ReadPlayback,
    /// Send playback commands (pause, seek, skip). Changes what the user sees.
    ControlPlayback,
    /// Show desktop notifications.
    Notifications,
    /// Read the plugin's own settings.
    ReadSettings,
    /// Write the plugin's own settings.
    WriteSettings,
    /// Make HTTPS requests to one exact host. No wildcards, ports or paths.
    Network { host: String },
    /// `playback.read` (M4-03): read playback state. No URLs, tokens or item ids
    /// beyond the Jellyfin item id. Requires a user grant.
    #[serde(rename = "playback.read")]
    PlaybackRead,
    /// `presence.write` (M4-03): ask the host to set a presence payload. The host
    /// performs the IPC; the plugin never opens sockets. Requires a user grant.
    #[serde(rename = "presence.write")]
    PresenceWrite,
    /// `settings.own` (M4-03): read and write the plugin's own settings namespace.
    /// Requires a user grant.
    #[serde(rename = "settings.own")]
    SettingsOwn,
    /// `log.write` (M4-03): write a length-capped log line tagged with the plugin id.
    /// Requires a user grant.
    #[serde(rename = "log.write")]
    LogWrite,
}

/// Wire names of every permission kind, in declaration order. A manifest may name
/// only these; any other name fails to parse.
pub const ALLOWED_WIRE_NAMES: &[&str] = &[
    "read_library",
    "read_playback",
    "control_playback",
    "notifications",
    "read_settings",
    "write_settings",
    "network",
    "playback.read",
    "presence.write",
    "settings.own",
    "log.write",
];

impl Permission {
    /// Whether the user must approve this permission before the plugin can use it.
    ///
    /// The M4-03 capabilities (`playback.read`, `presence.write`, `settings.own`,
    /// `log.write`) always need a grant, so nothing in them is implicit. The legacy
    /// read-only permissions are granted by declaration. Anything that changes
    /// playback, reaches the network or writes state needs explicit consent.
    pub fn requires_consent(&self) -> bool {
        matches!(
            self,
            Self::ControlPlayback
                | Self::WriteSettings
                | Self::Network { .. }
                | Self::PlaybackRead
                | Self::PresenceWrite
                | Self::SettingsOwn
                | Self::LogWrite
        )
    }

    /// The name used for this permission in manifests and in error messages.
    pub fn wire_name(&self) -> &'static str {
        match self {
            Self::ReadLibrary => "read_library",
            Self::ReadPlayback => "read_playback",
            Self::ControlPlayback => "control_playback",
            Self::Notifications => "notifications",
            Self::ReadSettings => "read_settings",
            Self::WriteSettings => "write_settings",
            Self::Network { .. } => "network",
            Self::PlaybackRead => "playback.read",
            Self::PresenceWrite => "presence.write",
            Self::SettingsOwn => "settings.own",
            Self::LogWrite => "log.write",
        }
    }

    /// Check the permission's own parameters.
    pub fn validate(&self) -> Result<(), PluginError> {
        if let Self::Network { host } = self {
            let ok = !host.is_empty()
                && host.len() <= 253
                && host.contains('.')
                && !host.starts_with('.')
                && !host.ends_with('.')
                && host.bytes().all(|b| {
                    b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'.'
                });
            if !ok {
                return Err(PluginError::Invalid {
                    field: "permissions.network.host",
                    reason: "must be one exact lowercase hostname (no wildcard, port or path)"
                        .to_owned(),
                });
            }
        }
        Ok(())
    }
}

/// The set of consent-requiring permissions the user has approved for one plugin.
///
/// Grants are per plugin. A set built with [`PermissionGrants::for_plugin`] is bound
/// to one plugin id and authorizes nothing for any other plugin. [`invoke`] requires
/// a binding, so the host path never authorizes from an unbound set.
///
/// [`invoke`]: crate::capability::invoke
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PermissionGrants {
    plugin_id: Option<String>,
    granted: HashSet<Permission>,
}

impl PermissionGrants {
    /// An empty, unbound grant set. It authorizes nothing; the host should use
    /// [`PermissionGrants::for_plugin`].
    pub fn new() -> Self {
        Self::default()
    }

    /// An empty grant set bound to `manifest`'s plugin id. No grants are implicit.
    pub fn for_plugin(manifest: &PluginManifest) -> Self {
        Self {
            plugin_id: Some(manifest.id.clone()),
            granted: HashSet::new(),
        }
    }

    /// The plugin this set is bound to, if any.
    pub fn bound_plugin_id(&self) -> Option<&str> {
        self.plugin_id.as_deref()
    }

    /// Record the user's approval. Only permissions that need consent are stored.
    pub fn grant(&mut self, permission: Permission) {
        if permission.requires_consent() {
            self.granted.insert(permission);
        }
    }

    /// Withdraw approval.
    pub fn revoke(&mut self, permission: &Permission) {
        self.granted.remove(permission);
    }

    pub fn is_granted(&self, permission: &Permission) -> bool {
        self.granted.contains(permission)
    }
}

/// Declared permissions that still need the user's approval, in manifest order.
pub fn pending_consent(manifest: &PluginManifest, grants: &PermissionGrants) -> Vec<Permission> {
    manifest
        .permissions
        .iter()
        .filter(|p| p.requires_consent() && !grants.is_granted(p))
        .cloned()
        .collect()
}

/// Decide whether a plugin may use `requested` right now.
pub fn authorize(
    manifest: &PluginManifest,
    grants: &PermissionGrants,
    requested: &Permission,
) -> Result<(), PluginError> {
    if !manifest.permissions.contains(requested) {
        return Err(PluginError::Undeclared {
            permission: format!("{requested:?}"),
        });
    }
    if grants
        .bound_plugin_id()
        .is_some_and(|bound| bound != manifest.id)
    {
        return Err(PluginError::NotGranted {
            permission: format!("{requested:?}"),
        });
    }
    if requested.requires_consent()
        && (grants.bound_plugin_id() != Some(manifest.id.as_str()) || !grants.is_granted(requested))
    {
        return Err(PluginError::NotGranted {
            permission: format!("{requested:?}"),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowed_wire_names_cover_every_variant_exactly() {
        let variants = [
            Permission::ReadLibrary,
            Permission::ReadPlayback,
            Permission::ControlPlayback,
            Permission::Notifications,
            Permission::ReadSettings,
            Permission::WriteSettings,
            Permission::Network {
                host: "api.example.org".into(),
            },
            Permission::PlaybackRead,
            Permission::PresenceWrite,
            Permission::SettingsOwn,
            Permission::LogWrite,
        ];
        for p in &variants {
            // Exhaustive on purpose: a new variant must be added to this test.
            match p {
                Permission::ReadLibrary
                | Permission::ReadPlayback
                | Permission::ControlPlayback
                | Permission::Notifications
                | Permission::ReadSettings
                | Permission::WriteSettings
                | Permission::Network { .. }
                | Permission::PlaybackRead
                | Permission::PresenceWrite
                | Permission::SettingsOwn
                | Permission::LogWrite => {}
            }
            assert!(ALLOWED_WIRE_NAMES.contains(&p.wire_name()));
        }
        assert_eq!(ALLOWED_WIRE_NAMES.len(), variants.len());
    }

    fn manifest(perms: &str) -> PluginManifest {
        let text = format!(
            r#"{{"id":"org.example.test","name":"T","version":"0.1.0","api_version":1,
                "entry":"main.js","permissions":[{perms}]}}"#
        );
        PluginManifest::from_json(&text).unwrap()
    }

    #[test]
    fn consent_policy_matches_the_documented_list() {
        assert!(!Permission::ReadLibrary.requires_consent());
        assert!(!Permission::ReadPlayback.requires_consent());
        assert!(Permission::ControlPlayback.requires_consent());
        assert!(
            Permission::Network {
                host: "a.example.org".into()
            }
            .requires_consent()
        );
    }

    #[test]
    fn network_hosts_must_be_exact_hostnames() {
        for bad in [
            "*.example.org",
            "example.org:443",
            "https://example.org",
            "localhost",
            "",
            "Example.org",
        ] {
            let p = Permission::Network {
                host: bad.to_owned(),
            };
            assert!(p.validate().is_err(), "host {bad:?} should be rejected");
        }
        assert!(
            Permission::Network {
                host: "api.example.org".into()
            }
            .validate()
            .is_ok()
        );
    }

    #[test]
    fn undeclared_permission_is_refused_even_if_granted() {
        let m = manifest("\"read_playback\"");
        let mut grants = PermissionGrants::new();
        grants.grant(Permission::ControlPlayback);
        assert!(matches!(
            authorize(&m, &grants, &Permission::ControlPlayback),
            Err(PluginError::Undeclared { .. })
        ));
    }

    #[test]
    fn unbound_grants_do_not_authorize_consent_permissions() {
        let m = manifest("\"control_playback\"");
        let mut grants = PermissionGrants::new();
        grants.grant(Permission::ControlPlayback);
        assert!(matches!(
            authorize(&m, &grants, &Permission::ControlPlayback),
            Err(PluginError::NotGranted { .. })
        ));
    }

    #[test]
    fn declared_consent_permission_needs_a_grant() {
        let m = manifest("\"control_playback\"");
        let mut grants = PermissionGrants::for_plugin(&m);
        assert!(matches!(
            authorize(&m, &grants, &Permission::ControlPlayback),
            Err(PluginError::NotGranted { .. })
        ));
        grants.grant(Permission::ControlPlayback);
        assert!(authorize(&m, &grants, &Permission::ControlPlayback).is_ok());
        grants.revoke(&Permission::ControlPlayback);
        assert!(authorize(&m, &grants, &Permission::ControlPlayback).is_err());
    }

    #[test]
    fn self_scoped_read_permission_needs_no_grant() {
        let m = manifest("\"read_library\"");
        assert!(authorize(&m, &PermissionGrants::new(), &Permission::ReadLibrary).is_ok());
    }

    #[test]
    fn grant_ignores_permissions_that_do_not_need_consent() {
        let mut grants = PermissionGrants::new();
        grants.grant(Permission::ReadLibrary);
        assert!(!grants.is_granted(&Permission::ReadLibrary));
    }

    #[test]
    fn pending_consent_lists_only_unapproved_consent_permissions_in_order() {
        let m = manifest(
            "\"read_library\",\"control_playback\",{\"network\":{\"host\":\"api.example.org\"}}",
        );
        let mut grants = PermissionGrants::new();
        assert_eq!(
            pending_consent(&m, &grants),
            vec![
                Permission::ControlPlayback,
                Permission::Network {
                    host: "api.example.org".into()
                }
            ]
        );
        grants.grant(Permission::ControlPlayback);
        assert_eq!(
            pending_consent(&m, &grants),
            vec![Permission::Network {
                host: "api.example.org".into()
            }]
        );
    }
}
