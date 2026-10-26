//! M4-05: lifecycle transition table, quarantine threshold and registry isolation.
//! No guest code runs here; the trap-in-on_event crash test needs the runtime.

use std::time::Duration;

use kattri_plugins::lifecycle::{
    DEFAULT_QUARANTINE_THRESHOLD, Failure, Lifecycle, LifecycleEvent, LifecycleState,
};
use kattri_plugins::registry::Registry;
use kattri_plugins::sandbox::SandboxFailure;
use kattri_plugins::{PluginError, PluginManifest};

use LifecycleState as S;

fn manifest(id: &str) -> PluginManifest {
    PluginManifest::from_json(&format!(
        r#"{{"id":"{id}","name":"P","version":"1.0.0","api_version":1,"entry":"main.wasm"}}"#
    ))
    .expect("test manifest is valid")
}

fn fuel_failure() -> LifecycleEvent {
    LifecycleEvent::CallFailed(SandboxFailure::FuelExhausted { budget: 10 })
}

/// A lifecycle standing in `state`, built only through legal transitions.
fn at(state: LifecycleState) -> Lifecycle {
    let mut lc = Lifecycle::new(DEFAULT_QUARANTINE_THRESHOLD).expect("threshold 1 is valid");
    let path: Vec<LifecycleEvent> = match state {
        S::Discovered => vec![],
        S::Validated => vec![LifecycleEvent::Validate],
        S::Loaded => vec![LifecycleEvent::Validate, LifecycleEvent::Loaded],
        S::Running => vec![
            LifecycleEvent::Validate,
            LifecycleEvent::Loaded,
            LifecycleEvent::CallSucceeded,
        ],
        S::Unloaded => vec![
            LifecycleEvent::Validate,
            LifecycleEvent::Loaded,
            LifecycleEvent::Shutdown,
        ],
        S::Quarantined => vec![
            LifecycleEvent::Validate,
            LifecycleEvent::Loaded,
            LifecycleEvent::PermissionViolation {
                permission: "network".to_owned(),
            },
        ],
    };
    for event in path {
        lc.apply(event).expect("setup path is legal");
    }
    assert_eq!(lc.state(), state);
    lc
}

fn all_states() -> [LifecycleState; 6] {
    [
        S::Discovered,
        S::Validated,
        S::Loaded,
        S::Running,
        S::Unloaded,
        S::Quarantined,
    ]
}

fn all_events() -> Vec<LifecycleEvent> {
    vec![
        LifecycleEvent::Validate,
        LifecycleEvent::Loaded,
        LifecycleEvent::CallSucceeded,
        fuel_failure(),
        LifecycleEvent::PermissionViolation {
            permission: "network".to_owned(),
        },
        LifecycleEvent::Shutdown,
        LifecycleEvent::Reload,
    ]
}

/// The full expected table with threshold 1: `Some(next)` if allowed, `None` if refused.
fn expected(state: LifecycleState, event: &LifecycleEvent) -> Option<LifecycleState> {
    use LifecycleEvent as E;
    match (state, event) {
        (S::Discovered, E::Validate) => Some(S::Validated),
        (S::Validated, E::Loaded) => Some(S::Loaded),
        (S::Loaded | S::Running, E::CallSucceeded) => Some(S::Running),
        (S::Loaded | S::Running, E::CallFailed(_)) => Some(S::Quarantined),
        (S::Loaded | S::Running, E::PermissionViolation { .. }) => Some(S::Quarantined),
        (S::Loaded | S::Running | S::Quarantined, E::Shutdown) => Some(S::Unloaded),
        (S::Quarantined | S::Unloaded, E::Reload) => Some(S::Validated),
        _ => None,
    }
}

#[test]
fn every_state_and_event_pair_follows_the_table() {
    for state in all_states() {
        for event in all_events() {
            let mut lc = at(state);
            let want = expected(state, &event);
            let got = lc.apply(event.clone());
            match want {
                Some(next) => {
                    assert_eq!(got, Ok(next), "{state:?} + {event:?}");
                    assert_eq!(lc.state(), next, "{state:?} + {event:?}");
                }
                None => {
                    assert!(
                        matches!(got, Err(PluginError::InvalidTransition { .. })),
                        "{state:?} + {event:?} must be refused, got {got:?}"
                    );
                    assert_eq!(lc.state(), state, "refused event must not move state");
                }
            }
        }
    }
}

#[test]
fn only_loaded_and_running_can_dispatch() {
    for state in all_states() {
        let want = matches!(state, S::Loaded | S::Running);
        assert_eq!(state.can_dispatch(), want, "{state:?}");
    }
}

#[test]
fn quarantine_threshold_counts_consecutive_failures() {
    let mut lc = Lifecycle::new(3).expect("3 is valid");
    for event in [
        LifecycleEvent::Validate,
        LifecycleEvent::Loaded,
        LifecycleEvent::CallSucceeded,
    ] {
        lc.apply(event).expect("legal");
    }
    lc.apply(fuel_failure()).expect("legal");
    lc.apply(fuel_failure()).expect("legal");
    assert_eq!(
        lc.state(),
        S::Running,
        "two failures are below the threshold"
    );
    lc.apply(LifecycleEvent::CallSucceeded).expect("legal");
    assert_eq!(lc.consecutive_failures(), 0, "a success resets the count");
    lc.apply(fuel_failure()).expect("legal");
    lc.apply(fuel_failure()).expect("legal");
    assert_eq!(lc.state(), S::Running);
    lc.apply(fuel_failure()).expect("legal");
    assert_eq!(
        lc.state(),
        S::Quarantined,
        "third consecutive failure quarantines"
    );
    assert_eq!(
        lc.last_failure(),
        Some(&Failure::Sandbox(SandboxFailure::FuelExhausted {
            budget: 10
        }))
    );
}

#[test]
fn permission_violation_quarantines_even_with_a_high_threshold() {
    let mut lc = Lifecycle::new(10).expect("10 is valid");
    lc.apply(LifecycleEvent::Validate).expect("legal");
    lc.apply(LifecycleEvent::Loaded).expect("legal");
    lc.apply(LifecycleEvent::PermissionViolation {
        permission: "presence.write".to_owned(),
    })
    .expect("legal");
    assert_eq!(lc.state(), S::Quarantined);
    assert_eq!(
        lc.last_failure(),
        Some(&Failure::PermissionViolation {
            permission: "presence.write".to_owned()
        })
    );
}

#[test]
fn threshold_must_be_between_one_and_ten() {
    assert!(matches!(
        Lifecycle::new(0),
        Err(PluginError::Invalid {
            field: "quarantine_threshold",
            ..
        })
    ));
    assert!(matches!(
        Lifecycle::new(11),
        Err(PluginError::Invalid {
            field: "quarantine_threshold",
            ..
        })
    ));
    assert!(Lifecycle::new(1).is_ok());
    assert!(Lifecycle::new(10).is_ok());
}

#[test]
fn reload_after_quarantine_returns_to_validated_and_clears_the_count() {
    let mut lc = at(S::Quarantined);
    assert!(!lc.state().can_dispatch());
    assert_eq!(lc.apply(LifecycleEvent::Reload), Ok(S::Validated));
    assert_eq!(lc.consecutive_failures(), 0);
    assert!(lc.last_failure().is_none());
    lc.apply(LifecycleEvent::Loaded).expect("legal");
    assert_eq!(lc.state(), S::Loaded);
}

#[test]
fn invalid_transition_names_both_state_and_event() {
    let mut lc = at(S::Discovered);
    assert_eq!(
        lc.apply(LifecycleEvent::CallSucceeded),
        Err(PluginError::InvalidTransition {
            from: "Discovered",
            event: "CallSucceeded",
        })
    );
}

#[test]
fn registry_isolates_a_failing_plugin() {
    let mut reg = Registry::new(1).expect("valid");
    reg.register(manifest("org.example.bad")).expect("register");
    reg.register(manifest("org.example.good"))
        .expect("register");
    for id in ["org.example.bad", "org.example.good"] {
        let generation = reg.generation(id).expect("known");
        reg.report(id, generation, LifecycleEvent::Loaded)
            .expect("loaded");
        reg.report(id, generation, LifecycleEvent::CallSucceeded)
            .expect("running");
    }
    let bad_gen = reg.generation("org.example.bad").expect("known");
    reg.report(
        "org.example.bad",
        bad_gen,
        LifecycleEvent::CallFailed(SandboxFailure::Timeout {
            limit: Duration::from_secs(2),
            elapsed: Duration::from_secs(3),
        }),
    )
    .expect("failure recorded");

    assert_eq!(reg.state("org.example.bad"), Ok(S::Quarantined));
    assert_eq!(reg.state("org.example.good"), Ok(S::Running));
    let targets: Vec<&str> = reg
        .dispatch_targets()
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    assert_eq!(
        targets,
        vec!["org.example.good"],
        "quarantined plugin receives no events"
    );
}

#[test]
fn register_rejects_duplicate_ids_and_unknown_ids_are_errors() {
    let mut reg = Registry::new(1).expect("valid");
    reg.register(manifest("org.example.one")).expect("first");
    assert_eq!(
        reg.register(manifest("org.example.one")),
        Err(PluginError::DuplicateId {
            id: "org.example.one".to_owned()
        })
    );
    assert_eq!(
        reg.state("org.example.nope"),
        Err(PluginError::UnknownPlugin {
            id: "org.example.nope".to_owned()
        })
    );
}

#[test]
fn result_from_before_a_reload_is_stale_and_changes_nothing() {
    let mut reg = Registry::new(1).expect("valid");
    reg.register(manifest("org.example.a")).expect("register");
    reg.apply("org.example.a", LifecycleEvent::Loaded)
        .expect("legal");
    reg.apply("org.example.a", LifecycleEvent::CallSucceeded)
        .expect("legal");
    let old_gen = reg.generation("org.example.a").expect("known");
    reg.apply("org.example.a", LifecycleEvent::Shutdown)
        .expect("legal");
    reg.apply("org.example.a", LifecycleEvent::Reload)
        .expect("legal");
    reg.apply("org.example.a", LifecycleEvent::Loaded)
        .expect("legal");
    assert_eq!(reg.generation("org.example.a"), Ok(old_gen + 1));

    let late = reg.report("org.example.a", old_gen, fuel_failure());
    assert_eq!(
        late,
        Err(PluginError::StaleResult {
            id: "org.example.a".to_owned()
        })
    );
    assert_eq!(
        reg.state("org.example.a"),
        Ok(S::Loaded),
        "stale failure must not quarantine"
    );
}

#[test]
fn shutdown_all_skips_plugins_that_cannot_shut_down() {
    let mut reg = Registry::new(1).expect("valid");
    reg.register(manifest("org.example.loaded"))
        .expect("register");
    reg.register(manifest("org.example.idle"))
        .expect("register");
    reg.apply("org.example.loaded", LifecycleEvent::Loaded)
        .expect("legal");
    assert_eq!(reg.shutdown_all(), 1);
    assert_eq!(reg.state("org.example.loaded"), Ok(S::Unloaded));
    assert_eq!(reg.state("org.example.idle"), Ok(S::Validated));
}
