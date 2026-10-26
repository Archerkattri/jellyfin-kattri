//! M4-05 plugin registry: the set of known plugins and their lifecycle states.
//!
//! The registry owns one [`Lifecycle`] per plugin and a generation counter. A reload bumps
//! the generation, so a call result that was in flight before the reload is rejected as
//! [`PluginError::StaleResult`] instead of changing the reloaded plugin's state.
//! Quarantined plugins never appear in [`Registry::dispatch_targets`].

use crate::error::PluginError;
use crate::lifecycle::{Lifecycle, LifecycleEvent, LifecycleState};
use crate::manifest::PluginManifest;

struct Entry {
    manifest: PluginManifest,
    lifecycle: Lifecycle,
    generation: u64,
}

/// Known plugins in registration order.
pub struct Registry {
    threshold: u32,
    entries: Vec<Entry>,
}

impl Registry {
    /// An empty registry. `threshold` is the consecutive-failure count that quarantines
    /// (1 to 10, see [`crate::lifecycle::MAX_QUARANTINE_THRESHOLD`]).
    pub fn new(threshold: u32) -> Result<Self, PluginError> {
        // Validate the threshold once, up front, with the same rule every lifecycle uses.
        Lifecycle::new(threshold)?;
        Ok(Self {
            threshold,
            entries: Vec::new(),
        })
    }

    /// Add a validated manifest. Its id must not already be registered. The plugin
    /// starts in `Validated` at generation 1.
    pub fn register(&mut self, manifest: PluginManifest) -> Result<(), PluginError> {
        if self.entries.iter().any(|e| e.manifest.id == manifest.id) {
            return Err(PluginError::DuplicateId { id: manifest.id });
        }
        let mut lifecycle = Lifecycle::new(self.threshold)?;
        lifecycle.apply(LifecycleEvent::Validate)?;
        self.entries.push(Entry {
            manifest,
            lifecycle,
            generation: 1,
        });
        Ok(())
    }

    /// Apply an event that is not a call result (`Validate`, `Shutdown`, `Reload`).
    pub fn apply(
        &mut self,
        id: &str,
        event: LifecycleEvent,
    ) -> Result<LifecycleState, PluginError> {
        let entry = self.entry_mut(id)?;
        let is_reload = event == LifecycleEvent::Reload;
        let state = entry.lifecycle.apply(event)?;
        if is_reload {
            entry.generation = entry.generation.saturating_add(1);
        }
        Ok(state)
    }

    /// Apply the result of a call made under `generation`. A result from an earlier
    /// generation (the plugin was reloaded since the call started) is rejected.
    pub fn report(
        &mut self,
        id: &str,
        generation: u64,
        event: LifecycleEvent,
    ) -> Result<LifecycleState, PluginError> {
        let entry = self.entry_mut(id)?;
        if entry.generation != generation {
            return Err(PluginError::StaleResult { id: id.to_owned() });
        }
        entry.lifecycle.apply(event)
    }

    pub fn state(&self, id: &str) -> Result<LifecycleState, PluginError> {
        Ok(self.entry(id)?.lifecycle.state())
    }

    pub fn generation(&self, id: &str) -> Result<u64, PluginError> {
        Ok(self.entry(id)?.generation)
    }

    pub fn manifest(&self, id: &str) -> Result<&PluginManifest, PluginError> {
        Ok(&self.entry(id)?.manifest)
    }

    /// Plugins that may receive an event now, with the generation to report against.
    /// Quarantined, unloaded and not-yet-loaded plugins are excluded.
    pub fn dispatch_targets(&self) -> Vec<(&str, u64)> {
        self.entries
            .iter()
            .filter(|e| e.lifecycle.state().can_dispatch())
            .map(|e| (e.manifest.id.as_str(), e.generation))
            .collect()
    }

    /// Shut down every plugin that accepts `Shutdown`. Returns how many were shut down.
    pub fn shutdown_all(&mut self) -> usize {
        let mut count = 0;
        for entry in &mut self.entries {
            if entry.lifecycle.apply(LifecycleEvent::Shutdown).is_ok() {
                count += 1;
            }
        }
        count
    }

    fn entry(&self, id: &str) -> Result<&Entry, PluginError> {
        self.entries
            .iter()
            .find(|e| e.manifest.id == id)
            .ok_or_else(|| PluginError::UnknownPlugin { id: id.to_owned() })
    }

    fn entry_mut(&mut self, id: &str) -> Result<&mut Entry, PluginError> {
        self.entries
            .iter_mut()
            .find(|e| e.manifest.id == id)
            .ok_or_else(|| PluginError::UnknownPlugin { id: id.to_owned() })
    }
}
