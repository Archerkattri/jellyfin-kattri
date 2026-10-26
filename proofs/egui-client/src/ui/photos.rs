//! Presentation-only photo browser. The caller owns paging, fetching, and image
//! decoding; this module only renders the supplied items and textures.

use super::{ui_text, ui_text_args};
use kattri_i18n::ids;
use std::collections::HashMap;

use egui::{self, TextureHandle, Ui, Vec2};
use jellyfin_api::BaseItemDto;

/// Maximum number of items rendered from one server page.
pub const MAX_PAGE_ITEMS: usize = 100;
/// Maximum grid columns, keeping unusually wide windows from creating huge rows.
pub const MAX_COLUMNS: usize = 8;
const THUMBNAIL_SIZE: Vec2 = Vec2::new(144.0, 108.0);
const CARD_WIDTH: f32 = 168.0;
const CARD_HEIGHT: f32 = 164.0;

/// Inputs prepared by the app/worker layer. Texture maps are keyed by item ID.
pub struct PhotosView<'a> {
    pub folder_title: &'a str,
    pub items: &'a [BaseItemDto],
    pub thumbnails: &'a HashMap<String, TextureHandle>,
    pub full_size_images: &'a HashMap<String, TextureHandle>,
    pub loading: bool,
    pub error: Option<&'a str>,
    pub page_start: usize,
    pub page_size: usize,
    pub total_count: Option<usize>,
    pub selected_photo_id: Option<&'a str>,
    pub full_size_error: bool,
    pub visible_thumbnails: &'a mut Vec<(String, String)>,
}

/// A single intent for the owner to handle. This module performs no I/O.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PhotosAction {
    OpenFolder(String),
    OpenPhoto(String),
    RequestPage { start_index: usize, limit: usize },
    ClosePhoto,
    PreviousPhoto(String),
    NextPhoto(String),
}

/// Render the photo folder and return at most one user intent.
pub fn draw(ui: &mut Ui, view: &mut PhotosView<'_>) -> Option<PhotosAction> {
    ui.heading(egui::RichText::new(view.folder_title).size(28.0).strong());
    ui.add_space(4.0);

    if let Some(photo_id) = view.selected_photo_id {
        return draw_full_size(ui, view, photo_id);
    }

    let page_size = normalized_page_size(view.page_size);
    let mut action = None;
    ui.horizontal_wrapped(|ui| {
        let previous = page_request(view, false);
        if ui
            .add_enabled(
                previous.is_some(),
                egui::Button::new(ui_text(ids::PHOTOS_BUTTON_PREVIOUS_PAGE)),
            )
            .clicked()
        {
            action = previous;
        }

        let next = page_request(view, true);
        if ui
            .add_enabled(
                next.is_some(),
                egui::Button::new(ui_text(ids::PHOTOS_BUTTON_NEXT_PAGE)),
            )
            .clicked()
        {
            action = next;
        }
    });

    if view.loading {
        ui.label(ui_text(ids::PHOTOS_STATUS_LOADING));
        return action;
    }
    if let Some(error) = view.error {
        ui.label(ui_text_args(
            ids::PHOTOS_ERROR_LOAD_FOLDER,
            &[("error", error)],
        ));
        return action;
    }
    if view.items.is_empty() {
        ui.label(ui_text(ids::PHOTOS_EMPTY_NO_PHOTOS));
        return action;
    }

    let items = view
        .items
        .iter()
        .take(page_size.min(MAX_PAGE_ITEMS))
        .collect::<Vec<_>>();
    let columns = grid_columns(ui.available_width());
    let rows = items.len().div_ceil(columns);

    egui::ScrollArea::vertical()
        .id_salt("photo_browser_grid")
        .auto_shrink([false, false])
        .show_rows(ui, CARD_HEIGHT, rows, |ui, row_range| {
            let overscan_rows = row_range.len().max(1);
            let first_near_row = row_range.start.saturating_sub(overscan_rows);
            let last_near_row = row_range.end.saturating_add(overscan_rows).min(rows);
            for row in first_near_row..last_near_row {
                let visible = row >= row_range.start && row < row_range.end;
                let start = row.saturating_mul(columns);
                let end = start.saturating_add(columns).min(items.len());
                for (index, item) in items.iter().enumerate().take(end).skip(start) {
                    if is_folder(item) {
                        continue;
                    }
                    if let Some(tag) = item
                        .image_tags
                        .as_ref()
                        .and_then(|tags| tags.get("Primary"))
                        .and_then(Option::as_deref)
                    {
                        view.visible_thumbnails.push((
                            item.id.clone(),
                            encode_decode_request(
                                tag,
                                view.page_start.saturating_add(index),
                                visible,
                            ),
                        ));
                    }
                }
            }
            for row in row_range {
                ui.horizontal(|ui| {
                    for column in 0..columns {
                        let index = row.saturating_mul(columns).saturating_add(column);
                        let Some(item) = items.get(index) else {
                            break;
                        };
                        ui.push_id(&item.id, |ui| {
                            ui.allocate_ui_with_layout(
                                Vec2::new(CARD_WIDTH, CARD_HEIGHT),
                                egui::Layout::top_down(egui::Align::Center),
                                |ui| {
                                    if let Some(texture) = view.thumbnails.get(&item.id) {
                                        ui.add(
                                            egui::Image::new(texture)
                                                .fit_to_exact_size(THUMBNAIL_SIZE),
                                        );
                                    } else {
                                        ui.allocate_ui_with_layout(
                                            THUMBNAIL_SIZE,
                                            egui::Layout::centered_and_justified(
                                                egui::Direction::TopDown,
                                            ),
                                            |ui| {
                                                ui.label(if is_folder(item) {
                                                    ui_text(ids::PHOTOS_KIND_FOLDER)
                                                } else {
                                                    ui_text(ids::PHOTOS_KIND_PHOTO)
                                                });
                                            },
                                        );
                                    }

                                    let label = if is_folder(item) {
                                        ui_text_args(
                                            ids::PHOTOS_A11Y_OPEN_FOLDER,
                                            &[("name", item.name.as_str())],
                                        )
                                    } else {
                                        ui_text_args(
                                            ids::PHOTOS_A11Y_OPEN_PHOTO,
                                            &[("name", item.name.as_str())],
                                        )
                                    };
                                    if ui
                                        .add_sized([CARD_WIDTH, 44.0], egui::Button::new(label))
                                        .clicked()
                                    {
                                        action = Some(item_action(item));
                                    }
                                },
                            );
                        });
                    }
                });
            }
        });

    action
}

fn draw_full_size(ui: &mut Ui, view: &PhotosView<'_>, photo_id: &str) -> Option<PhotosAction> {
    let current_index = view.items.iter().position(|item| item.id == photo_id)?;
    let current = &view.items[current_index];
    ui.label(&current.name);

    let available = ui.available_size();
    if let Some(texture) = view.full_size_images.get(photo_id) {
        let source = texture.size_vec2();
        let scale = (available.x / source.x)
            .min(available.y / source.y)
            .min(1.0);
        ui.add(egui::Image::new(texture).fit_to_exact_size(source * scale));
    } else {
        ui.centered_and_justified(|ui| {
            ui.label(if view.full_size_error {
                ui_text(ids::PHOTOS_ERROR_DISPLAY)
            } else {
                ui_text(ids::PHOTOS_STATUS_LOADING_PHOTO)
            });
        });
    }

    let mut action = None;
    ui.add_space(8.0);
    ui.horizontal_wrapped(|ui| {
        let previous = adjacent_photo_action(view.items, current_index, false);
        if ui
            .add_enabled(
                previous.is_some(),
                egui::Button::new(ui_text(ids::PHOTOS_BUTTON_PREVIOUS_PHOTO)),
            )
            .clicked()
        {
            action = previous;
        }
        if ui.button(ui_text(ids::PHOTOS_BUTTON_CLOSE)).clicked() {
            action = Some(PhotosAction::ClosePhoto);
        }
        let next = adjacent_photo_action(view.items, current_index, true);
        if ui
            .add_enabled(
                next.is_some(),
                egui::Button::new(ui_text(ids::PHOTOS_BUTTON_NEXT_PHOTO)),
            )
            .clicked()
        {
            action = next;
        }
    });
    action
}

fn normalized_page_size(page_size: usize) -> usize {
    page_size.clamp(1, MAX_PAGE_ITEMS)
}

fn grid_columns(available_width: f32) -> usize {
    ((available_width / CARD_WIDTH).floor() as usize).clamp(1, MAX_COLUMNS)
}

fn page_request(view: &PhotosView<'_>, next: bool) -> Option<PhotosAction> {
    let limit = normalized_page_size(view.page_size);
    let start_index = if next {
        view.page_start.checked_add(limit)?
    } else {
        view.page_start.checked_sub(limit)?
    };
    if next
        && view
            .total_count
            .map(|total| start_index >= total)
            .unwrap_or(view.items.len() < limit)
    {
        return None;
    }
    Some(PhotosAction::RequestPage { start_index, limit })
}

fn adjacent_photo_action(
    items: &[BaseItemDto],
    current_index: usize,
    next: bool,
) -> Option<PhotosAction> {
    let adjacent = if next {
        current_index
            .checked_add(1)
            .filter(|&index| index < items.len())?
    } else {
        current_index.checked_sub(1)?
    };
    let id = items.get(adjacent)?.id.clone();
    Some(if next {
        PhotosAction::NextPhoto(id)
    } else {
        PhotosAction::PreviousPhoto(id)
    })
}

fn is_folder(item: &BaseItemDto) -> bool {
    matches!(
        item.item_type.as_deref(),
        Some("Folder" | "CollectionFolder")
    )
}

fn encode_decode_request(tag: &str, index: usize, visible: bool) -> String {
    format!(
        "{tag}\u{1f}{index}\u{1f}{}",
        if visible { "v" } else { "p" }
    )
}

fn item_action(item: &BaseItemDto) -> PhotosAction {
    if is_folder(item) {
        PhotosAction::OpenFolder(item.id.clone())
    } else {
        PhotosAction::OpenPhoto(item.id.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        MAX_COLUMNS, MAX_PAGE_ITEMS, PhotosAction, PhotosView, adjacent_photo_action, draw,
        grid_columns, item_action, normalized_page_size, page_request,
    };
    use egui::{Context, RawInput};
    use jellyfin_api::BaseItemDto;
    use std::collections::HashMap;

    fn item(id: &str, name: &str, kind: &str) -> BaseItemDto {
        serde_json::from_value(serde_json::json!({
            "Id": id,
            "Name": name,
            "Type": kind,
        }))
        .expect("minimal BaseItemDto fixture")
    }

    fn render(view: &mut PhotosView<'_>) {
        let context = Context::default();
        let mut output = context.run_ui(RawInput::default(), |ui| {
            let _ = draw(ui, view);
        });
        output.textures_delta.clear();
    }

    #[test]
    fn folder_and_photo_selections_produce_distinct_intents() {
        assert_eq!(
            item_action(&item("folder", "Albums", "Folder")),
            PhotosAction::OpenFolder("folder".to_owned())
        );
        assert_eq!(
            item_action(&item("photo", "Sunset", "Photo")),
            PhotosAction::OpenPhoto("photo".to_owned())
        );
    }

    #[test]
    fn paging_size_is_bounded_and_never_zero() {
        assert_eq!(normalized_page_size(0), 1);
        assert_eq!(normalized_page_size(50), 50);
        assert_eq!(normalized_page_size(usize::MAX), MAX_PAGE_ITEMS);
    }

    #[test]
    fn column_count_is_clamped_to_a_bounded_row_width() {
        assert_eq!(grid_columns(0.0), 1);
        assert_eq!(grid_columns(1_000_000.0), MAX_COLUMNS);
    }

    #[test]
    fn paging_intents_respect_boundaries_and_server_total() {
        let items = (0..50)
            .map(|index| item(&format!("p{index}"), "Photo", "Photo"))
            .collect::<Vec<_>>();
        let textures = HashMap::new();
        let mut view = PhotosView {
            folder_title: "Photos",
            items: &items,
            thumbnails: &textures,
            full_size_images: &textures,
            loading: false,
            error: None,
            page_start: 50,
            page_size: 50,
            total_count: Some(125),
            selected_photo_id: None,
            full_size_error: false,
            visible_thumbnails: &mut Vec::new(),
        };

        assert_eq!(
            page_request(&view, false),
            Some(PhotosAction::RequestPage {
                start_index: 0,
                limit: 50
            })
        );
        assert_eq!(
            page_request(&view, true),
            Some(PhotosAction::RequestPage {
                start_index: 100,
                limit: 50
            })
        );
        assert!(page_request(&view, false).is_some());
        view.page_start = 0;
        assert_eq!(page_request(&view, false), None);
    }

    #[test]
    fn full_size_navigation_returns_adjacent_photo_intents() {
        let items = [item("a", "First", "Photo"), item("b", "Second", "Photo")];
        assert_eq!(
            adjacent_photo_action(&items, 0, true),
            Some(PhotosAction::NextPhoto("b".to_owned()))
        );
        assert_eq!(
            adjacent_photo_action(&items, 1, false),
            Some(PhotosAction::PreviousPhoto("a".to_owned()))
        );
        assert_eq!(adjacent_photo_action(&items, 0, false), None);
        assert_eq!(adjacent_photo_action(&items, 1, true), None);
    }

    #[test]
    fn item_and_row_limits_are_bounded_for_large_inputs() {
        let items = (0..10_000)
            .map(|index| item(&format!("p{index}"), "Photo", "Photo"))
            .collect::<Vec<_>>();
        let thumbnails = HashMap::new();
        let full_size_images = HashMap::new();
        let mut view = PhotosView {
            folder_title: "Photos",
            items: &items,
            thumbnails: &thumbnails,
            full_size_images: &full_size_images,
            loading: false,
            error: None,
            page_start: 0,
            page_size: usize::MAX,
            total_count: Some(items.len()),
            selected_photo_id: None,
            full_size_error: false,
            visible_thumbnails: &mut Vec::new(),
        };

        assert_eq!(
            view.items.iter().take(MAX_PAGE_ITEMS).count(),
            MAX_PAGE_ITEMS
        );
        assert!(
            items
                .len()
                .min(MAX_PAGE_ITEMS)
                .div_ceil(grid_columns(1_000_000.0))
                <= MAX_PAGE_ITEMS
        );
        render(&mut view);
    }

    #[test]
    fn empty_loading_and_error_states_render_without_items() {
        let items = [];
        let textures = HashMap::new();
        let mut view = PhotosView {
            folder_title: "Empty",
            items: &items,
            thumbnails: &textures,
            full_size_images: &textures,
            loading: false,
            error: None,
            page_start: 0,
            page_size: 100,
            total_count: Some(0),
            selected_photo_id: None,
            full_size_error: false,
            visible_thumbnails: &mut Vec::new(),
        };
        render(&mut view);
        view.loading = true;
        render(&mut view);
        view.loading = false;
        view.error = Some("unavailable");
        render(&mut view);
    }

    #[test]
    fn selected_photo_can_render_full_size_view() {
        let items = [item("a", "First", "Photo"), item("b", "Second", "Photo")];
        let textures = HashMap::new();
        let mut view = PhotosView {
            folder_title: "Album",
            items: &items,
            thumbnails: &textures,
            full_size_images: &textures,
            loading: false,
            error: None,
            page_start: 0,
            page_size: 100,
            total_count: Some(2),
            selected_photo_id: Some("a"),
            full_size_error: false,
            visible_thumbnails: &mut Vec::new(),
        };
        render(&mut view);
        assert_eq!(
            item_action(&items[0]),
            PhotosAction::OpenPhoto("a".to_owned())
        );
    }
}
