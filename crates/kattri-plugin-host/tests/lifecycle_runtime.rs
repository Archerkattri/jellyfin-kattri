//! M4-05 runtime lifecycle: guest components driven through the world loader and the
//! registry. The components are WAT text written for these tests, not the sample plugin,
//! which needs the wasm32-wasip2 target that this machine does not have.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use kattri_plugin_host::bindings::{PlaybackEvent, PositionInfo};
use kattri_plugin_host::error::RuntimeError;
use kattri_plugin_host::sandbox::{self, instantiate, new_store};
use kattri_plugin_host::world::{self, HostState, LoadedPlugin};
use kattri_plugins::lifecycle::{DEFAULT_QUARANTINE_THRESHOLD, LifecycleEvent, LifecycleState};
use kattri_plugins::registry::Registry;
use kattri_plugins::sandbox::{
    DEFAULT_FUEL_PER_CALL, DEFAULT_MAX_OUTPUT_BYTES, DEFAULT_MEMORY_BYTES, DEFAULT_WALL_CLOCK,
    SandboxFailure, SandboxLimits, WALL_CLOCK_TOLERANCE,
};
use kattri_plugins::settings::PluginSettingsStore;
use kattri_plugins::{Permission, PermissionGrants, PluginManifest};
use wasmtime::Module;
use wasmtime::component::Component;

use LifecycleState as S;

const OK_INIT: &str = "(i32.store8 (i32.const 1500) (i32.const 0))";
const NOOP: &str = "";
const TRAP: &str = "unreachable";
const NEVER_RETURNS: &str = "(loop $l (br $l))";

/// A component for the `kattri-plugin` world. A `result<_, string>` flattens to more than one
/// value, so the lifted `init` returns the address of its memory form, and the core body
/// writes the discriminant there. `on-playback-event` and `shutdown` run the given bodies.
fn component(init_body: &str, event_body: &str, shutdown_body: &str) -> String {
    format!(
        r#"
(component
  (core module $m
    (memory (export "memory") 1)
    (func (export "realloc") (param i32 i32 i32 i32) (result i32) (i32.const 1024))
    (func (export "init") (param i32 i32) (result i32)
      {init_body}
      (i32.const 1500))
    (func (export "on-playback-event") (param i32 i64)
      {event_body})
    (func (export "shutdown")
      {shutdown_body}))
  (core instance $i (instantiate $m))
  (type $position (record (field "ticks" u64)))
  (export $position-info "position-info" (type $position))
  (type $event-ty
    (variant
      (case "started")
      (case "paused")
      (case "resumed")
      (case "stopped")
      (case "position" $position-info)))
  (export $event "playback-event" (type $event-ty))
  (type $outcome (result (error string)))
  (func $init (param "plugin-id" string) (result $outcome)
    (canon lift (core func $i "init") (memory (core memory $i "memory"))
      (realloc (core func $i "realloc"))))
  (func $on-event (param "event" $event)
    (canon lift (core func $i "on-playback-event")))
  (func $shut
    (canon lift (core func $i "shutdown")))
  (export "init" (func $init))
  (export "on-playback-event" (func $on-event))
  (export "shutdown" (func $shut)))
"#
    )
}

fn limits() -> SandboxLimits {
    SandboxLimits::new(
        DEFAULT_FUEL_PER_CALL,
        DEFAULT_MEMORY_BYTES,
        DEFAULT_WALL_CLOCK,
        DEFAULT_MAX_OUTPUT_BYTES,
    )
    .expect("default limits are valid")
}

fn manifest(id: &str) -> PluginManifest {
    PluginManifest::from_json(&format!(
        r#"{{"id":"{id}","name":"Test","version":"0.1.0","api_version":1,
            "entry":"src/lib.rs","permissions":[]}}"#
    ))
    .expect("test manifest is valid")
}

/// Compile and instantiate a test component in its own engine, as the host does per plugin.
fn load(id: &str, wat: &str) -> LoadedPlugin {
    load_with_playback_grant(id, wat, true)
}

fn load_with_playback_grant(id: &str, wat: &str, granted: bool) -> LoadedPlugin {
    let engine = sandbox::engine().expect("engine");
    let component = Component::new(&engine, wat).expect("test component compiles");
    let manifest = PluginManifest::from_json(&format!(
        r#"{{"id":"{id}","name":"Test","version":"0.1.0","api_version":1,
            "entry":"src/lib.rs","permissions":["playback.read"]}}"#
    ))
    .expect("manifest is valid");
    let mut grants = PermissionGrants::for_plugin(&manifest);
    if granted {
        grants.grant(Permission::PlaybackRead);
    }
    let state = HostState::new(manifest, grants, PluginSettingsStore::new());
    world::load(&engine, &limits(), &component, state).expect("component instantiates")
}

#[test]
fn playback_events_require_a_grant_and_stop_after_revocation() {
    let id = "org.example.playback-permission";
    let mut denied = load_with_playback_grant(id, &component(OK_INIT, TRAP, NOOP), false);
    assert!(matches!(
        denied.on_playback_event(PlaybackEvent::Started),
        Err(RuntimeError::Plugin(_))
    ));

    let mut granted = load(id, &component(OK_INIT, TRAP, NOOP));
    granted
        .on_playback_event(PlaybackEvent::Started)
        .expect_err("the authorized call reaches the trapping guest");
    granted.revoke_permission(&Permission::PlaybackRead);
    assert!(matches!(
        granted.on_playback_event(PlaybackEvent::Paused),
        Err(RuntimeError::Plugin(_))
    ));
}

/// The lifecycle event for a failed call. Only sandbox failures are expected here; any other
/// error would mean the test component is wrong.
fn failed(err: RuntimeError) -> LifecycleEvent {
    match err {
        RuntimeError::Sandbox(failure) => LifecycleEvent::CallFailed(failure),
        other => panic!("expected a sandbox failure, got {other:?}"),
    }
}

/// Register `id` and bring it to `Loaded`, as the host does after instantiation.
fn register_loaded(registry: &mut Registry, id: &str) {
    registry.register(manifest(id)).expect("register");
    registry
        .apply(id, LifecycleEvent::Loaded)
        .expect("validated plugin loads");
}

/// Dispatch targets, copied out so the registry can be mutated afterwards.
fn targets(registry: &Registry) -> Vec<(String, u64)> {
    registry
        .dispatch_targets()
        .into_iter()
        .map(|(id, generation)| (id.to_owned(), generation))
        .collect()
}

#[test]
fn a_trap_in_on_event_quarantines_one_plugin_and_the_host_keeps_dispatching() {
    const BAD: &str = "org.example.bad";
    const GOOD: &str = "org.example.good";
    let mut plugins: HashMap<String, LoadedPlugin> = HashMap::new();
    plugins.insert(BAD.to_owned(), load(BAD, &component(OK_INIT, TRAP, NOOP)));
    plugins.insert(GOOD.to_owned(), load(GOOD, &component(OK_INIT, NOOP, NOOP)));

    let mut registry = Registry::new(DEFAULT_QUARANTINE_THRESHOLD).expect("registry");
    register_loaded(&mut registry, BAD);
    register_loaded(&mut registry, GOOD);
    assert_eq!(registry.state(BAD).expect("state"), S::Loaded);

    // init succeeds for both plugins, so each becomes Running.
    for id in [BAD, GOOD] {
        plugins
            .get_mut(id)
            .expect("loaded")
            .init(id)
            .expect("init succeeds");
        registry
            .report(id, 1, LifecycleEvent::CallSucceeded)
            .expect("init report");
    }
    assert_eq!(registry.state(BAD).expect("state"), S::Running);

    // The first event traps in BAD. GOOD handles the same event in the same dispatch.
    let mut outcomes: HashMap<String, Result<(), RuntimeError>> = HashMap::new();
    for (id, generation) in targets(&registry) {
        let outcome = plugins
            .get_mut(&id)
            .expect("loaded")
            .on_playback_event(PlaybackEvent::Started);
        let event = match &outcome {
            Ok(()) => LifecycleEvent::CallSucceeded,
            Err(err) => failed(err.clone()),
        };
        registry.report(&id, generation, event).expect("report");
        outcomes.insert(id, outcome);
    }

    let bad_outcome = outcomes.get(BAD).expect("BAD was dispatched");
    assert!(
        matches!(
            bad_outcome,
            Err(RuntimeError::Sandbox(SandboxFailure::Trap { .. }))
        ),
        "a trap, not a timeout or fuel error: {bad_outcome:?}"
    );
    assert!(outcomes.get(GOOD).is_some_and(Result::is_ok));
    assert_eq!(registry.state(BAD).expect("state"), S::Quarantined);
    assert_eq!(registry.state(GOOD).expect("state"), S::Running);

    // A quarantined plugin is no longer a dispatch target, and the host keeps dispatching.
    let next: Vec<String> = targets(&registry).into_iter().map(|(id, _)| id).collect();
    assert_eq!(next, [GOOD.to_owned()]);
    plugins
        .get_mut(GOOD)
        .expect("loaded")
        .on_playback_event(PlaybackEvent::Paused)
        .expect("the healthy plugin still answers");
}

#[test]
fn a_trap_on_the_first_event_after_a_good_init_quarantines_the_plugin() {
    let id = "org.example.first-event";
    let mut plugin = load(id, &component(OK_INIT, TRAP, NOOP));
    plugin.init(id).expect("init succeeds");

    let err = plugin
        .on_playback_event(PlaybackEvent::Started)
        .expect_err("the first event traps");
    assert!(
        matches!(
            err,
            RuntimeError::Sandbox(SandboxFailure::Trap { ref detail }) if detail.contains("unreachable")
        ),
        "got {err:?}"
    );

    let mut registry = Registry::new(DEFAULT_QUARANTINE_THRESHOLD).expect("registry");
    register_loaded(&mut registry, id);
    registry
        .report(id, 1, LifecycleEvent::CallSucceeded)
        .expect("init report");
    registry.report(id, 1, failed(err)).expect("event report");
    assert_eq!(registry.state(id).expect("state"), S::Quarantined);
}

#[test]
fn a_position_payload_reaches_the_guest_before_the_trap() {
    let id = "org.example.position";
    let mut plugin = load(id, &component(OK_INIT, TRAP, NOOP));
    plugin.init(id).expect("init succeeds");
    // The guest traps on every event. A trap, rather than a lowering error, shows that the
    // record payload was lifted and the call reached guest code.
    let err = plugin
        .on_playback_event(PlaybackEvent::Position(PositionInfo { ticks: 42 }))
        .expect_err("traps");
    assert!(
        matches!(err, RuntimeError::Sandbox(SandboxFailure::Trap { .. })),
        "got {err:?}"
    );
}

#[test]
fn reload_after_quarantine_rejects_a_result_from_the_old_generation() {
    let id = "org.example.reload";
    let mut plugin = load(id, &component(OK_INIT, TRAP, NOOP));
    plugin.init(id).expect("init succeeds");

    let mut registry = Registry::new(DEFAULT_QUARANTINE_THRESHOLD).expect("registry");
    register_loaded(&mut registry, id);
    registry
        .report(id, 1, LifecycleEvent::CallSucceeded)
        .expect("init report");

    // Two events are in flight under generation 1. The first trap quarantines the plugin,
    // the user reloads it, and then the second result arrives.
    let first = plugin.on_playback_event(PlaybackEvent::Started);
    let second = plugin.on_playback_event(PlaybackEvent::Stopped);
    registry
        .report(id, 1, failed(first.expect_err("traps")))
        .expect("first failure quarantines");
    assert_eq!(registry.state(id).expect("state"), S::Quarantined);

    registry
        .apply(id, LifecycleEvent::Reload)
        .expect("quarantined plugin reloads");
    assert_eq!(registry.generation(id).expect("generation"), 2);

    let stale = registry.report(id, 1, failed(second.expect_err("traps")));
    assert!(stale.is_err(), "a result from before the reload is refused");
    assert_eq!(registry.state(id).expect("state"), S::Validated);
}

#[test]
fn shutdown_unloads_the_plugin_and_removes_it_from_dispatch() {
    let id = "org.example.unload";
    let mut plugin = load(id, &component(OK_INIT, NOOP, NOOP));
    plugin.init(id).expect("init succeeds");

    let mut registry = Registry::new(DEFAULT_QUARANTINE_THRESHOLD).expect("registry");
    register_loaded(&mut registry, id);
    registry
        .report(id, 1, LifecycleEvent::CallSucceeded)
        .expect("init report");
    plugin
        .on_playback_event(PlaybackEvent::Started)
        .expect("a healthy event");
    registry
        .report(id, 1, LifecycleEvent::CallSucceeded)
        .expect("event report");
    assert_eq!(registry.state(id).expect("state"), S::Running);

    registry
        .apply(id, LifecycleEvent::Shutdown)
        .expect("running plugin shuts down");
    plugin.shutdown().expect("shutdown returns");
    assert_eq!(registry.state(id).expect("state"), S::Unloaded);
    assert!(
        targets(&registry).is_empty(),
        "an unloaded plugin is not dispatched"
    );
}

#[test]
fn a_shutdown_that_never_returns_is_stopped_with_a_sandbox_error() {
    let id = "org.example.stuck";
    let plugin = load(id, &component(OK_INIT, NOOP, NEVER_RETURNS));

    let started = Instant::now();
    let err = plugin.shutdown().expect_err("the loop never returns");
    let elapsed = started.elapsed();

    assert!(matches!(err, RuntimeError::Sandbox(_)), "got {err:?}");
    // The one-second shutdown budget plus the test tolerance bounds the wait.
    assert!(
        elapsed < Duration::from_secs(1) + WALL_CLOCK_TOLERANCE,
        "shutdown took {elapsed:?}"
    );
}

struct DropProbe(Arc<AtomicUsize>);

impl Drop for DropProbe {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn dropping_the_store_frees_the_instance_and_its_host_state() {
    // The host keeps one store per plugin, and the store owns the instance and the host
    // state. Dropping the store is what frees both, so a probe in the host state counts it.
    let drops = Arc::new(AtomicUsize::new(0));
    let engine = sandbox::engine().expect("engine");
    let module =
        Module::new(&engine, r#"(module (memory (export "memory") 1))"#).expect("module compiles");
    let limits = limits();
    {
        let mut store = new_store(&engine, &limits, DropProbe(Arc::clone(&drops))).expect("store");
        let _instance = instantiate(&engine, &limits, &mut store, &module).expect("instantiate");
        assert_eq!(
            drops.load(Ordering::SeqCst),
            0,
            "alive while the store lives"
        );
    }
    assert_eq!(drops.load(Ordering::SeqCst), 1, "dropped exactly once");
}
