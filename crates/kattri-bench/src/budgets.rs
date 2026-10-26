//! Performance budgets from `docs/spec/M5.md` (M5-17) and `docs/performance-budgets.md`.
//!
//! These are proposed values. The spec sets budgets from a first measured baseline and freezes
//! them after owner review, so a failing number is a review trigger, not a release verdict.
//! `tests/budgets.rs` says which ones it asserts on this machine and which it cannot measure.

use std::time::Duration;

/// Cold start to the first rendered connect screen. Spec M5-17, proposed.
pub const COLD_START_FIRST_FRAME: Duration = Duration::from_secs(2);

/// Idle resident memory with one library loaded, in MiB. Spec M5-17, proposed.
pub const IDLE_RESIDENT_MEMORY_MIB: u64 = 300;

/// Browse scroll frame time, 95th percentile, at 1080p (60 fps). Spec M5-17, proposed.
pub const BROWSE_FRAME_P95: Duration = Duration::from_micros(16_700);

/// Play action to first video frame, direct play. Not in the spec. Proposed in
/// `docs/performance-budgets.md` for owner decision, so the test does not assert it.
pub const PLAYBACK_FIRST_FRAME_PROPOSED: Duration = Duration::from_secs(3);

// No search latency or list paging budget exists in the spec. The search debounce interval
// (`SEARCH_DEBOUNCE`, 300 ms) is a design delay, not a latency budget, so none is defined here.
