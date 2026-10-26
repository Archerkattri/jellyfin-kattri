//! Generated bindings for the `kattri-plugin` world in `kattri-plugins/wit/kattri-plugin.wit`.
//!
//! The imports (`presence-write`, `settings-get`, `settings-set`, `log-write`) are implemented
//! for [`crate::world::HostState`], where each one is checked against the manifest grants. The
//! exports (`init`, `on-playback-event`) are called through [`KattriPlugin`].

mod generated {
    wasmtime::component::bindgen!({
        path: "../kattri-plugins/wit",
        world: "kattri-plugin",
    });
}

pub use generated::{KattriPlugin, KattriPluginImports, PlaybackEvent, PositionInfo};
