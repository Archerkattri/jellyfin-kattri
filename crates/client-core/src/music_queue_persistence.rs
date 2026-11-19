//! Music-queue persistence bookkeeping: when to encode and when to write.
//!
//! The queue is saved to settings on change, with a periodic checkpoint so a long listen that only
//! moves the position is still persisted. The app used to keep that bookkeeping in four loose fields
//! (`music_queue_last_saved_json`, `music_queue_saved_fingerprint`, `music_queue_last_saved_at`,
//! `music_queue_persistence_suspended`) and re-derive the "is it dirty / is a checkpoint due" rules
//! inline every frame.
//!
//! This type owns those rules. It is pure: the caller computes the fingerprint, encodes the queue
//! and performs the write; this decides whether that work is needed and records the outcome.

use std::time::Duration;

/// How often a save happens even when only the position moved.
pub const CHECKPOINT_INTERVAL: Duration = Duration::from_secs(10);

/// Persistence bookkeeping for the saved music queue.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MusicQueuePersistence {
    suspended: bool,
    last_saved_json: Option<String>,
    saved_fingerprint: u64,
    last_saved_at: Duration,
}

impl Default for MusicQueuePersistence {
    fn default() -> Self {
        Self::new()
    }
}

impl MusicQueuePersistence {
    /// A fresh state: nothing saved yet, so the first save is due immediately.
    pub const fn new() -> Self {
        Self {
            suspended: false,
            last_saved_json: None,
            saved_fingerprint: u64::MAX,
            last_saved_at: Duration::ZERO,
        }
    }

    /// Whether saving is suspended (no usable settings directory).
    pub fn is_suspended(&self) -> bool {
        self.suspended
    }

    /// Suspend or resume saving.
    pub fn set_suspended(&mut self, suspended: bool) {
        self.suspended = suspended;
    }

    /// Forget the last save. The next change is saved immediately; the suspension flag is left to
    /// the caller because the reasons differ (session reset vs restore finished).
    pub fn reset(&mut self) {
        self.last_saved_json = None;
        self.saved_fingerprint = u64::MAX;
        self.last_saved_at = Duration::ZERO;
    }

    /// Whether a checkpoint is due at `now`.
    fn checkpoint_due(&self, now: Duration) -> bool {
        now.saturating_sub(self.last_saved_at) >= CHECKPOINT_INTERVAL
    }

    /// Whether encoding the queue is worth doing: its fingerprint changed, or a checkpoint is due.
    pub fn needs_encode(&self, fingerprint: u64, now: Duration) -> bool {
        fingerprint != self.saved_fingerprint || self.checkpoint_due(now)
    }

    /// Whether an encoded queue must be written: the bytes differ from the last save, or a
    /// checkpoint is due.
    pub fn should_write(&self, encoded: &str, now: Duration) -> bool {
        self.last_saved_json.as_deref() != Some(encoded) || self.checkpoint_due(now)
    }

    /// Record that the same content was seen again, so it is not re-encoded every frame. The save
    /// time is deliberately not advanced: a checkpoint still falls due on schedule.
    pub fn record_unchanged(&mut self, fingerprint: u64) {
        self.saved_fingerprint = fingerprint;
    }

    /// Record a save attempt (successful or failed: the bytes are unchanged either way, and
    /// retrying a failing write every frame would be worse than waiting for the next checkpoint).
    pub fn record(&mut self, encoded: String, fingerprint: u64, now: Duration) {
        self.last_saved_json = Some(encoded);
        self.saved_fingerprint = fingerprint;
        self.last_saved_at = now;
    }

    /// Time until the next checkpoint, for repaint scheduling.
    pub fn next_checkpoint_in(&self, now: Duration) -> Duration {
        CHECKPOINT_INTERVAL.saturating_sub(now.saturating_sub(self.last_saved_at))
    }
}

#[cfg(test)]
mod tests {
    use super::{CHECKPOINT_INTERVAL, MusicQueuePersistence};
    use std::time::Duration;

    fn saved() -> MusicQueuePersistence {
        let mut state = MusicQueuePersistence::new();
        state.record("encoded-1".into(), 1, Duration::ZERO);
        state
    }

    #[test]
    fn a_fresh_state_wants_to_encode_and_write() {
        let state = MusicQueuePersistence::new();
        assert!(!state.is_suspended());
        assert!(state.needs_encode(0, Duration::ZERO));
        assert!(state.should_write("anything", Duration::ZERO));
    }

    #[test]
    fn an_unchanged_queue_is_not_encoded_before_the_checkpoint() {
        let state = saved();
        assert!(!state.needs_encode(1, Duration::from_secs(9)));
        assert!(!state.should_write("encoded-1", Duration::from_secs(9)));
    }

    #[test]
    fn a_changed_fingerprint_is_encoded_and_written_immediately() {
        let state = saved();
        assert!(state.needs_encode(2, Duration::from_secs(1)));
        assert!(state.should_write("encoded-2", Duration::from_secs(1)));
    }

    #[test]
    fn the_checkpoint_forces_a_write_even_when_nothing_changed() {
        let state = saved();
        let due = CHECKPOINT_INTERVAL;
        assert!(state.needs_encode(1, due));
        assert!(state.should_write("encoded-1", due));
    }

    #[test]
    fn recording_unchanged_keeps_the_checkpoint_schedule() {
        let mut state = saved();
        state.record_unchanged(5);
        assert!(!state.needs_encode(5, Duration::from_secs(5)));
        assert!(
            state.needs_encode(5, CHECKPOINT_INTERVAL),
            "the checkpoint still falls due"
        );
    }

    #[test]
    fn recording_a_save_restarts_the_checkpoint() {
        let mut state = saved();
        state.record("encoded-2".into(), 2, Duration::from_secs(30));
        assert!(!state.needs_encode(2, Duration::from_secs(35)));
        assert_eq!(
            state.next_checkpoint_in(Duration::from_secs(30)),
            CHECKPOINT_INTERVAL
        );
        assert_eq!(
            state.next_checkpoint_in(Duration::from_secs(35)),
            CHECKPOINT_INTERVAL - Duration::from_secs(5)
        );
    }

    #[test]
    fn suspension_is_reported_and_reversible() {
        let mut state = MusicQueuePersistence::new();
        state.set_suspended(true);
        assert!(state.is_suspended());
        state.set_suspended(false);
        assert!(!state.is_suspended());
    }

    #[test]
    fn reset_makes_the_next_save_due_immediately() {
        let mut state = saved();
        state.reset();
        assert!(state.needs_encode(1, Duration::ZERO));
        assert!(state.should_write("encoded-1", Duration::ZERO));
        assert_eq!(
            state.next_checkpoint_in(Duration::ZERO),
            CHECKPOINT_INTERVAL
        );
    }

    #[test]
    fn the_checkpoint_interval_is_ten_seconds() {
        assert_eq!(CHECKPOINT_INTERVAL, Duration::from_secs(10));
    }
}
