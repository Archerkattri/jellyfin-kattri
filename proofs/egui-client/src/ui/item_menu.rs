//! Right-click (secondary click) menu for library cards, modelled on the item context menu in
//! Jellyfin Web (`src/components/itemContextMenu.js`). A card attaches the menu to its
//! response; the chosen entry is parked in egui's temporary memory and the app applies it once
//! per frame with [`take`], so card widgets need no access to the app.

use kattri_i18n::ids;

use super::ui_text;
use crate::catalog::{MediaItem, MediaKind};

/// The item a menu entry acts on, copied from the card so the action survives a list reload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ItemMenuTarget {
    pub item_id: String,
    pub title: String,
    pub series_id: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ItemMenuAction {
    Play,
    PlayFromStart,
    Open,
    GoToSeries,
    SetPlayed(bool),
    SetFavorite(bool),
    Download,
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
    if matches!(
        item.kind,
        MediaKind::Movie | MediaKind::Episode | MediaKind::Audio
    ) {
        entries.push(ItemMenuAction::Download);
    }
    entries
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
    })
}

fn chosen_id() -> egui::Id {
    egui::Id::unique("kattri-item-menu-choice")
}

/// Shows the menu when `response` is secondary-clicked, and records the chosen entry.
pub(crate) fn attach(response: &egui::Response, item: &MediaItem) {
    response.context_menu(|ui| {
        ui.set_min_width(200.0);
        ui.label(egui::RichText::new(&item.title).strong());
        ui.separator();
        for action in entries(item) {
            if ui.button(label(action)).clicked() {
                let target = ItemMenuTarget {
                    item_id: item.id.clone(),
                    title: item.title.clone(),
                    series_id: item.series_id.clone(),
                };
                ui.ctx()
                    .data_mut(|data| data.insert_temp(chosen_id(), (target, action)));
                ui.close();
            }
        }
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
    fn chosen_entry_is_taken_once() {
        let ctx = egui::Context::default();
        let target = ItemMenuTarget {
            item_id: "a".into(),
            title: "A".into(),
            series_id: None,
        };
        ctx.data_mut(|data| data.insert_temp(chosen_id(), (target.clone(), ItemMenuAction::Play)));
        assert_eq!(take(&ctx), Some((target, ItemMenuAction::Play)));
        assert_eq!(take(&ctx), None);
    }
}
