//! M4-04 sandbox resource limits: the configuration, its validation, and the checks a
//! runtime applies to fuel, memory, wall-clock time and returned output.
//!
//! This crate defines the limit model and validates caller-provided settings. Runtime
//! enforcement is implemented in `kattri-plugin-host`, where Wasmtime arms per-call fuel,
//! attaches store memory limits, interrupts overdue calls, and bounds copied output.
//!
//! Every refusal is a [`SandboxFailure`]. The lifecycle (M4-05) treats each one as a
//! failure of the plugin, so a plugin that trips a limit is quarantined.

use std::time::Duration;

use crate::error::PluginError;

/// Fuel units granted to one host-to-plugin call (initial budget; sample calibration pending).
pub const DEFAULT_FUEL_PER_CALL: u64 = 50_000_000;
/// Linear memory cap per plugin store (proposed).
pub const DEFAULT_MEMORY_BYTES: u64 = 64 * 1024 * 1024;
/// Host-side wall-clock budget for one call (proposed).
pub const DEFAULT_WALL_CLOCK: Duration = Duration::from_secs(2);
/// Largest value a plugin may return to the host, in bytes (proposed).
pub const DEFAULT_MAX_OUTPUT_BYTES: usize = 64 * 1024;
/// Extra time a test may allow after the budget before calling a return late.
pub const WALL_CLOCK_TOLERANCE: Duration = Duration::from_millis(2500);

const MIN_FUEL_PER_CALL: u64 = 1_000;
const MAX_FUEL_PER_CALL: u64 = 1_000_000_000;
const MIN_MEMORY_BYTES: u64 = 1024 * 1024;
const MAX_MEMORY_BYTES: u64 = 512 * 1024 * 1024;
const MIN_WALL_CLOCK: Duration = Duration::from_millis(10);
const MAX_WALL_CLOCK: Duration = Duration::from_secs(30);
const MIN_MAX_OUTPUT_BYTES: usize = 1;
const MAX_MAX_OUTPUT_BYTES: usize = 1024 * 1024;

/// Resource limits applied to each plugin call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SandboxLimits {
    fuel_per_call: u64,
    memory_bytes: u64,
    wall_clock: Duration,
    max_output_bytes: usize,
}

/// Why a plugin call was refused or stopped. Every variant marks the plugin failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxFailure {
    /// The call used more fuel than its budget.
    FuelExhausted { budget: u64 },
    /// Linear memory would exceed the store cap.
    MemoryLimit { limit: u64, requested: u64 },
    /// The call ran longer than its wall-clock budget.
    Timeout { limit: Duration, elapsed: Duration },
    /// The plugin returned more bytes than allowed.
    OutputTooLarge { limit: usize, actual: usize },
    /// The guest trapped. The message is the runtime's diagnostic text.
    Trap { detail: String },
}

impl SandboxLimits {
    /// Build limits from explicit values, rejecting anything outside the allowed range.
    pub fn new(
        fuel_per_call: u64,
        memory_bytes: u64,
        wall_clock: Duration,
        max_output_bytes: usize,
    ) -> Result<Self, PluginError> {
        let limits = Self {
            fuel_per_call,
            memory_bytes,
            wall_clock,
            max_output_bytes,
        };
        limits.validate()?;
        Ok(limits)
    }

    /// Re-check the invariants. Used after construction and by any loader that
    /// receives limits from outside the crate.
    pub fn validate(&self) -> Result<(), PluginError> {
        check_range(
            "sandbox.fuel_per_call",
            self.fuel_per_call,
            MIN_FUEL_PER_CALL,
            MAX_FUEL_PER_CALL,
        )?;
        check_range(
            "sandbox.memory_bytes",
            self.memory_bytes,
            MIN_MEMORY_BYTES,
            MAX_MEMORY_BYTES,
        )?;
        if self.wall_clock < MIN_WALL_CLOCK || self.wall_clock > MAX_WALL_CLOCK {
            return Err(PluginError::Invalid {
                field: "sandbox.wall_clock",
                reason: "must be between 10 ms and 30 s".to_owned(),
            });
        }
        let output = u64::try_from(self.max_output_bytes).unwrap_or(u64::MAX);
        check_range(
            "sandbox.max_output_bytes",
            output,
            MIN_MAX_OUTPUT_BYTES as u64,
            MAX_MAX_OUTPUT_BYTES as u64,
        )?;
        Ok(())
    }

    pub const fn fuel_per_call(&self) -> u64 {
        self.fuel_per_call
    }

    pub const fn memory_bytes(&self) -> u64 {
        self.memory_bytes
    }

    pub const fn wall_clock(&self) -> Duration {
        self.wall_clock
    }

    pub const fn max_output_bytes(&self) -> usize {
        self.max_output_bytes
    }

    /// Latest a call may return and still count as on time, including tolerance.
    pub fn deadline_with_tolerance(&self) -> Duration {
        self.wall_clock.saturating_add(WALL_CLOCK_TOLERANCE)
    }

    /// Refuse a call that consumed more fuel than its budget.
    pub fn check_fuel(&self, consumed: u64) -> Result<(), SandboxFailure> {
        if consumed > self.fuel_per_call {
            return Err(SandboxFailure::FuelExhausted {
                budget: self.fuel_per_call,
            });
        }
        Ok(())
    }

    /// Refuse a call whose elapsed wall-clock time is past the budget.
    pub fn check_elapsed(&self, elapsed: Duration) -> Result<(), SandboxFailure> {
        if elapsed > self.wall_clock {
            return Err(SandboxFailure::Timeout {
                limit: self.wall_clock,
                elapsed,
            });
        }
        Ok(())
    }

    /// Refuse returned output larger than the cap.
    pub fn check_output(&self, actual: usize) -> Result<(), SandboxFailure> {
        if actual > self.max_output_bytes {
            return Err(SandboxFailure::OutputTooLarge {
                limit: self.max_output_bytes,
                actual,
            });
        }
        Ok(())
    }
}

/// Tracks the total linear memory a store has grown to. Growth is cumulative, so a
/// plugin that grows in many small steps is stopped at the same cap as one that grows
/// in one step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryMeter {
    limit: u64,
    used: u64,
}

impl MemoryMeter {
    pub const fn new(limit: u64) -> Self {
        Self { limit, used: 0 }
    }

    pub const fn used(&self) -> u64 {
        self.used
    }

    /// Account for `bytes` more linear memory. A refused growth leaves `used` unchanged.
    pub fn grow(&mut self, bytes: u64) -> Result<(), SandboxFailure> {
        let requested = self.used.saturating_add(bytes);
        if requested > self.limit {
            return Err(SandboxFailure::MemoryLimit {
                limit: self.limit,
                requested,
            });
        }
        self.used = requested;
        Ok(())
    }
}

fn check_range(field: &'static str, value: u64, min: u64, max: u64) -> Result<(), PluginError> {
    if value < min || value > max {
        return Err(PluginError::Invalid {
            field,
            reason: format!("must be between {min} and {max}"),
        });
    }
    Ok(())
}
