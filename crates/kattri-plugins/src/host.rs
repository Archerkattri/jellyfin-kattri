//! Plugin host contract. The sandboxed runtime lives in the `kattri-plugin-host` crate; the
//! default implementation here, [`NoRuntime`], refuses to load anything.
//!
//! Every host call goes through [`crate::permission::authorize`]. Only validated manifests
//! reach [`PluginRuntime::load`].

use crate::error::PluginError;
use crate::manifest::PluginManifest;

/// Opaque handle to a loaded plugin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PluginHandle(u64);

impl PluginHandle {
    pub const fn new(id: u64) -> Self {
        Self(id)
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Loads and unloads plugins.
pub trait PluginRuntime {
    /// Load a validated manifest's entry point. Must authorize every host call later.
    fn load(&mut self, manifest: &PluginManifest) -> Result<PluginHandle, PluginError>;
    fn unload(&mut self, handle: PluginHandle) -> Result<(), PluginError>;
}

/// Default runtime: refuses everything. Keeps `settings.plugins.enabled` harmless when no
/// runtime is wired in.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoRuntime;

impl PluginRuntime for NoRuntime {
    fn load(&mut self, _manifest: &PluginManifest) -> Result<PluginHandle, PluginError> {
        Err(PluginError::NotImplemented("plugin runtime (M4)"))
    }

    fn unload(&mut self, _handle: PluginHandle) -> Result<(), PluginError> {
        Err(PluginError::NotImplemented("plugin runtime (M4)"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_runtime_refuses_to_load_a_valid_manifest() {
        let m = PluginManifest::from_json(
            r#"{"id":"org.example.a","name":"A","version":"1.0.0","api_version":1,"entry":"main.js"}"#,
        )
        .unwrap();
        let mut rt = NoRuntime;
        assert_eq!(
            rt.load(&m),
            Err(PluginError::NotImplemented("plugin runtime (M4)"))
        );
        assert!(rt.unload(PluginHandle::new(1)).is_err());
    }
}
