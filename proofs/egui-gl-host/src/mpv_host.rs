use std::{
    ffi::OsString,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use egui::Key;
use jellyfin_api::JellyfinTicks;
use jellyfin_api::{DeviceProfile, device_profile_from_runtime_inventory};
use jellyfin_egui_client::PlaybackReportSubmitError;
use jellyfin_playback_relay::{RelayHandle, RelayPlaybackUrl};
use kattri_client_core::ab_loop::{AbLoop, AbLoopCycle, AbLoopError, plan_frame_step};
use kattri_client_core::audio::gapless_audio_option;
use kattri_client_core::cache::{BufferSettings, demuxer_options};
use kattri_client_core::controls::{PlaybackSpeed, SeekRequest, seek_target};
use kattri_client_core::language_memory::{
    AudioChoice, SeriesLanguageMemory, SubtitleChoice, TrackLanguage,
};
use kattri_client_core::media_info::{MediaInfo, MediaInput};
use kattri_client_core::media_info_mpv::media_input_from_mpv;
use kattri_client_core::mpv_options::{
    MpvOptionError, OptionValue, ValidatedOption, aspect_options, audio_channels_for_layout,
    audio_filter_options, default_speed_for_setting, framedrop_for_setting, hwdec_for_decode_mode,
    placement_options, subtitle_ass_options, subtitle_font_and_border_options,
    subtitle_size_and_color_options, validate, validate_audio_track, validate_subtitle_track,
    video_sync_for_sync_mode, zoom_reset_options,
};
use kattri_client_core::playback_reporting::{
    PlaybackReportAction, PlaybackReportInput, PlaybackReportMetadata, PlaybackReportScheduler,
    PlaybackReportSessionId,
};
use kattri_client_core::remote::LocalAction as RemoteAction;
use kattri_client_core::segments::{MediaSegment, SegmentSkipper};
use kattri_client_core::stream_url::StreamUrl;
use kattri_client_core::subtitle_style::SubtitleStyle;
use kattri_client_core::subtitle_sync::{OffsetDirection, OffsetStep, SubtitleOffset};
use kattri_client_core::subtitles::{
    ReselectObservation, SecondaryRefusal, SecondarySelection, SelectionOrigin,
    SubtitleReselectGate, SubtitleTrackId, choose_secondary_in_inventory, reconcile_secondary,
    reselect_sid_writes, secondary_candidates,
};
use kattri_client_core::volume::clamp_volume;
use kattri_player::{
    ProbeValue, ProbeWarning, RuntimeCapabilitySnapshot, RuntimeIdentity,
    cache_runtime_capabilities, cached_runtime_capabilities,
};
use kattri_settings::{
    AudioSettings, DecodeMode, PlaybackSettings, SubtitleSettings, VideoSettings,
};
use mpv_render_proof::playback_policy::OptionWriteLedger;
use mpv_render_proof::{
    BackendSelection, CalibrationMarker, CapabilityUnavailable, CapabilityValue, ChapterStep,
    EndFileReason, EventKind, FboTarget, FrameDecision, MpvError, MpvRenderer, MpvSession,
    MpvTrack, ObservationId, PlaybackProperty, PlaybackPropertyValue, RepaintNotifier,
    SubtitleDelayError, SubtitleSelectionError, SubtitleSyncController, SubtitleSyncFailure,
    SubtitleSyncUpdate, TrackInventory, TrackInventoryError, TrackKind, TrackSelection,
    TrackSelectionError, TrackSelectionState,
};
use winit::event_loop::EventLoopProxy;

use crate::gl_adapter::HostGlContextProvider;
use crate::media_info_panel::MediaInfoState;
use crate::player_controls::{SecondarySubtitleChoice, TrackChoice};
use crate::report_handoff::{ReportCounts, ReportHandoff};

pub(crate) const SYNC_MARKER_BUTTONS: [(CalibrationMarker, &str); 2] = [
    (CalibrationMarker::VoiceHeard, "I hear the spoken line"),
    (CalibrationMarker::TextSeen, "I see the subtitle"),
];

const PLAYBACK_REPORT_CADENCE: Duration = Duration::from_secs(10);
const MAX_CONTROL_CHAPTERS: usize = 128;

/// Validation can disable reporting before playback starts. An invalid cadence also leaves
/// reporting off with a warning rather than panicking.
fn playback_report_scheduler(enabled: bool) -> Option<PlaybackReportScheduler> {
    if !enabled {
        return None;
    }
    match PlaybackReportScheduler::new(PLAYBACK_REPORT_CADENCE) {
        Ok(scheduler) => Some(scheduler),
        Err(error) => {
            tracing::warn!(
                ?error,
                "playback reporting is off: invalid progress cadence"
            );
            None
        }
    }
}

/// The mpv value for a track menu choice: `auto`, `no`, or the track id.
fn track_selection_value(selection: TrackSelection) -> OptionValue<'static> {
    match selection {
        TrackSelection::Automatic => OptionValue::Text("auto"),
        TrackSelection::Disabled => OptionValue::Text("no"),
        TrackSelection::Track(id) => OptionValue::Int(id.get()),
    }
}

/// Check an audio track selection against the `aid` allowlist entry and the current inventory.
/// The session also checks the inventory; this keeps the allowlist on the write path.
fn audio_track_gate(
    selection: TrackSelection,
    inventory: &TrackInventory,
) -> Result<(), MpvOptionError> {
    let audio_ids: Vec<i64> = inventory
        .audio_tracks()
        .map(|track| track.id.get())
        .collect();
    validate_audio_track(track_selection_value(selection), &audio_ids).map(|_| ())
}

/// Check a subtitle track selection against the `sid` allowlist entry and the current inventory.
/// The write goes through the same gate as `aid`, so no `sid` value reaches libmpv unchecked.
fn subtitle_track_gate(
    selection: TrackSelection,
    inventory: &TrackInventory,
) -> Result<(), MpvOptionError> {
    let subtitle_ids: Vec<i64> = inventory
        .subtitle_tracks()
        .map(|track| track.id.get())
        .collect();
    validate_subtitle_track(track_selection_value(selection), &subtitle_ids).map(|_| ())
}

/// Why an `hwdec` write was not made for a decode mode (M2-22). The text names no value.
#[derive(Debug, PartialEq, Eq)]
enum HwdecRefusal {
    /// The mode has no mpv value, so the allowlist is never asked.
    NoMpvValue,
    /// The allowlist refused the mapped value.
    Rejected(MpvOptionError),
}

impl std::fmt::Display for HwdecRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoMpvValue => f.write_str("the decode mode has no mpv hwdec value"),
            Self::Rejected(error) => write!(f, "{error}"),
        }
    }
}

/// The checked `hwdec` write for a mapped decode mode. `None` is a mode with no mpv value.
fn hwdec_write(mapped: Option<&str>) -> Result<ValidatedOption, HwdecRefusal> {
    let value = mapped.ok_or(HwdecRefusal::NoMpvValue)?;
    validate("hwdec", OptionValue::Text(value)).map_err(HwdecRefusal::Rejected)
}

/// Queue `hwdec` for the file about to load (M2-22). mpv keeps `hwdec` across files, so a refused
/// mode falls back to the setting default; otherwise the previous file's value would carry over.
fn queue_hwdec(session: &MpvSession, mode: DecodeMode) {
    let option = match hwdec_write(hwdec_for_decode_mode(mode)) {
        Ok(option) => option,
        Err(refusal) => {
            tracing::warn!(
                setting = "video.decode_mode",
                %refusal,
                "hwdec refused; using the default"
            );
            match hwdec_write(hwdec_for_decode_mode(DecodeMode::Auto)) {
                Ok(option) => option,
                Err(error) => {
                    tracing::warn!(setting = "video.decode_mode", %error, "hwdec default was not queued");
                    return;
                }
            }
        }
    };
    if let Err(error) = session.command(&["set", option.name, &option.value]) {
        tracing::warn!(setting = "video.decode_mode", %error, "hwdec was not queued");
    }
}

/// The enqueue step the M2-21 re-select and its restore need. `MpvSession` implements it. The
/// tests use a scripted queue, so a refused write can be forced without libmpv.
trait CommandQueue {
    fn enqueue(&self, arguments: &[&str]) -> Result<(), MpvError>;
}

impl CommandQueue for MpvSession {
    fn enqueue(&self, arguments: &[&str]) -> Result<(), MpvError> {
        self.command(arguments).map(|_| ())
    }
}

/// Queue validated settings writes in order. A refused or unqueued write is logged by setting key
/// only, so a path or font name never reaches the log through an error.
fn queue_validated(
    session: &MpvSession,
    setting: &'static str,
    options: Result<Vec<ValidatedOption>, MpvOptionError>,
) {
    let options = match options {
        Ok(options) => options,
        Err(error) => {
            tracing::warn!(setting, %error, "setting rejected by the mpv allowlist");
            return;
        }
    };
    for option in options {
        if let Err(error) = session.command(&["set", option.name, &option.value]) {
            tracing::warn!(setting, %error, "setting was not queued");
        }
    }
}

/// The shortest time between two equalizer or filter writes while a slider is dragged.
pub(crate) const AUDIO_FILTER_APPLY_GAP: Duration = Duration::from_millis(150);

/// The mpv write for a remote volume command. It goes through the allowlist, so `volume`
/// cannot take a value outside its range even if the caller's clamp is wrong.
fn remote_volume_write(percent: u32) -> Result<ValidatedOption, MpvOptionError> {
    validate("volume", OptionValue::Int(i64::from(percent)))
}

/// Video layout writes from `video.aspect` and `video.allow_zoom` (M2-20). Both load paths queue
/// them before loadfile, so a file starts from the saved aspect and not from the last file's state.
fn video_layout_options(
    video: &VideoSettings,
) -> [(&'static str, Result<Vec<ValidatedOption>, MpvOptionError>); 2] {
    [
        ("video.aspect", aspect_options(video.aspect)),
        ("video.allow_zoom", zoom_reset_options()),
    ]
}

/// Subtitle placement, size, and colour writes (M2-20). Both load paths queue these before
/// loadfile, so a local file starts with the same placement, size, and colour as a stream.
fn subtitle_layout_options(
    subtitles: &SubtitleSettings,
) -> [(&'static str, Result<Vec<ValidatedOption>, MpvOptionError>); 4] {
    [
        (
            "subtitles.ass",
            subtitle_ass_options(
                subtitles.ass_scale_border_and_shadow,
                subtitles.ass_style_override.as_deref(),
            ),
        ),
        (
            "subtitles.font_and_border",
            subtitle_font_and_border_options(&subtitles.font, subtitles.border_size),
        ),
        (
            "subtitles.placement",
            placement_options(subtitles.placement.as_deref()),
        ),
        (
            "subtitles.size_and_color",
            subtitle_size_and_color_options(subtitles.size, subtitles.color.as_deref()),
        ),
    ]
}

/// The layout writes the local-file path queues before loadfile: video aspect and zoom, then
/// subtitle placement, size, and colour. The same builders feed `load_relay`.
fn local_file_layout_options(
    video: &VideoSettings,
    subtitles: &SubtitleSettings,
) -> Vec<(&'static str, Result<Vec<ValidatedOption>, MpvOptionError>)> {
    video_layout_options(video)
        .into_iter()
        .chain(subtitle_layout_options(subtitles))
        .collect()
}

/// Settings-driven writes that mpv reads when a file opens (M2-06, M2-07, M2-16, M2-22). Each value
/// comes from a builder that checks its allowlist entry, so a refused value is reported and not sent.
fn pre_load_options(
    audio: &AudioSettings,
    playback: &PlaybackSettings,
    video: &VideoSettings,
) -> Vec<(&'static str, Result<Vec<ValidatedOption>, MpvOptionError>)> {
    vec![
        (
            "audio.channels",
            validate(
                "audio-channels",
                OptionValue::Text(audio_channels_for_layout(audio.channels)),
            )
            .map(|option| vec![option]),
        ),
        (
            // `auto` and an empty field both mean "let mpv choose". A name that is not in the
            // allowed shape is refused and logged, and mpv keeps its current device.
            "audio.device",
            validate(
                "audio-device",
                OptionValue::Text(match audio.device.trim() {
                    "" => "auto",
                    device => device,
                }),
            )
            .map(|option| vec![option]),
        ),
        (
            "audio.normalize",
            validate(
                "audio-normalize-downmix",
                OptionValue::Flag(audio.normalize),
            )
            .map(|option| vec![option]),
        ),
        ("audio.filters", audio_filter_options(audio)),
        (
            "playback.frame_drop",
            validate(
                "framedrop",
                OptionValue::Text(framedrop_for_setting(playback.frame_drop)),
            )
            .map(|option| vec![option]),
        ),
        (
            "playback.default_speed",
            validate(
                "speed",
                OptionValue::Float(default_speed_for_setting(playback.default_speed)),
            )
            .map(|option| vec![option]),
        ),
        (
            "playback.cache_mb",
            demuxer_options(BufferSettings {
                cache_mb: playback.cache_mb,
                backbuffer_mb: playback.demuxer_backbuffer_mb,
                readahead_secs: playback.demuxer_readahead_secs,
            }),
        ),
        (
            "audio.gapless",
            gapless_audio_option(audio.gapless).map(|option| vec![option]),
        ),
        (
            "audio.exclusive",
            validate("audio-exclusive", OptionValue::Flag(audio.exclusive)).map(|o| vec![o]),
        ),
        (
            "audio.max_volume_percent",
            validate(
                "volume-max",
                OptionValue::Int(i64::from(audio.max_volume_percent)),
            )
            .map(|option| vec![option]),
        ),
        (
            "video.deinterlace",
            validate("deinterlace", OptionValue::Flag(video.deinterlace)).map(|o| vec![o]),
        ),
        (
            "video.tone_mapping",
            validate(
                "tone-mapping",
                OptionValue::Text(video.tone_mapping.mpv_value()),
            )
            .map(|o| vec![o]),
        ),
    ]
}

/// Why the M2-21 re-select did not finish. The text names no value.
#[derive(Debug)]
enum ReselectError {
    /// The allowlist refused a write. Nothing was queued.
    Rejected(MpvOptionError),
    /// `sid=no` was not admitted, so the track is still on.
    OffNotQueued(MpvError),
    /// `sid=N` was not admitted. The previous track was queued again at once.
    TrackRestored(MpvError),
    /// `sid=N` and its restore were not admitted. The host retries the restore on later frames,
    /// so the track stays off until one is admitted.
    RestorePending(MpvError),
}

impl std::fmt::Display for ReselectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rejected(error) => {
                write!(f, "Could not re-select the subtitle track: {error}")
            }
            Self::OffNotQueued(error) => {
                write!(f, "Could not re-select the subtitle track: {error}")
            }
            Self::TrackRestored(error) => write!(
                f,
                "Subtitle re-select was interrupted; the previous track was restored: {error}"
            ),
            Self::RestorePending(error) => write!(
                f,
                "Subtitle re-select was interrupted; the subtitle stays off until its restore is queued: {error}"
            ),
        }
    }
}

/// Run the M2-21 re-select for one position event: `sid=no`, then `sid=N`, both from the one-shot
/// allowlist. The pair is not one libmpv message, so a refused second write would leave subtitles
/// off. In that case `sid=N` is queued again at once; if the queue still refuses it, the restore is
/// kept in `pending_restore` for the host to retry.
fn run_subtitle_reselect<Q: CommandQueue>(
    queue: &Q,
    gate: &mut SubtitleReselectGate,
    pending_restore: &mut Option<ValidatedOption>,
    observation: ReselectObservation,
) -> Result<(), ReselectError> {
    let writes =
        reselect_sid_writes(gate.observe_position(observation)).map_err(ReselectError::Rejected)?;
    // NoAction produces no writes. A re-select is always exactly the off write, then the track.
    let [off, track] = writes.as_slice() else {
        return Ok(());
    };
    queue
        .enqueue(&["set", off.name, off.value.as_str()])
        .map_err(ReselectError::OffNotQueued)?;
    let Err(error) = queue.enqueue(&["set", track.name, track.value.as_str()]) else {
        return Ok(());
    };
    if queue
        .enqueue(&["set", track.name, track.value.as_str()])
        .is_ok()
    {
        return Err(ReselectError::TrackRestored(error));
    }
    *pending_restore = Some(track.clone());
    Err(ReselectError::RestorePending(error))
}

/// Retry a restore the queue refused earlier. Called once per frame until the queue admits it.
fn retry_pending_restore<Q: CommandQueue>(
    queue: &Q,
    pending_restore: &mut Option<ValidatedOption>,
) {
    let Some(restore) = pending_restore.as_ref() else {
        return;
    };
    if queue
        .enqueue(&["set", restore.name, restore.value.as_str()])
        .is_ok()
    {
        *pending_restore = None;
    }
}

#[derive(Clone, Copy)]
pub(crate) enum TrackMenuKind {
    Audio,
    Subtitle,
}

impl TrackMenuKind {
    fn label(self) -> &'static str {
        match self {
            Self::Audio => "audio",
            Self::Subtitle => "subtitle",
        }
    }

    fn track_kind(self) -> TrackKind {
        match self {
            Self::Audio => TrackKind::Audio,
            Self::Subtitle => TrackKind::Subtitle,
        }
    }
}

/// One audio or subtitle menu entry. The value is the track selection the host applies.
pub(crate) type TrackMenuOption = TrackChoice<TrackSelection>;

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PlaybackChapter {
    pub(crate) index: usize,
    pub(crate) title: String,
    pub(crate) time: Duration,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct PlaybackControlsSnapshot<'a> {
    pub(crate) position_seconds: Option<f64>,
    pub(crate) duration_seconds: Option<f64>,
    pub(crate) paused: Option<bool>,
    pub(crate) speed_multiplier: Option<f64>,
    /// Observed mpv `volume` in percent, before the boost cap is applied.
    pub(crate) volume_percent: Option<f64>,
    /// Observed mpv `mute`. Session-only (M2-16).
    pub(crate) muted: Option<bool>,
    pub(crate) chapters: &'a [PlaybackChapter],
}

#[derive(Default)]
struct PlaybackControlsState {
    position_seconds: Option<f64>,
    duration_seconds: Option<f64>,
    paused: Option<bool>,
    speed_multiplier: Option<f64>,
    volume_percent: Option<f64>,
    muted: Option<bool>,
    chapters: Vec<PlaybackChapter>,
}

impl PlaybackControlsState {
    fn clear(&mut self) {
        *self = Self::default();
    }

    fn snapshot(&self) -> PlaybackControlsSnapshot<'_> {
        PlaybackControlsSnapshot {
            position_seconds: self.position_seconds,
            duration_seconds: self.duration_seconds,
            paused: self.paused,
            speed_multiplier: self.speed_multiplier,
            volume_percent: self.volume_percent,
            muted: self.muted,
            chapters: &self.chapters,
        }
    }

    fn duration(&self) -> Option<Duration> {
        duration_from_seconds(self.duration_seconds?)
    }

    fn apply_observation(&mut self, property: PlaybackProperty, value: PlaybackPropertyValue) {
        match (property, value) {
            (PlaybackProperty::TimePosition, PlaybackPropertyValue::TimePosition(seconds)) => {
                self.position_seconds =
                    seconds.filter(|value| duration_from_seconds(*value).is_some());
            }
            (PlaybackProperty::Duration, PlaybackPropertyValue::Duration(seconds)) => {
                self.duration_seconds =
                    seconds.filter(|value| duration_from_seconds(*value).is_some());
            }
            (PlaybackProperty::Paused, PlaybackPropertyValue::Paused(paused)) => {
                self.paused = paused;
            }
            (PlaybackProperty::Speed, PlaybackPropertyValue::Speed(speed)) => {
                self.speed_multiplier = speed.filter(|value| value.is_finite() && *value > 0.0);
            }
            (PlaybackProperty::Volume, PlaybackPropertyValue::Volume(percent)) => {
                self.volume_percent = percent.filter(|value| value.is_finite() && *value >= 0.0);
            }
            (PlaybackProperty::Mute, PlaybackPropertyValue::Muted(muted)) => {
                self.muted = muted;
            }
            _ => {}
        }
    }
}

fn duration_from_seconds(seconds: f64) -> Option<Duration> {
    (seconds.is_finite() && seconds >= 0.0)
        .then(|| Duration::try_from_secs_f64(seconds).ok())
        .flatten()
}

/// The `ab-loop-a`/`ab-loop-b` value for one loop point: a decimal second count, or `no`.
fn ab_loop_time(point: Option<Duration>) -> String {
    point.map_or_else(
        || "no".to_owned(),
        |point| format!("{:.3}", point.as_secs_f64()),
    )
}

/// The feedback text for a refused A-B press.
fn ab_loop_error_text(error: AbLoopError) -> &'static str {
    match error {
        AbLoopError::MissingA => "Set loop point A before loop point B.",
        AbLoopError::NotAfterA => "Loop point B must come after loop point A.",
        AbLoopError::NotBeforeB => "Loop point A must come before loop point B.",
        AbLoopError::TooShort => "An A-B loop must be at least half a second long.",
    }
}

fn resume_position_seconds(ticks: Option<i64>) -> Option<f64> {
    ticks
        .filter(|ticks| *ticks > 0)
        .map(|ticks| ticks as f64 / 10_000_000.0)
}

fn playback_end_status(error: i32) -> String {
    if error < 0 {
        format!("Playback ended with libmpv error {error}")
    } else {
        "Playback ended".to_owned()
    }
}

fn playback_end_failed(reason: EndFileReason) -> bool {
    reason == EndFileReason::Error
}

fn playback_can_start_report(
    paused: Option<bool>,
    file_loaded: bool,
    playback_restart_seen: bool,
    resume_pending: bool,
) -> bool {
    paused == Some(false) && file_loaded && playback_restart_seen && !resume_pending
}

fn position_ticks_clamped(
    position_seconds: Option<f64>,
    duration_seconds: Option<f64>,
) -> Option<JellyfinTicks> {
    let seconds = position_seconds?;
    if !seconds.is_finite() || seconds < 0.0 {
        return None;
    }
    let duration = duration_seconds
        .filter(|duration| duration.is_finite() && *duration >= 0.0)
        .unwrap_or(seconds);
    JellyfinTicks::from_duration(Duration::try_from_secs_f64(seconds.min(duration)).ok()?)
}

fn stop_reset_speed() -> f64 {
    PlaybackSpeed::Normal.multiplier()
}

fn relative_seek_delta(
    current: Option<Duration>,
    duration: Duration,
    requested_seconds: i64,
) -> f64 {
    let Some(current) = current else {
        return requested_seconds as f64;
    };
    let current = current.min(duration.saturating_sub(Duration::from_secs(1)));
    seek_target(
        current,
        Some(duration),
        SeekRequest::RelativeSeconds(requested_seconds),
    )
    .map(|target| target.as_secs_f64() - current.as_secs_f64())
    .unwrap_or(requested_seconds as f64)
}

fn chapter_seek_target(
    chapters: &[PlaybackChapter],
    index: usize,
    duration: Option<Duration>,
) -> Option<Duration> {
    let chapter = chapters.iter().find(|chapter| chapter.index == index)?;
    seek_target(
        Duration::ZERO,
        duration,
        SeekRequest::Absolute(chapter.time),
    )
}

fn playback_chapters(
    chapters: impl IntoIterator<Item = (Option<String>, f64)>,
) -> Vec<PlaybackChapter> {
    chapters
        .into_iter()
        .enumerate()
        .filter_map(|(index, (title, seconds))| {
            let time = duration_from_seconds(seconds)?;
            let title = title
                .filter(|title| !title.trim().is_empty())
                .unwrap_or_else(|| format!("Chapter {}", index.saturating_add(1)));
            Some(PlaybackChapter { index, title, time })
        })
        .take(MAX_CONTROL_CHAPTERS)
        .collect()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SyncFeedbackTone {
    Guidance,
    Pending,
    Success,
    Error,
}

impl SyncFeedbackTone {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Guidance => "Status",
            Self::Pending => "In progress",
            Self::Success => "Complete",
            Self::Error => "Error",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SyncFeedback {
    tone: SyncFeedbackTone,
    message: String,
}

impl SyncFeedback {
    fn guidance(message: impl Into<String>) -> Self {
        Self {
            tone: SyncFeedbackTone::Guidance,
            message: message.into(),
        }
    }

    fn pending(message: impl Into<String>) -> Self {
        Self {
            tone: SyncFeedbackTone::Pending,
            message: message.into(),
        }
    }

    fn success(message: impl Into<String>) -> Self {
        Self {
            tone: SyncFeedbackTone::Success,
            message: message.into(),
        }
    }

    fn error(message: impl Into<String>) -> Self {
        Self {
            tone: SyncFeedbackTone::Error,
            message: message.into(),
        }
    }

    pub(crate) fn display(&self) -> (&str, SyncFeedbackTone) {
        (&self.message, self.tone)
    }
}

/// A viewer's explicit track choice, recorded in the per-series language memory (M2-10).
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum TrackMemoryEvent {
    Audio(TrackLanguage),
    Subtitle(TrackLanguage),
    SubtitlesOff,
}

/// Settings and per-series memory handed to the host when an item is queued.
pub(crate) struct ItemLoadSettings<'a> {
    pub(crate) audio: &'a AudioSettings,
    pub(crate) playback: &'a PlaybackSettings,
    pub(crate) subtitles: &'a SubtitleSettings,
    pub(crate) video: &'a VideoSettings,
    pub(crate) series_id: Option<String>,
    pub(crate) language_memory: SeriesLanguageMemory,
}

/// Per-item state for segment skip (M2-08) and remembered tracks (M2-10). Reset on each load.
#[derive(Default)]
struct ItemPlaybackState {
    /// The item whose segments the skipper holds. Segments for any other item are dropped.
    segment_item_id: Option<String>,
    segment_skipper: SegmentSkipper,
    /// Series GUID for an episode. `None` for movies and other non-series items.
    series_id: Option<String>,
    language_memory: SeriesLanguageMemory,
    /// The remembered tracks are applied once per item, after the first inventory.
    remembered_tracks_applied: bool,
    /// The user's track choice waiting for its libmpv reply: (command id, event).
    pending_track_memory: Option<(u64, TrackMemoryEvent)>,
}

struct PendingMediaLoad<T> {
    command_id: u64,
    generation: Option<u64>,
    relay: Option<T>,
    resume_seconds: Option<f64>,
    item: ItemPlaybackState,
}

struct MediaLoadState<T> {
    active: Option<T>,
    pending: Option<PendingMediaLoad<T>>,
    active_entry_id: Option<i64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RelayLoadError {
    Busy,
    Failed(&'static str),
}

pub(crate) struct PlaybackEnd {
    pub(crate) reason: EndFileReason,
    pub(crate) item_id: Option<String>,
    pub(crate) was_live: bool,
    pub(crate) generation: Option<u64>,
    pub(crate) was_loaded: bool,
    /// Last observed position, for reopening a file that failed mid-stream.
    pub(crate) position_ticks: Option<i64>,
}

impl<T> Default for MediaLoadState<T> {
    fn default() -> Self {
        Self {
            active: None,
            pending: None,
            active_entry_id: None,
        }
    }
}

impl<T> MediaLoadState<T> {
    fn started(&mut self, entry_id: Option<i64>) -> Option<PendingMediaLoad<T>> {
        let mut load = self.pending.take();
        if let Some(load) = load.as_mut() {
            self.active = load.relay.take();
        }
        self.active_entry_id = entry_id;
        load
    }

    fn ended(&mut self, entry_id: Option<i64>) -> bool {
        if let (Some(active), Some(ended)) = (self.active_entry_id, entry_id)
            && active != ended
        {
            return false;
        }
        self.active_entry_id = None;
        self.active = None;
        true
    }

    fn reject_command(&mut self, command_id: u64) -> bool {
        if self
            .pending
            .as_ref()
            .is_none_or(|load| load.command_id != command_id)
        {
            return false;
        }
        self.pending = None;
        true
    }

    fn load_segments(
        &mut self,
        active: &mut ItemPlaybackState,
        item_id: &str,
        segments: Vec<MediaSegment>,
    ) {
        if let Some(pending) = self.pending.as_mut()
            && pending.item.segment_item_id.as_deref() == Some(item_id)
        {
            pending.item.segment_skipper.load(item_id, segments);
        } else if active.segment_item_id.as_deref() == Some(item_id) {
            active.segment_skipper.load(item_id, segments);
        }
    }
}

pub(crate) struct MpvHost<'session> {
    session: Option<&'session MpvSession>,
    renderer: Option<MpvRenderer<'session>>,
    media_load: MediaLoadState<RelayHandle>,
    status: String,
    file_loaded: bool,
    playback_end_reason: Option<EndFileReason>,
    playback_end_item_id: Option<String>,
    playback_end_was_live: bool,
    playback_end_generation: Option<u64>,
    playback_end_was_loaded: bool,
    playback_end_position_ticks: Option<i64>,
    active_generation: Option<u64>,
    playback_loaded_generation: Option<u64>,
    playback_restart_seen: bool,
    subtitle_track_observation: Option<ObservationId>,
    subtitle_delay_observation: Option<ObservationId>,
    playback_position_observation: Option<ObservationId>,
    playback_paused_observation: Option<ObservationId>,
    playback_duration_observation: Option<ObservationId>,
    playback_speed_observation: Option<ObservationId>,
    playback_volume_observation: Option<ObservationId>,
    playback_mute_observation: Option<ObservationId>,
    subtitle_track_id: Option<i64>,
    subtitle_delay_seconds: Option<f64>,
    controls: PlaybackControlsState,
    /// A-B loop points for the current item. Session-only, so a loop never carries to the next
    /// item; cleared on `StartFile` (M6-12).
    ab_loop: AbLoop,
    chapter_list_request: Option<u64>,
    controls_feedback: Option<String>,
    track_inventory: Option<TrackInventory>,
    track_inventory_request: Option<u64>,
    track_inventory_attempts: u8,
    track_feedback: Option<String>,
    pending_track_selection: Option<(u64, TrackMenuKind)>,
    /// A `sub-add` of a downloaded subtitle; its reply refreshes the track list.
    pending_subtitle_add: Option<u64>,
    /// Set only at launch by the debug QA host; normal product sessions report as usual.
    reporting_enabled: bool,
    pending_report_metadata: Option<PlaybackReportMetadata>,
    report_metadata: Option<PlaybackReportMetadata>,
    report_session: Option<PlaybackReportSessionId>,
    report_started_at: Option<Instant>,
    pending_resume_seconds: Option<f64>,
    resume_seek_command: Option<u64>,
    playback_seek_pending: bool,
    next_report_generation: u64,
    report_scheduler: Option<PlaybackReportScheduler>,
    report_actions: ReportHandoff,
    sync_controller: SubtitleSyncController,
    sync_feedback: SyncFeedback,
    capability_snapshot_request: Option<u64>,
    media_info: MediaInfoState,
    /// Set while the saved settings ask for refresh-rate matching: whether to avoid 25 and 30 Hz.
    display_match: Option<bool>,
    /// The stream snapshot read that finds the frame rate for display matching.
    content_rate_request: Option<u64>,
    /// The frame rate found for the current file, taken once by the host.
    content_rate: Option<(f64, bool)>,
    device_profile: Option<DeviceProfile>,
    /// Output devices mpv listed in the capability probe, handed to the Settings page once.
    audio_devices: Option<Vec<String>>,
    /// Last equalizer and filter values written to the player, so a settings change writes only
    /// what moved, and when (a slider drag is applied at most every [`AUDIO_FILTER_APPLY_GAP`]).
    audio_filter_ledger: OptionWriteLedger,
    audio_filter_applied_at: Option<Instant>,
    capability_warning: Option<ProbeWarning>,
    gpu_identity: String,
    driver_identity: String,
    policy: PlaybackPolicyState,
    item: ItemPlaybackState,
    /// Track choices the viewer made, with the series they belong to. Drained once per frame.
    track_memory_events: Vec<(Option<String>, TrackMemoryEvent)>,
}

/// Per-item subtitle and track state that the playback policy needs between mpv events.
#[derive(Default)]
struct PlaybackPolicyState {
    /// The offset requested by the last `[` or `]` press. mpv's own `sub-delay` may lag behind
    /// it, so the next press starts from this value. Cleared once mpv reports the same delay.
    pending_subtitle_offset: Option<SubtitleOffset>,
    /// One-shot re-select gate for the current item.
    reselect_gate: SubtitleReselectGate,
    /// True once the viewer picked a subtitle track for this item.
    subtitle_user_choice: bool,
    /// The secondary subtitle track, when one is set.
    secondary_subtitle: Option<SubtitleTrackId>,
    /// Last value written for each subtitle style option. Style writes survive item changes.
    style_writes: OptionWriteLedger,
    /// `sid` = the track, queued again after a refused M2-21 write. Retried each frame until the
    /// queue admits it. Cleared when the viewer picks a subtitle or the item changes.
    pending_sid_restore: Option<ValidatedOption>,
}

impl PlaybackPolicyState {
    fn reset_item(&mut self) {
        self.pending_subtitle_offset = None;
        self.reselect_gate = SubtitleReselectGate::new();
        self.subtitle_user_choice = false;
        self.secondary_subtitle = None;
        self.pending_sid_restore = None;
    }
}

impl<'session> MpvHost<'session> {
    pub(crate) fn new(
        session: Option<&'session MpvSession>,
        runtime_error: Option<String>,
        provider: Arc<HostGlContextProvider>,
        repaint_proxy: EventLoopProxy<crate::HostEvent>,
        gpu_identity: String,
        driver_identity: String,
    ) -> Self {
        let Some(session) = session else {
            return Self {
                session: None,
                renderer: None,
                media_load: MediaLoadState::default(),
                status: runtime_error.unwrap_or_else(|| "libmpv is unavailable".into()),
                file_loaded: false,
                playback_end_reason: None,
                playback_end_item_id: None,
                playback_end_was_live: false,
                playback_end_generation: None,
                playback_end_was_loaded: false,
                playback_end_position_ticks: None,
                active_generation: None,
                playback_loaded_generation: None,
                playback_restart_seen: false,
                subtitle_track_observation: None,
                subtitle_delay_observation: None,
                playback_position_observation: None,
                playback_paused_observation: None,
                playback_duration_observation: None,
                playback_speed_observation: None,
                playback_volume_observation: None,
                playback_mute_observation: None,
                subtitle_track_id: None,
                subtitle_delay_seconds: None,
                controls: PlaybackControlsState::default(),
                chapter_list_request: None,
                controls_feedback: None,
                track_inventory: None,
                track_inventory_request: None,
                track_inventory_attempts: 0,
                track_feedback: None,
                pending_track_selection: None,
                pending_subtitle_add: None,
                reporting_enabled: true,
                pending_report_metadata: None,
                report_metadata: None,
                report_session: None,
                report_started_at: None,
                pending_resume_seconds: None,
                resume_seek_command: None,
                playback_seek_pending: false,
                next_report_generation: 0,
                report_scheduler: playback_report_scheduler(true),
                report_actions: ReportHandoff::default(),
                ab_loop: AbLoop::new(),
                sync_controller: SubtitleSyncController::new(),
                sync_feedback: SyncFeedback::guidance(
                    "Load media with a selectable subtitle track to sync it.",
                ),
                capability_snapshot_request: None,
                media_info: MediaInfoState::default(),
                display_match: None,
                content_rate_request: None,
                content_rate: None,
                device_profile: None,
                audio_devices: None,
                audio_filter_ledger: OptionWriteLedger::new(),
                audio_filter_applied_at: None,
                capability_warning: Some(ProbeWarning::RuntimeUnavailable),
                gpu_identity,
                driver_identity,
                policy: PlaybackPolicyState::default(),
                item: ItemPlaybackState::default(),
                track_memory_events: Vec::new(),
            };
        };

        let subtitle_track_observation = session
            .observe_property(PlaybackProperty::SubtitleTrackId)
            .ok();
        let subtitle_delay_observation = session
            .observe_property(PlaybackProperty::SubtitleDelay)
            .ok();
        let playback_position_observation = session
            .observe_property(PlaybackProperty::TimePosition)
            .ok();
        let playback_paused_observation = session.observe_property(PlaybackProperty::Paused).ok();
        let playback_duration_observation =
            session.observe_property(PlaybackProperty::Duration).ok();
        let playback_speed_observation = session.observe_property(PlaybackProperty::Speed).ok();
        let playback_volume_observation = session.observe_property(PlaybackProperty::Volume).ok();
        let playback_mute_observation = session.observe_property(PlaybackProperty::Mute).ok();
        let capability_snapshot_request = session.request_capability_snapshot().ok();
        let capability_warning = capability_snapshot_request
            .is_none()
            .then_some(ProbeWarning::RuntimeUnavailable);

        let notifier: Arc<dyn RepaintNotifier> = Arc::new(move || {
            let _ = repaint_proxy.send_event(crate::HostEvent::MpvRepaint);
        });
        let renderer = match MpvRenderer::create(
            session,
            provider,
            notifier,
            BackendSelection::OpenGlLibMpv,
        ) {
            Ok(renderer) => renderer,
            Err(error) => {
                return Self {
                    session: Some(session),
                    renderer: None,
                    media_load: MediaLoadState::default(),
                    status: format!("libmpv OpenGL renderer unavailable: {error}"),
                    file_loaded: false,
                    playback_end_reason: None,
                    playback_end_item_id: None,
                    playback_end_was_live: false,
                    playback_end_generation: None,
                    playback_end_was_loaded: false,
                    playback_end_position_ticks: None,
                    active_generation: None,
                    playback_loaded_generation: None,
                    playback_restart_seen: false,
                    subtitle_track_observation,
                    subtitle_delay_observation,
                    playback_position_observation,
                    playback_paused_observation,
                    playback_duration_observation,
                    playback_speed_observation,
                    playback_volume_observation,
                    playback_mute_observation,
                    subtitle_track_id: None,
                    subtitle_delay_seconds: None,
                    controls: PlaybackControlsState::default(),
                    chapter_list_request: None,
                    controls_feedback: None,
                    track_inventory: None,
                    track_inventory_request: None,
                    track_inventory_attempts: 0,
                    track_feedback: None,
                    pending_track_selection: None,
                    pending_subtitle_add: None,
                    reporting_enabled: true,
                    pending_report_metadata: None,
                    report_metadata: None,
                    report_session: None,
                    report_started_at: None,
                    pending_resume_seconds: None,
                    resume_seek_command: None,
                    playback_seek_pending: false,
                    next_report_generation: 0,
                    report_scheduler: playback_report_scheduler(true),
                    report_actions: ReportHandoff::default(),
                    ab_loop: AbLoop::new(),
                    sync_controller: SubtitleSyncController::new(),
                    sync_feedback: SyncFeedback::guidance(
                        "Load media with a selectable subtitle track to sync it.",
                    ),
                    capability_snapshot_request,
                    media_info: MediaInfoState::default(),
                    display_match: None,
                    content_rate_request: None,
                    content_rate: None,
                    device_profile: None,
                    audio_devices: None,
                    audio_filter_ledger: OptionWriteLedger::new(),
                    audio_filter_applied_at: None,
                    capability_warning,
                    gpu_identity,
                    driver_identity,
                    policy: PlaybackPolicyState::default(),
                    item: ItemPlaybackState::default(),
                    track_memory_events: Vec::new(),
                };
            }
        };

        Self {
            session: Some(session),
            renderer: Some(renderer),
            media_load: MediaLoadState::default(),
            status: "OpenGL video surface ready; no media loaded".into(),
            file_loaded: false,
            playback_end_reason: None,
            playback_end_item_id: None,
            playback_end_was_live: false,
            playback_end_generation: None,
            playback_end_was_loaded: false,
            playback_end_position_ticks: None,
            active_generation: None,
            playback_loaded_generation: None,
            playback_restart_seen: false,
            subtitle_track_observation,
            subtitle_delay_observation,
            playback_position_observation,
            playback_paused_observation,
            playback_duration_observation,
            playback_speed_observation,
            playback_volume_observation,
            playback_mute_observation,
            subtitle_track_id: None,
            subtitle_delay_seconds: None,
            controls: PlaybackControlsState::default(),
            chapter_list_request: None,
            controls_feedback: None,
            track_inventory: None,
            track_inventory_request: None,
            track_inventory_attempts: 0,
            track_feedback: None,
            pending_track_selection: None,
            pending_subtitle_add: None,
            reporting_enabled: true,
            pending_report_metadata: None,
            report_metadata: None,
            report_session: None,
            report_started_at: None,
            pending_resume_seconds: None,
            resume_seek_command: None,
            playback_seek_pending: false,
            next_report_generation: 0,
            report_scheduler: playback_report_scheduler(true),
            report_actions: ReportHandoff::default(),
            ab_loop: AbLoop::new(),
            sync_controller: SubtitleSyncController::new(),
            sync_feedback: SyncFeedback::guidance(
                "Load media with a selectable subtitle track to sync it.",
            ),
            capability_snapshot_request,
            media_info: MediaInfoState::default(),
            display_match: None,
            content_rate_request: None,
            content_rate: None,
            device_profile: None,
            audio_devices: None,
            audio_filter_ledger: OptionWriteLedger::new(),
            audio_filter_applied_at: None,
            capability_warning,
            gpu_identity,
            driver_identity,
            policy: PlaybackPolicyState::default(),
            item: ItemPlaybackState::default(),
            track_memory_events: Vec::new(),
        }
    }

    /// Queue the local file from the command line. The decode and layout settings come from the
    /// same accessors as the relay path. hwdec and the layout writes are queued before loadfile
    /// because mpv reads them when the file opens (M2-20, M2-22).
    pub(crate) fn load_local_media(
        &mut self,
        local_media: Result<Option<String>, String>,
        video: &VideoSettings,
        subtitles: &SubtitleSettings,
    ) {
        let Some(session) = self.session else {
            return;
        };
        match local_media {
            Ok(Some(path)) => {
                if self.media_load.pending.is_some() {
                    self.status = "A media load is already pending.".into();
                    return;
                }
                if let Err(error) = self.sync_controller.reset_subtitle_delay(session) {
                    self.sync_feedback = subtitle_delay_error(error);
                }
                queue_hwdec(session, video.decode_mode);
                for (setting, options) in local_file_layout_options(video, subtitles) {
                    queue_validated(session, setting, options);
                }
                match session.command(&["loadfile", &path, "replace"]) {
                    Ok(command) => {
                        self.media_load.pending = Some(PendingMediaLoad {
                            command_id: command.get(),
                            generation: None,
                            relay: None,
                            resume_seconds: None,
                            item: ItemPlaybackState::default(),
                        });
                        self.pending_report_metadata = None;
                        self.status = "Loading local media".into();
                    }
                    Err(error) => self.status = format!("Could not queue local media: {error}"),
                }
            }
            Ok(None) => {}
            Err(error) => self.status = error,
        }
    }

    /// Queues every settings-driven write that mpv reads when a file opens: passthrough, subtitle
    /// timing, sync mode, audio, video and subtitle layout, and hwdec. Both the server stream and
    /// a typed stream address use it, so they start from the same saved settings.
    fn queue_load_settings(
        &mut self,
        session: &MpvSession,
        load: &ItemLoadSettings<'_>,
    ) -> Result<(), &'static str> {
        self.display_match = load
            .video
            .refresh_rate_auto_switch
            .then_some(load.video.refresh_rate_avoid_25_30_hz);
        session
            .apply_audio_passthrough(load.audio)
            .map_err(|_| "Could not apply audio passthrough settings before playback.")?;
        self.sync_controller
            .reset_subtitle_delay(session)
            .map_err(|_| "Could not reset subtitle timing before playback.")?;
        // The secondary position is a global mpv option, so it is set before the file loads.
        // A refusal is logged, not shown: the feedback line is cleared when the file starts.
        if let Err(error) = session.apply_secondary_subtitle_position(Some(i64::from(
            load.subtitles.secondary_position_percent,
        ))) {
            tracing::warn!(%error, "secondary subtitle position was not queued");
        }
        // The sync mode is a global mpv option too (M2-22). The setting's spelling passes the
        // video-sync allowlist entry before it is queued, so a refused value is logged, not sent.
        match validate(
            "video-sync",
            OptionValue::Text(video_sync_for_sync_mode(load.playback.sync_mode)),
        ) {
            Ok(option) => {
                if let Err(error) = session.command(&["set", option.name, &option.value]) {
                    tracing::warn!(%error, "video sync mode was not queued");
                }
            }
            Err(error) => tracing::warn!(%error, "video sync mode rejected by the mpv allowlist"),
        }
        for (setting, options) in pre_load_options(load.audio, load.playback, load.video) {
            queue_validated(session, setting, options);
        }
        for (setting, options) in video_layout_options(load.video) {
            queue_validated(session, setting, options);
        }
        for (setting, options) in subtitle_layout_options(load.subtitles) {
            queue_validated(session, setting, options);
        }
        // mpv reads hwdec when the file opens, so it must be queued before loadfile.
        queue_hwdec(session, load.video.decode_mode);
        Ok(())
    }

    /// Plays a typed stream address directly. It is not a Jellyfin item: no relay, no playback
    /// reports, no segments and no remembered tracks. The address was checked by
    /// [`StreamUrl::parse`] (https, or http to a private host, no credentials) and the player's
    /// own start-up options already refuse redirects and scripts.
    pub(crate) fn load_stream_url(
        &mut self,
        url: &StreamUrl,
        load: ItemLoadSettings<'_>,
    ) -> Result<(), &'static str> {
        if self.renderer.is_none() {
            return Err("The libmpv video renderer is unavailable.");
        }
        if self.media_load.pending.is_some() {
            return Err("A media load is already pending. Try again shortly.");
        }
        let Some(session) = self.session else {
            return Err("libmpv is unavailable.");
        };
        self.queue_load_settings(session, &load)?;
        let command = session
            .command(&["loadfile", url.as_str(), "replace"])
            .map_err(|_| "Could not queue the stream address in libmpv.")?;
        self.media_load.pending = Some(PendingMediaLoad {
            command_id: command.get(),
            generation: None,
            relay: None,
            resume_seconds: None,
            item: ItemPlaybackState::default(),
        });
        self.pending_report_metadata = None;
        self.status = format!("Loading stream {}", url.redacted());
        self.sync_controller.reset();
        self.policy.reset_item();
        self.subtitle_track_id = None;
        self.subtitle_delay_seconds = None;
        self.sync_feedback =
            SyncFeedback::guidance("Playback changed; start a new subtitle-sync pair.");
        Ok(())
    }

    pub(crate) fn load_relay(
        &mut self,
        url: RelayPlaybackUrl,
        relay: RelayHandle,
        report_metadata: PlaybackReportMetadata,
        resume_position_ticks: Option<i64>,
        generation: u64,
        load: ItemLoadSettings<'_>,
    ) -> Result<(), RelayLoadError> {
        if self.renderer.is_none() {
            return Err(RelayLoadError::Failed(
                "The libmpv video renderer is unavailable.",
            ));
        }
        if self.media_load.pending.is_some() {
            return Err(RelayLoadError::Busy);
        }
        let Some(session) = self.session else {
            return Err(RelayLoadError::Failed("libmpv is unavailable."));
        };
        self.queue_load_settings(session, &load)
            .map_err(RelayLoadError::Failed)?;
        let command = session
            .command(&["loadfile", url.as_str(), "replace"])
            .map_err(|_| RelayLoadError::Failed("Could not queue the server stream in libmpv."))?;
        self.media_load.pending = Some(PendingMediaLoad {
            command_id: command.get(),
            generation: Some(generation),
            relay: Some(relay),
            resume_seconds: resume_position_seconds(resume_position_ticks),
            item: ItemPlaybackState {
                segment_item_id: Some(report_metadata.item_id.clone()),
                series_id: load.series_id,
                language_memory: load.language_memory,
                ..ItemPlaybackState::default()
            },
        });
        self.pending_report_metadata = Some(report_metadata);
        self.status = "Loading the Jellyfin stream through the local relay".into();
        self.sync_controller.reset();
        self.policy.reset_item();
        self.subtitle_track_id = None;
        self.subtitle_delay_seconds = None;
        self.sync_feedback =
            SyncFeedback::guidance("Playback changed; start a new subtitle-sync pair.");
        Ok(())
    }

    pub(crate) fn render_if_due(&mut self, target: FboTarget) -> Result<FrameDecision, MpvError> {
        match self.renderer.as_mut() {
            Some(renderer) => renderer.render_if_due(target),
            None => Ok(FrameDecision::NoFrame),
        }
    }

    pub(crate) fn report_swap(&mut self) -> Result<(), MpvError> {
        match self.renderer.as_mut() {
            Some(renderer) => renderer.report_swap(),
            None => Ok(()),
        }
    }

    pub(crate) fn poll_events(&mut self) {
        let Some(session) = self.session else {
            return;
        };
        retry_pending_restore(session, &mut self.policy.pending_sid_restore);
        self.refresh_media_info(session);
        while let Some(event) = session.try_event() {
            match event.kind {
                EventKind::StreamSnapshotReady
                    if Some(event.request_id) == self.content_rate_request =>
                {
                    self.content_rate_request = None;
                    let avoid = self.display_match;
                    let rate =
                        session
                            .take_stream_snapshot(event.request_id)
                            .and_then(|snapshot| {
                                mpv_render_proof::available_text(&snapshot.frame_rate)
                                    .and_then(|text| text.trim().parse::<f64>().ok())
                            });
                    if let (Some(avoid), Some(rate)) = (avoid, rate) {
                        self.content_rate = Some((rate, avoid));
                    }
                }
                EventKind::StreamSnapshotReady
                    if self.media_info.is_waiting_for(event.request_id) =>
                {
                    let snapshot = session.take_stream_snapshot(event.request_id);
                    self.media_info.read_finished(snapshot);
                }
                EventKind::FileLoaded => {
                    self.file_loaded = true;
                    self.playback_loaded_generation = self.active_generation;
                    self.status = if self.report_metadata.is_some() {
                        "Jellyfin stream is loaded".into()
                    } else {
                        "Media is loaded in libmpv".into()
                    };
                    self.apply_resume_position(session);
                    self.maybe_start_playback_report();
                    if self.display_match.is_some() {
                        self.content_rate_request = session.request_stream_snapshot().ok();
                    }
                    self.request_track_inventory();
                    self.request_chapter_list();
                    // StartFile cleared the controls. Pause, speed, volume and mute keep their
                    // values across a file load, so mpv sends no change event; read them once so
                    // the pause button, speed menu, volume slider and mute button know their state.
                    for property in [
                        PlaybackProperty::Paused,
                        PlaybackProperty::Speed,
                        PlaybackProperty::Volume,
                        PlaybackProperty::Mute,
                    ] {
                        let _ = session.request_property(property);
                    }
                }
                EventKind::PropertyReadReply { property, value }
                    if matches!(
                        property,
                        PlaybackProperty::Paused
                            | PlaybackProperty::Speed
                            | PlaybackProperty::Volume
                            | PlaybackProperty::Mute
                    ) =>
                {
                    #[cfg(debug_assertions)]
                    crate::debug_harness::log_if_changed(
                        &format!("mpv read {property:?}"),
                        &format!("{value:?} error={}", event.error),
                    );
                    if event.error >= 0 {
                        self.controls.apply_observation(property, value);
                    }
                }
                EventKind::StartFile { playlist_entry_id } => {
                    self.file_loaded = false;
                    self.media_info.clear_snapshot();
                    self.content_rate_request = None;
                    self.content_rate = None;
                    self.playback_restart_seen = false;
                    self.stop_playback_report(false);
                    if let Some(load) = self.media_load.started(playlist_entry_id) {
                        self.active_generation = load.generation;
                        self.pending_resume_seconds = load.resume_seconds;
                        self.item = load.item;
                    } else {
                        self.active_generation = None;
                    }
                    self.report_metadata = self.pending_report_metadata.take();
                    self.report_session = self.next_report_session();
                    self.report_started_at = None;
                    self.resume_seek_command = None;
                    self.playback_seek_pending = false;
                    self.controls.clear();
                    // A-B points belong to one item, so they never reach the next file (M6-12).
                    self.ab_loop.clear();
                    let _ = session.command(&["set", "ab-loop-a", "no"]);
                    let _ = session.command(&["set", "ab-loop-b", "no"]);
                    self.chapter_list_request = None;
                    self.controls_feedback = None;
                    self.track_inventory = None;
                    self.track_inventory_request = None;
                    self.track_inventory_attempts = 0;
                    self.track_feedback = None;
                    self.pending_track_selection = None;
                    self.reset_subtitle_state();
                }
                EventKind::EndFile {
                    reason,
                    playlist_entry_id,
                } => {
                    if !self.media_load.ended(playlist_entry_id) {
                        continue;
                    }
                    self.playback_end_reason = Some(reason);
                    self.playback_end_generation = self.active_generation.take();
                    self.playback_end_was_loaded = self.file_loaded;
                    self.playback_end_position_ticks = self
                        .file_loaded
                        .then(|| self.playback_position_ticks())
                        .flatten()
                        .map(JellyfinTicks::as_ticks);
                    self.playback_end_item_id = self
                        .report_metadata
                        .as_ref()
                        .map(|metadata| metadata.item_id.clone());
                    self.playback_end_was_live = self
                        .report_metadata
                        .as_ref()
                        .is_some_and(|metadata| metadata.live_stream_id.is_some());
                    self.status = playback_end_status(event.error);
                    self.media_info.close();
                    // EndFile is the point at which mpv has stopped consuming
                    // the relay URL. Dropping the handle requests listener shutdown.
                    self.file_loaded = false;
                    self.playback_restart_seen = false;
                    self.stop_playback_report(playback_end_failed(reason));
                    self.pending_resume_seconds = None;
                    self.resume_seek_command = None;
                    self.playback_seek_pending = false;
                    self.controls.clear();
                    self.chapter_list_request = None;
                    self.controls_feedback = None;
                    self.reset_speed_after_end();
                    self.track_inventory = None;
                    self.track_inventory_request = None;
                    self.track_inventory_attempts = 0;
                    self.track_feedback = None;
                    self.pending_track_selection = None;
                    self.reset_subtitle_state();
                }
                EventKind::CommandReply | EventKind::CommandSubmissionFailed
                    if self
                        .media_load
                        .pending
                        .as_ref()
                        .is_some_and(|load| load.command_id == event.request_id)
                        && (matches!(event.kind, EventKind::CommandSubmissionFailed)
                            || event.error < 0) =>
                {
                    let generation = self
                        .media_load
                        .pending
                        .as_ref()
                        .and_then(|load| load.generation);
                    let _ = self.media_load.reject_command(event.request_id);
                    let metadata = self.pending_report_metadata.take();
                    self.playback_end_reason = Some(EndFileReason::Error);
                    self.playback_end_generation = generation;
                    self.playback_end_was_loaded = false;
                    self.playback_end_position_ticks = None;
                    self.playback_end_item_id = metadata.as_ref().map(|item| item.item_id.clone());
                    self.playback_end_was_live = metadata
                        .as_ref()
                        .is_some_and(|item| item.live_stream_id.is_some());
                    self.status = format!("Could not start media (libmpv error {}).", event.error);
                }
                EventKind::CommandReply if Some(event.request_id) == self.resume_seek_command => {
                    self.resume_seek_command = None;
                    if event.error < 0 {
                        self.controls_feedback = Some(format!(
                            "Could not seek to the saved resume position (libmpv error {}).",
                            event.error
                        ));
                        self.pending_resume_seconds = None;
                        self.maybe_start_playback_report();
                    }
                }
                EventKind::CommandSubmissionFailed
                    if Some(event.request_id) == self.resume_seek_command =>
                {
                    self.resume_seek_command = None;
                    self.controls_feedback = Some(format!(
                        "Could not queue the saved resume seek (libmpv error {}).",
                        event.error
                    ));
                    self.pending_resume_seconds = None;
                    self.maybe_start_playback_report();
                }
                EventKind::CommandReply
                    if self
                        .pending_track_selection
                        .is_some_and(|(request_id, _)| request_id == event.request_id) =>
                {
                    self.complete_track_selection(event.request_id, event.error, false);
                }
                EventKind::CommandSubmissionFailed
                    if self
                        .pending_track_selection
                        .is_some_and(|(request_id, _)| request_id == event.request_id) =>
                {
                    self.complete_track_selection(event.request_id, event.error, true);
                }
                EventKind::CommandReply | EventKind::CommandSubmissionFailed
                    if self.pending_subtitle_add == Some(event.request_id) =>
                {
                    self.pending_subtitle_add = None;
                    if matches!(event.kind, EventKind::CommandSubmissionFailed) || event.error < 0 {
                        tracing::warn!(error = event.error, "downloaded subtitle was not loaded");
                    }
                    self.request_track_inventory();
                }
                EventKind::TrackInventoryReady => {
                    self.complete_track_inventory(event.request_id, session);
                }
                EventKind::ChapterListReady
                    if Some(event.request_id) == self.chapter_list_request =>
                {
                    self.complete_chapter_list(event.request_id, session);
                }
                EventKind::CapabilitySnapshotReady
                    if Some(event.request_id) == self.capability_snapshot_request =>
                {
                    if let Some(snapshot) = session.take_capability_snapshot(event.request_id) {
                        let identity = RuntimeIdentity {
                            mpv_version: match &snapshot.mpv_version {
                                CapabilityValue::Available(value) => value.clone(),
                                CapabilityValue::Unavailable(_) => String::new(),
                            },
                            ffmpeg_version: match &snapshot.ffmpeg_version {
                                CapabilityValue::Available(value) => Some(value.clone()),
                                CapabilityValue::Unavailable(_) => None,
                            },
                            operating_system: RuntimeIdentity::current_os(),
                            gpu: (!self.gpu_identity.trim().is_empty())
                                .then(|| self.gpu_identity.clone()),
                            driver: (!self.driver_identity.trim().is_empty())
                                .then(|| self.driver_identity.clone()),
                        };
                        let observed = RuntimeCapabilitySnapshot {
                            identity: identity.clone(),
                            decoder_names: probe_value(snapshot.decoders, "decoder-list"),
                            demuxer_names: probe_value(snapshot.demuxers, "demuxer-lavf-list"),
                            configured_hwdec: probe_value(snapshot.configured_hwdec, "hwdec"),
                            active_hwdec: probe_value(snapshot.active_hwdec, "hwdec-current"),
                            hwdec_interop: probe_value(snapshot.hwdec_interop, "hwdec-interop"),
                            audio_devices: probe_value(snapshot.audio_devices, "audio-device-list"),
                            warning: None,
                        };
                        let cached = cached_runtime_capabilities(&identity);
                        let runtime = cached.as_ref().unwrap_or(&observed);
                        let decoders = match &runtime.decoder_names {
                            ProbeValue::Available(values) => Some(values.as_slice()),
                            ProbeValue::Unavailable(_) => None,
                        };
                        let demuxers = match &runtime.demuxer_names {
                            ProbeValue::Available(values) => Some(values.as_slice()),
                            ProbeValue::Unavailable(_) => None,
                        };
                        self.device_profile =
                            Some(device_profile_from_runtime_inventory(decoders, demuxers));
                        self.audio_devices = match &runtime.audio_devices {
                            ProbeValue::Available(devices) => Some(devices.clone()),
                            ProbeValue::Unavailable(_) => None,
                        };
                        self.capability_warning = if decoders.is_none() {
                            Some(ProbeWarning::PropertyUnavailable {
                                property: "decoder-list".into(),
                            })
                        } else if demuxers.is_none() {
                            Some(ProbeWarning::PropertyUnavailable {
                                property: "demuxer-lavf-list".into(),
                            })
                        } else {
                            None
                        };
                        let _ = cache_runtime_capabilities(observed);
                    } else {
                        self.device_profile =
                            Some(device_profile_from_runtime_inventory(None, None));
                        self.capability_warning = Some(ProbeWarning::RuntimeUnavailable);
                    }
                }
                EventKind::Seek => {
                    self.sync_controller.reset();
                    self.sync_feedback =
                        SyncFeedback::guidance("Seek completed; start a new subtitle-sync pair.");
                    if self.pending_resume_seconds.is_none() {
                        self.playback_seek_pending = true;
                    }
                }
                EventKind::PlaybackRestart => {
                    self.playback_restart_seen = true;
                    self.sync_controller.reset();
                    self.sync_feedback =
                        SyncFeedback::guidance("Playback resumed; start a new subtitle-sync pair.");
                    self.maybe_start_playback_report();
                }
                EventKind::PropertyChange(change)
                    if Some(change.observation_id) == self.subtitle_track_observation
                        && change.property == PlaybackProperty::SubtitleTrackId =>
                {
                    let PlaybackPropertyValue::SubtitleTrackId(next_track_id) = change.value else {
                        continue;
                    };
                    if self.subtitle_track_id != next_track_id {
                        self.sync_controller.reset();
                        self.sync_feedback = if next_track_id.is_some() {
                            SyncFeedback::guidance("Subtitle track changed; start a new sync pair.")
                        } else {
                            SyncFeedback::guidance("Select a subtitle track to sync it.")
                        };
                        self.subtitle_track_id = next_track_id;
                        self.reconcile_subtitle_selection(session);
                    }
                }
                EventKind::PropertyChange(change)
                    if Some(change.observation_id) == self.subtitle_delay_observation
                        && change.property == PlaybackProperty::SubtitleDelay =>
                {
                    if let PlaybackPropertyValue::SubtitleDelay(delay) = change.value {
                        self.subtitle_delay_seconds = delay.filter(|value| value.is_finite());
                        let observed = observed_subtitle_offset(self.subtitle_delay_seconds);
                        if self.policy.pending_subtitle_offset == Some(observed) {
                            self.policy.pending_subtitle_offset = None;
                        }
                    }
                }
                EventKind::PropertyChange(change)
                    if Some(change.observation_id) == self.playback_position_observation
                        && change.property == PlaybackProperty::TimePosition =>
                {
                    self.controls
                        .apply_observation(change.property, change.value);
                    if let PlaybackPropertyValue::TimePosition(Some(seconds)) = change.value
                        && self
                            .pending_resume_seconds
                            .is_some_and(|target| (seconds - target).abs() < 1.0)
                    {
                        self.pending_resume_seconds = None;
                        self.playback_seek_pending = false;
                    }
                    self.maybe_start_playback_report();
                    if self.playback_seek_pending {
                        self.record_playback_seek_snapshot();
                        self.playback_seek_pending = false;
                    } else {
                        self.record_playback_report_snapshot();
                    }
                }
                EventKind::PropertyChange(change)
                    if Some(change.observation_id) == self.playback_paused_observation
                        && change.property == PlaybackProperty::Paused =>
                {
                    self.controls
                        .apply_observation(change.property, change.value);
                    self.maybe_start_playback_report();
                    self.record_playback_report_snapshot();
                }
                EventKind::PropertyChange(change)
                    if Some(change.observation_id) == self.playback_duration_observation
                        && change.property == PlaybackProperty::Duration =>
                {
                    self.controls
                        .apply_observation(change.property, change.value);
                }
                EventKind::PropertyChange(change)
                    if Some(change.observation_id) == self.playback_speed_observation
                        && change.property == PlaybackProperty::Speed =>
                {
                    self.controls
                        .apply_observation(change.property, change.value);
                }
                EventKind::PropertyChange(change)
                    if change.property == PlaybackProperty::Volume
                        && Some(change.observation_id) == self.playback_volume_observation =>
                {
                    self.controls
                        .apply_observation(change.property, change.value);
                }
                EventKind::PropertyChange(change)
                    if change.property == PlaybackProperty::Mute
                        && Some(change.observation_id) == self.playback_mute_observation =>
                {
                    self.controls
                        .apply_observation(change.property, change.value);
                }
                _ => {}
            }

            if let Some(update) = self.sync_controller.process_event(event, session) {
                self.sync_feedback = describe_sync_update(update);
            }
        }
    }

    /// Applies a changed equalizer or audio filter setting to the running player. Returns true
    /// when a change is waiting because the last one was applied too recently; the caller asks
    /// for another frame after [`AUDIO_FILTER_APPLY_GAP`].
    pub(crate) fn sync_audio_filters(&mut self, audio: &AudioSettings, now: Instant) -> bool {
        let Some(session) = self.session else {
            return false;
        };
        let options = match audio_filter_options(audio) {
            Ok(options) => options,
            Err(error) => {
                tracing::warn!(%error, "audio filter settings rejected by the mpv allowlist");
                return false;
            }
        };
        let pairs: Vec<_> = options
            .iter()
            .map(|option| (option.name, option.value.clone()))
            .collect();
        if pairs
            .iter()
            .all(|(name, value)| self.audio_filter_ledger.applied(name) == Some(value.as_str()))
        {
            return false;
        }
        if let Some(at) = self.audio_filter_applied_at
            && now.duration_since(at) < AUDIO_FILTER_APPLY_GAP
        {
            return true;
        }
        for (name, value) in self.audio_filter_ledger.changed_writes(&pairs) {
            if session.command(&["set", name, &value]).is_err() {
                tracing::warn!(setting = name, "audio filter write was not queued");
            }
        }
        self.audio_filter_applied_at = Some(now);
        false
    }

    /// The output devices from the latest capability probe, once.
    pub(crate) fn take_audio_devices(&mut self) -> Option<Vec<String>> {
        self.audio_devices.take()
    }

    pub(crate) fn take_device_profile(&mut self) -> Option<DeviceProfile> {
        self.device_profile.take()
    }

    pub(crate) fn capability_warning(&self) -> Option<&ProbeWarning> {
        self.capability_warning.as_ref()
    }

    pub(crate) fn controls_snapshot(&self) -> PlaybackControlsSnapshot<'_> {
        self.controls.snapshot()
    }

    pub(crate) fn controls_feedback(&self) -> Option<&str> {
        self.controls_feedback.as_deref()
    }

    pub(crate) fn playing_item_id(&self) -> Option<&str> {
        self.report_metadata
            .as_ref()
            .map(|metadata| metadata.item_id.as_str())
    }

    pub(crate) fn live_stream_active(&self) -> bool {
        self.report_metadata
            .as_ref()
            .or(self.pending_report_metadata.as_ref())
            .is_some_and(|metadata| metadata.live_stream_id.is_some())
    }

    pub(crate) fn take_live_tune_elapsed(&mut self) -> Option<Duration> {
        let metadata = self
            .report_metadata
            .as_mut()
            .or(self.pending_report_metadata.as_mut())?;
        let started_at = metadata.live_tune_started_at.take()?;
        Some(started_at.elapsed())
    }

    pub(crate) fn set_paused(&mut self, paused: bool) {
        let Some(session) = self.session else {
            self.controls_feedback = Some("libmpv is unavailable.".into());
            return;
        };
        self.record_control_result(session.set_paused(paused).map(|_| ()), "pause state");
    }

    pub(crate) fn toggle_paused(&mut self) {
        let Some(paused) = self.controls.paused else {
            self.controls_feedback = Some("Playback pause state is not available yet.".into());
            return;
        };
        self.set_paused(!paused);
    }

    pub(crate) fn seek_relative(&mut self, delta_seconds: i64) {
        let Some(duration) = self.controls.duration().filter(|value| !value.is_zero()) else {
            self.controls_feedback =
                Some("Seeking is unavailable for live or unknown-length media.".into());
            return;
        };
        let current = self
            .controls
            .position_seconds
            .and_then(duration_from_seconds);
        let delta = relative_seek_delta(current, duration, delta_seconds);
        let Some(session) = self.session else {
            self.controls_feedback = Some("libmpv is unavailable.".into());
            return;
        };
        self.record_control_result(session.seek_relative(delta).map(|_| ()), "seek");
    }

    pub(crate) fn seek_absolute(&mut self, target: Duration) {
        let Some(duration) = self.controls.duration().filter(|value| !value.is_zero()) else {
            self.controls_feedback =
                Some("Seeking is unavailable for live or unknown-length media.".into());
            return;
        };
        let current = self
            .controls
            .position_seconds
            .and_then(duration_from_seconds)
            .unwrap_or_default();
        let Some(target) = seek_target(current, Some(duration), SeekRequest::Absolute(target))
        else {
            self.controls_feedback = Some("The requested seek position is unavailable.".into());
            return;
        };
        let Some(session) = self.session else {
            self.controls_feedback = Some("libmpv is unavailable.".into());
            return;
        };
        self.record_control_result(
            session.seek_absolute(target.as_secs_f64()).map(|_| ()),
            "seek",
        );
    }

    /// The A-B loop points set for the current item, so the controls can label the next press
    /// (M6-12). Session-only state; cleared when the item changes.
    pub(crate) fn ab_loop_points(&self) -> (Option<Duration>, Option<Duration>) {
        (self.ab_loop.a(), self.ab_loop.b())
    }

    /// One press of the A-B control: set A, then set B, then clear both. The host keeps the
    /// points so the OSD can label the control, and mirrors them into libmpv so the player applies
    /// the loop itself (M6-12). The `L` key and the on-screen control share this path.
    pub(crate) fn cycle_ab_loop(&mut self) {
        let Some(session) = self.session else {
            self.controls_feedback = Some("libmpv is unavailable.".into());
            return;
        };
        if !self
            .controls
            .duration()
            .is_some_and(|value| !value.is_zero())
        {
            self.controls_feedback =
                Some("A-B looping is unavailable for live or unknown-length media.".into());
            return;
        }
        let position = self
            .controls
            .position_seconds
            .and_then(duration_from_seconds)
            .unwrap_or_default();
        match self.ab_loop.cycle(position) {
            AbLoopCycle::SetA(_) | AbLoopCycle::SetB(_) => {}
            AbLoopCycle::Cleared => {}
            AbLoopCycle::Rejected(error) => {
                self.controls_feedback = Some(ab_loop_error_text(error).into());
                return;
            }
        }
        let (a, b) = self.ab_loop_points();
        self.write_ab_loop(session, a, b);
    }

    /// Mirror the loop points into libmpv. `no` clears a point, a decimal number of seconds sets
    /// it; mpv accepts both on `ab-loop-a` and `ab-loop-b`.
    fn write_ab_loop(&mut self, session: &MpvSession, a: Option<Duration>, b: Option<Duration>) {
        let a_text = ab_loop_time(a);
        let b_text = ab_loop_time(b);
        let result = session
            .command(&["set", "ab-loop-a", &a_text])
            .and_then(|_| session.command(&["set", "ab-loop-b", &b_text]));
        self.record_control_result(result.map(|_| ()), "A-B loop");
    }

    pub(crate) fn set_playback_speed(&mut self, speed: PlaybackSpeed) {
        let Some(session) = self.session else {
            self.controls_feedback = Some("libmpv is unavailable.".into());
            return;
        };
        self.record_control_result(
            session.set_playback_speed(speed.multiplier()).map(|_| ()),
            "playback speed",
        );
    }

    pub(crate) fn stop_playback(&mut self) {
        let Some(session) = self.session else {
            self.controls_feedback = Some("libmpv is unavailable.".into());
            return;
        };
        if let Err(error) = self.sync_controller.reset_subtitle_delay(session) {
            self.sync_feedback = subtitle_delay_error(error);
        }
        let result = session.stop_playback().map(|_| ());
        if result.is_ok() {
            self.stop_playback_report(false);
        }
        self.record_control_result(result, "stop playback");
    }

    /// Apply one action from the egui client's remote-control path (M4-13). Each action
    /// reuses the same call the local controls make, so there is one playback owner. Stop
    /// goes through `stop_playback`, the path the Stop control uses, so the stop report is
    /// the same one a local stop sends.
    pub(crate) fn apply_remote_action(&mut self, action: RemoteAction) {
        match action {
            RemoteAction::Play => self.set_paused(false),
            RemoteAction::Pause => self.set_paused(true),
            RemoteAction::Seek { position_ms } => {
                self.seek_absolute(Duration::from_millis(position_ms));
            }
            RemoteAction::SetVolume { percent } => self.set_remote_volume(percent),
            RemoteAction::Stop => self.stop_playback(),
        }
    }

    fn set_remote_volume(&mut self, percent: u32) {
        let Some(session) = self.session else {
            self.controls_feedback = Some("libmpv is unavailable.".into());
            return;
        };
        match remote_volume_write(percent) {
            Ok(option) => {
                let result = session
                    .command(&["set", option.name, &option.value])
                    .map(|_| ());
                self.record_control_result(result, "volume");
            }
            Err(error) => {
                tracing::warn!(setting = "volume", %error, "remote volume refused by the allowlist");
                self.controls_feedback = Some("Could not apply volume: value refused.".into());
            }
        }
    }

    pub(crate) fn select_chapter(&mut self, index: usize) {
        let Some(duration) = self.controls.duration().filter(|value| !value.is_zero()) else {
            self.controls_feedback =
                Some("Chapter seeking is unavailable for live or unknown-length media.".into());
            return;
        };
        let Some(target) = chapter_seek_target(&self.controls.chapters, index, Some(duration))
        else {
            self.controls_feedback = Some("That chapter is no longer available.".into());
            return;
        };
        self.seek_absolute(target);
    }

    /// One press of the muted control (M2-16). Mute is session-only: it is never written to
    /// settings, so a later run starts unmuted.
    pub(crate) fn toggle_mute(&mut self) {
        let Some(session) = self.session else {
            self.controls_feedback = Some("libmpv is unavailable.".into());
            return;
        };
        let muted = self.controls.muted.unwrap_or(false);
        let write = match validate("mute", OptionValue::Flag(!muted)) {
            Ok(option) => option,
            Err(error) => {
                tracing::warn!(setting = "mute", %error, "mute refused by the mpv allowlist");
                self.controls_feedback = Some("Could not apply mute: value refused.".into());
                return;
            }
        };
        let result = session
            .command(&["set", write.name, &write.value])
            .map(|_| ());
        self.record_control_result(result, "mute");
    }

    /// Set the playback volume in percent, clamped to the configured boost cap. The slider
    /// and the remote-control path both end up here.
    pub(crate) fn set_volume_percent(&mut self, percent: u32, max_percent: u32) {
        let Some(session) = self.session else {
            self.controls_feedback = Some("libmpv is unavailable.".into());
            return;
        };
        let clamped = clamp_volume(percent, max_percent);
        let write = match validate("volume", OptionValue::Int(i64::from(clamped))) {
            Ok(option) => option,
            Err(error) => {
                tracing::warn!(setting = "volume", %error, "volume refused by the mpv allowlist");
                self.controls_feedback = Some("Could not apply volume: value refused.".into());
                return;
            }
        };
        let result = session
            .command(&["set", write.name, &write.value])
            .map(|_| ());
        self.record_control_result(result, "volume");
    }

    /// Move one chapter forward or back. Shares the PageUp/PageDown path so the button and the
    /// key cannot disagree.
    pub(crate) fn step_chapter_control(&mut self, forward: bool) {
        let step = if forward {
            ChapterStep::Next
        } else {
            ChapterStep::Previous
        };
        self.dispatch_playback_key(PlaybackKeyAction::Chapter(step));
    }

    fn record_control_result(&mut self, result: Result<(), impl std::fmt::Display>, action: &str) {
        self.controls_feedback = result
            .err()
            .map(|error| format!("Could not apply {action}: {error}"));
    }

    fn reset_speed_after_end(&mut self) {
        let Some(session) = self.session else {
            return;
        };
        self.record_control_result(
            session.set_playback_speed(stop_reset_speed()).map(|_| ()),
            "reset playback speed",
        );
    }

    fn reset_subtitle_state(&mut self) {
        self.sync_controller.reset();
        self.policy.reset_item();
        self.subtitle_track_id = None;
        self.subtitle_delay_seconds = None;
        self.sync_feedback =
            SyncFeedback::guidance("Playback changed; start a new subtitle-sync pair.");
    }

    fn request_track_inventory(&mut self) {
        self.track_inventory_attempts = 0;
        self.queue_track_inventory_request();
    }

    /// Apply the series' remembered audio and subtitle tracks once per item (M2-10). A movie
    /// has no series, so it applies nothing. A missing match leaves the server default in place.
    fn apply_remembered_tracks(&mut self, session: &MpvSession) {
        if self.item.remembered_tracks_applied {
            return;
        }
        let Some(series) = self.item.series_id.clone() else {
            return;
        };
        let Some(inventory) = self.track_inventory.as_ref() else {
            return;
        };
        self.item.remembered_tracks_applied = true;
        let audio = track_candidates(inventory, TrackKind::Audio);
        let subtitles = track_candidates(inventory, TrackKind::Subtitle);
        let audio_languages: Vec<TrackLanguage> =
            audio.iter().map(|(_, language)| language.clone()).collect();
        let subtitle_languages: Vec<TrackLanguage> = subtitles
            .iter()
            .map(|(_, language)| language.clone())
            .collect();
        let audio_choice = self
            .item
            .language_memory
            .choose_audio(&series, &audio_languages);
        let subtitle_choice = self
            .item
            .language_memory
            .choose_subtitle(&series, &subtitle_languages);
        if let AudioChoice::Track(index) = audio_choice
            && let Some((selection, _)) = audio.get(index)
        {
            self.apply_remembered_selection(session, TrackMenuKind::Audio, *selection);
        }
        match subtitle_choice {
            SubtitleChoice::Track(index) => {
                if let Some((selection, _)) = subtitles.get(index) {
                    self.apply_remembered_selection(session, TrackMenuKind::Subtitle, *selection);
                }
            }
            SubtitleChoice::Off => {
                self.apply_remembered_selection(
                    session,
                    TrackMenuKind::Subtitle,
                    TrackSelection::Disabled,
                );
            }
            SubtitleChoice::KeepDefault => {}
        }
    }

    /// Queue one remembered track. Unlike a viewer's pick, it is not recorded again.
    fn apply_remembered_selection(
        &mut self,
        session: &MpvSession,
        kind: TrackMenuKind,
        selection: TrackSelection,
    ) {
        let Some(inventory) = self.track_inventory.as_ref() else {
            return;
        };
        let result = match kind {
            TrackMenuKind::Audio => session.select_audio_track(inventory, selection),
            TrackMenuKind::Subtitle => session.select_subtitle_track(inventory, selection),
        };
        match result {
            // The reply refreshes the inventory through complete_track_selection.
            Ok(command_id) => self.pending_track_selection = Some((command_id.get(), kind)),
            Err(error) => {
                tracing::warn!(?error, "remembered track was not queued");
                self.track_feedback = Some(format!(
                    "Could not apply the remembered {} track.",
                    kind.label()
                ));
            }
        }
    }

    fn request_chapter_list(&mut self) {
        self.controls.chapters.clear();
        self.chapter_list_request = None;
        let Some(session) = self.session else {
            return;
        };
        match session.request_chapter_list() {
            Ok(request_id) => self.chapter_list_request = Some(request_id),
            Err(error) => {
                self.controls_feedback = Some(format!("Could not request chapters: {error}"));
            }
        }
    }

    fn complete_chapter_list(&mut self, request_id: u64, session: &MpvSession) {
        if self.chapter_list_request != Some(request_id) {
            return;
        }
        self.chapter_list_request = None;
        match session.take_chapter_list(request_id) {
            Ok(chapters) => {
                self.controls.chapters = playback_chapters(
                    chapters
                        .into_iter()
                        .map(|chapter| (chapter.title, chapter.time_seconds)),
                );
            }
            Err(error) => {
                self.controls.chapters.clear();
                self.controls_feedback = Some(format!("Could not load chapters: {error}"));
            }
        }
    }

    fn queue_track_inventory_request(&mut self) {
        let Some(session) = self.session else {
            self.track_feedback = Some("libmpv is unavailable.".into());
            return;
        };
        self.track_inventory = None;
        self.track_feedback = Some("Loading audio and subtitle tracks…".into());
        match session.request_track_inventory() {
            Ok(request_id) => {
                self.track_inventory_request = Some(request_id);
                self.track_inventory_attempts = self.track_inventory_attempts.saturating_add(1);
            }
            Err(error) => {
                self.track_inventory_request = None;
                self.track_feedback = Some(format!("Could not read the track list: {error}"));
            }
        }
    }

    fn complete_track_inventory(&mut self, request_id: u64, session: &MpvSession) {
        let result = session.take_track_inventory(request_id);
        if self.track_inventory_request != Some(request_id) {
            return;
        }
        self.track_inventory_request = None;
        match result {
            Some(Ok(inventory)) => {
                self.track_inventory = Some(inventory);
                self.track_inventory_attempts = 0;
                self.track_feedback = None;
                self.reconcile_subtitle_selection(session);
                self.apply_remembered_tracks(session);
            }
            Some(Err(TrackInventoryError::StaleSnapshot)) if self.track_inventory_attempts < 3 => {
                self.track_feedback = Some("Tracks changed while loading; refreshing…".into());
                self.queue_track_inventory_request();
            }
            Some(Err(error)) => {
                self.track_feedback = Some(format!("Could not read the track list: {error:?}"));
            }
            None => {
                self.track_feedback = Some("Track-list result was no longer available.".into());
            }
        }
    }

    fn complete_track_selection(&mut self, request_id: u64, error: i32, submission_failed: bool) {
        let Some((pending_id, kind)) = self.pending_track_selection else {
            return;
        };
        if pending_id != request_id {
            return;
        }
        self.pending_track_selection = None;
        if submission_failed || error < 0 {
            self.track_feedback = Some(format!(
                "Could not select the {} track (libmpv error {error}).",
                kind.label()
            ));
            return;
        }
        // Only a choice libmpv accepted is remembered.
        if let Some((memory_request, event)) = self.item.pending_track_memory.take()
            && memory_request == request_id
        {
            self.track_memory_events
                .push((self.item.series_id.clone(), event));
        }
        if let Some(metadata) = self.report_metadata.as_mut() {
            match kind {
                TrackMenuKind::Audio => metadata.audio_stream_index = None,
                TrackMenuKind::Subtitle => metadata.subtitle_stream_index = None,
            }
        }
        self.track_feedback = Some("Track selection updated; refreshing the list…".into());
        self.request_track_inventory();
    }

    fn select_track(&mut self, kind: TrackMenuKind, selection: TrackSelection) {
        let Some(session) = self.session else {
            self.track_feedback = Some("libmpv is unavailable.".into());
            return;
        };
        let Some(inventory) = self.track_inventory.as_ref() else {
            self.track_feedback = Some("Wait for the current track list to finish loading.".into());
            return;
        };
        if matches!(kind, TrackMenuKind::Audio)
            && let Err(error) = audio_track_gate(selection, inventory)
        {
            tracing::warn!(%error, "audio track selection rejected by the mpv allowlist");
            self.track_feedback = Some("Could not select the audio track.".into());
            return;
        }
        if matches!(kind, TrackMenuKind::Subtitle)
            && let Err(error) = subtitle_track_gate(selection, inventory)
        {
            tracing::warn!(%error, "subtitle track selection rejected by the mpv allowlist");
            self.track_feedback = Some("Could not select the subtitle track.".into());
            return;
        }
        let memory_event = track_memory_event(kind, selection, inventory);
        let result = match kind {
            TrackMenuKind::Audio => session.select_audio_track(inventory, selection),
            TrackMenuKind::Subtitle => session.select_subtitle_track(inventory, selection),
        };
        match result {
            Ok(command_id) => {
                self.pending_track_selection = Some((command_id.get(), kind));
                self.item.pending_track_memory =
                    memory_event.map(|event| (command_id.get(), event));
                self.track_feedback = Some(format!("Selecting {} track…", kind.label()));
                if matches!(kind, TrackMenuKind::Subtitle) {
                    self.policy.subtitle_user_choice = true;
                    // A viewer's choice must not be overwritten by a restore queued earlier.
                    self.policy.pending_sid_restore = None;
                }
            }
            Err(TrackSelectionError::StaleInventory) => {
                self.track_feedback =
                    Some("The track list changed; refreshing before selection…".into());
                self.request_track_inventory();
            }
            Err(error) => {
                self.track_feedback = Some(format!("Could not select the track: {error:?}"));
            }
        }
    }

    fn next_report_session(&mut self) -> Option<PlaybackReportSessionId> {
        self.next_report_generation = self.next_report_generation.checked_add(1)?;
        Some(PlaybackReportSessionId::new(self.next_report_generation))
    }

    fn apply_resume_position(&mut self, session: &MpvSession) {
        let Some(target_seconds) = self.pending_resume_seconds else {
            return;
        };
        if self
            .controls
            .position_seconds
            .is_some_and(|position| (position - target_seconds).abs() < 1.0)
        {
            self.pending_resume_seconds = None;
            return;
        }

        let Some(requested) = duration_from_seconds(target_seconds) else {
            self.pending_resume_seconds = None;
            self.controls_feedback = Some("The saved resume position is invalid.".into());
            return;
        };
        let target = match self.controls.duration() {
            Some(duration) if !duration.is_zero() => seek_target(
                Duration::ZERO,
                Some(duration),
                SeekRequest::Absolute(requested),
            )
            .unwrap_or(requested),
            _ => requested,
        };
        self.pending_resume_seconds = Some(target.as_secs_f64());

        match session.seek_absolute(target.as_secs_f64()) {
            Ok(command) => self.resume_seek_command = Some(command.get()),
            Err(error) => {
                self.pending_resume_seconds = None;
                self.controls_feedback = Some(format!(
                    "Could not apply the saved resume position: {error}"
                ));
            }
        }
    }

    fn maybe_start_playback_report(&mut self) {
        if playback_can_start_report(
            self.controls.paused,
            self.file_loaded,
            self.playback_restart_seen,
            self.pending_resume_seconds.is_some(),
        ) {
            self.start_playback_report();
        }
    }

    fn start_playback_report(&mut self) {
        if self.report_started_at.is_some() {
            return;
        }
        let (Some(session), Some(metadata)) = (self.report_session, self.report_metadata.as_ref())
        else {
            return;
        };
        let report = metadata.progress_info(self.playback_position_ticks(), self.controls.paused);
        let Some(scheduler) = self.report_scheduler.as_mut() else {
            return;
        };
        if self.report_actions.apply(
            scheduler,
            PlaybackReportInput::Snapshot {
                session,
                at: Duration::ZERO,
                report,
            },
        ) {
            self.report_started_at = Some(Instant::now());
        }
    }

    fn record_playback_seek_snapshot(&mut self) {
        let (Some(session), Some(started_at), Some(metadata)) = (
            self.report_session,
            self.report_started_at,
            self.report_metadata.as_ref(),
        ) else {
            return;
        };
        let report = metadata.progress_info(self.playback_position_ticks(), self.controls.paused);
        let Some(scheduler) = self.report_scheduler.as_mut() else {
            return;
        };
        self.report_actions.apply(
            scheduler,
            PlaybackReportInput::Seek {
                session,
                at: started_at.elapsed(),
                report,
            },
        );
    }

    fn record_playback_report_snapshot(&mut self) {
        let (Some(session), Some(started_at), Some(metadata)) = (
            self.report_session,
            self.report_started_at,
            self.report_metadata.as_ref(),
        ) else {
            return;
        };
        let report = metadata.progress_info(self.playback_position_ticks(), self.controls.paused);
        let Some(scheduler) = self.report_scheduler.as_mut() else {
            return;
        };
        self.report_actions.apply(
            scheduler,
            PlaybackReportInput::Snapshot {
                session,
                at: started_at.elapsed(),
                report,
            },
        );
    }

    fn stop_playback_report(&mut self, failed: bool) {
        if let (Some(session), Some(metadata)) =
            (self.report_session, self.report_metadata.as_ref())
        {
            let input = PlaybackReportInput::Stop {
                session,
                item_id: metadata.item_id.clone(),
                final_position: self.playback_position_ticks(),
                failed,
            };
            // With reporting off there is no lifecycle to close, so the state is cleared below.
            if let Some(scheduler) = self.report_scheduler.as_mut()
                && !self.report_actions.apply(scheduler, input)
            {
                return;
            }
        }
        self.report_session = None;
        self.report_metadata = None;
        self.report_started_at = None;
    }

    fn playback_position_ticks(&self) -> Option<JellyfinTicks> {
        position_ticks_clamped(
            self.controls.position_seconds,
            self.controls.duration_seconds,
        )
    }

    pub(crate) fn flush_report_actions(
        &mut self,
        now: Duration,
        submit: impl FnMut(PlaybackReportAction) -> Result<u64, PlaybackReportSubmitError>,
    ) {
        self.report_actions.flush(now, submit);
    }

    pub(crate) fn report_retry_delay(&self, now: Duration) -> Option<Duration> {
        self.report_actions.retry_delay(now)
    }

    pub(crate) fn has_pending_reports(&self) -> bool {
        self.report_actions.has_pending()
    }

    pub(crate) fn pending_report_counts(&self) -> ReportCounts {
        self.report_actions.counts()
    }

    pub(crate) fn report_handoff_feedback(&self) -> Option<String> {
        self.report_actions.feedback()
    }

    pub(crate) fn finish_playback_reporting(&mut self) {
        self.stop_playback_report(false);
        self.pending_report_metadata = None;
    }

    /// Debug-only stream validation must never create a reporting lifecycle on the owner's
    /// server. Configure this before loading anything; never discard an admitted lifecycle.
    #[cfg(debug_assertions)]
    pub(crate) fn reporting_enabled(&self) -> bool {
        self.reporting_enabled
    }

    #[cfg(debug_assertions)]
    pub(crate) fn disable_reporting_for_validation(&mut self) -> Result<(), &'static str> {
        if self.has_reporting_context() || self.has_pending_reports() {
            return Err("Stream-only validation must be selected before playback starts.");
        }
        self.reporting_enabled = false;
        self.report_scheduler = None;
        Ok(())
    }

    pub(crate) fn clear_reporting_for_connection_change(&mut self) -> ReportCounts {
        self.report_session = None;
        self.report_metadata = None;
        self.pending_report_metadata = None;
        self.report_started_at = None;
        self.report_scheduler = playback_report_scheduler(self.reporting_enabled);
        self.report_actions.clear_for_connection_change()
    }

    pub(crate) fn has_reporting_context(&self) -> bool {
        self.report_session.is_some()
            || self.report_metadata.is_some()
            || self.pending_report_metadata.is_some()
            || self.report_actions.has_pending()
    }

    pub(crate) fn track_menu_options(&self, kind: TrackMenuKind) -> Option<Vec<TrackMenuOption>> {
        let inventory = self.track_inventory.as_ref()?;
        let selected = match kind {
            TrackMenuKind::Audio => inventory.audio_selection(),
            TrackMenuKind::Subtitle => inventory.subtitle_selection(),
        };
        let is_selected = |selection| match (selected, selection) {
            (TrackSelectionState::Automatic, TrackSelection::Automatic)
            | (TrackSelectionState::Disabled, TrackSelection::Disabled) => true,
            (TrackSelectionState::Track(current), TrackSelection::Track(candidate)) => {
                current == candidate
            }
            _ => false,
        };
        let mut options = vec![
            TrackMenuOption {
                value: TrackSelection::Automatic,
                label: "Automatic".into(),
                selected: is_selected(TrackSelection::Automatic),
            },
            TrackMenuOption {
                value: TrackSelection::Disabled,
                label: "Off".into(),
                selected: is_selected(TrackSelection::Disabled),
            },
        ];
        for track in inventory
            .tracks()
            .iter()
            .filter(|track| track.kind == kind.track_kind())
        {
            options.push(TrackMenuOption {
                value: TrackSelection::Track(track.id),
                label: track_menu_label(track),
                selected: is_selected(TrackSelection::Track(track.id)),
            });
        }
        Some(options)
    }

    pub(crate) fn track_feedback(&self) -> Option<&str> {
        self.track_feedback.as_deref()
    }

    pub(crate) fn select_audio_track(&mut self, selection: TrackSelection) {
        self.select_track(TrackMenuKind::Audio, selection);
    }

    /// Load a subtitle file downloaded for the playing item and select it, then refresh the
    /// track list. The file is local, so libmpv still sees no server address or credential.
    pub(crate) fn add_external_subtitle(
        &mut self,
        path: &std::path::Path,
        title: Option<&str>,
        language: Option<&str>,
    ) {
        let (Some(session), Some(path)) = (self.session, path.to_str()) else {
            return;
        };
        // `sub-add <file> select [<title> [<lang>]]`; an empty title is still sent when only the
        // language is known, since the arguments are positional.
        let mut arguments = vec!["sub-add", path, "select"];
        match (
            title.filter(|t| !t.is_empty()),
            language.filter(|l| !l.is_empty()),
        ) {
            (Some(title), Some(language)) => arguments.extend([title, language]),
            (Some(title), None) => arguments.push(title),
            (None, Some(language)) => arguments.extend(["", language]),
            (None, None) => {}
        }
        match session.command(&arguments) {
            Ok(command_id) => {
                self.pending_subtitle_add = Some(command_id.get());
                // The viewer chose this subtitle; a remembered choice must not replace it.
                self.policy.subtitle_user_choice = true;
                self.policy.pending_sid_restore = None;
            }
            Err(error) => tracing::warn!(%error, "downloaded subtitle was not queued"),
        }
    }

    pub(crate) fn select_subtitle_track(&mut self, selection: TrackSelection) {
        self.select_track(TrackMenuKind::Subtitle, selection);
    }

    pub(crate) fn capture_sync_marker(&mut self, marker: CalibrationMarker) {
        if self.subtitle_track_id.is_none() {
            self.sync_feedback = SyncFeedback::guidance("Select a subtitle track before syncing.");
            return;
        }
        let Some(session) = self.session else {
            self.sync_feedback = SyncFeedback::error("libmpv is unavailable.");
            return;
        };
        match self.sync_controller.capture_marker(marker, session) {
            Ok(_) => {
                self.sync_feedback = match marker {
                    CalibrationMarker::VoiceHeard => {
                        SyncFeedback::pending("Capturing the spoken-line time…")
                    }
                    CalibrationMarker::TextSeen => {
                        SyncFeedback::pending("Capturing the subtitle time…")
                    }
                };
            }
            Err(_) => {
                let message = match marker {
                    CalibrationMarker::VoiceHeard => {
                        "Could not capture the spoken-line time. Keep playback running and try again."
                    }
                    CalibrationMarker::TextSeen => {
                        "Could not capture the subtitle time. Keep playback running and try again."
                    }
                };
                self.sync_feedback = SyncFeedback::error(message);
            }
        }
    }

    pub(crate) fn nudge_subtitle_delay(&mut self, delta_seconds: f64) {
        if self.subtitle_track_id.is_none() {
            self.sync_feedback =
                SyncFeedback::guidance("Select a subtitle track before adjusting its delay.");
            return;
        }
        let Some(session) = self.session else {
            self.sync_feedback = SyncFeedback::error("libmpv is unavailable.");
            return;
        };
        self.policy.pending_subtitle_offset = None;
        match self
            .sync_controller
            .nudge_subtitle_delay(delta_seconds, session)
        {
            Ok(()) => {
                self.sync_feedback = SyncFeedback::pending("Applying the subtitle adjustment…")
            }
            Err(error) => self.sync_feedback = subtitle_delay_error(error),
        }
    }

    /// Move the subtitle offset by one `[` or `]` step through the client-core [`SubtitleOffset`]
    /// model. The step starts from the last requested offset, so quick presses are not lost
    /// while mpv is still reporting the previous value.
    pub(crate) fn nudge_subtitle_offset(&mut self, direction: OffsetDirection, step: OffsetStep) {
        if self.subtitle_track_id.is_none() {
            self.sync_feedback =
                SyncFeedback::guidance("Select a subtitle track before adjusting its timing.");
            return;
        }
        let Some(session) = self.session else {
            self.sync_feedback = SyncFeedback::error("libmpv is unavailable.");
            return;
        };
        let base = self
            .policy
            .pending_subtitle_offset
            .unwrap_or_else(|| observed_subtitle_offset(self.subtitle_delay_seconds));
        let next = base.nudge(direction, step);
        match session.apply_subtitle_offset(next) {
            Ok(_) => {
                self.policy.pending_subtitle_offset = Some(next);
                self.sync_feedback =
                    SyncFeedback::pending(format!("Subtitle timing {}", next.osd_text()));
            }
            Err(error) => self.sync_feedback = subtitle_delay_error(error),
        }
    }

    /// Keep the secondary subtitle consistent with the primary track and the inventory, and let
    /// the one-shot re-select act on an automatically chosen track. Called after every inventory
    /// read and every change of the primary track.
    fn reconcile_subtitle_selection(&mut self, session: &MpvSession) {
        let Some(inventory) = self.track_inventory.as_ref() else {
            return;
        };
        let subtitle_tracks: Vec<(SubtitleTrackId, bool)> = inventory
            .tracks()
            .iter()
            .filter(|track| track.kind == TrackKind::Subtitle)
            .map(|track| {
                let flagged = track.default == Some(true) || track.forced == Some(true);
                (SubtitleTrackId::new(track.id.get()), flagged)
            })
            .collect();
        let ids: Vec<SubtitleTrackId> = subtitle_tracks.iter().map(|(id, _)| *id).collect();
        let primary = self.subtitle_track_id.map(SubtitleTrackId::new);

        if let Some(secondary) = self.policy.secondary_subtitle
            && reconcile_secondary(primary, Some(secondary), &ids).is_none()
        {
            self.clear_secondary_subtitle(session);
        }

        let Some(selected) = primary else {
            return;
        };
        // `subtitles.autoReselect` is not in kattri-settings yet. Its legacy default is on.
        let auto_flagged = subtitle_tracks
            .iter()
            .any(|(id, flagged)| *id == selected && *flagged);
        let origin = if self.policy.subtitle_user_choice {
            SelectionOrigin::UserChoice
        } else {
            SelectionOrigin::Automatic
        };
        let observation = ReselectObservation {
            enabled: true,
            selected: Some(selected),
            auto_flagged,
            origin,
        };
        if let Err(error) = run_subtitle_reselect(
            session,
            &mut self.policy.reselect_gate,
            &mut self.policy.pending_sid_restore,
            observation,
        ) {
            self.track_feedback = Some(error.to_string());
        }
    }

    /// Set or clear the secondary subtitle track. The request is checked against the file's
    /// inventory first; a refused request leaves the current secondary in place.
    pub(crate) fn select_secondary_subtitle(&mut self, track: Option<i64>) {
        let Some(session) = self.session else {
            self.track_feedback = Some("libmpv is unavailable.".into());
            return;
        };
        let ids = self.subtitle_inventory_ids();
        let primary = self.subtitle_track_id.map(SubtitleTrackId::new);
        let requested = track.map(SubtitleTrackId::new);
        let selection = match choose_secondary_in_inventory(primary, &ids, requested) {
            Ok(selection) => selection,
            Err(refusal) => {
                self.track_feedback = Some(secondary_refusal_message(refusal).into());
                return;
            }
        };
        match session.apply_secondary_subtitle(selection) {
            Ok(_) => {
                self.policy.secondary_subtitle = match selection {
                    SecondarySelection::Track(id) => Some(id),
                    _ => None,
                };
            }
            Err(error) => self.track_feedback = Some(secondary_error(error)),
        }
    }

    /// The secondary selector's choices: subtitle tracks other than the primary, in inventory
    /// order. Empty when the file has fewer than two subtitle tracks.
    pub(crate) fn secondary_subtitle_choices(&self) -> Vec<SecondarySubtitleChoice> {
        let Some(inventory) = self.track_inventory.as_ref() else {
            return Vec::new();
        };
        let primary = self.subtitle_track_id.map(SubtitleTrackId::new);
        secondary_candidates(primary, &self.subtitle_inventory_ids())
            .into_iter()
            .filter_map(|candidate| {
                let track = inventory
                    .tracks()
                    .iter()
                    .find(|track| track.id.get() == candidate.get())?;
                Some(SecondarySubtitleChoice {
                    track: candidate.get(),
                    title: track_menu_label(track),
                })
            })
            .collect()
    }

    /// The mpv track id now set as secondary, or `None` when it is off.
    pub(crate) fn secondary_subtitle_selected(&self) -> Option<i64> {
        self.policy.secondary_subtitle.map(SubtitleTrackId::get)
    }

    fn subtitle_inventory_ids(&self) -> Vec<SubtitleTrackId> {
        self.track_inventory
            .as_ref()
            .map(|inventory| {
                inventory
                    .tracks()
                    .iter()
                    .filter(|track| track.kind == TrackKind::Subtitle)
                    .map(|track| SubtitleTrackId::new(track.id.get()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The skipper for the current item. The host passes it to the player controls each frame.
    pub(crate) fn segment_skipper_mut(&mut self) -> &mut SegmentSkipper {
        &mut self.item.segment_skipper
    }

    /// Hand the item's fetched segments to the skipper. Segments for another item are dropped,
    /// so a late response cannot skip the wrong item (M2-08).
    pub(crate) fn load_media_segments(&mut self, item_id: &str, segments: Vec<MediaSegment>) {
        self.media_load
            .load_segments(&mut self.item, item_id, segments);
    }

    /// Choices the viewer made since the last call, each with its series (M2-10).
    pub(crate) fn take_track_memory_events(&mut self) -> Vec<(Option<String>, TrackMemoryEvent)> {
        std::mem::take(&mut self.track_memory_events)
    }

    fn clear_secondary_subtitle(&mut self, session: &MpvSession) {
        match session.apply_secondary_subtitle(SecondarySelection::Off) {
            Ok(_) => self.policy.secondary_subtitle = None,
            Err(error) => self.track_feedback = Some(secondary_error(error)),
        }
    }

    /// Write the subtitle style pairs, sending only the options whose value changed.
    #[allow(dead_code)] // Called when the subtitle style settings reach the host.
    pub(crate) fn apply_subtitle_style(&mut self, style: &SubtitleStyle) {
        let Some(session) = self.session else {
            self.sync_feedback = SyncFeedback::error("libmpv is unavailable.");
            return;
        };
        let options = match style.to_validated_mpv_options() {
            Ok(options) => options,
            Err(error) => {
                tracing::warn!(%error, "subtitle style option rejected by the mpv allowlist");
                self.controls_feedback = Some("Could not apply the subtitle settings.".into());
                return;
            }
        };
        let pairs: Vec<_> = options
            .iter()
            .map(|option| (option.name, option.value.clone()))
            .collect();
        for (name, value) in self.policy.style_writes.changed_writes(&pairs) {
            if session.command(&["set", name, &value]).is_err() {
                self.controls_feedback = Some(format!("Could not apply the subtitle {name}."));
            }
        }
    }

    /// Route one playback key to its control. Keys are ignored while no file is loaded.
    pub(crate) fn dispatch_playback_key(&mut self, action: PlaybackKeyAction) {
        if !self.file_loaded {
            return;
        }
        let Some(session) = self.session else {
            return;
        };
        match action {
            PlaybackKeyAction::Stop => self.stop_playback(),
            PlaybackKeyAction::SubtitleOffset { direction, step } => {
                self.nudge_subtitle_offset(direction, step);
            }
            PlaybackKeyAction::FrameStep { backward } => {
                let is_playing = self.controls.paused == Some(false);
                if plan_frame_step(is_playing).pause_first
                    && let Err(error) = session.set_paused(true)
                {
                    self.controls_feedback =
                        Some(format!("Could not pause for frame step: {error}"));
                    return;
                }
                self.record_control_result(session.step_frame(backward).map(|_| ()), "frame step");
            }
            PlaybackKeyAction::Chapter(step) => {
                self.record_control_result(
                    session.step_chapter(step).map(|_| ()),
                    "chapter change",
                );
            }
            PlaybackKeyAction::AbLoopCycle => {
                // The key and the OSD control share this path, so the displayed points cannot
                // drift from the loop mpv applies.
                self.cycle_ab_loop();
            }
            PlaybackKeyAction::MediaInfo => self.media_info.toggle(),
        }
    }

    /// The frame rate of the file that just loaded and whether to avoid 25 and 30 Hz, once, when
    /// refresh-rate matching is on. `None` when it is off or the rate is unknown.
    pub(crate) fn take_content_rate(&mut self) -> Option<(f64, bool)> {
        self.content_rate.take()
    }

    pub(crate) fn media_info_open(&self) -> bool {
        self.media_info.is_open()
    }

    pub(crate) fn close_media_info(&mut self) {
        self.media_info.close();
    }

    /// The rows of the media info panel, or `None` while it is closed. Until the first read of
    /// the player's properties lands, every value reads "Unavailable".
    pub(crate) fn media_info_view(&self) -> Option<MediaInfo> {
        if !self.media_info.is_open() {
            return None;
        }
        let input = match self.media_info.snapshot() {
            Some(snapshot) => {
                let (audio, subtitles) = self.track_inventory.as_ref().map_or_else(
                    || (Vec::new(), Vec::new()),
                    |inventory| {
                        (
                            inventory
                                .audio_tracks()
                                .map(MpvTrack::media_values)
                                .collect(),
                            inventory
                                .subtitle_tracks()
                                .map(MpvTrack::media_values)
                                .collect(),
                        )
                    },
                );
                media_input_from_mpv(&snapshot.stream_values(), &audio, &subtitles)
            }
            None => MediaInput::default(),
        };
        Some(MediaInfo::from_input(&input))
    }

    /// Queue the next read of the player's properties while the media info panel is open.
    fn refresh_media_info(&mut self, session: &MpvSession) {
        let now = Instant::now();
        if !self.file_loaded || !self.media_info.read_due(now) {
            return;
        }
        match session.request_stream_snapshot() {
            Ok(request_id) => self.media_info.read_queued(request_id, now),
            Err(_) => self.media_info.read_failed(now),
        }
    }

    pub(crate) fn reset_subtitle_delay(&mut self) {
        if self.subtitle_track_id.is_none() {
            self.sync_feedback =
                SyncFeedback::guidance("Select a subtitle track before resetting its delay.");
            return;
        }
        let Some(session) = self.session else {
            self.sync_feedback = SyncFeedback::error("libmpv is unavailable.");
            return;
        };
        self.policy.pending_subtitle_offset = None;
        self.sync_controller.reset();
        match self.sync_controller.reset_subtitle_delay(session) {
            Ok(()) => {
                self.sync_feedback = SyncFeedback::pending("Resetting the subtitle delay to zero…")
            }
            Err(error) => self.sync_feedback = subtitle_delay_error(error),
        }
    }

    pub(crate) fn subtitle_track_selected(&self) -> bool {
        self.subtitle_track_id.is_some()
    }

    pub(crate) fn subtitle_delay_seconds(&self) -> Option<f64> {
        self.subtitle_delay_seconds
    }

    pub(crate) fn sync_feedback(&self) -> &SyncFeedback {
        &self.sync_feedback
    }

    pub(crate) fn close(&mut self) -> Result<(), MpvError> {
        if let Some(renderer) = self.renderer.as_mut() {
            renderer.close()?;
        }
        self.renderer.take();
        self.unobserve_properties();
        Ok(())
    }

    fn unobserve_properties(&mut self) {
        let mut observations = [
            self.subtitle_track_observation.take(),
            self.subtitle_delay_observation.take(),
            self.playback_position_observation.take(),
            self.playback_paused_observation.take(),
            self.playback_duration_observation.take(),
            self.playback_speed_observation.take(),
            self.playback_volume_observation.take(),
            self.playback_mute_observation.take(),
        ];
        if let Some(session) = self.session {
            drain_observations(&mut observations, |observation| {
                let _ = session.unobserve_property(observation);
            });
        }
    }

    pub(crate) fn status(&self) -> &str {
        &self.status
    }

    pub(crate) fn take_playback_loaded(&mut self) -> Option<u64> {
        self.playback_loaded_generation.take()
    }

    pub(crate) fn take_playback_end(&mut self) -> Option<PlaybackEnd> {
        self.playback_end_reason.take().map(|reason| {
            let item_id = self.playback_end_item_id.take();
            let was_live = std::mem::take(&mut self.playback_end_was_live);
            let generation = self.playback_end_generation.take();
            let was_loaded = std::mem::take(&mut self.playback_end_was_loaded);
            let position_ticks = self.playback_end_position_ticks.take();
            PlaybackEnd {
                reason,
                item_id,
                was_live,
                generation,
                was_loaded,
                position_ticks,
            }
        })
    }

    pub(crate) fn has_renderer(&self) -> bool {
        self.renderer.is_some()
    }

    pub(crate) fn has_media(&self) -> bool {
        self.file_loaded || self.pending_report_metadata.is_some()
    }
}

impl Drop for MpvHost<'_> {
    fn drop(&mut self) {
        // The host may be replaced or dropped without the window's explicit close path.
        // Unregister even if GL teardown fails; MpvRenderer::drop handles that case separately.
        let _ = self.close();
        self.unobserve_properties();
    }
}

/// Generic over the id so the test can use plain integers; `ObservationId` cannot be built
/// outside mpv-render.
fn drain_observations<T>(observations: &mut [Option<T>], mut unobserve: impl FnMut(T)) {
    for observation in observations.iter_mut().filter_map(Option::take) {
        unobserve(observation);
    }
}

fn probe_value<T>(value: CapabilityValue<T>, property: &str) -> ProbeValue<T> {
    match value {
        CapabilityValue::Available(value) => ProbeValue::Available(value),
        CapabilityValue::Unavailable(
            CapabilityUnavailable::MalformedNode
            | CapabilityUnavailable::InvalidUtf8
            | CapabilityUnavailable::MissingTerminator
            | CapabilityUnavailable::LimitExceeded,
        ) => ProbeValue::Unavailable(ProbeWarning::MalformedProperty {
            property: property.to_owned(),
        }),
        CapabilityValue::Unavailable(_) => {
            ProbeValue::Unavailable(ProbeWarning::PropertyUnavailable {
                property: property.to_owned(),
            })
        }
    }
}

fn track_menu_label(track: &MpvTrack) -> String {
    let title = track
        .title
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .map(compact_track_text);
    let language = track
        .language
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .map(compact_track_text);
    let mut label = match (title, language) {
        (Some(title), Some(language)) if title != language => format!("{title} ({language})"),
        (Some(title), _) => title,
        (None, Some(language)) => language,
        (None, None) => track
            .codec
            .as_deref()
            .map(compact_track_text)
            .unwrap_or_else(|| "Track".into()),
    };
    if let Some(codec) = track
        .codec
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        && !label.contains(codec)
    {
        label.push_str(" · ");
        label.push_str(&compact_track_text(codec));
    }
    if let Some(channels) = track.channels.as_deref() {
        label.push_str(" · ");
        label.push_str(&compact_track_text(channels));
    }
    if let Some(rate) = track.sample_rate_hz {
        label.push_str(&format!(" · {} Hz", rate));
    }
    for (enabled, badge) in [
        (track.default == Some(true), "Default"),
        (track.forced == Some(true), "Forced"),
        (track.hearing_impaired == Some(true), "SDH"),
        (track.external == Some(true), "External"),
    ] {
        if enabled {
            label.push_str(" · ");
            label.push_str(badge);
        }
    }
    label.push_str(&format!(" · ID {}", track.id.get()));
    label
}

fn compact_track_text(value: &str) -> String {
    const MAX_CHARS: usize = 72;
    let mut chars = value.chars();
    let mut compact: String = chars.by_ref().take(MAX_CHARS).collect();
    if chars.next().is_some() {
        compact.push('…');
    }
    compact
}

fn describe_sync_update(update: SubtitleSyncUpdate) -> SyncFeedback {
    match update {
        SubtitleSyncUpdate::AwaitingOtherMarker {
            marker,
            replaced_previous,
        } => {
            let action = if replaced_previous {
                "updated"
            } else {
                "captured"
            };
            match marker {
                CalibrationMarker::VoiceHeard => SyncFeedback::guidance(format!(
                    "Spoken-line marker {action}. Now press ‘I see the subtitle’ when its text appears."
                )),
                CalibrationMarker::TextSeen => SyncFeedback::guidance(format!(
                    "Subtitle marker {action}. Now press ‘I hear the spoken line’ when it is spoken."
                )),
            }
        }
        SubtitleSyncUpdate::Applying {
            correction_seconds,
            target_delay_seconds,
        } => SyncFeedback::pending(format!(
            "Adjusting subtitle timing by {correction_seconds:+.3} s; target delay {target_delay_seconds:+.3} s…"
        )),
        SubtitleSyncUpdate::Confirmed { delay_seconds } => SyncFeedback::success(format!(
            "Subtitle delay confirmed at {delay_seconds:+.3} s."
        )),
        SubtitleSyncUpdate::Failed(failure) => SyncFeedback::error(match failure {
            SubtitleSyncFailure::PositionUnavailable { marker, .. } => match marker {
                CalibrationMarker::VoiceHeard => {
                    "Could not read the spoken-line time. Keep playback running and try again."
                }
                CalibrationMarker::TextSeen => {
                    "Could not read the subtitle time. Keep playback running and try again."
                }
            },
            SubtitleSyncFailure::CurrentDelayUnavailable { .. } => {
                "Could not read the current subtitle delay; no timing change was applied."
            }
            SubtitleSyncFailure::DelayCommandRejected { .. } => {
                "libmpv rejected the subtitle-delay change."
            }
            SubtitleSyncFailure::DelayConfirmationUnavailable { .. } => {
                "Could not confirm the subtitle-delay change; check the current delay before continuing."
            }
            SubtitleSyncFailure::DelayOutOfRange => {
                "The calculated subtitle delay is outside libmpv’s supported range."
            }
        }),
    }
}

fn subtitle_delay_error(error: SubtitleDelayError) -> SyncFeedback {
    if error.is_non_finite_input() {
        SyncFeedback::error("Subtitle delay must be finite.")
    } else {
        SyncFeedback::error("Could not queue the subtitle-delay control.")
    }
}

fn secondary_refusal_message(refusal: SecondaryRefusal) -> &'static str {
    match refusal {
        SecondaryRefusal::SelectorHidden => {
            "A second subtitle needs a file with at least two subtitle tracks."
        }
        SecondaryRefusal::SameAsPrimary => {
            "The secondary subtitle cannot be the primary subtitle track."
        }
        SecondaryRefusal::NotInInventory => "That subtitle track is not in this file.",
    }
}

/// The tracks of one kind, in inventory order, with the selection that picks each one and its
/// language for the per-series memory.
fn track_candidates(
    inventory: &TrackInventory,
    kind: TrackKind,
) -> Vec<(TrackSelection, TrackLanguage)> {
    inventory
        .tracks()
        .iter()
        .filter(|track| track.kind == kind)
        .map(|track| {
            (
                TrackSelection::Track(track.id),
                TrackLanguage::from_raw(track.language.as_deref(), track.title.as_deref()),
            )
        })
        .collect()
}

/// The memory event for a viewer's pick. Audio off and automatic selection are not language
/// choices, so they record nothing.
fn track_memory_event(
    kind: TrackMenuKind,
    selection: TrackSelection,
    inventory: &TrackInventory,
) -> Option<TrackMemoryEvent> {
    match (kind, selection) {
        (_, TrackSelection::Automatic) | (TrackMenuKind::Audio, TrackSelection::Disabled) => None,
        (TrackMenuKind::Subtitle, TrackSelection::Disabled) => Some(TrackMemoryEvent::SubtitlesOff),
        (kind, TrackSelection::Track(id)) => {
            let track = inventory.tracks().iter().find(|track| track.id == id)?;
            let language =
                TrackLanguage::from_raw(track.language.as_deref(), track.title.as_deref());
            Some(match kind {
                TrackMenuKind::Audio => TrackMemoryEvent::Audio(language),
                TrackMenuKind::Subtitle => TrackMemoryEvent::Subtitle(language),
            })
        }
    }
}

fn secondary_error(error: SubtitleSelectionError) -> String {
    format!("Could not set the secondary subtitle: {error}")
}

/// The offset that mpv's observed `sub-delay` represents, in whole milliseconds. Missing or
/// non-finite values read as zero.
fn observed_subtitle_offset(seconds: Option<f64>) -> SubtitleOffset {
    let millis = seconds
        .filter(|value| value.is_finite())
        .map_or(0, |value| (value * 1000.0).round() as i64);
    SubtitleOffset::from_millis_clamped(millis)
}

/// A playback control reached from a key press.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PlaybackKeyAction {
    Stop,
    SubtitleOffset {
        direction: OffsetDirection,
        step: OffsetStep,
    },
    FrameStep {
        backward: bool,
    },
    Chapter(ChapterStep),
    AbLoopCycle,
    /// Show or hide the media info panel (M6-14).
    MediaInfo,
}

/// Map one key to its playback control. `shift` selects the coarse subtitle step.
///
/// `X` stops playback. `[` and `]` move subtitles earlier and later by the fine step, or the
/// coarse step with Shift. `,` and `.` step one frame back and forward. Page Up and Page Down
/// jump to the next and previous chapter. `L` cycles the A-B loop. `I` shows or hides the media info
/// panel. Most follow mpv's defaults.
pub(crate) fn playback_key_action(key: Key, shift: bool) -> Option<PlaybackKeyAction> {
    let step = if shift {
        OffsetStep::Coarse
    } else {
        OffsetStep::Fine
    };
    match key {
        Key::X => Some(PlaybackKeyAction::Stop),
        Key::OpenBracket => Some(PlaybackKeyAction::SubtitleOffset {
            direction: OffsetDirection::Earlier,
            step,
        }),
        Key::CloseBracket => Some(PlaybackKeyAction::SubtitleOffset {
            direction: OffsetDirection::Later,
            step,
        }),
        Key::Comma => Some(PlaybackKeyAction::FrameStep { backward: true }),
        Key::Period => Some(PlaybackKeyAction::FrameStep { backward: false }),
        Key::PageUp => Some(PlaybackKeyAction::Chapter(ChapterStep::Next)),
        Key::PageDown => Some(PlaybackKeyAction::Chapter(ChapterStep::Previous)),
        Key::L => Some(PlaybackKeyAction::AbLoopCycle),
        Key::I => Some(PlaybackKeyAction::MediaInfo),
        _ => None,
    }
}

/// The playback actions for the pressed keys in one frame of input, in event order. Releases
/// are ignored. Repeats count, so holding a key keeps stepping.
pub(crate) fn playback_key_actions(events: &[egui::Event]) -> Vec<PlaybackKeyAction> {
    events
        .iter()
        .filter_map(|event| match event {
            egui::Event::Key {
                key,
                pressed: true,
                modifiers,
                ..
            } => {
                // Playback shortcuts are bare keys except for Shift (the documented step
                // modifier). Ctrl/Alt/Cmd chords belong to the app and must not stop playback
                // or change tracks.
                if modifiers.ctrl || modifiers.alt || modifiers.mac_cmd || modifiers.command {
                    return None;
                }
                playback_key_action(*key, modifiers.shift)
            }
            _ => None,
        })
        .collect()
}

/// The one local media file this proof opens. `arguments` are the words left after the
/// host's own flags (`--profile`) are removed.
pub(crate) fn local_media_path(arguments: Vec<OsString>) -> Result<Option<String>, String> {
    let mut arguments = arguments.into_iter();
    let Some(argument) = arguments.next() else {
        return Ok(None);
    };
    if arguments.next().is_some() {
        return Err("pass at most one local media file to this host proof".into());
    }

    let argument = argument
        .into_string()
        .map_err(|_| "the local media path must be valid UTF-8 for mpv's command API".to_owned())?;
    if argument.contains("://") || argument.starts_with("\\\\") || argument.starts_with("//") {
        return Err(
            "network URLs and network shares are not accepted by the mpv host proof".into(),
        );
    }

    let path = PathBuf::from(argument);
    let canonical = path
        .canonicalize()
        .map_err(|error| format!("could not resolve local media file: {error}"))?;
    if !canonical.is_file() {
        return Err("the mpv host proof accepts only an existing local media file".into());
    }
    Ok(Some(canonical.to_string_lossy().into_owned()))
}

#[cfg(test)]
mod tests {
    #[test]
    fn observation_cleanup_drains_each_registration_once() {
        let mut observations = [Some(1_u64), None, Some(3)];
        let mut removed = Vec::new();
        super::drain_observations(&mut observations, |id| removed.push(id));

        assert_eq!(removed, [1, 3]);
        assert!(observations.iter().all(Option::is_none));
        super::drain_observations(&mut observations, |id| removed.push(id));
        assert_eq!(removed, [1, 3]);
    }

    #[test]
    fn validation_reporting_policy_stays_disabled_when_schedulers_are_recreated() {
        assert!(super::playback_report_scheduler(true).is_some());
        assert!(super::playback_report_scheduler(false).is_none());
        // The same policy is used on connection-generation changes, not just first launch.
        assert!(super::playback_report_scheduler(false).is_none());
    }

    use super::*;

    #[test]
    fn remote_volume_writes_are_range_checked_by_the_allowlist() {
        let max = kattri_client_core::volume::VOLUME_BOOST_MAX_PERCENT;
        let low = remote_volume_write(0).expect("zero volume is allowed");
        assert_eq!((low.name, low.value.as_str()), ("volume", "0"));
        let top = remote_volume_write(max).expect("boost maximum is allowed");
        assert_eq!(top.value, max.to_string());
        assert!(remote_volume_write(max + 1).is_err());
    }

    #[test]
    fn decode_modes_map_to_checked_hwdec_writes_and_refusals_name_no_value() {
        for (mode, expected) in [
            (DecodeMode::Auto, "auto"),
            (DecodeMode::Off, "no"),
            (DecodeMode::D3d11va, "d3d11va"),
            (DecodeMode::Nvdec, "nvdec"),
            (DecodeMode::Vaapi, "vaapi"),
            (DecodeMode::Videotoolbox, "videotoolbox"),
        ] {
            assert_eq!(
                hwdec_write(hwdec_for_decode_mode(mode)).map(|option| (option.name, option.value)),
                Ok(("hwdec", expected.to_owned())),
                "{mode:?}"
            );
        }

        assert_eq!(
            hwdec_write(hwdec_for_decode_mode(DecodeMode::Vulkan)),
            Err(HwdecRefusal::NoMpvValue)
        );

        let refused = hwdec_write(Some("vaapi-egl"));
        assert_eq!(
            refused,
            Err(HwdecRefusal::Rejected(MpvOptionError::NotAllowedValue {
                name: "hwdec"
            }))
        );
        let text = refused.err().map(|refusal| refusal.to_string());
        assert_eq!(
            text.as_deref(),
            Some("mpv option `hwdec` is not an allowed value")
        );
    }

    #[test]
    fn end_file_status_reports_completion_or_the_libmpv_error_code() {
        assert_eq!(playback_end_status(0), "Playback ended");
        assert_eq!(
            playback_end_status(-12),
            "Playback ended with libmpv error -12"
        );
        assert_eq!(playback_end_status(12), "Playback ended");
    }

    #[test]
    fn only_error_end_reason_marks_playback_report_failed() {
        assert!(playback_end_failed(EndFileReason::Error));
        for reason in [
            EndFileReason::Eof,
            EndFileReason::Stop,
            EndFileReason::Quit,
            EndFileReason::Redirect,
            EndFileReason::Unknown(123),
        ] {
            assert!(!playback_end_failed(reason), "{reason:?}");
        }
    }

    #[test]
    fn replacement_keeps_the_new_relay_until_its_matching_entry_ends() {
        let mut state = MediaLoadState {
            active: Some("old relay"),
            active_entry_id: Some(11),
            pending: Some(PendingMediaLoad {
                command_id: 42,
                generation: Some(10),
                relay: Some("new relay"),
                resume_seconds: Some(8.0),
                item: ItemPlaybackState::default(),
            }),
        };
        assert!(state.ended(Some(11)));
        assert_eq!(state.active, None);
        assert_eq!(
            state.pending.as_ref().and_then(|load| load.relay),
            Some("new relay")
        );
        let started = state.started(Some(12)).expect("queued replacement");
        assert_eq!(started.resume_seconds, Some(8.0));
        assert_eq!(state.active, Some("new relay"));
        assert!(
            !state.ended(Some(11)),
            "late old end cannot close new relay"
        );
        assert_eq!(state.active, Some("new relay"));
        assert!(state.ended(Some(12)));
        assert_eq!(state.active, None);
    }

    #[test]
    fn failed_load_clears_only_its_pending_relay() {
        let mut state = MediaLoadState {
            active: Some("currently playing"),
            active_entry_id: Some(11),
            pending: Some(PendingMediaLoad {
                command_id: 42,
                generation: Some(10),
                relay: Some("queued relay"),
                resume_seconds: None,
                item: ItemPlaybackState::default(),
            }),
        };
        assert!(!state.reject_command(41));
        assert!(state.pending.is_some());
        assert!(state.reject_command(42));
        assert!(state.pending.is_none());
        assert_eq!(state.active, Some("currently playing"));
        assert_eq!(state.active_entry_id, Some(11));
    }

    #[test]
    fn segments_are_delivered_to_the_matching_pending_or_active_item() {
        let mut active = ItemPlaybackState {
            segment_item_id: Some("old".into()),
            ..Default::default()
        };
        let mut state = MediaLoadState::<()> {
            active: None,
            active_entry_id: Some(11),
            pending: Some(PendingMediaLoad {
                command_id: 42,
                generation: Some(10),
                relay: None,
                resume_seconds: None,
                item: ItemPlaybackState {
                    segment_item_id: Some("new".into()),
                    ..Default::default()
                },
            }),
        };
        state.load_segments(&mut active, "new", vec![]);
        assert_eq!(
            state
                .pending
                .as_ref()
                .and_then(|load| load.item.segment_skipper.item_id()),
            Some("new")
        );
        assert_eq!(active.segment_skipper.item_id(), None);
        state.load_segments(&mut active, "old", vec![]);
        assert_eq!(active.segment_skipper.item_id(), Some("old"));
        state.load_segments(&mut active, "unrelated", vec![]);
        assert_eq!(active.segment_skipper.item_id(), Some("old"));
        assert_eq!(
            state
                .pending
                .as_ref()
                .and_then(|load| load.item.segment_skipper.item_id()),
            Some("new")
        );
    }

    #[test]
    fn resume_target_uses_positive_ticks_and_report_start_waits_for_playing_and_seek() {
        assert_eq!(resume_position_seconds(None), None);
        assert_eq!(resume_position_seconds(Some(0)), None);
        assert_eq!(resume_position_seconds(Some(-1)), None);
        assert_eq!(resume_position_seconds(Some(80_000_000)), Some(8.0));

        assert!(playback_can_start_report(Some(false), true, true, false));
        assert!(!playback_can_start_report(Some(true), true, true, false));
        assert!(!playback_can_start_report(None, true, true, false));
        assert!(!playback_can_start_report(Some(false), false, true, false));
        assert!(!playback_can_start_report(Some(false), true, false, false));
        assert!(!playback_can_start_report(Some(false), true, true, true));
    }

    #[test]
    fn playback_reports_clamp_positions_to_known_duration() {
        assert!(
            position_ticks_clamped(Some(130.0), Some(120.0))
                == Some(JellyfinTicks::from_ticks(1_200_000_000)),
            "position above duration should clamp to the duration in ticks"
        );
        assert!(
            position_ticks_clamped(Some(0.0), Some(120.0)) == Some(JellyfinTicks::from_ticks(0)),
            "zero position should map to 0 ticks"
        );
        assert!(
            position_ticks_clamped(Some(f64::NAN), Some(120.0)).is_none(),
            "NaN position should produce no ticks"
        );
    }

    #[test]
    fn marker_button_labels_map_to_the_matching_calibration_markers() {
        assert_eq!(
            SYNC_MARKER_BUTTONS,
            [
                (CalibrationMarker::VoiceHeard, "I hear the spoken line"),
                (CalibrationMarker::TextSeen, "I see the subtitle"),
            ]
        );
    }

    #[test]
    fn first_marker_feedback_names_the_other_accessible_action() {
        let heard = describe_sync_update(SubtitleSyncUpdate::AwaitingOtherMarker {
            marker: CalibrationMarker::VoiceHeard,
            replaced_previous: false,
        });
        let (message, tone) = heard.display();
        assert_eq!(tone, SyncFeedbackTone::Guidance);
        assert!(message.contains("I see the subtitle"));

        let seen = describe_sync_update(SubtitleSyncUpdate::AwaitingOtherMarker {
            marker: CalibrationMarker::TextSeen,
            replaced_previous: true,
        });
        let (message, tone) = seen.display();
        assert_eq!(tone, SyncFeedbackTone::Guidance);
        assert!(message.contains("updated"));
        assert!(message.contains("I hear the spoken line"));
    }

    #[test]
    fn calculated_offset_and_readback_have_distinct_feedback_states() {
        let applying = describe_sync_update(SubtitleSyncUpdate::Applying {
            correction_seconds: -0.375,
            target_delay_seconds: 0.125,
        });
        let (message, tone) = applying.display();
        assert_eq!(tone, SyncFeedbackTone::Pending);
        assert!(message.contains("-0.375 s"));
        assert!(message.contains("+0.125 s"));

        let confirmed = describe_sync_update(SubtitleSyncUpdate::Confirmed {
            delay_seconds: 0.125,
        });
        let (message, tone) = confirmed.display();
        assert_eq!(tone, SyncFeedbackTone::Success);
        assert!(message.contains("confirmed"));
        assert!(message.contains("+0.125 s"));
    }

    #[test]
    fn controller_failures_are_presented_as_actionable_errors() {
        let failed = describe_sync_update(SubtitleSyncUpdate::Failed(
            SubtitleSyncFailure::PositionUnavailable {
                marker: CalibrationMarker::TextSeen,
                mpv_error: None,
            },
        ));
        let (message, tone) = failed.display();
        assert_eq!(tone, SyncFeedbackTone::Error);
        assert!(message.contains("subtitle time"));
        assert!(message.contains("Keep playback running"));
    }

    #[test]
    fn controls_snapshot_carries_observed_values_and_lifecycle_clear_resets_them() {
        let mut controls = PlaybackControlsState::default();
        controls.apply_observation(
            PlaybackProperty::TimePosition,
            PlaybackPropertyValue::TimePosition(Some(42.5)),
        );
        controls.apply_observation(
            PlaybackProperty::Duration,
            PlaybackPropertyValue::Duration(Some(300.0)),
        );
        controls.apply_observation(
            PlaybackProperty::Paused,
            PlaybackPropertyValue::Paused(Some(true)),
        );
        controls.apply_observation(
            PlaybackProperty::Speed,
            PlaybackPropertyValue::Speed(Some(1.25)),
        );
        controls.chapters.push(PlaybackChapter {
            index: 0,
            title: "Opening".into(),
            time: Duration::from_secs(12),
        });

        {
            let snapshot = controls.snapshot();
            assert_eq!(snapshot.position_seconds, Some(42.5));
            assert_eq!(snapshot.duration_seconds, Some(300.0));
            assert_eq!(snapshot.paused, Some(true));
            assert_eq!(snapshot.speed_multiplier, Some(1.25));
            assert_eq!(snapshot.chapters[0].title, "Opening");
        }

        controls.clear();
        let snapshot = controls.snapshot();
        assert_eq!(snapshot.position_seconds, None);
        assert_eq!(snapshot.duration_seconds, None);
        assert_eq!(snapshot.paused, None);
        assert_eq!(snapshot.speed_multiplier, None);
        assert!(snapshot.chapters.is_empty());
    }

    #[test]
    fn playback_end_reset_uses_normal_speed() {
        assert_eq!(stop_reset_speed(), 1.0);
    }

    #[test]
    fn invalid_property_observations_clear_only_the_matching_value() {
        let mut controls = PlaybackControlsState::default();
        controls.apply_observation(
            PlaybackProperty::Duration,
            PlaybackPropertyValue::Duration(Some(120.0)),
        );
        controls.apply_observation(
            PlaybackProperty::Speed,
            PlaybackPropertyValue::Speed(Some(1.0)),
        );
        controls.apply_observation(
            PlaybackProperty::Duration,
            PlaybackPropertyValue::Duration(Some(f64::NAN)),
        );
        controls.apply_observation(
            PlaybackProperty::Speed,
            PlaybackPropertyValue::Speed(Some(0.0)),
        );
        assert_eq!(controls.duration_seconds, None);
        assert_eq!(controls.speed_multiplier, None);
    }

    #[test]
    fn duration_conversion_rejects_unavailable_or_invalid_values() {
        assert_eq!(duration_from_seconds(0.0), Some(Duration::ZERO));
        assert_eq!(
            duration_from_seconds(1.25),
            Some(Duration::from_millis(1250))
        );
        assert_eq!(duration_from_seconds(-1.0), None);
        assert_eq!(duration_from_seconds(f64::NAN), None);
        assert_eq!(duration_from_seconds(f64::INFINITY), None);
    }

    #[test]
    fn relative_seek_delta_clamps_to_media_bounds_and_preserves_relative_seek() {
        let duration = Duration::from_secs(100);
        assert_eq!(
            relative_seek_delta(Some(Duration::from_secs(5)), duration, -10),
            -5.0
        );
        assert_eq!(
            relative_seek_delta(Some(Duration::from_secs(95)), duration, 10),
            4.0
        );
        assert_eq!(relative_seek_delta(None, duration, 10), 10.0);
    }

    #[test]
    fn chapter_choices_are_bounded_titled_and_skip_invalid_times() {
        let mut source = vec![
            (Some("Opening".into()), 12.5),
            (Some("  ".into()), 35.0),
            (Some("Invalid".into()), f64::NAN),
        ];
        source.extend((0..MAX_CONTROL_CHAPTERS + 2).map(|index| (Some(format!("C{index}")), 50.0)));

        let chapters = playback_chapters(source);
        assert_eq!(chapters.len(), MAX_CONTROL_CHAPTERS);
        assert_eq!(chapters[0].index, 0);
        assert_eq!(chapters[0].title, "Opening");
        assert_eq!(chapters[0].time, Duration::from_millis(12_500));
        assert_eq!(chapters[1].index, 1);
        assert_eq!(chapters[1].title, "Chapter 2");
        assert_eq!(chapters[2].index, 3);
        assert_eq!(
            chapter_seek_target(&chapters, chapters[2].index, Some(Duration::from_secs(100))),
            Some(Duration::from_secs(50))
        );
    }

    #[test]
    fn ab_loop_points_are_written_as_seconds_or_cleared() {
        assert_eq!(super::ab_loop_time(None), "no");
        assert_eq!(
            super::ab_loop_time(Some(Duration::from_millis(12_500))),
            "12.500"
        );
        assert_eq!(super::ab_loop_time(Some(Duration::ZERO)), "0.000");
    }

    #[test]
    fn a_refused_ab_loop_press_explains_why() {
        use kattri_client_core::ab_loop::AbLoopError;
        assert_eq!(
            super::ab_loop_error_text(AbLoopError::NotAfterA),
            "Loop point B must come after loop point A."
        );
        assert_eq!(
            super::ab_loop_error_text(AbLoopError::TooShort),
            "An A-B loop must be at least half a second long."
        );
    }

    #[test]
    fn playback_keys_map_to_their_controls() {
        assert_eq!(
            playback_key_action(Key::X, false),
            Some(PlaybackKeyAction::Stop)
        );
        assert_eq!(
            playback_key_action(Key::OpenBracket, false),
            Some(PlaybackKeyAction::SubtitleOffset {
                direction: OffsetDirection::Earlier,
                step: OffsetStep::Fine,
            })
        );
        assert_eq!(
            playback_key_action(Key::CloseBracket, true),
            Some(PlaybackKeyAction::SubtitleOffset {
                direction: OffsetDirection::Later,
                step: OffsetStep::Coarse,
            })
        );
        assert_eq!(
            playback_key_action(Key::Comma, false),
            Some(PlaybackKeyAction::FrameStep { backward: true })
        );
        assert_eq!(
            playback_key_action(Key::Period, false),
            Some(PlaybackKeyAction::FrameStep { backward: false })
        );
        assert_eq!(
            playback_key_action(Key::PageUp, false),
            Some(PlaybackKeyAction::Chapter(ChapterStep::Next))
        );
        assert_eq!(
            playback_key_action(Key::PageDown, false),
            Some(PlaybackKeyAction::Chapter(ChapterStep::Previous))
        );
        assert_eq!(
            playback_key_action(Key::L, false),
            Some(PlaybackKeyAction::AbLoopCycle)
        );
        assert_eq!(
            playback_key_action(Key::I, false),
            Some(PlaybackKeyAction::MediaInfo)
        );
        assert_eq!(playback_key_action(Key::Space, false), None);
    }

    #[test]
    fn key_events_yield_actions_only_for_presses_in_order() {
        use egui::{Event, Modifiers};
        let press = |key, modifiers| Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers,
        };
        let events = [
            press(Key::CloseBracket, Modifiers::NONE),
            Event::Key {
                key: Key::CloseBracket,
                physical_key: None,
                pressed: false,
                repeat: false,
                modifiers: Modifiers::NONE,
            },
            press(Key::OpenBracket, Modifiers::SHIFT),
            press(Key::Space, Modifiers::NONE),
        ];
        assert_eq!(
            playback_key_actions(&events),
            vec![
                PlaybackKeyAction::SubtitleOffset {
                    direction: OffsetDirection::Later,
                    step: OffsetStep::Fine,
                },
                PlaybackKeyAction::SubtitleOffset {
                    direction: OffsetDirection::Earlier,
                    step: OffsetStep::Coarse,
                },
            ]
        );
    }

    #[test]
    fn observed_offset_reads_mpv_seconds_in_whole_milliseconds() {
        assert_eq!(observed_subtitle_offset(None), SubtitleOffset::ZERO);
        assert_eq!(
            observed_subtitle_offset(Some(f64::NAN)),
            SubtitleOffset::ZERO
        );
        assert_eq!(observed_subtitle_offset(Some(-0.25)).millis(), -250);
        assert_eq!(observed_subtitle_offset(Some(0.05)).millis(), 50);
        assert_eq!(observed_subtitle_offset(Some(60.0)).millis(), 5_000);
    }

    #[test]
    fn quick_nudges_accumulate_from_the_requested_offset() {
        // Two presses before mpv reports anything: the second starts from the first.
        let first = SubtitleOffset::ZERO.nudge(OffsetDirection::Later, OffsetStep::Fine);
        let second = first.nudge(OffsetDirection::Later, OffsetStep::Fine);
        assert_eq!(second.millis(), 100);
        let back = second.nudge(OffsetDirection::Earlier, OffsetStep::Coarse);
        assert_eq!(back.millis(), -400);
    }

    #[test]
    fn chapter_seek_requires_known_duration_and_valid_index() {
        let chapters = [PlaybackChapter {
            index: 0,
            title: "Finale".into(),
            time: Duration::from_secs(120),
        }];
        assert_eq!(
            chapter_seek_target(&chapters, 0, Some(Duration::from_secs(100))),
            Some(Duration::from_secs(99))
        );
        assert_eq!(chapter_seek_target(&chapters, 0, None), None);
        assert_eq!(
            chapter_seek_target(&chapters, 1, Some(Duration::from_secs(100))),
            None
        );
    }
}

#[cfg(test)]
mod reselect_queue_tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::VecDeque;

    /// Records every attempt as (command, admitted). Each attempt consumes one script entry; an
    /// empty script admits everything, like a queue with room.
    #[derive(Default)]
    struct ScriptedQueue {
        admit: RefCell<VecDeque<bool>>,
        attempts: RefCell<Vec<(String, bool)>>,
    }

    impl ScriptedQueue {
        fn with_script(script: &[bool]) -> Self {
            Self {
                admit: RefCell::new(script.iter().copied().collect()),
                attempts: RefCell::default(),
            }
        }

        fn attempted(&self) -> Vec<(String, bool)> {
            self.attempts.borrow().clone()
        }
    }

    impl CommandQueue for ScriptedQueue {
        fn enqueue(&self, arguments: &[&str]) -> Result<(), MpvError> {
            let admitted = self.admit.borrow_mut().pop_front().unwrap_or(true);
            self.attempts
                .borrow_mut()
                .push((arguments.join(" "), admitted));
            if admitted {
                Ok(())
            } else {
                Err(MpvError::CommandQueueFull)
            }
        }
    }

    fn observation(track: i64) -> ReselectObservation {
        ReselectObservation {
            enabled: true,
            selected: Some(SubtitleTrackId::new(track)),
            auto_flagged: true,
            origin: SelectionOrigin::Automatic,
        }
    }

    fn sid(track: i64) -> ValidatedOption {
        ValidatedOption {
            name: "sid",
            value: track.to_string(),
        }
    }

    fn attempt(command: &str, admitted: bool) -> (String, bool) {
        (command.to_owned(), admitted)
    }

    #[test]
    fn both_writes_are_queued_in_order_when_the_queue_has_room() {
        let queue = ScriptedQueue::default();
        let mut gate = SubtitleReselectGate::new();
        let mut pending = None;
        let result = run_subtitle_reselect(&queue, &mut gate, &mut pending, observation(3));
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(
            queue.attempted(),
            vec![attempt("set sid no", true), attempt("set sid 3", true)]
        );
        assert_eq!(pending, None);
    }

    #[test]
    fn refused_second_write_restores_the_previous_track_at_once() {
        // The off write is admitted, the track write is refused, and the restore is admitted.
        let queue = ScriptedQueue::with_script(&[true, false, true]);
        let mut gate = SubtitleReselectGate::new();
        let mut pending = None;
        let result = run_subtitle_reselect(&queue, &mut gate, &mut pending, observation(3));
        assert!(
            matches!(
                result,
                Err(ReselectError::TrackRestored(MpvError::CommandQueueFull))
            ),
            "{result:?}"
        );
        assert_eq!(
            queue.attempted(),
            vec![
                attempt("set sid no", true),
                attempt("set sid 3", false),
                attempt("set sid 3", true),
            ]
        );
        // The last admitted write is the track, so subtitles end on, and nothing is pending.
        assert_eq!(pending, None);
    }

    #[test]
    fn refused_restore_is_kept_and_retried_until_the_queue_admits_it() {
        let queue = ScriptedQueue::with_script(&[true, false, false]);
        let mut gate = SubtitleReselectGate::new();
        let mut pending = None;
        let result = run_subtitle_reselect(&queue, &mut gate, &mut pending, observation(3));
        assert!(
            matches!(result, Err(ReselectError::RestorePending(_))),
            "{result:?}"
        );
        assert_eq!(pending, Some(sid(3)));

        // Still refused on the next frame: the restore stays pending.
        queue.admit.borrow_mut().push_back(false);
        retry_pending_restore(&queue, &mut pending);
        assert_eq!(pending, Some(sid(3)));

        // The queue has room on a later frame: the restore is sent and cleared.
        retry_pending_restore(&queue, &mut pending);
        assert_eq!(pending, None);
        assert_eq!(queue.attempted().last(), Some(&attempt("set sid 3", true)));
    }

    #[test]
    fn refused_first_write_leaves_the_track_on_and_sends_nothing_else() {
        let queue = ScriptedQueue::with_script(&[false]);
        let mut gate = SubtitleReselectGate::new();
        let mut pending = None;
        let result = run_subtitle_reselect(&queue, &mut gate, &mut pending, observation(3));
        assert!(
            matches!(result, Err(ReselectError::OffNotQueued(_))),
            "{result:?}"
        );
        assert_eq!(queue.attempted(), vec![attempt("set sid no", false)]);
        assert_eq!(pending, None);
    }

    #[test]
    fn a_user_choice_writes_nothing() {
        let queue = ScriptedQueue::default();
        let mut gate = SubtitleReselectGate::new();
        let mut pending = None;
        let mut user_choice = observation(3);
        user_choice.origin = SelectionOrigin::UserChoice;
        user_choice.auto_flagged = false;
        let result = run_subtitle_reselect(&queue, &mut gate, &mut pending, user_choice);
        assert!(result.is_ok(), "{result:?}");
        assert!(queue.attempted().is_empty());
    }

    #[test]
    fn an_item_change_drops_a_pending_restore() {
        let mut policy = PlaybackPolicyState {
            pending_sid_restore: Some(sid(3)),
            ..PlaybackPolicyState::default()
        };
        policy.reset_item();
        assert_eq!(policy.pending_sid_restore, None);
    }
}

#[cfg(test)]
mod pre_load_tests {
    use super::*;
    use kattri_client_core::mpv_options::spec_for;
    use kattri_settings::ChannelLayout;

    fn written(
        audio: &AudioSettings,
        playback: &PlaybackSettings,
        video: &VideoSettings,
    ) -> Vec<(&'static str, String)> {
        let mut out = Vec::new();
        for (setting, options) in pre_load_options(audio, playback, video) {
            let options = options.unwrap_or_else(|error| panic!("{setting}: {error}"));
            out.extend(
                options
                    .into_iter()
                    .map(|option| (option.name, option.value)),
            );
        }
        out
    }

    #[test]
    fn settings_values_reach_their_allowlisted_options() {
        let audio = AudioSettings {
            gapless: false,
            exclusive: true,
            max_volume_percent: 300,
            channels: ChannelLayout::Surround51,
            device: "alsa/default".to_owned(),
            normalize: false,
            ..AudioSettings::default()
        };
        let playback = PlaybackSettings {
            cache_mb: 16,
            demuxer_backbuffer_mb: 0,
            demuxer_readahead_secs: 1,
            frame_drop: false,
            default_speed: 1.5,
            ..PlaybackSettings::default()
        };
        let video = VideoSettings {
            deinterlace: true,
            tone_mapping: kattri_settings::ToneMapping::Hable,
            ..VideoSettings::default()
        };

        let got = written(&audio, &playback, &video);
        let expected = [
            ("demuxer-max-bytes", (16_u64 * 1024 * 1024).to_string()),
            ("demuxer-max-back-bytes", "0".to_owned()),
            ("demuxer-readahead-secs", "1".to_owned()),
            ("gapless-audio", "no".to_owned()),
            ("audio-exclusive", "yes".to_owned()),
            ("volume-max", "300".to_owned()),
            ("deinterlace", "yes".to_owned()),
            ("tone-mapping", "hable".to_owned()),
            ("audio-channels", "5.1".to_owned()),
            ("audio-device", "alsa/default".to_owned()),
            ("audio-normalize-downmix", "no".to_owned()),
            ("framedrop", "no".to_owned()),
            ("speed", "1.5".to_owned()),
        ];
        for (name, value) in expected {
            assert!(
                got.contains(&(name, value.clone())),
                "{name}={value} not in {got:?}"
            );
        }
        for (name, _) in &got {
            assert!(spec_for(name).is_some(), "{name} is not on the allowlist");
        }
    }

    #[test]
    fn default_settings_produce_valid_writes_for_every_pre_load_option() {
        let got = written(
            &AudioSettings::default(),
            &PlaybackSettings::default(),
            &VideoSettings::default(),
        );
        assert!(got.contains(&("gapless-audio", "yes".to_owned())));
        assert!(got.contains(&("volume-max", "100".to_owned())));
        assert!(got.contains(&("deinterlace", "no".to_owned())));
        assert!(got.contains(&("tone-mapping", "auto".to_owned())));
        assert!(got.contains(&("audio-channels", "stereo".to_owned())));
        assert!(got.contains(&("audio-device", "auto".to_owned())));
        assert!(got.contains(&("audio-normalize-downmix", "yes".to_owned())));
        assert!(got.contains(&("framedrop", "vo".to_owned())));
        assert!(got.contains(&("speed", "1".to_owned())));
        assert!(got.contains(&("af", String::new())));
        assert!(got.contains(&("audio-pitch-correction", "yes".to_owned())));
        assert!(got.iter().any(|(name, _)| *name == "demuxer-max-bytes"));
    }

    /// The writes the local-file path queues, flattened, failing on any refused value.
    fn local_file_written(
        video: &VideoSettings,
        subtitles: &SubtitleSettings,
    ) -> Vec<(&'static str, String)> {
        let mut out = Vec::new();
        for (setting, options) in local_file_layout_options(video, subtitles) {
            let options = options.unwrap_or_else(|error| panic!("{setting}: {error}"));
            out.extend(
                options
                    .into_iter()
                    .map(|option| (option.name, option.value)),
            );
        }
        out
    }

    #[test]
    fn local_file_queues_the_subtitle_layout_the_relay_path_queues() {
        let subtitles = SubtitleSettings {
            placement: Some("left,top".into()),
            size: 40,
            color: Some("#FF8800".into()),
            ..SubtitleSettings::default()
        };
        let got = local_file_written(&VideoSettings::default(), &subtitles);
        for expected in [
            ("sub-align-x", "left".to_owned()),
            ("sub-pos", "10".to_owned()),
            ("sub-color", "#FFFF8800".to_owned()),
        ] {
            assert!(got.contains(&expected), "{expected:?} not in {got:?}");
        }
        assert!(got.iter().any(|(name, _)| *name == "sub-scale"));
        for (name, _) in &got {
            assert!(spec_for(name).is_some(), "{name} is not on the allowlist");
        }
    }

    #[test]
    fn default_subtitle_settings_write_only_the_font_and_ass_scaling_on_local_files() {
        // The defaults (size -1, no colour, no placement, outline -1) mean "player default", so
        // mpv keeps its own values. The font and the ASS scaling choice are always written, so
        // a setting changed back to its default takes effect on the next file. The helper fails
        // the test if any default builder refuses.
        let got = local_file_written(&VideoSettings::default(), &SubtitleSettings::default());
        let subtitle_writes: Vec<_> = got
            .iter()
            .filter(|(name, _)| name.starts_with("sub-"))
            .collect();
        assert_eq!(
            subtitle_writes,
            [
                &(
                    "sub-ass-style-overrides",
                    "ScaledBorderAndShadow=yes".to_owned()
                ),
                &("sub-font", "sans-serif".to_owned())
            ],
            "unexpected subtitle writes in {got:?}"
        );
    }
}
