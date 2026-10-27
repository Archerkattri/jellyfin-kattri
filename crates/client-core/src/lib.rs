//! Rust-owned playback/session domain types shared by the UI and mpv adapter.

pub mod ab_loop;
pub mod audio;
pub mod audio_device;
pub mod audio_filters;
pub mod auto_play;
pub mod cache;
pub mod chapters;
pub mod controls;
pub mod crash_report;
pub mod display_match;
pub mod downloads;
pub mod epg;
pub mod equalizer;
pub mod events;
pub mod focus;
pub mod hdr_policy;
pub mod language_memory;
pub mod live_tv;
pub mod media_info;
pub mod media_info_mpv;
pub mod mpv_options;
pub mod paging;
pub mod photo_grid;
pub mod pip;
pub mod playback_plan;
pub mod playback_reporting;
pub mod playlist;
pub mod profile_match;
pub mod queue;
pub mod queue_store;
pub mod quick_connect_poll;
pub mod remote;
pub mod retry;
pub mod screenshot;
pub mod search;
pub mod segments;
pub mod servers;
pub mod stream_url;
pub mod subtitle_style;
pub mod subtitle_sync;
pub mod subtitles;
pub mod syncplay;
pub mod volume;
pub mod window_state;

use std::time::Duration;

const JELLYFIN_TICKS_PER_SECOND: u128 = 10_000_000;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct SessionGeneration(u64);

impl SessionGeneration {
    pub const fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlaybackPhase {
    Resolving,
    Loading,
    Buffering,
    Playing,
    Paused,
    Seeking,
    SwitchingStream,
    Ended,
    Stopped,
    Failed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlaybackFailure {
    AuthenticationRequired,
    Network,
    UnsupportedMedia,
    VideoOutput,
    AudioOutput,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Transition {
    Seek { resume: PlaybackPhase },
    StreamSwitch { resume: PlaybackPhase },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlaybackSnapshot {
    pub generation: SessionGeneration,
    pub item_id: String,
    pub phase: PlaybackPhase,
    pub position: Duration,
    pub duration: Option<Duration>,
    pub failure: Option<PlaybackFailure>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PlaybackEvent {
    Resolved {
        position: Duration,
        duration: Option<Duration>,
    },
    Playing,
    Paused,
    Buffering,
    SeekStarted {
        target: Duration,
    },
    SeekCompleted {
        actual: Duration,
    },
    StreamSwitchStarted,
    StreamSwitchCompleted,
    Ended,
    Stopped,
    Failed(PlaybackFailure),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GenerationExhausted;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PlaybackMachine {
    next_generation: u64,
    current: Option<PlaybackSnapshot>,
    transition: Option<Transition>,
}

impl PlaybackMachine {
    pub fn begin_load(
        &mut self,
        item_id: impl Into<String>,
    ) -> Result<SessionGeneration, GenerationExhausted> {
        self.next_generation = self
            .next_generation
            .checked_add(1)
            .ok_or(GenerationExhausted)?;

        let generation = SessionGeneration(self.next_generation);
        self.current = Some(PlaybackSnapshot {
            generation,
            item_id: item_id.into(),
            phase: PlaybackPhase::Resolving,
            position: Duration::ZERO,
            duration: None,
            failure: None,
        });
        self.transition = None;
        Ok(generation)
    }

    /// Apply an mpv/session event only to the matching active item.
    /// Late events from an item that has already been replaced are discarded.
    pub fn apply(&mut self, generation: SessionGeneration, event: PlaybackEvent) -> bool {
        let Some(snapshot) = self.current.as_mut() else {
            return false;
        };
        if snapshot.generation != generation || is_terminal(snapshot.phase) {
            return false;
        }

        match event {
            PlaybackEvent::Resolved { position, duration }
                if snapshot.phase == PlaybackPhase::Resolving =>
            {
                snapshot.position = position;
                snapshot.duration = duration;
                snapshot.phase = PlaybackPhase::Loading;
            }
            PlaybackEvent::Playing
                if matches!(
                    snapshot.phase,
                    PlaybackPhase::Loading | PlaybackPhase::Buffering | PlaybackPhase::Paused
                ) =>
            {
                snapshot.phase = PlaybackPhase::Playing;
            }
            PlaybackEvent::Paused
                if matches!(
                    snapshot.phase,
                    PlaybackPhase::Playing | PlaybackPhase::Buffering
                ) =>
            {
                snapshot.phase = PlaybackPhase::Paused;
            }
            PlaybackEvent::Buffering
                if matches!(
                    snapshot.phase,
                    PlaybackPhase::Loading | PlaybackPhase::Playing
                ) =>
            {
                snapshot.phase = PlaybackPhase::Buffering;
            }
            PlaybackEvent::SeekStarted { target }
                if matches!(
                    snapshot.phase,
                    PlaybackPhase::Playing | PlaybackPhase::Paused
                ) =>
            {
                self.transition = Some(Transition::Seek {
                    resume: snapshot.phase,
                });
                snapshot.phase = PlaybackPhase::Seeking;
                snapshot.position = target;
            }
            PlaybackEvent::SeekCompleted { actual } if snapshot.phase == PlaybackPhase::Seeking => {
                let Some(Transition::Seek { resume }) = self.transition.take() else {
                    return false;
                };
                snapshot.phase = resume;
                snapshot.position = actual;
            }
            PlaybackEvent::StreamSwitchStarted
                if matches!(
                    snapshot.phase,
                    PlaybackPhase::Playing | PlaybackPhase::Paused
                ) =>
            {
                self.transition = Some(Transition::StreamSwitch {
                    resume: snapshot.phase,
                });
                snapshot.phase = PlaybackPhase::SwitchingStream;
            }
            PlaybackEvent::StreamSwitchCompleted
                if snapshot.phase == PlaybackPhase::SwitchingStream =>
            {
                let Some(Transition::StreamSwitch { resume }) = self.transition.take() else {
                    return false;
                };
                snapshot.phase = resume;
            }
            PlaybackEvent::Ended => {
                snapshot.phase = PlaybackPhase::Ended;
                self.transition = None;
            }
            PlaybackEvent::Stopped => {
                snapshot.phase = PlaybackPhase::Stopped;
                self.transition = None;
            }
            PlaybackEvent::Failed(failure) => {
                snapshot.phase = PlaybackPhase::Failed;
                snapshot.failure = Some(failure);
                self.transition = None;
            }
            _ => return false,
        }
        true
    }

    /// Update the current item's observed media position from mpv.
    pub fn update_position(&mut self, generation: SessionGeneration, position: Duration) -> bool {
        let Some(snapshot) = self.current.as_mut() else {
            return false;
        };
        if snapshot.generation != generation || is_terminal(snapshot.phase) {
            return false;
        }
        snapshot.position = position;
        true
    }

    pub fn current(&self) -> Option<&PlaybackSnapshot> {
        self.current.as_ref()
    }

    /// PositionTicks uses 10,000,000 ticks per second in Jellyfin's DTOs.
    /// Negative or overflowing values are never emitted.
    pub fn position_ticks(&self, generation: SessionGeneration) -> Option<i64> {
        let snapshot = self.current.as_ref()?;
        if snapshot.generation != generation || snapshot.phase == PlaybackPhase::Resolving {
            return None;
        }
        duration_to_jellyfin_ticks(snapshot.position)
    }
}

pub fn duration_to_jellyfin_ticks(duration: Duration) -> Option<i64> {
    let ticks = u128::from(duration.as_secs())
        .checked_mul(JELLYFIN_TICKS_PER_SECOND)?
        .checked_add(u128::from(duration.subsec_nanos()) / 100)?;
    i64::try_from(ticks).ok()
}

pub fn jellyfin_ticks_to_duration(ticks: i64) -> Option<Duration> {
    let ticks = u64::try_from(ticks).ok()?;
    let seconds = ticks / JELLYFIN_TICKS_PER_SECOND as u64;
    let remaining = ticks % JELLYFIN_TICKS_PER_SECOND as u64;
    Some(Duration::new(seconds, (remaining * 100) as u32))
}

const fn is_terminal(phase: PlaybackPhase) -> bool {
    matches!(
        phase,
        PlaybackPhase::Ended | PlaybackPhase::Stopped | PlaybackPhase::Failed
    )
}
