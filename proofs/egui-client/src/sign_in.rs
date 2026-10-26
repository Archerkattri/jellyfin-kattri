//! Quick Connect on the sign-in screen. The app side keeps the polling state machine and
//! the text shown for each state. The worker holds the secret and makes the requests.

use std::time::Duration;

pub(crate) use kattri_client_core::quick_connect_poll::Action;
use kattri_client_core::quick_connect_poll::{
    Event, PollOutcome, QuickConnectPoll, QuickConnectState,
};

/// One Quick Connect attempt, from the request for a code to sign-in or cancel.
pub(crate) struct QuickConnectFlow {
    request_id: u64,
    machine: QuickConnectPoll,
}

impl QuickConnectFlow {
    pub(crate) fn new(request_id: u64) -> Self {
        Self {
            request_id,
            machine: QuickConnectPoll::new(),
        }
    }

    pub(crate) fn request_id(&self) -> u64 {
        self.request_id
    }

    pub(crate) fn state(&self) -> &QuickConnectState {
        self.machine.state()
    }

    /// True once the flow has ended for any reason, including approval.
    pub(crate) fn is_finished(&self) -> bool {
        self.machine.state().is_terminal()
    }

    /// The worker returned a code.
    pub(crate) fn code_issued(&mut self, now: Duration, code: String) -> Action {
        self.machine.handle(Event::Initiated { now, code })
    }

    /// The worker could not start the request.
    pub(crate) fn initiate_failed(&mut self) -> Action {
        self.machine.handle(Event::InitiateFailed)
    }

    /// Time has advanced. Returns `Poll` when a poll is due.
    pub(crate) fn tick(&mut self, now: Duration) -> Action {
        self.machine.handle(Event::Tick { now })
    }

    /// The worker finished one poll.
    pub(crate) fn poll_result(&mut self, now: Duration, outcome: PollOutcome) -> Action {
        self.machine.handle(Event::PollResult { now, outcome })
    }

    pub(crate) fn cancel(&mut self) -> Action {
        self.machine.handle(Event::Cancel)
    }
}

/// The code to show while the user approves the request, if any.
pub(crate) fn pending_code(state: &QuickConnectState) -> Option<&str> {
    match state {
        QuickConnectState::Initiated { code, .. } | QuickConnectState::Polling { code, .. } => {
            Some(code.as_str())
        }
        _ => None,
    }
}

/// Short status line for the sign-in screen. `None` when no attempt is in progress or
/// the attempt was cancelled.
pub(crate) fn status_text(state: &QuickConnectState) -> Option<&'static str> {
    use kattri_client_core::quick_connect_poll::FailureReason;
    match state {
        QuickConnectState::Idle | QuickConnectState::Cancelled => None,
        QuickConnectState::Initiated { .. } | QuickConnectState::Polling { .. } => {
            Some("Enter this code in an approved Jellyfin device. Waiting for approval.")
        }
        QuickConnectState::Authorized => Some("Approved. Signing in."),
        QuickConnectState::Expired => Some("Quick Connect code expired. Start again."),
        QuickConnectState::Failed(FailureReason::InitiateFailed) => {
            Some("Quick Connect could not start on this server.")
        }
        QuickConnectState::Failed(FailureReason::PollErrors) => {
            Some("Quick Connect stopped after repeated network errors. Start again.")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{QuickConnectFlow, pending_code, status_text};
    use kattri_client_core::quick_connect_poll::{Action, PollOutcome, QuickConnectState};
    use std::time::Duration;

    fn secs(value: u64) -> Duration {
        Duration::from_secs(value)
    }

    #[test]
    fn approval_after_one_pending_poll_leads_to_authentication() {
        let mut flow = QuickConnectFlow::new(7);
        assert_eq!(flow.code_issued(secs(0), "482913".into()), Action::None);
        assert_eq!(pending_code(flow.state()), Some("482913"));
        assert_eq!(flow.tick(secs(1)), Action::None);
        assert_eq!(flow.tick(secs(2)), Action::Poll);
        assert_eq!(
            flow.poll_result(secs(2), PollOutcome::Pending),
            Action::None
        );
        assert_eq!(flow.tick(secs(4)), Action::Poll);
        assert_eq!(
            flow.poll_result(secs(4), PollOutcome::Authorized),
            Action::Authenticate
        );
        assert!(flow.is_finished());
        assert_eq!(status_text(flow.state()), Some("Approved. Signing in."));
    }

    #[test]
    fn cancel_ends_the_flow_without_a_status_line() {
        let mut flow = QuickConnectFlow::new(3);
        flow.code_issued(secs(0), "111222".into());
        assert_eq!(flow.cancel(), Action::None);
        assert!(flow.is_finished());
        assert!(pending_code(flow.state()).is_none());
        assert!(status_text(flow.state()).is_none());
    }

    #[test]
    fn a_not_found_poll_reports_expiry() {
        let mut flow = QuickConnectFlow::new(4);
        flow.code_issued(secs(0), "555666".into());
        flow.tick(secs(2));
        flow.poll_result(secs(2), PollOutcome::Pending);
        flow.tick(secs(4));
        flow.poll_result(secs(4), PollOutcome::NotFound);
        assert_eq!(
            status_text(flow.state()),
            Some("Quick Connect code expired. Start again.")
        );
        assert!(pending_code(flow.state()).is_none());
    }

    #[test]
    fn a_failed_start_shows_the_start_message() {
        let mut flow = QuickConnectFlow::new(5);
        flow.initiate_failed();
        assert!(flow.is_finished());
        assert_eq!(
            status_text(flow.state()),
            Some("Quick Connect could not start on this server.")
        );
    }

    #[test]
    fn idle_state_has_no_code_and_no_text() {
        let flow = QuickConnectFlow::new(1);
        assert_eq!(*flow.state(), QuickConnectState::Idle);
        assert!(status_text(flow.state()).is_none());
        assert!(pending_code(flow.state()).is_none());
    }
}
