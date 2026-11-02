use std::time::{Duration, Instant};

const RETRY_DELAY: Duration = Duration::from_millis(500);

/// Only observed player state may declare an entry ready or buffering.
#[derive(Clone, Copy, Default)]
pub(crate) struct PlaybackSample {
    pub generation: Option<u64>,
    pub loaded: bool,
    pub restarted: bool,
    pub resume_pending: bool,
    pub cache_paused: Option<bool>,
    pub paused: Option<bool>,
    pub position_ticks: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PlaybackReport {
    pub queue_generation: u64,
    pub playlist_item_id: String,
    pub buffering: bool,
    pub position_ticks: u64,
    pub is_playing: bool,
}

struct Target {
    playback_generation: u64,
    queue_generation: u64,
    playlist_item_id: String,
    last_sent: Option<bool>,
    retry_at: Option<Instant>,
    in_flight: Option<(u64, PlaybackReport)>,
}

#[derive(Default)]
pub(crate) struct SyncPlaybackBridge {
    target: Option<Target>,
}

impl SyncPlaybackBridge {
    pub fn capture(&mut self, playback_generation: u64, queue: Option<(u64, String)>) {
        self.target = queue.map(|(queue_generation, playlist_item_id)| Target {
            playback_generation,
            queue_generation,
            playlist_item_id,
            last_sent: None,
            retry_at: None,
            in_flight: None,
        });
    }

    pub fn clear(&mut self) {
        self.target = None;
    }

    pub fn end(&mut self, generation: Option<u64>) {
        if self
            .target
            .as_ref()
            .is_some_and(|target| Some(target.playback_generation) == generation)
        {
            self.clear();
        }
    }

    pub fn poll(&mut self, sample: PlaybackSample, now: Instant) -> Option<PlaybackReport> {
        let target = self.target.as_mut()?;
        if target.in_flight.is_some() {
            return None;
        }
        if sample.generation != Some(target.playback_generation) || !sample.loaded {
            return None;
        }
        let buffering = sample.cache_paused == Some(true);
        if !buffering && (!sample.restarted || sample.resume_pending) {
            return None;
        }
        let paused = sample.paused?;
        let position_ticks = sample.position_ticks?;
        if target.last_sent == Some(buffering) || target.retry_at.is_some_and(|due| now < due) {
            return None;
        }
        // A full worker queue or unavailable clock is retried rather than silently accepted.
        target.retry_at = Some(now + RETRY_DELAY);
        Some(PlaybackReport {
            queue_generation: target.queue_generation,
            playlist_item_id: target.playlist_item_id.clone(),
            buffering,
            position_ticks,
            is_playing: !paused && !buffering,
        })
    }

    pub fn submitted(&mut self, request_id: u64, report: PlaybackReport) {
        if let Some(target) = self.target.as_mut()
            && target.queue_generation == report.queue_generation
            && target.playlist_item_id == report.playlist_item_id
        {
            target.retry_at = None;
            target.in_flight = Some((request_id, report));
        }
    }

    pub fn completed(&mut self, request_id: u64, accepted: bool, now: Instant) {
        if let Some(target) = self.target.as_mut()
            && target
                .in_flight
                .as_ref()
                .is_some_and(|(id, _)| *id == request_id)
            && let Some((_, report)) = target.in_flight.take()
        {
            if accepted {
                target.last_sent = Some(report.buffering);
                target.retry_at = None;
            } else {
                target.retry_at = Some(now + RETRY_DELAY);
            }
        }
    }

    pub fn retry_at(&self) -> Option<Instant> {
        self.target.as_ref()?.retry_at
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(generation: u64) -> PlaybackSample {
        PlaybackSample {
            generation: Some(generation),
            loaded: true,
            restarted: true,
            resume_pending: false,
            cache_paused: Some(false),
            paused: Some(true),
            position_ticks: Some(10_000_000),
        }
    }

    #[test]
    fn ready_requires_the_current_open_and_completed_resume() {
        let mut bridge = SyncPlaybackBridge::default();
        bridge.capture(4, Some((9, "entry-b".into())));
        let now = Instant::now();
        assert!(bridge.poll(sample(3), now).is_none());
        assert!(
            bridge
                .poll(
                    PlaybackSample {
                        restarted: false,
                        ..sample(4)
                    },
                    now
                )
                .is_none()
        );
        assert!(
            bridge
                .poll(
                    PlaybackSample {
                        resume_pending: true,
                        ..sample(4)
                    },
                    now
                )
                .is_none()
        );
        let ready = bridge
            .poll(sample(4), now)
            .expect("ready after restart even while user-paused");
        assert!(!ready.buffering);
        assert!(!ready.is_playing);
        assert_eq!(ready.queue_generation, 9);
        assert_eq!(ready.playlist_item_id, "entry-b");
    }

    #[test]
    fn rejected_status_retries_and_cache_transitions_are_coalesced() {
        let mut bridge = SyncPlaybackBridge::default();
        bridge.capture(4, Some((9, "entry-b".into())));
        let now = Instant::now();
        let first = bridge.poll(sample(4), now).expect("initial ready");
        assert!(bridge.poll(sample(4), now + RETRY_DELAY / 2).is_none());
        let retried = bridge.poll(sample(4), now + RETRY_DELAY).expect("retry");
        assert_eq!(first, retried);
        bridge.submitted(1, retried);
        assert!(bridge.poll(sample(4), now + RETRY_DELAY * 2).is_none());
        bridge.completed(1, true, now + RETRY_DELAY);
        assert!(bridge.poll(sample(4), now + RETRY_DELAY * 2).is_none());
        let buffering_sample = PlaybackSample {
            cache_paused: Some(true),
            paused: Some(false),
            ..sample(4)
        };
        let buffering = bridge
            .poll(buffering_sample, now + RETRY_DELAY * 2)
            .expect("real buffer");
        assert!(buffering.buffering);
        assert!(!buffering.is_playing);
        bridge.submitted(2, buffering);
        bridge.completed(2, true, now + RETRY_DELAY * 2);
        assert!(
            bridge
                .poll(buffering_sample, now + RETRY_DELAY * 3)
                .is_none()
        );
        let ready = bridge
            .poll(sample(4), now + RETRY_DELAY * 3)
            .expect("buffer recovered");
        assert!(!ready.buffering);
    }

    #[test]
    fn old_end_and_acknowledgement_cannot_change_a_new_entry() {
        let mut bridge = SyncPlaybackBridge::default();
        bridge.capture(4, Some((9, "entry-b".into())));
        let now = Instant::now();
        let old = bridge.poll(sample(4), now).expect("old status");
        bridge.submitted(1, old);
        bridge.capture(5, Some((10, "entry-c".into())));
        bridge.end(Some(4));
        bridge.completed(1, true, now);
        assert_eq!(
            bridge
                .poll(sample(5), now)
                .expect("new status")
                .playlist_item_id,
            "entry-c"
        );
        bridge.end(Some(5));
        assert!(bridge.poll(sample(5), now + RETRY_DELAY).is_none());
    }

    #[test]
    fn unrelated_local_playback_clears_the_group_target() {
        let mut bridge = SyncPlaybackBridge::default();
        bridge.capture(4, Some((9, "entry-b".into())));
        bridge.capture(5, None);
        assert!(bridge.poll(sample(5), Instant::now()).is_none());
    }

    #[test]
    fn http_failure_is_retried_without_duplicating_an_in_flight_request() {
        let mut bridge = SyncPlaybackBridge::default();
        bridge.capture(4, Some((9, "entry-b".into())));
        let now = Instant::now();
        let report = bridge.poll(sample(4), now).expect("first ready");
        bridge.submitted(30, report.clone());
        assert!(bridge.retry_at().is_none());
        assert!(bridge.poll(sample(4), now + RETRY_DELAY * 10).is_none());
        bridge.completed(29, true, now);
        assert!(bridge.poll(sample(4), now).is_none());
        bridge.completed(30, false, now);
        assert!(bridge.poll(sample(4), now + RETRY_DELAY / 2).is_none());
        assert_eq!(bridge.poll(sample(4), now + RETRY_DELAY), Some(report));
    }
}
