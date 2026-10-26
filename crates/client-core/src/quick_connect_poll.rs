//! Quick Connect polling state machine (M1-16). Pure: the caller supplies the
//! current time as a monotonic `Duration`, the results of the initiate and poll
//! requests, and cancel requests. The caller performs the HTTP calls and the
//! sleeps; this module only decides what the next step is.
//!
//! Times are measured from any fixed origin the caller chooses, such as the
//! moment the app started. Only differences between values matter.

use std::time::Duration;

/// Wait between a poll response and the next poll, and before the first poll.
pub const POLL_INTERVAL: Duration = Duration::from_secs(2);
/// The whole flow ends as expired this long after initiate succeeded.
pub const TOTAL_TIMEOUT: Duration = Duration::from_secs(5 * 60);
/// Consecutive poll errors (not 404, not pending) that fail the flow. The
/// count resets on any successful pending response.
pub const MAX_CONSECUTIVE_POLL_ERRORS: u32 = 3;

/// Where the Quick Connect flow stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum QuickConnectState {
    /// Nothing started.
    Idle,
    /// The server issued a code, which the user must approve. No poll yet.
    Initiated { code: String, started_at: Duration },
    /// Polling. `in_flight` is true while a poll request is outstanding.
    Polling {
        code: String,
        started_at: Duration,
        next_poll_at: Duration,
        in_flight: bool,
        consecutive_errors: u32,
    },
    /// The user approved. The caller now calls AuthenticateWithQuickConnect.
    Authorized,
    /// The code expired, by timeout or by a 404 on poll. Show "Quick Connect
    /// code expired. Start again."
    Expired,
    /// The user cancelled.
    Cancelled,
    /// The flow failed for a reason other than expiry.
    Failed(FailureReason),
}

impl QuickConnectState {
    /// True once no further polls or actions can happen.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Authorized | Self::Expired | Self::Cancelled | Self::Failed(_)
        )
    }
}

/// Why a flow failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailureReason {
    /// The initiate request failed or returned no usable code.
    InitiateFailed,
    /// Poll requests failed [`MAX_CONSECUTIVE_POLL_ERRORS`] times in a row.
    PollErrors,
}

/// Result of one poll request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PollOutcome {
    /// Not approved yet. Keep polling.
    Pending,
    /// Approved. The caller authenticates with the secret.
    Authorized,
    /// The server returned 404. The code is expired.
    NotFound,
    /// Any other failure: transport error, timeout, or unexpected status.
    Error,
}

/// Input to the state machine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// The initiate request succeeded with this short code.
    Initiated { now: Duration, code: String },
    /// The initiate request failed.
    InitiateFailed,
    /// Time has advanced. Asks whether a poll is due or the flow has timed out.
    Tick { now: Duration },
    /// The poll requested by [`Action::Poll`] completed.
    PollResult { now: Duration, outcome: PollOutcome },
    /// The user cancelled.
    Cancel,
}

/// What the caller should do next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    None,
    /// Send one poll request now (with the secret the caller holds), then
    /// report the outcome with [`Event::PollResult`].
    Poll,
    /// Call AuthenticateWithQuickConnect now.
    Authenticate,
}

/// The polling machine. Start it with [`QuickConnectPoll::new`], then feed it
/// [`Event`]s.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuickConnectPoll {
    state: QuickConnectState,
}

impl Default for QuickConnectPoll {
    fn default() -> Self {
        Self::new()
    }
}

impl QuickConnectPoll {
    pub fn new() -> Self {
        Self {
            state: QuickConnectState::Idle,
        }
    }

    pub fn state(&self) -> &QuickConnectState {
        &self.state
    }

    /// Applies one event and returns the action for the caller. Events that do
    /// not apply to the current state are ignored and return [`Action::None`].
    pub fn handle(&mut self, event: Event) -> Action {
        let current = std::mem::replace(&mut self.state, QuickConnectState::Idle);
        let (next, action) = step(current, event);
        self.state = next;
        action
    }
}

fn elapsed(now: Duration, since: Duration) -> Duration {
    now.saturating_sub(since)
}

fn step(state: QuickConnectState, event: Event) -> (QuickConnectState, Action) {
    use QuickConnectState as S;
    match event {
        Event::Cancel => {
            if state.is_terminal() {
                (state, Action::None)
            } else {
                (S::Cancelled, Action::None)
            }
        }
        Event::InitiateFailed => match state {
            S::Idle => (S::Failed(FailureReason::InitiateFailed), Action::None),
            other => (other, Action::None),
        },
        Event::Initiated { now, code } => match state {
            S::Idle if code.trim().is_empty() => {
                (S::Failed(FailureReason::InitiateFailed), Action::None)
            }
            S::Idle => (
                S::Initiated {
                    code,
                    started_at: now,
                },
                Action::None,
            ),
            other => (other, Action::None),
        },
        Event::Tick { now } => tick(state, now),
        Event::PollResult { now, outcome } => poll_result(state, now, outcome),
    }
}

fn tick(state: QuickConnectState, now: Duration) -> (QuickConnectState, Action) {
    use QuickConnectState as S;
    match state {
        S::Initiated { code, started_at } => {
            let age = elapsed(now, started_at);
            if age >= TOTAL_TIMEOUT {
                (S::Expired, Action::None)
            } else if age >= POLL_INTERVAL {
                (
                    S::Polling {
                        code,
                        started_at,
                        next_poll_at: now,
                        in_flight: true,
                        consecutive_errors: 0,
                    },
                    Action::Poll,
                )
            } else {
                (S::Initiated { code, started_at }, Action::None)
            }
        }
        S::Polling {
            code,
            started_at,
            next_poll_at,
            in_flight,
            consecutive_errors,
        } => {
            if elapsed(now, started_at) >= TOTAL_TIMEOUT {
                (S::Expired, Action::None)
            } else if !in_flight && now >= next_poll_at {
                (
                    S::Polling {
                        code,
                        started_at,
                        next_poll_at,
                        in_flight: true,
                        consecutive_errors,
                    },
                    Action::Poll,
                )
            } else {
                (
                    S::Polling {
                        code,
                        started_at,
                        next_poll_at,
                        in_flight,
                        consecutive_errors,
                    },
                    Action::None,
                )
            }
        }
        other => (other, Action::None),
    }
}

fn poll_result(
    state: QuickConnectState,
    now: Duration,
    outcome: PollOutcome,
) -> (QuickConnectState, Action) {
    use QuickConnectState as S;
    match state {
        S::Polling {
            code,
            started_at,
            in_flight: true,
            consecutive_errors,
            ..
        } => {
            // A response that arrives after the total timeout does not revive the flow.
            if elapsed(now, started_at) >= TOTAL_TIMEOUT {
                return (S::Expired, Action::None);
            }
            let next_poll_at = now.saturating_add(POLL_INTERVAL);
            match outcome {
                PollOutcome::Pending => (
                    S::Polling {
                        code,
                        started_at,
                        next_poll_at,
                        in_flight: false,
                        consecutive_errors: 0,
                    },
                    Action::None,
                ),
                PollOutcome::Authorized => (S::Authorized, Action::Authenticate),
                PollOutcome::NotFound => (S::Expired, Action::None),
                PollOutcome::Error => {
                    let errors = consecutive_errors.saturating_add(1);
                    if errors >= MAX_CONSECUTIVE_POLL_ERRORS {
                        (S::Failed(FailureReason::PollErrors), Action::None)
                    } else {
                        (
                            S::Polling {
                                code,
                                started_at,
                                next_poll_at,
                                in_flight: false,
                                consecutive_errors: errors,
                            },
                            Action::None,
                        )
                    }
                }
            }
        }
        // No poll is outstanding: a stale or late response after cancel,
        // expiry, or a repeated result. Ignore it.
        other => (other, Action::None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(value: u64) -> Duration {
        Duration::from_secs(value)
    }

    /// Machine with an initiate at t=0 and code "123456".
    fn initiated() -> QuickConnectPoll {
        let mut machine = QuickConnectPoll::new();
        let action = machine.handle(Event::Initiated {
            now: secs(0),
            code: "123456".to_owned(),
        });
        assert_eq!(action, Action::None);
        machine
    }

    /// Machine that has sent its first poll at t=2s.
    fn polling_in_flight() -> QuickConnectPoll {
        let mut machine = initiated();
        assert_eq!(machine.handle(Event::Tick { now: secs(2) }), Action::Poll);
        machine
    }

    #[test]
    fn initiate_moves_idle_to_initiated_with_code() {
        let machine = initiated();
        assert_eq!(
            machine.state(),
            &QuickConnectState::Initiated {
                code: "123456".to_owned(),
                started_at: secs(0),
            }
        );
    }

    #[test]
    fn no_poll_before_the_two_second_interval() {
        let mut machine = initiated();
        let almost = Duration::from_millis(1999);
        assert_eq!(machine.handle(Event::Tick { now: almost }), Action::None);
        assert!(matches!(
            machine.state(),
            QuickConnectState::Initiated { .. }
        ));
        assert_eq!(machine.handle(Event::Tick { now: secs(2) }), Action::Poll);
        assert!(matches!(
            machine.state(),
            QuickConnectState::Polling {
                in_flight: true,
                consecutive_errors: 0,
                ..
            }
        ));
    }

    #[test]
    fn a_poll_is_never_sent_twice_while_one_is_in_flight() {
        let mut machine = polling_in_flight();
        assert_eq!(machine.handle(Event::Tick { now: secs(10) }), Action::None);
        assert_eq!(machine.handle(Event::Tick { now: secs(100) }), Action::None);
    }

    #[test]
    fn pending_schedules_next_poll_two_seconds_later() {
        let mut machine = polling_in_flight();
        assert_eq!(
            machine.handle(Event::PollResult {
                now: secs(3),
                outcome: PollOutcome::Pending,
            }),
            Action::None
        );
        assert_eq!(machine.handle(Event::Tick { now: secs(4) }), Action::None);
        assert_eq!(machine.handle(Event::Tick { now: secs(5) }), Action::Poll);
    }

    #[test]
    fn three_pending_responses_then_authorized_completes() {
        let mut machine = polling_in_flight();
        let mut now = 2;
        for _ in 0..3 {
            machine.handle(Event::PollResult {
                now: secs(now),
                outcome: PollOutcome::Pending,
            });
            now += 2;
            assert_eq!(machine.handle(Event::Tick { now: secs(now) }), Action::Poll);
        }
        let action = machine.handle(Event::PollResult {
            now: secs(now),
            outcome: PollOutcome::Authorized,
        });
        assert_eq!(action, Action::Authenticate);
        assert_eq!(machine.state(), &QuickConnectState::Authorized);
        assert!(machine.state().is_terminal());
    }

    #[test]
    fn authorized_is_final_and_ignores_later_events() {
        let mut machine = polling_in_flight();
        machine.handle(Event::PollResult {
            now: secs(2),
            outcome: PollOutcome::Authorized,
        });
        assert_eq!(machine.handle(Event::Cancel), Action::None);
        assert_eq!(machine.state(), &QuickConnectState::Authorized);
        assert_eq!(machine.handle(Event::Tick { now: secs(50) }), Action::None);
    }

    #[test]
    fn not_found_on_poll_means_expired() {
        let mut machine = polling_in_flight();
        let action = machine.handle(Event::PollResult {
            now: secs(4),
            outcome: PollOutcome::NotFound,
        });
        assert_eq!(action, Action::None);
        assert_eq!(machine.state(), &QuickConnectState::Expired);
    }

    /// Consecutive error count while polling, or None in any other state.
    fn errors_of(machine: &QuickConnectPoll) -> Option<u32> {
        match machine.state() {
            QuickConnectState::Polling {
                consecutive_errors, ..
            } => Some(*consecutive_errors),
            _ => None,
        }
    }

    #[test]
    fn errors_below_the_limit_keep_polling_and_success_resets_the_count() {
        let mut machine = polling_in_flight();
        machine.handle(Event::PollResult {
            now: secs(2),
            outcome: PollOutcome::Error,
        });
        assert_eq!(errors_of(&machine), Some(1));

        machine.handle(Event::Tick { now: secs(4) });
        machine.handle(Event::PollResult {
            now: secs(4),
            outcome: PollOutcome::Error,
        });
        assert_eq!(errors_of(&machine), Some(2));

        // A successful (pending) response resets the consecutive count.
        machine.handle(Event::Tick { now: secs(6) });
        machine.handle(Event::PollResult {
            now: secs(6),
            outcome: PollOutcome::Pending,
        });
        assert_eq!(errors_of(&machine), Some(0));
    }

    #[test]
    fn consecutive_errors_fail_after_the_limit() {
        let mut machine = polling_in_flight();
        let mut now = 2;
        for attempt in 1..=MAX_CONSECUTIVE_POLL_ERRORS {
            machine.handle(Event::PollResult {
                now: secs(now),
                outcome: PollOutcome::Error,
            });
            if attempt < MAX_CONSECUTIVE_POLL_ERRORS {
                now += 2;
                assert_eq!(machine.handle(Event::Tick { now: secs(now) }), Action::Poll);
            }
        }
        assert_eq!(
            machine.state(),
            &QuickConnectState::Failed(FailureReason::PollErrors)
        );
    }

    #[test]
    fn timeout_expires_before_a_poll_is_due() {
        let mut machine = initiated();
        assert_eq!(
            machine.handle(Event::Tick { now: TOTAL_TIMEOUT }),
            Action::None
        );
        assert_eq!(machine.state(), &QuickConnectState::Expired);
    }

    #[test]
    fn timeout_applies_while_polling_and_stops_the_poll_schedule() {
        let mut machine = initiated();
        machine.handle(Event::Tick { now: secs(2) });
        // Just under the limit: the poll is still in flight and nothing changes.
        let almost_end = TOTAL_TIMEOUT - Duration::from_millis(1);
        assert_eq!(
            machine.handle(Event::Tick { now: almost_end }),
            Action::None
        );
        assert!(matches!(machine.state(), QuickConnectState::Polling { .. }));
        assert_eq!(
            machine.handle(Event::Tick { now: TOTAL_TIMEOUT }),
            Action::None
        );
        assert_eq!(machine.state(), &QuickConnectState::Expired);
        // No further poll after expiry.
        assert_eq!(
            machine.handle(Event::Tick {
                now: TOTAL_TIMEOUT + secs(10)
            }),
            Action::None
        );
    }

    #[test]
    fn late_success_after_timeout_does_not_authenticate() {
        let mut machine = polling_in_flight();
        let action = machine.handle(Event::PollResult {
            now: TOTAL_TIMEOUT + secs(1),
            outcome: PollOutcome::Authorized,
        });
        assert_eq!(action, Action::None);
        assert_eq!(machine.state(), &QuickConnectState::Expired);
    }

    #[test]
    fn late_error_after_timeout_expires_rather_than_counting() {
        let mut machine = polling_in_flight();
        machine.handle(Event::PollResult {
            now: TOTAL_TIMEOUT,
            outcome: PollOutcome::Error,
        });
        assert_eq!(machine.state(), &QuickConnectState::Expired);
    }

    #[test]
    fn cancel_from_idle_initiated_and_polling_is_cancelled() {
        let mut idle = QuickConnectPoll::new();
        assert_eq!(idle.handle(Event::Cancel), Action::None);
        assert_eq!(idle.state(), &QuickConnectState::Cancelled);

        let mut initiated = initiated();
        assert_eq!(initiated.handle(Event::Cancel), Action::None);
        assert_eq!(initiated.state(), &QuickConnectState::Cancelled);

        let mut polling = polling_in_flight();
        assert_eq!(polling.handle(Event::Cancel), Action::None);
        assert_eq!(polling.state(), &QuickConnectState::Cancelled);
    }

    #[test]
    fn late_poll_result_after_cancel_is_ignored() {
        let mut machine = polling_in_flight();
        machine.handle(Event::Cancel);
        let action = machine.handle(Event::PollResult {
            now: secs(3),
            outcome: PollOutcome::Authorized,
        });
        assert_eq!(action, Action::None);
        assert_eq!(machine.state(), &QuickConnectState::Cancelled);
        assert_eq!(machine.handle(Event::Tick { now: secs(10) }), Action::None);
    }

    #[test]
    fn cancel_keeps_terminal_outcomes() {
        let mut expired = polling_in_flight();
        expired.handle(Event::PollResult {
            now: secs(2),
            outcome: PollOutcome::NotFound,
        });
        expired.handle(Event::Cancel);
        assert_eq!(expired.state(), &QuickConnectState::Expired);

        let mut failed = QuickConnectPoll::new();
        failed.handle(Event::InitiateFailed);
        failed.handle(Event::Cancel);
        assert_eq!(
            failed.state(),
            &QuickConnectState::Failed(FailureReason::InitiateFailed)
        );
    }

    #[test]
    fn initiate_failure_fails_the_flow() {
        let mut machine = QuickConnectPoll::new();
        assert_eq!(machine.handle(Event::InitiateFailed), Action::None);
        assert_eq!(
            machine.state(),
            &QuickConnectState::Failed(FailureReason::InitiateFailed)
        );
    }

    #[test]
    fn blank_code_fails_initiate() {
        let mut machine = QuickConnectPoll::new();
        machine.handle(Event::Initiated {
            now: secs(0),
            code: "   ".to_owned(),
        });
        assert_eq!(
            machine.state(),
            &QuickConnectState::Failed(FailureReason::InitiateFailed)
        );
    }

    #[test]
    fn out_of_order_events_are_ignored() {
        // Initiated twice: the second must not reset the clock.
        let mut machine = initiated();
        machine.handle(Event::Initiated {
            now: secs(100),
            code: "999999".to_owned(),
        });
        assert_eq!(
            machine.state(),
            &QuickConnectState::Initiated {
                code: "123456".to_owned(),
                started_at: secs(0),
            }
        );

        // A poll result with no poll outstanding is stale.
        let mut idle = QuickConnectPoll::new();
        let action = idle.handle(Event::PollResult {
            now: secs(1),
            outcome: PollOutcome::Authorized,
        });
        assert_eq!(action, Action::None);
        assert_eq!(idle.state(), &QuickConnectState::Idle);

        // A tick while Initiated before the interval is a no-op.
        let mut early = initiated();
        assert_eq!(early.handle(Event::Tick { now: secs(1) }), Action::None);
    }

    #[test]
    fn expiry_on_404_before_timeout_has_no_follow_up_poll() {
        let mut machine = polling_in_flight();
        machine.handle(Event::PollResult {
            now: secs(2),
            outcome: PollOutcome::NotFound,
        });
        assert_eq!(machine.handle(Event::Tick { now: secs(4) }), Action::None);
        assert_eq!(machine.state(), &QuickConnectState::Expired);
    }
}
