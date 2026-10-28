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

    // The page already scrolls. Reserve the off-screen rows in that layout, but only build
    // widgets for rows near the clip; a nested scroll area here was squeezed to a sliver.
    for row in 0..rows {
        let row_top = ui.cursor().top();
        let row_rect = egui::Rect::from_min_size(
            egui::pos2(ui.min_rect().left(), row_top),
            Vec2::new(ui.available_width(), CARD_HEIGHT),
        );
        let visible = ui.is_rect_visible(row_rect);
        // Rows within a screen's height of the view are decoded early, at lower priority.
        let clip = ui.clip_rect();
        let near = row_rect.intersects(clip.expand2(Vec2::new(0.0, clip.height())));
        let start = row.saturating_mul(columns);
        let end = start.saturating_add(columns).min(items.len());
        if !near {
            ui.add_space(CARD_HEIGHT);
            continue;
        }
        for (index, item) in items.iter().enumerate().take(end).skip(start) {
            if let Some(tag) = item
                .image_tags
                .as_ref()
                .and_then(|tags| tags.get("Primary"))
                .and_then(Option::as_deref)
            {
                view.visible_thumbnails.push((
                    item.id.clone(),
                    encode_decode_request(tag, view.page_start.saturating_add(index), visible),
                ));
            }
        }
        ui.horizontal(|ui| {
            for item in items.iter().take(end).skip(start) {
                ui.push_id(&item.id, |ui| {
                    if photo_card(ui, item, view.thumbnails.get(&item.id)).clicked() {
                        action = Some(item_action(item));
                    }
                });
            }
        });
    }

    action
}

/// One grid card: the thumbnail (or a folder or photo placeholder) with the item's name under
/// it. The whole card opens the folder or photo.
fn photo_card(ui: &mut Ui, item: &BaseItemDto, texture: Option<&TextureHandle>) -> egui::Response {
    let (rect, response) =
        ui.allocate_exact_size(Vec2::new(CARD_WIDTH, CARD_HEIGHT), egui::Sense::click());
    let folder = is_folder(item);
    let visuals = ui.visuals().clone();
    let thumb = egui::Rect::from_center_size(
        egui::pos2(rect.center().x, rect.top() + 6.0 + THUMBNAIL_SIZE.y / 2.0),
        THUMBNAIL_SIZE,
    );
    let painter = ui.painter_at(rect);
    if response.hovered() || response.has_focus() {
        painter.rect_filled(rect, 10.0, visuals.widgets.hovered.weak_bg_fill);
    }
    if let Some(texture) = texture {
        egui::Image::new(texture)
            .fit_to_exact_size(THUMBNAIL_SIZE)
            .corner_radius(8.0)
            .paint_at(ui, thumb);
    } else {
        painter.rect_filled(thumb, 8.0, visuals.extreme_bg_color);
        let color = visuals.weak_text_color();
        if folder {
            folder_icon(&painter, thumb.center(), color);
        } else {
            painter.text(
                thumb.center(),
                egui::Align2::CENTER_CENTER,
                ui_text(ids::PHOTOS_KIND_PHOTO),
                egui::FontId::proportional(13.0),
                color,
            );
        }
    }
    let name_rect = egui::Rect::from_min_max(
        egui::pos2(rect.left() + 6.0, thumb.bottom() + 6.0),
        egui::pos2(rect.right() - 6.0, rect.bottom()),
    );
    let mut name_ui = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(name_rect)
            .layout(egui::Layout::top_down(egui::Align::Center)),
    );
    name_ui.add(
        egui::Label::new(egui::RichText::new(&item.name).color(visuals.text_color()))
            .truncate()
            .selectable(false),
    );
    if response.has_focus() {
        painter.rect_stroke(
            rect.shrink(1.0),
            10.0,
            egui::Stroke::new(2.0, visuals.selection.stroke.color),
            egui::StrokeKind::Inside,
        );
    }
    let label = if folder {
        ui_text_args(
            ids::PHOTOS_A11Y_OPEN_FOLDER,
            &[("name", item.name.as_str())],
        )
    } else {
        ui_text_args(ids::PHOTOS_A11Y_OPEN_PHOTO, &[("name", item.name.as_str())])
    };
    response.widget_info(move || egui::WidgetInfo::labeled(egui::Role::Button, true, &label));
    response.on_hover_text(item.name.as_str())
}

/// A folder outline: a tab on top of a body, centred on `center`.
fn folder_icon(painter: &egui::Painter, center: egui::Pos2, color: egui::Color32) {
    let stroke = egui::Stroke::new(2.0, color);
    let body = egui::Rect::from_center_size(center + Vec2::new(0.0, 4.0), Vec2::new(52.0, 36.0));
    painter.rect_stroke(body, 4.0, stroke, egui::StrokeKind::Middle);
    let tab =
        egui::Rect::from_min_size(body.left_top() - Vec2::new(0.0, 7.0), Vec2::new(20.0, 7.0));
    painter.rect_stroke(tab, 2.0, stroke, egui::StrokeKind::Middle);
}

/// How long the slideshow shows each photo once it has loaded.
const SLIDESHOW_INTERVAL_SECS: f64 = 5.0;

/// Slideshow state kept in egui memory: running, and when the shown photo appeared.
#[derive(Clone, Debug, Default, PartialEq)]
struct Slideshow {
    running: bool,
    photo_id: String,
    shown_at: f64,
}

/// Whether the slideshow should move on from a photo shown since `shown_at`.
fn slideshow_due(shown_at: f64, now: f64) -> bool {
    now - shown_at >= SLIDESHOW_INTERVAL_SECS
}

fn draw_full_size(ui: &mut Ui, view: &PhotosView<'_>, photo_id: &str) -> Option<PhotosAction> {
    let current_index = view.items.iter().position(|item| item.id == photo_id)?;
    let current = &view.items[current_index];
    ui.label(&current.name);
    let slideshow_id = egui::Id::unique("kattri-photo-slideshow");
    let now = ui.input(|input| input.time);
    let loaded = view.full_size_images.contains_key(photo_id);
    let mut slideshow: Slideshow = ui
        .ctx()
        .data(|data| data.get_temp(slideshow_id))
        .unwrap_or_default();
    if slideshow.photo_id != photo_id || !loaded {
        // The interval starts once the photo is on screen.
        slideshow.photo_id = photo_id.to_owned();
        slideshow.shown_at = now;
    }

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
            action = next.clone();
        }
        let toggle = if slideshow.running {
            ui_text(ids::PHOTOS_BUTTON_STOP_SLIDESHOW)
        } else {
            ui_text(ids::PHOTOS_BUTTON_SLIDESHOW)
        };
        if ui
            .add_enabled(
                next.is_some() || slideshow.running,
                egui::Button::new(toggle),
            )
            .clicked()
        {
            slideshow.running = !slideshow.running;
            slideshow.shown_at = now;
        }
        if slideshow.running && action.is_none() {
            match next {
                Some(next) if loaded && slideshow_due(slideshow.shown_at, now) => {
                    action = Some(next);
                }
                Some(_) => ui
                    .ctx()
                    .request_repaint_after(std::time::Duration::from_secs_f64(
                        (SLIDESHOW_INTERVAL_SECS - (now - slideshow.shown_at)).max(0.05),
                    )),
                // The last photo ends the slideshow.
                None => slideshow.running = false,
            }
        }
    });
    if matches!(action, Some(PhotosAction::ClosePhoto)) {
        slideshow.running = false;
    }
    ui.ctx()
        .data_mut(|data| data.insert_temp(slideshow_id, slideshow));
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
    let id = adjacent_photo_id(items, current_index, next)?.to_owned();
    Some(if next {
        PhotosAction::NextPhoto(id)
    } else {
        PhotosAction::PreviousPhoto(id)
    })
}

/// Share navigation's folder-skipping order with the full-size image prefetcher.
pub(crate) fn adjacent_photo_id(
    items: &[BaseItemDto],
    current_index: usize,
    next: bool,
) -> Option<&str> {
    let adjacent = if next {
        items
            .iter()
            .skip(current_index.saturating_add(1))
            .find(|item| item.item_type.as_deref() == Some("Photo"))
    } else {
        items
            .get(..current_index)?
            .iter()
            .rev()
            .find(|item| item.item_type.as_deref() == Some("Photo"))
    };
    Some(adjacent?.id.as_str())
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
        CARD_HEIGHT, MAX_COLUMNS, MAX_PAGE_ITEMS, PhotosAction, PhotosView, adjacent_photo_action,
        draw, grid_columns, ids, item_action, normalized_page_size, page_request, ui_text_args,
    };
    use egui::{Context, RawInput};
    use jellyfin_api::BaseItemDto;
    use std::collections::HashMap;

    #[test]
    fn slideshow_moves_on_after_the_interval() {
        assert!(!super::slideshow_due(10.0, 14.9));
        assert!(super::slideshow_due(10.0, 15.0));
    }

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
    fn folder_cards_are_named_and_whole_inside_a_scrolling_page() {
        let items = vec![
            item("a", "Summer 2024", "Folder"),
            item("b", "Grandparents' 50th anniversary party", "Folder"),
            item("c", "Beach", "Photo"),
        ];
        let thumbnails = HashMap::new();
        let full = HashMap::new();
        let mut visible = Vec::new();
        let context = Context::default();
        context.enable_accesskit();
        let mut output = context.run_ui(RawInput::default(), |ui| {
            // The app draws this page inside its own vertical scroll area.
            egui::ScrollArea::vertical().show(ui, |ui| {
                let _ = draw(
                    ui,
                    &mut PhotosView {
                        folder_title: "Photos",
                        items: &items,
                        thumbnails: &thumbnails,
                        full_size_images: &full,
                        loading: false,
                        error: None,
                        page_start: 0,
                        page_size: 100,
                        total_count: Some(3),
                        selected_photo_id: None,
                        full_size_error: false,
                        visible_thumbnails: &mut visible,
                    },
                );
            });
        });
        output.textures_delta.clear();
        let update = output.platform_output.accesskit_update.expect("tree");
        for name in ["Summer 2024", "Grandparents' 50th anniversary party"] {
            let label = ui_text_args(ids::PHOTOS_A11Y_OPEN_FOLDER, &[("name", name)]);
            let (_, card) = update
                .nodes
                .iter()
                .find(|(_, node)| node.label() == Some(label.as_str()))
                .unwrap_or_else(|| panic!("card for {name}"));
            let bounds = card.bounds().expect("bounds");
            assert!(
                bounds.y1 - bounds.y0 >= f64::from(CARD_HEIGHT) - 1.0,
                "{name}"
            );
        }
        // The visible name is drawn as text too, truncated to the card width when long.
        assert!(
            update
                .nodes
                .iter()
                .any(|(_, node)| node.value().or(node.label()) == Some("Summer 2024"))
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
    fn full_size_navigation_skips_folders_between_photos() {
        let items = [
            item("a", "First", "Photo"),
            item("folder", "Album", "Folder"),
            item("b", "Second", "Photo"),
            item("collection", "Collection", "CollectionFolder"),
            item("c", "Third", "Photo"),
        ];

        assert_eq!(
            adjacent_photo_action(&items, 0, true),
            Some(PhotosAction::NextPhoto("b".to_owned()))
        );
        assert_eq!(
            adjacent_photo_action(&items, 2, true),
            Some(PhotosAction::NextPhoto("c".to_owned()))
        );
        assert_eq!(
            adjacent_photo_action(&items, 4, false),
            Some(PhotosAction::PreviousPhoto("b".to_owned()))
        );
        assert_eq!(
            adjacent_photo_action(&items, 1, true),
            Some(PhotosAction::NextPhoto("b".to_owned()))
        );
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
