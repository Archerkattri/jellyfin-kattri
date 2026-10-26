//! Host state for the `kattri-plugin` world and the loader that instantiates a component.
//!
//! Every import checks the manifest and grants through `kattri_plugins::capability::invoke`
//! (or the settings store, which calls it) before it has any effect. A refused call returns
//! an error string to the guest and changes nothing on the host.

use std::time::Duration;

use kattri_plugins::capability::{HostCall, invoke};
use kattri_plugins::discord::{PresenceActivity, parse_activity};
use kattri_plugins::settings::PluginSettingsStore;
use kattri_plugins::{Permission, PermissionGrants, PluginManifest};
use wasmtime::Engine;
use wasmtime::component::{Component, HasSelf, Linker};

use crate::bindings::{KattriPlugin, KattriPluginImports, PlaybackEvent};
use crate::error::RuntimeError;
use crate::sandbox::{GuestData, arm, new_store, run_bounded};
use kattri_plugins::sandbox::SandboxLimits;

/// The app waits at most this long for a guest to return from `shutdown`.
const SHUTDOWN_WALL_CLOCK: Duration = Duration::from_secs(1);
/// Retained host log history per plugin, so a chatty guest cannot grow host memory forever.
pub const MAX_PLUGIN_LOG_LINES: usize = 1024;

/// What the host knows about one plugin while it runs.
pub struct HostState {
    manifest: PluginManifest,
    grants: PermissionGrants,
    settings: PluginSettingsStore,
    last_presence: Option<PresenceActivity>,
    log_lines: Vec<String>,
}

impl HostState {
    /// Host state for one plugin. `grants` must be bound to `manifest`'s id.
    pub fn new(
        manifest: PluginManifest,
        grants: PermissionGrants,
        settings: PluginSettingsStore,
    ) -> Self {
        Self {
            manifest,
            grants,
            settings,
            last_presence: None,
            log_lines: Vec::new(),
        }
    }

    /// The most recent presence the plugin set, after validation.
    pub fn last_presence(&self) -> Option<&PresenceActivity> {
        self.last_presence.as_ref()
    }

    /// Log lines the plugin wrote, each tagged with its plugin id.
    pub fn log_lines(&self) -> &[String] {
        &self.log_lines
    }

    /// The settings store, for the host to persist.
    pub fn settings(&self) -> &PluginSettingsStore {
        &self.settings
    }

    /// Withdraw a user's grant from this running plugin. The next host call observes it.
    pub fn revoke_permission(&mut self, permission: &Permission) {
        self.grants.revoke(permission);
    }

    fn authorize_playback_read(&self) -> Result<(), RuntimeError> {
        invoke(&self.manifest, &self.grants, &HostCall::PlaybackRead).map_err(RuntimeError::Plugin)
    }
}

impl KattriPluginImports for HostState {
    fn presence_write(&mut self, payload: String) -> Result<(), String> {
        invoke(
            &self.manifest,
            &self.grants,
            &HostCall::PresenceWrite {
                payload: payload.clone(),
            },
        )
        .map_err(|err| err.to_string())?;
        let activity = parse_activity(&payload).map_err(|err| err.to_string())?;
        self.last_presence = Some(activity);
        Ok(())
    }

    fn settings_get(&mut self, key: String) -> Result<Option<String>, String> {
        self.settings
            .get(&self.manifest, &self.grants, &key)
            .map(|value| value.map(str::to_owned))
            .map_err(|err| err.to_string())
    }

    fn settings_set(&mut self, key: String, value: String) -> Result<(), String> {
        self.settings
            .set(&self.manifest, &self.grants, &key, &value)
            .map_err(|err| err.to_string())
    }

    fn log_write(&mut self, line: String) -> Result<(), String> {
        invoke(
            &self.manifest,
            &self.grants,
            &HostCall::LogWrite { line: line.clone() },
        )
        .map_err(|err| err.to_string())?;
        if self.log_lines.len() == MAX_PLUGIN_LOG_LINES {
            self.log_lines.remove(0);
        }
        self.log_lines
            .push(format!("[{}] {line}", self.manifest.id));
        Ok(())
    }
}

/// A plugin component instantiated in its own store and engine.
pub struct LoadedPlugin {
    engine: Engine,
    limits: SandboxLimits,
    store: wasmtime::Store<GuestData<HostState>>,
    plugin: KattriPlugin,
}

/// Instantiate `component` with `state` as its host. The instantiation runs under the same
/// sandbox limits as any other call.
pub fn load(
    engine: &Engine,
    limits: &SandboxLimits,
    component: &Component,
    state: HostState,
) -> Result<LoadedPlugin, RuntimeError> {
    let mut linker: Linker<GuestData<HostState>> = Linker::new(engine);
    KattriPlugin::add_to_linker::<GuestData<HostState>, HasSelf<HostState>>(&mut linker, |data| {
        &mut data.state
    })
    .map_err(|err| RuntimeError::Runtime(err.to_string()))?;

    let mut store = new_store(engine, limits, state)?;
    arm(&mut store, limits)?;
    let plugin = run_bounded(engine, limits, || {
        KattriPlugin::instantiate(&mut store, component, &linker)
    })?;
    Ok(LoadedPlugin {
        engine: engine.clone(),
        limits: *limits,
        store,
        plugin,
    })
}

impl LoadedPlugin {
    /// Withdraw a user's grant while the plugin is running. Subsequent host calls and event
    /// dispatches are checked against the updated grant set.
    pub fn revoke_permission(&mut self, permission: &Permission) {
        self.store.data_mut().state.revoke_permission(permission);
    }

    /// Call the guest's `init` export with the plugin id.
    pub fn init(&mut self, plugin_id: &str) -> Result<(), RuntimeError> {
        arm(&mut self.store, &self.limits)?;
        let outcome = run_bounded(&self.engine, &self.limits, || {
            self.plugin.call_init(&mut self.store, plugin_id)
        })?;
        outcome.map_err(RuntimeError::Refused)
    }

    /// Call the guest's `on-playback-event` export.
    pub fn on_playback_event(&mut self, event: PlaybackEvent) -> Result<(), RuntimeError> {
        self.store.data().state.authorize_playback_read()?;
        arm(&mut self.store, &self.limits)?;
        run_bounded(&self.engine, &self.limits, || {
            self.plugin.call_on_playback_event(&mut self.store, event)
        })
    }

    /// The host state after the calls made so far.
    pub fn state(&self) -> &HostState {
        &self.store.data().state
    }

    /// Call the guest's `shutdown` export under a one-second budget, then drop the instance
    /// whether or not the call returned. Taking `self` makes later calls impossible.
    pub fn shutdown(mut self) -> Result<(), RuntimeError> {
        let limits = SandboxLimits::new(
            self.limits.fuel_per_call(),
            self.limits.memory_bytes(),
            SHUTDOWN_WALL_CLOCK,
            self.limits.max_output_bytes(),
        )?;
        arm(&mut self.store, &limits)?;
        run_bounded(&self.engine, &limits, || {
            self.plugin.call_shutdown(&mut self.store)
        })
    }
}
