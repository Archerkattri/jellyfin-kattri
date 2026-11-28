//! Home view: Continue Watching, Next Up and Latest rows under a hero banner.
//!
//! State transitions and item mapping are plain Rust so they can be unit-tested
//! without egui. Rendering only reads the state and reports at most one
//! [`HomeAction`]; the caller applies it after drawing, so no app borrow is held
//! while the frame is built.

use std::collections::BTreeSet;

use super::{ui_text, ui_text_args};
use egui::{CornerRadius, RichText, Stroke, load::SizedTexture};
use jellyfin_api::{BaseItemDto, UserView};
use kattri_client_core::focus::FocusId;
use kattri_i18n::ids;

use super::hero::{HeroBanner, HeroItem, HeroOptions};
use super::row::{PosterRow, RowCard, RowOptions};
use super::{CardFocus, HomePosterGroup};
use crate::catalog::{MediaItem, MediaKind};
use crate::parental::item_is_shown;
use crate::theme::Palette;
use crate::worker::{HomeCatalogKind, HomeRowKind};

const HERO_HEIGHT: f32 = 340.0;

/// Load state of one row. A row keeps its last good cards while it reloads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowStatus {
    Idle,
    Loading,
    Ready,
    Failed(String),
}

/// One home-screen item with the derived text and progress the card shows.
#[derive(Clone)]
pub struct HomeCard {
    pub media: MediaItem,
    pub subtitle: Option<String>,
    pub progress: Option<f32>,
    /// The server's `OfficialRating`, used by the profile rating ceiling.
    pub official_rating: Option<String>,
}

pub struct HomeRowState {
    kind: HomeRowKind,
    pub status: RowStatus,
    request_id: u64,
    cards: Vec<HomeCard>,
    poster: PosterRow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HomeViewTarget {
    Movies,
    Series,
    Music,
    Playlists,
    Collections,
    Books,
    HomeVideos,
    MusicVideos,
    Photos,
    LiveTv,
    AllItems,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MyMediaTile {
    pub id: String,
    pub name: String,
    pub target: HomeViewTarget,
}

pub fn route_for_collection_type(collection_type: Option<&str>) -> HomeViewTarget {
    match collection_type {
        Some("movies") => HomeViewTarget::Movies,
        Some("tvshows") => HomeViewTarget::Series,
        Some("music") => HomeViewTarget::Music,
        Some("playlists") => HomeViewTarget::Playlists,
        Some("boxsets") => HomeViewTarget::Collections,
        Some("books") => HomeViewTarget::Books,
        Some("homevideos") => HomeViewTarget::HomeVideos,
        Some("musicvideos") => HomeViewTarget::MusicVideos,
        Some("photos") => HomeViewTarget::Photos,
        Some("livetv") => HomeViewTarget::LiveTv,
        _ => HomeViewTarget::AllItems,
    }
}

fn my_media_tile(view: &UserView) -> MyMediaTile {
    MyMediaTile {
        id: view.id.clone(),
        name: view
            .name
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .unwrap_or(ui_text(ids::HOME_LIBRARY_UNTITLED))
            .to_owned(),
        target: route_for_collection_type(view.collection_type.as_deref()),
    }
}

impl HomeRowState {
    fn new(kind: HomeRowKind) -> Self {
        Self {
            kind,
            status: RowStatus::Idle,
            request_id: 0,
            cards: Vec::new(),
            poster: PosterRow::default(),
        }
    }

    fn accepts(&self, request_id: u64) -> bool {
        self.status == RowStatus::Loading && self.request_id == request_id
    }
}

/// Which home sections the viewer chose to show (General settings).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HomeSection {
    ContinueWatching,
    NextUp,
    Latest,
    MyMedia,
    Collections,
}

impl HomeSection {
    fn from_setting(value: &str) -> Option<Self> {
        match value {
            kattri_settings::HOME_SECTION_CONTINUE_WATCHING => Some(Self::ContinueWatching),
            kattri_settings::HOME_SECTION_NEXT_UP => Some(Self::NextUp),
            kattri_settings::HOME_SECTION_LATEST => Some(Self::Latest),
            kattri_settings::HOME_SECTION_MY_MEDIA => Some(Self::MyMedia),
            kattri_settings::HOME_SECTION_COLLECTIONS => Some(Self::Collections),
            _ => None,
        }
    }

    const DEFAULT_ORDER: [Self; 5] = [
        Self::ContinueWatching,
        Self::NextUp,
        Self::Latest,
        Self::MyMedia,
        Self::Collections,
    ];
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HomeSections {
    pub continue_watching: bool,
    pub next_up: bool,
    pub combine_continue_next_up: bool,
    pub latest: bool,
    pub my_media: bool,
    pub collections: bool,
    pub order: [HomeSection; 5],
}

impl Default for HomeSections {
    fn default() -> Self {
        Self {
            continue_watching: true,
            next_up: true,
            combine_continue_next_up: false,
            latest: true,
            my_media: true,
            collections: true,
            order: HomeSection::DEFAULT_ORDER,
        }
    }
}

impl HomeSections {
    pub fn from_settings(general: &kattri_settings::GeneralSettings) -> Self {
        Self {
            continue_watching: general.home_continue_watching,
            next_up: general.home_next_up,
            combine_continue_next_up: general.home_combine_continue_next_up,
            latest: general.home_latest,
            my_media: general.home_my_media,
            collections: general.home_collections,
            order: {
                let mut order = Vec::with_capacity(HomeSection::DEFAULT_ORDER.len());
                for section in &general.home_section_order {
                    if let Some(section) = HomeSection::from_setting(section)
                        && !order.contains(&section)
                    {
                        order.push(section);
                    }
                }
                for section in HomeSection::DEFAULT_ORDER {
                    if !order.contains(&section) {
                        order.push(section);
                    }
                }
                order.try_into().unwrap_or(HomeSection::DEFAULT_ORDER)
            },
        }
    }

    pub fn shows_row(self, kind: HomeRowKind) -> bool {
        match kind {
            HomeRowKind::ContinueWatching => self.continue_watching,
            HomeRowKind::NextUp => self.next_up,
            HomeRowKind::Latest => self.latest,
        }
    }

    pub(crate) fn rows_hidden_by(self, next: Self) -> Vec<HomeRowKind> {
        HomeRowKind::ALL
            .into_iter()
            .filter(|kind| self.shows_row(*kind) && !next.shows_row(*kind))
            .collect()
    }

    pub fn shows_continue_next_up(self) -> bool {
        self.continue_watching || self.next_up
    }

    fn combined_row_slot(self) -> Option<HomeSection> {
        if !self.combine_continue_next_up {
            return None;
        }
        self.order.iter().copied().find(|section| match section {
            HomeSection::ContinueWatching => self.continue_watching,
            HomeSection::NextUp => self.next_up,
            HomeSection::Latest | HomeSection::MyMedia | HomeSection::Collections => false,
        })
    }

    pub fn shows_catalog(self, kind: HomeCatalogKind) -> bool {
        match kind {
            HomeCatalogKind::Libraries => self.my_media,
            HomeCatalogKind::Collections => self.collections,
        }
    }
}

/// Home screen data for the signed-in account. Reset on sign-out and on every new connection.
pub struct HomeState {
    /// The sections drawn; hidden ones are neither drawn nor loaded.
    pub sections: HomeSections,
    rows: [HomeRowState; 3],
    suppressed_series_ids: BTreeSet<String>,
    hero: HeroBanner,
    libraries_status: RowStatus,
    libraries_request_id: u64,
    libraries: Vec<UserView>,
    collections_status: RowStatus,
    collections_request_id: u64,
    collections: Vec<HomeCard>,
    collections_poster: PosterRow,
}

impl Default for HomeState {
    fn default() -> Self {
        Self {
            sections: HomeSections::default(),
            rows: HomeRowKind::ALL.map(HomeRowState::new),
            suppressed_series_ids: BTreeSet::new(),
            hero: HeroBanner::default(),
            libraries_status: RowStatus::Idle,
            libraries_request_id: 0,
            libraries: Vec::new(),
            collections_status: RowStatus::Idle,
            collections_request_id: 0,
            collections: Vec::new(),
            collections_poster: PosterRow::default(),
        }
    }
}

/// A user intent produced while drawing the home view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HomeAction {
    /// Open the details and playback-options flow for this item id.
    Select(String),
    /// Start playing this item now (resuming where it was left).
    Play(String),
    /// Hide a series from the Home episode shelf(s) for this server and profile.
    #[cfg(test)]
    HideSeries(String),
    /// Reload all three rows.
    Refresh,
    /// Reload one row after a failure.
    Retry(HomeRowKind),
    /// Reload My Media or Collections after a failure.
    RetryCatalog(HomeCatalogKind),
    /// Open a library view in the paged catalog.
    OpenLibrary(MyMediaTile),
    /// Browse the children of a box-set collection.
    OpenCollection { id: String, name: String },
}

impl HomeState {
    fn row(&self, kind: HomeRowKind) -> &HomeRowState {
        &self.rows[row_index(kind)]
    }

    fn row_mut(&mut self, kind: HomeRowKind) -> &mut HomeRowState {
        &mut self.rows[row_index(kind)]
    }

    /// Applies only GUIDs from the current server/profile scope. Missing identity disables hide.
    pub fn set_series_suppressions(
        &mut self,
        can_hide_series: bool,
        series_ids: impl IntoIterator<Item = String>,
    ) {
        self.suppressed_series_ids = if can_hide_series {
            series_ids
                .into_iter()
                .filter_map(|id| canonical_guid(&id))
                .collect()
        } else {
            BTreeSet::new()
        };
    }

    fn combined_cards(&self) -> Vec<&HomeCard> {
        combined_home_cards(
            &self.rows[row_index(HomeRowKind::ContinueWatching)].cards,
            &self.rows[row_index(HomeRowKind::NextUp)].cards,
            self.sections.continue_watching,
            self.sections.next_up,
            &self.suppressed_series_ids,
        )
    }

    fn hero_card(&self) -> Option<&HomeCard> {
        let combined_slot = self.sections.combined_row_slot();
        for section in self.sections.order {
            if combined_slot == Some(section) {
                if let Some(card) = self.combined_cards().first().copied() {
                    return Some(card);
                }
                continue;
            }
            let card = match section {
                HomeSection::ContinueWatching
                    if !self.sections.combine_continue_next_up
                        && self.sections.continue_watching =>
                {
                    visible_row_cards(
                        HomeRowKind::ContinueWatching,
                        &self.rows[row_index(HomeRowKind::ContinueWatching)].cards,
                        &self.suppressed_series_ids,
                    )
                    .into_iter()
                    .next()
                }
                HomeSection::NextUp
                    if !self.sections.combine_continue_next_up && self.sections.next_up =>
                {
                    visible_row_cards(
                        HomeRowKind::NextUp,
                        &self.rows[row_index(HomeRowKind::NextUp)].cards,
                        &self.suppressed_series_ids,
                    )
                    .into_iter()
                    .next()
                }
                HomeSection::Latest if self.sections.latest => {
                    self.rows[row_index(HomeRowKind::Latest)].cards.first()
                }
                HomeSection::ContinueWatching
                | HomeSection::NextUp
                | HomeSection::Latest
                | HomeSection::MyMedia
                | HomeSection::Collections => None,
            };
            if card.is_some() {
                return card;
            }
        }
        None
    }

    /// Marks a row as loading under `request_id`; only the matching response is applied.
    pub fn begin_load(&mut self, kind: HomeRowKind, request_id: u64) {
        let row = self.row_mut(kind);
        row.status = RowStatus::Loading;
        row.request_id = request_id;
    }

    /// Cancels a pending row without discarding its last successful cards.
    pub(crate) fn cancel_load(&mut self, kind: HomeRowKind) {
        let row = self.row_mut(kind);
        if row.status == RowStatus::Loading {
            row.status = RowStatus::Idle;
            row.request_id = 0;
        }
    }

    /// True when this exact row is still waiting for `request_id`.
    pub fn is_pending(&self, kind: HomeRowKind, request_id: u64) -> bool {
        self.row(kind).accepts(request_id)
    }

    /// Applies a successful row response. Returns false for stale or unknown responses.
    pub fn apply_items(
        &mut self,
        kind: HomeRowKind,
        request_id: u64,
        cards: Vec<HomeCard>,
    ) -> bool {
        let row = self.row_mut(kind);
        if !row.accepts(request_id) {
            return false;
        }
        row.cards = cards;
        row.status = RowStatus::Ready;
        true
    }

    /// Applies a failed row response, keeping the row's previous cards.
    /// Returns false for stale or unknown responses.
    pub fn apply_failure(&mut self, kind: HomeRowKind, request_id: u64, message: String) -> bool {
        let row = self.row_mut(kind);
        if !row.accepts(request_id) {
            return false;
        }
        row.status = RowStatus::Failed(message);
        true
    }

    pub fn begin_catalog_load(&mut self, kind: HomeCatalogKind, request_id: u64) {
        match kind {
            HomeCatalogKind::Libraries => {
                self.libraries_status = RowStatus::Loading;
                self.libraries_request_id = request_id;
            }
            HomeCatalogKind::Collections => {
                self.collections_status = RowStatus::Loading;
                self.collections_request_id = request_id;
            }
        }
    }

    pub fn is_catalog_pending(&self, kind: HomeCatalogKind, request_id: u64) -> bool {
        match kind {
            HomeCatalogKind::Libraries => {
                self.libraries_status == RowStatus::Loading
                    && self.libraries_request_id == request_id
            }
            HomeCatalogKind::Collections => {
                self.collections_status == RowStatus::Loading
                    && self.collections_request_id == request_id
            }
        }
    }

    pub fn apply_libraries(&mut self, request_id: u64, libraries: Vec<UserView>) -> bool {
        if !self.is_catalog_pending(HomeCatalogKind::Libraries, request_id) {
            return false;
        }
        self.libraries = libraries;
        self.libraries_status = RowStatus::Ready;
        true
    }

    pub fn apply_collections(&mut self, request_id: u64, collections: Vec<HomeCard>) -> bool {
        if !self.is_catalog_pending(HomeCatalogKind::Collections, request_id) {
            return false;
        }
        self.collections = collections;
        self.collections_status = RowStatus::Ready;
        true
    }

    pub fn apply_catalog_failure(
        &mut self,
        kind: HomeCatalogKind,
        request_id: u64,
        message: String,
    ) -> bool {
        if !self.is_catalog_pending(kind, request_id) {
            return false;
        }
        match kind {
            HomeCatalogKind::Libraries => self.libraries_status = RowStatus::Failed(message),
            HomeCatalogKind::Collections => self.collections_status = RowStatus::Failed(message),
        }
        true
    }

    /// The user's libraries once `GET /UserViews` has answered; `None` while unknown or failed.
    pub fn library_views(&self) -> Option<&[UserView]> {
        (self.libraries_status == RowStatus::Ready).then_some(self.libraries.as_slice())
    }

    pub fn my_media_tiles(&self) -> Vec<MyMediaTile> {
        self.libraries.iter().map(my_media_tile).collect()
    }

    pub fn my_media_empty_message(&self) -> Option<&'static str> {
        (self.libraries_status == RowStatus::Ready && self.libraries.is_empty())
            .then_some(ui_text(ids::HOME_EMPTY_NO_LIBRARIES))
    }

    /// Shown sections that were never requested, for the app to load.
    pub fn idle_sections(&self) -> (Vec<HomeRowKind>, Vec<HomeCatalogKind>) {
        let rows = self
            .rows
            .iter()
            .filter(|row| self.sections.shows_row(row.kind) && row.status == RowStatus::Idle)
            .map(|row| row.kind)
            .collect();
        let mut catalogs = Vec::new();
        if self.sections.my_media && self.libraries_status == RowStatus::Idle {
            catalogs.push(HomeCatalogKind::Libraries);
        }
        if self.sections.collections && self.collections_status == RowStatus::Idle {
            catalogs.push(HomeCatalogKind::Collections);
        }
        (rows, catalogs)
    }

    pub fn collections_are_visible(&self) -> bool {
        !(self.collections_status == RowStatus::Ready && self.collections.is_empty())
    }

    /// Applies a graph-selected index to the matching poster row next frame.
    pub(super) fn queue_poster_focus(&mut self, group: HomePosterGroup, index: usize) {
        match group {
            HomePosterGroup::Collections => self.collections_poster.queue_focus(index),
            HomePosterGroup::Row(kind) => self.row_mut(kind).poster.queue_focus(index),
        }
    }

    /// Every media entry on the home screen, for lazy artwork bookkeeping.
    pub fn media(&self) -> impl Iterator<Item = &MediaItem> {
        self.rows
            .iter()
            .flat_map(|row| row.cards.iter().map(|card| &card.media))
            .chain(self.collections.iter().map(|card| &card.media))
    }

    pub fn media_mut(&mut self) -> impl Iterator<Item = &mut MediaItem> {
        self.rows
            .iter_mut()
            .flat_map(|row| row.cards.iter_mut().map(|card| &mut card.media))
            .chain(self.collections.iter_mut().map(|card| &mut card.media))
    }
    /// Drops `item_id` from every row, after the server deleted it.
    pub fn remove_item(&mut self, item_id: &str) {
        for row in &mut self.rows {
            row.cards.retain(|card| card.media.id != item_id);
        }
        self.collections.retain(|card| card.media.id != item_id);
    }
}

impl HomeState {
    /// Id of the item the hero shows, so the app can fetch its backdrop.
    pub fn hero_item_id(&self) -> Option<String> {
        self.hero_card().map(|card| card.media.backdrop_id.clone())
    }

    pub fn hero_artwork(&self) -> Option<egui::load::SizedTexture> {
        self.hero_card().map(card_view).and_then(|card| card.image)
    }
}

/// Builds the combined shelf in source order, keeping the first item for each item and series.
fn combined_home_cards<'a>(
    continue_watching: &'a [HomeCard],
    next_up: &'a [HomeCard],
    include_continue_watching: bool,
    include_next_up: bool,
    suppressed_series_ids: &BTreeSet<String>,
) -> Vec<&'a HomeCard> {
    let mut item_ids = BTreeSet::new();
    let mut series_ids = BTreeSet::new();
    let mut combined = Vec::new();
    for card in continue_watching
        .iter()
        .filter(|_| include_continue_watching)
        .chain(next_up.iter().filter(|_| include_next_up))
    {
        let series_id = card.media.series_id.as_deref().and_then(canonical_guid);
        if card_is_suppressed(card, suppressed_series_ids) {
            continue;
        }
        let item_id = canonical_guid(&card.media.id).unwrap_or_else(|| card.media.id.clone());
        if !item_ids.insert(item_id) {
            continue;
        }
        if let Some(series_id) = series_id
            && !series_ids.insert(series_id)
        {
            continue;
        }
        combined.push(card);
    }
    combined
}

fn card_is_suppressed(card: &HomeCard, suppressed_series_ids: &BTreeSet<String>) -> bool {
    card.media
        .series_id
        .as_deref()
        .and_then(canonical_guid)
        .is_some_and(|series_id| suppressed_series_ids.contains(&series_id))
}

fn visible_row_cards<'a>(
    kind: HomeRowKind,
    cards: &'a [HomeCard],
    suppressed_series_ids: &BTreeSet<String>,
) -> Vec<&'a HomeCard> {
    cards
        .iter()
        .filter(|card| {
            !matches!(kind, HomeRowKind::ContinueWatching | HomeRowKind::NextUp)
                || !card_is_suppressed(card, suppressed_series_ids)
        })
        .collect()
}

fn canonical_guid(raw: &str) -> Option<String> {
    let id = uuid::Uuid::parse_str(raw).ok()?;
    (!id.is_nil()).then(|| id.to_string())
}

fn row_index(kind: HomeRowKind) -> usize {
    match kind {
        HomeRowKind::ContinueWatching => 0,
        HomeRowKind::NextUp => 1,
        HomeRowKind::Latest => 2,
    }
}

pub fn title_for(kind: HomeRowKind) -> &'static str {
    match kind {
        HomeRowKind::ContinueWatching => ui_text(ids::HOME_ROW_CONTINUE_WATCHING),
        HomeRowKind::NextUp => ui_text(ids::HOME_ROW_NEXT_UP),
        HomeRowKind::Latest => ui_text(ids::HOME_ROW_LATEST),
    }
}

fn empty_message_for(kind: HomeRowKind) -> &'static str {
    match kind {
        HomeRowKind::ContinueWatching => ui_text(ids::HOME_EMPTY_CONTINUE),
        HomeRowKind::NextUp => ui_text(ids::HOME_EMPTY_NEXT_UP),
        HomeRowKind::Latest => ui_text(ids::HOME_EMPTY_LATEST),
    }
}

/// Maps one API item into a home card. Consumes the item because the media entry owns it.
pub fn card_from_item(item: BaseItemDto) -> HomeCard {
    let played_percentage = item
        .user_data
        .as_ref()
        .and_then(|data| data.played_percentage);
    let position_ticks = item
        .user_data
        .as_ref()
        .and_then(|data| data.playback_position_ticks);
    let progress = progress_fraction(played_percentage, position_ticks, item.run_time_ticks);
    let subtitle = card_subtitle(
        item.item_type.as_deref(),
        item.series_name.as_deref(),
        item.parent_index_number,
        item.index_number,
        item.production_year,
    );
    let official_rating = item.official_rating.clone();
    HomeCard {
        media: MediaItem::from_api(item),
        subtitle,
        progress,
        official_rating,
    }
}

fn combined_home_title(sections: HomeSections) -> String {
    match (sections.continue_watching, sections.next_up) {
        (true, true) => format!(
            "{} + {}",
            ui_text(ids::HOME_ROW_CONTINUE_WATCHING),
            ui_text(ids::HOME_ROW_NEXT_UP)
        ),
        (true, false) => ui_text(ids::HOME_ROW_CONTINUE_WATCHING).to_owned(),
        (false, true) => ui_text(ids::HOME_ROW_NEXT_UP).to_owned(),
        (false, false) => String::new(),
    }
}

fn render_combined_source_status(
    ui: &mut egui::Ui,
    kind: HomeRowKind,
    status: &RowStatus,
    colors: Palette,
    action: &mut Option<HomeAction>,
) {
    let title = title_for(kind);
    match status {
        RowStatus::Loading => {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(
                    RichText::new(ui_text(ids::HOME_STATUS_REFRESHING))
                        .size(11.0)
                        .color(colors.muted),
                );
            });
        }
        RowStatus::Failed(message) => {
            failure_line(ui, title, message, colors);
            if ui
                .button(ui_text_args(
                    ids::HOME_BUTTON_RETRY_TITLE,
                    &[("title", title)],
                ))
                .clicked()
                && action.is_none()
            {
                *action = Some(HomeAction::Retry(kind));
            }
        }
        RowStatus::Idle | RowStatus::Ready => {}
    }
}

/// Keeps the cards that stay visible under the profile's rating `ceiling`, in their order.
/// Called where a home row is built, so hidden items never reach the row.
pub fn cards_visible_under(cards: Vec<HomeCard>, ceiling: Option<u32>) -> Vec<HomeCard> {
    cards
        .into_iter()
        .filter(|card| item_is_shown(ceiling, card.official_rating.as_deref()))
        .collect()
}

/// Watched fraction in `0..=1`. Prefers Jellyfin's `PlayedPercentage`, then position over
/// runtime. Returns `None` when neither gives a usable value.
pub fn progress_fraction(
    played_percentage: Option<f64>,
    position_ticks: Option<i64>,
    runtime_ticks: Option<i64>,
) -> Option<f32> {
    if let Some(percent) = played_percentage.filter(|percent| percent.is_finite() && *percent > 0.0)
    {
        return Some((percent / 100.0).clamp(0.0, 1.0) as f32);
    }
    let position = position_ticks.filter(|ticks| *ticks > 0)?;
    let runtime = runtime_ticks.filter(|ticks| *ticks > 0)?;
    Some((position as f64 / runtime as f64).clamp(0.0, 1.0) as f32)
}

/// `S1E2` style code. Season and episode are both required for the full form.
pub fn episode_code(season: Option<i32>, episode: Option<i32>) -> Option<String> {
    match (season, episode) {
        (Some(season), Some(episode)) => Some(format!("S{season}E{episode}")),
        (None, Some(episode)) => Some(format!("E{episode}")),
        _ => None,
    }
}

/// Episodes show `Series · S1E2`; other items show their production year.
pub fn card_subtitle(
    item_type: Option<&str>,
    series_name: Option<&str>,
    season: Option<i32>,
    episode: Option<i32>,
    year: Option<i32>,
) -> Option<String> {
    let series = series_name.map(str::trim).filter(|name| !name.is_empty());
    if item_type == Some("Episode") || series.is_some() {
        return match (series, episode_code(season, episode)) {
            (Some(series), Some(code)) => Some(format!("{series} · {code}")),
            (Some(series), None) => Some(series.to_owned()),
            (None, Some(code)) => Some(code),
            (None, None) => None,
        };
    }
    year.filter(|year| *year > 0).map(|year| year.to_string())
}

fn accessible_card_name(card: &HomeCard) -> String {
    let kind = ui_text(match card.media.kind {
        MediaKind::Movie => ids::HOME_MEDIA_KIND_MOVIE,
        MediaKind::Series => ids::HOME_MEDIA_KIND_SERIES,
        MediaKind::Episode => ids::HOME_MEDIA_KIND_EPISODE,
        MediaKind::Audio => ids::HOME_MEDIA_KIND_AUDIO,
        MediaKind::Photo => ids::HOME_MEDIA_KIND_PHOTO,
        MediaKind::Playlist => ids::HOME_MEDIA_KIND_PLAYLIST,
        MediaKind::Folder => ids::HOME_MEDIA_KIND_FOLDER,
        MediaKind::Video => ids::HOME_MEDIA_KIND_VIDEO,
        MediaKind::Book => ids::HOME_MEDIA_KIND_BOOK,
        MediaKind::Other => ids::HOME_MEDIA_KIND_OTHER,
    });
    if let Some(progress) = card.progress.filter(|progress| progress.is_finite()) {
        let progress = (progress.clamp(0.0, 1.0) * 100.0).round().to_string();
        ui_text_args(
            ids::ROW_CARD_ACCESSIBLE_NAME_PROGRESS,
            &[
                ("title", &card.media.title),
                ("kind", kind),
                ("progress", &progress),
            ],
        )
    } else {
        ui_text_args(
            ids::ROW_CARD_ACCESSIBLE_NAME,
            &[("title", &card.media.title), ("kind", kind)],
        )
    }
}

fn card_view(card: &HomeCard) -> RowCard {
    RowCard {
        id: card.media.id.clone(),
        title: card.media.title.clone(),
        accessible_name: Some(accessible_card_name(card)),
        subtitle: card.subtitle.clone(),
        progress: card.progress,
        image: card
            .media
            .artwork_texture
            .as_ref()
            .map(|texture| SizedTexture::new(texture.id(), texture.size_vec2())),
    }
}

/// Draws the hero and the three rows. Artwork requests for visible cards are appended to
/// `visible_artwork`; the caller forwards them to the shared lazy-artwork scheduler.
pub(super) fn show(
    ui: &mut egui::Ui,
    state: &mut HomeState,
    colors: Palette,
    focus_entries: &mut Vec<CardFocus>,
    visible_artwork: &mut Vec<(String, String)>,
    hero_backdrop: Option<SizedTexture>,
) -> Option<HomeAction> {
    let mut action = None;
    let suppressed_series_ids = state.suppressed_series_ids.clone();
    let sections = state.sections;
    let combined_cards: Vec<HomeCard> = if sections.combine_continue_next_up {
        state.combined_cards().into_iter().cloned().collect()
    } else {
        Vec::new()
    };
    let hero_card = state.hero_card().cloned();
    let library_tiles: Vec<_> = state
        .my_media_tiles()
        .into_iter()
        .filter(|tile| tile.target == HomeViewTarget::AllItems)
        .collect();
    let library_empty_message = state.my_media_empty_message();
    let show_collections = state.collections_are_visible();
    let HomeState {
        rows,
        hero,
        libraries,
        libraries_status,
        collections,
        collections_status,
        collections_poster,
        ..
    } = state;

    if let Some(card) = hero_card.as_ref() {
        let overview = card
            .media
            .overview
            .as_deref()
            .map(str::trim)
            .filter(|text| !text.is_empty());
        let item = HeroItem {
            id: &card.media.id,
            title: &card.media.title,
            overview,
            // Episodes and some titles have no backdrop of their own; their primary artwork
            // (a 16:9 still for episodes) is the fallback.
            backdrop: hero_backdrop.or_else(|| card_view(card).image),
        };
        let options = HeroOptions {
            height: (ui.ctx().content_rect().height() * 0.6).clamp(HERO_HEIGHT, 640.0),
            palette: colors,
            ..HeroOptions::default()
        };
        let response = hero.show(ui, &item, &options);
        if response.play {
            action = action.or_else(|| Some(HomeAction::Play(card.media.id.clone())));
        } else if response.info {
            action = action.or_else(|| Some(HomeAction::Select(card.media.id.clone())));
        }
    }

    macro_rules! render_row {
        ($selected_kind:expr, $focus_row:expr) => {{
            for row in rows
                .iter_mut()
                .filter(|row| row.kind == $selected_kind && sections.shows_row(row.kind))
            {
                let kind = row.kind;
                let title = title_for(kind);
                let visible_cards = visible_row_cards(kind, &row.cards, &suppressed_series_ids);
                if matches!(kind, HomeRowKind::ContinueWatching | HomeRowKind::NextUp)
                    && !row.cards.is_empty()
                    && visible_cards.is_empty()
                    && row.status == RowStatus::Ready
                {
                    continue;
                }
                ui.add_space(26.0);

                if visible_cards.is_empty() {
                    ui.label(RichText::new(title).size(22.0).color(colors.text));
                    ui.add_space(8.0);
                    match &row.status {
                        RowStatus::Idle | RowStatus::Loading => {
                            ui.horizontal(|ui| {
                                ui.spinner();
                                ui.label(
                                    RichText::new(ui_text_args(
                                        ids::HOME_STATUS_LOADING_TITLE,
                                        &[("title", title)],
                                    ))
                                    .color(colors.muted),
                                );
                            });
                        }
                        RowStatus::Ready => {
                            ui.label(RichText::new(empty_message_for(kind)).color(colors.muted));
                        }
                        RowStatus::Failed(message) => {
                            failure_line(ui, title, message, colors);
                            if ui
                                .button(ui_text_args(
                                    ids::HOME_BUTTON_RETRY_TITLE,
                                    &[("title", title)],
                                ))
                                .clicked()
                            {
                                action = action.or(Some(HomeAction::Retry(kind)));
                            }
                        }
                    }
                    continue;
                }

                let cards: Vec<RowCard> =
                    visible_cards.iter().map(|card| card_view(card)).collect();
                let options = RowOptions {
                    card_width: if kind == HomeRowKind::Latest {
                        190.0
                    } else {
                        216.0
                    },
                    image_aspect: if kind == HomeRowKind::Latest {
                        1.5
                    } else {
                        2.0 / 3.0
                    },
                    palette: colors,
                    ..RowOptions::default()
                };
                let response = row.poster.show(ui, title, &cards, &options);
                if let Some(card) = response
                    .menu_card
                    .and_then(|index| visible_cards.get(index).copied())
                {
                    super::item_menu::attach(&response.focus_response, &card.media);
                }
                let focus_row = $focus_row;
                for (index, card) in visible_cards.iter().enumerate() {
                    let focus_response = response.focus_response.clone();
                    focus_entries.push(CardFocus {
                        row: focus_row,
                        id: FocusId::new(format!("home:row:{kind:?}:{}", card.media.id)),
                        center_x: response.card_centers[index],
                        focused: focus_response.has_focus() && response.focused == Some(index),
                        response: focus_response,
                        home_poster: Some((HomePosterGroup::Row(kind), index)),
                        selection: HomeAction::Select(card.media.id.clone()),
                    });
                }
                for index in response.visible.clone() {
                    if let Some(card) = visible_cards.get(index)
                        && let Some(tag) = card.media.primary_image_tag.as_deref()
                    {
                        visible_artwork.push((card.media.artwork_id.clone(), tag.to_owned()));
                    }
                }
                if let Some(index) = response.activated
                    && let Some(card) = visible_cards.get(index)
                {
                    action = action.or_else(|| Some(HomeAction::Select(card.media.id.clone())));
                }

                match &row.status {
                    RowStatus::Loading => {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(
                                RichText::new(ui_text(ids::HOME_STATUS_REFRESHING))
                                    .size(11.0)
                                    .color(colors.muted),
                            );
                        });
                    }
                    RowStatus::Failed(message) => {
                        failure_line(ui, title, message, colors);
                        if ui
                            .button(ui_text_args(
                                ids::HOME_BUTTON_RETRY_TITLE,
                                &[("title", title)],
                            ))
                            .clicked()
                        {
                            action = action.or(Some(HomeAction::Retry(kind)));
                        }
                    }
                    RowStatus::Idle | RowStatus::Ready => {}
                }
            }
        }};
    }
    macro_rules! render_my_media {
        ($focus_row:expr) => {{
            if sections.my_media && (!library_tiles.is_empty() || libraries.is_empty()) {
                ui.add_space(24.0);
                ui.label(
                    RichText::new(ui_text(ids::HOME_HEADING_MY_MEDIA))
                        .size(22.0)
                        .color(colors.text),
                );
                ui.add_space(8.0);
                if !libraries.is_empty() {
                    egui::ScrollArea::horizontal()
                        .id_salt("home_my_media")
                        .auto_shrink([false, true])
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                for tile in &library_tiles {
                                    let response = ui.add_sized(
                                        egui::vec2(176.0, 96.0),
                                        egui::Button::new(
                                            RichText::new(&tile.name)
                                                .size(18.0)
                                                .strong()
                                                .color(colors.text),
                                        )
                                        .fill(colors.panel)
                                        .stroke(Stroke::new(1.0, colors.divider))
                                        .corner_radius(CornerRadius::same(16)),
                                    );
                                    focus_entries.push(CardFocus {
                                        row: $focus_row,
                                        id: FocusId::new(format!("home:library:{}", tile.id)),
                                        center_x: response.rect.center().x,
                                        focused: response.has_focus(),
                                        response: response.clone(),
                                        home_poster: None,
                                        selection: HomeAction::OpenLibrary(tile.clone()),
                                    });
                                    if response.clicked() && action.is_none() {
                                        action = Some(HomeAction::OpenLibrary(tile.clone()));
                                    }
                                    ui.add_space(8.0);
                                }
                            });
                        });
                }
                match libraries_status {
                    RowStatus::Idle | RowStatus::Loading if libraries.is_empty() => {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(
                                RichText::new(ui_text(ids::HOME_STATUS_LOADING_LIBRARIES))
                                    .color(colors.muted),
                            );
                        });
                    }
                    RowStatus::Idle | RowStatus::Loading => {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(
                                RichText::new(ui_text(ids::HOME_STATUS_REFRESHING_LIBRARIES))
                                    .color(colors.muted),
                            );
                        });
                    }
                    RowStatus::Ready if libraries.is_empty() => {
                        ui.label(
                            RichText::new(
                                library_empty_message
                                    .unwrap_or(ui_text(ids::HOME_EMPTY_NO_LIBRARIES)),
                            )
                            .color(colors.muted),
                        );
                    }
                    RowStatus::Ready => {}
                    RowStatus::Failed(message) => {
                        failure_line(ui, ui_text(ids::HOME_HEADING_MY_MEDIA), message, colors);
                        if ui
                            .button(ui_text(ids::HOME_BUTTON_RETRY_MY_MEDIA))
                            .clicked()
                        {
                            action = action
                                .or(Some(HomeAction::RetryCatalog(HomeCatalogKind::Libraries)));
                        }
                    }
                }
            }
        }};
    }

    macro_rules! render_continue_next_up {
        ($focus_row:expr) => {{
            if sections.shows_continue_next_up() {
                let title = combined_home_title(sections);
                let active_kinds: Vec<HomeRowKind> = [
                    (HomeRowKind::ContinueWatching, sections.continue_watching),
                    (HomeRowKind::NextUp, sections.next_up),
                ]
                .into_iter()
                .filter_map(|(kind, enabled)| enabled.then_some(kind))
                .collect();
                let active_statuses: Vec<(HomeRowKind, RowStatus)> = active_kinds
                    .iter()
                    .map(|kind| (*kind, rows[row_index(*kind)].status.clone()))
                    .collect();

                if !combined_cards.is_empty() {
                    let cards: Vec<RowCard> = combined_cards.iter().map(card_view).collect();
                    ui.add_space(26.0);
                    let response = rows[row_index(HomeRowKind::ContinueWatching)].poster.show(
                        ui,
                        &title,
                        &cards,
                        &RowOptions {
                            card_width: 216.0,
                            image_aspect: 2.0 / 3.0,
                            palette: colors,
                            ..RowOptions::default()
                        },
                    );
                    if let Some(card) = response
                        .menu_card
                        .and_then(|index| combined_cards.get(index))
                    {
                        super::item_menu::attach(&response.focus_response, &card.media);
                    }
                    for (index, card) in combined_cards.iter().enumerate() {
                        let focus_response = response.focus_response.clone();
                        focus_entries.push(CardFocus {
                            row: $focus_row,
                            id: FocusId::new(format!(
                                "home:row:continue_next_up:{}",
                                card.media.id
                            )),
                            center_x: response.card_centers[index],
                            focused: focus_response.has_focus() && response.focused == Some(index),
                            response: focus_response,
                            home_poster: Some((
                                HomePosterGroup::Row(HomeRowKind::ContinueWatching),
                                index,
                            )),
                            selection: HomeAction::Select(card.media.id.clone()),
                        });
                    }
                    for index in response.visible.clone() {
                        if let Some(card) = combined_cards.get(index)
                            && let Some(tag) = card.media.primary_image_tag.as_deref()
                        {
                            visible_artwork.push((card.media.artwork_id.clone(), tag.to_owned()));
                        }
                    }
                    if let Some(index) = response.activated
                        && let Some(card) = combined_cards.get(index)
                    {
                        action = action.or_else(|| Some(HomeAction::Select(card.media.id.clone())));
                    }

                    for (kind, status) in &active_statuses {
                        render_combined_source_status(ui, *kind, status, colors, &mut action);
                    }
                } else {
                    let mut laid_out = false;
                    for (kind, status) in &active_statuses {
                        match status {
                            RowStatus::Idle | RowStatus::Loading => {
                                if !laid_out {
                                    ui.add_space(26.0);
                                }
                                ui.label(RichText::new(&title).size(22.0).color(colors.text));
                                ui.horizontal(|ui| {
                                    ui.spinner();
                                    ui.label(
                                        RichText::new(ui_text_args(
                                            ids::HOME_STATUS_LOADING_TITLE,
                                            &[("title", title.as_str())],
                                        ))
                                        .color(colors.muted),
                                    );
                                });
                                break;
                            }
                            RowStatus::Failed(message) => {
                                if !laid_out {
                                    ui.add_space(26.0);
                                    laid_out = true;
                                }
                                let source_title = title_for(*kind);
                                failure_line(ui, source_title, message, colors);
                                if ui
                                    .button(ui_text_args(
                                        ids::HOME_BUTTON_RETRY_TITLE,
                                        &[("title", source_title)],
                                    ))
                                    .clicked()
                                {
                                    action = action.or(Some(HomeAction::Retry(*kind)));
                                }
                            }
                            RowStatus::Ready => {}
                        }
                    }
                }
            }
        }};
    }
    macro_rules! render_collections {
        ($focus_row:expr) => {{
            if show_collections && sections.collections {
                ui.add_space(24.0);
                if !collections.is_empty() {
                    let cards: Vec<RowCard> = collections.iter().map(card_view).collect();
                    let response = collections_poster.show(
                        ui,
                        ui_text(ids::HOME_HEADING_COLLECTIONS),
                        &cards,
                        &RowOptions {
                            palette: colors,
                            ..RowOptions::default()
                        },
                    );
                    for (index, card) in collections.iter().enumerate() {
                        let focus_response = response.focus_response.clone();
                        focus_entries.push(CardFocus {
                            row: $focus_row,
                            id: FocusId::new(format!("home:collection:{}", card.media.id)),
                            center_x: response.card_centers[index],
                            focused: focus_response.has_focus() && response.focused == Some(index),
                            response: focus_response,
                            home_poster: Some((HomePosterGroup::Collections, index)),
                            selection: HomeAction::OpenCollection {
                                id: card.media.id.clone(),
                                name: card.media.title.clone(),
                            },
                        });
                    }
                    for index in response.visible.clone() {
                        if let Some(card) = collections.get(index)
                            && let Some(tag) = card.media.primary_image_tag.as_deref()
                        {
                            visible_artwork.push((card.media.artwork_id.clone(), tag.to_owned()));
                        }
                    }
                    if let Some(index) = response.activated
                        && let Some(card) = collections.get(index)
                    {
                        action = action.or(Some(HomeAction::OpenCollection {
                            id: card.media.id.clone(),
                            name: card.media.title.clone(),
                        }));
                    }
                }
                match collections_status {
                    RowStatus::Idle | RowStatus::Loading if collections.is_empty() => {
                        ui.label(
                            RichText::new(ui_text(ids::HOME_HEADING_COLLECTIONS))
                                .size(22.0)
                                .color(colors.text),
                        );
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(
                                RichText::new(ui_text(ids::HOME_STATUS_LOADING_COLLECTIONS))
                                    .color(colors.muted),
                            );
                        });
                    }
                    RowStatus::Idle | RowStatus::Loading => {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(
                                RichText::new(ui_text(ids::HOME_STATUS_REFRESHING_COLLECTIONS))
                                    .color(colors.muted),
                            );
                        });
                    }
                    RowStatus::Ready => {}
                    RowStatus::Failed(message) => {
                        if collections.is_empty() {
                            ui.label(
                                RichText::new(ui_text(ids::HOME_HEADING_COLLECTIONS))
                                    .size(22.0)
                                    .color(colors.text),
                            );
                        }
                        failure_line(ui, ui_text(ids::HOME_HEADING_COLLECTIONS), message, colors);
                        if ui
                            .button(ui_text(ids::HOME_BUTTON_RETRY_COLLECTIONS))
                            .clicked()
                        {
                            action = action
                                .or(Some(HomeAction::RetryCatalog(HomeCatalogKind::Collections)));
                        }
                    }
                }
            }
        }};
    }

    let combined_row_slot = sections.combined_row_slot();
    for (section_index, section) in sections.order.iter().enumerate() {
        match section {
            HomeSection::ContinueWatching if sections.combine_continue_next_up => {
                if combined_row_slot == Some(HomeSection::ContinueWatching) {
                    render_continue_next_up!(section_index);
                }
            }
            HomeSection::NextUp if sections.combine_continue_next_up => {
                if combined_row_slot == Some(HomeSection::NextUp) {
                    render_continue_next_up!(section_index);
                }
            }
            HomeSection::ContinueWatching => {
                render_row!(HomeRowKind::ContinueWatching, section_index)
            }
            HomeSection::NextUp => render_row!(HomeRowKind::NextUp, section_index),
            HomeSection::Latest => render_row!(HomeRowKind::Latest, section_index),
            HomeSection::MyMedia => render_my_media!(section_index),
            HomeSection::Collections => render_collections!(section_index),
        }
    }

    action
}

fn failure_line(ui: &mut egui::Ui, title: &str, message: &str, colors: Palette) {
    egui::Frame::new()
        .fill(colors.danger.gamma_multiply(0.14))
        .stroke(Stroke::new(1.0, colors.danger))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(egui::Margin::same(10))
        .show(ui, |ui| {
            ui.label(
                RichText::new(ui_text_args(
                    ids::HOME_ERROR_LOAD_TITLE,
                    &[("title", title), ("message", message)],
                ))
                .size(12.0)
                .color(colors.danger),
            );
        });
    ui.add_space(6.0);
}
