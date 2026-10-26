//! Error type for manifest validation, permission checks and the (future) runtime.

/// Everything that can reject a plugin manifest or a permission request.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PluginError {
    /// The manifest is not valid JSON, or names an unknown permission kind.
    #[error("plugin manifest could not be parsed: {0}")]
    Json(String),
    /// A field is outside its allowed shape or range.
    #[error("invalid plugin manifest field `{field}`: {reason}")]
    Invalid { field: &'static str, reason: String },
    /// The manifest targets a different plugin API version.
    #[error("plugin API version {found} is not supported (this build implements {supported})")]
    UnsupportedApi { found: u32, supported: u32 },
    /// The plugin used a permission its manifest does not declare.
    #[error("permission {permission} is not declared in the manifest")]
    Undeclared { permission: String },
    /// The permission is declared but the user has not approved it.
    #[error("permission {permission} has not been granted by the user")]
    NotGranted { permission: String },
    /// A host call was refused because the capability is not declared in the
    /// manifest, not granted to this plugin, or the grants belong to another plugin.
    #[error("permission {permission} was denied: not declared in the manifest or not granted")]
    PermissionDenied { permission: String },
    /// A plugin id is already held by a loaded plugin.
    #[error("plugin id {id} is already loaded")]
    DuplicateId { id: String },
    /// The runtime that would execute plugins is not built yet (M4).
    #[error("{0} is not implemented yet")]
    NotImplemented(&'static str),
    /// A lifecycle event is not allowed from the plugin's current state.
    #[error("lifecycle event {event} is not allowed from state {from}")]
    InvalidTransition {
        from: &'static str,
        event: &'static str,
    },
    /// No plugin with this id is registered.
    #[error("no plugin with id {id} is registered")]
    UnknownPlugin { id: String },
    /// A result arrived for an earlier load of the plugin (before a reload). It is ignored.
    #[error("result for an earlier load of plugin {id} is stale")]
    StaleResult { id: String },
    /// The plugin settings file could not be read or written.
    #[error("plugin settings file {path}: {reason}")]
    Storage { path: String, reason: String },
    /// Another writer holds the settings lock. The lock file must be removed by hand if its
    /// writer is gone.
    #[error("plugin settings are locked by another writer: {path}")]
    Locked { path: String },
}
