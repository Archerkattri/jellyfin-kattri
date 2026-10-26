//! Defers an update while playback is active (M4-16 edge case). Pure: the app passes its
//! playback state in and acts on the result.

/// The playback state as the updater sees it. The app maps its own phase onto these four.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaybackState {
    /// No media session is open. Ended and failed sessions also map here.
    Stopped,
    /// A session is opening its media.
    Loading,
    Playing,
    Paused,
}

/// Whether an update may be applied now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateGate {
    Proceed,
    /// Apply later, when playback has stopped. Applying restarts the app.
    Deferred,
}

/// Defer the update while any media session is open.
///
/// Paused counts as active: a restart would end the session the user is still in.
pub fn playback_gate(state: PlaybackState) -> UpdateGate {
    match state {
        PlaybackState::Stopped => UpdateGate::Proceed,
        PlaybackState::Loading | PlaybackState::Playing | PlaybackState::Paused => {
            UpdateGate::Deferred
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{PlaybackState, UpdateGate, playback_gate};

    #[test]
    fn playing_defers_the_update() {
        assert_eq!(playback_gate(PlaybackState::Playing), UpdateGate::Deferred);
    }

    #[test]
    fn loading_and_paused_sessions_also_defer() {
        assert_eq!(playback_gate(PlaybackState::Loading), UpdateGate::Deferred);
        assert_eq!(playback_gate(PlaybackState::Paused), UpdateGate::Deferred);
    }

    #[test]
    fn stopped_playback_lets_the_update_proceed() {
        assert_eq!(playback_gate(PlaybackState::Stopped), UpdateGate::Proceed);
    }
}
