//! Commands the app sends to a playback engine, and events it reports back.

use std::fmt;
use std::time::Duration;

use crate::track::{Selection, Track, TrackId};

/// Opaque reference to a media source held by the playback relay.
///
/// Carries no URL and no credential. Its `Debug` output shows only the number.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct SourceHandle(u64);

impl SourceHandle {
    pub const fn new(id: u64) -> Self {
        Self(id)
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Debug for SourceHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SourceHandle({})", self.0)
    }
}

/// Instruction from the app to the engine.
#[derive(Debug, Clone, PartialEq)]
pub enum PlayerCommand {
    Load(SourceHandle),
    Play,
    Pause,
    TogglePause,
    Stop,
    SeekTo(Duration),
    /// Relative seek in seconds. Negative values seek backwards.
    SeekBy {
        seconds: f64,
    },
    SetSpeed(f32),
    /// Volume in percent. The valid range is governed by `audio.max_volume`.
    SetVolumePercent(u32),
    SetAudioTrack(TrackId),
    SetSubtitleTrack(Selection),
    /// Audio delay in milliseconds (legacy `audio_delay`, step 50 ms).
    SetAudioDelayMs(i32),
    /// Subtitle delay in milliseconds. Applies to the current item only; reset on stop (M2).
    SetSubtitleDelayMs(i32),
}

/// Notification from the engine to the app.
#[derive(Debug, Clone, PartialEq)]
pub enum PlayerEvent {
    Started {
        duration: Option<Duration>,
    },
    Position {
        position: Duration,
        duration: Option<Duration>,
    },
    Paused(bool),
    Buffering(bool),
    TracksChanged(Vec<Track>),
    Ended,
    Error(PlayerError),
}

/// Failure reported by a playback engine.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PlayerError {
    /// The engine has no implementation for this command yet.
    #[error("player is not implemented in this build")]
    NotImplemented,
    /// The source handle is unknown or has expired.
    #[error("unknown or expired source handle")]
    UnknownSource,
    /// The engine cannot decode or present this source. The app should request a transcode.
    #[error("source cannot be played directly")]
    UnsupportedSource,
    /// The engine failed. The message must not contain URLs or tokens.
    #[error("engine error: {0}")]
    Engine(String),
}

/// A playback engine. Implementations must not block in `execute`.
pub trait PlayerBackend {
    fn execute(&mut self, command: PlayerCommand) -> Result<(), PlayerError>;
    /// Drain events produced since the last call.
    fn drain_events(&mut self) -> Vec<PlayerEvent>;
}

/// Test double that records commands and replays scripted events.
#[derive(Debug, Default)]
pub struct RecordingPlayer {
    commands: Vec<PlayerCommand>,
    events: Vec<PlayerEvent>,
    fail_with: Option<PlayerError>,
}

impl RecordingPlayer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Queue an event for the next `drain_events` call.
    pub fn script_event(&mut self, event: PlayerEvent) {
        self.events.push(event);
    }

    /// Make every subsequent `execute` fail with `error`.
    pub fn fail_next_with(&mut self, error: PlayerError) {
        self.fail_with = Some(error);
    }

    pub fn commands(&self) -> &[PlayerCommand] {
        &self.commands
    }
}

impl PlayerBackend for RecordingPlayer {
    fn execute(&mut self, command: PlayerCommand) -> Result<(), PlayerError> {
        if let Some(error) = self.fail_with.clone() {
            return Err(error);
        }
        self.commands.push(command);
        Ok(())
    }

    fn drain_events(&mut self) -> Vec<PlayerEvent> {
        std::mem::take(&mut self.events)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_handle_debug_hides_nothing_secret_because_it_holds_no_url() {
        let h = SourceHandle::new(7);
        assert_eq!(format!("{h:?}"), "SourceHandle(7)");
        assert_eq!(h.get(), 7);
    }

    #[test]
    fn recording_player_records_commands_and_drains_events_once() {
        let mut p = RecordingPlayer::new();
        p.execute(PlayerCommand::Load(SourceHandle::new(1)))
            .unwrap();
        p.execute(PlayerCommand::Play).unwrap();
        assert_eq!(p.commands().len(), 2);

        p.script_event(PlayerEvent::Paused(true));
        assert_eq!(p.drain_events(), vec![PlayerEvent::Paused(true)]);
        assert!(p.drain_events().is_empty());
    }

    #[test]
    fn scripted_failure_is_returned_and_not_recorded() {
        let mut p = RecordingPlayer::new();
        p.fail_next_with(PlayerError::UnsupportedSource);
        assert_eq!(
            p.execute(PlayerCommand::Play),
            Err(PlayerError::UnsupportedSource)
        );
        assert!(p.commands().is_empty());
    }
}
