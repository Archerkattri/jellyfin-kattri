//! M4-04: sandbox limit model. These tests check the limit arithmetic and validation.
//! They do not run guest code; the guest-run limit tests need the runtime (not in this crate).

use std::time::Duration;

use kattri_plugins::PluginError;
use kattri_plugins::sandbox::{
    DEFAULT_FUEL_PER_CALL, DEFAULT_MAX_OUTPUT_BYTES, DEFAULT_MEMORY_BYTES, DEFAULT_WALL_CLOCK,
    MemoryMeter, SandboxFailure, SandboxLimits, WALL_CLOCK_TOLERANCE,
};

fn defaults() -> SandboxLimits {
    SandboxLimits::new(
        DEFAULT_FUEL_PER_CALL,
        DEFAULT_MEMORY_BYTES,
        DEFAULT_WALL_CLOCK,
        DEFAULT_MAX_OUTPUT_BYTES,
    )
    .expect("spec defaults are valid")
}

#[test]
fn defaults_match_the_proposed_spec_values() {
    let limits = defaults();
    assert_eq!(limits.fuel_per_call(), 50_000_000);
    assert_eq!(limits.memory_bytes(), 64 * 1024 * 1024);
    assert_eq!(limits.wall_clock(), Duration::from_secs(2));
    assert_eq!(limits.max_output_bytes(), 64 * 1024);
    assert_eq!(
        limits.deadline_with_tolerance(),
        Duration::from_millis(4500)
    );
    assert_eq!(WALL_CLOCK_TOLERANCE, Duration::from_millis(2500));
}

#[test]
fn out_of_range_limits_are_rejected_with_their_field_name() {
    let cases: [(u64, u64, Duration, usize, &str); 8] = [
        (
            0,
            DEFAULT_MEMORY_BYTES,
            DEFAULT_WALL_CLOCK,
            1,
            "sandbox.fuel_per_call",
        ),
        (
            2_000_000_000,
            DEFAULT_MEMORY_BYTES,
            DEFAULT_WALL_CLOCK,
            1,
            "sandbox.fuel_per_call",
        ),
        (
            DEFAULT_FUEL_PER_CALL,
            1024,
            DEFAULT_WALL_CLOCK,
            1,
            "sandbox.memory_bytes",
        ),
        (
            DEFAULT_FUEL_PER_CALL,
            1024 * 1024 * 1024,
            DEFAULT_WALL_CLOCK,
            1,
            "sandbox.memory_bytes",
        ),
        (
            DEFAULT_FUEL_PER_CALL,
            DEFAULT_MEMORY_BYTES,
            Duration::ZERO,
            1,
            "sandbox.wall_clock",
        ),
        (
            DEFAULT_FUEL_PER_CALL,
            DEFAULT_MEMORY_BYTES,
            Duration::from_secs(31),
            1,
            "sandbox.wall_clock",
        ),
        (
            DEFAULT_FUEL_PER_CALL,
            DEFAULT_MEMORY_BYTES,
            DEFAULT_WALL_CLOCK,
            0,
            "sandbox.max_output_bytes",
        ),
        (
            DEFAULT_FUEL_PER_CALL,
            DEFAULT_MEMORY_BYTES,
            DEFAULT_WALL_CLOCK,
            2 * 1024 * 1024,
            "sandbox.max_output_bytes",
        ),
    ];
    for (fuel, memory, wall, output, expected_field) in cases {
        let err = SandboxLimits::new(fuel, memory, wall, output)
            .expect_err("out-of-range limit must be rejected");
        assert!(
            matches!(&err, PluginError::Invalid { field, .. } if *field == expected_field),
            "expected {expected_field}, got {err:?}"
        );
    }
}

#[test]
fn bounds_themselves_are_accepted() {
    assert!(SandboxLimits::new(1_000, 1024 * 1024, Duration::from_millis(10), 1).is_ok());
    assert!(
        SandboxLimits::new(
            1_000_000_000,
            512 * 1024 * 1024,
            Duration::from_secs(30),
            1024 * 1024
        )
        .is_ok()
    );
}

// CPU / fuel: the budget is spent exactly, one more unit is refused.
#[test]
fn fuel_at_budget_passes_and_one_over_is_refused() {
    let limits = defaults();
    assert_eq!(limits.check_fuel(50_000_000), Ok(()));
    assert_eq!(
        limits.check_fuel(50_000_001),
        Err(SandboxFailure::FuelExhausted { budget: 50_000_000 })
    );
}

// Memory: a single large growth and many small growths hit the same cap.
#[test]
fn memory_cap_refuses_growth_past_the_limit_in_one_step() {
    let limits = defaults();
    let mut meter = MemoryMeter::new(limits.memory_bytes());
    assert_eq!(meter.grow(limits.memory_bytes()), Ok(()));
    assert_eq!(
        meter.grow(1),
        Err(SandboxFailure::MemoryLimit {
            limit: 64 * 1024 * 1024,
            requested: 64 * 1024 * 1024 + 1,
        })
    );
    assert_eq!(
        meter.used(),
        64 * 1024 * 1024,
        "refused growth must not be counted"
    );
}

#[test]
fn memory_cap_counts_small_steps_cumulatively() {
    let mut meter = MemoryMeter::new(64 * 1024 * 1024);
    let step: u64 = 64 * 1024;
    let steps_to_cap = 64 * 1024 * 1024 / step;
    let mut refused_at = None;
    for i in 0..=steps_to_cap {
        if meter.grow(step).is_err() {
            refused_at = Some(i);
            break;
        }
    }
    assert_eq!(refused_at, Some(steps_to_cap));
}

#[test]
fn memory_growth_overflow_is_refused_not_wrapped() {
    let mut meter = MemoryMeter::new(u64::MAX - 1);
    assert_eq!(meter.grow(u64::MAX - 1), Ok(()));
    assert!(matches!(
        meter.grow(u64::MAX),
        Err(SandboxFailure::MemoryLimit { .. })
    ));
}

// Time: at the budget is on time, past it is a timeout.
#[test]
fn elapsed_time_at_budget_passes_and_past_it_times_out() {
    let limits = defaults();
    assert_eq!(limits.check_elapsed(Duration::from_secs(2)), Ok(()));
    assert_eq!(
        limits.check_elapsed(Duration::from_millis(2001)),
        Err(SandboxFailure::Timeout {
            limit: Duration::from_secs(2),
            elapsed: Duration::from_millis(2001),
        })
    );
}

// Output: exactly the cap passes, one byte more is refused.
#[test]
fn output_at_cap_passes_and_one_byte_over_is_refused() {
    let limits = defaults();
    assert_eq!(limits.check_output(64 * 1024), Ok(()));
    assert_eq!(
        limits.check_output(64 * 1024 + 1),
        Err(SandboxFailure::OutputTooLarge {
            limit: 64 * 1024,
            actual: 64 * 1024 + 1,
        })
    );
}
