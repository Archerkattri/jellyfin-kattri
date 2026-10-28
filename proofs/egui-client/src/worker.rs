use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{Semaphore, mpsc};

mod clock_sampler;
mod custom_backdrop;
pub(crate) mod downloads;
mod saved_sign_in;
mod scheduler;
mod session_socket;
pub(crate) mod update;
use scheduler::{CommandQueue, EVENT_CAPACITY};
pub use scheduler::{WorkerShutdownReport, WorkerSubmitError};
use std::thread::JoinHandle;

use crate::ui::admin::{AdminLoad, AdminSnapshot};
use crate::ui::downloads::DownloadIntent;
use image::{ImageDecoder, ImageFormat, ImageReader, Limits};
use jellyfin_api::remote_command::RemoteCommand;
use jellyfin_api::{
    ApiError, BackdropImageError, BaseItemDto, ClientConfig, DeviceProfile, ErrorClass,
    GuideWindow, ItemFilter, ItemQuery, ItemSortBy, ItemSortOrder, ItemsResponse, JellyfinClient,
    JellyfinTicks, LiveTvChannelPage, LiveTvProgramPage, MediaSegmentType, PlayMethod,
    PlaybackInfoRequest, PlaybackInfoResponse, PlaybackSourceRequestOptions, PlaylistEntry,
    PublicSystemInfo, QuickConnectRequest, QuickConnectStatus, Session, UserDto, UserItemDataDto,
    UserView,
};
use jellyfin_playback_relay::{RelayHandle, RelayPlaybackUrl, start_relay};
use kattri_client_core::playback_reporting::{PlaybackReportAction, PlaybackReportMetadata};
use kattri_client_core::queue::MAX_QUEUE_LEN;
use kattri_client_core::quick_connect_poll::PollOutcome;
use kattri_client_core::remote::LocalAction;
use kattri_downloads::manager::DownloadManager;
pub(crate) use saved_sign_in::SavedSignInRequest;
use saved_sign_in::{SavedEntry, SavedSignInStore};
use update::{ApplyJob, UpdateOutcome};
use zeroize::{Zeroize, Zeroizing};

/// Items requested per home-screen row. Bounded so one row cannot grow the UI without limit.
pub const HOME_ROW_LIMIT: u32 = 16;
const CATALOG_PAGE_SIZE: u32 = 50;
/// Libraries whose item count is read per dashboard load. A server with more
/// libraries shows the first ones and reports the rest without a count.
const MAX_COUNTED_LIBRARIES: usize = 32;

/// The three home-screen rows, each loaded and retried independently.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HomeRowKind {
    ContinueWatching,
    NextUp,
    Latest,
}

impl HomeRowKind {
    pub const ALL: [Self; 3] = [Self::ContinueWatching, Self::NextUp, Self::Latest];
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HomeCatalogKind {
    Libraries,
    Collections,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlaylistMutationKind {
    Added,
    Removed,
    Moved,
}

impl HomeCatalogKind {
    pub const ALL: [Self; 2] = [Self::Libraries, Self::Collections];
}

pub enum HomeCatalogData {
    Libraries(Vec<UserView>),
    Collections(Vec<BaseItemDto>),
}

impl HomeCatalogData {
    pub(crate) fn kind(&self) -> HomeCatalogKind {
        match self {
            Self::Libraries(_) => HomeCatalogKind::Libraries,
            Self::Collections(_) => HomeCatalogKind::Collections,
        }
    }
}

pub struct ApiWorker {
    commands: Arc<CommandQueue>,
    events: Mutex<mpsc::Receiver<WorkerEvent>>,
    thread: Option<JoinHandle<WorkerShutdownReport>>,
    #[cfg(feature = "photo-counters")]
    photo_decode_counters: kattri_client_core::photo_grid::DecodeCounters,
}

#[derive(Clone)]
pub struct ServerIdentity {
    pub name: String,
    pub version: Option<String>,
    /// The server's own id, used to name its saved sign-in. Never shown.
    pub(crate) id: Option<String>,
}

pub struct PreparedPlayback {
    url: RelayPlaybackUrl,
    relay: RelayHandle,
    report_metadata: PlaybackReportMetadata,
    start_time_ticks: Option<i64>,
    /// Series GUID for an episode, used by the per-series language memory (M2-10).
    series_id: Option<String>,
}

impl PreparedPlayback {
    pub(crate) fn live_stream_id(&self) -> Option<&str> {
        self.report_metadata.live_stream_id.as_deref()
    }

    pub fn series_id(&self) -> Option<&str> {
        self.series_id.as_deref()
    }

    /// The media source the server chose for this playback, when it named one.
    pub(crate) fn media_source_id(&self) -> Option<&str> {
        self.report_metadata.media_source_id.as_deref()
    }

    pub(crate) fn set_series_id(&mut self, series_id: Option<String>) {
        self.series_id = series_id;
    }

    pub(crate) fn set_queue_report_context(
        &mut self,
        repeat_mode: jellyfin_api::session_reporting::RepeatMode,
        playback_order: jellyfin_api::session_reporting::PlaybackOrder,
        now_playing_queue: Vec<jellyfin_api::session_reporting::QueueItem>,
    ) {
        self.report_metadata.repeat_mode = Some(repeat_mode);
        self.report_metadata.playback_order = Some(playback_order);
        self.report_metadata.now_playing_queue = Some(now_playing_queue);
    }

    pub fn into_parts(
        self,
    ) -> (
        RelayPlaybackUrl,
        RelayHandle,
        PlaybackReportMetadata,
        Option<i64>,
    ) {
        (
            self.url,
            self.relay,
            self.report_metadata,
            self.start_time_ticks,
        )
    }
}

/// A group command for the SyncPlay group, sent when the local player changes (M4-11).
/// Pause and unpause carry no position: the server applies them to the group's position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncPlayCommand {
    Pause,
    Unpause,
    /// Jellyfin ticks (100-nanosecond units).
    Seek {
        position_ticks: u64,
    },
}

/// An administrator action from the card menu.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ItemAdminAction {
    RefreshMetadata,
    Delete,
    AddToCollection,
}

/// One step of an administrator metadata dialog (edit, identify or images).
#[derive(Clone, Debug)]
pub(crate) enum MetadataOp {
    Load,
    Save {
        item: Box<jellyfin_api::EditableItem>,
        fields: Box<jellyfin_api::MetadataFields>,
    },
    Search {
        item_type: String,
        query: jellyfin_api::RemoteSearchQuery,
    },
    Apply {
        result: Box<jellyfin_api::RemoteSearchResult>,
        replace_images: bool,
    },
    ListImages,
    RemoteImages {
        image_type: jellyfin_api::ImageType,
        all_languages: bool,
    },
    Download {
        image_type: jellyfin_api::ImageType,
        url: String,
    },
    DeleteImage {
        image_type: jellyfin_api::ImageType,
        index: u32,
    },
}

/// The server's answer to a [`MetadataOp`].
#[derive(Debug)]
pub(crate) enum MetadataReply {
    Loaded(Box<jellyfin_api::EditableItem>),
    Saved,
    SearchResults(Vec<jellyfin_api::RemoteSearchResult>),
    Applied,
    Images(Vec<jellyfin_api::ItemImage>),
    RemoteImages(Vec<jellyfin_api::RemoteImage>),
    /// An image was downloaded or deleted; the list needs a reload.
    ImagesChanged,
}

/// Where a metadata dialog preview comes from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ThumbnailSource {
    /// One of the item's current images, from the Jellyfin server.
    Item {
        item_id: String,
        image: jellyfin_api::ItemImage,
    },
    /// A provider address; fetched without Jellyfin credentials.
    Remote(String),
}

/// A change to the Live TV recording schedule.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TimerChange {
    /// Record this program with the server's default timer.
    Record { program_id: String },
    /// Cancel this scheduled recording.
    Cancel { timer_id: String },
}

/// Most episodes one shuffle queues.
const MAX_SHUFFLE_EPISODES: u32 = 200;

/// How many playlists the card menu lists under Add to playlist.
const MAX_MENU_PLAYLISTS: u32 = 100;

pub enum WorkerEvent {
    UpdateFinished {
        outcome: UpdateOutcome,
    },
    CheckingServer {
        request_id: u64,
    },
    DashboardLoaded {
        request_id: u64,
        load: AdminLoad,
    },
    ServerChecked {
        request_id: u64,
        server: ServerIdentity,
    },
    ServerCheckFailed {
        request_id: u64,
        message: String,
    },
    Authenticating {
        request_id: u64,
        server: ServerIdentity,
    },
    CatalogLoading {
        request_id: u64,
        server: ServerIdentity,
        user: UserDto,
    },
    ItemsLoaded {
        request_id: u64,
        server: ServerIdentity,
        user: UserDto,
        page: ItemsResponse,
    },
    ItemsFailed {
        request_id: u64,
        unsupported_filter: Option<ItemFilter>,
        error_class: ErrorClass,
        retry_after: Option<std::time::Duration>,
    },
    ItemDetailsLoaded {
        request_id: u64,
        item: Box<BaseItemDto>,
    },
    ItemDetailsFailed {
        request_id: u64,
        item_id: String,
        message: String,
        error_class: ErrorClass,
        retry_after: Option<std::time::Duration>,
    },
    WatchedStateUpdated {
        request_id: u64,
        item_id: String,
        played: bool,
        user_data: Box<UserItemDataDto>,
    },
    WatchedStateFailed {
        request_id: u64,
        item_id: String,
        message: String,
    },
    FavoriteStateUpdated {
        request_id: u64,
        item_id: String,
        favorite: bool,
        user_data: Box<UserItemDataDto>,
    },
    FavoriteStateFailed {
        request_id: u64,
        item_id: String,
        message: String,
    },
    HomeRowLoaded {
        request_id: u64,
        row: HomeRowKind,
        items: Vec<BaseItemDto>,
    },
    HomeRowFailed {
        request_id: u64,
        row: HomeRowKind,
        error_class: ErrorClass,
        retry_after: Option<std::time::Duration>,
    },
    HomeCatalogLoaded {
        request_id: u64,
        data: HomeCatalogData,
    },
    HomeCatalogFailed {
        request_id: u64,
        kind: HomeCatalogKind,
        error_class: ErrorClass,
        retry_after: Option<std::time::Duration>,
    },
    LiveTvChannelsLoaded {
        request_id: u64,
        page: LiveTvChannelPage,
    },
    LiveTvChannelsFailed {
        request_id: u64,
        error_class: ErrorClass,
    },
    LiveTvSearchResultsLoaded {
        request_id: u64,
        items: Vec<BaseItemDto>,
        start_index: u32,
        total_record_count: Option<u32>,
    },
    LiveTvSearchResultsFailed {
        request_id: u64,
    },
    LiveTvProgramsLoaded {
        request_id: u64,
        page: LiveTvProgramPage,
    },
    LiveTvProgramsFailed {
        request_id: u64,
        error_class: ErrorClass,
    },
    SeriesSeasonsLoaded {
        request_id: u64,
        series_id: String,
        seasons: Vec<BaseItemDto>,
    },
    SeriesSeasonsFailed {
        request_id: u64,
        series_id: String,
        message: String,
    },
    SeriesEpisodesLoaded {
        request_id: u64,
        season_id: String,
        episodes: Vec<BaseItemDto>,
    },
    SeriesEpisodesFailed {
        request_id: u64,
        season_id: String,
        message: String,
    },
    /// The server answered a forgot-password request.
    ForgotPasswordFinished {
        request_id: u64,
        result: Result<jellyfin_api::ForgotPasswordResult, ErrorClass>,
    },
    /// A metadata dialog request finished.
    MetadataFinished {
        request_id: u64,
        result: Result<MetadataReply, ErrorClass>,
    },
    /// A metadata dialog preview, decoded. `None` when it could not be loaded.
    MetadataThumbnailLoaded {
        request_id: u64,
        image: Option<(u32, u32, Vec<u8>)>,
    },
    /// An administrator action on one item finished.
    ItemAdminFinished {
        request_id: u64,
        item_id: String,
        action: ItemAdminAction,
        result: Result<(), ErrorClass>,
    },
    /// Genre names and studios `(id, name)` for the library page's filters. Empty on failure.
    GenresLoaded {
        request_id: u64,
        genres: Vec<String>,
        studios: Vec<(String, String)>,
    },
    /// Trickplay resolutions of the playing item's media source. Empty when it has none or on
    /// failure.
    TrickplayResolutionsLoaded {
        request_id: u64,
        item_id: String,
        resolutions: Vec<jellyfin_api::trickplay::TrickplayInfo>,
    },
    /// One decoded trickplay sheet as `(width, height, rgba)`, or `None` on failure.
    TrickplayTileLoaded {
        request_id: u64,
        item_id: String,
        image: Option<(u32, u32, Vec<u8>)>,
    },
    /// Remote subtitle search results for the subtitle search dialog.
    RemoteSubtitlesLoaded {
        request_id: u64,
        result: Result<Vec<jellyfin_api::RemoteSubtitleInfo>, SubtitleRequestError>,
    },
    /// The server's languages for the subtitle search dialog. Empty on failure.
    SubtitleCulturesLoaded {
        request_id: u64,
        cultures: Vec<jellyfin_api::CultureDto>,
    },
    /// A remote subtitle was downloaded to the server. `files` are the new external text
    /// subtitles the worker copied to local files for the player (only when asked).
    RemoteSubtitleDownloaded {
        request_id: u64,
        item_id: String,
        result: Result<Vec<DownloadedSubtitle>, SubtitleRequestError>,
    },
    /// Lyrics of the track on screen. Empty when it has none or on failure.
    LyricsLoaded {
        request_id: u64,
        lines: Vec<jellyfin_api::LyricLine>,
    },
    /// Trailers and special features of the item on screen. Empty on failure.
    ExtrasLoaded {
        request_id: u64,
        extras: Vec<BaseItemDto>,
    },
    /// The episodes of the season an episode page belongs to. Empty on failure.
    SeasonSiblingsLoaded {
        request_id: u64,
        episodes: Vec<BaseItemDto>,
    },
    /// Live TV recordings and scheduled timers.
    RecordingsLoaded {
        request_id: u64,
        result: Result<(Vec<BaseItemDto>, Vec<jellyfin_api::LiveTvTimer>), ErrorClass>,
    },
    /// A recording was scheduled or cancelled, or the server refused.
    TimerChanged {
        request_id: u64,
        result: Result<(), ErrorClass>,
    },
    /// A series' episodes in random order. Empty on failure.
    ShuffleLoaded {
        request_id: u64,
        episodes: Vec<BaseItemDto>,
    },
    /// The server's collections as `(id, name)`, for the card menu. Empty on failure.
    MenuCollectionsLoaded {
        request_id: u64,
        collections: Vec<(String, String)>,
    },
    /// The signed-in user's playlists as `(id, name)`, for the card menu. Empty on failure.
    UserPlaylistsLoaded {
        request_id: u64,
        playlists: Vec<(String, String)>,
    },
    /// The episode to play next in `series_id`. `None` when there is none or the request failed.
    SeriesNextUpLoaded {
        request_id: u64,
        series_id: String,
        episode: Option<Box<BaseItemDto>>,
    },
    /// The episodes either side of `episode_id`. Both are `None` when the request failed.
    AdjacentEpisodesLoaded {
        request_id: u64,
        episode_id: String,
        previous: Option<Box<BaseItemDto>>,
        next: Option<Box<BaseItemDto>>,
    },
    MusicAlbumTracksLoaded {
        request_id: u64,
        album_id: String,
        tracks: Vec<BaseItemDto>,
    },
    MusicAlbumTracksFailed {
        request_id: u64,
        album_id: String,
        error_class: ErrorClass,
    },
    PlaylistItemsLoaded {
        request_id: u64,
        playlist_id: String,
        entries: Vec<PlaylistEntry>,
    },
    PlaylistItemsFailed {
        request_id: u64,
        playlist_id: String,
        error_class: ErrorClass,
    },
    PlaylistCandidatesLoaded {
        request_id: u64,
        items: Vec<BaseItemDto>,
    },
    PlaylistCandidatesFailed {
        request_id: u64,
        error_class: ErrorClass,
    },
    QueueItemsRestored {
        request_id: u64,
        items: Vec<BaseItemDto>,
    },
    QueueItemsRestoreFailed {
        request_id: u64,
        error_class: ErrorClass,
    },
    PlaylistCreated {
        request_id: u64,
        playlist_id: String,
        name: String,
    },
    PlaylistCreateFailed {
        request_id: u64,
        error_class: ErrorClass,
    },
    PlaylistMutationCompleted {
        request_id: u64,
        playlist_id: String,
        kind: PlaylistMutationKind,
    },
    PlaylistMutationFailed {
        request_id: u64,
        playlist_id: String,
        kind: PlaylistMutationKind,
        error_class: ErrorClass,
    },
    SimilarItemsLoaded {
        request_id: u64,
        item_id: String,
        items: Vec<BaseItemDto>,
    },
    SimilarItemsFailed {
        request_id: u64,
        item_id: String,
    },
    PersonItemsLoaded {
        request_id: u64,
        person_id: String,
        items: Vec<BaseItemDto>,
    },
    PersonItemsFailed {
        request_id: u64,
        person_id: String,
        message: String,
    },
    PersonImageLoaded {
        request_id: u64,
        name: String,
        width: u32,
        height: u32,
        rgba: Vec<u8>,
    },
    PersonImageFailed {
        request_id: u64,
    },
    PrimaryArtworkLoaded {
        request_id: u64,
        item_id: String,
        width: u32,
        height: u32,
        rgba: Vec<u8>,
    },
    PrimaryArtworkFailed {
        request_id: u64,
    },
    PhotoThumbnailLoaded {
        request_id: u64,
        item_id: String,
        width: u32,
        height: u32,
        rgba: Vec<u8>,
    },
    PhotoThumbnailFailed {
        request_id: u64,
        item_id: String,
    },
    PhotoFullSizeLoaded {
        request_id: u64,
        item_id: String,
        width: u32,
        height: u32,
        rgba: Vec<u8>,
    },
    PhotoFullSizeFailed {
        request_id: u64,
        item_id: String,
    },
    BackdropArtworkLoaded {
        request_id: u64,
        item_id: String,
        width: u32,
        height: u32,
        rgba: Vec<u8>,
    },
    /// No backdrop exists; callers should continue with the detail view's fallback artwork.
    BackdropArtworkUnavailable {
        request_id: u64,
        item_id: String,
    },
    BackdropArtworkFailed {
        request_id: u64,
        item_id: String,
    },
    CustomBackdropLoaded {
        request_id: u64,
        width: u32,
        height: u32,
        rgba: Vec<u8>,
    },
    CustomBackdropFailed {
        request_id: u64,
    },
    LiveTvLogoLoaded {
        request_id: u64,
        channel_id: String,
        width: u32,
        height: u32,
        rgba: Vec<u8>,
    },
    LiveTvLogoFailed {
        request_id: u64,
        channel_id: String,
    },
    PlaybackInfoLoaded {
        request_id: u64,
        response: PlaybackInfoResponse,
    },
    PlaybackInfoFailed {
        request_id: u64,
        error_class: ErrorClass,
        retry_after: Option<std::time::Duration>,
    },
    PlaybackPrepared {
        request_id: u64,
        item_id: String,
        playback: Box<PreparedPlayback>,
    },
    /// Intro and outro segments for one item. Only sent when a skip setting is on (M2-08).
    MediaSegmentsLoaded {
        request_id: u64,
        item_id: String,
        segments: Vec<kattri_client_core::segments::MediaSegment>,
    },
    /// The segments request failed. Not shown to the viewer: playback continues without skip.
    MediaSegmentsFailed {
        request_id: u64,
    },
    /// A remote session command, parsed on the worker, that asks for this player action (M4-13).
    RemoteAction {
        action: LocalAction,
    },
    /// A command read from the session socket. The app maps it with the local volume maximum,
    /// which the worker does not know (M4-13).
    RemoteCommandReceived {
        command: RemoteCommand,
    },
    /// A SyncPlay playback command from the joined group, read from the session socket. The app
    /// maps it with its group clock and echo guard (M4-11).
    SyncPlayCommandReceived {
        command: jellyfin_api::websocket::SyncPlayMessage,
    },
    /// Group membership, participant, and state updates read from the session socket.
    SyncPlayGroupUpdate {
        update: jellyfin_api::websocket::SyncPlayGroupUpdate,
    },
    SyncPlayGroupsLoaded {
        request_id: u64,
        groups: Vec<jellyfin_api::SyncPlayGroup>,
    },
    SyncPlayGroupsFailed {
        request_id: u64,
    },
    SyncPlayGroupCreated {
        request_id: u64,
        group: jellyfin_api::SyncPlayGroup,
    },
    SyncPlayGroupCreateFailed {
        request_id: u64,
    },
    /// A server clock reading for the SyncPlay clock estimate (M4-11). `sent_at` and
    /// `received_at` bracket the request on the worker's monotonic clock. The app converts
    /// them to its own clock. `server_ms` is the server's reply time in Unix milliseconds.
    ServerClockSampled {
        sent_at: std::time::Instant,
        received_at: std::time::Instant,
        server_ms: i64,
    },
    /// Outcome of a SyncPlay join or leave (M4-11). `request_id` is the command's.
    SyncPlayJoined {
        request_id: u64,
        group_id: String,
    },
    SyncPlayJoinFailed {
        request_id: u64,
    },
    SyncPlayLeft {
        request_id: u64,
    },
    SyncPlayLeaveFailed {
        request_id: u64,
    },
    PlaybackPrepareFailed {
        request_id: u64,
        message: String,
    },
    PlaybackReportSent {
        request_id: u64,
    },
    PlaybackReportFailed {
        request_id: u64,
        message: String,
    },
    ConnectionFailed {
        request_id: Option<u64>,
        message: String,
    },
    ReauthenticationRequired {
        request_id: u64,
        message: String,
    },
    /// Whether "stay signed in" can work here: the operating system's credential store opened.
    /// When it cannot, the sign-in form does not offer the option.
    SavedSignInAvailability {
        available: bool,
    },
    /// A start-up attempt to resume a saved sign-in found nothing usable. `expired` is true
    /// when a saved token existed and the server refused it (it was deleted). The UI goes back
    /// to the sign-in form; the first case is silent, the second says so.
    ResumeUnavailable {
        request_id: u64,
        expired: bool,
    },
    QuickConnectAvailability {
        request_id: u64,
        enabled: bool,
    },
    /// Quick Connect issued a code for the user to approve on another device.
    QuickConnectCode {
        request_id: u64,
        code: String,
    },
    /// Quick Connect could not start. The message is safe to show.
    QuickConnectFailed {
        request_id: u64,
        message: String,
    },
    /// Result of one Quick Connect poll.
    QuickConnectPolled {
        request_id: u64,
        outcome: PollOutcome,
    },
    /// The download list after a command, or after a refresh. The rows hold no URLs or tokens.
    DownloadsUpdated {
        rows: Vec<downloads::DownloadRow>,
    },
    /// A download command could not be carried out. The message is safe to show.
    DownloadFailed {
        message: String,
    },
}

/// A Quick Connect request in flight. The secret stays inside the worker; the UI only
/// sees the short code.
struct QuickConnectSession {
    request_id: u64,
    client: JellyfinClient,
    request: QuickConnectRequest,
    identity: ServerIdentity,
}

enum WorkerCommand {
    CheckServer {
        request_id: u64,
        server_url: String,
        allow_insecure_remote_http: bool,
    },
    Connect {
        request_id: u64,
        server_url: String,
        username: String,
        password: Zeroizing<String>,
        allow_insecure_remote_http: bool,
        saved: SavedSignInRequest,
    },
    /// Ask whether the credential store can keep a sign-in.
    ProbeSavedSignIn,
    /// Sign in again with the token saved by an earlier "stay signed in" sign-in.
    ResumeSignIn {
        request_id: u64,
        server_url: String,
        allow_insecure_remote_http: bool,
        saved: SavedSignInRequest,
    },
    FetchItems {
        request_id: u64,
        query: ItemQuery,
    },
    RestoreQueueItems {
        request_id: u64,
        item_ids: Vec<String>,
    },
    GetItem {
        request_id: u64,
        item_id: String,
    },
    SetPlayed {
        request_id: u64,
        item_id: String,
        played: bool,
    },
    SetFavorite {
        request_id: u64,
        item_id: String,
        favorite: bool,
    },
    LoadDashboard {
        request_id: u64,
    },
    DownloadsOpen {
        dir: PathBuf,
    },
    DownloadsList,
    DownloadStart {
        item_id: String,
        base_url: String,
    },
    DownloadPause {
        item_id: String,
    },
    DownloadResume {
        item_id: String,
        base_url: String,
    },
    LoadHomeRow {
        request_id: u64,
        row: HomeRowKind,
    },
    LoadHomeCatalog {
        request_id: u64,
        kind: HomeCatalogKind,
    },
    LoadLiveTvChannels {
        request_id: u64,
        start_index: u32,
        limit: u32,
    },
    SearchLiveTvChannels {
        request_id: u64,
        term: String,
        start_index: u32,
        limit: u32,
    },
    LoadLiveTvPrograms {
        request_id: u64,
        window: GuideWindow,
        channel_ids: Vec<String>,
    },
    ListSeasons {
        request_id: u64,
        series_id: String,
    },
    ListMediaSegments {
        request_id: u64,
        item_id: String,
    },
    RemoteSessionMessage {
        message: String,
        max_volume_percent: u32,
    },
    SyncPlayJoin {
        request_id: u64,
        group_id: String,
    },
    SyncPlayList {
        request_id: u64,
    },
    SyncPlayCreate {
        request_id: u64,
        group_name: String,
    },
    SyncPlayLeave {
        request_id: u64,
    },
    SyncPlaySend {
        request_id: u64,
        command: SyncPlayCommand,
    },
    ListEpisodes {
        request_id: u64,
        series_id: String,
        season_id: String,
    },
    GetAdjacentEpisodes {
        request_id: u64,
        series_id: String,
        episode_id: String,
    },
    GetSeriesNextUp {
        request_id: u64,
        series_id: String,
    },
    ListUserPlaylists {
        request_id: u64,
    },
    ListGenres {
        request_id: u64,
        parent_id: Option<String>,
        item_types: Option<Vec<String>>,
    },
    ItemAdmin {
        request_id: u64,
        item_id: String,
        action: ItemAdminAction,
        /// The box set for [`ItemAdminAction::AddToCollection`].
        collection_id: Option<String>,
    },
    ListMenuCollections {
        request_id: u64,
    },
    Metadata {
        request_id: u64,
        item_id: String,
        op: MetadataOp,
    },
    MetadataThumbnail {
        request_id: u64,
        source: ThumbnailSource,
    },
    ShuffleSeries {
        request_id: u64,
        series_id: String,
    },
    LoadRecordings {
        request_id: u64,
    },
    ListSeasonSiblings {
        request_id: u64,
        series_id: String,
        season: i32,
    },
    ListExtras {
        request_id: u64,
        item_id: String,
    },
    GetLyrics {
        request_id: u64,
        item_id: String,
    },
    GetTrickplayResolutions {
        request_id: u64,
        item_id: String,
        media_source_id: Option<String>,
    },
    GetTrickplayTile {
        request_id: u64,
        item_id: String,
        media_source_id: Option<String>,
        width: u32,
        sheet: u32,
    },
    SearchRemoteSubtitles {
        request_id: u64,
        item_id: String,
        language: String,
    },
    ListSubtitleCultures {
        request_id: u64,
    },
    DownloadRemoteSubtitle {
        request_id: u64,
        item_id: String,
        subtitle_id: String,
        /// Copy the new subtitle to a local file so the playing video can load it.
        for_player: bool,
    },
    ChangeTimer {
        request_id: u64,
        change: TimerChange,
    },
    ListMusicAlbumTracks {
        request_id: u64,
        album_id: String,
    },
    ListPlaylistItems {
        request_id: u64,
        playlist_id: String,
    },
    LoadPlaylistCandidates {
        request_id: u64,
    },
    CreatePlaylist {
        request_id: u64,
        name: String,
    },
    AddPlaylistItems {
        request_id: u64,
        playlist_id: String,
        item_ids: Vec<String>,
    },
    RemovePlaylistEntry {
        request_id: u64,
        playlist_id: String,
        entry_id: String,
    },
    MovePlaylistEntry {
        request_id: u64,
        playlist_id: String,
        entry_id: String,
        new_index: u32,
    },
    ListSimilar {
        request_id: u64,
        item_id: String,
        limit: u32,
    },
    ListPersonItems {
        request_id: u64,
        person_id: String,
        limit: u32,
    },
    GetPersonArtwork {
        request_id: u64,
        name: String,
        tag: Option<String>,
    },
    GetPrimaryArtwork {
        request_id: u64,
        item_id: String,
        image_tag: Option<String>,
    },
    LoadPhotoThumbnail {
        request_id: u64,
        item_id: String,
        image_tag: Option<String>,
        photo_index: usize,
        visible: bool,
    },
    LoadPhotoFullSize {
        request_id: u64,
        item_id: String,
        image_tag: Option<String>,
        photo_index: usize,
        visible: bool,
    },
    GetBackdropArtwork {
        request_id: u64,
        item_id: String,
    },
    LoadCustomBackdrop {
        request_id: u64,
        path: PathBuf,
    },
    GetLiveTvLogo {
        request_id: u64,
        channel_id: String,
        image_tag: String,
    },
    PlaybackInfo {
        request_id: u64,
        item_id: String,
        is_audio: bool,
        request: Box<PlaybackInfoRequest>,
    },
    PreparePlayback {
        request_id: u64,
        negotiation_id: u64,
        item_id: String,
        source_index: usize,
        play_method: PlayMethod,
        start_time_ticks: Option<i64>,
        live_device_profile: Option<Box<DeviceProfile>>,
        live_tune_started_at: Option<std::time::Instant>,
    },
    CloseLiveStream {
        live_stream_id: String,
    },
    ReportPlayback {
        request_id: u64,
        action: PlaybackReportAction,
    },
    QuickConnectStart {
        request_id: u64,
        server_url: String,
        allow_insecure_remote_http: bool,
    },
    CheckQuickConnect {
        request_id: u64,
        server_url: String,
        allow_insecure_remote_http: bool,
    },
    QuickConnectPoll {
        request_id: u64,
    },
    ForgotPassword {
        request_id: u64,
        server_url: String,
        allow_insecure_remote_http: bool,
        username: String,
    },
    QuickConnectAuthenticate {
        request_id: u64,
        saved: SavedSignInRequest,
    },
    QuickConnectCancel {
        request_id: u64,
    },
    Disconnect,
    ApplyUpdate {
        job: Box<ApplyJob>,
    },
    RollbackUpdate {
        store_dir: PathBuf,
    },
    Shutdown,
}

impl ApiWorker {
    #[cfg(test)]
    pub(crate) fn queued_for_test() -> Self {
        let (_sender, receiver) = mpsc::channel(EVENT_CAPACITY);
        Self {
            commands: Arc::new(CommandQueue::default()),
            events: Mutex::new(receiver),
            thread: None,
            #[cfg(feature = "photo-counters")]
            photo_decode_counters: Default::default(),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_event_sender_for_test() -> (Self, mpsc::Sender<WorkerEvent>) {
        let (sender, receiver) = mpsc::channel(EVENT_CAPACITY);
        (
            Self {
                commands: Arc::new(CommandQueue::default()),
                events: Mutex::new(receiver),
                thread: None,
                #[cfg(feature = "photo-counters")]
                photo_decode_counters: Default::default(),
            },
            sender,
        )
    }

    pub fn new(ctx: egui::Context) -> std::io::Result<Self> {
        let commands = Arc::new(CommandQueue::default());
        let worker_commands = commands.clone();
        let (event_tx, event_rx) = mpsc::channel(EVENT_CAPACITY);
        #[cfg(feature = "photo-counters")]
        let photo_decode_counters = kattri_client_core::photo_grid::DecodeCounters::default();
        let sink = WorkerSink {
            sender: event_tx,
            ctx,
            decoders: Arc::new(Semaphore::new(2)),
            authentication_expired: Arc::new(AtomicBool::new(false)),
            remote_socket: true,
            server_clock: true,
            saved_sign_in: SavedSignInStore::os(),
            #[cfg(feature = "photo-counters")]
            photo_decode_counters: photo_decode_counters.clone(),
        };
        let device_id = crate::device_id::load_or_create();
        let thread = std::thread::Builder::new()
            .name("jellyfin-api-worker".into())
            .spawn(move || run_worker(worker_commands, sink, device_id))?;
        Ok(Self {
            commands,
            events: Mutex::new(event_rx),
            thread: Some(thread),
            #[cfg(feature = "photo-counters")]
            photo_decode_counters,
        })
    }

    pub fn connect(
        &self,
        request_id: u64,
        server_url: String,
        username: String,
        password: Zeroizing<String>,
        allow_insecure_remote_http: bool,
        saved: SavedSignInRequest,
    ) -> Result<(), WorkerSubmitError> {
        self.commands.send(WorkerCommand::Connect {
            request_id,
            server_url,
            username,
            password,
            allow_insecure_remote_http,
            saved,
        })
    }

    /// Ask whether "stay signed in" can work on this machine. Answers with
    /// `SavedSignInAvailability`.
    pub(crate) fn probe_saved_sign_in(&self) -> Result<(), WorkerSubmitError> {
        self.commands.send(WorkerCommand::ProbeSavedSignIn)
    }

    /// Sign in with the token saved earlier for this profile and server, if there is one.
    /// Answers with the same events as `connect`, or `ResumeUnavailable`.
    pub(crate) fn resume_sign_in(
        &self,
        request_id: u64,
        server_url: String,
        allow_insecure_remote_http: bool,
        saved: SavedSignInRequest,
    ) -> Result<(), WorkerSubmitError> {
        self.commands.send(WorkerCommand::ResumeSignIn {
            request_id,
            server_url,
            allow_insecure_remote_http,
            saved,
        })
    }

    pub(crate) fn check_server(
        &self,
        request_id: u64,
        server_url: String,
        allow_insecure_remote_http: bool,
    ) -> Result<(), WorkerSubmitError> {
        self.commands.send(WorkerCommand::CheckServer {
            request_id,
            server_url,
            allow_insecure_remote_http,
        })
    }

    pub fn fetch_items(&self, request_id: u64, query: ItemQuery) -> Result<(), WorkerSubmitError> {
        self.commands
            .send(WorkerCommand::FetchItems { request_id, query })
    }

    pub(crate) fn restore_queue_items(
        &self,
        request_id: u64,
        item_ids: Vec<String>,
    ) -> Result<(), WorkerSubmitError> {
        self.commands.send(WorkerCommand::RestoreQueueItems {
            request_id,
            item_ids,
        })
    }

    pub fn get_item(&self, request_id: u64, item_id: String) -> Result<(), WorkerSubmitError> {
        self.commands.send(WorkerCommand::GetItem {
            request_id,
            item_id,
        })
    }

    pub(crate) fn set_played(
        &self,
        request_id: u64,
        item_id: String,
        played: bool,
    ) -> Result<(), WorkerSubmitError> {
        self.commands.send(WorkerCommand::SetPlayed {
            request_id,
            item_id,
            played,
        })
    }

    pub(crate) fn set_favorite(
        &self,
        request_id: u64,
        item_id: String,
        favorite: bool,
    ) -> Result<(), WorkerSubmitError> {
        self.commands.send(WorkerCommand::SetFavorite {
            request_id,
            item_id,
            favorite,
        })
    }

    /// Opens the profile's download folder. Later download commands use the manager it opens.
    pub(crate) fn download_open(&self, dir: PathBuf) -> Result<(), WorkerSubmitError> {
        self.commands.send(WorkerCommand::DownloadsOpen { dir })
    }

    pub(crate) fn download_list(&self) -> Result<(), WorkerSubmitError> {
        self.commands.send(WorkerCommand::DownloadsList)
    }

    pub(crate) fn download_start(
        &self,
        item_id: String,
        base_url: String,
    ) -> Result<(), WorkerSubmitError> {
        self.commands
            .send(WorkerCommand::DownloadStart { item_id, base_url })
    }

    /// Sends a pause or resume the user chose on the Downloads page.
    pub(crate) fn download_intent(
        &self,
        intent: &DownloadIntent,
        base_url: &str,
    ) -> Result<(), WorkerSubmitError> {
        self.commands
            .send(downloads::command_for_intent(intent, base_url))
    }

    /// Read-only admin dashboard reads (M4-22). The result arrives as `DashboardLoaded`.
    pub fn load_dashboard(&self, request_id: u64) -> Result<(), WorkerSubmitError> {
        self.commands
            .send(WorkerCommand::LoadDashboard { request_id })
    }

    pub fn load_home_row(
        &self,
        request_id: u64,
        row: HomeRowKind,
    ) -> Result<(), WorkerSubmitError> {
        self.commands
            .send(WorkerCommand::LoadHomeRow { request_id, row })
    }

    pub fn load_home_catalog(
        &self,
        request_id: u64,
        kind: HomeCatalogKind,
    ) -> Result<(), WorkerSubmitError> {
        self.commands
            .send(WorkerCommand::LoadHomeCatalog { request_id, kind })
    }

    pub fn load_live_tv_channels(
        &self,
        request_id: u64,
        start_index: u32,
        limit: u32,
    ) -> Result<(), WorkerSubmitError> {
        self.commands.send(WorkerCommand::LoadLiveTvChannels {
            request_id,
            start_index,
            limit,
        })
    }

    pub(crate) fn search_live_tv_channels(
        &self,
        request_id: u64,
        term: String,
        start_index: u32,
        limit: u32,
    ) -> Result<(), WorkerSubmitError> {
        self.commands.send(WorkerCommand::SearchLiveTvChannels {
            request_id,
            term,
            start_index,
            limit,
        })
    }

    pub fn load_live_tv_programs(
        &self,
        request_id: u64,
        window: GuideWindow,
        channel_ids: Vec<String>,
    ) -> Result<(), WorkerSubmitError> {
        self.commands.send(WorkerCommand::LoadLiveTvPrograms {
            request_id,
            window,
            channel_ids,
        })
    }

    pub(crate) fn list_seasons(
        &self,
        request_id: u64,
        series_id: String,
    ) -> Result<(), WorkerSubmitError> {
        self.commands.send(WorkerCommand::ListSeasons {
            request_id,
            series_id,
        })
    }

    pub(crate) fn list_media_segments(
        &self,
        request_id: u64,
        item_id: String,
    ) -> Result<(), WorkerSubmitError> {
        self.commands.send(WorkerCommand::ListMediaSegments {
            request_id,
            item_id,
        })
    }

    /// Queue one inbound remote session message for parsing on the worker (M4-13).
    pub fn remote_session_message(
        &self,
        message: String,
        max_volume_percent: u32,
    ) -> Result<(), WorkerSubmitError> {
        self.commands.send(WorkerCommand::RemoteSessionMessage {
            message,
            max_volume_percent,
        })
    }

    pub(crate) fn sync_play_join(
        &self,
        request_id: u64,
        group_id: String,
    ) -> Result<(), WorkerSubmitError> {
        self.commands.send(WorkerCommand::SyncPlayJoin {
            request_id,
            group_id,
        })
    }

    pub(crate) fn sync_play_list(&self, request_id: u64) -> Result<(), WorkerSubmitError> {
        self.commands
            .send(WorkerCommand::SyncPlayList { request_id })
    }

    pub(crate) fn sync_play_create(
        &self,
        request_id: u64,
        group_name: String,
    ) -> Result<(), WorkerSubmitError> {
        self.commands.send(WorkerCommand::SyncPlayCreate {
            request_id,
            group_name,
        })
    }

    pub(crate) fn sync_play_leave(&self, request_id: u64) -> Result<(), WorkerSubmitError> {
        self.commands
            .send(WorkerCommand::SyncPlayLeave { request_id })
    }

    /// Send a local pause, unpause or seek to the joined group (M4-11). A refusal is logged by
    /// the worker, since no group is joined to report it against.
    pub(crate) fn sync_play_command(
        &self,
        request_id: u64,
        command: SyncPlayCommand,
    ) -> Result<(), WorkerSubmitError> {
        self.commands.send(WorkerCommand::SyncPlaySend {
            request_id,
            command,
        })
    }

    pub(crate) fn list_episodes(
        &self,
        request_id: u64,
        series_id: String,
        season_id: String,
    ) -> Result<(), WorkerSubmitError> {
        self.commands.send(WorkerCommand::ListEpisodes {
            request_id,
            series_id,
            season_id,
        })
    }

    pub(crate) fn get_adjacent_episodes(
        &self,
        request_id: u64,
        series_id: String,
        episode_id: String,
    ) -> Result<(), WorkerSubmitError> {
        self.commands.send(WorkerCommand::GetAdjacentEpisodes {
            request_id,
            series_id,
            episode_id,
        })
    }

    pub(crate) fn item_admin(
        &self,
        request_id: u64,
        item_id: String,
        action: ItemAdminAction,
        collection_id: Option<String>,
    ) -> Result<(), WorkerSubmitError> {
        self.commands.send(WorkerCommand::ItemAdmin {
            request_id,
            item_id,
            action,
            collection_id,
        })
    }

    pub(crate) fn metadata(
        &self,
        request_id: u64,
        item_id: String,
        op: MetadataOp,
    ) -> Result<(), WorkerSubmitError> {
        self.commands.send(WorkerCommand::Metadata {
            request_id,
            item_id,
            op,
        })
    }

    pub(crate) fn metadata_thumbnail(
        &self,
        request_id: u64,
        source: ThumbnailSource,
    ) -> Result<(), WorkerSubmitError> {
        self.commands
            .send(WorkerCommand::MetadataThumbnail { request_id, source })
    }

    pub(crate) fn get_trickplay_resolutions(
        &self,
        request_id: u64,
        item_id: String,
        media_source_id: Option<String>,
    ) -> Result<(), WorkerSubmitError> {
        self.commands.send(WorkerCommand::GetTrickplayResolutions {
            request_id,
            item_id,
            media_source_id,
        })
    }

    pub(crate) fn get_trickplay_tile(
        &self,
        request_id: u64,
        item_id: String,
        media_source_id: Option<String>,
        width: u32,
        sheet: u32,
    ) -> Result<(), WorkerSubmitError> {
        self.commands.send(WorkerCommand::GetTrickplayTile {
            request_id,
            item_id,
            media_source_id,
            width,
            sheet,
        })
    }

    pub(crate) fn search_remote_subtitles(
        &self,
        request_id: u64,
        item_id: String,
        language: String,
    ) -> Result<(), WorkerSubmitError> {
        self.commands.send(WorkerCommand::SearchRemoteSubtitles {
            request_id,
            item_id,
            language,
        })
    }

    pub(crate) fn list_subtitle_cultures(&self, request_id: u64) -> Result<(), WorkerSubmitError> {
        self.commands
            .send(WorkerCommand::ListSubtitleCultures { request_id })
    }

    pub(crate) fn download_remote_subtitle(
        &self,
        request_id: u64,
        item_id: String,
        subtitle_id: String,
        for_player: bool,
    ) -> Result<(), WorkerSubmitError> {
        self.commands.send(WorkerCommand::DownloadRemoteSubtitle {
            request_id,
            item_id,
            subtitle_id,
            for_player,
        })
    }

    pub(crate) fn get_lyrics(
        &self,
        request_id: u64,
        item_id: String,
    ) -> Result<(), WorkerSubmitError> {
        self.commands.send(WorkerCommand::GetLyrics {
            request_id,
            item_id,
        })
    }

    pub(crate) fn list_extras(
        &self,
        request_id: u64,
        item_id: String,
    ) -> Result<(), WorkerSubmitError> {
        self.commands.send(WorkerCommand::ListExtras {
            request_id,
            item_id,
        })
    }

    pub(crate) fn list_season_siblings(
        &self,
        request_id: u64,
        series_id: String,
        season: i32,
    ) -> Result<(), WorkerSubmitError> {
        self.commands.send(WorkerCommand::ListSeasonSiblings {
            request_id,
            series_id,
            season,
        })
    }

    pub(crate) fn load_recordings(&self, request_id: u64) -> Result<(), WorkerSubmitError> {
        self.commands
            .send(WorkerCommand::LoadRecordings { request_id })
    }

    pub(crate) fn change_timer(
        &self,
        request_id: u64,
        change: TimerChange,
    ) -> Result<(), WorkerSubmitError> {
        self.commands
            .send(WorkerCommand::ChangeTimer { request_id, change })
    }

    pub(crate) fn shuffle_series(
        &self,
        request_id: u64,
        series_id: String,
    ) -> Result<(), WorkerSubmitError> {
        self.commands.send(WorkerCommand::ShuffleSeries {
            request_id,
            series_id,
        })
    }

    pub(crate) fn list_menu_collections(&self, request_id: u64) -> Result<(), WorkerSubmitError> {
        self.commands
            .send(WorkerCommand::ListMenuCollections { request_id })
    }

    pub(crate) fn list_genres(
        &self,
        request_id: u64,
        parent_id: Option<String>,
        item_types: Option<Vec<String>>,
    ) -> Result<(), WorkerSubmitError> {
        self.commands.send(WorkerCommand::ListGenres {
            request_id,
            parent_id,
            item_types,
        })
    }

    pub(crate) fn list_user_playlists(&self, request_id: u64) -> Result<(), WorkerSubmitError> {
        self.commands
            .send(WorkerCommand::ListUserPlaylists { request_id })
    }

    pub(crate) fn get_series_next_up(
        &self,
        request_id: u64,
        series_id: String,
    ) -> Result<(), WorkerSubmitError> {
        self.commands.send(WorkerCommand::GetSeriesNextUp {
            request_id,
            series_id,
        })
    }

    pub(crate) fn list_music_album_tracks(
        &self,
        request_id: u64,
        album_id: String,
    ) -> Result<(), WorkerSubmitError> {
        self.commands.send(WorkerCommand::ListMusicAlbumTracks {
            request_id,
            album_id,
        })
    }

    pub(crate) fn list_playlist_items(
        &self,
        request_id: u64,
        playlist_id: String,
    ) -> Result<(), WorkerSubmitError> {
        self.commands.send(WorkerCommand::ListPlaylistItems {
            request_id,
            playlist_id,
        })
    }

    pub(crate) fn load_playlist_candidates(
        &self,
        request_id: u64,
    ) -> Result<(), WorkerSubmitError> {
        self.commands
            .send(WorkerCommand::LoadPlaylistCandidates { request_id })
    }

    pub(crate) fn create_playlist(
        &self,
        request_id: u64,
        name: String,
    ) -> Result<(), WorkerSubmitError> {
        self.commands
            .send(WorkerCommand::CreatePlaylist { request_id, name })
    }

    pub(crate) fn add_playlist_items(
        &self,
        request_id: u64,
        playlist_id: String,
        item_ids: Vec<String>,
    ) -> Result<(), WorkerSubmitError> {
        self.commands.send(WorkerCommand::AddPlaylistItems {
            request_id,
            playlist_id,
            item_ids,
        })
    }

    pub(crate) fn remove_playlist_entry(
        &self,
        request_id: u64,
        playlist_id: String,
        entry_id: String,
    ) -> Result<(), WorkerSubmitError> {
        self.commands.send(WorkerCommand::RemovePlaylistEntry {
            request_id,
            playlist_id,
            entry_id,
        })
    }

    pub(crate) fn move_playlist_entry(
        &self,
        request_id: u64,
        playlist_id: String,
        entry_id: String,
        new_index: u32,
    ) -> Result<(), WorkerSubmitError> {
        self.commands.send(WorkerCommand::MovePlaylistEntry {
            request_id,
            playlist_id,
            entry_id,
            new_index,
        })
    }

    pub(crate) fn list_similar(
        &self,
        request_id: u64,
        item_id: String,
        limit: u32,
    ) -> Result<(), WorkerSubmitError> {
        self.commands.send(WorkerCommand::ListSimilar {
            request_id,
            item_id,
            limit,
        })
    }

    pub(crate) fn list_person_items(
        &self,
        request_id: u64,
        person_id: String,
        limit: u32,
    ) -> Result<(), WorkerSubmitError> {
        self.commands.send(WorkerCommand::ListPersonItems {
            request_id,
            person_id,
            limit,
        })
    }

    pub(crate) fn get_person_artwork(
        &self,
        request_id: u64,
        name: String,
        tag: Option<String>,
    ) -> Result<(), WorkerSubmitError> {
        self.commands.send(WorkerCommand::GetPersonArtwork {
            request_id,
            name,
            tag,
        })
    }

    pub fn get_primary_artwork(
        &self,
        request_id: u64,
        item_id: String,
        image_tag: Option<String>,
    ) -> Result<(), WorkerSubmitError> {
        self.commands.send(WorkerCommand::GetPrimaryArtwork {
            request_id,
            item_id,
            image_tag,
        })
    }

    pub(crate) fn load_photo_thumbnail(
        &self,
        request_id: u64,
        item_id: String,
        image_tag: Option<String>,
        photo_index: usize,
        visible: bool,
    ) -> Result<(), WorkerSubmitError> {
        self.commands.send(WorkerCommand::LoadPhotoThumbnail {
            request_id,
            item_id,
            image_tag,
            photo_index,
            visible,
        })
    }

    pub(crate) fn load_photo_full_size(
        &self,
        request_id: u64,
        item_id: String,
        image_tag: Option<String>,
        photo_index: usize,
        visible: bool,
    ) -> Result<(), WorkerSubmitError> {
        self.commands.send(WorkerCommand::LoadPhotoFullSize {
            request_id,
            item_id,
            image_tag,
            photo_index,
            visible,
        })
    }

    /// Cancel only queued or active photo image work matching these request IDs.
    /// Queued jobs are removed immediately; active jobs are aborted by the scheduler,
    /// dropping their HTTP/decode future. Completion events already delivered remain
    /// subject to the UI's request-ID stale-result check.
    pub fn cancel_photo_image_requests(
        &self,
        request_ids: Vec<u64>,
    ) -> Result<(), WorkerSubmitError> {
        self.commands.cancel_photo_image_requests(request_ids)
    }

    #[cfg(feature = "photo-counters")]
    pub(crate) fn photo_decode_counters(&self) -> kattri_client_core::photo_grid::DecodeSnapshot {
        self.photo_decode_counters.snapshot()
    }

    #[cfg(feature = "photo-counters")]
    pub(crate) fn record_photo_decode_request(&self) {
        self.photo_decode_counters.record_request();
    }

    #[cfg(feature = "photo-counters")]
    pub(crate) fn record_photo_decode_cancellations(&self, count: usize) {
        self.photo_decode_counters.record_cancelled(count);
    }

    pub fn get_backdrop_artwork(
        &self,
        request_id: u64,
        item_id: String,
    ) -> Result<(), WorkerSubmitError> {
        self.commands.send(WorkerCommand::GetBackdropArtwork {
            request_id,
            item_id,
        })
    }

    pub fn load_custom_backdrop(
        &self,
        request_id: u64,
        path: PathBuf,
    ) -> Result<(), WorkerSubmitError> {
        self.commands
            .send(WorkerCommand::LoadCustomBackdrop { request_id, path })
    }

    pub(crate) fn get_live_tv_logo(
        &self,
        request_id: u64,
        channel_id: String,
        image_tag: String,
    ) -> Result<(), WorkerSubmitError> {
        self.commands.send(WorkerCommand::GetLiveTvLogo {
            request_id,
            channel_id,
            image_tag,
        })
    }

    pub fn playback_info(
        &self,
        request_id: u64,
        item_id: String,
        is_audio: bool,
        request: PlaybackInfoRequest,
    ) -> Result<(), WorkerSubmitError> {
        self.commands.send(WorkerCommand::PlaybackInfo {
            request_id,
            item_id,
            is_audio,
            request: Box::new(request),
        })
    }

    pub fn prepare_playback(
        &self,
        request_id: u64,
        negotiation_id: u64,
        item_id: String,
        source_index: usize,
        play_method: PlayMethod,
        start_time_ticks: Option<i64>,
    ) -> Result<(), WorkerSubmitError> {
        self.commands.send(WorkerCommand::PreparePlayback {
            request_id,
            negotiation_id,
            item_id,
            source_index,
            play_method,
            start_time_ticks,
            live_device_profile: None,
            live_tune_started_at: None,
        })
    }

    pub fn tune_live_tv_channel(
        &self,
        request_id: u64,
        channel_id: String,
        device_profile: &DeviceProfile,
        tune_started_at: std::time::Instant,
    ) -> Result<(), WorkerSubmitError> {
        self.commands.send(WorkerCommand::PreparePlayback {
            request_id,
            negotiation_id: 0,
            item_id: channel_id,
            source_index: 0,
            play_method: PlayMethod::DirectPlay,
            start_time_ticks: None,
            live_device_profile: Some(Box::new(device_profile.clone())),
            live_tune_started_at: Some(tune_started_at),
        })
    }

    pub(crate) fn close_live_stream(
        &self,
        live_stream_id: String,
    ) -> Result<(), WorkerSubmitError> {
        self.commands
            .send(WorkerCommand::CloseLiveStream { live_stream_id })
    }

    pub fn report_playback_action(
        &self,
        request_id: u64,
        action: PlaybackReportAction,
    ) -> Result<(), WorkerSubmitError> {
        self.commands
            .send(WorkerCommand::ReportPlayback { request_id, action })
    }

    pub fn disconnect(&self) -> Result<(), WorkerSubmitError> {
        self.commands.send(WorkerCommand::Disconnect)
    }

    /// Queue a verified update for the file work. Runs on the worker, never the UI thread.
    pub(crate) fn apply_update(&self, job: ApplyJob) -> Result<(), WorkerSubmitError> {
        self.commands
            .send(WorkerCommand::ApplyUpdate { job: Box::new(job) })
    }

    /// Queue a rollback. Only the user asking for it reaches this.
    pub(crate) fn rollback_update(&self, store_dir: PathBuf) -> Result<(), WorkerSubmitError> {
        self.commands
            .send(WorkerCommand::RollbackUpdate { store_dir })
    }

    pub(crate) fn quick_connect_start(
        &self,
        request_id: u64,
        server_url: String,
        allow_insecure_remote_http: bool,
    ) -> Result<(), WorkerSubmitError> {
        self.commands.send(WorkerCommand::QuickConnectStart {
            request_id,
            server_url,
            allow_insecure_remote_http,
        })
    }

    pub(crate) fn check_quick_connect(
        &self,
        request_id: u64,
        server_url: String,
        allow_insecure_remote_http: bool,
    ) -> Result<(), WorkerSubmitError> {
        self.commands.send(WorkerCommand::CheckQuickConnect {
            request_id,
            server_url,
            allow_insecure_remote_http,
        })
    }

    pub(crate) fn forgot_password(
        &self,
        request_id: u64,
        server_url: String,
        allow_insecure_remote_http: bool,
        username: String,
    ) -> Result<(), WorkerSubmitError> {
        self.commands.send(WorkerCommand::ForgotPassword {
            request_id,
            server_url,
            allow_insecure_remote_http,
            username,
        })
    }

    pub(crate) fn quick_connect_poll(&self, request_id: u64) -> Result<(), WorkerSubmitError> {
        self.commands
            .send(WorkerCommand::QuickConnectPoll { request_id })
    }

    pub(crate) fn quick_connect_authenticate(
        &self,
        request_id: u64,
        saved: SavedSignInRequest,
    ) -> Result<(), WorkerSubmitError> {
        self.commands
            .send(WorkerCommand::QuickConnectAuthenticate { request_id, saved })
    }

    pub(crate) fn quick_connect_cancel(&self, request_id: u64) -> Result<(), WorkerSubmitError> {
        self.commands
            .send(WorkerCommand::QuickConnectCancel { request_id })
    }

    pub fn try_recv(&self) -> Option<WorkerEvent> {
        self.events.lock().ok()?.try_recv().ok()
    }
    /// Cancel obsolete read-only work after its UI loading/request IDs are reset.
    pub(crate) fn cancel_reads(&self) {
        self.commands.cancel_reads();
    }

    pub(crate) fn cancel_navigation_reads(&self) {
        self.commands.cancel_navigation_reads();
    }

    /// Flush accepted ordered work for at most two seconds, then report any
    /// abandoned commands. An interrupted mutation's server outcome is unknown.
    pub fn shutdown(&mut self) -> WorkerShutdownReport {
        let _ = self.commands.send(WorkerCommand::Shutdown);
        if let Ok(events) = self.events.get_mut() {
            events.close();
            while events.try_recv().is_ok() {}
        }
        let report = match self.thread.take() {
            Some(thread) => thread.join().unwrap_or(WorkerShutdownReport {
                abandoned_commands: 0,
                worker_panicked: true,
            }),
            None => WorkerShutdownReport::default(),
        };
        if report.abandoned_commands != 0 || report.worker_panicked {
            tracing::warn!(
                abandoned_commands = report.abandoned_commands,
                worker_panicked = report.worker_panicked,
                "network worker closed with unfinished ordered work; server outcomes may be unknown"
            );
        }
        report
    }
}

/// Builds a client for `server_url`. The error text is safe to show.
fn build_client(
    server_url: &str,
    allow_insecure_remote_http: bool,
    device_id: &str,
) -> Result<JellyfinClient, String> {
    let mut config = ClientConfig::new(
        server_url,
        "Jellyfin Kattri",
        std::env::consts::OS,
        device_id,
        env!("CARGO_PKG_VERSION"),
    )
    .map_err(|error| error.to_string())?;
    if allow_insecure_remote_http {
        config = config.with_insecure_remote_http();
    }
    JellyfinClient::new(config).map_err(|error| error.to_string())
}

fn sign_in_error_message(error: &ApiError, authenticating: bool) -> String {
    match error {
        ApiError::Tls => {
            "Secure connection failed. Check the server certificate and TLS settings.".into()
        }
        ApiError::Unreachable => {
            "Could not reach the Jellyfin server. Check its address and your network.".into()
        }
        ApiError::Timeout => "The Jellyfin server did not respond in time.".into(),
        ApiError::HttpStatus(status) if status.as_u16() == 404 && !authenticating => {
            "Server not found at this address.".into()
        }
        ApiError::ReauthenticationRequired if authenticating => {
            "Sign-in failed. Check your username and password.".into()
        }
        ApiError::HttpStatus(status) if status.as_u16() == 401 && authenticating => {
            "Sign-in failed. Check your username and password.".into()
        }
        _ => error.to_string(),
    }
}

#[cfg(test)]
mod sign_in_error_tests {
    use super::sign_in_error_message;
    use jellyfin_api::ApiError;

    #[test]
    fn connection_and_authentication_failures_have_plain_language_messages() {
        assert!(sign_in_error_message(&ApiError::Tls, false).contains("certificate"));
        assert!(sign_in_error_message(&ApiError::Unreachable, false).contains("Could not reach"));
        assert!(sign_in_error_message(&ApiError::Timeout, false).contains("did not respond"));
        assert_eq!(
            sign_in_error_message(&ApiError::ReauthenticationRequired, true),
            "Sign-in failed. Check your username and password."
        );
    }
}

/// Maps one Quick Connect status request to the poll outcome the state machine expects.
/// A 404 means the code expired or was never issued.
fn quick_connect_outcome(result: Result<QuickConnectStatus, ApiError>) -> PollOutcome {
    match result {
        Ok(QuickConnectStatus::Pending) => PollOutcome::Pending,
        Ok(QuickConnectStatus::Authorized) => PollOutcome::Authorized,
        Err(ApiError::HttpStatus(status)) if status.as_u16() == 404 => PollOutcome::NotFound,
        Err(_) => PollOutcome::Error,
    }
}

impl Drop for ApiWorker {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

#[derive(Clone)]
struct WorkerSink {
    sender: mpsc::Sender<WorkerEvent>,
    ctx: egui::Context,
    decoders: Arc<Semaphore>,
    authentication_expired: Arc<AtomicBool>,
    /// Whether a sign-in opens the remote-control socket (M4-13). Test sinks leave it off:
    /// the loopback fixtures in worker tests count every request they receive.
    remote_socket: bool,
    /// Whether sign-in starts the server clock sampler (M4-11). Test sinks leave it off for
    /// the same reason as `remote_socket`.
    server_clock: bool,
    /// Where "stay signed in" tokens go. Test sinks use no store unless a test injects one.
    saved_sign_in: SavedSignInStore,
    #[cfg(feature = "photo-counters")]
    photo_decode_counters: kattri_client_core::photo_grid::DecodeCounters,
}

impl WorkerSink {
    fn for_request(&self) -> Self {
        Self {
            authentication_expired: Arc::new(AtomicBool::new(false)),
            ..self.clone()
        }
    }

    fn authentication_expired(&self) -> bool {
        self.authentication_expired.load(Ordering::Acquire)
    }

    #[cfg(test)]
    fn for_test(sender: mpsc::Sender<WorkerEvent>) -> Self {
        Self {
            sender,
            ctx: egui::Context::default(),
            decoders: Arc::new(Semaphore::new(2)),
            authentication_expired: Arc::new(AtomicBool::new(false)),
            remote_socket: false,
            server_clock: false,
            saved_sign_in: SavedSignInStore::off(),
            #[cfg(feature = "photo-counters")]
            photo_decode_counters: Default::default(),
        }
    }
}

#[derive(Default)]
struct WorkerState {
    client: Option<JellyfinClient>,
    session: Option<Session>,
    server: Option<ServerIdentity>,
    playback_info_cache: Option<CachedPlaybackInfo>,
    quick_connect: Option<QuickConnectSession>,
    /// Owned by the worker so that its disk and network calls stay off the UI thread.
    downloads: Option<DownloadManager>,
    /// Inbound remote-control socket for the signed-in session (M4-13). Dropping it stops
    /// the socket thread. Snapshots for read commands never hold one.
    session_socket: Option<session_socket::SessionSocketHandle>,
    /// Server clock sampler for SyncPlay (M4-11). It follows the session the way the socket
    /// does, and it reads the clock only while a group is joined.
    clock_sampler: Option<clock_sampler::ClockSamplerHandle>,
    /// The "stay signed in" token saved for the live session. It is deleted when the session
    /// ends by sign-out, expiry, or being replaced.
    saved_sign_in: Option<SavedEntry>,
}

impl WorkerState {
    fn read_snapshot(&self) -> Self {
        Self {
            client: self.client.clone(),
            session: self.session.clone(),
            server: self.server.clone(),
            ..Self::default()
        }
    }

    fn clear_authentication(&mut self) {
        // The session is over, so its saved token goes with it.
        if let Some(saved) = self.saved_sign_in.take() {
            saved.forget();
        }
        // A running transfer keeps the bearer header it started with. It stops with the
        // session; the manager and its saved jobs are kept for the next sign-in.
        let downloads = self.downloads.take();
        if let Some(manager) = downloads.as_ref() {
            downloads::pause_running(manager);
        }
        *self = Self::default();
        self.downloads = downloads;
    }
}

fn run_worker(
    commands: Arc<CommandQueue>,
    events: WorkerSink,
    device_id: String,
) -> WorkerShutdownReport {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(2)
        .build()
    {
        Ok(runtime) => runtime,
        Err(_) => {
            events.ctx.request_repaint();
            let _ = events.sender.blocking_send(WorkerEvent::ConnectionFailed {
                request_id: None,
                message: "Could not start the network worker.".into(),
            });
            return WorkerShutdownReport {
                worker_panicked: true,
                ..WorkerShutdownReport::default()
            };
        }
    };
    let report = runtime.block_on(scheduler::run(commands, events, device_id));
    // Image decoders own no session/token. Their allocation and concurrency are bounded.
    runtime.shutdown_timeout(std::time::Duration::from_millis(100));
    report
}

async fn execute_command(
    command: WorkerCommand,
    state: &mut WorkerState,
    events: &WorkerSink,
    device_id: &str,
) {
    let WorkerState {
        mut client,
        mut session,
        mut server,
        mut playback_info_cache,
        mut quick_connect,
        mut downloads,
        mut session_socket,
        mut clock_sampler,
        mut saved_sign_in,
    } = std::mem::take(state);
    let ctx = &events.ctx;
    async {
        match command {
            WorkerCommand::CheckServer {
                request_id,
                server_url,
                allow_insecure_remote_http,
            } => {
                let result = match build_client(&server_url, allow_insecure_remote_http, device_id)
                {
                    Ok(client) => client
                        .public_system_info()
                        .await
                        .map(|info| server_identity(&info))
                        .map_err(|error| sign_in_error_message(&error, false)),
                    Err(error) => Err(error),
                };
                match result {
                    Ok(server) => {
                        send_event(
                            events,
                            ctx,
                            WorkerEvent::ServerChecked { request_id, server },
                        )
                        .await
                    }
                    Err(message) => {
                        send_event(
                            events,
                            ctx,
                            WorkerEvent::ServerCheckFailed {
                                request_id,
                                message,
                            },
                        )
                        .await
                    }
                }
            }
            WorkerCommand::Connect {
                request_id,
                server_url,
                username,
                mut password,
                allow_insecure_remote_http,
                saved,
            } => {
                session = None;
                client = None;
                server = None;
                playback_info_cache = None;
                quick_connect = None;
                // The old session's saved token stays in the OS store; only this worker's
                // claim on it ends. It is replaced below if the same server is remembered.
                saved_sign_in = None;
                send_event(events, ctx, WorkerEvent::CheckingServer { request_id }).await;

                let new_client =
                    match build_client(&server_url, allow_insecure_remote_http, device_id) {
                        Ok(client) => client,
                        Err(message) => {
                            password.zeroize();
                            send_event(
                                events,
                                ctx,
                                WorkerEvent::ConnectionFailed {
                                    request_id: Some(request_id),
                                    message,
                                },
                            )
                            .await;
                            return;
                        }
                    };

                let public_info = match (new_client.public_system_info()).await {
                    Ok(info) => info,
                    Err(error) => {
                        password.zeroize();
                        send_event(
                            events,
                            ctx,
                            WorkerEvent::ConnectionFailed {
                                request_id: Some(request_id),
                                message: sign_in_error_message(&error, false),
                            },
                        )
                        .await;
                        return;
                    }
                };
                let identity = server_identity(&public_info);
                send_event(
                    events,
                    ctx,
                    WorkerEvent::Authenticating {
                        request_id,
                        server: identity.clone(),
                    },
                )
                .await;

                let auth_result = (new_client.authenticate(&username, password.as_str())).await;
                password.zeroize();

                let new_session = match auth_result {
                    Ok(session) => session,
                    Err(error) => {
                        send_event(
                            events,
                            ctx,
                            WorkerEvent::ConnectionFailed {
                                request_id: Some(request_id),
                                message: sign_in_error_message(&error, true),
                            },
                        )
                        .await;
                        return;
                    }
                };

                let identity = server_identity(&public_info);
                let user = new_session.user.clone();
                saved_sign_in = saved_sign_in::apply_after_sign_in(
                    &events.saved_sign_in,
                    &saved,
                    &new_client,
                    identity.id.as_deref(),
                    new_session.token_for_credential_store().as_str(),
                );
                client = Some(new_client);
                session = Some(new_session);
                session_socket = session_socket::start(client.as_ref(), session.as_ref(), events);
                clock_sampler = clock_sampler::start(client.as_ref(), session.as_ref(), events);
                server = Some(identity.clone());
                send_event(
                    events,
                    ctx,
                    WorkerEvent::CatalogLoading {
                        request_id,
                        server: identity.clone(),
                        user: user.clone(),
                    },
                )
                .await;

                let page_result = match (client.as_ref(), session.as_ref()) {
                    (Some(client), Some(session)) => {
                        (client.list_items(
                            session,
                            &item_query(
                                0,
                                None,
                                None,
                                None,
                                ItemSortBy::SortName,
                                ItemSortOrder::Ascending,
                                None,
                            ),
                        ))
                        .await
                    }
                    _ => Err(ApiError::InvalidConfiguration),
                };
                handle_items_result(
                    page_result,
                    request_id,
                    identity,
                    user,
                    events,
                    ctx,
                    &mut client,
                    &mut session,
                    &mut server,
                    &mut playback_info_cache,
                )
                .await;
            }
            WorkerCommand::ProbeSavedSignIn => {
                let available = events.saved_sign_in.is_available();
                send_event(
                    events,
                    ctx,
                    WorkerEvent::SavedSignInAvailability { available },
                )
                .await;
            }
            WorkerCommand::ResumeSignIn {
                request_id,
                server_url,
                allow_insecure_remote_http,
                saved,
            } => {
                session = None;
                client = None;
                server = None;
                playback_info_cache = None;
                quick_connect = None;
                saved_sign_in = None;
                send_event(events, ctx, WorkerEvent::CheckingServer { request_id }).await;

                let new_client =
                    match build_client(&server_url, allow_insecure_remote_http, device_id) {
                        Ok(client) => client,
                        Err(message) => {
                            send_event(
                                events,
                                ctx,
                                WorkerEvent::ConnectionFailed {
                                    request_id: Some(request_id),
                                    message,
                                },
                            )
                            .await;
                            return;
                        }
                    };
                let public_info = match (new_client.public_system_info()).await {
                    Ok(info) => info,
                    Err(error) => {
                        send_event(
                            events,
                            ctx,
                            WorkerEvent::ConnectionFailed {
                                request_id: Some(request_id),
                                message: sign_in_error_message(&error, false),
                            },
                        )
                        .await;
                        return;
                    }
                };
                let identity = server_identity(&public_info);
                let key = identity
                    .id
                    .as_deref()
                    .and_then(|id| saved_sign_in::credential_key(&saved.scope, id, &new_client));
                let Some(token) = key.as_ref().and_then(|key| events.saved_sign_in.load(key))
                else {
                    send_event(
                        events,
                        ctx,
                        WorkerEvent::ResumeUnavailable {
                            request_id,
                            expired: false,
                        },
                    )
                    .await;
                    return;
                };
                send_event(
                    events,
                    ctx,
                    WorkerEvent::Authenticating {
                        request_id,
                        server: identity.clone(),
                    },
                )
                .await;
                let resumed = (new_client.resume_session(token.as_str())).await;
                drop(token);
                let new_session = match resumed {
                    Ok(session) => session,
                    Err(ApiError::ReauthenticationRequired) => {
                        // The server no longer accepts the token: delete it and ask again.
                        if let Some(key) = key.as_ref() {
                            events.saved_sign_in.forget(key);
                        }
                        send_event(
                            events,
                            ctx,
                            WorkerEvent::ResumeUnavailable {
                                request_id,
                                expired: true,
                            },
                        )
                        .await;
                        return;
                    }
                    Err(error) => {
                        // Offline or a server error: keep the saved token for the next start.
                        send_event(
                            events,
                            ctx,
                            WorkerEvent::ConnectionFailed {
                                request_id: Some(request_id),
                                message: sign_in_error_message(&error, false),
                            },
                        )
                        .await;
                        return;
                    }
                };

                let user = new_session.user.clone();
                saved_sign_in = key.map(|key| SavedEntry::new(events.saved_sign_in.clone(), key));
                client = Some(new_client);
                session = Some(new_session);
                session_socket = session_socket::start(client.as_ref(), session.as_ref(), events);
                clock_sampler = clock_sampler::start(client.as_ref(), session.as_ref(), events);
                server = Some(identity.clone());
                send_event(
                    events,
                    ctx,
                    WorkerEvent::CatalogLoading {
                        request_id,
                        server: identity.clone(),
                        user: user.clone(),
                    },
                )
                .await;

                let page_result = match (client.as_ref(), session.as_ref()) {
                    (Some(client), Some(session)) => {
                        (client.list_items(
                            session,
                            &item_query(
                                0,
                                None,
                                None,
                                None,
                                ItemSortBy::SortName,
                                ItemSortOrder::Ascending,
                                None,
                            ),
                        ))
                        .await
                    }
                    _ => Err(ApiError::InvalidConfiguration),
                };
                handle_items_result(
                    page_result,
                    request_id,
                    identity,
                    user,
                    events,
                    ctx,
                    &mut client,
                    &mut session,
                    &mut server,
                    &mut playback_info_cache,
                )
                .await;
            }
            WorkerCommand::FetchItems { request_id, query } => {
                let Some(current_server) = server.clone() else {
                    send_event(
                        events,
                        ctx,
                        WorkerEvent::ConnectionFailed {
                            request_id: Some(request_id),
                            message: "Connect to a server before loading its library.".into(),
                        },
                    )
                    .await;
                    return;
                };
                let Some(current_user) = session.as_ref().map(|session| session.user.clone())
                else {
                    send_event(
                        events,
                        ctx,
                        WorkerEvent::ConnectionFailed {
                            request_id: Some(request_id),
                            message: "Connect to a server before loading its library.".into(),
                        },
                    )
                    .await;
                    return;
                };
                send_event(
                    events,
                    ctx,
                    WorkerEvent::CatalogLoading {
                        request_id,
                        server: current_server.clone(),
                        user: current_user.clone(),
                    },
                )
                .await;

                let page_result = match (client.as_ref(), session.as_ref()) {
                    (Some(client), Some(session)) => (client.list_items(session, &query)).await,
                    _ => Err(ApiError::InvalidConfiguration),
                };
                handle_items_result(
                    page_result,
                    request_id,
                    current_server,
                    current_user,
                    events,
                    ctx,
                    &mut client,
                    &mut session,
                    &mut server,
                    &mut playback_info_cache,
                )
                .await;
            }
            WorkerCommand::RestoreQueueItems {
                request_id,
                item_ids,
            } => {
                let result = match (client.as_ref(), session.as_ref()) {
                    (Some(client), Some(session)) => {
                        client.list_items_by_ids(session, &item_ids).await
                    }
                    _ => Err(ApiError::InvalidConfiguration),
                };
                match result {
                    Ok(items) => {
                        send_event(
                            events,
                            ctx,
                            WorkerEvent::QueueItemsRestored { request_id, items },
                        )
                        .await
                    }
                    Err(ApiError::ReauthenticationRequired) => {
                        session = None;
                        client = None;
                        server = None;
                        playback_info_cache = None;
                        send_event(
                            events,
                            ctx,
                            WorkerEvent::ReauthenticationRequired {
                                request_id,
                                message: "The server requires you to sign in again.".into(),
                            },
                        )
                        .await;
                    }
                    Err(error) => {
                        send_event(
                            events,
                            ctx,
                            WorkerEvent::QueueItemsRestoreFailed {
                                request_id,
                                error_class: error.classify(),
                            },
                        )
                        .await
                    }
                }
            }
            WorkerCommand::GetItem {
                request_id,
                item_id,
            } => {
                let result = match (client.as_ref(), session.as_ref()) {
                    (Some(client), Some(session)) => (client.get_item(session, &item_id)).await,
                    _ => Err(ApiError::InvalidConfiguration),
                };
                match result {
                    Ok(item) => {
                        send_event(
                            events,
                            ctx,
                            WorkerEvent::ItemDetailsLoaded {
                                request_id,
                                item: Box::new(item),
                            },
                        )
                        .await
                    }
                    Err(ApiError::ReauthenticationRequired) => {
                        session = None;
                        client = None;
                        server = None;
                        playback_info_cache = None;
                        send_event(
                            events,
                            ctx,
                            WorkerEvent::ReauthenticationRequired {
                                request_id,
                                message: "The server requires you to sign in again.".into(),
                            },
                        )
                        .await;
                    }
                    Err(error) => {
                        send_event(
                            events,
                            ctx,
                            WorkerEvent::ItemDetailsFailed {
                                request_id,
                                item_id,
                                message: error.to_string(),
                                error_class: error.classify(),
                                retry_after: error.retry_after(),
                            },
                        )
                        .await
                    }
                }
            }
            WorkerCommand::SetPlayed {
                request_id,
                item_id,
                played,
            } => {
                let result = match (client.as_ref(), session.as_ref()) {
                    (Some(client), Some(session)) => {
                        (client.set_played(session, &item_id, played)).await
                    }
                    _ => Err(ApiError::InvalidConfiguration),
                };
                match result {
                    Ok(user_data) => {
                        let played = user_data.played.unwrap_or(played);
                        send_event(
                            events,
                            ctx,
                            WorkerEvent::WatchedStateUpdated {
                                request_id,
                                item_id,
                                played,
                                user_data: Box::new(user_data),
                            },
                        )
                        .await;
                    }
                    Err(ApiError::ReauthenticationRequired) => {
                        session = None;
                        client = None;
                        server = None;
                        playback_info_cache = None;
                        send_event(
                            events,
                            ctx,
                            WorkerEvent::ReauthenticationRequired {
                                request_id,
                                message: "The server requires you to sign in again.".into(),
                            },
                        )
                        .await;
                    }
                    Err(error) => {
                        send_event(
                            events,
                            ctx,
                            WorkerEvent::WatchedStateFailed {
                                request_id,
                                item_id,
                                message: error.to_string(),
                            },
                        )
                        .await
                    }
                }
            }
            WorkerCommand::SetFavorite {
                request_id,
                item_id,
                favorite,
            } => {
                let result = match (client.as_ref(), session.as_ref()) {
                    (Some(client), Some(session)) => {
                        client.set_favorite(session, &item_id, favorite).await
                    }
                    _ => Err(ApiError::InvalidConfiguration),
                };
                match result {
                    Ok(user_data) => {
                        let favorite = user_data.is_favorite.unwrap_or(favorite);
                        send_event(
                            events,
                            ctx,
                            WorkerEvent::FavoriteStateUpdated {
                                request_id,
                                item_id,
                                favorite,
                                user_data: Box::new(user_data),
                            },
                        )
                        .await;
                    }
                    Err(ApiError::ReauthenticationRequired) => {
                        session = None;
                        client = None;
                        server = None;
                        playback_info_cache = None;
                        send_event(
                            events,
                            ctx,
                            WorkerEvent::ReauthenticationRequired {
                                request_id,
                                message: "The server requires you to sign in again.".into(),
                            },
                        )
                        .await;
                    }
                    Err(error) => {
                        send_event(
                            events,
                            ctx,
                            WorkerEvent::FavoriteStateFailed {
                                request_id,
                                item_id,
                                message: error.to_string(),
                            },
                        )
                        .await;
                    }
                }
            }
            WorkerCommand::LoadHomeRow { request_id, row } => {
                let result = match (client.as_ref(), session.as_ref()) {
                    (Some(client), Some(session)) => (load_home_row(client, session, row)).await,
                    _ => Err(ApiError::InvalidConfiguration),
                };
                match result {
                    Ok(items) => {
                        send_event(
                            events,
                            ctx,
                            WorkerEvent::HomeRowLoaded {
                                request_id,
                                row,
                                items,
                            },
                        )
                        .await
                    }
                    Err(ApiError::ReauthenticationRequired) => {
                        session = None;
                        client = None;
                        server = None;
                        playback_info_cache = None;
                        send_event(
                            events,
                            ctx,
                            WorkerEvent::ReauthenticationRequired {
                                request_id,
                                message: "The server requires you to sign in again.".into(),
                            },
                        )
                        .await;
                    }
                    Err(error) => {
                        send_event(
                            events,
                            ctx,
                            WorkerEvent::HomeRowFailed {
                                request_id,
                                row,
                                error_class: error.classify(),
                                retry_after: error.retry_after(),
                            },
                        )
                        .await
                    }
                }
            }
            WorkerCommand::LoadHomeCatalog { request_id, kind } => {
                let result = match (client.as_ref(), session.as_ref()) {
                    (Some(client), Some(session)) => {
                        (load_home_catalog(client, session, kind)).await
                    }
                    _ => Err(ApiError::InvalidConfiguration),
                };
                match result {
                    Ok(data) => {
                        send_event(
                            events,
                            ctx,
                            WorkerEvent::HomeCatalogLoaded { request_id, data },
                        )
                        .await
                    }
                    Err(ApiError::ReauthenticationRequired) => {
                        session = None;
                        client = None;
                        server = None;
                        playback_info_cache = None;
                        send_event(
                            events,
                            ctx,
                            WorkerEvent::ReauthenticationRequired {
                                request_id,
                                message: "The server requires you to sign in again.".into(),
                            },
                        )
                        .await;
                    }
                    Err(error) => {
                        send_event(
                            events,
                            ctx,
                            WorkerEvent::HomeCatalogFailed {
                                request_id,
                                kind,
                                error_class: error.classify(),
                                retry_after: error.retry_after(),
                            },
                        )
                        .await
                    }
                }
            }
            WorkerCommand::LoadLiveTvChannels {
                request_id,
                start_index,
                limit,
            } => {
                let result = match (client.as_ref(), session.as_ref()) {
                    (Some(client), Some(session)) => {
                        client
                            .list_live_tv_channels(session, start_index, limit)
                            .await
                    }
                    _ => Err(ApiError::InvalidConfiguration),
                };
                match result {
                    Ok(page) => {
                        send_event(
                            events,
                            ctx,
                            WorkerEvent::LiveTvChannelsLoaded { request_id, page },
                        )
                        .await
                    }
                    Err(ApiError::ReauthenticationRequired) => {
                        session = None;
                        client = None;
                        server = None;
                        playback_info_cache = None;
                        send_event(
                            events,
                            ctx,
                            WorkerEvent::ReauthenticationRequired {
                                request_id,
                                message: "The server requires you to sign in again.".into(),
                            },
                        )
                        .await;
                    }
                    Err(error) => {
                        send_event(
                            events,
                            ctx,
                            WorkerEvent::LiveTvChannelsFailed {
                                request_id,
                                error_class: error.classify(),
                            },
                        )
                        .await
                    }
                }
            }
            WorkerCommand::SearchLiveTvChannels {
                request_id,
                term,
                start_index,
                limit,
            } => {
                let result = match (client.as_ref(), session.as_ref()) {
                    (Some(client), Some(session)) => {
                        client
                            .search_live_tv_channels(session, &term, start_index, limit)
                            .await
                    }
                    _ => Err(ApiError::InvalidConfiguration.into()),
                };
                let event = match result {
                    Ok(page) => WorkerEvent::LiveTvSearchResultsLoaded {
                        request_id,
                        items: page.items.into_iter().map(|channel| channel.item).collect(),
                        start_index: page.start_index,
                        total_record_count: page.total_record_count,
                    },
                    Err(_) => WorkerEvent::LiveTvSearchResultsFailed { request_id },
                };
                send_event(events, ctx, event).await;
            }
            WorkerCommand::LoadLiveTvPrograms {
                request_id,
                window,
                channel_ids,
            } => {
                let result = match (client.as_ref(), session.as_ref()) {
                    (Some(client), Some(session)) => {
                        client
                            .list_live_tv_programs_for_window(session, window, &channel_ids)
                            .await
                    }
                    _ => Err(ApiError::InvalidConfiguration),
                };
                match result {
                    Ok(page) => {
                        send_event(
                            events,
                            ctx,
                            WorkerEvent::LiveTvProgramsLoaded { request_id, page },
                        )
                        .await
                    }
                    Err(ApiError::ReauthenticationRequired) => {
                        session = None;
                        client = None;
                        server = None;
                        playback_info_cache = None;
                        send_event(
                            events,
                            ctx,
                            WorkerEvent::ReauthenticationRequired {
                                request_id,
                                message: "The server requires you to sign in again.".into(),
                            },
                        )
                        .await;
                    }
                    Err(error) => {
                        send_event(
                            events,
                            ctx,
                            WorkerEvent::LiveTvProgramsFailed {
                                request_id,
                                error_class: error.classify(),
                            },
                        )
                        .await
                    }
                }
            }
            WorkerCommand::ListSeasons {
                request_id,
                series_id,
            } => {
                let result = match (client.as_ref(), session.as_ref()) {
                    (Some(client), Some(session)) => {
                        (client.list_seasons(session, &series_id)).await
                    }
                    _ => Err(ApiError::InvalidConfiguration),
                };
                match result {
                    Ok(response) => {
                        send_event(
                            events,
                            ctx,
                            WorkerEvent::SeriesSeasonsLoaded {
                                request_id,
                                series_id,
                                seasons: response.items,
                            },
                        )
                        .await
                    }
                    Err(ApiError::ReauthenticationRequired) => {
                        session = None;
                        client = None;
                        server = None;
                        playback_info_cache = None;
                        send_event(
                            events,
                            ctx,
                            WorkerEvent::ReauthenticationRequired {
                                request_id,
                                message: "The server requires you to sign in again.".into(),
                            },
                        )
                        .await;
                    }
                    Err(error) => {
                        send_event(
                            events,
                            ctx,
                            WorkerEvent::SeriesSeasonsFailed {
                                request_id,
                                series_id,
                                message: error.to_string(),
                            },
                        )
                        .await
                    }
                }
            }
            WorkerCommand::RemoteSessionMessage {
                message,
                max_volume_percent,
            } => {
                if let Some(event) = remote_action_event(&message, max_volume_percent) {
                    send_event(events, ctx, event).await;
                }
            }
            WorkerCommand::SyncPlayJoin {
                request_id,
                group_id,
            } => {
                let result = match (client.as_ref(), session.as_ref()) {
                    (Some(client), Some(session)) => {
                        client.join_sync_play_group(session, &group_id).await
                    }
                    _ => Err(ApiError::InvalidConfiguration),
                };
                match result {
                    Ok(()) => {
                        if let Some(sampler) = clock_sampler.as_ref() {
                            sampler.set_joined(true);
                        }
                        send_event(
                            events,
                            ctx,
                            WorkerEvent::SyncPlayJoined {
                                request_id,
                                group_id,
                            },
                        )
                        .await
                    }
                    Err(ApiError::ReauthenticationRequired) => {
                        session = None;
                        client = None;
                        server = None;
                        playback_info_cache = None;
                        send_event(
                            events,
                            ctx,
                            WorkerEvent::ReauthenticationRequired {
                                request_id,
                                message: "The server requires you to sign in again.".into(),
                            },
                        )
                        .await;
                    }
                    Err(_) => {
                        send_event(events, ctx, WorkerEvent::SyncPlayJoinFailed { request_id })
                            .await
                    }
                }
            }
            WorkerCommand::SyncPlayList { request_id } => {
                let result = match (client.as_ref(), session.as_ref()) {
                    (Some(client), Some(session)) => client.list_sync_play_groups(session).await,
                    _ => Err(ApiError::InvalidConfiguration),
                };
                match result {
                    Ok(groups) => {
                        send_event(
                            events,
                            ctx,
                            WorkerEvent::SyncPlayGroupsLoaded { request_id, groups },
                        )
                        .await;
                    }
                    Err(ApiError::ReauthenticationRequired) => {
                        session = None;
                        client = None;
                        server = None;
                        playback_info_cache = None;
                        send_event(
                            events,
                            ctx,
                            WorkerEvent::ReauthenticationRequired {
                                request_id,
                                message: "The server requires you to sign in again.".into(),
                            },
                        )
                        .await;
                    }
                    Err(_) => {
                        send_event(
                            events,
                            ctx,
                            WorkerEvent::SyncPlayGroupsFailed { request_id },
                        )
                        .await;
                    }
                }
            }
            WorkerCommand::SyncPlayCreate {
                request_id,
                group_name,
            } => {
                let result = match (client.as_ref(), session.as_ref()) {
                    (Some(client), Some(session)) => {
                        client.create_sync_play_group(session, &group_name).await
                    }
                    _ => Err(ApiError::InvalidConfiguration),
                };
                match result {
                    Ok(group) => {
                        send_event(
                            events,
                            ctx,
                            WorkerEvent::SyncPlayGroupCreated { request_id, group },
                        )
                        .await;
                    }
                    Err(ApiError::ReauthenticationRequired) => {
                        session = None;
                        client = None;
                        server = None;
                        playback_info_cache = None;
                        send_event(
                            events,
                            ctx,
                            WorkerEvent::ReauthenticationRequired {
                                request_id,
                                message: "The server requires you to sign in again.".into(),
                            },
                        )
                        .await;
                    }
                    Err(_) => {
                        send_event(
                            events,
                            ctx,
                            WorkerEvent::SyncPlayGroupCreateFailed { request_id },
                        )
                        .await;
                    }
                }
            }
            WorkerCommand::SyncPlayLeave { request_id } => {
                let result = match (client.as_ref(), session.as_ref()) {
                    (Some(client), Some(session)) => client.leave_sync_play_group(session).await,
                    _ => Err(ApiError::InvalidConfiguration),
                };
                match result {
                    Ok(()) => {
                        if let Some(sampler) = clock_sampler.as_ref() {
                            sampler.set_joined(false);
                        }
                        send_event(events, ctx, WorkerEvent::SyncPlayLeft { request_id }).await
                    }
                    Err(ApiError::ReauthenticationRequired) => {
                        session = None;
                        client = None;
                        server = None;
                        playback_info_cache = None;
                        send_event(
                            events,
                            ctx,
                            WorkerEvent::ReauthenticationRequired {
                                request_id,
                                message: "The server requires you to sign in again.".into(),
                            },
                        )
                        .await;
                    }
                    Err(_) => {
                        send_event(events, ctx, WorkerEvent::SyncPlayLeaveFailed { request_id })
                            .await
                    }
                }
            }
            WorkerCommand::SyncPlaySend {
                request_id,
                command,
            } => {
                let result = match (client.as_ref(), session.as_ref()) {
                    (Some(client), Some(session)) => match command {
                        SyncPlayCommand::Pause => client.sync_play_pause(session).await,
                        SyncPlayCommand::Unpause => client.sync_play_unpause(session).await,
                        SyncPlayCommand::Seek { position_ticks } => {
                            client.sync_play_seek(session, position_ticks).await
                        }
                    },
                    _ => Err(ApiError::InvalidConfiguration),
                };
                match result {
                    Ok(()) => {}
                    Err(ApiError::ReauthenticationRequired) => {
                        session = None;
                        client = None;
                        server = None;
                        playback_info_cache = None;
                        send_event(
                            events,
                            ctx,
                            WorkerEvent::ReauthenticationRequired {
                                request_id,
                                message: "The server requires you to sign in again.".into(),
                            },
                        )
                        .await;
                    }
                    Err(_) => {
                        tracing::warn!("SyncPlay command was not accepted by the server");
                    }
                }
            }
            WorkerCommand::ListMediaSegments {
                request_id,
                item_id,
            } => {
                // Only intro and outro segments are used. The filter keeps the response small.
                let result = match (client.as_ref(), session.as_ref()) {
                    (Some(client), Some(session)) => {
                        client
                            .get_media_segments(
                                session,
                                &item_id,
                                &[MediaSegmentType::Intro, MediaSegmentType::Outro],
                            )
                            .await
                    }
                    _ => Err(ApiError::InvalidConfiguration),
                };
                match result {
                    Ok(response) => {
                        let segments = response
                            .items
                            .into_iter()
                            .filter_map(|segment| {
                                kattri_client_core::segments::MediaSegment::from_server(
                                    &segment.id,
                                    segment.segment_type.as_api_value(),
                                    segment.start_ticks,
                                    segment.end_ticks,
                                )
                            })
                            .collect();
                        send_event(
                            events,
                            ctx,
                            WorkerEvent::MediaSegmentsLoaded {
                                request_id,
                                item_id,
                                segments,
                            },
                        )
                        .await
                    }
                    Err(ApiError::ReauthenticationRequired) => {
                        session = None;
                        client = None;
                        server = None;
                        playback_info_cache = None;
                        send_event(
                            events,
                            ctx,
                            WorkerEvent::ReauthenticationRequired {
                                request_id,
                                message: "The server requires you to sign in again.".into(),
                            },
                        )
                        .await;
                    }
                    Err(_) => {
                        send_event(events, ctx, WorkerEvent::MediaSegmentsFailed { request_id })
                            .await
                    }
                }
            }
            WorkerCommand::ListEpisodes {
                request_id,
                series_id,
                season_id,
            } => {
                let result = match (client.as_ref(), session.as_ref()) {
                    (Some(client), Some(session)) => {
                        (client.list_episodes(session, &series_id, &season_id)).await
                    }
                    _ => Err(ApiError::InvalidConfiguration),
                };
                match result {
                    Ok(response) => {
                        send_event(
                            events,
                            ctx,
                            WorkerEvent::SeriesEpisodesLoaded {
                                request_id,
                                season_id,
                                episodes: response.items,
                            },
                        )
                        .await
                    }
                    Err(ApiError::ReauthenticationRequired) => {
                        session = None;
                        client = None;
                        server = None;
                        playback_info_cache = None;
                        send_event(
                            events,
                            ctx,
                            WorkerEvent::ReauthenticationRequired {
                                request_id,
                                message: "The server requires you to sign in again.".into(),
                            },
                        )
                        .await;
                    }
                    Err(error) => {
                        send_event(
                            events,
                            ctx,
                            WorkerEvent::SeriesEpisodesFailed {
                                request_id,
                                season_id,
                                message: error.to_string(),
                            },
                        )
                        .await
                    }
                }
            }
            WorkerCommand::GetAdjacentEpisodes {
                request_id,
                series_id,
                episode_id,
            } => {
                let adjacent = match (client.as_ref(), session.as_ref()) {
                    (Some(client), Some(session)) => client
                        .adjacent_episodes(session, &series_id, &episode_id)
                        .await
                        .unwrap_or_else(|error| {
                            tracing::debug!(%error, "adjacent episodes unavailable");
                            jellyfin_api::AdjacentEpisodes::default()
                        }),
                    _ => jellyfin_api::AdjacentEpisodes::default(),
                };
                send_event(
                    events,
                    ctx,
                    WorkerEvent::AdjacentEpisodesLoaded {
                        request_id,
                        episode_id,
                        previous: adjacent.previous.map(Box::new),
                        next: adjacent.next.map(Box::new),
                    },
                )
                .await
            }
            WorkerCommand::ItemAdmin {
                request_id,
                item_id,
                action,
                collection_id,
            } => {
                let result = match (client.as_ref(), session.as_ref()) {
                    (Some(client), Some(session)) => match action {
                        ItemAdminAction::RefreshMetadata => {
                            client.refresh_item_metadata(session, &item_id).await
                        }
                        ItemAdminAction::Delete => client.delete_item(session, &item_id).await,
                        ItemAdminAction::AddToCollection => match collection_id.as_deref() {
                            Some(collection_id) => {
                                client
                                    .add_to_collection(session, collection_id, &item_id)
                                    .await
                            }
                            None => Err(ApiError::InvalidConfiguration),
                        },
                    },
                    _ => Err(ApiError::InvalidConfiguration),
                };
                send_event(
                    events,
                    ctx,
                    WorkerEvent::ItemAdminFinished {
                        request_id,
                        item_id,
                        action,
                        result: result.map_err(|error| error.classify()),
                    },
                )
                .await
            }
            WorkerCommand::Metadata {
                request_id,
                item_id,
                op,
            } => {
                let result = match (client.as_ref(), session.as_ref()) {
                    (Some(client), Some(session)) => {
                        run_metadata_op(client, session, &item_id, op).await
                    }
                    _ => Err(ApiError::InvalidConfiguration),
                };
                if matches!(result, Err(ApiError::ReauthenticationRequired)) {
                    send_event(
                        events,
                        ctx,
                        WorkerEvent::ReauthenticationRequired {
                            request_id,
                            message: "The server requires you to sign in again.".into(),
                        },
                    )
                    .await;
                }
                send_event(
                    events,
                    ctx,
                    WorkerEvent::MetadataFinished {
                        request_id,
                        result: result.map_err(|error| error.classify()),
                    },
                )
                .await
            }
            WorkerCommand::MetadataThumbnail { request_id, source } => {
                let bytes = match (client.as_ref(), session.as_ref(), &source) {
                    (Some(client), Some(session), ThumbnailSource::Item { item_id, image }) => {
                        client.get_item_image(session, item_id, image).await.ok()
                    }
                    (Some(client), _, ThumbnailSource::Remote(url)) => {
                        client.get_remote_thumbnail(url).await.ok()
                    }
                    _ => None,
                };
                let image = match bytes {
                    Some(bytes) => decode_metadata_thumbnail(bytes, events).await.ok(),
                    None => None,
                };
                send_event(
                    events,
                    ctx,
                    WorkerEvent::MetadataThumbnailLoaded { request_id, image },
                )
                .await
            }
            WorkerCommand::ListGenres {
                request_id,
                parent_id,
                item_types,
            } => {
                let (genres, studios) = match (client.as_ref(), session.as_ref()) {
                    (Some(client), Some(session)) => {
                        let genres = client
                            .list_genres(session, parent_id.as_deref(), item_types.as_deref())
                            .await
                            .unwrap_or_else(|error| {
                                tracing::debug!(%error, "genres unavailable");
                                Vec::new()
                            });
                        let studios = client
                            .list_studios(session, parent_id.as_deref(), item_types.as_deref())
                            .await
                            .unwrap_or_else(|error| {
                                tracing::debug!(%error, "studios unavailable");
                                Vec::new()
                            });
                        (genres, studios)
                    }
                    _ => (Vec::new(), Vec::new()),
                };
                send_event(
                    events,
                    ctx,
                    WorkerEvent::GenresLoaded {
                        request_id,
                        genres,
                        studios,
                    },
                )
                .await
            }
            WorkerCommand::GetTrickplayResolutions {
                request_id,
                item_id,
                media_source_id,
            } => {
                let resolutions = match (client.as_ref(), session.as_ref()) {
                    (Some(client), Some(session)) => client
                        .trickplay_resolutions(session, &item_id, media_source_id.as_deref())
                        .await
                        .unwrap_or_else(|error| {
                            tracing::debug!(%error, "trickplay unavailable");
                            Vec::new()
                        }),
                    _ => Vec::new(),
                };
                send_event(
                    events,
                    ctx,
                    WorkerEvent::TrickplayResolutionsLoaded {
                        request_id,
                        item_id,
                        resolutions,
                    },
                )
                .await
            }
            WorkerCommand::GetTrickplayTile {
                request_id,
                item_id,
                media_source_id,
                width,
                sheet,
            } => {
                let bytes = match (client.as_ref(), session.as_ref()) {
                    (Some(client), Some(session)) => client
                        .trickplay_tile(session, &item_id, media_source_id.as_deref(), width, sheet)
                        .await
                        .map_err(|error| tracing::debug!(%error, "trickplay sheet unavailable"))
                        .ok(),
                    _ => None,
                };
                let image = match bytes {
                    Some(bytes) => decode_trickplay_sheet(bytes, events).await.ok(),
                    None => None,
                };
                send_event(
                    events,
                    ctx,
                    WorkerEvent::TrickplayTileLoaded {
                        request_id,
                        item_id,
                        image,
                    },
                )
                .await
            }
            WorkerCommand::SearchRemoteSubtitles {
                request_id,
                item_id,
                language,
            } => {
                let result = match (client.as_ref(), session.as_ref()) {
                    (Some(client), Some(session)) => client
                        .search_remote_subtitles(session, &item_id, &language)
                        .await
                        .map_err(SubtitleRequestError::from_api),
                    _ => Err(SubtitleRequestError::Failed),
                };
                send_event(
                    events,
                    ctx,
                    WorkerEvent::RemoteSubtitlesLoaded { request_id, result },
                )
                .await
            }
            WorkerCommand::ListSubtitleCultures { request_id } => {
                let cultures = match (client.as_ref(), session.as_ref()) {
                    (Some(client), Some(session)) => {
                        client.cultures(session).await.unwrap_or_else(|error| {
                            tracing::debug!(%error, "cultures unavailable");
                            Vec::new()
                        })
                    }
                    _ => Vec::new(),
                };
                send_event(
                    events,
                    ctx,
                    WorkerEvent::SubtitleCulturesLoaded {
                        request_id,
                        cultures,
                    },
                )
                .await
            }
            WorkerCommand::DownloadRemoteSubtitle {
                request_id,
                item_id,
                subtitle_id,
                for_player,
            } => {
                let result = match (client.as_ref(), session.as_ref()) {
                    (Some(client), Some(session)) => {
                        download_remote_subtitle(
                            client,
                            session,
                            &item_id,
                            &subtitle_id,
                            for_player,
                        )
                        .await
                    }
                    _ => Err(SubtitleRequestError::Failed),
                };
                send_event(
                    events,
                    ctx,
                    WorkerEvent::RemoteSubtitleDownloaded {
                        request_id,
                        item_id,
                        result,
                    },
                )
                .await
            }
            WorkerCommand::GetLyrics {
                request_id,
                item_id,
            } => {
                let lines = match (client.as_ref(), session.as_ref()) {
                    (Some(client), Some(session)) => client
                        .track_lyrics(session, &item_id)
                        .await
                        .unwrap_or_else(|error| {
                            tracing::debug!(%error, "lyrics unavailable");
                            Vec::new()
                        }),
                    _ => Vec::new(),
                };
                send_event(events, ctx, WorkerEvent::LyricsLoaded { request_id, lines }).await
            }
            WorkerCommand::ListExtras {
                request_id,
                item_id,
            } => {
                let extras = match (client.as_ref(), session.as_ref()) {
                    (Some(client), Some(session)) => client
                        .item_extras(session, &item_id)
                        .await
                        .unwrap_or_else(|error| {
                            tracing::debug!(%error, "extras unavailable");
                            Vec::new()
                        }),
                    _ => Vec::new(),
                };
                send_event(
                    events,
                    ctx,
                    WorkerEvent::ExtrasLoaded { request_id, extras },
                )
                .await
            }
            WorkerCommand::ListSeasonSiblings {
                request_id,
                series_id,
                season,
            } => {
                let episodes = match (client.as_ref(), session.as_ref()) {
                    (Some(client), Some(session)) => client
                        .season_episodes_by_number(session, &series_id, season)
                        .await
                        .map(|response| response.items)
                        .unwrap_or_else(|error| {
                            tracing::debug!(%error, "season episodes unavailable");
                            Vec::new()
                        }),
                    _ => Vec::new(),
                };
                send_event(
                    events,
                    ctx,
                    WorkerEvent::SeasonSiblingsLoaded {
                        request_id,
                        episodes,
                    },
                )
                .await
            }
            WorkerCommand::LoadRecordings { request_id } => {
                let result = match (client.as_ref(), session.as_ref()) {
                    (Some(client), Some(session)) => {
                        match client.list_live_tv_recordings(session).await {
                            Ok(recordings) => client
                                .list_live_tv_timers(session)
                                .await
                                .map(|timers| (recordings, timers)),
                            Err(error) => Err(error),
                        }
                        .map_err(|error| error.classify())
                    }
                    _ => Err(ErrorClass::Unexpected),
                };
                send_event(
                    events,
                    ctx,
                    WorkerEvent::RecordingsLoaded { request_id, result },
                )
                .await
            }
            WorkerCommand::ChangeTimer { request_id, change } => {
                let result = match (client.as_ref(), session.as_ref()) {
                    (Some(client), Some(session)) => match &change {
                        TimerChange::Record { program_id } => {
                            client.record_live_tv_program(session, program_id).await
                        }
                        TimerChange::Cancel { timer_id } => {
                            client.cancel_live_tv_timer(session, timer_id).await
                        }
                    }
                    .map_err(|error| error.classify()),
                    _ => Err(ErrorClass::Unexpected),
                };
                send_event(
                    events,
                    ctx,
                    WorkerEvent::TimerChanged { request_id, result },
                )
                .await
            }
            WorkerCommand::ShuffleSeries {
                request_id,
                series_id,
            } => {
                let episodes = match (client.as_ref(), session.as_ref()) {
                    (Some(client), Some(session)) => client
                        .list_items(
                            session,
                            &ItemQuery {
                                parent_id: Some(series_id),
                                include_item_types: Some(vec!["Episode".to_owned()]),
                                sort_by: Some(vec![jellyfin_api::ItemSortBy::Random]),
                                limit: MAX_SHUFFLE_EPISODES,
                                ..ItemQuery::default()
                            },
                        )
                        .await
                        .map(|response| response.items)
                        .unwrap_or_else(|error| {
                            tracing::debug!(%error, "shuffle unavailable");
                            Vec::new()
                        }),
                    _ => Vec::new(),
                };
                send_event(
                    events,
                    ctx,
                    WorkerEvent::ShuffleLoaded {
                        request_id,
                        episodes,
                    },
                )
                .await
            }
            WorkerCommand::ListMenuCollections { request_id } => {
                let collections = match (client.as_ref(), session.as_ref()) {
                    (Some(client), Some(session)) => client
                        .box_sets(session, MAX_MENU_PLAYLISTS)
                        .await
                        .map(|items| items.into_iter().map(|item| (item.id, item.name)).collect())
                        .unwrap_or_else(|error| {
                            tracing::debug!(%error, "collections for the item menu unavailable");
                            Vec::new()
                        }),
                    _ => Vec::new(),
                };
                send_event(
                    events,
                    ctx,
                    WorkerEvent::MenuCollectionsLoaded {
                        request_id,
                        collections,
                    },
                )
                .await
            }
            WorkerCommand::ListUserPlaylists { request_id } => {
                let playlists = match (client.as_ref(), session.as_ref()) {
                    (Some(client), Some(session)) => client
                        .list_playlists(session, MAX_MENU_PLAYLISTS)
                        .await
                        .map(|response| {
                            response
                                .items
                                .into_iter()
                                .map(|item| (item.id, item.name))
                                .collect()
                        })
                        .unwrap_or_else(|error| {
                            tracing::debug!(%error, "playlists for the item menu unavailable");
                            Vec::new()
                        }),
                    _ => Vec::new(),
                };
                send_event(
                    events,
                    ctx,
                    WorkerEvent::UserPlaylistsLoaded {
                        request_id,
                        playlists,
                    },
                )
                .await
            }
            WorkerCommand::GetSeriesNextUp {
                request_id,
                series_id,
            } => {
                let episode = match (client.as_ref(), session.as_ref()) {
                    (Some(client), Some(session)) => client
                        .series_next_up(session, &series_id)
                        .await
                        .unwrap_or_else(|error| {
                            tracing::debug!(%error, "series next up unavailable");
                            None
                        }),
                    _ => None,
                };
                send_event(
                    events,
                    ctx,
                    WorkerEvent::SeriesNextUpLoaded {
                        request_id,
                        series_id,
                        episode: episode.map(Box::new),
                    },
                )
                .await
            }
            WorkerCommand::ListMusicAlbumTracks {
                request_id,
                album_id,
            } => {
                let result = match (client.as_ref(), session.as_ref()) {
                    (Some(client), Some(session)) => {
                        list_music_album_tracks(client, session, &album_id).await
                    }
                    _ => Err(ApiError::InvalidConfiguration),
                };
                match result {
                    Ok(tracks) => {
                        send_event(
                            events,
                            ctx,
                            WorkerEvent::MusicAlbumTracksLoaded {
                                request_id,
                                album_id,
                                tracks,
                            },
                        )
                        .await
                    }
                    Err(ApiError::ReauthenticationRequired) => {
                        session = None;
                        client = None;
                        server = None;
                        playback_info_cache = None;
                        send_event(
                            events,
                            ctx,
                            WorkerEvent::ReauthenticationRequired {
                                request_id,
                                message: "The server requires you to sign in again.".into(),
                            },
                        )
                        .await;
                    }
                    Err(error) => {
                        send_event(
                            events,
                            ctx,
                            WorkerEvent::MusicAlbumTracksFailed {
                                request_id,
                                album_id,
                                error_class: error.classify(),
                            },
                        )
                        .await
                    }
                }
            }
            WorkerCommand::ListPlaylistItems {
                request_id,
                playlist_id,
            } => {
                let result = match (client.as_ref(), session.as_ref()) {
                    (Some(client), Some(session)) => {
                        list_playlist_entries(client, session, &playlist_id).await
                    }
                    _ => Err(ApiError::InvalidConfiguration),
                };
                match result {
                    Ok(entries) => {
                        send_event(
                            events,
                            ctx,
                            WorkerEvent::PlaylistItemsLoaded {
                                request_id,
                                playlist_id,
                                entries,
                            },
                        )
                        .await
                    }
                    Err(ApiError::ReauthenticationRequired) => {
                        session = None;
                        client = None;
                        server = None;
                        playback_info_cache = None;
                        send_event(
                            events,
                            ctx,
                            WorkerEvent::ReauthenticationRequired {
                                request_id,
                                message: "The server requires you to sign in again.".into(),
                            },
                        )
                        .await;
                    }
                    Err(error) => {
                        send_event(
                            events,
                            ctx,
                            WorkerEvent::PlaylistItemsFailed {
                                request_id,
                                playlist_id,
                                error_class: error.classify(),
                            },
                        )
                        .await
                    }
                }
            }
            WorkerCommand::LoadPlaylistCandidates { request_id } => {
                let result = match (client.as_ref(), session.as_ref()) {
                    (Some(client), Some(session)) => {
                        client
                            .list_items(
                                session,
                                &ItemQuery {
                                    recursive: true,
                                    limit: 100,
                                    include_item_types: Some(
                                        ["Movie", "Episode", "Audio"]
                                            .into_iter()
                                            .map(str::to_owned)
                                            .collect(),
                                    ),
                                    ..ItemQuery::default()
                                },
                            )
                            .await
                    }
                    _ => Err(ApiError::InvalidConfiguration),
                };
                match result {
                    Ok(page) => {
                        send_event(
                            events,
                            ctx,
                            WorkerEvent::PlaylistCandidatesLoaded {
                                request_id,
                                items: page.items,
                            },
                        )
                        .await
                    }
                    Err(ApiError::ReauthenticationRequired) => {
                        session = None;
                        client = None;
                        server = None;
                        playback_info_cache = None;
                        send_event(
                            events,
                            ctx,
                            WorkerEvent::ReauthenticationRequired {
                                request_id,
                                message: "The server requires you to sign in again.".into(),
                            },
                        )
                        .await;
                    }
                    Err(error) => {
                        send_event(
                            events,
                            ctx,
                            WorkerEvent::PlaylistCandidatesFailed {
                                request_id,
                                error_class: error.classify(),
                            },
                        )
                        .await
                    }
                }
            }
            WorkerCommand::CreatePlaylist { request_id, name } => {
                let result = match (client.as_ref(), session.as_ref()) {
                    (Some(client), Some(session)) => {
                        client.create_playlist(session, &name, &[]).await
                    }
                    _ => Err(ApiError::InvalidConfiguration),
                };
                match result {
                    Ok(playlist_id) => {
                        send_event(
                            events,
                            ctx,
                            WorkerEvent::PlaylistCreated {
                                request_id,
                                playlist_id,
                                name,
                            },
                        )
                        .await
                    }
                    Err(ApiError::ReauthenticationRequired) => {
                        session = None;
                        client = None;
                        server = None;
                        playback_info_cache = None;
                        send_event(
                            events,
                            ctx,
                            WorkerEvent::ReauthenticationRequired {
                                request_id,
                                message: "The server requires you to sign in again.".into(),
                            },
                        )
                        .await;
                    }
                    Err(error) => {
                        send_event(
                            events,
                            ctx,
                            WorkerEvent::PlaylistCreateFailed {
                                request_id,
                                error_class: error.classify(),
                            },
                        )
                        .await
                    }
                }
            }
            WorkerCommand::AddPlaylistItems {
                request_id,
                playlist_id,
                item_ids,
            } => {
                let result = match (client.as_ref(), session.as_ref()) {
                    (Some(client), Some(session)) => {
                        client
                            .add_playlist_items(session, &playlist_id, &item_ids)
                            .await
                    }
                    _ => Err(ApiError::InvalidConfiguration),
                };
                match result {
                    Ok(()) => {
                        send_event(
                            events,
                            ctx,
                            WorkerEvent::PlaylistMutationCompleted {
                                request_id,
                                playlist_id,
                                kind: PlaylistMutationKind::Added,
                            },
                        )
                        .await
                    }
                    Err(ApiError::ReauthenticationRequired) => {
                        session = None;
                        client = None;
                        server = None;
                        playback_info_cache = None;
                        send_event(
                            events,
                            ctx,
                            WorkerEvent::ReauthenticationRequired {
                                request_id,
                                message: "The server requires you to sign in again.".into(),
                            },
                        )
                        .await;
                    }
                    Err(error) => {
                        send_event(
                            events,
                            ctx,
                            WorkerEvent::PlaylistMutationFailed {
                                request_id,
                                playlist_id,
                                kind: PlaylistMutationKind::Added,
                                error_class: error.classify(),
                            },
                        )
                        .await
                    }
                }
            }
            WorkerCommand::RemovePlaylistEntry {
                request_id,
                playlist_id,
                entry_id,
            } => {
                let result = match (client.as_ref(), session.as_ref()) {
                    (Some(client), Some(session)) => {
                        client
                            .remove_playlist_entries(session, &playlist_id, &[entry_id])
                            .await
                    }
                    _ => Err(ApiError::InvalidConfiguration),
                };
                match result {
                    Ok(()) => {
                        send_event(
                            events,
                            ctx,
                            WorkerEvent::PlaylistMutationCompleted {
                                request_id,
                                playlist_id,
                                kind: PlaylistMutationKind::Removed,
                            },
                        )
                        .await
                    }
                    Err(ApiError::ReauthenticationRequired) => {
                        session = None;
                        client = None;
                        server = None;
                        playback_info_cache = None;
                        send_event(
                            events,
                            ctx,
                            WorkerEvent::ReauthenticationRequired {
                                request_id,
                                message: "The server requires you to sign in again.".into(),
                            },
                        )
                        .await;
                    }
                    Err(error) => {
                        send_event(
                            events,
                            ctx,
                            WorkerEvent::PlaylistMutationFailed {
                                request_id,
                                playlist_id,
                                kind: PlaylistMutationKind::Removed,
                                error_class: error.classify(),
                            },
                        )
                        .await
                    }
                }
            }
            WorkerCommand::MovePlaylistEntry {
                request_id,
                playlist_id,
                entry_id,
                new_index,
            } => {
                let result = match (client.as_ref(), session.as_ref()) {
                    (Some(client), Some(session)) => {
                        client
                            .move_playlist_entry(session, &playlist_id, &entry_id, new_index)
                            .await
                    }
                    _ => Err(ApiError::InvalidConfiguration),
                };
                match result {
                    Ok(()) => {
                        send_event(
                            events,
                            ctx,
                            WorkerEvent::PlaylistMutationCompleted {
                                request_id,
                                playlist_id,
                                kind: PlaylistMutationKind::Moved,
                            },
                        )
                        .await
                    }
                    Err(ApiError::ReauthenticationRequired) => {
                        session = None;
                        client = None;
                        server = None;
                        playback_info_cache = None;
                        send_event(
                            events,
                            ctx,
                            WorkerEvent::ReauthenticationRequired {
                                request_id,
                                message: "The server requires you to sign in again.".into(),
                            },
                        )
                        .await;
                    }
                    Err(error) => {
                        send_event(
                            events,
                            ctx,
                            WorkerEvent::PlaylistMutationFailed {
                                request_id,
                                playlist_id,
                                kind: PlaylistMutationKind::Moved,
                                error_class: error.classify(),
                            },
                        )
                        .await
                    }
                }
            }
            WorkerCommand::ListSimilar {
                request_id,
                item_id,
                limit,
            } => {
                let result = match (client.as_ref(), session.as_ref()) {
                    (Some(client), Some(session)) => {
                        (client.list_similar(session, &item_id, limit)).await
                    }
                    _ => Err(ApiError::InvalidConfiguration),
                };
                match result {
                    Ok(response) => {
                        send_event(
                            events,
                            ctx,
                            WorkerEvent::SimilarItemsLoaded {
                                request_id,
                                item_id,
                                items: response.items,
                            },
                        )
                        .await
                    }
                    Err(ApiError::ReauthenticationRequired) => {
                        session = None;
                        client = None;
                        server = None;
                        playback_info_cache = None;
                        send_event(
                            events,
                            ctx,
                            WorkerEvent::ReauthenticationRequired {
                                request_id,
                                message: "The server requires you to sign in again.".into(),
                            },
                        )
                        .await;
                    }
                    Err(_) => {
                        tracing::debug!(request_id, "similar items request failed");
                        send_event(
                            events,
                            ctx,
                            WorkerEvent::SimilarItemsFailed {
                                request_id,
                                item_id,
                            },
                        )
                        .await;
                    }
                }
            }
            WorkerCommand::ListPersonItems {
                request_id,
                person_id,
                limit,
            } => {
                let result = match (client.as_ref(), session.as_ref()) {
                    (Some(client), Some(session)) => {
                        (client.list_person_items(session, &person_id, limit)).await
                    }
                    _ => Err(ApiError::InvalidConfiguration),
                };
                match result {
                    Ok(response) => {
                        send_event(
                            events,
                            ctx,
                            WorkerEvent::PersonItemsLoaded {
                                request_id,
                                person_id,
                                items: response.items,
                            },
                        )
                        .await
                    }
                    Err(ApiError::ReauthenticationRequired) => {
                        session = None;
                        client = None;
                        server = None;
                        playback_info_cache = None;
                        send_event(
                            events,
                            ctx,
                            WorkerEvent::ReauthenticationRequired {
                                request_id,
                                message: "The server requires you to sign in again.".into(),
                            },
                        )
                        .await;
                    }
                    Err(error) => {
                        send_event(
                            events,
                            ctx,
                            WorkerEvent::PersonItemsFailed {
                                request_id,
                                person_id,
                                message: error.to_string(),
                            },
                        )
                        .await
                    }
                }
            }
            WorkerCommand::GetPersonArtwork {
                request_id,
                name,
                tag,
            } => {
                start_person_artwork_load(
                    request_id,
                    name,
                    tag,
                    client.as_ref().cloned(),
                    session.as_ref().cloned(),
                    events.clone(),
                    ctx.clone(),
                )
                .await;
            }
            WorkerCommand::GetPrimaryArtwork {
                request_id,
                item_id,
                image_tag,
            } => {
                start_primary_artwork_load(
                    request_id,
                    item_id,
                    image_tag,
                    client.as_ref().cloned(),
                    session.as_ref().cloned(),
                    events.clone(),
                    ctx.clone(),
                )
                .await;
            }
            WorkerCommand::LoadPhotoThumbnail {
                request_id,
                item_id,
                image_tag,
                photo_index,
                visible,
            } => {
                start_photo_image_load(
                    PhotoImageRequest {
                        request_id,
                        item_id,
                        image_tag,
                        full_size: false,
                        photo_index,
                        visible,
                    },
                    client.as_ref().cloned(),
                    session.as_ref().cloned(),
                    events.clone(),
                    ctx.clone(),
                )
                .await;
            }
            WorkerCommand::LoadPhotoFullSize {
                request_id,
                item_id,
                image_tag,
                photo_index,
                visible,
            } => {
                start_photo_image_load(
                    PhotoImageRequest {
                        request_id,
                        item_id,
                        image_tag,
                        full_size: true,
                        photo_index,
                        visible,
                    },
                    client.as_ref().cloned(),
                    session.as_ref().cloned(),
                    events.clone(),
                    ctx.clone(),
                )
                .await;
            }
            WorkerCommand::GetBackdropArtwork {
                request_id,
                item_id,
            } => {
                start_backdrop_artwork_load(
                    request_id,
                    item_id,
                    client.as_ref().cloned(),
                    session.as_ref().cloned(),
                    events.clone(),
                    ctx.clone(),
                )
                .await;
            }
            WorkerCommand::LoadCustomBackdrop { request_id, path } => {
                let result = custom_backdrop::read_custom_backdrop(&path)
                    .await
                    .and_then(|bytes| decode_backdrop_artwork(bytes).ok());
                let event = match result {
                    Some((width, height, rgba)) => WorkerEvent::CustomBackdropLoaded {
                        request_id,
                        width,
                        height,
                        rgba,
                    },
                    None => WorkerEvent::CustomBackdropFailed { request_id },
                };
                send_event(events, ctx, event).await;
            }
            WorkerCommand::GetLiveTvLogo {
                request_id,
                channel_id,
                image_tag,
            } => {
                start_live_tv_logo_load(
                    request_id,
                    channel_id,
                    (!image_tag.is_empty()).then_some(image_tag),
                    client.as_ref().cloned(),
                    session.as_ref().cloned(),
                    events.clone(),
                    ctx.clone(),
                )
                .await;
            }
            WorkerCommand::PlaybackInfo {
                request_id,
                item_id,
                is_audio,
                request,
            } => {
                playback_info_cache = None;
                let result = match (client.as_ref(), session.as_ref()) {
                    (Some(client), Some(session)) => {
                        (client.playback_info(session, &item_id, &request)).await
                    }
                    _ => Err(ApiError::InvalidConfiguration),
                };
                match result {
                    Ok(response) => {
                        if let Some(error_code) = response.error_code {
                            // Only the typed enum is safe to log; the response can hold signed URLs.
                            tracing::debug!(
                                request_id,
                                ?error_code,
                                "playback negotiation returned an error"
                            );
                        }
                        playback_info_cache = Some(CachedPlaybackInfo {
                            request_id,
                            item_id,
                            is_audio,
                            response: response.clone(),
                        });
                        send_event(
                            events,
                            ctx,
                            WorkerEvent::PlaybackInfoLoaded {
                                request_id,
                                response,
                            },
                        )
                        .await;
                    }
                    Err(ApiError::ReauthenticationRequired) => {
                        session = None;
                        client = None;
                        server = None;
                        playback_info_cache = None;
                        send_event(
                            events,
                            ctx,
                            WorkerEvent::ReauthenticationRequired {
                                request_id,
                                message: "The server requires you to sign in again.".into(),
                            },
                        )
                        .await;
                    }
                    Err(error) => {
                        send_event(
                            events,
                            ctx,
                            WorkerEvent::PlaybackInfoFailed {
                                request_id,
                                error_class: error.classify(),
                                retry_after: error.retry_after(),
                            },
                        )
                        .await
                    }
                }
            }
            WorkerCommand::PreparePlayback {
                request_id,
                negotiation_id,
                item_id,
                source_index,
                play_method,
                start_time_ticks,
                live_device_profile,
                live_tune_started_at,
            } => {
                let result = match (client.as_ref(), session.as_ref()) {
                    (Some(client), Some(session)) => match live_device_profile {
                        Some(device_profile) => {
                            tune_live_tv(
                                client,
                                session,
                                &item_id,
                                &device_profile,
                                live_tune_started_at.unwrap_or_else(std::time::Instant::now),
                            )
                            .await
                        }
                        None => prepare_playback(PlaybackPreparation {
                            client: Some(client),
                            session: Some(session),
                            cached: playback_info_cache.as_ref(),
                            negotiation_id,
                            item_id: &item_id,
                            source_index,
                            play_method,
                            start_time_ticks,
                        }),
                    },
                    _ => Err("Connect to a Jellyfin server before starting playback.".into()),
                };
                match result {
                    Ok(playback) => {
                        send_event(
                            events,
                            ctx,
                            WorkerEvent::PlaybackPrepared {
                                request_id,
                                item_id,
                                playback: Box::new(playback),
                            },
                        )
                        .await
                    }
                    Err(message) => {
                        send_event(
                            events,
                            ctx,
                            WorkerEvent::PlaybackPrepareFailed {
                                request_id,
                                message,
                            },
                        )
                        .await
                    }
                }
            }
            WorkerCommand::CloseLiveStream { live_stream_id } => {
                if let (Some(client), Some(session)) = (client.as_ref(), session.as_ref())
                    && let Err(error) = client.close_live_stream(session, &live_stream_id).await
                {
                    tracing::debug!(
                        error_class = ?error.classify(),
                        "could not close a stale Live TV stream"
                    );
                }
            }
            WorkerCommand::ReportPlayback { request_id, action } => {
                let live_stream_id = match &action {
                    PlaybackReportAction::Stopped(info) => info.live_stream_id.clone(),
                    PlaybackReportAction::Started(_) | PlaybackReportAction::Progress(_) => None,
                };
                let result = match (client.as_ref(), session.as_ref()) {
                    (Some(client), Some(session)) => {
                        (async {
                            match action {
                                PlaybackReportAction::Started(info) => {
                                    client.report_playback_start(session, &info).await
                                }
                                PlaybackReportAction::Progress(info) => {
                                    client.report_playback_progress(session, &info).await
                                }
                                PlaybackReportAction::Stopped(info) => {
                                    client.report_playback_stopped(session, &info).await
                                }
                            }
                        })
                        .await
                    }
                    _ => Err(ApiError::InvalidConfiguration),
                };
                if let (Some(live_stream_id), Some(client), Some(session)) =
                    (live_stream_id.as_deref(), client.as_ref(), session.as_ref())
                {
                    // Server cleanup is independent of whether session reporting succeeded.
                    let _ = client.close_live_stream(session, live_stream_id).await;
                }
                match result {
                    Ok(()) => {
                        send_event(events, ctx, WorkerEvent::PlaybackReportSent { request_id })
                            .await
                    }
                    Err(ApiError::ReauthenticationRequired) => {
                        session = None;
                        client = None;
                        server = None;
                        playback_info_cache = None;
                        send_event(
                            events,
                            ctx,
                            WorkerEvent::ReauthenticationRequired {
                                request_id,
                                message: "The server requires you to sign in again.".into(),
                            },
                        )
                        .await;
                    }
                    Err(error) => {
                        send_event(
                            events,
                            ctx,
                            WorkerEvent::PlaybackReportFailed {
                                request_id,
                                message: error.to_string(),
                            },
                        )
                        .await
                    }
                }
            }
            WorkerCommand::Disconnect => {
                if let Some(saved) = saved_sign_in.take() {
                    saved.forget();
                }
                session = None;
                client = None;
                server = None;
                playback_info_cache = None;
                quick_connect = None;
            }
            WorkerCommand::QuickConnectStart {
                request_id,
                server_url,
                allow_insecure_remote_http,
            } => {
                session = None;
                client = None;
                server = None;
                playback_info_cache = None;
                quick_connect = None;
                send_event(events, ctx, WorkerEvent::CheckingServer { request_id }).await;
                let new_client =
                    match build_client(&server_url, allow_insecure_remote_http, device_id) {
                        Ok(client) => client,
                        Err(message) => {
                            send_event(
                                events,
                                ctx,
                                WorkerEvent::ConnectionFailed {
                                    request_id: Some(request_id),
                                    message,
                                },
                            )
                            .await;
                            return;
                        }
                    };
                match (new_client.quick_connect_enabled()).await {
                    Ok(true) => {}
                    Ok(false) => {
                        send_event(
                            events,
                            ctx,
                            WorkerEvent::ConnectionFailed {
                                request_id: Some(request_id),
                                message: "Quick Connect is not enabled on this server.".into(),
                            },
                        )
                        .await;
                        return;
                    }
                    Err(error) => {
                        send_event(
                            events,
                            ctx,
                            WorkerEvent::ConnectionFailed {
                                request_id: Some(request_id),
                                message: error.to_string(),
                            },
                        )
                        .await;
                        return;
                    }
                }
                let identity = match (new_client.public_system_info()).await {
                    Ok(info) => server_identity(&info),
                    Err(error) => {
                        send_event(
                            events,
                            ctx,
                            WorkerEvent::ConnectionFailed {
                                request_id: Some(request_id),
                                message: error.to_string(),
                            },
                        )
                        .await;
                        return;
                    }
                };
                match (new_client.initiate_quick_connect()).await {
                    Ok(request) => {
                        let code = request.code().to_owned();
                        quick_connect = Some(QuickConnectSession {
                            request_id,
                            client: new_client,
                            request,
                            identity,
                        });
                        send_event(
                            events,
                            ctx,
                            WorkerEvent::QuickConnectCode { request_id, code },
                        )
                        .await;
                    }
                    Err(error) => {
                        send_event(
                            events,
                            ctx,
                            WorkerEvent::QuickConnectFailed {
                                request_id,
                                message: error.to_string(),
                            },
                        )
                        .await
                    }
                }
            }
            WorkerCommand::CheckQuickConnect {
                request_id,
                server_url,
                allow_insecure_remote_http,
            } => {
                let enabled = match build_client(&server_url, allow_insecure_remote_http, device_id)
                {
                    Ok(client) => client.quick_connect_enabled().await.unwrap_or(false),
                    Err(_) => false,
                };
                send_event(
                    events,
                    ctx,
                    WorkerEvent::QuickConnectAvailability {
                        request_id,
                        enabled,
                    },
                )
                .await;
            }
            WorkerCommand::ForgotPassword {
                request_id,
                server_url,
                allow_insecure_remote_http,
                username,
            } => {
                let result = match build_client(&server_url, allow_insecure_remote_http, device_id)
                {
                    Ok(client) => client
                        .forgot_password(&username)
                        .await
                        .map_err(|error| error.classify()),
                    Err(_) => Err(ErrorClass::Unexpected),
                };
                send_event(
                    events,
                    ctx,
                    WorkerEvent::ForgotPasswordFinished { request_id, result },
                )
                .await;
            }
            WorkerCommand::QuickConnectPoll { request_id } => {
                let outcome = match quick_connect.as_ref() {
                    Some(flow) => quick_connect_outcome(
                        (flow.client.quick_connect_status(flow.request.secret())).await,
                    ),
                    None => PollOutcome::Error,
                };
                send_event(
                    events,
                    ctx,
                    WorkerEvent::QuickConnectPolled {
                        request_id,
                        outcome,
                    },
                )
                .await;
            }
            WorkerCommand::QuickConnectAuthenticate { request_id, saved } => {
                let Some(flow) = quick_connect.take() else {
                    send_event(
                        events,
                        ctx,
                        WorkerEvent::ConnectionFailed {
                            request_id: Some(request_id),
                            message: "Quick Connect is no longer active. Start again.".into(),
                        },
                    )
                    .await;
                    return;
                };
                let auth_result = (flow
                    .client
                    .authenticate_with_quick_connect(flow.request.secret()))
                .await;
                let new_session = match auth_result {
                    Ok(session) => session,
                    Err(error) => {
                        send_event(
                            events,
                            ctx,
                            WorkerEvent::ConnectionFailed {
                                request_id: Some(request_id),
                                message: error.to_string(),
                            },
                        )
                        .await;
                        return;
                    }
                };
                let identity = flow.identity.clone();
                let user = new_session.user.clone();
                saved_sign_in = saved_sign_in::apply_after_sign_in(
                    &events.saved_sign_in,
                    &saved,
                    &flow.client,
                    identity.id.as_deref(),
                    new_session.token_for_credential_store().as_str(),
                );
                client = Some(flow.client);
                session = Some(new_session);
                session_socket = session_socket::start(client.as_ref(), session.as_ref(), events);
                clock_sampler = clock_sampler::start(client.as_ref(), session.as_ref(), events);
                server = Some(identity.clone());
                send_event(
                    events,
                    ctx,
                    WorkerEvent::CatalogLoading {
                        request_id,
                        server: identity.clone(),
                        user: user.clone(),
                    },
                )
                .await;
                let page_result = match (client.as_ref(), session.as_ref()) {
                    (Some(client), Some(session)) => {
                        (client.list_items(
                            session,
                            &item_query(
                                0,
                                None,
                                None,
                                None,
                                ItemSortBy::SortName,
                                ItemSortOrder::Ascending,
                                None,
                            ),
                        ))
                        .await
                    }
                    _ => Err(ApiError::InvalidConfiguration),
                };
                handle_items_result(
                    page_result,
                    request_id,
                    identity,
                    user,
                    events,
                    ctx,
                    &mut client,
                    &mut session,
                    &mut server,
                    &mut playback_info_cache,
                )
                .await;
            }
            WorkerCommand::QuickConnectCancel { request_id } => {
                if quick_connect
                    .as_ref()
                    .is_some_and(|flow| flow.request_id == request_id)
                {
                    quick_connect = None;
                }
            }
            WorkerCommand::LoadDashboard { request_id } => {
                let result = match (client.as_ref(), session.as_ref()) {
                    (Some(client), Some(session)) => load_dashboard(client, session).await,
                    _ => Err(ApiError::InvalidConfiguration),
                };
                match result {
                    Ok(snapshot) => {
                        send_event(
                            events,
                            ctx,
                            WorkerEvent::DashboardLoaded {
                                request_id,
                                load: AdminLoad::Loaded(snapshot),
                            },
                        )
                        .await
                    }
                    Err(ApiError::ReauthenticationRequired) => {
                        session = None;
                        client = None;
                        server = None;
                        playback_info_cache = None;
                        send_event(
                            events,
                            ctx,
                            WorkerEvent::ReauthenticationRequired {
                                request_id,
                                message: "The server requires you to sign in again.".into(),
                            },
                        )
                        .await;
                    }
                    Err(ApiError::Forbidden) => {
                        send_event(
                            events,
                            ctx,
                            WorkerEvent::DashboardLoaded {
                                request_id,
                                load: AdminLoad::Refused,
                            },
                        )
                        .await
                    }
                    Err(_) => {
                        send_event(
                            events,
                            ctx,
                            WorkerEvent::DashboardLoaded {
                                request_id,
                                load: AdminLoad::Failed,
                            },
                        )
                        .await
                    }
                }
            }
            WorkerCommand::DownloadsOpen { dir } => {
                let event = match downloads::open(&dir) {
                    Ok(manager) => {
                        let event = downloads::list_event(Some(&manager));
                        downloads = Some(manager);
                        event
                    }
                    Err(message) => downloads::failure_event(message),
                };
                send_event(events, ctx, event).await;
            }
            WorkerCommand::DownloadsList => {
                let event = downloads::list_event(downloads.as_ref());
                send_event(events, ctx, event).await;
            }
            WorkerCommand::DownloadStart { item_id, base_url } => {
                let event = match (client.as_ref(), session.as_ref()) {
                    (Some(client), Some(session)) => {
                        let auth = client.session_authorization(session);
                        downloads::start_event(
                            downloads.as_ref(),
                            session.server_id.as_deref(),
                            &base_url,
                            &item_id,
                            &auth,
                        )
                    }
                    _ => downloads::failure_event(downloads::SIGN_IN_MESSAGE),
                };
                send_event(events, ctx, event).await;
            }
            WorkerCommand::DownloadPause { item_id } => {
                let event = downloads::pause_event(downloads.as_ref(), &item_id);
                send_event(events, ctx, event).await;
            }
            WorkerCommand::DownloadResume { item_id, base_url } => {
                let event = match (client.as_ref(), session.as_ref()) {
                    (Some(client), Some(session)) => {
                        let auth = client.session_authorization(session);
                        downloads::resume_event(
                            downloads.as_ref(),
                            session.server_id.as_deref(),
                            &base_url,
                            &item_id,
                            &auth,
                        )
                    }
                    _ => downloads::failure_event(downloads::SIGN_IN_MESSAGE),
                };
                send_event(events, ctx, event).await;
            }
            WorkerCommand::ApplyUpdate { job } => {
                let outcome = update::run_apply(*job).await;
                send_event(events, ctx, WorkerEvent::UpdateFinished { outcome }).await;
            }
            WorkerCommand::RollbackUpdate { store_dir } => {
                let outcome = update::run_rollback(store_dir).await;
                send_event(events, ctx, WorkerEvent::UpdateFinished { outcome }).await;
            }
            WorkerCommand::Shutdown => (),
        }
    }
    .await;
    // The socket belongs to a session. Any path that ends the session (sign-out, expiry,
    // failed sign-in) drops it here, and the drop stops its thread.
    let session_socket = session_socket.filter(|_| session.is_some());
    let clock_sampler = clock_sampler.filter(|_| session.is_some());
    *state = WorkerState {
        client,
        session,
        server,
        playback_info_cache,
        quick_connect,
        downloads,
        session_socket,
        clock_sampler,
        saved_sign_in,
    };
}

async fn run_metadata_op(
    client: &JellyfinClient,
    session: &Session,
    item_id: &str,
    op: MetadataOp,
) -> Result<MetadataReply, ApiError> {
    Ok(match op {
        MetadataOp::Load => {
            MetadataReply::Loaded(Box::new(client.get_item_metadata(session, item_id).await?))
        }
        MetadataOp::Save { item, fields } => {
            client.update_item_metadata(session, &item, &fields).await?;
            MetadataReply::Saved
        }
        MetadataOp::Search { item_type, query } => MetadataReply::SearchResults(
            client
                .remote_search(session, item_id, &item_type, &query)
                .await?,
        ),
        MetadataOp::Apply {
            result,
            replace_images,
        } => {
            client
                .apply_remote_search_result(session, item_id, &result, replace_images)
                .await?;
            MetadataReply::Applied
        }
        MetadataOp::ListImages => {
            MetadataReply::Images(client.item_images(session, item_id).await?)
        }
        MetadataOp::RemoteImages {
            image_type,
            all_languages,
        } => MetadataReply::RemoteImages(
            client
                .remote_images(session, item_id, image_type, all_languages)
                .await?,
        ),
        MetadataOp::Download { image_type, url } => {
            client
                .download_remote_image(session, item_id, image_type, &url)
                .await?;
            MetadataReply::ImagesChanged
        }
        MetadataOp::DeleteImage { image_type, index } => {
            client
                .delete_item_image(session, item_id, image_type, index)
                .await?;
            MetadataReply::ImagesChanged
        }
    })
}

async fn decode_metadata_thumbnail(
    bytes: Vec<u8>,
    events: &WorkerSink,
) -> Result<(u32, u32, Vec<u8>), ()> {
    let permit = events
        .decoders
        .clone()
        .acquire_owned()
        .await
        .map_err(|_| ())?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        decode_metadata_thumbnail_bytes(bytes)
    })
    .await
    .map_err(|_| ())?
}

/// Provider previews can be poster-sized; they are decoded with bounded limits, then shrunk
/// to a small preview so a dialog of results keeps little texture memory.
fn decode_metadata_thumbnail_bytes(bytes: Vec<u8>) -> Result<(u32, u32, Vec<u8>), ()> {
    const MAX_SOURCE_DIMENSION: u32 = 4_000;
    const MAX_SOURCE_ALLOC: u64 = 64 * 1024 * 1024;
    const PREVIEW_DIMENSION: u32 = 240;
    let mut reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|_| ())?;
    if !matches!(reader.format(), Some(ImageFormat::Jpeg | ImageFormat::Png)) {
        return Err(());
    }
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_SOURCE_DIMENSION);
    limits.max_image_height = Some(MAX_SOURCE_DIMENSION);
    limits.max_alloc = Some(MAX_SOURCE_ALLOC);
    reader.limits(limits);
    let decoded = reader.decode().map_err(|_| ())?;
    let image = if decoded.width() > PREVIEW_DIMENSION || decoded.height() > PREVIEW_DIMENSION {
        decoded.thumbnail(PREVIEW_DIMENSION, PREVIEW_DIMENSION)
    } else {
        decoded
    }
    .to_rgba8();
    let (width, height) = (image.width(), image.height());
    if width == 0 || height == 0 {
        return Err(());
    }
    Ok((width, height, image.into_raw()))
}

/// Why a subtitle search or download did not complete.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SubtitleRequestError {
    /// The user may not manage subtitles on this server (HTTP 403).
    Forbidden,
    /// Anything else: the server, the network or the provider failed.
    Failed,
}

impl SubtitleRequestError {
    fn from_api(error: ApiError) -> Self {
        tracing::debug!(%error, "subtitle request failed");
        if matches!(error, ApiError::Forbidden) {
            Self::Forbidden
        } else {
            Self::Failed
        }
    }
}

/// An external subtitle copied from the server to a local file the player can open.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DownloadedSubtitle {
    pub path: PathBuf,
    pub title: Option<String>,
    pub language: Option<String>,
}

/// How often the stream list is read again while the server is still saving a download.
const SUBTITLE_STREAM_RETRIES: u32 = 3;

/// Downloads `subtitle_id` to the server, then, for the player, copies the external text
/// subtitles that appeared on the item to local files.
async fn download_remote_subtitle(
    client: &JellyfinClient,
    session: &Session,
    item_id: &str,
    subtitle_id: &str,
    for_player: bool,
) -> Result<Vec<DownloadedSubtitle>, SubtitleRequestError> {
    let before = if for_player {
        client
            .item_subtitle_streams(session, item_id)
            .await
            .map_err(SubtitleRequestError::from_api)?
    } else {
        Vec::new()
    };
    client
        .download_remote_subtitle(session, item_id, subtitle_id)
        .await
        .map_err(SubtitleRequestError::from_api)?;
    if !for_player {
        return Ok(Vec::new());
    }
    // The server saves the file and rescans the item; the new stream can take a moment to
    // appear, so the list is read a few times before giving up on loading it now.
    let mut added = Vec::new();
    for attempt in 0..SUBTITLE_STREAM_RETRIES {
        if attempt > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(700)).await;
        }
        let Ok(after) = client.item_subtitle_streams(session, item_id).await else {
            continue;
        };
        added = after
            .into_iter()
            .filter(|stream| stream.is_external && stream.is_text && !stream.codec.is_empty())
            .filter(|stream| !before.contains(stream))
            .collect::<Vec<_>>();
        if !added.is_empty() {
            break;
        }
    }
    let mut files = Vec::new();
    for stream in added.into_iter().take(4) {
        let bytes = match client.subtitle_stream_file(session, item_id, &stream).await {
            Ok(bytes) => bytes,
            Err(error) => {
                tracing::debug!(%error, "downloaded subtitle could not be read back");
                continue;
            }
        };
        let name = format!(
            "{}-{}-{}.{}",
            safe_file_part(item_id),
            safe_file_part(&stream.media_source_id),
            stream.index,
            stream.codec
        );
        let written = tokio::task::spawn_blocking(move || write_subtitle_file(&name, &bytes))
            .await
            .ok()
            .flatten();
        if let Some(path) = written {
            files.push(DownloadedSubtitle {
                path,
                title: stream.title,
                language: stream.language,
            });
        }
    }
    Ok(files)
}

/// Letters, digits and `-` only, at most 64 characters, so the name cannot leave its folder.
fn safe_file_part(value: &str) -> String {
    value
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .take(64)
        .collect()
}

/// Writes one subtitle file into the app's cache folder, replacing an older copy.
fn write_subtitle_file(name: &str, bytes: &[u8]) -> Option<PathBuf> {
    let folder = directories::ProjectDirs::from("org", "Jellyfin", "Kattri")?
        .cache_dir()
        .join("subtitles");
    write_subtitle_file_in(&folder, name, bytes)
}

fn write_subtitle_file_in(folder: &Path, name: &str, bytes: &[u8]) -> Option<PathBuf> {
    std::fs::create_dir_all(folder).ok()?;
    let path = folder.join(name);
    static TEMP_FILE_ID: AtomicU64 = AtomicU64::new(0);
    let temp_path = folder.join(format!(
        ".subtitle-{}-{}.part",
        std::process::id(),
        TEMP_FILE_ID.fetch_add(1, Ordering::Relaxed)
    ));
    if std::fs::write(&temp_path, bytes).is_err() {
        let _ = std::fs::remove_file(&temp_path);
        return None;
    }
    #[cfg(windows)]
    if path.exists() && std::fs::remove_file(&path).is_err() {
        let _ = std::fs::remove_file(&temp_path);
        return None;
    }
    if std::fs::rename(&temp_path, &path).is_err() {
        let _ = std::fs::remove_file(&temp_path);
        return None;
    }
    Some(path)
}

/// Trickplay sheets are larger than card artwork (a 10x10 sheet of 320 px thumbnails is
/// 3200x1800), so they get their own bounds.
async fn decode_trickplay_sheet(
    bytes: Vec<u8>,
    events: &WorkerSink,
) -> Result<(u32, u32, Vec<u8>), ()> {
    let permit = events
        .decoders
        .clone()
        .acquire_owned()
        .await
        .map_err(|_| ())?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        decode_trickplay_sheet_bytes(bytes)
    })
    .await
    .map_err(|_| ())?
}

fn decode_trickplay_sheet_bytes(bytes: Vec<u8>) -> Result<(u32, u32, Vec<u8>), ()> {
    const MAX_DIMENSION: u32 = 6_400;
    const MAX_ALLOC: u64 = 96 * 1024 * 1024;
    let mut reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|_| ())?;
    if !matches!(reader.format(), Some(ImageFormat::Jpeg | ImageFormat::Png)) {
        return Err(());
    }
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_DIMENSION);
    limits.max_image_height = Some(MAX_DIMENSION);
    limits.max_alloc = Some(MAX_ALLOC);
    reader.limits(limits);
    let image = reader.decode().map_err(|_| ())?.to_rgba8();
    let (width, height) = (image.width(), image.height());
    if width == 0 || height == 0 {
        return Err(());
    }
    Ok((width, height, image.into_raw()))
}
async fn decode_artwork(
    bytes: Vec<u8>,
    person: bool,
    events: &WorkerSink,
) -> Result<(u32, u32, Vec<u8>), ()> {
    let permit = events
        .decoders
        .clone()
        .acquire_owned()
        .await
        .map_err(|_| ())?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        if person {
            decode_person_artwork(bytes)
        } else {
            decode_primary_artwork(bytes)
        }
    })
    .await
    .map_err(|_| ())?
}

/// Backdrops fill the hero and the detail header, so they get larger limits than card
/// artwork (which is capped at 512x768 and would reject a full-width backdrop).
async fn decode_backdrop(bytes: Vec<u8>, events: &WorkerSink) -> Result<(u32, u32, Vec<u8>), ()> {
    let permit = events
        .decoders
        .clone()
        .acquire_owned()
        .await
        .map_err(|_| ())?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        decode_backdrop_artwork(bytes)
    })
    .await
    .map_err(|_| ())?
}

async fn decode_live_tv_logo(
    bytes: Vec<u8>,
    events: &WorkerSink,
) -> Result<(u32, u32, Vec<u8>), ()> {
    let permit = events
        .decoders
        .clone()
        .acquire_owned()
        .await
        .map_err(|_| ())?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        decode_live_tv_logo_image(bytes)
    })
    .await
    .map_err(|_| ())?
}

async fn decode_photo_image(
    bytes: Vec<u8>,
    full_size: bool,
    photo_index: usize,
    visible: bool,
    events: &WorkerSink,
) -> Result<(u32, u32, Vec<u8>), ()> {
    let permit = events
        .decoders
        .clone()
        .acquire_owned()
        .await
        .map_err(|_| ())?;
    #[cfg(feature = "photo-counters")]
    let counters = events.photo_decode_counters.clone();
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        #[cfg(feature = "photo-counters")]
        {
            decode_photo_image_bytes_counted(bytes, full_size, photo_index, visible, &counters)
        }
        #[cfg(not(feature = "photo-counters"))]
        {
            let _ = (photo_index, visible);
            decode_photo_image_bytes(bytes, full_size)
        }
    })
    .await
    .map_err(|_| ())?
}

#[cfg(feature = "photo-counters")]
pub(crate) fn decode_photo_image_bytes_counted(
    bytes: Vec<u8>,
    full_size: bool,
    photo_index: usize,
    visible: bool,
    counters: &kattri_client_core::photo_grid::DecodeCounters,
) -> Result<(u32, u32, Vec<u8>), ()> {
    let decoded = decode_photo_image_bytes(bytes, full_size)?;
    counters.record_decode(photo_index, visible);
    Ok(decoded)
}

fn decode_photo_image_bytes(bytes: Vec<u8>, full_size: bool) -> Result<(u32, u32, Vec<u8>), ()> {
    const THUMBNAIL_MAX_DIMENSION: u32 = 512;
    const THUMBNAIL_MAX_ALLOC: u64 = 4 * 1024 * 1024;
    const FULL_SIZE_MAX_DIMENSION: u32 = 8_000;
    const FULL_SIZE_MAX_ALLOC: u64 = 256 * 1024 * 1024;
    let mut reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|_| ())?;
    if !matches!(reader.format(), Some(ImageFormat::Jpeg | ImageFormat::Png)) {
        return Err(());
    }
    let (max_dimension, max_alloc) = if full_size {
        (FULL_SIZE_MAX_DIMENSION, FULL_SIZE_MAX_ALLOC)
    } else {
        (THUMBNAIL_MAX_DIMENSION, THUMBNAIL_MAX_ALLOC)
    };
    let mut limits = Limits::default();
    limits.max_image_width = Some(max_dimension);
    limits.max_image_height = Some(max_dimension);
    limits.max_alloc = Some(max_alloc);
    reader.limits(limits);
    let mut decoder = reader.into_decoder().map_err(|_| ())?;
    let orientation = decoder.orientation().map_err(|_| ())?;
    let mut decoded = image::DynamicImage::from_decoder(decoder).map_err(|_| ())?;
    decoded.apply_orientation(orientation);
    let image = decoded.to_rgba8();
    let (width, height) = (image.width(), image.height());
    let rgba = image.into_raw();
    let expected_len = usize::try_from(width)
        .ok()
        .and_then(|width| {
            usize::try_from(height)
                .ok()
                .and_then(|height| width.checked_mul(height))
        })
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or(())?;
    if width == 0
        || height == 0
        || width > max_dimension
        || height > max_dimension
        || rgba.len() != expected_len
        || u64::try_from(rgba.len()).map_err(|_| ())? > max_alloc
    {
        return Err(());
    }
    Ok((width, height, rgba))
}

fn decode_live_tv_logo_image(bytes: Vec<u8>) -> Result<(u32, u32, Vec<u8>), ()> {
    const MAX_DECODED_BYTES: usize = 1024 * 1024;
    let mut reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|_| ())?;
    if !matches!(reader.format(), Some(ImageFormat::Jpeg | ImageFormat::Png)) {
        return Err(());
    }
    let mut limits = Limits::default();
    limits.max_image_width = Some(128);
    limits.max_image_height = Some(128);
    limits.max_alloc = Some(MAX_DECODED_BYTES as u64);
    reader.limits(limits);
    let image = reader.decode().map_err(|_| ())?.to_rgba8();
    let (width, height) = (image.width(), image.height());
    let decoded_len = usize::try_from(width)
        .ok()
        .and_then(|width| {
            usize::try_from(height)
                .ok()
                .and_then(|height| width.checked_mul(height))
        })
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or(())?;
    let rgba = image.into_raw();
    if width == 0 || height == 0 || rgba.len() != decoded_len || rgba.len() > MAX_DECODED_BYTES {
        return Err(());
    }
    Ok((width, height, rgba))
}

fn decode_primary_artwork(bytes: Vec<u8>) -> Result<(u32, u32, Vec<u8>), ()> {
    let mut reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|_| ())?;
    if !matches!(reader.format(), Some(ImageFormat::Jpeg | ImageFormat::Png)) {
        return Err(());
    }
    let mut limits = Limits::default();
    limits.max_image_width = Some(512);
    limits.max_image_height = Some(768);
    limits.max_alloc = Some(8 * 1024 * 1024);
    reader.limits(limits);
    let image = reader.decode().map_err(|_| ())?.to_rgba8();
    let (width, height) = (image.width(), image.height());
    if width == 0 || height == 0 {
        return Err(());
    }
    Ok((width, height, image.into_raw()))
}

fn decode_backdrop_artwork(bytes: Vec<u8>) -> Result<(u32, u32, Vec<u8>), ()> {
    let mut reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|_| ())?;
    if !matches!(reader.format(), Some(ImageFormat::Jpeg | ImageFormat::Png)) {
        return Err(());
    }
    let mut limits = Limits::default();
    limits.max_image_width = Some(BACKDROP_MAX_WIDTH);
    limits.max_image_height = Some(BACKDROP_MAX_HEIGHT);
    limits.max_alloc = Some(u64::from(BACKDROP_MAX_WIDTH) * u64::from(BACKDROP_MAX_HEIGHT) * 4);
    reader.limits(limits);
    let image = reader.decode().map_err(|_| ())?.to_rgba8();
    let (width, height) = (image.width(), image.height());
    if width == 0 || height == 0 {
        return Err(());
    }
    Ok((width, height, image.into_raw()))
}

/// The request asks for 1920 wide; these bounds also accept a server that ignores maxWidth
/// on a 4K backdrop.
const BACKDROP_MAX_WIDTH: u32 = 3840;
const BACKDROP_MAX_HEIGHT: u32 = 2160;

fn decode_person_artwork(bytes: Vec<u8>) -> Result<(u32, u32, Vec<u8>), ()> {
    let mut reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|_| ())?;
    if !matches!(reader.format(), Some(ImageFormat::Jpeg | ImageFormat::Png)) {
        return Err(());
    }

    let mut limits = Limits::default();
    limits.max_image_width = Some(512);
    limits.max_image_height = Some(768);
    limits.max_alloc = Some(8 * 1024 * 1024);
    reader.limits(limits);
    let image = reader.decode().map_err(|_| ())?.to_rgba8();
    let (width, height) = (image.width(), image.height());
    if width == 0 || height == 0 {
        return Err(());
    }
    Ok((width, height, image.into_raw()))
}

async fn start_person_artwork_load(
    request_id: u64,
    name: String,
    tag: Option<String>,
    client: Option<JellyfinClient>,
    session: Option<Session>,
    events: WorkerSink,
    ctx: egui::Context,
) {
    let (Some(client), Some(session)) = (client, session) else {
        send_event(&events, &ctx, WorkerEvent::PersonImageFailed { request_id }).await;
        return;
    };

    let result = (client.get_person_image(&session, &name, tag.as_deref())).await;
    match result {
        Ok(bytes) => match decode_artwork(bytes, true, &events).await {
            Ok((width, height, rgba)) => {
                send_event(
                    &events,
                    &ctx,
                    WorkerEvent::PersonImageLoaded {
                        request_id,
                        name,
                        width,
                        height,
                        rgba,
                    },
                )
                .await
            }
            Err(()) => {
                send_event(&events, &ctx, WorkerEvent::PersonImageFailed { request_id }).await
            }
        },
        Err(ApiError::ReauthenticationRequired) => {
            send_event(
                &events,
                &ctx,
                WorkerEvent::ReauthenticationRequired {
                    request_id,
                    message: "The server requires you to sign in again.".into(),
                },
            )
            .await
        }
        Err(_) => send_event(&events, &ctx, WorkerEvent::PersonImageFailed { request_id }).await,
    }
}

async fn start_primary_artwork_load(
    request_id: u64,
    item_id: String,
    image_tag: Option<String>,
    client: Option<JellyfinClient>,
    session: Option<Session>,
    events: WorkerSink,
    ctx: egui::Context,
) {
    let (Some(client), Some(session)) = (client, session) else {
        send_event(
            &events,
            &ctx,
            WorkerEvent::PrimaryArtworkFailed { request_id },
        )
        .await;
        return;
    };

    let result = (client.get_primary_image(&session, &item_id, image_tag.as_deref())).await;
    match result {
        Ok(bytes) => match decode_artwork(bytes, false, &events).await {
            Ok((width, height, rgba)) => {
                send_event(
                    &events,
                    &ctx,
                    WorkerEvent::PrimaryArtworkLoaded {
                        request_id,
                        item_id,
                        width,
                        height,
                        rgba,
                    },
                )
                .await
            }
            Err(()) => {
                send_event(
                    &events,
                    &ctx,
                    WorkerEvent::PrimaryArtworkFailed { request_id },
                )
                .await
            }
        },
        Err(ApiError::ReauthenticationRequired) => {
            send_event(
                &events,
                &ctx,
                WorkerEvent::ReauthenticationRequired {
                    request_id,
                    message: "The server requires you to sign in again.".into(),
                },
            )
            .await
        }
        Err(_) => {
            send_event(
                &events,
                &ctx,
                WorkerEvent::PrimaryArtworkFailed { request_id },
            )
            .await
        }
    }
}

async fn start_backdrop_artwork_load(
    request_id: u64,
    item_id: String,
    client: Option<JellyfinClient>,
    session: Option<Session>,
    events: WorkerSink,
    ctx: egui::Context,
) {
    let (Some(client), Some(session)) = (client, session) else {
        send_event(
            &events,
            &ctx,
            WorkerEvent::BackdropArtworkFailed {
                request_id,
                item_id,
            },
        )
        .await;
        return;
    };

    let result = (client.get_backdrop_image(&session, &item_id)).await;
    match result {
        Ok(bytes) => match decode_backdrop(bytes, &events).await {
            Ok((width, height, rgba)) => {
                send_event(
                    &events,
                    &ctx,
                    WorkerEvent::BackdropArtworkLoaded {
                        request_id,
                        item_id,
                        width,
                        height,
                        rgba,
                    },
                )
                .await
            }
            Err(()) => {
                send_event(
                    &events,
                    &ctx,
                    WorkerEvent::BackdropArtworkFailed {
                        request_id,
                        item_id,
                    },
                )
                .await
            }
        },
        Err(BackdropImageError::NotFoundOrEmptyImage) => {
            send_event(
                &events,
                &ctx,
                WorkerEvent::BackdropArtworkUnavailable {
                    request_id,
                    item_id,
                },
            )
            .await
        }
        Err(BackdropImageError::Api(ApiError::ReauthenticationRequired)) => {
            send_event(
                &events,
                &ctx,
                WorkerEvent::ReauthenticationRequired {
                    request_id,
                    message: "The server requires you to sign in again.".into(),
                },
            )
            .await
        }
        Err(BackdropImageError::Api(_)) => {
            send_event(
                &events,
                &ctx,
                WorkerEvent::BackdropArtworkFailed {
                    request_id,
                    item_id,
                },
            )
            .await
        }
    }
}

async fn start_live_tv_logo_load(
    request_id: u64,
    channel_id: String,
    image_tag: Option<String>,
    client: Option<JellyfinClient>,
    session: Option<Session>,
    events: WorkerSink,
    ctx: egui::Context,
) {
    let (Some(client), Some(session)) = (client, session) else {
        send_event(
            &events,
            &ctx,
            WorkerEvent::LiveTvLogoFailed {
                request_id,
                channel_id,
            },
        )
        .await;
        return;
    };

    match client
        .get_live_tv_channel_logo(&session, &channel_id, image_tag.as_deref())
        .await
    {
        Ok(bytes) => match decode_live_tv_logo(bytes, &events).await {
            Ok((width, height, rgba)) => {
                send_event(
                    &events,
                    &ctx,
                    WorkerEvent::LiveTvLogoLoaded {
                        request_id,
                        channel_id,
                        width,
                        height,
                        rgba,
                    },
                )
                .await;
            }
            Err(()) => {
                send_event(
                    &events,
                    &ctx,
                    WorkerEvent::LiveTvLogoFailed {
                        request_id,
                        channel_id,
                    },
                )
                .await;
            }
        },
        Err(ApiError::ReauthenticationRequired) => {
            send_event(
                &events,
                &ctx,
                WorkerEvent::ReauthenticationRequired {
                    request_id,
                    message: "The server requires you to sign in again.".into(),
                },
            )
            .await;
        }
        Err(_) => {
            send_event(
                &events,
                &ctx,
                WorkerEvent::LiveTvLogoFailed {
                    request_id,
                    channel_id,
                },
            )
            .await;
        }
    }
}

struct PhotoImageRequest {
    request_id: u64,
    item_id: String,
    image_tag: Option<String>,
    full_size: bool,
    photo_index: usize,
    visible: bool,
}

async fn start_photo_image_load(
    request: PhotoImageRequest,
    client: Option<JellyfinClient>,
    session: Option<Session>,
    events: WorkerSink,
    ctx: egui::Context,
) {
    let PhotoImageRequest {
        request_id,
        item_id,
        image_tag,
        full_size,
        photo_index,
        visible,
    } = request;
    let (Some(client), Some(session)) = (client, session) else {
        send_photo_image_failure(request_id, item_id, full_size, &events, &ctx).await;
        return;
    };

    let result = if full_size {
        client
            .get_photo_full_size_image(&session, &item_id, image_tag.as_deref())
            .await
    } else {
        client
            .get_photo_thumbnail(&session, &item_id, image_tag.as_deref())
            .await
    };
    match result {
        Ok(bytes) => {
            match decode_photo_image(bytes, full_size, photo_index, visible, &events).await {
                Ok((width, height, rgba)) => {
                    let event = if full_size {
                        WorkerEvent::PhotoFullSizeLoaded {
                            request_id,
                            item_id,
                            width,
                            height,
                            rgba,
                        }
                    } else {
                        WorkerEvent::PhotoThumbnailLoaded {
                            request_id,
                            item_id,
                            width,
                            height,
                            rgba,
                        }
                    };
                    send_event(&events, &ctx, event).await;
                }
                Err(()) => {
                    send_photo_image_failure(request_id, item_id, full_size, &events, &ctx).await
                }
            }
        }
        Err(ApiError::ReauthenticationRequired) => {
            send_event(
                &events,
                &ctx,
                WorkerEvent::ReauthenticationRequired {
                    request_id,
                    message: "The server requires you to sign in again.".into(),
                },
            )
            .await;
        }
        Err(_) => send_photo_image_failure(request_id, item_id, full_size, &events, &ctx).await,
    }
}

async fn send_photo_image_failure(
    request_id: u64,
    item_id: String,
    full_size: bool,
    events: &WorkerSink,
    ctx: &egui::Context,
) {
    let event = if full_size {
        WorkerEvent::PhotoFullSizeFailed {
            request_id,
            item_id,
        }
    } else {
        WorkerEvent::PhotoThumbnailFailed {
            request_id,
            item_id,
        }
    };
    send_event(events, ctx, event).await;
}

/// The read-only admin calls. Only the decoded counts and names reach the UI.
async fn load_dashboard(
    client: &JellyfinClient,
    session: &Session,
) -> Result<AdminSnapshot, ApiError> {
    let server = client.server_info(session).await?;
    let libraries = client.virtual_folders(session).await?;
    let active_sessions = client.active_session_count(session).await?;
    let users = client.user_counts(session).await?;
    let tasks = client.scheduled_tasks(session).await?;
    let mut library_item_counts = Vec::with_capacity(libraries.len().min(MAX_COUNTED_LIBRARIES));
    for library in libraries.iter().take(MAX_COUNTED_LIBRARIES) {
        let count = match library.item_id.as_deref() {
            Some(id) => match client.library_item_count(session, id).await {
                Ok(count) => count,
                // A revoked admin right must hide the section, not crash it.
                Err(ApiError::Forbidden) => return Err(ApiError::Forbidden),
                // One unreadable count must not hide the whole dashboard.
                Err(_) => None,
            },
            None => None,
        };
        library_item_counts.push(count);
    }
    Ok(AdminSnapshot::from_reads(
        &server,
        &libraries,
        &library_item_counts,
        active_sessions,
        users,
        &tasks,
    ))
}

async fn load_home_row(
    client: &JellyfinClient,
    session: &Session,
    row: HomeRowKind,
) -> Result<Vec<BaseItemDto>, ApiError> {
    let mut items = match row {
        HomeRowKind::ContinueWatching => client.resume_items(session, HOME_ROW_LIMIT).await?.items,
        HomeRowKind::NextUp => client.next_up(session, HOME_ROW_LIMIT).await?.items,
        HomeRowKind::Latest => client.latest_items(session, None, HOME_ROW_LIMIT).await?,
    };
    items.truncate(HOME_ROW_LIMIT as usize);
    Ok(items)
}

async fn list_music_album_tracks(
    client: &JellyfinClient,
    session: &Session,
    album_id: &str,
) -> Result<Vec<BaseItemDto>, ApiError> {
    const PAGE_SIZE: u32 = 100;
    let mut start_index = 0_u32;
    let mut tracks = Vec::new();
    loop {
        let response = client
            .list_items(
                session,
                &ItemQuery {
                    recursive: false,
                    start_index,
                    limit: PAGE_SIZE,
                    parent_id: Some(album_id.to_owned()),
                    include_item_types: Some(vec!["Audio".to_owned()]),
                    enable_total_record_count: Some(true),
                    name_starts_with: None,
                    name_less_than: None,
                    studio_ids: None,
                    ..ItemQuery::default()
                },
            )
            .await?;
        if response
            .total_record_count
            .is_some_and(|total| usize::try_from(total).is_ok_and(|total| total > MAX_QUEUE_LEN))
            || tracks.len().saturating_add(response.items.len()) > MAX_QUEUE_LEN
        {
            return Err(ApiError::InvalidResponse);
        }
        let received =
            u32::try_from(response.items.len()).map_err(|_| ApiError::InvalidResponse)?;
        tracks.extend(
            response
                .items
                .into_iter()
                .filter(|item| item.item_type.as_deref() == Some("Audio")),
        );
        start_index = start_index
            .checked_add(received)
            .ok_or(ApiError::InvalidResponse)?;
        let total_reached = response
            .total_record_count
            .is_some_and(|total| start_index >= total);
        if total_reached || received < PAGE_SIZE {
            break;
        }
    }
    sort_music_album_tracks(&mut tracks);
    Ok(tracks)
}

async fn list_playlist_entries(
    client: &JellyfinClient,
    session: &Session,
    playlist_id: &str,
) -> Result<Vec<PlaylistEntry>, ApiError> {
    const PAGE_SIZE: u32 = 100;
    let mut start_index = 0_u32;
    let mut entries = Vec::new();
    loop {
        let page = client
            .list_playlist_items(session, playlist_id, start_index, PAGE_SIZE)
            .await?;
        if page
            .total_record_count
            .is_some_and(|total| usize::try_from(total).is_ok_and(|total| total > MAX_QUEUE_LEN))
            || entries.len().saturating_add(page.items.len()) > MAX_QUEUE_LEN
        {
            return Err(ApiError::InvalidResponse);
        }
        let received = u32::try_from(page.items.len()).map_err(|_| ApiError::InvalidResponse)?;
        entries.extend(page.items);
        start_index = start_index
            .checked_add(received)
            .ok_or(ApiError::InvalidResponse)?;
        if page
            .total_record_count
            .is_some_and(|total| start_index >= total)
            || received < PAGE_SIZE
        {
            break;
        }
    }
    Ok(entries)
}

fn sort_music_album_tracks(tracks: &mut [BaseItemDto]) {
    tracks.sort_by_key(|track| {
        (
            track.index_number.is_none(),
            track.index_number.unwrap_or_default(),
        )
    });
}

#[cfg(test)]
mod music_album_track_tests {
    use super::{list_music_album_tracks, list_playlist_entries, sort_music_album_tracks};
    use jellyfin_api::{BaseItemDto, ClientConfig, ItemQuery, JellyfinClient};
    use mock_jellyfin::{MockConfig, spawn};

    #[tokio::test]
    async fn album_loader_requests_only_child_audio_and_orders_by_track_index() {
        let server = spawn(MockConfig::default())
            .await
            .expect("start mock Jellyfin");
        let client = JellyfinClient::new(
            ClientConfig::new(
                &server.base_url(),
                "Kattri test",
                "test device",
                "device-id",
                "test-version",
            )
            .expect("valid loopback config"),
        )
        .expect("create Jellyfin client");
        let session = client
            .authenticate("demo", "demo")
            .await
            .expect("authenticate with mock server");
        let album = client
            .list_items(
                &session,
                &ItemQuery {
                    include_item_types: Some(vec!["MusicAlbum".to_owned()]),
                    limit: 10,
                    ..ItemQuery::default()
                },
            )
            .await
            .expect("list mock albums")
            .items
            .into_iter()
            .find(|item| item.name == "Tidewater Sessions")
            .expect("four-track album exists");

        let tracks = list_music_album_tracks(&client, &session, &album.id)
            .await
            .expect("load album tracks");

        assert_eq!(
            tracks
                .iter()
                .map(|item| item.index_number)
                .collect::<Vec<_>>(),
            [Some(1), Some(2), Some(3), Some(4)]
        );
        assert!(
            tracks
                .iter()
                .all(|item| item.item_type.as_deref() == Some("Audio"))
        );
        server.shutdown().await;
    }

    #[tokio::test]
    async fn playlist_loader_pages_all_entries_and_preserves_duplicate_entry_ids() {
        let server = spawn(MockConfig::default())
            .await
            .expect("start mock Jellyfin");
        let client = JellyfinClient::new(
            ClientConfig::new(
                &server.base_url(),
                "Kattri test",
                "test device",
                "device-id",
                "test-version",
            )
            .expect("valid loopback config"),
        )
        .expect("create Jellyfin client");
        let session = client
            .authenticate("demo", "demo")
            .await
            .expect("authenticate with mock server");
        let first_item = client
            .list_items(&session, &ItemQuery::default())
            .await
            .expect("list mock items")
            .items
            .into_iter()
            .next()
            .expect("mock has library items");
        let playlist_id = client
            .create_playlist(&session, "Queue pagination", &[])
            .await
            .expect("create mock playlist");
        let duplicate_ids = vec![first_item.id; 125];
        client
            .add_playlist_items(&session, &playlist_id, &duplicate_ids)
            .await
            .expect("add duplicate playlist entries");

        let entries = list_playlist_entries(&client, &session, &playlist_id)
            .await
            .expect("load all playlist entries");

        assert_eq!(entries.len(), 125);
        assert!(
            entries
                .iter()
                .all(|entry| entry.item.id == duplicate_ids[0])
        );
        let entry_ids = entries
            .iter()
            .map(|entry| entry.playlist_item_id.as_deref())
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(entry_ids.len(), 125);
        assert!(!entry_ids.contains(&None));
        server.shutdown().await;
    }

    #[test]
    fn album_tracks_sort_by_index_and_keep_unindexed_tracks_last_stably() {
        let track = |id: &str, index_number| BaseItemDto {
            id: id.to_owned(),
            index_number,
            ..serde_json::from_str(r#"{"Id":"template","Name":"template"}"#)
                .expect("valid minimal item fixture")
        };
        let mut tracks = vec![
            track("unindexed-a", None),
            track("third", Some(3)),
            track("first", Some(1)),
            track("unindexed-b", None),
            track("second", Some(2)),
        ];

        sort_music_album_tracks(&mut tracks);

        assert_eq!(
            tracks
                .iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>(),
            ["first", "second", "third", "unindexed-a", "unindexed-b"]
        );
    }
}

async fn load_home_catalog(
    client: &JellyfinClient,
    session: &Session,
    kind: HomeCatalogKind,
) -> Result<HomeCatalogData, ApiError> {
    match kind {
        HomeCatalogKind::Libraries => client
            .user_views(session)
            .await
            .map(HomeCatalogData::Libraries),
        HomeCatalogKind::Collections => client
            .box_sets(session, HOME_ROW_LIMIT)
            .await
            .map(HomeCatalogData::Collections),
    }
}

fn item_query(
    start_index: u32,
    search_term: Option<String>,
    parent_id: Option<String>,
    include_item_types: Option<Vec<String>>,
    sort_by: ItemSortBy,
    sort_order: ItemSortOrder,
    filters: Option<Vec<ItemFilter>>,
) -> ItemQuery {
    ItemQuery {
        recursive: true,
        start_index,
        limit: CATALOG_PAGE_SIZE,
        parent_id,
        include_item_types,
        search_term,
        sort_by: Some(vec![sort_by]),
        sort_order: Some(vec![sort_order]),
        filters,
        genres: None,
        years: None,
        person_ids: None,
        ids: None,
        enable_total_record_count: Some(true),
        name_starts_with: None,
        name_less_than: None,
        studio_ids: None,
        ..ItemQuery::default()
    }
}

struct CachedPlaybackInfo {
    request_id: u64,
    item_id: String,
    is_audio: bool,
    response: PlaybackInfoResponse,
}

struct PlaybackPreparation<'a> {
    client: Option<&'a JellyfinClient>,
    session: Option<&'a Session>,
    cached: Option<&'a CachedPlaybackInfo>,
    negotiation_id: u64,
    item_id: &'a str,
    source_index: usize,
    play_method: PlayMethod,
    start_time_ticks: Option<i64>,
}

fn prepare_playback(preparation: PlaybackPreparation<'_>) -> Result<PreparedPlayback, String> {
    let PlaybackPreparation {
        client,
        session,
        cached,
        negotiation_id,
        item_id,
        source_index,
        play_method,
        start_time_ticks,
    } = preparation;
    let (Some(client), Some(session)) = (client, session) else {
        return Err("Connect to a Jellyfin server before starting playback.".into());
    };
    let Some(cached) = cached else {
        return Err("Request server playback options before starting a stream.".into());
    };
    if cached.request_id != negotiation_id || cached.item_id != item_id {
        return Err("Playback options expired; request them again.".into());
    }
    let Some(media_source) = cached.response.media_sources.get(source_index) else {
        return Err("The selected playback source is no longer available.".into());
    };
    let start_time_ticks = start_time_ticks.filter(|ticks| *ticks > 0);
    let options = PlaybackSourceRequestOptions {
        item_id,
        media_source,
        play_method,
        play_session_id: cached.response.play_session_id.as_deref(),
        audio_stream_index: media_source.default_audio_stream_index,
        subtitle_stream_index: media_source.default_subtitle_stream_index,
        start_time_ticks: start_time_ticks.map(JellyfinTicks::from_ticks),
    };
    let request = if cached.is_audio {
        client.build_audio_playback_source_request(session, options)
    } else {
        client.build_playback_source_request(session, options)
    }
    .map_err(|error| error.to_string())?;
    let allowed_origin = client.server_origin();
    let (url, relay) =
        start_relay(allowed_origin.as_str(), request).map_err(|error| error.to_string())?;
    Ok(PreparedPlayback {
        url,
        relay,
        start_time_ticks,
        report_metadata: PlaybackReportMetadata {
            item_id: item_id.to_owned(),
            media_source_id: media_source.id.clone(),
            live_stream_id: media_source.live_stream_id.clone(),
            live_tune_started_at: None,
            audio_stream_index: media_source.default_audio_stream_index,
            subtitle_stream_index: media_source.default_subtitle_stream_index,
            play_session_id: cached.response.play_session_id.clone(),
            play_method,
            can_seek: None,
            repeat_mode: None,
            playback_order: None,
            now_playing_queue: None,
        },
        series_id: None,
    })
}

async fn tune_live_tv(
    client: &JellyfinClient,
    session: &Session,
    channel_id: &str,
    device_profile: &DeviceProfile,
    tune_started_at: std::time::Instant,
) -> Result<PreparedPlayback, String> {
    let opened = client
        .open_live_stream(
            session,
            channel_id,
            device_profile,
            None,
            None,
            None,
            None,
            None,
            Some(true),
            Some(true),
            None,
            None,
        )
        .await
        .map_err(live_tune_error)?;
    let live_stream_id = opened
        .media_source
        .live_stream_id
        .as_deref()
        .unwrap_or_default()
        .to_owned();

    let result = async {
        let request = PlaybackInfoRequest {
            device_profile: Some(device_profile.clone()),
            live_stream_id: Some(live_stream_id.clone()),
            enable_direct_play: Some(true),
            enable_direct_stream: Some(true),
            enable_transcoding: Some(true),
            ..PlaybackInfoRequest::default()
        };
        let mut response = client
            .playback_info(session, channel_id, &request)
            .await
            .map_err(live_tune_error)?;
        let selection = kattri_client_core::playback_plan::select_playback_info_source_for_profile(
            &response,
            device_profile,
        )
        .ok_or_else(|| "The server did not offer a playable live source.".to_owned())?;
        let source_index = selection.source_index;
        let play_method = match selection.method {
            kattri_client_core::playback_plan::PlaybackMethod::DirectPlay => PlayMethod::DirectPlay,
            kattri_client_core::playback_plan::PlaybackMethod::DirectStream => {
                PlayMethod::DirectStream
            }
            kattri_client_core::playback_plan::PlaybackMethod::Transcode => PlayMethod::Transcode,
        };
        let source = response
            .media_sources
            .get_mut(source_index)
            .ok_or_else(|| "The server's live source is no longer available.".to_owned())?;
        if source.live_stream_id.is_none() {
            source.live_stream_id = Some(live_stream_id.clone());
        }
        let cached = CachedPlaybackInfo {
            request_id: 0,
            item_id: channel_id.to_owned(),
            is_audio: false,
            response,
        };
        let mut playback = prepare_playback(PlaybackPreparation {
            client: Some(client),
            session: Some(session),
            cached: Some(&cached),
            negotiation_id: 0,
            item_id: channel_id,
            source_index,
            play_method,
            start_time_ticks: None,
        })?;
        playback.report_metadata.live_stream_id = Some(live_stream_id.clone());
        playback.report_metadata.live_tune_started_at = Some(tune_started_at);
        if playback.report_metadata.play_session_id.is_none() {
            playback.report_metadata.play_session_id = opened.play_session_id.clone();
        }
        Ok(playback)
    }
    .await;

    if result.is_err() {
        let _ = client.close_live_stream(session, &live_stream_id).await;
    }
    result
}

fn live_tune_error(error: ApiError) -> String {
    if matches!(error, ApiError::Unavailable { .. }) {
        "All tuners are busy.".into()
    } else if matches!(error, ApiError::ReauthenticationRequired) {
        "The server requires you to sign in again.".into()
    } else {
        "Could not tune this channel.".into()
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_items_result(
    result: Result<ItemsResponse, ApiError>,
    request_id: u64,
    server_identity: ServerIdentity,
    user: UserDto,
    events: &WorkerSink,
    ctx: &egui::Context,
    client: &mut Option<JellyfinClient>,
    session: &mut Option<Session>,
    server: &mut Option<ServerIdentity>,
    playback_info_cache: &mut Option<CachedPlaybackInfo>,
) {
    match result {
        Ok(page) => {
            send_event(
                events,
                ctx,
                WorkerEvent::ItemsLoaded {
                    request_id,
                    server: server_identity,
                    user,
                    page,
                },
            )
            .await
        }
        Err(ApiError::ReauthenticationRequired) => {
            *session = None;
            *client = None;
            *server = None;
            *playback_info_cache = None;
            send_event(
                events,
                ctx,
                WorkerEvent::ReauthenticationRequired {
                    request_id,
                    message: "The server requires you to sign in again.".into(),
                },
            )
            .await;
        }
        Err(error) => {
            let unsupported_filter = match &error {
                ApiError::UnsupportedItemFilter(filter) => Some(*filter),
                _ => None,
            };
            send_event(
                events,
                ctx,
                WorkerEvent::ItemsFailed {
                    request_id,
                    unsupported_filter,
                    error_class: error.classify(),
                    retry_after: error.retry_after(),
                },
            )
            .await;
        }
    }
}

fn server_identity(info: &PublicSystemInfo) -> ServerIdentity {
    ServerIdentity {
        name: info
            .server_name
            .clone()
            .or_else(|| info.product_name.clone())
            .unwrap_or_else(|| "Jellyfin server".into()),
        version: info.version.clone(),
        id: info.id.clone().filter(|id| !id.is_empty()),
    }
}

/// Largest remote session message the worker parses. Real command envelopes are far smaller;
/// the bound keeps a broken or hostile peer from queueing unbounded text.
const MAX_REMOTE_MESSAGE_BYTES: usize = 16 * 1024;

/// Parse one inbound session message into the player action it asks for. `None` means the
/// message was refused or is not a command this client runs. Nothing is guessed, and the
/// payload is not logged.
pub(crate) fn remote_action_event(message: &str, max_volume_percent: u32) -> Option<WorkerEvent> {
    if message.len() > MAX_REMOTE_MESSAGE_BYTES {
        tracing::warn!("remote session message refused: larger than the size bound");
        return None;
    }
    match kattri_client_core::remote::map_session_message(message, max_volume_percent) {
        Ok(action) => Some(WorkerEvent::RemoteAction { action }),
        Err(error) => {
            tracing::warn!(?error, "remote session command not executed");
            None
        }
    }
}

#[cfg(test)]
mod remote_action_tests {
    use super::{MAX_REMOTE_MESSAGE_BYTES, WorkerEvent, remote_action_event};
    use kattri_client_core::remote::LocalAction;

    #[test]
    fn playstate_pause_maps_to_a_pause_event() {
        let message = r#"{"MessageType":"Playstate","Data":{"Command":"Pause"}}"#;
        assert!(matches!(
            remote_action_event(message, 100),
            Some(WorkerEvent::RemoteAction {
                action: LocalAction::Pause
            })
        ));
    }

    #[test]
    fn volume_is_clamped_to_the_local_maximum() {
        let message = r#"{"MessageType":"GeneralCommand","Data":{"Name":"SetVolume","Arguments":{"Volume":"60"}}}"#;
        assert!(matches!(
            remote_action_event(message, 50),
            Some(WorkerEvent::RemoteAction {
                action: LocalAction::SetVolume { percent: 50 }
            })
        ));
    }

    #[test]
    fn unsupported_invalid_and_oversized_messages_produce_no_event() {
        let rewind = r#"{"MessageType":"Playstate","Data":{"Command":"Rewind"}}"#;
        assert!(remote_action_event(rewind, 100).is_none());
        assert!(remote_action_event("not json", 100).is_none());
        let oversized = format!(
            r#"{{"MessageType":"Sessions","Data":"{}"}}"#,
            "x".repeat(MAX_REMOTE_MESSAGE_BYTES)
        );
        assert!(remote_action_event(&oversized, 100).is_none());
    }
}

async fn send_event(events: &WorkerSink, ctx: &egui::Context, event: WorkerEvent) {
    if matches!(event, WorkerEvent::ReauthenticationRequired { .. }) {
        events.authentication_expired.store(true, Ordering::Release);
    }
    ctx.request_repaint();
    let _ = events.sender.send(event).await;
    ctx.request_repaint();
}

#[cfg(test)]
mod item_query_tests {
    use super::item_query;
    use jellyfin_api::{ItemFilter, ItemQuery, ItemSortBy, ItemSortOrder};

    #[test]
    fn item_query_forwards_server_category_filter_and_pagination() {
        let query = item_query(
            200,
            None,
            Some("series-id".into()),
            Some(vec!["Series".into(), "Episode".into()]),
            ItemSortBy::SortName,
            ItemSortOrder::Ascending,
            Some(vec![ItemFilter::IsUnplayed, ItemFilter::IsFavorite]),
        );

        assert_eq!(
            query,
            ItemQuery {
                recursive: true,
                start_index: 200,
                limit: 50,
                parent_id: Some("series-id".into()),
                include_item_types: Some(vec!["Series".into(), "Episode".into()]),
                search_term: None,
                sort_by: Some(vec![ItemSortBy::SortName]),
                sort_order: Some(vec![ItemSortOrder::Ascending]),
                filters: Some(vec![ItemFilter::IsUnplayed, ItemFilter::IsFavorite]),
                genres: None,
                years: None,
                person_ids: None,
                ids: None,
                enable_total_record_count: Some(true),
                name_starts_with: None,
                name_less_than: None,
                studio_ids: None,
                ..ItemQuery::default()
            }
        );
    }

    #[test]
    fn global_search_does_not_inherit_a_category_filter() {
        let query = item_query(
            0,
            Some("space".into()),
            None,
            None,
            ItemSortBy::DateCreated,
            ItemSortOrder::Descending,
            None,
        );

        assert_eq!(query.include_item_types, None);
        assert_eq!(query.search_term.as_deref(), Some("space"));
    }
}

#[cfg(test)]
mod artwork_decode_tests {
    use super::decode_primary_artwork;
    use image::{
        ExtendedColorType, ImageEncoder,
        codecs::{jpeg::JpegEncoder, png::PngEncoder},
    };

    #[test]
    fn decodes_server_jpeg_to_bounded_rgba_pixels() {
        let rgb = [32_u8, 96, 160, 220, 180, 40];
        let mut encoded = Vec::new();
        JpegEncoder::new(&mut encoded)
            .encode(&rgb, 2, 1, ExtendedColorType::Rgb8)
            .expect("encode small JPEG fixture");

        let (width, height, rgba) = decode_primary_artwork(encoded).expect("decode JPEG");
        assert_eq!((width, height), (2, 1));
        assert_eq!(rgba.len(), 8);
    }

    #[test]
    fn decodes_png_channel_logo_to_bounded_rgba_pixels() {
        let pixels = [16_u8, 80, 144, 255, 200, 120, 40, 255];
        let mut encoded = Vec::new();
        PngEncoder::new(&mut encoded)
            .write_image(&pixels, 2, 1, ExtendedColorType::Rgba8)
            .expect("encode small PNG fixture");

        let (width, height, rgba) = decode_primary_artwork(encoded).expect("decode PNG logo");
        assert_eq!((width, height), (2, 1));
        assert_eq!(rgba.len(), 8);
    }

    #[test]
    fn rejects_unsupported_primary_image_payloads() {
        assert!(decode_primary_artwork(b"not an image".to_vec()).is_err());
    }
}

#[cfg(test)]
mod person_artwork_decode_tests {
    use super::decode_person_artwork;
    use image::{
        ExtendedColorType, ImageEncoder,
        codecs::{jpeg::JpegEncoder, png::PngEncoder},
    };

    #[test]
    fn decodes_valid_jpeg_to_bounded_rgba_pixels() {
        let pixels = [16_u8, 80, 144, 200, 120, 40];
        let mut encoded = Vec::new();
        JpegEncoder::new(&mut encoded)
            .encode(&pixels, 2, 1, ExtendedColorType::Rgb8)
            .expect("encode small JPEG fixture");

        let (width, height, rgba) = decode_person_artwork(encoded).expect("decode JPEG");
        assert_eq!((width, height), (2, 1));
        assert_eq!(rgba.len(), 8);
    }

    #[test]
    fn decodes_valid_png_to_bounded_rgba_pixels() {
        let pixels = [16_u8, 80, 144, 255, 200, 120, 40, 255];
        let mut encoded = Vec::new();
        PngEncoder::new(&mut encoded)
            .write_image(&pixels, 2, 1, ExtendedColorType::Rgba8)
            .expect("encode small PNG fixture");

        let (width, height, rgba) = decode_person_artwork(encoded).expect("decode PNG");
        assert_eq!((width, height), (2, 1));
        assert_eq!(rgba.len(), 8);
    }

    #[test]
    fn rejects_person_artwork_exceeding_dimension_limits() {
        let pixels = vec![255_u8; 514 * 4];
        let mut encoded = Vec::new();
        PngEncoder::new(&mut encoded)
            .write_image(&pixels, 514, 1, ExtendedColorType::Rgba8)
            .expect("encode oversized PNG fixture");

        assert!(decode_person_artwork(encoded).is_err());
    }
}

#[cfg(test)]
mod live_tv_logo_decode_tests {
    use super::decode_live_tv_logo_image;
    use image::{ExtendedColorType, ImageEncoder, codecs::png::PngEncoder};

    #[test]
    fn decodes_logo_within_128_pixel_and_one_mib_limits() {
        let pixels = vec![255_u8; 128 * 128 * 4];
        let mut encoded = Vec::new();
        PngEncoder::new(&mut encoded)
            .write_image(&pixels, 128, 128, ExtendedColorType::Rgba8)
            .expect("encode bounded PNG fixture");

        let (width, height, rgba) =
            decode_live_tv_logo_image(encoded).expect("decode bounded logo");
        assert_eq!((width, height), (128, 128));
        assert_eq!(rgba.len(), 128 * 128 * 4);
        assert!(rgba.len() <= 1024 * 1024);
    }

    #[test]
    fn rejects_logo_dimensions_above_128_pixels() {
        let pixels = vec![255_u8; 129 * 4];
        let mut encoded = Vec::new();
        PngEncoder::new(&mut encoded)
            .write_image(&pixels, 129, 1, ExtendedColorType::Rgba8)
            .expect("encode oversized PNG fixture");

        assert!(decode_live_tv_logo_image(encoded).is_err());
    }
}

#[cfg(test)]
mod photo_image_worker_tests {
    use super::{ApiWorker, CommandQueue, WorkerCommand, decode_photo_image_bytes};
    use image::{ExtendedColorType, ImageEncoder, codecs::png::PngEncoder};
    use std::sync::{Arc, Mutex};

    fn worker_with_command_receiver() -> (ApiWorker, Arc<CommandQueue>) {
        let commands = Arc::new(CommandQueue::default());
        let command_rx = commands.clone();
        let (_events, event_rx) = tokio::sync::mpsc::channel(32);
        (
            ApiWorker {
                commands,
                events: Mutex::new(event_rx),
                thread: None,
                #[cfg(feature = "photo-counters")]
                photo_decode_counters: Default::default(),
            },
            command_rx,
        )
    }

    fn png(width: u32, height: u32) -> Vec<u8> {
        let pixels = vec![128_u8; width as usize * height as usize * 4];
        let mut encoded = Vec::new();
        PngEncoder::new(&mut encoded)
            .write_image(&pixels, width, height, ExtendedColorType::Rgba8)
            .expect("encode PNG fixture");
        encoded
    }

    #[test]
    fn thumbnail_and_full_size_commands_preserve_request_metadata() {
        let (worker, commands) = worker_with_command_receiver();
        assert_eq!(
            worker.load_photo_thumbnail(71, "photo-1".into(), Some("thumb".into()), 12, true),
            Ok(())
        );
        assert!(matches!(
            commands.recv(),
            Ok(WorkerCommand::LoadPhotoThumbnail {
                request_id: 71,
                item_id,
                image_tag: Some(tag),
                photo_index: 12,
                visible: true,
            }) if item_id == "photo-1" && tag == "thumb"
        ));
        assert_eq!(
            worker.load_photo_full_size(72, "photo-2".into(), None, 13, false),
            Ok(())
        );
        assert!(matches!(
            commands.recv(),
            Ok(WorkerCommand::LoadPhotoFullSize {
                request_id: 72,
                item_id,
                image_tag: None,
                photo_index: 13,
                visible: false,
            }) if item_id == "photo-2"
        ));
    }

    #[test]
    fn thumbnail_decode_is_bounded_and_full_size_allows_eight_thousand_pixels() {
        let (width, height, rgba) =
            decode_photo_image_bytes(png(512, 256), false).expect("decode thumbnail");
        assert_eq!((width, height), (512, 256));
        assert_eq!(rgba.len(), 512 * 256 * 4);

        let (width, height, rgba) =
            decode_photo_image_bytes(png(1, 8_000), true).expect("decode full-size image");
        assert_eq!((width, height), (1, 8_000));
        assert_eq!(rgba.len(), 8_000 * 4);
        assert!(decode_photo_image_bytes(png(513, 1), false).is_err());
        assert!(decode_photo_image_bytes(png(8_001, 1), true).is_err());
        assert!(decode_photo_image_bytes(b"not an image".to_vec(), true).is_err());
    }
}

#[cfg(test)]
mod series_command_tests {
    use super::WorkerCommand;
    use super::{ApiWorker, CommandQueue};
    use std::sync::{Arc, Mutex};

    fn worker_with_command_receiver() -> (ApiWorker, Arc<CommandQueue>) {
        let commands = Arc::new(CommandQueue::default());
        let command_rx = commands.clone();
        let (_events, event_rx) = tokio::sync::mpsc::channel(32);
        (
            ApiWorker {
                commands,
                events: Mutex::new(event_rx),
                thread: None,
                #[cfg(feature = "photo-counters")]
                photo_decode_counters: Default::default(),
            },
            command_rx,
        )
    }

    #[test]
    fn list_media_segments_enqueues_the_item_request() {
        let (worker, commands) = worker_with_command_receiver();

        assert_eq!(worker.list_media_segments(51, "episode-4".into()), Ok(()));
        assert!(matches!(
            commands.recv(),
            Ok(WorkerCommand::ListMediaSegments {
                request_id: 51,
                item_id,
            }) if item_id == "episode-4"
        ));
    }

    #[test]
    fn list_seasons_enqueues_the_series_request() {
        let (worker, commands) = worker_with_command_receiver();

        assert_eq!(worker.list_seasons(41, "series-7".into()), Ok(()));
        assert!(matches!(
            commands.recv(),
            Ok(WorkerCommand::ListSeasons {
                request_id: 41,
                series_id,
            }) if series_id == "series-7"
        ));
    }

    #[test]
    fn list_episodes_enqueues_both_parent_ids() {
        let (worker, commands) = worker_with_command_receiver();

        assert_eq!(
            worker.list_episodes(42, "series-7".into(), "season-3".into()),
            Ok(())
        );
        assert!(matches!(
            commands.recv(),
            Ok(WorkerCommand::ListEpisodes {
                request_id: 42,
                series_id,
                season_id,
            }) if series_id == "series-7" && season_id == "season-3"
        ));
    }
}

#[cfg(test)]
mod people_command_tests {
    use super::WorkerCommand;
    use super::{ApiWorker, CommandQueue};
    use std::sync::{Arc, Mutex};

    fn worker_with_command_receiver() -> (ApiWorker, Arc<CommandQueue>) {
        let commands = Arc::new(CommandQueue::default());
        let command_rx = commands.clone();
        let (_events, event_rx) = tokio::sync::mpsc::channel(32);
        (
            ApiWorker {
                commands,
                events: Mutex::new(event_rx),
                thread: None,
                #[cfg(feature = "photo-counters")]
                photo_decode_counters: Default::default(),
            },
            command_rx,
        )
    }

    #[test]
    fn set_played_enqueues_item_and_played_flag() {
        let (worker, commands) = worker_with_command_receiver();

        assert_eq!(worker.set_played(60, "item-7".into(), true), Ok(()));
        assert!(matches!(
            commands.recv(),
            Ok(WorkerCommand::SetPlayed {
                request_id: 60,
                item_id,
                played: true,
            }) if item_id == "item-7"
        ));
        assert_eq!(worker.set_played(61, "item-7".into(), false), Ok(()));
        assert!(matches!(
            commands.recv(),
            Ok(WorkerCommand::SetPlayed {
                request_id: 61,
                played: false,
                ..
            })
        ));
    }

    #[test]
    fn list_similar_enqueues_item_and_limit() {
        let (worker, commands) = worker_with_command_receiver();

        assert_eq!(worker.list_similar(51, "item-9".into(), 12), Ok(()));
        assert!(matches!(
            commands.recv(),
            Ok(WorkerCommand::ListSimilar {
                request_id: 51,
                item_id,
                limit: 12,
            }) if item_id == "item-9"
        ));
    }

    #[test]
    fn list_person_items_enqueues_person_and_limit() {
        let (worker, commands) = worker_with_command_receiver();

        assert_eq!(worker.list_person_items(52, "person-4".into(), 30), Ok(()));
        assert!(matches!(
            commands.recv(),
            Ok(WorkerCommand::ListPersonItems {
                request_id: 52,
                person_id,
                limit: 30,
            }) if person_id == "person-4"
        ));
    }

    #[test]
    fn get_person_artwork_enqueues_name_and_tag() {
        let (worker, commands) = worker_with_command_receiver();

        assert_eq!(
            worker.get_person_artwork(53, "Performer Name".into(), Some("tag-1".into()),),
            Ok(())
        );
        assert!(matches!(
            commands.recv(),
            Ok(WorkerCommand::GetPersonArtwork {
                request_id: 53,
                name,
                tag: Some(tag),
            }) if name == "Performer Name" && tag == "tag-1"
        ));
    }
}

#[cfg(test)]
mod playback_preparation_tests {
    use super::{CachedPlaybackInfo, PlaybackPreparation, prepare_playback};
    use jellyfin_api::{
        ClientConfig, ItemQuery, JellyfinClient, PlayMethod, PlaybackInfoRequest,
        baseline_device_profile,
    };
    use kattri_client_core::playback_plan::{
        PlaybackMethod, select_playback_info_source_for_profile,
    };
    use mock_jellyfin::{MockConfig, spawn};
    use std::net::SocketAddr;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpStream,
    };

    #[tokio::test]
    async fn negotiated_play_methods_are_handed_to_mpv_only_through_loopback_relay() {
        let server = spawn(MockConfig::default())
            .await
            .expect("start mock Jellyfin");
        let config = ClientConfig::new(
            &server.base_url(),
            "Kattri test",
            "test device",
            "device-id",
            "test-version",
        )
        .expect("valid loopback config");
        let client = JellyfinClient::new(config).expect("create client");
        let session = client
            .authenticate("demo", "demo")
            .await
            .expect("authenticate to mock server");
        let items = client
            .list_items(
                &session,
                &ItemQuery {
                    include_item_types: Some(vec!["Movie".into()]),
                    ..ItemQuery::default()
                },
            )
            .await
            .expect("list mock movies");
        let item = items.items.first().expect("mock movie exists");
        let response = client
            .playback_info(&session, &item.id, &PlaybackInfoRequest::default())
            .await
            .expect("negotiate playback");
        let cached = CachedPlaybackInfo {
            request_id: 12,
            item_id: item.id.clone(),
            is_audio: false,
            response,
        };

        for play_method in [
            PlayMethod::DirectPlay,
            PlayMethod::DirectStream,
            PlayMethod::Transcode,
        ] {
            let playback = prepare_playback(PlaybackPreparation {
                client: Some(&client),
                session: Some(&session),
                cached: Some(&cached),
                negotiation_id: 12,
                item_id: &item.id,
                source_index: 0,
                play_method,
                start_time_ticks: Some(123_456_789),
            })
            .expect("build the negotiated media source relay");
            let (url, relay, metadata, start_time_ticks) = playback.into_parts();
            assert!(url.as_str().starts_with("http://127.0.0.1:"));
            assert!(url.as_str().contains("/r/"));
            assert_eq!(metadata.play_method, play_method);
            assert_eq!(start_time_ticks, Some(123_456_789));
            relay.shutdown().expect("stop relay");
        }

        for start_time_ticks in [Some(0), Some(-1), None] {
            let playback = prepare_playback(PlaybackPreparation {
                client: Some(&client),
                session: Some(&session),
                cached: Some(&cached),
                negotiation_id: 12,
                item_id: &item.id,
                source_index: 0,
                play_method: PlayMethod::DirectPlay,
                start_time_ticks,
            })
            .expect("build playback with absent or invalid resume position");
            let (_, relay, _, resume_ticks) = playback.into_parts();
            assert_eq!(resume_ticks, None);
            relay.shutdown().expect("stop relay");
        }

        server.shutdown().await;
    }

    #[tokio::test]
    async fn audio_track_uses_negotiated_profile_and_audio_route_through_relay() {
        let server = spawn(MockConfig::default())
            .await
            .expect("start mock Jellyfin");
        let client = JellyfinClient::new(
            ClientConfig::new(
                &server.base_url(),
                "Kattri test",
                "test device",
                "device-id",
                "test-version",
            )
            .expect("valid loopback config"),
        )
        .expect("create client");
        let session = client
            .authenticate("demo", "demo")
            .await
            .expect("authenticate to mock server");
        let items = client
            .list_items(
                &session,
                &ItemQuery {
                    include_item_types: Some(vec!["Audio".into()]),
                    ..ItemQuery::default()
                },
            )
            .await
            .expect("list mock audio tracks");
        let item = items.items.first().expect("mock audio track exists");
        let response = client
            .playback_info(&session, &item.id, &PlaybackInfoRequest::default())
            .await
            .expect("negotiate audio playback");
        let selection =
            select_playback_info_source_for_profile(&response, &baseline_device_profile())
                .expect("the mock mp3 source matches the software audio profile");
        assert_eq!(selection.method, PlaybackMethod::DirectPlay);
        let source_index = selection.source_index;
        let cached = CachedPlaybackInfo {
            request_id: 22,
            item_id: item.id.clone(),
            is_audio: true,
            response,
        };
        let playback = prepare_playback(PlaybackPreparation {
            client: Some(&client),
            session: Some(&session),
            cached: Some(&cached),
            negotiation_id: 22,
            item_id: &item.id,
            source_index,
            play_method: PlayMethod::DirectPlay,
            start_time_ticks: None,
        })
        .expect("prepare audio stream through credential-isolating relay");
        let (url, relay, _, _) = playback.into_parts();
        let local_url = url.as_str();
        let address_path = local_url
            .strip_prefix("http://")
            .and_then(|rest| rest.split_once('/'))
            .expect("relay URL is local HTTP with a path");
        let address: SocketAddr = address_path.0.parse().expect("loopback relay address");
        assert!(address.ip().is_loopback());
        let mut stream = TcpStream::connect(address)
            .await
            .expect("connect to the loopback relay");
        let request = format!(
            "GET /{} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
            address_path.1, address
        );
        stream
            .write_all(request.as_bytes())
            .await
            .expect("send the local relay request");
        let mut bytes = Vec::new();
        stream
            .read_to_end(&mut bytes)
            .await
            .expect("read audio response through relay");
        let response_text = String::from_utf8_lossy(&bytes).to_ascii_lowercase();
        assert!(response_text.starts_with("http/1.1 200"));
        assert!(response_text.contains("content-type: audio/mpeg"));
        relay.shutdown().expect("stop relay");
        server.shutdown().await;
    }
}

#[cfg(test)]
mod live_tv_worker_tests {
    use super::{WorkerCommand, WorkerEvent, WorkerSink, WorkerState, execute_command};
    use jellyfin_api::{
        ClientConfig, DeviceProfile, JellyfinClient, PlayMethod, baseline_device_profile,
    };
    use serde_json::{Value, json};
    use std::{
        net::SocketAddr,
        sync::{Arc, Mutex},
    };
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::{TcpListener, TcpStream},
        task::JoinHandle,
    };

    const CHANNEL_ID: &str = "00000000000000000000000000000001";
    const ACCESS_TOKEN: &str = "sentinel-access-token";
    const LIVE_STREAM_ID: &str = "sentinel-live-stream-id";
    const PRIVATE_VALUE: &str = "sentinel-private-value";

    struct CapturedRequest {
        path: String,
        body: Value,
    }

    async fn spawn_fixture(
        playback_status: u16,
    ) -> (String, Arc<Mutex<Vec<CapturedRequest>>>, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind in-process Jellyfin fixture");
        let address = listener.local_addr().expect("read fixture address");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = requests.clone();
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let captured = captured.clone();
                tokio::spawn(async move {
                    serve_request(stream, captured, playback_status).await;
                });
            }
        });
        (format!("http://{address}"), requests, task)
    }

    async fn serve_request(
        mut stream: TcpStream,
        requests: Arc<Mutex<Vec<CapturedRequest>>>,
        playback_status: u16,
    ) {
        let mut bytes = Vec::new();
        let mut chunk = [0_u8; 4096];
        let header_end = loop {
            let Ok(read) = stream.read(&mut chunk).await else {
                return;
            };
            if read == 0 {
                return;
            }
            bytes.extend_from_slice(&chunk[..read]);
            if let Some(index) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                break index + 4;
            }
        };
        let headers = String::from_utf8_lossy(&bytes[..header_end]).into_owned();
        let content_length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())
                    .flatten()
            })
            .unwrap_or(0);
        while bytes.len() < header_end.saturating_add(content_length) {
            let Ok(read) = stream.read(&mut chunk).await else {
                return;
            };
            if read == 0 {
                return;
            }
            bytes.extend_from_slice(&chunk[..read]);
        }
        let request_line = headers.lines().next().unwrap_or_default();
        let path = request_line
            .split_whitespace()
            .nth(1)
            .unwrap_or_default()
            .to_owned();
        let body = serde_json::from_slice(&bytes[header_end..header_end + content_length])
            .unwrap_or(Value::Null);
        if let Ok(mut requests) = requests.lock() {
            requests.push(CapturedRequest {
                path: path.clone(),
                body,
            });
        }

        let (status, body) = match path.split('?').next().unwrap_or_default() {
            "/Users/AuthenticateByName" => (
                200,
                json!({
                    "User": {"Id": "test-user", "Name": "test"},
                    "AccessToken": ACCESS_TOKEN,
                }),
            ),
            "/LiveStreams/Open" => (
                200,
                json!({
                    "MediaSource": {
                        "Protocol": "File", "Id": "opened-source", "Container": "mp4",
                        "SupportsDirectPlay": true, "SupportsDirectStream": true,
                        "SupportsTranscoding": false, "LiveStreamId": LIVE_STREAM_ID,
                        "DefaultAudioStreamIndex": 1,
                    },
                    "PlaySessionId": "opened-play-session",
                }),
            ),
            path if path.ends_with("/PlaybackInfo") => (
                playback_status,
                json!({
                    "MediaSources": [{
                        "Protocol": "File", "Id": "negotiated-source", "Container": "mp4",
                        "SupportsDirectPlay": false, "SupportsDirectStream": false,
                        "SupportsTranscoding": true,
                        "TranscodingUrl": "/Videos/live/master.m3u8?PrivateFixtureValue=sentinel-private-value",
                        "TranscodingSubProtocol": "hls", "TranscodingContainer": "ts",
                        "MediaStreams": [
                            {"Type": "Video", "Codec": "h264", "Width": 1920, "Height": 1080},
                            {"Type": "Audio", "Codec": "aac", "Channels": 2},
                        ],
                        "DefaultAudioStreamIndex": 1,
                    }],
                    "PlaySessionId": "negotiated-play-session",
                    "PrivateFixtureValue": PRIVATE_VALUE,
                }),
            ),
            path if path.split('?').next() == Some("/Videos/live/master.m3u8") => {
                (200, Value::Null)
            }
            "/LiveStreams/Close" => (204, Value::Null),
            "/Sessions/Playing/Stopped" => (500, json!({"Message": PRIVATE_VALUE})),
            _ => (404, json!({"Message": "not found"})),
        };
        let manifest_path = path.split('?').next().unwrap_or_default();
        let (content_type, response_body) = if status == 204 {
            ("application/json", Vec::new())
        } else if manifest_path == "/Videos/live/master.m3u8" {
            (
                "application/vnd.apple.mpegurl",
                b"#EXTM3U\n#EXT-X-TARGETDURATION:6\n#EXTINF:6,\nsegment.ts\n".to_vec(),
            )
        } else {
            (
                "application/json",
                serde_json::to_vec(&body).expect("serialize fixture response"),
            )
        };
        let reason = match status {
            200 => "OK",
            204 => "No Content",
            503 => "Service Unavailable",
            500 => "Internal Server Error",
            _ => "Not Found",
        };
        let response = format!(
            "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            response_body.len()
        );
        let _ = stream.write_all(response.as_bytes()).await;
        let _ = stream.write_all(&response_body).await;
    }

    async fn authenticated_client(base_url: &str) -> (JellyfinClient, jellyfin_api::Session) {
        let client = JellyfinClient::new(
            ClientConfig::new(base_url, "worker-test", "fixture", "device", "test")
                .expect("valid fixture client config"),
        )
        .expect("construct fixture client");
        let session = client
            .authenticate("test", "test")
            .await
            .expect("authenticate to in-process fixture");
        (client, session)
    }

    fn playback_command(profile: DeviceProfile) -> WorkerCommand {
        WorkerCommand::PreparePlayback {
            request_id: 77,
            negotiation_id: 0,
            item_id: CHANNEL_ID.to_owned(),
            source_index: 0,
            play_method: PlayMethod::DirectPlay,
            start_time_ticks: None,
            live_device_profile: Some(Box::new(profile)),
            live_tune_started_at: Some(std::time::Instant::now()),
        }
    }

    #[tokio::test]
    async fn tune_command_negotiates_selects_relays_and_emits_channel_metadata() {
        let (base_url, requests, fixture) = spawn_fixture(200).await;
        let (client, session) = authenticated_client(&base_url).await;
        assert!(!format!("{session:?}").contains(ACCESS_TOKEN));
        let mut state = WorkerState {
            client: Some(client),
            session: Some(session),
            ..WorkerState::default()
        };
        let (event_sender, mut event_receiver) = tokio::sync::mpsc::channel(4);
        let events = WorkerSink::for_test(event_sender);
        execute_command(
            playback_command(baseline_device_profile()),
            &mut state,
            &events,
            "device",
        )
        .await;
        let event = event_receiver.recv().await.expect("prepared event");
        let WorkerEvent::PlaybackPrepared {
            request_id,
            item_id,
            playback,
        } = event
        else {
            if let WorkerEvent::PlaybackPrepareFailed { message, .. } = event {
                panic!("live tune failed: {message}");
            }
            panic!("expected PlaybackPrepared event");
        };
        assert_eq!(request_id, 77);
        assert_eq!(item_id, CHANNEL_ID);
        let (url, relay, metadata, _) = playback.into_parts();
        assert!(url.as_str().starts_with("http://127.0.0.1:"));
        assert!(!url.as_str().contains(ACCESS_TOKEN));
        assert!(!url.as_str().contains(PRIVATE_VALUE));
        let host = url
            .as_str()
            .strip_prefix("http://")
            .and_then(|rest| rest.split_once('/'))
            .map(|(host, _)| host)
            .expect("relay URL host");
        let address: SocketAddr = host.parse().expect("relay URL is a socket address");
        assert!(address.ip().is_loopback());
        let relay_path = url
            .as_str()
            .strip_prefix(&format!("http://{host}"))
            .expect("relay URL path follows its loopback authority");
        let mut relay_request = TcpStream::connect(address)
            .await
            .expect("connect to the loopback playback relay");
        relay_request
            .write_all(
                format!("GET {relay_path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n")
                    .as_bytes(),
            )
            .await
            .expect("request the live HLS manifest through the relay");
        let mut relay_response = Vec::new();
        relay_request
            .read_to_end(&mut relay_response)
            .await
            .expect("read the bounded mock HLS manifest");
        let body_start = relay_response
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .map(|index| index + 4)
            .expect("relay response headers");
        let manifest = String::from_utf8_lossy(&relay_response[body_start..]);
        assert!(manifest.contains("#EXTM3U"));
        assert!(manifest.contains("#EXTINF:6,"));
        assert!(!manifest.contains("#EXT-X-ENDLIST"));
        assert!(!manifest.contains(PRIVATE_VALUE));
        assert_eq!(metadata.item_id, CHANNEL_ID);
        assert_eq!(
            metadata.media_source_id.as_deref(),
            Some("negotiated-source")
        );
        assert_eq!(metadata.live_stream_id.as_deref(), Some(LIVE_STREAM_ID));
        assert_eq!(
            metadata.play_session_id.as_deref(),
            Some("negotiated-play-session")
        );
        assert_eq!(metadata.play_method, PlayMethod::Transcode);

        let captured = requests.lock().expect("fixture request lock");
        assert_eq!(captured.len(), 4);
        assert_eq!(captured[0].path, "/Users/AuthenticateByName");
        assert_eq!(captured[1].path, "/LiveStreams/Open");
        assert_eq!(captured[1].body["ItemId"], CHANNEL_ID);
        assert_eq!(
            captured[1].body["DeviceProfile"]["Name"],
            "Jellyfin Kattri (libmpv)"
        );
        assert_eq!(
            captured[2].path,
            format!("/Items/{CHANNEL_ID}/PlaybackInfo")
        );
        assert_eq!(captured[2].body["LiveStreamId"], LIVE_STREAM_ID);
        assert!(captured[2].body["DeviceProfile"].is_object());
        assert!(captured[3].path.starts_with("/Videos/live/master.m3u8"));
        drop(captured);
        relay.shutdown().expect("stop loopback relay");
        fixture.abort();
    }

    #[tokio::test]
    async fn failed_live_negotiation_closes_opened_stream_and_redacts_errors() {
        let (base_url, requests, fixture) = spawn_fixture(503).await;
        let (client, session) = authenticated_client(&base_url).await;
        let mut state = WorkerState {
            client: Some(client),
            session: Some(session),
            ..WorkerState::default()
        };
        let (event_sender, mut event_receiver) = tokio::sync::mpsc::channel(4);
        let events = WorkerSink::for_test(event_sender);
        execute_command(
            playback_command(baseline_device_profile()),
            &mut state,
            &events,
            "device",
        )
        .await;
        let event = event_receiver.recv().await.expect("failure event");
        let WorkerEvent::PlaybackPrepareFailed {
            request_id,
            message,
        } = event
        else {
            panic!("expected playback failure event");
        };
        assert_eq!(request_id, 77);
        assert_eq!(message, "All tuners are busy.");
        assert!(!message.contains(ACCESS_TOKEN));
        assert!(!message.contains(PRIVATE_VALUE));
        let captured = requests.lock().expect("fixture request lock");
        assert_eq!(captured.len(), 4);
        assert_eq!(captured[1].path, "/LiveStreams/Open");
        assert_eq!(
            captured[2].path,
            format!("/Items/{CHANNEL_ID}/PlaybackInfo")
        );
        assert_eq!(
            captured[3].path,
            format!("/LiveStreams/Close?liveStreamId={LIVE_STREAM_ID}")
        );
        let debug_paths = format!(
            "{:?}",
            captured
                .iter()
                .map(|request| &request.path)
                .collect::<Vec<_>>()
        );
        assert!(!debug_paths.contains(ACCESS_TOKEN));
        assert!(!debug_paths.contains(PRIVATE_VALUE));
        fixture.abort();
    }

    #[tokio::test]
    async fn stopped_live_report_closes_stream_even_when_report_fails() {
        let (base_url, requests, fixture) = spawn_fixture(200).await;
        let (client, session) = authenticated_client(&base_url).await;
        let mut state = WorkerState {
            client: Some(client),
            session: Some(session),
            ..WorkerState::default()
        };
        let (event_sender, mut event_receiver) = tokio::sync::mpsc::channel(4);
        let events = WorkerSink::for_test(event_sender);
        let mut stop = jellyfin_api::session_reporting::PlaybackStopInfo::new(CHANNEL_ID);
        stop.live_stream_id = Some(LIVE_STREAM_ID.to_owned());
        let command = WorkerCommand::ReportPlayback {
            request_id: 88,
            action: kattri_client_core::playback_reporting::PlaybackReportAction::Stopped(stop),
        };
        execute_command(command, &mut state, &events, "device").await;
        let event = event_receiver.recv().await.expect("failed report event");
        assert!(matches!(
            event,
            WorkerEvent::PlaybackReportFailed { request_id: 88, message }
                if message.contains("500") && !message.contains(PRIVATE_VALUE)
        ));
        let captured = requests.lock().expect("fixture request lock");
        assert_eq!(captured.len(), 3);
        assert_eq!(captured[1].path, "/Sessions/Playing/Stopped");
        assert_eq!(
            captured[2].path,
            format!("/LiveStreams/Close?liveStreamId={LIVE_STREAM_ID}")
        );
        fixture.abort();
    }
}

#[cfg(test)]
mod playback_reporting_worker_tests {
    use super::{
        ApiWorker, CommandQueue, PlaylistMutationKind, SavedSignInRequest, WorkerEvent, WorkerSink,
        item_query, run_worker,
    };
    use jellyfin_api::{ItemQuery, JellyfinTicks, PlayMethod};
    use kattri_client_core::playback_reporting::{
        PlaybackReportAction, PlaybackReportInput, PlaybackReportMetadata, PlaybackReportScheduler,
        PlaybackReportSessionId,
    };
    use kattri_client_core::quick_connect_poll::PollOutcome;
    use mock_jellyfin::{MockConfig, RunningServer, spawn};
    use serde_json::Value;
    use std::{
        sync::{Arc, Mutex},
        thread,
        time::Duration,
    };
    use zeroize::Zeroizing;

    #[tokio::test]
    async fn quick_connect_availability_probe_reports_enabled_and_disabled_servers() {
        for (request_id, disabled, expected) in [(120, false, true), (121, true, false)] {
            let server = spawn(MockConfig {
                quick_connect_disabled: disabled,
                ..MockConfig::default()
            })
            .await
            .expect("start Quick Connect mock");
            let worker = test_worker();
            worker
                .check_quick_connect(request_id, server.base_url(), false)
                .expect("queue availability probe");
            let event = wait_for_event(&worker, |event| {
                matches!(
                    event,
                    WorkerEvent::QuickConnectAvailability {
                        request_id: actual,
                        ..
                    } if *actual == request_id
                )
            })
            .await;
            assert!(matches!(
                event,
                WorkerEvent::QuickConnectAvailability {
                    request_id: actual,
                    enabled,
                } if actual == request_id && enabled == expected
            ));
            drop(worker);
            server.shutdown().await;
        }
    }

    #[tokio::test]
    async fn playlist_mutations_run_through_worker_and_keep_entry_ids_stable() {
        let server = spawn(MockConfig::default())
            .await
            .expect("start playlist mock");
        let worker = test_worker();
        connect_worker(&worker, server.base_url())
            .await
            .expect("connect worker to mock");

        worker
            .fetch_items(
                201,
                ItemQuery {
                    include_item_types: Some(vec!["Movie".into()]),
                    limit: 2,
                    ..ItemQuery::default()
                },
            )
            .expect("queue candidate items");
        let candidates = match wait_for_event(&worker, |event| {
            matches!(
                event,
                WorkerEvent::ItemsLoaded {
                    request_id: 201,
                    ..
                }
            )
        })
        .await
        {
            WorkerEvent::ItemsLoaded { page, .. } => page.items,
            _ => unreachable!("predicate accepts only item results"),
        };
        assert!(candidates.len() >= 2, "mock provides at least two movies");
        let first_item_id = candidates[0].id.clone();
        let second_item_id = candidates[1].id.clone();

        worker
            .create_playlist(202, "Worker mutation integration".into())
            .expect("queue playlist creation");
        let playlist_id = match wait_for_event(&worker, |event| {
            matches!(
                event,
                WorkerEvent::PlaylistCreated {
                    request_id: 202,
                    ..
                }
            )
        })
        .await
        {
            WorkerEvent::PlaylistCreated {
                playlist_id, name, ..
            } => {
                assert_eq!(name, "Worker mutation integration");
                playlist_id
            }
            _ => unreachable!("predicate accepts only playlist creation"),
        };

        worker
            .add_playlist_items(
                203,
                playlist_id.clone(),
                vec![
                    first_item_id.clone(),
                    second_item_id.clone(),
                    first_item_id.clone(),
                ],
            )
            .expect("queue playlist additions including a duplicate item");
        assert_playlist_mutation(&worker, 203, &playlist_id, PlaylistMutationKind::Added).await;

        let initial = load_worker_playlist(&worker, 204, &playlist_id).await;
        assert_eq!(
            initial
                .iter()
                .map(|entry| entry.item.id.as_str())
                .collect::<Vec<_>>(),
            [
                first_item_id.as_str(),
                second_item_id.as_str(),
                first_item_id.as_str()
            ]
        );
        let entry_ids = initial
            .iter()
            .map(|entry| {
                entry
                    .playlist_item_id
                    .clone()
                    .expect("mock response supplies a playlist entry ID")
            })
            .collect::<Vec<_>>();
        assert_ne!(
            entry_ids[0], entry_ids[2],
            "duplicate media has distinct entry IDs"
        );

        worker
            .move_playlist_entry(205, playlist_id.clone(), entry_ids[1].clone(), 0)
            .expect("queue move using the second entry ID");
        assert_playlist_mutation(&worker, 205, &playlist_id, PlaylistMutationKind::Moved).await;
        let moved = load_worker_playlist(&worker, 206, &playlist_id).await;
        assert_eq!(
            moved
                .iter()
                .map(|entry| entry.item.id.as_str())
                .collect::<Vec<_>>(),
            [
                second_item_id.as_str(),
                first_item_id.as_str(),
                first_item_id.as_str()
            ]
        );
        assert_eq!(
            moved
                .iter()
                .map(|entry| entry.playlist_item_id.as_deref().expect("entry ID"))
                .collect::<Vec<_>>(),
            [
                entry_ids[1].as_str(),
                entry_ids[0].as_str(),
                entry_ids[2].as_str()
            ],
            "moving an entry preserves its identity and the duplicate's identity"
        );

        worker
            .remove_playlist_entry(207, playlist_id.clone(), entry_ids[0].clone())
            .expect("queue removal by selected entry ID");
        assert_playlist_mutation(&worker, 207, &playlist_id, PlaylistMutationKind::Removed).await;
        let remaining = load_worker_playlist(&worker, 208, &playlist_id).await;
        assert_eq!(remaining.len(), 2);
        assert_eq!(remaining[0].item.id, second_item_id);
        assert_eq!(
            remaining[0].playlist_item_id.as_deref(),
            Some(entry_ids[1].as_str())
        );
        assert_eq!(remaining[1].item.id, first_item_id);
        assert_eq!(
            remaining[1].playlist_item_id.as_deref(),
            Some(entry_ids[2].as_str())
        );

        let requests = get_mock_json(&server, "/__mock/requests").await;
        let requests = requests.as_array().expect("mock request log is an array");
        let playlist_paths = requests
            .iter()
            .filter_map(|request| Some((request["Method"].as_str()?, request["Path"].as_str()?)))
            .collect::<Vec<_>>();
        assert!(playlist_paths.contains(&("POST", "/Playlists")));
        assert!(playlist_paths.iter().any(|(method, path)| {
            *method == "POST"
                && path.starts_with(&format!("/Playlists/{playlist_id}/Items?ids="))
                && path.contains(&first_item_id)
                && path.contains(&second_item_id)
        }));
        assert!(playlist_paths.contains(&(
            "POST",
            format!("/Playlists/{playlist_id}/Items/{}/Move/0", entry_ids[1]).as_str()
        )));
        assert!(playlist_paths.iter().any(|(method, path)| {
            *method == "DELETE"
                && path.starts_with(&format!("/Playlists/{playlist_id}/Items?entryIds="))
                && path.contains(&entry_ids[0])
        }));

        drop(worker);
        server.shutdown().await;
    }

    #[tokio::test]
    async fn live_tv_channel_search_runs_through_worker_and_filters_paged_lineup() {
        let server = spawn(MockConfig::default())
            .await
            .expect("start Live TV search mock");
        let worker = test_worker();
        connect_worker(&worker, server.base_url())
            .await
            .expect("connect worker to mock");

        worker
            .search_live_tv_channels(209, "mock channel".into(), 30, 30)
            .expect("queue channel search");
        let event = wait_for_event(&worker, |event| {
            matches!(
                event,
                WorkerEvent::LiveTvSearchResultsLoaded {
                    request_id: 209,
                    ..
                } | WorkerEvent::LiveTvSearchResultsFailed { request_id: 209 }
            )
        })
        .await;
        let WorkerEvent::LiveTvSearchResultsLoaded { items, .. } = event else {
            panic!("mock channel search failed");
        };
        assert_eq!(items.len(), 30);
        assert_eq!(items[0].name, "Mock Channel 031");
        assert_eq!(items[29].name, "Mock Channel 060");
        assert_eq!(items[0].item_type.as_deref(), Some("TvChannel"));

        let requests = get_mock_json(&server, "/__mock/requests").await;
        let channel_requests = requests
            .as_array()
            .expect("mock request log is an array")
            .iter()
            .filter_map(|request| {
                let path = request["Path"].as_str()?;
                path.starts_with("/LiveTv/Channels?").then_some(path)
            })
            .collect::<Vec<_>>();
        assert_eq!(
            channel_requests.len(),
            2,
            "search must page all 150 channels"
        );
        assert!(
            channel_requests
                .iter()
                .all(|path| !path.contains("searchTerm"))
        );

        drop(worker);
        server.shutdown().await;
    }

    async fn assert_playlist_mutation(
        worker: &ApiWorker,
        request_id: u64,
        playlist_id: &str,
        expected_kind: PlaylistMutationKind,
    ) {
        let event = wait_for_event(worker, |event| {
            matches!(
                event,
                WorkerEvent::PlaylistMutationCompleted {
                    request_id: actual,
                    ..
                } if *actual == request_id
            ) || matches!(
                event,
                WorkerEvent::PlaylistMutationFailed {
                    request_id: actual,
                    ..
                } if *actual == request_id
            )
        })
        .await;
        assert!(
            matches!(
                event,
                WorkerEvent::PlaylistMutationCompleted {
                    request_id: actual_request,
                    playlist_id: actual_playlist,
                    kind,
                } if actual_request == request_id
                    && actual_playlist == playlist_id
                    && kind == expected_kind
            ),
            "playlist mutation did not complete with the expected playlist and operation"
        );
    }

    async fn load_worker_playlist(
        worker: &ApiWorker,
        request_id: u64,
        playlist_id: &str,
    ) -> Vec<jellyfin_api::PlaylistEntry> {
        worker
            .list_playlist_items(request_id, playlist_id.to_owned())
            .expect("queue playlist refresh");
        match wait_for_event(worker, |event| {
            matches!(event, WorkerEvent::PlaylistItemsLoaded { request_id: actual, .. } if *actual == request_id)
                || matches!(event, WorkerEvent::PlaylistItemsFailed { request_id: actual, .. } if *actual == request_id)
        })
        .await
        {
            WorkerEvent::PlaylistItemsLoaded { playlist_id: actual, entries, .. } => {
                assert_eq!(actual, playlist_id);
                entries
            }
            _ => panic!("playlist reload failed"),
        }
    }

    #[tokio::test]
    async fn server_check_returns_public_identity_without_authenticating() {
        let server = spawn(MockConfig::default())
            .await
            .expect("start public-info mock");
        let worker = test_worker();
        worker
            .check_server(129, server.base_url(), false)
            .expect("queue public-info check");
        let event = wait_for_event(&worker, |event| {
            matches!(
                event,
                WorkerEvent::ServerChecked {
                    request_id: 129,
                    ..
                }
            )
        })
        .await;
        assert!(matches!(
            event,
            WorkerEvent::ServerChecked { server, .. }
                if server.name == "Mock Jellyfin" && server.version.as_deref() == Some("10.10.7")
        ));

        worker
            .check_server(130, format!("{}/missing-path", server.base_url()), false)
            .expect("queue failed public-info check");
        assert!(matches!(
            wait_for_event(&worker, |event| matches!(event, WorkerEvent::ServerCheckFailed { request_id: 130, .. })).await,
            WorkerEvent::ServerCheckFailed { message, .. }
                if message == "Server not found at this address."
        ));
        drop(worker);
        server.shutdown().await;
    }

    #[tokio::test]
    async fn live_tv_logo_worker_loads_small_image_and_emits_placeholder_failure_on_404() {
        const PRESENT_CHANNEL: &str = "00000000000000000000000000000001";
        const MISSING_LOGO_CHANNEL: &str = "00000000000000000000000000000002";
        let server = spawn(MockConfig::default())
            .await
            .expect("start Live TV logo mock");
        let worker = test_worker();
        connect_worker(&worker, server.base_url())
            .await
            .expect("connect worker to Live TV mock");

        worker
            .get_live_tv_logo(131, PRESENT_CHANNEL.into(), "logo-tag".into())
            .expect("queue channel logo");
        let loaded = wait_for_event(&worker, |event| {
            matches!(
                event,
                WorkerEvent::LiveTvLogoLoaded {
                    request_id: 131,
                    ..
                }
            )
        })
        .await;
        assert!(matches!(
            loaded,
            WorkerEvent::LiveTvLogoLoaded {
                channel_id,
                width,
                height,
                rgba,
                ..
            } if channel_id == PRESENT_CHANNEL
                && width <= 128
                && height <= 128
                && !rgba.is_empty()
                && rgba.len() <= 1024 * 1024
        ));

        worker
            .get_live_tv_logo(132, MISSING_LOGO_CHANNEL.into(), String::new())
            .expect("queue channel without a logo");
        assert!(matches!(
            wait_for_event(&worker, |event| matches!(
                event,
                WorkerEvent::LiveTvLogoFailed { request_id: 132, .. }
            ))
            .await,
            WorkerEvent::LiveTvLogoFailed { channel_id, .. }
                if channel_id == MISSING_LOGO_CHANNEL
        ));

        drop(worker);
        server.shutdown().await;
    }

    #[tokio::test]
    async fn queue_restore_runs_through_worker_and_keeps_bounded_batches_ordered() {
        let server = spawn(MockConfig {
            audio_item_count: Some(200),
            ..MockConfig::default()
        })
        .await
        .expect("start 200-track mock server");
        let worker = test_worker();
        connect_worker(&worker, server.base_url())
            .await
            .expect("connect");
        worker
            .fetch_items(
                2,
                ItemQuery {
                    include_item_types: Some(vec!["Audio".into()]),
                    limit: 200,
                    ..ItemQuery::default()
                },
            )
            .expect("queue fixture item request");
        let source_items = match wait_for_event(&worker, |event| {
            matches!(event, WorkerEvent::ItemsLoaded { request_id: 2, .. })
        })
        .await
        {
            WorkerEvent::ItemsLoaded { page, .. } => page.items,
            _ => unreachable!("predicate accepts only item results"),
        };
        assert_eq!(source_items.len(), 200);
        let requested_ids = source_items
            .iter()
            .map(|item| item.id.clone())
            .collect::<Vec<_>>();

        worker
            .restore_queue_items(3, requested_ids.clone())
            .expect("queue saved queue restoration");
        let restored = match wait_for_event(&worker, |event| {
            matches!(event, WorkerEvent::QueueItemsRestored { request_id: 3, .. })
        })
        .await
        {
            WorkerEvent::QueueItemsRestored { items, .. } => items,
            _ => unreachable!("predicate accepts only restored queue items"),
        };
        assert_eq!(
            restored
                .iter()
                .map(|item| item.id.clone())
                .collect::<Vec<_>>(),
            requested_ids
        );

        let requests = get_mock_json(&server, "/__mock/requests").await;
        let batches = requests
            .as_array()
            .expect("request entries")
            .iter()
            .filter_map(|entry| entry["Path"].as_str())
            .filter(|path| path.contains("ids="))
            .collect::<Vec<_>>();
        assert_eq!(batches.len(), 4);
        for path in batches {
            let ids = path
                .split_once("ids=")
                .map(|(_, tail)| tail.split('&').next().unwrap_or_default())
                .expect("batch has item IDs");
            let separator_count = ids.matches(',').count() + ids.matches("%2C").count();
            assert!(separator_count < 50, "batch too large: {path}");
        }
        drop(worker);
        server.shutdown().await;
    }

    #[tokio::test]
    async fn quick_connect_worker_shows_code_polls_until_approved_and_authenticates() {
        let server = spawn(MockConfig {
            quick_connect_pending_polls: 2,
            ..MockConfig::default()
        })
        .await
        .expect("start Quick Connect mock");
        let worker = test_worker();
        worker
            .quick_connect_start(125, server.base_url(), false)
            .expect("queue Quick Connect start");
        let code = wait_for_event(&worker, |event| {
            matches!(
                event,
                WorkerEvent::QuickConnectCode {
                    request_id: 125,
                    ..
                }
            )
        })
        .await;
        assert!(matches!(
            code,
            WorkerEvent::QuickConnectCode { code, .. } if !code.is_empty()
        ));

        for (request_id, expected) in [
            (126, PollOutcome::Pending),
            (127, PollOutcome::Pending),
            (128, PollOutcome::Authorized),
        ] {
            worker
                .quick_connect_poll(request_id)
                .expect("queue Quick Connect poll");
            let result = wait_for_event(&worker, |event| {
                matches!(event, WorkerEvent::QuickConnectPolled { request_id: actual, .. } if *actual == request_id)
            })
            .await;
            assert!(matches!(
                result,
                WorkerEvent::QuickConnectPolled { outcome, .. } if outcome == expected
            ));
        }

        worker
            .quick_connect_authenticate(125, SavedSignInRequest::not_remembered())
            .expect("queue Quick Connect token exchange");
        assert!(matches!(
            wait_for_event(&worker, |event| {
                matches!(
                    event,
                    WorkerEvent::CatalogLoading {
                        request_id: 125,
                        ..
                    }
                )
            })
            .await,
            WorkerEvent::CatalogLoading { .. }
        ));
        drop(worker);
        server.shutdown().await;
    }

    #[tokio::test]
    async fn switching_servers_drops_the_previous_session_before_authenticating_the_new_one() {
        let previous_server = spawn(MockConfig::default())
            .await
            .expect("start previous mock server");
        let selected_server = spawn(MockConfig::default())
            .await
            .expect("start selected mock server");
        let worker = test_worker();
        connect_worker(&worker, previous_server.base_url())
            .await
            .expect("sign in to previous server");

        let previous_requests_before = get_mock_json(&previous_server, "/__mock/requests").await;
        let previous_item_requests_before = previous_requests_before
            .as_array()
            .expect("previous server request log is an array")
            .iter()
            .filter(|entry| {
                entry["Path"]
                    .as_str()
                    .is_some_and(|path| path.starts_with("/Items"))
            })
            .count();

        worker
            .connect(
                122,
                selected_server.base_url(),
                "demo".into(),
                Zeroizing::new("wrong-password".into()),
                false,
                SavedSignInRequest::not_remembered(),
            )
            .expect("queue authentication against selected server");
        let failed = wait_for_event(&worker, |event| {
            matches!(
                event,
                WorkerEvent::ConnectionFailed {
                    request_id: Some(122),
                    ..
                }
            )
        })
        .await;
        assert!(matches!(
            failed,
            WorkerEvent::ConnectionFailed { message, .. }
                if message == "Sign-in failed. Check your username and password."
        ));

        worker
            .fetch_items(
                123,
                item_query(
                    0,
                    None,
                    None,
                    None,
                    jellyfin_api::ItemSortBy::SortName,
                    jellyfin_api::ItemSortOrder::Ascending,
                    None,
                ),
            )
            .expect("queue a request after failed server switch");
        assert!(matches!(
            wait_for_event(&worker, |event| matches!(
                event,
                WorkerEvent::ConnectionFailed {
                    request_id: Some(123),
                    ..
                }
            ))
            .await,
            WorkerEvent::ConnectionFailed { message, .. }
                if message.contains("Connect to a server")
        ));

        drop(worker);
        let previous_requests_after = get_mock_json(&previous_server, "/__mock/requests").await;
        let previous_item_requests_after = previous_requests_after
            .as_array()
            .expect("previous server request log is an array")
            .iter()
            .filter(|entry| {
                entry["Path"]
                    .as_str()
                    .is_some_and(|path| path.starts_with("/Items"))
            })
            .count();
        assert_eq!(previous_item_requests_after, previous_item_requests_before);
        previous_server.shutdown().await;
        selected_server.shutdown().await;
    }

    #[tokio::test]
    async fn missing_server_path_shows_a_server_not_found_message() {
        let server = spawn(MockConfig::default())
            .await
            .expect("start mock Jellyfin");
        let worker = test_worker();
        worker
            .connect(
                124,
                format!("{}/missing-path", server.base_url().trim_end_matches('/')),
                "demo".into(),
                Zeroizing::new("demo".into()),
                false,
                SavedSignInRequest::not_remembered(),
            )
            .expect("queue a connection with an invalid base path");
        let event = wait_for_event(&worker, |event| {
            matches!(
                event,
                WorkerEvent::ConnectionFailed {
                    request_id: Some(124),
                    ..
                }
            )
        })
        .await;
        match event {
            WorkerEvent::ConnectionFailed { message, .. } => {
                assert_eq!(message, "Server not found at this address.");
            }
            _ => panic!("unexpected result for missing server path"),
        }
        drop(worker);
        server.shutdown().await;
    }

    #[tokio::test]
    async fn worker_posts_start_periodic_seek_pause_resume_and_stop_in_order() {
        let server = spawn(MockConfig::default())
            .await
            .expect("start mock Jellyfin");
        let worker = test_worker();
        connect_worker(&worker, server.base_url())
            .await
            .expect("connect worker to success mock");

        let actions = scripted_report_actions();
        assert_eq!(actions.len(), 6);
        let expected_routes = [
            "Playing", "Progress", "Progress", "Progress", "Progress", "Stopped",
        ];
        for (offset, action) in actions.into_iter().enumerate() {
            assert_eq!(report_route_kind(&action), expected_routes[offset]);
            let request_id = 20 + offset as u64;
            worker
                .report_playback_action(request_id, action)
                .expect("queue playback report");
            assert_report_sent(&worker, request_id).await;
        }

        drop(worker);

        let sessions = get_mock_json(&server, "/__mock/sessions").await;
        let reports = sessions.as_array().expect("session log is an array");
        let expected_events = [
            "Playing", "Progress", "Progress", "Progress", "Progress", "Stopped",
        ];
        let expected_ticks = [
            100_000_000_i64,
            320_000_000,
            70_000_000,
            70_000_000,
            80_000_000,
            90_000_000,
        ];
        assert_eq!(reports.len(), expected_events.len());
        for ((report, event), ticks) in reports.iter().zip(expected_events).zip(expected_ticks) {
            assert_eq!(report["Event"], event);
            assert_eq!(report["Body"]["PositionTicks"], ticks);
        }
        assert_eq!(reports[0]["Body"]["PlayMethod"], "DirectPlay");
        assert_eq!(reports[2]["Body"]["IsPaused"], false);
        assert_eq!(reports[3]["Body"]["IsPaused"], true);
        assert_eq!(reports[4]["Body"]["IsPaused"], false);

        let requests = get_mock_json(&server, "/__mock/requests").await;
        assert!(
            requests.as_array().is_some_and(|entries| {
                entries.iter().all(|entry| {
                    !entry["Path"]
                        .as_str()
                        .is_some_and(|path| path.starts_with("/UserPlayedItems/"))
                })
            }),
            "playback reporting must not send a separate watched-state mutation: {requests}"
        );

        server.shutdown().await;
    }

    #[tokio::test]
    async fn stop_report_is_posted_after_three_progress_responses_fail() {
        let server = spawn(MockConfig {
            fail_progress_posts: 3,
            ..MockConfig::default()
        })
        .await
        .expect("start failure-configured mock Jellyfin");
        let worker = test_worker();
        connect_worker(&worker, server.base_url())
            .await
            .expect("connect worker to failure mock");

        let actions = three_progress_failure_actions();
        assert_eq!(actions.len(), 5);
        let expected_routes = ["Playing", "Progress", "Progress", "Progress", "Stopped"];
        for (offset, action) in actions.into_iter().enumerate() {
            assert_eq!(report_route_kind(&action), expected_routes[offset]);
            let request_id = 40 + offset as u64;
            worker
                .report_playback_action(request_id, action)
                .expect("queue playback report");
            if (1..=3).contains(&offset) {
                assert_report_failed_with_500(&worker, request_id).await;
            } else {
                assert_report_sent(&worker, request_id).await;
            }
        }

        drop(worker);

        let sessions = get_mock_json(&server, "/__mock/sessions").await;
        let reports = sessions.as_array().expect("session log is an array");
        assert_eq!(reports.len(), 2);
        assert_eq!(reports[0]["Event"], "Playing");
        assert_eq!(reports[0]["Body"]["PositionTicks"], 10_000_000);
        assert_eq!(reports[1]["Event"], "Stopped");
        assert_eq!(reports[1]["Body"]["PositionTicks"], 30_000_000);

        server.shutdown().await;
    }

    #[tokio::test]
    async fn dropping_worker_drains_a_queued_stop_report_before_exit() {
        let server = spawn(MockConfig::default())
            .await
            .expect("start mock Jellyfin");
        let worker = test_worker();
        connect_worker(&worker, server.base_url())
            .await
            .expect("connect worker to success mock");

        let stop = scripted_report_actions()
            .pop()
            .expect("script ends with a stop report");
        worker
            .report_playback_action(90, stop)
            .expect("queue stop before shutdown");
        let drop_thread = thread::spawn(move || drop(worker));

        let sessions = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let reports = get_mock_json(&server, "/__mock/sessions").await;
                if reports
                    .as_array()
                    .is_some_and(|entries| entries.iter().any(|entry| entry["Event"] == "Stopped"))
                {
                    break reports;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("queued stop reaches Jellyfin before worker exits");

        tokio::task::spawn_blocking(move || drop_thread.join())
            .await
            .expect("join worker shutdown waiter")
            .expect("worker shutdown waiter exits");
        let reports = sessions.as_array().expect("session log is an array");
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0]["Event"], "Stopped");
        assert_eq!(reports[0]["Body"]["PositionTicks"], 90_000_000_i64);

        server.shutdown().await;
    }

    #[tokio::test]
    async fn watched_toggle_server_failure_reaches_ui_and_preserves_method_route() {
        let server = spawn(MockConfig {
            played_toggle_status: Some(500),
            ..MockConfig::default()
        })
        .await
        .expect("start failing-toggle mock Jellyfin");
        let worker = test_worker();
        connect_worker(&worker, server.base_url())
            .await
            .expect("connect worker to mock");

        worker
            .set_played(91, "item-7".into(), true)
            .expect("queue optimistic mark-played request");
        let event = wait_for_event(&worker, |event| {
            matches!(
                event,
                WorkerEvent::WatchedStateFailed { request_id: 91, .. }
            )
        })
        .await;
        assert!(matches!(
            event,
            WorkerEvent::WatchedStateFailed {
                request_id: 91,
                item_id,
                message,
            } if item_id == "item-7" && message.contains("500")
        ));

        drop(worker);
        let requests = get_mock_json(&server, "/__mock/requests").await;
        assert!(
            requests.as_array().is_some_and(|entries| {
                entries.iter().any(|entry| {
                    entry["Method"] == "POST"
                        && entry["Path"]
                            .as_str()
                            .is_some_and(|path| path.starts_with("/UserPlayedItems/item-7?userId="))
                })
            }),
            "mock request log: {requests}"
        );
        server.shutdown().await;
    }

    #[tokio::test]
    async fn watched_toggle_success_posts_and_deletes_through_worker() {
        let server = spawn(MockConfig::default())
            .await
            .expect("start mock Jellyfin");
        let worker = test_worker();
        let expected_item_id = "00000000000000000000000000000100";
        connect_worker(&worker, server.base_url())
            .await
            .expect("connect worker to mock");

        for (request_id, played) in [(92, true), (93, false)] {
            worker
                .set_played(request_id, expected_item_id.into(), played)
                .expect("queue watched-state toggle");
            let event = wait_for_event(&worker, |event| {
                matches!(
                    event,
                    WorkerEvent::WatchedStateUpdated {
                        request_id: actual,
                        ..
                    } if *actual == request_id
                )
            })
            .await;
            assert!(matches!(
                event,
                WorkerEvent::WatchedStateUpdated {
                    item_id,
                    played: actual,
                    ..
                } if item_id == expected_item_id && actual == played
            ));
        }

        drop(worker);
        let requests = get_mock_json(&server, "/__mock/requests").await;
        let entries = requests.as_array().expect("mock request log is an array");
        let toggles = entries
            .iter()
            .filter(|entry| {
                entry["Path"].as_str().is_some_and(|path| {
                    path.starts_with(&format!("/UserPlayedItems/{expected_item_id}?userId="))
                })
            })
            .collect::<Vec<_>>();
        assert_eq!(toggles.len(), 2, "mock request log: {requests}");
        assert_eq!(toggles[0]["Method"], "POST");
        assert_eq!(toggles[1]["Method"], "DELETE");
        server.shutdown().await;
    }

    #[tokio::test]
    async fn favorite_toggle_updates_user_data_and_posts_then_deletes_through_worker() {
        let server = spawn(MockConfig::default())
            .await
            .expect("start mock Jellyfin");
        let worker = test_worker();
        let item_id = "00000000000000000000000000000100";
        connect_worker(&worker, server.base_url())
            .await
            .expect("connect worker to mock");

        for (request_id, favorite) in [(131, true), (132, false)] {
            worker
                .set_favorite(request_id, item_id.into(), favorite)
                .expect("queue favorite toggle");
            let event = wait_for_event(&worker, |event| {
                matches!(event, WorkerEvent::FavoriteStateUpdated { request_id: actual, .. } if *actual == request_id)
            })
            .await;
            assert!(matches!(
                event,
                WorkerEvent::FavoriteStateUpdated {
                    item_id: actual_item,
                    favorite: actual,
                    user_data,
                    ..
                } if actual_item == item_id
                    && actual == favorite
                    && user_data.is_favorite == Some(favorite)
            ));
        }

        drop(worker);
        let requests = get_mock_json(&server, "/__mock/requests").await;
        let entries = requests.as_array().expect("mock request log is an array");
        let toggles = entries
            .iter()
            .filter(|entry| {
                entry["Path"].as_str().is_some_and(|path| {
                    path.starts_with(&format!("/UserFavoriteItems/{item_id}?userId="))
                })
            })
            .collect::<Vec<_>>();
        assert_eq!(toggles.len(), 2, "mock request log: {requests}");
        assert_eq!(toggles[0]["Method"], "POST");
        assert_eq!(toggles[1]["Method"], "DELETE");
        server.shutdown().await;
    }

    fn test_worker() -> ApiWorker {
        let commands = Arc::new(CommandQueue::default());
        let command_rx = commands.clone();
        let (event_tx, events) = tokio::sync::mpsc::channel(32);
        let sink = WorkerSink::for_test(event_tx);
        let thread = thread::Builder::new()
            .name("m1-13-test-worker".into())
            .spawn(move || run_worker(command_rx, sink, "test-device".into()))
            .expect("spawn API worker");
        ApiWorker {
            commands,
            events: Mutex::new(events),
            thread: Some(thread),
            #[cfg(feature = "photo-counters")]
            photo_decode_counters: Default::default(),
        }
    }

    async fn connect_worker(worker: &ApiWorker, base_url: String) -> Result<(), String> {
        worker
            .connect(
                1,
                base_url,
                "demo".into(),
                Zeroizing::new("demo".into()),
                false,
                SavedSignInRequest::not_remembered(),
            )
            .expect("queue mock connection");
        match wait_for_event(worker, |event| {
            matches!(
                event,
                WorkerEvent::ItemsLoaded { request_id: 1, .. }
                    | WorkerEvent::ConnectionFailed {
                        request_id: Some(1),
                        ..
                    }
            )
        })
        .await
        {
            WorkerEvent::ItemsLoaded { .. } => Ok(()),
            WorkerEvent::ConnectionFailed { message, .. } => Err(message),
            _ => unreachable!("event predicate accepts only connect completion"),
        }
    }

    async fn assert_report_sent(worker: &ApiWorker, request_id: u64) {
        assert!(matches!(
            wait_for_event(worker, |event| matches!(
                event,
                WorkerEvent::PlaybackReportSent { request_id: actual }
                    | WorkerEvent::PlaybackReportFailed { request_id: actual, .. }
                    if *actual == request_id
            ))
            .await,
            WorkerEvent::PlaybackReportSent { request_id: actual } if actual == request_id
        ));
    }

    async fn assert_report_failed_with_500(worker: &ApiWorker, request_id: u64) {
        match wait_for_event(worker, |event| {
            matches!(
                event,
                WorkerEvent::PlaybackReportSent { request_id: actual }
                    | WorkerEvent::PlaybackReportFailed { request_id: actual, .. }
                    if *actual == request_id
            )
        })
        .await
        {
            WorkerEvent::PlaybackReportFailed { message, .. } => {
                assert!(
                    message.contains("500"),
                    "unexpected report error: {message}"
                );
            }
            WorkerEvent::PlaybackReportSent { .. } => {
                panic!("progress report unexpectedly succeeded instead of returning HTTP 500");
            }
            _ => unreachable!("event predicate accepts only report completion"),
        }
    }

    async fn wait_for_event(
        worker: &ApiWorker,
        predicate: impl Fn(&WorkerEvent) -> bool,
    ) -> WorkerEvent {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                while let Some(event) = worker.try_recv() {
                    if predicate(&event) {
                        return event;
                    }
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .expect("worker event arrives before timeout")
    }

    fn scripted_report_actions() -> Vec<PlaybackReportAction> {
        let metadata = report_metadata();
        let session = PlaybackReportSessionId::new(1);
        let mut scheduler =
            PlaybackReportScheduler::new(Duration::from_secs(10)).expect("non-zero report cadence");
        let mut actions = Vec::new();
        actions.extend(scheduler.apply(PlaybackReportInput::Snapshot {
            session,
            at: Duration::ZERO,
            report: metadata.progress_info(Some(ticks(10)), Some(false)),
        }));
        actions.extend(scheduler.apply(PlaybackReportInput::Snapshot {
            session,
            at: Duration::from_secs(10),
            report: metadata.progress_info(Some(ticks(32)), Some(false)),
        }));
        actions.extend(scheduler.apply(PlaybackReportInput::Seek {
            session,
            at: Duration::from_secs(11),
            report: metadata.progress_info(Some(ticks(7)), Some(false)),
        }));
        actions.extend(scheduler.apply(PlaybackReportInput::Snapshot {
            session,
            at: Duration::from_secs(12),
            report: metadata.progress_info(Some(ticks(7)), Some(true)),
        }));
        actions.extend(scheduler.apply(PlaybackReportInput::Snapshot {
            session,
            at: Duration::from_secs(13),
            report: metadata.progress_info(Some(ticks(8)), Some(false)),
        }));
        actions.extend(scheduler.apply(PlaybackReportInput::Stop {
            session,
            item_id: metadata.item_id.clone(),
            final_position: Some(ticks(9)),
            failed: false,
        }));
        actions
    }

    fn three_progress_failure_actions() -> Vec<PlaybackReportAction> {
        let metadata = report_metadata();
        let session = PlaybackReportSessionId::new(2);
        let mut scheduler =
            PlaybackReportScheduler::new(Duration::from_secs(5)).expect("non-zero report cadence");
        let mut actions = Vec::new();
        actions.extend(scheduler.apply(PlaybackReportInput::Snapshot {
            session,
            at: Duration::ZERO,
            report: metadata.progress_info(Some(ticks(1)), Some(false)),
        }));
        actions.extend(scheduler.apply(PlaybackReportInput::Snapshot {
            session,
            at: Duration::from_secs(5),
            report: metadata.progress_info(Some(ticks(5)), Some(false)),
        }));
        actions.extend(scheduler.apply(PlaybackReportInput::Seek {
            session,
            at: Duration::from_secs(6),
            report: metadata.progress_info(Some(ticks(2)), Some(false)),
        }));
        actions.extend(scheduler.apply(PlaybackReportInput::Snapshot {
            session,
            at: Duration::from_secs(7),
            report: metadata.progress_info(Some(ticks(2)), Some(true)),
        }));
        actions.extend(scheduler.apply(PlaybackReportInput::Stop {
            session,
            item_id: metadata.item_id.clone(),
            final_position: Some(ticks(3)),
            failed: false,
        }));
        actions
    }

    fn report_metadata() -> PlaybackReportMetadata {
        PlaybackReportMetadata {
            item_id: "00000000000000000000000000000100".into(),
            media_source_id: Some("mock-media-source".into()),
            live_stream_id: None,
            live_tune_started_at: None,
            audio_stream_index: Some(0),
            subtitle_stream_index: None,
            play_session_id: Some("mock-play-session".into()),
            play_method: PlayMethod::DirectPlay,
            can_seek: Some(true),
            repeat_mode: None,
            playback_order: None,
            now_playing_queue: None,
        }
    }

    fn ticks(seconds: i64) -> JellyfinTicks {
        JellyfinTicks::from_ticks(seconds * 10_000_000)
    }

    fn report_route_kind(action: &PlaybackReportAction) -> &'static str {
        match action {
            PlaybackReportAction::Started(_) => "Playing",
            PlaybackReportAction::Progress(_) => "Progress",
            PlaybackReportAction::Stopped(_) => "Stopped",
        }
    }

    async fn get_mock_json(server: &RunningServer, path: &str) -> Value {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpStream as TokioTcpStream;

        let mut stream = TokioTcpStream::connect(server.addr)
            .await
            .expect("connect to local mock");
        stream
            .write_all(
                format!(
                    "GET {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
                    server.addr
                )
                .as_bytes(),
            )
            .await
            .expect("request mock session log");
        let mut response = Vec::new();
        stream
            .read_to_end(&mut response)
            .await
            .expect("read mock session log");
        let header_end = response
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .expect("HTTP response headers");
        let headers = String::from_utf8_lossy(&response[..header_end]);
        let body = &response[header_end + 4..];
        let decoded = if headers
            .lines()
            .any(|line| line.eq_ignore_ascii_case("transfer-encoding: chunked"))
        {
            decode_chunked_body(body)
        } else {
            body.to_vec()
        };
        serde_json::from_slice(&decoded).expect("mock session JSON")
    }

    fn decode_chunked_body(mut body: &[u8]) -> Vec<u8> {
        let mut decoded = Vec::new();
        loop {
            let line_end = body
                .windows(2)
                .position(|window| window == b"\r\n")
                .expect("chunk length line");
            let length_text = std::str::from_utf8(&body[..line_end])
                .expect("chunk length is UTF-8")
                .split(';')
                .next()
                .expect("chunk size exists");
            let length = usize::from_str_radix(length_text, 16).expect("chunk size is hex");
            body = &body[line_end + 2..];
            if length == 0 {
                return decoded;
            }
            decoded.extend_from_slice(&body[..length]);
            body = &body[length + 2..];
        }
    }
}

#[cfg(test)]
mod saved_sign_in_worker_tests {
    //! "Stay signed in" through the real worker against the in-process mock, with an in-memory
    //! credential store standing in for the OS one. These prove the worker's save, resume,
    //! refusal and sign-out behavior, not Credential Manager, Keychain or Secret Service.

    use super::saved_sign_in::{SavedSignInStore, credential_key};
    use super::{ApiWorker, CommandQueue, SavedSignInRequest, WorkerEvent, WorkerSink, run_worker};
    use kattri_platform::memory::MemoryCredentialStore;
    use kattri_platform::{CredentialKey, CredentialStore, Secret};
    use mock_jellyfin::{MockConfig, spawn};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::Duration;
    use zeroize::Zeroizing;

    /// The mock's fixed server id and access token (crate-private in mock-jellyfin).
    const MOCK_SERVER_ID: &str = "00000000000000000000000000000005e7";
    const MOCK_TOKEN: &str = "mock-access-token-0001";

    type Backing = Arc<Mutex<MemoryCredentialStore>>;

    fn worker_on(backing: &Backing) -> ApiWorker {
        let commands = Arc::new(CommandQueue::default());
        let command_rx = commands.clone();
        let (event_tx, events) = tokio::sync::mpsc::channel(32);
        let shared: Arc<Mutex<dyn CredentialStore + Send>> = backing.clone();
        let sink = WorkerSink {
            saved_sign_in: SavedSignInStore::shared(shared),
            ..WorkerSink::for_test(event_tx)
        };
        let thread = thread::Builder::new()
            .name("saved-sign-in-test-worker".into())
            .spawn(move || run_worker(command_rx, sink, "test-device".into()))
            .expect("spawn API worker");
        ApiWorker {
            commands,
            events: Mutex::new(events),
            thread: Some(thread),
            #[cfg(feature = "photo-counters")]
            photo_decode_counters: Default::default(),
        }
    }

    fn key(scope: &str, server_url: &str) -> CredentialKey {
        let client = super::build_client(server_url, false, "test-device").expect("client");
        credential_key(scope, MOCK_SERVER_ID, &client).expect("key")
    }

    fn request(scope: &str, remember: bool) -> SavedSignInRequest {
        SavedSignInRequest {
            scope: scope.to_owned(),
            remember,
        }
    }

    fn stored(backing: &Backing, scope: &str, server_url: &str) -> Option<String> {
        let secret = backing
            .lock()
            .expect("lock")
            .get(&key(scope, server_url))
            .expect("get")?;
        Some(String::from_utf8(secret.expose().to_vec()).expect("utf-8 token"))
    }

    async fn wait_for(worker: &ApiWorker, predicate: impl Fn(&WorkerEvent) -> bool) -> WorkerEvent {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                while let Some(event) = worker.try_recv() {
                    if predicate(&event) {
                        return event;
                    }
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .expect("worker event arrives before timeout")
    }

    async fn sign_in(worker: &ApiWorker, url: &str, saved: SavedSignInRequest) {
        worker
            .connect(
                1,
                url.to_owned(),
                "demo".into(),
                Zeroizing::new("demo".into()),
                false,
                saved,
            )
            .expect("queue sign-in");
        let event = wait_for(worker, |event| {
            matches!(
                event,
                WorkerEvent::ItemsLoaded { .. } | WorkerEvent::ConnectionFailed { .. }
            )
        })
        .await;
        assert!(
            matches!(event, WorkerEvent::ItemsLoaded { .. }),
            "sign-in must succeed"
        );
    }

    fn resume(worker: &ApiWorker, url: &str, id: u64, scope: &str) {
        worker
            .resume_sign_in(id, url.to_owned(), false, request(scope, true))
            .expect("queue resume");
    }

    #[tokio::test]
    async fn a_remembered_sign_in_is_saved_and_a_new_worker_resumes_it() {
        let server = spawn(MockConfig::default()).await.expect("mock");
        let backing: Backing = Arc::default();

        let first = worker_on(&backing);
        sign_in(&first, &server.base_url(), request("default", true)).await;
        assert_eq!(
            stored(&backing, "default", &server.base_url()).as_deref(),
            Some(MOCK_TOKEN)
        );
        drop(first);

        // A restart: a new worker with the same credential store, no password involved.
        let second = worker_on(&backing);
        resume(&second, &server.base_url(), 7, "default");
        let event = wait_for(&second, |event| {
            matches!(
                event,
                WorkerEvent::ItemsLoaded { request_id: 7, .. }
                    | WorkerEvent::ResumeUnavailable { .. }
                    | WorkerEvent::ConnectionFailed { .. }
            )
        })
        .await;
        assert!(matches!(
            event,
            WorkerEvent::ItemsLoaded { request_id: 7, .. }
        ));
        assert_eq!(
            stored(&backing, "default", &server.base_url()).as_deref(),
            Some(MOCK_TOKEN)
        );
        server.shutdown().await;
    }

    #[tokio::test]
    async fn another_endpoint_claiming_the_same_server_id_cannot_resume_a_saved_token() {
        let trusted = spawn(MockConfig::default()).await.expect("trusted mock");
        let impostor = spawn(MockConfig::default()).await.expect("second mock");
        let backing: Backing = Arc::default();
        let first = worker_on(&backing);
        sign_in(&first, &trusted.base_url(), request("default", true)).await;
        drop(first);

        // Both mocks advertise the same public server ID, but listen at distinct addresses.
        let second = worker_on(&backing);
        resume(&second, &impostor.base_url(), 19, "default");
        let event = wait_for(&second, |event| {
            matches!(
                event,
                WorkerEvent::ResumeUnavailable { .. }
                    | WorkerEvent::ItemsLoaded { .. }
                    | WorkerEvent::ConnectionFailed { .. }
            )
        })
        .await;
        assert!(
            matches!(
                event,
                WorkerEvent::ResumeUnavailable {
                    request_id: 19,
                    expired: false
                }
            ),
            "the server ID alone must never authorize disclosure of a saved token"
        );
        drop(second);
        trusted.shutdown().await;
        impostor.shutdown().await;
    }

    #[tokio::test]
    async fn resume_with_nothing_saved_is_silent() {
        let server = spawn(MockConfig::default()).await.expect("mock");
        let backing: Backing = Arc::default();
        let worker = worker_on(&backing);
        resume(&worker, &server.base_url(), 3, "default");
        let event = wait_for(&worker, |event| {
            matches!(
                event,
                WorkerEvent::ResumeUnavailable { .. }
                    | WorkerEvent::ItemsLoaded { .. }
                    | WorkerEvent::ConnectionFailed { .. }
            )
        })
        .await;
        assert!(matches!(
            event,
            WorkerEvent::ResumeUnavailable {
                request_id: 3,
                expired: false
            }
        ));
        server.shutdown().await;
    }

    #[tokio::test]
    async fn a_token_the_server_refuses_is_deleted_and_reported_as_expired() {
        let server = spawn(MockConfig::default()).await.expect("mock");
        let backing: Backing = Arc::default();
        backing
            .lock()
            .expect("lock")
            .set(
                &key("default", &server.base_url()),
                Secret::new(b"revoked-token".to_vec()),
            )
            .expect("seed");
        let worker = worker_on(&backing);
        resume(&worker, &server.base_url(), 4, "default");
        let event = wait_for(&worker, |event| {
            matches!(
                event,
                WorkerEvent::ResumeUnavailable { .. }
                    | WorkerEvent::ItemsLoaded { .. }
                    | WorkerEvent::ConnectionFailed { .. }
            )
        })
        .await;
        assert!(matches!(
            event,
            WorkerEvent::ResumeUnavailable {
                request_id: 4,
                expired: true
            }
        ));
        assert_eq!(
            stored(&backing, "default", &server.base_url()),
            None,
            "a refused token is deleted"
        );
        server.shutdown().await;
    }

    #[tokio::test]
    async fn another_profile_never_resumes_this_profiles_sign_in() {
        let server = spawn(MockConfig::default()).await.expect("mock");
        let backing: Backing = Arc::default();
        let first = worker_on(&backing);
        sign_in(&first, &server.base_url(), request("default", true)).await;
        drop(first);

        let kids = worker_on(&backing);
        resume(&kids, &server.base_url(), 5, "kids");
        let event = wait_for(&kids, |event| {
            matches!(
                event,
                WorkerEvent::ResumeUnavailable { .. } | WorkerEvent::ItemsLoaded { .. }
            )
        })
        .await;
        assert!(matches!(
            event,
            WorkerEvent::ResumeUnavailable { expired: false, .. }
        ));
        assert_eq!(
            stored(&backing, "default", &server.base_url()).as_deref(),
            Some(MOCK_TOKEN)
        );
        server.shutdown().await;
    }

    #[tokio::test]
    async fn signing_in_without_the_option_saves_nothing_and_clears_an_older_token() {
        let server = spawn(MockConfig::default()).await.expect("mock");
        let backing: Backing = Arc::default();
        backing
            .lock()
            .expect("lock")
            .set(
                &key("default", &server.base_url()),
                Secret::new(b"older-token".to_vec()),
            )
            .expect("seed");
        let worker = worker_on(&backing);
        sign_in(&worker, &server.base_url(), request("default", false)).await;
        assert_eq!(stored(&backing, "default", &server.base_url()), None);
        server.shutdown().await;
    }

    #[tokio::test]
    async fn the_probe_reports_whether_a_credential_store_is_available() {
        let backing: Backing = Arc::default();
        let with_store = worker_on(&backing);
        with_store.probe_saved_sign_in().expect("queue probe");
        let event = wait_for(&with_store, |event| {
            matches!(event, WorkerEvent::SavedSignInAvailability { .. })
        })
        .await;
        assert!(matches!(
            event,
            WorkerEvent::SavedSignInAvailability { available: true }
        ));

        // A sink with no store (the test default) reports it unavailable.
        let commands = Arc::new(CommandQueue::default());
        let command_rx = commands.clone();
        let (event_tx, events) = tokio::sync::mpsc::channel(32);
        let sink = WorkerSink::for_test(event_tx);
        let thread = thread::Builder::new()
            .name("saved-sign-in-probe-worker".into())
            .spawn(move || run_worker(command_rx, sink, "test-device".into()))
            .expect("spawn API worker");
        let without_store = ApiWorker {
            commands,
            events: Mutex::new(events),
            thread: Some(thread),
            #[cfg(feature = "photo-counters")]
            photo_decode_counters: Default::default(),
        };
        without_store.probe_saved_sign_in().expect("queue probe");
        let event = wait_for(&without_store, |event| {
            matches!(event, WorkerEvent::SavedSignInAvailability { .. })
        })
        .await;
        assert!(matches!(
            event,
            WorkerEvent::SavedSignInAvailability { available: false }
        ));
    }

    #[tokio::test]
    async fn signing_out_deletes_the_saved_token() {
        let server = spawn(MockConfig::default()).await.expect("mock");
        let backing: Backing = Arc::default();
        let worker = worker_on(&backing);
        sign_in(&worker, &server.base_url(), request("default", true)).await;
        assert_eq!(
            stored(&backing, "default", &server.base_url()).as_deref(),
            Some(MOCK_TOKEN)
        );

        worker.disconnect().expect("queue sign-out");
        tokio::time::timeout(Duration::from_secs(10), async {
            while stored(&backing, "default", &server.base_url()).is_some() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the saved token is deleted after sign-out");
        server.shutdown().await;
    }

    #[tokio::test]
    async fn subtitle_and_trickplay_commands_return_worker_events_and_local_files() {
        const VIDEO_ID: &str = "00000000000000000000000000000100";
        let server = spawn(MockConfig::default()).await.expect("mock");
        let backing: Backing = Arc::default();
        let worker = worker_on(&backing);
        sign_in(&worker, &server.base_url(), request("default", false)).await;

        worker
            .search_remote_subtitles(20, VIDEO_ID.into(), "eng".into())
            .expect("queue subtitle search");
        let event = wait_for(&worker, |event| {
            matches!(
                event,
                WorkerEvent::RemoteSubtitlesLoaded { request_id: 20, .. }
            )
        })
        .await;
        assert!(
            matches!(event, WorkerEvent::RemoteSubtitlesLoaded { result: Ok(results), .. } if results.len() == 2)
        );

        worker
            .list_subtitle_cultures(21)
            .expect("queue subtitle cultures");
        let event = wait_for(&worker, |event| {
            matches!(
                event,
                WorkerEvent::SubtitleCulturesLoaded { request_id: 21, .. }
            )
        })
        .await;
        assert!(
            matches!(event, WorkerEvent::SubtitleCulturesLoaded { cultures, .. } if !cultures.is_empty())
        );

        worker
            .get_trickplay_resolutions(22, VIDEO_ID.into(), None)
            .expect("queue trickplay manifest");
        let event = wait_for(&worker, |event| {
            matches!(
                event,
                WorkerEvent::TrickplayResolutionsLoaded { request_id: 22, .. }
            )
        })
        .await;
        assert!(
            matches!(event, WorkerEvent::TrickplayResolutionsLoaded { resolutions, .. } if !resolutions.is_empty())
        );

        worker
            .get_trickplay_tile(23, VIDEO_ID.into(), None, 64, 0)
            .expect("queue trickplay tile");
        let event = wait_for(&worker, |event| {
            matches!(
                event,
                WorkerEvent::TrickplayTileLoaded { request_id: 23, .. }
            )
        })
        .await;
        assert!(
            matches!(event, WorkerEvent::TrickplayTileLoaded { image: Some((640, 360, rgba)), .. } if rgba.len() == 640 * 360 * 4)
        );

        worker
            .download_remote_subtitle(24, VIDEO_ID.into(), "mock-eng-1".into(), true)
            .expect("queue subtitle download");
        let event = wait_for(&worker, |event| {
            matches!(
                event,
                WorkerEvent::RemoteSubtitleDownloaded { request_id: 24, .. }
            )
        })
        .await;
        let paths = match event {
            WorkerEvent::RemoteSubtitleDownloaded {
                result: Ok(files), ..
            } => files.into_iter().map(|file| file.path).collect::<Vec<_>>(),
            _ => panic!("expected subtitle files"),
        };
        assert_eq!(paths.len(), 1);
        assert!(paths[0].is_file());
        for path in paths {
            std::fs::remove_file(&path).expect("remove downloaded subtitle test artifact");
            assert!(!path.exists());
        }
        server.shutdown().await;
    }

    #[test]
    fn failed_subtitle_file_publish_removes_its_temporary_file() {
        static TEST_DIR_ID: AtomicU64 = AtomicU64::new(0);
        let folder = std::env::temp_dir().join(format!(
            "kattri-subtitle-cleanup-{}-{}",
            std::process::id(),
            TEST_DIR_ID.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(folder.join("blocked.srt")).expect("create blocking directory");

        assert!(super::write_subtitle_file_in(&folder, "blocked.srt", b"subtitle").is_none());
        let remaining = std::fs::read_dir(&folder)
            .expect("read temporary folder")
            .map(|entry| entry.expect("folder entry").file_name())
            .collect::<Vec<_>>();
        assert_eq!(remaining, [std::ffi::OsString::from("blocked.srt")]);
        std::fs::remove_dir_all(folder).expect("remove subtitle test folder");
    }
}
