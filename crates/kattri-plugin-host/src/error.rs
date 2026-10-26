//! Errors from running guest code.

use kattri_plugins::PluginError;
use kattri_plugins::sandbox::SandboxFailure;

/// Why a guest call failed. Every [`RuntimeError::Sandbox`] marks the plugin as failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RuntimeError {
    /// The guest hit one of its resource limits, or trapped.
    #[error("sandbox limit reached: {0:?}")]
    Sandbox(SandboxFailure),
    /// A plugin-model rule refused the request.
    #[error("plugin error: {0}")]
    Plugin(#[from] PluginError),
    /// The wasmtime runtime rejected the module or the call for a non-guest reason.
    #[error("runtime error: {0}")]
    Runtime(String),
    /// The guest ran to completion and returned an error from its export.
    #[error("plugin refused the call: {0}")]
    Refused(String),
}
