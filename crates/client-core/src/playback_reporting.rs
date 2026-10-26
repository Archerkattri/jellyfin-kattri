//! Pure scheduler for Jellyfin playback session reports.
//!
//! The scheduler emits the existing Jellyfin request DTOs as actions; it does
//! not perform HTTP requests or depend on a player. `PlaybackReportSessionId`
//! is a client-local lifecycle key (not a Jellyfin body field) and must be
//! unique for each newly loaded playback session.

use std::time::Duration;

use jellyfin_api::{
    JellyfinTicks, PlayMethod, PlaybackOrder, PlaybackProgressInfo, PlaybackStartInfo,
    PlaybackStopInfo, QueueItem, RepeatMode,
};

/// Client-local identity for one playback lifecycle.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PlaybackReportSessionId(u64);

impl PlaybackReportSessionId {
    /// Create an identity from the caller's monotonically increasing session
    /// generation. The value is never serialized into a Jellyfin report.
    pub const fn new(generation: u64) -> Self {
        Self(generation)
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Input to [`PlaybackReportScheduler`]. `at` is monotonic elapsed time from
/// an arbitrary origin; callers should use a monotonic clock and provide the
/// current playback snapshot whenever player state or position changes.
///
/// Keep the Jellyfin DTO inline: snapshots originate from player events, and
/// boxing this field would add a heap allocation to every observed update.
#[allow(clippy::large_enum_variant)]
#[derive(Clone)]
pub enum PlaybackReportInput {
    Snapshot {
        session: PlaybackReportSessionId,
        at: Duration,
        report: PlaybackProgressInfo,
    },
    /// An explicit seek observation. Unlike an ordinary snapshot, a seek
    /// always emits progress immediately and becomes the latest stop position.
    Seek {
        session: PlaybackReportSessionId,
        at: Duration,
        report: PlaybackProgressInfo,
    },
    Stop {
        session: PlaybackReportSessionId,
        item_id: String,
        /// The final player position, when available. `None` falls back to the
        /// most recently observed snapshot position.
        final_position: Option<JellyfinTicks>,
        failed: bool,
    },
}

/// A DTO ready for the caller to send through the existing Jellyfin API.
#[derive(Clone)]
pub enum PlaybackReportAction {
    Started(PlaybackStartInfo),
    Progress(PlaybackProgressInfo),
    Stopped(PlaybackStopInfo),
}

/// Server-side identifiers and playback mode used to construct Jellyfin
/// session reports for one selected media source. This deliberately excludes
/// the client-local lifecycle key, which is assigned when the player actually
/// begins loading the source.
#[derive(Clone, Eq, PartialEq)]
pub struct PlaybackReportMetadata {
    pub item_id: String,
    pub media_source_id: Option<String>,
    pub live_stream_id: Option<String>,
    /// Monotonic time of the user's Tune action; used only for first-frame diagnostics.
    pub live_tune_started_at: Option<std::time::Instant>,
    pub audio_stream_index: Option<i32>,
    pub subtitle_stream_index: Option<i32>,
    pub play_session_id: Option<String>,
    pub play_method: PlayMethod,
    pub can_seek: Option<bool>,
    pub repeat_mode: Option<RepeatMode>,
    pub playback_order: Option<PlaybackOrder>,
    pub now_playing_queue: Option<Vec<QueueItem>>,
}

impl PlaybackReportMetadata {
    /// Construct the report DTO for one player snapshot. Player time is
    /// already converted into Jellyfin's 100-nanosecond tick units.
    pub fn progress_info(
        &self,
        position_ticks: Option<JellyfinTicks>,
        is_paused: Option<bool>,
    ) -> PlaybackProgressInfo {
        let mut report = PlaybackProgressInfo::new(self.item_id.clone());
        report.can_seek = self.can_seek;
        report.media_source_id.clone_from(&self.media_source_id);
        report.live_stream_id.clone_from(&self.live_stream_id);
        report.audio_stream_index = self.audio_stream_index;
        report.subtitle_stream_index = self.subtitle_stream_index;
        report.is_paused = is_paused;
        report.position_ticks = position_ticks;
        report.play_method = Some(self.play_method);
        report.play_session_id.clone_from(&self.play_session_id);
        report.repeat_mode = self.repeat_mode;
        report.playback_order = self.playback_order;
        report.now_playing_queue.clone_from(&self.now_playing_queue);
        report
    }
}

#[derive(Clone)]
struct ActiveReport {
    session: PlaybackReportSessionId,
    item_id: String,
    latest: PlaybackProgressInfo,
    last_observed_at: Duration,
    last_progress_at: Duration,
}

/// Deterministic start/progress/stop scheduler for Jellyfin playback reports.
///
/// The first snapshot starts a lifecycle. A changed session key or item ID
/// closes the prior lifecycle and starts the new one. Progress is emitted at
/// most once per cadence, except that pause/resume state changes are reported
/// immediately. Explicit stop position wins; absent one, the latest snapshot
/// position is used (and omitted if no snapshot supplied one).
pub struct PlaybackReportScheduler {
    cadence: Duration,
    active: Option<ActiveReport>,
    newest_session: Option<PlaybackReportSessionId>,
}

impl PlaybackReportScheduler {
    /// Construct a scheduler with a non-zero progress cadence.
    pub fn new(cadence: Duration) -> Result<Self, InvalidProgressCadence> {
        if cadence.is_zero() {
            return Err(InvalidProgressCadence);
        }
        Ok(Self {
            cadence,
            active: None,
            newest_session: None,
        })
    }

    /// Apply one typed player observation or terminal event and return the
    /// reports that should be sent, in wire order.
    pub fn apply(&mut self, input: PlaybackReportInput) -> Vec<PlaybackReportAction> {
        match input {
            PlaybackReportInput::Snapshot {
                session,
                at,
                report,
            } => self.apply_snapshot(session, at, report, false),
            PlaybackReportInput::Seek {
                session,
                at,
                report,
            } => self.apply_snapshot(session, at, report, true),
            PlaybackReportInput::Stop {
                session,
                item_id,
                final_position,
                failed,
            } => self.apply_stop(session, &item_id, final_position, failed),
        }
    }

    pub const fn cadence(&self) -> Duration {
        self.cadence
    }

    pub fn active_session(&self) -> Option<PlaybackReportSessionId> {
        self.active.as_ref().map(|active| active.session)
    }

    fn apply_snapshot(
        &mut self,
        session: PlaybackReportSessionId,
        at: Duration,
        report: PlaybackProgressInfo,
        explicit_seek: bool,
    ) -> Vec<PlaybackReportAction> {
        let item_id = report.item_id.clone();
        if let Some(active) = &self.active
            && active.session == session
            && active.item_id == item_id
        {
            if at < active.last_observed_at {
                return Vec::new();
            }
            return self.update_active(at, report, explicit_seek);
        }
        if self.newest_session.is_some_and(|newest| session <= newest) {
            // A lifecycle key is unique and monotonic per loaded item. Equal
            // generations with a different item ID cannot distinguish a stale
            // player event from a genuine replacement, so reject them too.
            return Vec::new();
        }

        let mut actions = Vec::with_capacity(2);
        if let Some(active) = self.active.take() {
            actions.push(PlaybackReportAction::Stopped(stop_from(
                &active.latest,
                None,
                false,
            )));
        }

        self.newest_session = Some(session);
        let start: PlaybackStartInfo = report.clone();
        self.active = Some(ActiveReport {
            session,
            item_id,
            latest: report,
            last_observed_at: at,
            last_progress_at: at,
        });
        actions.push(PlaybackReportAction::Started(start));
        actions
    }

    fn update_active(
        &mut self,
        at: Duration,
        report: PlaybackProgressInfo,
        explicit_seek: bool,
    ) -> Vec<PlaybackReportAction> {
        let Some(active) = self.active.as_mut() else {
            return Vec::new();
        };
        let pause_changed = active.latest.is_paused != report.is_paused;
        let cadence_due = at.saturating_sub(active.last_progress_at) >= self.cadence;
        active.last_observed_at = at;
        active.latest = report.clone();
        if explicit_seek || pause_changed || cadence_due {
            active.last_progress_at = at;
            vec![PlaybackReportAction::Progress(report)]
        } else {
            Vec::new()
        }
    }

    fn apply_stop(
        &mut self,
        session: PlaybackReportSessionId,
        item_id: &str,
        final_position: Option<JellyfinTicks>,
        failed: bool,
    ) -> Vec<PlaybackReportAction> {
        let Some(active) = self.active.as_ref() else {
            return Vec::new();
        };
        if active.session != session || active.item_id != item_id {
            return Vec::new();
        }
        let Some(active) = self.active.take() else {
            return Vec::new();
        };
        vec![PlaybackReportAction::Stopped(stop_from(
            &active.latest,
            final_position,
            failed,
        ))]
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidProgressCadence;

fn stop_from(
    latest: &PlaybackProgressInfo,
    final_position: Option<JellyfinTicks>,
    failed: bool,
) -> PlaybackStopInfo {
    let mut stop = PlaybackStopInfo::new(latest.item_id.clone());
    stop.media_source_id = latest.media_source_id.clone();
    stop.position_ticks = final_position.or(latest.position_ticks);
    stop.live_stream_id = latest.live_stream_id.clone();
    stop.play_session_id = latest.play_session_id.clone();
    stop.failed = failed;
    stop.playlist_item_id = latest.playlist_item_id.clone();
    stop.now_playing_queue = latest.now_playing_queue.clone();
    stop
}

#[cfg(test)]
mod tests {
    use super::{
        InvalidProgressCadence, PlaybackReportAction, PlaybackReportInput, PlaybackReportMetadata,
        PlaybackReportScheduler, PlaybackReportSessionId,
    };
    use jellyfin_api::{
        JellyfinTicks, PlayMethod, PlaybackOrder, PlaybackProgressInfo, QueueItem, RepeatMode,
    };
    use std::time::Duration;

    const CADENCE: Duration = Duration::from_secs(10);

    fn session(id: u64) -> PlaybackReportSessionId {
        PlaybackReportSessionId::new(id)
    }

    fn report(item: &str, at_ticks: i64, paused: bool) -> PlaybackProgressInfo {
        let mut report = PlaybackProgressInfo::new(item);
        report.position_ticks = Some(JellyfinTicks::from_ticks(at_ticks));
        report.is_paused = Some(paused);
        report.media_source_id = Some("source".into());
        report.play_session_id = Some("play-session".into());
        report
    }

    fn snapshot(
        scheduler: &mut PlaybackReportScheduler,
        session: PlaybackReportSessionId,
        seconds: u64,
        report: PlaybackProgressInfo,
    ) -> Vec<PlaybackReportAction> {
        scheduler.apply(PlaybackReportInput::Snapshot {
            session,
            at: Duration::from_secs(seconds),
            report,
        })
    }

    fn stop(
        scheduler: &mut PlaybackReportScheduler,
        session: PlaybackReportSessionId,
        item_id: &str,
        final_position: Option<i64>,
        failed: bool,
    ) -> Vec<PlaybackReportAction> {
        scheduler.apply(PlaybackReportInput::Stop {
            session,
            item_id: item_id.into(),
            final_position: final_position.map(JellyfinTicks::from_ticks),
            failed,
        })
    }

    #[test]
    fn rejects_zero_cadence() {
        assert_eq!(
            PlaybackReportScheduler::new(Duration::ZERO).err(),
            Some(InvalidProgressCadence)
        );
        assert_eq!(
            PlaybackReportScheduler::new(CADENCE).unwrap().cadence(),
            CADENCE
        );
    }

    #[test]
    fn report_metadata_builds_server_context_without_client_local_ids() {
        let metadata = PlaybackReportMetadata {
            item_id: "item".into(),
            media_source_id: Some("source".into()),
            live_stream_id: Some("server-live-stream".into()),
            live_tune_started_at: None,
            audio_stream_index: Some(2),
            subtitle_stream_index: Some(4),
            play_session_id: Some("server-session".into()),
            play_method: PlayMethod::Transcode,
            can_seek: Some(true),
            repeat_mode: Some(RepeatMode::All),
            playback_order: Some(PlaybackOrder::Shuffle),
            now_playing_queue: Some(vec![QueueItem::new("first"), QueueItem::new("second")]),
        };

        let report = metadata.progress_info(Some(JellyfinTicks::from_ticks(42)), Some(false));

        assert_eq!(report.item_id, "item");
        assert_eq!(report.media_source_id.as_deref(), Some("source"));
        assert_eq!(report.live_stream_id.as_deref(), Some("server-live-stream"));
        assert_eq!(report.audio_stream_index, Some(2));
        assert_eq!(report.subtitle_stream_index, Some(4));
        assert_eq!(report.play_session_id.as_deref(), Some("server-session"));
        assert_eq!(report.play_method, Some(PlayMethod::Transcode));
        assert_eq!(report.can_seek, Some(true));
        assert_eq!(report.repeat_mode, Some(RepeatMode::All));
        assert_eq!(report.playback_order, Some(PlaybackOrder::Shuffle));
        assert_eq!(
            report.now_playing_queue.as_ref().map(|items| items
                .iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>()),
            Some(vec!["first", "second"])
        );
        assert_eq!(report.position_ticks.map(JellyfinTicks::as_ticks), Some(42));
        assert_eq!(report.is_paused, Some(false));
        assert!(report.playback_start_time_ticks.is_none());
    }

    #[test]
    fn queue_context_is_preserved_in_start_progress_and_stop_reports() {
        let metadata = PlaybackReportMetadata {
            item_id: "track-2".into(),
            media_source_id: None,
            live_stream_id: None,
            live_tune_started_at: None,
            audio_stream_index: None,
            subtitle_stream_index: None,
            play_session_id: None,
            play_method: PlayMethod::DirectPlay,
            can_seek: Some(true),
            repeat_mode: Some(RepeatMode::All),
            playback_order: Some(PlaybackOrder::Shuffle),
            now_playing_queue: Some(vec![QueueItem::new("track-2"), QueueItem::new("track-1")]),
        };
        let mut scheduler = PlaybackReportScheduler::new(CADENCE).expect("valid cadence");
        let session_id = session(44);
        let actions = scheduler.apply(PlaybackReportInput::Snapshot {
            session: session_id,
            at: Duration::ZERO,
            report: metadata.progress_info(None, Some(false)),
        });
        let [PlaybackReportAction::Started(start)] = actions.as_slice() else {
            panic!("first queue snapshot starts playback reporting");
        };
        assert_eq!(start.repeat_mode, Some(RepeatMode::All));
        assert_eq!(start.playback_order, Some(PlaybackOrder::Shuffle));
        assert_eq!(start.now_playing_queue.as_ref().map(Vec::len), Some(2));
        assert!(start.live_stream_id.is_none());

        let actions = scheduler.apply(PlaybackReportInput::Stop {
            session: session_id,
            item_id: "track-2".into(),
            final_position: None,
            failed: false,
        });
        let [PlaybackReportAction::Stopped(stop)] = actions.as_slice() else {
            panic!("queue stop report is emitted");
        };
        assert_eq!(stop.now_playing_queue.as_ref().map(Vec::len), Some(2));
    }

    #[test]
    fn live_stream_id_is_forwarded_in_start_progress_and_stop_reports() {
        let metadata = PlaybackReportMetadata {
            item_id: "channel-1".into(),
            media_source_id: Some("live-source".into()),
            live_stream_id: Some("live-stream-opaque".into()),
            live_tune_started_at: None,
            audio_stream_index: None,
            subtitle_stream_index: None,
            play_session_id: Some("play-session".into()),
            play_method: PlayMethod::Transcode,
            can_seek: Some(false),
            repeat_mode: None,
            playback_order: None,
            now_playing_queue: None,
        };
        let mut scheduler = PlaybackReportScheduler::new(CADENCE).expect("valid cadence");
        let session_id = session(88);
        let started = scheduler.apply(PlaybackReportInput::Snapshot {
            session: session_id,
            at: Duration::ZERO,
            report: metadata.progress_info(None, Some(false)),
        });
        let [PlaybackReportAction::Started(start)] = started.as_slice() else {
            panic!("the first live snapshot starts reporting");
        };
        assert_eq!(start.live_stream_id.as_deref(), Some("live-stream-opaque"));

        let stopped = scheduler.apply(PlaybackReportInput::Stop {
            session: session_id,
            item_id: "channel-1".into(),
            final_position: None,
            failed: false,
        });
        let [PlaybackReportAction::Stopped(stop)] = stopped.as_slice() else {
            panic!("stopping live playback emits a stop report");
        };
        assert_eq!(stop.live_stream_id.as_deref(), Some("live-stream-opaque"));
    }

    #[test]
    fn emits_one_start_progress_and_stop_and_ignores_duplicates() {
        let mut scheduler = PlaybackReportScheduler::new(CADENCE).unwrap();
        let id = session(1);
        assert!(matches!(
            snapshot(&mut scheduler, id, 0, report("item", 0, false)).as_slice(),
            [PlaybackReportAction::Started(_)]
        ));
        assert!(snapshot(&mut scheduler, id, 3, report("item", 30, false)).is_empty());
        assert!(matches!(
            snapshot(&mut scheduler, id, 10, report("item", 100, false)).as_slice(),
            [PlaybackReportAction::Progress(_)]
        ));
        assert!(matches!(
            stop(&mut scheduler, id, "item", None, false).as_slice(),
            [PlaybackReportAction::Stopped(_)]
        ));
        assert!(stop(&mut scheduler, id, "item", None, false).is_empty());
        assert!(snapshot(&mut scheduler, id, 11, report("item", 110, false)).is_empty());
        assert_eq!(scheduler.active_session(), None);
    }

    #[test]
    fn pause_and_resume_each_emit_immediate_progress_snapshot() {
        let mut scheduler = PlaybackReportScheduler::new(CADENCE).unwrap();
        let id = session(1);
        snapshot(&mut scheduler, id, 0, report("item", 0, false));
        assert!(
            matches!(snapshot(&mut scheduler, id, 2, report("item", 20, true)).as_slice(), [PlaybackReportAction::Progress(progress)] if progress.is_paused == Some(true))
        );
        assert!(
            matches!(snapshot(&mut scheduler, id, 3, report("item", 30, false)).as_slice(), [PlaybackReportAction::Progress(progress)] if progress.is_paused == Some(false))
        );
        assert!(snapshot(&mut scheduler, id, 4, report("item", 40, false)).is_empty());
    }

    #[test]
    fn seek_back_to_start_emits_progress_and_stop_uses_seek_position() {
        let mut scheduler = PlaybackReportScheduler::new(CADENCE).unwrap();
        let id = session(1);
        snapshot(&mut scheduler, id, 0, report("item", 1_200_000_000, false));

        let seek_actions = scheduler.apply(PlaybackReportInput::Seek {
            session: id,
            at: Duration::from_secs(2),
            report: report("item", 0, false),
        });
        assert!(matches!(
            seek_actions.as_slice(),
            [PlaybackReportAction::Progress(progress)]
                if progress.position_ticks == Some(JellyfinTicks::from_ticks(0))
        ));

        assert!(matches!(
            stop(&mut scheduler, id, "item", None, false).as_slice(),
            [PlaybackReportAction::Stopped(stopped)]
                if stopped.position_ticks == Some(JellyfinTicks::from_ticks(0))
        ));
    }

    #[test]
    fn item_replacement_stops_old_then_starts_new() {
        let mut scheduler = PlaybackReportScheduler::new(CADENCE).unwrap();
        snapshot(&mut scheduler, session(1), 0, report("first", 0, false));
        let actions = snapshot(&mut scheduler, session(2), 1, report("second", 10, false));
        assert!(
            matches!(actions.as_slice(), [PlaybackReportAction::Stopped(stopped), PlaybackReportAction::Started(started)] if stopped.item_id == "first" && stopped.position_ticks == Some(JellyfinTicks::from_ticks(0)) && started.item_id == "second")
        );
    }

    #[test]
    fn rejects_same_generation_item_replacement_and_late_previous_item_snapshot() {
        let mut scheduler = PlaybackReportScheduler::new(CADENCE).unwrap();
        let first = session(1);
        snapshot(&mut scheduler, first, 0, report("first", 0, false));
        assert!(snapshot(&mut scheduler, first, 1, report("second", 10, false)).is_empty());

        let second = session(2);
        let actions = snapshot(&mut scheduler, second, 2, report("second", 20, false));
        assert!(matches!(
            actions.as_slice(),
            [
                PlaybackReportAction::Stopped(_),
                PlaybackReportAction::Started(_)
            ]
        ));
        assert!(snapshot(&mut scheduler, first, 3, report("first", 30, false)).is_empty());
        assert_eq!(scheduler.active_session(), Some(second));
    }

    #[test]
    fn session_replacement_stops_previous_even_for_same_item() {
        let mut scheduler = PlaybackReportScheduler::new(CADENCE).unwrap();
        snapshot(&mut scheduler, session(1), 0, report("item", 0, false));
        let actions = snapshot(&mut scheduler, session(2), 1, report("item", 10, false));
        assert!(
            matches!(actions.as_slice(), [PlaybackReportAction::Stopped(stopped), PlaybackReportAction::Started(started)] if stopped.item_id == "item" && started.item_id == "item")
        );
    }

    #[test]
    fn explicit_final_position_wins_and_stop_preserves_known_dto_context() {
        let mut scheduler = PlaybackReportScheduler::new(CADENCE).unwrap();
        let id = session(1);
        snapshot(&mut scheduler, id, 0, report("item", 12, false));
        assert!(
            matches!(stop(&mut scheduler, id, "item", Some(0), true).as_slice(), [PlaybackReportAction::Stopped(stopped)] if stopped.position_ticks == Some(JellyfinTicks::from_ticks(0)) && stopped.failed && stopped.media_source_id.as_deref() == Some("source") && stopped.play_session_id.as_deref() == Some("play-session"))
        );
    }

    #[test]
    fn absent_final_position_uses_latest_observation_or_omits_it() {
        let mut scheduler = PlaybackReportScheduler::new(CADENCE).unwrap();
        let id = session(1);
        snapshot(&mut scheduler, id, 0, report("known", 42, false));
        assert!(
            matches!(stop(&mut scheduler, id, "known", None, false).as_slice(), [PlaybackReportAction::Stopped(stopped)] if stopped.position_ticks == Some(JellyfinTicks::from_ticks(42)))
        );

        snapshot(
            &mut scheduler,
            session(2),
            1,
            PlaybackProgressInfo::new("unknown"),
        );
        assert!(
            matches!(stop(&mut scheduler, session(2), "unknown", None, false).as_slice(), [PlaybackReportAction::Stopped(stopped)] if stopped.position_ticks.is_none())
        );
    }

    #[test]
    fn stale_session_stop_and_out_of_order_snapshot_are_ignored() {
        let mut scheduler = PlaybackReportScheduler::new(CADENCE).unwrap();
        let id = session(2);
        snapshot(&mut scheduler, id, 5, report("item", 50, false));
        snapshot(
            &mut scheduler,
            session(3),
            6,
            report("replacement", 60, false),
        );
        assert!(snapshot(&mut scheduler, session(2), 7, report("item", 70, false)).is_empty());
        assert_eq!(scheduler.active_session(), Some(session(3)));
        assert!(stop(&mut scheduler, session(1), "item", Some(1), false).is_empty());
        assert!(
            snapshot(
                &mut scheduler,
                session(3),
                5,
                report("replacement", 40, false)
            )
            .is_empty()
        );
        assert!(
            matches!(stop(&mut scheduler, session(3), "replacement", None, false).as_slice(), [PlaybackReportAction::Stopped(stopped)] if stopped.position_ticks == Some(JellyfinTicks::from_ticks(60)))
        );
    }
}
