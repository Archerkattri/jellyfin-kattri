use std::{
    collections::{HashMap, VecDeque},
    error::Error,
    ffi::{CStr, CString, c_void},
    fmt, ptr,
    rc::Rc,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, AtomicPtr, AtomicU64, Ordering},
        mpsc::{self, Receiver, SyncSender, TrySendError},
    },
    thread::{self, JoinHandle},
};

use crate::{
    CapabilitySnapshot, MpvError, MpvLibrary, StreamSnapshot,
    ffi::{
        Api, MPV_EVENT_COMMAND_REPLY, MPV_EVENT_END_FILE, MPV_EVENT_FILE_LOADED,
        MPV_EVENT_GET_PROPERTY_REPLY, MPV_EVENT_NONE, MPV_EVENT_PLAYBACK_RESTART,
        MPV_EVENT_PROPERTY_CHANGE, MPV_EVENT_SEEK, MPV_EVENT_SHUTDOWN, MPV_EVENT_START_FILE,
        MPV_FORMAT_DOUBLE, MPV_FORMAT_FLAG, MPV_FORMAT_INT64, MPV_FORMAT_NODE,
        MPV_FORMAT_NODE_ARRAY, MPV_FORMAT_NODE_MAP, MPV_FORMAT_NONE, MPV_FORMAT_STRING,
        MpvEventEndFile, MpvEventProperty, MpvEventStartFile, MpvHandle, MpvNode,
    },
    playback_policy::{
        AB_LOOP_CYCLE_ARGUMENTS, ChapterStep, chapter_step_arguments, frame_step_command,
    },
    render::{CallbackGate, wakeup_callback},
    track::{
        MAX_TRACK_COUNT, RawTrack, TrackInventory, TrackInventoryError, TrackKind, TrackSelection,
        TrackSelectionError, parse_track_inventory, validate_inventory_identity,
        validate_selection,
    },
};
use kattri_client_core::{
    audio::audio_spdif_option,
    mpv_options::{MpvOptionError, OptionValue, validate},
    subtitle_sync::SubtitleOffset,
    subtitles::{SecondarySelection, secondary_position_option, secondary_sid_write},
};
use kattri_settings::AudioSettings;

/// `MPV_ERROR_INVALID_PARAMETER` from mpv's client.h: reported when a name or value cannot be a
/// C string, instead of calling mpv with it.
const MPV_ERROR_INVALID_PARAMETER: std::ffi::c_int = -4;

const WORKER_QUEUE_CAPACITY: usize = 128;
const MAX_IN_FLIGHT_TRACK_READS: usize = 64;
const MAX_TRACK_TEXT_BYTES: usize = 4096;
const MAX_STORED_TRACK_INVENTORIES: usize = 32;
const MAX_STORED_CAPABILITY_SNAPSHOTS: usize = 8;
const MAX_STORED_STREAM_SNAPSHOTS: usize = 4;
const MAX_PENDING_TRACK_INVENTORIES: usize = 4;
const MAX_STORED_CHAPTER_LISTS: usize = 8;
const MAX_CHAPTERS: usize = 512;
const MAX_CHAPTER_MAP_ENTRIES: usize = 16;
const MAX_CHAPTER_TITLE_BYTES: usize = 1024;
const MAX_CHAPTER_KEY_BYTES: usize = 64;
/// Playback rates offered by the M1 controls.
pub const PLAYBACK_SPEED_CHOICES: [f64; 6] = [0.5, 0.75, 1.0, 1.25, 1.5, 2.0];
static NEXT_OBSERVATION_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_ASYNC_REQUEST_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_SESSION_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct CommandId(u64);

impl CommandId {
    pub fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ObservationId(u64);

impl ObservationId {
    pub fn get(self) -> u64 {
        self.0
    }
}

/// A subtitle-delay request was invalid or could not be queued.
#[derive(Debug)]
pub enum SubtitleDelayError {
    /// mpv delay values must be finite seconds.
    NonFiniteInput,
    /// The allowlist refused the offset value. Nothing was queued.
    Rejected(MpvOptionError),
    /// The existing owner-thread command queue rejected the request.
    Mpv(MpvError),
}

/// A secondary-track, re-select, or subtitle write was refused by the allowlist or not queued.
#[derive(Debug)]
pub enum SubtitleSelectionError {
    /// The allowlist refused the value. Nothing was queued.
    Rejected(MpvOptionError),
    /// The owner-thread command queue rejected the request.
    Mpv(MpvError),
}

/// A passthrough settings write was refused or could not be queued to libmpv.
#[derive(Debug)]
pub enum AudioPassthroughError {
    /// The settings-derived value did not pass the `audio-spdif` allowlist.
    Rejected(MpvOptionError),
    /// The owner-thread command queue rejected the write.
    Mpv(MpvError),
}

impl fmt::Display for AudioPassthroughError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rejected(error) => write!(formatter, "passthrough setting was refused: {error}"),
            Self::Mpv(error) => write!(formatter, "could not queue passthrough setting: {error}"),
        }
    }
}

impl Error for AudioPassthroughError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Rejected(error) => Some(error),
            Self::Mpv(error) => Some(error),
        }
    }
}

/// A playback-control value was invalid or its command could not be queued.
#[derive(Debug)]
pub enum PlaybackControlError {
    /// A seek target/delta or speed was NaN or infinite.
    NonFiniteInput,
    /// Absolute seeks cannot target a negative timestamp.
    NegativeAbsoluteSeek,
    /// The requested speed is not one of the supported control choices.
    UnsupportedSpeed(f64),
    /// The libmpv owner-thread command queue rejected the request.
    Mpv(MpvError),
}

impl fmt::Display for PlaybackControlError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NonFiniteInput => formatter.write_str("playback control value must be finite"),
            Self::NegativeAbsoluteSeek => {
                formatter.write_str("absolute seek target cannot be negative")
            }
            Self::UnsupportedSpeed(speed) => {
                write!(formatter, "unsupported playback speed: {speed}")
            }
            Self::Mpv(error) => write!(formatter, "could not queue playback command: {error}"),
        }
    }
}

impl Error for PlaybackControlError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Mpv(error) => Some(error),
            Self::NonFiniteInput | Self::NegativeAbsoluteSeek | Self::UnsupportedSpeed(_) => None,
        }
    }
}

/// Direction for moving through [`PLAYBACK_SPEED_CHOICES`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlaybackSpeedStep {
    Previous,
    Next,
}

impl SubtitleDelayError {
    /// Whether the request was rejected because its seconds value was NaN or
    /// infinite.
    pub const fn is_non_finite_input(&self) -> bool {
        matches!(self, Self::NonFiniteInput)
    }

    /// The command-queue error, if validation succeeded but enqueueing failed.
    pub const fn mpv_error(&self) -> Option<&MpvError> {
        match self {
            Self::NonFiniteInput | Self::Rejected(_) => None,
            Self::Mpv(error) => Some(error),
        }
    }
}

impl fmt::Display for SubtitleDelayError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NonFiniteInput => {
                formatter.write_str("subtitle delay must be a finite number of seconds")
            }
            Self::Rejected(error) => {
                write!(formatter, "subtitle delay refused by allowlist: {error}")
            }
            Self::Mpv(error) => {
                write!(formatter, "could not queue subtitle delay command: {error}")
            }
        }
    }
}

impl Error for SubtitleDelayError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::NonFiniteInput => None,
            Self::Rejected(error) => Some(error),
            Self::Mpv(error) => Some(error),
        }
    }
}

impl fmt::Display for SubtitleSelectionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rejected(error) => {
                write!(formatter, "subtitle write refused by allowlist: {error}")
            }
            Self::Mpv(error) => write!(formatter, "could not queue subtitle write: {error}"),
        }
    }
}

impl Error for SubtitleSelectionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Rejected(error) => Some(error),
            Self::Mpv(error) => Some(error),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PlaybackProperty {
    TimePosition,
    Duration,
    Paused,
    Speed,
    Volume,
    /// Session mute (mpv `mute`, M2-16). Not persisted across runs.
    Mute,
    SubtitleDelay,
    SubtitleTrackId,
}

impl PlaybackProperty {
    pub const fn name(self) -> &'static str {
        match self {
            Self::TimePosition => "time-pos",
            Self::Duration => "duration",
            Self::Paused => "pause",
            Self::Speed => "speed",
            Self::Volume => "volume",
            Self::Mute => "mute",
            Self::SubtitleDelay => "sub-delay",
            Self::SubtitleTrackId => "current-tracks/sub/id",
        }
    }

    const fn format(self) -> i32 {
        match self {
            Self::TimePosition
            | Self::Duration
            | Self::Speed
            | Self::Volume
            | Self::SubtitleDelay => MPV_FORMAT_DOUBLE,
            Self::Paused | Self::Mute => MPV_FORMAT_FLAG,
            Self::SubtitleTrackId => MPV_FORMAT_INT64,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PlaybackPropertyValue {
    /// Time position in seconds. `None` means libmpv reported it unavailable.
    TimePosition(Option<f64>),
    /// Media duration in seconds. `None` means unknown or unavailable.
    Duration(Option<f64>),
    /// `None` means libmpv reported the property unavailable.
    Paused(Option<bool>),
    /// User-requested playback speed. `None` means unavailable or invalid.
    Speed(Option<f64>),
    /// The libmpv volume property value. `None` means unavailable.
    Volume(Option<f64>),
    /// Whether the session is muted. `None` means libmpv reported it unavailable.
    Muted(Option<bool>),
    /// Primary subtitle delay in seconds. `None` means unavailable or that
    /// libmpv reported a non-finite value.
    SubtitleDelay(Option<f64>),
    /// ID of the currently selected primary subtitle track. `None` means no
    /// adjustable track is selected or libmpv reports it unavailable.
    SubtitleTrackId(Option<i64>),
    /// A value format not requested by this adapter; its data is never read.
    UnsupportedFormat {
        property: PlaybackProperty,
        format: i32,
    },
    /// A successful read reply had a null payload, an invalid format marker,
    /// or a non-finite value.
    InvalidData(PlaybackProperty),
    /// The flag format was returned, but it did not contain a valid 0/1 flag.
    InvalidFlagValue(i32),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PlaybackPropertyChange {
    pub observation_id: ObservationId,
    pub property: PlaybackProperty,
    pub value: PlaybackPropertyValue,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum EventKind {
    Shutdown,
    CommandReply,
    /// Completion of an asynchronous property read. `MpvEvent` carries the
    /// request ID and libmpv error code.
    PropertyReadReply {
        property: PlaybackProperty,
        value: PlaybackPropertyValue,
    },
    /// Start and end IDs identify the same playlist entry when libmpv supplies them.
    StartFile {
        playlist_entry_id: Option<i64>,
    },
    EndFile {
        reason: EndFileReason,
        playlist_entry_id: Option<i64>,
    },
    FileLoaded,
    Seek,
    PlaybackRestart,
    PropertyChange(PlaybackPropertyChange),
    /// An owned inventory is ready for `MpvSession::take_track_inventory`.
    TrackInventoryReady,
    /// An owned runtime capability snapshot is ready to take.
    CapabilitySnapshotReady,
    /// An owned stream property snapshot is ready for `MpvSession::take_stream_snapshot`.
    StreamSnapshotReady,
    /// An owned chapter list is ready to take.
    ChapterListReady,
    CommandSubmissionFailed,
    Other(i32),
}

/// Why libmpv ended the current file. Unknown integer values stay explicit so
/// callers never mistake a new libmpv reason for natural completion.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EndFileReason {
    Eof,
    Stop,
    Quit,
    Error,
    Redirect,
    Unknown(i32),
}

#[cfg(test)]
mod end_file_reason_tests {
    use super::{EndFileReason, MpvEventEndFile, end_file_details};

    #[test]
    fn maps_known_end_reasons_without_treating_user_stop_as_eof() {
        assert_eq!(EndFileReason::from_raw(0), EndFileReason::Eof);
        assert_eq!(EndFileReason::from_raw(2), EndFileReason::Stop);
        assert_eq!(EndFileReason::from_raw(3), EndFileReason::Quit);
        assert_eq!(EndFileReason::from_raw(4), EndFileReason::Error);
        assert_eq!(EndFileReason::from_raw(5), EndFileReason::Redirect);
        assert_eq!(EndFileReason::from_raw(123), EndFileReason::Unknown(123));
    }

    #[test]
    fn end_file_uses_payload_error_for_failure_and_handles_missing_data() {
        let payload = MpvEventEndFile {
            reason: 4,
            error: -13,
            playlist_entry_id: 7,
            playlist_insert_id: 0,
            playlist_insert_num_entries: 0,
        };
        assert_eq!(
            end_file_details(Some(payload), 0),
            (EndFileReason::Error, -13)
        );
        assert_eq!(
            end_file_details(None, -20),
            (EndFileReason::Unknown(-1), -20)
        );
        assert_eq!(
            end_file_details(
                Some(MpvEventEndFile {
                    reason: 0,
                    error: 0,
                    ..payload
                }),
                -20
            ),
            (EndFileReason::Eof, 0)
        );
    }
}

fn end_file_details(payload: Option<MpvEventEndFile>, event_error: i32) -> (EndFileReason, i32) {
    match payload {
        Some(payload) => (EndFileReason::from_raw(payload.reason), payload.error),
        None => (EndFileReason::Unknown(-1), event_error),
    }
}

impl EndFileReason {
    fn from_raw(value: i32) -> Self {
        match value {
            0 => Self::Eof,
            2 => Self::Stop,
            3 => Self::Quit,
            4 => Self::Error,
            5 => Self::Redirect,
            other => Self::Unknown(other),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MpvEvent {
    pub kind: EventKind,
    pub request_id: u64,
    pub error: i32,
}

/// A chapter copied out of libmpv's borrowed node tree.
#[derive(Clone, Debug, PartialEq)]
pub struct Chapter {
    pub title: Option<String>,
    pub time_seconds: f64,
}

/// Failure while reading or validating an asynchronous chapter-list result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChapterListError {
    /// The asynchronous result is not available (yet), or was already taken.
    ResultUnavailable,
    /// libmpv rejected the property read.
    Mpv(i32),
    /// The node did not match mpv's documented chapter-list schema.
    InvalidNode,
    /// A count or string exceeded the parser's explicit bound.
    LimitExceeded,
    /// A field string was malformed or not valid UTF-8.
    InvalidString,
    /// The required chapter time was missing, non-finite, or negative.
    InvalidTime,
    /// A chapter map contained a duplicate recognized key.
    DuplicateField,
}

impl fmt::Display for ChapterListError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ResultUnavailable => formatter.write_str("chapter-list result is unavailable"),
            Self::Mpv(code) => write!(formatter, "libmpv chapter-list read failed: {code}"),
            Self::InvalidNode => {
                formatter.write_str("libmpv returned an invalid chapter-list node")
            }
            Self::LimitExceeded => {
                formatter.write_str("libmpv chapter list exceeded its safety bounds")
            }
            Self::InvalidString => formatter.write_str("libmpv returned an invalid chapter string"),
            Self::InvalidTime => formatter.write_str("libmpv returned an invalid chapter time"),
            Self::DuplicateField => {
                formatter.write_str("libmpv returned a duplicate chapter field")
            }
        }
    }
}

impl Error for ChapterListError {}

/// Owns one initialized mpv core. All normal command and event APIs run on a
/// dedicated owner thread; render APIs use the separate GL thread/context.
/// The session is !Send so its renderer remains tied to the host GL thread.
pub struct MpvSession {
    session_id: u64,
    pub(crate) core: Arc<Core>,
    worker_tx: SyncSender<WorkerMessage>,
    event_rx: Receiver<MpvEvent>,
    worker: Option<JoinHandle<()>>,
    _not_send: std::marker::PhantomData<Rc<()>>,
}

impl MpvSession {
    pub fn new(library: &MpvLibrary) -> Result<Self, MpvError> {
        Self::new_with_mode(library, SessionMode::Render)
    }

    /// Create a headless software-decode session (`hwdec=no`, `vo=null`,
    /// `ao=null`). This is for deterministic validation only; product playback
    /// uses [`MpvSession::new`] with the host render surface.
    pub fn new_software_null(library: &MpvLibrary) -> Result<Self, MpvError> {
        Self::new_with_mode(library, SessionMode::SoftwareNull)
    }

    fn new_with_mode(library: &MpvLibrary, mode: SessionMode) -> Result<Self, MpvError> {
        let session_id = NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed);
        let (worker_tx, worker_rx) = mpsc::sync_channel(WORKER_QUEUE_CAPACITY);
        let (event_tx, event_rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let api = Arc::clone(&library.api);
        let wake_sender = worker_tx.clone();
        let worker = thread::Builder::new()
            .name("libmpv-owner".to_owned())
            .spawn(move || worker_main(api, worker_rx, event_tx, ready_tx, wake_sender, mode))
            .map_err(|_| MpvError::WorkerUnavailable)?;

        match ready_rx.recv().map_err(|_| MpvError::WorkerUnavailable)? {
            Ok(core) => Ok(Self {
                session_id,
                core,
                worker_tx,
                event_rx,
                worker: Some(worker),
                _not_send: std::marker::PhantomData,
            }),
            Err(error) => {
                let _ = worker.join();
                Err(error)
            }
        }
    }

    /// Queue a copied command for the owner thread. Replies and enqueue errors
    /// arrive through `try_event`; this call does not wait on libmpv.
    pub fn command(&self, arguments: &[&str]) -> Result<CommandId, MpvError> {
        if arguments.is_empty() {
            return Err(MpvError::EmptyCommand);
        }
        let arguments = arguments
            .iter()
            .map(|argument| CString::new(*argument).map_err(|_| MpvError::CommandContainsNul))
            .collect::<Result<Vec<_>, _>>()?;
        let id = NEXT_ASYNC_REQUEST_ID.fetch_add(1, Ordering::Relaxed);
        self.worker_tx
            .try_send(WorkerMessage::Command { id, arguments })
            .map_err(|error| match error {
                TrySendError::Full(_) => MpvError::CommandQueueFull,
                TrySendError::Disconnected(_) => MpvError::WorkerUnavailable,
            })?;
        Ok(CommandId(id))
    }

    /// Queue the persisted M2-04 `audio-spdif` codec list through the settings allowlist.
    /// Call before loading a file so a device-type change takes effect on the next item.
    pub fn apply_audio_passthrough(
        &self,
        settings: &AudioSettings,
    ) -> Result<CommandId, AudioPassthroughError> {
        let option = audio_spdif_option(settings).map_err(AudioPassthroughError::Rejected)?;
        self.command(&["set", option.name, &option.value])
            .map_err(AudioPassthroughError::Mpv)
    }

    /// Queue an explicit pause state. Observe [`PlaybackProperty::Paused`] to
    /// track the applied value; the command reply is asynchronous.
    pub fn set_paused(&self, paused: bool) -> Result<CommandId, MpvError> {
        self.command(&pause_command_arguments(paused))
    }

    /// Queue a relative seek in seconds. Negative values seek backward.
    /// Duration and current position remain separate observations so callers
    /// can clamp a seek before issuing it when those values are available.
    pub fn seek_relative(&self, delta_seconds: f64) -> Result<CommandId, PlaybackControlError> {
        let arguments = relative_seek_command_arguments(delta_seconds)?;
        self.command(&[&arguments[0], &arguments[1], &arguments[2]])
            .map_err(PlaybackControlError::Mpv)
    }

    /// Queue an absolute seek in seconds. Negative timestamps are rejected.
    pub fn seek_absolute(&self, target_seconds: f64) -> Result<CommandId, PlaybackControlError> {
        let arguments = absolute_seek_command_arguments(target_seconds)?;
        self.command(&[&arguments[0], &arguments[1], &arguments[2]])
            .map_err(PlaybackControlError::Mpv)
    }

    /// Queue one of the M1 playback speed choices.
    pub fn set_playback_speed(&self, speed: f64) -> Result<CommandId, PlaybackControlError> {
        let arguments = speed_command_arguments(speed)?;
        self.command(&[&arguments[0], &arguments[1], &arguments[2]])
            .map_err(PlaybackControlError::Mpv)
    }

    /// Select the adjacent supported rate relative to the observed speed.
    /// Values outside the offered range clamp at the nearest endpoint.
    pub fn step_playback_speed(
        &self,
        current_speed: f64,
        step: PlaybackSpeedStep,
    ) -> Result<CommandId, PlaybackControlError> {
        let speed = adjacent_playback_speed(current_speed, step)?;
        self.set_playback_speed(speed)
    }

    /// Reset playback speed before stopping. Both commands are queued in order
    /// on the libmpv owner thread; this method does not wait for either reply.
    pub fn stop_playback(&self) -> Result<CommandId, PlaybackControlError> {
        let reset = stop_speed_reset_arguments();
        self.command(&reset).map_err(PlaybackControlError::Mpv)?;
        let stop = stop_command_arguments();
        self.command(&stop).map_err(PlaybackControlError::Mpv)
    }

    /// Queue an absolute primary-subtitle delay in seconds. Positive values
    /// delay subtitles; negative values advance them. This only queues work on
    /// the libmpv owner thread and returns immediately; observe `sub-delay`
    /// and consume the matching command reply to learn the eventual result.
    /// Values pass the M2 settings allowlist, including its finite ±5-second range.
    pub fn set_subtitle_delay(&self, seconds: f64) -> Result<CommandId, SubtitleDelayError> {
        let seconds = subtitle_delay_argument(seconds)?;
        self.command(&["set", "sub-delay", &seconds])
            .map_err(SubtitleDelayError::Mpv)
    }

    /// Queue a relative primary-subtitle delay change in seconds. Positive
    /// values delay subtitles; negative values advance them. This does not
    /// block waiting for libmpv or its command reply.
    pub fn nudge_subtitle_delay(
        &self,
        delta_seconds: f64,
    ) -> Result<CommandId, SubtitleDelayError> {
        let delta_seconds = subtitle_delay_argument(delta_seconds)?;
        self.command(&["add", "sub-delay", &delta_seconds])
            .map_err(SubtitleDelayError::Mpv)
    }

    /// Queue a reset of the primary-subtitle delay to zero seconds. This does
    /// not block waiting for libmpv or its command reply.
    pub fn reset_subtitle_delay(&self) -> Result<CommandId, SubtitleDelayError> {
        let zero = subtitle_delay_argument(0.0)?;
        self.command(&["set", "sub-delay", &zero])
            .map_err(SubtitleDelayError::Mpv)
    }

    /// Queue the primary subtitle offset as mpv `sub-delay` (M2-11). This is the one place the
    /// session applies a [`SubtitleOffset`]. The seconds value passes the allowlist first, so an
    /// out-of-range offset is refused and nothing is queued. Does not wait for libmpv.
    pub fn apply_subtitle_offset(
        &self,
        offset: SubtitleOffset,
    ) -> Result<CommandId, SubtitleDelayError> {
        let arguments = subtitle_offset_command_arguments(offset)?;
        self.command(&[&arguments[0], &arguments[1], &arguments[2]])
            .map_err(SubtitleDelayError::Mpv)
    }

    /// Apply a secondary subtitle selection (M2-09). `Off` writes `secondary-sid` = `no`, which is
    /// also the write for a reconciled-off secondary. `RejectedSameAsPrimary` queues nothing and
    /// returns `Ok(None)`, so the current secondary stays. Does not wait for libmpv.
    pub fn apply_secondary_subtitle(
        &self,
        selection: SecondarySelection,
    ) -> Result<Option<CommandId>, SubtitleSelectionError> {
        let Some(arguments) = secondary_sid_command_arguments(selection)? else {
            return Ok(None);
        };
        self.command(&[&arguments[0], &arguments[1], &arguments[2]])
            .map(Some)
            .map_err(SubtitleSelectionError::Mpv)
    }

    /// Queue the configured secondary subtitle position (M2-09) through the settings allowlist.
    /// An unset setting leaves mpv's default in place.
    pub fn apply_secondary_subtitle_position(
        &self,
        setting: Option<i64>,
    ) -> Result<Option<CommandId>, SubtitleSelectionError> {
        let Some(arguments) = secondary_position_command_arguments(setting)? else {
            return Ok(None);
        };
        self.command(&[&arguments[0], &arguments[1], &arguments[2]])
            .map(Some)
            .map_err(SubtitleSelectionError::Mpv)
    }

    /// Queue one frame step, forward or backward. mpv pauses playback when a step is taken, so a
    /// caller that is playing should queue a pause first. Does not wait for libmpv.
    pub fn step_frame(&self, backward: bool) -> Result<CommandId, MpvError> {
        self.command(&[frame_step_command(backward)])
    }

    /// Queue a relative chapter jump through mpv's `add chapter` command. Does not wait for
    /// libmpv; the command reply reports whether the jump happened.
    pub fn step_chapter(&self, step: ChapterStep) -> Result<CommandId, MpvError> {
        self.command(&chapter_step_arguments(step))
    }

    /// Queue one press of mpv's A-B loop cycle, which sets A, then B, then clears both.
    pub fn cycle_ab_loop(&self) -> Result<CommandId, MpvError> {
        self.command(&AB_LOOP_CYCLE_ARGUMENTS)
    }

    /// Request a fresh `time-pos` sample from libmpv. The request is queued on
    /// the owner thread and this method returns immediately. Match the returned
    /// ID against `MpvEvent::request_id` on `EventKind::PropertyReadReply`;
    /// `MpvEvent::error` reports mpv-side failures.
    pub fn request_time_position(&self) -> Result<u64, MpvError> {
        self.request_property(PlaybackProperty::TimePosition)
    }

    /// Asynchronously read a supported playback property from libmpv.
    ///
    /// The request is queued on the owner thread and this method returns
    /// immediately. Match the returned ID against
    /// `MpvEvent::request_id` on `EventKind::PropertyReadReply`.
    pub fn request_property(&self, property: PlaybackProperty) -> Result<u64, MpvError> {
        let id = NEXT_ASYNC_REQUEST_ID.fetch_add(1, Ordering::Relaxed);
        self.worker_tx
            .try_send(WorkerMessage::GetPropertyAsync { id, property })
            .map_err(map_control_enqueue_error)?;
        Ok(id)
    }

    /// Request an owned snapshot of the current audio, video, and subtitle
    /// tracks. Completion arrives as `EventKind::TrackInventoryReady`; retrieve
    /// the owned result with `take_track_inventory` using the corresponding
    /// `MpvEvent::request_id`. The request is
    /// asynchronous and rejects inventories larger than the proof's bound.
    pub fn request_track_inventory(&self) -> Result<u64, MpvError> {
        let id = NEXT_ASYNC_REQUEST_ID.fetch_add(1, Ordering::Relaxed);
        self.worker_tx
            .try_send(WorkerMessage::GetTrackInventory {
                id,
                session_id: self.session_id,
            })
            .map_err(map_control_enqueue_error)?;
        Ok(id)
    }

    /// Take the owned result signaled by `EventKind::TrackInventoryReady`.
    /// Results remain available until taken or displaced by more than 32
    /// later inventory completions.
    pub fn take_track_inventory(
        &self,
        request_id: u64,
    ) -> Option<Result<TrackInventory, TrackInventoryError>> {
        let mut results = lock(&self.core.track_inventory_results);
        let index = results
            .iter()
            .position(|(candidate, _)| *candidate == request_id)?;
        results.remove(index).map(|(_, result)| result)
    }

    /// Queue a runtime capability probe on libmpv's owner thread. Completion
    /// arrives as `CapabilitySnapshotReady`; retrieve its owned snapshot with
    /// `take_capability_snapshot` and the matching request ID.
    pub fn request_capability_snapshot(&self) -> Result<u64, MpvError> {
        let id = NEXT_ASYNC_REQUEST_ID.fetch_add(1, Ordering::Relaxed);
        self.worker_tx
            .try_send(WorkerMessage::GetCapabilitySnapshot { id })
            .map_err(map_control_enqueue_error)?;
        Ok(id)
    }

    /// Take the owned result signaled by `CapabilitySnapshotReady`. Results
    /// remain available until taken or displaced by more than eight probes.
    pub fn take_capability_snapshot(&self, request_id: u64) -> Option<CapabilitySnapshot> {
        let mut results = lock(&self.core.capability_snapshot_results);
        let index = results
            .iter()
            .position(|(candidate, _)| *candidate == request_id)?;
        results.remove(index).map(|(_, result)| result)
    }

    /// Queue a read of the playing stream's properties (container, codecs, color, decoder,
    /// audio output) on libmpv's owner thread. Completion arrives as `StreamSnapshotReady`;
    /// retrieve the snapshot with `take_stream_snapshot` and the matching request ID. Call it
    /// after `FileLoaded`: before then most of the properties are unavailable.
    pub fn request_stream_snapshot(&self) -> Result<u64, MpvError> {
        let id = NEXT_ASYNC_REQUEST_ID.fetch_add(1, Ordering::Relaxed);
        self.worker_tx
            .try_send(WorkerMessage::GetStreamSnapshot { id })
            .map_err(map_control_enqueue_error)?;
        Ok(id)
    }

    /// Take the owned result signaled by `StreamSnapshotReady`. Results remain available until
    /// taken or displaced by more than four later reads.
    pub fn take_stream_snapshot(&self, request_id: u64) -> Option<StreamSnapshot> {
        let mut results = lock(&self.core.stream_snapshot_results);
        let index = results
            .iter()
            .position(|(candidate, _)| *candidate == request_id)?;
        results.remove(index).map(|(_, result)| result)
    }

    /// Request an owned snapshot of mpv's current chapter list. Completion is
    /// signaled by `EventKind::ChapterListReady`; the result is bounded and no
    /// libmpv-owned node memory escapes the owner thread.
    pub fn request_chapter_list(&self) -> Result<u64, MpvError> {
        let id = NEXT_ASYNC_REQUEST_ID.fetch_add(1, Ordering::Relaxed);
        self.worker_tx
            .try_send(WorkerMessage::GetChapterList { id })
            .map_err(map_control_enqueue_error)?;
        Ok(id)
    }

    /// Take the owned chapter snapshot associated with a completed request.
    /// Results remain available until taken or displaced by eight later reads.
    pub fn take_chapter_list(&self, request_id: u64) -> Result<Vec<Chapter>, ChapterListError> {
        let mut results = lock(&self.core.chapter_list_results);
        let index = results
            .iter()
            .position(|(candidate, _)| *candidate == request_id)
            .ok_or(ChapterListError::ResultUnavailable)?;
        results
            .remove(index)
            .map(|(_, result)| result)
            .unwrap_or(Err(ChapterListError::ResultUnavailable))
    }

    /// Queue selection of an audio track, `auto`, or `no`. Explicit IDs must
    /// belong to this session's current inventory and be audio tracks. The
    /// owner thread rechecks the inventory generation before submitting the
    /// `aid` command.
    pub fn select_audio_track(
        &self,
        inventory: &TrackInventory,
        selection: TrackSelection,
    ) -> Result<CommandId, TrackSelectionError> {
        self.select_track(inventory, TrackKind::Audio, selection)
    }

    /// Queue selection of a subtitle track, `auto`, or `no`. Explicit IDs must
    /// belong to this session's current inventory and be subtitle tracks;
    /// external subtitle tracks retain their ordinary libmpv IDs and are
    /// selectable the same way as embedded tracks.
    pub fn select_subtitle_track(
        &self,
        inventory: &TrackInventory,
        selection: TrackSelection,
    ) -> Result<CommandId, TrackSelectionError> {
        self.select_track(inventory, TrackKind::Subtitle, selection)
    }

    fn select_track(
        &self,
        inventory: &TrackInventory,
        kind: TrackKind,
        selection: TrackSelection,
    ) -> Result<CommandId, TrackSelectionError> {
        validate_inventory_identity(
            inventory,
            self.session_id,
            self.core.track_generation.load(Ordering::Acquire),
        )?;
        let value = validate_selection(inventory, kind.clone(), selection)?;
        let request_id = NEXT_ASYNC_REQUEST_ID.fetch_add(1, Ordering::Relaxed);
        let (response_tx, response_rx) = mpsc::sync_channel(1);
        self.worker_tx
            .try_send(WorkerMessage::SelectTrack {
                request_id,
                generation: inventory.generation,
                kind,
                value,
                response: response_tx,
            })
            .map_err(|error| match error {
                TrySendError::Full(_) => TrackSelectionError::QueueFull,
                TrySendError::Disconnected(_) => TrackSelectionError::WorkerUnavailable,
            })?;
        response_rx
            .recv()
            .map_err(|_| TrackSelectionError::WorkerUnavailable)??;
        Ok(CommandId(request_id))
    }

    pub fn try_event(&self) -> Option<MpvEvent> {
        self.event_rx.try_recv().ok()
    }

    /// Observe a playback property. Registration is executed on libmpv's
    /// serialized owner thread; updates arrive as owned `PropertyChange`
    /// events from `try_event`.
    pub fn observe_property(&self, property: PlaybackProperty) -> Result<ObservationId, MpvError> {
        let raw_id = NEXT_OBSERVATION_ID
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                current.checked_add(1)
            })
            .map_err(|_| MpvError::ObservationIdExhausted)?;
        let id = ObservationId(raw_id);
        let (response_tx, response_rx) = mpsc::sync_channel(1);
        self.worker_tx
            .try_send(WorkerMessage::ObserveProperty {
                id,
                property,
                response: response_tx,
            })
            .map_err(map_control_enqueue_error)?;
        response_rx
            .recv()
            .map_err(|_| MpvError::WorkerUnavailable)??;
        Ok(id)
    }

    /// Stop one observation created by this session. The unobserve call is
    /// serialized on libmpv's owner thread and rejects unknown/stale IDs.
    pub fn unobserve_property(&self, id: ObservationId) -> Result<(), MpvError> {
        let (response_tx, response_rx) = mpsc::sync_channel(1);
        self.worker_tx
            .try_send(WorkerMessage::UnobserveProperty {
                id,
                response: response_tx,
            })
            .map_err(map_control_enqueue_error)?;
        response_rx
            .recv()
            .map_err(|_| MpvError::WorkerUnavailable)?
    }
}

fn subtitle_delay_argument(seconds: f64) -> Result<String, SubtitleDelayError> {
    if !seconds.is_finite() {
        return Err(SubtitleDelayError::NonFiniteInput);
    }
    validate("sub-delay", OptionValue::Float(seconds))
        .map(|option| option.value)
        .map_err(SubtitleDelayError::Rejected)
}

fn finite_playback_argument(value: f64) -> Result<String, PlaybackControlError> {
    if value.is_finite() {
        Ok(value.to_string())
    } else {
        Err(PlaybackControlError::NonFiniteInput)
    }
}

/// Pure part of M2-11: the `sub-delay` command for one offset. The seconds value is checked by
/// the allowlist inside `SubtitleOffset::as_mpv_seconds`.
fn subtitle_offset_command_arguments(
    offset: SubtitleOffset,
) -> Result<[String; 3], SubtitleDelayError> {
    let seconds = offset
        .as_mpv_seconds()
        .map_err(SubtitleDelayError::Rejected)?;
    Ok([
        "set".to_owned(),
        "sub-delay".to_owned(),
        seconds.to_string(),
    ])
}

/// Pure part of M2-09: the `secondary-sid` command for a selection. `None` means nothing is
/// written (the selection named the primary track).
fn secondary_sid_command_arguments(
    selection: SecondarySelection,
) -> Result<Option<[String; 3]>, SubtitleSelectionError> {
    let write = secondary_sid_write(selection).map_err(SubtitleSelectionError::Rejected)?;
    Ok(write.map(|option| ["set".to_owned(), option.name.to_owned(), option.value]))
}

/// Pure part of M2-09: the optional secondary subtitle position command.
fn secondary_position_command_arguments(
    setting: Option<i64>,
) -> Result<Option<[String; 3]>, SubtitleSelectionError> {
    let Some(option) =
        secondary_position_option(setting).map_err(SubtitleSelectionError::Rejected)?
    else {
        return Ok(None);
    };
    Ok(Some([
        "set".to_owned(),
        option.name.to_owned(),
        option.value,
    ]))
}

#[cfg(test)]
mod subtitle_selection_tests {
    use super::*;
    use kattri_client_core::{
        subtitle_sync::{OffsetDirection, OffsetStep},
        subtitles::SubtitleTrackId,
    };

    fn args(first: &str, second: &str, third: &str) -> [String; 3] {
        [first.to_owned(), second.to_owned(), third.to_owned()]
    }

    fn track(id: i64) -> SubtitleTrackId {
        SubtitleTrackId::new(id)
    }

    #[test]
    fn subtitle_offset_sends_seconds_that_the_allowlist_accepts() {
        assert_eq!(
            subtitle_offset_command_arguments(SubtitleOffset::ZERO).unwrap(),
            args("set", "sub-delay", "0")
        );
        let later = SubtitleOffset::ZERO.nudge(OffsetDirection::Later, OffsetStep::Fine);
        assert_eq!(
            subtitle_offset_command_arguments(later).unwrap(),
            args("set", "sub-delay", "0.05")
        );
        assert_eq!(
            subtitle_offset_command_arguments(SubtitleOffset::from_millis_clamped(-250)).unwrap(),
            args("set", "sub-delay", "-0.25")
        );
        // Out-of-range input is clamped to the product limit, which is inside the allowlist range.
        assert_eq!(
            subtitle_offset_command_arguments(SubtitleOffset::from_millis_clamped(-9_000)).unwrap(),
            args("set", "sub-delay", "-5")
        );
        assert!(matches!(
            subtitle_delay_argument(5.01),
            Err(SubtitleDelayError::Rejected(
                MpvOptionError::OutOfRange { .. }
            ))
        ));
        assert!(matches!(
            subtitle_delay_argument(f64::NAN),
            Err(SubtitleDelayError::NonFiniteInput)
        ));
    }

    #[test]
    fn secondary_selection_writes_no_or_the_track_id() {
        assert_eq!(
            secondary_sid_command_arguments(SecondarySelection::Off).unwrap(),
            Some(args("set", "secondary-sid", "no"))
        );
        assert_eq!(
            secondary_sid_command_arguments(SecondarySelection::Track(track(2))).unwrap(),
            Some(args("set", "secondary-sid", "2"))
        );
    }

    #[test]
    fn secondary_position_setting_is_clamped_and_allowlisted() {
        assert_eq!(
            secondary_position_command_arguments(Some(42)).unwrap(),
            Some(args("set", "secondary-sub-pos", "42"))
        );
        assert_eq!(
            secondary_position_command_arguments(Some(250)).unwrap(),
            Some(args("set", "secondary-sub-pos", "100"))
        );
        assert_eq!(secondary_position_command_arguments(None).unwrap(), None);
    }

    #[test]
    fn secondary_selection_same_as_primary_writes_nothing() {
        assert_eq!(
            secondary_sid_command_arguments(SecondarySelection::RejectedSameAsPrimary).unwrap(),
            None
        );
    }

    #[test]
    fn secondary_track_id_below_one_is_refused_before_queueing() {
        assert!(matches!(
            secondary_sid_command_arguments(SecondarySelection::Track(track(0))),
            Err(SubtitleSelectionError::Rejected(_))
        ));
    }
}

fn pause_command_arguments(paused: bool) -> [&'static str; 3] {
    ["set", "pause", if paused { "yes" } else { "no" }]
}

fn stop_speed_reset_arguments() -> [&'static str; 3] {
    ["set", "speed", "1"]
}

fn stop_command_arguments() -> [&'static str; 1] {
    ["stop"]
}

fn relative_seek_command_arguments(
    delta_seconds: f64,
) -> Result<[String; 3], PlaybackControlError> {
    Ok([
        "seek".to_owned(),
        finite_playback_argument(delta_seconds)?,
        "relative".to_owned(),
    ])
}

fn absolute_seek_command_arguments(
    target_seconds: f64,
) -> Result<[String; 3], PlaybackControlError> {
    let target = finite_playback_argument(target_seconds)?;
    if target_seconds < 0.0 {
        return Err(PlaybackControlError::NegativeAbsoluteSeek);
    }
    Ok(["seek".to_owned(), target, "absolute".to_owned()])
}

fn speed_command_arguments(speed: f64) -> Result<[String; 3], PlaybackControlError> {
    if !speed.is_finite() {
        return Err(PlaybackControlError::NonFiniteInput);
    }
    if !PLAYBACK_SPEED_CHOICES.contains(&speed) {
        return Err(PlaybackControlError::UnsupportedSpeed(speed));
    }
    Ok(["set".to_owned(), "speed".to_owned(), speed.to_string()])
}

fn adjacent_playback_speed(
    current_speed: f64,
    step: PlaybackSpeedStep,
) -> Result<f64, PlaybackControlError> {
    if !current_speed.is_finite() {
        return Err(PlaybackControlError::NonFiniteInput);
    }
    let index = match step {
        PlaybackSpeedStep::Previous => PLAYBACK_SPEED_CHOICES
            .iter()
            .rposition(|choice| *choice < current_speed)
            .unwrap_or(0),
        PlaybackSpeedStep::Next => PLAYBACK_SPEED_CHOICES
            .iter()
            .position(|choice| *choice > current_speed)
            .unwrap_or(PLAYBACK_SPEED_CHOICES.len() - 1),
    };
    Ok(PLAYBACK_SPEED_CHOICES[index])
}

fn map_control_enqueue_error(error: TrySendError<WorkerMessage>) -> MpvError {
    match error {
        TrySendError::Full(_) => MpvError::WorkerQueueFull,
        TrySendError::Disconnected(_) => MpvError::WorkerUnavailable,
    }
}

impl Drop for MpvSession {
    fn drop(&mut self) {
        // A renderer borrows the session, so its Drop runs first. If its GL
        // context was lost, it marks destruction unsafe and the owner thread
        // deliberately retains the mpv core to avoid freeing a live renderer.
        let _ = self.worker_tx.send(WorkerMessage::Shutdown);
        if self
            .worker
            .take()
            .is_some_and(|worker| worker.join().is_err())
        {
            self.core.destruction_safe.store(false, Ordering::Release);
        }
    }
}

type ChapterListResults = VecDeque<(u64, Result<Vec<Chapter>, ChapterListError>)>;

pub(crate) struct Core {
    handle: AtomicPtr<MpvHandle>,
    pub(crate) api: Arc<Api>,
    pub(crate) destruction_safe: AtomicBool,
    pub(crate) render_attached: AtomicBool,
    track_list_observed: AtomicBool,
    track_generation: AtomicU64,
    track_inventory_results: Mutex<VecDeque<(u64, Result<TrackInventory, TrackInventoryError>)>>,
    capability_snapshot_results: Mutex<VecDeque<(u64, CapabilitySnapshot)>>,
    stream_snapshot_results: Mutex<VecDeque<(u64, StreamSnapshot)>>,
    chapter_list_results: Mutex<ChapterListResults>,
    wakeup_context: Mutex<Option<Box<WakeupContext>>>,
}

impl Core {
    pub(crate) fn handle(&self) -> *mut MpvHandle {
        self.handle.load(Ordering::Acquire)
    }
}

impl Drop for Core {
    fn drop(&mut self) {
        if self.destruction_safe.load(Ordering::Acquire) {
            return;
        }
        // A failed GL teardown or owner-thread panic means libmpv may still
        // retain callback/function pointers. Leak those small owners and one
        // library reference so no pointer can outlive its backing storage.
        if let Ok(slot) = self.wakeup_context.get_mut()
            && let Some(context) = slot.take()
        {
            let _ = Box::leak(context);
        }
        let _keep_library_loaded = Arc::into_raw(Arc::clone(&self.api));
    }
}

enum WorkerMessage {
    Wake,
    Command {
        id: u64,
        arguments: Vec<CString>,
    },
    GetPropertyAsync {
        id: u64,
        property: PlaybackProperty,
    },
    GetTrackInventory {
        id: u64,
        session_id: u64,
    },
    GetCapabilitySnapshot {
        id: u64,
    },
    GetStreamSnapshot {
        id: u64,
    },
    GetChapterList {
        id: u64,
    },
    SelectTrack {
        request_id: u64,
        generation: u64,
        kind: TrackKind,
        value: String,
        response: SyncSender<Result<(), TrackSelectionError>>,
    },
    ObserveProperty {
        id: ObservationId,
        property: PlaybackProperty,
        response: SyncSender<Result<(), MpvError>>,
    },
    UnobserveProperty {
        id: ObservationId,
        response: SyncSender<Result<(), MpvError>>,
    },
    Shutdown,
}

#[derive(Clone, Copy)]
enum SessionMode {
    Render,
    SoftwareNull,
}

#[derive(Clone, Copy)]
enum TrackField {
    Id,
    Kind,
    Title,
    Language,
    Codec,
    Decoder,
    Selected,
    External,
    Default,
    Forced,
    VisualImpaired,
    HearingImpaired,
    Channels,
    SampleRate,
}

impl TrackField {
    const fn property_name(self) -> &'static str {
        match self {
            Self::Id => "id",
            Self::Kind => "type",
            Self::Title => "title",
            Self::Language => "lang",
            Self::Codec => "codec",
            Self::Decoder => "decoder",
            Self::Selected => "selected",
            Self::External => "external",
            Self::Default => "default",
            Self::Forced => "forced",
            Self::VisualImpaired => "visual-impaired",
            Self::HearingImpaired => "hearing-impaired",
            Self::Channels => "demux-channels",
            Self::SampleRate => "demux-samplerate",
        }
    }

    const fn format(self) -> i32 {
        match self {
            Self::Id | Self::SampleRate => MPV_FORMAT_INT64,
            Self::Kind
            | Self::Title
            | Self::Language
            | Self::Codec
            | Self::Decoder
            | Self::Channels => MPV_FORMAT_STRING,
            Self::Selected
            | Self::External
            | Self::Default
            | Self::Forced
            | Self::VisualImpaired
            | Self::HearingImpaired => MPV_FORMAT_FLAG,
        }
    }
}

#[derive(Clone, Copy)]
enum TrackReadTarget {
    Field { index: usize, field: TrackField },
    AudioSelection,
    SubtitleSelection,
}

struct TrackReadRequest {
    property: CString,
    format: i32,
    target: TrackReadTarget,
}

enum PendingRead {
    Playback(PlaybackProperty),
    ChapterList,
    TrackCount {
        inventory_id: u64,
    },
    TrackValue {
        inventory_id: u64,
        target: TrackReadTarget,
        property: String,
    },
}

struct PendingTrackInventory {
    session_id: u64,
    generation: u64,
    tracks: Vec<RawTrack>,
    audio_selection: Option<String>,
    subtitle_selection: Option<String>,
    queued_reads: VecDeque<TrackReadRequest>,
    in_flight: usize,
}

enum TrackReadValue {
    Unavailable,
    String(String),
    Integer(i64),
    Flag(bool),
}

fn track_read_queue(count: usize) -> VecDeque<TrackReadRequest> {
    let mut reads = VecDeque::with_capacity(count * 14 + 2);
    reads.push_back(TrackReadRequest {
        property: c"aid".to_owned(),
        format: MPV_FORMAT_STRING,
        target: TrackReadTarget::AudioSelection,
    });
    reads.push_back(TrackReadRequest {
        property: c"sid".to_owned(),
        format: MPV_FORMAT_STRING,
        target: TrackReadTarget::SubtitleSelection,
    });
    let fields = [
        TrackField::Id,
        TrackField::Kind,
        TrackField::Title,
        TrackField::Language,
        TrackField::Codec,
        TrackField::Decoder,
        TrackField::Selected,
        TrackField::External,
        TrackField::Default,
        TrackField::Forced,
        TrackField::VisualImpaired,
        TrackField::HearingImpaired,
        TrackField::Channels,
        TrackField::SampleRate,
    ];
    for index in 0..count {
        for field in fields {
            // The names are ASCII built here, so a NUL cannot occur; skip rather than panic.
            let Ok(property) =
                CString::new(format!("track-list/{index}/{}", field.property_name()))
            else {
                continue;
            };
            reads.push_back(TrackReadRequest {
                property,
                format: field.format(),
                target: TrackReadTarget::Field { index, field },
            });
        }
    }
    reads
}

fn decode_track_read_value(
    property: &str,
    expected_format: i32,
    error: i32,
    event_data: *mut c_void,
) -> Result<TrackReadValue, TrackInventoryError> {
    if error < 0 || event_data.is_null() {
        return Ok(TrackReadValue::Unavailable);
    }

    // SAFETY: GET_PROPERTY_REPLY data is mpv_event_property* according to the
    // documented client API; this copy occurs before the next mpv_wait_event.
    let property_event = unsafe { &*event_data.cast::<MpvEventProperty>() };
    if property_event.format == MPV_FORMAT_NONE {
        return Ok(TrackReadValue::Unavailable);
    }
    if property_event.format != expected_format {
        return Err(TrackInventoryError::UnexpectedPropertyFormat {
            property: property.to_owned(),
            format: property_event.format,
        });
    }
    if property_event.data.is_null() {
        return Ok(TrackReadValue::Unavailable);
    }

    match expected_format {
        MPV_FORMAT_STRING => {
            // SAFETY: The client header specifies MPV_FORMAT_STRING event data
            // as a pointer to a char*; its C string is event-owned and copied
            // before the next wait. The value is lossy UTF-8 because mpv tags
            // are not guaranteed to use UTF-8 on every platform/container.
            let value = unsafe {
                ptr::read_unaligned(property_event.data.cast::<*const std::ffi::c_char>())
            };
            if value.is_null() {
                return Ok(TrackReadValue::Unavailable);
            }
            // SAFETY: libmpv supplies a NUL-terminated string for this format.
            let bytes = unsafe { CStr::from_ptr(value) }.to_bytes();
            let bounded = &bytes[..bytes.len().min(MAX_TRACK_TEXT_BYTES)];
            Ok(TrackReadValue::String(
                String::from_utf8_lossy(bounded).into_owned(),
            ))
        }
        MPV_FORMAT_INT64 => {
            // SAFETY: The returned format was checked and data is non-null.
            let value = unsafe { ptr::read_unaligned(property_event.data.cast::<i64>()) };
            Ok(TrackReadValue::Integer(value))
        }
        MPV_FORMAT_FLAG => {
            // SAFETY: MPV_FORMAT_FLAG is a C int, as declared in client.h.
            let value =
                unsafe { ptr::read_unaligned(property_event.data.cast::<std::ffi::c_int>()) };
            match value {
                0 => Ok(TrackReadValue::Flag(false)),
                1 => Ok(TrackReadValue::Flag(true)),
                _ => Err(TrackInventoryError::InvalidPropertyValue {
                    property: property.to_owned(),
                }),
            }
        }
        format => Err(TrackInventoryError::UnexpectedPropertyFormat {
            property: property.to_owned(),
            format,
        }),
    }
}

fn apply_track_read(
    pending: &mut PendingTrackInventory,
    target: TrackReadTarget,
    value: TrackReadValue,
) {
    match target {
        TrackReadTarget::AudioSelection => {
            pending.audio_selection = match value {
                TrackReadValue::String(value) => Some(value),
                TrackReadValue::Unavailable => None,
                TrackReadValue::Integer(_) | TrackReadValue::Flag(_) => None,
            };
        }
        TrackReadTarget::SubtitleSelection => {
            pending.subtitle_selection = match value {
                TrackReadValue::String(value) => Some(value),
                TrackReadValue::Unavailable => None,
                TrackReadValue::Integer(_) | TrackReadValue::Flag(_) => None,
            };
        }
        TrackReadTarget::Field { index, field } => {
            let Some(track) = pending.tracks.get_mut(index) else {
                return;
            };
            match (field, value) {
                (TrackField::Id, TrackReadValue::Integer(value)) => track.id = Some(value),
                (TrackField::Kind, TrackReadValue::String(value)) => track.kind = Some(value),
                (TrackField::Title, TrackReadValue::String(value)) => track.title = Some(value),
                (TrackField::Language, TrackReadValue::String(value)) => {
                    track.language = Some(value);
                }
                (TrackField::Codec, TrackReadValue::String(value)) => track.codec = Some(value),
                (TrackField::Decoder, TrackReadValue::String(value)) => {
                    track.decoder = Some(value);
                }
                (TrackField::Selected, TrackReadValue::Flag(value)) => {
                    track.selected = Some(value);
                }
                (TrackField::External, TrackReadValue::Flag(value)) => {
                    track.external = Some(value);
                }
                (TrackField::Default, TrackReadValue::Flag(value)) => {
                    track.default = Some(value);
                }
                (TrackField::Forced, TrackReadValue::Flag(value)) => track.forced = Some(value),
                (TrackField::VisualImpaired, TrackReadValue::Flag(value)) => {
                    track.visual_impaired = Some(value);
                }
                (TrackField::HearingImpaired, TrackReadValue::Flag(value)) => {
                    track.hearing_impaired = Some(value);
                }
                (TrackField::Channels, TrackReadValue::String(value)) => {
                    track.channels = Some(value);
                }
                (TrackField::SampleRate, TrackReadValue::Integer(value)) => {
                    track.sample_rate_hz = Some(value);
                }
                (_, TrackReadValue::Unavailable) => {}
                _ => {}
            }
        }
    }
}

struct WakeupContext {
    gate: CallbackGate,
    worker_tx: SyncSender<WorkerMessage>,
    wake_pending: AtomicBool,
}

fn worker_main(
    api: Arc<Api>,
    worker_rx: Receiver<WorkerMessage>,
    event_tx: mpsc::Sender<MpvEvent>,
    ready_tx: SyncSender<Result<Arc<Core>, MpvError>>,
    wake_sender: SyncSender<WorkerMessage>,
    mode: SessionMode,
) {
    // SAFETY: Function pointers and argument types are loaded from the matching
    // libmpv C client API. All calls taking this handle remain on this thread,
    // apart from the separate render API, which is permitted by libmpv's rules.
    let handle = unsafe { (api.functions.create)() };
    if handle.is_null() {
        let _ = ready_tx.send(Err(MpvError::CreateFailed));
        return;
    }
    let init_options: &[(&CStr, &CStr)] = match mode {
        SessionMode::Render => &[
            (c"config", c"no"),
            (c"load-scripts", c"no"),
            (c"vo", c"libmpv"),
            // Do not let either mpv's libcurl backend or FFmpeg's HTTP backend
            // forward playback headers through an unvalidated redirect.
            (c"curl-max-redirects", c"0"),
            (c"stream-lavf-o", c"max_redirects=0"),
            (c"demuxer-lavf-o", c"max_redirects=0"),
        ],
        SessionMode::SoftwareNull => &[
            (c"config", c"no"),
            (c"load-scripts", c"no"),
            (c"vo", c"null"),
            (c"ao", c"null"),
            (c"hwdec", c"no"),
            (c"pause", c"yes"),
            (c"curl-max-redirects", c"0"),
            (c"stream-lavf-o", c"max_redirects=0"),
            (c"demuxer-lavf-o", c"max_redirects=0"),
        ],
    };
    for &(option_name, option_value) in init_options {
        // SAFETY: handle is a new, uninitialized mpv instance; the option
        // strings are static NUL-terminated C strings and mpv copies them.
        let option_result = unsafe {
            (api.functions.set_option_string)(handle, option_name.as_ptr(), option_value.as_ptr())
        };
        if option_result < 0 {
            // SAFETY: the handle was created above and may be destroyed before init.
            unsafe { (api.functions.terminate_destroy)(handle) };
            let option = option_name.to_string_lossy();
            let value = option_value.to_string_lossy();
            let _ = ready_tx.send(Err(MpvError::SetOptionFailed {
                option: match (option.as_ref(), value.as_ref()) {
                    ("config", "no") => "config=no",
                    ("load-scripts", "no") => "load-scripts=no",
                    ("vo", "libmpv") => "vo=libmpv",
                    ("vo", "null") => "vo=null",
                    ("ao", "null") => "ao=null",
                    ("hwdec", "no") => "hwdec=no",
                    ("pause", "yes") => "pause=yes",
                    ("curl-max-redirects", "0") => "curl-max-redirects=0",
                    ("stream-lavf-o", "max_redirects=0") => "stream-lavf-o=max_redirects=0",
                    ("demuxer-lavf-o", "max_redirects=0") => "demuxer-lavf-o=max_redirects=0",
                    _ => "unknown mpv initialization option",
                },
                code: option_result,
            }));
            return;
        }
    }
    // Debug builds only: with KATTRI_DEBUG_DIR set, mpv writes a verbose log to mpv.log there so
    // playback problems can be read after the fact. Release builds do not contain this.
    #[cfg(debug_assertions)]
    if let Some(dir) = std::env::var_os("KATTRI_DEBUG_DIR") {
        let log_path = std::path::Path::new(&dir).join("mpv.log");
        for (name, value) in [
            ("log-file", log_path.to_string_lossy().into_owned()),
            ("msg-level", "all=v".to_owned()),
        ] {
            if let (Ok(name), Ok(value)) =
                (std::ffi::CString::new(name), std::ffi::CString::new(value))
            {
                // SAFETY: handle is a new, uninitialized mpv instance; mpv copies both strings.
                unsafe {
                    (api.functions.set_option_string)(handle, name.as_ptr(), value.as_ptr());
                }
            }
        }
    }
    // SAFETY: handle remains owned by this worker; initialization is serialized
    // before callbacks or commands are enabled.
    let init_result = unsafe { (api.functions.initialize)(handle) };
    if init_result < 0 {
        // SAFETY: mpv_terminate_destroy is valid after a failed initialize.
        unsafe { (api.functions.terminate_destroy)(handle) };
        let _ = ready_tx.send(Err(MpvError::InitializeFailed(init_result)));
        return;
    }

    // MPV_FORMAT_NONE requests invalidation-only notifications, avoiding a
    // native node snapshot. ID 0 is reserved here; public observation IDs start
    // at 1. The flag is retained so track selection can fail closed if the
    // runtime could not install this guard.
    // SAFETY: The initialized handle is owned by this worker; the property name
    // and format match the documented mpv_observe_property signature.
    let track_list_observed = unsafe {
        (api.functions.observe_property)(handle, 0, c"track-list".as_ptr(), MPV_FORMAT_NONE) >= 0
    };

    let core = Arc::new(Core {
        handle: AtomicPtr::new(handle),
        api: Arc::clone(&api),
        destruction_safe: AtomicBool::new(true),
        render_attached: AtomicBool::new(false),
        track_list_observed: AtomicBool::new(track_list_observed),
        track_generation: AtomicU64::new(1),
        track_inventory_results: Mutex::new(VecDeque::new()),
        capability_snapshot_results: Mutex::new(VecDeque::new()),
        stream_snapshot_results: Mutex::new(VecDeque::new()),
        chapter_list_results: Mutex::new(VecDeque::new()),
        wakeup_context: Mutex::new(None),
    });
    let wakeup_pointer = {
        let mut slot = lock(&core.wakeup_context);
        let context = slot.insert(Box::new(WakeupContext {
            gate: CallbackGate::default(),
            worker_tx: wake_sender,
            wake_pending: AtomicBool::new(false),
        }));
        (&mut **context as *mut WakeupContext).cast::<c_void>()
    };
    // SAFETY: The callback context is a stable Box owned by Core and remains
    // alive until after callback removal and mpv_terminate_destroy.
    unsafe {
        (api.functions.set_wakeup_callback)(handle, Some(wakeup), wakeup_pointer);
    }
    if ready_tx.send(Ok(Arc::clone(&core))).is_err() {
        core.destruction_safe.store(false, Ordering::Release);
        return;
    }

    let mut observations = HashMap::new();
    let mut pending_reads = HashMap::new();
    let mut pending_track_inventories = HashMap::new();
    loop {
        match worker_rx.recv() {
            Ok(WorkerMessage::Wake) => drain_events(
                &api,
                handle,
                &event_tx,
                &core,
                &observations,
                &mut pending_reads,
                &mut pending_track_inventories,
            ),
            Ok(WorkerMessage::Command { id, arguments }) => {
                let mut pointers: Vec<*const std::ffi::c_char> =
                    arguments.iter().map(|argument| argument.as_ptr()).collect();
                pointers.push(ptr::null());
                // SAFETY: arguments and pointer array remain alive through this
                // call; mpv_command_async copies the argument strings.
                let result =
                    unsafe { (api.functions.command_async)(handle, id, pointers.as_ptr()) };
                if result < 0 {
                    let _ = event_tx.send(MpvEvent {
                        kind: EventKind::CommandSubmissionFailed,
                        request_id: id,
                        error: result,
                    });
                }
                drain_events(
                    &api,
                    handle,
                    &event_tx,
                    &core,
                    &observations,
                    &mut pending_reads,
                    &mut pending_track_inventories,
                );
            }
            Ok(WorkerMessage::GetPropertyAsync { id, property }) => {
                // SAFETY: This uses the current client.h signature on the
                // serialized owner thread. mpv copies the name and reports the
                // result later through MPV_EVENT_GET_PROPERTY_REPLY.
                let code = match CString::new(property.name()) {
                    Ok(name) => unsafe {
                        (api.functions.get_property_async)(
                            handle,
                            id,
                            name.as_ptr(),
                            property.format(),
                        )
                    },
                    // The names are static ASCII, so this is unreachable in practice.
                    Err(_) => MPV_ERROR_INVALID_PARAMETER,
                };
                if code < 0 {
                    let _ = event_tx.send(MpvEvent {
                        kind: EventKind::PropertyReadReply {
                            property,
                            value: unavailable_property_value(property),
                        },
                        request_id: id,
                        error: code,
                    });
                } else {
                    pending_reads.insert(id, PendingRead::Playback(property));
                }
                drain_events(
                    &api,
                    handle,
                    &event_tx,
                    &core,
                    &observations,
                    &mut pending_reads,
                    &mut pending_track_inventories,
                );
            }
            Ok(WorkerMessage::GetTrackInventory { id, session_id }) => {
                // Flush already-queued track-list invalidations before taking
                // the generation stamped onto this asynchronous snapshot.
                drain_events(
                    &api,
                    handle,
                    &event_tx,
                    &core,
                    &observations,
                    &mut pending_reads,
                    &mut pending_track_inventories,
                );
                if pending_track_inventories.len() >= MAX_PENDING_TRACK_INVENTORIES {
                    send_track_inventory_result(
                        &event_tx,
                        &core,
                        id,
                        Err(TrackInventoryError::TooManyPendingInventories {
                            limit: MAX_PENDING_TRACK_INVENTORIES,
                        }),
                    );
                    continue;
                }
                let generation = core.track_generation.load(Ordering::Acquire);
                pending_track_inventories.insert(
                    id,
                    PendingTrackInventory {
                        session_id,
                        generation,
                        tracks: Vec::new(),
                        audio_selection: None,
                        subtitle_selection: None,
                        queued_reads: VecDeque::new(),
                        in_flight: 0,
                    },
                );
                let read_id = NEXT_ASYNC_REQUEST_ID.fetch_add(1, Ordering::Relaxed);
                // SAFETY: The C string is valid for the call, uses the
                // documented track-list/count property, and mpv reports an
                // INT64 result through the owner-thread event queue.
                let code = unsafe {
                    (api.functions.get_property_async)(
                        handle,
                        read_id,
                        c"track-list/count".as_ptr(),
                        MPV_FORMAT_INT64,
                    )
                };
                if code < 0 {
                    finish_track_inventory(
                        &event_tx,
                        &core,
                        &mut pending_track_inventories,
                        id,
                        Err(TrackInventoryError::PropertyReadFailed {
                            property: "track-list/count".to_owned(),
                            code,
                        }),
                    );
                } else {
                    pending_reads.insert(read_id, PendingRead::TrackCount { inventory_id: id });
                }
                drain_events(
                    &api,
                    handle,
                    &event_tx,
                    &core,
                    &observations,
                    &mut pending_reads,
                    &mut pending_track_inventories,
                );
            }
            Ok(WorkerMessage::GetCapabilitySnapshot { id }) => {
                let snapshot = CapabilitySnapshot::probe(&api, handle);
                {
                    let mut results = lock(&core.capability_snapshot_results);
                    if results.len() >= MAX_STORED_CAPABILITY_SNAPSHOTS {
                        results.pop_front();
                    }
                    results.push_back((id, snapshot));
                }
                let _ = event_tx.send(MpvEvent {
                    kind: EventKind::CapabilitySnapshotReady,
                    request_id: id,
                    error: 0,
                });
            }
            Ok(WorkerMessage::GetStreamSnapshot { id }) => {
                let snapshot = StreamSnapshot::probe(&api, handle);
                {
                    let mut results = lock(&core.stream_snapshot_results);
                    if results.len() >= MAX_STORED_STREAM_SNAPSHOTS {
                        results.pop_front();
                    }
                    results.push_back((id, snapshot));
                }
                let _ = event_tx.send(MpvEvent {
                    kind: EventKind::StreamSnapshotReady,
                    request_id: id,
                    error: 0,
                });
            }
            Ok(WorkerMessage::GetChapterList { id }) => {
                // SAFETY: This serialized owner-thread request uses the
                // documented MPV_FORMAT_NODE property interface.
                let code = unsafe {
                    (api.functions.get_property_async)(
                        handle,
                        id,
                        c"chapter-list".as_ptr(),
                        MPV_FORMAT_NODE,
                    )
                };
                if code < 0 {
                    send_chapter_list_result(
                        &event_tx,
                        &core,
                        id,
                        Err(ChapterListError::Mpv(code)),
                        code,
                    );
                } else {
                    pending_reads.insert(id, PendingRead::ChapterList);
                }
                drain_events(
                    &api,
                    handle,
                    &event_tx,
                    &core,
                    &observations,
                    &mut pending_reads,
                    &mut pending_track_inventories,
                );
            }
            Ok(WorkerMessage::SelectTrack {
                request_id,
                generation,
                kind,
                value,
                response,
            }) => {
                drain_events(
                    &api,
                    handle,
                    &event_tx,
                    &core,
                    &observations,
                    &mut pending_reads,
                    &mut pending_track_inventories,
                );
                let result = if !core.track_list_observed.load(Ordering::Acquire) {
                    Err(TrackSelectionError::TrackListObservationUnavailable)
                } else if core.track_generation.load(Ordering::Acquire) != generation {
                    Err(TrackSelectionError::StaleInventory)
                } else {
                    let property = match kind {
                        TrackKind::Audio => Some(c"aid"),
                        TrackKind::Subtitle => Some(c"sid"),
                        TrackKind::Video | TrackKind::Other(_) => None,
                    };
                    if let Some(property) = property {
                        let value = CString::new(value);
                        let code = match &value {
                            Ok(value) => {
                                let arguments = [c"set", property, value.as_c_str()];
                                let mut pointers: Vec<*const std::ffi::c_char> =
                                    arguments.iter().map(|argument| argument.as_ptr()).collect();
                                pointers.push(ptr::null());
                                // SAFETY: These NUL-terminated command arguments live
                                // through the call; mpv copies them into its queue.
                                unsafe {
                                    (api.functions.command_async)(
                                        handle,
                                        request_id,
                                        pointers.as_ptr(),
                                    )
                                }
                            }
                            // A value with an interior NUL is never sent to mpv.
                            Err(_) => MPV_ERROR_INVALID_PARAMETER,
                        };
                        if code < 0 {
                            Err(TrackSelectionError::SubmissionFailed(code))
                        } else {
                            Ok(())
                        }
                    } else {
                        Err(TrackSelectionError::WrongTrackKind)
                    }
                };
                let _ = response.send(result);
                drain_events(
                    &api,
                    handle,
                    &event_tx,
                    &core,
                    &observations,
                    &mut pending_reads,
                    &mut pending_track_inventories,
                );
            }
            Ok(WorkerMessage::ObserveProperty {
                id,
                property,
                response,
            }) => {
                // SAFETY: This runs on the sole normal-API owner thread. The
                // property name is NUL-terminated and valid for the call; the
                // format and signature match client.h.
                let code = match CString::new(property.name()) {
                    Ok(name) => unsafe {
                        (api.functions.observe_property)(
                            handle,
                            id.get(),
                            name.as_ptr(),
                            property.format(),
                        )
                    },
                    // The names are static ASCII, so this is unreachable in practice.
                    Err(_) => MPV_ERROR_INVALID_PARAMETER,
                };
                let result = if code >= 0 {
                    observations.insert(id, property);
                    Ok(())
                } else {
                    Err(MpvError::ObservePropertyFailed {
                        property: property.name(),
                        code,
                    })
                };
                let _ = response.send(result);
                drain_events(
                    &api,
                    handle,
                    &event_tx,
                    &core,
                    &observations,
                    &mut pending_reads,
                    &mut pending_track_inventories,
                );
            }
            Ok(WorkerMessage::UnobserveProperty { id, response }) => {
                let result = if observations.contains_key(&id) {
                    // SAFETY: This runs on the sole normal-API owner thread;
                    // the ID was registered by this session on the same handle.
                    let code = unsafe { (api.functions.unobserve_property)(handle, id.get()) };
                    if code >= 0 {
                        observations.remove(&id);
                        Ok(())
                    } else {
                        Err(MpvError::UnobservePropertyFailed {
                            observation_id: id.get(),
                            code,
                        })
                    }
                } else {
                    Err(MpvError::UnknownObservation(id.get()))
                };
                let _ = response.send(result);
                drain_events(
                    &api,
                    handle,
                    &event_tx,
                    &core,
                    &observations,
                    &mut pending_reads,
                    &mut pending_track_inventories,
                );
            }
            Ok(WorkerMessage::Shutdown) | Err(_) => break,
        }
    }

    // Stop future wakeups, wait for callbacks already admitted, then drain the
    // event queue and destroy the handle on its serialized owner thread.
    // SAFETY: this worker is the only normal-API caller for this handle.
    unsafe {
        (api.functions.set_wakeup_callback)(handle, None, ptr::null_mut());
    }
    if let Some(context) = lock(&core.wakeup_context).as_ref() {
        context.gate.close_and_wait();
    }
    drain_events(
        &api,
        handle,
        &event_tx,
        &core,
        &observations,
        &mut pending_reads,
        &mut pending_track_inventories,
    );
    if core.destruction_safe.load(Ordering::Acquire) {
        // SAFETY: the render context was closed before MpvSession::drop, or no
        // renderer was created. This is the final owner-thread API call.
        unsafe { (api.functions.terminate_destroy)(handle) };
        core.handle.store(ptr::null_mut(), Ordering::Release);
        lock(&core.wakeup_context).take();
    }
}

fn drain_events(
    api: &Api,
    handle: *mut MpvHandle,
    sender: &mpsc::Sender<MpvEvent>,
    core: &Core,
    observations: &HashMap<ObservationId, PlaybackProperty>,
    pending_reads: &mut HashMap<u64, PendingRead>,
    pending_track_inventories: &mut HashMap<u64, PendingTrackInventory>,
) {
    if let Some(context) = lock(&core.wakeup_context).as_ref() {
        context.wake_pending.store(false, Ordering::Release);
    }
    loop {
        // SAFETY: mpv_wait_event is called only by the serialized owner thread;
        // the returned event is copied before the next wait invalidates it.
        let event = unsafe { (api.functions.wait_event)(handle, 0.0) };
        if event.is_null() {
            break;
        }
        // SAFETY: libmpv returns a valid event pointer or null.
        let event = unsafe { &*event };
        if event.event_id == MPV_EVENT_NONE {
            break;
        }
        let mut reported_error = event.error;
        let kind = match event.event_id {
            MPV_EVENT_SHUTDOWN => EventKind::Shutdown,
            MPV_EVENT_COMMAND_REPLY => EventKind::CommandReply,
            MPV_EVENT_GET_PROPERTY_REPLY => {
                let Some(pending) = pending_reads.remove(&event.reply_userdata) else {
                    // Ignore replies not requested by this session, or already
                    // consumed. Never infer a property from borrowed payload.
                    continue;
                };
                match pending {
                    PendingRead::Playback(property) => EventKind::PropertyReadReply {
                        property,
                        value: decode_property_read_value(property, event.error, event.data),
                    },
                    PendingRead::ChapterList => {
                        let result = decode_chapter_list_reply(api, event.error, event.data);
                        send_chapter_list_result(
                            sender,
                            core,
                            event.reply_userdata,
                            result,
                            event.error,
                        );
                        continue;
                    }
                    PendingRead::TrackCount { inventory_id } => {
                        handle_track_count_reply(
                            api,
                            handle,
                            sender,
                            core,
                            pending_reads,
                            pending_track_inventories,
                            inventory_id,
                            event.error,
                            event.data,
                        );
                        continue;
                    }
                    PendingRead::TrackValue {
                        inventory_id,
                        target,
                        property,
                    } => {
                        handle_track_value_reply(
                            api,
                            handle,
                            sender,
                            core,
                            pending_reads,
                            pending_track_inventories,
                            inventory_id,
                            target,
                            &property,
                            event.error,
                            event.data,
                        );
                        continue;
                    }
                }
            }
            MPV_EVENT_START_FILE => {
                core.track_generation.fetch_add(1, Ordering::AcqRel);
                let playlist_entry_id = if event.data.is_null() {
                    None
                } else {
                    // SAFETY: Start-file data is an mpv_event_start_file on the
                    // supported client API; copy it before the next wait call.
                    Some(
                        unsafe { ptr::read_unaligned(event.data.cast::<MpvEventStartFile>()) }
                            .playlist_entry_id,
                    )
                };
                EventKind::StartFile { playlist_entry_id }
            }
            MPV_EVENT_END_FILE => {
                core.track_generation.fetch_add(1, Ordering::AcqRel);
                let payload = if event.data.is_null() {
                    None
                } else {
                    // SAFETY: MPV_EVENT_END_FILE data points to the documented
                    // mpv_event_end_file payload and remains valid until the
                    // next mpv_wait_event call.
                    Some(unsafe { ptr::read_unaligned(event.data.cast::<MpvEventEndFile>()) })
                };
                let playlist_entry_id = payload.map(|details| details.playlist_entry_id);
                let (reason, error) = end_file_details(payload, event.error);
                reported_error = error;
                EventKind::EndFile {
                    reason,
                    playlist_entry_id,
                }
            }
            MPV_EVENT_FILE_LOADED => EventKind::FileLoaded,
            MPV_EVENT_SEEK => EventKind::Seek,
            MPV_EVENT_PLAYBACK_RESTART => EventKind::PlaybackRestart,
            MPV_EVENT_PROPERTY_CHANGE => {
                if event.reply_userdata == 0 {
                    // Internal MPV_FORMAT_NONE observer for track-list changes.
                    core.track_generation.fetch_add(1, Ordering::AcqRel);
                    continue;
                }
                let observation_id = ObservationId(event.reply_userdata);
                let Some(property) = observations.get(&observation_id).copied() else {
                    // An event already queued before unobserve can arrive after
                    // its mapping is removed. Do not guess from borrowed data.
                    continue;
                };
                EventKind::PropertyChange(PlaybackPropertyChange {
                    observation_id,
                    property,
                    value: decode_property_value(property, event.data),
                })
            }
            other => EventKind::Other(other),
        };
        let _ = sender.send(MpvEvent {
            kind,
            request_id: event.reply_userdata,
            error: reported_error,
        });
    }
}

#[allow(clippy::too_many_arguments)] // The owner-thread state is needed to validate and finish this async read.
fn handle_track_count_reply(
    api: &Api,
    handle: *mut MpvHandle,
    sender: &mpsc::Sender<MpvEvent>,
    core: &Core,
    pending_reads: &mut HashMap<u64, PendingRead>,
    pending_track_inventories: &mut HashMap<u64, PendingTrackInventory>,
    inventory_id: u64,
    error: i32,
    event_data: *mut c_void,
) {
    if error < 0 {
        finish_track_inventory(
            sender,
            core,
            pending_track_inventories,
            inventory_id,
            Err(TrackInventoryError::PropertyReadFailed {
                property: "track-list/count".to_owned(),
                code: error,
            }),
        );
        return;
    }
    let count =
        match decode_track_read_value("track-list/count", MPV_FORMAT_INT64, error, event_data) {
            Ok(TrackReadValue::Integer(value)) => match usize::try_from(value) {
                Ok(value) => value,
                Err(_) => {
                    finish_track_inventory(
                        sender,
                        core,
                        pending_track_inventories,
                        inventory_id,
                        Err(TrackInventoryError::InvalidTrackCount),
                    );
                    return;
                }
            },
            Ok(TrackReadValue::Unavailable) => {
                finish_track_inventory(
                    sender,
                    core,
                    pending_track_inventories,
                    inventory_id,
                    Err(TrackInventoryError::InvalidTrackCount),
                );
                return;
            }
            Ok(TrackReadValue::String(_) | TrackReadValue::Flag(_)) => {
                finish_track_inventory(
                    sender,
                    core,
                    pending_track_inventories,
                    inventory_id,
                    Err(TrackInventoryError::InvalidTrackCount),
                );
                return;
            }
            Err(error) => {
                finish_track_inventory(
                    sender,
                    core,
                    pending_track_inventories,
                    inventory_id,
                    Err(error),
                );
                return;
            }
        };
    if count > MAX_TRACK_COUNT {
        finish_track_inventory(
            sender,
            core,
            pending_track_inventories,
            inventory_id,
            Err(TrackInventoryError::TooManyTracks {
                count,
                limit: MAX_TRACK_COUNT,
            }),
        );
        return;
    }
    if core.track_generation.load(Ordering::Acquire)
        != pending_track_inventories
            .get(&inventory_id)
            .map_or(0, |pending| pending.generation)
    {
        finish_track_inventory(
            sender,
            core,
            pending_track_inventories,
            inventory_id,
            Err(TrackInventoryError::StaleSnapshot),
        );
        return;
    }
    if let Some(pending) = pending_track_inventories.get_mut(&inventory_id) {
        pending.tracks = vec![RawTrack::default(); count];
        pending.queued_reads = track_read_queue(count);
    } else {
        return;
    }
    schedule_track_reads(
        api,
        handle,
        sender,
        core,
        pending_reads,
        pending_track_inventories,
        inventory_id,
    );
}

#[allow(clippy::too_many_arguments)] // The owner-thread state is needed to validate and finish this async read.
fn handle_track_value_reply(
    api: &Api,
    handle: *mut MpvHandle,
    sender: &mpsc::Sender<MpvEvent>,
    core: &Core,
    pending_reads: &mut HashMap<u64, PendingRead>,
    pending_track_inventories: &mut HashMap<u64, PendingTrackInventory>,
    inventory_id: u64,
    target: TrackReadTarget,
    property: &str,
    error: i32,
    event_data: *mut c_void,
) {
    let Some(pending) = pending_track_inventories.get(&inventory_id) else {
        return;
    };
    if core.track_generation.load(Ordering::Acquire) != pending.generation {
        finish_track_inventory(
            sender,
            core,
            pending_track_inventories,
            inventory_id,
            Err(TrackInventoryError::StaleSnapshot),
        );
        return;
    }
    let format = match target {
        TrackReadTarget::Field { field, .. } => field.format(),
        TrackReadTarget::AudioSelection | TrackReadTarget::SubtitleSelection => MPV_FORMAT_STRING,
    };
    let value = match decode_track_read_value(property, format, error, event_data) {
        Ok(value) => value,
        Err(error) => {
            finish_track_inventory(
                sender,
                core,
                pending_track_inventories,
                inventory_id,
                Err(error),
            );
            return;
        }
    };
    if let Some(pending) = pending_track_inventories.get_mut(&inventory_id) {
        apply_track_read(pending, target, value);
        pending.in_flight = pending.in_flight.saturating_sub(1);
    }
    schedule_track_reads(
        api,
        handle,
        sender,
        core,
        pending_reads,
        pending_track_inventories,
        inventory_id,
    );
}

fn schedule_track_reads(
    api: &Api,
    handle: *mut MpvHandle,
    sender: &mpsc::Sender<MpvEvent>,
    core: &Core,
    pending_reads: &mut HashMap<u64, PendingRead>,
    pending_track_inventories: &mut HashMap<u64, PendingTrackInventory>,
    inventory_id: u64,
) {
    loop {
        let request = {
            let Some(pending) = pending_track_inventories.get_mut(&inventory_id) else {
                return;
            };
            if pending.in_flight >= MAX_IN_FLIGHT_TRACK_READS {
                None
            } else {
                pending.queued_reads.pop_front()
            }
        };
        let Some(request) = request else {
            break;
        };
        if pending_track_inventories
            .get(&inventory_id)
            .is_some_and(|pending| {
                core.track_generation.load(Ordering::Acquire) != pending.generation
            })
        {
            finish_track_inventory(
                sender,
                core,
                pending_track_inventories,
                inventory_id,
                Err(TrackInventoryError::StaleSnapshot),
            );
            return;
        }

        let request_id = NEXT_ASYNC_REQUEST_ID.fetch_add(1, Ordering::Relaxed);
        let property = request.property.to_string_lossy().into_owned();
        // SAFETY: This async property request runs on the owner thread. Its
        // NUL-terminated generated property name and documented format remain
        // valid for the call; the copied result is routed by request ID.
        let code = unsafe {
            (api.functions.get_property_async)(
                handle,
                request_id,
                request.property.as_ptr(),
                request.format,
            )
        };
        if code < 0 {
            finish_track_inventory(
                sender,
                core,
                pending_track_inventories,
                inventory_id,
                Err(TrackInventoryError::PropertyReadFailed { property, code }),
            );
            return;
        }
        pending_reads.insert(
            request_id,
            PendingRead::TrackValue {
                inventory_id,
                target: request.target,
                property,
            },
        );
        if let Some(pending) = pending_track_inventories.get_mut(&inventory_id) {
            pending.in_flight += 1;
        }
    }
    finish_track_inventory_if_ready(sender, core, pending_track_inventories, inventory_id);
}

fn finish_track_inventory_if_ready(
    sender: &mpsc::Sender<MpvEvent>,
    core: &Core,
    pending_track_inventories: &mut HashMap<u64, PendingTrackInventory>,
    inventory_id: u64,
) {
    let ready = pending_track_inventories
        .get(&inventory_id)
        .is_some_and(|pending| pending.in_flight == 0 && pending.queued_reads.is_empty());
    if !ready {
        return;
    }
    let Some(pending) = pending_track_inventories.remove(&inventory_id) else {
        return;
    };
    let result = if core.track_generation.load(Ordering::Acquire) != pending.generation {
        Err(TrackInventoryError::StaleSnapshot)
    } else {
        parse_track_inventory(
            pending.tracks,
            pending.audio_selection.as_deref(),
            pending.subtitle_selection.as_deref(),
            pending.session_id,
            pending.generation,
        )
    };
    send_track_inventory_result(sender, core, inventory_id, result);
}

fn finish_track_inventory(
    sender: &mpsc::Sender<MpvEvent>,
    core: &Core,
    pending_track_inventories: &mut HashMap<u64, PendingTrackInventory>,
    inventory_id: u64,
    result: Result<TrackInventory, TrackInventoryError>,
) {
    if pending_track_inventories.remove(&inventory_id).is_some() {
        send_track_inventory_result(sender, core, inventory_id, result);
    }
}

fn send_track_inventory_result(
    sender: &mpsc::Sender<MpvEvent>,
    core: &Core,
    inventory_id: u64,
    result: Result<TrackInventory, TrackInventoryError>,
) {
    let error = result
        .as_ref()
        .err()
        .map_or(0, TrackInventoryError::mpv_error);
    {
        let mut results = lock(&core.track_inventory_results);
        if results.len() >= MAX_STORED_TRACK_INVENTORIES {
            results.pop_front();
        }
        results.push_back((inventory_id, result));
    }
    let _ = sender.send(MpvEvent {
        kind: EventKind::TrackInventoryReady,
        request_id: inventory_id,
        error,
    });
}

fn send_chapter_list_result(
    sender: &mpsc::Sender<MpvEvent>,
    core: &Core,
    request_id: u64,
    result: Result<Vec<Chapter>, ChapterListError>,
    mpv_error: i32,
) {
    {
        let mut results = lock(&core.chapter_list_results);
        if results.len() >= MAX_STORED_CHAPTER_LISTS {
            results.pop_front();
        }
        results.push_back((request_id, result));
    }
    let _ = sender.send(MpvEvent {
        kind: EventKind::ChapterListReady,
        request_id,
        error: mpv_error,
    });
}

fn decode_chapter_list_reply(
    api: &Api,
    mpv_error: i32,
    event_data: *mut c_void,
) -> Result<Vec<Chapter>, ChapterListError> {
    if mpv_error < 0 {
        return Err(ChapterListError::Mpv(mpv_error));
    }
    if event_data.is_null() {
        return Err(ChapterListError::InvalidNode);
    }

    // SAFETY: A successful GET_PROPERTY_REPLY has an mpv_event_property in
    // event.data, valid until the next mpv_wait_event call on this owner thread.
    let property_event = unsafe { &*event_data.cast::<MpvEventProperty>() };
    if property_event.format != MPV_FORMAT_NODE || property_event.data.is_null() {
        return Err(ChapterListError::InvalidNode);
    }

    let node_pointer = property_event.data.cast::<MpvNode>();
    // SAFETY: the successful property result contains an mpv_node pointer and
    // this function pairs it with libmpv's documented node cleanup function.
    unsafe { parse_and_free_chapter_node(node_pointer, api.functions.free_node_contents) }
}

struct NodeContentsGuard {
    free_node_contents: unsafe extern "C" fn(*mut MpvNode),
    node: *mut MpvNode,
}

impl Drop for NodeContentsGuard {
    fn drop(&mut self) {
        // SAFETY: this guard is created only for a non-null node returned by a
        // successful MPV_FORMAT_NODE property read, and is dropped once.
        unsafe { (self.free_node_contents)(self.node) };
    }
}

unsafe fn parse_and_free_chapter_node(
    node: *mut MpvNode,
    free_node_contents: unsafe extern "C" fn(*mut MpvNode),
) -> Result<Vec<Chapter>, ChapterListError> {
    if node.is_null() {
        return Err(ChapterListError::InvalidNode);
    }
    // SAFETY: The caller passes a successful MPV_FORMAT_NODE response owned by
    // libmpv. The guard guarantees cleanup even when parsing returns an error.
    let _contents = NodeContentsGuard {
        free_node_contents,
        node,
    };
    // SAFETY: Non-null node pointer was validated immediately above.
    unsafe { parse_chapter_list(&*node) }
}

unsafe fn parse_chapter_list(node: &MpvNode) -> Result<Vec<Chapter>, ChapterListError> {
    if node.format != MPV_FORMAT_NODE_ARRAY {
        return Err(ChapterListError::InvalidNode);
    }
    // SAFETY: The format tag identifies the list member of this documented
    // mpv_node union; only its nullness and bounded metadata are inspected.
    let list_pointer = unsafe { node.value.list };
    if list_pointer.is_null() {
        return Err(ChapterListError::InvalidNode);
    }
    // SAFETY: libmpv owns a valid mpv_node_list for a non-null compound node.
    let list = unsafe { &*list_pointer };
    if list.count < 0 || list.count as usize > MAX_CHAPTERS {
        return Err(ChapterListError::LimitExceeded);
    }
    if list.count > 0 && list.values.is_null() {
        return Err(ChapterListError::InvalidNode);
    }

    let mut chapters = Vec::with_capacity(list.count as usize);
    for index in 0..list.count as usize {
        // SAFETY: The count and values pointer were checked above; index is
        // strictly below the count reported by libmpv.
        let chapter_node = unsafe { &*list.values.add(index) };
        // SAFETY: Each documented chapter entry is a map node.
        chapters.push(unsafe { parse_chapter(chapter_node)? });
    }
    Ok(chapters)
}

unsafe fn parse_chapter(node: &MpvNode) -> Result<Chapter, ChapterListError> {
    if node.format != MPV_FORMAT_NODE_MAP {
        return Err(ChapterListError::InvalidNode);
    }
    // SAFETY: The format tag identifies the map list member of the union.
    let list_pointer = unsafe { node.value.list };
    if list_pointer.is_null() {
        return Err(ChapterListError::InvalidNode);
    }
    // SAFETY: A non-null map node points to an mpv_node_list owned by mpv.
    let list = unsafe { &*list_pointer };
    if list.count < 0 || list.count as usize > MAX_CHAPTER_MAP_ENTRIES {
        return Err(ChapterListError::LimitExceeded);
    }
    if list.count > 0 && (list.values.is_null() || list.keys.is_null()) {
        return Err(ChapterListError::InvalidNode);
    }

    let mut title = None;
    let mut saw_title = false;
    let mut time_seconds = None;
    for index in 0..list.count as usize {
        // SAFETY: The map arrays are non-null for nonzero count and the index
        // remains inside the validated count.
        let key_pointer = unsafe { *list.keys.add(index) };
        let key = unsafe { copy_chapter_string(key_pointer, MAX_CHAPTER_KEY_BYTES)? };
        if key != "title" && key != "time" {
            continue;
        }
        // SAFETY: The value array and index have been validated above.
        let value = unsafe { &*list.values.add(index) };
        match key.as_str() {
            "title" => {
                if saw_title {
                    return Err(ChapterListError::DuplicateField);
                }
                saw_title = true;
                if value.format == MPV_FORMAT_NONE {
                    title = None;
                } else {
                    if value.format != MPV_FORMAT_STRING {
                        return Err(ChapterListError::InvalidNode);
                    }
                    // SAFETY: The tagged string pointer is copied with a hard byte
                    // limit and never retained beyond the node's cleanup.
                    title = Some(unsafe {
                        copy_chapter_string(value.value.string, MAX_CHAPTER_TITLE_BYTES)?
                    });
                }
            }
            "time" => {
                if time_seconds.is_some() {
                    return Err(ChapterListError::DuplicateField);
                }
                if value.format != MPV_FORMAT_DOUBLE {
                    return Err(ChapterListError::InvalidTime);
                }
                // SAFETY: The format tag identifies the double member.
                let seconds = unsafe { value.value.double_ };
                if !seconds.is_finite() || seconds < 0.0 {
                    return Err(ChapterListError::InvalidTime);
                }
                time_seconds = Some(seconds);
            }
            _ => {}
        }
    }

    Ok(Chapter {
        title,
        time_seconds: time_seconds.ok_or(ChapterListError::InvalidTime)?,
    })
}

unsafe fn copy_chapter_string(
    pointer: *const std::ffi::c_char,
    maximum_bytes: usize,
) -> Result<String, ChapterListError> {
    if pointer.is_null() {
        return Err(ChapterListError::InvalidString);
    }
    let mut bytes = Vec::with_capacity(maximum_bytes.min(MAX_CHAPTER_TITLE_BYTES));
    for index in 0..=maximum_bytes {
        // SAFETY: mpv guarantees its string fields are NUL-terminated and
        // readable. The loop stops at that terminator or after the configured
        // bound, so it never scans an unbounded field.
        // C char signedness varies by target; decode the underlying UTF-8 bytes.
        let byte = unsafe { pointer.cast::<u8>().add(index).read() };
        if byte == 0 {
            return String::from_utf8(bytes).map_err(|_| ChapterListError::InvalidString);
        }
        if index == maximum_bytes {
            return Err(ChapterListError::LimitExceeded);
        }
        bytes.push(byte);
    }
    Err(ChapterListError::LimitExceeded)
}

fn decode_property_value(
    property: PlaybackProperty,
    event_data: *mut c_void,
) -> PlaybackPropertyValue {
    let unavailable = match property {
        PlaybackProperty::TimePosition => PlaybackPropertyValue::TimePosition(None),
        PlaybackProperty::Duration => PlaybackPropertyValue::Duration(None),
        PlaybackProperty::Paused => PlaybackPropertyValue::Paused(None),
        PlaybackProperty::Speed => PlaybackPropertyValue::Speed(None),
        PlaybackProperty::Volume => PlaybackPropertyValue::Volume(None),
        PlaybackProperty::Mute => PlaybackPropertyValue::Muted(None),
        PlaybackProperty::SubtitleDelay => PlaybackPropertyValue::SubtitleDelay(None),
        PlaybackProperty::SubtitleTrackId => PlaybackPropertyValue::SubtitleTrackId(None),
    };
    if event_data.is_null() {
        return unavailable;
    }

    // SAFETY: This is called only for MPV_EVENT_PROPERTY_CHANGE. client.h
    // specifies its data pointer as mpv_event_property* and this value is
    // copied before the next mpv_wait_event call invalidates the event.
    let property_event = unsafe { &*event_data.cast::<MpvEventProperty>() };
    if property_event.format == MPV_FORMAT_NONE {
        return unavailable;
    }
    if property_event.format != property.format() {
        return PlaybackPropertyValue::UnsupportedFormat {
            property,
            format: property_event.format,
        };
    }
    if property_event.data.is_null() {
        return unavailable;
    }

    match property {
        PlaybackProperty::TimePosition => {
            // SAFETY: Format was checked against MPV_FORMAT_DOUBLE and data is
            // non-null. The scalar is copied while the event is still valid.
            let value = unsafe { ptr::read_unaligned(property_event.data.cast::<f64>()) };
            PlaybackPropertyValue::TimePosition(Some(value))
        }
        PlaybackProperty::Duration => {
            // SAFETY: Format was checked against MPV_FORMAT_DOUBLE and data is
            // non-null. The scalar is copied while the event is still valid.
            let value = unsafe { ptr::read_unaligned(property_event.data.cast::<f64>()) };
            PlaybackPropertyValue::Duration(Some(value))
        }
        PlaybackProperty::Paused => {
            // SAFETY: Format was checked against MPV_FORMAT_FLAG (C int) and
            // data is non-null. The scalar is copied before the next wait.
            let value =
                unsafe { ptr::read_unaligned(property_event.data.cast::<std::ffi::c_int>()) };
            match value {
                0 => PlaybackPropertyValue::Paused(Some(false)),
                1 => PlaybackPropertyValue::Paused(Some(true)),
                other => PlaybackPropertyValue::InvalidFlagValue(other),
            }
        }
        PlaybackProperty::Speed => {
            // SAFETY: Format was checked against MPV_FORMAT_DOUBLE and data is
            // non-null. Non-finite values are not exposed as valid rates.
            let value = unsafe { ptr::read_unaligned(property_event.data.cast::<f64>()) };
            PlaybackPropertyValue::Speed(value.is_finite().then_some(value))
        }
        PlaybackProperty::Volume => {
            // SAFETY: Format was checked against MPV_FORMAT_DOUBLE and data is
            // non-null. The scalar is copied while the event is still valid.
            let value = unsafe { ptr::read_unaligned(property_event.data.cast::<f64>()) };
            PlaybackPropertyValue::Volume(Some(value))
        }
        PlaybackProperty::Mute => {
            // SAFETY: Format was checked against MPV_FORMAT_FLAG (C int) and
            // data is non-null. The scalar is copied before the next wait.
            let value =
                unsafe { ptr::read_unaligned(property_event.data.cast::<std::ffi::c_int>()) };
            match value {
                0 => PlaybackPropertyValue::Muted(Some(false)),
                1 => PlaybackPropertyValue::Muted(Some(true)),
                other => PlaybackPropertyValue::InvalidFlagValue(other),
            }
        }
        PlaybackProperty::SubtitleDelay => {
            // SAFETY: Format was checked against MPV_FORMAT_DOUBLE and data is
            // non-null. Non-finite values are kept out of the public value.
            let value = unsafe { ptr::read_unaligned(property_event.data.cast::<f64>()) };
            PlaybackPropertyValue::SubtitleDelay(value.is_finite().then_some(value))
        }
        PlaybackProperty::SubtitleTrackId => {
            // SAFETY: Format was checked against MPV_FORMAT_INT64 and data is
            // non-null. The scalar is copied while the event is still valid.
            let value = unsafe { ptr::read_unaligned(property_event.data.cast::<i64>()) };
            PlaybackPropertyValue::SubtitleTrackId((value >= 0).then_some(value))
        }
    }
}

fn unavailable_property_value(property: PlaybackProperty) -> PlaybackPropertyValue {
    match property {
        PlaybackProperty::TimePosition => PlaybackPropertyValue::TimePosition(None),
        PlaybackProperty::Duration => PlaybackPropertyValue::Duration(None),
        PlaybackProperty::Paused => PlaybackPropertyValue::Paused(None),
        PlaybackProperty::Speed => PlaybackPropertyValue::Speed(None),
        PlaybackProperty::Volume => PlaybackPropertyValue::Volume(None),
        PlaybackProperty::Mute => PlaybackPropertyValue::Muted(None),
        PlaybackProperty::SubtitleDelay => PlaybackPropertyValue::SubtitleDelay(None),
        PlaybackProperty::SubtitleTrackId => PlaybackPropertyValue::SubtitleTrackId(None),
    }
}

fn decode_property_read_value(
    property: PlaybackProperty,
    error: i32,
    event_data: *mut c_void,
) -> PlaybackPropertyValue {
    // A failed get-property reply may carry invalid or placeholder payload
    // storage. Its error code is authoritative; do not inspect that payload.
    if error < 0 {
        return unavailable_property_value(property);
    }
    if event_data.is_null() {
        return PlaybackPropertyValue::InvalidData(property);
    }

    // SAFETY: client.h defines GET_PROPERTY_REPLY data as mpv_event_property*.
    // The event remains valid until the next mpv_wait_event call, and this
    // function copies the only scalar used before that call.
    let property_event = unsafe { &*event_data.cast::<MpvEventProperty>() };
    if property_event.format == MPV_FORMAT_NONE || property_event.data.is_null() {
        return PlaybackPropertyValue::InvalidData(property);
    }
    if property_event.format != property.format() {
        return PlaybackPropertyValue::UnsupportedFormat {
            property,
            format: property_event.format,
        };
    }
    match property {
        PlaybackProperty::Paused | PlaybackProperty::Mute => {
            // SAFETY: FLAG replies contain one C int, just like property-change events.
            let value =
                unsafe { ptr::read_unaligned(property_event.data.cast::<std::ffi::c_int>()) };
            match value {
                0 | 1 => {
                    let flag = Some(value == 1);
                    if property == PlaybackProperty::Paused {
                        PlaybackPropertyValue::Paused(flag)
                    } else {
                        PlaybackPropertyValue::Muted(flag)
                    }
                }
                other => PlaybackPropertyValue::InvalidFlagValue(other),
            }
        }
        PlaybackProperty::TimePosition
        | PlaybackProperty::Duration
        | PlaybackProperty::Speed
        | PlaybackProperty::Volume
        | PlaybackProperty::SubtitleDelay => {
            // SAFETY: The requested and returned formats are DOUBLE, and data
            // is non-null. Copy before the next event wait invalidates it.
            let value = unsafe { ptr::read_unaligned(property_event.data.cast::<f64>()) };
            if !value.is_finite() {
                return PlaybackPropertyValue::InvalidData(property);
            }
            match property {
                PlaybackProperty::TimePosition => PlaybackPropertyValue::TimePosition(Some(value)),
                PlaybackProperty::Duration => PlaybackPropertyValue::Duration(Some(value)),
                PlaybackProperty::Speed => PlaybackPropertyValue::Speed(Some(value)),
                PlaybackProperty::Volume => PlaybackPropertyValue::Volume(Some(value)),
                PlaybackProperty::SubtitleDelay => {
                    PlaybackPropertyValue::SubtitleDelay(Some(value))
                }
                PlaybackProperty::Paused
                | PlaybackProperty::Mute
                | PlaybackProperty::SubtitleTrackId => PlaybackPropertyValue::InvalidData(property),
            }
        }
        PlaybackProperty::SubtitleTrackId => {
            // SAFETY: The requested and returned formats are INT64, and data
            // is non-null. Copy before the next event wait invalidates it.
            let value = unsafe { ptr::read_unaligned(property_event.data.cast::<i64>()) };
            PlaybackPropertyValue::SubtitleTrackId((value >= 0).then_some(value))
        }
    }
}

unsafe extern "C" fn wakeup(context: *mut c_void) {
    if context.is_null() {
        return;
    }
    // SAFETY: context is the stable WakeupContext box stored in Core. No libmpv
    // API is called here; this only coalesces a message to the owner thread.
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        let context = &*context.cast::<WakeupContext>();
        wakeup_callback(&context.gate, || {
            if !context.wake_pending.swap(true, Ordering::AcqRel) {
                match context.worker_tx.try_send(WorkerMessage::Wake) {
                    Ok(()) | Err(TrySendError::Full(_)) => {}
                    Err(TrySendError::Disconnected(_)) => {
                        context.wake_pending.store(false, Ordering::Release);
                    }
                }
            }
        });
    }));
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asynchronous_pause_reads_preserve_the_observed_flag() {
        for (mut flag, expected) in [
            (0_i32, PlaybackPropertyValue::Paused(Some(false))),
            (1_i32, PlaybackPropertyValue::Paused(Some(true))),
            (2_i32, PlaybackPropertyValue::InvalidFlagValue(2)),
        ] {
            let mut reply = MpvEventProperty {
                _name: c"pause".as_ptr(),
                format: MPV_FORMAT_FLAG,
                data: (&mut flag as *mut i32).cast(),
            };
            assert_eq!(
                decode_property_read_value(
                    PlaybackProperty::Paused,
                    0,
                    (&mut reply as *mut MpvEventProperty).cast(),
                ),
                expected,
            );
        }
    }

    #[test]
    fn failed_or_missing_pause_reads_do_not_invent_a_playing_state() {
        assert_eq!(
            decode_property_read_value(PlaybackProperty::Paused, -1, ptr::null_mut()),
            PlaybackPropertyValue::Paused(None),
        );
        assert_eq!(
            decode_property_read_value(PlaybackProperty::Paused, 0, ptr::null_mut()),
            PlaybackPropertyValue::InvalidData(PlaybackProperty::Paused),
        );
    }
    use crate::ffi::{MpvNodeList, MpvNodeValue};

    #[test]
    fn pause_commands_use_explicit_yes_and_no_values() {
        assert_eq!(pause_command_arguments(true), ["set", "pause", "yes"]);
        assert_eq!(pause_command_arguments(false), ["set", "pause", "no"]);
    }

    #[test]
    fn seek_commands_encode_relative_and_absolute_modes() {
        assert_eq!(
            relative_seek_command_arguments(-10.0).unwrap(),
            ["seek", "-10", "relative"]
        );
        assert_eq!(
            absolute_seek_command_arguments(42.5).unwrap(),
            ["seek", "42.5", "absolute"]
        );
        assert!(matches!(
            absolute_seek_command_arguments(-1.0),
            Err(PlaybackControlError::NegativeAbsoluteSeek)
        ));
    }

    #[test]
    fn seek_commands_reject_non_finite_values() {
        assert!(matches!(
            relative_seek_command_arguments(f64::NAN),
            Err(PlaybackControlError::NonFiniteInput)
        ));
        assert!(matches!(
            absolute_seek_command_arguments(f64::INFINITY),
            Err(PlaybackControlError::NonFiniteInput)
        ));
    }

    #[test]
    fn speed_choices_are_validated_and_step_in_both_directions() {
        assert_eq!(PLAYBACK_SPEED_CHOICES, [0.5, 0.75, 1.0, 1.25, 1.5, 2.0]);
        assert_eq!(
            speed_command_arguments(1.25).unwrap(),
            ["set", "speed", "1.25"]
        );
        assert!(matches!(
            speed_command_arguments(1.1),
            Err(PlaybackControlError::UnsupportedSpeed(1.1))
        ));
        assert_eq!(
            adjacent_playback_speed(1.0, PlaybackSpeedStep::Previous).unwrap(),
            0.75
        );
        assert_eq!(
            adjacent_playback_speed(1.0, PlaybackSpeedStep::Next).unwrap(),
            1.25
        );
        assert_eq!(
            adjacent_playback_speed(0.1, PlaybackSpeedStep::Previous).unwrap(),
            0.5
        );
        assert_eq!(
            adjacent_playback_speed(5.0, PlaybackSpeedStep::Next).unwrap(),
            2.0
        );
    }

    #[test]
    fn stop_resets_normal_speed_before_issuing_stop() {
        assert_eq!(stop_speed_reset_arguments(), ["set", "speed", "1"]);
        assert_eq!(stop_command_arguments(), ["stop"]);
    }

    #[test]
    fn duration_time_pause_and_speed_property_contracts_are_typed() {
        assert_eq!(PlaybackProperty::TimePosition.name(), "time-pos");
        assert_eq!(PlaybackProperty::Duration.name(), "duration");
        assert_eq!(PlaybackProperty::Paused.name(), "pause");
        assert_eq!(PlaybackProperty::Speed.name(), "speed");
        assert_eq!(PlaybackProperty::Paused.format(), MPV_FORMAT_FLAG);
        assert_eq!(PlaybackProperty::Speed.format(), MPV_FORMAT_DOUBLE);
        assert_eq!(
            unavailable_property_value(PlaybackProperty::Speed),
            PlaybackPropertyValue::Speed(None)
        );
    }

    #[test]
    fn chapter_list_parser_copies_optional_titles_and_times() {
        let fixture = chapter_list_fixture(&[(Some("Opening"), 0.0), (None, 95.25)]);
        // Synthetic mpv_node fixture matching the pinned chapter-list schema.
        let chapters = unsafe { parse_chapter_list(&fixture.root) }.unwrap();
        assert_eq!(
            chapters,
            [
                Chapter {
                    title: Some("Opening".to_owned()),
                    time_seconds: 0.0,
                },
                Chapter {
                    title: None,
                    time_seconds: 95.25,
                },
            ]
        );
    }

    #[test]
    fn chapter_list_parser_rejects_invalid_times_and_missing_time() {
        for time in [-0.01, f64::NAN, f64::INFINITY] {
            let fixture = chapter_list_fixture(&[(None, time)]);
            assert_eq!(
                unsafe { parse_chapter_list(&fixture.root) },
                Err(ChapterListError::InvalidTime)
            );
        }

        let fixture = chapter_map_fixture(&[("title", FixtureValue::String("No timestamp"))]);
        assert_eq!(
            unsafe { parse_chapter_list(&fixture.root) },
            Err(ChapterListError::InvalidTime)
        );
    }

    #[test]
    fn chapter_list_parser_checks_root_and_count_bounds_before_reading_entries() {
        let wrong_root = MpvNode {
            value: MpvNodeValue { integer: 0 },
            format: MPV_FORMAT_NODE_MAP,
        };
        assert_eq!(
            unsafe { parse_chapter_list(&wrong_root) },
            Err(ChapterListError::InvalidNode)
        );

        let oversized_list = Box::new(MpvNodeList {
            count: (MAX_CHAPTERS + 1) as i32,
            values: ptr::null_mut(),
            keys: ptr::null_mut(),
        });
        let oversized_root = MpvNode {
            value: MpvNodeValue {
                list: (&*oversized_list as *const MpvNodeList).cast_mut(),
            },
            format: MPV_FORMAT_NODE_ARRAY,
        };
        assert_eq!(
            unsafe { parse_chapter_list(&oversized_root) },
            Err(ChapterListError::LimitExceeded)
        );

        let null_list = MpvNode {
            value: MpvNodeValue {
                list: ptr::null_mut(),
            },
            format: MPV_FORMAT_NODE_ARRAY,
        };
        assert_eq!(
            unsafe { parse_chapter_list(&null_list) },
            Err(ChapterListError::InvalidNode)
        );

        let missing_values = Box::new(MpvNodeList {
            count: 1,
            values: ptr::null_mut(),
            keys: ptr::null_mut(),
        });
        let missing_values_root = MpvNode {
            value: MpvNodeValue {
                list: (&*missing_values as *const MpvNodeList).cast_mut(),
            },
            format: MPV_FORMAT_NODE_ARRAY,
        };
        assert_eq!(
            unsafe { parse_chapter_list(&missing_values_root) },
            Err(ChapterListError::InvalidNode)
        );
    }

    #[test]
    fn chapter_node_contents_are_freed_even_when_parsing_fails() {
        static FREE_COUNT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

        unsafe extern "C" fn count_free(_node: *mut MpvNode) {
            FREE_COUNT.fetch_add(1, Ordering::Relaxed);
        }

        FREE_COUNT.store(0, Ordering::Relaxed);
        let malformed = Box::new(MpvNode {
            value: MpvNodeValue { integer: 0 },
            format: MPV_FORMAT_NODE_MAP,
        });
        assert_eq!(
            unsafe {
                parse_and_free_chapter_node((&*malformed as *const MpvNode).cast_mut(), count_free)
            },
            Err(ChapterListError::InvalidNode)
        );
        assert_eq!(FREE_COUNT.load(Ordering::Relaxed), 1);

        let fixture = chapter_list_fixture(&[(Some("Valid"), 1.0)]);
        assert_eq!(
            unsafe {
                parse_and_free_chapter_node(
                    (&fixture.root as *const MpvNode).cast_mut(),
                    count_free,
                )
            }
            .map(|chapters| chapters.len()),
            Ok(1)
        );
        assert_eq!(FREE_COUNT.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn chapter_list_parser_bounds_strings_and_rejects_duplicate_fields() {
        let long_title = "x".repeat(MAX_CHAPTER_TITLE_BYTES + 1);
        let too_long = chapter_list_fixture(&[(Some(&long_title), 1.0)]);
        assert_eq!(
            unsafe { parse_chapter_list(&too_long.root) },
            Err(ChapterListError::LimitExceeded)
        );

        let duplicate = chapter_map_fixture(&[
            ("time", FixtureValue::Time(1.0)),
            ("time", FixtureValue::Time(2.0)),
        ]);
        assert_eq!(
            unsafe { parse_chapter_list(&duplicate.root) },
            Err(ChapterListError::DuplicateField)
        );
    }

    enum FixtureValue<'a> {
        String(&'a str),
        Time(f64),
    }

    struct ChapterMapFixture {
        _keys: Vec<CString>,
        _key_pointers: Vec<*mut std::ffi::c_char>,
        _strings: Vec<CString>,
        _values: Vec<MpvNode>,
        _list: Box<MpvNodeList>,
        node: MpvNode,
        _root_values: Vec<MpvNode>,
        _root_list: Box<MpvNodeList>,
        root: MpvNode,
        _children: Vec<ChapterMapFixture>,
    }

    fn chapter_list_fixture(entries: &[(Option<&str>, f64)]) -> ChapterMapFixture {
        let entries = entries
            .iter()
            .map(|(title, time)| {
                let mut fields = Vec::new();
                if let Some(title) = title {
                    fields.push(("title", FixtureValue::String(title)));
                }
                fields.push(("time", FixtureValue::Time(*time)));
                chapter_map_fixture(&fields)
            })
            .collect::<Vec<_>>();
        root_from_chapter_fixtures(entries)
    }

    fn chapter_map_fixture(fields: &[(&str, FixtureValue<'_>)]) -> ChapterMapFixture {
        let keys = fields
            .iter()
            .map(|(key, _)| CString::new(*key).expect("static fixture key has no NUL"))
            .collect::<Vec<_>>();
        let strings = fields
            .iter()
            .filter_map(|(_, value)| match value {
                FixtureValue::String(value) => {
                    Some(CString::new(*value).expect("fixture string has no NUL"))
                }
                FixtureValue::Time(_) => None,
            })
            .collect::<Vec<_>>();
        let mut string_index = 0;
        let mut values = fields
            .iter()
            .map(|(_, value)| match value {
                FixtureValue::String(_) => {
                    let string = &strings[string_index];
                    string_index += 1;
                    MpvNode {
                        value: MpvNodeValue {
                            string: string.as_ptr().cast_mut(),
                        },
                        format: MPV_FORMAT_STRING,
                    }
                }
                FixtureValue::Time(seconds) => MpvNode {
                    value: MpvNodeValue { double_: *seconds },
                    format: MPV_FORMAT_DOUBLE,
                },
            })
            .collect::<Vec<_>>();
        let mut key_pointers = keys
            .iter()
            .map(|key| key.as_ptr().cast_mut())
            .collect::<Vec<_>>();
        let list = Box::new(MpvNodeList {
            count: values.len() as i32,
            values: values.as_mut_ptr(),
            keys: key_pointers.as_mut_ptr(),
        });
        let node = MpvNode {
            value: MpvNodeValue {
                list: (&*list as *const MpvNodeList).cast_mut(),
            },
            format: MPV_FORMAT_NODE_MAP,
        };
        let node_copy = copy_map_node(&node);
        let mut root_values = vec![node_copy];
        let root_list = Box::new(MpvNodeList {
            count: 1,
            values: root_values.as_mut_ptr(),
            keys: ptr::null_mut(),
        });
        let root = MpvNode {
            value: MpvNodeValue {
                list: (&*root_list as *const MpvNodeList).cast_mut(),
            },
            format: MPV_FORMAT_NODE_ARRAY,
        };
        ChapterMapFixture {
            _keys: keys,
            _key_pointers: key_pointers,
            _strings: strings,
            _values: values,
            _list: list,
            node,
            _root_values: root_values,
            _root_list: root_list,
            root,
            _children: Vec::new(),
        }
    }

    fn root_from_chapter_fixtures(entries: Vec<ChapterMapFixture>) -> ChapterMapFixture {
        let mut root_values = entries
            .iter()
            .map(|entry| copy_map_node(&entry.node))
            .collect::<Vec<_>>();
        let root_list = Box::new(MpvNodeList {
            count: root_values.len() as i32,
            values: root_values.as_mut_ptr(),
            keys: ptr::null_mut(),
        });
        let root = MpvNode {
            value: MpvNodeValue {
                list: (&*root_list as *const MpvNodeList).cast_mut(),
            },
            format: MPV_FORMAT_NODE_ARRAY,
        };
        ChapterMapFixture {
            _keys: Vec::new(),
            _key_pointers: Vec::new(),
            _strings: Vec::new(),
            _values: Vec::new(),
            _list: Box::new(MpvNodeList {
                count: 0,
                values: ptr::null_mut(),
                keys: ptr::null_mut(),
            }),
            node: MpvNode {
                value: MpvNodeValue { integer: 0 },
                format: MPV_FORMAT_NODE_MAP,
            },
            _root_values: root_values,
            _root_list: root_list,
            root,
            _children: entries,
        }
    }

    fn copy_map_node(node: &MpvNode) -> MpvNode {
        // SAFETY: Every caller passes a synthetic map node fixture.
        let list = unsafe { node.value.list };
        MpvNode {
            value: MpvNodeValue { list },
            format: MPV_FORMAT_NODE_MAP,
        }
    }
}
