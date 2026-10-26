//! Plugin host for Jellyfin Kattri: manifest, permission model and runtime contract.
//!
//! Status:
//! - Implemented and tested: manifest parsing and validation (id, semver version,
//!   API version, relative entry path, duplicate and malformed permissions), the
//!   permission model with consent rules and the `authorize` gate, the `NoRuntime`
//!   default, the sandbox limit model, the plugin settings model and its `plugins.toml`
//!   file, the Discord presence frame encoder and the SSDP discovery byte formats.
//! - The wasm runtime lives in the `kattri-plugin-host` crate. This crate has no
//!   WebAssembly dependency.
//!
//! Setting `settings.plugins.enabled` has no effect until the runtime exists.

pub mod capability;
pub mod cast;
pub mod discord;
pub mod error;
pub mod host;
pub mod lifecycle;
pub mod manifest;
pub mod permission;
pub mod registry;
pub mod sandbox;
pub mod settings;
pub mod settings_file;
pub mod wit_contract;

pub use capability::{HostCall, invoke};
pub use error::PluginError;
pub use host::{NoRuntime, PluginHandle, PluginRuntime};
pub use manifest::{PLUGIN_API_VERSION, PluginManifest, ensure_id_available, resolve_entry};
pub use permission::{Permission, PermissionGrants, authorize, pending_consent};
