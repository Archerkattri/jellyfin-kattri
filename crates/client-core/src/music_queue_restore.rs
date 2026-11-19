//! Saved music-queue restore: the request lifecycle as an explicit, pure machine.
//!
//! On sign-in the app loads a persisted queue, asks the worker to re-fetch the item records, then
//! reconciles the saved queue against what the server still has. The app used to track that with
//! three loose fields (`pending_stored_music_queue`, `music_queue_restore_request_id`,
//! `music_queue_restore_loading`) and a hand-written guard in the event arm:
//!
//! ```text
//! WorkerEvent::QueueItemsRestored { request_id, items }
//!     if request_id == self.music_queue_restore_request_id
//!         && self.pending_stored_music_queue.is_some()
//! ```
//!
//! This machine owns that lifecycle. A result whose request id does not match the request in flight
//! is refused here, once, so the caller cannot apply a stale restore over a newer queue. Pure: no
//! I/O, no clock, no threads; the caller loads settings, submits the request and reconciles the
//! queue it gets back.

use crate::queue::Queue;
use crate::queue_store::StoredQueue;

/// A loaded saved queue waiting for its item records.
#[derive(Clone)]
pub struct PendingRestore {
    /// The saved queue order, cursor, shuffle and repeat state.
    pub queue: Queue,
    /// The saved playback offset, in milliseconds.
    pub position_ms: u64,
}

/// Where the restore lifecycle is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RestoreState {
    /// No saved queue, or the last restore finished.
    #[default]
    Idle,
    /// A saved queue is loaded and its item records have not been requested yet.
    WaitingForRequest,
    /// The item-record request is in flight under this id.
    Loading { request_id: u64 },
}

/// Owns the saved-queue restore lifecycle.
#[derive(Clone, Default)]
pub struct MusicQueueRestoreMachine {
    state: RestoreState,
    pending: Option<PendingRestore>,
}

impl MusicQueueRestoreMachine {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record the settings load result. `Some` starts a restore; `None` clears the lifecycle.
    pub fn loaded(&mut self, stored: Option<StoredQueue>) {
        match stored {
            Some(stored) => {
                self.pending = Some(PendingRestore {
                    queue: stored.queue,
                    position_ms: stored.position_ms,
                });
                self.state = RestoreState::WaitingForRequest;
            }
            None => {
                self.pending = None;
                self.state = RestoreState::Idle;
            }
        }
    }

    pub fn state(&self) -> RestoreState {
        self.state
    }

    /// Whether a restore is waiting to be requested or is in flight.
    pub fn has_pending(&self) -> bool {
        self.pending.is_some()
    }

    /// Whether the item-record request is in flight.
    pub fn is_loading(&self) -> bool {
        matches!(self.state, RestoreState::Loading { .. })
    }

    /// The item ids to fetch, when a request should be issued. `None` while idle or in flight, so
    /// a second call cannot issue the request twice.
    pub fn item_ids_to_restore(&self) -> Option<Vec<String>> {
        if self.state != RestoreState::WaitingForRequest {
            return None;
        }
        let pending = self.pending.as_ref()?;
        Some(
            pending
                .queue
                .items()
                .iter()
                .map(|item| item.id.clone())
                .collect(),
        )
    }

    /// Record that the request was submitted under `request_id`.
    ///
    /// Only meaningful while waiting for a request; a submission after the state moved on is
    /// ignored so a stale caller cannot put the machine back into `Loading`.
    pub fn request_issued(&mut self, request_id: u64) {
        if self.state == RestoreState::WaitingForRequest {
            self.state = RestoreState::Loading { request_id };
        }
    }

    /// The submission itself failed (worker stopped or queue full). Keep the loaded queue so a
    /// later attempt can retry; only the in-flight marker clears.
    pub fn request_failed(&mut self) {
        if self.is_loading() {
            self.state = RestoreState::WaitingForRequest;
        }
    }

    /// A restored result arrived. Returns the pending queue when the result is current; `None`
    /// means it is stale (or nothing was in flight) and must be dropped.
    ///
    /// A current result consumes the pending restore: the caller reconciles the returned queue
    /// against the returned items and installs it.
    pub fn restored(&mut self, request_id: u64) -> Option<PendingRestore> {
        if self.state != (RestoreState::Loading { request_id }) {
            return None;
        }
        self.state = RestoreState::Idle;
        self.pending.take()
    }

    /// A restore failure arrived. Returns `true` when it belongs to the request in flight, so the
    /// caller reports the failure; the loaded queue is kept for a later retry.
    pub fn restore_failed(&mut self, request_id: u64) -> bool {
        if self.state != (RestoreState::Loading { request_id }) {
            return false;
        }
        self.state = RestoreState::WaitingForRequest;
        true
    }

    /// The session ended: drop the lifecycle. A late result is refused because nothing is in
    /// flight afterwards.
    pub fn reset(&mut self) {
        self.state = RestoreState::Idle;
        self.pending = None;
    }
}

#[cfg(test)]
mod tests {
    use super::{MusicQueueRestoreMachine, RestoreState};
    use crate::queue::Queue;
    use crate::queue_store::StoredQueue;
    use jellyfin_api::session_reporting::QueueItem;

    fn stored() -> StoredQueue {
        let mut queue = Queue::with_seed(3);
        for id in ["track-1", "track-2"] {
            queue.add(QueueItem::new(id)).expect("append fixture");
        }
        queue.select(1).expect("select fixture");
        StoredQueue {
            queue,
            position_ms: 12_345,
        }
    }

    #[test]
    fn loading_a_saved_queue_waits_for_a_request() {
        let mut machine = MusicQueueRestoreMachine::new();
        assert_eq!(machine.state(), RestoreState::Idle);

        machine.loaded(Some(stored()));
        assert_eq!(machine.state(), RestoreState::WaitingForRequest);
        assert!(machine.has_pending());
        assert!(!machine.is_loading());
        assert_eq!(
            machine.item_ids_to_restore(),
            Some(vec!["track-1".to_owned(), "track-2".to_owned()])
        );
    }

    #[test]
    fn the_request_is_issued_once() {
        let mut machine = MusicQueueRestoreMachine::new();
        machine.loaded(Some(stored()));
        machine.request_issued(7);
        assert!(machine.is_loading());
        assert_eq!(
            machine.item_ids_to_restore(),
            None,
            "a second call must not issue the request again"
        );
    }

    #[test]
    fn a_current_result_returns_the_pending_queue_and_finishes() {
        let mut machine = MusicQueueRestoreMachine::new();
        machine.loaded(Some(stored()));
        machine.request_issued(7);

        let pending = machine.restored(7).expect("current result");
        assert_eq!(pending.position_ms, 12_345);
        assert_eq!(pending.queue.items().len(), 2);
        assert_eq!(machine.state(), RestoreState::Idle);
        assert!(
            !machine.has_pending(),
            "the result consumes the pending queue"
        );
        assert!(
            machine.restored(7).is_none(),
            "a second delivery is refused"
        );
    }

    #[test]
    fn a_stale_result_is_refused() {
        let mut machine = MusicQueueRestoreMachine::new();
        machine.loaded(Some(stored()));
        machine.request_issued(7);
        assert!(machine.restored(6).is_none());
        assert!(
            machine.is_loading(),
            "the current request is still in flight"
        );
        assert!(machine.has_pending());
    }

    #[test]
    fn a_result_after_a_reload_is_refused() {
        // Sign-out then sign-in loads a new saved queue: the old request must not apply.
        let mut machine = MusicQueueRestoreMachine::new();
        machine.loaded(Some(stored()));
        machine.request_issued(7);
        machine.loaded(Some(stored()));
        assert!(machine.restored(7).is_none());
        assert_eq!(machine.state(), RestoreState::WaitingForRequest);
    }

    #[test]
    fn a_failed_submission_keeps_the_queue_for_a_retry() {
        let mut machine = MusicQueueRestoreMachine::new();
        machine.loaded(Some(stored()));
        machine.request_issued(7);
        machine.request_failed();
        assert_eq!(machine.state(), RestoreState::WaitingForRequest);
        assert!(machine.has_pending());
        assert!(
            machine.item_ids_to_restore().is_some(),
            "a retry is possible"
        );
    }

    #[test]
    fn a_failure_result_is_only_accepted_for_the_request_in_flight() {
        let mut machine = MusicQueueRestoreMachine::new();
        machine.loaded(Some(stored()));
        machine.request_issued(7);
        assert!(!machine.restore_failed(6));
        assert!(machine.is_loading());
        assert!(machine.restore_failed(7));
        assert_eq!(machine.state(), RestoreState::WaitingForRequest);
        assert!(machine.has_pending());
    }

    #[test]
    fn reset_drops_the_lifecycle_and_refuses_late_results() {
        let mut machine = MusicQueueRestoreMachine::new();
        machine.loaded(Some(stored()));
        machine.request_issued(7);
        machine.reset();
        assert_eq!(machine.state(), RestoreState::Idle);
        assert!(!machine.has_pending());
        assert!(machine.restored(7).is_none());
    }

    #[test]
    fn no_saved_queue_is_idle_and_requests_nothing() {
        let mut machine = MusicQueueRestoreMachine::new();
        machine.loaded(Some(stored()));
        machine.loaded(None);
        assert_eq!(machine.state(), RestoreState::Idle);
        assert_eq!(machine.item_ids_to_restore(), None);
    }
}
