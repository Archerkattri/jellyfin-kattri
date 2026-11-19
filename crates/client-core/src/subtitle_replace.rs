//! Burn-in subtitle replacement: ownership of the pending operation and its terminal failure.
//!
//! Replacing the burned-in subtitle of a transcoded stream is a relay handoff: the app asks the
//! worker for a new stream, waits for it to open, and falls back to the previous subtitle if the
//! new one fails. The app tracked that with a pending-operation record plus a "terminal failure
//! generation" used to end the host's generation-scoped SyncPlay bridge.
//!
//! This type owns that state and the staleness predicate. It is deliberately narrow: the relay
//! orchestration stays in the app, which is where the playback and player state live. What moves
//! here is the answer to "which replacement is in flight, does this result belong to it, and did
//! this generation fail terminally" — asked in several places before, answered here once.
//!
//! Pure: no I/O, no clock, no threads.

/// One in-flight subtitle replacement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingReplacement {
    /// The request id the worker answered under.
    pub request_id: u64,
    /// The playback generation this replacement belongs to. A result for another generation is
    /// stale even when its request id matches, because the player has moved on.
    pub playback_generation: u64,
    pub item_id: String,
    /// The subtitle stream the replacement is switching to.
    pub subtitle_stream_index: Option<i32>,
    /// Where the player was when the replacement was requested.
    pub position_ticks: i64,
    /// Whether the player was paused, so the replacement can restore that state.
    pub was_paused: bool,
    /// Whether this replacement is itself a rollback to the previous subtitle.
    pub rollback: bool,
}

/// Owns the pending subtitle replacement and the terminal failure generation.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SubtitleReplaceMachine {
    pending: Option<PendingReplacement>,
    terminal_failure_generation: Option<u64>,
}

impl SubtitleReplaceMachine {
    pub const fn new() -> Self {
        Self {
            pending: None,
            terminal_failure_generation: None,
        }
    }

    /// Record a replacement the worker accepted. A newer replacement supersedes an older one.
    pub fn prepare(&mut self, pending: PendingReplacement) {
        self.pending = Some(pending);
    }

    /// The pending replacement, if any.
    pub fn pending(&self) -> Option<&PendingReplacement> {
        self.pending.as_ref()
    }

    /// Take the pending replacement, leaving the machine empty.
    pub fn take_pending(&mut self) -> Option<PendingReplacement> {
        self.pending.take()
    }

    /// Drop the pending replacement without returning it.
    pub fn clear_pending(&mut self) {
        self.pending = None;
    }

    pub fn is_pending(&self) -> bool {
        self.pending.is_some()
    }

    /// The pending replacement when it belongs to `playback_generation`.
    pub fn pending_for_generation(&self, playback_generation: u64) -> Option<&PendingReplacement> {
        self.pending
            .as_ref()
            .filter(|pending| pending.playback_generation == playback_generation)
    }

    /// Whether `request_id` names the pending replacement for `playback_generation` and `item_id`.
    ///
    /// This is the request-id half of staleness. The app additionally checks that the generation
    /// and item still match the live player before applying a result.
    pub fn pending_matches(
        &self,
        request_id: u64,
        playback_generation: u64,
        item_id: &str,
    ) -> bool {
        self.pending.as_ref().is_some_and(|pending| {
            pending.request_id == request_id
                && pending.playback_generation == playback_generation
                && pending.item_id == item_id
        })
    }

    /// Record that `playback_generation` failed terminally (a failed rollback). The host reads this
    /// once to end its generation-scoped SyncPlay bridge.
    pub fn record_terminal_failure(&mut self, playback_generation: u64) {
        self.terminal_failure_generation = Some(playback_generation);
    }

    /// Take the terminal failure generation, if one is waiting for the host.
    pub fn take_terminal_failure(&mut self) -> Option<u64> {
        self.terminal_failure_generation.take()
    }

    /// The terminal failure generation without consuming it, for tests and logging.
    pub fn terminal_failure(&self) -> Option<u64> {
        self.terminal_failure_generation
    }

    /// Drop both the pending replacement and the terminal failure. Used by a session or playback
    /// reset, where neither belongs to the new generation.
    pub fn reset(&mut self) {
        self.pending = None;
        self.terminal_failure_generation = None;
    }
}

#[cfg(test)]
mod tests {
    use super::{PendingReplacement, SubtitleReplaceMachine};

    fn pending(request_id: u64, generation: u64) -> PendingReplacement {
        PendingReplacement {
            request_id,
            playback_generation: generation,
            item_id: "movie".into(),
            subtitle_stream_index: Some(2),
            position_ticks: 600_000_000,
            was_paused: false,
            rollback: false,
        }
    }

    #[test]
    fn a_prepared_replacement_is_pending_and_matches_its_request() {
        let mut machine = SubtitleReplaceMachine::new();
        assert!(!machine.is_pending());
        machine.prepare(pending(7, 3));
        assert!(machine.is_pending());
        assert!(machine.pending_matches(7, 3, "movie"));
        assert_eq!(
            machine.pending().map(|p| p.position_ticks),
            Some(600_000_000)
        );
    }

    #[test]
    fn a_result_for_another_request_generation_or_item_does_not_match() {
        let mut machine = SubtitleReplaceMachine::new();
        machine.prepare(pending(7, 3));
        assert!(!machine.pending_matches(6, 3, "movie"), "wrong request id");
        assert!(
            !machine.pending_matches(7, 4, "movie"),
            "the player moved to another generation"
        );
        assert!(!machine.pending_matches(7, 3, "other"), "wrong item");
    }

    #[test]
    fn taking_the_pending_replacement_empties_the_machine() {
        let mut machine = SubtitleReplaceMachine::new();
        machine.prepare(pending(7, 3));
        let taken = machine.take_pending().expect("pending");
        assert_eq!(taken.request_id, 7);
        assert!(!machine.is_pending());
        assert!(machine.take_pending().is_none());
    }

    #[test]
    fn a_newer_replacement_supersedes_an_older_one() {
        let mut machine = SubtitleReplaceMachine::new();
        machine.prepare(pending(7, 3));
        machine.prepare(pending(8, 3));
        assert!(machine.pending_matches(8, 3, "movie"));
        assert!(!machine.pending_matches(7, 3, "movie"));
    }

    #[test]
    fn pending_for_generation_filters_by_the_player_generation() {
        let mut machine = SubtitleReplaceMachine::new();
        machine.prepare(pending(7, 3));
        assert_eq!(
            machine.pending_for_generation(3).map(|p| p.request_id),
            Some(7)
        );
        assert!(machine.pending_for_generation(4).is_none());
    }

    #[test]
    fn a_terminal_failure_is_recorded_once_and_taken_once() {
        let mut machine = SubtitleReplaceMachine::new();
        assert_eq!(machine.take_terminal_failure(), None);
        machine.record_terminal_failure(3);
        assert_eq!(machine.terminal_failure(), Some(3));
        assert_eq!(machine.take_terminal_failure(), Some(3));
        assert_eq!(
            machine.take_terminal_failure(),
            None,
            "the host must not see it twice"
        );
    }

    #[test]
    fn reset_clears_the_pending_replacement_and_the_terminal_failure() {
        let mut machine = SubtitleReplaceMachine::new();
        machine.prepare(pending(7, 3));
        machine.record_terminal_failure(3);
        machine.reset();
        assert!(!machine.is_pending());
        assert_eq!(machine.terminal_failure(), None);
    }

    #[test]
    fn clearing_the_pending_replacement_keeps_the_terminal_failure() {
        let mut machine = SubtitleReplaceMachine::new();
        machine.prepare(pending(7, 3));
        machine.record_terminal_failure(2);
        machine.clear_pending();
        assert!(!machine.is_pending());
        assert_eq!(
            machine.terminal_failure(),
            Some(2),
            "a failure for another generation is not cleared by a later handoff"
        );
    }
}
