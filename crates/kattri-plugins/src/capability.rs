//! Host calls a plugin can make, each tied to the capability it needs.
//!
//! Every call the model exposes goes through [`invoke`]. It checks, in order:
//! 1. the grants are bound to this plugin (grants are never shared between plugins);
//! 2. the capability is declared in the manifest and granted by the user (see
//!    [`authorize`]);
//! 3. the payload is within its limits.
//!
//! Only then does the call succeed. Performing the real IPC or storage is the job of
//! the M4 runtime (M4-07 settings, M4-09 presence) and is not implemented here, so a
//! successful call currently means "authorized and well-formed".

use crate::error::PluginError;
use crate::manifest::PluginManifest;
use crate::permission::{Permission, PermissionGrants, authorize};

/// Longest log line a plugin may write, in characters.
pub const MAX_LOG_LINE_CHARS: usize = 512;

/// Every host call the plugin model exposes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostCall {
    /// Read playback state (`playback.read`).
    PlaybackRead,
    /// Set a presence payload (`presence.write`).
    PresenceWrite { payload: String },
    /// Read one key from the plugin's own settings (`settings.own`).
    SettingsOwnRead { key: String },
    /// Write one key in the plugin's own settings (`settings.own`).
    SettingsOwnWrite { key: String, value: String },
    /// Write a log line tagged with the plugin id (`log.write`).
    LogWrite { line: String },
}

impl HostCall {
    /// The capability this call requires.
    pub fn required_permission(&self) -> Permission {
        match self {
            Self::PlaybackRead => Permission::PlaybackRead,
            Self::PresenceWrite { .. } => Permission::PresenceWrite,
            Self::SettingsOwnRead { .. } | Self::SettingsOwnWrite { .. } => Permission::SettingsOwn,
            Self::LogWrite { .. } => Permission::LogWrite,
        }
    }
}

/// Authorize and validate one host call on behalf of `manifest`'s plugin.
///
/// Authorization runs before payload validation, so a refused call learns nothing
/// about the payload rules. Any refusal from the grant check is returned as
/// [`PluginError::PermissionDenied`].
pub fn invoke(
    manifest: &PluginManifest,
    grants: &PermissionGrants,
    call: &HostCall,
) -> Result<(), PluginError> {
    let permission = call.required_permission();
    if grants.bound_plugin_id() != Some(manifest.id.as_str()) {
        return Err(denied(&permission));
    }
    authorize(manifest, grants, &permission).map_err(|_| denied(&permission))?;

    match call {
        HostCall::LogWrite { line } => {
            if line.chars().count() > MAX_LOG_LINE_CHARS {
                return Err(PluginError::Invalid {
                    field: "log.write",
                    reason: format!("line is longer than {MAX_LOG_LINE_CHARS} characters"),
                });
            }
        }
        HostCall::SettingsOwnRead { key } | HostCall::SettingsOwnWrite { key, .. } => {
            validate_settings_key(key)?;
        }
        HostCall::PlaybackRead | HostCall::PresenceWrite { .. } => {}
    }
    Ok(())
}

/// Longest settings key in bytes. Shared by host calls and manifest declarations.
pub(crate) const MAX_SETTINGS_KEY_BYTES: usize = 128;

/// The settings key rule: 1 to 128 bytes. Used for host calls and manifest declarations.
pub(crate) fn validate_settings_key(key: &str) -> Result<(), PluginError> {
    if key.is_empty() || key.len() > MAX_SETTINGS_KEY_BYTES {
        return Err(PluginError::Invalid {
            field: "settings.own",
            reason: format!("key must be 1 to {MAX_SETTINGS_KEY_BYTES} bytes"),
        });
    }
    Ok(())
}

fn denied(permission: &Permission) -> PluginError {
    PluginError::PermissionDenied {
        permission: permission.wire_name().to_owned(),
    }
}
