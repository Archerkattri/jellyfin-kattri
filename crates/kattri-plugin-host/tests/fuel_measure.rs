//! Opt-in fuel measurement for the sample presence component (M4-04 fuel calibration).
//!
//! The component must be built first; see the build command in
//! `crates/kattri-plugins/examples/sample-presence/src/lib.rs`. Run with:
//! `cargo test --locked --offline -p kattri-plugin-host --test fuel_measure -- --ignored --nocapture`
//!
//! Each host-to-guest call is armed with the full default budget, so the fuel used by one
//! call is the budget minus the fuel left in the store afterwards.

use kattri_plugin_host::bindings::{KattriPlugin, PlaybackEvent};
use kattri_plugin_host::error::RuntimeError;
use kattri_plugin_host::sandbox::{self, GuestData, arm, new_store, run_bounded};
use kattri_plugin_host::world::HostState;
use kattri_plugins::sandbox::{
    DEFAULT_FUEL_PER_CALL, DEFAULT_MAX_OUTPUT_BYTES, DEFAULT_MEMORY_BYTES, DEFAULT_WALL_CLOCK,
    SandboxLimits,
};
use kattri_plugins::settings::PluginSettingsStore;
use kattri_plugins::{Permission, PermissionGrants, PluginManifest};
use wasmtime::component::{Component, HasSelf, Linker};
use wasmtime::{Engine, Store};

const COMPONENT_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../target/wasm32-wasip2/release/sample_presence.wasm"
);
const PLUGIN_ID: &str = "org.kattri.sample-presence";

fn manifest() -> PluginManifest {
    let text = format!(
        r#"{{"id":"{PLUGIN_ID}","name":"Sample Presence","version":"0.1.0",
            "api_version":1,"entry":"src/lib.rs",
            "permissions":["presence.write","log.write"]}}"#
    );
    PluginManifest::from_json(&text).expect("test manifest is valid")
}

fn host_state() -> HostState {
    let manifest = manifest();
    let mut grants = PermissionGrants::for_plugin(&manifest);
    grants.grant(Permission::PresenceWrite);
    grants.grant(Permission::LogWrite);
    HostState::new(manifest, grants, PluginSettingsStore::new())
}

/// Run one guest call with the full budget armed, and return its result with the fuel used.
fn fuel_for_call<R>(
    engine: &Engine,
    limits: &SandboxLimits,
    store: &mut Store<GuestData<HostState>>,
    call: impl FnOnce(&mut Store<GuestData<HostState>>) -> wasmtime::Result<R>,
) -> Result<(R, u64), RuntimeError> {
    arm(store, limits)?;
    let outcome = run_bounded(engine, limits, || call(&mut *store))?;
    let left = store
        .get_fuel()
        .map_err(|err| RuntimeError::Runtime(err.to_string()))?;
    Ok((outcome, limits.fuel_per_call() - left))
}

#[test]
#[ignore = "needs the built sample component at target/wasm32-wasip2/release/sample_presence.wasm"]
fn sample_component_fuel_per_call() {
    let engine = sandbox::engine().expect("engine");
    let limits = SandboxLimits::new(
        DEFAULT_FUEL_PER_CALL,
        DEFAULT_MEMORY_BYTES,
        DEFAULT_WALL_CLOCK,
        DEFAULT_MAX_OUTPUT_BYTES,
    )
    .expect("default limits are valid");
    let component = Component::from_file(&engine, COMPONENT_PATH).expect("sample component loads");

    let mut linker: Linker<GuestData<HostState>> = Linker::new(&engine);
    KattriPlugin::add_to_linker::<GuestData<HostState>, HasSelf<HostState>>(&mut linker, |data| {
        &mut data.state
    })
    .expect("host imports link");
    let mut store = new_store(&engine, &limits, host_state()).expect("store");

    let (plugin, instantiate_fuel) = fuel_for_call(&engine, &limits, &mut store, |store| {
        KattriPlugin::instantiate(store, &component, &linker)
    })
    .expect("instantiate under budget");

    let (init_result, init_fuel) = fuel_for_call(&engine, &limits, &mut store, |store| {
        plugin.call_init(store, PLUGIN_ID)
    })
    .expect("init under budget");
    init_result.expect("init returns ok");
    assert_eq!(
        store.data().state.log_lines(),
        ["[org.kattri.sample-presence] sample-presence started".to_owned()],
        "init logged through the granted host call"
    );

    let (_, event_fuel) = fuel_for_call(&engine, &limits, &mut store, |store| {
        plugin.call_on_playback_event(store, PlaybackEvent::Started)
    })
    .expect("one playback event under budget");
    let presence = store
        .data()
        .state
        .last_presence()
        .expect("the playback event reached the presence host call");
    assert_eq!(presence.state.as_deref(), Some("Playing"));

    let (_, shutdown_fuel) = fuel_for_call(&engine, &limits, &mut store, |store| {
        plugin.call_shutdown(store)
    })
    .expect("shutdown under budget");

    let budget = limits.fuel_per_call();
    println!("fuel budget per call: {budget}");
    println!(
        "fuel used: instantiate={instantiate_fuel} init={init_fuel} on_playback_event(started)={event_fuel} shutdown={shutdown_fuel}"
    );
    let worst = [instantiate_fuel, init_fuel, event_fuel, shutdown_fuel]
        .into_iter()
        .max()
        .unwrap_or(0);
    println!(
        "largest single call: {worst} units, headroom factor {:.1}x",
        budget as f64 / worst.max(1) as f64
    );

    assert!(worst < budget, "a call used the whole budget");
    assert!(
        event_fuel > 0,
        "the event call consumed fuel, so metering is active"
    );
}
