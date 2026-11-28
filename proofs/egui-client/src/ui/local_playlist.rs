//! Profile-local playlist panel (M6-13).
//!
//! Presentation only: this module never reads or writes a file and performs no server call. It
//! returns typed [`LocalPlaylistIntent`] values and the app owns persistence, including keeping
//! the previous list when a save fails.
//!
//! The list is deliberately separate from the server-playlist cards on the same page: those are
//! Jellyfin playlists (M3), while this records ordered item IDs for the running profile only.

use egui::{RichText, Ui};
use kattri_i18n::ids;

use super::ui_text;
use crate::app::KattriApp;

/// What the viewer asked for in this frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum LocalPlaylistIntent {
    /// Remove the occurrence with this entry ID.
    Remove(String),
    MoveUp(String),
    MoveDown(String),
}

/// How much of an item ID the row shows. The full ID stays available as hover text.
const SHORT_ID_CHARS: usize = 8;

fn short_id(item_id: &str) -> String {
    if item_id.chars().count() <= SHORT_ID_CHARS {
        return item_id.to_owned();
    }
    let head: String = item_id.chars().take(SHORT_ID_CHARS).collect();
    format!("{head}…")
}

/// Accessible text of a row action: the action, the row's 1-based position, and the item.
///
/// The position is what keeps labels unique: a playlist may hold the same item twice, and two
/// different IDs can share their first characters, so identity alone is not enough.
fn row_action_label(action_id: &str, position: usize, short: &str) -> String {
    format!("{} {position}: {short}", ui_text(action_id))
}

/// Draw the local playlist and return the intents it produced.
pub(crate) fn show(ui: &mut Ui, app: &mut KattriApp) -> Vec<LocalPlaylistIntent> {
    let colors = crate::theme::active_palette();
    let mut intents = Vec::new();

    ui.label(
        RichText::new(ui_text(ids::LOCAL_PLAYLIST_TITLE))
            .size(16.0)
            .strong()
            .color(colors.text),
    )
    .widget_info(|| {
        egui::WidgetInfo::labeled(
            egui::Role::Label,
            true,
            ui_text(ids::LOCAL_PLAYLIST_TITLE).to_owned(),
        )
    });
    ui.label(
        RichText::new(ui_text(ids::LOCAL_PLAYLIST_HELP))
            .size(12.0)
            .color(colors.muted),
    )
    .widget_info(|| {
        egui::WidgetInfo::labeled(
            egui::Role::Label,
            true,
            ui_text(ids::LOCAL_PLAYLIST_HELP).to_owned(),
        )
    });
    if let Some(notice) = app.local_playlist_notice.clone() {
        ui.label(RichText::new(&notice).size(12.0).color(colors.danger))
            .widget_info(|| egui::WidgetInfo::labeled(egui::Role::Label, true, notice.clone()));
    }
    if app.local_playlist.is_empty() {
        ui.label(
            RichText::new(ui_text(ids::LOCAL_PLAYLIST_EMPTY))
                .size(13.0)
                .color(colors.muted),
        )
        .widget_info(|| {
            egui::WidgetInfo::labeled(
                egui::Role::Label,
                true,
                ui_text(ids::LOCAL_PLAYLIST_EMPTY).to_owned(),
            )
        });
        return intents;
    }

    let count = app.local_playlist.len();
    for (index, entry) in app.local_playlist.entries().iter().enumerate() {
        ui.horizontal(|ui| {
            let label = short_id(entry.item_id());
            ui.label(RichText::new(&label).size(13.0).color(colors.text))
                .on_hover_text(entry.item_id());
            let position = index + 1;
            // Each action names its row: static labels are not published to assistive
            // technology, so the item identity has to ride on the controls themselves.
            if ui
                .add_enabled(
                    index > 0,
                    egui::Button::new(row_action_label(
                        ids::LOCAL_PLAYLIST_MOVE_UP,
                        position,
                        &label,
                    )),
                )
                .clicked()
            {
                intents.push(LocalPlaylistIntent::MoveUp(entry.entry_id().to_owned()));
            }
            if ui
                .add_enabled(
                    index + 1 < count,
                    egui::Button::new(row_action_label(
                        ids::LOCAL_PLAYLIST_MOVE_DOWN,
                        position,
                        &label,
                    )),
                )
                .clicked()
            {
                intents.push(LocalPlaylistIntent::MoveDown(entry.entry_id().to_owned()));
            }
            if ui
                .button(row_action_label(
                    ids::LOCAL_PLAYLIST_REMOVE,
                    position,
                    &label,
                ))
                .clicked()
            {
                intents.push(LocalPlaylistIntent::Remove(entry.entry_id().to_owned()));
            }
        });
    }
    intents
}

#[cfg(test)]
mod tests {
    use super::{LocalPlaylistIntent, short_id, show};
    use crate::app::KattriApp;
    use kattri_client_core::playlist::{PlaylistEntry, PlaylistModel};

    fn entry(n: usize, item: &str) -> PlaylistEntry {
        PlaylistEntry::new(&format!("{:032x}", 0x5000 + n), item).expect("valid entry")
    }

    fn rendered(app: &mut KattriApp) -> (Vec<LocalPlaylistIntent>, egui::accesskit::TreeUpdate) {
        let context = egui::Context::default();
        context.enable_accesskit();
        let mut intents = Vec::new();
        let mut output = context.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(800.0, 600.0),
                )),
                ..Default::default()
            },
            |ui| {
                intents = show(ui, app);
            },
        );
        output.textures_delta.clear();
        let tree = output
            .platform_output
            .accesskit_update
            .expect("playlist panel tree");
        (intents, tree)
    }

    fn labels(tree: &egui::accesskit::TreeUpdate) -> Vec<String> {
        tree.nodes
            .iter()
            .filter_map(|(_, node)| node.label().map(str::to_owned))
            .collect()
    }

    #[test]
    fn an_empty_list_offers_no_row_actions() {
        let mut app = KattriApp::new(egui::Context::default());
        let (intents, tree) = rendered(&mut app);
        assert!(intents.is_empty());
        // egui publishes interactive nodes only, so the assertion is about controls rather
        // than the static heading and hint text.
        assert!(
            labels(&tree).is_empty(),
            "an empty list has no controls: {:?}",
            labels(&tree)
        );
    }

    #[test]
    fn every_row_action_names_its_own_row_and_position() {
        let mut app = KattriApp::new(egui::Context::default());
        app.local_playlist = PlaylistModel::from_entries(vec![
            entry(0, "0000000000000000000000000000aaaa"),
            entry(1, "0000000000000000000000000000bbbb"),
        ])
        .expect("two entries");

        let (intents, tree) = rendered(&mut app);

        assert!(intents.is_empty(), "nothing was clicked");
        let labels = labels(&tree);
        let first = short_id("0000000000000000000000000000aaaa");
        let second = short_id("0000000000000000000000000000bbbb");
        assert_eq!(
            labels
                .iter()
                .filter(|label| label.contains("Remove"))
                .count(),
            2,
            "one remove control per row: {labels:?}"
        );
        for action in ["Remove", "Move up", "Move down"] {
            assert!(
                labels
                    .iter()
                    .any(|label| label.starts_with(action) && label.ends_with(&first)),
                "{action} for the first row is missing its item name: {labels:?}"
            );
            assert!(
                labels
                    .iter()
                    .any(|label| label.starts_with(action) && label.ends_with(&second)),
                "{action} for the second row is missing its item name: {labels:?}"
            );
            // The position distinguishes rows that share an item name.
            assert!(
                labels
                    .iter()
                    .any(|label| label.contains(" 1: ") && label.ends_with(&first)),
                "{action} for the first row is missing its position: {labels:?}"
            );
            assert!(
                labels
                    .iter()
                    .any(|label| label.contains(" 2: ") && label.ends_with(&second)),
                "{action} for the second row is missing its position: {labels:?}"
            );
        }
    }

    #[test]
    fn repeated_items_and_shared_prefixes_still_get_unique_action_labels() {
        // The same item twice, plus two different IDs whose first eight characters match.
        let repeated = "0000000000000000000000000000aaaa";
        let prefix_a = "aaaaaaaa00000000000000000000cccc";
        let prefix_b = "aaaaaaaa00000000000000000000dddd";
        // Every pair of distinct IDs may share the first eight characters, so identity alone
        // would produce identical labels for the third and fourth rows.
        assert_eq!(short_id(prefix_a), short_id(prefix_b));

        let mut app = KattriApp::new(egui::Context::default());
        app.local_playlist = PlaylistModel::from_entries(vec![
            entry(0, repeated),
            entry(1, repeated),
            entry(2, prefix_a),
            entry(3, prefix_b),
        ])
        .expect("four entries");

        let (_, tree) = rendered(&mut app);

        let labels = labels(&tree);
        let removes: Vec<&String> = labels
            .iter()
            .filter(|label| label.contains("Remove"))
            .collect();
        assert_eq!(removes.len(), 4, "one remove control per row: {labels:?}");
        let unique: std::collections::HashSet<&&String> = removes.iter().collect();
        assert_eq!(
            unique.len(),
            4,
            "every row's action label is unique by position: {removes:?}"
        );
    }

    #[test]
    fn a_save_failure_keeps_the_rows_and_records_the_reason() {
        let mut app = KattriApp::new(egui::Context::default());
        app.local_playlist =
            PlaylistModel::from_entries(vec![entry(0, "0000000000000000000000000000aaaa")])
                .expect("one entry");
        app.local_playlist_notice = Some("Could not save the local playlist.".to_owned());

        let (_, tree) = rendered(&mut app);
        // The notice is static text; what matters is that the list still renders its controls.
        assert_eq!(
            labels(&tree)
                .iter()
                .filter(|label| label.contains("Remove"))
                .count(),
            1
        );
    }
}
