use crate::catalog::MediaItem;
use crate::saved_servers::{SavedServers, default_servers_path};
use crate::settings_state::{SettingsState, default_profiles_dir};
use crate::sign_in::QuickConnectFlow;
use crate::ui::admin::AdminLoad;
use crate::ui::detail::WatchedToggle;
use crate::ui::downloads::DownloadIntent;
use crate::ui::home::{HomeAction, HomeState, HomeViewTarget};
use crate::ui::music::MusicAction;
use crate::ui::playlists::{MoveDirection, PlaylistIntent, PlaylistUiState};
use crate::ui::profile_picker::{self, PickerOutcome, PickerState};
use crate::ui::{ui_text, ui_text_args};
use crate::worker::downloads::DownloadsState;
use crate::worker::update::{ApplyRequest, UpdateState, UpdateStatus, prepare};
use crate::worker::{
    ApiWorker, HomeCatalogData, HomeCatalogKind, HomeRowKind, PlaylistMutationKind,
    PreparedPlayback, SavedSignInRequest, ServerIdentity, SyncPlayCommand, WorkerEvent,
    WorkerSubmitError,
};
use jellyfin_api::remote_command::RemoteCommand;
use jellyfin_api::session_reporting::QueueItem;
use jellyfin_api::websocket::{SyncPlayGroupUpdate, SyncPlayMessage};
use jellyfin_api::{
    BaseItemDto, BaseItemPerson, CollectionRuntime, DeviceProfile, ErrorClass, FavoriteToggle,
    ItemFilter, ItemQuery, ItemSortBy, ItemSortOrder, ItemsResponse, LiveTvChannel,
    PlaybackInfoRequest, PlaybackInfoResponse, PlaybackRequestOverrides, PlaylistEntry,
    baseline_device_profile, collection_runtime, connectivity::OfflineTracker,
};
use kattri_client_core::epg::{EpgProgram, EpgWindow};
use kattri_client_core::focus::FocusGraph;
use kattri_client_core::language_memory::{RecordOutcome, SeriesLanguageMemory, TrackLanguage};
use kattri_client_core::paging::PagingState;
use kattri_client_core::playback_plan::{
    PlaybackMethod, PlaybackSelection, playback_info_candidates_for_profile,
    select_playback_info_source_for_profile,
};
use kattri_client_core::playback_reporting::PlaybackReportAction;
use kattri_client_core::queue::{Queue, Step};
use kattri_client_core::queue_store::{StoredLoadOutcome, StoredQueue};
use kattri_client_core::remote::{LocalAction, local_action};
use kattri_client_core::retry::{RetryDecision, RetryPolicy, RetrySchedule};
use kattri_client_core::search::{SearchDebouncer, SearchInputAction};
use kattri_client_core::segments::{MediaSegment, SkipSettings};
use kattri_client_core::syncplay::playback::group_command_from_wire;
use kattri_client_core::syncplay::{
    ClockEstimate, ClockEstimator, DriftController, GroupCommand, LocalAction as GroupAction,
    PlayAnchor, SyncPlayError, SyncPlayState, clock_sample_from_exchange, map_command,
    target_position_ms,
};
use kattri_i18n::ids;
use kattri_input::input_store::{MapName, map_file_changed, map_file_mtime};
use kattri_input::{GamepadBackend, GamepadError, GamepadEvent, NullGamepad};
use kattri_updater::gate::PlaybackState;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use zeroize::{Zeroize, Zeroizing};

const MAX_PENDING_ARTWORK_REQUESTS: usize = 3;
const MAX_CACHED_ARTWORK_TEXTURES: usize = 36;
const MAX_LIVE_TV_LOGO_TEXTURES: usize = 128;
const CATALOG_PAGE_SIZE: u32 = 50;
const QUICK_CONNECT_PROBE_DELAY: Duration = Duration::from_millis(500);
/// How often the download list is re-read while a transfer runs.
const DOWNLOAD_REFRESH: Duration = Duration::from_millis(750);

/// Ticks per millisecond in Jellyfin's 100-nanosecond unit.
const TICKS_PER_MS: i64 = 10_000;
/// How long after an inbound group command the player's own events count as its echo.
const SYNC_PLAY_ECHO_WINDOW_MS: i64 = 5_000;
/// Position slack when matching an echo, since the player reports a little after a seek.
const SYNC_PLAY_ECHO_TOLERANCE_MS: i64 = 1_500;
/// A forward jump beyond the play time elapsed since the last report counts as a seek. A stall
/// never moves the position forward, so buffering cannot trip this.
const SYNC_PLAY_FORWARD_SEEK_MS: i64 = 2_000;
/// Backward movement beyond this counts as a seek. Reporting jitter stays under it.
const SYNC_PLAY_BACKWARD_SEEK_MS: i64 = 1_000;

/// The player as its last report described it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PlayerSnapshot {
    paused: bool,
    position_ms: i64,
    at_ms: i64,
}

/// The state an inbound group command leaves the player in. The player's own reports that
/// match it are the echo of that command, so they are not sent back to the group (M4-11).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct InboundEcho {
    paused: bool,
    position_ms: i64,
    applied_at_ms: i64,
}

/// What the app knows about the player. It feeds the SyncPlay outbound path and the update
/// gate (M4-11, M4-16). Every `now_ms` in it comes from the same clock.
#[derive(Debug)]
pub(crate) struct PlayerView {
    clock: Instant,
    /// `None` while no session is open.
    player: Option<PlayerSnapshot>,
    echo: Option<InboundEcho>,
}

impl PlayerView {
    fn new() -> Self {
        Self {
            clock: Instant::now(),
            player: None,
            echo: None,
        }
    }

    /// Monotonic milliseconds on the clock every `now_ms` argument uses.
    pub(crate) fn now_ms(&self) -> i64 {
        i64::try_from(self.clock.elapsed().as_millis()).unwrap_or(i64::MAX)
    }

    /// The same clock as [`Self::now_ms`], read at an instant taken on the worker. An instant
    /// before the clock started reads as 0.
    pub(crate) fn monotonic_ms_at(&self, at: Instant) -> i64 {
        i64::try_from(at.saturating_duration_since(self.clock).as_millis()).unwrap_or(i64::MAX)
    }
}

/// A user change in the player that becomes a group command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LocalPlayerChange {
    Pause,
    Unpause,
    Seek { position_ms: i64 },
}

/// Classify the change between two reports. Pause state wins; otherwise a position that moved
/// by more than play time explains is a seek.
fn classify_player_change(prev: PlayerSnapshot, now: PlayerSnapshot) -> Option<LocalPlayerChange> {
    if prev.paused != now.paused {
        return Some(if now.paused {
            LocalPlayerChange::Pause
        } else {
            LocalPlayerChange::Unpause
        });
    }
    let elapsed = now.at_ms.saturating_sub(prev.at_ms).max(0);
    let expected = if now.paused {
        prev.position_ms
    } else {
        prev.position_ms.saturating_add(elapsed)
    };
    let forward = now.position_ms.saturating_sub(expected) > SYNC_PLAY_FORWARD_SEEK_MS;
    let backward = prev.position_ms.saturating_sub(now.position_ms) > SYNC_PLAY_BACKWARD_SEEK_MS;
    (forward || backward).then_some(LocalPlayerChange::Seek {
        position_ms: now.position_ms,
    })
}

/// The position the player should show at `now_ms` if it has applied `echo`.
fn echo_position_ms(echo: InboundEcho, now_ms: i64) -> i64 {
    if echo.paused {
        echo.position_ms
    } else {
        echo.position_ms
            .saturating_add(now_ms.saturating_sub(echo.applied_at_ms).max(0))
    }
}

fn group_command_for(change: LocalPlayerChange) -> SyncPlayCommand {
    match change {
        LocalPlayerChange::Pause => SyncPlayCommand::Pause,
        LocalPlayerChange::Unpause => SyncPlayCommand::Unpause,
        LocalPlayerChange::Seek { position_ms } => SyncPlayCommand::Seek {
            position_ticks: u64::try_from(position_ms.saturating_mul(TICKS_PER_MS)).unwrap_or(0),
        },
    }
}

fn decode_photo_request_tag(encoded: &str) -> Option<(Option<String>, usize, bool)> {
    let mut parts = encoded.split('\u{1f}');
    let tag = parts.next()?;
    let index = parts.next()?.parse().ok()?;
    let visible = match parts.next()? {
        "v" => true,
        "p" => false,
        _ => return None,
    };
    if parts.next().is_some() {
        return None;
    }
    Some((Some(tag.to_owned()), index, visible))
}

fn quick_connect_is_enabled_for(
    server_url: &str,
    allow_insecure_remote_http: bool,
    checked: Option<(&str, bool, bool)>,
) -> bool {
    checked.is_some_and(|(checked_url, checked_opt_in, enabled)| {
        enabled && checked_url == server_url && checked_opt_in == allow_insecure_remote_http
    })
}

#[cfg(debug_assertions)]
static DEBUG_SIGNIN_PENDING: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

fn server_preview_matches(
    server_url: &str,
    allow_insecure_remote_http: bool,
    checked: Option<(&str, bool)>,
) -> bool {
    checked.is_some_and(|(checked_url, checked_opt_in)| {
        checked_url == server_url && checked_opt_in == allow_insecure_remote_http
    })
}

pub(crate) fn duration_from_egui_time(seconds: f64) -> Duration {
    if !seconds.is_finite() || seconds <= 0.0 {
        return Duration::ZERO;
    }
    Duration::from_millis((seconds * 1000.0).min(u64::MAX as f64) as u64)
}

pub(crate) fn unix_now_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX)
        })
}

fn music_queue_report_context(
    queue: &Queue,
    item_id: &str,
) -> Option<(
    jellyfin_api::session_reporting::RepeatMode,
    jellyfin_api::session_reporting::PlaybackOrder,
    Vec<QueueItem>,
)> {
    if !queue.current().is_some_and(|current| current.id == item_id) {
        return None;
    }
    let ordered_items = queue
        .play_order()
        .into_iter()
        .filter_map(|index| queue.items().get(index).cloned())
        .collect();
    Some((queue.repeat(), queue.playback_order(), ordered_items))
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum CatalogSort {
    #[default]
    Name,
    DateAdded,
    ReleaseDate,
    Rating,
}

impl CatalogSort {
    pub(crate) const ALL: [Self; 4] =
        [Self::Name, Self::DateAdded, Self::ReleaseDate, Self::Rating];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Name => ui_text(ids::BROWSE_SORT_NAME),
            Self::DateAdded => ui_text(ids::BROWSE_SORT_DATE_ADDED),
            Self::ReleaseDate => ui_text(ids::BROWSE_SORT_RELEASE_DATE),
            Self::Rating => ui_text(ids::BROWSE_SORT_RATING),
        }
    }

    const fn api_value(self) -> ItemSortBy {
        match self {
            Self::Name => ItemSortBy::SortName,
            Self::DateAdded => ItemSortBy::DateCreated,
            Self::ReleaseDate => ItemSortBy::PremiereDate,
            Self::Rating => ItemSortBy::CommunityRating,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum CatalogPlayedFilter {
    #[default]
    Any,
    Played,
    Unplayed,
}

impl CatalogPlayedFilter {
    pub(crate) const ALL: [Self; 3] = [Self::Any, Self::Played, Self::Unplayed];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Any => ui_text(ids::BROWSE_PLAYED_ANY),
            Self::Played => ui_text(ids::BROWSE_PLAYED_PLAYED),
            Self::Unplayed => ui_text(ids::BROWSE_PLAYED_UNPLAYED),
        }
    }

    const fn api_filter(self) -> Option<ItemFilter> {
        match self {
            Self::Any => None,
            Self::Played => Some(ItemFilter::IsPlayed),
            Self::Unplayed => Some(ItemFilter::IsUnplayed),
        }
    }
}

/// Top-level view: the Home screen (rows and hero) or the Library catalog pages.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AppView {
    Home,
    Library,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Page {
    Home,
    Movies,
    Series,
    Music,
    Photos,
    Playlists,
    Downloads,
    LiveTv,
    /// Read-only server dashboard (M4-22). Reachable only when `admin.dashboard` is on.
    Dashboard,
    /// Browse and manage Jellyfin SyncPlay groups.
    SyncPlay,
    Settings,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BrowseContext {
    pub parent_id: String,
    pub title: String,
    pub is_collection: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlaybackReportSubmitError {
    WorkerUnavailable,
    CommandQueueClosed,
    /// Admission was refused; the caller still owns responsibility for this report.
    CommandQueueFull,
    ProgressRetryBudgetExhausted,
}

const MAX_CONSECUTIVE_PROGRESS_REPORT_FAILURES: u8 = 3;

#[cfg(test)]
mod worker_admission_tests {
    use super::*;
    use jellyfin_api::PlaybackProgressInfo;

    #[test]
    fn refused_start_does_not_advance_report_session_or_reset_failure_budget() {
        let mut app = KattriApp::new(egui::Context::default());
        let worker = ApiWorker::queued_for_test();
        let progress = PlaybackReportAction::Progress(PlaybackProgressInfo::new("local-fixture"));
        for request_id in 0..32 {
            worker
                .report_playback_action(request_id, progress.clone())
                .expect("fill queue");
        }
        app.worker = Some(worker);
        app.playback_report_session_generation = 7;
        app.consecutive_progress_report_failures = 2;
        assert_eq!(
            app.report_playback_action(PlaybackReportAction::Started(PlaybackProgressInfo::new(
                "local-fixture"
            ))),
            Err(PlaybackReportSubmitError::CommandQueueFull)
        );
        assert_eq!(app.playback_report_session_generation, 7);
        assert_eq!(app.consecutive_progress_report_failures, 2);
        assert!(app.pending_playback_report_requests.is_empty());
        assert!(
            app.playback_report_feedback
                .as_deref()
                .is_some_and(|text| text.contains("busy"))
        );
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PlaybackReportKind {
    Started,
    Progress,
    Stopped,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PendingPlaybackReport {
    kind: PlaybackReportKind,
    session_generation: u64,
}

impl PlaybackReportKind {
    fn from_action(action: &PlaybackReportAction) -> Self {
        match action {
            PlaybackReportAction::Started(_) => Self::Started,
            PlaybackReportAction::Progress(_) => Self::Progress,
            PlaybackReportAction::Stopped(_) => Self::Stopped,
        }
    }
}

impl Page {
    pub const ALL: [Self; 11] = [
        Self::Home,
        Self::Movies,
        Self::Series,
        Self::Music,
        Self::Photos,
        Self::Playlists,
        Self::Downloads,
        Self::LiveTv,
        Self::Dashboard,
        Self::SyncPlay,
        Self::Settings,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Home => ui_text(ids::SHELL_NAV_ALL_ITEMS),
            Self::Movies => ui_text(ids::SHELL_NAV_MOVIES),
            Self::Series => ui_text(ids::SHELL_NAV_SERIES),
            Self::Music => ui_text(ids::SHELL_NAV_MUSIC),
            Self::Photos => ui_text(ids::SHELL_NAV_PHOTOS),
            Self::Playlists => ui_text(ids::SHELL_NAV_PLAYLISTS),
            Self::Downloads => ui_text(ids::SHELL_NAV_DOWNLOADS),
            Self::LiveTv => ui_text(ids::SHELL_NAV_LIVE_TV),
            Self::Dashboard => ui_text(ids::SHELL_NAV_DASHBOARD),
            Self::SyncPlay => "Watch Together",
            Self::Settings => ui_text(ids::SHELL_NAV_SETTINGS),
        }
    }

    pub fn mark(self) -> &'static str {
        match self {
            Self::Home => ui_text(ids::SHELL_RAIL_HOME),
            Self::Movies => ui_text(ids::SHELL_RAIL_MOVIES),
            Self::Series => ui_text(ids::SHELL_RAIL_SERIES),
            Self::Music => ui_text(ids::SHELL_RAIL_MUSIC),
            Self::Photos => ui_text(ids::SHELL_RAIL_PHOTOS),
            Self::Playlists => ui_text(ids::SHELL_RAIL_PLAYLISTS),
            Self::Downloads => ui_text(ids::SHELL_RAIL_DOWNLOADS),
            Self::LiveTv => ui_text(ids::SHELL_RAIL_LIVE_TV),
            Self::Dashboard => ui_text(ids::SHELL_RAIL_DASHBOARD),
            Self::SyncPlay => "SyncPlay",
            Self::Settings => ui_text(ids::SHELL_RAIL_SETTINGS),
        }
    }

    /// Server-side `/Items` filters for the library pages.
    /// Search intentionally remains global, so callers omit this when a
    /// search term is present.
    pub(crate) const fn item_types(self) -> Option<&'static [&'static str]> {
        match self {
            Self::Movies => Some(&["Movie"]),
            // Shows only, as in Jellyfin's own clients; episodes are reached through each show.
            Self::Series => Some(&["Series"]),
            Self::Music => Some(&["Audio", "MusicAlbum"]),
            Self::Photos => Some(&["Photo", "Folder", "CollectionFolder"]),
            Self::Playlists => Some(&["Playlist"]),
            Self::Home
            | Self::Downloads
            | Self::LiveTv
            | Self::Dashboard
            | Self::SyncPlay
            | Self::Settings => None,
        }
    }

    pub(crate) const fn is_catalog(self) -> bool {
        matches!(
            self,
            Self::Home | Self::Movies | Self::Series | Self::Music | Self::Photos | Self::Playlists
        )
    }
}

#[derive(Clone, Debug)]
pub enum ConnectionStatus {
    Disconnected { error: Option<String> },
    Connecting { message: String },
    LoadingItems,
    Connected,
    LibraryError(String),
}

/// A one-click playback request: the item is selected, its playback options are negotiated,
/// and the stream starts as soon as both the item record and the options are ready.
#[derive(Clone, Debug)]
pub(crate) struct AutoPlay {
    item_id: String,
    start_over: bool,
}

struct PlaybackRecovery {
    item_id: String,
    options_request_id: u64,
    start_time_ticks: Option<i64>,
    remaining: VecDeque<PlaybackSelection>,
}

/// The source and method of the most recent movie/episode load, kept so a
/// failure after playback began can reopen the same item at its position.
#[derive(Clone)]
struct ActivePlayback {
    item_id: String,
    options_request_id: u64,
    start_request_id: u64,
    selection: PlaybackSelection,
}

/// Reopens allowed after mid-stream failures for one user-started playback.
const MAX_MID_STREAM_RECOVERIES: u8 = 3;

fn api_play_method(method: PlaybackMethod) -> jellyfin_api::PlayMethod {
    match method {
        PlaybackMethod::DirectPlay => jellyfin_api::PlayMethod::DirectPlay,
        PlaybackMethod::DirectStream => jellyfin_api::PlayMethod::DirectStream,
        PlaybackMethod::Transcode => jellyfin_api::PlayMethod::Transcode,
    }
}

fn selection_method(method: jellyfin_api::PlayMethod) -> PlaybackMethod {
    match method {
        jellyfin_api::PlayMethod::DirectPlay => PlaybackMethod::DirectPlay,
        jellyfin_api::PlayMethod::DirectStream => PlaybackMethod::DirectStream,
        jellyfin_api::PlayMethod::Transcode => PlaybackMethod::Transcode,
    }
}

pub struct KattriApp {
    pub(crate) worker: Option<ApiWorker>,
    /// Download list and page state. The worker owns the jobs; this copy is what the page reads.
    pub(crate) downloads: DownloadsState,
    /// Last update result for the update panel. Only the UI thread writes it.
    pub(crate) update: UpdateState,
    pub(crate) connection: ConnectionStatus,
    pub(crate) offline_tracker: OfflineTracker,
    pub(crate) network_notice: Option<ErrorClass>,
    network_retry_schedule: RetrySchedule,
    network_retry_deadline: Option<Duration>,
    network_retry_context: Option<(AppView, Page, Option<String>)>,
    ui_time: Duration,
    pub(crate) server_url: String,
    pub(crate) username: String,
    // This is only the volatile input buffer; it is moved to the worker and zeroized on drop.
    pub(crate) password_input: Zeroizing<String>,
    pub(crate) allow_insecure_remote_http: bool,
    /// The "Stay signed in on this device" choice on the sign-in form. Off until ticked.
    pub(crate) remember_sign_in: bool,
    /// Whether the credential store can keep a sign-in. `None` until the worker has asked it;
    /// the form offers the option unless this is `Some(false)`.
    pub(crate) saved_sign_in_available: Option<bool>,
    pub(crate) server: Option<ServerIdentity>,
    pub(crate) user_name: Option<String>,
    /// The signed-in user is a server administrator: the card menu offers admin actions.
    pub(crate) user_is_admin: bool,
    /// An item the viewer asked to delete, waiting for confirmation: `(id, title)`.
    pub(crate) pending_delete: Option<(String, String)>,
    /// Admin actions in flight: request id to item title.
    admin_requests: HashMap<u64, String>,
    pub(crate) items: Vec<crate::catalog::MediaItem>,
    pub(crate) total_record_count: Option<u32>,
    pub(crate) collection_runtime: CollectionRuntime,
    pub(crate) start_index: u32,
    pub(crate) catalog_next_index: u32,
    pub(crate) catalog_end_reached: bool,
    pub(crate) catalog_loaded: bool,
    loaded_search: Option<String>,
    pub(crate) catalog_loading: bool,
    pub(crate) catalog_error: Option<String>,
    pub(crate) catalog_filter_notice: Option<String>,
    unsupported_catalog_filters: Vec<(Page, ItemFilter)>,
    pub(crate) selected_item_id: Option<String>,
    pub(crate) selected_details: Option<BaseItemDto>,
    pub(crate) selected_artwork_texture: Option<egui::TextureHandle>,
    detail_artwork_request_id: u64,
    detail_backdrop_attempted: bool,
    /// Backdrop texture for the Home hero, keyed by the item it belongs to.
    pub(crate) home_hero_backdrop: Option<(String, egui::TextureHandle)>,
    /// The backdrop request in flight or already tried for the Home hero: (request id, item id).
    home_hero_request: Option<(u64, String)>,
    /// Set by `play_item`; consumed by `poll_auto_play` once the stream can start.
    pub(crate) auto_play: Option<AutoPlay>,
    pub(crate) details_loading: bool,
    pub(crate) details_error: Option<String>,
    pub(crate) series_seasons: Vec<BaseItemDto>,
    pub(crate) series_seasons_loading: bool,
    pub(crate) series_seasons_error: Option<String>,
    pub(crate) series_selected_season_id: Option<String>,
    pub(crate) series_episodes: Vec<BaseItemDto>,
    pub(crate) series_episodes_loading: bool,
    pub(crate) series_episodes_error: Option<String>,
    pub(crate) similar_items: Vec<BaseItemDto>,
    pub(crate) similar_loading: bool,
    pub(crate) selected_person: Option<BaseItemPerson>,
    pub(crate) person_items: Vec<BaseItemDto>,
    pub(crate) person_items_loading: bool,
    pub(crate) person_items_error: Option<String>,
    pub(crate) person_artwork_textures: HashMap<String, egui::TextureHandle>,
    pub(crate) playback_info: Option<PlaybackInfoResponse>,
    device_profile: DeviceProfile,
    /// Output devices mpv reported, for the Settings device picker. Empty until the player
    /// has probed, and in builds without a player; the page then shows a text field.
    pub(crate) audio_devices: Vec<String>,
    playback_overrides: PlaybackRequestOverrides,
    pub(crate) playback_loading: bool,
    pub(crate) playback_error: Option<String>,
    pub(crate) playback_start_loading: bool,
    pub(crate) playback_start_error: Option<String>,
    pub(crate) playback_start_status: Option<String>,
    playback_report_feedback: Option<String>,
    pending_playback: Option<PreparedPlayback>,
    playback_recovery: Option<PlaybackRecovery>,
    active_playback: Option<ActivePlayback>,
    mid_stream_recoveries: u8,
    /// A prepared episode waiting for its item record, which carries the series ID (M2-10).
    /// Kept with its start request ID, so a newer request drops it.
    held_playback: Option<(u64, String, Box<PreparedPlayback>)>,
    pub(crate) page: Page,
    pub(crate) view: AppView,
    pub(crate) browse_context: Option<BrowseContext>,
    pub(crate) home: HomeState,
    pub(crate) live_tv_channels: Vec<LiveTvChannel>,
    pub(crate) live_tv_channel_logos: HashMap<String, egui::TextureHandle>,
    live_tv_logo_queue: VecDeque<(String, String)>,
    live_tv_logo_pending: HashMap<u64, (String, String)>,
    live_tv_logo_attempted: HashSet<String>,
    pub(crate) live_tv_total_count: Option<u32>,
    pub(crate) live_tv_loading: bool,
    pub(crate) live_tv_loaded: bool,
    pub(crate) live_tv_not_set_up: bool,
    pub(crate) live_tv_error: Option<ErrorClass>,
    pub(crate) live_tv_notice: Option<String>,
    pub(crate) live_tv_selected_channel_id: Option<String>,
    pub(crate) live_tv_tune_request_id: Option<u64>,
    pub(crate) live_tv_retry_available: bool,
    live_tv_request_id: u64,
    live_tv_requested_start_index: u32,
    pub(crate) live_tv_programs: Vec<EpgProgram>,
    pub(crate) live_tv_guide_window: Option<EpgWindow>,
    pub(crate) live_tv_guide_loading: bool,
    pub(crate) live_tv_guide_available: bool,
    pub(crate) live_tv_guide_error: Option<ErrorClass>,
    live_tv_guide_request_id: u64,
    live_tv_guide_requested_channel_ids: Vec<String>,
    live_tv_guide_pending_channel_ids: Vec<String>,
    pub(crate) live_tv_guide_refreshed_at_unix: Option<i64>,
    pub(crate) photo_items: Vec<BaseItemDto>,
    pub(crate) photo_thumbnails: HashMap<String, egui::TextureHandle>,
    pub(crate) photo_full_size_images: HashMap<String, egui::TextureHandle>,
    pub(crate) photo_selected_id: Option<String>,
    pub(crate) photo_page_start: usize,
    photo_thumbnail_pending: HashMap<u64, String>,
    photo_thumbnail_attempted: HashSet<String>,
    /// Photo ids in the visible thumbnail window of the latest frame.
    photo_visible_ids: HashSet<String>,
    photo_full_size_pending: HashMap<u64, String>,
    pub(crate) photo_full_size_failed: HashSet<String>,
    pub(crate) music_queue: Queue,
    /// Neighbours of the episode last started, for previous/next and auto-play.
    episode_neighbors: Option<EpisodeNeighbors>,
    /// The episode to play next in the series on screen.
    series_next_up: Option<SeriesNextUp>,
    /// A shuffled run of episodes; previous, next and auto-play follow it while one of its
    /// episodes plays.
    shuffle_queue: Option<ShuffleQueue>,
    shuffle_request_id: u64,
    /// The page an item was opened from (a series for its episode), where Back returns.
    detail_parent: Option<String>,
    /// Stills for the episode rows of the series on screen.
    pub(crate) episode_thumbnails: HashMap<String, egui::TextureHandle>,
    episode_thumbnail_pending: HashMap<u64, String>,
    /// The user's playlists for the card menu's Add to playlist.
    menu_playlists: crate::ui::item_menu::MenuPlaylists,
    /// The server's collections for the administrator entry Add to collection.
    menu_collections: crate::ui::item_menu::MenuPlaylists,
    menu_collections_request_id: u64,
    menu_playlists_request_id: u64,
    /// A card-menu playlist addition in flight: request, item title, playlist name.
    menu_playlist_add: Option<(u64, String, String)>,
    /// A short confirmation shown over the page, with the time it disappears (set when first
    /// drawn).
    toast: Option<(String, Option<f64>)>,
    pub(crate) music_track_names: HashMap<String, String>,
    pub(crate) music_album_tracks_loading: bool,
    pub(crate) music_album_tracks_error: Option<ErrorClass>,
    music_album_tracks_request_id: u64,
    music_queue_auto_play_item_id: Option<String>,
    pub(crate) music_queue_notice: Option<String>,
    pending_stored_music_queue: Option<StoredQueue>,
    music_queue_restore_request_id: u64,
    pub(crate) music_queue_restore_loading: bool,
    pub(crate) music_queue_resume_position_ms: Option<u64>,
    music_queue_resume_position_ticks: Option<i64>,
    music_queue_last_saved_json: Option<String>,
    music_queue_last_saved_at: Duration,
    music_queue_position_ms: u64,
    music_queue_persistence_suspended: bool,
    pub(crate) playlist_entries: Vec<PlaylistEntry>,
    pub(crate) playlist_items_loading: bool,
    pub(crate) playlist_items_error: Option<ErrorClass>,
    pub(crate) opened_playlist_id: Option<String>,
    playlist_items_request_id: u64,
    pub(crate) playlist_mutation_loading: bool,
    pub(crate) playlist_mutation_error: Option<ErrorClass>,
    pub(crate) playlist_mutation_notice: Option<String>,
    playlist_mutation_request_id: u64,
    pub(crate) playlist_ui_state: PlaylistUiState,
    pub(crate) playlist_candidates: Vec<BaseItemDto>,
    pub(crate) playlist_candidates_loading: bool,
    pub(crate) playlist_candidates_error: Option<ErrorClass>,
    playlist_candidates_request_id: u64,
    pub(crate) focus_graph: FocusGraph,
    pub(crate) gamepad: Box<dyn GamepadBackend>,
    /// True while a video fills the window: gamepad input then drives the player, not the
    /// hidden library page.
    player_owns_input: bool,
    /// Gamepad actions collected for the player since the host last took them.
    pub(crate) player_nav_actions: Vec<kattri_client_core::focus::NavAction>,
    // Keyboard map mtime at the last load, and when the next check is due.
    keymap_mtime_seen: Option<SystemTime>,
    keymap_check_due: Duration,
    // Draft text is never sent until the user submits it.
    pub(crate) search_query: String,
    pub(crate) submitted_search: Option<String>,
    pub(crate) search_results: Vec<BaseItemDto>,
    pub(crate) search_browse_group: Option<crate::ui::search::SearchGroup>,
    pub(crate) live_tv_search_results: Vec<BaseItemDto>,
    pub(crate) live_tv_search_loading: bool,
    pub(crate) live_tv_search_start_index: u32,
    pub(crate) live_tv_search_total: Option<u32>,
    live_tv_search_request_id: Option<u64>,
    pub(crate) search_stale: bool,
    pub(crate) search_active: bool,
    pub(crate) catalog_sort: CatalogSort,
    pub(crate) catalog_sort_order: ItemSortOrder,
    pub(crate) catalog_played_filter: CatalogPlayedFilter,
    pub(crate) catalog_favorites_only: bool,
    /// The alphabet picker's letter, `'#'` for names that start with a digit or symbol.
    pub(crate) catalog_name_prefix: Option<char>,
    /// The genre filter's choice.
    pub(crate) catalog_genre: Option<String>,
    /// Genres offered by the genre filter, for [`Self::catalog_genres_scope`].
    pub(crate) catalog_genres: Vec<String>,
    catalog_genres_request_id: u64,
    /// The page and folder the genre list belongs to.
    catalog_genres_scope: Option<(Page, Option<String>)>,
    pub(crate) high_contrast: bool,
    pub(crate) reduce_motion: bool,
    pub(crate) interface_size: kattri_settings::InterfaceSize,
    pub(crate) settings: SettingsState,
    /// Profile picker window, open while `Some`. Opened from Settings or at launch.
    pub(crate) profile_picker: Option<PickerState>,
    /// The "Open stream address" dialog while it is open (M6-15).
    open_url: Option<crate::ui::open_url::OpenUrlState>,
    /// An address the viewer confirmed, waiting for the player host to take it.
    pending_stream_url: Option<kattri_client_core::stream_url::StreamUrl>,
    /// Launch notice, such as a missing profile folder. The user dismisses it once.
    launch_notice: Option<String>,
    pub(crate) saved_servers: SavedServers,
    pub(crate) quick_connect: Option<QuickConnectFlow>,
    server_preview: Option<(String, bool, ServerIdentity)>,
    server_check_pending: Option<(u64, String, bool)>,
    pub(crate) server_check_error: Option<String>,
    quick_connect_probe_after: Option<Duration>,
    quick_connect_probe_pending: Option<(u64, String, bool)>,
    quick_connect_enabled_for: Option<(String, bool, bool)>,
    next_request_id: u64,
    search_debouncer: SearchDebouncer,
    connection_request_id: u64,
    catalog_request_id: u64,
    /// Latest admin dashboard read (M4-22). A reply for any other request is stale.
    pub(crate) admin_load: AdminLoad,
    admin_request_id: Option<u64>,
    details_request_id: u64,
    watched: Option<WatchedItemState>,
    favorite: Option<FavoriteItemState>,
    series_seasons_request_id: u64,
    series_episodes_request_id: u64,
    /// The segments request for the item being prepared: (request id, item id).
    media_segments_request: Option<(u64, String)>,
    /// Segments fetched for the current item, waiting for the native host to take them.
    loaded_media_segments: Option<(String, Vec<MediaSegment>)>,
    /// Player actions for the native host to apply, in arrival order (M4-13, M4-11).
    pending_player_intents: Vec<LocalAction>,
    /// SyncPlay membership and the last applied group command (M4-11).
    sync_play_state: SyncPlayState,
    /// User-facing group list and request state for the SyncPlay screen.
    pub(crate) sync_play_ui: crate::ui::syncplay::SyncPlayScreenState,
    /// Join request awaiting the worker's answer: (request id, group id).
    sync_play_pending_join: Option<(u64, String)>,
    /// Leave request awaiting the worker's answer.
    sync_play_pending_leave: Option<u64>,
    sync_play_drift: DriftController,
    /// Server clock estimate for inbound SyncPlay commands. It has no samples until a server
    /// time exchange records one, so inbound commands are refused until then (M4-11).
    sync_play_clock: ClockEstimator,
    /// Player view for the SyncPlay outbound path and the update gate (M4-11, M4-16).
    player_view: PlayerView,
    similar_request_id: u64,
    person_items_request_id: u64,
    person_artwork_queue: Vec<BaseItemPerson>,
    pending_person_artwork: HashMap<u64, String>,
    attempted_person_artwork: HashSet<String>,
    playback_request_id: u64,
    playback_start_request_id: u64,
    pending_playback_report_requests: HashMap<u64, PendingPlaybackReport>,
    playback_report_session_generation: u64,
    consecutive_progress_report_failures: u8,
    pending_artwork_requests: HashMap<u64, (String, String)>,
    attempted_artwork: HashSet<(String, String)>,
    artwork_access_ticks: HashMap<(String, String), u64>,
    next_artwork_access_tick: u64,
    /// First access tick of the latest visible-artwork pass. Textures touched at or after it
    /// are on screen and are never evicted.
    visible_artwork_from_tick: u64,
}

impl KattriApp {
    #[cfg(feature = "photo-counters")]
    #[doc(hidden)]
    pub fn decode_photo_counter_fixture(
        bytes: Vec<u8>,
        index: usize,
        visible: bool,
        counters: &kattri_client_core::photo_grid::DecodeCounters,
    ) -> Option<(u32, u32, Vec<u8>)> {
        crate::worker::decode_photo_image_bytes_counted(bytes, false, index, visible, counters).ok()
    }

    #[cfg(feature = "photo-counters")]
    pub fn photo_decode_counter_snapshot(
        &self,
    ) -> Option<kattri_client_core::photo_grid::DecodeSnapshot> {
        self.worker.as_ref().map(ApiWorker::photo_decode_counters)
    }

    fn cancel_pending_photo_decodes(&mut self) {
        let request_ids = self
            .photo_thumbnail_pending
            .keys()
            .chain(self.photo_full_size_pending.keys())
            .copied()
            .collect::<Vec<_>>();
        if let Some(worker) = self.worker.as_ref()
            && let Err(error) = worker.cancel_photo_image_requests(request_ids.clone())
        {
            tracing::debug!(%error, "could not cancel stale photo image requests");
        }
        #[cfg(feature = "photo-counters")]
        if let Some(worker) = self.worker.as_ref() {
            worker.record_photo_decode_cancellations(request_ids.len());
        }
        self.photo_thumbnail_pending.clear();
        self.photo_full_size_pending.clear();
    }

    pub(crate) fn update_photo_thumbnail_window(&mut self, visible: &[(String, String)]) {
        self.photo_visible_ids = visible.iter().map(|(item_id, _)| item_id.clone()).collect();
        let requested_ids = visible
            .iter()
            .map(|(item_id, _)| item_id.as_str())
            .collect::<HashSet<_>>();
        let cancelled = self
            .photo_thumbnail_pending
            .iter()
            .filter(|(_, item_id)| !requested_ids.contains(item_id.as_str()))
            .map(|(&request_id, _)| request_id)
            .collect::<Vec<_>>();
        if cancelled.is_empty() {
            return;
        }
        for request_id in &cancelled {
            if let Some(item_id) = self.photo_thumbnail_pending.remove(request_id) {
                self.photo_thumbnail_attempted.remove(&item_id);
            }
        }
        if let Some(worker) = self.worker.as_ref()
            && let Err(error) = worker.cancel_photo_image_requests(cancelled.clone())
        {
            tracing::debug!(%error, "could not cancel off-window photo image requests");
        }
        #[cfg(feature = "photo-counters")]
        if let Some(worker) = self.worker.as_ref() {
            worker.record_photo_decode_cancellations(cancelled.len());
        }
    }

    pub fn new(ctx: egui::Context) -> Self {
        Self::with_launch_profile(ctx, None)
    }

    /// Start the app on the profile named by `--profile`, or on the active profile when it is
    /// `None`. A missing profile falls back to `default` with a launch notice.
    pub fn with_launch_profile(ctx: egui::Context, requested: Option<&str>) -> Self {
        #[allow(unused_mut)]
        let mut app = Self::with_launch_profile_inner(ctx, requested);
        #[cfg(debug_assertions)]
        app.apply_debug_signin();
        app
    }

    /// Debug builds only: logs the connection and catalog state when it changes.
    #[cfg(debug_assertions)]
    fn debug_log_changes(&self) {
        static LAST: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());
        let snapshot = format!(
            "connection={:?} server_check_error={:?} catalog_error={:?} catalog_loading={} items={} user={:?} selected={:?} details_loading={} auto_play={:?} info_loading={} info_ready={} playback_error={:?} start_loading={} start_status={:?} start_error={:?}",
            self.connection,
            self.server_check_error,
            self.catalog_error,
            self.catalog_loading,
            self.items.len(),
            self.user_name,
            self.selected_item_id,
            self.details_loading,
            self.auto_play,
            self.playback_loading,
            self.playback_info.is_some(),
            self.playback_error,
            self.playback_start_loading,
            self.playback_start_status,
            self.playback_start_error,
        );
        if let Ok(mut last) = LAST.lock()
            && *last != snapshot
        {
            crate::debug_log::log(&snapshot);
            *last = snapshot;
        }
    }

    /// Debug builds only. A launcher script (scripts/run_debug_signin.ps1) decrypts the stored
    /// test credential in memory and sets KATTRI_DEBUG_URL, KATTRI_DEBUG_USER and
    /// KATTRI_DEBUG_PASSWORD for this one process. They fill the sign-in form and start the
    /// server check; Connect is pressed once the server answers. Release builds do not contain
    /// this code. Nothing is written to disk.
    #[cfg(debug_assertions)]
    fn apply_debug_signin(&mut self) {
        let (Some(url), Some(user), Some(password)) = (
            std::env::var_os("KATTRI_DEBUG_URL"),
            std::env::var_os("KATTRI_DEBUG_USER"),
            std::env::var_os("KATTRI_DEBUG_PASSWORD"),
        ) else {
            return;
        };
        let (Some(url), Some(user), Some(password)) =
            (url.to_str(), user.to_str(), password.to_str())
        else {
            return;
        };
        self.server_url = url.to_owned();
        self.username = user.to_owned();
        self.password_input = Zeroizing::new(password.to_owned());
        self.allow_insecure_remote_http = url.starts_with("http://");
        crate::debug_log::install_panic_hook();
        crate::debug_log::log(&format!("debug sign-in: prefilled form for {url}"));
        // Start the server check now; the per-frame hook presses Connect once it answers.
        self.check_current_server();
        DEBUG_SIGNIN_PENDING.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    fn with_launch_profile_inner(ctx: egui::Context, requested: Option<&str>) -> Self {
        crate::theme::install_fonts(&ctx);
        crate::theme::apply(&ctx, false, true);
        let worker = ApiWorker::new(ctx.clone()).ok();
        let connection = if worker.is_some() {
            ConnectionStatus::Disconnected { error: None }
        } else {
            ConnectionStatus::Disconnected {
                error: Some(ui_text(ids::APP_ERROR_WORKER_START).into()),
            }
        };

        let settings = SettingsState::open(default_profiles_dir().as_deref(), requested);
        // Off unless the saved switch is on. The switch is read once here, so a change in
        // Settings takes effect at the next start. Nothing is uploaded from the crash folder.
        let crash_reporting =
            kattri_client_core::crash_report::CrashReportingSettings::from_diagnostics(
                &settings.settings().diagnostics,
            );
        if crash_reporting.enabled
            && let Some(crash_dir) = kattri_logging::crash_directory()
        {
            kattri_client_core::crash_report::install_panic_hook(
                crash_dir,
                crash_reporting,
                env!("CARGO_PKG_VERSION"),
                &["jellyfin_egui_client", "kattri_client_core"],
            );
        }
        // Profile changes restart the app, so loading once here covers the running profile.
        // The mtime is read before the load, as reload_keymap_if_changed does.
        let keymap_dir = settings.keymap_dir();
        let keymap_mtime_seen = keyboard_map_mtime(keymap_dir.as_deref());
        crate::input::reload_keymap(&ctx, keymap_dir.as_deref());
        let (pending_stored_music_queue, music_queue_notice) = match settings.load_queue() {
            Some(Ok(StoredLoadOutcome::Loaded(stored))) if !stored.queue.is_empty() => {
                (Some(stored), None)
            }
            Some(Ok(StoredLoadOutcome::MovedAside { .. })) => (
                None,
                Some(ui_text(ids::MUSIC_QUEUE_INVALID_MOVED).to_owned()),
            ),
            Some(Err(_)) => (None, Some(ui_text(ids::MUSIC_QUEUE_READ_FAILED).to_owned())),
            _ => (None, None),
        };
        let playback_overrides = playback_overrides_from_settings(settings.settings());
        let high_contrast = settings.settings().general.high_contrast;
        let reduce_motion = settings.settings().general.reduce_motion;
        let interface_size = settings.settings().general.interface_size;
        let launch_notice = settings.launch_notice().map(str::to_owned);
        Self {
            worker,
            connection,
            offline_tracker: OfflineTracker::new(),
            network_notice: None,
            network_retry_schedule: RetrySchedule::new(RetryPolicy::default()),
            network_retry_deadline: None,
            network_retry_context: None,
            ui_time: Duration::ZERO,
            server_url: String::new(),
            username: String::new(),
            password_input: Zeroizing::new(String::new()),
            allow_insecure_remote_http: false,
            remember_sign_in: false,
            saved_sign_in_available: None,
            server: None,
            user_name: None,
            user_is_admin: false,
            pending_delete: None,
            admin_requests: HashMap::new(),
            items: Vec::new(),
            total_record_count: None,
            collection_runtime: CollectionRuntime::default(),
            start_index: 0,
            catalog_next_index: 0,
            catalog_end_reached: false,
            catalog_loaded: false,
            loaded_search: None,
            catalog_loading: false,
            catalog_error: None,
            downloads: DownloadsState::default(),
            update: UpdateState::default(),
            catalog_filter_notice: None,
            unsupported_catalog_filters: Vec::new(),
            selected_item_id: None,
            selected_details: None,
            selected_artwork_texture: None,
            detail_artwork_request_id: 0,
            detail_backdrop_attempted: false,
            home_hero_backdrop: None,
            home_hero_request: None,
            auto_play: None,
            details_loading: false,
            details_error: None,
            series_seasons: Vec::new(),
            series_seasons_loading: false,
            series_seasons_error: None,
            series_selected_season_id: None,
            series_episodes: Vec::new(),
            series_episodes_loading: false,
            series_episodes_error: None,
            similar_items: Vec::new(),
            similar_loading: false,
            selected_person: None,
            person_items: Vec::new(),
            person_items_loading: false,
            person_items_error: None,
            person_artwork_textures: HashMap::new(),
            playback_info: None,
            device_profile: baseline_device_profile(),
            audio_devices: Vec::new(),
            playback_overrides,
            playback_loading: false,
            playback_error: None,
            playback_start_loading: false,
            playback_start_error: None,
            playback_start_status: None,
            playback_report_feedback: None,
            pending_playback: None,
            playback_recovery: None,
            active_playback: None,
            mid_stream_recoveries: 0,
            held_playback: None,
            page: Page::Home,
            view: AppView::Home,
            browse_context: None,
            home: HomeState::default(),
            live_tv_channels: Vec::new(),
            live_tv_channel_logos: HashMap::new(),
            live_tv_logo_queue: VecDeque::new(),
            live_tv_logo_pending: HashMap::new(),
            live_tv_logo_attempted: HashSet::new(),
            live_tv_total_count: None,
            live_tv_loading: false,
            live_tv_loaded: false,
            live_tv_not_set_up: false,
            live_tv_error: None,
            live_tv_notice: None,
            live_tv_selected_channel_id: None,
            live_tv_tune_request_id: None,
            live_tv_retry_available: false,
            live_tv_request_id: 0,
            live_tv_requested_start_index: 0,
            live_tv_programs: Vec::new(),
            live_tv_guide_window: None,
            live_tv_guide_loading: false,
            live_tv_guide_available: false,
            live_tv_guide_error: None,
            live_tv_guide_request_id: 0,
            live_tv_guide_requested_channel_ids: Vec::new(),
            live_tv_guide_pending_channel_ids: Vec::new(),
            live_tv_guide_refreshed_at_unix: None,
            photo_items: Vec::new(),
            photo_thumbnails: HashMap::new(),
            photo_full_size_images: HashMap::new(),
            photo_selected_id: None,
            photo_page_start: 0,
            photo_thumbnail_pending: HashMap::new(),
            photo_thumbnail_attempted: HashSet::new(),
            photo_visible_ids: HashSet::new(),
            photo_full_size_pending: HashMap::new(),
            photo_full_size_failed: HashSet::new(),
            music_queue: Queue::new(),
            episode_neighbors: None,
            series_next_up: None,
            shuffle_queue: None,
            shuffle_request_id: 0,
            detail_parent: None,
            episode_thumbnails: HashMap::new(),
            episode_thumbnail_pending: HashMap::new(),
            menu_playlists: Default::default(),
            menu_collections: Default::default(),
            menu_collections_request_id: 0,
            menu_playlists_request_id: 0,
            menu_playlist_add: None,
            toast: None,
            music_track_names: HashMap::new(),
            music_album_tracks_loading: false,
            music_album_tracks_error: None,
            music_album_tracks_request_id: 0,
            music_queue_auto_play_item_id: None,
            music_queue_notice,
            pending_stored_music_queue,
            music_queue_restore_request_id: 0,
            music_queue_restore_loading: false,
            music_queue_resume_position_ms: None,
            music_queue_resume_position_ticks: None,
            music_queue_last_saved_json: None,
            music_queue_last_saved_at: Duration::ZERO,
            music_queue_position_ms: 0,
            music_queue_persistence_suspended: false,
            playlist_entries: Vec::new(),
            playlist_items_loading: false,
            playlist_items_error: None,
            opened_playlist_id: None,
            playlist_items_request_id: 0,
            playlist_mutation_loading: false,
            playlist_mutation_error: None,
            playlist_mutation_notice: None,
            playlist_mutation_request_id: 0,
            playlist_ui_state: PlaylistUiState::default(),
            playlist_candidates: Vec::new(),
            playlist_candidates_loading: false,
            playlist_candidates_error: None,
            playlist_candidates_request_id: 0,
            focus_graph: FocusGraph::default(),
            gamepad: Box::new(ObservedGamepad {
                ctx: ctx.clone(),
                inner: gamepad_backend(),
            }),
            player_owns_input: false,
            player_nav_actions: Vec::new(),
            keymap_mtime_seen,
            keymap_check_due: Duration::ZERO,
            search_query: String::new(),
            submitted_search: None,
            search_results: Vec::new(),
            search_browse_group: None,
            live_tv_search_results: Vec::new(),
            live_tv_search_loading: false,
            live_tv_search_start_index: 0,
            live_tv_search_total: None,
            live_tv_search_request_id: None,
            search_stale: false,
            search_active: false,
            catalog_sort: CatalogSort::Name,
            catalog_sort_order: ItemSortOrder::Ascending,
            catalog_played_filter: CatalogPlayedFilter::Any,
            catalog_favorites_only: false,
            catalog_name_prefix: None,
            catalog_genre: None,
            catalog_genres: Vec::new(),
            catalog_genres_request_id: 0,
            catalog_genres_scope: None,
            high_contrast,
            interface_size,
            reduce_motion,
            settings,
            profile_picker: None,
            open_url: None,
            pending_stream_url: None,
            launch_notice,
            saved_servers: SavedServers::open(default_servers_path()),
            quick_connect: None,
            server_preview: None,
            server_check_pending: None,
            server_check_error: None,
            quick_connect_probe_after: None,
            quick_connect_probe_pending: None,
            quick_connect_enabled_for: None,
            next_request_id: 0,
            search_debouncer: SearchDebouncer::default(),
            connection_request_id: 0,
            catalog_request_id: 0,
            admin_load: AdminLoad::Pending,
            admin_request_id: None,
            details_request_id: 0,
            watched: None,
            favorite: None,
            series_seasons_request_id: 0,
            series_episodes_request_id: 0,
            media_segments_request: None,
            loaded_media_segments: None,
            pending_player_intents: Vec::new(),
            sync_play_state: SyncPlayState::NotJoined,
            sync_play_ui: crate::ui::syncplay::SyncPlayScreenState::default(),
            sync_play_pending_join: None,
            sync_play_pending_leave: None,
            sync_play_drift: DriftController::new(),
            sync_play_clock: ClockEstimator::new(),
            player_view: PlayerView::new(),
            similar_request_id: 0,
            person_items_request_id: 0,
            person_artwork_queue: Vec::new(),
            pending_person_artwork: HashMap::new(),
            attempted_person_artwork: HashSet::new(),
            playback_request_id: 0,
            playback_start_request_id: 0,
            pending_playback_report_requests: HashMap::new(),
            playback_report_session_generation: 0,
            consecutive_progress_report_failures: 0,
            pending_artwork_requests: HashMap::new(),
            attempted_artwork: HashSet::new(),
            artwork_access_ticks: HashMap::new(),
            next_artwork_access_tick: 0,
            visible_artwork_from_tick: 0,
        }
    }

    /// Route gamepad input to the player (true) or to the library page (false). The host sets
    /// this each frame before drawing.
    pub fn set_player_owns_input(&mut self, owns: bool) {
        self.player_owns_input = owns;
        if !owns {
            self.player_nav_actions.clear();
        }
    }

    pub(crate) fn player_owns_input(&self) -> bool {
        self.player_owns_input
    }

    /// The gamepad actions pressed for the player since the last call, in order.
    pub fn take_player_nav_actions(&mut self) -> Vec<kattri_client_core::focus::NavAction> {
        std::mem::take(&mut self.player_nav_actions)
    }

    /// The output devices mpv listed. Names the player would refuse (odd characters, too long)
    /// and the reserved `auto` are dropped, so every entry can be written to `audio-device`.
    pub fn set_audio_devices(&mut self, devices: Vec<String>) {
        const MAX_DEVICES: usize = 64;
        let mut kept: Vec<String> = Vec::new();
        for device in devices {
            if device != "auto"
                && kattri_client_core::mpv_options::validate(
                    "audio-device",
                    kattri_client_core::mpv_options::OptionValue::Text(&device),
                )
                .is_ok()
                && !kept.contains(&device)
            {
                kept.push(device);
            }
            if kept.len() == MAX_DEVICES {
                break;
            }
        }
        self.audio_devices = kept;
    }

    /// Replace the baseline with the profile refined by the local runtime probe.
    pub fn set_device_profile(&mut self, profile: DeviceProfile) {
        #[cfg(debug_assertions)]
        {
            let text = profile
                .direct_play_profiles
                .as_deref()
                .unwrap_or_default()
                .iter()
                .map(|p| {
                    format!(
                        "[{:?}: container={:?} video={:?} audio={:?}]",
                        p.profile_type, p.container, p.video_codec, p.audio_codec
                    )
                })
                .collect::<Vec<_>>()
                .join(" ");
            crate::debug_log::log(&format!(
                "device profile set: {}",
                text.chars().take(900).collect::<String>()
            ));
        }
        self.device_profile = profile;
    }

    /// False when the selected item is audio, so music keeps the compact player instead of a
    /// full-window video view. Items whose record has not loaded count as video.
    pub fn selected_is_video(&self) -> bool {
        !self
            .selected_details
            .as_ref()
            .is_some_and(|details| details.item_type.as_deref() == Some("Audio"))
    }

    pub(crate) fn device_profile(&self) -> &DeviceProfile {
        &self.device_profile
    }

    /// Apply the existing settings hooks for bitrate limits and forced transcode.
    pub fn set_playback_overrides(&mut self, overrides: PlaybackRequestOverrides) {
        self.playback_overrides = overrides;
    }

    /// Map the versioned settings schema to Jellyfin's playback negotiation.
    pub fn apply_settings(&mut self, settings: &kattri_settings::Settings) {
        self.playback_overrides = playback_overrides_from_settings(settings);
    }

    /// The log detail the viewer chose in Settings, applied to `kattri.log` while the app runs.
    pub fn log_detail(&self) -> kattri_settings::LogDetail {
        self.settings.settings().diagnostics.log_level
    }

    /// The audio settings of the running profile, read each frame by the host to apply
    /// equalizer and filter changes while a file plays.
    pub fn audio_settings(&self) -> &kattri_settings::AudioSettings {
        &self.settings.settings().audio
    }

    /// Snapshot audio settings for the native host before it queues a new file.
    pub fn playback_audio_settings(&self) -> kattri_settings::AudioSettings {
        self.settings.settings().audio.clone()
    }

    /// Playback settings of the running profile: skip switches and the outro delay (M2-08).
    pub fn playback_settings(&self) -> &kattri_settings::PlaybackSettings {
        &self.settings.settings().playback
    }

    /// Subtitle settings of the running profile, including the secondary position (M2-09).
    pub fn subtitle_settings(&self) -> &kattri_settings::SubtitleSettings {
        &self.settings.settings().subtitles
    }

    /// Title of an item by id, for chrome such as the player's on-screen title. Falls back to the
    /// loaded music-queue names, which outlive a catalog reload.
    pub fn item_title(&self, item_id: &str) -> Option<&str> {
        self.items
            .iter()
            .find(|item| item.id == item_id)
            .map(|item| item.title.as_str())
            .or_else(|| {
                self.selected_details
                    .as_ref()
                    .filter(|item| item.id == item_id)
                    .map(|item| item.name.as_str())
            })
            .or_else(|| self.music_track_names.get(item_id).map(String::as_str))
    }

    /// Video settings of the running profile: the decode mode for the next file load (M2-22).
    pub fn video_settings(&self) -> &kattri_settings::VideoSettings {
        &self.settings.settings().video
    }

    /// Window settings of the running profile: the remembered placement and fullscreen state (M2-14).
    pub fn window_settings(&self) -> &kattri_settings::WindowSettings {
        &self.settings.settings().window
    }

    /// Remember a normal-state window placement (M2-14). Uses the same queued save as the settings
    /// page, so the UI thread only enqueues the write and does not touch the disk.
    pub fn save_window_geometry(&mut self, saved: &kattri_client_core::window_state::SavedWindow) {
        let before = self.settings.settings().clone();
        let window = &mut self.settings.settings_mut().window;
        window.geometry = Some(kattri_settings::WindowGeometry {
            x: saved.x,
            y: saved.y,
            width: saved.width,
            height: saved.height,
            monitor: (!saved.monitor_name.is_empty()).then(|| saved.monitor_name.clone()),
        });
        window.fullscreen = saved.fullscreen;
        self.settings.save_if_changed(&before);
    }

    /// Segments fetched for the current item, if they arrived. The host takes them once.
    pub fn take_media_segments(&mut self) -> Option<(String, Vec<MediaSegment>)> {
        self.loaded_media_segments.take()
    }

    /// Player actions for the native host to apply, in arrival order. The host applies them
    /// on the thread that owns the player, so this app side never touches libmpv.
    pub fn take_player_intents(&mut self) -> Vec<LocalAction> {
        std::mem::take(&mut self.pending_player_intents)
    }

    /// Hand one inbound remote session message to the worker, which parses it and queues the
    /// player action (M4-13). The host calls this when the session transport delivers one.
    pub fn handle_remote_session_message(
        &mut self,
        message: String,
    ) -> Result<(), WorkerSubmitError> {
        let max_volume_percent = self.playback_audio_settings().max_volume_percent;
        self.worker
            .as_ref()
            .ok_or(WorkerSubmitError::Stopped)?
            .remote_session_message(message, max_volume_percent)
    }

    /// Queue the player action for one command read from the session socket (M4-13). The
    /// volume maximum is read now, so a change in settings applies to the next command.
    /// Commands without a player action are refused and queue nothing.
    fn queue_socket_command(&mut self, command: RemoteCommand) {
        let max_volume_percent = self.playback_audio_settings().max_volume_percent;
        match local_action(command, max_volume_percent) {
            Ok(action) => self.pending_player_intents.push(action),
            Err(error) => tracing::warn!(?error, "remote session command not executed"),
        }
    }

    /// Feed one server clock reading from the worker into the SyncPlay estimator (M4-11). The
    /// worker's instants are placed on the player clock, the same clock the command times use.
    /// A reading that is not a valid exchange is dropped, and the estimator keeps its samples.
    fn feed_server_clock(&mut self, sent_at: Instant, received_at: Instant, server_ms: i64) {
        let sent_local_ms = self.player_view.monotonic_ms_at(sent_at);
        let received_local_ms = self.player_view.monotonic_ms_at(received_at);
        match clock_sample_from_exchange(sent_local_ms, server_ms, received_local_ms) {
            Ok(sample) => {
                self.sync_play_clock.add_sample(sample);
            }
            Err(error) => tracing::debug!(?error, "server clock sample dropped"),
        }
    }

    /// Apply one SyncPlay command from the joined group (M4-11). The server time is placed on
    /// the local clock with the group clock estimate. `apply_sync_play_command` then queues the
    /// player intents and sets the echo guard, so the player's reports for this command are not
    /// sent back. Without a clock estimate the command is refused, because its time cannot be
    /// placed.
    fn queue_sync_play_command(&mut self, message: SyncPlayMessage) {
        let Ok(command) = group_command_from_wire(&message) else {
            tracing::warn!("SyncPlay command refused: server time is not in UTC form");
            return;
        };
        let Some(clock) = self.sync_play_clock.estimate() else {
            tracing::warn!("SyncPlay command refused: no server clock estimate yet");
            return;
        };
        let now_ms = self.player_view.now_ms();
        let playing = self.player_view.player.is_some_and(|player| !player.paused);
        if let Err(error) = self.apply_sync_play_command(command, clock, now_ms, playing) {
            tracing::warn!(?error, "SyncPlay command refused");
        }
    }

    /// Ask the worker to join a SyncPlay group (M4-11). The group counts as joined only after
    /// the worker reports success for this request.
    pub fn join_sync_play_group(&mut self, group_id: String) -> Result<(), WorkerSubmitError> {
        let request_id = self.allocate_request_id();
        self.worker
            .as_ref()
            .ok_or(WorkerSubmitError::Stopped)?
            .sync_play_join(request_id, group_id.clone())?;
        self.sync_play_pending_join = Some((request_id, group_id));
        self.sync_play_ui.action_succeeded();
        Ok(())
    }

    pub(crate) fn request_sync_play_groups(&mut self) {
        let request_id = self.allocate_request_id();
        self.sync_play_ui.begin_list(request_id);
        let submitted = self
            .worker
            .as_ref()
            .is_some_and(|worker| worker.sync_play_list(request_id).is_ok());
        if !submitted {
            self.sync_play_ui.list_failed(request_id);
        }
    }

    pub(crate) fn create_sync_play_group_from_ui(&mut self) {
        let name = self.sync_play_ui.group_name.trim().to_owned();
        if name.is_empty() {
            self.sync_play_ui.action_failed();
            return;
        }
        let request_id = self.allocate_request_id();
        self.sync_play_ui.begin_create(request_id);
        let submitted = self
            .worker
            .as_ref()
            .is_some_and(|worker| worker.sync_play_create(request_id, name).is_ok());
        if !submitted {
            self.sync_play_ui.create_failed(request_id);
        }
    }

    pub(crate) fn joined_sync_play_group_id(&self) -> Option<&str> {
        match &self.sync_play_state {
            SyncPlayState::Joined { group_id }
            | SyncPlayState::Waiting { group_id, .. }
            | SyncPlayState::Playing { group_id, .. }
            | SyncPlayState::Paused { group_id, .. } => Some(group_id),
            SyncPlayState::NotJoined => None,
        }
    }

    pub(crate) fn sync_play_join_pending(&self) -> bool {
        self.sync_play_pending_join.is_some()
    }

    pub(crate) fn sync_play_leave_pending(&self) -> bool {
        self.sync_play_pending_leave.is_some()
    }

    /// Ask the worker to leave the SyncPlay group. The state clears when the worker confirms.
    pub fn leave_sync_play_group(&mut self) -> Result<(), WorkerSubmitError> {
        let request_id = self.allocate_request_id();
        self.worker
            .as_ref()
            .ok_or(WorkerSubmitError::Stopped)?
            .sync_play_leave(request_id)?;
        self.sync_play_pending_leave = Some(request_id);
        Ok(())
    }

    /// Apply one membership or state update from Jellyfin's session socket. These events are
    /// separate from scheduled playback commands: they keep the Watch Together screen in step
    /// with group creation, joins, leaves, and participant changes.
    fn apply_sync_play_group_update(&mut self, update: SyncPlayGroupUpdate) {
        match update {
            SyncPlayGroupUpdate::Joined(group) => {
                let group_id = group.group_id.clone();
                self.sync_play_ui.upsert_group(group);
                if self.joined_sync_play_group_id() != Some(group_id.as_str()) {
                    self.sync_play_state = SyncPlayState::Joined { group_id };
                }
                self.sync_play_pending_join = None;
                self.sync_play_pending_leave = None;
                self.sync_play_ui.action_succeeded();
            }
            SyncPlayGroupUpdate::Left { group_id }
            | SyncPlayGroupUpdate::NotInGroup { group_id } => {
                let matches_current = self.joined_sync_play_group_id().is_some_and(|current| {
                    group_id
                        .as_deref()
                        .is_none_or(|group_id| current == group_id)
                });
                if matches_current {
                    self.sync_play_state = SyncPlayState::NotJoined;
                    self.sync_play_pending_join = None;
                    self.sync_play_pending_leave = None;
                    self.request_sync_play_groups();
                }
            }
            SyncPlayGroupUpdate::Updated(group) => {
                self.sync_play_ui.upsert_group(group);
            }
            SyncPlayGroupUpdate::UserJoined {
                group_id,
                user_name,
            } => self
                .sync_play_ui
                .update_participant(group_id.as_deref(), &user_name, true),
            SyncPlayGroupUpdate::UserLeft {
                group_id,
                user_name,
            } => self
                .sync_play_ui
                .update_participant(group_id.as_deref(), &user_name, false),
            SyncPlayGroupUpdate::StateUpdate {
                group_id, state, ..
            } => {
                self.sync_play_ui
                    .update_group_state(group_id.as_deref(), state);
            }
            SyncPlayGroupUpdate::Other => {}
        }
    }

    /// Apply one server SyncPlay command to the local player (M4-11). Pause and unpause seek
    /// to the group position first, so the local player lands where the group is. `clock` is
    /// the server clock estimate, and `playing` is the local play state.
    pub fn apply_sync_play_command(
        &mut self,
        command: GroupCommand,
        clock: ClockEstimate,
        local_now_ms: i64,
        playing: bool,
    ) -> Result<(), SyncPlayError> {
        let group_id = match &self.sync_play_state {
            SyncPlayState::Joined { group_id }
            | SyncPlayState::Waiting { group_id, .. }
            | SyncPlayState::Playing { group_id, .. }
            | SyncPlayState::Paused { group_id, .. } => group_id.clone(),
            SyncPlayState::NotJoined => {
                return Err(SyncPlayError::InvalidTransition {
                    from: "NotJoined",
                    event: "GroupCommand",
                });
            }
        };
        let mapped = map_command(command, clock, local_now_ms, playing)?;
        let position_ms = mapped.position_ms;
        let seek_to = u64::try_from(position_ms).unwrap_or(0);
        let server_time_ms = mapped.apply_at_local_ms.saturating_add(clock.offset_ms);
        let anchor = PlayAnchor {
            position_ms,
            server_time_ms,
        };
        match mapped.action {
            GroupAction::Pause => {
                self.pending_player_intents.extend([
                    LocalAction::Seek {
                        position_ms: seek_to,
                    },
                    LocalAction::Pause,
                ]);
                self.sync_play_state = SyncPlayState::Paused {
                    group_id,
                    position_ms,
                };
            }
            GroupAction::Unpause => {
                self.pending_player_intents.extend([
                    LocalAction::Seek {
                        position_ms: seek_to,
                    },
                    LocalAction::Play,
                ]);
                self.sync_play_state = SyncPlayState::Playing { group_id, anchor };
            }
            GroupAction::Seek => {
                self.pending_player_intents.push(LocalAction::Seek {
                    position_ms: seek_to,
                });
                self.sync_play_state = if playing {
                    SyncPlayState::Playing { group_id, anchor }
                } else {
                    SyncPlayState::Paused {
                        group_id,
                        position_ms,
                    }
                };
            }
        }
        // The player will report the state this command leaves it in. Those reports are the
        // echo, and they are not sent back to the group (M4-11). `local_now_ms` is on the
        // PlayerView clock, so the echo window uses the same timeline as the reports.
        let echo_paused = match mapped.action {
            GroupAction::Pause => true,
            GroupAction::Unpause => false,
            GroupAction::Seek => !playing,
        };
        self.player_view.echo = Some(InboundEcho {
            paused: echo_paused,
            position_ms,
            applied_at_ms: local_now_ms,
        });
        Ok(())
    }

    /// Feed one player report to the SyncPlay path. Returns the group command when the user
    /// changed the player. A report that matches an inbound command's echo returns `None`.
    pub(crate) fn observe_local_player(
        &mut self,
        paused: bool,
        position_ms: i64,
        now_ms: i64,
    ) -> Option<SyncPlayCommand> {
        let now = PlayerSnapshot {
            paused,
            position_ms,
            at_ms: now_ms,
        };
        let prev = self.player_view.player.replace(now);
        let change = prev.and_then(|prev| classify_player_change(prev, now));
        if let Some(echo) = self.player_view.echo {
            if now_ms.saturating_sub(echo.applied_at_ms) > SYNC_PLAY_ECHO_WINDOW_MS {
                self.player_view.echo = None;
            } else {
                let landed = paused == echo.paused
                    && position_ms
                        .saturating_sub(echo_position_ms(echo, now_ms))
                        .abs()
                        <= SYNC_PLAY_ECHO_TOLERANCE_MS;
                if landed {
                    self.player_view.echo = None;
                }
                // Inside the window every report belongs to the group command still taking
                // effect, so none of them is sent back.
                return None;
            }
        }
        let change = change?;
        if matches!(self.sync_play_state, SyncPlayState::NotJoined) {
            return None;
        }
        Some(group_command_for(change))
    }

    /// Keep the player view in step with the player's reports, and send a user change to the
    /// joined group (M4-11). Called for every report, before the report is queued.
    fn track_player_report(&mut self, action: &PlaybackReportAction) {
        let info = match action {
            PlaybackReportAction::Stopped(_) => {
                self.player_view.player = None;
                return;
            }
            PlaybackReportAction::Started(info) => {
                self.player_view.player = None;
                info
            }
            PlaybackReportAction::Progress(info) => info,
        };
        let Some(ticks) = info.position_ticks.as_ref().map(|ticks| ticks.as_ticks()) else {
            return;
        };
        let paused = info.is_paused.unwrap_or(false);
        let now_ms = self.player_view.now_ms();
        if let Some(command) =
            self.observe_local_player(paused, ticks.saturating_div(TICKS_PER_MS), now_ms)
        {
            self.send_sync_play_command(command);
        }
    }

    /// Queue a local change for the joined group. A refusal is shown in the playback feedback
    /// line, where a refused progress report is shown.
    fn send_sync_play_command(&mut self, command: SyncPlayCommand) {
        let request_id = self.allocate_request_id();
        let Some(worker) = self.worker.as_ref() else {
            self.playback_report_feedback = Some(ui_text(ids::CONNECT_WORKER_UNAVAILABLE).into());
            return;
        };
        if let Err(error) = worker.sync_play_command(request_id, command) {
            self.playback_report_feedback = Some(error.to_string());
        }
    }

    /// Compare the local position with the group target and queue at most one corrective seek
    /// (M4-11). Only checks while the group plays. The drift controller limits how often.
    pub fn check_sync_play_drift(
        &mut self,
        measured_ms: i64,
        clock: ClockEstimate,
        local_now_ms: i64,
    ) {
        if !matches!(self.sync_play_state, SyncPlayState::Playing { .. }) {
            return;
        }
        let now_server_ms = local_now_ms.saturating_add(clock.offset_ms);
        let Some(expected_ms) = target_position_ms(&self.sync_play_state, now_server_ms) else {
            return;
        };
        if let Some(target_ms) = self
            .sync_play_drift
            .check(expected_ms, measured_ms, local_now_ms)
        {
            self.pending_player_intents.push(LocalAction::Seek {
                position_ms: u64::try_from(target_ms).unwrap_or(0),
            });
        }
    }

    /// Rebuild the per-series language memory from the running profile (M2-10).
    pub fn series_language_memory(&self) -> SeriesLanguageMemory {
        SeriesLanguageMemory::from_settings_records(
            self.settings.settings().series_languages.iter().cloned(),
        )
    }

    /// Remember the audio track the viewer picked. Series-less items write nothing.
    pub fn record_audio_choice(&mut self, series: Option<&str>, track: &TrackLanguage) {
        let mut memory = self.series_language_memory();
        if memory.record_audio(series, track) == RecordOutcome::Stored {
            self.store_series_language_memory(&memory);
        }
    }

    /// Remember the subtitle track the viewer picked. Series-less items write nothing.
    pub fn record_subtitle_choice(&mut self, series: Option<&str>, track: &TrackLanguage) {
        let mut memory = self.series_language_memory();
        if memory.record_subtitle(series, track) == RecordOutcome::Stored {
            self.store_series_language_memory(&memory);
        }
    }

    /// Remember that the viewer turned subtitles off. Series-less items write nothing.
    pub fn record_subtitles_off_choice(&mut self, series: Option<&str>) {
        let mut memory = self.series_language_memory();
        if memory.record_subtitles_off(series) == RecordOutcome::Stored {
            self.store_series_language_memory(&memory);
        }
    }

    fn store_series_language_memory(&mut self, memory: &SeriesLanguageMemory) {
        let before = self.settings.settings().clone();
        self.settings.settings_mut().series_languages = memory.to_settings_records();
        self.settings.save_if_changed(&before);
    }

    /// Ask the server for intro and outro segments, but only when a skip switch is on (M2-08).
    /// The previous item's segments are dropped here.
    fn request_media_segments(&mut self, item_id: &str) {
        self.media_segments_request = None;
        self.loaded_media_segments = None;
        if !segments_needed(&self.settings.settings().playback) {
            return;
        }
        let request_id = self.allocate_request_id();
        let result = self
            .worker
            .as_ref()
            .ok_or(WorkerSubmitError::Stopped)
            .and_then(|worker| worker.list_media_segments(request_id, item_id.to_owned()));
        if result.is_ok() {
            self.media_segments_request = Some((request_id, item_id.to_owned()));
        }
    }

    /// Save a settings change made on the settings page and apply it to playback.
    /// `before` is the copy taken before the page drew its controls.
    pub(crate) fn commit_settings(&mut self, before: &kattri_settings::Settings) {
        self.settings.save_if_changed(before);
        let current = self.settings.settings().clone();
        self.apply_settings(&current);
    }

    /// Open the launch picker when several profiles exist and `--profile` was not given.
    pub fn offer_launch_picker(&mut self) {
        if self.settings.offers_launch_picker() {
            self.profile_picker = Some(PickerState::default());
        }
    }

    /// Open the profile picker from the Settings page.
    pub(crate) fn open_profile_picker(&mut self) {
        self.profile_picker = Some(PickerState::default());
    }

    /// Draw the launch notice and the profile picker over the current screen, then act on a
    /// switch the user picked.
    fn show_profile_overlays(&mut self, ctx: &egui::Context) {
        self.show_launch_notice(ctx);
        let running = self.settings.running_name().unwrap_or_default().to_owned();
        let outcome = match self.settings.profile_store_mut() {
            Some(store) => {
                profile_picker::show_window(ctx, &mut self.profile_picker, store, &running)
            }
            None => {
                self.profile_picker = None;
                None
            }
        };
        match outcome {
            Some(PickerOutcome::SwitchTo(name)) => match self.switch_profile(&name) {
                Ok(()) => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
                Err(message) => {
                    if let Some(picker) = self.profile_picker.as_mut() {
                        picker.error = Some(message);
                    }
                }
            },
            Some(PickerOutcome::Created(_)) | None => {}
        }
    }

    fn show_launch_notice(&mut self, ctx: &egui::Context) {
        let mut dismissed = false;
        if let Some(notice) = self.launch_notice.as_deref() {
            egui::Window::new(ui_text(ids::PROFILE_NOTICE_TITLE))
                .anchor(egui::Align2::CENTER_BOTTOM, egui::vec2(0.0, -24.0))
                .collapsible(false)
                .resizable(false)
                .show(ctx, |ui| {
                    ui.label(notice);
                    dismissed = ui.button(ui_text(ids::APP_BUTTON_OK)).clicked();
                });
        }
        if dismissed {
            self.launch_notice = None;
        }
    }

    /// Save pending settings, make `name` the active profile, and start the app again with
    /// `--profile <name>`. The caller closes this process once the new one has started.
    fn switch_profile(&mut self, name: &str) -> Result<(), String> {
        self.settings.finish_pending_saves().map_err(|error| {
            ui_text_args(
                ids::APP_ERROR_SETTINGS_NOT_SAVED,
                &[("error", &error.to_string())],
            )
        })?;
        let store = self
            .settings
            .profile_store_mut()
            .ok_or_else(|| ui_text(ids::PROFILE_ERROR_UNAVAILABLE).to_owned())?;
        profile_picker::switch_and_restart(store, name, profile_picker::relaunch_current_exe)
            .map_err(|error| error.to_string())
    }

    fn allocate_request_id(&mut self) -> u64 {
        self.next_request_id = self.next_request_id.wrapping_add(1);
        self.next_request_id
    }

    /// What to tell the worker about saving this sign-in. The scope is the running local
    /// profile, so one profile never inherits another's saved sign-in.
    fn saved_sign_in_request(&self, remember: bool) -> SavedSignInRequest {
        SavedSignInRequest {
            scope: self.settings.running_name().unwrap_or("default").to_owned(),
            remember,
        }
    }

    /// At start-up: signs back in with the token saved by an earlier "stay signed in" sign-in,
    /// on the default saved server. Call it once, after `offer_launch_picker`, from the real
    /// entry points only (not from tests, which must never reach a real server or the OS
    /// credential store). Does nothing while the profile picker is open, when the form is
    /// already filled in (for example by the debug sign-in), or when no server is saved. If no
    /// usable token exists the worker answers `ResumeUnavailable` and the sign-in form stays
    /// as it is.
    pub fn resume_saved_sign_in(&mut self) {
        if let Some(worker) = self.worker.as_ref() {
            // Asked first, so the answer reaches the form before it is drawn. A failure leaves
            // the option on offer; saving then fails quietly.
            let _ = worker.probe_saved_sign_in();
        }
        if self.worker.is_none()
            || self.profile_picker.is_some()
            || !self.server_url.trim().is_empty()
            || !matches!(self.connection, ConnectionStatus::Disconnected { .. })
        {
            return;
        }
        let Some((server_url, allow_insecure)) =
            self.saved_servers.resume_candidate().map(|entry| {
                (
                    entry.base_url().to_owned(),
                    entry.allow_insecure_remote_http(),
                )
            })
        else {
            return;
        };
        self.server_url = server_url.clone();
        self.allow_insecure_remote_http = allow_insecure;
        self.remember_sign_in = true;
        let saved = self.saved_sign_in_request(true);
        let request_id = self.begin_connection(ui_text(ids::CONNECT_CHECKING_SERVER));
        self.connection = ConnectionStatus::Connecting {
            message: ui_text(ids::CONNECT_CHECKING_SERVER).into(),
        };
        let submitted = self
            .worker
            .as_ref()
            .map_or(Err(WorkerSubmitError::Stopped), |worker| {
                worker.resume_sign_in(request_id, server_url, allow_insecure, saved)
            });
        if submitted.is_err() {
            self.catalog_loading = false;
            self.connection = ConnectionStatus::Disconnected { error: None };
        }
    }

    pub(crate) fn start_connection(&mut self) {
        if self.quick_connect.is_some() {
            self.cancel_quick_connect();
        }
        if self.server_url.trim().is_empty() {
            self.connection = ConnectionStatus::Disconnected {
                error: Some(ui_text(ids::CONNECT_ERROR_SERVER_REQUIRED).into()),
            };
            return;
        }
        if self.server_preview().is_none() {
            self.check_current_server();
            return;
        }
        if self.username.trim().is_empty() {
            self.connection = ConnectionStatus::Disconnected {
                error: Some(ui_text(ids::CONNECT_ERROR_USERNAME_REQUIRED).into()),
            };
            return;
        }
        if self.worker.is_none() {
            self.connection = ConnectionStatus::Disconnected {
                error: Some(ui_text(ids::CONNECT_WORKER_UNAVAILABLE).into()),
            };
            return;
        }

        let password = std::mem::take(&mut *self.password_input);
        let saved = self.saved_sign_in_request(self.remember_sign_in);
        let request_id = self.begin_connection(ui_text(ids::CONNECT_CHECKING_SERVER));

        let connect_result =
            self.worker
                .as_ref()
                .map_or(Err(WorkerSubmitError::Stopped), |worker| {
                    worker.connect(
                        request_id,
                        self.server_url.trim().to_owned(),
                        self.username.clone(),
                        Zeroizing::new(password),
                        self.allow_insecure_remote_http,
                        saved,
                    )
                });
        if connect_result.is_err() {
            self.catalog_loading = false;
            self.connection = ConnectionStatus::Disconnected {
                error: Some(ui_text(ids::APP_ERROR_QUEUE_NETWORK).into()),
            };
        }
    }

    pub(crate) fn select_saved_server(
        &mut self,
        server_url: &str,
        allow_insecure_remote_http: bool,
    ) {
        if self.quick_connect.is_some() {
            self.cancel_quick_connect();
        }
        if !self.disconnect() {
            return;
        }
        self.server_url = server_url.to_owned();
        self.allow_insecure_remote_http = allow_insecure_remote_http;
        self.username.clear();
        self.password_input.zeroize();
        self.password_input.clear();
        self.server_address_changed(Duration::ZERO);
    }

    pub(crate) fn server_address_changed(&mut self, now: Duration) {
        self.server_preview = None;
        self.server_check_pending = None;
        self.server_check_error = None;
        self.schedule_quick_connect_probe(now);
    }

    fn server_preview(&self) -> Option<&ServerIdentity> {
        let server_url = self.server_url.trim();
        self.server_preview
            .as_ref()
            .and_then(|(checked_url, checked_opt_in, identity)| {
                server_preview_matches(
                    server_url,
                    self.allow_insecure_remote_http,
                    Some((checked_url, *checked_opt_in)),
                )
                .then_some(identity)
            })
    }

    pub(crate) fn visible_server_preview(&self) -> Option<&ServerIdentity> {
        self.server_preview()
    }

    pub(crate) fn server_check_pending(&self) -> bool {
        self.server_check_pending.is_some()
    }

    pub(crate) fn check_current_server(&mut self) {
        let server_url = self.server_url.trim().to_owned();
        if server_url.is_empty() {
            self.server_check_error = Some(ui_text(ids::CONNECT_ERROR_SERVER_REQUIRED).into());
            return;
        }
        if self.worker.is_none() {
            self.server_check_error = Some(ui_text(ids::CONNECT_WORKER_UNAVAILABLE).into());
            return;
        }
        let request_id = self.allocate_request_id();
        let allow_insecure_remote_http = self.allow_insecure_remote_http;
        self.server_check_pending =
            Some((request_id, server_url.clone(), allow_insecure_remote_http));
        self.server_check_error = None;
        let result = self
            .worker
            .as_ref()
            .map_or(Err(WorkerSubmitError::Stopped), |worker| {
                worker.check_server(request_id, server_url, allow_insecure_remote_http)
            });
        if result.is_err() {
            self.server_check_pending = None;
            self.server_check_error = Some(ui_text(ids::APP_ERROR_QUEUE_NETWORK).into());
        }
    }

    pub(crate) fn quick_connect_available(&self) -> bool {
        self.server_preview().is_some()
            && quick_connect_is_enabled_for(
                self.server_url.trim(),
                self.allow_insecure_remote_http,
                self.quick_connect_enabled_for
                    .as_ref()
                    .map(|(url, allow_http, enabled)| (url.as_str(), *allow_http, *enabled)),
            )
    }

    pub(crate) fn quick_connect_check_pending(&self) -> bool {
        self.quick_connect_probe_after.is_some() || self.quick_connect_probe_pending.is_some()
    }

    pub(crate) fn schedule_quick_connect_probe(&mut self, now: Duration) {
        self.quick_connect_enabled_for = None;
        self.quick_connect_probe_pending = None;
        self.quick_connect_probe_after = (!self.server_url.trim().is_empty())
            .then_some(now.saturating_add(QUICK_CONNECT_PROBE_DELAY));
    }

    fn tick_quick_connect_probe(&mut self, now: Duration) {
        let server_url = self.server_url.trim().to_owned();
        if server_url.is_empty() {
            self.quick_connect_probe_after = None;
            self.quick_connect_probe_pending = None;
            self.quick_connect_enabled_for = None;
            return;
        }
        if self.quick_connect_enabled_for.as_ref().is_some_and(
            |(checked_url, checked_http_opt_in, _)| {
                checked_url == &server_url
                    && *checked_http_opt_in == self.allow_insecure_remote_http
            },
        ) {
            self.quick_connect_probe_after = None;
            return;
        }
        if self.quick_connect_probe_pending.as_ref().is_some_and(
            |(_, pending_url, pending_http_opt_in)| {
                pending_url == &server_url
                    && *pending_http_opt_in == self.allow_insecure_remote_http
            },
        ) {
            return;
        }
        let Some(deadline) = self.quick_connect_probe_after else {
            return;
        };
        if now < deadline {
            return;
        }

        let request_id = self.allocate_request_id();
        let allow_insecure_remote_http = self.allow_insecure_remote_http;
        self.quick_connect_probe_pending =
            Some((request_id, server_url.clone(), allow_insecure_remote_http));
        self.quick_connect_probe_after = None;
        let result = self
            .worker
            .as_ref()
            .map_or(Err(WorkerSubmitError::Stopped), |worker| {
                worker.check_quick_connect(
                    request_id,
                    server_url.clone(),
                    allow_insecure_remote_http,
                )
            });
        if result.is_err() {
            self.quick_connect_probe_pending = None;
            self.quick_connect_enabled_for = Some((server_url, allow_insecure_remote_http, false));
        }
    }

    pub(crate) fn start_quick_connect(&mut self) {
        if self.server_url.trim().is_empty() {
            self.connection = ConnectionStatus::Disconnected {
                error: Some(ui_text(ids::CONNECT_ERROR_SERVER_REQUIRED).into()),
            };
            return;
        }
        if self.server_preview().is_none() {
            self.connection = ConnectionStatus::Disconnected {
                error: Some(ui_text(ids::CONNECT_ERROR_CHECK_ADDRESS).into()),
            };
            return;
        }
        if !self.quick_connect_available() {
            self.connection = ConnectionStatus::Disconnected {
                error: Some(ui_text(ids::CONNECT_ERROR_QUICK_CONNECT_DISABLED).into()),
            };
            return;
        }
        if self.worker.is_none() {
            self.connection = ConnectionStatus::Disconnected {
                error: Some(ui_text(ids::CONNECT_WORKER_UNAVAILABLE).into()),
            };
            return;
        }

        let request_id = self.begin_connection(ui_text(ids::CONNECT_STATUS_STARTING_QUICK_CONNECT));
        self.quick_connect = Some(QuickConnectFlow::new(request_id));
        let result = self
            .worker
            .as_ref()
            .map_or(Err(WorkerSubmitError::Stopped), |worker| {
                worker.quick_connect_start(
                    request_id,
                    self.server_url.trim().to_owned(),
                    self.allow_insecure_remote_http,
                )
            });
        if result.is_err() {
            if let Some(flow) = self.quick_connect.as_mut() {
                let _ = flow.initiate_failed();
            }
            self.connection = ConnectionStatus::Disconnected {
                error: Some(ui_text(ids::APP_ERROR_QUEUE_NETWORK).into()),
            };
        }
    }

    pub(crate) fn cancel_quick_connect(&mut self) {
        let Some(flow) = self.quick_connect.as_mut() else {
            return;
        };
        let request_id = flow.request_id();
        let _ = flow.cancel();
        let mut error = None;
        if let Some(worker) = self.worker.as_ref()
            && let Err(rejection) = worker.quick_connect_cancel(request_id)
        {
            // Cancel never disappears under pressure: use the reserved local
            // disconnect barrier, keeping preceding ordered work intact.
            if worker.disconnect().is_err() {
                error = Some(rejection.to_string());
            }
        }
        self.quick_connect = None;
        self.connection = ConnectionStatus::Disconnected { error };
    }

    fn handle_quick_connect_action(&mut self, request_id: u64, action: crate::sign_in::Action) {
        use crate::sign_in::Action;
        let saved = self.saved_sign_in_request(self.remember_sign_in);
        let Some(worker) = self.worker.as_ref() else {
            return;
        };
        let result = match action {
            Action::None => return,
            Action::Poll => worker.quick_connect_poll(request_id),
            Action::Authenticate => worker.quick_connect_authenticate(request_id, saved),
        };
        if result.is_err() {
            self.connection = ConnectionStatus::Disconnected {
                error: Some(ui_text(ids::APP_ERROR_QUEUE_NETWORK).into()),
            };
            self.quick_connect = None;
        }
    }

    /// Clears the library, playback and sign-in state for a new attempt and returns its
    /// request id. Results tagged with an older id are ignored from now on.
    fn begin_connection(&mut self, message: &str) -> u64 {
        if let Some(worker) = self.worker.as_ref() {
            worker.cancel_reads();
        }
        let request_id = self.allocate_request_id();
        self.connection_request_id = request_id;
        self.catalog_request_id = request_id;
        self.details_request_id = self.allocate_request_id();
        self.playback_request_id = self.allocate_request_id();
        self.playback_start_request_id = self.allocate_request_id();
        self.server = None;
        self.user_name = None;
        self.user_is_admin = false;
        self.items.clear();
        self.selected_item_id = None;
        self.selected_details = None;
        self.selected_artwork_texture = None;
        self.detail_artwork_request_id = self.allocate_request_id();
        self.detail_backdrop_attempted = false;
        self.total_record_count = None;
        self.unsupported_catalog_filters.clear();
        self.start_index = 0;
        self.catalog_next_index = 0;
        self.catalog_end_reached = false;
        self.catalog_loaded = false;
        self.loaded_search = None;
        self.catalog_loading = true;
        self.catalog_error = None;
        self.details_loading = false;
        self.details_error = None;
        self.playback_info = None;
        self.playback_loading = false;
        self.playback_error = None;
        self.playback_start_loading = false;
        self.playback_start_error = None;
        self.playback_start_status = None;
        self.playback_report_feedback = None;
        self.pending_playback_report_requests.clear();
        self.consecutive_progress_report_failures = 0;
        self.pending_artwork_requests.clear();
        self.attempted_artwork.clear();
        self.artwork_access_ticks.clear();
        self.pending_playback = None;
        self.submitted_search = None;
        self.search_active = false;
        self.search_results.clear();
        self.search_browse_group = None;
        self.invalidate_live_tv_search();
        self.search_stale = false;
        self.search_debouncer.input("", Duration::ZERO);
        self.search_query.clear();
        self.view = AppView::Home;
        self.browse_context = None;
        self.home = HomeState::default();
        self.live_tv_channels.clear();
        self.live_tv_channel_logos.clear();
        self.live_tv_logo_queue.clear();
        self.live_tv_logo_pending.clear();
        self.live_tv_logo_attempted.clear();
        self.photo_items.clear();
        self.photo_thumbnails.clear();
        self.photo_full_size_images.clear();
        self.photo_selected_id = None;
        self.cancel_pending_photo_decodes();
        self.photo_thumbnail_attempted.clear();
        self.photo_full_size_failed.clear();
        self.live_tv_total_count = None;
        self.live_tv_loading = false;
        self.live_tv_loaded = false;
        self.live_tv_not_set_up = false;
        self.live_tv_error = None;
        self.live_tv_notice = None;
        self.live_tv_selected_channel_id = None;
        self.live_tv_request_id = self.allocate_request_id();
        self.live_tv_requested_start_index = 0;
        self.live_tv_programs.clear();
        self.live_tv_guide_window = None;
        self.live_tv_guide_loading = false;
        self.live_tv_guide_available = false;
        self.live_tv_guide_error = None;
        self.live_tv_guide_request_id = self.allocate_request_id();
        self.live_tv_guide_requested_channel_ids.clear();
        self.live_tv_guide_pending_channel_ids.clear();
        self.live_tv_guide_refreshed_at_unix = None;
        self.music_queue = Queue::new();
        self.music_track_names.clear();
        self.music_queue_auto_play_item_id = None;
        self.music_queue_notice = None;
        self.pending_stored_music_queue = match self.settings.load_queue() {
            Some(Ok(StoredLoadOutcome::Loaded(stored))) if !stored.queue.is_empty() => Some(stored),
            _ => None,
        };
        self.music_queue_restore_request_id = self.allocate_request_id();
        self.music_queue_restore_loading = false;
        self.music_queue_resume_position_ms = None;
        self.music_queue_resume_position_ticks = None;
        self.music_queue_last_saved_json = None;
        self.music_queue_persistence_suspended = self.pending_stored_music_queue.is_some();
        self.playlist_entries.clear();
        self.playlist_items_loading = false;
        self.playlist_items_error = None;
        self.opened_playlist_id = None;
        self.playlist_items_request_id = self.allocate_request_id();
        self.playlist_mutation_loading = false;
        self.playlist_mutation_error = None;
        self.playlist_mutation_notice = None;
        self.playlist_mutation_request_id = self.allocate_request_id();
        self.playlist_candidates.clear();
        self.playlist_candidates_loading = false;
        self.playlist_candidates_error = None;
        self.playlist_candidates_request_id = self.allocate_request_id();
        self.music_album_tracks_loading = false;
        self.music_album_tracks_error = None;
        self.music_album_tracks_request_id = self.allocate_request_id();
        self.music_queue_auto_play_item_id = None;
        self.music_queue_notice = None;
        self.focus_graph = FocusGraph::default();
        self.connection = ConnectionStatus::Connecting {
            message: message.into(),
        };
        request_id
    }

    pub(crate) fn retry_items(&mut self) {
        self.request_catalog(self.catalog_next_index, self.submitted_search.clone());
    }

    pub(crate) fn retry_current_network_request(&mut self) {
        self.network_notice = None;
        self.network_retry_schedule.reset();
        self.network_retry_deadline = None;
        self.network_retry_context = None;
        self.dispatch_network_retry();
    }

    fn dispatch_network_retry(&mut self) {
        if self.playback_error.is_some() {
            self.request_playback_options();
        } else if self.details_error.is_some() || self.selected_item_id.is_some() {
            self.retry_item_details();
        } else if self.view == AppView::Home {
            self.refresh_home();
        } else {
            self.retry_items();
        }
    }

    fn tick_network_retry(&mut self, now: Duration, ctx: &egui::Context) {
        let Some(deadline) = self.network_retry_deadline else {
            return;
        };
        let current_context = (self.view, self.page, self.selected_item_id.clone());
        if self.network_retry_context.as_ref() != Some(&current_context) {
            self.network_retry_deadline = None;
            self.network_retry_context = None;
            self.network_retry_schedule.reset();
            return;
        }
        if deadline <= now {
            self.network_retry_deadline = None;
            self.network_retry_context = None;
            self.network_notice = None;
            self.dispatch_network_retry();
        } else {
            ctx.request_repaint_after(deadline.saturating_sub(now));
        }
    }

    pub(crate) fn cancel_network_retry_for_user_action(&mut self) {
        if self.network_retry_deadline.is_some() {
            self.network_retry_deadline = None;
            self.network_retry_context = None;
            self.network_retry_schedule.reset();
        }
    }

    fn record_network_error(&mut self, class: ErrorClass, retry_after: Option<Duration>) {
        self.offline_tracker.record_error_class(class);
        self.network_notice = if self.offline_tracker.is_offline() {
            Some(ErrorClass::Offline)
        } else if matches!(class, ErrorClass::Offline | ErrorClass::Timeout) {
            None
        } else {
            Some(class)
        };
        let jitter_unit = (self.ui_time.subsec_nanos() as f64 / 1_000_000_000.0).mul_add(2.0, -1.0);
        self.network_retry_deadline = match self.network_retry_schedule.next_decision(
            class.is_retryable(),
            retry_after,
            jitter_unit,
        ) {
            RetryDecision::Retry { delay } => Some(self.ui_time.saturating_add(delay)),
            RetryDecision::Stop(_) => None,
        };
        self.network_retry_context = self
            .network_retry_deadline
            .map(|_| (self.view, self.page, self.selected_item_id.clone()));
    }

    fn record_network_success(&mut self) {
        self.offline_tracker.record_success();
        self.network_notice = None;
        self.network_retry_schedule.reset();
        self.network_retry_deadline = None;
        self.network_retry_context = None;
    }

    fn cancel_navigation_requests(&mut self) {
        if let Some(worker) = self.worker.as_ref() {
            worker.cancel_navigation_reads();
        }
        self.catalog_request_id = self.allocate_request_id();
        self.details_request_id = self.allocate_request_id();
        self.series_seasons_request_id = self.allocate_request_id();
        self.series_episodes_request_id = self.allocate_request_id();
        self.similar_request_id = self.allocate_request_id();
        self.person_items_request_id = self.allocate_request_id();
        self.catalog_loading = false;
        self.details_loading = false;
        self.series_seasons_loading = false;
        self.series_episodes_loading = false;
        self.similar_loading = false;
        self.person_items_loading = false;
        self.cancel_network_retry_for_user_action();
    }

    pub(crate) fn navigate_to(&mut self, page: Page) {
        self.view = AppView::Library;
        let left_browse_context = self.browse_context.take().is_some();
        if self.page == page && !left_browse_context {
            return;
        }
        self.cancel_navigation_requests();
        self.page = page;
        self.catalog_name_prefix = None;
        self.catalog_genre = None;
        self.detail_parent = None;
        self.photo_selected_id = None;
        self.photo_page_start = 0;
        self.photo_items.clear();
        self.photo_thumbnails.clear();
        self.photo_full_size_images.clear();
        self.cancel_pending_photo_decodes();
        self.photo_thumbnail_attempted.clear();
        self.photo_full_size_failed.clear();
        self.search_query.clear();
        self.submitted_search = None;
        self.search_active = false;
        self.search_results.clear();
        self.search_browse_group = None;
        self.search_stale = false;
        self.search_debouncer.input("", Duration::ZERO);
        self.catalog_request_id = self.allocate_request_id();
        self.catalog_loading = false;
        if page.is_catalog() && self.server.is_some() {
            self.request_catalog(0, None);
        }
        if page == Page::Dashboard {
            self.request_dashboard();
        }
        if page == Page::SyncPlay {
            self.request_sync_play_groups();
        }
    }

    /// Whether the admin dashboard nav entry and page are on. Read from the settings file
    /// (`admin.dashboard`), which is off by default.
    pub(crate) fn dashboard_enabled(&self) -> bool {
        self.settings.settings().admin.dashboard
    }

    /// Start one read-only dashboard load. The gate and a signed-in server are checked here,
    /// so a disabled dashboard sends no request.
    fn request_dashboard(&mut self) {
        if !self.dashboard_enabled() || self.server.is_none() {
            return;
        }
        let request_id = self.allocate_request_id();
        self.admin_load = AdminLoad::Pending;
        self.admin_request_id = Some(request_id);
        let submitted = self
            .worker
            .as_ref()
            .is_some_and(|worker| worker.load_dashboard(request_id).is_ok());
        if !submitted {
            self.admin_request_id = None;
            self.admin_load = AdminLoad::Failed;
        }
    }

    /// Write an error message to the local crash folder when crash reporting is on (M4-21).
    /// Nothing is uploaded. A failed write is ignored, since the error is already on screen.
    fn record_error_for_crash_report(&self, error_text: &str) {
        let settings = kattri_client_core::crash_report::CrashReportingSettings::from_diagnostics(
            &self.settings.settings().diagnostics,
        );
        if !settings.enabled {
            return;
        }
        let Some(crash_dir) = kattri_logging::crash_directory() else {
            return;
        };
        let _ = kattri_client_core::crash_report::record_error_report(
            &crash_dir,
            settings,
            env!("CARGO_PKG_VERSION"),
            error_text,
        );
    }

    pub(crate) fn navigate_to_parent(
        &mut self,
        page: Page,
        parent_id: String,
        title: String,
        is_collection: bool,
    ) {
        self.cancel_navigation_requests();
        self.view = AppView::Library;
        self.page = page;
        self.catalog_name_prefix = None;
        self.catalog_genre = None;
        self.photo_selected_id = None;
        self.photo_items.clear();
        self.photo_thumbnails.clear();
        self.photo_full_size_images.clear();
        self.cancel_pending_photo_decodes();
        self.photo_thumbnail_attempted.clear();
        self.photo_full_size_failed.clear();
        self.browse_context = Some(BrowseContext {
            parent_id,
            title,
            is_collection,
        });
        self.search_query.clear();
        self.submitted_search = None;
        self.search_active = false;
        self.search_results.clear();
        self.search_stale = false;
        self.search_debouncer.input("", Duration::ZERO);
        self.catalog_request_id = self.allocate_request_id();
        self.catalog_loading = false;
        if self.server.is_some() {
            self.request_catalog(0, None);
        }
    }

    pub(crate) fn activate_catalog_item(&mut self, item_id: String) {
        let nested_collection = self
            .items
            .iter()
            .find(|item| item.id == item_id)
            .filter(|item| item.remote_type.as_deref() == Some("BoxSet"))
            .map(|item| (item.id.clone(), item.title.clone()));
        if let Some((id, title)) = nested_collection {
            self.navigate_to_parent(Page::Home, id, title, true);
        } else {
            self.select_item(item_id);
        }
    }

    fn accumulate_collection_runtime(&mut self, items: &[BaseItemDto]) {
        let page_runtime = collection_runtime(items);
        self.collection_runtime.total_ticks = self
            .collection_runtime
            .total_ticks
            .saturating_add(page_runtime.total_ticks);
        self.collection_runtime.items_with_runtime = self
            .collection_runtime
            .items_with_runtime
            .saturating_add(page_runtime.items_with_runtime);
        self.collection_runtime.items_without_runtime = self
            .collection_runtime
            .items_without_runtime
            .saturating_add(page_runtime.items_without_runtime);
    }

    pub(crate) fn set_view(&mut self, view: AppView) {
        self.view = view;
    }

    pub(crate) fn navigate_back(&mut self) {
        self.cancel_navigation_requests();
        if self.photo_selected_id.take().is_some() {
            self.cancel_pending_photo_decodes();
            return;
        }
        if self.page == Page::Photos && self.browse_context.is_some() {
            self.navigate_to(Page::Photos);
            return;
        }
        if self.selected_person.take().is_some() {
            self.person_items_request_id = self.allocate_request_id();
            self.person_items.clear();
            self.person_items_loading = false;
            self.person_items_error = None;
            return;
        }
        if self.selected_item_id.is_some()
            && let Some(parent) = self.detail_parent.take()
        {
            self.select_item(parent);
            return;
        }
        if self.selected_item_id.take().is_some() {
            self.details_request_id = self.allocate_request_id();
            self.playback_request_id = self.allocate_request_id();
            self.playback_start_request_id = self.allocate_request_id();
            self.selected_details = None;
            self.details_loading = false;
            self.details_error = None;
            self.reset_series_detail();
            self.reset_people_detail();
            self.playback_info = None;
            self.playback_loading = false;
            self.playback_error = None;
            self.playback_start_loading = false;
            self.playback_start_error = None;
            self.playback_start_status = None;
            self.pending_playback = None;
        } else if self.view == AppView::Library {
            self.view = AppView::Home;
        }
    }

    /// Reloads the home rows and library/collection tiles. Previous content stays visible while
    /// each request is pending.
    pub(crate) fn refresh_home(&mut self) {
        self.request_menu_playlists();
        let sections = self.home.sections;
        for kind in HomeRowKind::ALL {
            if sections.shows_row(kind) {
                self.request_home_row(kind);
            }
        }
        for kind in HomeCatalogKind::ALL {
            if sections.shows_catalog(kind) {
                self.request_home_catalog(kind);
            }
        }
    }

    /// Applies the home section settings, and loads a section the viewer just turned on.
    fn sync_home_sections(&mut self) {
        self.home.sections =
            crate::ui::home::HomeSections::from_settings(&self.settings.settings().general);
        if self.user_name.is_none() {
            return;
        }
        let (rows, catalogs) = self.home.idle_sections();
        for kind in rows {
            self.request_home_row(kind);
        }
        for kind in catalogs {
            self.request_home_catalog(kind);
        }
    }

    /// Refreshes the playlists (and, for administrators, the collections) the card menu offers.
    fn request_menu_playlists(&mut self) {
        let request_id = self.allocate_request_id();
        if self
            .worker
            .as_ref()
            .is_some_and(|worker| worker.list_user_playlists(request_id).is_ok())
        {
            self.menu_playlists_request_id = request_id;
        }
        if self.user_is_admin {
            let request_id = self.allocate_request_id();
            if self
                .worker
                .as_ref()
                .is_some_and(|worker| worker.list_menu_collections(request_id).is_ok())
            {
                self.menu_collections_request_id = request_id;
            }
        }
    }

    /// Removes a deleted item from every list showing it, and leaves its page.
    fn forget_deleted_item(&mut self, item_id: &str) {
        self.items.retain(|item| item.id != item_id);
        self.home.remove_item(item_id);
        self.series_episodes.retain(|episode| episode.id != item_id);
        if self.selected_item_id.as_deref() == Some(item_id) {
            self.navigate_back();
        }
    }

    fn show_toast(&mut self, text: String) {
        self.toast = Some((text, None));
    }

    /// Draws the toast at the bottom of the window until it expires.
    fn draw_toast(&mut self, ctx: &egui::Context) {
        const TOAST_SECONDS: f64 = 3.0;
        let now = ctx.input(|input| input.time);
        let Some((text, until)) = self.toast.as_mut() else {
            return;
        };
        let until = *until.get_or_insert(now + TOAST_SECONDS);
        if now >= until {
            self.toast = None;
            return;
        }
        egui::Area::new(egui::Id::unique("kattri-toast"))
            .order(egui::Order::Foreground)
            .anchor(egui::Align2::CENTER_BOTTOM, egui::vec2(0.0, -32.0))
            .interactable(false)
            .show(ctx, |ui| {
                egui::Frame::new()
                    .fill(egui::Color32::from_rgba_unmultiplied(20, 20, 24, 235))
                    .corner_radius(egui::CornerRadius::same(12))
                    .inner_margin(egui::Margin::symmetric(18, 12))
                    .show(ui, |ui| {
                        ui.label(egui::RichText::new(text.as_str()).color(egui::Color32::WHITE));
                    });
            });
        ctx.request_repaint_after(std::time::Duration::from_secs_f64(until - now));
    }

    pub(crate) fn request_home_row(&mut self, kind: HomeRowKind) {
        let request_id = self.allocate_request_id();
        self.home.begin_load(kind, request_id);
        let result = self
            .worker
            .as_ref()
            .ok_or(WorkerSubmitError::Stopped)
            .and_then(|worker| worker.load_home_row(request_id, kind));
        if result.is_err() {
            self.home.apply_failure(
                kind,
                request_id,
                ui_text(ids::APP_ERROR_QUEUE_NETWORK).into(),
            );
        }
    }

    pub(crate) fn request_home_catalog(&mut self, kind: HomeCatalogKind) {
        let request_id = self.allocate_request_id();
        self.home.begin_catalog_load(kind, request_id);
        let result = self
            .worker
            .as_ref()
            .ok_or(WorkerSubmitError::Stopped)
            .and_then(|worker| worker.load_home_catalog(request_id, kind));
        if result.is_err() {
            self.home.apply_catalog_failure(
                kind,
                request_id,
                ui_text(ids::APP_ERROR_QUEUE_NETWORK).into(),
            );
        }
    }

    pub(crate) fn request_live_tv_channels(&mut self, start_index: u32) {
        if self.live_tv_loading || (self.live_tv_loaded && start_index == 0) {
            return;
        }
        self.live_tv_loading = true;
        self.live_tv_error = None;
        self.live_tv_notice = None;
        self.live_tv_requested_start_index = start_index;
        let request_id = self.allocate_request_id();
        self.live_tv_request_id = request_id;
        let result = self
            .worker
            .as_ref()
            .ok_or(WorkerSubmitError::Stopped)
            .and_then(|worker| worker.load_live_tv_channels(request_id, start_index, 100));
        if result.is_err() {
            self.live_tv_loading = false;
            self.live_tv_error = Some(ErrorClass::Unexpected);
        }
    }

    fn enqueue_live_tv_channel_logos(&mut self, channels: &[LiveTvChannel]) {
        for channel in channels {
            let Some(image_tag) = channel
                .item
                .image_tags
                .as_ref()
                .and_then(|tags| tags.get("Primary"))
                .and_then(Option::as_deref)
                .filter(|tag| !tag.is_empty())
            else {
                continue;
            };
            let channel_id = channel.item.id.clone();
            if self.live_tv_channel_logos.contains_key(&channel_id)
                || self.live_tv_logo_attempted.contains(&channel_id)
                || self
                    .live_tv_logo_pending
                    .values()
                    .any(|(pending_id, _)| pending_id == &channel_id)
                || self
                    .live_tv_logo_queue
                    .iter()
                    .any(|(queued_id, _)| queued_id == &channel_id)
            {
                continue;
            }
            self.live_tv_logo_queue
                .push_back((channel_id, image_tag.to_owned()));
        }
        self.pump_live_tv_logo_requests();
    }

    fn pump_live_tv_logo_requests(&mut self) {
        while self.live_tv_logo_pending.len() < MAX_PENDING_ARTWORK_REQUESTS {
            let Some((channel_id, image_tag)) = self.live_tv_logo_queue.pop_front() else {
                break;
            };
            let request_id = self.allocate_request_id();
            let result = self
                .worker
                .as_ref()
                .ok_or(WorkerSubmitError::Stopped)
                .and_then(|worker| {
                    worker.get_live_tv_logo(request_id, channel_id.clone(), image_tag.clone())
                });
            if result.is_ok() {
                self.live_tv_logo_attempted.insert(channel_id.clone());
                self.live_tv_logo_pending
                    .insert(request_id, (channel_id, image_tag));
            } else {
                self.live_tv_logo_queue.push_front((channel_id, image_tag));
                break;
            }
        }
    }

    pub(crate) fn retry_live_tv_channels(&mut self) {
        self.live_tv_channels.clear();
        self.live_tv_channel_logos.clear();
        self.live_tv_logo_queue.clear();
        self.live_tv_logo_pending.clear();
        self.live_tv_logo_attempted.clear();
        self.live_tv_total_count = None;
        self.live_tv_loaded = false;
        self.live_tv_not_set_up = false;
        self.live_tv_error = None;
        self.live_tv_notice = None;
        self.live_tv_programs.clear();
        self.live_tv_guide_window = None;
        self.live_tv_guide_available = false;
        self.live_tv_guide_error = None;
        self.live_tv_guide_pending_channel_ids.clear();
        self.live_tv_guide_refreshed_at_unix = None;
        self.request_live_tv_channels(0);
    }

    pub(crate) fn tune_live_tv_channel(&mut self, channel_id: String) {
        if !self
            .live_tv_channels
            .iter()
            .any(|channel| channel.item.id == channel_id)
        {
            self.live_tv_notice = Some(ui_text(ids::LIVE_TV_NOTICE_UNAVAILABLE).into());
            return;
        }
        let request_id = self.allocate_request_id();
        self.live_tv_selected_channel_id = Some(channel_id.clone());
        self.selected_item_id = Some(channel_id.clone());
        self.live_tv_tune_request_id = Some(request_id);
        self.live_tv_retry_available = false;
        self.playback_start_request_id = request_id;
        self.playback_start_loading = true;
        self.playback_start_error = None;
        self.playback_start_status = Some(ui_text(ids::LIVE_TV_STATUS_TUNING).into());
        self.pending_playback = None;
        self.live_tv_notice = None;
        let tune_started_at = std::time::Instant::now();

        let result = self
            .worker
            .as_ref()
            .ok_or(WorkerSubmitError::Stopped)
            .and_then(|worker| {
                worker.tune_live_tv_channel(
                    request_id,
                    channel_id,
                    self.device_profile(),
                    tune_started_at,
                )
            });
        if result.is_err() {
            self.live_tv_tune_request_id = None;
            self.live_tv_retry_available = true;
            self.playback_start_loading = false;
            self.playback_start_status = None;
            self.playback_start_error = Some(ui_text(ids::LIVE_TV_ERROR_QUEUE_TUNE).into());
            self.live_tv_notice = Some(ui_text(ids::LIVE_TV_ERROR_TUNE_FAILED).into());
        }
    }

    /// Show recovery controls when the selected native Live TV session ends unexpectedly.
    pub fn on_live_tv_stream_ended(&mut self, channel_id: &str) {
        if self.live_tv_selected_channel_id.as_deref() != Some(channel_id) {
            return;
        }
        self.playback_start_loading = false;
        self.playback_start_status = None;
        self.live_tv_tune_request_id = None;
        self.live_tv_notice = Some(ui_text(ids::LIVE_TV_NOTICE_STOPPED).into());
        self.live_tv_retry_available = true;
    }

    pub(crate) fn retry_live_tv_channel(&mut self) {
        if let Some(channel_id) = self.live_tv_selected_channel_id.clone() {
            self.tune_live_tv_channel(channel_id);
        }
    }

    /// Queue server-side cleanup when the native player cannot start an opened Live TV stream.
    pub fn close_live_tv_stream(&self, live_stream_id: String) {
        if let Some(worker) = self.worker.as_ref()
            && let Err(error) = worker.close_live_stream(live_stream_id)
        {
            tracing::warn!(%error, "could not queue cleanup after Live TV playback setup failed");
        }
    }

    pub(crate) fn request_live_tv_programs(&mut self, channel_ids: Vec<String>, now_unix: i64) {
        if channel_ids.is_empty() {
            return;
        }
        if self.live_tv_guide_loading {
            let mut pending = self
                .live_tv_guide_requested_channel_ids
                .iter()
                .chain(&self.live_tv_guide_pending_channel_ids)
                .cloned()
                .collect::<HashSet<_>>();
            self.live_tv_guide_pending_channel_ids.extend(
                channel_ids
                    .into_iter()
                    .filter(|id| pending.insert(id.clone())),
            );
            return;
        }
        let current = self.live_tv_guide_window;
        let window = match current {
            Some(window) if !kattri_client_core::epg::guide_window_is_stale(window, now_unix) => {
                window
            }
            _ => kattri_client_core::epg::guide_window_at(now_unix),
        };
        let needs_new_window = current != Some(window);
        // Moving the window forward by a slot or two keeps the programs that are still on, so
        // the guide does not blank while the next request runs.
        let keep_current_programs = current.is_some_and(|old| {
            !kattri_client_core::epg::should_roll_window(old.end_unix, now_unix)
        });
        let Ok(api_window) = jellyfin_api::GuideWindow::new(window.start_unix, window.end_unix)
        else {
            self.live_tv_guide_error = Some(ErrorClass::Unexpected);
            return;
        };
        if needs_new_window {
            if keep_current_programs {
                self.live_tv_programs.retain(|program| {
                    program
                        .end_unix
                        .unwrap_or(program.start_unix + kattri_client_core::epg::EPG_SLOT_SECONDS)
                        > window.start_unix
                });
            } else {
                self.live_tv_programs.clear();
                self.live_tv_guide_available = false;
            }
            self.live_tv_guide_window = Some(window);
        }
        self.live_tv_guide_loading = true;
        self.live_tv_guide_error = None;
        self.live_tv_guide_requested_channel_ids = channel_ids.clone();
        let request_id = self.allocate_request_id();
        self.live_tv_guide_request_id = request_id;
        let result = self
            .worker
            .as_ref()
            .ok_or(WorkerSubmitError::Stopped)
            .and_then(|worker| worker.load_live_tv_programs(request_id, api_window, channel_ids));
        if result.is_err() {
            self.live_tv_guide_loading = false;
            self.live_tv_guide_error = Some(ErrorClass::Unexpected);
        }
    }

    pub(crate) fn retry_live_tv_guide(&mut self) {
        let channel_ids = if self.live_tv_guide_requested_channel_ids.is_empty() {
            self.live_tv_channels
                .iter()
                .map(|channel| channel.item.id.clone())
                .collect()
        } else {
            self.live_tv_guide_requested_channel_ids.clone()
        };
        self.live_tv_guide_error = None;
        self.request_live_tv_programs(channel_ids, unix_now_seconds());
    }

    pub(crate) fn queue_selected_audio(&mut self, play_next: bool) {
        let Some(item) = self.selected_details.as_ref().filter(|item| {
            item.item_type.as_deref() == Some("Audio")
                && self.selected_item_id.as_deref() == Some(item.id.as_str())
        }) else {
            return;
        };
        let queue_item = QueueItem::new(item.id.clone());
        let result = if play_next {
            self.music_queue.play_next(queue_item)
        } else {
            self.music_queue.add(queue_item)
        };
        if result.is_ok() {
            self.music_track_names
                .insert(item.id.clone(), item.name.clone());
        }
    }

    pub(crate) fn request_selected_album_tracks(&mut self) {
        let Some(album_id) = self.selected_details.as_ref().and_then(|item| {
            (item.item_type.as_deref() == Some("MusicAlbum")
                && self.selected_item_id.as_deref() == Some(item.id.as_str()))
            .then(|| item.id.clone())
        }) else {
            return;
        };
        let request_id = self.allocate_request_id();
        self.music_album_tracks_request_id = request_id;
        self.music_album_tracks_loading = true;
        self.music_album_tracks_error = None;
        let result = self
            .worker
            .as_ref()
            .ok_or(WorkerSubmitError::Stopped)
            .and_then(|worker| worker.list_music_album_tracks(request_id, album_id));
        if result.is_err() {
            self.music_album_tracks_loading = false;
            self.music_album_tracks_error = Some(ErrorClass::Unexpected);
        }
    }

    pub(crate) fn apply_music_action(&mut self, action: MusicAction) {
        match action {
            MusicAction::PlayNow { item_id } => {
                if let Some(index) = self
                    .music_queue
                    .items()
                    .iter()
                    .position(|item| item.id == item_id)
                    && self.music_queue.select(index).is_ok()
                {
                    self.music_queue_position_ms = 0;
                    self.music_queue_resume_position_ms = None;
                    self.music_queue_resume_position_ticks = None;
                    self.select_item(item_id);
                }
            }
            MusicAction::PlayNext { item_id } => {
                if self
                    .music_queue
                    .play_next(QueueItem::new(item_id.clone()))
                    .is_ok()
                    && let Some(name) = self
                        .items
                        .iter()
                        .find(|item| item.id == item_id)
                        .map(|item| item.title.clone())
                {
                    self.music_track_names.insert(item_id, name);
                }
            }
            MusicAction::AddToQueue { item_id } => {
                if self
                    .music_queue
                    .add(QueueItem::new(item_id.clone()))
                    .is_ok()
                    && let Some(name) = self
                        .items
                        .iter()
                        .find(|item| item.id == item_id)
                        .map(|item| item.title.clone())
                {
                    self.music_track_names.insert(item_id, name);
                }
            }
            MusicAction::Remove {
                queue_index,
                item_id,
            } => {
                if self
                    .music_queue
                    .items()
                    .get(queue_index)
                    .is_some_and(|item| item.id == item_id)
                {
                    let _ = self.music_queue.remove(queue_index);
                }
            }
            MusicAction::SetShuffle(shuffle) => self.music_queue.set_shuffle(shuffle),
            MusicAction::SetRepeat(repeat) => self.music_queue.set_repeat(repeat),
        }
    }

    pub fn on_music_track_ended(&mut self, ended_item_id: &str, failed: bool) {
        let current = self
            .music_queue
            .current()
            .or_else(|| self.music_queue.removed_current());
        if current.is_none_or(|item| item.id != ended_item_id) {
            return;
        }
        let step = if failed {
            self.music_queue.track_failed()
        } else {
            self.music_queue.track_ended()
        };
        match step {
            Step::End => {
                self.music_queue_auto_play_item_id = None;
                self.music_queue_notice = Some(ui_text(ids::MUSIC_QUEUE_FINISHED).to_owned());
            }
            Step::Empty => {
                self.music_queue_auto_play_item_id = None;
                self.music_queue_notice = None;
            }
            Step::Changed(_) | Step::Restart => {
                self.schedule_current_music_track(ui_text(ids::MUSIC_STATUS_PREPARING_NEXT));
            }
        }
    }

    /// Handle the native player's Previous control for the currently loaded
    /// music-queue item. Returns true when the player should seek to zero.
    pub fn on_music_previous(&mut self, playing_item_id: &str, elapsed: Duration) -> bool {
        let current = self
            .music_queue
            .current()
            .or_else(|| self.music_queue.removed_current());
        if current.is_none_or(|item| item.id != playing_item_id) {
            return false;
        }
        match self.music_queue.previous_track(elapsed) {
            Step::Restart => true,
            Step::Changed(_) => {
                self.schedule_current_music_track(ui_text(ids::MUSIC_STATUS_PREPARING_PREVIOUS));
                false
            }
            Step::End | Step::Empty => false,
        }
    }

    pub fn music_queue_current_item_id(&self) -> Option<&str> {
        self.music_queue
            .current()
            .or_else(|| self.music_queue.removed_current())
            .map(|item| item.id.as_str())
    }

    /// Handle the native player's Next control for the current music item.
    pub fn on_music_next(&mut self, playing_item_id: &str) {
        if self.music_queue_current_item_id() != Some(playing_item_id) {
            return;
        }
        match self.music_queue.next_track() {
            Step::Changed(_) | Step::Restart => {
                self.schedule_current_music_track(ui_text(ids::MUSIC_STATUS_PREPARING_NEXT));
            }
            Step::End => {
                self.music_queue_notice = Some(ui_text(ids::MUSIC_QUEUE_FINISHED).into());
            }
            Step::Empty => {}
        }
    }

    fn schedule_current_music_track(&mut self, notice: &str) {
        let Some(item_id) = self.music_queue.current().map(|item| item.id.clone()) else {
            self.music_queue_auto_play_item_id = None;
            return;
        };
        self.music_queue_auto_play_item_id = Some(item_id.clone());
        self.music_queue_position_ms = 0;
        self.music_queue_resume_position_ms = None;
        self.music_queue_resume_position_ticks = None;
        self.music_queue_notice = Some(notice.to_owned());
        if self.selected_item_id.as_deref() == Some(item_id.as_str()) {
            self.request_playback_options();
        } else {
            self.select_item(item_id);
        }
    }

    fn request_saved_music_queue(&mut self) {
        let Some(stored) = self.pending_stored_music_queue.as_ref() else {
            return;
        };
        if self.music_queue_restore_loading {
            return;
        }
        let item_ids = stored
            .queue
            .items()
            .iter()
            .map(|item| item.id.clone())
            .collect();
        let request_id = self.allocate_request_id();
        self.music_queue_restore_request_id = request_id;
        self.music_queue_restore_loading = true;
        let result = self
            .worker
            .as_ref()
            .ok_or(WorkerSubmitError::Stopped)
            .and_then(|worker| worker.restore_queue_items(request_id, item_ids));
        if result.is_err() {
            self.music_queue_restore_loading = false;
            self.music_queue_notice = Some(ui_text(ids::MUSIC_QUEUE_REFRESH_FAILED).into());
        }
    }

    fn reconcile_restored_music_queue(&mut self, items: Vec<BaseItemDto>) {
        let Some(mut stored) = self.pending_stored_music_queue.take() else {
            return;
        };
        let audio_items = items
            .into_iter()
            .filter(|item| item.item_type.as_deref() == Some("Audio"))
            .collect::<Vec<_>>();
        let available_ids = audio_items
            .iter()
            .map(|item| item.id.as_str())
            .collect::<HashSet<_>>();
        let removed = stored
            .queue
            .retain_items(|item| available_ids.contains(item.id.as_str()));
        self.music_track_names = audio_items
            .into_iter()
            .map(|item| (item.id, item.name))
            .collect();
        if stored.queue.current_index().is_none()
            && !stored.queue.is_empty()
            && let Some(last) = stored.queue.items().len().checked_sub(1)
        {
            let _ = stored.queue.select(last);
        }
        self.music_queue = stored.queue;
        self.music_queue_resume_position_ms = Some(stored.position_ms);
        self.music_queue_restore_loading = false;
        self.music_queue_persistence_suspended = false;
        self.music_queue_last_saved_json = None;
        self.music_queue_position_ms = stored.position_ms;
        self.music_queue_notice = match (removed, self.music_queue.is_empty()) {
            (_, true) => Some(ui_text(ids::MUSIC_QUEUE_NO_SAVED_TRACKS).into()),
            (0, false) => Some(ui_text(ids::MUSIC_QUEUE_RESTORED).into()),
            (count, false) => Some(ui_text_args(
                ids::MUSIC_QUEUE_UNAVAILABLE_REMOVED,
                &[
                    ("count", &count.to_string()),
                    (
                        "noun",
                        if count == 1 {
                            ui_text(ids::MUSIC_NOUN_TRACK)
                        } else {
                            ui_text(ids::MUSIC_NOUN_TRACKS)
                        },
                    ),
                    (
                        "verb",
                        if count == 1 {
                            ui_text(ids::MUSIC_VERB_WAS)
                        } else {
                            ui_text(ids::MUSIC_VERB_WERE)
                        },
                    ),
                ],
            )),
        };
    }

    pub(crate) fn resume_saved_music_queue(&mut self) {
        let Some(item_id) = self.music_queue.current().map(|item| item.id.clone()) else {
            self.music_queue_notice = Some(ui_text(ids::MUSIC_QUEUE_NOTHING_TO_RESUME).into());
            return;
        };
        self.music_queue_resume_position_ticks = self
            .music_queue_resume_position_ms
            .take()
            .and_then(|position| i64::try_from(position.saturating_mul(10_000)).ok());
        self.music_queue_auto_play_item_id = Some(item_id.clone());
        self.select_item(item_id);
    }

    pub fn observe_music_queue_position(
        &mut self,
        playing_item_id: Option<&str>,
        position_seconds: Option<f64>,
    ) {
        let Some(playing_item_id) = playing_item_id else {
            return;
        };
        if self.music_queue_current_item_id() != Some(playing_item_id) {
            return;
        }
        let Some(seconds) =
            position_seconds.filter(|seconds| seconds.is_finite() && *seconds >= 0.0)
        else {
            return;
        };
        self.music_queue_position_ms = (seconds * 1000.0).min(u64::MAX as f64) as u64;
    }

    fn persist_music_queue_if_needed(&mut self) {
        if self.music_queue_persistence_suspended
            || self.pending_stored_music_queue.is_some()
            || self.music_queue_restore_loading
        {
            return;
        }
        let Ok(encoded) = self.music_queue.encode() else {
            self.music_queue_notice = Some(ui_text(ids::MUSIC_QUEUE_SAVE_FAILED).into());
            return;
        };
        let dirty = self.music_queue_last_saved_json.as_deref() != Some(encoded.as_str());
        let checkpoint_due =
            self.ui_time.saturating_sub(self.music_queue_last_saved_at) >= Duration::from_secs(10);
        if !dirty && !checkpoint_due {
            return;
        }
        match self
            .settings
            .save_queue(&self.music_queue, self.music_queue_position_ms)
        {
            Some(Ok(())) => {
                self.music_queue_last_saved_json = Some(encoded);
                self.music_queue_last_saved_at = self.ui_time;
            }
            Some(Err(_)) => {
                self.music_queue_last_saved_json = Some(encoded);
                self.music_queue_last_saved_at = self.ui_time;
                self.music_queue_notice = Some(ui_text(ids::MUSIC_QUEUE_SAVE_FAILED).into());
            }
            None => {
                self.music_queue_persistence_suspended = true;
                self.music_queue_notice =
                    Some(ui_text(ids::MUSIC_QUEUE_PERSISTENCE_UNAVAILABLE).into());
            }
        }
    }

    /// Routes a home-screen intent into the same details and playback flow as the library.
    pub(crate) fn apply_home_action(&mut self, action: HomeAction) {
        match action {
            HomeAction::Select(item_id) => self.select_item(item_id),
            HomeAction::Play(item_id) => self.play_item(item_id, false),
            HomeAction::Refresh => self.refresh_home(),
            HomeAction::Retry(kind) => self.request_home_row(kind),
            HomeAction::RetryCatalog(kind) => self.request_home_catalog(kind),
            HomeAction::OpenLibrary(tile) => match tile.target {
                HomeViewTarget::Playlists => {
                    self.view = AppView::Library;
                    self.page = Page::Playlists;
                    self.browse_context = None;
                    self.search_query.clear();
                    self.submitted_search = None;
                    self.search_active = false;
                    self.search_results.clear();
                    self.search_stale = false;
                    self.search_debouncer.input("", Duration::ZERO);
                    if self.server.is_some() {
                        self.request_catalog(0, None);
                    }
                }
                target => {
                    let page = match target {
                        HomeViewTarget::Movies => Page::Movies,
                        HomeViewTarget::Series => Page::Series,
                        HomeViewTarget::Music => Page::Music,
                        HomeViewTarget::AllItems | HomeViewTarget::Playlists => Page::Home,
                    };
                    self.navigate_to_parent(page, tile.id, tile.name, false);
                }
            },
            HomeAction::OpenCollection { id, name } => {
                self.navigate_to_parent(Page::Home, id, name, true);
            }
        }
    }

    /// Library items and home-row items share the lazy artwork scheduler and texture cache.
    fn artwork_items(&self) -> impl Iterator<Item = &MediaItem> {
        self.items.iter().chain(self.home.media())
    }

    pub(crate) fn submit_search(&mut self, now: Duration) {
        let query = self.search_query.clone();
        self.handle_search_input(&query, now);
    }

    pub(crate) fn handle_search_input(&mut self, input: &str, now: Duration) {
        self.search_query = input.to_owned();
        self.search_browse_group = None;
        self.search_active = true;
        self.selected_item_id = None;
        self.selected_details = None;
        self.details_request_id = self.allocate_request_id();
        self.reset_series_detail();
        self.reset_people_detail();
        self.details_loading = false;
        self.details_error = None;
        let action = self.search_debouncer.input(input, now);
        self.catalog_request_id = self.allocate_request_id();
        self.invalidate_live_tv_search();
        self.catalog_loading = false;
        self.catalog_error = None;

        match action {
            SearchInputAction::Clear { .. } => {
                self.submitted_search = None;
                self.search_results.clear();
                self.search_stale = false;
                self.items.clear();
                self.catalog_next_index = 0;
                self.catalog_end_reached = true;
            }
            SearchInputAction::Debouncing { .. } => {
                self.search_stale = !self.search_results.is_empty();
                if self.search_results.is_empty() {
                    self.items.clear();
                }
            }
        }
    }

    fn poll_search_debounce(&mut self, now: Duration) {
        if let Some(request) = self.search_debouncer.take_ready(now) {
            self.dispatch_search(request.query);
        }
    }

    fn dispatch_search(&mut self, query: String) {
        let query = query.trim().to_owned();
        if query.chars().count() < 2 {
            self.search_results.clear();
            self.search_stale = false;
            return;
        }
        self.search_query = query.clone();
        self.submitted_search = Some(query.clone());
        self.search_stale = !self.search_results.is_empty();
        self.request_catalog(0, Some(query));
    }

    pub(crate) fn clear_search(&mut self, now: Duration) {
        let had_search = self.search_active
            || !self.search_query.trim().is_empty()
            || self.submitted_search.is_some()
            || !self.search_results.is_empty();
        self.search_query.clear();
        self.search_active = false;
        self.search_debouncer.input("", now);
        self.search_stale = false;
        self.search_results.clear();
        self.search_browse_group = None;
        self.invalidate_live_tv_search();
        if had_search {
            self.submitted_search = None;
            self.request_catalog(0, None);
        }
    }

    pub(crate) fn next_page(&mut self) {
        let next_start = self.catalog_next_index;
        if !self.catalog_loading && !self.catalog_end_reached {
            self.request_catalog(next_start, self.submitted_search.clone());
        }
    }

    pub(crate) fn maybe_request_next_catalog_page(
        &mut self,
        focused_index: usize,
        items_per_row: usize,
    ) {
        if (self.submitted_search.is_some() && self.search_browse_group.is_none())
            || self.catalog_loading
            || self.catalog_end_reached
        {
            return;
        }
        if PagingState::focus_within_prefetch_range(focused_index, self.items.len(), items_per_row)
        {
            self.next_page();
        }
    }

    pub(crate) fn show_all_search_group(&mut self, group: crate::ui::search::SearchGroup) {
        let Some(term) = self.submitted_search.clone() else {
            return;
        };
        if let Some(worker) = self.worker.as_ref() {
            worker.cancel_navigation_reads();
        }
        self.search_browse_group = Some(group);
        self.search_active = false;
        self.search_results.clear();
        self.items.clear();
        self.total_record_count = None;
        self.catalog_next_index = 0;
        self.catalog_end_reached = false;
        self.catalog_error = None;
        self.search_stale = false;
        if group == crate::ui::search::SearchGroup::Channels {
            self.invalidate_live_tv_search();
            self.start_live_tv_search_page(Some(&term), 0);
        } else {
            self.request_catalog(0, Some(term));
        }
    }

    pub(crate) fn return_to_search_results(&mut self) {
        let Some(term) = self.submitted_search.clone() else {
            self.search_browse_group = None;
            self.invalidate_live_tv_search();
            return;
        };
        if let Some(worker) = self.worker.as_ref() {
            worker.cancel_navigation_reads();
        }
        self.search_browse_group = None;
        self.search_active = false;
        self.items.clear();
        self.search_results.clear();
        self.total_record_count = None;
        self.catalog_next_index = 0;
        self.catalog_end_reached = false;
        self.search_stale = false;
        self.request_catalog(0, Some(term));
    }

    pub(crate) fn next_live_tv_search_page(&mut self) {
        let Some(term) = self.submitted_search.clone() else {
            return;
        };
        if self.search_browse_group == Some(crate::ui::search::SearchGroup::Channels)
            && !self.live_tv_search_loading
            && self
                .live_tv_search_total
                .is_some_and(|total| self.live_tv_search_start_index < total)
        {
            self.start_live_tv_search_page(Some(&term), self.live_tv_search_start_index);
        }
    }

    /// The rating ceiling for browse rows of the running profile, or `None` when no ceiling
    /// applies (no PIN, no limit, or the profile was unlocked in this run).
    fn browse_rating_ceiling(&self, ctx: &egui::Context) -> Option<u32> {
        let parental = &self.settings.settings().parental;
        crate::parental::running_profile_ceiling(
            ctx,
            &parental.pin_record,
            parental.max_rating,
            self.settings.running_name(),
        )
    }

    fn apply_catalog_response(
        &mut self,
        request_id: u64,
        page: ItemsResponse,
        ceiling: Option<u32>,
    ) -> bool {
        if !catalog_response_is_current(request_id, self.catalog_request_id) {
            return false;
        }

        if self.page == Page::Photos {
            self.photo_items = crate::parental::shown_items(&page.items, ceiling);
            self.photo_page_start = page.start_index as usize;
            self.photo_thumbnail_attempted.clear();
            self.cancel_pending_photo_decodes();
        }
        // Paging counts use the raw page length. Hidden items still advance the server offset.
        let page_size = page.items.len() as u32;
        let shown = crate::parental::shown_items(&page.items, ceiling);
        let page_start = page.start_index;
        self.start_index = 0;
        if self.submitted_search.is_some() && self.search_browse_group.is_none() {
            self.search_results = shown.clone();
            self.search_stale = false;
        } else {
            self.search_results.clear();
            self.search_stale = false;
        }
        if page_start == 0 {
            self.items.clear();
        }
        update_total_record_count(
            &mut self.total_record_count,
            page_start,
            page.total_record_count,
        );
        let mut collection_page_items = Vec::new();
        self.catalog_next_index = page_start.saturating_add(page_size);
        let page_limit = if self.search_browse_group.is_some() {
            30
        } else {
            CATALOG_PAGE_SIZE
        };
        self.catalog_end_reached = page_size < page_limit
            || self
                .total_record_count
                .is_some_and(|total| self.catalog_next_index >= total);
        self.loaded_search = self.submitted_search.clone();
        let is_collection_browse = self
            .browse_context
            .as_ref()
            .is_some_and(|context| context.is_collection);
        let mut known_ids: HashSet<String> =
            self.items.iter().map(|item| item.id.clone()).collect();
        let mut duplicate_count = 0;
        self.items.extend(shown.into_iter().filter_map(|item| {
            if known_ids.insert(item.id.clone()) {
                if is_collection_browse {
                    collection_page_items.push(item.clone());
                }
                Some(crate::catalog::MediaItem::from_api(item))
            } else {
                duplicate_count += 1;
                None
            }
        }));
        if is_collection_browse {
            self.accumulate_collection_runtime(&collection_page_items);
        }
        if duplicate_count > 0 {
            tracing::warn!(
                duplicate_count,
                page_start,
                "Jellyfin catalog page repeated item IDs; duplicates were dropped"
            );
        }
        self.attempted_artwork.clear();
        self.artwork_access_ticks.clear();
        self.catalog_loaded = true;
        self.catalog_loading = false;
        self.catalog_error = None;
        self.connection = ConnectionStatus::Connected;
        true
    }

    pub(crate) fn set_catalog_sort(&mut self, sort: CatalogSort) {
        if self.catalog_sort == sort {
            return;
        }
        self.catalog_sort = sort;
        self.restart_catalog_query();
    }

    pub(crate) fn set_catalog_sort_order(&mut self, order: ItemSortOrder) {
        if self.catalog_sort_order == order {
            return;
        }
        self.catalog_sort_order = order;
        self.restart_catalog_query();
    }

    pub(crate) fn set_catalog_played_filter(&mut self, filter: CatalogPlayedFilter) {
        if self.catalog_played_filter == filter {
            return;
        }
        self.catalog_played_filter = filter;
        self.restart_catalog_query();
    }

    pub(crate) fn set_catalog_favorites_only(&mut self, enabled: bool) {
        if self.catalog_favorites_only == enabled {
            return;
        }
        self.catalog_favorites_only = enabled;
        self.restart_catalog_query();
    }

    pub(crate) fn set_catalog_genre(&mut self, genre: Option<String>) {
        if self.catalog_genre == genre {
            return;
        }
        self.catalog_genre = genre;
        self.restart_catalog_query();
    }

    /// Pages whose items carry genres.
    fn page_has_genres(page: Page) -> bool {
        matches!(page, Page::Movies | Page::Series | Page::Music)
    }

    /// Loads the genre list for the page and folder on screen, once per scope.
    fn request_catalog_genres(&mut self, item_types: Option<Vec<String>>) {
        let parent_id = self
            .browse_context
            .as_ref()
            .map(|context| context.parent_id.clone());
        let scope = (self.page, parent_id.clone());
        if !Self::page_has_genres(self.page) {
            self.catalog_genres.clear();
            self.catalog_genres_scope = None;
            return;
        }
        if self.catalog_genres_scope.as_ref() == Some(&scope) {
            return;
        }
        self.catalog_genres.clear();
        let request_id = self.allocate_request_id();
        if self.worker.as_ref().is_some_and(|worker| {
            worker
                .list_genres(request_id, parent_id, item_types)
                .is_ok()
        }) {
            self.catalog_genres_request_id = request_id;
            self.catalog_genres_scope = Some(scope);
        }
    }

    pub(crate) fn set_catalog_name_prefix(&mut self, prefix: Option<char>) {
        if self.catalog_name_prefix == prefix {
            return;
        }
        self.catalog_name_prefix = prefix;
        self.restart_catalog_query();
    }

    pub(crate) fn clear_catalog_filters(&mut self) {
        if !self.has_catalog_filters() {
            return;
        }
        self.catalog_played_filter = CatalogPlayedFilter::Any;
        self.catalog_favorites_only = false;
        self.catalog_name_prefix = None;
        self.catalog_genre = None;
        self.restart_catalog_query();
    }

    pub(crate) fn has_catalog_filters(&self) -> bool {
        self.catalog_played_filter != CatalogPlayedFilter::Any
            || self.catalog_favorites_only
            || self.catalog_name_prefix.is_some()
            || self.catalog_genre.is_some()
    }

    /// `(nameStartsWith, nameLessThan)` for the alphabet picker. `'#'` asks for names that
    /// sort before "A", which are the ones starting with a digit or symbol.
    fn catalog_name_bounds(&self) -> (Option<String>, Option<String>) {
        match self.catalog_name_prefix {
            Some('#') => (None, Some("A".to_owned())),
            Some(letter) => (Some(letter.to_string()), None),
            None => (None, None),
        }
    }

    fn catalog_filters(&self) -> Option<Vec<ItemFilter>> {
        let mut filters = Vec::with_capacity(2);
        if let Some(filter) = self.catalog_played_filter.api_filter()
            && self.catalog_filter_supported(filter)
        {
            filters.push(filter);
        }
        if self.catalog_favorites_only && self.catalog_filter_supported(ItemFilter::IsFavorite) {
            filters.push(ItemFilter::IsFavorite);
        }
        (!filters.is_empty()).then_some(filters)
    }

    pub(crate) fn catalog_filter_supported(&self, filter: ItemFilter) -> bool {
        !self
            .unsupported_catalog_filters
            .contains(&(self.page, filter))
    }

    fn apply_unsupported_catalog_filter(&mut self, filter: ItemFilter) {
        if self
            .unsupported_catalog_filters
            .contains(&(self.page, filter))
        {
            return;
        }
        self.unsupported_catalog_filters.push((self.page, filter));
        match filter {
            ItemFilter::IsPlayed if self.catalog_played_filter == CatalogPlayedFilter::Played => {
                self.catalog_played_filter = CatalogPlayedFilter::Any;
            }
            ItemFilter::IsUnplayed
                if self.catalog_played_filter == CatalogPlayedFilter::Unplayed =>
            {
                self.catalog_played_filter = CatalogPlayedFilter::Any;
            }
            ItemFilter::IsFavorite => self.catalog_favorites_only = false,
            _ => {}
        }
        self.catalog_filter_notice = Some(ui_text_args(
            ids::BROWSE_FILTER_UNSUPPORTED,
            &[("filter", &format!("{filter:?}"))],
        ));
        self.restart_catalog_query();
    }

    fn restart_catalog_query(&mut self) {
        self.request_catalog(0, self.submitted_search.clone());
    }

    pub(crate) fn request_photo_page(&mut self, start_index: usize) {
        let Ok(start_index) = u32::try_from(start_index) else {
            return;
        };
        self.photo_items.clear();
        self.photo_thumbnails.clear();
        self.cancel_pending_photo_decodes();
        self.photo_thumbnail_attempted.clear();
        self.items.clear();
        self.photo_page_start = start_index as usize;
        self.request_catalog(start_index, None);
    }

    pub(crate) fn request_photo_thumbnail(&mut self, item_id: &str, image_tag: Option<&str>) {
        if self.photo_thumbnails.contains_key(item_id)
            || self.photo_thumbnail_attempted.contains(item_id)
            || self
                .photo_thumbnail_pending
                .values()
                .any(|pending| pending == item_id)
            || self.photo_thumbnail_pending.len() >= MAX_PENDING_ARTWORK_REQUESTS
        {
            return;
        }
        let request_id = self.allocate_request_id();
        let fallback_index = self
            .photo_items
            .iter()
            .position(|item| item.id == item_id)
            .map_or(self.photo_page_start, |index| self.photo_page_start + index);
        let (image_tag, photo_index, visible) =
            image_tag.map_or((None, fallback_index, true), |tag| {
                decode_photo_request_tag(tag)
                    .unwrap_or_else(|| (Some(tag.to_owned()), fallback_index, true))
            });
        let Some(worker) = self.worker.as_ref() else {
            return;
        };
        if worker
            .load_photo_thumbnail(
                request_id,
                item_id.to_owned(),
                image_tag,
                photo_index,
                visible,
            )
            .is_ok()
        {
            #[cfg(feature = "photo-counters")]
            worker.record_photo_decode_request();
            self.photo_thumbnail_pending
                .insert(request_id, item_id.to_owned());
            self.photo_thumbnail_attempted.insert(item_id.to_owned());
        }
    }

    pub(crate) fn request_photo_full_size(&mut self, item_id: &str) {
        if self.photo_full_size_images.contains_key(item_id)
            || self
                .photo_full_size_pending
                .values()
                .any(|pending| pending == item_id)
        {
            return;
        }
        self.photo_full_size_failed.remove(item_id);
        let image_tag = self
            .photo_items
            .iter()
            .find(|item| item.id == item_id)
            .and_then(|item| item.image_tags.as_ref())
            .and_then(|tags| tags.get("Primary"))
            .and_then(Option::as_deref)
            .map(str::to_owned);
        let photo_index = self
            .photo_items
            .iter()
            .position(|item| item.id == item_id)
            .map_or(self.photo_page_start, |index| self.photo_page_start + index);
        let visible = self.photo_selected_id.as_deref() == Some(item_id);
        let request_id = self.allocate_request_id();
        let Some(worker) = self.worker.as_ref() else {
            return;
        };
        if worker
            .load_photo_full_size(
                request_id,
                item_id.to_owned(),
                image_tag,
                photo_index,
                visible,
            )
            .is_ok()
        {
            self.photo_full_size_pending
                .insert(request_id, item_id.to_owned());
            #[cfg(feature = "photo-counters")]
            worker.record_photo_decode_request();
        }
    }

    fn request_catalog(&mut self, start_index: u32, search_term: Option<String>) {
        let is_search = search_term.is_some() && self.search_browse_group.is_none();
        let is_search_browse = search_term.is_some() && self.search_browse_group.is_some();
        let channel_search_term = if start_index == 0
            && self.search_browse_group != Some(crate::ui::search::SearchGroup::Channels)
        {
            search_term.clone()
        } else {
            None
        };
        if start_index == 0 {
            self.invalidate_live_tv_search();
        }
        let request_id = self.allocate_request_id();
        self.catalog_request_id = request_id;
        self.start_index = start_index;
        if start_index == 0 {
            self.collection_runtime = CollectionRuntime::default();
            if !is_search {
                self.items.clear();
                self.search_results.clear();
                self.search_stale = false;
            } else if self.search_results.is_empty() {
                self.items.clear();
            }
            self.catalog_next_index = 0;
            self.catalog_end_reached = false;
        }
        if start_index == 0 || search_term != self.loaded_search {
            self.total_record_count = None;
        }
        self.catalog_loading = true;
        self.catalog_error = None;
        self.selected_item_id = None;
        self.selected_details = None;
        self.details_loading = false;
        self.details_error = None;
        self.playback_info = None;
        self.playback_loading = false;
        self.playback_error = None;
        self.playback_start_loading = false;
        self.playback_start_error = None;
        self.playback_start_status = None;
        self.pending_playback = None;
        self.details_request_id = self.allocate_request_id();
        self.playback_request_id = self.allocate_request_id();
        self.playback_start_request_id = self.allocate_request_id();
        if !self.catalog_loaded {
            self.connection = ConnectionStatus::LoadingItems;
        }

        if self.worker.is_none() {
            self.catalog_loading = false;
            self.catalog_error = Some(ui_text(ids::CONNECT_WORKER_UNAVAILABLE).into());
            return;
        }

        let browsing = !is_search && !is_search_browse;
        let name_bounds = if browsing {
            self.catalog_name_bounds()
        } else {
            (None, None)
        };
        let genres = self
            .catalog_genre
            .clone()
            .filter(|_| browsing)
            .map(|genre| vec![genre]);
        if browsing && start_index == 0 {
            let page_types = self
                .page
                .item_types()
                .map(|types| types.iter().map(|kind| (*kind).to_owned()).collect());
            self.request_catalog_genres(page_types);
        }
        let request_result =
            self.worker
                .as_ref()
                .map_or(Err(WorkerSubmitError::Stopped), |worker| {
                    let include_item_types = if is_search {
                        Some(
                            [
                                "Movie",
                                "Series",
                                "Episode",
                                "Audio",
                                "MusicAlbum",
                                "Playlist",
                                "Photo",
                                "Person",
                            ]
                            .into_iter()
                            .map(str::to_owned)
                            .collect(),
                        )
                    } else if is_search_browse {
                        self.search_browse_group.map(|group| {
                            group
                                .item_types()
                                .iter()
                                .map(|kind| (*kind).to_owned())
                                .collect()
                        })
                    } else {
                        self.page.item_types().map(|types| {
                            types
                                .iter()
                                .map(|item_type| (*item_type).to_owned())
                                .collect()
                        })
                    };
                    let parent_id = if is_search {
                        None
                    } else {
                        self.browse_context
                            .as_ref()
                            .map(|context| context.parent_id.clone())
                    };
                    worker.fetch_items(
                        request_id,
                        ItemQuery {
                            recursive: self.page != Page::Photos,
                            start_index,
                            limit: if is_search || is_search_browse {
                                30
                            } else if self.page == Page::Photos {
                                100
                            } else {
                                CATALOG_PAGE_SIZE
                            },
                            parent_id,
                            include_item_types,
                            search_term,
                            sort_by: Some(vec![self.catalog_sort.api_value()]),
                            sort_order: Some(vec![self.catalog_sort_order]),
                            filters: if is_search || is_search_browse {
                                None
                            } else {
                                self.catalog_filters()
                            },
                            genres,
                            years: None,
                            person_ids: None,
                            ids: None,
                            enable_total_record_count: Some(true),
                            name_starts_with: name_bounds.0,
                            name_less_than: name_bounds.1,
                        },
                    )
                });
        if request_result.is_err() {
            self.catalog_loading = false;
            let message = ui_text(ids::APP_ERROR_QUEUE_NETWORK).to_owned();
            self.catalog_error = Some(message.clone());
            if !self.catalog_loaded {
                self.connection = ConnectionStatus::LibraryError(message);
            }
        } else if start_index == 0 && self.page == Page::Playlists && !is_search {
            self.request_playlist_candidates();
        }
        if start_index == 0 {
            self.start_live_tv_search(channel_search_term.as_deref());
        }
    }

    fn request_playlist_candidates(&mut self) {
        let request_id = self.allocate_request_id();
        self.playlist_candidates_request_id = request_id;
        self.playlist_candidates_loading = true;
        self.playlist_candidates_error = None;
        let result = self
            .worker
            .as_ref()
            .ok_or(WorkerSubmitError::Stopped)
            .and_then(|worker| worker.load_playlist_candidates(request_id));
        if result.is_err() {
            self.playlist_candidates_loading = false;
            self.playlist_candidates_error = Some(ErrorClass::Unexpected);
        }
    }

    /// Opens `item_id` (an episode) from the page on screen; Back returns to that page.
    pub(crate) fn open_child_item(&mut self, item_id: String) {
        let parent = self.selected_item_id.clone();
        self.select_item(item_id);
        self.detail_parent = parent;
    }

    pub(crate) fn select_item(&mut self, item_id: String) {
        if self.selected_item_id.as_deref() == Some(item_id.as_str())
            && (self.selected_details.is_some() || self.details_loading)
        {
            return;
        }
        self.detail_parent = None;
        self.cancel_navigation_requests();
        self.music_album_tracks_request_id = self.allocate_request_id();
        self.music_album_tracks_loading = false;
        self.music_album_tracks_error = None;
        self.selected_item_id = Some(item_id.clone());
        self.selected_details = None;
        self.reset_series_detail();
        self.reset_people_detail();
        self.selected_artwork_texture = None;
        self.detail_backdrop_attempted = false;
        self.detail_artwork_request_id = self.allocate_request_id();
        self.details_error = None;
        self.playback_info = None;
        self.playback_loading = false;
        self.playback_error = None;
        self.playback_request_id = self.allocate_request_id();
        self.playback_start_request_id = self.allocate_request_id();
        self.playback_start_loading = false;
        self.playback_start_error = None;
        self.playback_start_status = None;
        self.pending_playback = None;
        self.held_playback = None;
        self.load_selected_details(item_id);
    }

    fn request_playlist_items(&mut self, playlist_id: String) {
        let request_id = self.allocate_request_id();
        self.playlist_items_request_id = request_id;
        if self.opened_playlist_id.as_deref() != Some(playlist_id.as_str()) {
            self.playlist_ui_state.selected_media_ids.clear();
        }
        self.opened_playlist_id = Some(playlist_id.clone());
        self.playlist_entries.clear();
        self.playlist_items_loading = true;
        self.playlist_items_error = None;
        let result = self
            .worker
            .as_ref()
            .ok_or(WorkerSubmitError::Stopped)
            .and_then(|worker| worker.list_playlist_items(request_id, playlist_id));
        if result.is_err() {
            self.playlist_items_loading = false;
            self.playlist_items_error = Some(ErrorClass::Unexpected);
        }
    }

    pub(crate) fn play_selected_playlist(&mut self) {
        let queue_items = self
            .playlist_entries
            .iter()
            .filter(|entry| entry.item.item_type.as_deref() == Some("Audio"))
            .map(|entry| QueueItem::new(entry.item.id.clone()))
            .collect::<Vec<_>>();
        if queue_items.is_empty() {
            self.music_queue_notice = Some(ui_text(ids::PLAYLISTS_NOTICE_NO_AUDIO).into());
            return;
        }
        let mut queue = Queue::new();
        if queue.add_album_and_play(queue_items).is_err() {
            self.music_queue_notice = Some(ui_text(ids::PLAYLISTS_NOTICE_TOO_LARGE).into());
            return;
        }
        self.music_track_names.extend(
            self.playlist_entries
                .iter()
                .filter(|entry| entry.item.item_type.as_deref() == Some("Audio"))
                .map(|entry| (entry.item.id.clone(), entry.item.name.clone())),
        );
        self.music_queue = queue;
        self.schedule_current_music_track(ui_text(ids::PLAYLISTS_STATUS_PREPARING));
    }

    pub(crate) fn apply_playlist_intent(&mut self, intent: PlaylistIntent) {
        match intent {
            PlaylistIntent::OpenPlaylist { playlist_id } => self.select_item(playlist_id),
            PlaylistIntent::SubmitCreateName { name } => {
                let Ok(name) = crate::ui::playlists::validate_playlist_name(&name) else {
                    self.playlist_mutation_error = Some(ErrorClass::Unexpected);
                    return;
                };
                let request_id = self.begin_playlist_mutation();
                let result = self
                    .worker
                    .as_ref()
                    .ok_or(WorkerSubmitError::Stopped)
                    .and_then(|worker| worker.create_playlist(request_id, name));
                self.mark_playlist_mutation_submission(result);
            }
            PlaylistIntent::PlayPlaylist { playlist_id } => {
                if self.opened_playlist_id.as_deref() == Some(playlist_id.as_str()) {
                    self.play_selected_playlist();
                }
            }
            PlaylistIntent::AddSelectedMedia {
                playlist_id,
                media_ids,
            } => {
                if media_ids.is_empty()
                    || self.opened_playlist_id.as_deref() != Some(playlist_id.as_str())
                {
                    return;
                }
                let request_id = self.begin_playlist_mutation();
                let result = self
                    .worker
                    .as_ref()
                    .ok_or(WorkerSubmitError::Stopped)
                    .and_then(|worker| {
                        worker.add_playlist_items(request_id, playlist_id, media_ids)
                    });
                self.mark_playlist_mutation_submission(result);
            }
            PlaylistIntent::RemoveEntry {
                playlist_id,
                playlist_entry_id,
            } => {
                if self.opened_playlist_id.as_deref() != Some(playlist_id.as_str()) {
                    return;
                }
                let request_id = self.begin_playlist_mutation();
                let result = self
                    .worker
                    .as_ref()
                    .ok_or(WorkerSubmitError::Stopped)
                    .and_then(|worker| {
                        worker.remove_playlist_entry(request_id, playlist_id, playlist_entry_id)
                    });
                self.mark_playlist_mutation_submission(result);
            }
            PlaylistIntent::MoveEntry {
                playlist_id,
                playlist_entry_id,
                direction,
            } => {
                if self.opened_playlist_id.as_deref() != Some(playlist_id.as_str()) {
                    return;
                }
                let model_entries = self
                    .playlist_entries
                    .iter()
                    .filter_map(|entry| {
                        entry.playlist_item_id.as_deref().and_then(|entry_id| {
                            kattri_client_core::playlist::PlaylistEntry::new(
                                entry_id,
                                &entry.item.id,
                            )
                            .ok()
                        })
                    })
                    .collect::<Vec<_>>();
                let Ok(mut model) =
                    kattri_client_core::playlist::PlaylistModel::from_entries(model_entries)
                else {
                    return;
                };
                let moved = match direction {
                    MoveDirection::Up => model.move_up(&playlist_entry_id),
                    MoveDirection::Down => model.move_down(&playlist_entry_id),
                };
                let Ok(kattri_client_core::playlist::MoveOutcome::Moved { to, .. }) = moved else {
                    return;
                };
                let Ok(new_index) = u32::try_from(to) else {
                    return;
                };
                let request_id = self.begin_playlist_mutation();
                let result = self
                    .worker
                    .as_ref()
                    .ok_or(WorkerSubmitError::Stopped)
                    .and_then(|worker| {
                        worker.move_playlist_entry(
                            request_id,
                            playlist_id,
                            playlist_entry_id,
                            new_index,
                        )
                    });
                self.mark_playlist_mutation_submission(result);
            }
        }
    }

    fn invalidate_live_tv_search(&mut self) {
        self.live_tv_search_request_id = None;
        self.live_tv_search_results.clear();
        self.live_tv_search_loading = false;
        self.live_tv_search_start_index = 0;
        self.live_tv_search_total = None;
    }

    fn start_live_tv_search(&mut self, term: Option<&str>) {
        self.start_live_tv_search_page(term, 0);
    }

    fn start_live_tv_search_page(&mut self, term: Option<&str>, start_index: u32) {
        self.live_tv_search_request_id = None;
        self.live_tv_search_loading = false;
        if start_index == 0 {
            self.live_tv_search_results.clear();
            self.live_tv_search_total = None;
        }
        let Some(term) = term.map(str::trim).filter(|term| term.chars().count() >= 2) else {
            return;
        };
        let request_id = self.allocate_request_id();
        self.live_tv_search_request_id = Some(request_id);
        let Some(worker) = self.worker.as_ref() else {
            return;
        };
        self.live_tv_search_loading = worker
            .search_live_tv_channels(
                request_id,
                term.to_owned(),
                start_index,
                if self.search_browse_group == Some(crate::ui::search::SearchGroup::Channels) {
                    30
                } else {
                    11
                },
            )
            .is_ok();
        if self.live_tv_search_loading {
            self.live_tv_search_start_index = start_index;
        }
    }

    fn begin_playlist_mutation(&mut self) -> u64 {
        let request_id = self.allocate_request_id();
        self.playlist_mutation_request_id = request_id;
        self.playlist_mutation_loading = true;
        self.playlist_mutation_error = None;
        self.playlist_mutation_notice = None;
        request_id
    }

    fn mark_playlist_mutation_submission(&mut self, result: Result<(), WorkerSubmitError>) {
        if result.is_err() {
            self.playlist_mutation_loading = false;
            self.playlist_mutation_error = Some(ErrorClass::Unexpected);
        }
    }

    pub(crate) fn retry_item_details(&mut self) {
        if let Some(item_id) = self.selected_item_id.clone() {
            self.load_selected_details(item_id);
        }
    }

    fn load_selected_details(&mut self, item_id: String) {
        let request_id = self.allocate_request_id();
        self.details_request_id = request_id;
        self.details_loading = true;
        self.details_error = None;
        let result = self
            .worker
            .as_ref()
            .ok_or(WorkerSubmitError::Stopped)
            .and_then(|worker| worker.get_item(request_id, item_id));
        if result.is_err() {
            self.details_loading = false;
            self.details_error = Some(ui_text(ids::APP_ERROR_QUEUE_NETWORK).into());
        }
    }

    fn reset_series_detail(&mut self) {
        self.series_seasons_request_id = self.allocate_request_id();
        self.series_episodes_request_id = self.allocate_request_id();
        self.series_seasons.clear();
        self.series_seasons_loading = false;
        self.series_seasons_error = None;
        self.series_selected_season_id = None;
        self.series_episodes.clear();
        self.series_episodes_loading = false;
        self.series_episodes_error = None;
        self.episode_thumbnails.clear();
        self.episode_thumbnail_pending.clear();
    }

    /// Asks for the stills of the episodes now listed, keeping at most
    /// [`MAX_EPISODE_THUMBNAILS`] textures.
    fn request_episode_thumbnails(&mut self) {
        if self.episode_thumbnails.len() > MAX_EPISODE_THUMBNAILS {
            self.episode_thumbnails.clear();
        }
        let wanted: Vec<(String, String)> = self
            .series_episodes
            .iter()
            .filter(|episode| !self.episode_thumbnails.contains_key(&episode.id))
            .filter_map(|episode| {
                episode
                    .image_tags
                    .as_ref()
                    .and_then(|tags| tags.get("Primary").cloned().flatten())
                    .map(|tag| (episode.id.clone(), tag))
            })
            .take(MAX_EPISODE_THUMBNAILS)
            .collect();
        self.episode_thumbnail_pending.clear();
        for (episode_id, tag) in wanted {
            let request_id = self.allocate_request_id();
            let sent = self.worker.as_ref().is_some_and(|worker| {
                worker
                    .get_primary_artwork(request_id, episode_id.clone(), Some(tag))
                    .is_ok()
            });
            if !sent {
                break;
            }
            self.episode_thumbnail_pending
                .insert(request_id, episode_id);
        }
    }

    fn reset_people_detail(&mut self) {
        self.similar_request_id = self.allocate_request_id();
        self.person_items_request_id = self.allocate_request_id();
        self.similar_items.clear();
        self.similar_loading = false;
        self.selected_person = None;
        self.person_items.clear();
        self.person_items_loading = false;
        self.person_items_error = None;
        self.person_artwork_queue.clear();
        self.pending_person_artwork.clear();
        self.attempted_person_artwork.clear();
        self.person_artwork_textures.clear();
    }

    fn request_similar_items(&mut self) {
        let Some(item_id) = self.selected_item_id.clone() else {
            return;
        };
        self.similar_items.clear();
        self.similar_loading = true;
        let request_id = self.allocate_request_id();
        self.similar_request_id = request_id;
        if self
            .worker
            .as_ref()
            .is_none_or(|worker| worker.list_similar(request_id, item_id, 12).is_err())
        {
            self.similar_loading = false;
        }
    }

    pub(crate) fn open_person_page(&mut self, person: BaseItemPerson) {
        let Some(person_id) = person.id.clone() else {
            return;
        };
        self.selected_person = Some(person);
        self.person_items.clear();
        self.person_items_error = None;
        self.person_items_loading = true;
        let request_id = self.allocate_request_id();
        self.person_items_request_id = request_id;
        if self
            .worker
            .as_ref()
            .is_none_or(|worker| worker.list_person_items(request_id, person_id, 50).is_err())
        {
            self.person_items_loading = false;
            self.person_items_error = Some(ui_text(ids::APP_ERROR_QUEUE_NETWORK).into());
        }
    }

    pub(crate) fn open_person_page_by_id(&mut self, person_id: &str) {
        let person = self
            .selected_details
            .as_ref()
            .and_then(|item| item.people.as_deref())
            .into_iter()
            .flatten()
            .find(|person| person.id.as_deref() == Some(person_id))
            .cloned();
        if let Some(person) = person {
            self.open_person_page(person);
        }
    }

    fn queue_person_artwork(&mut self) {
        self.person_artwork_queue = self
            .selected_details
            .as_ref()
            .and_then(|item| item.people.as_ref())
            .into_iter()
            .flatten()
            .take(20)
            .filter(|person| {
                person
                    .name
                    .as_deref()
                    .is_some_and(|name| !name.trim().is_empty())
            })
            .cloned()
            .collect();
        self.pump_person_artwork_requests();
    }

    fn pump_person_artwork_requests(&mut self) {
        while self.pending_person_artwork.len() < MAX_PENDING_ARTWORK_REQUESTS {
            let Some(person) = self.person_artwork_queue.pop() else {
                break;
            };
            let Some(name) = person
                .name
                .as_deref()
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(str::to_owned)
            else {
                continue;
            };
            if self.person_artwork_textures.contains_key(&name)
                || !self.attempted_person_artwork.insert(name.clone())
            {
                continue;
            }
            let request_id = self.allocate_request_id();
            self.pending_person_artwork.insert(request_id, name.clone());
            let result = self
                .worker
                .as_ref()
                .ok_or(WorkerSubmitError::Stopped)
                .and_then(|worker| {
                    worker.get_person_artwork(request_id, name, person.primary_image_tag.clone())
                });
            if result.is_err() {
                self.pending_person_artwork.remove(&request_id);
            }
        }
    }

    fn request_series_seasons(&mut self, series_id: String) {
        self.series_seasons.clear();
        self.series_episodes.clear();
        self.series_selected_season_id = None;
        self.series_seasons_error = None;
        self.series_episodes_error = None;
        self.series_seasons_loading = true;
        self.series_episodes_loading = false;
        let next_up_request_id = self.allocate_request_id();
        self.series_next_up = self
            .worker
            .as_ref()
            .is_some_and(|worker| {
                worker
                    .get_series_next_up(next_up_request_id, series_id.clone())
                    .is_ok()
            })
            .then(|| SeriesNextUp {
                series_id: series_id.clone(),
                request_id: next_up_request_id,
                episode: None,
            });
        let request_id = self.allocate_request_id();
        self.series_seasons_request_id = request_id;
        if self
            .worker
            .as_ref()
            .is_none_or(|worker| worker.list_seasons(request_id, series_id).is_err())
        {
            self.series_seasons_loading = false;
            self.series_seasons_error = Some(ui_text(ids::APP_ERROR_QUEUE_NETWORK).into());
        }
    }

    /// The episode the series page offers to play, once the server has named one.
    pub(crate) fn series_next_up_episode(&self, series_id: &str) -> Option<&BaseItemDto> {
        self.series_next_up
            .as_ref()
            .filter(|next_up| next_up.series_id == series_id)
            .and_then(|next_up| next_up.episode.as_ref())
    }

    pub(crate) fn select_series_season(&mut self, season_id: String) {
        let Some(series_id) = self
            .selected_details
            .as_ref()
            .filter(|item| item.item_type.as_deref() == Some("Series"))
            .map(|item| item.id.clone())
        else {
            return;
        };
        if self.series_selected_season_id.as_deref() == Some(season_id.as_str())
            && (self.series_episodes_loading || !self.series_episodes.is_empty())
        {
            return;
        }
        self.series_selected_season_id = Some(season_id.clone());
        self.series_episodes.clear();
        self.series_episodes_error = None;
        self.series_episodes_loading = true;
        let request_id = self.allocate_request_id();
        self.series_episodes_request_id = request_id;
        if self.worker.as_ref().is_none_or(|worker| {
            worker
                .list_episodes(request_id, series_id, season_id)
                .is_err()
        }) {
            self.series_episodes_loading = false;
            self.series_episodes_error = Some(ui_text(ids::APP_ERROR_QUEUE_NETWORK).into());
        }
    }

    /// Watched state for the selected item's toggle button.
    pub(crate) fn watched_view(&self) -> WatchedView {
        let Some(item) = self.selected_details.as_ref() else {
            return WatchedView::default();
        };
        match self.watched.as_ref() {
            Some(state) if self.selected_item_id.as_deref() == Some(state.item_id.as_str()) => {
                WatchedView {
                    played: state.toggle.played(),
                    pending: state.toggle.is_pending(),
                    error: state.error.clone(),
                }
            }
            _ => WatchedView {
                played: WatchedToggle::from_user_data(item.user_data.as_ref()).played(),
                pending: false,
                error: None,
            },
        }
    }

    /// Flips the selected item's played state and sends it to the server.
    /// Opens the profile's download folder on the worker, once per run. The folder sits in the
    /// profile directory, next to the queue file.
    fn ensure_downloads_open(&mut self) -> bool {
        if self.downloads.opened {
            return true;
        }
        let Some(profile_dir) = self
            .settings
            .queue_path()
            .and_then(|queue| queue.parent().map(Path::to_path_buf))
        else {
            self.downloads
                .fail(ui_text(ids::DOWNLOADS_ERROR_NO_PROFILE).to_owned());
            return false;
        };
        let sent = self
            .worker
            .as_ref()
            .ok_or(WorkerSubmitError::Stopped)
            .and_then(|worker| worker.download_open(profile_dir.join("downloads")));
        if sent.is_err() {
            self.downloads
                .fail(ui_text(ids::APP_ERROR_DOWNLOAD_QUEUE).to_owned());
            return false;
        }
        self.downloads.opened = true;
        true
    }

    /// Starts a download of `item_id` from the connected server. The title stays in memory for
    /// the page; the worker does not receive it.
    pub fn start_download(&mut self, item_id: &str, title: &str) {
        if !self.ensure_downloads_open() {
            return;
        }
        let base_url = self.server_url.trim().to_owned();
        let sent = self
            .worker
            .as_ref()
            .ok_or(WorkerSubmitError::Stopped)
            .and_then(|worker| worker.download_start(item_id.to_owned(), base_url));
        if sent.is_err() {
            self.downloads
                .fail(ui_text(ids::APP_ERROR_DOWNLOAD_QUEUE).to_owned());
            return;
        }
        self.downloads.remember_title(item_id, title);
    }

    /// Sends a pause or resume the user chose on the Downloads page.
    pub fn download_intent(&mut self, intent: &DownloadIntent) {
        let base_url = self.server_url.trim().to_owned();
        let sent = self
            .worker
            .as_ref()
            .ok_or(WorkerSubmitError::Stopped)
            .and_then(|worker| worker.download_intent(intent, &base_url));
        if sent.is_err() {
            self.downloads
                .fail(ui_text(ids::APP_ERROR_DOWNLOAD_QUEUE).to_owned());
        }
    }

    /// Asks the worker for the current download list.
    pub fn refresh_downloads(&mut self) {
        if !self.ensure_downloads_open() {
            return;
        }
        let sent = self
            .worker
            .as_ref()
            .ok_or(WorkerSubmitError::Stopped)
            .and_then(ApiWorker::download_list);
        self.downloads.last_refresh = Some(Instant::now());
        if sent.is_err() {
            self.downloads
                .fail(ui_text(ids::APP_ERROR_DOWNLOAD_QUEUE).to_owned());
        }
    }

    /// While a transfer runs, re-reads the list about twice a second so the page shows it
    /// moving. The worker does not push progress.
    fn tick_downloads(&mut self, ctx: &egui::Context) {
        if !self.downloads.has_running() {
            return;
        }
        let due = self
            .downloads
            .last_refresh
            .is_none_or(|last| last.elapsed() >= DOWNLOAD_REFRESH);
        if due {
            self.refresh_downloads();
        }
        ctx.request_repaint_after(DOWNLOAD_REFRESH);
    }

    /// Does nothing while a previous toggle for this item is in flight.
    pub(crate) fn request_watched_toggle(&mut self) {
        let Some(item_id) = self.selected_item_id.clone() else {
            return;
        };
        let Some(details) = self.selected_details.as_ref() else {
            return;
        };
        let mut toggle = match self.watched.as_ref() {
            Some(state) if state.item_id == item_id => state.toggle,
            _ => WatchedToggle::from_user_data(details.user_data.as_ref()),
        };
        let Some(played) = toggle.begin() else {
            return;
        };
        let request_id = self.allocate_request_id();
        let mut error = None;
        let sent = self
            .worker
            .as_ref()
            .ok_or(WorkerSubmitError::Stopped)
            .and_then(|worker| worker.set_played(request_id, item_id.clone(), played));
        if sent.is_err() {
            toggle.fail();
            error = Some(ui_text(ids::APP_ERROR_QUEUE_NETWORK).to_owned());
        }
        self.watched = Some(WatchedItemState {
            item_id,
            request_id,
            toggle,
            error,
        });
    }

    fn watched_request_matches(&self, request_id: u64, item_id: &str) -> bool {
        self.watched
            .as_ref()
            .is_some_and(|state| state.request_id == request_id && state.item_id == item_id)
    }

    pub(crate) fn favorite_view(&self) -> FavoriteView {
        let Some(item) = self.selected_details.as_ref() else {
            return FavoriteView::default();
        };
        match self.favorite.as_ref() {
            Some(state) if self.selected_item_id.as_deref() == Some(state.item_id.as_str()) => {
                FavoriteView {
                    favorite: state.toggle.displayed(),
                    pending: state.toggle.in_flight(),
                    error: state.error.clone(),
                }
            }
            _ => FavoriteView {
                favorite: item
                    .user_data
                    .as_ref()
                    .and_then(|data| data.is_favorite)
                    .unwrap_or(false),
                pending: false,
                error: None,
            },
        }
    }

    pub(crate) fn request_favorite_toggle(&mut self) {
        let Some(item_id) = self.selected_item_id.clone() else {
            return;
        };
        let Some(details) = self.selected_details.as_ref() else {
            return;
        };
        let mut toggle = match self.favorite.as_ref() {
            Some(state) if state.item_id == item_id => state.toggle,
            _ => FavoriteToggle::new(
                details
                    .user_data
                    .as_ref()
                    .and_then(|data| data.is_favorite)
                    .unwrap_or(false),
            ),
        };
        let target = toggle.toggle();
        let mut request_id = self
            .favorite
            .as_ref()
            .filter(|state| state.item_id == item_id)
            .map_or(0, |state| state.request_id);
        let mut error = None;
        if let Some(favorite) = target {
            request_id = self.allocate_request_id();
            let sent = self
                .worker
                .as_ref()
                .ok_or(WorkerSubmitError::Stopped)
                .and_then(|worker| worker.set_favorite(request_id, item_id.clone(), favorite));
            if sent.is_err() {
                toggle.complete_failure();
                error = Some(ui_text(ids::APP_ERROR_QUEUE_NETWORK).to_owned());
            }
        }
        self.favorite = Some(FavoriteItemState {
            item_id,
            request_id,
            toggle,
            error,
        });
    }

    fn favorite_request_matches(&self, request_id: u64, item_id: &str) -> bool {
        self.favorite
            .as_ref()
            .is_some_and(|state| state.request_id == request_id && state.item_id == item_id)
    }

    /// One-click playback. Selects `item_id`, requests its playback options, and starts the
    /// stream from `poll_auto_play` as soon as both the item record and the options have
    /// arrived. `start_over` ignores a saved resume position.
    pub(crate) fn play_item(&mut self, item_id: String, start_over: bool) {
        self.select_item(item_id.clone());
        self.auto_play = Some(AutoPlay {
            item_id,
            start_over,
        });
        if self.playback_info.is_none() && !self.playback_loading {
            self.request_playback_options();
        }
    }

    /// Starts a pending `play_item` once everything it needs is loaded. Drops the request when
    /// the viewer moved to another item or negotiation failed; the details panel shows the error.
    fn poll_auto_play(&mut self) {
        use kattri_client_core::auto_play::{
            AutoPlayDecision, AutoPlayInput, DetailsState, OptionsState, decide,
        };

        let Some(auto) = self.auto_play.clone() else {
            return;
        };
        let options = match self.playback_info.as_ref() {
            None => OptionsState::NotLoaded,
            Some(info) if info.error_code.is_some() || info.media_sources.is_empty() => {
                OptionsState::Unusable
            }
            Some(info) => OptionsState::Ready {
                pick: select_playback_info_source_for_profile(info, self.device_profile()).map(
                    |selection| kattri_client_core::playback_plan::PlaybackSelection {
                        source_index: selection.source_index,
                        method: selection.method,
                    },
                ),
            },
        };
        let details = match self.selected_details.as_ref() {
            None => DetailsState::Missing {
                loading: self.details_loading,
            },
            Some(details) => DetailsState::Loaded {
                resume_ticks: details.user_data.as_ref().and_then(|user_data| {
                    crate::ui::resumable_position_ticks(
                        user_data.playback_position_ticks,
                        user_data.played,
                        details.run_time_ticks,
                    )
                }),
            },
        };
        let decision = decide(&AutoPlayInput {
            item_unchanged: self.selected_item_id.as_deref() == Some(auto.item_id.as_str()),
            options_failed: self.playback_error.is_some(),
            details_failed: self.details_error.is_some(),
            start_in_flight: self.playback_start_loading,
            options,
            details,
            start_over: auto.start_over,
        });
        match decision {
            AutoPlayDecision::Wait => {}
            AutoPlayDecision::Drop => self.auto_play = None,
            AutoPlayDecision::DropNoCompatibleSource => {
                self.auto_play = None;
                self.playback_start_error =
                    Some(ui_text(ids::PLAYBACK_ERROR_NO_COMPATIBLE_SOURCE).into());
            }
            AutoPlayDecision::Start {
                source_index,
                method,
                resume_ticks,
            } => {
                let play_method = match method {
                    PlaybackMethod::DirectPlay => jellyfin_api::PlayMethod::DirectPlay,
                    PlaybackMethod::DirectStream => jellyfin_api::PlayMethod::DirectStream,
                    PlaybackMethod::Transcode => jellyfin_api::PlayMethod::Transcode,
                };
                #[cfg(debug_assertions)]
                if let Some(info) = self.playback_info.as_ref() {
                    let sources: Vec<String> = info
                        .media_sources
                        .iter()
                        .map(|source| {
                            format!(
                                "container={:?} direct_play={} direct_stream={} transcode={} bitrate={:?}",
                                source.container,
                                source.supports_direct_play,
                                source.supports_direct_stream,
                                source.supports_transcoding,
                                source.bitrate,
                            )
                        })
                        .collect();
                    crate::debug_log::log(&format!(
                        "playback plan: chosen source {source_index} via {method:?}; sources: {sources:?}"
                    ));
                }
                self.auto_play = None;
                self.start_server_playback(source_index, play_method, resume_ticks);
            }
        }
    }

    pub(crate) fn request_playback_options(&mut self) {
        let Some(item_id) = self.selected_item_id.clone() else {
            return;
        };
        let request_id = self.allocate_request_id();
        self.playback_request_id = request_id;
        self.playback_info = None;
        self.playback_error = None;
        self.playback_loading = true;
        self.playback_start_request_id = self.allocate_request_id();
        self.playback_start_loading = false;
        self.playback_start_error = None;
        self.playback_start_status = None;
        self.pending_playback = None;
        let is_audio = self
            .selected_details
            .as_ref()
            .is_some_and(|details| details.item_type.as_deref() == Some("Audio"));
        let result = self
            .worker
            .as_ref()
            .ok_or(WorkerSubmitError::Stopped)
            .and_then(|worker| {
                worker.playback_info(
                    request_id,
                    item_id,
                    is_audio,
                    PlaybackInfoRequest::for_mpv_profile(
                        self.device_profile.clone(),
                        self.playback_overrides,
                    ),
                )
            });
        if result.is_err() {
            self.playback_loading = false;
            self.playback_error = Some(ui_text(ids::APP_ERROR_QUEUE_NETWORK).into());
        }
    }

    fn start_automatic_music_track(&mut self) {
        let selection = self
            .playback_info
            .as_ref()
            .and_then(|response| {
                select_playback_info_source_for_profile(response, self.device_profile())
            })
            .map(|selection| (selection.source_index, selection.method));
        let Some((source_index, method)) = selection else {
            self.playback_start_error =
                Some(ui_text(ids::PLAYBACK_ERROR_NO_COMPATIBLE_SOURCE).into());
            if let Some(item_id) = self.music_queue_auto_play_item_id.clone() {
                self.on_music_track_ended(&item_id, true);
            }
            return;
        };
        let play_method = match method {
            PlaybackMethod::DirectPlay => jellyfin_api::PlayMethod::DirectPlay,
            PlaybackMethod::DirectStream => jellyfin_api::PlayMethod::DirectStream,
            PlaybackMethod::Transcode => jellyfin_api::PlayMethod::Transcode,
        };
        let resume_ticks = self.music_queue_resume_position_ticks.take();
        self.start_server_playback(source_index, play_method, resume_ticks);
    }

    pub(crate) fn start_server_playback(
        &mut self,
        source_index: usize,
        play_method: jellyfin_api::PlayMethod,
        start_time_ticks: Option<i64>,
    ) {
        self.playback_recovery = None;
        self.mid_stream_recoveries = 0;
        // Only ordinary movie/episode startup is retried. Music and Live TV
        // retain their existing queue/reconnect policies.
        if let Some(item_id) = self.selected_item_id.clone()
            && self.selected_is_movie_or_episode(&item_id)
            && let Some(info) = self.playback_info.as_ref()
        {
            let chosen = PlaybackSelection {
                source_index,
                method: selection_method(play_method),
            };
            let mut candidates = playback_info_candidates_for_profile(info, self.device_profile());
            if candidates.by_ref().any(|candidate| candidate == chosen) {
                self.playback_recovery = Some(PlaybackRecovery {
                    item_id,
                    options_request_id: self.playback_request_id,
                    start_time_ticks,
                    // Bound automatic retries even for an unusually large response.
                    remaining: candidates.take(32).collect(),
                });
            }
        }
        self.request_episode_neighbors();
        self.prepare_server_playback(source_index, play_method, start_time_ticks);
    }

    /// Asks for the neighbours of the episode about to play, once per episode.
    fn request_episode_neighbors(&mut self) {
        let Some((episode_id, series_id)) = self.selected_details.as_ref().and_then(|item| {
            (item.item_type.as_deref() == Some("Episode"))
                .then(|| {
                    item.series_id
                        .clone()
                        .map(|series| (item.id.clone(), series))
                })
                .flatten()
        }) else {
            return;
        };
        if self
            .episode_neighbors
            .as_ref()
            .is_some_and(|neighbors| neighbors.episode_id == episode_id)
        {
            return;
        }
        let request_id = self.allocate_request_id();
        let submitted = self.worker.as_ref().is_some_and(|worker| {
            worker
                .get_adjacent_episodes(request_id, series_id, episode_id.clone())
                .is_ok()
        });
        self.episode_neighbors = submitted.then_some(EpisodeNeighbors {
            episode_id,
            request_id,
            previous: None,
            next: None,
            next_label: None,
            up_next_cancelled: false,
        });
    }

    fn neighbors_of(&self, playing_item_id: &str) -> Option<&EpisodeNeighbors> {
        self.episode_neighbors
            .as_ref()
            .filter(|neighbors| neighbors.episode_id == playing_item_id)
    }

    fn adjacent(&self, playing_item_id: &str) -> Option<Adjacent<'_>> {
        if let Some(queue) = self.shuffle_queue.as_ref()
            && let Some(index) = queue
                .episodes
                .iter()
                .position(|(id, _)| id == playing_item_id)
        {
            return Some(Adjacent {
                previous: index
                    .checked_sub(1)
                    .map(|previous| queue.episodes[previous].0.as_str()),
                next: queue
                    .episodes
                    .get(index + 1)
                    .map(|(id, label)| (id.as_str(), label.as_str())),
                up_next_cancelled: queue.up_next_cancelled_for.as_deref() == Some(playing_item_id),
            });
        }
        self.neighbors_of(playing_item_id)
            .map(|neighbors| Adjacent {
                previous: neighbors.previous.as_deref(),
                next: neighbors
                    .next
                    .as_deref()
                    .map(|id| (id, neighbors.next_label.as_deref().unwrap_or_default())),
                up_next_cancelled: neighbors.up_next_cancelled,
            })
    }

    /// Whether the player's previous and next buttons have an episode to go to.
    pub fn episode_navigation(&self, playing_item_id: &str) -> (bool, bool) {
        self.adjacent(playing_item_id)
            .map_or((false, false), |adjacent| {
                (adjacent.previous.is_some(), adjacent.next.is_some())
            })
    }

    /// The player's Previous control on an episode. True when the previous episode was started.
    pub fn on_episode_previous(&mut self, playing_item_id: &str) -> bool {
        let Some(previous) = self
            .adjacent(playing_item_id)
            .and_then(|adjacent| adjacent.previous.map(str::to_owned))
        else {
            return false;
        };
        self.play_item(previous, false);
        true
    }

    /// The player's Next control on an episode. True when the next episode was started.
    pub fn on_episode_next(&mut self, playing_item_id: &str) -> bool {
        let Some(next) = self
            .adjacent(playing_item_id)
            .and_then(|adjacent| adjacent.next.map(|(id, _)| id.to_owned()))
        else {
            return false;
        };
        self.play_item(next, false);
        true
    }

    /// Starts a shuffled run of `series_id`'s episodes.
    pub(crate) fn shuffle_series(&mut self, series_id: String) {
        let request_id = self.allocate_request_id();
        if self
            .worker
            .as_ref()
            .is_some_and(|worker| worker.shuffle_series(request_id, series_id).is_ok())
        {
            self.shuffle_request_id = request_id;
        }
    }

    /// An episode played to its end. Starts the next one when the viewer allows it.
    pub fn on_episode_ended(&mut self, ended_item_id: &str) -> bool {
        self.up_next_label(ended_item_id).is_some() && self.on_episode_next(ended_item_id)
    }

    /// The next episode's label while it is set to start when `playing_item_id` ends.
    pub fn up_next_label(&self, playing_item_id: &str) -> Option<&str> {
        if !self.playback_settings().auto_play_next_episode {
            return None;
        }
        self.adjacent(playing_item_id)
            .filter(|adjacent| !adjacent.up_next_cancelled)
            .and_then(|adjacent| adjacent.next.map(|(_, label)| label))
    }

    /// The viewer dismissed the Up Next card: `playing_item_id` ends without auto-play.
    pub fn cancel_up_next(&mut self, playing_item_id: &str) {
        if let Some(queue) = self.shuffle_queue.as_mut()
            && queue.episodes.iter().any(|(id, _)| id == playing_item_id)
        {
            queue.up_next_cancelled_for = Some(playing_item_id.to_owned());
            return;
        }
        if let Some(neighbors) = self
            .episode_neighbors
            .as_mut()
            .filter(|neighbors| neighbors.episode_id == playing_item_id)
        {
            neighbors.up_next_cancelled = true;
        }
    }

    fn retry_playback_start(&mut self) -> bool {
        let Some(recovery) = self.playback_recovery.as_mut() else {
            return false;
        };
        if self.selected_item_id.as_deref() != Some(recovery.item_id.as_str())
            || self.playback_request_id != recovery.options_request_id
        {
            self.playback_recovery = None;
            return false;
        }
        let Some(candidate) = recovery.remaining.pop_front() else {
            self.playback_recovery = None;
            return false;
        };
        let ticks = recovery.start_time_ticks;
        self.prepare_server_playback(
            candidate.source_index,
            api_play_method(candidate.method),
            ticks,
        );
        true
    }

    fn selected_is_movie_or_episode(&self, item_id: &str) -> bool {
        self.selected_details.as_ref().is_some_and(|item| {
            item.id == item_id && matches!(item.item_type.as_deref(), Some("Movie" | "Episode"))
        })
    }

    /// Reopen a movie or episode that failed after it began playing. The same
    /// source is tried first at the last position, because a dropped connection
    /// or a stalled transcode is the usual cause; the other compatible sources
    /// follow through the startup retry path if that reopen fails. Each reopen
    /// gets a new play session from the worker, and the failed session has
    /// already been stopped by the player.
    fn recover_mid_stream(&mut self, position_ticks: Option<i64>) -> bool {
        let Some(active) = self.active_playback.clone() else {
            return false;
        };
        if self.selected_item_id.as_deref() != Some(active.item_id.as_str())
            || self.playback_request_id != active.options_request_id
            || self.playback_start_request_id != active.start_request_id
            || self.mid_stream_recoveries >= MAX_MID_STREAM_RECOVERIES
            || !self.selected_is_movie_or_episode(&active.item_id)
        {
            return false;
        }
        let Some(info) = self.playback_info.as_ref() else {
            return false;
        };
        let remaining = playback_info_candidates_for_profile(info, self.device_profile())
            .filter(|candidate| *candidate != active.selection)
            .take(32)
            .collect();
        self.mid_stream_recoveries += 1;
        self.playback_recovery = Some(PlaybackRecovery {
            item_id: active.item_id,
            options_request_id: active.options_request_id,
            start_time_ticks: position_ticks,
            remaining,
        });
        self.prepare_server_playback(
            active.selection.source_index,
            api_play_method(active.selection.method),
            position_ticks,
        );
        true
    }

    fn prepare_server_playback(
        &mut self,
        source_index: usize,
        play_method: jellyfin_api::PlayMethod,
        start_time_ticks: Option<i64>,
    ) {
        let Some(item_id) = self.selected_item_id.clone() else {
            return;
        };
        let request_id = self.allocate_request_id();
        self.playback_start_request_id = request_id;
        self.active_playback = Some(ActivePlayback {
            item_id: item_id.clone(),
            options_request_id: self.playback_request_id,
            start_request_id: request_id,
            selection: PlaybackSelection {
                source_index,
                method: selection_method(play_method),
            },
        });
        self.playback_start_loading = true;
        self.playback_start_error = None;
        self.playback_start_status = Some(ui_text(ids::PLAYBACK_STATUS_PREPARING_LOCAL).into());
        self.pending_playback = None;
        let result = self
            .worker
            .as_ref()
            .ok_or(WorkerSubmitError::Stopped)
            .and_then(|worker| {
                worker.prepare_playback(
                    request_id,
                    self.playback_request_id,
                    item_id,
                    source_index,
                    play_method,
                    start_time_ticks,
                )
            });
        if result.is_err() {
            self.playback_start_loading = false;
            self.playback_start_status = None;
            self.playback_start_error = Some(ui_text(ids::APP_ERROR_QUEUE_NETWORK).into());
        }
    }

    pub(crate) fn request_visible_artwork(&mut self, requests: Vec<(String, String)>) {
        self.visible_artwork_from_tick = self.next_artwork_access_tick.saturating_add(1);
        // One pass over the items instead of two scans per visible card.
        let mut known: HashMap<(&str, &str), bool> = HashMap::new();
        for item in self.artwork_items() {
            if let Some(tag) = item.primary_image_tag.as_deref() {
                let loaded = known
                    .entry((item.artwork_id.as_str(), tag))
                    .or_insert(false);
                *loaded |= item.artwork_texture.is_some();
            }
        }
        let known: HashMap<(String, String), bool> = known
            .into_iter()
            .map(|((id, tag), loaded)| ((id.to_owned(), tag.to_owned()), loaded))
            .collect();
        let mut pending: HashSet<(String, String)> =
            self.pending_artwork_requests.values().cloned().collect();
        for (item_id, image_tag) in requests {
            let key = (item_id.clone(), image_tag.clone());
            let Some(&already_loaded) = known.get(&key) else {
                continue;
            };

            self.next_artwork_access_tick = self.next_artwork_access_tick.saturating_add(1);
            self.artwork_access_ticks
                .insert(key.clone(), self.next_artwork_access_tick);

            if already_loaded {
                continue;
            }

            if self.pending_artwork_requests.len() >= MAX_PENDING_ARTWORK_REQUESTS {
                break;
            }
            if self.attempted_artwork.contains(&key) || pending.contains(&key) {
                continue;
            }

            self.attempted_artwork.insert(key.clone());
            let request_id = self.allocate_request_id();
            let result = self
                .worker
                .as_ref()
                .ok_or(WorkerSubmitError::Stopped)
                .and_then(|worker| {
                    worker.get_primary_artwork(request_id, item_id, Some(image_tag))
                });
            if result.is_ok() {
                pending.insert(key.clone());
                self.pending_artwork_requests.insert(request_id, key);
            } else {
                self.attempted_artwork.remove(&key);
                break;
            }
        }
        self.trim_artwork_cache();
    }

    fn trim_artwork_cache(&mut self) {
        let mut media: Vec<&mut MediaItem> =
            self.items.iter_mut().chain(self.home.media_mut()).collect();
        trim_artwork_textures(
            &mut media,
            &mut self.artwork_access_ticks,
            &mut self.attempted_artwork,
            self.visible_artwork_from_tick,
        );
    }

    /// Asks for the backdrop of the item the Home hero shows. One attempt per item.
    fn request_home_hero_backdrop(&mut self) {
        if self.view != AppView::Home {
            return;
        }
        let Some(item_id) = self.home.hero_item_id() else {
            return;
        };
        if self
            .home_hero_backdrop
            .as_ref()
            .is_some_and(|(shown, _)| *shown == item_id)
            || self
                .home_hero_request
                .as_ref()
                .is_some_and(|(_, wanted)| *wanted == item_id)
        {
            return;
        }
        let request_id = self.allocate_request_id();
        if self.worker.as_ref().is_some_and(|worker| {
            worker
                .get_backdrop_artwork(request_id, item_id.clone())
                .is_ok()
        }) {
            self.home_hero_request = Some((request_id, item_id));
        }
    }

    fn request_detail_backdrop(&mut self) {
        let Some(item_id) = self.selected_item_id.clone() else {
            return;
        };
        if self.detail_backdrop_attempted {
            return;
        }
        self.detail_backdrop_attempted = true;
        let request_id = self.allocate_request_id();
        self.detail_artwork_request_id = request_id;
        if self
            .worker
            .as_ref()
            .is_none_or(|worker| worker.get_backdrop_artwork(request_id, item_id).is_err())
        {
            self.request_detail_primary_artwork();
        }
    }

    fn request_detail_primary_artwork(&mut self) {
        let Some(item) = self.selected_details.as_ref() else {
            return;
        };
        let tag = item
            .image_tags
            .as_ref()
            .and_then(|tags| tags.get("Primary"))
            .and_then(Option::as_deref)
            .filter(|tag| !tag.is_empty())
            .map(str::to_owned);
        let item_id = item.id.clone();
        let request_id = self.allocate_request_id();
        self.detail_artwork_request_id = request_id;
        let result = self
            .worker
            .as_ref()
            .ok_or(WorkerSubmitError::Stopped)
            .and_then(|worker| worker.get_primary_artwork(request_id, item_id, tag));
        if result.is_err() {
            self.detail_artwork_request_id = 0;
        }
    }

    /// Open the "Open stream address" dialog.
    pub(crate) fn open_stream_address_dialog(&mut self) {
        self.open_url = Some(crate::ui::open_url::OpenUrlState::default());
    }

    /// A checked stream address the viewer asked to play, once. The player host plays it
    /// directly; it is not a Jellyfin item.
    pub fn take_stream_url(&mut self) -> Option<kattri_client_core::stream_url::StreamUrl> {
        self.pending_stream_url.take()
    }

    pub fn take_prepared_playback(&mut self) -> Option<PreparedPlayback> {
        self.pending_playback.take()
    }

    /// Identifies the current prepared playback request for native event correlation.
    pub fn playback_start_generation(&self) -> u64 {
        self.playback_start_request_id
    }

    /// Series GUID of the playing item from its loaded record (M2-10). `None` means the record
    /// has not arrived for this item; `Some(None)` means it arrived and the item has no series.
    fn playing_series_id(&self, item_id: &str) -> Option<Option<String>> {
        self.selected_details
            .as_ref()
            .filter(|details| details.id == item_id)
            .map(|details| details.series_id.clone())
    }

    fn hand_over_playback(&mut self, item_id: String, mut playback: Box<PreparedPlayback>) {
        let series_id = self.playing_series_id(&item_id).flatten();
        playback.set_series_id(series_id);
        if playback.live_stream_id().is_none() {
            self.request_media_segments(&item_id);
        }
        self.pending_playback = Some(*playback);
        self.playback_start_loading = false;
        self.playback_start_error = None;
        self.playback_start_status = Some(ui_text(ids::PLAYBACK_STATUS_OPENING_LOCAL).into());
    }

    /// Hand over a held stream once its item record has arrived or failed (M2-10). A stream
    /// whose start request was superseded, or whose item is no longer selected, is dropped.
    fn release_held_playback(&mut self) {
        let Some((request_id, item_id, playback)) = self.held_playback.take() else {
            return;
        };
        if request_id != self.playback_start_request_id
            || self.selected_item_id.as_deref() != Some(item_id.as_str())
        {
            return;
        }
        self.hand_over_playback(item_id, playback);
    }

    pub fn playback_start_opening(&mut self) {
        self.playback_start_status = Some(ui_text(ids::PLAYBACK_STATUS_OPENING_LOCAL).into());
        if self.music_queue_auto_play_item_id.as_deref() == self.selected_item_id.as_deref() {
            self.music_queue_auto_play_item_id = None;
            self.music_queue_notice = None;
        }
    }

    /// Once this exact request has loaded, failures need position-aware recovery.
    pub fn playback_loaded(&mut self, generation: u64) {
        if self.playback_start_request_id == generation {
            self.playback_recovery = None;
        }
    }

    /// Retry an asynchronous open failure only for the still-current request.
    pub fn playback_open_failed(&mut self, item_id: &str, generation: u64) {
        if self.playback_start_request_id != generation
            || self.selected_item_id.as_deref() != Some(item_id)
        {
            return;
        }
        if !self.retry_playback_start() {
            self.playback_failed(item_id);
        }
    }

    pub fn playback_start_failed(&mut self, message: impl Into<String>) {
        if self.retry_playback_start() {
            return;
        }
        let queue_failed = self.music_queue_auto_play_item_id.clone();
        self.playback_start_loading = false;
        self.playback_start_status = None;
        self.playback_start_error = Some(message.into());
        if let Some(item_id) = queue_failed {
            self.on_music_track_ended(&item_id, true);
        }
    }

    /// A busy player is backpressure, not evidence that another source will work.
    pub fn playback_start_busy(&mut self) {
        self.playback_recovery = None;
        self.playback_start_loading = false;
        self.playback_start_status = None;
        self.playback_start_error =
            Some("A media load is already pending. Try again shortly.".into());
        self.music_queue_auto_play_item_id = None;
    }

    /// A loaded movie or episode failed while playing. Reopen it at the last
    /// position when this is still the current request, otherwise surface it.
    pub fn playback_failed_mid_stream(
        &mut self,
        item_id: &str,
        generation: u64,
        position_ticks: Option<i64>,
    ) {
        if self.playback_start_request_id != generation
            || self.selected_item_id.as_deref() != Some(item_id)
        {
            return;
        }
        if !self.recover_mid_stream(position_ticks) {
            self.playback_failed(item_id);
        }
    }

    /// Surface an asynchronous player failure without restarting a superseded item.
    /// Music keeps its queue policy.
    pub fn playback_failed(&mut self, item_id: &str) {
        if self.music_queue_current_item_id() == Some(item_id) {
            self.on_music_track_ended(item_id, true);
        } else if self.selected_item_id.as_deref() == Some(item_id) {
            self.playback_recovery = None;
            self.playback_start_loading = false;
            self.playback_start_status = None;
            self.playback_start_error =
                Some("Playback failed. Try another source or play again.".into());
        }
    }

    /// Queue a scheduler-produced report through the authenticated API worker.
    /// The returned request ID is local to this app instance and is accepted
    /// only while it remains in the pending-report set.
    pub fn report_playback_action(
        &mut self,
        action: PlaybackReportAction,
    ) -> Result<u64, PlaybackReportSubmitError> {
        self.track_player_report(&action);
        let kind = PlaybackReportKind::from_action(&action);
        if should_suppress_playback_report(kind, self.consecutive_progress_report_failures) {
            return Err(PlaybackReportSubmitError::ProgressRetryBudgetExhausted);
        }
        let request_id = self.allocate_request_id();
        let Some(worker) = self.worker.as_ref() else {
            self.playback_report_feedback = Some(ui_text(ids::CONNECT_WORKER_UNAVAILABLE).into());
            return Err(PlaybackReportSubmitError::WorkerUnavailable);
        };

        if let Err(error) = worker.report_playback_action(request_id, action) {
            self.playback_report_feedback = Some(error.to_string());
            return Err(match error {
                WorkerSubmitError::Busy => PlaybackReportSubmitError::CommandQueueFull,
                WorkerSubmitError::Stopped => PlaybackReportSubmitError::CommandQueueClosed,
            });
        }
        if kind == PlaybackReportKind::Started {
            begin_playback_report_session(
                &mut self.playback_report_session_generation,
                &mut self.consecutive_progress_report_failures,
                &mut self.playback_report_feedback,
            );
        }
        self.pending_playback_report_requests.insert(
            request_id,
            PendingPlaybackReport {
                kind,
                session_generation: self.playback_report_session_generation,
            },
        );
        Ok(request_id)
    }

    /// Apply a signed update whose build is already staged on disk (M4-16).
    ///
    /// The envelope is verified, then the playback gate runs, both on this thread. Only then
    /// does the file work go to the worker. Playback that is open defers the update with no write.
    /// The result arrives later as [`WorkerEvent::UpdateFinished`].
    pub fn apply_update(&mut self, request: ApplyRequest<'_>) {
        if self.update.is_busy() {
            return;
        }
        let job = match prepare(&request) {
            Ok(job) => job,
            Err(status) => {
                self.update.status = status;
                return;
            }
        };
        let version = job.version().to_owned();
        let store_dir = job.store_dir().to_owned();
        let Some(worker) = self.worker.as_ref() else {
            self.update.status = UpdateStatus::Failed {
                version: Some(version),
                reason: ui_text(ids::APP_ERROR_NETWORK_WORKER_UNAVAILABLE).to_owned(),
            };
            return;
        };
        self.update.status = match worker.apply_update(job) {
            Ok(()) => UpdateStatus::Applying { version, store_dir },
            Err(error) => UpdateStatus::Failed {
                version: Some(version),
                reason: error.to_string(),
            },
        };
    }

    /// The player state the update gate needs (M4-16). A session still opening counts as loading.
    pub(crate) fn update_playback_state(&self) -> PlaybackState {
        match self.player_view.player {
            Some(player) if player.paused => PlaybackState::Paused,
            Some(_) => PlaybackState::Playing,
            None if self.playback_start_loading => PlaybackState::Loading,
            None => PlaybackState::Stopped,
        }
    }

    /// Restore the kept previous build. Runs only when the user asks for it, never on its own.
    pub fn rollback_update(&mut self) {
        let UpdateStatus::Applied { version, store_dir } = self.update.status.clone() else {
            return;
        };
        let Some(worker) = self.worker.as_ref() else {
            self.update.status = UpdateStatus::RollbackFailed {
                reason: ui_text(ids::APP_ERROR_NETWORK_WORKER_UNAVAILABLE).to_owned(),
            };
            return;
        };
        self.update.status = match worker.rollback_update(store_dir.clone()) {
            Ok(()) => UpdateStatus::RollingBack { version, store_dir },
            Err(error) => UpdateStatus::RollbackFailed {
                reason: error.to_string(),
            },
        };
    }

    /// Local account/connection epoch; never a server ID, URL or credential.
    /// Native playback must cancel its old report outbox when this changes.
    pub fn connection_generation(&self) -> u64 {
        self.connection_request_id
    }

    /// Latest non-authentication playback-report failure, if any.
    pub fn playback_report_feedback(&self) -> Option<&str> {
        self.playback_report_feedback.as_deref()
    }

    pub(crate) fn disconnect(&mut self) -> bool {
        if let Some(worker) = self.worker.as_ref()
            && let Err(error) = worker.disconnect()
        {
            self.playback_report_feedback = Some(error.to_string());
            return false;
        }
        self.clear_connection();
        true
    }

    fn clear_connection(&mut self) {
        if let Some(worker) = self.worker.as_ref() {
            worker.cancel_reads();
        }
        self.connection_request_id = self.allocate_request_id();
        self.catalog_request_id = self.allocate_request_id();
        self.unsupported_catalog_filters.clear();
        self.details_request_id = self.allocate_request_id();
        self.playback_request_id = self.allocate_request_id();
        self.playback_start_request_id = self.allocate_request_id();
        self.pending_playback_report_requests.clear();
        self.playback_report_session_generation =
            self.playback_report_session_generation.wrapping_add(1);
        self.consecutive_progress_report_failures = 0;
        self.pending_artwork_requests.clear();
        self.attempted_artwork.clear();
        self.artwork_access_ticks.clear();
        self.server = None;
        self.user_name = None;
        self.user_is_admin = false;
        self.items.clear();
        self.total_record_count = None;
        self.start_index = 0;
        self.catalog_next_index = 0;
        self.catalog_end_reached = false;
        self.catalog_loaded = false;
        self.loaded_search = None;
        self.catalog_loading = false;
        self.catalog_error = None;
        self.search_query.clear();
        self.selected_item_id = None;
        self.selected_details = None;
        self.selected_artwork_texture = None;
        self.detail_artwork_request_id = self.allocate_request_id();
        self.detail_backdrop_attempted = false;
        self.details_loading = false;
        self.details_error = None;
        self.playback_info = None;
        self.playback_loading = false;
        self.playback_error = None;
        self.playback_start_loading = false;
        self.playback_start_error = None;
        self.playback_start_status = None;
        self.pending_playback = None;
        self.submitted_search = None;
        self.search_results.clear();
        self.search_stale = false;
        self.search_active = false;
        self.search_debouncer.input("", Duration::ZERO);
        self.view = AppView::Home;
        self.browse_context = None;
        self.home = HomeState::default();
        self.live_tv_channels.clear();
        self.live_tv_channel_logos.clear();
        self.live_tv_logo_queue.clear();
        self.live_tv_logo_pending.clear();
        self.live_tv_logo_attempted.clear();
        self.live_tv_total_count = None;
        self.live_tv_loading = false;
        self.live_tv_loaded = false;
        self.live_tv_not_set_up = false;
        self.live_tv_error = None;
        self.live_tv_notice = None;
        self.live_tv_selected_channel_id = None;
        self.live_tv_request_id = self.allocate_request_id();
        self.live_tv_requested_start_index = 0;
        self.live_tv_programs.clear();
        self.live_tv_guide_window = None;
        self.live_tv_guide_loading = false;
        self.live_tv_guide_available = false;
        self.live_tv_guide_error = None;
        self.live_tv_guide_request_id = self.allocate_request_id();
        self.live_tv_guide_requested_channel_ids.clear();
        self.live_tv_guide_pending_channel_ids.clear();
        self.live_tv_guide_refreshed_at_unix = None;
        self.music_queue = Queue::new();
        self.music_track_names.clear();
        self.music_queue_auto_play_item_id = None;
        self.music_queue_notice = None;
        self.pending_stored_music_queue = match self.settings.load_queue() {
            Some(Ok(StoredLoadOutcome::Loaded(stored))) if !stored.queue.is_empty() => Some(stored),
            _ => None,
        };
        self.music_queue_restore_request_id = self.allocate_request_id();
        self.music_queue_restore_loading = false;
        self.music_queue_resume_position_ms = None;
        self.music_queue_resume_position_ticks = None;
        self.music_queue_last_saved_json = None;
        self.music_queue_persistence_suspended = self.pending_stored_music_queue.is_some();
        self.playlist_entries.clear();
        self.playlist_items_loading = false;
        self.playlist_items_error = None;
        self.opened_playlist_id = None;
        self.playlist_items_request_id = self.allocate_request_id();
        self.playlist_mutation_loading = false;
        self.playlist_mutation_error = None;
        self.playlist_mutation_notice = None;
        self.playlist_mutation_request_id = self.allocate_request_id();
        self.playlist_candidates.clear();
        self.playlist_candidates_loading = false;
        self.playlist_candidates_error = None;
        self.playlist_candidates_request_id = self.allocate_request_id();
        self.music_album_tracks_loading = false;
        self.music_album_tracks_error = None;
        self.music_album_tracks_request_id = self.allocate_request_id();
        self.focus_graph = FocusGraph::default();
        self.sync_play_state = SyncPlayState::NotJoined;
        self.sync_play_pending_join = None;
        self.sync_play_pending_leave = None;
        self.sync_play_ui = crate::ui::syncplay::SyncPlayScreenState::default();
        self.connection = ConnectionStatus::Disconnected { error: None };
    }

    fn receive_worker_events(&mut self, ctx: &egui::Context) {
        loop {
            let event = self.worker.as_ref().and_then(ApiWorker::try_recv);
            let Some(event) = event else {
                break;
            };

            match event {
                WorkerEvent::ServerChecked { request_id, server } => {
                    if let Some((pending_id, server_url, allow_http)) =
                        self.server_check_pending.take()
                    {
                        if pending_id == request_id
                            && server_url == self.server_url.trim()
                            && allow_http == self.allow_insecure_remote_http
                        {
                            self.server_preview = Some((server_url, allow_http, server));
                            self.server_check_error = None;
                            ctx.request_repaint();
                        } else if pending_id != request_id {
                            self.server_check_pending = Some((pending_id, server_url, allow_http));
                        }
                    }
                }
                WorkerEvent::ServerCheckFailed {
                    request_id,
                    message,
                } => {
                    if let Some((pending_id, server_url, allow_http)) =
                        self.server_check_pending.take()
                    {
                        if pending_id == request_id
                            && server_url == self.server_url.trim()
                            && allow_http == self.allow_insecure_remote_http
                        {
                            self.server_preview = None;
                            self.record_error_for_crash_report(&format!(
                                "server check failed: {message}"
                            ));
                            self.server_check_error = Some(message);
                            ctx.request_repaint();
                        } else if pending_id != request_id {
                            self.server_check_pending = Some((pending_id, server_url, allow_http));
                        }
                    }
                }
                WorkerEvent::QuickConnectCode { request_id, code }
                    if self
                        .quick_connect
                        .as_ref()
                        .is_some_and(|flow| flow.request_id() == request_id) =>
                {
                    if let Some(flow) = self.quick_connect.as_mut() {
                        let _ = flow.code_issued(
                            duration_from_egui_time(ctx.input(|input| input.time)),
                            code,
                        );
                    }
                    ctx.request_repaint();
                }
                WorkerEvent::QuickConnectAvailability {
                    request_id,
                    enabled,
                } => {
                    if let Some((pending_id, server_url, allow_http)) =
                        self.quick_connect_probe_pending.take()
                    {
                        if pending_id == request_id {
                            self.quick_connect_enabled_for =
                                Some((server_url, allow_http, enabled));
                            ctx.request_repaint();
                        } else {
                            self.quick_connect_probe_pending =
                                Some((pending_id, server_url, allow_http));
                        }
                    }
                }
                WorkerEvent::QuickConnectFailed {
                    request_id,
                    message,
                } if self
                    .quick_connect
                    .as_ref()
                    .is_some_and(|flow| flow.request_id() == request_id) =>
                {
                    self.record_error_for_crash_report(&format!("quick connect failed: {message}"));
                    if let Some(flow) = self.quick_connect.as_mut() {
                        let _ = flow.initiate_failed();
                    }
                    self.connection = ConnectionStatus::Disconnected {
                        error: Some(message),
                    };
                    self.quick_connect = None;
                    ctx.request_repaint();
                }
                WorkerEvent::QuickConnectPolled {
                    request_id,
                    outcome,
                } if self
                    .quick_connect
                    .as_ref()
                    .is_some_and(|flow| flow.request_id() == request_id) =>
                {
                    let action = self.quick_connect.as_mut().map(|flow| {
                        flow.poll_result(
                            duration_from_egui_time(ctx.input(|input| input.time)),
                            outcome,
                        )
                    });
                    if let Some(action) = action {
                        self.handle_quick_connect_action(request_id, action);
                    }
                    ctx.request_repaint();
                }
                WorkerEvent::CheckingServer { request_id }
                    if request_id == self.connection_request_id =>
                {
                    self.connection = ConnectionStatus::Connecting {
                        message: ui_text(ids::CONNECT_CHECKING_SERVER).into(),
                    };
                }
                WorkerEvent::SavedSignInAvailability { available } => {
                    self.saved_sign_in_available = Some(available);
                    if !available {
                        self.remember_sign_in = false;
                    }
                }
                WorkerEvent::ResumeUnavailable {
                    request_id,
                    expired,
                } if request_id == self.connection_request_id => {
                    // No saved sign-in, or the server refused it. Back to the form, which
                    // keeps the server address; only an expired token says anything.
                    self.clear_connection();
                    self.remember_sign_in = false;
                    self.connection = ConnectionStatus::Disconnected {
                        error: expired.then(|| ui_text(ids::CONNECT_SAVED_SIGN_IN_EXPIRED).into()),
                    };
                    self.server_address_changed(Duration::ZERO);
                }
                WorkerEvent::Authenticating { request_id, server }
                    if request_id == self.connection_request_id =>
                {
                    self.connection = ConnectionStatus::Connecting {
                        message: ui_text_args(
                            ids::APP_STATUS_SIGNING_IN,
                            &[("server", server.name.as_str())],
                        ),
                    };
                    self.server = Some(server);
                }
                WorkerEvent::CatalogLoading {
                    request_id,
                    server,
                    user,
                } if catalog_response_is_current(request_id, self.catalog_request_id) => {
                    let signed_in = request_id == self.connection_request_id;
                    if signed_in {
                        let _ = self.saved_servers.remember(
                            &server.name,
                            self.server_url.trim(),
                            self.allow_insecure_remote_http,
                        );
                        self.quick_connect = None;
                    }
                    self.server = Some(server);
                    self.user_is_admin = user.is_administrator();
                    self.user_name = Some(user.name);
                    self.catalog_loading = true;
                    if !self.catalog_loaded {
                        self.connection = ConnectionStatus::LoadingItems;
                    }
                    if signed_in {
                        // Home loads once per successful sign-in; Refresh reloads it later.
                        self.refresh_home();
                        self.request_saved_music_queue();
                    }
                }
                WorkerEvent::DashboardLoaded { request_id, load }
                    if self.admin_request_id == Some(request_id) =>
                {
                    self.admin_request_id = None;
                    self.admin_load = load;
                }
                WorkerEvent::HomeRowLoaded {
                    request_id,
                    row,
                    items,
                } if self.home.is_pending(row, request_id) => {
                    self.record_network_success();
                    let ceiling = self.browse_rating_ceiling(ctx);
                    let cards = items
                        .into_iter()
                        .map(crate::ui::home::card_from_item)
                        .collect();
                    let cards = crate::ui::home::cards_visible_under(cards, ceiling);
                    self.home.apply_items(row, request_id, cards);
                }
                WorkerEvent::HomeRowFailed {
                    request_id,
                    row,
                    error_class,
                    retry_after,
                } if self.home.is_pending(row, request_id) => {
                    self.record_network_error(error_class, retry_after);
                    self.home.apply_failure(
                        row,
                        request_id,
                        crate::ui::status::message(error_class).to_owned(),
                    );
                }
                WorkerEvent::HomeCatalogLoaded { request_id, data }
                    if self.home.is_catalog_pending(data.kind(), request_id) =>
                {
                    self.record_network_success();
                    match data {
                        HomeCatalogData::Libraries(views) => {
                            self.home.apply_libraries(request_id, views);
                        }
                        HomeCatalogData::Collections(items) => {
                            let ceiling = self.browse_rating_ceiling(ctx);
                            let cards = items
                                .into_iter()
                                .map(crate::ui::home::card_from_item)
                                .collect();
                            let cards = crate::ui::home::cards_visible_under(cards, ceiling);
                            self.home.apply_collections(request_id, cards);
                        }
                    }
                }
                WorkerEvent::HomeCatalogFailed {
                    request_id,
                    kind,
                    error_class,
                    retry_after,
                } if self.home.is_catalog_pending(kind, request_id) => {
                    self.record_network_error(error_class, retry_after);
                    self.home.apply_catalog_failure(
                        kind,
                        request_id,
                        crate::ui::status::message(error_class).to_owned(),
                    );
                }
                WorkerEvent::LiveTvChannelsLoaded { request_id, page }
                    if self.live_tv_request_id == request_id =>
                {
                    self.record_network_success();
                    let logo_channels = page.items.clone();
                    let channel_ids = page
                        .items
                        .iter()
                        .map(|channel| channel.item.id.clone())
                        .collect::<Vec<_>>();
                    self.live_tv_loading = false;
                    self.live_tv_loaded = true;
                    if self.live_tv_requested_start_index == 0 {
                        self.live_tv_channels = page.items;
                        self.live_tv_not_set_up = self.live_tv_channels.is_empty();
                    } else {
                        let known = self
                            .live_tv_channels
                            .iter()
                            .map(|channel| channel.item.id.clone())
                            .collect::<HashSet<_>>();
                        self.live_tv_channels.extend(
                            page.items
                                .into_iter()
                                .filter(|channel| !known.contains(&channel.item.id)),
                        );
                    }
                    self.live_tv_total_count = page.total_record_count.or(self.live_tv_total_count);
                    self.live_tv_error = None;
                    self.enqueue_live_tv_channel_logos(&logo_channels);
                    if !self.live_tv_not_set_up && !channel_ids.is_empty() {
                        self.request_live_tv_programs(channel_ids, unix_now_seconds());
                    }
                }
                WorkerEvent::LiveTvChannelsFailed {
                    request_id,
                    error_class,
                } if self.live_tv_request_id == request_id => {
                    self.record_network_error(error_class, None);
                    self.live_tv_loading = false;
                    self.live_tv_loaded = true;
                    self.live_tv_not_set_up = error_class == ErrorClass::NotFound;
                    self.live_tv_error = (!self.live_tv_not_set_up).then_some(error_class);
                }
                WorkerEvent::LiveTvSearchResultsLoaded {
                    request_id,
                    items,
                    start_index,
                    total_record_count,
                } if self.live_tv_search_request_id == Some(request_id)
                    && self.live_tv_search_loading =>
                {
                    self.live_tv_search_loading = false;
                    let page_count = items.len() as u32;
                    if start_index == 0 {
                        self.live_tv_search_results = items;
                    } else {
                        let mut known_ids = self
                            .live_tv_search_results
                            .iter()
                            .map(|item| item.id.clone())
                            .collect::<HashSet<_>>();
                        self.live_tv_search_results.extend(
                            items
                                .into_iter()
                                .filter(|item| known_ids.insert(item.id.clone())),
                        );
                    }
                    self.live_tv_search_start_index = start_index.saturating_add(page_count);
                    self.live_tv_search_total = total_record_count;
                }
                WorkerEvent::LiveTvSearchResultsFailed { request_id }
                    if self.live_tv_search_request_id == Some(request_id) =>
                {
                    self.live_tv_search_loading = false;
                    self.live_tv_search_results.clear();
                }
                WorkerEvent::LiveTvLogoLoaded {
                    request_id,
                    channel_id,
                    width,
                    height,
                    rgba,
                } => {
                    if self.live_tv_logo_pending.remove(&request_id).is_some_and(
                        |(expected_id, expected_tag)| {
                            expected_id == channel_id
                                && self.live_tv_channels.iter().any(|channel| {
                                    channel.item.id == channel_id
                                        && channel
                                            .item
                                            .image_tags
                                            .as_ref()
                                            .and_then(|tags| tags.get("Primary"))
                                            .and_then(Option::as_deref)
                                            == Some(expected_tag.as_str())
                                })
                        },
                    ) {
                        if self.live_tv_channel_logos.len() >= MAX_LIVE_TV_LOGO_TEXTURES
                            && let Some(evicted) = self.live_tv_channel_logos.keys().next().cloned()
                        {
                            self.live_tv_channel_logos.remove(&evicted);
                        }
                        let color_image = egui::ColorImage::from_rgba_unmultiplied(
                            [width as usize, height as usize],
                            &rgba,
                        );
                        self.live_tv_channel_logos.insert(
                            channel_id.clone(),
                            ctx.load_texture(
                                format!("jellyfin-live-tv-logo:{channel_id}"),
                                color_image,
                                egui::TextureOptions::LINEAR,
                            ),
                        );
                    }
                    self.pump_live_tv_logo_requests();
                }
                WorkerEvent::LiveTvLogoFailed {
                    request_id,
                    channel_id,
                } => {
                    if self
                        .live_tv_logo_pending
                        .remove(&request_id)
                        .is_some_and(|(expected_id, _)| expected_id == channel_id)
                    {
                        self.pump_live_tv_logo_requests();
                    }
                }
                WorkerEvent::LiveTvProgramsLoaded { request_id, page }
                    if self.live_tv_guide_request_id == request_id =>
                {
                    self.record_network_success();
                    let requested = self
                        .live_tv_guide_requested_channel_ids
                        .iter()
                        .cloned()
                        .collect::<HashSet<_>>();
                    self.live_tv_programs
                        .retain(|program| !requested.contains(&program.channel_id));
                    self.live_tv_programs
                        .extend(page.items.into_iter().filter_map(|program| {
                            let channel_id = program.channel_id?;
                            let start_unix = kattri_client_core::epg::parse_rfc3339_unix_seconds(
                                program.start_date.as_deref()?,
                            )?;
                            let end_unix = program
                                .end_date
                                .as_deref()
                                .and_then(kattri_client_core::epg::parse_rfc3339_unix_seconds);
                            Some(EpgProgram {
                                channel_id,
                                title: program.item.name,
                                start_unix,
                                end_unix,
                            })
                        }));
                    self.live_tv_guide_loading = false;
                    self.live_tv_guide_available = true;
                    self.live_tv_guide_error = None;
                    self.live_tv_guide_refreshed_at_unix = Some(unix_now_seconds());
                    let pending = std::mem::take(&mut self.live_tv_guide_pending_channel_ids);
                    if !pending.is_empty() {
                        self.request_live_tv_programs(pending, unix_now_seconds());
                    }
                }
                WorkerEvent::LiveTvProgramsFailed {
                    request_id,
                    error_class,
                } if self.live_tv_guide_request_id == request_id => {
                    self.record_network_error(error_class, None);
                    self.live_tv_guide_loading = false;
                    self.live_tv_guide_error = Some(error_class);
                    let pending = std::mem::take(&mut self.live_tv_guide_pending_channel_ids);
                    if !pending.is_empty() {
                        self.request_live_tv_programs(pending, unix_now_seconds());
                    }
                }
                WorkerEvent::ItemsLoaded {
                    request_id,
                    server,
                    user,
                    page,
                } => {
                    self.record_network_success();
                    let ceiling = self.browse_rating_ceiling(ctx);
                    if self.apply_catalog_response(request_id, page, ceiling) {
                        self.server = Some(server);
                        self.user_is_admin = user.is_administrator();
                        self.user_name = Some(user.name);
                    }
                }
                WorkerEvent::ItemsFailed {
                    request_id,
                    unsupported_filter,
                    error_class,
                    retry_after,
                } if catalog_response_is_current(request_id, self.catalog_request_id) => {
                    self.catalog_loading = false;
                    self.search_stale =
                        self.submitted_search.is_some() && !self.search_results.is_empty();
                    if let Some(filter) = unsupported_filter {
                        self.apply_unsupported_catalog_filter(filter);
                    } else {
                        self.record_network_error(error_class, retry_after);
                        let safe_message = crate::ui::status::message(error_class).to_owned();
                        self.record_error_for_crash_report(&format!(
                            "catalog load failed: {error_class:?}"
                        ));
                        self.catalog_error = Some(safe_message.clone());
                        self.connection = if self.catalog_loaded {
                            ConnectionStatus::Connected
                        } else {
                            ConnectionStatus::LibraryError(safe_message)
                        };
                    }
                }
                WorkerEvent::ItemDetailsLoaded { request_id, item }
                    if request_id == self.details_request_id
                        && self.selected_item_id.as_deref() == Some(item.id.as_str()) =>
                {
                    self.record_network_success();
                    self.selected_details = Some(*item);
                    self.details_loading = false;
                    self.details_error = None;
                    self.request_detail_backdrop();
                    self.request_similar_items();
                    self.queue_person_artwork();
                    if self
                        .selected_details
                        .as_ref()
                        .is_some_and(|item| item.item_type.as_deref() == Some("Playlist"))
                        && let Some(playlist_id) = self.selected_item_id.clone()
                    {
                        self.request_playlist_items(playlist_id);
                    }
                    if self
                        .selected_details
                        .as_ref()
                        .is_some_and(|item| item.item_type.as_deref() == Some("Series"))
                        && let Some(series_id) = self.selected_item_id.clone()
                    {
                        self.request_series_seasons(series_id);
                    }
                    if self.music_queue_auto_play_item_id.as_deref()
                        == self.selected_item_id.as_deref()
                    {
                        self.request_playback_options();
                    }
                    self.release_held_playback();
                }
                WorkerEvent::ItemDetailsFailed {
                    request_id,
                    item_id,
                    message,
                    error_class,
                    retry_after,
                } if request_id == self.details_request_id
                    && self.selected_item_id.as_deref() == Some(item_id.as_str()) =>
                {
                    self.record_network_error(error_class, retry_after);
                    self.details_loading = false;
                    self.details_error = Some(if error_class == ErrorClass::NotFound {
                        item_details_error_message(&message)
                    } else {
                        crate::ui::status::message(error_class).to_owned()
                    });
                    // A failed record leaves no series, so the held stream goes out without one.
                    self.release_held_playback();
                }
                WorkerEvent::SeriesSeasonsLoaded {
                    request_id,
                    series_id,
                    seasons,
                } if request_id == self.series_seasons_request_id
                    && self.selected_item_id.as_deref() == Some(series_id.as_str()) =>
                {
                    self.series_seasons_loading = false;
                    self.series_seasons_error = None;
                    self.series_seasons = seasons;
                    if let Some(season_id) = ordered_season_ids(&self.series_seasons).first() {
                        self.select_series_season(season_id.clone());
                    }
                }
                WorkerEvent::SeriesSeasonsFailed {
                    request_id,
                    series_id,
                    message,
                } if request_id == self.series_seasons_request_id
                    && self.selected_item_id.as_deref() == Some(series_id.as_str()) =>
                {
                    self.series_seasons_loading = false;
                    self.series_seasons_error = Some(message);
                }
                WorkerEvent::SeriesEpisodesLoaded {
                    request_id,
                    season_id,
                    episodes,
                } if request_id == self.series_episodes_request_id
                    && self.series_selected_season_id.as_deref() == Some(season_id.as_str()) =>
                {
                    self.series_episodes_loading = false;
                    self.series_episodes_error = None;
                    self.series_episodes = episodes;
                    self.request_episode_thumbnails();
                }
                WorkerEvent::SeriesEpisodesFailed {
                    request_id,
                    season_id,
                    message,
                } if request_id == self.series_episodes_request_id
                    && self.series_selected_season_id.as_deref() == Some(season_id.as_str()) =>
                {
                    self.series_episodes_loading = false;
                    self.series_episodes_error = Some(message);
                }
                WorkerEvent::ItemAdminFinished {
                    request_id,
                    item_id,
                    action,
                    result,
                } => {
                    if let Some(title) = self.admin_requests.remove(&request_id) {
                        let args = [("title", title.as_str())];
                        let text = match (action, &result) {
                            (crate::worker::ItemAdminAction::RefreshMetadata, Ok(())) => {
                                ui_text_args(ids::TOAST_REFRESH_QUEUED, &args)
                            }
                            (crate::worker::ItemAdminAction::Delete, Ok(())) => {
                                ui_text_args(ids::TOAST_DELETED, &args)
                            }
                            (crate::worker::ItemAdminAction::AddToCollection, Ok(())) => {
                                ui_text_args(ids::TOAST_ADDED_TO_COLLECTION, &args)
                            }
                            (_, Err(_)) => ui_text_args(ids::TOAST_ADMIN_FAILED, &args),
                        };
                        self.show_toast(text);
                        if action == crate::worker::ItemAdminAction::Delete && result.is_ok() {
                            self.forget_deleted_item(&item_id);
                        }
                    }
                }
                WorkerEvent::GenresLoaded { request_id, genres } => {
                    if request_id == self.catalog_genres_request_id {
                        self.catalog_genres = genres;
                    }
                }
                WorkerEvent::ShuffleLoaded {
                    request_id,
                    episodes,
                } => {
                    if request_id == self.shuffle_request_id && !episodes.is_empty() {
                        let episodes: Vec<(String, String)> = episodes
                            .iter()
                            .map(|episode| {
                                (
                                    episode.id.clone(),
                                    crate::ui::episode_display_label(episode),
                                )
                            })
                            .collect();
                        let first = episodes[0].0.clone();
                        self.shuffle_queue = Some(ShuffleQueue {
                            episodes,
                            up_next_cancelled_for: None,
                        });
                        self.play_item(first, true);
                    }
                }
                WorkerEvent::MenuCollectionsLoaded {
                    request_id,
                    collections,
                } => {
                    if request_id == self.menu_collections_request_id {
                        self.menu_collections = std::sync::Arc::new(collections);
                    }
                }
                WorkerEvent::UserPlaylistsLoaded {
                    request_id,
                    playlists,
                } => {
                    if request_id == self.menu_playlists_request_id {
                        self.menu_playlists = std::sync::Arc::new(playlists);
                    }
                }
                WorkerEvent::PlaylistMutationCompleted { request_id, .. }
                | WorkerEvent::PlaylistMutationFailed { request_id, .. }
                    if self
                        .menu_playlist_add
                        .as_ref()
                        .is_some_and(|(pending, _, _)| *pending == request_id) =>
                {
                    let added = matches!(event, WorkerEvent::PlaylistMutationCompleted { .. });
                    if let Some((_, title, playlist)) = self.menu_playlist_add.take() {
                        let args = [("title", title.as_str()), ("playlist", playlist.as_str())];
                        self.show_toast(if added {
                            ui_text_args(ids::TOAST_ADDED_TO_PLAYLIST, &args)
                        } else {
                            ui_text_args(ids::TOAST_ADD_TO_PLAYLIST_FAILED, &args)
                        });
                    }
                    if let WorkerEvent::PlaylistMutationCompleted { playlist_id, .. } = &event
                        && self.opened_playlist_id.as_deref() == Some(playlist_id.as_str())
                    {
                        self.request_playlist_items(playlist_id.clone());
                    }
                }
                WorkerEvent::SeriesNextUpLoaded {
                    request_id,
                    series_id,
                    episode,
                } => {
                    if let Some(next_up) = self.series_next_up.as_mut()
                        && next_up.request_id == request_id
                        && next_up.series_id == series_id
                    {
                        next_up.episode = episode.map(|episode| *episode);
                    }
                }
                WorkerEvent::AdjacentEpisodesLoaded {
                    request_id,
                    episode_id,
                    previous,
                    next,
                } => {
                    if let Some(neighbors) = self.episode_neighbors.as_mut()
                        && neighbors.request_id == request_id
                        && neighbors.episode_id == episode_id
                    {
                        neighbors.previous = previous.map(|item| item.id);
                        neighbors.next_label =
                            next.as_deref().map(crate::ui::episode_display_label);
                        neighbors.next = next.map(|item| item.id);
                    }
                }
                WorkerEvent::MusicAlbumTracksLoaded {
                    request_id,
                    album_id,
                    tracks,
                } if request_id == self.music_album_tracks_request_id
                    && self.selected_item_id.as_deref() == Some(album_id.as_str()) =>
                {
                    self.record_network_success();
                    self.music_album_tracks_loading = false;
                    let mut names = HashMap::new();
                    let queue_items = tracks
                        .into_iter()
                        .filter(|track| track.item_type.as_deref() == Some("Audio"))
                        .map(|track| {
                            names.insert(track.id.clone(), track.name);
                            QueueItem::new(track.id)
                        })
                        .collect::<Vec<_>>();
                    let first_id = queue_items.first().map(|item| item.id.clone());
                    if queue_items.is_empty()
                        || self.music_queue.add_album_and_play(queue_items).is_err()
                    {
                        self.music_album_tracks_error = Some(ErrorClass::Unexpected);
                    } else if let Some(first_id) = first_id {
                        self.music_track_names.extend(names);
                        self.music_album_tracks_error = None;
                        self.music_queue_position_ms = 0;
                        self.music_queue_resume_position_ms = None;
                        self.music_queue_resume_position_ticks = None;
                        self.select_item(first_id);
                    }
                }
                WorkerEvent::MusicAlbumTracksFailed {
                    request_id,
                    album_id,
                    error_class,
                } if request_id == self.music_album_tracks_request_id
                    && self.selected_item_id.as_deref() == Some(album_id.as_str()) =>
                {
                    self.record_network_error(error_class, None);
                    self.music_album_tracks_loading = false;
                    self.music_album_tracks_error = Some(error_class);
                }
                WorkerEvent::QueueItemsRestored { request_id, items }
                    if request_id == self.music_queue_restore_request_id
                        && self.pending_stored_music_queue.is_some() =>
                {
                    self.record_network_success();
                    self.reconcile_restored_music_queue(items);
                }
                WorkerEvent::QueueItemsRestoreFailed {
                    request_id,
                    error_class,
                } if request_id == self.music_queue_restore_request_id => {
                    self.record_network_error(error_class, None);
                    self.music_queue_restore_loading = false;
                    self.music_queue_notice =
                        Some(ui_text(ids::MUSIC_QUEUE_REFRESH_RECONNECT).into());
                }
                WorkerEvent::PlaylistItemsLoaded {
                    request_id,
                    playlist_id,
                    entries,
                } if request_id == self.playlist_items_request_id
                    && self.opened_playlist_id.as_deref() == Some(playlist_id.as_str()) =>
                {
                    self.record_network_success();
                    self.playlist_items_loading = false;
                    self.playlist_items_error = None;
                    self.playlist_entries = entries;
                }
                WorkerEvent::PlaylistItemsFailed {
                    request_id,
                    playlist_id,
                    error_class,
                } if request_id == self.playlist_items_request_id
                    && self.opened_playlist_id.as_deref() == Some(playlist_id.as_str()) =>
                {
                    self.record_network_error(error_class, None);
                    self.playlist_items_loading = false;
                    self.playlist_items_error = Some(error_class);
                }
                WorkerEvent::PlaylistCandidatesLoaded { request_id, items }
                    if request_id == self.playlist_candidates_request_id
                        && self.page == Page::Playlists =>
                {
                    self.record_network_success();
                    self.playlist_candidates_loading = false;
                    self.playlist_candidates_error = None;
                    self.playlist_candidates = items;
                }
                WorkerEvent::PlaylistCandidatesFailed {
                    request_id,
                    error_class,
                } if request_id == self.playlist_candidates_request_id
                    && self.page == Page::Playlists =>
                {
                    self.record_network_error(error_class, None);
                    self.playlist_candidates_loading = false;
                    self.playlist_candidates_error = Some(error_class);
                }
                WorkerEvent::PlaylistCreated {
                    request_id,
                    playlist_id,
                    name,
                } if request_id == self.playlist_mutation_request_id => {
                    self.record_network_success();
                    self.playlist_mutation_loading = false;
                    self.playlist_mutation_error = None;
                    if self.playlist_ui_state.create_name.trim() == name {
                        self.playlist_ui_state.create_name.clear();
                    }
                    self.playlist_mutation_notice =
                        Some(ui_text(ids::PLAYLISTS_NOTICE_CREATED).into());
                    self.request_menu_playlists();
                    self.request_catalog(0, None);
                    self.select_item(playlist_id);
                }
                WorkerEvent::PlaylistCreateFailed {
                    request_id,
                    error_class,
                } if request_id == self.playlist_mutation_request_id => {
                    self.record_network_error(error_class, None);
                    self.playlist_mutation_loading = false;
                    self.playlist_mutation_error = Some(error_class);
                    self.playlist_mutation_notice =
                        Some(ui_text(ids::PLAYLISTS_ERROR_CREATE_FAILED).into());
                }
                WorkerEvent::PlaylistMutationCompleted {
                    request_id,
                    playlist_id,
                    kind,
                } if request_id == self.playlist_mutation_request_id
                    && self.opened_playlist_id.as_deref() == Some(playlist_id.as_str()) =>
                {
                    self.record_network_success();
                    self.playlist_mutation_loading = false;
                    self.playlist_mutation_error = None;
                    self.playlist_mutation_notice = Some(
                        match kind {
                            PlaylistMutationKind::Added => ui_text(ids::PLAYLISTS_NOTICE_ADDED),
                            PlaylistMutationKind::Removed => ui_text(ids::PLAYLISTS_NOTICE_REMOVED),
                            PlaylistMutationKind::Moved => ui_text(ids::PLAYLISTS_NOTICE_MOVED),
                        }
                        .into(),
                    );
                    self.playlist_ui_state.selected_media_ids.clear();
                    self.request_playlist_items(playlist_id);
                }
                WorkerEvent::PlaylistMutationFailed {
                    request_id,
                    playlist_id,
                    kind,
                    error_class,
                } if request_id == self.playlist_mutation_request_id
                    && self.opened_playlist_id.as_deref() == Some(playlist_id.as_str()) =>
                {
                    self.record_network_error(error_class, None);
                    self.playlist_mutation_loading = false;
                    self.playlist_mutation_error = Some(error_class);
                    self.playlist_mutation_notice = Some(
                        match kind {
                            PlaylistMutationKind::Added => ui_text(ids::PLAYLISTS_ERROR_ADD_FAILED),
                            PlaylistMutationKind::Removed => {
                                ui_text(ids::PLAYLISTS_ERROR_REMOVE_FAILED)
                            }
                            PlaylistMutationKind::Moved => {
                                ui_text(ids::PLAYLISTS_ERROR_REORDER_FAILED)
                            }
                        }
                        .into(),
                    );
                }
                WorkerEvent::WatchedStateUpdated {
                    request_id,
                    item_id,
                    played,
                    user_data,
                } if self.watched_request_matches(request_id, &item_id) => {
                    if let Some(state) = self.watched.as_mut() {
                        state.toggle.succeed(played);
                        state.error = None;
                    }
                    if self.selected_item_id.as_deref() == Some(item_id.as_str())
                        && let Some(details) = self.selected_details.as_mut()
                    {
                        details.user_data = Some(*user_data);
                    }
                }
                WorkerEvent::DownloadsUpdated { rows, .. } => self.downloads.apply_rows(rows),
                WorkerEvent::DownloadFailed { message, .. } => self.downloads.fail(message),
                WorkerEvent::UpdateFinished { outcome } => self.update.finish(outcome),
                WorkerEvent::WatchedStateFailed {
                    request_id,
                    item_id,
                    message,
                } if self.watched_request_matches(request_id, &item_id) => {
                    if let Some(state) = self.watched.as_mut() {
                        state.toggle.fail();
                        state.error = Some(watched_error_message(&message));
                    }
                }
                WorkerEvent::FavoriteStateUpdated {
                    request_id,
                    item_id,
                    favorite,
                    user_data,
                } if self.favorite_request_matches(request_id, &item_id) => {
                    let queued_target = self
                        .favorite
                        .as_mut()
                        .and_then(|state| state.toggle.complete_success(favorite));
                    if self.selected_item_id.as_deref() == Some(item_id.as_str())
                        && let Some(details) = self.selected_details.as_mut()
                    {
                        details.user_data = Some(*user_data);
                    }
                    if let Some(target) = queued_target {
                        let next_request_id = self.allocate_request_id();
                        let sent = self
                            .worker
                            .as_ref()
                            .ok_or(WorkerSubmitError::Stopped)
                            .and_then(|worker| {
                                worker.set_favorite(next_request_id, item_id.clone(), target)
                            });
                        if let Some(state) = self.favorite.as_mut() {
                            state.request_id = next_request_id;
                            state.error = sent
                                .err()
                                .map(|_| ui_text(ids::APP_ERROR_QUEUE_NETWORK).into());
                            if state.error.is_some() {
                                state.toggle.complete_failure();
                            }
                        }
                    } else if let Some(state) = self.favorite.as_mut() {
                        state.error = None;
                    }
                }
                WorkerEvent::FavoriteStateFailed {
                    request_id,
                    item_id,
                    message,
                } if self.favorite_request_matches(request_id, &item_id) => {
                    if let Some(state) = self.favorite.as_mut() {
                        state.toggle.complete_failure();
                        state.error = Some(favorite_error_message(&message));
                    }
                }
                WorkerEvent::SimilarItemsLoaded {
                    request_id,
                    item_id,
                    items,
                } if request_id == self.similar_request_id
                    && self.selected_item_id.as_deref() == Some(item_id.as_str()) =>
                {
                    self.similar_loading = false;
                    self.similar_items = remove_current_item(&item_id, items);
                }
                WorkerEvent::SimilarItemsFailed {
                    request_id,
                    item_id,
                } if request_id == self.similar_request_id
                    && self.selected_item_id.as_deref() == Some(item_id.as_str()) =>
                {
                    self.similar_loading = false;
                    self.similar_items.clear();
                }
                WorkerEvent::PersonItemsLoaded {
                    request_id,
                    person_id,
                    items,
                } if request_id == self.person_items_request_id
                    && self
                        .selected_person
                        .as_ref()
                        .and_then(|person| person.id.as_deref())
                        == Some(person_id.as_str()) =>
                {
                    self.person_items_loading = false;
                    self.person_items_error = None;
                    self.person_items = items;
                }
                WorkerEvent::PersonItemsFailed {
                    request_id,
                    person_id,
                    message,
                } if request_id == self.person_items_request_id
                    && self
                        .selected_person
                        .as_ref()
                        .and_then(|person| person.id.as_deref())
                        == Some(person_id.as_str()) =>
                {
                    self.person_items_loading = false;
                    self.person_items_error = Some(message);
                }
                WorkerEvent::PersonImageLoaded {
                    request_id,
                    name,
                    width,
                    height,
                    rgba,
                } => {
                    let expected_name = self.pending_person_artwork.remove(&request_id);
                    if expected_name.as_deref() == Some(name.as_str()) {
                        let color_image = egui::ColorImage::from_rgba_unmultiplied(
                            [width as usize, height as usize],
                            &rgba,
                        );
                        self.person_artwork_textures.insert(
                            name.clone(),
                            ctx.load_texture(
                                format!("jellyfin-person:{name}"),
                                color_image,
                                egui::TextureOptions::LINEAR,
                            ),
                        );
                    }
                    if expected_name.is_some() {
                        self.pump_person_artwork_requests();
                    }
                }
                WorkerEvent::PersonImageFailed { request_id } => {
                    if self.pending_person_artwork.remove(&request_id).is_some() {
                        self.pump_person_artwork_requests();
                    }
                }
                WorkerEvent::PhotoThumbnailLoaded {
                    request_id,
                    item_id,
                    width,
                    height,
                    rgba,
                } => {
                    if self.photo_thumbnail_pending.remove(&request_id).as_deref()
                        == Some(item_id.as_str())
                        && self.photo_items.iter().any(|item| item.id == item_id)
                    {
                        let image = egui::ColorImage::from_rgba_unmultiplied(
                            [width as usize, height as usize],
                            &rgba,
                        );
                        self.photo_thumbnails.insert(
                            item_id.clone(),
                            ctx.load_texture(
                                format!("jellyfin-photo-thumb:{item_id}"),
                                image,
                                egui::TextureOptions::LINEAR,
                            ),
                        );
                        evict_photo_thumbnails(
                            &mut self.photo_thumbnails,
                            &mut self.photo_thumbnail_attempted,
                            &self.photo_visible_ids,
                            &item_id,
                        );
                    }
                }
                WorkerEvent::PhotoThumbnailFailed {
                    request_id,
                    item_id,
                } => {
                    if self
                        .photo_thumbnail_pending
                        .get(&request_id)
                        .is_some_and(|expected| expected == &item_id)
                    {
                        self.photo_thumbnail_pending.remove(&request_id);
                    }
                }
                WorkerEvent::PhotoFullSizeLoaded {
                    request_id,
                    item_id,
                    width,
                    height,
                    rgba,
                } => {
                    let selected = self.photo_selected_id.as_deref() == Some(item_id.as_str());
                    if self.photo_full_size_pending.remove(&request_id).as_deref()
                        == Some(item_id.as_str())
                        && self.photo_items.iter().any(|item| item.id == item_id)
                    {
                        let image = egui::ColorImage::from_rgba_unmultiplied(
                            [width as usize, height as usize],
                            &rgba,
                        );
                        self.photo_full_size_images.insert(
                            item_id.clone(),
                            ctx.load_texture(
                                format!("jellyfin-photo-full:{item_id}"),
                                image,
                                egui::TextureOptions::LINEAR,
                            ),
                        );
                        self.photo_full_size_failed.remove(&item_id);
                        while self.photo_full_size_images.len() > 3 {
                            let evict = self
                                .photo_full_size_images
                                .keys()
                                .find(|id| self.photo_selected_id.as_deref() != Some(id.as_str()))
                                .cloned();
                            if let Some(evict) = evict {
                                self.photo_full_size_images.remove(&evict);
                            } else {
                                break;
                            }
                        }
                        if selected {
                            let neighbors = self
                                .photo_items
                                .iter()
                                .position(|item| item.id == item_id)
                                .into_iter()
                                .flat_map(|index| [index.checked_sub(1), index.checked_add(1)])
                                .flatten()
                                .filter_map(|index| self.photo_items.get(index))
                                .filter(|item| item.item_type.as_deref() == Some("Photo"))
                                .map(|item| item.id.clone())
                                .collect::<Vec<_>>();
                            for neighbor in neighbors {
                                self.request_photo_full_size(&neighbor);
                            }
                        }
                    }
                }
                WorkerEvent::PhotoFullSizeFailed {
                    request_id,
                    item_id,
                } => {
                    self.photo_full_size_pending.remove(&request_id);
                    if self.photo_selected_id.as_deref() == Some(item_id.as_str()) {
                        self.photo_full_size_failed.insert(item_id);
                    }
                }
                WorkerEvent::PrimaryArtworkLoaded {
                    request_id,
                    item_id,
                    width,
                    height,
                    rgba,
                } => {
                    if request_id == self.detail_artwork_request_id
                        && self.selected_item_id.as_deref() == Some(item_id.as_str())
                    {
                        let color_image = egui::ColorImage::from_rgba_unmultiplied(
                            [width as usize, height as usize],
                            &rgba,
                        );
                        self.selected_artwork_texture = Some(ctx.load_texture(
                            format!("jellyfin-detail-primary:{item_id}"),
                            color_image,
                            egui::TextureOptions::LINEAR,
                        ));
                    } else if let Some(episode_id) =
                        self.episode_thumbnail_pending.remove(&request_id)
                    {
                        if episode_id == item_id {
                            self.episode_thumbnails.insert(
                                item_id.clone(),
                                ctx.load_texture(
                                    format!("jellyfin-episode-still:{item_id}"),
                                    egui::ColorImage::from_rgba_unmultiplied(
                                        [width as usize, height as usize],
                                        &rgba,
                                    ),
                                    egui::TextureOptions::LINEAR,
                                ),
                            );
                        }
                    } else if let Some((expected_item_id, expected_tag)) =
                        self.pending_artwork_requests.remove(&request_id)
                        && expected_item_id == item_id
                    {
                        // Episodes of one series can share the series poster, so every entry
                        // showing this image gets the texture.
                        let mut texture: Option<egui::TextureHandle> = None;
                        for item in self.items.iter_mut().chain(self.home.media_mut()) {
                            if item.artwork_id != item_id
                                || item.primary_image_tag.as_deref() != Some(expected_tag.as_str())
                            {
                                continue;
                            }
                            let texture = texture.get_or_insert_with(|| {
                                ctx.load_texture(
                                    format!("jellyfin-primary:{item_id}:{expected_tag}"),
                                    egui::ColorImage::from_rgba_unmultiplied(
                                        [width as usize, height as usize],
                                        &rgba,
                                    ),
                                    egui::TextureOptions::LINEAR,
                                )
                            });
                            item.artwork_texture = Some(texture.clone());
                        }
                    }
                    self.trim_artwork_cache();
                }
                WorkerEvent::PrimaryArtworkFailed { request_id } => {
                    self.pending_artwork_requests.remove(&request_id);
                    self.episode_thumbnail_pending.remove(&request_id);
                }
                WorkerEvent::BackdropArtworkLoaded {
                    request_id,
                    item_id,
                    width,
                    height,
                    rgba,
                } if self.home_hero_request.as_ref().is_some_and(
                    |(wanted_request, wanted_item)| {
                        *wanted_request == request_id && *wanted_item == item_id
                    },
                ) =>
                {
                    let color_image = egui::ColorImage::from_rgba_unmultiplied(
                        [width as usize, height as usize],
                        &rgba,
                    );
                    let texture = ctx.load_texture(
                        format!("jellyfin-home-hero:{item_id}"),
                        color_image,
                        egui::TextureOptions::LINEAR,
                    );
                    #[cfg(debug_assertions)]
                    crate::debug_log::log(&format!(
                        "home hero backdrop loaded {width}x{height} for {item_id}"
                    ));
                    self.home_hero_backdrop = Some((item_id, texture));
                }
                WorkerEvent::BackdropArtworkUnavailable {
                    request_id,
                    item_id,
                }
                | WorkerEvent::BackdropArtworkFailed {
                    request_id,
                    item_id,
                } if self.home_hero_request.as_ref().is_some_and(
                    |(wanted_request, wanted_item)| {
                        *wanted_request == request_id && *wanted_item == item_id
                    },
                ) =>
                {
                    // No backdrop for this item: the hero falls back to its primary artwork.
                    // The request stays recorded so the same item is not asked for again.
                    #[cfg(debug_assertions)]
                    crate::debug_log::log(&format!("home hero backdrop unavailable for {item_id}"));
                }
                WorkerEvent::BackdropArtworkLoaded {
                    request_id,
                    item_id,
                    width,
                    height,
                    rgba,
                } if request_id == self.detail_artwork_request_id
                    && self.selected_item_id.as_deref() == Some(item_id.as_str()) =>
                {
                    let color_image = egui::ColorImage::from_rgba_unmultiplied(
                        [width as usize, height as usize],
                        &rgba,
                    );
                    self.selected_artwork_texture = Some(ctx.load_texture(
                        format!("jellyfin-detail-backdrop:{item_id}"),
                        color_image,
                        egui::TextureOptions::LINEAR,
                    ));
                }
                WorkerEvent::BackdropArtworkUnavailable {
                    request_id,
                    item_id,
                }
                | WorkerEvent::BackdropArtworkFailed {
                    request_id,
                    item_id,
                } if request_id == self.detail_artwork_request_id
                    && self.selected_item_id.as_deref() == Some(item_id.as_str()) =>
                {
                    self.request_detail_primary_artwork();
                }
                WorkerEvent::PlaybackInfoLoaded {
                    request_id,
                    response,
                } if request_id == self.playback_request_id => {
                    self.record_network_success();
                    if let Some(error_code) = response.error_code {
                        tracing::debug!(
                            ?error_code,
                            "Jellyfin playback negotiation returned an error code"
                        );
                    }
                    self.playback_info = Some(response);
                    self.playback_loading = false;
                    self.playback_error = None;
                    if self.music_queue_auto_play_item_id.as_deref()
                        == self.selected_item_id.as_deref()
                    {
                        self.start_automatic_music_track();
                    }
                }
                WorkerEvent::PlaybackInfoFailed {
                    request_id,
                    error_class,
                    retry_after,
                } if request_id == self.playback_request_id => {
                    self.record_network_error(error_class, retry_after);
                    self.playback_loading = false;
                    self.record_error_for_crash_report(&format!(
                        "playback info failed: {error_class:?}"
                    ));
                    self.playback_error = Some(crate::ui::status::message(error_class).to_owned());
                    if let Some(item_id) = self.music_queue_auto_play_item_id.clone() {
                        self.on_music_track_ended(&item_id, true);
                    }
                }
                WorkerEvent::PlaybackPrepared {
                    request_id,
                    item_id,
                    mut playback,
                } if request_id == self.playback_start_request_id
                    && self.selected_item_id.as_deref() == Some(item_id.as_str()) =>
                {
                    if let Some((repeat, order, queue)) =
                        music_queue_report_context(&self.music_queue, &item_id)
                    {
                        playback.set_queue_report_context(repeat, order, queue);
                    }
                    // The series ID lives in the item record, which may still be loading. Hold
                    // the stream until it arrives, so the choice is not recorded without a series.
                    // Live TV has no series and no fixed segment list, so it never waits.
                    if playback.live_stream_id().is_none()
                        && self.playing_series_id(&item_id).is_none()
                        && self.details_loading
                    {
                        self.held_playback = Some((request_id, item_id, playback));
                    } else {
                        self.hand_over_playback(item_id, playback);
                    }
                    if self.live_tv_tune_request_id == Some(request_id) {
                        self.live_tv_tune_request_id = None;
                        self.live_tv_retry_available = false;
                        self.live_tv_notice = None;
                    }
                }
                WorkerEvent::PlaybackPrepared { playback, .. } => {
                    if let Some(live_stream_id) = playback.live_stream_id()
                        && let Some(worker) = self.worker.as_ref()
                        && let Err(error) = worker.close_live_stream(live_stream_id.to_owned())
                    {
                        tracing::warn!(%error, "could not queue cleanup for a stale Live TV tune");
                    }
                    drop(playback);
                }
                WorkerEvent::PlaybackPrepareFailed {
                    request_id,
                    message,
                } if request_id == self.playback_start_request_id => {
                    if self.retry_playback_start() {
                        continue;
                    }
                    self.playback_start_loading = false;
                    self.playback_start_status = None;
                    self.record_error_for_crash_report(&format!(
                        "playback start failed: {message}"
                    ));
                    self.playback_start_error = Some(message.clone());
                    if self.live_tv_tune_request_id == Some(request_id) {
                        self.live_tv_tune_request_id = None;
                        self.live_tv_retry_available = true;
                        self.live_tv_notice = Some(message);
                    }
                    if let Some(item_id) = self.music_queue_auto_play_item_id.clone() {
                        self.on_music_track_ended(&item_id, true);
                    }
                }
                WorkerEvent::PlaybackReportSent { request_id } => {
                    finish_playback_report(
                        &mut self.pending_playback_report_requests,
                        self.playback_report_session_generation,
                        &mut self.consecutive_progress_report_failures,
                        &mut self.playback_report_feedback,
                        request_id,
                        true,
                    );
                }
                WorkerEvent::PlaybackReportFailed {
                    request_id,
                    message,
                } => {
                    drop(message);
                    finish_playback_report(
                        &mut self.pending_playback_report_requests,
                        self.playback_report_session_generation,
                        &mut self.consecutive_progress_report_failures,
                        &mut self.playback_report_feedback,
                        request_id,
                        false,
                    );
                }
                WorkerEvent::ConnectionFailed {
                    request_id,
                    message,
                } if request_id.is_none_or(|id| {
                    id == self.connection_request_id || id == self.catalog_request_id
                }) =>
                {
                    if self
                        .quick_connect
                        .as_ref()
                        .is_some_and(|flow| Some(flow.request_id()) == request_id)
                    {
                        self.quick_connect = None;
                    }
                    self.record_error_for_crash_report(&format!("sign-in failed: {message}"));
                    self.clear_connection();
                    self.connection = ConnectionStatus::Disconnected {
                        error: Some(message),
                    };
                }
                WorkerEvent::ReauthenticationRequired {
                    request_id,
                    message,
                } if request_id == self.connection_request_id
                    || request_id == self.catalog_request_id
                    || request_id == self.details_request_id
                    || request_id == self.playback_request_id
                    || request_id == self.playback_start_request_id
                    || request_id == self.detail_artwork_request_id
                    || self
                        .watched
                        .as_ref()
                        .is_some_and(|state| state.request_id == request_id)
                    || self.home.owns_request(request_id)
                    || self.pending_artwork_requests.contains_key(&request_id)
                    || is_pending_playback_report(
                        &self.pending_playback_report_requests,
                        request_id,
                    ) =>
                {
                    self.network_notice = Some(ErrorClass::AuthExpired);
                    if let Some(worker) = self.worker.as_ref() {
                        let _ = worker.disconnect();
                    }
                    self.clear_connection();
                    self.connection = ConnectionStatus::Disconnected {
                        error: Some(message),
                    };
                }
                WorkerEvent::MediaSegmentsLoaded {
                    request_id,
                    item_id,
                    segments,
                } if self.media_segments_request == Some((request_id, item_id.clone())) => {
                    self.media_segments_request = None;
                    self.loaded_media_segments = Some((item_id, segments));
                }
                WorkerEvent::MediaSegmentsFailed { request_id }
                    if self
                        .media_segments_request
                        .as_ref()
                        .is_some_and(|(pending, _)| *pending == request_id) =>
                {
                    self.media_segments_request = None;
                }
                WorkerEvent::RemoteAction { action } => self.pending_player_intents.push(action),
                WorkerEvent::RemoteCommandReceived { command } => {
                    self.queue_socket_command(command);
                }
                WorkerEvent::SyncPlayCommandReceived { command } => {
                    self.queue_sync_play_command(command);
                }
                WorkerEvent::SyncPlayGroupUpdate { update } => {
                    self.apply_sync_play_group_update(update);
                }
                WorkerEvent::SyncPlayGroupsLoaded { request_id, groups } => {
                    self.sync_play_ui.list_loaded(request_id, groups);
                }
                WorkerEvent::SyncPlayGroupsFailed { request_id } => {
                    self.sync_play_ui.list_failed(request_id);
                }
                WorkerEvent::SyncPlayGroupCreated { request_id, group } => {
                    if self.sync_play_ui.group_created(request_id, group.clone()) {
                        // Jellyfin's Web client creates a group without a separate Join call;
                        // NewGroup puts the creating session in the group and announces it over
                        // the session socket.
                        self.sync_play_state = SyncPlayState::Joined {
                            group_id: group.group_id,
                        };
                    }
                }
                WorkerEvent::SyncPlayGroupCreateFailed { request_id } => {
                    self.sync_play_ui.create_failed(request_id);
                }
                WorkerEvent::ServerClockSampled {
                    sent_at,
                    received_at,
                    server_ms,
                } => self.feed_server_clock(sent_at, received_at, server_ms),
                WorkerEvent::SyncPlayJoined {
                    request_id,
                    group_id,
                } if self
                    .sync_play_pending_join
                    .as_ref()
                    .is_some_and(|(pending, _)| *pending == request_id) =>
                {
                    self.sync_play_pending_join = None;
                    self.sync_play_state = SyncPlayState::Joined { group_id };
                    self.sync_play_ui.action_succeeded();
                }
                WorkerEvent::SyncPlayJoinFailed { request_id }
                    if self
                        .sync_play_pending_join
                        .as_ref()
                        .is_some_and(|(pending, _)| *pending == request_id) =>
                {
                    self.sync_play_pending_join = None;
                    self.sync_play_ui.action_failed();
                }
                WorkerEvent::SyncPlayLeft { request_id }
                    if self.sync_play_pending_leave == Some(request_id) =>
                {
                    self.sync_play_pending_leave = None;
                    self.sync_play_state = SyncPlayState::NotJoined;
                    self.sync_play_ui.action_succeeded();
                }
                WorkerEvent::SyncPlayLeaveFailed { request_id }
                    if self.sync_play_pending_leave == Some(request_id) =>
                {
                    self.sync_play_pending_leave = None;
                    self.sync_play_ui.action_failed();
                }
                _ => {}
            }
        }
    }
}

fn gamepad_backend() -> Box<dyn GamepadBackend> {
    #[cfg(test)]
    {
        Box::new(NullGamepad)
    }
    #[cfg(not(test))]
    {
        match kattri_input::GilrsGamepad::new() {
            Ok(gamepad) => Box::new(gamepad),
            Err(_) => Box::new(NullGamepad),
        }
    }
}

/// How often the keyboard map file is checked for edits made outside the app.
const KEYMAP_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Modification time of the keyboard map in `maps_dir`. `None` when there is no folder or file.
fn keyboard_map_mtime(maps_dir: Option<&Path>) -> Option<SystemTime> {
    let name = MapName::new(crate::input::KEYBOARD_MAP).ok()?;
    map_file_mtime(maps_dir?, &name)
}

/// Reloads the keymap when the keyboard map changed since `seen`. Returns true when it reloaded.
/// The time is read before the load and stored after it, so an edit made during the load shows
/// at the next check.
fn reload_keymap_if_changed(
    ctx: &egui::Context,
    maps_dir: Option<&Path>,
    seen: &mut Option<SystemTime>,
) -> bool {
    let mtime = keyboard_map_mtime(maps_dir);
    if !map_file_changed(*seen, mtime) {
        return false;
    }
    crate::input::reload_keymap(ctx, maps_dir);
    *seen = mtime;
    true
}

/// Controller transitions from the last poll. Navigation maps them to actions and drops the
/// button names, so the Input page reads them from here to capture a binding.
#[derive(Clone, Default)]
struct PadPresses(Vec<GamepadEvent>);

const PAD_PRESSES_ID: &str = "kattri_pad_presses";

/// Records the pressed transitions of each poll in egui memory before navigation sees them.
struct ObservedGamepad {
    ctx: egui::Context,
    inner: Box<dyn GamepadBackend>,
}

impl GamepadBackend for ObservedGamepad {
    fn name(&self) -> &'static str {
        self.inner.name()
    }

    fn poll(&mut self) -> Result<Vec<GamepadEvent>, GamepadError> {
        let events = self.inner.poll()?;
        let pressed = events
            .iter()
            .filter(|event| event.pressed)
            .cloned()
            .collect();
        self.ctx.data_mut(|data| {
            data.insert_temp(egui::Id::unique(PAD_PRESSES_ID), PadPresses(pressed));
        });
        Ok(events)
    }

    fn connected_controllers(&self) -> usize {
        self.inner.connected_controllers()
    }
}

/// The controller presses recorded by the last poll. Taking them clears them, so each press is
/// offered to a capture once.
pub(crate) fn take_pad_presses(ctx: &egui::Context) -> Vec<GamepadEvent> {
    ctx.data_mut(|data| data.remove_temp::<PadPresses>(egui::Id::unique(PAD_PRESSES_ID)))
        .map(|presses| presses.0)
        .unwrap_or_default()
}

#[cfg(test)]
mod keymap_watch_tests {
    use super::{
        GamepadBackend, GamepadError, GamepadEvent, ObservedGamepad, reload_keymap_if_changed,
        take_pad_presses,
    };
    use kattri_input::input_map::InputMap;
    use kattri_input::input_store::{MapName, save_map};
    use std::fs;
    use std::path::Path;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    struct Scripted(Vec<GamepadEvent>);

    impl GamepadBackend for Scripted {
        fn name(&self) -> &'static str {
            "scripted"
        }

        fn poll(&mut self) -> Result<Vec<GamepadEvent>, GamepadError> {
            Ok(std::mem::take(&mut self.0))
        }
    }

    fn event(button: &str, pressed: bool) -> GamepadEvent {
        GamepadEvent {
            device: "test-pad".to_owned(),
            button: button.to_owned(),
            pressed,
        }
    }

    fn at(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    fn set_mtime(dir: &Path, time: SystemTime) {
        fs::OpenOptions::new()
            .write(true)
            .open(dir.join("keyboard.json"))
            .expect("open map")
            .set_modified(time)
            .expect("set mtime");
    }

    #[test]
    fn a_poll_passes_every_transition_on_and_records_only_the_presses() {
        let ctx = egui::Context::default();
        let mut pad = ObservedGamepad {
            ctx: ctx.clone(),
            inner: Box::new(Scripted(vec![
                event("South", true),
                event("South", false),
                event("East", true),
            ])),
        };
        let returned = pad.poll().expect("poll");
        assert_eq!(returned.len(), 3, "navigation still gets every transition");
        assert_eq!(
            take_pad_presses(&ctx),
            vec![event("South", true), event("East", true)]
        );
        assert!(take_pad_presses(&ctx).is_empty(), "a press is offered once");
    }

    #[test]
    fn a_changed_mtime_reloads_once_and_records_the_new_time() {
        let dir =
            std::env::temp_dir().join(format!("kattri-keymap-watch-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).expect("create dir");
        let ctx = egui::Context::default();
        let mut seen = None;

        assert!(
            !reload_keymap_if_changed(&ctx, Some(&dir), &mut seen),
            "no file and no change"
        );
        assert_eq!(seen, None);

        let name = MapName::new("keyboard").expect("valid name");
        save_map(&dir, &name, &InputMap::legacy_defaults()).expect("save");
        set_mtime(&dir, at(100));
        assert!(reload_keymap_if_changed(&ctx, Some(&dir), &mut seen));
        assert_eq!(seen, Some(at(100)));
        assert!(
            !reload_keymap_if_changed(&ctx, Some(&dir), &mut seen),
            "the same mtime is not a change"
        );

        set_mtime(&dir, at(200));
        assert!(reload_keymap_if_changed(&ctx, Some(&dir), &mut seen));
        assert_eq!(seen, Some(at(200)));

        fs::remove_file(dir.join("keyboard.json")).expect("remove map");
        assert!(
            reload_keymap_if_changed(&ctx, Some(&dir), &mut seen),
            "a deleted file is a change"
        );
        assert_eq!(seen, None);
        let _ = fs::remove_dir_all(&dir);
    }
}

fn catalog_response_is_current(response_id: u64, active_id: u64) -> bool {
    response_id == active_id
}

fn update_total_record_count(current: &mut Option<u32>, page_start: u32, reported: Option<u32>) {
    if page_start == 0 {
        *current = None;
    }
    if let Some(total) = reported {
        *current = Some(total);
    }
}

/// Local state for the watched toggle on the selected item.
#[derive(Debug, Clone)]
pub(crate) struct WatchedItemState {
    item_id: String,
    request_id: u64,
    toggle: WatchedToggle,
    error: Option<String>,
}

/// What the detail view shows for the watched toggle.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct WatchedView {
    pub(crate) played: bool,
    pub(crate) pending: bool,
    pub(crate) error: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct FavoriteItemState {
    item_id: String,
    request_id: u64,
    toggle: FavoriteToggle,
    error: Option<String>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct FavoriteView {
    pub(crate) favorite: bool,
    pub(crate) pending: bool,
    pub(crate) error: Option<String>,
}

fn watched_error_message(message: &str) -> String {
    if message == "Jellyfin returned HTTP 404" {
        ui_text(ids::DETAIL_ERROR_UNAVAILABLE).into()
    } else {
        ui_text(ids::DETAIL_ERROR_WATCHED_FAILED).into()
    }
}

fn favorite_error_message(message: &str) -> String {
    if message == "Jellyfin returned HTTP 404" {
        ui_text(ids::DETAIL_ERROR_UNAVAILABLE).into()
    } else {
        ui_text(ids::DETAIL_ERROR_FAVORITE_FAILED).into()
    }
}

fn item_details_error_message(message: &str) -> String {
    if message == "Jellyfin returned HTTP 404" {
        ui_text(ids::DETAIL_ERROR_UNAVAILABLE).into()
    } else {
        message.into()
    }
}

fn ordered_season_ids(seasons: &[BaseItemDto]) -> Vec<String> {
    let mut seasons = seasons.iter().collect::<Vec<_>>();
    seasons.sort_by(|left, right| {
        let sort_key = |item: &BaseItemDto| match item.index_number {
            Some(0) => (1_u8, 0_i32),
            Some(index) => (0, index),
            None => (2, 0),
        };
        sort_key(left)
            .cmp(&sort_key(right))
            .then_with(|| left.id.cmp(&right.id))
    });
    seasons
        .into_iter()
        .map(|season| season.id.clone())
        .collect()
}

fn remove_current_item(current_item_id: &str, items: Vec<BaseItemDto>) -> Vec<BaseItemDto> {
    items
        .into_iter()
        .filter(|item| item.id != current_item_id)
        .collect()
}

#[cfg(test)]
mod catalog_query_tests {
    use super::{
        CatalogPlayedFilter, CatalogSort, KattriApp, PagingState, catalog_response_is_current,
        item_details_error_message, ordered_season_ids, update_total_record_count,
    };
    use jellyfin_api::{
        ClientConfig, ErrorClass, ItemFilter, ItemQuery, ItemSortBy, ItemSortOrder, JellyfinClient,
    };
    use mock_jellyfin::{MockConfig, spawn};

    #[test]
    fn item_title_resolves_from_the_catalog_details_and_queue_names() {
        let mut app = KattriApp::new(egui::Context::default());
        assert_eq!(app.item_title("missing"), None);

        app.music_track_names
            .insert("track-1".to_owned(), "Queue name".to_owned());
        assert_eq!(app.item_title("track-1"), Some("Queue name"));

        app.selected_details = Some(
            serde_json::from_value(serde_json::json!({"Id": "track-1", "Name": "Details name"}))
                .expect("details fixture"),
        );
        // The details entry wins over the remembered queue name for the same id.
        assert_eq!(app.item_title("track-1"), Some("Details name"));
    }

    #[test]
    fn offline_banner_requires_two_connection_failures_and_clears_on_success() {
        let mut app = KattriApp::new(egui::Context::default());
        app.record_network_error(ErrorClass::Offline, None);
        assert_eq!(app.network_notice, None);
        app.record_network_error(ErrorClass::Timeout, None);
        assert_eq!(app.network_notice, Some(ErrorClass::Offline));
        assert_eq!(app.network_retry_schedule.retries_made(), 2);
        assert!(app.network_retry_deadline.is_some());

        app.record_network_success();
        assert_eq!(app.network_notice, None);
        assert_eq!(app.offline_tracker.consecutive_failures(), 0);
        assert_eq!(app.network_retry_schedule.retries_made(), 0);
        assert_eq!(app.network_retry_deadline, None);
    }

    #[test]
    fn retry_after_sets_a_minimum_delay_for_the_ui_retry() {
        let mut app = KattriApp::new(egui::Context::default());
        app.ui_time = std::time::Duration::from_secs(5);
        app.record_network_error(
            ErrorClass::ServerUnavailable,
            Some(std::time::Duration::from_secs(7)),
        );
        assert_eq!(
            app.network_retry_deadline,
            Some(std::time::Duration::from_secs(12))
        );
    }

    #[test]
    fn user_action_cancels_the_pending_automatic_retry() {
        let mut app = KattriApp::new(egui::Context::default());
        app.record_network_error(ErrorClass::Offline, None);
        app.record_network_error(ErrorClass::Offline, None);
        assert!(app.network_retry_deadline.is_some());
        assert_eq!(app.network_retry_schedule.retries_made(), 2);

        app.cancel_network_retry_for_user_action();
        assert_eq!(app.network_retry_deadline, None);
        assert_eq!(app.network_retry_context, None);
        assert_eq!(app.network_retry_schedule.retries_made(), 0);
    }

    #[test]
    fn series_seasons_order_regular_then_specials_then_unindexed() {
        let seasons: [jellyfin_api::BaseItemDto; 4] = [
            serde_json::from_value(serde_json::json!({"Id":"unindexed","Name":"Unindexed"}))
                .expect("season fixture"),
            serde_json::from_value(
                serde_json::json!({"Id":"specials","Name":"Specials","IndexNumber":0}),
            )
            .expect("season fixture"),
            serde_json::from_value(
                serde_json::json!({"Id":"season-2","Name":"Season 2","IndexNumber":2}),
            )
            .expect("season fixture"),
            serde_json::from_value(
                serde_json::json!({"Id":"season-1","Name":"Season 1","IndexNumber":1}),
            )
            .expect("season fixture"),
        ];
        assert_eq!(
            ordered_season_ids(&seasons),
            vec![
                "season-1".to_owned(),
                "season-2".to_owned(),
                "specials".to_owned(),
                "unindexed".to_owned(),
            ]
        );
    }

    #[test]
    fn duplicate_season_numbers_are_tied_by_id() {
        let seasons: [jellyfin_api::BaseItemDto; 2] = [
            serde_json::from_value(serde_json::json!({"Id":"z","Name":"Same","IndexNumber":2}))
                .expect("season fixture"),
            serde_json::from_value(serde_json::json!({"Id":"a","Name":"Same","IndexNumber":2}))
                .expect("season fixture"),
        ];
        assert_eq!(
            ordered_season_ids(&seasons),
            vec!["a".to_owned(), "z".to_owned()]
        );
    }

    #[test]
    fn catalog_sort_values_match_jellyfin_query_names() {
        assert_eq!(CatalogSort::Name.api_value(), ItemSortBy::SortName);
        assert_eq!(CatalogSort::DateAdded.api_value(), ItemSortBy::DateCreated);
        assert_eq!(
            CatalogSort::ReleaseDate.api_value(),
            ItemSortBy::PremiereDate
        );
        assert_eq!(CatalogSort::Rating.api_value(), ItemSortBy::CommunityRating);
    }

    #[test]
    fn item_details_404_uses_unavailable_message_and_preserves_other_errors() {
        assert_eq!(
            item_details_error_message("Jellyfin returned HTTP 404"),
            "This item is no longer available"
        );
        assert_eq!(
            item_details_error_message("Jellyfin returned HTTP 500"),
            "Jellyfin returned HTTP 500"
        );
    }

    #[test]
    fn catalog_filters_combine_play_state_and_favorites() {
        let mut app = KattriApp::new(egui::Context::default());
        app.catalog_played_filter = CatalogPlayedFilter::Unplayed;
        app.catalog_favorites_only = true;
        assert_eq!(
            app.catalog_filters(),
            Some(vec![ItemFilter::IsUnplayed, ItemFilter::IsFavorite])
        );
    }

    #[test]
    fn alphabet_picker_maps_to_name_bounds_and_counts_as_a_filter() {
        let mut app = KattriApp::new(egui::Context::default());
        app.worker = None;
        assert_eq!(app.catalog_name_bounds(), (None, None));
        app.set_catalog_name_prefix(Some('K'));
        assert_eq!(app.catalog_name_bounds(), (Some("K".into()), None));
        assert!(app.has_catalog_filters());
        app.set_catalog_name_prefix(Some('#'));
        assert_eq!(app.catalog_name_bounds(), (None, Some("A".into())));
        app.clear_catalog_filters();
        assert_eq!(app.catalog_name_prefix, None);
        app.set_catalog_name_prefix(Some('B'));
        app.navigate_to(crate::app::Page::Movies);
        assert_eq!(
            app.catalog_name_prefix, None,
            "a new page starts unfiltered"
        );
    }

    #[test]
    fn genre_list_loads_once_per_page_and_the_choice_is_a_filter() {
        let mut app = KattriApp::new(egui::Context::default());
        let (worker, sender) = crate::worker::ApiWorker::with_event_sender_for_test();
        app.worker = Some(worker);
        app.page = crate::app::Page::Movies;
        app.request_catalog_genres(Some(vec!["Movie".into()]));
        let request_id = app.catalog_genres_request_id;
        assert_ne!(request_id, 0);
        app.request_catalog_genres(Some(vec!["Movie".into()]));
        assert_eq!(
            app.catalog_genres_request_id, request_id,
            "same scope, no new request"
        );
        sender
            .try_send(crate::worker::WorkerEvent::GenresLoaded {
                request_id,
                genres: vec!["Action".into(), "Drama".into()],
            })
            .expect("send genres");
        let ctx = egui::Context::default();
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
            app.receive_worker_events(ui.ctx())
        });
        output.textures_delta.clear();
        assert_eq!(app.catalog_genres, ["Action", "Drama"]);
        app.set_catalog_genre(Some("Drama".into()));
        assert!(app.has_catalog_filters());
        app.clear_catalog_filters();
        assert_eq!(app.catalog_genre, None);

        app.page = crate::app::Page::Photos;
        app.request_catalog_genres(None);
        assert!(app.catalog_genres.is_empty(), "photos have no genre filter");
    }

    #[test]
    fn an_episode_opened_from_its_series_goes_back_to_the_series() {
        let mut app = KattriApp::new(egui::Context::default());
        app.worker = None;
        app.select_item("show".into());
        app.open_child_item("episode".into());
        assert_eq!(app.selected_item_id.as_deref(), Some("episode"));
        app.navigate_back();
        assert_eq!(app.selected_item_id.as_deref(), Some("show"));
        app.navigate_back();
        assert_eq!(
            app.selected_item_id, None,
            "the series page goes back to the library"
        );

        app.select_item("show".into());
        app.open_child_item("episode".into());
        app.select_item("elsewhere".into());
        app.navigate_back();
        assert_eq!(
            app.selected_item_id, None,
            "another selection forgets the series"
        );
    }

    #[test]
    fn unsupported_filter_is_hidden_and_removed_before_the_retry() {
        let mut app = KattriApp::new(egui::Context::default());
        app.worker = None;
        app.catalog_played_filter = CatalogPlayedFilter::Unplayed;
        app.catalog_favorites_only = true;

        app.apply_unsupported_catalog_filter(ItemFilter::IsUnplayed);

        assert_eq!(app.catalog_played_filter, CatalogPlayedFilter::Any);
        assert!(!app.catalog_filter_supported(ItemFilter::IsUnplayed));
        assert!(app.catalog_filter_supported(ItemFilter::IsFavorite));
        assert_eq!(app.catalog_filters(), Some(vec![ItemFilter::IsFavorite]));
        assert!(app.catalog_filter_notice.take().is_some());
        assert!(app.catalog_filter_notice.is_none());
    }

    #[test]
    fn changing_sort_restarts_query_and_invalidates_the_previous_request() {
        let mut app = KattriApp::new(egui::Context::default());
        app.worker = None;
        app.total_record_count = Some(90);
        app.catalog_next_index = 50;
        app.catalog_end_reached = true;

        app.set_catalog_sort(CatalogSort::Rating);

        assert_eq!(app.catalog_request_id, 1);
        assert_eq!(app.start_index, 0);
        assert_eq!(app.catalog_next_index, 0);
        assert!(!app.catalog_end_reached);
        assert_eq!(app.total_record_count, None);
        assert!(!app.catalog_loading);
        assert!(app.catalog_error.is_some());
    }

    #[test]
    fn automatic_page_trigger_starts_within_two_layout_rows_of_end() {
        assert!(!PagingState::focus_within_prefetch_range(37, 50, 6));
        assert!(PagingState::focus_within_prefetch_range(38, 50, 6));
        assert!(PagingState::focus_within_prefetch_range(0, 8, 6));
        assert!(!PagingState::focus_within_prefetch_range(45, 50, 2));
        assert!(PagingState::focus_within_prefetch_range(46, 50, 2));
    }

    #[test]
    fn stale_catalog_request_ids_are_rejected() {
        assert!(!catalog_response_is_current(8, 9));
        assert!(catalog_response_is_current(9, 9));
    }

    #[test]
    fn catalog_total_is_preserved_when_appended_page_omits_it() {
        let mut total = Some(120);

        update_total_record_count(&mut total, 50, None);
        assert_eq!(total, Some(120));

        update_total_record_count(&mut total, 100, Some(125));
        assert_eq!(total, Some(125));

        update_total_record_count(&mut total, 0, None);
        assert_eq!(total, None);
    }

    #[test]
    fn late_old_sort_response_cannot_replace_the_current_catalog_page() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        runtime.block_on(async {
            let server = spawn(MockConfig {
                item_count: Some(120),
                ..MockConfig::default()
            })
            .await
            .expect("spawn mock Jellyfin server");
            let base_url = server.base_url();
            let config = ClientConfig::new(
                &base_url,
                "Kattri test",
                "test device",
                "device-id",
                "test-version",
            )
            .expect("valid mock client config");
            let client = JellyfinClient::new(config).expect("mock loopback server is allowed");
            let session = client
                .authenticate("demo", "demo")
                .await
                .expect("authenticate to mock server");
            let query_for_sort = |sort_by, sort_order| ItemQuery {
                recursive: true,
                start_index: 0,
                limit: 50,
                parent_id: None,
                include_item_types: None,
                search_term: None,
                sort_by: Some(vec![sort_by]),
                sort_order: Some(vec![sort_order]),
                filters: None,
                genres: None,
                years: None,
                person_ids: None,
                ids: None,
                enable_total_record_count: Some(true),
                name_starts_with: None,
                name_less_than: None,
            };
            let old_page = client
                .list_items(
                    &session,
                    &query_for_sort(ItemSortBy::SortName, ItemSortOrder::Ascending),
                )
                .await
                .expect("fetch old name-sorted response");
            let current_page = client
                .list_items(
                    &session,
                    &query_for_sort(ItemSortBy::CommunityRating, ItemSortOrder::Descending),
                )
                .await
                .expect("fetch current rating-sorted response");
            let item_ids = |page: &jellyfin_api::ItemsResponse| {
                page.items
                    .iter()
                    .map(|item| item.id.clone())
                    .collect::<Vec<_>>()
            };
            let old_order = item_ids(&old_page);
            let current_order = item_ids(&current_page);
            assert_ne!(old_order, current_order, "mock sorts must differ");

            let active_request_id = 12;
            let mut app = KattriApp::new(egui::Context::default());
            app.catalog_request_id = active_request_id;
            assert!(app.apply_catalog_response(active_request_id, current_page, None));
            assert!(!app.apply_catalog_response(active_request_id - 1, old_page, None));

            let loaded_order: Vec<_> = app.items.iter().map(|item| item.id.clone()).collect();
            assert_eq!(loaded_order, current_order);
            server.shutdown().await;
        });
    }

    #[test]
    fn late_old_search_response_cannot_replace_latest_mock_results() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        runtime.block_on(async {
            let server = spawn(MockConfig::default())
                .await
                .expect("spawn mock Jellyfin server");
            let config = ClientConfig::new(
                &server.base_url(),
                "Kattri test",
                "test device",
                "device-id",
                "test-version",
            )
            .expect("valid mock client config");
            let client = JellyfinClient::new(config).expect("mock loopback server is allowed");
            let session = client
                .authenticate("demo", "demo")
                .await
                .expect("authenticate to mock server");
            let query = |term: &str| ItemQuery {
                limit: 30,
                search_term: Some(term.to_owned()),
                include_item_types: Some(
                    ["Movie", "Series", "Episode", "Audio", "Person"]
                        .into_iter()
                        .map(str::to_owned)
                        .collect(),
                ),
                ..ItemQuery::default()
            };
            let first_response = client
                .list_items(&session, &query("Hollow"))
                .await
                .expect("first search response");
            let latest_response = client
                .list_items(&session, &query("Glass"))
                .await
                .expect("latest search response");
            assert!(
                first_response
                    .items
                    .iter()
                    .any(|item| item.name == "Hollow Pines")
            );
            assert!(
                latest_response
                    .items
                    .iter()
                    .any(|item| item.name == "The Glass Orchard")
            );

            let mut app = KattriApp::new(egui::Context::default());
            app.search_active = true;
            app.submitted_search = Some("Glass".into());
            app.catalog_request_id = 22;
            assert!(app.apply_catalog_response(22, latest_response, None));
            assert!(!app.apply_catalog_response(21, first_response, None));
            assert!(
                app.search_results
                    .iter()
                    .any(|item| item.name == "The Glass Orchard")
            );
            assert!(
                app.search_results
                    .iter()
                    .all(|item| item.name != "Hollow Pines")
            );
            server.shutdown().await;
        });
    }

    #[test]
    fn loading_page_suppresses_another_automatic_request() {
        let mut app = KattriApp::new(egui::Context::default());
        app.worker = None;
        app.catalog_loading = true;
        app.maybe_request_next_catalog_page(0, 6);
        assert_eq!(app.catalog_request_id, 0);
    }
}

#[cfg(test)]
mod search_browse_tests {
    use super::{AppView, KattriApp, Page};
    use crate::{ui::search::SearchGroup, worker::ApiWorker};

    #[test]
    fn show_all_keeps_the_term_and_opens_a_typed_catalog_browse() {
        let mut app = KattriApp::new(egui::Context::default());
        let (worker, _events) = ApiWorker::with_event_sender_for_test();
        app.worker = Some(worker);
        app.view = AppView::Library;
        app.page = Page::Home;
        app.submitted_search = Some("quiet river".into());
        app.search_active = true;
        app.show_all_search_group(SearchGroup::Photos);

        assert_eq!(app.search_browse_group, Some(SearchGroup::Photos));
        assert_eq!(app.submitted_search.as_deref(), Some("quiet river"));
        assert!(!app.search_active);
        assert!(app.catalog_loading);
        assert_eq!(app.catalog_next_index, 0);
        assert_eq!(app.items.len(), 0);
    }

    #[test]
    fn show_all_channels_keeps_a_separate_paged_search_request() {
        let mut app = KattriApp::new(egui::Context::default());
        let (worker, _events) = ApiWorker::with_event_sender_for_test();
        app.worker = Some(worker);
        app.submitted_search = Some("news".into());
        app.show_all_search_group(SearchGroup::Channels);

        assert_eq!(app.search_browse_group, Some(SearchGroup::Channels));
        assert_eq!(app.submitted_search.as_deref(), Some("news"));
        assert!(app.live_tv_search_loading);
        assert_eq!(app.live_tv_search_start_index, 0);
        assert_eq!(app.live_tv_search_total, None);
    }

    #[test]
    fn returning_from_group_browse_preserves_and_resubmits_the_search_term() {
        let mut app = KattriApp::new(egui::Context::default());
        let (worker, _events) = ApiWorker::with_event_sender_for_test();
        app.worker = Some(worker);
        app.search_browse_group = Some(SearchGroup::Channels);
        app.submitted_search = Some("news".into());
        app.live_tv_search_results = vec![
            serde_json::from_value(serde_json::json!({
                "Id": "channel-1", "Name": "News", "Type": "TvChannel"
            }))
            .expect("channel fixture"),
        ];
        app.live_tv_search_start_index = 30;
        app.live_tv_search_total = Some(47);

        app.return_to_search_results();

        assert_eq!(app.search_browse_group, None);
        assert_eq!(app.submitted_search.as_deref(), Some("news"));
        assert!(!app.search_active);
        assert!(app.catalog_loading);
        assert!(app.live_tv_search_loading);
        assert!(app.live_tv_search_results.is_empty());
        assert_eq!(app.live_tv_search_start_index, 0);
        assert_eq!(app.live_tv_search_total, None);
    }
}

/// The episodes before and after one episode, as the server ordered them.
#[derive(Clone, Debug, PartialEq, Eq)]
struct EpisodeNeighbors {
    episode_id: String,
    request_id: u64,
    previous: Option<String>,
    next: Option<String>,
    /// "S1:E4 · Title" of the next episode, for the Up Next card.
    next_label: Option<String>,
    /// The viewer dismissed Up Next for this episode: it ends without starting the next.
    up_next_cancelled: bool,
}

/// Episodes in shuffled order as `(id, label)`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct ShuffleQueue {
    episodes: Vec<(String, String)>,
    /// The episode whose Up Next card was dismissed.
    up_next_cancelled_for: Option<String>,
}

/// What comes before and after the playing item: from the shuffle when it holds the item,
/// else from the server's episode order.
struct Adjacent<'a> {
    previous: Option<&'a str>,
    next: Option<(&'a str, &'a str)>,
    up_next_cancelled: bool,
}

/// The server's Next Up episode for one series.
#[derive(Clone, Debug, PartialEq)]
struct SeriesNextUp {
    series_id: String,
    request_id: u64,
    episode: Option<BaseItemDto>,
}

const MAX_PHOTO_THUMBNAILS: usize = 64;
/// Most episode stills kept for one series page (about two seasons).
const MAX_EPISODE_THUMBNAILS: usize = 48;

/// Keeps at most [`MAX_PHOTO_THUMBNAILS`] thumbnails, never evicting one in the visible
/// window or the one just loaded. An evicted id may be requested again when it scrolls back.
fn evict_photo_thumbnails(
    thumbnails: &mut HashMap<String, egui::TextureHandle>,
    attempted: &mut HashSet<String>,
    visible: &HashSet<String>,
    just_loaded: &str,
) {
    if thumbnails.len() <= MAX_PHOTO_THUMBNAILS {
        return;
    }
    let excess = thumbnails.len() - MAX_PHOTO_THUMBNAILS;
    let evict: Vec<String> = thumbnails
        .keys()
        .filter(|id| id.as_str() != just_loaded && !visible.contains(id.as_str()))
        .take(excess)
        .cloned()
        .collect();
    for id in evict {
        thumbnails.remove(&id);
        attempted.remove(&id);
    }
}

/// Drops the least recently shown textures past [`MAX_CACHED_ARTWORK_TEXTURES`]. Textures
/// shown at or after `visible_from_tick` are on screen and stay, so a screen showing more
/// cards than the cap grows the cache instead of evicting and reloading visible artwork.
fn trim_artwork_textures(
    items: &mut [&mut MediaItem],
    access_ticks: &mut HashMap<(String, String), u64>,
    attempted: &mut HashSet<(String, String)>,
    visible_from_tick: u64,
) {
    let loaded_count = items
        .iter()
        .filter(|item| item.artwork_texture.is_some())
        .count();
    if loaded_count <= MAX_CACHED_ARTWORK_TEXTURES {
        return;
    }

    let mut least_recently_used: Vec<_> = items
        .iter()
        .filter(|item| item.artwork_texture.is_some())
        .filter_map(|item| {
            item.primary_image_tag.as_ref().map(|tag| {
                let key = (item.artwork_id.clone(), tag.clone());
                let tick = access_ticks.get(&key).copied().unwrap_or(0);
                (tick, key)
            })
        })
        .filter(|(tick, _)| visible_from_tick == 0 || *tick < visible_from_tick)
        .collect();
    least_recently_used.sort_unstable_by_key(|(tick, _)| *tick);

    for (_, (item_id, image_tag)) in least_recently_used
        .into_iter()
        .take(loaded_count - MAX_CACHED_ARTWORK_TEXTURES)
    {
        for item in items.iter_mut() {
            if item.artwork_id == item_id
                && item.primary_image_tag.as_deref() == Some(image_tag.as_str())
            {
                item.artwork_texture = None;
            }
        }
        let key = (item_id, image_tag);
        access_ticks.remove(&key);
        attempted.remove(&key);
    }
}

fn finish_playback_report(
    pending: &mut HashMap<u64, PendingPlaybackReport>,
    current_session_generation: u64,
    consecutive_progress_failures: &mut u8,
    feedback: &mut Option<String>,
    request_id: u64,
    succeeded: bool,
) -> bool {
    let Some(pending_report) = pending.remove(&request_id) else {
        return false;
    };
    if pending_report.session_generation != current_session_generation {
        return true;
    }
    let kind = pending_report.kind;
    match (kind, succeeded) {
        (PlaybackReportKind::Progress, true) => *consecutive_progress_failures = 0,
        (PlaybackReportKind::Progress, false) => {
            *consecutive_progress_failures = consecutive_progress_failures.saturating_add(1);
            if *consecutive_progress_failures >= MAX_CONSECUTIVE_PROGRESS_REPORT_FAILURES {
                if *consecutive_progress_failures == MAX_CONSECUTIVE_PROGRESS_REPORT_FAILURES {
                    tracing::warn!(
                        "Playback progress reporting stopped after repeated failures; playback continues"
                    );
                }
                *feedback = Some(ui_text(ids::APP_PLAYBACK_PROGRESS_STOPPED).into());
            }
        }
        (PlaybackReportKind::Started | PlaybackReportKind::Stopped, false) => {
            if *consecutive_progress_failures < MAX_CONSECUTIVE_PROGRESS_REPORT_FAILURES {
                *feedback = Some(ui_text(ids::PLAYBACK_ERROR_SESSION_UPDATE).into());
            }
        }
        (PlaybackReportKind::Started | PlaybackReportKind::Stopped, true) => {}
    }
    true
}

fn is_pending_playback_report(
    pending: &HashMap<u64, PendingPlaybackReport>,
    request_id: u64,
) -> bool {
    pending.contains_key(&request_id)
}

fn begin_playback_report_session(
    session_generation: &mut u64,
    consecutive_progress_failures: &mut u8,
    feedback: &mut Option<String>,
) {
    *session_generation = session_generation.wrapping_add(1);
    *consecutive_progress_failures = 0;
    *feedback = None;
}

fn should_suppress_playback_report(
    kind: PlaybackReportKind,
    consecutive_progress_failures: u8,
) -> bool {
    kind == PlaybackReportKind::Progress
        && consecutive_progress_failures >= MAX_CONSECUTIVE_PROGRESS_REPORT_FAILURES
}

/// True when a skip switch is on, so the segments endpoint is worth requesting (M2-08).
/// An invalid outro delay disables segment handling, the same as the host's skip model.
fn segments_needed(playback: &kattri_settings::PlaybackSettings) -> bool {
    SkipSettings::new(
        playback.skip_intro,
        playback.skip_outro,
        playback.auto_skip_intro,
        playback.auto_skip_outro,
        playback.outro_delay_ms,
    )
    .is_ok_and(|skip| skip.fetch_needed())
}

fn playback_overrides_from_settings(
    settings: &kattri_settings::Settings,
) -> PlaybackRequestOverrides {
    PlaybackRequestOverrides {
        max_streaming_bitrate: Some(settings.playback.max_streaming_bitrate),
        force_transcode: settings.video.transcode.always_force,
    }
}

impl KattriApp {
    /// Checks the keyboard map about once a second, so an edit made outside the app takes effect
    /// without a restart.
    fn poll_keymap_file(&mut self, ctx: &egui::Context, now: Duration) {
        if now >= self.keymap_check_due {
            self.keymap_check_due = now.saturating_add(KEYMAP_POLL_INTERVAL);
            let maps_dir = self.settings.keymap_dir();
            reload_keymap_if_changed(ctx, maps_dir.as_deref(), &mut self.keymap_mtime_seen);
        }
        ctx.request_repaint_after(self.keymap_check_due.saturating_sub(now));
    }

    /// Advances asynchronous application work without drawing catalog widgets or taking focus.
    /// The native host calls this while immersive playback owns the visible interface.
    pub fn update_background(&mut self, ctx: &egui::Context) {
        self.settings.poll_save_results();
        #[cfg(debug_assertions)]
        self.debug_log_changes();
        self.poll_auto_play();
        #[cfg(debug_assertions)]
        if DEBUG_SIGNIN_PENDING.load(std::sync::atomic::Ordering::Relaxed)
            && self.server_preview().is_some()
            && matches!(self.connection, ConnectionStatus::Disconnected { .. })
        {
            DEBUG_SIGNIN_PENDING.store(false, std::sync::atomic::Ordering::Relaxed);
            self.start_connection();
        }
        #[cfg(debug_assertions)]
        if std::env::var_os("KATTRI_DEBUG_DIR").is_some() {
            ctx.request_repaint_after(Duration::from_millis(500));
        }
        let user_input = ctx.input(|input| {
            input.pointer.any_click()
                || input
                    .events
                    .iter()
                    .any(|event| matches!(event, egui::Event::Key { pressed: true, .. }))
        });
        if user_input {
            self.cancel_network_retry_for_user_action();
        }
        let now = duration_from_egui_time(ctx.input(|input| input.time));
        self.ui_time = now;
        self.receive_worker_events(ctx);
        self.persist_music_queue_if_needed();
        if !self.music_queue.is_empty()
            && !self.music_queue_persistence_suspended
            && self.pending_stored_music_queue.is_none()
        {
            let elapsed = now.saturating_sub(self.music_queue_last_saved_at);
            ctx.request_repaint_after(Duration::from_secs(10).saturating_sub(elapsed));
        }
        self.tick_network_retry(now, ctx);
        self.tick_downloads(ctx);
        self.tick_quick_connect_probe(now);
        if let Some(deadline) = self.quick_connect_probe_after {
            ctx.request_repaint_after(deadline.saturating_sub(now));
        }
        if let Some(flow) = self.quick_connect.as_mut()
            && !flow.is_finished()
        {
            let request_id = flow.request_id();
            let action = flow.tick(now);
            self.handle_quick_connect_action(request_id, action);
            ctx.request_repaint_after(Duration::from_millis(250));
        }
        self.poll_search_debounce(now);
        self.poll_keymap_file(ctx, now);
        crate::theme::apply(ctx, self.high_contrast, self.reduce_motion);
        crate::theme::apply_interface_size(ctx, self.interface_size);
    }

    pub fn ui(&mut self, root: &mut egui::Ui) {
        let ctx = root.ctx().clone();
        self.update_background(&ctx);
        self.request_home_hero_backdrop();
        let width = root.available_width();
        self.show_profile_overlays(&ctx);
        if let Some(url) = crate::ui::open_url::show(&ctx, &mut self.open_url) {
            self.pending_stream_url = Some(url);
        }
        self.sync_home_sections();
        crate::ui::item_menu::publish_playlists(&ctx, self.menu_playlists.clone());
        crate::ui::item_menu::publish_admin(&ctx, self.user_is_admin);
        crate::ui::item_menu::publish_collections(&ctx, self.menu_collections.clone());
        crate::ui::draw(root, self, width);
        if let Some((target, action)) = crate::ui::item_menu::take(&ctx) {
            self.apply_item_menu_action(target, action);
        }
        self.draw_delete_confirmation(&ctx);
        self.draw_toast(&ctx);
    }

    fn submit_item_admin(
        &mut self,
        item_id: String,
        title: String,
        action: crate::worker::ItemAdminAction,
        collection_id: Option<String>,
    ) {
        if !self.user_is_admin {
            return;
        }
        let request_id = self.allocate_request_id();
        if self.worker.as_ref().is_some_and(|worker| {
            worker
                .item_admin(request_id, item_id, action, collection_id)
                .is_ok()
        }) {
            self.admin_requests.insert(request_id, title);
        }
    }

    /// Asks before a delete; the server removes the files, so there is no undo.
    fn draw_delete_confirmation(&mut self, ctx: &egui::Context) {
        let Some((item_id, title)) = self.pending_delete.clone() else {
            return;
        };
        let mut choice = None;
        let modal = egui::Modal::new(egui::Id::unique("kattri-confirm-delete")).show(ctx, |ui| {
            ui.set_max_width(420.0);
            ui.heading(ui_text_args(
                ids::DELETE_CONFIRM_TITLE,
                &[("title", title.as_str())],
            ));
            ui.label(ui_text(ids::DELETE_CONFIRM_BODY));
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                let delete = egui::Button::new(
                    egui::RichText::new(ui_text(ids::DELETE_CONFIRM_DELETE))
                        .color(egui::Color32::WHITE),
                )
                .fill(egui::Color32::from_rgb(0xB0, 0x2A, 0x2A));
                if ui.add(delete).clicked() {
                    choice = Some(true);
                }
                if ui.button(ui_text(ids::DELETE_CONFIRM_CANCEL)).clicked() {
                    choice = Some(false);
                }
            });
        });
        if modal.should_close() && choice.is_none() {
            choice = Some(false);
        }
        match choice {
            Some(true) => {
                self.pending_delete = None;
                self.submit_item_admin(
                    item_id,
                    title,
                    crate::worker::ItemAdminAction::Delete,
                    None,
                );
            }
            Some(false) => self.pending_delete = None,
            None => {}
        }
    }

    /// Runs an entry chosen from a card's context menu. Played and favorite changes are sent
    /// straight to the server for that item and mirrored on every loaded card that shows it.
    pub(crate) fn apply_item_menu_action(
        &mut self,
        target: crate::ui::item_menu::ItemMenuTarget,
        action: crate::ui::item_menu::ItemMenuAction,
    ) {
        use crate::ui::item_menu::ItemMenuAction;
        match action {
            ItemMenuAction::Play => self.play_item(target.item_id, false),
            ItemMenuAction::PlayFromStart => self.play_item(target.item_id, true),
            ItemMenuAction::Open => self.select_item(target.item_id),
            ItemMenuAction::GoToSeries => {
                if let Some(series_id) = target.series_id {
                    self.select_item(series_id);
                }
            }
            ItemMenuAction::Download => self.start_download(&target.item_id, &target.title),
            ItemMenuAction::AddToQueue | ItemMenuAction::PlayNextInQueue => {
                let queue_item = QueueItem::new(target.item_id.clone());
                let result = if action == ItemMenuAction::PlayNextInQueue {
                    self.music_queue.play_next(queue_item)
                } else {
                    self.music_queue.add(queue_item)
                };
                if result.is_ok() {
                    self.show_toast(ui_text_args(
                        ids::TOAST_ADDED_TO_QUEUE,
                        &[("title", target.title.as_str())],
                    ));
                    self.music_track_names.insert(target.item_id, target.title);
                }
            }
            ItemMenuAction::RefreshMetadata => {
                self.submit_item_admin(
                    target.item_id,
                    target.title,
                    crate::worker::ItemAdminAction::RefreshMetadata,
                    None,
                );
            }
            ItemMenuAction::AddToCollection => {
                if let Some(collection_id) = target.list_id {
                    self.submit_item_admin(
                        target.item_id,
                        target.title,
                        crate::worker::ItemAdminAction::AddToCollection,
                        Some(collection_id),
                    );
                }
            }
            ItemMenuAction::Delete => {
                self.pending_delete = Some((target.item_id, target.title));
            }
            ItemMenuAction::AddToPlaylist => {
                let Some(playlist_id) = target.list_id else {
                    return;
                };
                let playlist_name = self
                    .menu_playlists
                    .iter()
                    .find(|(id, _)| *id == playlist_id)
                    .map_or_else(String::new, |(_, name)| name.clone());
                let request_id = self.allocate_request_id();
                let sent = self.worker.as_ref().is_some_and(|worker| {
                    worker
                        .add_playlist_items(request_id, playlist_id, vec![target.item_id])
                        .is_ok()
                });
                if sent {
                    self.menu_playlist_add = Some((request_id, target.title, playlist_name));
                }
            }
            ItemMenuAction::SetPlayed(played) => {
                let request_id = self.allocate_request_id();
                let sent = self.worker.as_ref().is_some_and(|worker| {
                    worker
                        .set_played(request_id, target.item_id.clone(), played)
                        .is_ok()
                });
                if sent {
                    for item in self.items.iter_mut().chain(self.home.media_mut()) {
                        if item.id == target.item_id {
                            item.played = played;
                        }
                    }
                }
            }
            ItemMenuAction::SetFavorite(favorite) => {
                let request_id = self.allocate_request_id();
                let sent = self.worker.as_ref().is_some_and(|worker| {
                    worker
                        .set_favorite(request_id, target.item_id.clone(), favorite)
                        .is_ok()
                });
                if sent {
                    for item in self.items.iter_mut().chain(self.home.media_mut()) {
                        if item.id == target.item_id {
                            item.favorite = favorite;
                        }
                    }
                }
            }
        }
    }

    /// Consume a profile-mapped play/pause shortcut while the native player owns the screen.
    /// Focused widgets and open menus retain their own keyboard activation semantics.
    pub fn take_player_pause_shortcut(&self, ctx: &egui::Context) -> bool {
        !ctx.egui_wants_keyboard_input()
            && !egui::Popup::is_any_open(ctx)
            && crate::input::take_action(
                ctx,
                &[kattri_client_core::focus::NavAction::PlayPause],
                false,
            )
            .is_some()
    }
}

#[cfg(test)]
mod background_update_tests {
    use super::{ConnectionStatus, KattriApp};
    use crate::worker::{ApiWorker, WorkerEvent};

    #[test]
    fn native_pause_uses_the_profile_keymap_and_consumes_the_press_once() {
        let ctx = egui::Context::default();
        let app = KattriApp::new(ctx.clone());
        crate::input::reload_keymap(
            &ctx,
            Some(std::path::Path::new("nonexistent-test-inputmaps")),
        );
        let mut output = ctx.run_ui(
            egui::RawInput {
                events: vec![egui::Event::Key {
                    key: egui::Key::Space,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::NONE,
                }],
                ..Default::default()
            },
            |root| {
                assert!(app.take_player_pause_shortcut(root.ctx()));
                assert!(!app.take_player_pause_shortcut(root.ctx()));
            },
        );
        output.textures_delta.clear();
    }

    #[test]
    fn native_pause_does_not_consume_a_focused_widgets_space_press() {
        let ctx = egui::Context::default();
        let app = KattriApp::new(ctx.clone());
        let mut output = ctx.run_ui(egui::RawInput::default(), |root| {
            let button = root.button("Player action");
            button.request_focus();
            assert!(!app.take_player_pause_shortcut(root.ctx()));
        });
        output.textures_delta.clear();
    }

    #[test]
    fn background_updates_do_not_draw_catalog_or_move_player_focus() {
        let ctx = egui::Context::default();
        let mut app = KattriApp::new(ctx.clone());
        let player_focus = egui::Id::unique("player-pause");
        let mut output = ctx.run_ui(egui::RawInput::default(), |root| {
            root.ctx()
                .memory_mut(|memory| memory.request_focus(player_focus));
            let layout_before = root.min_rect();
            app.update_background(root.ctx());
            assert_eq!(root.min_rect(), layout_before);
            assert_eq!(
                root.ctx().memory(|memory| memory.focused()),
                Some(player_focus)
            );
        });
        assert!(
            output
                .shapes
                .iter()
                .all(|shape| matches!(shape.shape, egui::Shape::Noop))
        );
        output.textures_delta.clear();
    }

    #[test]
    fn background_updates_deliver_worker_errors_without_drawing_the_catalog() {
        let ctx = egui::Context::default();
        let mut app = KattriApp::new(ctx.clone());
        let (worker, events) = ApiWorker::with_event_sender_for_test();
        app.worker = Some(worker);
        events
            .try_send(WorkerEvent::ConnectionFailed {
                request_id: None,
                message: "offline fixture".to_owned(),
            })
            .expect("queue fixture");
        let mut output = ctx.run_ui(egui::RawInput::default(), |root| {
            app.update_background(root.ctx());
        });
        assert!(
            matches!(app.connection, ConnectionStatus::Disconnected { error: Some(ref error) } if error == "offline fixture")
        );
        assert!(
            output
                .shapes
                .iter()
                .all(|shape| matches!(shape.shape, egui::Shape::Noop))
        );
        output.textures_delta.clear();
    }
}

#[cfg(test)]
mod segment_request_tests {
    use super::segments_needed;
    use kattri_settings::PlaybackSettings;

    #[test]
    fn segments_are_requested_only_when_a_button_switch_is_on() {
        assert!(!segments_needed(&PlaybackSettings::default()));

        let intro = PlaybackSettings {
            skip_intro: true,
            ..PlaybackSettings::default()
        };
        assert!(segments_needed(&intro));

        let outro = PlaybackSettings {
            skip_outro: true,
            ..PlaybackSettings::default()
        };
        assert!(segments_needed(&outro));

        // An automatic switch alone has no button, so the request stays off.
        let automatic_only = PlaybackSettings {
            auto_skip_intro: true,
            auto_skip_outro: true,
            ..PlaybackSettings::default()
        };
        assert!(!segments_needed(&automatic_only));

        let out_of_range = PlaybackSettings {
            skip_outro: true,
            outro_delay_ms: 60_001,
            ..PlaybackSettings::default()
        };
        assert!(!segments_needed(&out_of_range));
    }
}

#[cfg(test)]
mod page_query_tests {
    use super::Page;

    #[test]
    fn library_categories_map_to_jellyfin_server_item_types() {
        assert_eq!(Page::Home.item_types(), None);
        assert_eq!(Page::Movies.item_types(), Some(["Movie"].as_slice()));
        assert_eq!(Page::Series.item_types(), Some(["Series"].as_slice()));
        assert_eq!(
            Page::Music.item_types(),
            Some(["Audio", "MusicAlbum"].as_slice())
        );
        assert_eq!(
            Page::Photos.item_types(),
            Some(["Photo", "Folder", "CollectionFolder"].as_slice())
        );
        assert_eq!(Page::Playlists.item_types(), Some(["Playlist"].as_slice()));
    }

    #[test]
    fn non_catalog_pages_do_not_issue_item_type_filters() {
        assert_eq!(Page::Downloads.item_types(), None);
        assert_eq!(Page::LiveTv.item_types(), None);
        assert_eq!(Page::Settings.item_types(), None);
    }
}

#[cfg(test)]
mod home_navigation_tests {
    use super::{AppView, BrowseContext, KattriApp, Page};
    use crate::catalog::MediaItem;
    use crate::ui::home::{HomeAction, HomeViewTarget, MyMediaTile};
    use jellyfin_api::BaseItemDto;

    fn catalog_item(id: &str, item_type: &str) -> BaseItemDto {
        BaseItemDto {
            id: id.into(),
            name: id.into(),
            item_type: Some(item_type.into()),
            overview: None,
            production_year: None,
            official_rating: None,
            run_time_ticks: None,
            premiere_date: None,
            series_name: None,
            parent_index_number: None,
            index_number: None,
            series_id: None,
            series_primary_image_tag: None,
            genres: None,
            people: None,
            primary_image_aspect_ratio: None,
            image_tags: None,
            backdrop_image_tags: None,
            user_data: None,
        }
    }

    #[test]
    fn library_tiles_open_their_typed_parent_in_the_catalog() {
        let mut app = KattriApp::new(egui::Context::default());
        app.apply_home_action(HomeAction::OpenLibrary(MyMediaTile {
            id: "movies-view".into(),
            name: "Movies".into(),
            target: HomeViewTarget::Movies,
        }));

        assert_eq!(app.view, AppView::Library);
        assert_eq!(app.page, Page::Movies);
        assert_eq!(
            app.browse_context,
            Some(BrowseContext {
                parent_id: "movies-view".into(),
                title: "Movies".into(),
                is_collection: false,
            })
        );
    }

    #[test]
    fn collection_tiles_open_their_children_and_playlist_tiles_open_the_playlist_page() {
        let mut app = KattriApp::new(egui::Context::default());
        app.apply_home_action(HomeAction::OpenCollection {
            id: "box-set".into(),
            name: "Collection".into(),
        });
        assert_eq!(app.page, Page::Home);
        assert_eq!(
            app.browse_context,
            Some(BrowseContext {
                parent_id: "box-set".into(),
                title: "Collection".into(),
                is_collection: true,
            })
        );

        app.browse_context = None;
        app.view = AppView::Home;
        app.apply_home_action(HomeAction::OpenLibrary(MyMediaTile {
            id: "playlists-view".into(),
            name: "Playlists".into(),
            target: HomeViewTarget::Playlists,
        }));
        assert_eq!(app.view, AppView::Library);
        assert_eq!(app.page, Page::Playlists);
        assert_eq!(app.browse_context, None);
    }

    #[test]
    fn nested_box_set_activation_opens_it_as_a_collection() {
        let mut app = KattriApp::new(egui::Context::default());
        app.items = vec![MediaItem::from_api(catalog_item("nested", "BoxSet"))];

        app.activate_catalog_item("nested".into());

        assert_eq!(app.selected_item_id, None);
        assert_eq!(
            app.browse_context,
            Some(BrowseContext {
                parent_id: "nested".into(),
                title: "nested".into(),
                is_collection: true,
            })
        );
    }

    #[test]
    fn collection_runtime_accumulates_across_pages() {
        let mut app = KattriApp::new(egui::Context::default());
        let mut first = catalog_item("first", "Movie");
        first.run_time_ticks = Some(3_600 * 10_000_000);
        app.accumulate_collection_runtime(&[first]);
        app.accumulate_collection_runtime(&[catalog_item("second", "Movie")]);

        assert_eq!(app.collection_runtime.total_seconds(), 3_600);
        assert_eq!(app.collection_runtime.items_with_runtime, 1);
        assert_eq!(app.collection_runtime.items_without_runtime, 1);
    }

    #[test]
    fn back_clears_item_details_before_returning_from_catalog_to_home() {
        let mut app = KattriApp::new(egui::Context::default());
        app.view = AppView::Library;
        app.selected_item_id = Some("selected-item".into());

        app.navigate_back();
        assert_eq!(app.selected_item_id, None);
        assert_eq!(app.view, AppView::Library);

        app.navigate_back();
        assert_eq!(app.view, AppView::Home);
    }
}

#[cfg(test)]
mod watched_state_event_tests {
    use super::{KattriApp, WatchedItemState};
    use crate::worker::{ApiWorker, WorkerEvent};
    use jellyfin_api::{BaseItemDto, UserItemDataDto};

    fn item(id: &str, played: bool) -> BaseItemDto {
        BaseItemDto {
            id: id.into(),
            name: id.into(),
            user_data: Some(UserItemDataDto {
                played: Some(played),
                ..Default::default()
            }),
            ..serde_json::from_str(r#"{"Id":"template","Name":"template"}"#)
                .expect("valid minimal item fixture")
        }
    }

    #[test]
    fn episode_choice_is_saved_under_series_when_its_record_arrives_late() {
        const SERIES: &str = "9a1f0c2e5b7d4e6f8a9b0c1d2e3f4a5b";
        let mut app = KattriApp::new(egui::Context::default());
        app.settings = crate::settings_state::SettingsState::open(None, None);
        let (worker, events) = ApiWorker::with_event_sender_for_test();
        app.worker = Some(worker);
        app.selected_item_id = Some("episode-1".into());
        app.details_request_id = 7;
        app.details_loading = true;
        // Before the record arrives the playing item's series is unknown, so the stream waits.
        assert_eq!(app.playing_series_id("episode-1"), None);

        let mut episode = item("episode-1", false);
        episode.series_id = Some(SERIES.into());
        events
            .try_send(WorkerEvent::ItemDetailsLoaded {
                request_id: 7,
                item: Box::new(episode),
            })
            .expect("queue item record");
        app.receive_worker_events(&egui::Context::default());

        let series = app.playing_series_id("episode-1").flatten();
        assert_eq!(series.as_deref(), Some(SERIES));
        let track = kattri_client_core::language_memory::TrackLanguage::from_raw(
            Some("ja"),
            Some("Japanese"),
        );
        app.record_audio_choice(series.as_deref(), &track);

        let saved = &app.settings.settings().series_languages;
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].series_id, SERIES);
        assert_eq!(saved[0].audio_language.as_deref(), Some("ja"));
    }

    #[test]
    fn movie_record_resolves_no_series_so_nothing_is_written() {
        let mut app = KattriApp::new(egui::Context::default());
        app.settings = crate::settings_state::SettingsState::open(None, None);
        app.selected_item_id = Some("movie-9".into());
        app.selected_details = Some(item("movie-9", false));

        let series = app.playing_series_id("movie-9");
        assert_eq!(series, Some(None));
        let track = kattri_client_core::language_memory::TrackLanguage::from_raw(Some("ja"), None);
        app.record_audio_choice(series.flatten().as_deref(), &track);

        assert!(app.settings.settings().series_languages.is_empty());
    }

    fn app_with_pending_toggle(
        item_id: &str,
    ) -> (KattriApp, tokio::sync::mpsc::Sender<WorkerEvent>) {
        let mut app = KattriApp::new(egui::Context::default());
        let (worker, events) = ApiWorker::with_event_sender_for_test();
        app.worker = Some(worker);
        app.selected_item_id = Some(item_id.into());
        app.selected_details = Some(item(item_id, false));
        let mut toggle = crate::ui::detail::WatchedToggle::from_user_data(
            app.selected_details
                .as_ref()
                .and_then(|details| details.user_data.as_ref()),
        );
        assert_eq!(toggle.begin(), Some(true));
        app.watched = Some(WatchedItemState {
            item_id: item_id.into(),
            request_id: 41,
            toggle,
            error: None,
        });
        (app, events)
    }

    #[test]
    fn watched_success_refreshes_user_data_for_the_selected_detail() {
        let (mut app, events) = app_with_pending_toggle("movie-1");
        events
            .try_send(WorkerEvent::WatchedStateUpdated {
                request_id: 41,
                item_id: "movie-1".into(),
                played: true,
                user_data: Box::new(UserItemDataDto {
                    played: Some(true),
                    play_count: Some(1),
                    ..Default::default()
                }),
            })
            .expect("queue watched update");

        app.receive_worker_events(&egui::Context::default());

        let user_data = app
            .selected_details
            .as_ref()
            .and_then(|details| details.user_data.as_ref())
            .expect("server returned refreshed UserData");
        assert_eq!(user_data.played, Some(true));
        assert_eq!(user_data.play_count, Some(1));
        let view = app.watched_view();
        assert!(view.played);
        assert!(!view.pending);
        assert_eq!(view.error, None);
    }

    #[test]
    fn late_watched_success_does_not_replace_another_selected_item_data() {
        let (mut app, events) = app_with_pending_toggle("movie-1");
        app.selected_item_id = Some("movie-2".into());
        app.selected_details = Some(item("movie-2", false));
        events
            .try_send(WorkerEvent::WatchedStateUpdated {
                request_id: 41,
                item_id: "movie-1".into(),
                played: true,
                user_data: Box::new(UserItemDataDto {
                    played: Some(true),
                    play_count: Some(1),
                    ..Default::default()
                }),
            })
            .expect("queue stale watched update");

        app.receive_worker_events(&egui::Context::default());

        let user_data = app
            .selected_details
            .as_ref()
            .and_then(|details| details.user_data.as_ref())
            .expect("current item UserData remains present");
        assert_eq!(user_data.played, Some(false));
        assert_eq!(user_data.play_count, None);
        assert!(!app.watched_view().played);
    }
}

#[cfg(test)]
mod favorite_state_event_tests {
    use super::{FavoriteItemState, KattriApp};
    use crate::worker::{ApiWorker, WorkerEvent};
    use jellyfin_api::{BaseItemDto, FavoriteToggle, UserItemDataDto};

    fn app_with_item(
        item_id: &str,
        favorite: bool,
    ) -> (KattriApp, tokio::sync::mpsc::Sender<WorkerEvent>) {
        let mut app = KattriApp::new(egui::Context::default());
        let (worker, events) = ApiWorker::with_event_sender_for_test();
        app.worker = Some(worker);
        app.selected_item_id = Some(item_id.into());
        app.selected_details = Some(BaseItemDto {
            id: item_id.into(),
            name: item_id.into(),
            user_data: Some(UserItemDataDto {
                is_favorite: Some(favorite),
                ..Default::default()
            }),
            ..serde_json::from_str(r#"{"Id":"template","Name":"template"}"#)
                .expect("valid item fixture")
        });
        (app, events)
    }

    #[test]
    fn favorite_updates_optimistically_and_reverts_after_failure() {
        let (mut app, events) = app_with_item("photo-1", false);
        app.request_favorite_toggle();
        assert!(app.favorite_view().favorite);
        assert!(app.favorite_view().pending);

        let request_id = app.favorite.as_ref().expect("favorite request").request_id;
        events
            .try_send(WorkerEvent::FavoriteStateFailed {
                request_id,
                item_id: "photo-1".into(),
                message: "HTTP 500".into(),
            })
            .expect("queue favorite failure");
        app.receive_worker_events(&egui::Context::default());

        let view = app.favorite_view();
        assert!(!view.favorite);
        assert!(!view.pending);
        assert!(view.error.is_some());
    }

    #[test]
    fn second_favorite_toggle_waits_for_first_response_then_reconciles_server_state() {
        let (mut app, events) = app_with_item("music-1", false);
        app.request_favorite_toggle();
        app.request_favorite_toggle();
        assert!(!app.favorite_view().favorite);
        assert!(app.favorite_view().pending);

        let first_request = app.favorite.as_ref().expect("first request").request_id;
        events
            .try_send(WorkerEvent::FavoriteStateUpdated {
                request_id: first_request,
                item_id: "music-1".into(),
                favorite: true,
                user_data: Box::new(UserItemDataDto {
                    is_favorite: Some(true),
                    ..Default::default()
                }),
            })
            .expect("queue first favorite response");
        app.receive_worker_events(&egui::Context::default());

        let second_request = app.favorite.as_ref().expect("second request").request_id;
        assert_ne!(second_request, first_request);
        assert!(app.favorite_view().pending);
        events
            .try_send(WorkerEvent::FavoriteStateUpdated {
                request_id: second_request,
                item_id: "music-1".into(),
                favorite: false,
                user_data: Box::new(UserItemDataDto {
                    is_favorite: Some(false),
                    ..Default::default()
                }),
            })
            .expect("queue second favorite response");
        app.receive_worker_events(&egui::Context::default());
        assert!(!app.favorite_view().favorite);
        assert!(!app.favorite_view().pending);
    }

    #[test]
    fn late_favorite_response_does_not_replace_another_detail_item() {
        let (mut app, events) = app_with_item("movie-1", false);
        app.favorite = Some(FavoriteItemState {
            item_id: "movie-1".into(),
            request_id: 51,
            toggle: FavoriteToggle::new(false),
            error: None,
        });
        app.favorite
            .as_mut()
            .expect("favorite state")
            .toggle
            .toggle();
        app.selected_item_id = Some("movie-2".into());
        app.selected_details.as_mut().expect("details").id = "movie-2".into();
        app.selected_details.as_mut().expect("details").user_data = Some(UserItemDataDto {
            is_favorite: Some(false),
            ..Default::default()
        });
        events
            .try_send(WorkerEvent::FavoriteStateUpdated {
                request_id: 51,
                item_id: "movie-1".into(),
                favorite: true,
                user_data: Box::new(UserItemDataDto {
                    is_favorite: Some(true),
                    ..Default::default()
                }),
            })
            .expect("queue stale favorite update");
        app.receive_worker_events(&egui::Context::default());

        assert_eq!(
            app.selected_details
                .as_ref()
                .and_then(|item| item.user_data.as_ref())
                .and_then(|data| data.is_favorite),
            Some(false)
        );
        assert!(!app.favorite_view().favorite);
    }
}

#[cfg(test)]
mod live_tv_event_tests {
    use super::KattriApp;
    use crate::worker::{ApiWorker, WorkerEvent};
    use jellyfin_api::{BaseItemDto, ErrorClass, LiveTvChannel, LiveTvChannelPage};
    use std::collections::HashMap;

    fn channel(id: &str) -> LiveTvChannel {
        LiveTvChannel {
            item: BaseItemDto {
                id: id.to_owned(),
                name: format!("Channel {id}"),
                ..serde_json::from_str(r#"{"Id":"template","Name":"template"}"#)
                    .expect("valid channel item fixture")
            },
            channel_number: Some("7".to_owned()),
            number: None,
            channel_type: None,
            current_program: None,
        }
    }

    #[test]
    fn live_tv_channels_load_into_the_page_and_empty_setup_is_not_an_error() {
        let mut app = KattriApp::new(egui::Context::default());
        let (worker, events) = ApiWorker::with_event_sender_for_test();
        app.worker = Some(worker);
        app.live_tv_request_id = 44;
        app.live_tv_loading = true;
        events
            .try_send(WorkerEvent::LiveTvChannelsLoaded {
                request_id: 44,
                page: LiveTvChannelPage {
                    items: vec![channel("channel-1")],
                    total_record_count: Some(1),
                    start_index: 0,
                },
            })
            .expect("queue Live TV page");

        app.receive_worker_events(&egui::Context::default());

        assert!(app.live_tv_loaded);
        assert!(!app.live_tv_loading);
        assert_eq!(app.live_tv_channels.len(), 1);
        assert!(!app.live_tv_not_set_up);
        assert_eq!(app.live_tv_error, None);

        app.live_tv_request_id = 45;
        app.live_tv_loading = true;
        events
            .try_send(WorkerEvent::LiveTvChannelsFailed {
                request_id: 45,
                error_class: ErrorClass::NotFound,
            })
            .expect("queue unavailable Live TV result");
        app.receive_worker_events(&egui::Context::default());
        assert!(app.live_tv_not_set_up);
        assert_eq!(app.live_tv_error, None);
    }

    #[test]
    fn retry_live_tv_guide_reissues_the_failed_channel_batch() {
        let mut app = KattriApp::new(egui::Context::default());
        let (worker, events) = ApiWorker::with_event_sender_for_test();
        app.worker = Some(worker);
        app.live_tv_channels = vec![channel("channel-1"), channel("channel-2")];
        app.live_tv_guide_requested_channel_ids = vec!["channel-1".into(), "channel-2".into()];
        app.live_tv_guide_request_id = 44;
        app.live_tv_guide_error = Some(ErrorClass::Unexpected);

        app.retry_live_tv_guide();

        assert!(app.live_tv_guide_loading);
        assert_eq!(app.live_tv_guide_error, None);
        assert_ne!(app.live_tv_guide_request_id, 44);
        assert_eq!(
            app.live_tv_guide_requested_channel_ids,
            ["channel-1", "channel-2"]
        );
        drop(events);
    }

    #[test]
    fn live_tv_tune_failure_is_represented_on_the_channel_page() {
        let mut app = KattriApp::new(egui::Context::default());
        let (worker, events) = ApiWorker::with_event_sender_for_test();
        app.worker = Some(worker);
        app.live_tv_channels = vec![channel("channel-1")];
        app.playback_start_request_id = 92;
        app.playback_start_loading = true;
        app.live_tv_tune_request_id = Some(92);
        app.selected_item_id = Some("channel-1".into());

        events
            .try_send(WorkerEvent::PlaybackPrepareFailed {
                request_id: 92,
                message: "All tuners are busy.".into(),
            })
            .expect("queue live tune failure");
        app.receive_worker_events(&egui::Context::default());

        assert_eq!(app.live_tv_tune_request_id, None);
        assert_eq!(app.live_tv_notice.as_deref(), Some("All tuners are busy."));
        assert!(app.live_tv_retry_available);
        assert!(!app.playback_start_loading);
    }

    #[test]
    fn unexpected_live_stream_end_shows_channel_stopped_and_enables_retry() {
        let mut app = KattriApp::new(egui::Context::default());
        app.live_tv_selected_channel_id = Some("channel-1".into());
        app.live_tv_tune_request_id = Some(92);
        app.playback_start_loading = true;
        app.playback_start_status = Some("Tuning live channel…".into());

        app.on_live_tv_stream_ended("channel-1");

        assert_eq!(app.live_tv_notice.as_deref(), Some("Channel stopped."));
        assert!(app.live_tv_retry_available);
        assert!(!app.playback_start_loading);
        assert_eq!(app.playback_start_status, None);
        assert_eq!(app.live_tv_tune_request_id, None);

        app.on_live_tv_stream_ended("old-channel");
        assert_eq!(app.live_tv_notice.as_deref(), Some("Channel stopped."));
    }

    #[test]
    fn live_tv_logo_success_is_cached_and_failure_keeps_placeholder_state() {
        let mut app = KattriApp::new(egui::Context::default());
        let (worker, events) = ApiWorker::with_event_sender_for_test();
        app.worker = Some(worker);
        let mut logo_channel = channel("logo-channel");
        logo_channel.item.image_tags = Some(HashMap::from([(
            "Primary".to_owned(),
            Some("tag-1".to_owned()),
        )]));
        app.live_tv_channels = vec![logo_channel];
        app.live_tv_logo_pending
            .insert(72, ("logo-channel".into(), "tag-1".into()));

        events
            .try_send(WorkerEvent::LiveTvLogoLoaded {
                request_id: 72,
                channel_id: "logo-channel".into(),
                width: 2,
                height: 2,
                rgba: vec![255; 16],
            })
            .expect("send logo success");
        app.receive_worker_events(&egui::Context::default());
        assert!(app.live_tv_channel_logos.contains_key("logo-channel"));

        app.live_tv_logo_pending
            .insert(73, ("missing-logo".into(), "tag-404".into()));
        events
            .try_send(WorkerEvent::LiveTvLogoFailed {
                request_id: 73,
                channel_id: "missing-logo".into(),
            })
            .expect("send logo 404 fallback");
        app.receive_worker_events(&egui::Context::default());
        assert!(!app.live_tv_channel_logos.contains_key("missing-logo"));
        assert!(!app.live_tv_logo_pending.contains_key(&73));
    }

    #[test]
    fn stale_live_tv_channel_response_is_ignored() {
        let mut app = KattriApp::new(egui::Context::default());
        let (worker, events) = ApiWorker::with_event_sender_for_test();
        app.worker = Some(worker);
        app.live_tv_request_id = 92;
        app.live_tv_loading = true;
        events
            .try_send(WorkerEvent::LiveTvChannelsLoaded {
                request_id: 91,
                page: LiveTvChannelPage {
                    items: vec![channel("stale")],
                    total_record_count: Some(1),
                    start_index: 0,
                },
            })
            .expect("queue stale Live TV page");

        app.receive_worker_events(&egui::Context::default());

        assert!(app.live_tv_loading);
        assert!(!app.live_tv_loaded);
        assert!(app.live_tv_channels.is_empty());
    }

    #[test]
    fn invalidated_live_tv_search_response_is_ignored() {
        let mut app = KattriApp::new(egui::Context::default());
        let (worker, events) = ApiWorker::with_event_sender_for_test();
        app.worker = Some(worker);
        app.live_tv_search_request_id = Some(104);
        app.live_tv_search_loading = true;
        app.invalidate_live_tv_search();
        events
            .try_send(WorkerEvent::LiveTvSearchResultsLoaded {
                request_id: 104,
                items: vec![channel("stale-channel").item],
                start_index: 0,
                total_record_count: Some(1),
            })
            .expect("queue invalidated channel search result");

        app.receive_worker_events(&egui::Context::default());

        assert!(!app.live_tv_search_loading);
        assert!(app.live_tv_search_results.is_empty());
    }

    #[test]
    fn live_tv_search_pages_append_results_and_advance_the_offset() {
        let mut app = KattriApp::new(egui::Context::default());
        let (worker, events) = ApiWorker::with_event_sender_for_test();
        app.worker = Some(worker);
        app.search_browse_group = Some(crate::ui::search::SearchGroup::Channels);
        app.submitted_search = Some("news".into());
        app.live_tv_search_request_id = Some(301);
        app.live_tv_search_loading = true;
        events
            .try_send(WorkerEvent::LiveTvSearchResultsLoaded {
                request_id: 301,
                items: vec![channel("channel-0").item, channel("channel-1").item],
                start_index: 0,
                total_record_count: Some(4),
            })
            .expect("queue first channel page");

        app.receive_worker_events(&egui::Context::default());
        assert_eq!(app.live_tv_search_results.len(), 2);
        assert_eq!(app.live_tv_search_start_index, 2);
        app.next_live_tv_search_page();
        assert!(app.live_tv_search_loading);
        assert_eq!(app.live_tv_search_start_index, 2);
        let next_request_id = app.live_tv_search_request_id.expect("next page request ID");
        events
            .try_send(WorkerEvent::LiveTvSearchResultsLoaded {
                request_id: next_request_id,
                items: vec![channel("channel-2").item, channel("channel-3").item],
                start_index: 2,
                total_record_count: Some(4),
            })
            .expect("queue second channel page");

        app.receive_worker_events(&egui::Context::default());
        assert_eq!(app.live_tv_search_results.len(), 4);
        assert_eq!(app.live_tv_search_start_index, 4);
        assert_eq!(
            app.live_tv_search_results
                .iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>(),
            ["channel-0", "channel-1", "channel-2", "channel-3"]
        );
    }

    #[test]
    fn live_tv_pages_append_in_server_order_without_duplicate_channels() {
        let mut app = KattriApp::new(egui::Context::default());
        let (worker, events) = ApiWorker::with_event_sender_for_test();
        app.worker = Some(worker);
        app.live_tv_request_id = 101;
        app.live_tv_loading = true;
        events
            .try_send(WorkerEvent::LiveTvChannelsLoaded {
                request_id: 101,
                page: LiveTvChannelPage {
                    items: vec![channel("first"), channel("second")],
                    total_record_count: Some(150),
                    start_index: 0,
                },
            })
            .expect("queue first channel page");
        app.receive_worker_events(&egui::Context::default());

        app.live_tv_request_id = 102;
        app.live_tv_requested_start_index = 100;
        app.live_tv_loading = true;
        events
            .try_send(WorkerEvent::LiveTvChannelsLoaded {
                request_id: 102,
                page: LiveTvChannelPage {
                    items: vec![channel("second"), channel("third")],
                    total_record_count: Some(150),
                    start_index: 100,
                },
            })
            .expect("queue next channel page");
        app.receive_worker_events(&egui::Context::default());

        assert_eq!(
            app.live_tv_channels
                .iter()
                .map(|channel| channel.item.id.as_str())
                .collect::<Vec<_>>(),
            ["first", "second", "third"]
        );
        assert_eq!(app.live_tv_total_count, Some(150));
        assert!(!app.live_tv_loading);
    }
}

#[cfg(test)]
mod music_queue_app_tests {
    use super::{
        KattriApp, MusicAction, PlaylistEntry, Queue, StoredQueue, music_queue_report_context,
    };
    use crate::worker::{ApiWorker, WorkerEvent};
    use jellyfin_api::BaseItemDto;
    use jellyfin_api::session_reporting::RepeatMode;
    use std::time::Duration;

    fn audio(id: &str) -> BaseItemDto {
        BaseItemDto {
            id: id.to_owned(),
            name: format!("Track {id}"),
            item_type: Some("Audio".to_owned()),
            ..serde_json::from_str(r#"{"Id":"template","Name":"template"}"#)
                .expect("valid minimal item fixture")
        }
    }

    fn indexed_audio(id: &str, index: i32) -> BaseItemDto {
        BaseItemDto {
            index_number: Some(index),
            ..audio(id)
        }
    }

    fn playlist_entry(item_id: &str, entry_id: &str) -> PlaylistEntry {
        PlaylistEntry {
            item: audio(item_id),
            playlist_item_id: Some(entry_id.to_owned()),
        }
    }

    #[test]
    fn selected_audio_can_be_appended_or_inserted_next_with_a_safe_label() {
        let mut app = KattriApp::new(egui::Context::default());
        app.selected_item_id = Some("track-1".to_owned());
        app.selected_details = Some(audio("track-1"));

        app.queue_selected_audio(false);
        assert_eq!(app.music_queue.items()[0].id, "track-1");
        assert_eq!(
            app.music_track_names.get("track-1").map(String::as_str),
            Some("Track track-1")
        );

        app.music_queue
            .add(jellyfin_api::session_reporting::QueueItem::new("track-2"))
            .expect("add");
        app.music_queue.select(0).expect("select current track");
        app.selected_item_id = Some("track-3".to_owned());
        app.selected_details = Some(audio("track-3"));
        app.queue_selected_audio(true);
        assert_eq!(
            app.music_queue
                .items()
                .iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>(),
            ["track-1", "track-3", "track-2"]
        );
    }

    #[test]
    fn music_ui_actions_update_queue_shuffle_repeat_and_remove_safely() {
        let mut app = KattriApp::new(egui::Context::default());
        app.apply_music_action(MusicAction::AddToQueue {
            item_id: "track-1".into(),
        });
        app.apply_music_action(MusicAction::AddToQueue {
            item_id: "track-2".into(),
        });
        app.apply_music_action(MusicAction::SetRepeat(RepeatMode::All));
        app.apply_music_action(MusicAction::SetShuffle(true));
        assert_eq!(app.music_queue.repeat(), RepeatMode::All);
        assert!(app.music_queue.is_shuffled());

        app.apply_music_action(MusicAction::Remove {
            queue_index: 0,
            item_id: "wrong-id".into(),
        });
        assert_eq!(app.music_queue.len(), 2);
        app.apply_music_action(MusicAction::Remove {
            queue_index: 0,
            item_id: app.music_queue.items()[0].id.clone(),
        });
        assert_eq!(app.music_queue.len(), 1);
    }

    #[test]
    fn playback_report_queue_context_is_only_attached_for_the_current_queued_track() {
        let mut app = KattriApp::new(egui::Context::default());
        for item_id in ["track-1", "track-2", "track-3"] {
            app.music_queue
                .add(jellyfin_api::session_reporting::QueueItem::new(item_id))
                .expect("append track");
        }
        app.music_queue.select(1).expect("select current track");
        app.music_queue.set_repeat(RepeatMode::One);
        app.music_queue.set_shuffle(true);

        assert!(music_queue_report_context(&app.music_queue, "unrelated").is_none());
        let (repeat, order, items) = music_queue_report_context(&app.music_queue, "track-2")
            .expect("current queue item includes report context");
        assert_eq!(repeat, RepeatMode::One);
        assert_eq!(
            order,
            jellyfin_api::session_reporting::PlaybackOrder::Shuffle
        );
        assert_eq!(items.first().map(|item| item.id.as_str()), Some("track-2"));
        assert_eq!(items.len(), 3);
    }

    #[test]
    fn ended_music_track_moves_to_the_next_queue_entry_and_retries_repeat_one_once() {
        let mut app = KattriApp::new(egui::Context::default());
        let worker = ApiWorker::queued_for_test();
        app.worker = Some(worker);
        for item_id in ["track-1", "track-2"] {
            app.music_queue
                .add(jellyfin_api::session_reporting::QueueItem::new(item_id))
                .expect("append track");
        }
        app.music_queue.select(0).expect("select first track");
        app.selected_item_id = Some("track-1".to_owned());
        app.selected_details = Some(audio("track-1"));

        app.on_music_track_ended("unrelated-video", false);
        assert_eq!(
            app.music_queue.current().map(|item| item.id.as_str()),
            Some("track-1")
        );

        app.on_music_track_ended("track-1", false);

        assert_eq!(
            app.music_queue.current().map(|item| item.id.as_str()),
            Some("track-2")
        );
        assert_eq!(app.selected_item_id.as_deref(), Some("track-2"));
        assert_eq!(
            app.music_queue_auto_play_item_id.as_deref(),
            Some("track-2")
        );

        app.music_queue.select(0).expect("return to first track");
        app.music_queue.set_repeat(RepeatMode::One);
        app.selected_item_id = Some("track-1".to_owned());
        app.music_queue_auto_play_item_id = None;
        app.on_music_track_ended("track-1", true);
        assert_eq!(
            app.music_queue.current().map(|item| item.id.as_str()),
            Some("track-1")
        );
        assert_eq!(
            app.music_queue_auto_play_item_id.as_deref(),
            Some("track-1")
        );

        app.on_music_track_ended("track-1", true);
        assert_eq!(
            app.music_queue.current().map(|item| item.id.as_str()),
            Some("track-2")
        );
        assert_eq!(
            app.music_queue_auto_play_item_id.as_deref(),
            Some("track-2")
        );
    }

    #[test]
    fn music_previous_is_bound_to_active_queue_item_and_honors_three_second_rule() {
        let mut app = KattriApp::new(egui::Context::default());
        app.worker = Some(ApiWorker::queued_for_test());
        for item_id in ["track-1", "track-2"] {
            app.music_queue
                .add(jellyfin_api::session_reporting::QueueItem::new(item_id))
                .expect("append track");
        }
        app.music_queue.select(1).expect("select second track");
        app.selected_item_id = Some("track-2".to_owned());

        assert!(!app.on_music_previous("video-not-in-queue", Duration::ZERO));
        assert_eq!(app.music_queue_current_item_id(), Some("track-2"));

        assert!(app.on_music_previous("track-2", Duration::from_millis(3001)));
        assert_eq!(app.music_queue_current_item_id(), Some("track-2"));

        assert!(!app.on_music_previous("track-2", Duration::from_secs(3)));
        assert_eq!(app.music_queue_current_item_id(), Some("track-1"));
        assert_eq!(app.selected_item_id.as_deref(), Some("track-1"));
        assert_eq!(
            app.music_queue_auto_play_item_id.as_deref(),
            Some("track-1")
        );
    }

    #[test]
    fn music_next_advances_only_the_active_queue_item_and_reports_the_end() {
        let mut app = KattriApp::new(egui::Context::default());
        app.worker = Some(ApiWorker::queued_for_test());
        for item_id in ["track-1", "track-2"] {
            app.music_queue
                .add(jellyfin_api::session_reporting::QueueItem::new(item_id))
                .expect("append track");
        }
        app.music_queue.select(0).expect("select first track");
        app.selected_item_id = Some("track-1".to_owned());

        app.on_music_next("unrelated-video");
        assert_eq!(app.music_queue_current_item_id(), Some("track-1"));

        app.on_music_next("track-1");
        assert_eq!(app.music_queue_current_item_id(), Some("track-2"));
        assert_eq!(app.selected_item_id.as_deref(), Some("track-2"));

        app.on_music_next("track-2");
        assert_eq!(app.music_queue_current_item_id(), Some("track-2"));
        assert_eq!(
            app.music_queue_notice.as_deref(),
            Some("The music queue has finished.")
        );
    }

    #[test]
    fn playlist_items_reject_stale_results_and_queue_duplicate_audio_entries() {
        let mut app = KattriApp::new(egui::Context::default());
        let (worker, sender) = ApiWorker::with_event_sender_for_test();
        app.worker = Some(worker);
        app.opened_playlist_id = Some("playlist-1".into());
        app.playlist_items_request_id = 82;
        app.playlist_items_loading = true;
        sender
            .try_send(WorkerEvent::PlaylistItemsLoaded {
                request_id: 81,
                playlist_id: "playlist-1".into(),
                entries: vec![playlist_entry("track-1", "entry-1")],
            })
            .expect("send stale fixture");
        app.receive_worker_events(&egui::Context::default());
        assert!(app.playlist_entries.is_empty());
        assert!(app.playlist_items_loading);

        sender
            .try_send(WorkerEvent::PlaylistItemsLoaded {
                request_id: 82,
                playlist_id: "playlist-1".into(),
                entries: vec![
                    playlist_entry("track-1", "entry-1"),
                    playlist_entry("track-1", "entry-2"),
                ],
            })
            .expect("send current fixture");
        app.receive_worker_events(&egui::Context::default());
        assert_eq!(app.playlist_entries.len(), 2);
        assert!(!app.playlist_items_loading);

        app.play_selected_playlist();
        assert_eq!(app.music_queue.len(), 2);
        assert_eq!(
            app.music_queue
                .items()
                .iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>(),
            ["track-1", "track-1"]
        );
        assert_eq!(app.music_queue_current_item_id(), Some("track-1"));
    }

    #[test]
    fn switching_playlists_clears_selected_candidates() {
        let mut app = KattriApp::new(egui::Context::default());
        app.worker = Some(ApiWorker::queued_for_test());
        app.opened_playlist_id = Some("playlist-1".into());
        app.playlist_ui_state
            .selected_media_ids
            .insert("movie-1".into());

        app.request_playlist_items("playlist-2".into());

        assert_eq!(app.opened_playlist_id.as_deref(), Some("playlist-2"));
        assert!(app.playlist_ui_state.selected_media_ids.is_empty());
    }

    #[test]
    fn restoring_saved_queue_reconciles_missing_audio_without_starting_playback() {
        let mut queue = Queue::with_seed(17);
        for id in ["track-1", "missing-track", "track-1"] {
            queue
                .add(jellyfin_api::session_reporting::QueueItem::new(id))
                .expect("append saved queue entry");
        }
        queue.select(2).expect("select saved current track");

        let mut app = KattriApp::new(egui::Context::default());
        app.settings = crate::settings_state::SettingsState::open(None, None);
        app.pending_stored_music_queue = Some(StoredQueue {
            queue,
            position_ms: 12_345,
        });
        app.music_queue_restore_request_id = 91;
        app.music_queue_restore_loading = true;
        let (worker, sender) = ApiWorker::with_event_sender_for_test();
        app.worker = Some(worker);
        sender
            .try_send(WorkerEvent::QueueItemsRestored {
                request_id: 91,
                items: vec![audio("track-1")],
            })
            .expect("send restored fixture");

        app.receive_worker_events(&egui::Context::default());

        assert_eq!(
            app.music_queue
                .items()
                .iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>(),
            ["track-1", "track-1"]
        );
        assert_eq!(app.music_queue.current_index(), Some(1));
        assert_eq!(app.music_queue_resume_position_ms, Some(12_345));
        assert_eq!(app.selected_item_id, None, "restore must not auto-play");
        assert!(
            app.music_queue_notice
                .as_deref()
                .is_some_and(|notice| { notice.contains("1 unavailable track was removed") })
        );
    }

    #[test]
    fn queue_changes_save_immediately_and_position_checkpoints_after_ten_seconds() {
        let dir =
            std::env::temp_dir().join(format!("kattri-app-queue-save-{}", uuid::Uuid::new_v4()));
        let mut app = KattriApp::new(egui::Context::default());
        app.settings = crate::settings_state::SettingsState::open(Some(&dir), None);
        app.music_queue
            .add(jellyfin_api::session_reporting::QueueItem::new("track-1"))
            .expect("add queue item");
        app.music_queue.select(0).expect("select queue item");
        app.ui_time = Duration::from_secs(1);
        app.persist_music_queue_if_needed();

        app.observe_music_queue_position(Some("track-1"), Some(17.5));
        app.ui_time = Duration::from_secs(9);
        app.persist_music_queue_if_needed();
        let stored = match app
            .settings
            .load_queue()
            .expect("profile path")
            .expect("load")
        {
            kattri_client_core::queue_store::StoredLoadOutcome::Loaded(stored) => stored,
            other => panic!("expected queue snapshot, got {other:?}"),
        };
        assert_eq!(
            stored.position_ms, 0,
            "position waits for the ten-second tick"
        );

        app.ui_time = Duration::from_secs(11);
        app.persist_music_queue_if_needed();
        let stored = match app
            .settings
            .load_queue()
            .expect("profile path")
            .expect("load")
        {
            kattri_client_core::queue_store::StoredLoadOutcome::Loaded(stored) => stored,
            other => panic!("expected queue snapshot, got {other:?}"),
        };
        assert_eq!(stored.position_ms, 17_500);
        drop(app);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn selected_non_audio_item_cannot_enter_the_music_queue() {
        let mut app = KattriApp::new(egui::Context::default());
        app.selected_item_id = Some("movie-1".to_owned());
        app.selected_details = Some(BaseItemDto {
            id: "movie-1".to_owned(),
            name: "Movie".to_owned(),
            item_type: Some("Movie".to_owned()),
            ..serde_json::from_str(r#"{"Id":"template","Name":"template"}"#)
                .expect("valid minimal item fixture")
        });

        app.queue_selected_audio(false);
        assert!(app.music_queue.is_empty());
    }

    #[test]
    fn album_tracks_are_sorted_then_queued_and_select_the_first_track() {
        let mut app = KattriApp::new(egui::Context::default());
        let (worker, events) = ApiWorker::with_event_sender_for_test();
        app.worker = Some(worker);
        app.selected_item_id = Some("album-1".to_owned());
        app.selected_details = Some(BaseItemDto {
            id: "album-1".to_owned(),
            name: "Album".to_owned(),
            item_type: Some("MusicAlbum".to_owned()),
            ..serde_json::from_str(r#"{"Id":"template","Name":"template"}"#)
                .expect("valid minimal item fixture")
        });
        app.music_album_tracks_request_id = 81;
        app.music_album_tracks_loading = true;
        events
            .try_send(WorkerEvent::MusicAlbumTracksLoaded {
                request_id: 80,
                album_id: "album-1".to_owned(),
                tracks: vec![indexed_audio("stale-track", 1)],
            })
            .expect("send stale album result");
        app.receive_worker_events(&egui::Context::default());
        assert!(app.music_queue.is_empty());
        assert_eq!(app.selected_item_id.as_deref(), Some("album-1"));

        events
            .try_send(WorkerEvent::MusicAlbumTracksLoaded {
                request_id: 81,
                album_id: "album-1".to_owned(),
                tracks: vec![indexed_audio("track-1", 1), indexed_audio("track-2", 2)],
            })
            .expect("send current album result");
        app.receive_worker_events(&egui::Context::default());

        assert_eq!(
            app.music_queue
                .items()
                .iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>(),
            ["track-1", "track-2"]
        );
        assert_eq!(
            app.music_queue.current().map(|item| item.id.as_str()),
            Some("track-1")
        );
        assert_eq!(app.selected_item_id.as_deref(), Some("track-1"));
        assert_eq!(
            app.music_track_names.get("track-1").map(String::as_str),
            Some("Track track-1")
        );
    }
}

#[cfg(test)]
mod playback_report_feedback_tests {
    use super::{
        MAX_CONSECUTIVE_PROGRESS_REPORT_FAILURES, PendingPlaybackReport, PlaybackReportKind,
        begin_playback_report_session, finish_playback_report, is_pending_playback_report,
        should_suppress_playback_report,
    };
    use std::collections::HashMap;

    #[test]
    fn three_consecutive_progress_failures_exhaust_only_progress_retries() {
        let current_session = 0;
        let mut pending = HashMap::from([(
            12,
            PendingPlaybackReport {
                kind: PlaybackReportKind::Progress,
                session_generation: current_session,
            },
        )]);
        let mut failures = 0;
        let mut feedback = None;

        assert!(finish_playback_report(
            &mut pending,
            current_session,
            &mut failures,
            &mut feedback,
            12,
            false,
        ));
        assert!(pending.is_empty());
        assert_eq!(failures, 1);
        assert_eq!(feedback, None);

        for request_id in [13, 14] {
            pending.insert(
                request_id,
                PendingPlaybackReport {
                    kind: PlaybackReportKind::Progress,
                    session_generation: current_session,
                },
            );
            assert!(finish_playback_report(
                &mut pending,
                current_session,
                &mut failures,
                &mut feedback,
                request_id,
                false,
            ));
        }

        assert_eq!(failures, MAX_CONSECUTIVE_PROGRESS_REPORT_FAILURES);
        assert!(should_suppress_playback_report(
            PlaybackReportKind::Progress,
            failures,
        ));
        assert!(
            feedback
                .as_deref()
                .is_some_and(|message| message.contains("Check the server connection"))
        );
        assert!(!is_pending_playback_report(&pending, 14));
    }

    #[test]
    fn successful_progress_report_resets_the_consecutive_failure_count() {
        let current_session = 1;
        let mut pending = HashMap::from([(
            15,
            PendingPlaybackReport {
                kind: PlaybackReportKind::Progress,
                session_generation: current_session,
            },
        )]);
        let mut failures = 2;
        let mut feedback = None;

        assert!(finish_playback_report(
            &mut pending,
            current_session,
            &mut failures,
            &mut feedback,
            15,
            true,
        ));
        assert_eq!(failures, 0);
        assert_eq!(feedback, None);
    }

    #[test]
    fn failure_of_progress_does_not_block_a_later_stop_report() {
        let current_session = 2;
        let mut pending = HashMap::from([(
            16,
            PendingPlaybackReport {
                kind: PlaybackReportKind::Progress,
                session_generation: current_session,
            },
        )]);
        let mut failures = 0;
        let mut feedback = None;

        assert!(finish_playback_report(
            &mut pending,
            current_session,
            &mut failures,
            &mut feedback,
            16,
            false,
        ));
        pending.insert(
            17,
            PendingPlaybackReport {
                kind: PlaybackReportKind::Stopped,
                session_generation: current_session,
            },
        );
        assert!(is_pending_playback_report(&pending, 17));
        assert!(finish_playback_report(
            &mut pending,
            current_session,
            &mut failures,
            &mut feedback,
            17,
            true,
        ));
        assert!(pending.is_empty());
        assert_eq!(failures, 1);
    }

    #[test]
    fn stale_arbitrary_report_result_is_ignored() {
        let current_session = 3;
        let mut pending = HashMap::from([(
            7,
            PendingPlaybackReport {
                kind: PlaybackReportKind::Progress,
                session_generation: current_session,
            },
        )]);
        let mut failures = 0;
        let mut feedback = Some("existing feedback".to_owned());

        assert!(!finish_playback_report(
            &mut pending,
            current_session,
            &mut failures,
            &mut feedback,
            99,
            false,
        ));
        assert_eq!(pending.len(), 1);
        assert_eq!(failures, 0);
        assert_eq!(feedback.as_deref(), Some("existing feedback"));
    }

    #[test]
    fn start_and_stop_failures_do_not_consume_progress_retry_budget() {
        let mut pending = HashMap::from([
            (
                20,
                PendingPlaybackReport {
                    kind: PlaybackReportKind::Started,
                    session_generation: 4,
                },
            ),
            (
                21,
                PendingPlaybackReport {
                    kind: PlaybackReportKind::Stopped,
                    session_generation: 4,
                },
            ),
        ]);
        let mut failures = 0;
        let mut feedback = None;

        for request_id in [20, 21] {
            assert!(finish_playback_report(
                &mut pending,
                4,
                &mut failures,
                &mut feedback,
                request_id,
                false,
            ));
        }
        assert_eq!(failures, 0);
        assert_eq!(
            feedback.as_deref(),
            Some("Jellyfin could not update the playback session.")
        );
    }

    #[test]
    fn exhausted_progress_budget_never_suppresses_start_or_stop() {
        for kind in [PlaybackReportKind::Started, PlaybackReportKind::Stopped] {
            assert!(!should_suppress_playback_report(
                kind,
                MAX_CONSECUTIVE_PROGRESS_REPORT_FAILURES,
            ));
        }
        assert!(should_suppress_playback_report(
            PlaybackReportKind::Progress,
            MAX_CONSECUTIVE_PROGRESS_REPORT_FAILURES,
        ));
    }

    #[test]
    fn starting_a_new_session_resets_budget_and_ignores_late_old_progress_failure() {
        let mut current_session = 41;
        let mut failures = MAX_CONSECUTIVE_PROGRESS_REPORT_FAILURES;
        let mut feedback = Some("old session retry message".to_owned());
        let mut pending = HashMap::from([(
            22,
            PendingPlaybackReport {
                kind: PlaybackReportKind::Progress,
                session_generation: current_session,
            },
        )]);

        begin_playback_report_session(&mut current_session, &mut failures, &mut feedback);
        assert_eq!(current_session, 42);
        assert_eq!(failures, 0);
        assert_eq!(feedback, None);
        assert!(!should_suppress_playback_report(
            PlaybackReportKind::Progress,
            failures,
        ));

        assert!(finish_playback_report(
            &mut pending,
            current_session,
            &mut failures,
            &mut feedback,
            22,
            false,
        ));
        assert_eq!(failures, 0);
        assert_eq!(feedback, None);
    }
}

#[cfg(test)]
mod artwork_cache_tests {
    use super::{
        MAX_CACHED_ARTWORK_TEXTURES, MAX_PHOTO_THUMBNAILS, evict_photo_thumbnails,
        trim_artwork_textures,
    };
    use crate::catalog::{ArtworkMotif, ArtworkPalette, MediaItem, MediaKind};
    use std::collections::{HashMap, HashSet};

    #[test]
    fn evicts_the_oldest_texture_and_allows_it_to_be_requested_again() {
        let context = egui::Context::default();
        let artwork = ArtworkPalette {
            top: [10, 20, 30],
            bottom: [30, 20, 10],
            shape: [50, 60, 70],
            detail: [70, 60, 50],
            motif: ArtworkMotif::Orbit,
        };
        let mut items = Vec::new();
        let mut access_ticks = HashMap::new();
        let mut attempted = HashSet::new();

        for index in 0..(MAX_CACHED_ARTWORK_TEXTURES + 1) {
            let id = format!("item-{index}");
            let tag = format!("tag-{index}");
            let texture = context.load_texture(
                id.clone(),
                egui::ColorImage::filled([1, 1], egui::Color32::WHITE),
                egui::TextureOptions::LINEAR,
            );
            access_ticks.insert((id.clone(), tag.clone()), index as u64);
            attempted.insert((id.clone(), tag.clone()));
            items.push(MediaItem {
                series_id: None,
                played: false,
                favorite: false,
                resumable: false,
                artwork_id: id.clone(),
                backdrop_id: id.clone(),
                id,
                title: format!("Title {index}"),
                remote_type: Some("Movie".into()),
                overview: None,
                kind: MediaKind::Movie,
                artwork,
                primary_image_tag: Some(tag),
                artwork_texture: Some(texture),
            });
        }

        {
            let mut refs: Vec<&mut MediaItem> = items.iter_mut().collect();
            trim_artwork_textures(&mut refs, &mut access_ticks, &mut attempted, 0);
        }

        assert_eq!(
            items
                .iter()
                .filter(|item| item.artwork_texture.is_some())
                .count(),
            MAX_CACHED_ARTWORK_TEXTURES
        );
        assert!(items[0].artwork_texture.is_none());
        assert!(items[MAX_CACHED_ARTWORK_TEXTURES].artwork_texture.is_some());
        assert!(!attempted.contains(&("item-0".into(), "tag-0".into())));
    }

    #[test]
    fn textures_on_screen_are_kept_past_the_cap() {
        let context = egui::Context::default();
        let mut items = Vec::new();
        let mut access_ticks = HashMap::new();
        let mut attempted = HashSet::new();
        let shown = MAX_CACHED_ARTWORK_TEXTURES + 10;
        for index in 0..shown {
            let id = format!("item-{index}");
            let tag = format!("tag-{index}");
            access_ticks.insert((id.clone(), tag.clone()), 100 + index as u64);
            attempted.insert((id.clone(), tag.clone()));
            let texture = context.load_texture(
                id.clone(),
                egui::ColorImage::filled([1, 1], egui::Color32::WHITE),
                egui::TextureOptions::LINEAR,
            );
            items.push(MediaItem {
                series_id: None,
                played: false,
                favorite: false,
                resumable: false,
                artwork_id: id.clone(),
                backdrop_id: id.clone(),
                id,
                title: format!("Title {index}"),
                remote_type: Some("Movie".into()),
                overview: None,
                kind: MediaKind::Movie,
                artwork: ArtworkPalette {
                    top: [10, 20, 30],
                    bottom: [30, 20, 10],
                    shape: [50, 60, 70],
                    detail: [70, 60, 50],
                    motif: ArtworkMotif::Orbit,
                },
                primary_image_tag: Some(tag),
                artwork_texture: Some(texture),
            });
        }
        {
            let mut refs: Vec<&mut MediaItem> = items.iter_mut().collect();
            trim_artwork_textures(&mut refs, &mut access_ticks, &mut attempted, 100);
        }
        assert!(items.iter().all(|item| item.artwork_texture.is_some()));
    }

    #[test]
    fn photo_thumbnail_eviction_spares_the_visible_window_and_allows_reloads() {
        let context = egui::Context::default();
        let mut thumbnails = HashMap::new();
        let mut attempted = HashSet::new();
        for index in 0..=MAX_PHOTO_THUMBNAILS {
            let id = format!("photo-{index}");
            thumbnails.insert(
                id.clone(),
                context.load_texture(
                    id.clone(),
                    egui::ColorImage::filled([1, 1], egui::Color32::WHITE),
                    egui::TextureOptions::LINEAR,
                ),
            );
            attempted.insert(id);
        }
        let visible: HashSet<String> = (1..=MAX_PHOTO_THUMBNAILS)
            .map(|index| format!("photo-{index}"))
            .collect();
        evict_photo_thumbnails(&mut thumbnails, &mut attempted, &visible, "photo-64");
        assert_eq!(thumbnails.len(), MAX_PHOTO_THUMBNAILS);
        assert!(!thumbnails.contains_key("photo-0"));
        assert!(
            !attempted.contains("photo-0"),
            "an evicted photo can load again"
        );

        // Every thumbnail visible: the cache grows rather than dropping one on screen.
        let all: HashSet<String> = thumbnails.keys().cloned().collect();
        thumbnails.insert(
            "photo-extra".into(),
            context.load_texture(
                "photo-extra",
                egui::ColorImage::filled([1, 1], egui::Color32::WHITE),
                egui::TextureOptions::LINEAR,
            ),
        );
        evict_photo_thumbnails(&mut thumbnails, &mut attempted, &all, "photo-extra");
        assert_eq!(thumbnails.len(), MAX_PHOTO_THUMBNAILS + 1);
    }
}

#[cfg(test)]
mod playback_settings_tests {
    use super::playback_overrides_from_settings;
    use kattri_settings::Settings;

    #[test]
    fn existing_bitrate_and_force_transcode_settings_map_to_playback_overrides() {
        let mut settings = Settings::default();
        settings.playback.max_streaming_bitrate = 24_000_000;
        settings.video.transcode.always_force = true;

        let overrides = playback_overrides_from_settings(&settings);
        assert_eq!(overrides.max_streaming_bitrate, Some(24_000_000));
        assert!(overrides.force_transcode);
    }
}

#[cfg(test)]
mod window_settings_tests {
    use super::KattriApp;
    use kattri_client_core::window_state::SavedWindow;

    #[test]
    fn only_devices_the_player_would_accept_reach_the_settings_picker() {
        let mut app = KattriApp::new(egui::Context::default());
        app.set_audio_devices(vec![
            "auto".into(),
            "alsa/default".into(),
            "alsa/default".into(),
            "wasapi/{0.0.0.00000000}.{abc}".into(),
            "alsa/bad;name".into(),
            "no-backend".into(),
            String::new(),
        ]);
        assert_eq!(
            app.audio_devices,
            ["alsa/default", "wasapi/{0.0.0.00000000}.{abc}"]
        );
        app.set_audio_devices((0..200).map(|n| format!("alsa/dev{n}")).collect());
        assert_eq!(app.audio_devices.len(), 64);
        app.set_audio_devices(Vec::new());
        assert!(app.audio_devices.is_empty());
    }

    #[test]
    fn window_geometry_is_saved_through_the_profile_and_read_back() {
        let dir = std::env::temp_dir().join(format!("kattri-app-window-{}", uuid::Uuid::new_v4()));
        let mut app = KattriApp::new(egui::Context::default());
        app.settings = crate::settings_state::SettingsState::open(Some(&dir), None);
        let saved = SavedWindow {
            x: -1500,
            y: 40,
            width: 1024,
            height: 768,
            monitor_name: "LEFT".to_owned(),
            maximized: false,
            fullscreen: false,
        };

        app.save_window_geometry(&saved);
        app.settings
            .finish_pending_saves()
            .expect("queued window save reaches disk");

        let reopened = crate::settings_state::SettingsState::open(Some(&dir), None);
        let geometry = reopened
            .settings()
            .window
            .geometry
            .clone()
            .expect("geometry was saved");
        assert_eq!(
            (geometry.x, geometry.y, geometry.width, geometry.height),
            (-1500, 40, 1024, 768)
        );
        assert_eq!(geometry.monitor.as_deref(), Some("LEFT"));
        assert!(!reopened.settings().window.fullscreen);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod quick_connect_availability_tests {
    use super::{quick_connect_is_enabled_for, server_preview_matches};

    #[test]
    fn server_identity_preview_is_invalidated_by_url_or_transport_changes() {
        assert!(server_preview_matches(
            "https://media.example",
            false,
            Some(("https://media.example", false))
        ));
        assert!(!server_preview_matches(
            "https://other.example",
            false,
            Some(("https://media.example", false))
        ));
        assert!(!server_preview_matches(
            "http://media.example",
            true,
            Some(("http://media.example", false))
        ));
        assert!(!server_preview_matches(
            "https://media.example",
            false,
            None
        ));
    }

    #[test]
    fn quick_connect_is_shown_only_for_the_confirmed_server_and_transport_choice() {
        assert!(quick_connect_is_enabled_for(
            "https://media.example",
            false,
            Some(("https://media.example", false, true))
        ));
        assert!(!quick_connect_is_enabled_for(
            "https://media.example",
            false,
            Some(("https://other.example", false, true))
        ));
        assert!(!quick_connect_is_enabled_for(
            "https://media.example",
            false,
            Some(("https://media.example", true, true))
        ));
        assert!(!quick_connect_is_enabled_for(
            "https://media.example",
            false,
            Some(("https://media.example", false, false))
        ));
        assert!(!quick_connect_is_enabled_for(
            "https://media.example",
            false,
            None
        ));
    }
}

#[cfg(test)]
mod remote_and_sync_play_tests {
    use super::{
        ApiWorker, ClockEstimate, GroupCommand, KattriApp, LocalAction, SyncPlayState, WorkerEvent,
    };
    use jellyfin_api::remote_command::RemoteCommand;
    use jellyfin_api::websocket::{SyncPlayMessage, SyncPlayWireCommand};
    use kattri_client_core::syncplay::ClockSample;

    const CLOCK: ClockEstimate = ClockEstimate {
        offset_ms: 0,
        error_bound_ms: 0,
    };

    #[test]
    fn remote_pause_command_reaches_the_player_intent() {
        let ctx = egui::Context::default();
        let mut app = KattriApp::new(ctx.clone());
        let (worker, events) = ApiWorker::with_event_sender_for_test();
        app.worker = Some(worker);
        let message = r#"{"MessageType":"Playstate","Data":{"Command":"Pause"}}"#;
        let event = crate::worker::remote_action_event(message, 100).expect("pause parses");
        events.try_send(event).expect("event queued");

        app.receive_worker_events(&ctx);

        assert_eq!(app.take_player_intents(), vec![LocalAction::Pause]);
        assert!(app.take_player_intents().is_empty());
    }

    #[test]
    fn socket_pause_becomes_the_pause_player_intent() {
        let ctx = egui::Context::default();
        let mut app = KattriApp::new(ctx.clone());
        let (worker, events) = ApiWorker::with_event_sender_for_test();
        app.worker = Some(worker);
        events
            .try_send(WorkerEvent::RemoteCommandReceived {
                command: RemoteCommand::Pause,
            })
            .expect("event queued");

        app.receive_worker_events(&ctx);

        assert_eq!(app.take_player_intents(), vec![LocalAction::Pause]);
    }

    #[test]
    fn socket_command_without_a_player_action_queues_nothing() {
        let ctx = egui::Context::default();
        let mut app = KattriApp::new(ctx.clone());
        let (worker, events) = ApiWorker::with_event_sender_for_test();
        app.worker = Some(worker);
        events
            .try_send(WorkerEvent::RemoteCommandReceived {
                command: RemoteCommand::Next,
            })
            .expect("event queued");

        app.receive_worker_events(&ctx);

        assert!(app.take_player_intents().is_empty());
    }

    /// A joined app whose clock maps server time 1_000_000 ms onto local time 0.
    fn joined_app_with_clock() -> KattriApp {
        let mut app = KattriApp::new(egui::Context::default());
        app.sync_play_state = SyncPlayState::Joined {
            group_id: "group-1".into(),
        };
        app.sync_play_clock.add_sample(ClockSample {
            sent_local_ms: 0,
            received_local_ms: 0,
            server_ms: 1_000_000,
        });
        app
    }

    fn sync_play_event(command: SyncPlayWireCommand) -> WorkerEvent {
        WorkerEvent::SyncPlayCommandReceived {
            command: SyncPlayMessage {
                command,
                position_ticks: 600_000_000,
                when: "1970-01-01T00:16:40.0000000Z".into(),
            },
        }
    }

    #[test]
    fn inbound_sync_play_pause_becomes_a_pause_intent_behind_the_echo_guard() {
        let ctx = egui::Context::default();
        let mut app = joined_app_with_clock();
        let (worker, events) = ApiWorker::with_event_sender_for_test();
        app.worker = Some(worker);
        events
            .try_send(sync_play_event(SyncPlayWireCommand::Pause))
            .expect("event queued");

        app.receive_worker_events(&ctx);

        assert_eq!(
            app.take_player_intents(),
            vec![
                LocalAction::Seek {
                    position_ms: 60_000,
                },
                LocalAction::Pause,
            ]
        );
        assert!(matches!(
            app.sync_play_state,
            SyncPlayState::Paused {
                position_ms: 60_000,
                ..
            }
        ));
        // The player reports the state the command left it in. That report is the echo, so it
        // is not sent back to the group.
        let now_ms = app.player_view.now_ms();
        assert_eq!(app.observe_local_player(true, 60_000, now_ms), None);
    }

    #[test]
    fn server_clock_reading_feeds_the_estimate_that_inbound_commands_use() {
        let ctx = egui::Context::default();
        let mut app = KattriApp::new(ctx.clone());
        let (worker, events) = ApiWorker::with_event_sender_for_test();
        app.worker = Some(worker);
        let now = std::time::Instant::now();
        events
            .try_send(WorkerEvent::ServerClockSampled {
                sent_at: now,
                received_at: now,
                server_ms: 1_000_000,
            })
            .expect("event queued");

        app.receive_worker_events(&ctx);

        assert_eq!(app.sync_play_clock.sample_count(), 1);
        let sent_ms = app.player_view.monotonic_ms_at(now);
        let estimate = app.sync_play_clock.estimate().expect("sample fed");
        assert_eq!(
            estimate.offset_ms,
            1_000_000 - sent_ms,
            "a zero round trip puts the server time at the send instant"
        );
    }

    #[test]
    fn server_clock_reading_over_the_exchange_bound_is_not_fed() {
        let ctx = egui::Context::default();
        let mut app = KattriApp::new(ctx.clone());
        let (worker, events) = ApiWorker::with_event_sender_for_test();
        app.worker = Some(worker);
        let now = std::time::Instant::now();
        let received_at = now + std::time::Duration::from_secs(3);
        events
            .try_send(WorkerEvent::ServerClockSampled {
                sent_at: now,
                received_at,
                server_ms: 1_000_000,
            })
            .expect("event queued");

        app.receive_worker_events(&ctx);

        assert_eq!(app.sync_play_clock.sample_count(), 0);
        assert!(app.sync_play_clock.estimate().is_none());
    }

    #[test]
    fn inbound_sync_play_is_refused_without_a_clock_estimate() {
        let ctx = egui::Context::default();
        let mut app = KattriApp::new(ctx.clone());
        app.sync_play_state = SyncPlayState::Joined {
            group_id: "group-1".into(),
        };
        let (worker, events) = ApiWorker::with_event_sender_for_test();
        app.worker = Some(worker);
        events
            .try_send(sync_play_event(SyncPlayWireCommand::Unpause))
            .expect("event queued");

        app.receive_worker_events(&ctx);

        assert!(app.take_player_intents().is_empty());
    }

    #[test]
    fn sync_play_join_counts_the_group_only_after_the_worker_confirms() {
        let ctx = egui::Context::default();
        let mut app = KattriApp::new(ctx.clone());
        let (worker, events) = ApiWorker::with_event_sender_for_test();
        app.worker = Some(worker);
        app.join_sync_play_group("group-1".into())
            .expect("join queued");
        let (request_id, _) = app.sync_play_pending_join.clone().expect("join is pending");
        assert_eq!(app.sync_play_state, SyncPlayState::NotJoined);

        events
            .try_send(WorkerEvent::SyncPlayJoinFailed {
                request_id: request_id.wrapping_add(1),
            })
            .expect("stale event queued");
        app.receive_worker_events(&ctx);
        assert!(app.sync_play_pending_join.is_some());

        events
            .try_send(WorkerEvent::SyncPlayJoined {
                request_id,
                group_id: "group-1".into(),
            })
            .expect("join event queued");
        app.receive_worker_events(&ctx);
        assert_eq!(
            app.sync_play_state,
            SyncPlayState::Joined {
                group_id: "group-1".into()
            }
        );
        assert!(app.sync_play_pending_join.is_none());
    }

    #[test]
    fn sync_play_leave_clears_the_group_only_when_the_worker_confirms() {
        let ctx = egui::Context::default();
        let mut app = KattriApp::new(ctx.clone());
        let (worker, events) = ApiWorker::with_event_sender_for_test();
        app.worker = Some(worker);
        app.sync_play_state = SyncPlayState::Joined {
            group_id: "group-1".into(),
        };
        app.leave_sync_play_group().expect("leave queued");
        let request_id = app.sync_play_pending_leave.expect("leave is pending");

        events
            .try_send(WorkerEvent::SyncPlayLeaveFailed { request_id })
            .expect("failure queued");
        app.receive_worker_events(&ctx);
        assert!(matches!(app.sync_play_state, SyncPlayState::Joined { .. }));
        assert!(app.sync_play_pending_leave.is_none());

        app.leave_sync_play_group().expect("second leave queued");
        let request_id = app.sync_play_pending_leave.expect("leave is pending");
        events
            .try_send(WorkerEvent::SyncPlayLeft { request_id })
            .expect("leave confirmed");
        app.receive_worker_events(&ctx);
        assert_eq!(app.sync_play_state, SyncPlayState::NotJoined);
    }

    #[test]
    fn websocket_group_updates_drive_membership_and_screen_details() {
        use jellyfin_api::websocket::SyncPlayGroupUpdate;
        use jellyfin_api::{GroupState, SyncPlayGroup};

        let mut app = KattriApp::new(egui::Context::default());
        let group = SyncPlayGroup {
            group_id: "group-1".into(),
            group_name: "Movie night".into(),
            state: GroupState::Idle,
            participants: vec!["Alex".into()],
            last_updated_at: String::new(),
        };
        app.apply_sync_play_group_update(SyncPlayGroupUpdate::Joined(group.clone()));
        assert_eq!(app.joined_sync_play_group_id(), Some("group-1"));
        assert_eq!(app.sync_play_ui.groups(), &[group]);

        app.apply_sync_play_group_update(SyncPlayGroupUpdate::UserJoined {
            group_id: Some("group-1".into()),
            user_name: "Sam".into(),
        });
        app.apply_sync_play_group_update(SyncPlayGroupUpdate::StateUpdate {
            group_id: Some("group-1".into()),
            state: GroupState::Playing,
            reason: "Unpaused".into(),
        });
        assert_eq!(app.sync_play_ui.groups()[0].participants, ["Alex", "Sam"]);
        assert_eq!(app.sync_play_ui.groups()[0].state, GroupState::Playing);

        app.apply_sync_play_group_update(SyncPlayGroupUpdate::Left {
            group_id: Some("group-1".into()),
        });
        assert_eq!(app.joined_sync_play_group_id(), None);
    }

    #[test]
    fn sync_play_commands_are_refused_until_joined() {
        let mut app = KattriApp::new(egui::Context::default());
        let command = GroupCommand::Pause {
            position_ms: 0,
            server_time_ms: 1_000,
        };

        assert!(
            app.apply_sync_play_command(command, CLOCK, 1_000, true)
                .is_err()
        );
        assert!(app.take_player_intents().is_empty());
    }

    #[test]
    fn inbound_group_commands_seek_then_act_and_drift_is_rate_limited() {
        let mut app = KattriApp::new(egui::Context::default());
        app.sync_play_state = SyncPlayState::Joined {
            group_id: "group-1".into(),
        };

        app.apply_sync_play_command(
            GroupCommand::Unpause {
                position_ms: 1_000,
                server_time_ms: 1_000,
            },
            CLOCK,
            1_000,
            false,
        )
        .expect("unpause applies");
        assert_eq!(
            app.take_player_intents(),
            vec![LocalAction::Seek { position_ms: 1_000 }, LocalAction::Play]
        );

        // Expected 2000 at local 2000; drift 500 is over the threshold and no correction has run.
        app.check_sync_play_drift(2_500, CLOCK, 2_000);
        assert_eq!(
            app.take_player_intents(),
            vec![LocalAction::Seek { position_ms: 2_000 }]
        );
        // Drift is over the threshold, but the last correction was 500 ms ago.
        app.check_sync_play_drift(3_000, CLOCK, 2_500);
        assert!(app.take_player_intents().is_empty());
        // Drift of 50 ms is inside the threshold.
        app.check_sync_play_drift(4_550, CLOCK, 4_500);
        assert!(app.take_player_intents().is_empty());
        // Drift of 300 ms, 3 s after the last correction.
        app.check_sync_play_drift(5_300, CLOCK, 5_000);
        assert_eq!(
            app.take_player_intents(),
            vec![LocalAction::Seek { position_ms: 5_000 }]
        );
    }

    #[test]
    fn pause_seeks_and_holds_and_a_future_command_is_refused() {
        let mut app = KattriApp::new(egui::Context::default());
        app.sync_play_state = SyncPlayState::Joined {
            group_id: "group-1".into(),
        };

        app.apply_sync_play_command(
            GroupCommand::Pause {
                position_ms: 7_000,
                server_time_ms: 1_000,
            },
            CLOCK,
            1_000,
            true,
        )
        .expect("pause applies");
        assert_eq!(
            app.take_player_intents(),
            vec![LocalAction::Seek { position_ms: 7_000 }, LocalAction::Pause]
        );
        assert_eq!(
            app.sync_play_state,
            SyncPlayState::Paused {
                group_id: "group-1".into(),
                position_ms: 7_000,
            }
        );
        // Drift is only checked while the group plays.
        app.check_sync_play_drift(0, CLOCK, 9_000);
        assert!(app.take_player_intents().is_empty());

        let future = GroupCommand::Pause {
            position_ms: 0,
            server_time_ms: 100_000,
        };
        assert!(
            app.apply_sync_play_command(future, CLOCK, 1_000, true)
                .is_err()
        );
        assert!(app.take_player_intents().is_empty());
    }

    fn joined_app() -> KattriApp {
        let mut app = KattriApp::new(egui::Context::default());
        app.sync_play_state = SyncPlayState::Joined {
            group_id: "group-1".into(),
        };
        app
    }

    #[test]
    fn echo_guard_does_not_send_back_the_group_pause() {
        let mut app = joined_app();
        assert_eq!(app.observe_local_player(false, 6_000, 900), None);
        app.apply_sync_play_command(
            GroupCommand::Pause {
                position_ms: 7_000,
                server_time_ms: 1_000,
            },
            CLOCK,
            1_000,
            true,
        )
        .expect("pause applies");
        // Before the pause lands, the player still reports playing. Inside the window that is
        // still the group's command, so nothing is sent.
        assert_eq!(app.observe_local_player(false, 7_000, 1_100), None);
        // The echo of the pause: it matches the applied state, so it is swallowed.
        assert_eq!(app.observe_local_player(true, 7_000, 1_200), None);
        // A later unpause is the user's and is sent.
        assert_eq!(
            app.observe_local_player(false, 7_000, 1_300),
            Some(crate::worker::SyncPlayCommand::Unpause)
        );
    }

    #[test]
    fn echo_guard_does_not_send_back_a_group_seek() {
        let mut app = joined_app();
        assert_eq!(app.observe_local_player(false, 1_000, 0), None);
        app.apply_sync_play_command(
            GroupCommand::Unpause {
                position_ms: 20_000,
                server_time_ms: 1_000,
            },
            CLOCK,
            1_000,
            false,
        )
        .expect("unpause applies");
        // The seek lands 400 ms behind the expected position. That is the echo.
        assert_eq!(app.observe_local_player(false, 20_100, 1_500), None);
        // A user pause after the echo is sent.
        assert_eq!(
            app.observe_local_player(true, 20_200, 2_000),
            Some(crate::worker::SyncPlayCommand::Pause)
        );
    }

    #[test]
    fn echo_guard_expires_so_later_user_changes_are_sent() {
        let mut app = joined_app();
        assert_eq!(app.observe_local_player(false, 0, 0), None);
        app.apply_sync_play_command(
            GroupCommand::Pause {
                position_ms: 0,
                server_time_ms: 1_000,
            },
            CLOCK,
            1_000,
            true,
        )
        .expect("pause applies");
        // The player never reports the echo. Once the window has passed, a pause is the user's.
        assert_eq!(app.observe_local_player(false, 0, 1_100), None);
        assert_eq!(
            app.observe_local_player(true, 0, 9_000),
            Some(crate::worker::SyncPlayCommand::Pause)
        );
    }

    #[test]
    fn user_changes_are_not_sent_while_not_joined() {
        let mut app = KattriApp::new(egui::Context::default());
        assert_eq!(app.observe_local_player(false, 0, 0), None);
        assert_eq!(app.observe_local_player(true, 0, 100), None);
    }

    #[test]
    fn classify_separates_seeks_from_stalls_and_normal_playback() {
        use super::{LocalPlayerChange, PlayerSnapshot, classify_player_change, group_command_for};
        let at = |paused, position_ms, at_ms| PlayerSnapshot {
            paused,
            position_ms,
            at_ms,
        };
        // A stall keeps the position where it was, so it is not a seek.
        assert_eq!(
            classify_player_change(at(false, 10_000, 0), at(false, 10_000, 5_000)),
            None
        );
        // Normal playback advances by the elapsed time.
        assert_eq!(
            classify_player_change(at(false, 10_000, 0), at(false, 11_000, 1_000)),
            None
        );
        assert_eq!(
            classify_player_change(at(false, 10_000, 0), at(false, 2_000, 1_000)),
            Some(LocalPlayerChange::Seek { position_ms: 2_000 })
        );
        assert_eq!(
            classify_player_change(at(false, 10_000, 0), at(false, 60_000, 1_000)),
            Some(LocalPlayerChange::Seek {
                position_ms: 60_000
            })
        );
        assert_eq!(
            group_command_for(LocalPlayerChange::Seek { position_ms: 2 }),
            crate::worker::SyncPlayCommand::Seek {
                position_ticks: 20_000
            }
        );
    }
}

#[cfg(test)]
mod photo_decode_event_tests {
    use super::{KattriApp, decode_photo_request_tag};
    use crate::worker::{ApiWorker, WorkerEvent};
    use egui::{Context, RawInput};

    #[test]
    fn thumbnail_request_metadata_preserves_index_and_visibility_class() {
        assert_eq!(
            decode_photo_request_tag("tag\u{1f}137\u{1f}p"),
            Some((Some("tag".to_owned()), 137, false))
        );
        assert_eq!(
            decode_photo_request_tag("tag\u{1f}4\u{1f}v"),
            Some((Some("tag".to_owned()), 4, true))
        );
        assert_eq!(decode_photo_request_tag("tag\u{1f}bad\u{1f}v"), None);
    }

    #[test]
    fn real_photo_completion_is_cached_and_stale_completion_is_discarded() {
        let mut app = KattriApp::new(Context::default());
        let (worker, sender) = ApiWorker::with_event_sender_for_test();
        app.worker = Some(worker);
        app.photo_items.push(
            serde_json::from_value(serde_json::json!({
                "Id": "photo-1", "Name": "Photo", "Type": "Photo"
            }))
            .expect("photo DTO fixture"),
        );
        app.photo_thumbnail_pending.insert(41, "photo-1".into());
        sender
            .try_send(WorkerEvent::PhotoThumbnailLoaded {
                request_id: 41,
                item_id: "photo-1".into(),
                width: 2,
                height: 2,
                rgba: vec![255; 16],
            })
            .expect("send decoded photo result");

        let ctx = Context::default();
        let mut output = ctx.run_ui(RawInput::default(), |ui| {
            app.receive_worker_events(ui.ctx())
        });
        output.textures_delta.clear();
        assert!(app.photo_thumbnails.contains_key("photo-1"));
        assert!(!app.photo_thumbnail_pending.contains_key(&41));

        sender
            .try_send(WorkerEvent::PhotoThumbnailLoaded {
                request_id: 40,
                item_id: "photo-1".into(),
                width: 2,
                height: 2,
                rgba: vec![0; 16],
            })
            .expect("send stale decoded photo result");
        let mut output = ctx.run_ui(RawInput::default(), |ui| {
            app.receive_worker_events(ui.ctx())
        });
        output.textures_delta.clear();
        assert_eq!(app.photo_thumbnails.len(), 1);
    }

    #[test]
    fn episode_neighbors_drive_previous_next_and_auto_play() {
        let mut app = KattriApp::new(Context::default());
        let (worker, sender) = ApiWorker::with_event_sender_for_test();
        app.worker = Some(worker);
        app.selected_details = Some(
            serde_json::from_value(serde_json::json!({
                "Id": "e2", "Name": "Two", "Type": "Episode", "SeriesId": "show"
            }))
            .expect("episode fixture"),
        );
        app.request_episode_neighbors();
        let request_id = app
            .episode_neighbors
            .as_ref()
            .expect("neighbours requested")
            .request_id;
        assert_eq!(app.episode_navigation("e2"), (false, false));

        let dto = |id: &str| -> Option<Box<jellyfin_api::BaseItemDto>> {
            Some(Box::new(
                serde_json::from_value(serde_json::json!({"Id": id, "Name": id}))
                    .expect("neighbour fixture"),
            ))
        };
        // A reply for another request is ignored.
        sender
            .try_send(WorkerEvent::AdjacentEpisodesLoaded {
                request_id: request_id + 100,
                episode_id: "e2".into(),
                previous: dto("x"),
                next: dto("y"),
            })
            .expect("send stale neighbours");
        sender
            .try_send(WorkerEvent::AdjacentEpisodesLoaded {
                request_id,
                episode_id: "e2".into(),
                previous: None,
                next: dto("e3"),
            })
            .expect("send neighbours");
        let ctx = Context::default();
        let mut output = ctx.run_ui(RawInput::default(), |ui| {
            app.receive_worker_events(ui.ctx())
        });
        output.textures_delta.clear();
        assert_eq!(app.episode_navigation("e2"), (false, true));
        assert_eq!(app.episode_navigation("other"), (false, false));
        assert!(!app.on_episode_previous("e2"));

        assert_eq!(app.up_next_label("e2"), Some("e3"));
        app.cancel_up_next("e2");
        assert_eq!(app.up_next_label("e2"), None);
        assert!(!app.on_episode_ended("e2"), "Up Next was dismissed");
        app.episode_neighbors
            .as_mut()
            .expect("neighbours")
            .up_next_cancelled = false;
        app.settings.settings_mut().playback.auto_play_next_episode = false;
        assert!(!app.on_episode_ended("e2"), "auto-play is off");
        assert_eq!(app.up_next_label("e2"), None);

        assert!(app.on_episode_next("e2"));
        assert_eq!(app.selected_item_id.as_deref(), Some("e3"));
        assert_eq!(
            app.auto_play.as_ref().map(|auto| auto.item_id.as_str()),
            Some("e3")
        );
    }

    #[test]
    fn series_next_up_is_kept_only_for_its_own_request_and_series() {
        let mut app = KattriApp::new(Context::default());
        let (worker, sender) = ApiWorker::with_event_sender_for_test();
        app.worker = Some(worker);
        app.request_series_seasons("show".into());
        let request_id = app
            .series_next_up
            .as_ref()
            .expect("next up requested")
            .request_id;
        let episode = || -> Option<Box<jellyfin_api::BaseItemDto>> {
            Some(Box::new(
                serde_json::from_value(serde_json::json!({"Id": "e4", "Name": "Four"}))
                    .expect("episode fixture"),
            ))
        };
        for (id, series) in [(request_id + 1, "show"), (request_id, "other")] {
            sender
                .try_send(WorkerEvent::SeriesNextUpLoaded {
                    request_id: id,
                    series_id: series.into(),
                    episode: episode(),
                })
                .expect("send stale next up");
        }
        sender
            .try_send(WorkerEvent::SeriesNextUpLoaded {
                request_id,
                series_id: "show".into(),
                episode: episode(),
            })
            .expect("send next up");
        let ctx = Context::default();
        let mut output = ctx.run_ui(RawInput::default(), |ui| {
            app.receive_worker_events(ui.ctx())
        });
        output.textures_delta.clear();
        assert_eq!(
            app.series_next_up_episode("show")
                .map(|item| item.id.as_str()),
            Some("e4")
        );
        assert!(app.series_next_up_episode("other").is_none());
    }

    #[test]
    fn card_menu_queues_tracks_and_reports_playlist_additions() {
        use crate::ui::item_menu::{ItemMenuAction, ItemMenuTarget};
        let mut app = KattriApp::new(Context::default());
        let (worker, sender) = ApiWorker::with_event_sender_for_test();
        app.worker = Some(worker);
        let target = |playlist_id: Option<&str>| ItemMenuTarget {
            item_id: "track".into(),
            title: "Song".into(),
            series_id: None,
            list_id: playlist_id.map(str::to_owned),
        };
        app.apply_item_menu_action(target(None), ItemMenuAction::AddToQueue);
        assert_eq!(
            app.music_queue
                .items()
                .iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>(),
            ["track"]
        );
        assert!(
            app.toast
                .as_ref()
                .is_some_and(|(text, _)| text.contains("Song"))
        );

        app.menu_playlists = std::sync::Arc::new(vec![("list".into(), "Road trip".into())]);
        app.apply_item_menu_action(target(Some("list")), ItemMenuAction::AddToPlaylist);
        let (request_id, _, playlist) = app.menu_playlist_add.clone().expect("addition submitted");
        assert_eq!(playlist, "Road trip");
        app.toast = None;
        sender
            .try_send(WorkerEvent::PlaylistMutationCompleted {
                request_id,
                playlist_id: "list".into(),
                kind: crate::worker::PlaylistMutationKind::Added,
            })
            .expect("send completion");
        let ctx = Context::default();
        let mut output = ctx.run_ui(RawInput::default(), |ui| {
            app.receive_worker_events(ui.ctx())
        });
        output.textures_delta.clear();
        assert!(app.menu_playlist_add.is_none());
        assert!(
            app.toast
                .as_ref()
                .is_some_and(|(text, _)| text.contains("Road trip"))
        );
    }

    #[test]
    fn episode_rows_get_their_stills() {
        let mut app = KattriApp::new(Context::default());
        let (worker, sender) = ApiWorker::with_event_sender_for_test();
        app.worker = Some(worker);
        app.series_episodes = vec![
            serde_json::from_value(serde_json::json!({
                "Id": "e1", "Name": "One", "ImageTags": {"Primary": "still-1"}
            }))
            .expect("episode fixture"),
            serde_json::from_value(serde_json::json!({"Id": "e2", "Name": "Two"}))
                .expect("episode without a still"),
        ];
        app.request_episode_thumbnails();
        assert_eq!(app.episode_thumbnail_pending.len(), 1);
        let (&request_id, _) = app
            .episode_thumbnail_pending
            .iter()
            .next()
            .expect("one still requested");
        sender
            .try_send(WorkerEvent::PrimaryArtworkLoaded {
                request_id,
                item_id: "e1".into(),
                width: 2,
                height: 2,
                rgba: vec![255; 16],
            })
            .expect("send still");
        let ctx = Context::default();
        let mut output = ctx.run_ui(RawInput::default(), |ui| {
            app.receive_worker_events(ui.ctx())
        });
        output.textures_delta.clear();
        assert!(app.episode_thumbnails.contains_key("e1"));
        assert!(app.episode_thumbnail_pending.is_empty());
        app.reset_series_detail();
        assert!(app.episode_thumbnails.is_empty());
    }

    #[test]
    fn admin_actions_need_an_administrator_and_a_confirmed_delete_drops_the_item() {
        use crate::ui::item_menu::{ItemMenuAction, ItemMenuTarget};
        let mut app = KattriApp::new(Context::default());
        let (worker, sender) = ApiWorker::with_event_sender_for_test();
        app.worker = Some(worker);
        let target = || ItemMenuTarget {
            item_id: "movie".into(),
            title: "Film".into(),
            series_id: None,
            list_id: None,
        };
        app.apply_item_menu_action(target(), ItemMenuAction::RefreshMetadata);
        assert!(app.admin_requests.is_empty(), "not an administrator");

        app.user_is_admin = true;
        app.apply_item_menu_action(target(), ItemMenuAction::Delete);
        assert_eq!(app.pending_delete, Some(("movie".into(), "Film".into())));
        assert!(
            app.admin_requests.is_empty(),
            "nothing is sent before the confirmation"
        );
        let (item_id, title) = app.pending_delete.take().expect("pending delete");
        app.submit_item_admin(item_id, title, crate::worker::ItemAdminAction::Delete, None);
        let (&request_id, _) = app.admin_requests.iter().next().expect("delete sent");

        app.items.push(crate::catalog::MediaItem::from_api(
            serde_json::from_value(
                serde_json::json!({"Id": "movie", "Name": "Film", "Type": "Movie"}),
            )
            .expect("movie fixture"),
        ));
        sender
            .try_send(WorkerEvent::ItemAdminFinished {
                request_id,
                item_id: "movie".into(),
                action: crate::worker::ItemAdminAction::Delete,
                result: Ok(()),
            })
            .expect("send result");
        let ctx = Context::default();
        let mut output = ctx.run_ui(RawInput::default(), |ui| {
            app.receive_worker_events(ui.ctx())
        });
        output.textures_delta.clear();
        assert!(app.items.is_empty());
        assert!(
            app.toast
                .as_ref()
                .is_some_and(|(text, _)| text.contains("Film"))
        );
    }

    #[test]
    fn shuffle_starts_a_random_run_that_previous_next_and_up_next_follow() {
        let mut app = KattriApp::new(Context::default());
        let (worker, sender) = ApiWorker::with_event_sender_for_test();
        app.worker = Some(worker);
        app.shuffle_series("show".into());
        let request_id = app.shuffle_request_id;
        let episode = |id: &str, number: i32| -> jellyfin_api::BaseItemDto {
            serde_json::from_value(serde_json::json!({
                "Id": id, "Name": id, "ParentIndexNumber": 1, "IndexNumber": number
            }))
            .expect("episode fixture")
        };
        sender
            .try_send(WorkerEvent::ShuffleLoaded {
                request_id,
                episodes: vec![episode("e7", 7), episode("e2", 2), episode("e5", 5)],
            })
            .expect("send shuffle");
        let ctx = Context::default();
        let mut output = ctx.run_ui(RawInput::default(), |ui| {
            app.receive_worker_events(ui.ctx())
        });
        output.textures_delta.clear();
        assert_eq!(
            app.selected_item_id.as_deref(),
            Some("e7"),
            "the run starts"
        );
        assert_eq!(app.episode_navigation("e7"), (false, true));
        assert_eq!(app.episode_navigation("e2"), (true, true));
        assert_eq!(app.episode_navigation("e5"), (true, false));
        assert_eq!(app.up_next_label("e2"), Some("S1:E5 · e5"));
        app.cancel_up_next("e2");
        assert_eq!(app.up_next_label("e2"), None);
        assert!(app.on_episode_next("e2"));
        assert_eq!(app.selected_item_id.as_deref(), Some("e5"));
        assert!(app.on_episode_previous("e5"));
        assert_eq!(app.selected_item_id.as_deref(), Some("e2"));
    }

    #[test]
    fn thumbnail_window_cancels_only_requests_outside_the_current_window() {
        let mut app = KattriApp::new(Context::default());
        let (worker, _sender) = ApiWorker::with_event_sender_for_test();
        app.worker = Some(worker);
        app.photo_thumbnail_pending.insert(7, "photo-7".into());
        app.photo_thumbnail_pending.insert(8, "photo-8".into());
        app.photo_thumbnail_attempted
            .extend(["photo-7".into(), "photo-8".into()]);

        app.update_photo_thumbnail_window(&[("photo-7".into(), "tag".into())]);

        assert_eq!(
            app.photo_thumbnail_pending.get(&7).map(String::as_str),
            Some("photo-7")
        );
        assert!(!app.photo_thumbnail_pending.contains_key(&8));
        assert!(app.photo_thumbnail_attempted.contains("photo-7"));
        assert!(!app.photo_thumbnail_attempted.contains("photo-8"));
    }

    #[cfg(feature = "photo-counters")]
    #[test]
    fn invalidating_pending_thumbnail_requests_counts_cancellations() {
        let mut app = KattriApp::new(Context::default());
        let (worker, _sender) = ApiWorker::with_event_sender_for_test();
        app.worker = Some(worker);
        app.photo_thumbnail_pending.insert(7, "photo-7".into());
        app.cancel_pending_photo_decodes();
        assert_eq!(
            app.photo_decode_counter_snapshot()
                .expect("worker counter snapshot")
                .cancelled,
            1
        );
    }
}

#[cfg(test)]
mod playback_recovery_tests {
    use super::*;

    fn recovery_app() -> KattriApp {
        let mut app = KattriApp::new(egui::Context::default());
        app.worker = Some(ApiWorker::queued_for_test());
        app.selected_item_id = Some("movie".into());
        app.playback_request_id = 19;
        app.playback_recovery = Some(PlaybackRecovery {
            item_id: "movie".into(),
            options_request_id: 19,
            start_time_ticks: Some(40_000_000),
            remaining: [PlaybackSelection {
                source_index: 1,
                method: PlaybackMethod::Transcode,
            }]
            .into(),
        });
        app
    }

    #[test]
    fn startup_retry_consumes_candidate_once_then_surfaces_terminal_error() {
        let mut app = recovery_app();
        let previous_request = app.playback_start_request_id;
        app.playback_start_failed("first source failed");
        assert!(app.playback_start_loading);
        assert!(app.playback_start_error.is_none());
        assert_ne!(app.playback_start_request_id, previous_request);
        assert!(
            app.playback_recovery
                .as_ref()
                .expect("retry context")
                .remaining
                .is_empty()
        );
        app.playback_start_failed("last source failed");
        assert!(!app.playback_start_loading);
        assert_eq!(
            app.playback_start_error.as_deref(),
            Some("last source failed")
        );
        assert!(app.playback_recovery.is_none());
    }

    #[test]
    fn navigation_or_renegotiation_invalidates_recovery() {
        let mut app = recovery_app();
        app.selected_item_id = Some("other".into());
        assert!(!app.retry_playback_start());
        assert!(app.playback_recovery.is_none());
        let mut app = recovery_app();
        app.playback_request_id += 1;
        assert!(!app.retry_playback_start());
        assert!(app.playback_recovery.is_none());
    }

    #[test]
    fn busy_player_surfaces_backpressure_without_trying_another_source() {
        let mut app = recovery_app();
        app.playback_start_request_id = 50;
        app.playback_start_loading = true;
        app.playback_start_busy();
        assert_eq!(app.playback_start_request_id, 50);
        assert!(!app.playback_start_loading);
        assert!(app.playback_recovery.is_none());
        assert!(
            app.playback_start_error
                .as_deref()
                .is_some_and(|message| message.contains("pending"))
        );
    }

    #[test]
    fn preparation_failure_retries_but_stale_worker_event_does_not_consume_candidate() {
        let mut app = recovery_app();
        let (worker, sender) = ApiWorker::with_event_sender_for_test();
        app.worker = Some(worker);
        app.playback_start_request_id = 100;
        sender
            .try_send(WorkerEvent::PlaybackPrepareFailed {
                request_id: 99,
                message: "stale".into(),
            })
            .expect("stale event");
        app.receive_worker_events(&egui::Context::default());
        assert_eq!(
            app.playback_recovery
                .as_ref()
                .expect("context")
                .remaining
                .len(),
            1
        );
        sender
            .try_send(WorkerEvent::PlaybackPrepareFailed {
                request_id: 100,
                message: "failed".into(),
            })
            .expect("current event");
        app.receive_worker_events(&egui::Context::default());
        assert!(
            app.playback_recovery
                .as_ref()
                .expect("context")
                .remaining
                .is_empty()
        );
        assert!(app.playback_start_loading);
        assert!(app.playback_start_error.is_none());
    }

    #[test]
    fn explicit_movie_start_builds_only_remaining_candidates_and_keeps_resume() {
        let mut app = recovery_app();
        app.selected_details = Some(
            serde_json::from_value(
                serde_json::json!({"Id":"movie", "Name":"Fixture", "Type":"Movie"}),
            )
            .expect("item fixture"),
        );
        app.playback_info = Some(serde_json::from_value(serde_json::json!({
            "MediaSources":[{
                "Container":"mkv", "SupportsDirectPlay":true, "SupportsTranscoding":true,
                "TranscodingUrl":"/fixture",
                "MediaStreams":[{"Type":"Video","Codec":"h264","BitDepth":8,"VideoRangeType":"SDR"}, {"Type":"Audio","Codec":"aac","IsDefault":true,"Index":1}]
            }]
        })).expect("negotiation fixture"));
        app.start_server_playback(0, jellyfin_api::PlayMethod::DirectPlay, Some(40_000_000));
        let recovery = app.playback_recovery.as_ref().expect("movie recovery");
        assert_eq!(recovery.start_time_ticks, Some(40_000_000));
        assert_eq!(
            recovery.remaining.iter().copied().collect::<Vec<_>>(),
            [PlaybackSelection {
                source_index: 0,
                method: PlaybackMethod::Transcode
            }]
        );
        app.start_server_playback(0, jellyfin_api::PlayMethod::Transcode, None);
        assert!(
            app.playback_recovery
                .as_ref()
                .expect("explicit transcode")
                .remaining
                .is_empty()
        );
        app.selected_details.as_mut().expect("item").item_type = Some("Audio".into());
        app.start_server_playback(0, jellyfin_api::PlayMethod::DirectPlay, None);
        assert!(app.playback_recovery.is_none());
    }

    #[test]
    fn loaded_event_clears_only_its_own_recovery_and_stale_error_is_ignored() {
        let mut app = recovery_app();
        app.playback_start_request_id = 50;
        app.playback_start_opening();
        assert!(app.playback_recovery.is_some());
        app.playback_loaded(49);
        assert!(app.playback_recovery.is_some());
        app.playback_loaded(50);
        assert!(app.playback_recovery.is_none());
        app.playback_failed("other");
        assert!(app.playback_start_error.is_none());
        app.playback_failed("movie");
        assert!(app.playback_start_error.is_some());
        assert!(!app.playback_start_loading);
    }

    #[test]
    fn asynchronous_open_failure_retries_only_current_item_and_generation() {
        let mut app = recovery_app();
        app.playback_start_request_id = 50;
        app.playback_start_opening();
        app.playback_open_failed("movie", 49);
        app.playback_open_failed("other", 50);
        assert_eq!(app.playback_start_request_id, 50);
        assert_eq!(
            app.playback_recovery
                .as_ref()
                .expect("context")
                .remaining
                .len(),
            1
        );
        app.playback_open_failed("movie", 50);
        assert_ne!(app.playback_start_request_id, 50);
        assert!(app.playback_start_loading);
        let current = app.playback_start_request_id;
        app.playback_open_failed("movie", 50);
        assert_eq!(app.playback_start_request_id, current);
        app.playback_open_failed("movie", current);
        assert!(app.playback_start_error.is_some());
        assert!(app.playback_recovery.is_none());
    }

    fn playing_movie_app() -> KattriApp {
        let mut app = recovery_app();
        app.selected_details = Some(
            serde_json::from_value(
                serde_json::json!({"Id":"movie", "Name":"Fixture", "Type":"Movie"}),
            )
            .expect("item fixture"),
        );
        app.playback_info = Some(serde_json::from_value(serde_json::json!({
            "MediaSources":[{
                "Container":"mkv", "SupportsDirectPlay":true, "SupportsTranscoding":true,
                "TranscodingUrl":"/fixture",
                "MediaStreams":[{"Type":"Video","Codec":"h264","BitDepth":8,"VideoRangeType":"SDR"}, {"Type":"Audio","Codec":"aac","IsDefault":true,"Index":1}]
            }]
        })).expect("negotiation fixture"));
        app.start_server_playback(0, jellyfin_api::PlayMethod::DirectPlay, None);
        let generation = app.playback_start_request_id;
        app.playback_start_opening();
        app.playback_loaded(generation);
        assert!(app.playback_recovery.is_none());
        app
    }

    #[test]
    fn mid_stream_failure_reopens_the_same_source_at_the_last_position() {
        let mut app = playing_movie_app();
        let generation = app.playback_start_request_id;
        app.playback_failed_mid_stream("movie", generation, Some(600_000_000));
        assert_ne!(app.playback_start_request_id, generation);
        assert!(app.playback_start_loading);
        assert!(app.playback_start_error.is_none());
        let active = app.active_playback.as_ref().expect("reopened load");
        assert_eq!(
            active.selection,
            PlaybackSelection {
                source_index: 0,
                method: PlaybackMethod::DirectPlay
            }
        );
        let recovery = app.playback_recovery.as_ref().expect("fallback context");
        assert_eq!(recovery.start_time_ticks, Some(600_000_000));
        assert_eq!(
            recovery.remaining.iter().copied().collect::<Vec<_>>(),
            [PlaybackSelection {
                source_index: 0,
                method: PlaybackMethod::Transcode
            }]
        );

        // If the reopen cannot start, the other source is tried at the same position.
        let reopen = app.playback_start_request_id;
        app.playback_open_failed("movie", reopen);
        assert_ne!(app.playback_start_request_id, reopen);
        assert_eq!(
            app.active_playback.as_ref().expect("fallback").selection,
            PlaybackSelection {
                source_index: 0,
                method: PlaybackMethod::Transcode
            }
        );
    }

    #[test]
    fn stale_mid_stream_failure_is_ignored() {
        let mut app = playing_movie_app();
        let generation = app.playback_start_request_id;
        app.playback_failed_mid_stream("movie", generation - 1, Some(1));
        app.playback_failed_mid_stream("other", generation, Some(1));
        assert_eq!(app.playback_start_request_id, generation);
        assert!(app.playback_start_error.is_none());
        assert_eq!(app.mid_stream_recoveries, 0);
    }

    #[test]
    fn mid_stream_recovery_is_bounded_then_surfaces_the_failure() {
        let mut app = playing_movie_app();
        for _ in 0..MAX_MID_STREAM_RECOVERIES {
            let generation = app.playback_start_request_id;
            app.playback_loaded(generation);
            app.playback_failed_mid_stream("movie", generation, Some(10));
            assert!(app.playback_start_error.is_none());
        }
        let generation = app.playback_start_request_id;
        app.playback_loaded(generation);
        app.playback_failed_mid_stream("movie", generation, Some(10));
        assert_eq!(app.playback_start_request_id, generation);
        assert!(!app.playback_start_loading);
        assert!(app.playback_start_error.is_some());

        // A new user-started playback gets a fresh budget.
        app.start_server_playback(0, jellyfin_api::PlayMethod::DirectPlay, None);
        assert_eq!(app.mid_stream_recoveries, 0);
    }

    #[test]
    fn mid_stream_failure_after_renegotiation_is_not_retried() {
        let mut app = playing_movie_app();
        let generation = app.playback_start_request_id;
        app.playback_request_id += 1;
        app.playback_failed_mid_stream("movie", generation, Some(10));
        assert_eq!(app.playback_start_request_id, generation);
        assert!(app.playback_start_error.is_some());
    }
}
