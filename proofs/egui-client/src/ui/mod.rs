pub(crate) mod admin;
pub mod anim;
mod artwork;
pub(crate) mod detail;
pub mod downloads;
pub mod hero;
pub mod home;
pub(crate) mod item_menu;
pub mod live_tv;
pub mod music;
pub(crate) mod open_url;
mod people;
pub mod photos;
pub mod playlists;
// Not drawn on any screen yet: the Settings "Switch profile" entry and the launch-time picker
// are wired in a later change. Only the unit tests use it until then.
#[allow(dead_code)]
pub mod profile_picker;
pub mod row;
pub(crate) mod search;
mod series;
mod series_memory;
mod settings;
mod settings_input;
mod settings_media;
mod settings_parental;
pub(crate) mod status;
pub(crate) mod syncplay;
mod update;

use crate::app::{
    AppView, BrowseContext, CatalogPlayedFilter, CatalogSort, ConnectionStatus, KattriApp, Page,
    duration_from_egui_time,
};
use crate::catalog::{MediaItem, MediaKind};
use crate::sign_in::{pending_code, status_text};
use crate::theme::{self, Palette};
use crate::ui::home::HomeAction;
use crate::worker::HomeRowKind;
use egui::{self, Align, Align2, Color32, CornerRadius, FontId, RichText, Stroke};
use jellyfin_api::{ItemFilter, ItemSortOrder};
use kattri_client_core::focus::{FocusGraph, FocusId, FocusNode, NavAction};
use kattri_client_core::playback_plan::{PlaybackMethod, select_playback_info_source_for_profile};
use kattri_i18n::ids;
use std::collections::BTreeMap;
use std::sync::OnceLock;
use std::time::Duration;

/// Gives a control the accessible name the catalog holds for `id`. The visible label does not
/// change; only assistive technology reads the name.
fn with_access_name(response: egui::Response, kind: egui::Role, id: &str) -> egui::Response {
    let enabled = response.enabled();
    let name = ui_text(id).to_owned();
    response.widget_info(|| egui::WidgetInfo::labeled(kind, enabled, name.clone()));
    response
}

/// The catalog name of the library button a state panel shows for `action`, if any.
fn state_access_id(action: StateAction) -> Option<&'static str> {
    match action {
        StateAction::RetryItems => Some(ids::ACCESS_BROWSE_RETRY),
        StateAction::ClearCatalogFilters => Some(ids::ACCESS_BROWSE_CLEAR_FILTERS),
        StateAction::ClearSearch | StateAction::OpenHome => None,
    }
}

/// The accessible name for a control, read from the en-US catalog. The catalog is parsed once.
/// A missing message falls back to the English literal, so a raw id never reaches a screen reader.
fn accessible_name(id: &str, fallback: &'static str) -> String {
    static CATALOG: OnceLock<Option<kattri_i18n::Catalog>> = OnceLock::new();
    CATALOG
        .get_or_init(|| kattri_i18n::english().ok())
        .as_ref()
        .and_then(|catalog| catalog.get(id))
        .unwrap_or(fallback)
        .to_owned()
}

/// The en-US catalog, rendered once. The templates are kept for messages with placeholders.
struct UiCatalog {
    plain: BTreeMap<String, String>,
    templates: BTreeMap<String, String>,
}

fn ui_catalog() -> Option<&'static UiCatalog> {
    static CATALOG: OnceLock<Option<UiCatalog>> = OnceLock::new();
    CATALOG
        .get_or_init(|| {
            let catalog = kattri_i18n::english().ok()?;
            let mut plain = BTreeMap::new();
            let mut templates = BTreeMap::new();
            for id in catalog.ids() {
                let template = catalog.get(id)?;
                let rendered = kattri_i18n::render(template, &[]).ok()?;
                plain.insert(id.to_owned(), rendered.text);
                templates.insert(id.to_owned(), template.to_owned());
            }
            Some(UiCatalog { plain, templates })
        })
        .as_ref()
}

/// Catalog text for a message without placeholders. A missing id shows the catalog's missing
/// marker, never the raw id.
pub(crate) fn ui_text(id: &str) -> &'static str {
    ui_catalog()
        .and_then(|catalog| catalog.plain.get(id))
        .map_or(kattri_i18n::MISSING_TEXT, String::as_str)
}

/// Catalog text with `{name}` placeholders filled from `args`. Values are inserted literally.
pub(crate) fn ui_text_args(id: &str, args: &[(&str, &str)]) -> String {
    let Some(template) = ui_catalog().and_then(|catalog| catalog.templates.get(id)) else {
        return kattri_i18n::MISSING_TEXT.to_owned();
    };
    kattri_i18n::render(template, args).map_or_else(
        |_| kattri_i18n::MISSING_TEXT.to_owned(),
        |rendered| rendered.text,
    )
}

thread_local! {
    static DETAIL_PANEL_STATE: std::cell::RefCell<Option<(egui::Id, detail::DetailPanel)>> =
        const { std::cell::RefCell::new(None) };
    static SERIES_PRESENTATION_STATE: std::cell::RefCell<Option<(egui::Id, series::SeriesPresentation)>> =
        const { std::cell::RefCell::new(None) };
}

/// Per-frame switches for [`move_media_focus`].
struct MediaFocusOptions<'a> {
    allow_key_repeat: bool,
    enter_content: bool,
    /// The top bar tab for the current view; Up from the first row moves focus to it.
    top_bar_tab: Option<&'a egui::Response>,
}

struct CardFocus {
    row: usize,
    id: FocusId,
    center_x: f32,
    focused: bool,
    response: egui::Response,
    home_poster: Option<(HomePosterGroup, usize)>,
    selection: HomeAction,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum FocusActivation {
    Home(HomeAction),
    SearchShowAll(search::SearchGroup),
}

#[derive(Clone, Copy)]
pub(super) enum HomePosterGroup {
    Collections,
    Row(HomeRowKind),
}

struct ShelfContent<'a, I> {
    title: &'a str,
    description: &'a str,
    items: I,
    row: usize,
}

pub fn draw(root: &mut egui::Ui, app: &mut KattriApp, window_width: f32) {
    let ctx = root.ctx().clone();
    let colors = theme::palette(app.high_contrast);
    let mut gamepad_actions = if app.settings.settings().input.sdl_gamepad_enabled {
        crate::input::poll_gamepad(app.gamepad.as_mut()).unwrap_or_default()
    } else {
        Vec::new()
    };
    if !gamepad_actions.is_empty() {
        app.cancel_network_retry_for_user_action();
    }
    // While a video fills the window the library page is hidden; the player takes the pad.
    if app.player_owns_input() {
        app.player_nav_actions.append(&mut gamepad_actions);
    }

    let nav_focus = egui::Panel::top("topbar")
        .exact_size(64.0)
        .frame(egui::Frame::new().fill(colors.background))
        .show(root, |ui| topbar(ui, app, colors))
        .inner;

    if app.network_notice.is_some() {
        egui::Panel::top("network_status_banner")
            .exact_size(42.0)
            .frame(egui::Frame::new().fill(colors.panel))
            .show(root, |ui| status::show(ui, app, colors));
    }

    let enter_content = move_navigation_focus(&ctx, &nav_focus, app, &mut gamepad_actions);
    // Keep each destination's scroll position independent; opening details starts at its title.
    let content_scroll_id = egui::Id::unique((
        "library_content_scroll",
        format!("{:?}:{:?}", app.view, app.page),
        app.selected_item_id.as_deref(),
    ));

    let (card_focus, visible_artwork, search_show_all_targets) = egui::CentralPanel::default()
        .frame(egui::Frame::new().fill(colors.background))
        .show(root, |ui| {
            let mut entries = Vec::new();
            let mut search_show_all_targets = Vec::new();
            let mut visible_artwork = Vec::new();
            egui::ScrollArea::vertical()
                .id_salt(content_scroll_id)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.add_space(22.0);
                    ui.horizontal(|ui| {
                        ui.add_space(24.0);
                        let connection = app.connection.clone();
                        ui.vertical(|ui| {
                            // A right gutter that matches the left one.
                            ui.set_max_width((ui.available_width() - 24.0).max(240.0));
                            if app.view == AppView::Library && app.page == Page::Settings {
                                settings::show(ui, app, colors, &mut gamepad_actions);
                                return;
                            }
                            match connection {
                                ConnectionStatus::Disconnected { error } => {
                                    sign_in_form(ui, app, colors, error.as_deref());
                                }
                                ConnectionStatus::Connecting { message }
                                    if app.quick_connect.is_some() =>
                                {
                                    sign_in_form(ui, app, colors, None);
                                    ui.label(RichText::new(message).color(colors.muted));
                                }
                                ConnectionStatus::Connecting { message } => {
                                    connection_progress(ui, colors, &message);
                                }
                                ConnectionStatus::LoadingItems if app.view == AppView::Home => {
                                    render_connected(
                                        ui,
                                        app,
                                        colors,
                                        &mut entries,
                                        &mut search_show_all_targets,
                                        &mut visible_artwork,
                                        &mut gamepad_actions,
                                    );
                                }
                                ConnectionStatus::LoadingItems => {
                                    connection_progress(
                                        ui,
                                        colors,
                                        ui_text(ids::BROWSE_STATE_LOADING_FIRST_PAGE),
                                    );
                                }
                                ConnectionStatus::Connected => {
                                    render_connected(
                                        ui,
                                        app,
                                        colors,
                                        &mut entries,
                                        &mut search_show_all_targets,
                                        &mut visible_artwork,
                                        &mut gamepad_actions,
                                    );
                                }
                                ConnectionStatus::LibraryError(message) => {
                                    if app.server.is_some() {
                                        render_connected(
                                            ui,
                                            app,
                                            colors,
                                            &mut entries,
                                            &mut search_show_all_targets,
                                            &mut visible_artwork,
                                            &mut gamepad_actions,
                                        );
                                    } else {
                                        state_panel(
                                            ui,
                                            colors,
                                            ui_text(ids::BROWSE_ERROR_LOAD_FAILED),
                                            &message,
                                            Some((
                                                ui_text(ids::BROWSE_BUTTON_RETRY),
                                                StateAction::RetryItems,
                                            )),
                                            app,
                                        );
                                    }
                                }
                            }
                        });
                        ui.add_space(24.0);
                    });
                    ui.add_space(32.0);
                });
            (entries, visible_artwork, search_show_all_targets)
        })
        .inner;
    app.request_visible_artwork(visible_artwork);
    if app.view == AppView::Library && app.page.is_catalog() {
        // Each card occupies 150 px plus the 6 px shelf gap. Treat the number
        // visible across this window as one row when scheduling the next page.
        let items_per_row = ((window_width - 80.0) / 156.0).floor().max(1.0) as usize;
        let grouped_home_catalog = app.page == Page::Home && app.submitted_search.is_none();
        for entry in &card_focus {
            if !entry.focused {
                continue;
            }
            if let HomeAction::Select(item_id) = &entry.selection {
                let index = if grouped_home_catalog {
                    // Home shelves regroup the source catalog by media kind. Card focus entries
                    // are appended by `shelf()` in the same order those cards are displayed.
                    displayed_catalog_item_index(
                        card_focus
                            .iter()
                            .filter_map(|entry| match &entry.selection {
                                HomeAction::Select(id) => Some(id.as_str()),
                                _ => None,
                            }),
                        item_id,
                    )
                } else {
                    // Typed catalog and search views keep the previous source-list indexing.
                    app.items.iter().position(|item| item.id == *item_id)
                };
                if let Some(index) = index {
                    app.maybe_request_next_catalog_page(index, items_per_row);
                    break;
                }
            }
        }
    }
    let allow_key_repeat = app.settings.settings().input.key_repeat;
    let top_bar_index = active_topbar_index(app);
    if let Some(target_id) = move_media_focus(
        &ctx,
        &card_focus,
        &mut app.focus_graph,
        &mut app.home,
        &mut gamepad_actions,
        MediaFocusOptions {
            allow_key_repeat,
            enter_content,
            top_bar_tab: nav_focus.get(top_bar_index),
        },
    ) && let Some(action) =
        resolve_focus_activation(&target_id, &card_focus, &search_show_all_targets)
    {
        match action {
            FocusActivation::Home(action) => app.apply_home_action(action),
            FocusActivation::SearchShowAll(group) => app.show_all_search_group(group),
        }
    }
    handle_shell_action(&ctx, app, &mut gamepad_actions);
}

fn displayed_catalog_item_index<'a>(
    displayed_item_ids: impl IntoIterator<Item = &'a str>,
    focused_item_id: &str,
) -> Option<usize> {
    displayed_item_ids
        .into_iter()
        .position(|item_id| item_id == focused_item_id)
}

#[cfg(test)]
mod catalog_prefetch_tests {
    use super::displayed_catalog_item_index;
    use kattri_client_core::paging::PagingState;

    #[test]
    fn interleaved_home_shelves_prefetch_by_displayed_position() {
        let mut catalog = Vec::new();
        for index in 1..=10 {
            catalog.push((format!("movie-{index}"), "movie"));
            catalog.push((format!("folder-{index}"), "folder"));
        }
        catalog.push(("movie-11".to_owned(), "movie"));

        let original_item_ids: Vec<_> = catalog.iter().map(|(id, _)| id.as_str()).collect();
        let mut displayed_item_ids: Vec<_> = catalog
            .iter()
            .filter(|(_, kind)| *kind == "movie")
            .map(|(id, _)| id.as_str())
            .collect();
        displayed_item_ids.extend(
            catalog
                .iter()
                .filter(|(_, kind)| *kind == "folder")
                .map(|(id, _)| id.as_str()),
        );

        let focused_item_id = "folder-7";
        let original_index = original_item_ids
            .iter()
            .position(|id| *id == focused_item_id)
            .expect("the focused item is in the source catalog");
        let displayed_index =
            displayed_catalog_item_index(displayed_item_ids.iter().copied(), focused_item_id)
                .expect("the focused item is in the rendered shelves");

        assert_eq!(original_index, 13);
        assert_eq!(displayed_index, 17);
        assert!(!PagingState::focus_within_prefetch_range(
            original_index,
            catalog.len(),
            3,
        ));
        assert!(PagingState::focus_within_prefetch_range(
            displayed_index,
            catalog.len(),
            3,
        ));
    }
}

/// Apple TV style top bar: brand at the left, centred text tabs, search and profile at the
/// right. Returns the tab responses in order (Home first, then the pages of
/// `navigation_pages`) for keyboard and gamepad focus.
fn topbar(ui: &mut egui::Ui, app: &mut KattriApp, colors: Palette) -> Vec<egui::Response> {
    let mut responses = Vec::new();
    let mut submit_search = false;
    let mut clear_search = false;
    let mut search_changed = false;
    let connected = has_server(&app.connection);
    let font = FontId::proportional(16.0);
    ui.horizontal_centered(|ui| {
        ui.add_space(20.0);
        ui.label(
            RichText::new(ui_text(ids::SHELL_BRAND_NAME))
                .size(20.0)
                .strong()
                .color(colors.text),
        );
        if connected {
            let mut tabs: Vec<(String, Option<Page>)> =
                vec![(ui_text(ids::SHELL_NAV_HOME).to_owned(), None)];
            for page in navigation_pages(app.dashboard_enabled()) {
                tabs.push((page.label().to_owned(), Some(page)));
            }
            // Reserve room for Search and the account menu even when every library tab is shown.
            let nav_width = (ui.available_width() - 340.0).max(100.0);
            // Pick the roomiest size at which every tab fits; the tightest is the floor.
            let mut font = font.clone();
            let mut padding = 28.0;
            for (size, pad) in [(16.0, 28.0), (15.0, 20.0), (14.0, 14.0), (13.0, 10.0)] {
                font = FontId::proportional(size);
                padding = pad;
                let total: f32 = tabs
                    .iter()
                    .map(|(label, _)| {
                        ui.painter()
                            .layout_no_wrap(label.clone(), font.clone(), colors.text)
                            .size()
                            .x
                            + pad
                            + 4.0
                    })
                    .sum();
                if total <= nav_width {
                    break;
                }
            }
            let widths: Vec<f32> = tabs
                .iter()
                .map(|(label, _)| {
                    ui.painter()
                        .layout_no_wrap(label.clone(), font.clone(), colors.text)
                        .size()
                        .x
                        + padding
                })
                .collect();
            ui.allocate_ui_with_layout(
                egui::vec2(nav_width, 44.0),
                egui::Layout::left_to_right(Align::Center),
                |ui| {
                    egui::ScrollArea::horizontal()
                        .id_salt("topbar_tabs")
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 4.0;
                                for ((label, page), width) in tabs.into_iter().zip(widths) {
                                    let active = match page {
                                        None => app.view == AppView::Home,
                                        Some(page) => {
                                            app.view == AppView::Library && app.page == page
                                        }
                                    };
                                    let (rect, response) = ui.allocate_exact_size(
                                        egui::vec2(width, 36.0),
                                        egui::Sense::click(),
                                    );
                                    let painter = ui.painter_at(rect);
                                    if active {
                                        painter.rect_filled(
                                            rect,
                                            CornerRadius::same(10),
                                            Color32::from_white_alpha(30),
                                        );
                                    } else if response.hovered() {
                                        painter.rect_filled(
                                            rect,
                                            CornerRadius::same(10),
                                            Color32::from_white_alpha(14),
                                        );
                                    }
                                    if response.has_focus() {
                                        painter.rect_stroke(
                                            rect.shrink(1.0),
                                            CornerRadius::same(10),
                                            Stroke::new(2.0, colors.focus),
                                            egui::StrokeKind::Inside,
                                        );
                                    }
                                    painter.text(
                                        rect.center(),
                                        Align2::CENTER_CENTER,
                                        &label,
                                        font.clone(),
                                        if active { colors.text } else { colors.muted },
                                    );
                                    let name = label.clone();
                                    response.widget_info(move || {
                                        egui::WidgetInfo::selected(
                                            egui::Role::Button,
                                            true,
                                            active,
                                            name.clone(),
                                        )
                                    });
                                    if response.clicked() {
                                        match page {
                                            None => app.set_view(AppView::Home),
                                            Some(page) => app.navigate_to(page),
                                        }
                                    }
                                    if response.has_focus() {
                                        response.scroll_to_me(Some(Align::Center));
                                    }
                                    responses.push(response);
                                }
                            });
                        });
                },
            );
        }
        ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
            ui.add_space(20.0);
            if connected {
                let profile = app
                    .user_name
                    .clone()
                    .unwrap_or_else(|| ui_text(ids::SHELL_PROFILE_MAIN).to_owned());
                let server_name = app.server.as_ref().map(|server| server.name.clone());
                ui.menu_button(
                    RichText::new(format!("{profile}  ▾")).color(colors.text),
                    |ui| {
                        if let Some(name) = server_name {
                            ui.label(RichText::new(name).color(colors.muted));
                        }
                        // All destinations remain directly reachable when the tab strip scrolls.
                        for page in navigation_pages(app.dashboard_enabled()) {
                            if ui.button(page.label()).clicked() {
                                app.navigate_to(page);
                                ui.close();
                            }
                        }
                        ui.separator();
                        if ui.button(ui_text(ids::OPEN_URL_MENU)).clicked() {
                            app.open_stream_address_dialog();
                            ui.close();
                        }
                        let high_contrast = ui.checkbox(
                            &mut app.high_contrast,
                            ui_text(ids::ACCESS_NAV_HIGH_CONTRAST),
                        );
                        if high_contrast.changed() {
                            let before = app.settings.settings().clone();
                            app.settings.settings_mut().general.high_contrast = app.high_contrast;
                            app.commit_settings(&before);
                        }
                        if app.view == AppView::Home
                            && ui.button(ui_text(ids::HOME_BUTTON_REFRESH)).clicked()
                        {
                            app.apply_home_action(home::HomeAction::Refresh);
                            ui.close();
                        }
                        if ui.button(ui_text(ids::ACCESS_NAV_DISCONNECT)).clicked() {
                            app.disconnect();
                            ui.close();
                        }
                    },
                );
            } else {
                let high_contrast = ui.checkbox(
                    &mut app.high_contrast,
                    ui_text(ids::ACCESS_NAV_HIGH_CONTRAST),
                );
                if high_contrast.changed() {
                    let before = app.settings.settings().clone();
                    app.settings.settings_mut().general.high_contrast = app.high_contrast;
                    app.commit_settings(&before);
                }
            }
            if connected
                && (app.view == AppView::Home
                    || !matches!(
                        app.page,
                        Page::Settings
                            | Page::Downloads
                            | Page::LiveTv
                            | Page::Dashboard
                            | Page::SyncPlay
                    ))
            {
                let search_width = (ui.available_width() - 12.0).clamp(140.0, 300.0);
                let search = ui.add_sized(
                    [search_width, 36.0],
                    egui::TextEdit::singleline(&mut app.search_query)
                        .hint_text(ui_text(ids::BROWSE_SEARCH_HINT))
                        .margin(egui::Margin {
                            left: 36,
                            right: 14,
                            top: 9,
                            bottom: 9,
                        }),
                );
                search_icon(ui.painter(), search.rect, colors.muted);
                search_changed = search.changed();
                search.widget_info(|| {
                    egui::WidgetInfo::labeled(
                        egui::Role::TextInput,
                        true,
                        ui_text(ids::ACCESS_BROWSE_SEARCH_FIELD),
                    )
                });
                submit_search |=
                    search.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter));
                if (!app.search_query.is_empty()
                    || app.submitted_search.is_some()
                    || app.search_active)
                    && with_access_name(
                        ui.button(ui_text(ids::BROWSE_BUTTON_CLEAR_SEARCH)),
                        egui::Role::Button,
                        ids::ACCESS_BROWSE_SEARCH_CLEAR,
                    )
                    .clicked()
                {
                    clear_search = true;
                }
            }
        });
    });
    let search_now = duration_from_egui_time(ui.ctx().input(|input| input.time));
    if clear_search {
        app.clear_search(search_now);
    } else if submit_search || search_changed {
        if app.view == AppView::Home {
            app.set_view(AppView::Library);
            app.page = Page::Home;
        }
        app.submit_search(search_now);
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(300));
    }
    responses
}

/// Index in the `topbar` responses of the tab for the current view, so focus can return to it.
fn active_topbar_index(app: &KattriApp) -> usize {
    if app.view == AppView::Home {
        return 0;
    }
    navigation_pages(app.dashboard_enabled())
        .iter()
        .position(|page| *page == app.page)
        .map_or(0, |index| index + 1)
}

/// A magnifier drawn inside the left padding of the search field.
fn search_icon(painter: &egui::Painter, field: egui::Rect, color: Color32) {
    let center = egui::pos2(field.left() + 18.0, field.center().y - 1.0);
    let stroke = Stroke::new(1.6, color);
    painter.circle_stroke(center, 5.5, stroke);
    painter.line_segment(
        [center + egui::vec2(4.0, 4.0), center + egui::vec2(8.0, 8.0)],
        stroke,
    );
}

#[cfg_attr(not(test), allow(dead_code))]
// Vector icons stay sharp at every UI scale without relying on symbol-font coverage.
fn navigation_icon(painter: &egui::Painter, center: egui::Pos2, page: Page, color: Color32) {
    let stroke = Stroke::new(1.6, color);
    let point = |x, y| center + egui::vec2(x, y);
    let line = |a: [f32; 2], b: [f32; 2]| {
        painter.line_segment([point(a[0], a[1]), point(b[0], b[1])], stroke);
    };
    let outline = |min: [f32; 2], max: [f32; 2]| {
        painter.rect_stroke(
            egui::Rect::from_min_max(point(min[0], min[1]), point(max[0], max[1])),
            CornerRadius::same(2),
            stroke,
            egui::StrokeKind::Inside,
        );
    };
    match page {
        Page::Home => {
            for x in [-8.0, 1.0] {
                for y in [-8.0, 1.0] {
                    outline([x, y], [x + 7.0, y + 7.0]);
                }
            }
        }
        Page::Movies => {
            outline([-9.0, -7.0], [9.0, 7.0]);
            for x in [-5.0, 5.0] {
                line([x, -7.0], [x, 7.0]);
            }
            for y in [-2.0, 3.0] {
                line([-9.0, y], [-5.0, y]);
                line([5.0, y], [9.0, y]);
            }
        }
        Page::Series => {
            outline([-9.0, -8.0], [6.0, 4.0]);
            outline([-5.0, -4.0], [9.0, 8.0]);
        }
        Page::Music => {
            line([-3.0, 5.0], [-3.0, -6.0]);
            line([-3.0, -6.0], [7.0, -8.0]);
            line([7.0, -8.0], [7.0, 3.0]);
            painter.circle_filled(point(-5.0, 6.0), 3.0, color);
            painter.circle_filled(point(5.0, 4.0), 3.0, color);
        }
        Page::Photos => {
            outline([-9.0, -6.0], [9.0, 7.0]);
            painter.circle_stroke(point(4.0, -2.0), 2.0, stroke);
            line([-7.0, 5.0], [-2.0, 0.0]);
            line([-2.0, 0.0], [2.0, 4.0]);
            line([2.0, 4.0], [5.0, 1.0]);
            line([5.0, 1.0], [8.0, 4.0]);
        }
        Page::Playlists => {
            for y in [-6.0, 0.0, 6.0] {
                line([-8.0, y], [1.0, y]);
            }
            line([5.0, -3.0], [9.0, 0.0]);
            line([9.0, 0.0], [5.0, 3.0]);
            line([5.0, 3.0], [5.0, -3.0]);
        }
        Page::Downloads => {
            line([0.0, -8.0], [0.0, 3.0]);
            line([-4.0, -1.0], [0.0, 3.0]);
            line([0.0, 3.0], [4.0, -1.0]);
            line([-8.0, 4.0], [-8.0, 8.0]);
            line([-8.0, 8.0], [8.0, 8.0]);
            line([8.0, 8.0], [8.0, 4.0]);
        }
        Page::LiveTv => {
            outline([-9.0, -4.0], [9.0, 8.0]);
            line([-5.0, -9.0], [0.0, -4.0]);
            line([0.0, -4.0], [5.0, -9.0]);
        }
        Page::Dashboard => {
            line([-9.0, 8.0], [9.0, 8.0]);
            for (x, top) in [(-6.0, -2.0), (-1.0, -7.0), (4.0, -4.0)] {
                outline([x, top], [x + 3.0, 8.0]);
            }
        }
        Page::SyncPlay => {
            painter.circle_stroke(point(-4.0, -3.0), 4.0, stroke);
            painter.circle_stroke(point(5.0, -3.0), 4.0, stroke);
            line([-10.0, 8.0], [-8.0, 3.0]);
            line([-8.0, 3.0], [-3.0, 1.0]);
            line([-3.0, 1.0], [0.0, 3.0]);
            line([0.0, 3.0], [3.0, 1.0]);
            line([3.0, 1.0], [8.0, 3.0]);
            line([8.0, 3.0], [10.0, 8.0]);
        }
        Page::Settings => {
            painter.circle_stroke(center, 5.5, stroke);
            painter.circle_stroke(center, 2.0, stroke);
            for index in 0..8 {
                let direction = egui::Vec2::angled(index as f32 * std::f32::consts::TAU / 8.0);
                painter.line_segment([center + direction * 6.0, center + direction * 9.0], stroke);
            }
        }
    }
}

#[cfg(test)]
mod navigation_icon_tests {
    use super::{Page, navigation_icon};

    #[test]
    fn every_library_icon_is_drawn_inside_its_compact_hit_target() {
        for page in Page::ALL {
            let ctx = egui::Context::default();
            let center = egui::pos2(40.0, 40.0);
            let target = egui::Rect::from_center_size(center, egui::vec2(40.0, 40.0));
            let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
                navigation_icon(ui.painter(), center, page, egui::Color32::WHITE);
            });
            // Headless geometry inspection has no renderer to upload the font atlas.
            output.textures_delta.clear();
            assert!(!output.shapes.is_empty(), "missing icon for {page:?}");
            for shape in output.shapes {
                let bounds = shape.shape.visual_bounding_rect();
                if bounds.is_positive() {
                    assert!(target.contains_rect(bounds), "icon overflow for {page:?}");
                }
            }
        }
    }
}

fn sign_in_form(ui: &mut egui::Ui, app: &mut KattriApp, colors: Palette, error: Option<&str>) {
    ui.set_max_width(650.0);
    ui.add_space(48.0);
    ui.label(
        RichText::new(ui_text(ids::BROWSE_HEADING))
            .size(30.0)
            .strong()
            .color(colors.text),
    );
    ui.add_space(8.0);
    ui.label(
        RichText::new(ui_text(ids::CONNECT_INTRO))
            .size(14.0)
            .color(colors.muted),
    );
    ui.add_space(24.0);

    egui::Frame::new()
        .fill(colors.panel)
        .stroke(Stroke::new(1.0, colors.divider))
        .corner_radius(CornerRadius::same(12))
        .inner_margin(egui::Margin::same(24))
        .show(ui, |ui| {
            ui.set_min_width(430.0_f32.min(ui.available_width()));
            let mut quick_connect_probe_requested = false;
            let saved_servers = app
                .saved_servers
                .entries()
                .iter()
                .map(|entry| {
                    (
                        entry.display_name().to_owned(),
                        entry.base_url().to_owned(),
                        entry.allow_insecure_remote_http(),
                    )
                })
                .collect::<Vec<_>>();
            if !saved_servers.is_empty() {
                ui.label(
                    RichText::new(ui_text(ids::CONNECT_SAVED_SERVERS))
                        .strong()
                        .color(colors.text),
                );
                ui.horizontal_wrapped(|ui| {
                    for (name, url, allow_http) in saved_servers {
                        if ui.button(name).clicked() {
                            app.select_saved_server(&url, allow_http);
                            quick_connect_probe_requested = true;
                        }
                    }
                });
                ui.add_space(12.0);
            }
            if let Some(notice) = app.saved_servers.notice() {
                ui.label(RichText::new(notice).color(Color32::from_rgb(0xE8, 0xB9, 0x77)));
                ui.add_space(8.0);
            }
            ui.label(
                RichText::new(ui_text(ids::CONNECT_FIELD_SERVER_ADDRESS))
                    .strong()
                    .color(colors.text),
            );
            let quick_connect_active = app.quick_connect.is_some();
            let server_address_response = ui
                .add_enabled_ui(!quick_connect_active, |ui| {
                    ui.add_sized(
                        [ui.available_width(), 40.0],
                        egui::TextEdit::singleline(&mut app.server_url)
                            .hint_text(ui_text(ids::CONNECT_HINT_SERVER_ADDRESS))
                            .desired_width(f32::INFINITY),
                    )
                })
                .inner;
            server_address_response.widget_info(|| {
                egui::WidgetInfo::labeled(
                    egui::Role::TextInput,
                    !quick_connect_active,
                    accessible_name(
                        ids::ACCESS_CONNECT_SERVER_ADDRESS,
                        ui_text(ids::ACCESS_CONNECT_SERVER_ADDRESS),
                    ),
                )
            });
            if server_address_response.changed() {
                app.server_address_changed(duration_from_egui_time(ui.input(|input| input.time)));
                quick_connect_probe_requested = true;
            }

            let is_http = app
                .server_url
                .trim()
                .to_ascii_lowercase()
                .starts_with("http://");
            if is_http {
                ui.add_space(16.0);
                let warning = Color32::from_rgb(0xE8, 0xB9, 0x77);
                egui::Frame::new()
                    .fill(Color32::from_rgba_unmultiplied(232, 185, 119, 18))
                    .stroke(Stroke::new(1.0, warning))
                    .corner_radius(CornerRadius::same(8))
                    .inner_margin(egui::Margin::same(12))
                    .show(ui, |ui| {
                        ui.label(
                            RichText::new(ui_text(ids::CONNECT_WARNING_HTTP_TITLE))
                                .strong()
                                .color(warning),
                        );
                        ui.label(
                            RichText::new(ui_text(ids::CONNECT_WARNING_HTTP_BODY))
                                .size(12.0)
                                .color(colors.text),
                        );
                        ui.add_space(6.0);
                        let opt_in_response = with_access_name(
                            ui.checkbox(
                                &mut app.allow_insecure_remote_http,
                                ui_text(ids::CONNECT_OPTION_ALLOW_PUBLIC_HTTP),
                            ),
                            egui::Role::CheckBox,
                            ids::ACCESS_CONNECT_ALLOW_PUBLIC_HTTP,
                        );
                        if opt_in_response.changed() {
                            app.server_address_changed(duration_from_egui_time(
                                ui.input(|input| input.time),
                            ));
                            quick_connect_probe_requested = true;
                        }
                        ui.label(
                            RichText::new(ui_text(ids::CONNECT_NOTE_LAN_HTTP))
                                .size(11.0)
                                .color(colors.muted),
                        );
                    });
            }

            if let Some(server) = app.visible_server_preview() {
                let version = server
                    .version
                    .as_deref()
                    .unwrap_or(ui_text(ids::SHELL_SERVER_VERSION_UNKNOWN));
                ui.label(
                    RichText::new(ui_text_args(
                        ids::CONNECT_SUMMARY_SERVER,
                        &[("name", &server.name), ("version", version)],
                    ))
                    .strong()
                    .color(colors.text),
                );
                ui.add_space(12.0);
                ui.label(
                    RichText::new(ui_text(ids::CONNECT_FIELD_USERNAME))
                        .strong()
                        .color(colors.text),
                );
                let username_response = ui.add_sized(
                    [ui.available_width(), 40.0],
                    egui::TextEdit::singleline(&mut app.username)
                        .hint_text(ui_text(ids::CONNECT_HINT_USERNAME))
                        .desired_width(f32::INFINITY),
                );
                username_response.widget_info(|| {
                    egui::WidgetInfo::labeled(
                        egui::Role::TextInput,
                        true,
                        accessible_name(
                            ids::ACCESS_CONNECT_USERNAME,
                            ui_text(ids::ACCESS_CONNECT_USERNAME),
                        ),
                    )
                });
                ui.add_space(12.0);
                ui.label(
                    RichText::new(ui_text(ids::CONNECT_FIELD_PASSWORD))
                        .strong()
                        .color(colors.text),
                );
                let password_response = ui.add_sized(
                    [ui.available_width(), 40.0],
                    egui::TextEdit::singleline(&mut *app.password_input)
                        .password(true)
                        .hint_text(ui_text(ids::CONNECT_HINT_PASSWORD))
                        .desired_width(f32::INFINITY),
                );
                // Labels only: the password value must not reach the accessibility tree.
                password_response.widget_info(|| {
                    egui::WidgetInfo::labeled(
                        egui::Role::TextInput,
                        true,
                        accessible_name(
                            ids::ACCESS_CONNECT_PASSWORD,
                            ui_text(ids::ACCESS_CONNECT_PASSWORD),
                        ),
                    )
                });
                if app.saved_sign_in_available != Some(false) {
                    ui.add_space(8.0);
                    ui.checkbox(
                        &mut app.remember_sign_in,
                        ui_text(ids::CONNECT_OPTION_REMEMBER_SIGN_IN),
                    )
                    .on_hover_text(ui_text(ids::CONNECT_NOTE_REMEMBER_SIGN_IN));
                }
            } else {
                ui.add_space(12.0);
                if app.server_check_pending() {
                    ui.label(
                        RichText::new(ui_text(ids::CONNECT_CHECKING_SERVER)).color(colors.muted),
                    );
                } else if ui
                    .button(ui_text(ids::CONNECT_BUTTON_CHECK_SERVER))
                    .clicked()
                {
                    app.check_current_server();
                }
                if let Some(message) = app.server_check_error.as_deref() {
                    ui.label(RichText::new(message).color(Color32::from_rgb(0xF0, 0xA0, 0x99)));
                }
            }

            if let Some(error) = error {
                ui.add_space(14.0);
                ui.label(RichText::new(error).color(Color32::from_rgb(0xF0, 0xA0, 0x99)));
            }
            ui.add_space(18.0);
            if app.visible_server_preview().is_some()
                && with_access_name(
                    ui.button(ui_text(ids::CONNECT_BUTTON_SUBMIT)),
                    egui::Role::Button,
                    ids::ACCESS_CONNECT_SUBMIT,
                )
                .clicked()
            {
                app.start_connection();
            }
            ui.horizontal(|ui| {
                if quick_connect_active {
                    if ui
                        .button(ui_text(ids::CONNECT_BUTTON_CANCEL_QUICK_CONNECT))
                        .clicked()
                    {
                        app.cancel_quick_connect();
                    }
                } else if app.quick_connect_available()
                    && ui
                        .button(ui_text(ids::CONNECT_BUTTON_USE_QUICK_CONNECT))
                        .clicked()
                {
                    app.start_quick_connect();
                }
                if !quick_connect_active
                    && !app.quick_connect_available()
                    && app.quick_connect_check_pending()
                {
                    ui.label(
                        RichText::new(ui_text(ids::CONNECT_STATUS_CHECKING_QUICK_CONNECT))
                            .color(colors.muted),
                    );
                }
                if let Some(flow) = app.quick_connect.as_ref() {
                    if let Some(code) = pending_code(flow.state()) {
                        ui.label(RichText::new(code).size(22.0).strong().color(colors.accent));
                    }
                    if let Some(status) = status_text(flow.state()) {
                        ui.label(RichText::new(status).color(colors.muted));
                    }
                }
            });
            if quick_connect_probe_requested {
                ui.ctx().request_repaint_after(Duration::from_millis(600));
            }
        });
}

fn live_tv_page(ui: &mut egui::Ui, app: &mut KattriApp, colors: Palette) {
    if !app.live_tv_loading && !app.live_tv_loaded {
        app.request_live_tv_channels(0);
    }
    if app.live_tv_loading && !app.live_tv_loaded {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label(
                RichText::new(ui_text(ids::LIVE_TV_STATUS_LOADING_CHANNELS)).color(colors.muted),
            );
        });
        return;
    }
    if let Some(error) = app.live_tv_error {
        ui.label(
            RichText::new(crate::ui::status::message(error))
                .color(Color32::from_rgb(0xF0, 0xA0, 0x99)),
        );
        if ui
            .button(ui_text(ids::LIVE_TV_BUTTON_RETRY_CHANNELS))
            .clicked()
        {
            app.retry_live_tv_channels();
        }
        return;
    }

    let favorites = app
        .live_tv_channels
        .iter()
        .filter(|channel| {
            channel
                .item
                .user_data
                .as_ref()
                .is_some_and(|data| data.is_favorite == Some(true))
        })
        .map(|channel| channel.item.id.clone())
        .collect::<std::collections::HashSet<_>>();
    let now = crate::app::unix_now_seconds();
    let window = app
        .live_tv_guide_window
        .unwrap_or_else(|| kattri_client_core::epg::guide_window_at(now));
    let refresh_due = app.live_tv_guide_refreshed_at_unix.is_none_or(|refreshed| {
        now.saturating_sub(refreshed) >= kattri_client_core::epg::EPG_REFRESH_INTERVAL_SECONDS
    });
    let window_near_end = kattri_client_core::epg::should_roll_window(window.end_unix, now);
    if !app.live_tv_guide_loading && app.live_tv_guide_available && (refresh_due || window_near_end)
    {
        app.request_live_tv_programs(
            app.live_tv_channels
                .iter()
                .map(|channel| channel.item.id.clone())
                .collect(),
            now,
        );
    }
    let response = live_tv::show(
        ui,
        live_tv::LiveTvInput {
            channels: &app.live_tv_channels,
            channel_logos: &app.live_tv_channel_logos,
            favorite_channel_ids: &favorites,
            endpoint_available: !app.live_tv_not_set_up,
            guide_available: app.live_tv_guide_available,
            programs: &app.live_tv_programs,
            window,
            now_unix: now,
            selected_channel_id: app.live_tv_selected_channel_id.as_deref(),
        },
    );
    if let Some(error) = app.live_tv_guide_error {
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(ui_text_args(
                    ids::LIVE_TV_ERROR_GUIDE_FAILED,
                    &[("detail", crate::ui::status::message(error))],
                ))
                .color(Color32::from_rgb(0xF0, 0xA0, 0x99)),
            );
            if ui
                .button(ui_text(ids::LIVE_TV_BUTTON_RETRY_GUIDE))
                .clicked()
            {
                app.retry_live_tv_guide();
            }
        });
    }
    for action in response.actions {
        match action {
            live_tv::LiveTvAction::SelectChannel(channel_id) => {
                app.live_tv_selected_channel_id = Some(channel_id);
            }
            live_tv::LiveTvAction::TuneChannel(channel_id) => {
                app.tune_live_tv_channel(channel_id);
            }
        }
    }
    if let Some(notice) = app.live_tv_notice.as_deref() {
        ui.label(RichText::new(notice).color(colors.muted));
    }
    if app.live_tv_retry_available
        && !app.playback_start_loading
        && ui
            .button(ui_text(ids::LIVE_TV_BUTTON_RETRY_CHANNEL))
            .clicked()
    {
        app.retry_live_tv_channel();
    }
    if app.live_tv_loading {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label(RichText::new(ui_text(ids::LIVE_TV_STATUS_LOADING_MORE)).color(colors.muted));
        });
    } else if app.live_tv_total_count.is_some_and(|total| {
        usize::try_from(total).is_ok_and(|total| total > app.live_tv_channels.len())
    }) && ui.button(ui_text(ids::LIVE_TV_BUTTON_LOAD_MORE)).clicked()
    {
        let start_index = u32::try_from(app.live_tv_channels.len()).unwrap_or(u32::MAX);
        app.request_live_tv_channels(start_index);
    }
}

/// How often the Downloads page re-reads the job list while it is open. The worker does not
/// push progress, so the page asks for it.
const DOWNLOADS_PAGE_REFRESH: Duration = Duration::from_millis(750);

fn downloads_page(ui: &mut egui::Ui, app: &mut KattriApp, colors: Palette) {
    let due = app
        .downloads
        .last_refresh
        .is_none_or(|last| last.elapsed() >= DOWNLOADS_PAGE_REFRESH);
    if due {
        app.refresh_downloads();
    }
    ui.ctx().request_repaint_after(DOWNLOADS_PAGE_REFRESH);
    let intents = {
        let rows = app
            .downloads
            .rows
            .iter()
            .map(|row| downloads::DownloadRow {
                id: &row.item_id,
                title: app.downloads.title(&row.item_id),
                job: &row.job,
                running: row.running,
                notice: row.notice,
            })
            .collect::<Vec<_>>();
        downloads::show(ui, &rows, colors)
    };
    if let Some(message) = app.downloads.error.as_deref() {
        ui.label(RichText::new(message).color(Color32::from_rgb(0xF0, 0xA0, 0x99)));
    }
    for intent in intents {
        app.download_intent(&intent);
    }
}

fn photos_page(ui: &mut egui::Ui, app: &mut KattriApp) {
    let mut visible_thumbnails = Vec::new();
    let folder_title = app
        .browse_context
        .as_ref()
        .map_or(ui_text(ids::PHOTOS_TITLE_DEFAULT), |context| {
            context.title.as_str()
        });
    let action = photos::draw(
        ui,
        &mut photos::PhotosView {
            folder_title,
            items: &app.photo_items,
            thumbnails: &app.photo_thumbnails,
            full_size_images: &app.photo_full_size_images,
            loading: app.catalog_loading,
            error: app.catalog_error.as_deref(),
            page_start: app.photo_page_start,
            page_size: 100,
            total_count: app.total_record_count.map(|value| value as usize),
            selected_photo_id: app.photo_selected_id.as_deref(),
            full_size_error: app
                .photo_selected_id
                .as_ref()
                .is_some_and(|id| app.photo_full_size_failed.contains(id)),
            visible_thumbnails: &mut visible_thumbnails,
        },
    );
    app.update_photo_thumbnail_window(&visible_thumbnails);
    for (item_id, image_tag) in visible_thumbnails {
        app.request_photo_thumbnail(&item_id, Some(&image_tag));
    }
    match action {
        Some(photos::PhotosAction::RequestPage { start_index, .. }) => {
            app.request_photo_page(start_index);
        }
        Some(photos::PhotosAction::OpenFolder(folder_id)) => {
            if let Some(folder) = app.photo_items.iter().find(|item| item.id == folder_id) {
                app.navigate_to_parent(Page::Photos, folder_id, folder.name.clone(), false);
            }
        }
        Some(photos::PhotosAction::OpenPhoto(item_id))
        | Some(photos::PhotosAction::PreviousPhoto(item_id))
        | Some(photos::PhotosAction::NextPhoto(item_id)) => {
            app.photo_selected_id = Some(item_id.clone());
            app.request_photo_full_size(&item_id);
        }
        Some(photos::PhotosAction::ClosePhoto) => {
            app.photo_selected_id = None;
        }
        None => {}
    }
}

/// Home view: the selected item's details (if any) sit above the hero and rows, so an
/// activation from a row or the hero shows its playback options without leaving Home.
fn home_view(
    ui: &mut egui::Ui,
    app: &mut KattriApp,
    colors: Palette,
    entries: &mut Vec<CardFocus>,
    visible_artwork: &mut Vec<(String, String)>,
) {
    if app.selected_item_id.is_some() {
        details_panel(ui, app, colors);
    }
    let hero_id = app.home.hero_item_id();
    let hero_backdrop = app
        .home_hero_backdrop
        .as_ref()
        .filter(|(id, _)| Some(id) == hero_id.as_ref())
        .map(|(_, texture)| egui::load::SizedTexture::new(texture.id(), texture.size_vec2()));
    if let Some(action) = home::show(
        ui,
        &mut app.home,
        colors,
        entries,
        visible_artwork,
        hero_backdrop,
    ) {
        app.apply_home_action(action);
    }
}

fn connection_progress(ui: &mut egui::Ui, colors: Palette, message: &str) {
    ui.set_max_width(700.0);
    ui.add_space(104.0);
    ui.label(
        RichText::new(ui_text(ids::CONNECT_EYEBROW))
            .size(11.0)
            .strong()
            .color(colors.accent),
    );
    ui.add_space(12.0);
    ui.label(
        RichText::new(message)
            .size(24.0)
            .strong()
            .color(colors.text),
    );
    ui.add_space(8.0);
    ui.label(
        RichText::new(ui_text(ids::CONNECT_NOTE_BACKGROUND))
            .size(14.0)
            .color(colors.muted),
    );
}

fn render_connected(
    ui: &mut egui::Ui,
    app: &mut KattriApp,
    colors: Palette,
    entries: &mut Vec<CardFocus>,
    search_show_all_targets: &mut Vec<(FocusId, search::SearchGroup)>,
    visible_artwork: &mut Vec<(String, String)>,
    gamepad_actions: &mut Vec<NavAction>,
) {
    if app.view == AppView::Home {
        home_view(ui, app, colors, entries, visible_artwork);
        return;
    }
    if app.page == Page::Settings {
        settings::show(ui, app, colors, gamepad_actions);
        return;
    }
    if app.page == Page::Downloads {
        downloads_page(ui, app, colors);
        return;
    }
    if app.page == Page::LiveTv {
        live_tv_page(ui, app, colors);
        return;
    }
    if app.page == Page::Dashboard {
        let view = admin::dashboard_view(app.dashboard_enabled(), &app.admin_load);
        admin::show(ui, &view, colors);
        return;
    }
    if app.page == Page::SyncPlay {
        syncplay::show(ui, app, colors);
        return;
    }
    if app.page == Page::Photos {
        photos_page(ui, app);
        return;
    }
    if let Some(message) = app.catalog_error.clone() {
        error_banner(ui, app, colors, &message);
    }
    if let Some(message) = app.catalog_filter_notice.take() {
        ui.label(RichText::new(message).color(colors.muted));
    }
    if app.catalog_loading {
        ui.horizontal(|ui| {
            ui.spinner();
            let message = if app.search_active || app.submitted_search.is_some() {
                ui_text(ids::BROWSE_STATUS_SEARCHING)
            } else {
                ui_text(ids::BROWSE_STATUS_LOADING_PAGE)
            };
            ui.label(RichText::new(message).color(colors.muted));
        });
        ui.add_space(10.0);
    }

    if app.page == Page::Playlists {
        playlists_page(ui, app);
        return;
    }

    if let Some(group) = app.search_browse_group {
        if ui
            .button(ui_text(ids::SEARCH_BUTTON_BACK_TO_RESULTS))
            .clicked()
        {
            app.return_to_search_results();
            return;
        }
        details_panel(ui, app, colors);
        let term = app.submitted_search.as_deref().unwrap_or_default();
        ui.heading(ui_text_args(
            ids::SEARCH_HEADING_MATCHING,
            &[("group", group.label()), ("term", term)],
        ));
        ui.add_space(8.0);
        if app.catalog_loading || app.live_tv_search_loading {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(ui_text(ids::SEARCH_STATUS_LOADING_ALL));
            });
        }
        if let Some(error) = app.catalog_error.as_deref() {
            ui.label(RichText::new(error).color(colors.muted));
        }
        if group == search::SearchGroup::Channels {
            let mut selected = None;
            for (row, item) in app.live_tv_search_results.iter().enumerate() {
                let response = ui.button(&item.name);
                push_search_focus(entries, row, &item.id, response.clone());
                if response.clicked() {
                    selected = Some(item.id.clone());
                }
            }
            if let Some(item_id) = selected {
                app.select_item(item_id);
            }
            let loaded = app.live_tv_search_results.len() as u32;
            let total = app.live_tv_search_total;
            let can_next = !app.live_tv_search_loading && total.is_some_and(|total| loaded < total);
            ui.horizontal(|ui| {
                ui.label(catalog_count_label(loaded as usize, total));
                if with_access_name(
                    ui.add_enabled(
                        can_next,
                        egui::Button::new(ui_text(ids::BROWSE_BUTTON_NEXT_PAGE)),
                    ),
                    egui::Role::Button,
                    ids::ACCESS_BROWSE_NEXT_PAGE,
                )
                .clicked()
                {
                    app.next_live_tv_search_page();
                }
            });
        } else {
            let mut selected = None;
            for (row, item) in app.items.iter().enumerate() {
                let response = ui.button(&item.title);
                push_search_focus(entries, row, &item.id, response.clone());
                if response.clicked() {
                    selected = Some(item.id.clone());
                }
            }
            if let Some(item_id) = selected {
                app.select_item(item_id);
            }
            catalog_pagination(ui, app, colors);
        }
        if app.items.is_empty()
            && app.live_tv_search_results.is_empty()
            && !app.catalog_loading
            && !app.live_tv_search_loading
            && app.catalog_error.is_none()
        {
            ui.label(ui_text(ids::SEARCH_EMPTY_NO_MATCHES));
        }
        return;
    }

    if app.search_active || app.submitted_search.is_some() {
        details_panel(ui, app, colors);
        if app.search_stale {
            ui.label(RichText::new(ui_text(ids::SEARCH_NOTICE_STALE)).color(colors.muted));
        }
        let query = app.submitted_search.clone();
        let mut grouped_results = app.search_results.clone();
        grouped_results.extend(app.live_tv_search_results.iter().cloned());
        if grouped_results.is_empty() {
            if let Some(query) = query {
                if !app.catalog_loading
                    && !app.live_tv_search_loading
                    && app.catalog_error.is_none()
                {
                    state_panel(
                        ui,
                        colors,
                        ui_text(ids::SEARCH_EMPTY_TITLE),
                        &ui_text_args(ids::SEARCH_EMPTY_HINT, &[("query", query.as_str())]),
                        Some((ui_text(ids::SEARCH_BUTTON_CLEAR), StateAction::ClearSearch)),
                        app,
                    );
                }
            } else if app.search_query.trim().chars().count() < 2 {
                state_panel(
                    ui,
                    colors,
                    ui_text(ids::BROWSE_SEARCH_TOO_SHORT_TITLE),
                    ui_text(ids::BROWSE_SEARCH_TOO_SHORT_HINT),
                    None,
                    app,
                );
            } else if !app.catalog_loading {
                ui.label(RichText::new(ui_text(ids::SEARCH_STATUS_WAITING)).color(colors.muted));
            }
        } else {
            let mut show_all_responses = Vec::new();
            let action = search::draw_with_focus_targets(
                ui,
                &grouped_results,
                app.live_tv_search_total,
                |row, item, response| {
                    push_search_focus(entries, row, &item.id, response);
                },
                |row, group, response| {
                    show_all_responses.push((row, group, response));
                },
            );
            for (row, group, response) in show_all_responses {
                push_search_show_all_focus(entries, search_show_all_targets, row, group, response);
            }
            if let Some(action) = action {
                match action {
                    search::SearchAction::SelectItem(item_id) => app.select_item(item_id),
                    search::SearchAction::ShowAll(group) => app.show_all_search_group(group),
                }
            }
        }
        if app.live_tv_search_loading {
            ui.label(RichText::new(ui_text(ids::LIVE_TV_STATUS_SEARCHING)).color(colors.muted));
        }
        return;
    }

    if app.page == Page::Music {
        music_queue_panel(ui, app, gamepad_actions);
    }
    // Sorting and paging belong to the list; an open item or person page replaces them.
    if app.selected_item_id.is_none() && app.selected_person.is_none() {
        catalog_controls(ui, app, colors);
        catalog_pagination(ui, app, colors);
    }
    details_panel(ui, app, colors);

    if let Some(context) = app.browse_context.as_ref() {
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(if context.is_collection {
                    ui_text(ids::BROWSE_KICKER_COLLECTION)
                } else {
                    ui_text(ids::BROWSE_KICKER_LIBRARY)
                })
                .size(11.0)
                .strong()
                .color(colors.accent),
            );
            ui.label(
                RichText::new(&context.title)
                    .size(20.0)
                    .strong()
                    .color(colors.text),
            );
        });
        if context.is_collection {
            ui.label(
                RichText::new(collection_summary_text(
                    app.total_record_count,
                    app.items.len(),
                    app.collection_runtime,
                ))
                .size(13.0)
                .color(colors.muted),
            );
        }
        ui.add_space(8.0);
    }

    if app.items.is_empty() && app.catalog_loading {
        return;
    }

    if app.page == Page::Home {
        home_header(ui, app, colors);
        if app.items.is_empty() {
            if app.catalog_error.is_some() {
                return;
            }
            if let Some((title, message)) = empty_collection_detail(app.browse_context.as_ref()) {
                state_panel(ui, colors, title, message, None, app);
                return;
            }
            if app.has_catalog_filters() {
                state_panel(
                    ui,
                    colors,
                    ui_text(ids::BROWSE_EMPTY_FILTERED_TITLE),
                    ui_text(ids::BROWSE_EMPTY_FILTERED_HINT),
                    Some((
                        ui_text(ids::BROWSE_BUTTON_CLEAR_FILTERS),
                        StateAction::ClearCatalogFilters,
                    )),
                    app,
                );
                return;
            }
            let message = if app.total_record_count == Some(0) {
                ui_text(ids::BROWSE_EMPTY_SERVER_NONE)
            } else {
                ui_text(ids::BROWSE_EMPTY_LOADED_HINT)
            };
            state_panel(
                ui,
                colors,
                ui_text(ids::BROWSE_EMPTY_LOADED_TITLE),
                message,
                Some((
                    ui_text(ids::BROWSE_BUTTON_RETRY_REQUEST),
                    StateAction::RetryItems,
                )),
                app,
            );
            return;
        }
        let groups = [
            (
                ui_text(ids::CATEGORY_MOVIES_TITLE),
                ui_text(ids::CATEGORY_MOVIES_DESCRIPTION),
                MediaKind::Movie,
            ),
            (
                ui_text(ids::CATEGORY_SERIES_TITLE),
                ui_text(ids::CATEGORY_SERIES_DESCRIPTION),
                MediaKind::Series,
            ),
            (
                ui_text(ids::CATEGORY_MUSIC_TITLE),
                ui_text(ids::CATEGORY_MUSIC_DESCRIPTION),
                MediaKind::Audio,
            ),
            (
                ui_text(ids::CATEGORY_PLAYLISTS_TITLE),
                ui_text(ids::CATEGORY_PLAYLISTS_DESCRIPTION),
                MediaKind::Playlist,
            ),
        ];
        let mut row = 0;
        for (title, description, kind) in groups {
            let mut items = app
                .items
                .iter()
                .filter(|item| match kind {
                    MediaKind::Series => {
                        matches!(item.kind, MediaKind::Series | MediaKind::Episode)
                    }
                    other => item.kind == other,
                })
                .peekable();
            if items.peek().is_some() {
                ui.add_space(26.0);
                let selected_item_id = app.selected_item_id.clone();
                if let Some(item_id) = shelf(
                    ui,
                    ShelfContent {
                        title,
                        description,
                        items,
                        row,
                    },
                    colors,
                    entries,
                    selected_item_id.as_deref(),
                    visible_artwork,
                ) {
                    app.activate_catalog_item(item_id);
                }
                row += 1;
            }
        }
        let mut other_items = app
            .items
            .iter()
            .filter(|item| matches!(item.kind, MediaKind::Folder | MediaKind::Other))
            .peekable();
        if other_items.peek().is_some() {
            ui.add_space(26.0);
            let selected_item_id = app.selected_item_id.clone();
            if let Some(item_id) = shelf(
                ui,
                ShelfContent {
                    title: ui_text(ids::CATEGORY_OTHER_TITLE),
                    description: ui_text(ids::CATEGORY_OTHER_DESCRIPTION),
                    items: other_items,
                    row,
                },
                colors,
                entries,
                selected_item_id.as_deref(),
                visible_artwork,
            ) {
                app.activate_catalog_item(item_id);
            }
        }
        return;
    }

    let matches_item: fn(MediaKind) -> bool = match app.page {
        Page::Movies => |kind| kind == MediaKind::Movie,
        Page::Series => |kind| kind == MediaKind::Series,
        Page::Music => |kind| kind == MediaKind::Audio,
        Page::Photos => |kind| kind == MediaKind::Photo,
        Page::Playlists => |kind| kind == MediaKind::Playlist,
        Page::Home
        | Page::Downloads
        | Page::LiveTv
        | Page::Dashboard
        | Page::SyncPlay
        | Page::Settings => {
            unreachable!()
        }
    };
    let mut items = app
        .items
        .iter()
        .filter(|item| matches_item(item.kind))
        .peekable();
    if items.peek().is_none() {
        if app.has_catalog_filters() {
            state_panel(
                ui,
                colors,
                ui_text(ids::BROWSE_EMPTY_FILTERED_TITLE),
                ui_text(ids::BROWSE_EMPTY_FILTERED_HINT),
                Some((
                    ui_text(ids::BROWSE_BUTTON_CLEAR_FILTERS),
                    StateAction::ClearCatalogFilters,
                )),
                app,
            );
            return;
        }
        state_panel(
            ui,
            colors,
            &ui_text_args(
                ids::BROWSE_EMPTY_TYPE_LOADED_TITLE,
                &[("kind", &app.page.label().to_lowercase())],
            ),
            ui_text(ids::BROWSE_EMPTY_TYPE_SERVER_HINT),
            Some((ui_text(ids::BROWSE_BUTTON_SHOW_ALL), StateAction::OpenHome)),
            app,
        );
        return;
    }
    let items: Vec<&MediaItem> = items.collect();
    let selected_item_id = app.selected_item_id.clone();
    if let Some(item_id) = poster_grid(
        ui,
        app.page.label(),
        &items,
        colors,
        entries,
        selected_item_id.as_deref(),
        visible_artwork,
    ) {
        app.select_item(item_id);
    }
}

fn music_queue_panel(ui: &mut egui::Ui, app: &mut KattriApp, navigation: &[NavAction]) {
    if app.music_queue_restore_loading {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label(ui_text(ids::MUSIC_QUEUE_REFRESHING));
        });
    }
    if app.music_queue_resume_position_ms.is_some() && !app.music_queue.is_empty() {
        let resume = ui.button(ui_text(ids::MUSIC_QUEUE_RESUME));
        resume.widget_info(|| {
            egui::WidgetInfo::labeled(
                egui::Role::Button,
                true,
                ui_text(ids::MUSIC_QUEUE_RESUME_TITLE).to_owned(),
            )
        });
        if resume.clicked() {
            app.resume_saved_music_queue();
        }
    }
    let response = music::show(
        ui,
        music::MusicQueueInput {
            queue: &app.music_queue,
            ordered_tracks: app.music_queue.items(),
            track_names: &app.music_track_names,
            navigation,
        },
    );
    for action in response.actions {
        app.apply_music_action(action);
    }
    if let Some(notice) = app.music_queue_notice.as_deref() {
        ui.label(notice);
    }
    ui.add_space(12.0);
}

fn playlists_page(ui: &mut egui::Ui, app: &mut KattriApp) {
    use playlists::{
        AddableMedia, OpenPlaylist, PlaylistCard, PlaylistEntry as UiPlaylistEntry, PlaylistInput,
    };

    let cards = app
        .items
        .iter()
        .filter(|item| item.kind == MediaKind::Playlist)
        .map(|item| PlaylistCard {
            playlist_id: item.id.clone(),
            name: item.title.clone(),
            entry_count: (app.opened_playlist_id.as_deref() == Some(item.id.as_str()))
                .then_some(app.playlist_entries.len()),
        })
        .collect::<Vec<_>>();
    let entries = app
        .playlist_entries
        .iter()
        .map(|entry| UiPlaylistEntry {
            playlist_entry_id: entry.playlist_item_id.clone().unwrap_or_default(),
            media_id: entry.item.id.clone(),
            title: entry.item.name.clone(),
        })
        .collect::<Vec<_>>();
    let addable_media = app
        .playlist_candidates
        .iter()
        .filter(|item| {
            matches!(
                item.item_type.as_deref(),
                Some("Movie" | "Episode" | "Audio")
            )
        })
        .map(|item| AddableMedia {
            media_id: item.id.clone(),
            title: item.name.clone(),
        })
        .collect::<Vec<_>>();
    let opened_name = app.opened_playlist_id.as_deref().and_then(|playlist_id| {
        app.items
            .iter()
            .find(|item| item.id == playlist_id)
            .map(|item| item.title.clone())
            .or_else(|| {
                app.selected_details
                    .as_ref()
                    .filter(|item| item.id == playlist_id)
                    .map(|item| item.name.clone())
            })
    });
    let opened = app
        .opened_playlist_id
        .as_deref()
        .zip(opened_name.as_deref())
        .map(|(playlist_id, name)| OpenPlaylist {
            playlist_id,
            name,
            entries: &entries,
        });
    let error = app
        .playlist_mutation_error
        .or(app.playlist_items_error)
        .or(app.playlist_candidates_error)
        .map(|error| match error {
            jellyfin_api::ErrorClass::NotFound => ui_text(ids::PLAYLISTS_ERROR_NOT_FOUND),
            jellyfin_api::ErrorClass::Forbidden => ui_text(ids::PLAYLISTS_ERROR_FORBIDDEN),
            other => status::message(other),
        });
    if app.playlist_items_loading || app.playlist_candidates_loading {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label(ui_text(ids::PLAYLISTS_STATUS_LOADING));
        });
    }
    if app.playlist_mutation_loading {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label(ui_text(ids::PLAYLISTS_STATUS_SAVING));
        });
    }
    if let Some(notice) = app.playlist_mutation_notice.as_deref() {
        ui.label(notice);
    }
    let response = playlists::show(
        ui,
        PlaylistInput {
            playlists: &cards,
            opened_playlist: opened,
            addable_media: &addable_media,
            error,
        },
        &mut app.playlist_ui_state,
    );
    for intent in response.intents {
        app.apply_playlist_intent(intent);
    }
}

fn empty_collection_detail(
    context: Option<&BrowseContext>,
) -> Option<(&'static str, &'static str)> {
    context.filter(|context| context.is_collection).map(|_| {
        (
            ui_text(ids::BROWSE_COLLECTION_EMPTY_TITLE),
            ui_text(ids::BROWSE_COLLECTION_EMPTY_HINT),
        )
    })
}

fn collection_summary_text(
    total_item_count: Option<u32>,
    loaded_count: usize,
    runtime: jellyfin_api::CollectionRuntime,
) -> String {
    let item_count =
        total_item_count.unwrap_or_else(|| u32::try_from(loaded_count).unwrap_or(u32::MAX));
    let item_label = if item_count == 1 {
        ui_text(ids::SHELL_COUNT_ITEM)
    } else {
        ui_text(ids::SHELL_COUNT_ITEMS)
    };
    let mut summary = if total_item_count.is_some() {
        format!("{item_count} {item_label}")
    } else {
        ui_text_args(
            ids::BROWSE_COUNT_LOADED_ITEMS,
            &[("count", &item_count.to_string()), ("label", item_label)],
        )
    };
    let fully_loaded = total_item_count
        .is_some_and(|count| usize::try_from(count).is_ok_and(|count| loaded_count >= count));
    if runtime.items_with_runtime > 0 {
        let total_seconds = runtime.total_seconds().max(0);
        let hours = total_seconds / 3600;
        let minutes = (total_seconds % 3600) / 60;
        let runtime_label = if fully_loaded {
            ui_text(ids::BROWSE_SUMMARY_RUNTIME)
        } else {
            ui_text(ids::BROWSE_SUMMARY_RUNTIME_HINT)
        };
        summary.push_str(" · ");
        summary.push_str(&ui_text_args(
            ids::BROWSE_SUMMARY_RUNTIME_VALUE,
            &[
                ("runtime_label", runtime_label),
                ("hours", &hours.to_string()),
                ("minutes", &minutes.to_string()),
            ],
        ));
    } else if item_count > 0 && fully_loaded {
        summary.push_str(" · ");
        summary.push_str(ui_text(ids::BROWSE_SUMMARY_RUNTIME_UNAVAILABLE));
    }
    if runtime.has_unknown_runtime() {
        summary.push_str(" · ");
        summary.push_str(ui_text(ids::BROWSE_SUMMARY_SOME_RUNTIMES_UNAVAILABLE));
    }
    if total_item_count.is_some() && !fully_loaded {
        summary.push_str(" · ");
        summary.push_str(&ui_text_args(
            ids::BROWSE_COUNT_LOADED,
            &[("count", &loaded_count.to_string())],
        ));
    }
    summary
}

fn catalog_pagination(ui: &mut egui::Ui, app: &mut KattriApp, colors: Palette) {
    // Everything is loaded: the page title already shows the count.
    if app.catalog_end_reached && !app.catalog_loading {
        return;
    }
    let range_label = catalog_count_label(app.items.len(), app.total_record_count);
    ui.horizontal(|ui| {
        ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
            // Once the server has sent every item there is no next page to offer.
            if !app.catalog_end_reached
                && with_access_name(
                    ui.add_enabled(
                        !app.catalog_loading,
                        egui::Button::new(ui_text(ids::BROWSE_BUTTON_NEXT_PAGE)),
                    ),
                    egui::Role::Button,
                    ids::ACCESS_BROWSE_NEXT_PAGE,
                )
                .clicked()
            {
                app.next_page();
            }
            ui.label(RichText::new(range_label).size(12.0).color(colors.muted));
        });
    });
    ui.add_space(16.0);
}

fn catalog_count_label(loaded: usize, total: Option<u32>) -> String {
    total.map_or_else(
        || ui_text_args(ids::BROWSE_COUNT_LOADED, &[("count", &loaded.to_string())]),
        |total| {
            ui_text_args(
                ids::BROWSE_COUNT_OF_TOTAL,
                &[
                    ("loaded", &loaded.to_string()),
                    ("total", &total.to_string()),
                ],
            )
        },
    )
}

#[cfg(test)]
mod catalog_label_tests {
    use super::catalog_count_label;

    #[test]
    fn count_label_reports_total_only_when_available() {
        assert_eq!(catalog_count_label(37, Some(120)), "37 of 120");
        assert_eq!(catalog_count_label(37, None), "37 loaded");
        assert_eq!(catalog_count_label(0, Some(0)), "0 of 0");
    }
}

fn catalog_controls(ui: &mut egui::Ui, app: &mut KattriApp, colors: Palette) {
    let mut sort = app.catalog_sort;
    let mut order = app.catalog_sort_order;
    let mut played_filter = app.catalog_played_filter;
    let mut favorites_only = app.catalog_favorites_only;
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new(ui_text(ids::BROWSE_SORT_LABEL)).color(colors.muted));
        egui::ComboBox::from_id_salt("catalog_sort")
            .selected_text(sort.label())
            .show_ui(ui, |ui| {
                for option in CatalogSort::ALL {
                    ui.selectable_value(&mut sort, option, option.label());
                }
            })
            .response
            .widget_info(|| {
                egui::WidgetInfo::labeled(
                    egui::Role::ComboBox,
                    true,
                    accessible_name(
                        ids::ACCESS_BROWSE_SORT_BY,
                        ui_text(ids::ACCESS_BROWSE_SORT_BY),
                    ),
                )
            });
        egui::ComboBox::from_id_salt("catalog_sort_order")
            .selected_text(match order {
                ItemSortOrder::Ascending => ui_text(ids::BROWSE_SORT_ASCENDING),
                ItemSortOrder::Descending => ui_text(ids::BROWSE_SORT_DESCENDING),
            })
            .show_ui(ui, |ui| {
                ui.selectable_value(
                    &mut order,
                    ItemSortOrder::Ascending,
                    ui_text(ids::BROWSE_SORT_ASCENDING),
                );
                ui.selectable_value(
                    &mut order,
                    ItemSortOrder::Descending,
                    ui_text(ids::BROWSE_SORT_DESCENDING),
                );
            })
            .response
            .widget_info(|| {
                egui::WidgetInfo::labeled(
                    egui::Role::ComboBox,
                    true,
                    accessible_name(ids::ACCESS_BROWSE_SORT, ui_text(ids::ACCESS_BROWSE_SORT)),
                )
            });
        egui::ComboBox::from_id_salt("catalog_played_filter")
            .selected_text(played_filter.label())
            .show_ui(ui, |ui| {
                for option in CatalogPlayedFilter::ALL
                    .into_iter()
                    .filter(|option| match option {
                        CatalogPlayedFilter::Played => {
                            app.catalog_filter_supported(ItemFilter::IsPlayed)
                        }
                        CatalogPlayedFilter::Unplayed => {
                            app.catalog_filter_supported(ItemFilter::IsUnplayed)
                        }
                        CatalogPlayedFilter::Any => true,
                    })
                {
                    ui.selectable_value(&mut played_filter, option, option.label());
                }
            })
            .response
            .widget_info(|| {
                egui::WidgetInfo::labeled(
                    egui::Role::ComboBox,
                    true,
                    accessible_name(
                        ids::ACCESS_BROWSE_PLAYED_FILTER,
                        ui_text(ids::ACCESS_BROWSE_PLAYED_FILTER),
                    ),
                )
            });
        if app.catalog_filter_supported(ItemFilter::IsFavorite) {
            ui.checkbox(
                &mut favorites_only,
                ui_text(ids::BROWSE_FILTER_FAVORITES_ONLY),
            );
        }
    });
    let mut genre = app.catalog_genre.clone();
    if !app.catalog_genres.is_empty() {
        ui.horizontal(|ui| {
            ui.label(RichText::new(ui_text(ids::BROWSE_GENRE_LABEL)).color(colors.muted));
            egui::ComboBox::from_id_salt("catalog_genre")
                .selected_text(
                    genre
                        .clone()
                        .unwrap_or_else(|| ui_text(ids::BROWSE_GENRE_ALL).to_owned()),
                )
                .height(360.0)
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut genre, None, ui_text(ids::BROWSE_GENRE_ALL));
                    for name in &app.catalog_genres {
                        ui.selectable_value(&mut genre, Some(name.clone()), name);
                    }
                });
        });
    }
    let mut name_prefix = app.catalog_name_prefix;
    alphabet_picker(ui, &mut name_prefix, colors);
    app.set_catalog_genre(genre);
    app.set_catalog_sort(sort);
    app.set_catalog_sort_order(order);
    app.set_catalog_played_filter(played_filter);
    app.set_catalog_favorites_only(favorites_only);
    app.set_catalog_name_prefix(name_prefix);
}

/// The letters the alphabet picker offers after "All": `#` for digits and symbols, then A-Z.
const ALPHABET_PICKER_LETTERS: [char; 27] = [
    '#', 'A', 'B', 'C', 'D', 'E', 'F', 'G', 'H', 'I', 'J', 'K', 'L', 'M', 'N', 'O', 'P', 'Q', 'R',
    'S', 'T', 'U', 'V', 'W', 'X', 'Y', 'Z',
];

/// A row of letters that narrows the library to names starting with one of them.
fn alphabet_picker(ui: &mut egui::Ui, prefix: &mut Option<char>, colors: Palette) {
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 2.0;
        let all = ui.add(egui::Button::selectable(
            prefix.is_none(),
            RichText::new(ui_text(ids::BROWSE_ALPHABET_ALL)).color(colors.text),
        ));
        if all.clicked() {
            *prefix = None;
        }
        for letter in ALPHABET_PICKER_LETTERS {
            let selected = *prefix == Some(letter);
            let response = ui
                .add(
                    egui::Button::selectable(
                        selected,
                        RichText::new(letter.to_string()).monospace(),
                    )
                    .min_size(egui::vec2(22.0, 22.0)),
                )
                .on_hover_text(if letter == '#' {
                    ui_text(ids::BROWSE_ALPHABET_SYMBOLS).to_owned()
                } else {
                    ui_text_args(
                        ids::BROWSE_ALPHABET_LETTER,
                        &[("letter", &letter.to_string())],
                    )
                });
            if response.clicked() {
                *prefix = if selected { None } else { Some(letter) };
            }
        }
    });
}

/// Only a movie or an episode is one file the server can return from `/Items/{id}/Download`.
/// Series, seasons, folders, and collections have no single file, so they get no button.
fn is_downloadable_item_type(item_type: Option<&str>) -> bool {
    matches!(item_type, Some("Movie" | "Episode"))
}

#[cfg(test)]
mod download_button_tests {
    use super::is_downloadable_item_type;

    #[test]
    fn only_movies_and_episodes_get_a_download_button() {
        assert!(is_downloadable_item_type(Some("Movie")));
        assert!(is_downloadable_item_type(Some("Episode")));
        for item_type in ["Series", "Season", "Folder", "BoxSet", "Audio", "Playlist"] {
            assert!(!is_downloadable_item_type(Some(item_type)), "{item_type}");
        }
        assert!(!is_downloadable_item_type(None));
    }
}

fn details_panel(ui: &mut egui::Ui, app: &mut KattriApp, colors: Palette) {
    if app.selected_person.is_some() {
        person_page_panel(ui, app, colors);
        return;
    }
    if app.selected_item_id.is_none() {
        return;
    }

    // Some(start_over) when the viewer pressed Play, Resume or Start over.
    let mut play_request: Option<bool> = None;
    let mut detail_back = false;
    let mut watched_clicked = false;
    let mut favorite_clicked = false;
    let mut queue_audio_clicked = false;
    let mut play_next_clicked = false;
    let mut queue_album_clicked = false;
    let mut detail_error_action = None;
    let mut season_to_load = None;
    let mut episode_to_open = None;
    let mut episode_page_to_open = None;
    let mut person_to_open = None;
    let mut similar_item_to_open = None;
    let mut download_target: Option<(String, String)> = None;
    egui::Frame::new()
        .fill(colors.panel)
        .corner_radius(CornerRadius::same(16))
        .inner_margin(egui::Margin::same(16))
        .show(ui, |ui| {
            if app.details_loading {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(
                        RichText::new(ui_text(ids::DETAIL_STATUS_LOADING)).color(colors.muted),
                    );
                });
            }

            if let Some(item) = app.selected_details.as_ref() {
                let detail_panel_id = egui::Id::unique(("item_detail_panel", &item.id));
                DETAIL_PANEL_STATE.with_borrow_mut(|state| {
                    if state.as_ref().is_none_or(|(id, _)| *id != detail_panel_id) {
                        *state = Some((detail_panel_id, detail::DetailPanel::default()));
                    }
                    if let Some((_, panel)) = state.as_mut() {
                        let header =
                            panel.show(ui, item, app.selected_artwork_texture.as_ref(), |ui| {
                                detail_play_buttons(
                                    ui,
                                    app,
                                    item,
                                    colors,
                                    &mut play_request,
                                    &mut download_target,
                                );
                            });
                        detail_back |= header.back;
                    }
                });
                if item.item_type.as_deref() == Some("Audio") {
                    ui.horizontal(|ui| {
                        if ui.button(ui_text(ids::MUSIC_QUEUE_ADD)).clicked() {
                            queue_audio_clicked = true;
                        }
                        if ui.button(ui_text(ids::MUSIC_QUEUE_PLAY_NEXT)).clicked() {
                            play_next_clicked = true;
                        }
                    });
                } else if item.item_type.as_deref() == Some("MusicAlbum") {
                    ui.horizontal(|ui| {
                        if ui
                            .add_enabled(
                                !app.music_album_tracks_loading,
                                egui::Button::new(ui_text(ids::MUSIC_ALBUM_PLAY_FIRST)),
                            )
                            .clicked()
                        {
                            queue_album_clicked = true;
                        }
                        if app.music_album_tracks_loading {
                            ui.spinner();
                            ui.label(
                                RichText::new(ui_text(ids::MUSIC_ALBUM_LOADING_TRACKS))
                                    .color(colors.muted),
                            );
                        }
                    });
                    if let Some(error) = app.music_album_tracks_error {
                        ui.label(
                            RichText::new(ui_text_args(
                                ids::MUSIC_ALBUM_LOAD_FAILED,
                                &[("error", crate::ui::status::message(error))],
                            ))
                            .color(Color32::from_rgb(0xF0, 0xA0, 0x99)),
                        );
                    }
                }
                if item.item_type.as_deref() == Some("Series") {
                    if let Some(episode) = app.series_next_up_episode(&item.id) {
                        ui.add_space(10.0);
                        if series_next_up_button(ui, episode, colors).clicked() {
                            episode_to_open = Some(episode.id.clone());
                        }
                    }
                    ui.add_space(14.0);
                    ui.heading(ui_text(ids::SERIES_HEADING_SEASONS));
                    if app.series_seasons_loading {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(
                                RichText::new(ui_text(ids::SERIES_STATUS_LOADING_SEASONS))
                                    .color(colors.muted),
                            );
                        });
                    } else if let Some(error) = app.series_seasons_error.as_deref() {
                        ui.label(
                            RichText::new(ui_text_args(
                                ids::SERIES_ERROR_LOAD_SEASONS,
                                &[("error", error)],
                            ))
                            .color(Color32::from_rgb(0xF0, 0xA0, 0x99)),
                        );
                    } else if app.series_seasons.is_empty() {
                        ui.label(
                            RichText::new(ui_text(ids::SERIES_EMPTY_SEASONS)).color(colors.muted),
                        );
                    } else {
                        let series_id = item.id.clone();
                        let series_view_id = egui::Id::unique(("series_presentation", series_id));
                        SERIES_PRESENTATION_STATE.with_borrow_mut(|state| {
                            if state.as_ref().is_none_or(|(id, _)| *id != series_view_id) {
                                *state =
                                    Some((series_view_id, series::SeriesPresentation::default()));
                            }
                            if let Some((_, presentation)) = state.as_mut() {
                                let response = presentation.show(
                                    ui,
                                    &app.series_seasons,
                                    &app.series_episodes,
                                );
                                if response.selected_season_id.as_deref()
                                    != app.series_selected_season_id.as_deref()
                                {
                                    season_to_load = response.selected_season_id.clone();
                                }
                                episode_to_open = response.activated_episode_id.clone();
                                episode_page_to_open = response.opened_episode_id.clone();
                            }
                        });
                        if app.series_episodes_loading {
                            ui.horizontal(|ui| {
                                ui.spinner();
                                ui.label(
                                    RichText::new(ui_text(ids::SERIES_STATUS_LOADING_EPISODES))
                                        .color(colors.muted),
                                );
                            });
                        } else if let Some(error) = app.series_episodes_error.as_deref() {
                            ui.label(
                                RichText::new(ui_text_args(
                                    ids::SERIES_ERROR_LOAD_SEASON,
                                    &[("error", error)],
                                ))
                                .color(Color32::from_rgb(0xF0, 0xA0, 0x99)),
                            );
                        }
                    }
                }
                let cast = item
                    .people
                    .as_deref()
                    .unwrap_or_default()
                    .iter()
                    .take(20)
                    .filter_map(|person| {
                        let name = person
                            .name
                            .as_deref()
                            .map(str::trim)
                            .filter(|name| !name.is_empty())?;
                        Some(people::CastPerson {
                            id: person.id.clone(),
                            name: name.to_owned(),
                            role: person
                                .role
                                .as_deref()
                                .map(str::trim)
                                .filter(|role| !role.is_empty())
                                .map(str::to_owned),
                            primary_image_tag: person.primary_image_tag.clone(),
                        })
                    })
                    .collect::<Vec<_>>();
                let people_action = people::draw(
                    ui,
                    &cast,
                    &app.similar_items,
                    &item.id,
                    &app.person_artwork_textures,
                );
                person_to_open = people_action.person_id;
                similar_item_to_open = people_action.item_id;
                if let Some(user_data) = item.user_data.as_ref() {
                    let mut state = Vec::new();
                    if user_data.is_favorite == Some(true) {
                        state.push(ui_text(ids::DETAIL_STATE_FAVORITE).to_owned());
                    }
                    if user_data.played == Some(true) {
                        state.push(ui_text(ids::DETAIL_STATE_PLAYED).to_owned());
                    }
                    if let Some(count) = user_data.play_count.filter(|count| *count > 0) {
                        state.push(ui_text_args(
                            ids::DETAIL_STATE_PLAYED_COUNT,
                            &[("count", &count.to_string())],
                        ));
                    }
                    if !state.is_empty() {
                        ui.add_space(6.0);
                        ui.label(
                            RichText::new(state.join("  ·  "))
                                .size(11.0)
                                .color(colors.muted),
                        );
                    }
                }
                let watched = app.watched_view();
                let favorite = app.favorite_view();
                ui.horizontal(|ui| {
                    let label = if favorite.favorite {
                        ui_text(ids::DETAIL_BUTTON_REMOVE_FAVORITE)
                    } else {
                        ui_text(ids::DETAIL_BUTTON_ADD_FAVORITE)
                    };
                    if ui.button(label).clicked() {
                        favorite_clicked = true;
                    }
                    if favorite.pending {
                        ui.spinner();
                    }
                    let label = if watched.played {
                        ui_text(ids::DETAIL_BUTTON_MARK_UNPLAYED)
                    } else {
                        ui_text(ids::DETAIL_BUTTON_MARK_PLAYED)
                    };
                    if ui
                        .add_enabled(!watched.pending, egui::Button::new(label))
                        .clicked()
                    {
                        watched_clicked = true;
                    }
                    if watched.pending {
                        ui.spinner();
                    }
                });
                if let Some(message) = watched.error.as_deref() {
                    ui.label(
                        RichText::new(message)
                            .size(12.0)
                            .color(Color32::from_rgb(0xF0, 0xA0, 0x99)),
                    );
                }
                if let Some(message) = favorite.error.as_deref() {
                    ui.label(
                        RichText::new(message)
                            .size(12.0)
                            .color(Color32::from_rgb(0xF0, 0xA0, 0x99)),
                    );
                }
                if let Some(message) = app.playback_error.as_deref() {
                    ui.label(
                        RichText::new(ui_text_args(
                            ids::PLAYBACK_ERROR_NEGOTIATION,
                            &[("message", message)],
                        ))
                        .size(12.0)
                        .color(Color32::from_rgb(0xF0, 0xA0, 0x99)),
                    );
                }
                if let Some(info) = app.playback_info.as_ref() {
                    if let Some(error_code) = info.error_code {
                        let message = match error_code {
                            jellyfin_api::PlaybackErrorCode::NotAllowed => {
                                ui_text(ids::PLAYBACK_ERROR_NOT_ALLOWED)
                            }
                            jellyfin_api::PlaybackErrorCode::NoCompatibleStream => {
                                ui_text(ids::PLAYBACK_ERROR_NO_STREAM)
                            }
                            jellyfin_api::PlaybackErrorCode::RateLimitExceeded => {
                                ui_text(ids::PLAYBACK_ERROR_RATE_LIMITED)
                            }
                            jellyfin_api::PlaybackErrorCode::Unknown => {
                                ui_text(ids::PLAYBACK_ERROR_UNKNOWN)
                            }
                        };
                        ui.label(
                            RichText::new(message)
                                .size(12.0)
                                .color(Color32::from_rgb(0xF0, 0xA0, 0x99)),
                        );
                    } else if info.media_sources.is_empty() {
                        ui.label(
                            RichText::new(ui_text(ids::PLAYBACK_ERROR_NO_SOURCE))
                                .size(12.0)
                                .color(colors.muted),
                        );
                    } else {
                        if let Some(selection) =
                            select_playback_info_source_for_profile(info, app.device_profile())
                        {
                            let label = match selection.method {
                                PlaybackMethod::DirectPlay => {
                                    ui_text(ids::PLAYBACK_METHOD_DIRECT_PLAY)
                                }
                                PlaybackMethod::DirectStream => {
                                    ui_text(ids::PLAYBACK_METHOD_DIRECT_STREAM)
                                }
                                PlaybackMethod::Transcode => {
                                    ui_text(ids::PLAYBACK_METHOD_TRANSCODE)
                                }
                            };
                            ui.label(
                                RichText::new(ui_text_args(
                                    ids::PLAYBACK_STATUS_READY_VIA,
                                    &[("label", label)],
                                ))
                                .size(12.0)
                                .color(colors.muted),
                            );
                        } else {
                            ui.label(
                                RichText::new(ui_text(ids::PLAYBACK_ERROR_NO_SOURCE))
                                    .color(colors.muted),
                            );
                        }
                    }
                }
                if app.playback_start_loading {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(
                            RichText::new(ui_text(ids::PLAYBACK_STATUS_PREPARING))
                                .color(colors.muted),
                        );
                    });
                }
                if let Some(status) = app.playback_start_status.as_deref() {
                    ui.label(RichText::new(status).size(11.0).color(colors.muted));
                }
                if let Some(message) = app.playback_start_error.as_deref() {
                    ui.label(
                        RichText::new(ui_text_args(
                            ids::PLAYBACK_ERROR_SERVER_START,
                            &[("message", message)],
                        ))
                        .size(12.0)
                        .color(Color32::from_rgb(0xF0, 0xA0, 0x99)),
                    );
                }
            } else if let Some(message) = app.details_error.clone() {
                detail_error_action = detail_error_panel(ui, &message);
            } else if !app.details_loading {
                ui.add_space(6.0);
                ui.label(RichText::new(ui_text(ids::DETAIL_EMPTY_DETAILS)).color(colors.muted));
            }
        });
    if detail_back {
        app.navigate_back();
    }
    match detail_error_action {
        Some(DetailErrorAction::Back) => app.navigate_back(),
        Some(DetailErrorAction::Retry) => app.retry_item_details(),
        None => {}
    }
    if watched_clicked {
        app.request_watched_toggle();
    }
    if favorite_clicked {
        app.request_favorite_toggle();
    }
    if queue_audio_clicked || play_next_clicked {
        app.queue_selected_audio(play_next_clicked);
    }
    if queue_album_clicked {
        app.request_selected_album_tracks();
    }
    if let Some(season_id) = season_to_load {
        app.select_series_season(season_id);
        ui.ctx().request_repaint();
    }
    if let Some(episode_id) = episode_to_open {
        // The play button plays the episode, resuming where it was left.
        app.play_item(episode_id, false);
        ui.ctx().request_repaint();
    }
    if let Some(episode_id) = episode_page_to_open {
        app.open_child_item(episode_id);
        ui.ctx().request_repaint();
    }
    if let Some(person_id) = person_to_open {
        app.open_person_page_by_id(&person_id);
        ui.ctx().request_repaint();
    }
    if let Some(item_id) = similar_item_to_open {
        app.select_item(item_id);
        ui.ctx().request_repaint();
    }
    if let Some((item_id, title)) = download_target {
        app.start_download(&item_id, &title);
    }
    if let Some(start_over) = play_request
        && detail_error_action.is_none()
        && let Some(item_id) = app.selected_item_id.clone()
    {
        app.play_item(item_id, start_over);
        ui.ctx().request_repaint();
    }
    ui.add_space(20.0);
}

/// Item types that are containers or people. Play does not apply to them; their pages list
/// children (seasons, tracks, items) instead.
fn is_playable_item_type(item_type: Option<&str>) -> bool {
    !matches!(
        item_type,
        Some(
            "Series"
                | "Season"
                | "BoxSet"
                | "MusicAlbum"
                | "MusicArtist"
                | "Person"
                | "Folder"
                | "CollectionFolder"
                | "Playlist"
                | "Genre"
                | "Studio"
        )
    )
}

/// The buttons on the details header: a white Play (or Resume) pill, Start over when there is a
/// saved position, and Download.
/// "Play S1:E3 · Title", or "Resume …" when the episode was left part way.
fn series_next_up_button(
    ui: &mut egui::Ui,
    episode: &jellyfin_api::BaseItemDto,
    colors: Palette,
) -> egui::Response {
    let label = episode_display_label(episode);
    let resumable = episode.user_data.as_ref().is_some_and(|user_data| {
        resumable_position_ticks(
            user_data.playback_position_ticks,
            user_data.played,
            episode.run_time_ticks,
        )
        .is_some()
    });
    let text = ui_text_args(
        if resumable {
            ids::SERIES_BUTTON_RESUME_NEXT_UP
        } else {
            ids::SERIES_BUTTON_PLAY_NEXT_UP
        },
        &[("episode", label.as_str())],
    );
    ui.add(
        egui::Button::new(
            RichText::new(text)
                .size(17.0)
                .strong()
                .color(colors.background),
        )
        .fill(colors.accent)
        .corner_radius(CornerRadius::same(24))
        .min_size(egui::vec2(150.0, 48.0)),
    )
}

/// "S1:E3 · Title" for an episode with season and episode numbers, else its name.
pub(crate) fn episode_display_label(episode: &jellyfin_api::BaseItemDto) -> String {
    match (episode.parent_index_number, episode.index_number) {
        (Some(season), Some(number)) => format!("S{season}:E{number} · {}", episode.name),
        (None, Some(number)) => format!("E{number} · {}", episode.name),
        _ => episode.name.clone(),
    }
}

#[cfg(test)]
mod next_up_label_tests {
    use super::episode_display_label;

    #[test]
    fn episode_labels_carry_season_and_number_when_known() {
        let episode = |json: serde_json::Value| -> jellyfin_api::BaseItemDto {
            serde_json::from_value(json).expect("episode fixture")
        };
        assert_eq!(
            episode_display_label(&episode(serde_json::json!({
                "Id": "e", "Name": "Pilot", "ParentIndexNumber": 1, "IndexNumber": 3
            }))),
            "S1:E3 · Pilot"
        );
        assert_eq!(
            episode_display_label(&episode(serde_json::json!({
                "Id": "e", "Name": "Pilot", "IndexNumber": 3
            }))),
            "E3 · Pilot"
        );
        assert_eq!(
            episode_display_label(&episode(serde_json::json!({"Id": "e", "Name": "Pilot"}))),
            "Pilot"
        );
    }
}

fn detail_play_buttons(
    ui: &mut egui::Ui,
    app: &KattriApp,
    item: &jellyfin_api::BaseItemDto,
    colors: Palette,
    play_request: &mut Option<bool>,
    download_target: &mut Option<(String, String)>,
) {
    let playable = is_playable_item_type(item.item_type.as_deref());
    let downloadable = is_downloadable_item_type(item.item_type.as_deref());
    if !playable && !downloadable {
        return;
    }
    let resume_ticks = item.user_data.as_ref().and_then(|user_data| {
        resumable_position_ticks(
            user_data.playback_position_ticks,
            user_data.played,
            item.run_time_ticks,
        )
    });
    let busy = app.playback_loading || app.playback_start_loading || app.auto_play.is_some();
    ui.horizontal(|ui| {
        if playable {
            let primary_label = match resume_ticks {
                Some(ticks) => ui_text_args(
                    ids::PLAYBACK_BUTTON_RESUME_AT,
                    &[("position", &format_resume_position(ticks))],
                ),
                None => ui_text(ids::PLAYBACK_BUTTON_PLAY).to_owned(),
            };
            let primary = egui::Button::new(
                RichText::new(primary_label)
                    .size(17.0)
                    .strong()
                    .color(colors.background),
            )
            .fill(colors.accent)
            .corner_radius(CornerRadius::same(24))
            .min_size(egui::vec2(150.0, 48.0));
            if ui.add_enabled(!busy, primary).clicked() {
                *play_request = Some(false);
            }
            if resume_ticks.is_some()
                && ui
                    .add_enabled(
                        !busy,
                        egui::Button::new(
                            RichText::new(ui_text(ids::PLAYBACK_BUTTON_START_OVER)).size(16.0),
                        )
                        .fill(Color32::from_white_alpha(46))
                        .corner_radius(CornerRadius::same(24))
                        .min_size(egui::vec2(130.0, 48.0)),
                    )
                    .clicked()
            {
                *play_request = Some(true);
            }
        }
        if downloadable
            && ui
                .add(
                    egui::Button::new(
                        RichText::new(ui_text(ids::DETAIL_BUTTON_DOWNLOAD)).size(16.0),
                    )
                    .fill(Color32::from_white_alpha(46))
                    .corner_radius(CornerRadius::same(24))
                    .min_size(egui::vec2(120.0, 48.0)),
                )
                .clicked()
        {
            *download_target = Some((item.id.clone(), item.name.clone()));
        }
        if busy {
            ui.spinner();
            let status = if app.playback_start_loading {
                ui_text(ids::PLAYBACK_STATUS_PREPARING)
            } else {
                ui_text(ids::PLAYBACK_STATUS_REQUESTING)
            };
            ui.label(RichText::new(status).color(Color32::from_gray(210)));
        }
    });
}

fn person_page_panel(ui: &mut egui::Ui, app: &mut KattriApp, colors: Palette) {
    let Some(person) = app.selected_person.as_ref() else {
        return;
    };
    let mut go_back = false;
    let mut selected_item_id = None;
    egui::Frame::new()
        .fill(colors.panel)
        .stroke(Stroke::new(1.0, colors.divider))
        .corner_radius(CornerRadius::same(10))
        .inner_margin(egui::Margin::same(16))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                if ui
                    .button(ui_text(ids::PERSON_BUTTON_BACK_TO_ITEM))
                    .clicked()
                {
                    go_back = true;
                }
                ui.heading(
                    person
                        .name
                        .as_deref()
                        .unwrap_or(ui_text(ids::PERSON_TITLE_DEFAULT)),
                );
            });
            if app.person_items_loading {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(
                        RichText::new(ui_text(ids::PERSON_STATUS_LOADING)).color(colors.muted),
                    );
                });
            } else if let Some(error) = app.person_items_error.as_deref() {
                ui.label(
                    RichText::new(ui_text_args(ids::PERSON_ERROR_LOAD, &[("error", error)]))
                        .color(Color32::from_rgb(0xF0, 0xA0, 0x99)),
                );
            } else if app.person_items.is_empty() {
                ui.label(RichText::new(ui_text(ids::PERSON_EMPTY)).color(colors.muted));
            } else {
                ui.add_space(8.0);
                ui.horizontal_wrapped(|ui| {
                    for item in &app.person_items {
                        if ui
                            .add(egui::Button::new(&item.name).min_size(egui::vec2(180.0, 56.0)))
                            .clicked()
                        {
                            selected_item_id = Some(item.id.clone());
                        }
                    }
                });
            }
        });
    if go_back {
        app.navigate_back();
    } else if let Some(item_id) = selected_item_id {
        app.select_item(item_id);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DetailErrorAction {
    Back,
    Retry,
}

fn detail_error_panel(ui: &mut egui::Ui, message: &str) -> Option<DetailErrorAction> {
    let unavailable = message == ui_text(ids::DETAIL_ERROR_UNAVAILABLE);
    let label = if unavailable {
        message.to_owned()
    } else {
        ui_text_args(ids::DETAIL_ERROR_LOAD, &[("message", message)])
    };
    ui.add_space(6.0);
    ui.label(
        RichText::new(label)
            .size(12.0)
            .color(Color32::from_rgb(0xF0, 0xA0, 0x99)),
    );
    let button = if unavailable {
        ui_text(ids::DETAIL_BUTTON_BACK)
    } else {
        ui_text(ids::DETAIL_BUTTON_RETRY)
    };
    if ui.button(button).clicked() {
        Some(if unavailable {
            DetailErrorAction::Back
        } else {
            DetailErrorAction::Retry
        })
    } else {
        None
    }
}

#[cfg(test)]
mod detail_error_panel_tests {
    use super::{DetailErrorAction, detail_error_panel};
    use egui::{Event, Modifiers, PointerButton, Pos2, RawInput, Rect, Shape, Vec2};

    const NOT_AVAILABLE: &str = "This item is no longer available";

    #[test]
    fn item_404_displays_unavailable_message_and_back_action() {
        let context = egui::Context::default();
        let (initial, action) = show_panel(&context, Vec::new());

        assert_eq!(action, None);
        assert!(text_shape(&initial, NOT_AVAILABLE).is_some());
        let back = text_shape(&initial, "Back").expect("Back action is rendered");
        let click_position = back.pos + back.galley.rect.size() / 2.0;
        let pointer_events = |pressed| {
            vec![
                Event::PointerMoved(click_position),
                Event::PointerButton {
                    pos: click_position,
                    button: PointerButton::Primary,
                    pressed,
                    modifiers: Modifiers::NONE,
                },
            ]
        };

        let (_, press_action) = show_panel(&context, pointer_events(true));
        let (_, release_action) = show_panel(&context, pointer_events(false));

        assert_eq!(press_action, None);
        assert_eq!(release_action, Some(DetailErrorAction::Back));
    }

    fn show_panel(
        context: &egui::Context,
        events: Vec<Event>,
    ) -> (egui::FullOutput, Option<DetailErrorAction>) {
        let mut action = None;
        let mut output = context.run_ui(
            RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(640.0, 360.0))),
                events,
                ..Default::default()
            },
            |ui| {
                action = detail_error_panel(ui, NOT_AVAILABLE);
            },
        );
        output.textures_delta.clear();
        (output, action)
    }

    fn text_shape<'a>(
        output: &'a egui::FullOutput,
        text: &str,
    ) -> Option<&'a egui::epaint::TextShape> {
        fn find<'a>(shape: &'a Shape, text: &str) -> Option<&'a egui::epaint::TextShape> {
            match shape {
                Shape::Text(shape) if shape.galley.job.text == text => Some(shape),
                Shape::Vec(shapes) => shapes.iter().find_map(|shape| find(shape, text)),
                _ => None,
            }
        }

        output
            .shapes
            .iter()
            .find_map(|clipped| find(&clipped.shape, text))
    }
}

pub(crate) fn resumable_position_ticks(
    position_ticks: Option<i64>,
    played: Option<bool>,
    runtime_ticks: Option<i64>,
) -> Option<i64> {
    if played == Some(true) {
        return None;
    }
    let position_ticks = position_ticks.filter(|ticks| *ticks > 0)?;
    if runtime_ticks.is_some_and(|runtime| runtime <= position_ticks) {
        return None;
    }
    Some(position_ticks)
}

fn format_resume_position(ticks: i64) -> String {
    let total_seconds = (ticks.max(0) as u64) / 10_000_000;
    let hours = total_seconds / 3_600;
    let minutes = (total_seconds / 60) % 60;
    let seconds = total_seconds % 60;
    format!("{hours:02}:{minutes:02}:{seconds:02}")
}

#[cfg(test)]
mod resume_playback_tests {
    use super::{format_resume_position, resumable_position_ticks};

    #[test]
    fn offers_only_a_valid_unfinished_resume_position() {
        assert_eq!(
            resumable_position_ticks(Some(42), Some(false), Some(100)),
            Some(42)
        );
        assert_eq!(resumable_position_ticks(Some(42), None, None), Some(42));
    }

    #[test]
    fn rejects_empty_watched_negative_and_past_end_positions() {
        assert_eq!(resumable_position_ticks(None, Some(false), Some(100)), None);
        assert_eq!(
            resumable_position_ticks(Some(0), Some(false), Some(100)),
            None
        );
        assert_eq!(
            resumable_position_ticks(Some(-1), Some(false), Some(100)),
            None
        );
        assert_eq!(
            resumable_position_ticks(Some(42), Some(true), Some(100)),
            None
        );
        assert_eq!(
            resumable_position_ticks(Some(100), Some(false), Some(100)),
            None
        );
        assert_eq!(
            resumable_position_ticks(Some(101), Some(false), Some(100)),
            None
        );
    }

    #[test]
    fn displays_resume_time_as_a_clock_timestamp() {
        assert_eq!(format_resume_position(37_230_000_000), "01:02:03");
    }
}

#[cfg(test)]
mod collection_detail_tests {
    use super::{collection_summary_text, empty_collection_detail};
    use crate::app::BrowseContext;
    use jellyfin_api::CollectionRuntime;

    #[test]
    fn empty_box_set_children_show_the_collection_empty_detail() {
        let context = BrowseContext {
            parent_id: "box-set".into(),
            title: "Collection".into(),
            is_collection: true,
        };
        assert_eq!(
            empty_collection_detail(Some(&context)),
            Some((
                "This collection is empty",
                "Jellyfin returned no child items for this collection."
            ))
        );
        assert_eq!(empty_collection_detail(None), None);
    }

    #[test]
    fn collection_summary_shows_count_runtime_and_unknown_marker() {
        assert_eq!(
            collection_summary_text(
                Some(5),
                5,
                CollectionRuntime {
                    total_ticks: 5_400 * 10_000_000,
                    items_with_runtime: 4,
                    items_without_runtime: 1,
                },
            ),
            "5 items · Runtime: 1h 30m · some item runtimes unavailable"
        );
    }

    #[test]
    fn collection_summary_marks_partial_page_runtime_as_loaded_only() {
        assert_eq!(
            collection_summary_text(
                Some(60),
                50,
                CollectionRuntime {
                    total_ticks: 3_600 * 10_000_000,
                    items_with_runtime: 10,
                    items_without_runtime: 0,
                },
            ),
            "60 items · Runtime for loaded items: 1h 0m · 50 loaded"
        );
    }

    #[test]
    fn collection_summary_does_not_assume_an_unknown_total_is_fully_loaded() {
        assert_eq!(
            collection_summary_text(
                None,
                50,
                CollectionRuntime {
                    total_ticks: 1_800 * 10_000_000,
                    items_with_runtime: 50,
                    items_without_runtime: 0,
                },
            ),
            "50 loaded items · Runtime for loaded items: 0h 30m"
        );
    }

    #[test]
    fn collection_summary_marks_runtime_unavailable_when_none_is_known() {
        assert_eq!(
            collection_summary_text(
                Some(2),
                2,
                CollectionRuntime {
                    total_ticks: 0,
                    items_with_runtime: 0,
                    items_without_runtime: 2,
                },
            ),
            "2 items · Runtime unavailable · some item runtimes unavailable"
        );
    }
}

fn home_header(ui: &mut egui::Ui, app: &KattriApp, colors: Palette) {
    let Some(server) = app.server.as_ref() else {
        return;
    };
    let user_name = app
        .user_name
        .as_deref()
        .unwrap_or(ui_text(ids::SHELL_USER_DEFAULT));
    let item_count = app.total_record_count.unwrap_or(app.items.len() as u32);
    let first = if app.items.is_empty() {
        0
    } else {
        app.start_index + 1
    };
    let last = app
        .start_index
        .saturating_add(app.items.len() as u32)
        .min(item_count);
    egui::Frame::new()
        .fill(colors.panel)
        .stroke(Stroke::new(1.0, colors.divider))
        .corner_radius(CornerRadius::same(12))
        .inner_margin(egui::Margin::same(24))
        .show(ui, |ui| {
            ui.set_min_height(150.0);
            ui.label(
                RichText::new(ui_text(ids::SHELL_EYEBROW_CONNECTED))
                    .size(11.0)
                    .strong()
                    .color(colors.accent),
            );
            ui.add_space(8.0);
            ui.label(
                RichText::new(&server.name)
                    .size(28.0)
                    .strong()
                    .color(colors.text),
            );
            let version = server
                .version
                .as_deref()
                .unwrap_or(ui_text(ids::SHELL_SERVER_VERSION_MISSING));
            ui.label(
                RichText::new(ui_text_args(
                    ids::SHELL_SUMMARY_USER_SERVER,
                    &[("user_name", user_name), ("version", version)],
                ))
                .size(14.0)
                .color(colors.muted),
            );
            ui.add_space(10.0);
            ui.label(
                RichText::new(ui_text_args(
                    ids::BROWSE_RANGE_SHOWING,
                    &[
                        (
                            "range",
                            &app.total_record_count.map_or_else(
                                || {
                                    ui_text_args(
                                        ids::BROWSE_COUNT_LOADED,
                                        &[("count", &item_count.to_string())],
                                    )
                                },
                                |total| {
                                    ui_text_args(
                                        ids::BROWSE_COUNT_TOTAL_REPORTED,
                                        &[("total", &total.to_string())],
                                    )
                                },
                            ),
                        ),
                        ("first", &first.to_string()),
                        ("last", &last.to_string()),
                    ],
                ))
                .size(12.0)
                .color(colors.muted),
            );
        });
}

fn shelf<'a, I>(
    ui: &mut egui::Ui,
    content: ShelfContent<'a, I>,
    colors: Palette,
    entries: &mut Vec<CardFocus>,
    selected_item_id: Option<&str>,
    visible_artwork: &mut Vec<(String, String)>,
) -> Option<String>
where
    I: Iterator<Item = &'a MediaItem>,
{
    let ShelfContent {
        title,
        description,
        items,
        row,
    } = content;
    ui.horizontal(|ui| {
        ui.vertical(|ui| {
            ui.label(RichText::new(title).size(20.0).strong().color(colors.text));
            ui.label(RichText::new(description).size(12.0).color(colors.muted));
        });
        ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
            ui.label(
                RichText::new(ui_text(ids::BROWSE_HINT_KEYBOARD))
                    .size(11.0)
                    .color(colors.muted),
            );
        });
    });
    ui.add_space(12.0);

    let mut clicked_item_id = None;
    egui::ScrollArea::horizontal()
        .id_salt(("media_shelf", row, title))
        .auto_shrink([false, true])
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                for item in items {
                    let selected = selected_item_id == Some(item.id.as_str());
                    if catalog_card(ui, item, row, colors, entries, selected, visible_artwork) {
                        clicked_item_id = Some(item.id.clone());
                    }
                    ui.add_space(6.0);
                }
            });
        });
    clicked_item_id
}

/// One poster in a shelf or grid. Registers it for keyboard and gamepad focus on `row`, and
/// queues its artwork when it is on screen. True when it was clicked.
fn catalog_card(
    ui: &mut egui::Ui,
    item: &MediaItem,
    row: usize,
    colors: Palette,
    entries: &mut Vec<CardFocus>,
    selected: bool,
    visible_artwork: &mut Vec<(String, String)>,
) -> bool {
    let response = artwork::media_card(ui, item, colors, selected);
    item_menu::attach(&response, item);
    if ui.clip_rect().intersects(response.rect)
        && let Some(tag) = item.primary_image_tag.as_deref()
    {
        visible_artwork.push((item.artwork_id.clone(), tag.to_owned()));
    }
    let clicked = response.clicked();
    entries.push(CardFocus {
        row,
        id: FocusId::new(format!("catalog:{}", item.id)),
        center_x: response.rect.center().x,
        focused: response.has_focus(),
        response,
        home_poster: None,
        selection: HomeAction::Select(item.id.clone()),
    });
    clicked
}

/// Add a search result to the directional and gamepad focus graph. Search cards use the same
/// selection action as catalog cards, while their row grouping comes from the result category.
fn push_search_focus(
    entries: &mut Vec<CardFocus>,
    row: usize,
    item_id: &str,
    response: egui::Response,
) {
    entries.push(CardFocus {
        row,
        id: FocusId::new(format!("catalog:{item_id}")),
        center_x: response.rect.center().x,
        focused: response.has_focus(),
        response,
        home_poster: None,
        selection: HomeAction::Select(item_id.to_owned()),
    });
}

fn push_search_show_all_focus(
    entries: &mut Vec<CardFocus>,
    targets: &mut Vec<(FocusId, search::SearchGroup)>,
    row: usize,
    group: search::SearchGroup,
    response: egui::Response,
) {
    let id = FocusId::new(format!("search-show-all:{group:?}"));
    targets.push((id.clone(), group));
    // The shared graph entry carries a HomeAction; activation resolves this target first.
    entries.push(CardFocus {
        row,
        id,
        center_x: response.rect.center().x,
        focused: response.has_focus(),
        response,
        home_poster: None,
        selection: HomeAction::Select(String::new()),
    });
}

fn resolve_focus_activation(
    target_id: &FocusId,
    entries: &[CardFocus],
    search_show_all_targets: &[(FocusId, search::SearchGroup)],
) -> Option<FocusActivation> {
    if let Some((_, group)) = search_show_all_targets
        .iter()
        .find(|(id, _)| id == target_id)
    {
        return Some(FocusActivation::SearchShowAll(*group));
    }
    entries
        .iter()
        .find(|entry| &entry.id == target_id)
        .map(|entry| FocusActivation::Home(entry.selection.clone()))
}

/// Horizontal space one poster takes in a grid line, including the gap after it.
const GRID_CARD_STRIDE: f32 = artwork::CARD_WIDTH + GRID_GAP;
const GRID_GAP: f32 = 20.0;

/// How many posters fit on one grid line at `width`.
fn grid_columns(width: f32) -> usize {
    ((width + GRID_GAP) / GRID_CARD_STRIDE).floor().max(1.0) as usize
}

/// A library page: the title with its item count, then the posters in wrapping lines. Each line
/// is its own focus row, so Up and Down move between lines and Left and Right along one.
fn poster_grid(
    ui: &mut egui::Ui,
    title: &str,
    items: &[&MediaItem],
    colors: Palette,
    entries: &mut Vec<CardFocus>,
    selected_item_id: Option<&str>,
    visible_artwork: &mut Vec<(String, String)>,
) -> Option<String> {
    ui.horizontal(|ui| {
        ui.label(RichText::new(title).size(26.0).strong().color(colors.text));
        ui.label(
            RichText::new(items.len().to_string())
                .size(16.0)
                .color(colors.muted),
        );
    });
    ui.add_space(18.0);
    let columns = grid_columns(ui.available_width());
    let mut clicked_item_id = None;
    for (line, chunk) in items.chunks(columns).enumerate() {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = GRID_GAP;
            for item in chunk {
                let selected = selected_item_id == Some(item.id.as_str());
                if catalog_card(ui, item, line, colors, entries, selected, visible_artwork) {
                    clicked_item_id = Some(item.id.clone());
                }
            }
        });
        ui.add_space(14.0);
    }
    clicked_item_id
}

fn error_banner(ui: &mut egui::Ui, app: &mut KattriApp, colors: Palette, message: &str) {
    egui::Frame::new()
        .fill(Color32::from_rgb(0x3A, 0x25, 0x2A))
        .stroke(Stroke::new(1.0, Color32::from_rgb(0xD9, 0x7F, 0x78)))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(egui::Margin::same(12))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.label(
                        RichText::new(ui_text(ids::BROWSE_ERROR_REQUEST_FAILED))
                            .strong()
                            .color(colors.text),
                    );
                    ui.label(RichText::new(message).size(12.0).color(colors.muted));
                });
                if with_access_name(
                    ui.button(ui_text(ids::BROWSE_BUTTON_RETRY)),
                    egui::Role::Button,
                    ids::ACCESS_BROWSE_RETRY,
                )
                .clicked()
                {
                    app.retry_items();
                }
            });
        });
    ui.add_space(18.0);
}

fn state_panel(
    ui: &mut egui::Ui,
    colors: Palette,
    title: &str,
    message: &str,
    action: Option<(&str, StateAction)>,
    app: &mut KattriApp,
) {
    ui.add_space(70.0);
    ui.vertical_centered(|ui| {
        ui.label(RichText::new("◌").size(42.0).color(colors.accent));
        ui.add_space(10.0);
        ui.label(RichText::new(title).size(23.0).strong().color(colors.text));
        ui.add_space(6.0);
        ui.label(RichText::new(message).size(14.0).color(colors.muted));
        if let Some((label, action)) = action {
            ui.add_space(16.0);
            let button = ui.button(label);
            let button = match state_access_id(action) {
                Some(id) => with_access_name(button, egui::Role::Button, id),
                None => button,
            };
            if button.clicked() {
                match action {
                    StateAction::RetryItems => app.retry_items(),
                    StateAction::ClearSearch => app
                        .clear_search(duration_from_egui_time(ui.ctx().input(|input| input.time))),
                    StateAction::ClearCatalogFilters => app.clear_catalog_filters(),
                    StateAction::OpenHome => app.navigate_to(Page::Home),
                }
            }
        }
    });
}

#[derive(Clone, Copy)]
enum StateAction {
    RetryItems,
    ClearSearch,
    ClearCatalogFilters,
    OpenHome,
}

fn has_server(status: &ConnectionStatus) -> bool {
    matches!(
        status,
        ConnectionStatus::Connected
            | ConnectionStatus::LoadingItems
            | ConnectionStatus::LibraryError(_)
    )
}

fn move_navigation_focus(
    ctx: &egui::Context,
    responses: &[egui::Response],
    app: &mut KattriApp,
    gamepad_actions: &mut Vec<NavAction>,
) -> bool {
    let Some(index) = responses.iter().position(egui::Response::has_focus) else {
        return false;
    };
    let accepted = [
        NavAction::Left,
        NavAction::Right,
        NavAction::Down,
        NavAction::Select,
    ];
    let action = (!ctx.egui_wants_keyboard_input())
        .then(|| {
            crate::input::take_action(ctx, &accepted, app.settings.settings().input.key_repeat)
        })
        .flatten()
        .or_else(|| take_gamepad_action(gamepad_actions, &accepted));
    match action {
        Some(NavAction::Left) if index > 0 => responses[index - 1].request_focus(),
        Some(NavAction::Right) if index + 1 < responses.len() => {
            responses[index + 1].request_focus();
        }
        Some(NavAction::Select) => match index.checked_sub(1) {
            None => app.set_view(AppView::Home),
            Some(page_index) => {
                let pages = navigation_pages(app.dashboard_enabled());
                if let Some(page) = pages.get(page_index) {
                    app.navigate_to(*page);
                }
            }
        },
        Some(NavAction::Down) => return true,
        _ => {}
    }
    false
}

fn move_media_focus(
    ctx: &egui::Context,
    entries: &[CardFocus],
    graph: &mut FocusGraph,
    home: &mut home::HomeState,
    gamepad_actions: &mut Vec<NavAction>,
    options: MediaFocusOptions<'_>,
) -> Option<FocusId> {
    let MediaFocusOptions {
        allow_key_repeat,
        enter_content,
        top_bar_tab,
    } = options;
    if entries.is_empty() {
        let _ = graph.set_rows(Vec::new());
        return None;
    }

    let previous = graph.current().cloned();
    let row_count = entries.iter().map(|entry| entry.row).max().unwrap_or(0) + 1;
    let mut rows = vec![Vec::new(); row_count];
    for entry in entries {
        rows[entry.row].push(FocusNode::new(entry.id.clone(), entry.center_x));
    }
    if graph.set_rows(rows).is_err() {
        return None;
    }
    let current = graph.current().cloned();
    let focused_item_was_removed = previous.is_some() && previous != current;
    if !focused_item_was_removed && let Some(active) = entries.iter().find(|entry| entry.focused) {
        graph.focus(&active.id);
    }

    let focused_in_content = entries.iter().any(|entry| entry.response.has_focus());
    let focused_elsewhere = ctx.memory(|memory| memory.focused().is_some()) && !focused_in_content;
    if focused_elsewhere
        && !enter_content
        && (!focused_item_was_removed || ctx.egui_wants_keyboard_input())
    {
        return None;
    }

    let accepted = [
        NavAction::Left,
        NavAction::Right,
        NavAction::Up,
        NavAction::Down,
        NavAction::Select,
    ];
    let action = if enter_content {
        None
    } else {
        (!ctx.egui_wants_keyboard_input())
            .then(|| crate::input::take_action(ctx, &accepted, allow_key_repeat))
            .flatten()
            .or_else(|| take_gamepad_action(gamepad_actions, &accepted))
    };

    if let Some(action) = action {
        let Some(target_id) = graph.navigate(action) else {
            if action == NavAction::Up
                && let Some(tab) = top_bar_tab
            {
                tab.request_focus();
            }
            return None;
        };
        let target = entries.iter().find(|entry| entry.id == target_id)?;
        target.response.request_focus();
        target.response.scroll_to_me(Some(Align::Center));
        if let Some((group, index)) = target.home_poster {
            home.queue_poster_focus(group, index);
        }
        if action == NavAction::Select {
            return Some(target.id.clone());
        }
    } else if (enter_content
        || focused_item_was_removed
        || (ctx.memory(|memory| memory.focused().is_none()) && current.is_some()))
        && !ctx.egui_wants_keyboard_input()
        && let Some(target_id) = current
        && let Some(target) = entries.iter().find(|entry| entry.id == target_id)
    {
        target.response.request_focus();
        target.response.scroll_to_me(Some(Align::Center));
        if let Some((group, index)) = target.home_poster {
            home.queue_poster_focus(group, index);
        }
    }
    None
}

#[cfg(test)]
mod search_focus_tests {
    use super::{
        CardFocus, FocusActivation, MediaFocusOptions, move_media_focus,
        push_search_show_all_focus, resolve_focus_activation, search,
    };
    use egui::{Event, Key, Modifiers, Pos2, RawInput, Rect, Vec2};
    use kattri_client_core::focus::{FocusGraph, NavAction};

    #[test]
    fn keyboard_and_gamepad_select_activate_registered_show_all_target() {
        fn activate(
            events: Vec<Event>,
            mut gamepad_actions: Vec<NavAction>,
        ) -> Option<FocusActivation> {
            let context = egui::Context::default();
            let mut entries: Vec<CardFocus> = Vec::new();
            let mut show_all_targets = Vec::new();
            let mut graph = FocusGraph::default();
            let mut home = super::home::HomeState::default();
            let frame = RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(800.0, 600.0))),
                events,
                ..Default::default()
            };

            let mut activated = None;
            let mut output = context.run_ui(frame, |ui| {
                let response = ui.button("Show all Photos");
                push_search_show_all_focus(
                    &mut entries,
                    &mut show_all_targets,
                    0,
                    search::SearchGroup::Photos,
                    response,
                );
                let focused_id = move_media_focus(
                    ui.ctx(),
                    &entries,
                    &mut graph,
                    &mut home,
                    &mut gamepad_actions,
                    MediaFocusOptions {
                        allow_key_repeat: false,
                        enter_content: false,
                        top_bar_tab: None,
                    },
                );
                activated = focused_id
                    .and_then(|id| resolve_focus_activation(&id, &entries, &show_all_targets));
            });
            output.textures_delta.clear();
            activated
        }

        let gamepad = activate(Vec::new(), vec![NavAction::Select]);
        let keyboard = activate(
            vec![Event::Key {
                key: Key::Enter,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: Modifiers::default(),
            }],
            Vec::new(),
        );
        let expected = Some(FocusActivation::SearchShowAll(search::SearchGroup::Photos));
        assert_eq!(gamepad, expected);
        assert_eq!(keyboard, expected);
    }
}

fn take_gamepad_action(actions: &mut Vec<NavAction>, accepted: &[NavAction]) -> Option<NavAction> {
    let index = actions
        .iter()
        .position(|action| accepted.contains(action))?;
    Some(actions.remove(index))
}

fn handle_shell_action(
    ctx: &egui::Context,
    app: &mut KattriApp,
    gamepad_actions: &mut Vec<NavAction>,
) {
    let accepted = [NavAction::Back, NavAction::Menu, NavAction::PlayPause];
    let action = (!ctx.egui_wants_keyboard_input())
        .then(|| {
            crate::input::take_action(ctx, &accepted, app.settings.settings().input.key_repeat)
        })
        .flatten()
        .or_else(|| take_gamepad_action(gamepad_actions, &accepted));
    match action {
        Some(NavAction::Back) => app.navigate_back(),
        Some(NavAction::Menu) => app.navigate_to(Page::Settings),
        Some(NavAction::PlayPause) | None => {}
        Some(
            NavAction::Up
            | NavAction::Down
            | NavAction::Left
            | NavAction::Right
            | NavAction::Select,
        ) => {}
    }
}

#[cfg(test)]
mod dashboard_gate_tests {
    use super::{Page, navigation_pages};

    #[test]
    fn dashboard_entry_is_absent_until_the_gate_is_on() {
        assert!(!navigation_pages(false).contains(&Page::Dashboard));
        assert!(navigation_pages(false).contains(&Page::SyncPlay));
        let enabled = navigation_pages(true);
        assert!(enabled.contains(&Page::Dashboard));
        assert_eq!(enabled.last(), Some(&Page::Settings));
    }
}

/// Pages in navigation order. The dashboard entry appears only when its gate is on, so a
/// disabled dashboard is not reachable by keyboard or by the menu.
fn navigation_pages(dashboard_enabled: bool) -> Vec<Page> {
    let mut pages = navigation_pages_without_settings(dashboard_enabled);
    pages.push(Page::Settings);
    pages
}

fn navigation_pages_without_settings(dashboard_enabled: bool) -> Vec<Page> {
    Page::ALL
        .into_iter()
        .filter(|page| *page != Page::Settings)
        .filter(|page| *page != Page::Dashboard || dashboard_enabled)
        .collect()
}

#[cfg(test)]
mod accessible_name_tests {
    use super::*;
    use egui::accesskit::Role;

    #[test]
    fn home_search_remains_named_and_inside_narrow_navigation() {
        for width in [640.0, 800.0, 1280.0] {
            let context = egui::Context::default();
            context.enable_accesskit();
            let mut app = KattriApp::new(context.clone());
            app.connection = ConnectionStatus::Connected;
            app.view = AppView::Home;
            let mut output = context.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(width, 720.0),
                    )),
                    ..Default::default()
                },
                |ui| {
                    topbar(ui, &mut app, theme::palette(false));
                },
            );
            output.textures_delta.clear();
            let update = output
                .platform_output
                .accesskit_update
                .expect("navigation tree");
            let search = update
                .nodes
                .iter()
                .find(|(_, node)| {
                    node.role() == Role::TextInput
                        && node.label() == Some(ui_text(ids::ACCESS_BROWSE_SEARCH_FIELD))
                })
                .expect("Home exposes search");
            let bounds = search.1.bounds().expect("search bounds");
            assert!(
                bounds.x0 >= 0.0 && bounds.x1 <= f64::from(width),
                "search outside {width}: {bounds:?}"
            );
        }
    }

    #[test]
    fn pilot_control_names_resolve_from_the_en_us_catalog() {
        // An empty fallback makes a missing catalog message fail the test instead of passing.
        for (id, expected) in [
            (ids::ACCESS_CONNECT_SERVER_ADDRESS, "Server address"),
            (ids::ACCESS_CONNECT_USERNAME, "Username"),
            (ids::ACCESS_CONNECT_PASSWORD, "Password"),
            (ids::ACCESS_BROWSE_SORT_BY, "Sort by"),
            (ids::ACCESS_BROWSE_SORT, "Sort order"),
            (ids::ACCESS_BROWSE_PLAYED_FILTER, "Play state"),
        ] {
            assert_eq!(accessible_name(id, ""), expected, "{id}");
        }
    }

    #[test]
    fn connect_screen_text_fields_buttons_and_checkboxes_have_names() {
        let context = egui::Context::default();
        context.enable_accesskit();
        let mut app = KattriApp::new(context.clone());
        let mut output = context.run_ui(egui::RawInput::default(), |ui| {
            draw(ui, &mut app, 1280.0);
        });
        // Font and image uploads are not needed here; clear them so the test drops cleanly.
        output.textures_delta.clear();
        let update = output
            .platform_output
            .accesskit_update
            .expect("headless frame should produce an AccessKit tree update");

        let mut checked = 0;
        let mut unnamed = Vec::new();
        for (_, node) in &update.nodes {
            let interactive = matches!(
                node.role(),
                Role::TextInput | Role::Button | Role::CheckBox | Role::ComboBox | Role::Slider
            );
            if !interactive {
                continue;
            }
            checked += 1;
            let named = node.label().is_some_and(|label| !label.trim().is_empty())
                || !node.labelled_by().is_empty();
            if !named {
                unnamed.push(format!("{:?}", node.role()));
            }
        }
        assert!(
            checked > 0,
            "the connect screen should expose interactive nodes"
        );
        assert!(unnamed.is_empty(), "unnamed controls: {unnamed:?}");
    }
}
