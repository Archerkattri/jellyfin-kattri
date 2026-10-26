//! Sandbox enforcement for guest code.
//!
//! Limits applied to every call:
//! - fuel: the store is armed with [`SandboxLimits::fuel_per_call`] before each call;
//! - memory: a [`StoreLimits`] caps linear memory growth at [`SandboxLimits::memory_bytes`];
//! - wall clock: [`run_bounded`] starts a watchdog that ticks the engine epoch once the
//!   budget elapses, and the store traps on the next epoch check;
//! - output: [`read_output`] refuses guest output longer than
//!   [`SandboxLimits::max_output_bytes`] before copying it out of guest memory.
//!
//! Each engine has one watchdog per call, so calls on one engine must not overlap. The host
//! makes one engine per plugin and runs its calls one at a time.

use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use kattri_plugins::sandbox::{SandboxFailure, SandboxLimits};
use wasmtime::{
    Config, Engine, Instance, Memory, Module, Store, StoreLimits, StoreLimitsBuilder, Trap,
    WasmParams, WasmResults,
};

use crate::error::RuntimeError;

/// Epoch ticks a call may run past its start. The watchdog fires exactly one tick.
const EPOCH_DEADLINE_TICKS: u64 = 1;

/// Store data: the resource limiter plus the host state the imports read and write.
pub struct GuestData<T> {
    limits: StoreLimits,
    /// Host state visible to imports.
    pub state: T,
}

/// Build the engine for guest code. Fuel metering and epoch interruption are enabled.
pub fn engine() -> Result<Engine, RuntimeError> {
    let mut config = Config::new();
    config.wasm_component_model(true);
    config.consume_fuel(true);
    config.epoch_interruption(true);
    Engine::new(&config).map_err(runtime_error)
}

/// Create a store whose linear memory is capped by `limits`.
pub fn new_store<T>(
    engine: &Engine,
    limits: &SandboxLimits,
    state: T,
) -> Result<Store<GuestData<T>>, RuntimeError> {
    let memory_cap = usize::try_from(limits.memory_bytes()).unwrap_or(usize::MAX);
    let store_limits = StoreLimitsBuilder::new().memory_size(memory_cap).build();
    let mut store = Store::new(
        engine,
        GuestData {
            limits: store_limits,
            state,
        },
    );
    store.limiter(|data| &mut data.limits);
    Ok(store)
}

/// Refill the fuel budget and arm the epoch deadline for the next call.
pub fn arm<T>(store: &mut Store<GuestData<T>>, limits: &SandboxLimits) -> Result<(), RuntimeError> {
    store
        .set_fuel(limits.fuel_per_call())
        .map_err(runtime_error)?;
    store.set_epoch_deadline(EPOCH_DEADLINE_TICKS);
    Ok(())
}

/// Instantiate a module with no imports, under the call limits.
pub fn instantiate<T>(
    engine: &Engine,
    limits: &SandboxLimits,
    store: &mut Store<GuestData<T>>,
    module: &Module,
) -> Result<Instance, RuntimeError> {
    arm(store, limits)?;
    run_bounded(engine, limits, || Instance::new(&mut *store, module, &[]))
}

/// Call an exported function with typed parameters and results, under the call limits.
pub fn call_typed<T, P, R>(
    engine: &Engine,
    limits: &SandboxLimits,
    store: &mut Store<GuestData<T>>,
    instance: Instance,
    export: &str,
    params: P,
) -> Result<R, RuntimeError>
where
    P: WasmParams,
    R: WasmResults,
{
    let func = instance
        .get_typed_func::<P, R>(&mut *store, export)
        .map_err(runtime_error)?;
    arm(store, limits)?;
    run_bounded(engine, limits, || func.call(&mut *store, params))
}

/// Call an exported `() -> i32` that returns the length of the output in the guest's
/// `memory` export, starting at offset 0, and return that output after the size check.
pub fn output_from_export<T>(
    engine: &Engine,
    limits: &SandboxLimits,
    store: &mut Store<GuestData<T>>,
    instance: Instance,
    export: &str,
) -> Result<Vec<u8>, RuntimeError> {
    let len: i32 = call_typed(engine, limits, store, instance, export, ())?;
    let memory = instance
        .get_memory(&mut *store, "memory")
        .ok_or_else(|| RuntimeError::Runtime("guest exports no memory".to_owned()))?;
    read_output(store, &memory, len, limits)
}

/// Run `call` with the wall-clock watchdog, then classify any error it returns.
///
/// The watchdog waits on a channel for `limits.wall_clock()`. If the call has not finished by
/// then, the watchdog ticks the engine epoch, and the guest traps at its next epoch check.
pub fn run_bounded<R>(
    engine: &Engine,
    limits: &SandboxLimits,
    call: impl FnOnce() -> wasmtime::Result<R>,
) -> Result<R, RuntimeError> {
    let (done_tx, done_rx) = mpsc::channel::<()>();
    let watchdog_engine = engine.clone();
    let budget = limits.wall_clock();
    let watchdog = thread::spawn(move || {
        if let Err(RecvTimeoutError::Timeout) = done_rx.recv_timeout(budget) {
            watchdog_engine.increment_epoch();
        }
    });

    let started = Instant::now();
    let outcome = call();
    let elapsed = started.elapsed();
    // A closed channel is the signal that the call finished. Both send errors and join
    // errors are ignored: the watchdog holds no state that needs recovery.
    let _ = done_tx.send(());
    let _ = watchdog.join();

    outcome.map_err(|err| classify(err, limits, elapsed))
}

/// Copy `len` bytes of guest output out of `memory`, refusing output over the cap.
pub fn read_output<T>(
    store: &Store<GuestData<T>>,
    memory: &Memory,
    len: i32,
    limits: &SandboxLimits,
) -> Result<Vec<u8>, RuntimeError> {
    let len = usize::try_from(len)
        .map_err(|_| RuntimeError::Runtime("guest returned a negative output length".to_owned()))?;
    limits.check_output(len).map_err(RuntimeError::Sandbox)?;
    memory
        .data(store)
        .get(..len)
        .map(<[u8]>::to_vec)
        .ok_or_else(|| RuntimeError::Runtime("output range is outside guest memory".to_owned()))
}

/// Map a wasmtime error to a sandbox failure when it is a known trap, and to a runtime error
/// otherwise.
pub fn classify(err: wasmtime::Error, limits: &SandboxLimits, elapsed: Duration) -> RuntimeError {
    match err.downcast_ref::<Trap>() {
        Some(Trap::OutOfFuel) => RuntimeError::Sandbox(SandboxFailure::FuelExhausted {
            budget: limits.fuel_per_call(),
        }),
        Some(Trap::Interrupt) => RuntimeError::Sandbox(SandboxFailure::Timeout {
            limit: limits.wall_clock(),
            elapsed,
        }),
        Some(trap) => RuntimeError::Sandbox(SandboxFailure::Trap {
            detail: trap.to_string(),
        }),
        None => RuntimeError::Runtime(err.to_string()),
    }
}

fn runtime_error(err: wasmtime::Error) -> RuntimeError {
    RuntimeError::Runtime(err.to_string())
}
