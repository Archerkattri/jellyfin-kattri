//! Async VLC-style subtitle-sync calibration on the libmpv owner thread.

use std::{
    collections::{HashMap, HashSet},
    time::Duration,
};

use kattri_client_core::subtitle_sync::{
    CalibrationMarker, CaptureOutcome, SignedDuration, SubtitleSyncCalibration,
};

use crate::{
    EventKind, MpvError, MpvEvent, MpvSession, PlaybackProperty, PlaybackPropertyValue,
    SubtitleDelayError,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PendingRead {
    Marker(CalibrationMarker),
    CurrentDelay(SignedDuration),
    ConfirmDelay,
}

/// Results for rendering a nonblocking subtitle-sync control surface.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SubtitleSyncUpdate {
    /// One marker was sampled; wait for the other one.
    AwaitingOtherMarker {
        marker: CalibrationMarker,
        replaced_previous: bool,
    },
    /// The target delay command was accepted; a readback is pending.
    Applying {
        correction_seconds: f64,
        target_delay_seconds: f64,
    },
    /// mpv's post-command `sub-delay` property readback.
    Confirmed { delay_seconds: f64 },
    /// A capture/read/apply stage could not complete.
    Failed(SubtitleSyncFailure),
}

/// Failure stage to present in the playback UI without leaking media details.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubtitleSyncFailure {
    PositionUnavailable {
        marker: CalibrationMarker,
        mpv_error: Option<i32>,
    },
    CurrentDelayUnavailable {
        mpv_error: Option<i32>,
    },
    DelayCommandRejected {
        mpv_error: Option<i32>,
    },
    DelayConfirmationUnavailable {
        mpv_error: Option<i32>,
    },
    DelayOutOfRange,
}

/// Coordinates fresh media-clock reads and nonblocking `sub-delay` updates.
///
/// Call [`Self::capture_marker`] from the two user-facing sync buttons and
/// pass each libmpv event to [`Self::process_event`]. Call [`Self::reset`] when
/// seeking, changing the active item, or switching subtitle tracks. Reset
/// discards pending reads; it cannot cancel an mpv command already accepted by
/// libmpv's owner thread.
#[derive(Default)]
pub struct SubtitleSyncController {
    calibration: SubtitleSyncCalibration,
    pending_reads: HashMap<u64, PendingRead>,
    latest_marker_reads: HashMap<CalibrationMarker, u64>,
    pending_commands: HashSet<u64>,
}

impl SubtitleSyncController {
    pub fn new() -> Self {
        Self::default()
    }

    /// Queue an owner-thread read of the current playback clock for one marker.
    ///
    /// This call never waits for libmpv. The returned request ID is consumed
    /// internally by [`Self::process_event`]. A later tap of the same marker
    /// supersedes its earlier outstanding sample.
    pub fn capture_marker(
        &mut self,
        marker: CalibrationMarker,
        session: &MpvSession,
    ) -> Result<u64, MpvError> {
        let request_id = session.request_time_position()?;
        if let Some(previous) = self.latest_marker_reads.insert(marker, request_id) {
            self.pending_reads.remove(&previous);
        }
        self.pending_reads
            .insert(request_id, PendingRead::Marker(marker));
        Ok(request_id)
    }

    /// Queue a relative subtitle-delay adjustment, such as the UI's 50 ms
    /// nudge buttons. The eventual delay readback arrives from
    /// [`Self::process_event`].
    pub fn nudge_subtitle_delay(
        &mut self,
        delta_seconds: f64,
        session: &MpvSession,
    ) -> Result<(), SubtitleDelayError> {
        let command_id = session.nudge_subtitle_delay(delta_seconds)?;
        self.pending_commands.insert(command_id.get());
        Ok(())
    }

    /// Queue a reset to zero and asynchronously read back the resulting delay.
    pub fn reset_subtitle_delay(&mut self, session: &MpvSession) -> Result<(), SubtitleDelayError> {
        let command_id = session.reset_subtitle_delay()?;
        self.pending_commands.insert(command_id.get());
        Ok(())
    }

    /// Discard partial marker pairs and all outstanding read/apply tracking.
    /// A delay command already queued with libmpv itself cannot be retracted.
    pub fn reset(&mut self) {
        self.calibration.reset();
        self.pending_reads.clear();
        self.latest_marker_reads.clear();
        self.pending_commands.clear();
    }

    pub fn is_calibrating(&self) -> bool {
        self.calibration.is_pending()
            || !self.latest_marker_reads.is_empty()
            || !self.pending_reads.is_empty()
            || !self.pending_commands.is_empty()
    }

    /// Consume an mpv event relevant to this calibration, if any.
    pub fn process_event(
        &mut self,
        event: MpvEvent,
        session: &MpvSession,
    ) -> Option<SubtitleSyncUpdate> {
        match event.kind {
            EventKind::PropertyReadReply { property, value } => {
                let purpose = self.pending_reads.remove(&event.request_id)?;
                match purpose {
                    PendingRead::Marker(marker) => {
                        if self.latest_marker_reads.get(&marker) != Some(&event.request_id) {
                            return None;
                        }
                        self.latest_marker_reads.remove(&marker);
                        let media_time = if property == PlaybackProperty::TimePosition {
                            time_position(event.error, value)
                        } else {
                            None
                        };
                        let Some(media_time) = media_time else {
                            self.calibration.reset();
                            return Some(SubtitleSyncUpdate::Failed(
                                SubtitleSyncFailure::PositionUnavailable {
                                    marker,
                                    mpv_error: (event.error < 0).then_some(event.error),
                                },
                            ));
                        };
                        self.handle_marker(marker, media_time, session)
                    }
                    PendingRead::CurrentDelay(correction) => {
                        self.handle_current_delay(event.error, property, value, correction, session)
                    }
                    PendingRead::ConfirmDelay => {
                        if event.error < 0 || property != PlaybackProperty::SubtitleDelay {
                            return Some(SubtitleSyncUpdate::Failed(
                                SubtitleSyncFailure::DelayConfirmationUnavailable {
                                    mpv_error: (event.error < 0).then_some(event.error),
                                },
                            ));
                        }
                        match value {
                            PlaybackPropertyValue::SubtitleDelay(Some(delay_seconds))
                                if delay_seconds.is_finite() =>
                            {
                                Some(SubtitleSyncUpdate::Confirmed { delay_seconds })
                            }
                            _ => Some(SubtitleSyncUpdate::Failed(
                                SubtitleSyncFailure::DelayConfirmationUnavailable {
                                    mpv_error: None,
                                },
                            )),
                        }
                    }
                }
            }
            EventKind::CommandReply => {
                self.pending_commands
                    .remove(&event.request_id)
                    .then_some(())?;
                if event.error < 0 {
                    return Some(SubtitleSyncUpdate::Failed(
                        SubtitleSyncFailure::DelayCommandRejected {
                            mpv_error: Some(event.error),
                        },
                    ));
                }
                self.queue_delay_confirmation(session)
            }
            EventKind::CommandSubmissionFailed => {
                if !self.pending_commands.remove(&event.request_id) {
                    return None;
                }
                Some(SubtitleSyncUpdate::Failed(
                    SubtitleSyncFailure::DelayCommandRejected {
                        mpv_error: (event.error < 0).then_some(event.error),
                    },
                ))
            }
            _ => None,
        }
    }

    fn handle_marker(
        &mut self,
        marker: CalibrationMarker,
        media_time: Duration,
        session: &MpvSession,
    ) -> Option<SubtitleSyncUpdate> {
        let outcome = match marker {
            CalibrationMarker::VoiceHeard => self.calibration.capture_voice_heard(media_time),
            CalibrationMarker::TextSeen => self.calibration.capture_text_seen(media_time),
        };
        match outcome {
            CaptureOutcome::AwaitingOtherMarker {
                captured,
                replaced_previous,
            } => Some(SubtitleSyncUpdate::AwaitingOtherMarker {
                marker: captured,
                replaced_previous,
            }),
            CaptureOutcome::Calibrated(correction) => {
                let request_id = match session.request_property(PlaybackProperty::SubtitleDelay) {
                    Ok(request_id) => request_id,
                    Err(_) => {
                        return Some(SubtitleSyncUpdate::Failed(
                            SubtitleSyncFailure::CurrentDelayUnavailable { mpv_error: None },
                        ));
                    }
                };
                self.pending_reads
                    .insert(request_id, PendingRead::CurrentDelay(correction));
                None
            }
        }
    }

    fn handle_current_delay(
        &mut self,
        mpv_error: i32,
        property: PlaybackProperty,
        value: PlaybackPropertyValue,
        correction: SignedDuration,
        session: &MpvSession,
    ) -> Option<SubtitleSyncUpdate> {
        if mpv_error < 0 || property != PlaybackProperty::SubtitleDelay {
            return Some(SubtitleSyncUpdate::Failed(
                SubtitleSyncFailure::CurrentDelayUnavailable {
                    mpv_error: (mpv_error < 0).then_some(mpv_error),
                },
            ));
        }
        let PlaybackPropertyValue::SubtitleDelay(Some(current_delay)) = value else {
            return Some(SubtitleSyncUpdate::Failed(
                SubtitleSyncFailure::CurrentDelayUnavailable { mpv_error: None },
            ));
        };
        if !current_delay.is_finite() {
            return Some(SubtitleSyncUpdate::Failed(
                SubtitleSyncFailure::CurrentDelayUnavailable { mpv_error: None },
            ));
        }

        let correction_seconds = correction.as_seconds_f64();
        let target_delay_seconds = current_delay + correction_seconds;
        if !target_delay_seconds.is_finite() {
            return Some(SubtitleSyncUpdate::Failed(
                SubtitleSyncFailure::DelayOutOfRange,
            ));
        }
        let command_id = match session.set_subtitle_delay(target_delay_seconds) {
            Ok(command_id) => command_id,
            Err(_) => {
                return Some(SubtitleSyncUpdate::Failed(
                    SubtitleSyncFailure::DelayCommandRejected { mpv_error: None },
                ));
            }
        };
        self.pending_commands.insert(command_id.get());
        Some(SubtitleSyncUpdate::Applying {
            correction_seconds,
            target_delay_seconds,
        })
    }

    fn queue_delay_confirmation(&mut self, session: &MpvSession) -> Option<SubtitleSyncUpdate> {
        let request_id = match session.request_property(PlaybackProperty::SubtitleDelay) {
            Ok(request_id) => request_id,
            Err(_) => {
                return Some(SubtitleSyncUpdate::Failed(
                    SubtitleSyncFailure::DelayConfirmationUnavailable { mpv_error: None },
                ));
            }
        };
        self.pending_reads
            .insert(request_id, PendingRead::ConfirmDelay);
        None
    }
}

fn time_position(error: i32, value: PlaybackPropertyValue) -> Option<Duration> {
    if error < 0 {
        return None;
    }
    let PlaybackPropertyValue::TimePosition(Some(seconds)) = value else {
        return None;
    };
    Duration::try_from_secs_f64(seconds).ok()
}
