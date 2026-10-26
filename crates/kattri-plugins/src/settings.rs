//! M4-07 plugin settings: a per-plugin key/value namespace, validated against the keys the
//! manifest declares.
//!
//! Every read and write goes through [`crate::capability::invoke`] first, so the
//! `settings.own` grant, the plugin binding and the key length rule apply exactly as they
//! do for any other host call. The store is keyed by plugin id, so one plugin cannot name
//! another plugin's namespace.
//!
//! Limits of this module (see the M4-07 spec):
//! - Pure model. The TOML file, atomic write, unknown-key preservation and the lock against
//!   a second app instance are not implemented here. The `toml` crate is not a dependency of
//!   this crate, and adding it changes the root `Cargo.lock`.
//! - The host cannot stop a plugin from storing what it is given inside its own namespace.
//!   Plugins must not be given secrets at all. The value rules below only refuse URL-looking
//!   and credential-query-looking text.
//! - The credential and URL rules come from `kattri-guard`, the shared denylist.

use std::collections::BTreeMap;

use crate::capability::{HostCall, invoke, validate_settings_key};
use crate::error::PluginError;
use crate::manifest::PluginManifest;
use crate::permission::PermissionGrants;

/// Longest settings value in bytes.
pub const MAX_SETTING_VALUE_BYTES: usize = 1024;

/// Check a key a manifest declares. Applies the shared key length rule and refuses
/// credential-like names, so a manifest cannot ask for a token slot.
pub fn validate_declared_key(key: &str) -> Result<(), PluginError> {
    validate_settings_key(key)?;
    if kattri_guard::is_secret_like_key(key) {
        return Err(PluginError::Invalid {
            field: "settings",
            reason: format!("key `{key}` looks like a credential and cannot be declared"),
        });
    }
    Ok(())
}

/// Check a settings value before it is stored.
pub fn validate_value(value: &str) -> Result<(), PluginError> {
    if value.len() > MAX_SETTING_VALUE_BYTES {
        return Err(PluginError::Invalid {
            field: "settings.value",
            reason: format!("must be at most {MAX_SETTING_VALUE_BYTES} bytes"),
        });
    }
    if value.chars().any(char::is_control) {
        return Err(PluginError::Invalid {
            field: "settings.value",
            reason: "contains control characters".to_owned(),
        });
    }
    if kattri_guard::is_secret_like_value(value) {
        return Err(PluginError::Invalid {
            field: "settings.value",
            reason: "looks like a URL or credential and is not stored".to_owned(),
        });
    }
    Ok(())
}

/// Settings for every plugin, keyed by plugin id.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PluginSettingsStore {
    plugins: BTreeMap<String, BTreeMap<String, String>>,
}

impl PluginSettingsStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Store `value` under `key` in the plugin's namespace.
    ///
    /// Refused unless the plugin holds `settings.own`, `key` is declared in its manifest,
    /// and the value passes [`validate_value`]. A refused write changes nothing.
    pub fn set(
        &mut self,
        manifest: &PluginManifest,
        grants: &PermissionGrants,
        key: &str,
        value: &str,
    ) -> Result<(), PluginError> {
        invoke(
            manifest,
            grants,
            &HostCall::SettingsOwnWrite {
                key: key.to_owned(),
                value: value.to_owned(),
            },
        )?;
        ensure_declared(manifest, key)?;
        validate_value(value)?;
        self.plugins
            .entry(manifest.id.clone())
            .or_default()
            .insert(key.to_owned(), value.to_owned());
        Ok(())
    }

    /// Read `key` from the plugin's namespace. `Ok(None)` if the key is declared but unset.
    pub fn get<'a>(
        &'a self,
        manifest: &PluginManifest,
        grants: &PermissionGrants,
        key: &str,
    ) -> Result<Option<&'a str>, PluginError> {
        invoke(
            manifest,
            grants,
            &HostCall::SettingsOwnRead {
                key: key.to_owned(),
            },
        )?;
        ensure_declared(manifest, key)?;
        Ok(self
            .plugins
            .get(&manifest.id)
            .and_then(|table| table.get(key))
            .map(String::as_str))
    }

    /// Remove `key` from the plugin's namespace. Returns the old value if one was set.
    pub fn remove(
        &mut self,
        manifest: &PluginManifest,
        grants: &PermissionGrants,
        key: &str,
    ) -> Result<Option<String>, PluginError> {
        invoke(
            manifest,
            grants,
            &HostCall::SettingsOwnWrite {
                key: key.to_owned(),
                value: String::new(),
            },
        )?;
        ensure_declared(manifest, key)?;
        Ok(self
            .plugins
            .get_mut(&manifest.id)
            .and_then(|table| table.remove(key)))
    }

    /// Host-side view of one plugin's stored values, for persistence. Not a plugin call.
    pub fn table(&self, plugin_id: &str) -> Option<&BTreeMap<String, String>> {
        self.plugins.get(plugin_id)
    }

    /// Load a stored value from the settings file. The caller has already checked the key
    /// against the manifest and the value with [`validate_value`]. Not a plugin call.
    pub(crate) fn restore(&mut self, plugin_id: &str, key: &str, value: String) {
        self.plugins
            .entry(plugin_id.to_owned())
            .or_default()
            .insert(key.to_owned(), value);
    }
}

fn ensure_declared(manifest: &PluginManifest, key: &str) -> Result<(), PluginError> {
    if manifest.settings.iter().any(|declared| declared == key) {
        Ok(())
    } else {
        Err(PluginError::Invalid {
            field: "settings",
            reason: format!("key `{key}` is not declared in the manifest"),
        })
    }
}
