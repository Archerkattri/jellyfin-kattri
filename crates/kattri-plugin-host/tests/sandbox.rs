//! Sandbox enforcement tests. Guest modules are WAT text compiled by wasmtime.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

use kattri_plugin_host::error::RuntimeError;
use kattri_plugin_host::sandbox::{
    self, call_typed, instantiate, new_store, output_from_export, read_output,
};
use kattri_plugins::sandbox::{
    DEFAULT_FUEL_PER_CALL, DEFAULT_MAX_OUTPUT_BYTES, DEFAULT_MEMORY_BYTES, DEFAULT_WALL_CLOCK,
    SandboxFailure, SandboxLimits, WALL_CLOCK_TOLERANCE,
};
use wasmtime::Module;

const MIB: u64 = 1024 * 1024;

const SPIN: &str = r#"(module (func (export "spin") (loop $l (br $l))))"#;

const GROW: &str = r#"
(module
  (memory (export "memory") 1)
  (func (export "grow") (param i32) (result i32) (memory.grow (local.get 0)))
  (func (export "size") (result i32) (memory.size)))
"#;

const OUTPUT: &str = r#"
(module
  (memory (export "memory") 1)
  (data (i32.const 0) "0123456789abcdef0123456789abcdef")
  (func (export "out") (result i32) (i32.const 32)))
"#;

const BOOM: &str = r#"(module (func (export "boom") unreachable))"#;

#[test]
fn sandbox_engine_accepts_component_model_components() {
    let engine = sandbox::engine().expect("component-enabled engine");
    wasmtime::component::Component::new(&engine, "(component)")
        .expect("component model is enabled on the plugin engine");
}

fn limits(fuel: u64, memory: u64, wall: Duration, output: usize) -> SandboxLimits {
    SandboxLimits::new(fuel, memory, wall, output).expect("test limits are in range")
}

#[test]
fn infinite_loop_trips_the_fuel_limit_with_default_limits() {
    let engine = sandbox::engine().expect("engine");
    let limits = limits(
        DEFAULT_FUEL_PER_CALL,
        DEFAULT_MEMORY_BYTES,
        DEFAULT_WALL_CLOCK,
        DEFAULT_MAX_OUTPUT_BYTES,
    );
    let module = Module::new(&engine, SPIN).expect("spin module compiles");
    let mut store = new_store(&engine, &limits, ()).expect("store");
    let instance = instantiate(&engine, &limits, &mut store, &module).expect("instantiate");

    let err = call_typed::<(), (), ()>(&engine, &limits, &mut store, instance, "spin", ())
        .expect_err("an infinite loop must not return");
    assert_eq!(
        err,
        RuntimeError::Sandbox(SandboxFailure::FuelExhausted {
            budget: DEFAULT_FUEL_PER_CALL
        })
    );
}

#[test]
fn infinite_loop_trips_the_epoch_deadline_when_fuel_is_ample() {
    let engine = sandbox::engine().expect("engine");
    let wall = Duration::from_millis(50);
    let limits = limits(
        1_000_000_000,
        DEFAULT_MEMORY_BYTES,
        wall,
        DEFAULT_MAX_OUTPUT_BYTES,
    );
    let module = Module::new(&engine, SPIN).expect("spin module compiles");
    let mut store = new_store(&engine, &limits, ()).expect("store");
    let instance = instantiate(&engine, &limits, &mut store, &module).expect("instantiate");

    let err = call_typed::<(), (), ()>(&engine, &limits, &mut store, instance, "spin", ())
        .expect_err("an infinite loop must not return");
    match err {
        RuntimeError::Sandbox(SandboxFailure::Timeout { limit, elapsed }) => {
            assert_eq!(limit, wall);
            assert!(
                elapsed < limits.deadline_with_tolerance(),
                "the call ran {elapsed:?}, past the tolerance {WALL_CLOCK_TOLERANCE:?}"
            );
        }
        other => panic!("expected a timeout, got {other:?}"),
    }
}

#[test]
fn timed_out_guest_runs_off_the_host_thread_and_heartbeat_keeps_advancing() {
    let heartbeat = Arc::new(AtomicUsize::new(0));
    let guest = thread::spawn(move || {
        let engine = sandbox::engine().expect("engine");
        let limits = limits(
            1_000_000_000,
            DEFAULT_MEMORY_BYTES,
            Duration::from_millis(50),
            DEFAULT_MAX_OUTPUT_BYTES,
        );
        let module = Module::new(&engine, SPIN).expect("spin module compiles");
        let mut store = new_store(&engine, &limits, ()).expect("store");
        let instance = instantiate(&engine, &limits, &mut store, &module).expect("instantiate");
        call_typed::<(), (), ()>(&engine, &limits, &mut store, instance, "spin", ())
    });

    while !guest.is_finished() {
        heartbeat.fetch_add(1, Ordering::Relaxed);
        thread::yield_now();
    }
    let result = guest
        .join()
        .expect("guest worker thread survives the timeout");

    assert!(heartbeat.load(Ordering::Relaxed) > 0);
    assert!(matches!(
        result,
        Err(RuntimeError::Sandbox(SandboxFailure::Timeout { .. }))
    ));
}

#[test]
fn memory_growth_past_the_limit_fails_cleanly() {
    let engine = sandbox::engine().expect("engine");
    let limits = limits(
        DEFAULT_FUEL_PER_CALL,
        MIB,
        DEFAULT_WALL_CLOCK,
        DEFAULT_MAX_OUTPUT_BYTES,
    );
    let module = Module::new(&engine, GROW).expect("grow module compiles");
    let mut store = new_store(&engine, &limits, ()).expect("store");
    let instance = instantiate(&engine, &limits, &mut store, &module).expect("instantiate");

    // Asking for 1000 pages (about 62 MiB) against a 1 MiB cap returns -1 to the guest.
    let refused: i32 = call_typed(&engine, &limits, &mut store, instance, "grow", (1000_i32,))
        .expect("a refused growth is not a trap");
    assert_eq!(refused, -1);
    let size: i32 =
        call_typed(&engine, &limits, &mut store, instance, "size", ()).expect("size call");
    assert_eq!(size, 1, "a refused growth leaves the memory unchanged");

    // Growing to exactly the cap succeeds: 15 more pages makes 16 pages, which is 1 MiB.
    let old: i32 = call_typed(&engine, &limits, &mut store, instance, "grow", (15_i32,))
        .expect("growth to the cap");
    assert_eq!(old, 1);

    // One more page is past the cap and is refused again.
    let refused_again: i32 = call_typed(&engine, &limits, &mut store, instance, "grow", (1_i32,))
        .expect("refused growth");
    assert_eq!(refused_again, -1);
    let memory = instance
        .get_memory(&mut store, "memory")
        .expect("memory export");
    assert_eq!(memory.data_size(&store), MIB as usize);
}

#[test]
fn initial_memory_past_the_limit_is_refused_at_instantiation() {
    let engine = sandbox::engine().expect("engine");
    let limits = limits(
        DEFAULT_FUEL_PER_CALL,
        MIB,
        DEFAULT_WALL_CLOCK,
        DEFAULT_MAX_OUTPUT_BYTES,
    );
    // 32 pages is 2 MiB, above the 1 MiB cap.
    let module =
        Module::new(&engine, r#"(module (memory (export "memory") 32))"#).expect("module compiles");
    let mut store = new_store(&engine, &limits, ()).expect("store");
    let result = instantiate(&engine, &limits, &mut store, &module);
    assert!(
        result.is_err(),
        "instantiation above the memory cap must fail"
    );
}

#[test]
fn output_past_the_cap_is_refused() {
    let engine = sandbox::engine().expect("engine");
    let module = Module::new(&engine, OUTPUT).expect("output module compiles");

    let tight = limits(
        DEFAULT_FUEL_PER_CALL,
        DEFAULT_MEMORY_BYTES,
        DEFAULT_WALL_CLOCK,
        16,
    );
    let mut store = new_store(&engine, &tight, ()).expect("store");
    let instance = instantiate(&engine, &tight, &mut store, &module).expect("instantiate");
    let err = output_from_export(&engine, &tight, &mut store, instance, "out")
        .expect_err("32 bytes is over a 16 byte cap");
    assert_eq!(
        err,
        RuntimeError::Sandbox(SandboxFailure::OutputTooLarge {
            limit: 16,
            actual: 32
        })
    );
}

#[test]
fn output_within_the_cap_is_returned_exactly() {
    let engine = sandbox::engine().expect("engine");
    let module = Module::new(&engine, OUTPUT).expect("output module compiles");
    let roomy = limits(
        DEFAULT_FUEL_PER_CALL,
        DEFAULT_MEMORY_BYTES,
        DEFAULT_WALL_CLOCK,
        32,
    );
    let mut store = new_store(&engine, &roomy, ()).expect("store");
    let instance = instantiate(&engine, &roomy, &mut store, &module).expect("instantiate");
    let bytes = output_from_export(&engine, &roomy, &mut store, instance, "out")
        .expect("32 bytes at a 32 byte cap");
    assert_eq!(bytes, b"0123456789abcdef0123456789abcdef");
}

#[test]
fn guest_trap_is_reported_as_a_sandbox_trap() {
    let engine = sandbox::engine().expect("engine");
    let limits = limits(
        DEFAULT_FUEL_PER_CALL,
        DEFAULT_MEMORY_BYTES,
        DEFAULT_WALL_CLOCK,
        DEFAULT_MAX_OUTPUT_BYTES,
    );
    let module = Module::new(&engine, BOOM).expect("boom module compiles");
    let mut store = new_store(&engine, &limits, ()).expect("store");
    let instance = instantiate(&engine, &limits, &mut store, &module).expect("instantiate");
    let err = call_typed::<(), (), ()>(&engine, &limits, &mut store, instance, "boom", ())
        .expect_err("unreachable traps");
    assert!(
        matches!(err, RuntimeError::Sandbox(SandboxFailure::Trap { .. })),
        "got {err:?}"
    );
}

const COUNT: &str = r#"
(module
  (func (export "count") (param $n i32) (result i32)
    (local $i i32)
    (block $done
      (loop $next
        (br_if $done (i32.ge_u (local.get $i) (local.get $n)))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $next)))
    (local.get $i)))
"#;

#[test]
fn fuel_budget_is_refilled_for_each_call() {
    let engine = sandbox::engine().expect("engine");
    let module = Module::new(&engine, COUNT).expect("count module compiles");
    let roomy = limits(
        1_000_000_000,
        DEFAULT_MEMORY_BYTES,
        DEFAULT_WALL_CLOCK,
        DEFAULT_MAX_OUTPUT_BYTES,
    );
    let mut store = new_store(&engine, &roomy, ()).expect("store");
    let instance = instantiate(&engine, &roomy, &mut store, &module).expect("instantiate");

    // Measure one call's cost with a generous budget, then run with a budget of twice that
    // cost. Three calls cost more than the budget in total, so a budget that was not reset
    // per call would run out on the third call.
    let counted: i32 = call_typed(
        &engine,
        &roomy,
        &mut store,
        instance,
        "count",
        (10_000_i32,),
    )
    .expect("measurement call");
    assert_eq!(counted, 10_000);
    let one_call = 1_000_000_000 - store.get_fuel().expect("fuel is enabled");
    assert!(one_call > 0, "the loop consumes fuel");

    let tight = limits(
        one_call * 2,
        DEFAULT_MEMORY_BYTES,
        DEFAULT_WALL_CLOCK,
        DEFAULT_MAX_OUTPUT_BYTES,
    );
    assert!(one_call * 3 > tight.fuel_per_call());
    for call in 0..3 {
        let result: i32 = call_typed(
            &engine,
            &tight,
            &mut store,
            instance,
            "count",
            (10_000_i32,),
        )
        .unwrap_or_else(|err| panic!("call {call} refused: {err:?}"));
        assert_eq!(result, 10_000);
    }
}

#[test]
fn memory_grown_one_page_at_a_time_stops_at_the_cap() {
    let engine = sandbox::engine().expect("engine");
    let limits = limits(
        DEFAULT_FUEL_PER_CALL,
        MIB,
        DEFAULT_WALL_CLOCK,
        DEFAULT_MAX_OUTPUT_BYTES,
    );
    let module = Module::new(&engine, GROW).expect("grow module compiles");
    let mut store = new_store(&engine, &limits, ()).expect("store");
    let instance = instantiate(&engine, &limits, &mut store, &module).expect("instantiate");

    // Starting at one page, a 1 MiB cap (16 pages) allows exactly 15 single-page growths.
    let mut granted = 0_u32;
    for _ in 0..64 {
        let old: i32 = call_typed(&engine, &limits, &mut store, instance, "grow", (1_i32,))
            .expect("single-page growth call");
        if old == -1 {
            break;
        }
        granted += 1;
    }
    assert_eq!(granted, 15);
    let memory = instance
        .get_memory(&mut store, "memory")
        .expect("memory export");
    assert_eq!(memory.data_size(&store), MIB as usize);
}

#[test]
fn a_timed_out_call_does_not_stop_later_calls_on_the_engine() {
    let engine = sandbox::engine().expect("engine");
    let spin_limits = limits(
        1_000_000_000,
        DEFAULT_MEMORY_BYTES,
        Duration::from_millis(50),
        DEFAULT_MAX_OUTPUT_BYTES,
    );
    let spin = Module::new(&engine, SPIN).expect("spin module compiles");
    let mut spin_store = new_store(&engine, &spin_limits, ()).expect("store");
    let spin_instance =
        instantiate(&engine, &spin_limits, &mut spin_store, &spin).expect("instantiate");
    let timed_out = call_typed::<(), (), ()>(
        &engine,
        &spin_limits,
        &mut spin_store,
        spin_instance,
        "spin",
        (),
    );
    assert!(matches!(
        timed_out,
        Err(RuntimeError::Sandbox(SandboxFailure::Timeout { .. }))
    ));

    // The engine's epoch moved during the timeout. A second instance on the same engine
    // must still get a fresh deadline and answer.
    let ok_limits = limits(
        DEFAULT_FUEL_PER_CALL,
        DEFAULT_MEMORY_BYTES,
        DEFAULT_WALL_CLOCK,
        DEFAULT_MAX_OUTPUT_BYTES,
    );
    let grow = Module::new(&engine, GROW).expect("grow module compiles");
    let mut ok_store = new_store(&engine, &ok_limits, ()).expect("store");
    let ok_instance = instantiate(&engine, &ok_limits, &mut ok_store, &grow).expect("instantiate");
    let pages: i32 = call_typed(&engine, &ok_limits, &mut ok_store, ok_instance, "size", ())
        .expect("a later call on the same engine is not interrupted");
    assert_eq!(pages, 1);
}

#[test]
fn negative_output_length_is_refused_before_reading_memory() {
    let engine = sandbox::engine().expect("engine");
    let limits = limits(
        DEFAULT_FUEL_PER_CALL,
        DEFAULT_MEMORY_BYTES,
        DEFAULT_WALL_CLOCK,
        DEFAULT_MAX_OUTPUT_BYTES,
    );
    let module = Module::new(&engine, OUTPUT).expect("output module compiles");
    let mut store = new_store(&engine, &limits, ()).expect("store");
    let instance = instantiate(&engine, &limits, &mut store, &module).expect("instantiate");
    let memory = instance
        .get_memory(&mut store, "memory")
        .expect("memory export");

    let err = read_output(&store, &memory, -1, &limits).expect_err("negative length");
    assert!(matches!(err, RuntimeError::Runtime(_)), "got {err:?}");
}
