//! Native report admission: retain FIFO order until the worker accepts a copy.

use std::{collections::VecDeque, time::Duration};

use jellyfin_egui_client::PlaybackReportSubmitError;
use kattri_client_core::playback_reporting::{
    PlaybackReportAction, PlaybackReportInput, PlaybackReportScheduler,
};

const MAX_PENDING_REPORTS: usize = 64;
pub(crate) const REPORT_RETRY_INTERVAL: Duration = Duration::from_millis(100);
const CLOSE_ADMISSION_GRACE: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct ReportCounts {
    pub(crate) started: usize,
    pub(crate) progress: usize,
    pub(crate) stopped: usize,
}

pub(crate) struct ReportHandoff {
    pending: VecDeque<PlaybackReportAction>,
    capacity: usize,
    admission_error: Option<PlaybackReportSubmitError>,
    next_attempt: Duration,
    refused_updates: usize,
    rejected_progress: usize,
}

impl Default for ReportHandoff {
    fn default() -> Self {
        Self {
            pending: VecDeque::new(),
            capacity: MAX_PENDING_REPORTS,
            admission_error: None,
            next_attempt: Duration::ZERO,
            refused_updates: 0,
            rejected_progress: 0,
        }
    }
}

impl ReportHandoff {
    /// Check capacity before advancing the scheduler. Snapshots reserve a slot
    /// for the active session's final Stop, including replacement Stop/Start.
    /// A refusal is visible and leaves the scheduler/lifecycle unchanged.
    pub(crate) fn apply(
        &mut self,
        scheduler: &mut PlaybackReportScheduler,
        input: PlaybackReportInput,
    ) -> bool {
        let required = match &input {
            PlaybackReportInput::Stop { .. } => usize::from(scheduler.active_session().is_some()),
            // A snapshot can replace the scheduler's current item even if a
            // caller reuses a session key. Budget both Stop/Start plus the
            // future final Stop before applying it; no action is evicted.
            PlaybackReportInput::Snapshot { .. } | PlaybackReportInput::Seek { .. } => 3,
        };
        if self.capacity.saturating_sub(self.pending.len()) < required {
            self.refused_updates = self.refused_updates.saturating_add(1);
            return false;
        }
        self.pending.extend(scheduler.apply(input));
        true
    }

    pub(crate) fn flush(
        &mut self,
        now: Duration,
        mut submit: impl FnMut(PlaybackReportAction) -> Result<u64, PlaybackReportSubmitError>,
    ) {
        if now < self.next_attempt {
            return;
        }
        self.admission_error = None;
        while let Some(action) = self.pending.front() {
            // The public app API consumes the argument even when admission is
            // refused. Keep the original head until acceptance is known.
            match submit(action.clone()) {
                Ok(_) => {
                    self.pending.pop_front();
                }
                Err(PlaybackReportSubmitError::ProgressRetryBudgetExhausted)
                    if matches!(action, PlaybackReportAction::Progress(_)) =>
                {
                    // This is the app's explicit terminal Progress policy,
                    // not queue pressure. Expose the rejection and allow Stop.
                    self.rejected_progress = self.rejected_progress.saturating_add(1);
                    self.pending.pop_front();
                }
                Err(error) => {
                    self.admission_error = Some(error);
                    self.next_attempt = now.saturating_add(REPORT_RETRY_INTERVAL);
                    break;
                }
            }
        }
    }

    pub(crate) fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    pub(crate) fn retry_delay(&self, now: Duration) -> Option<Duration> {
        self.has_pending()
            .then(|| self.next_attempt.saturating_sub(now))
    }

    pub(crate) fn counts(&self) -> ReportCounts {
        let mut counts = ReportCounts::default();
        for action in &self.pending {
            match action {
                PlaybackReportAction::Started(_) => counts.started += 1,
                PlaybackReportAction::Progress(_) => counts.progress += 1,
                PlaybackReportAction::Stopped(_) => counts.stopped += 1,
            }
        }
        counts
    }

    /// Do not replay reports created for one Jellyfin account through another.
    /// The caller reports only aggregate counts to the UI and logs.
    pub(crate) fn clear_for_connection_change(&mut self) -> ReportCounts {
        let counts = self.counts();
        self.pending.clear();
        self.admission_error = None;
        self.next_attempt = Duration::ZERO;
        self.refused_updates = 0;
        self.rejected_progress = 0;
        counts
    }

    pub(crate) fn feedback(&self) -> Option<String> {
        let mut messages = Vec::new();
        if let Some(error) = self.admission_error {
            let reason = match error {
                PlaybackReportSubmitError::CommandQueueFull => "The network queue is full",
                PlaybackReportSubmitError::CommandQueueClosed => "The network worker has stopped",
                PlaybackReportSubmitError::WorkerUnavailable => "The network worker is unavailable",
                PlaybackReportSubmitError::ProgressRetryBudgetExhausted => {
                    "A required playback report was refused"
                }
            };
            let counts = self.counts();
            messages.push(format!(
                "{reason}. Retrying retained playback reports in order (Start: {}, Progress: {}, Stop: {}).",
                counts.started, counts.progress, counts.stopped,
            ));
        }
        if self.refused_updates != 0 {
            messages.push(format!(
                "Playback report buffer reached its limit: {} player updates could not be scheduled. Existing reports are retained; final Stop space is reserved.",
                self.refused_updates,
            ));
        }
        if self.rejected_progress != 0 {
            messages.push(format!(
                "{} Progress reports were rejected after the network retry budget was exhausted. Start and Stop reports remain protected.",
                self.rejected_progress,
            ));
        }
        (!messages.is_empty()).then(|| messages.join(" "))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CloseAdmission {
    Idle,
    Retry,
    Ready,
    TimedOut,
}

#[derive(Default)]
pub(crate) struct ReportCloseGrace {
    started_at: Option<Duration>,
}

impl ReportCloseGrace {
    pub(crate) fn begin(&mut self, now: Duration) {
        self.started_at = Some(now);
    }

    pub(crate) fn advance(&mut self, now: Duration, pending: bool) -> CloseAdmission {
        let Some(started_at) = self.started_at else {
            return CloseAdmission::Idle;
        };
        if !pending {
            self.started_at = None;
            CloseAdmission::Ready
        } else if now.saturating_sub(started_at) < CLOSE_ADMISSION_GRACE {
            CloseAdmission::Retry
        } else {
            self.started_at = None;
            CloseAdmission::TimedOut
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jellyfin_api::{JellyfinTicks, PlaybackProgressInfo};
    use kattri_client_core::playback_reporting::PlaybackReportSessionId;

    fn scheduler() -> PlaybackReportScheduler {
        PlaybackReportScheduler::new(Duration::from_secs(10)).unwrap()
    }

    fn snapshot(session: u64, seconds: u64) -> PlaybackReportInput {
        let mut report = PlaybackProgressInfo::new(format!("test-item-{session}"));
        report.position_ticks = Some(JellyfinTicks::from_ticks(seconds as i64 * 10_000_000));
        report.is_paused = Some(false);
        PlaybackReportInput::Snapshot {
            session: PlaybackReportSessionId::new(session),
            at: Duration::from_secs(seconds),
            report,
        }
    }

    fn stop(session: u64, seconds: u64) -> PlaybackReportInput {
        PlaybackReportInput::Stop {
            session: PlaybackReportSessionId::new(session),
            item_id: format!("test-item-{session}"),
            final_position: Some(JellyfinTicks::from_ticks(seconds as i64 * 10_000_000)),
            failed: false,
        }
    }

    fn key(action: &PlaybackReportAction) -> (&'static str, i64) {
        let (kind, position) = match action {
            PlaybackReportAction::Started(report) => ("start", report.position_ticks),
            PlaybackReportAction::Progress(report) => ("progress", report.position_ticks),
            PlaybackReportAction::Stopped(report) => ("stop", report.position_ticks),
        };
        (kind, position.unwrap().as_ticks() / 10_000_000)
    }

    #[test]
    fn busy_start_retains_every_report_and_retries_multiple_sessions_in_fifo_order() {
        let mut queue = ReportHandoff::default();
        let mut scheduler = scheduler();
        assert!(queue.apply(&mut scheduler, snapshot(1, 0)));
        let mut attempted = Vec::new();
        queue.flush(Duration::ZERO, |action| {
            attempted.push(key(&action));
            Err(PlaybackReportSubmitError::CommandQueueFull)
        });
        assert_eq!(attempted, [("start", 0)]);
        assert!(queue.feedback().unwrap().contains("network queue is full"));
        assert!(queue.apply(&mut scheduler, snapshot(1, 10)));
        assert!(queue.apply(&mut scheduler, snapshot(1, 20)));
        assert!(queue.apply(&mut scheduler, stop(1, 21)));
        assert!(queue.apply(&mut scheduler, snapshot(2, 1)));
        assert!(queue.apply(&mut scheduler, stop(2, 2)));
        let mut accepted = Vec::new();
        queue.flush(REPORT_RETRY_INTERVAL, |action| {
            accepted.push(key(&action));
            Ok(1)
        });
        assert_eq!(
            accepted,
            [
                ("start", 0),
                ("progress", 10),
                ("progress", 20),
                ("stop", 21),
                ("start", 1),
                ("stop", 2)
            ]
        );
        assert!(!queue.has_pending());
        assert!(queue.feedback().is_none());
    }

    #[test]
    fn busy_progress_and_busy_stop_never_replay_an_already_accepted_start() {
        let mut queue = ReportHandoff::default();
        let mut scheduler = scheduler();
        for input in [snapshot(1, 0), snapshot(1, 10), stop(1, 11)] {
            assert!(queue.apply(&mut scheduler, input));
        }
        let mut accepted = Vec::new();
        queue.flush(Duration::ZERO, |action| {
            if matches!(action, PlaybackReportAction::Progress(_)) {
                Err(PlaybackReportSubmitError::CommandQueueFull)
            } else {
                accepted.push(key(&action));
                Ok(1)
            }
        });
        assert_eq!(accepted, [("start", 0)]);
        queue.flush(REPORT_RETRY_INTERVAL, |action| {
            if matches!(action, PlaybackReportAction::Stopped(_)) {
                Err(PlaybackReportSubmitError::CommandQueueFull)
            } else {
                accepted.push(key(&action));
                Ok(2)
            }
        });
        assert_eq!(
            queue.counts(),
            ReportCounts {
                stopped: 1,
                ..ReportCounts::default()
            }
        );
        queue.flush(REPORT_RETRY_INTERVAL * 2, |action| {
            accepted.push(key(&action));
            Ok(3)
        });
        assert_eq!(accepted, [("start", 0), ("progress", 10), ("stop", 11)]);
    }

    #[test]
    fn busy_retries_wait_for_the_timer_and_new_stop_cannot_bypass_the_head() {
        let mut queue = ReportHandoff::default();
        let mut scheduler = scheduler();
        queue.apply(&mut scheduler, snapshot(1, 0));
        queue.flush(Duration::ZERO, |_| {
            Err(PlaybackReportSubmitError::CommandQueueFull)
        });
        queue.apply(&mut scheduler, stop(1, 1));
        let before_retry = REPORT_RETRY_INTERVAL - Duration::from_millis(1);
        let mut attempts = 0;
        queue.flush(before_retry, |_| {
            attempts += 1;
            Ok(1)
        });
        assert_eq!(attempts, 0);
        assert_eq!(
            queue.retry_delay(before_retry),
            Some(Duration::from_millis(1))
        );
        let mut accepted = Vec::new();
        queue.flush(REPORT_RETRY_INTERVAL, |action| {
            accepted.push(key(&action));
            Ok(1)
        });
        assert_eq!(accepted, [("start", 0), ("stop", 1)]);
        assert_eq!(queue.retry_delay(REPORT_RETRY_INTERVAL), None);
    }

    #[test]
    fn buffer_limit_visibly_refuses_new_updates_and_reserves_final_stop() {
        let mut queue = ReportHandoff {
            capacity: 5,
            ..ReportHandoff::default()
        };
        let mut scheduler = scheduler();
        for input in [snapshot(1, 0), snapshot(1, 10), snapshot(2, 0)] {
            assert!(queue.apply(&mut scheduler, input));
        }
        assert!(!queue.apply(&mut scheduler, snapshot(2, 30)));
        // A replacement is refused before the old lifecycle is changed.
        assert!(!queue.apply(&mut scheduler, snapshot(3, 0)));
        assert_eq!(
            scheduler.active_session(),
            Some(PlaybackReportSessionId::new(2))
        );
        assert!(
            queue
                .feedback()
                .unwrap()
                .contains("2 player updates could not be scheduled")
        );
        assert!(queue.apply(&mut scheduler, stop(2, 31)));
        assert_eq!(queue.pending.len(), queue.capacity);
        assert_eq!(
            queue.counts(),
            ReportCounts {
                started: 2,
                progress: 1,
                stopped: 2
            }
        );
        assert!(!queue.apply(&mut scheduler, snapshot(3, 0)));
        let mut accepted = Vec::new();
        queue.flush(Duration::ZERO, |action| {
            accepted.push(key(&action));
            Ok(1)
        });
        assert_eq!(
            accepted,
            [
                ("start", 0),
                ("progress", 10),
                ("stop", 10),
                ("start", 0),
                ("stop", 31)
            ]
        );
        assert!(queue.apply(&mut scheduler, snapshot(3, 1)));
        assert_eq!(key(queue.pending.front().unwrap()), ("start", 1));
    }

    #[test]
    fn closed_or_unavailable_worker_and_protected_rejection_retain_the_head() {
        for error in [
            PlaybackReportSubmitError::CommandQueueClosed,
            PlaybackReportSubmitError::WorkerUnavailable,
            PlaybackReportSubmitError::ProgressRetryBudgetExhausted,
        ] {
            let mut queue = ReportHandoff::default();
            let mut scheduler = scheduler();
            queue.apply(&mut scheduler, snapshot(1, 0));
            queue.apply(&mut scheduler, stop(1, 1));
            let mut attempts = 0;
            queue.flush(Duration::ZERO, |_| {
                attempts += 1;
                Err(error)
            });
            assert_eq!(attempts, 1);
            assert_eq!(
                queue.counts(),
                ReportCounts {
                    started: 1,
                    stopped: 1,
                    ..ReportCounts::default()
                }
            );
            assert!(queue.feedback().is_some());
        }
    }

    #[test]
    fn exhausted_progress_is_visibly_rejected_and_does_not_prevent_stop() {
        let mut queue = ReportHandoff::default();
        let mut scheduler = scheduler();
        for input in [snapshot(1, 0), snapshot(1, 10), stop(1, 11)] {
            queue.apply(&mut scheduler, input);
        }
        let mut accepted = Vec::new();
        queue.flush(Duration::ZERO, |action| {
            if matches!(action, PlaybackReportAction::Progress(_)) {
                Err(PlaybackReportSubmitError::ProgressRetryBudgetExhausted)
            } else {
                accepted.push(key(&action));
                Ok(1)
            }
        });
        assert_eq!(accepted, [("start", 0), ("stop", 11)]);
        assert!(!queue.has_pending());
        assert!(
            queue
                .feedback()
                .unwrap()
                .contains("1 Progress reports were rejected")
        );
    }

    #[test]
    fn connection_change_discards_old_outbox_and_reports_only_aggregate_counts() {
        let mut queue = ReportHandoff::default();
        let mut scheduler = scheduler();
        queue.apply(&mut scheduler, snapshot(1, 0));
        queue.apply(&mut scheduler, snapshot(1, 10));
        queue.apply(&mut scheduler, stop(1, 11));
        assert_eq!(
            queue.clear_for_connection_change(),
            ReportCounts {
                started: 1,
                progress: 1,
                stopped: 1,
            }
        );
        assert!(!queue.has_pending());
        assert_eq!(queue.counts(), ReportCounts::default());
        let mut attempted = false;
        queue.flush(Duration::ZERO, |_| {
            attempted = true;
            Ok(1)
        });
        assert!(!attempted);
    }

    #[test]
    fn close_waits_for_admission_then_leaves_accepted_commands_to_worker_drop() {
        let mut grace = ReportCloseGrace::default();
        assert_eq!(grace.advance(Duration::ZERO, true), CloseAdmission::Idle);
        grace.begin(Duration::from_secs(10));
        assert_eq!(
            grace.advance(Duration::from_millis(11_999), true),
            CloseAdmission::Retry
        );
        assert_eq!(
            grace.advance(Duration::from_millis(11_999), false),
            CloseAdmission::Ready
        );
        assert_eq!(
            grace.advance(Duration::from_secs(12), false),
            CloseAdmission::Idle
        );
    }

    #[test]
    fn close_timeout_does_not_drain_reports_or_automatically_exit_later() {
        let mut queue = ReportHandoff::default();
        let mut scheduler = scheduler();
        queue.apply(&mut scheduler, snapshot(1, 0));
        queue.apply(&mut scheduler, stop(1, 1));
        let mut grace = ReportCloseGrace::default();
        grace.begin(Duration::ZERO);
        queue.flush(Duration::ZERO, |_| {
            Err(PlaybackReportSubmitError::CommandQueueFull)
        });
        assert_eq!(
            grace.advance(Duration::from_secs(2), queue.has_pending()),
            CloseAdmission::TimedOut
        );
        assert_eq!(
            queue.counts(),
            ReportCounts {
                started: 1,
                stopped: 1,
                ..ReportCounts::default()
            }
        );
        queue.flush(Duration::from_secs(3), |_| Ok(1));
        assert_eq!(
            grace.advance(Duration::from_secs(3), queue.has_pending()),
            CloseAdmission::Idle
        );
        grace.begin(Duration::from_secs(4));
        assert_eq!(
            grace.advance(Duration::from_secs(4), queue.has_pending()),
            CloseAdmission::Ready
        );
    }
}
