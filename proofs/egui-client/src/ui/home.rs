//! Home view: Continue Watching, Next Up and Latest rows under a hero banner.
//!
//! State transitions and item mapping are plain Rust so they can be unit-tested
//! without egui. Rendering only reads the state and reports at most one
//! [`HomeAction`]; the caller applies it after drawing, so no app borrow is held
//! while the frame is built.

use super::{ui_text, ui_text_args};
use egui::{CornerRadius, RichText, Stroke, load::SizedTexture};
use jellyfin_api::{BaseItemDto, UserView};
use kattri_client_core::focus::FocusId;
use kattri_i18n::ids;

use super::hero::{HeroBanner, HeroItem, HeroOptions};
use super::row::{PosterRow, RowCard, RowOptions};
use super::{CardFocus, HomePosterGroup};
use crate::catalog::MediaItem;
use crate::parental::item_is_shown;
use crate::theme::Palette;
use crate::worker::{HomeCatalogKind, HomeRowKind};

const HERO_HEIGHT: f32 = 340.0;
const ERROR_TEXT: egui::Color32 = egui::Color32::from_rgb(0xF0, 0xA0, 0x99);

/// Load state of one row. A row keeps its last good cards while it reloads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowStatus {
    Idle,
    Loading,
    Ready,
    Failed(String),
}

/// One home-screen item with the derived text and progress the card shows.
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

/// Home screen data for the signed-in account. Reset on sign-out and on every new connection.
pub struct HomeState {
    rows: [HomeRowState; 3],
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
            rows: HomeRowKind::ALL.map(HomeRowState::new),
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

    /// Marks a row as loading under `request_id`; only the matching response is applied.
    pub fn begin_load(&mut self, kind: HomeRowKind, request_id: u64) {
        let row = self.row_mut(kind);
        row.status = RowStatus::Loading;
        row.request_id = request_id;
    }

    /// True when `request_id` belongs to a row that is still waiting for its response.
    pub fn owns_request(&self, request_id: u64) -> bool {
        self.rows.iter().any(|row| row.accepts(request_id))
            || self.is_catalog_pending(HomeCatalogKind::Libraries, request_id)
            || self.is_catalog_pending(HomeCatalogKind::Collections, request_id)
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

    pub fn my_media_tiles(&self) -> Vec<MyMediaTile> {
        self.libraries.iter().map(my_media_tile).collect()
    }

    pub fn my_media_empty_message(&self) -> Option<&'static str> {
        (self.libraries_status == RowStatus::Ready && self.libraries.is_empty())
            .then_some(ui_text(ids::HOME_EMPTY_NO_LIBRARIES))
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
}

impl HomeState {
    /// Id of the item the hero shows, so the app can fetch its backdrop.
    pub fn hero_item_id(&self) -> Option<String> {
        hero_card(&self.rows).map(|card| card.media.id.clone())
    }
}

/// The hero shows the first Continue Watching item, else the first Latest item.
fn hero_card(rows: &[HomeRowState; 3]) -> Option<&HomeCard> {
    rows[row_index(HomeRowKind::ContinueWatching)]
        .cards
        .first()
        .or_else(|| rows[row_index(HomeRowKind::Latest)].cards.first())
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

fn card_view(card: &HomeCard) -> RowCard {
    RowCard {
        id: card.media.id.clone(),
        title: card.media.title.clone(),
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
    let library_tiles = state.my_media_tiles();
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

    if let Some(card) = hero_card(rows) {
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

    for row in rows.iter_mut() {
        let kind = row.kind;
        let title = title_for(kind);
        ui.add_space(26.0);

        if row.cards.is_empty() {
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
                    failure_line(ui, title, message);
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

        let cards: Vec<RowCard> = row.cards.iter().map(card_view).collect();
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
        let focus_row = match kind {
            HomeRowKind::ContinueWatching => 0,
            HomeRowKind::NextUp => 1,
            HomeRowKind::Latest => 2,
        };
        for (index, card) in row.cards.iter().enumerate() {
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
            if let Some(card) = row.cards.get(index)
                && let Some(tag) = card.media.primary_image_tag.as_deref()
            {
                visible_artwork.push((card.media.id.clone(), tag.to_owned()));
            }
        }
        if let Some(index) = response.activated
            && let Some(card) = row.cards.get(index)
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
                failure_line(ui, title, message);
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
                    for tile in library_tiles {
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
                            row: 3,
                            id: FocusId::new(format!("home:library:{}", tile.id)),
                            center_x: response.rect.center().x,
                            focused: response.has_focus(),
                            response: response.clone(),
                            home_poster: None,
                            selection: HomeAction::OpenLibrary(tile.clone()),
                        });
                        if response.clicked() && action.is_none() {
                            action = Some(HomeAction::OpenLibrary(tile));
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
                    RichText::new(ui_text(ids::HOME_STATUS_LOADING_LIBRARIES)).color(colors.muted),
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
                    library_empty_message.unwrap_or(ui_text(ids::HOME_EMPTY_NO_LIBRARIES)),
                )
                .color(colors.muted),
            );
        }
        RowStatus::Ready => {}
        RowStatus::Failed(message) => {
            failure_line(ui, ui_text(ids::HOME_HEADING_MY_MEDIA), message);
            if ui
                .button(ui_text(ids::HOME_BUTTON_RETRY_MY_MEDIA))
                .clicked()
            {
                action = action.or(Some(HomeAction::RetryCatalog(HomeCatalogKind::Libraries)));
            }
        }
    }

    if show_collections {
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
                    row: 4,
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
                    visible_artwork.push((card.media.id.clone(), tag.to_owned()));
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
                failure_line(ui, ui_text(ids::HOME_HEADING_COLLECTIONS), message);
                if ui
                    .button(ui_text(ids::HOME_BUTTON_RETRY_COLLECTIONS))
                    .clicked()
                {
                    action =
                        action.or(Some(HomeAction::RetryCatalog(HomeCatalogKind::Collections)));
                }
            }
        }
    }

    action
}

fn failure_line(ui: &mut egui::Ui, title: &str, message: &str) {
    egui::Frame::new()
        .fill(egui::Color32::from_rgb(0x3A, 0x25, 0x2A))
        .stroke(Stroke::new(1.0, egui::Color32::from_rgb(0xD9, 0x7F, 0x78)))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(egui::Margin::same(10))
        .show(ui, |ui| {
            ui.label(
                RichText::new(ui_text_args(
                    ids::HOME_ERROR_LOAD_TITLE,
                    &[("title", title), ("message", message)],
                ))
                .size(12.0)
                .color(ERROR_TEXT),
            );
        });
    ui.add_space(6.0);
}

#[cfg(test)]
mod tests {
    use super::*;
    use jellyfin_api::{ClientConfig, JellyfinClient};
    use mock_jellyfin::{MockConfig, spawn};
    use serde_json::json;

    fn item(value: serde_json::Value) -> BaseItemDto {
        serde_json::from_value(value).expect("fixture parses as BaseItemDto")
    }

    fn card(id: &str) -> HomeCard {
        card_from_item(item(json!({ "Id": id, "Name": format!("Title {id}") })))
    }

    #[test]
    fn card_carries_the_server_official_rating() {
        let rated = card_from_item(item(json!({
            "Id": "r1",
            "Name": "Rated",
            "OfficialRating": "PG-13"
        })));
        assert_eq!(rated.official_rating.as_deref(), Some("PG-13"));
        assert_eq!(card("plain").official_rating, None);
    }

    #[test]
    fn a_filtered_row_hides_items_above_the_ceiling() {
        let cards = vec![
            card_from_item(item(
                json!({ "Id": "g", "Name": "G", "OfficialRating": "G" }),
            )),
            card_from_item(item(
                json!({ "Id": "r", "Name": "R", "OfficialRating": "R" }),
            )),
            card("bare"),
            card_from_item(item(
                json!({ "Id": "u", "Name": "U", "OfficialRating": "Unrated" }),
            )),
        ];
        let shown: Vec<String> = cards_visible_under(cards, Some(2))
            .into_iter()
            .map(|card| card.media.id)
            .collect();
        assert_eq!(shown, ["g", "bare"]);
    }

    fn client_for(base_url: &str) -> JellyfinClient {
        let config = ClientConfig::new(
            base_url,
            "Kattri test",
            "test device",
            "device-id",
            "test-version",
        )
        .expect("valid mock client config");
        JellyfinClient::new(config).expect("mock loopback server is allowed")
    }

    #[test]
    fn view_type_router_chooses_expected_home_target() {
        for (collection_type, expected) in [
            (Some("movies"), HomeViewTarget::Movies),
            (Some("tvshows"), HomeViewTarget::Series),
            (Some("music"), HomeViewTarget::Music),
            (Some("playlists"), HomeViewTarget::Playlists),
            (Some("unknown"), HomeViewTarget::AllItems),
            (None, HomeViewTarget::AllItems),
        ] {
            assert_eq!(route_for_collection_type(collection_type), expected);
        }
    }

    #[test]
    fn unnamed_library_gets_a_stable_fallback_tile_name() {
        let view = UserView {
            id: "library-id".into(),
            name: None,
            collection_type: None,
        };
        let tile = my_media_tile(&view);
        assert_eq!(tile.name, "Untitled library");
        assert_eq!(tile.target, HomeViewTarget::AllItems);
    }

    #[test]
    fn mock_home_catalogs_render_two_library_and_two_collection_tiles_and_empty_state() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        runtime.block_on(async {
            let server = spawn(MockConfig {
                user_view_limit: Some(2),
                ..MockConfig::default()
            })
            .await
            .expect("spawn two-view mock server");
            let client = client_for(&server.base_url());
            let session = client.authenticate("demo", "demo").await.expect("sign in");

            let mut state = HomeState::default();
            state.begin_catalog_load(HomeCatalogKind::Libraries, 1);
            let views = client.user_views(&session).await.expect("load views");
            assert!(state.apply_libraries(1, views));
            state.begin_catalog_load(HomeCatalogKind::Collections, 2);
            let items = client.box_sets(&session, 16).await.expect("load box sets");
            assert!(state.apply_collections(2, items.into_iter().map(card_from_item).collect(),));

            let tiles = state.my_media_tiles();
            assert_eq!(tiles.len(), 2);
            assert_eq!(tiles[0].name, "Movies");
            assert_eq!(tiles[0].target, HomeViewTarget::Movies);
            assert_eq!(tiles[1].name, "Shows");
            assert_eq!(tiles[1].target, HomeViewTarget::Series);
            let collection_names: Vec<&str> = state
                .collections
                .iter()
                .map(|card| card.media.title.as_str())
                .collect();
            assert_eq!(collection_names, ["Coastal Anthology", "Winter Archive"]);
            assert!(state.collections_are_visible());

            let context = egui::Context::default();
            let mut visible_artwork = Vec::new();
            let mut focus_entries = Vec::new();
            let mut output = context.run_ui(egui::RawInput::default(), |ui| {
                let _ = show(
                    ui,
                    &mut state,
                    crate::theme::palette(false),
                    &mut focus_entries,
                    &mut visible_artwork,
                    None,
                );
            });
            output.textures_delta.clear();
            server.shutdown().await;

            let empty_server = spawn(MockConfig {
                user_view_limit: Some(0),
                ..MockConfig::default()
            })
            .await
            .expect("spawn zero-view mock server");
            let empty_client = client_for(&empty_server.base_url());
            let empty_session = empty_client
                .authenticate("demo", "demo")
                .await
                .expect("sign in to empty server");
            let mut empty_state = HomeState::default();
            empty_state.begin_catalog_load(HomeCatalogKind::Libraries, 3);
            let views = empty_client
                .user_views(&empty_session)
                .await
                .expect("load empty views");
            assert!(empty_state.apply_libraries(3, views));
            assert_eq!(
                empty_state.my_media_empty_message(),
                Some("This server has no libraries for this user.")
            );

            let context = egui::Context::default();
            let mut visible_artwork = Vec::new();
            let mut focus_entries = Vec::new();
            let mut output = context.run_ui(egui::RawInput::default(), |ui| {
                let _ = show(
                    ui,
                    &mut empty_state,
                    crate::theme::palette(false),
                    &mut focus_entries,
                    &mut visible_artwork,
                    None,
                );
            });
            output.textures_delta.clear();

            empty_state.begin_catalog_load(HomeCatalogKind::Collections, 4);
            assert!(empty_state.apply_collections(4, Vec::new()));
            assert!(!empty_state.collections_are_visible());
            empty_server.shutdown().await;
        });
    }

    #[test]
    fn progress_prefers_played_percentage_and_falls_back_to_position() {
        assert_eq!(progress_fraction(Some(50.0), Some(1), Some(100)), Some(0.5));
        assert_eq!(progress_fraction(None, Some(250), Some(1000)), Some(0.25));
        assert_eq!(progress_fraction(Some(150.0), None, None), Some(1.0));
    }

    #[test]
    fn progress_is_hidden_when_nothing_usable_is_reported() {
        assert_eq!(progress_fraction(None, None, None), None);
        assert_eq!(progress_fraction(Some(0.0), Some(0), Some(100)), None);
        assert_eq!(progress_fraction(Some(f64::NAN), None, Some(100)), None);
        assert_eq!(progress_fraction(None, Some(-5), Some(100)), None);
        assert_eq!(progress_fraction(None, Some(5), Some(0)), None);
    }

    #[test]
    fn episode_codes_need_a_season_and_episode_pair_or_an_episode() {
        assert_eq!(episode_code(Some(1), Some(2)), Some("S1E2".into()));
        assert_eq!(episode_code(None, Some(3)), Some("E3".into()));
        assert_eq!(episode_code(Some(1), None), None);
    }

    #[test]
    fn episode_subtitle_combines_series_and_code() {
        assert_eq!(
            card_subtitle(Some("Episode"), Some("Show"), Some(1), Some(2), None),
            Some("Show · S1E2".into())
        );
        assert_eq!(
            card_subtitle(Some("Episode"), Some("Show"), None, None, None),
            Some("Show".into())
        );
        assert_eq!(
            card_subtitle(Some("Episode"), Some("  "), None, Some(4), None),
            Some("E4".into())
        );
        assert_eq!(card_subtitle(Some("Episode"), None, None, None, None), None);
    }

    #[test]
    fn movie_subtitle_is_the_production_year_when_present() {
        assert_eq!(
            card_subtitle(Some("Movie"), None, None, None, Some(1999)),
            Some("1999".into())
        );
        assert_eq!(
            card_subtitle(Some("Movie"), None, None, None, Some(0)),
            None
        );
        assert_eq!(card_subtitle(Some("Movie"), None, None, None, None), None);
    }

    #[test]
    fn episode_item_maps_to_card_with_subtitle_and_progress() {
        let mapped = card_from_item(item(json!({
            "Id": "ep1",
            "Name": "Pilot",
            "Type": "Episode",
            "SeriesName": "Show",
            "ParentIndexNumber": 1,
            "IndexNumber": 2,
            "RunTimeTicks": 1000,
            "UserData": { "PlaybackPositionTicks": 250 }
        })));
        assert_eq!(mapped.media.id, "ep1");
        assert_eq!(mapped.media.title, "Pilot");
        assert_eq!(mapped.subtitle.as_deref(), Some("Show · S1E2"));
        assert_eq!(mapped.progress, Some(0.25));
        assert_eq!(mapped.media.primary_image_tag, None);
    }

    #[test]
    fn sparse_item_maps_without_optional_fields() {
        let mapped = card_from_item(item(json!({ "Id": "m1", "Name": "Film" })));
        assert_eq!(mapped.subtitle, None);
        assert_eq!(mapped.progress, None);
        assert!(mapped.media.artwork_texture.is_none());
    }

    #[test]
    fn stale_or_unknown_responses_do_not_change_a_row() {
        let mut state = HomeState::default();
        state.begin_load(HomeRowKind::NextUp, 7);

        assert!(state.owns_request(7));
        assert!(!state.apply_items(HomeRowKind::NextUp, 6, vec![card("old")]));
        assert!(!state.apply_failure(HomeRowKind::NextUp, 6, "stale".into()));
        assert_eq!(state.row(HomeRowKind::NextUp).status, RowStatus::Loading);
        assert!(state.row(HomeRowKind::NextUp).cards.is_empty());
        assert!(!state.apply_items(HomeRowKind::Latest, 7, vec![card("wrong-row")]));
    }

    #[test]
    fn one_failing_row_keeps_the_others_loaded() {
        let mut state = HomeState::default();
        state.begin_load(HomeRowKind::ContinueWatching, 1);
        state.begin_load(HomeRowKind::Latest, 2);
        assert!(state.apply_items(HomeRowKind::ContinueWatching, 1, vec![card("a")]));
        assert!(state.apply_failure(HomeRowKind::Latest, 2, "server error".into()));

        assert_eq!(
            state.row(HomeRowKind::ContinueWatching).status,
            RowStatus::Ready
        );
        assert_eq!(state.row(HomeRowKind::ContinueWatching).cards.len(), 1);
        assert_eq!(
            state.row(HomeRowKind::Latest).status,
            RowStatus::Failed("server error".into())
        );
        assert!(!state.owns_request(2));
    }

    #[test]
    fn failed_reload_keeps_previous_cards_and_retry_restarts_loading() {
        let mut state = HomeState::default();
        state.begin_load(HomeRowKind::NextUp, 3);
        assert!(state.apply_items(HomeRowKind::NextUp, 3, vec![card("kept")]));
        state.begin_load(HomeRowKind::NextUp, 4);
        assert!(state.apply_failure(HomeRowKind::NextUp, 4, "timeout".into()));
        assert_eq!(state.row(HomeRowKind::NextUp).cards.len(), 1);

        state.begin_load(HomeRowKind::NextUp, 5);
        assert_eq!(state.row(HomeRowKind::NextUp).status, RowStatus::Loading);
        assert!(state.owns_request(5));
    }

    #[test]
    fn hero_prefers_continue_watching_then_latest() {
        let mut state = HomeState::default();
        assert!(hero_card(&state.rows).is_none());

        state.begin_load(HomeRowKind::Latest, 1);
        state.apply_items(HomeRowKind::Latest, 1, vec![card("latest")]);
        assert_eq!(
            hero_card(&state.rows).map(|c| c.media.id.as_str()),
            Some("latest")
        );

        state.begin_load(HomeRowKind::ContinueWatching, 2);
        state.apply_items(HomeRowKind::ContinueWatching, 2, vec![card("resume")]);
        assert_eq!(
            hero_card(&state.rows).map(|c| c.media.id.as_str()),
            Some("resume")
        );
    }

    #[test]
    fn reset_drops_every_row_and_ignores_old_requests() {
        let mut state = HomeState::default();
        state.begin_load(HomeRowKind::Latest, 9);
        state.apply_items(HomeRowKind::Latest, 9, vec![card("x")]);
        state.begin_load(HomeRowKind::NextUp, 10);

        state = HomeState::default();
        assert!(!state.owns_request(10));
        assert_eq!(state.media().count(), 0);
        assert!(!state.apply_items(HomeRowKind::NextUp, 10, vec![card("late")]));
    }
}
