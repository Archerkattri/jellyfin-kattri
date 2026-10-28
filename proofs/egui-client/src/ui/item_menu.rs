//! Right-click (secondary click) menu for library cards, modelled on the item context menu in
//! Jellyfin Web (`src/components/itemContextMenu.js`). A card attaches the menu to its
//! response; the chosen entry is parked in egui's temporary memory and the app applies it once
//! per frame with [`take`], so card widgets need no access to the app.

use std::sync::Arc;

use kattri_i18n::ids;

use super::ui_text;
use crate::catalog::{MediaItem, MediaKind};

/// The item a menu entry acts on, copied from the card so the action survives a list reload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ItemMenuTarget {
    pub item_id: String,
    pub title: String,
    pub series_id: Option<String>,
    /// The playlist or collection chosen under Add to playlist or Add to collection.
    pub list_id: Option<String>,
}

/// The user's playlists as `(id, name)`, published by the app for the Add to playlist submenu.
pub(crate) type MenuPlaylists = Arc<Vec<(String, String)>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ItemMenuAction {
    Play,
    PlayFromStart,
    Open,
    GoToSeries,
    SetPlayed(bool),
    SetFavorite(bool),
    Download,
    /// Append a music track to the play queue.
    AddToQueue,
    /// Put a music track right after the one playing.
    PlayNextInQueue,
    /// Add the item to the playlist in [`ItemMenuTarget::playlist_id`].
    AddToPlaylist,
    /// Administrators: ask the server to refresh the item's metadata.
    RefreshMetadata,
    /// Administrators: delete the item, after a confirmation.
    Delete,
    /// Administrators: add the item to the collection in [`ItemMenuTarget::list_id`].
    AddToCollection,
    /// Administrators: open the metadata editor.
    EditMetadata,
    /// Administrators: match the item against the server's metadata providers.
    Identify,
    /// Administrators: open the image editor.
    EditImages,
    /// Users allowed to manage subtitles: search and download subtitles for a video.
    SearchSubtitles,
}

/// Entries offered for `item`, in menu order. Folders and playlists only open; series and
/// seasons cannot be downloaded as a single file.
pub(crate) fn entries(item: &MediaItem) -> Vec<ItemMenuAction> {
    if matches!(
        item.kind,
        MediaKind::Folder | MediaKind::Playlist | MediaKind::Other
    ) {
        return vec![ItemMenuAction::Open];
    }
    if item.kind == MediaKind::Book {
        // Books are not played; their file can be downloaded to read elsewhere.
        return vec![
            ItemMenuAction::Open,
            ItemMenuAction::SetPlayed(!item.played),
            ItemMenuAction::SetFavorite(!item.favorite),
            ItemMenuAction::Download,
        ];
    }
    let mut entries = vec![ItemMenuAction::Play];
    if item.resumable {
        entries.push(ItemMenuAction::PlayFromStart);
    }
    entries.push(ItemMenuAction::Open);
    if item.series_id.is_some() && item.kind != MediaKind::Series {
        entries.push(ItemMenuAction::GoToSeries);
    }
    entries.push(ItemMenuAction::SetPlayed(!item.played));
    entries.push(ItemMenuAction::SetFavorite(!item.favorite));
    if is_track(item) {
        entries.push(ItemMenuAction::PlayNextInQueue);
        entries.push(ItemMenuAction::AddToQueue);
    }
    if matches!(
        item.kind,
        MediaKind::Movie | MediaKind::Episode | MediaKind::Audio | MediaKind::Video
    ) {
        entries.push(ItemMenuAction::Download);
    }
    entries
}

/// A single music track; albums share [`MediaKind::Audio`] but cannot join the queue as one.
fn is_track(item: &MediaItem) -> bool {
    item.kind == MediaKind::Audio && item.remote_type.as_deref() == Some("Audio")
}

/// Items a Jellyfin playlist accepts from this menu.
fn can_join_playlist(item: &MediaItem) -> bool {
    matches!(
        item.kind,
        MediaKind::Movie | MediaKind::Episode | MediaKind::Audio | MediaKind::Series
    )
}

fn label(action: ItemMenuAction) -> &'static str {
    ui_text(match action {
        ItemMenuAction::Play => ids::ITEM_MENU_PLAY,
        ItemMenuAction::PlayFromStart => ids::ITEM_MENU_PLAY_FROM_START,
        ItemMenuAction::Open => ids::ITEM_MENU_OPEN,
        ItemMenuAction::GoToSeries => ids::ITEM_MENU_GO_TO_SERIES,
        ItemMenuAction::SetPlayed(true) => ids::ITEM_MENU_MARK_PLAYED,
        ItemMenuAction::SetPlayed(false) => ids::ITEM_MENU_MARK_UNPLAYED,
        ItemMenuAction::SetFavorite(true) => ids::ITEM_MENU_ADD_FAVORITE,
        ItemMenuAction::SetFavorite(false) => ids::ITEM_MENU_REMOVE_FAVORITE,
        ItemMenuAction::Download => ids::ITEM_MENU_DOWNLOAD,
        ItemMenuAction::AddToQueue => ids::ITEM_MENU_ADD_TO_QUEUE,
        ItemMenuAction::PlayNextInQueue => ids::ITEM_MENU_PLAY_NEXT,
        ItemMenuAction::AddToPlaylist => ids::ITEM_MENU_ADD_TO_PLAYLIST,
        ItemMenuAction::RefreshMetadata => ids::ITEM_MENU_REFRESH_METADATA,
        ItemMenuAction::Delete => ids::ITEM_MENU_DELETE,
        ItemMenuAction::AddToCollection => ids::ITEM_MENU_ADD_TO_COLLECTION,
        ItemMenuAction::EditMetadata => ids::ITEM_MENU_EDIT_METADATA,
        ItemMenuAction::Identify => ids::ITEM_MENU_IDENTIFY,
        ItemMenuAction::EditImages => ids::ITEM_MENU_EDIT_IMAGES,
        ItemMenuAction::SearchSubtitles => ids::ITEM_MENU_SEARCH_SUBTITLES,
    })
}

fn chosen_id() -> egui::Id {
    egui::Id::unique("kattri-item-menu-choice")
}

fn playlists_id() -> egui::Id {
    egui::Id::unique("kattri-item-menu-playlists")
}

fn collections_id() -> egui::Id {
    egui::Id::unique("kattri-item-menu-collections")
}

/// Makes `collections` available to the administrator entries of every card menu.
pub(crate) fn publish_collections(ctx: &egui::Context, collections: MenuPlaylists) {
    ctx.data_mut(|data| data.insert_temp(collections_id(), collections));
}

fn admin_id() -> egui::Id {
    egui::Id::unique("kattri-item-menu-admin")
}

/// Whether card menus drawn after this call offer the administrator entries.
pub(crate) fn publish_admin(ctx: &egui::Context, admin: bool) {
    ctx.data_mut(|data| data.insert_temp(admin_id(), admin));
}

fn subtitles_id() -> egui::Id {
    egui::Id::unique("kattri-item-menu-subtitles")
}

/// Makes subtitle management available only to authorized users.
pub(crate) fn publish_subtitle_management(ctx: &egui::Context, allowed: bool) {
    ctx.data_mut(|data| data.insert_temp(subtitles_id(), allowed));
}

/// Only movies and episodes can be searched for remote subtitles.
pub(crate) fn can_search_subtitles(item: &MediaItem) -> bool {
    matches!(item.kind, MediaKind::Movie | MediaKind::Episode)
}

/// Administrator entries for `item`, as Jellyfin Web offers them: edit metadata and images
/// for everything but library views, identify for the types the providers can match, refresh,
/// and delete for media.
pub(crate) fn admin_entries(item: &MediaItem) -> Vec<ItemMenuAction> {
    let view = matches!(
        item.remote_type.as_deref(),
        Some("UserView" | "CollectionFolder" | "UserRootFolder")
    );
    let mut entries = Vec::new();
    if !view {
        entries.push(ItemMenuAction::EditMetadata);
        if item
            .remote_type
            .as_deref()
            .is_some_and(jellyfin_api::is_identifiable)
        {
            entries.push(ItemMenuAction::Identify);
        }
        entries.push(ItemMenuAction::EditImages);
    }
    entries.push(ItemMenuAction::RefreshMetadata);
    if !matches!(item.kind, MediaKind::Folder | MediaKind::Other) {
        entries.push(ItemMenuAction::Delete);
    }
    entries
}

/// Makes `playlists` available to every card menu drawn after this call.
pub(crate) fn publish_playlists(ctx: &egui::Context, playlists: MenuPlaylists) {
    ctx.data_mut(|data| data.insert_temp(playlists_id(), playlists));
}

fn choose(ui: &egui::Ui, item: &MediaItem, action: ItemMenuAction, list_id: Option<String>) {
    let target = ItemMenuTarget {
        item_id: item.id.clone(),
        title: item.title.clone(),
        series_id: item.series_id.clone(),
        list_id,
    };
    ui.ctx()
        .data_mut(|data| data.insert_temp(chosen_id(), (target, action)));
}

/// Shows the menu when `response` is secondary-clicked, or focused and Shift+F10 or the Menu
/// key is pressed, and records the chosen entry.
pub(crate) fn attach(response: &egui::Response, item: &MediaItem) {
    menu_popup(response).show(|ui| show(ui, item));
}

/// The card's context menu. Opened from the keyboard it sits under the card instead of at the
/// pointer.
fn menu_popup(response: &egui::Response) -> egui::Popup<'_> {
    let ctx = &response.ctx;
    let opened_by_keyboard = response.id.with("kattri-keyboard-menu");
    let pressed = response.has_focus()
        && ctx.input_mut(|input| {
            input.consume_key(egui::Modifiers::SHIFT, egui::Key::F10)
                || input.consume_key(egui::Modifiers::NONE, egui::Key::Menu)
        });
    if pressed {
        ctx.data_mut(|data| data.insert_temp(opened_by_keyboard, true));
    }
    let popup = egui::Popup::context_menu(response);
    if response.secondary_clicked() {
        ctx.data_mut(|data| data.remove::<bool>(opened_by_keyboard));
        return popup;
    }
    if !ctx
        .data(|data| data.get_temp::<bool>(opened_by_keyboard))
        .unwrap_or(false)
    {
        return popup;
    }
    let popup = popup.anchor(egui::PopupAnchor::ParentRect(response.rect));
    if pressed {
        popup.open_memory(Some(egui::SetOpenCommand::Bool(true)))
    } else {
        if !popup.is_open() {
            ctx.data_mut(|data| data.remove::<bool>(opened_by_keyboard));
        }
        popup
    }
}

fn show(ui: &mut egui::Ui, item: &MediaItem) {
    ui.set_min_width(200.0);
    ui.label(egui::RichText::new(&item.title).strong());
    ui.separator();
    for action in entries(item) {
        if ui.button(label(action)).clicked() {
            choose(ui, item, action, None);
            ui.close();
        }
    }
    let playlists: Option<MenuPlaylists> = ui.ctx().data(|data| data.get_temp(playlists_id()));
    if let Some(playlists) = playlists.filter(|playlists| !playlists.is_empty())
        && can_join_playlist(item)
    {
        list_submenu(ui, item, ItemMenuAction::AddToPlaylist, &playlists);
    }
    let subtitles: bool = ui
        .ctx()
        .data(|data| data.get_temp(subtitles_id()))
        .unwrap_or(false);
    if subtitles && can_search_subtitles(item) {
        let action = ItemMenuAction::SearchSubtitles;
        if ui.button(label(action)).clicked() {
            choose(ui, item, action, None);
            ui.close();
        }
    }
    let admin: bool = ui
        .ctx()
        .data(|data| data.get_temp(admin_id()))
        .unwrap_or(false);
    if admin {
        ui.separator();
        let collections: Option<MenuPlaylists> =
            ui.ctx().data(|data| data.get_temp(collections_id()));
        if let Some(collections) = collections.filter(|collections| !collections.is_empty())
            && can_join_playlist(item)
        {
            list_submenu(ui, item, ItemMenuAction::AddToCollection, &collections);
        }
        for action in admin_entries(item) {
            if ui.button(label(action)).clicked() {
                choose(ui, item, action, None);
                ui.close();
            }
        }
    }
}

/// A submenu of named lists (playlists or collections); choosing one records `action` with it.
fn list_submenu(
    ui: &mut egui::Ui,
    item: &MediaItem,
    action: ItemMenuAction,
    lists: &MenuPlaylists,
) {
    ui.menu_button(label(action), |ui| {
        egui::ScrollArea::vertical()
            .max_height(320.0)
            .show(ui, |ui| {
                for (list_id, name) in lists.iter() {
                    if ui.button(name).clicked() {
                        choose(ui, item, action, Some(list_id.clone()));
                        ui.close();
                    }
                }
            });
    });
}

/// The entry chosen since the last call, if any.
pub(crate) fn take(ctx: &egui::Context) -> Option<(ItemMenuTarget, ItemMenuAction)> {
    ctx.data_mut(|data| {
        let chosen = data.get_temp(chosen_id());
        data.remove::<(ItemMenuTarget, ItemMenuAction)>(chosen_id());
        chosen
    })
}

/// Same menu for a server DTO such as a search result, which is not a catalog entry yet. The
/// catalog item is built only when the menu opens, so a frame of results costs nothing.
pub(crate) fn attach_dto(response: &egui::Response, dto: &jellyfin_api::BaseItemDto) {
    menu_popup(response).show(|ui| show(ui, &MediaItem::from_api(dto.clone())));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(json: serde_json::Value) -> MediaItem {
        MediaItem::from_api(serde_json::from_value(json).expect("item fixture"))
    }

    #[test]
    fn episode_menu_offers_resume_series_and_toggles_from_user_data() {
        let episode = item(serde_json::json!({
            "Id": "ep", "Name": "Pilot", "Type": "Episode", "SeriesId": "show",
            "UserData": {"PlaybackPositionTicks": 5, "Played": false, "IsFavorite": true}
        }));
        assert_eq!(
            entries(&episode),
            [
                ItemMenuAction::Play,
                ItemMenuAction::PlayFromStart,
                ItemMenuAction::Open,
                ItemMenuAction::GoToSeries,
                ItemMenuAction::SetPlayed(true),
                ItemMenuAction::SetFavorite(false),
                ItemMenuAction::Download,
            ]
        );
    }

    #[test]
    fn books_offer_download_instead_of_play_and_videos_play() {
        let book = item(serde_json::json!({"Id": "b", "Name": "Notes", "Type": "Book"}));
        let actions = entries(&book);
        assert!(!actions.contains(&ItemMenuAction::Play));
        assert!(actions.contains(&ItemMenuAction::Download));
        for item_type in ["Video", "MusicVideo"] {
            let video = item(serde_json::json!({"Id": "v", "Name": "Clip", "Type": item_type}));
            let actions = entries(&video);
            assert_eq!(actions.first(), Some(&ItemMenuAction::Play), "{item_type}");
            assert!(actions.contains(&ItemMenuAction::Download), "{item_type}");
        }
    }

    #[test]
    fn series_cannot_be_downloaded_and_folders_only_open() {
        let series = item(serde_json::json!({"Id": "s", "Name": "Show", "Type": "Series"}));
        let actions = entries(&series);
        assert!(!actions.contains(&ItemMenuAction::Download));
        assert!(!actions.contains(&ItemMenuAction::GoToSeries));
        assert!(!actions.contains(&ItemMenuAction::PlayFromStart));
        let folder = item(serde_json::json!({"Id": "f", "Name": "F", "Type": "Folder"}));
        assert_eq!(entries(&folder), [ItemMenuAction::Open]);
    }

    #[test]
    fn a_search_result_offers_the_menu_a_catalog_card_does() {
        // Search hands the menu a server DTO, so it converts on demand. The entries must match
        // what the same item shows as a library card.
        let dto: jellyfin_api::BaseItemDto = serde_json::from_value(serde_json::json!({
            "Id": "mov", "Name": "Film", "Type": "Movie"
        }))
        .expect("dto fixture");
        assert_eq!(
            entries(&MediaItem::from_api(dto)),
            [
                ItemMenuAction::Play,
                ItemMenuAction::Open,
                ItemMenuAction::SetPlayed(true),
                ItemMenuAction::SetFavorite(true),
                ItemMenuAction::Download,
            ]
        );
    }

    #[test]
    fn music_tracks_can_join_the_queue_but_albums_cannot() {
        let track = item(serde_json::json!({"Id": "t", "Name": "Song", "Type": "Audio"}));
        let actions = entries(&track);
        assert!(actions.contains(&ItemMenuAction::PlayNextInQueue));
        assert!(actions.contains(&ItemMenuAction::AddToQueue));
        let album = item(serde_json::json!({"Id": "a", "Name": "LP", "Type": "MusicAlbum"}));
        assert!(!entries(&album).contains(&ItemMenuAction::AddToQueue));
        assert!(can_join_playlist(&album));
        let folder = item(serde_json::json!({"Id": "f", "Name": "F", "Type": "Folder"}));
        assert!(!can_join_playlist(&folder));
    }

    #[test]
    fn shift_f10_on_a_focused_card_opens_its_menu() {
        let ctx = egui::Context::default();
        let card = item(serde_json::json!({"Id": "m", "Name": "Film", "Type": "Movie"}));
        let frame = |ctx: &egui::Context, events: Vec<egui::Event>, focus: bool| {
            let mut opened = false;
            let mut output = ctx.run_ui(
                egui::RawInput {
                    events,
                    ..Default::default()
                },
                |ui| {
                    let response = ui.add(egui::Button::new("card").sense(egui::Sense::click()));
                    if focus {
                        response.request_focus();
                    }
                    opened = menu_popup(&response).show(|ui| show(ui, &card)).is_some();
                },
            );
            output.textures_delta.clear();
            opened
        };
        assert!(!frame(&ctx, Vec::new(), true), "closed until asked");
        let shift_f10 = egui::Event::Key {
            key: egui::Key::F10,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::SHIFT,
        };
        assert!(frame(&ctx, vec![shift_f10], false), "opens under the card");
        assert!(frame(&ctx, Vec::new(), false), "and stays open");
    }

    #[test]
    fn admin_entries_follow_jellyfin_web() {
        let movie = item(serde_json::json!({"Id": "m", "Name": "Film", "Type": "Movie"}));
        assert_eq!(
            admin_entries(&movie),
            [
                ItemMenuAction::EditMetadata,
                ItemMenuAction::Identify,
                ItemMenuAction::EditImages,
                ItemMenuAction::RefreshMetadata,
                ItemMenuAction::Delete,
            ]
        );
        let episode = item(serde_json::json!({"Id": "e", "Name": "Pilot", "Type": "Episode"}));
        assert!(!admin_entries(&episode).contains(&ItemMenuAction::Identify));
        let view =
            item(serde_json::json!({"Id": "v", "Name": "Films", "Type": "CollectionFolder"}));
        assert_eq!(admin_entries(&view), [ItemMenuAction::RefreshMetadata]);
    }

    #[test]
    fn chosen_entry_is_taken_once() {
        let ctx = egui::Context::default();
        let target = ItemMenuTarget {
            item_id: "a".into(),
            title: "A".into(),
            series_id: None,
            list_id: None,
        };
        ctx.data_mut(|data| data.insert_temp(chosen_id(), (target.clone(), ItemMenuAction::Play)));
        assert_eq!(take(&ctx), Some((target, ItemMenuAction::Play)));
        assert_eq!(take(&ctx), None);
    }
}
