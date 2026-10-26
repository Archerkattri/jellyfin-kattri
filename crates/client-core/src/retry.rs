//! Pure retry policy for network requests (M1-17): capped exponential backoff,
//! jitter supplied by the caller, and an explicit stop rule.
//!
//! Nothing here reads a clock, draws random numbers, or performs I/O, so every
//! delay is deterministic for a given jitter value. The caller decides whether
//! a failure is retryable (for example from `ErrorClass::is_retryable` in
//! jellyfin-api) so this module does not depend on the API crate.

use std::time::Duration;

/// Backoff parameters for one kind of request.
///
/// The defaults follow M1-17: the first retry waits 1 s, each retry doubles
/// the wait, the wait is capped at 30 s, jitter is plus or minus 20 percent,
/// and at most 10 automatic retries are scheduled.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RetryPolicy {
    /// Wait before the first retry, before jitter.
    pub initial: Duration,
    /// Multiplier applied for each later retry. Values below 1 act like 1.
    pub factor: u32,
    /// Upper bound on any single wait, including jitter and `Retry-After`.
    pub cap: Duration,
    /// Jitter half-width as a fraction of the wait (0.2 means plus or minus 20 percent).
    pub jitter_ratio: f64,
    /// Automatic retries scheduled after the first failure. Once this many
    /// retries have run, the policy stops until [`RetrySchedule::reset`].
    pub max_retries: u32,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            initial: Duration::from_secs(1),
            factor: 2,
            cap: Duration::from_secs(30),
            jitter_ratio: 0.2,
            max_retries: 10,
        }
    }
}

/// Why the schedule stopped retrying.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StopReason {
    /// The failure is not one that a retry can fix (for example 401 or 404).
    NotRetryable,
    /// The automatic retry budget is spent. A user action restarts it.
    RetriesExhausted,
}

/// The result of asking the schedule what to do after a failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RetryDecision {
    /// Wait this long, then retry.
    Retry { delay: Duration },
    /// Stop retrying automatically.
    Stop(StopReason),
}

impl RetryPolicy {
    /// The capped exponential wait before retry `retry_number` (1-based), with
    /// no jitter: `initial * factor^(retry_number - 1)`, at most `cap`.
    ///
    /// Retry numbers below 1 are treated as 1. Very large retry numbers
    /// saturate at `cap` and never panic.
    pub fn backoff(&self, retry_number: u32) -> Duration {
        let exponent = i32::try_from(retry_number.max(1) - 1).unwrap_or(i32::MAX);
        let cap_secs = self.cap.as_secs_f64();
        let raw = self.initial.as_secs_f64() * f64::from(self.factor.max(1)).powi(exponent);
        // `0 * inf` is NaN when the initial wait is zero; the wait is then zero.
        let secs = if raw.is_nan() { 0.0 } else { raw.min(cap_secs) };
        Duration::from_secs_f64(secs.max(0.0))
    }

    /// Apply jitter to a base wait. `jitter` is a unit value in `-1.0..=1.0`
    /// supplied by the caller; values outside that range are clamped and a
    /// NaN is treated as zero. The result is `base * (1 + jitter_ratio * jitter)`,
    /// never above `cap`.
    pub fn apply_jitter(&self, base: Duration, jitter: f64) -> Duration {
        let jitter = if jitter.is_nan() {
            0.0
        } else {
            jitter.clamp(-1.0, 1.0)
        };
        let ratio = if self.jitter_ratio.is_nan() {
            0.0
        } else {
            self.jitter_ratio.clamp(0.0, 1.0)
        };
        let scale = 1.0 + ratio * jitter;
        let secs = (base.as_secs_f64() * scale).min(self.cap.as_secs_f64());
        Duration::from_secs_f64(secs.max(0.0))
    }
}

/// Retry state for one operation. Create one per operation, call
/// [`next_decision`](Self::next_decision) after each retryable failure, and
/// call [`reset`](Self::reset) on success or on a user action.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RetrySchedule {
    policy: RetryPolicy,
    retries_made: u32,
}

impl RetrySchedule {
    /// Start a schedule with no retries made.
    pub fn new(policy: RetryPolicy) -> Self {
        Self {
            policy,
            retries_made: 0,
        }
    }

    /// Number of automatic retries scheduled since the last reset.
    pub fn retries_made(&self) -> u32 {
        self.retries_made
    }

    /// Decide what to do after a failure.
    ///
    /// * A failure that is not retryable stops at once and does not use up
    ///   the budget.
    /// * Otherwise the retry budget is checked first; once spent, the schedule
    ///   stops with [`StopReason::RetriesExhausted`].
    /// * Otherwise the delay is the jittered backoff. A `Retry-After` value from
    ///   the server raises the wait to at least that value, still bounded by
    ///   the cap, and jitter never lowers it below that value.
    ///
    /// `jitter` is a unit value in `-1.0..=1.0`, injected so tests are exact.
    pub fn next_decision(
        &mut self,
        retryable: bool,
        retry_after: Option<Duration>,
        jitter: f64,
    ) -> RetryDecision {
        if !retryable {
            return RetryDecision::Stop(StopReason::NotRetryable);
        }
        if self.retries_made >= self.policy.max_retries {
            return RetryDecision::Stop(StopReason::RetriesExhausted);
        }
        self.retries_made += 1;
        let jittered = self
            .policy
            .apply_jitter(self.policy.backoff(self.retries_made), jitter);
        let delay = match retry_after {
            Some(requested) => jittered.max(requested.min(self.policy.cap)),
            None => jittered,
        };
        RetryDecision::Retry { delay }
    }

    /// Clear the retry count. Call on success and on a user action such as
    /// "Sign in again" or pressing Retry.
    pub fn reset(&mut self) {
        self.retries_made = 0;
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{RetryDecision, RetryPolicy, RetrySchedule, StopReason};

    const EPSILON: f64 = 1e-9;

    fn secs(value: Duration) -> f64 {
        value.as_secs_f64()
    }

    fn assert_secs(actual: Duration, expected: f64) {
        assert!(
            (secs(actual) - expected).abs() < EPSILON,
            "expected {expected} s, got {} s",
            secs(actual)
        );
    }

    fn delay_of(decision: RetryDecision) -> Duration {
        match decision {
            RetryDecision::Retry { delay } => delay,
            RetryDecision::Stop(reason) => panic!("expected a retry, got stop {reason:?}"),
        }
    }

    #[test]
    fn default_policy_matches_m1_17() {
        let policy = RetryPolicy::default();
        assert_eq!(policy.initial, Duration::from_secs(1));
        assert_eq!(policy.factor, 2);
        assert_eq!(policy.cap, Duration::from_secs(30));
        assert!((policy.jitter_ratio - 0.2).abs() < EPSILON);
        assert_eq!(policy.max_retries, 10);
    }

    #[test]
    fn backoff_sequence_without_jitter_is_exact_and_capped() {
        let policy = RetryPolicy::default();
        let expected = [1.0, 2.0, 4.0, 8.0, 16.0, 30.0, 30.0, 30.0];
        for (index, expected) in expected.iter().enumerate() {
            let retry_number = u32::try_from(index + 1).expect("small index");
            assert_secs(policy.backoff(retry_number), *expected);
        }
    }

    #[test]
    fn backoff_saturates_at_cap_for_huge_retry_numbers_without_panicking() {
        let policy = RetryPolicy::default();
        assert_secs(policy.backoff(1_000), 30.0);
        assert_secs(policy.backoff(u32::MAX), 30.0);
    }

    #[test]
    fn retry_number_zero_is_treated_as_first_retry() {
        let policy = RetryPolicy::default();
        assert_secs(policy.backoff(0), 1.0);
    }

    #[test]
    fn zero_initial_wait_stays_zero_even_with_large_exponents() {
        let policy = RetryPolicy {
            initial: Duration::ZERO,
            ..RetryPolicy::default()
        };
        assert_secs(policy.backoff(u32::MAX), 0.0);
        assert_secs(policy.apply_jitter(policy.backoff(3), 1.0), 0.0);
    }

    #[test]
    fn factor_one_keeps_the_initial_wait() {
        let policy = RetryPolicy {
            factor: 1,
            ..RetryPolicy::default()
        };
        for retry_number in 1..=12 {
            assert_secs(policy.backoff(retry_number), 1.0);
        }
    }

    #[test]
    fn zero_jitter_returns_the_base_unchanged() {
        let policy = RetryPolicy::default();
        assert_secs(policy.apply_jitter(Duration::from_secs(8), 0.0), 8.0);
        assert_secs(policy.apply_jitter(Duration::from_secs(8), f64::NAN), 8.0);
    }

    #[test]
    fn jitter_stays_within_plus_or_minus_twenty_percent() {
        let policy = RetryPolicy::default();
        for retry_number in 1..=10 {
            let base = policy.backoff(retry_number);
            let base_secs = secs(base);
            let low = policy.apply_jitter(base, -1.0);
            let high = policy.apply_jitter(base, 1.0);
            assert_secs(low, base_secs * 0.8);
            let expected_high = (base_secs * 1.2).min(30.0);
            assert_secs(high, expected_high);
            for step in 0..=20 {
                let jitter = -1.0 + f64::from(step) * 0.1;
                let delay = secs(policy.apply_jitter(base, jitter));
                assert!(
                    delay >= base_secs * 0.8 - EPSILON && delay <= expected_high + EPSILON,
                    "retry {retry_number}, jitter {jitter}: {delay} outside bounds"
                );
            }
        }
    }

    #[test]
    fn jitter_never_exceeds_the_cap() {
        let policy = RetryPolicy::default();
        assert_secs(policy.apply_jitter(Duration::from_secs(30), 1.0), 30.0);
        assert_secs(policy.apply_jitter(policy.backoff(6), 1.0), 30.0);
    }

    #[test]
    fn out_of_range_jitter_is_clamped() {
        let policy = RetryPolicy::default();
        let base = Duration::from_secs(4);
        assert_eq!(
            policy.apply_jitter(base, 7.5),
            policy.apply_jitter(base, 1.0)
        );
        assert_eq!(
            policy.apply_jitter(base, -7.5),
            policy.apply_jitter(base, -1.0)
        );
    }

    #[test]
    fn schedule_delays_follow_backoff_with_injected_jitter() {
        let mut schedule = RetrySchedule::new(RetryPolicy::default());
        let first = delay_of(schedule.next_decision(true, None, 0.0));
        let second = delay_of(schedule.next_decision(true, None, -1.0));
        let third = delay_of(schedule.next_decision(true, None, 1.0));
        assert_secs(first, 1.0);
        assert_secs(second, 2.0 * 0.8);
        assert_secs(third, 4.0 * 1.2);
    }

    #[test]
    fn retry_after_is_respected_up_to_the_cap() {
        let mut schedule = RetrySchedule::new(RetryPolicy::default());
        let delay = delay_of(schedule.next_decision(true, Some(Duration::from_secs(10)), 0.0));
        assert_secs(delay, 10.0);

        let mut schedule = RetrySchedule::new(RetryPolicy::default());
        let delay = delay_of(schedule.next_decision(true, Some(Duration::from_secs(90)), 0.0));
        assert_secs(delay, 30.0);
    }

    #[test]
    fn jitter_never_lowers_a_retry_after_wait() {
        let mut schedule = RetrySchedule::new(RetryPolicy::default());
        let delay = delay_of(schedule.next_decision(true, Some(Duration::from_secs(10)), -1.0));
        assert_secs(delay, 10.0);
    }

    #[test]
    fn short_retry_after_does_not_shorten_the_backoff() {
        let mut schedule = RetrySchedule::new(RetryPolicy::default());
        for _ in 0..4 {
            schedule.next_decision(true, None, 0.0);
        }
        // Retry 5 backs off to 16 s; a 2 s Retry-After must not reduce it.
        let delay = delay_of(schedule.next_decision(true, Some(Duration::from_secs(2)), 0.0));
        assert_secs(delay, 16.0);
    }

    #[test]
    fn stops_after_ten_scheduled_retries() {
        let mut schedule = RetrySchedule::new(RetryPolicy::default());
        for _ in 0..10 {
            assert!(matches!(
                schedule.next_decision(true, None, 0.0),
                RetryDecision::Retry { .. }
            ));
        }
        assert_eq!(schedule.retries_made(), 10);
        assert_eq!(
            schedule.next_decision(true, None, 0.0),
            RetryDecision::Stop(StopReason::RetriesExhausted)
        );
        assert_eq!(schedule.retries_made(), 10);
    }

    #[test]
    fn non_retryable_failure_stops_without_using_budget() {
        let mut schedule = RetrySchedule::new(RetryPolicy::default());
        assert_eq!(
            schedule.next_decision(false, None, 0.0),
            RetryDecision::Stop(StopReason::NotRetryable)
        );
        assert_eq!(schedule.retries_made(), 0);
    }

    #[test]
    fn reset_on_success_or_user_action_restarts_the_budget() {
        let mut schedule = RetrySchedule::new(RetryPolicy::default());
        for _ in 0..10 {
            schedule.next_decision(true, None, 0.0);
        }
        assert_eq!(
            schedule.next_decision(true, None, 0.0),
            RetryDecision::Stop(StopReason::RetriesExhausted)
        );
        schedule.reset();
        assert_eq!(schedule.retries_made(), 0);
        assert_secs(delay_of(schedule.next_decision(true, None, 0.0)), 1.0);
    }

    #[test]
    fn zero_retry_budget_stops_on_first_retryable_failure() {
        let policy = RetryPolicy {
            max_retries: 0,
            ..RetryPolicy::default()
        };
        let mut schedule = RetrySchedule::new(policy);
        assert_eq!(
            schedule.next_decision(true, None, 0.0),
            RetryDecision::Stop(StopReason::RetriesExhausted)
        );
    }
}
