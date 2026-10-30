//! Settings screen: the category list, the pages for playback, audio, subtitles, video
//! and input, and a read-only About section.
//!
//! Each page reads the active profile's settings when it opens and saves a change as soon
//! as the control changes. The open category and focused entry live in egui temporary
//! memory. Pure helpers (category order, About rows, server address text, the focus
//! model) are kept free of egui so they can be unit-tested.
//!
//! Keys: on the list, Up and Down move focus and Enter (Select) opens the focused
//! category. On a category page, Escape or Backspace (Back) returns to the list.
//! Back on the list is left to the app shell, which leaves the Settings page.

use std::path::PathBuf;

use super::{ui_text, ui_text_args};
use egui::{RichText, Ui};
use kattri_client_core::focus::NavAction;
use kattri_i18n::ids;
use kattri_settings::{HdrPreference, ImageFormat, InterfaceSize, LogDetail, Settings};

use crate::app::{KattriApp, Page};
use crate::theme::Palette;
use crate::ui::series_memory;
use crate::ui::settings_input;
use crate::ui::settings_media;

const STATE_ID: &str = "kattri_settings_page_state";
const LIST_KEYS: [NavAction; 3] = [NavAction::Up, NavAction::Down, NavAction::Select];
const ARROW_KEYS: [NavAction; 4] = [
    NavAction::Up,
    NavAction::Down,
    NavAction::Left,
    NavAction::Right,
];
fn scope_text() -> &'static str {
    ui_text(ids::SETTINGS_SCOPE_TEXT)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SettingsSection {
    General,
    Server,
    Playback,
    Audio,
    Subtitles,
    Video,
    Input,
    About,
}

impl SettingsSection {
    /// Display and focus order. Keep this in the order M1-19 names the categories.
    pub(crate) const ALL: [Self; 8] = [
        Self::General,
        Self::Server,
        Self::Playback,
        Self::Audio,
        Self::Subtitles,
        Self::Video,
        Self::Input,
        Self::About,
    ];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::General => ui_text(ids::SETTINGS_TAB_GENERAL),
            Self::Server => ui_text(ids::SETTINGS_TAB_SERVER),
            Self::Playback => ui_text(ids::SETTINGS_TAB_PLAYBACK),
            Self::Audio => ui_text(ids::SETTINGS_TAB_AUDIO),
            Self::Subtitles => ui_text(ids::SETTINGS_TAB_SUBTITLES),
            Self::Video => ui_text(ids::SETTINGS_TAB_VIDEO),
            Self::Input => ui_text(ids::SETTINGS_TAB_INPUT),
            Self::About => ui_text(ids::SETTINGS_TAB_ABOUT),
        }
    }

    /// One line under the label on the settings list: what the page holds.
    pub(crate) fn description(self) -> &'static str {
        match self {
            Self::General => ui_text(ids::SETTINGS_SECTION_GENERAL_DESCRIPTION),
            Self::Server => ui_text(ids::SETTINGS_SECTION_SERVER_DESCRIPTION),
            Self::Playback => ui_text(ids::SETTINGS_SECTION_PLAYBACK_DESCRIPTION),
            Self::Audio => ui_text(ids::SETTINGS_SECTION_AUDIO_DESCRIPTION),
            Self::Subtitles => ui_text(ids::SETTINGS_SECTION_SUBTITLES_DESCRIPTION),
            Self::Video => ui_text(ids::SETTINGS_SECTION_VIDEO_DESCRIPTION),
            Self::Input => ui_text(ids::SETTINGS_SECTION_INPUT_DESCRIPTION),
            Self::About => ui_text(ids::SETTINGS_SECTION_ABOUT_DESCRIPTION),
        }
    }
}

/// Read-only facts shown on the About page. Nothing here is a credential.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AboutInfo {
    pub(crate) version: String,
    pub(crate) target: String,
    pub(crate) libmpv_version: Option<String>,
    pub(crate) gpu_name: Option<String>,
    pub(crate) device_id_path: Option<String>,
    /// Folder of `kattri.log`, for testers who are asked to share it.
    pub(crate) log_folder: Option<String>,
}

impl AboutInfo {
    fn current() -> Self {
        Self {
            version: env!("CARGO_PKG_VERSION").to_owned(),
            target: format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS),
            // This client process does not load libmpv; the mpv host is a separate process.
            libmpv_version: None,
            // No GL host is attached to this process, so no GPU name is available.
            gpu_name: None,
            device_id_path: device_id_path(),
            log_folder: kattri_logging::log_directory().map(|dir| dir.display().to_string()),
        }
    }
}

/// Mirrors the location used by `device_id::load_or_create` so the path can be shown.
/// The file holds a random identifier only; this shows its path, never its contents.
fn device_id_path() -> Option<String> {
    directories::ProjectDirs::from("org", "Jellyfin", "Kattri").map(|dirs| {
        dirs.config_local_dir()
            .join("device-id")
            .display()
            .to_string()
    })
}

/// Label and value pairs for the About page, in display order.
pub(crate) fn about_rows(info: &AboutInfo) -> Vec<(&'static str, String)> {
    vec![
        (
            ui_text(ids::SETTINGS_ABOUT_APP_VERSION),
            info.version.clone(),
        ),
        (
            ui_text(ids::SETTINGS_ABOUT_BUILD_TARGET),
            info.target.clone(),
        ),
        (
            ui_text(ids::SETTINGS_ABOUT_LIBMPV),
            info.libmpv_version
                .clone()
                .unwrap_or_else(|| ui_text(ids::SETTINGS_ABOUT_LIBMPV_NOT_LOADED).to_owned()),
        ),
        (
            ui_text(ids::SETTINGS_ABOUT_GPU),
            info.gpu_name
                .clone()
                .unwrap_or_else(|| ui_text(ids::SETTINGS_VALUE_NOT_AVAILABLE).to_owned()),
        ),
        (
            ui_text(ids::SETTINGS_ABOUT_DEVICE_ID_FILE),
            info.device_id_path
                .clone()
                .unwrap_or_else(|| ui_text(ids::SETTINGS_VALUE_NOT_AVAILABLE).to_owned()),
        ),
        (
            ui_text(ids::SETTINGS_ABOUT_LOG_FOLDER),
            info.log_folder
                .clone()
                .unwrap_or_else(|| ui_text(ids::SETTINGS_VALUE_NOT_AVAILABLE).to_owned()),
        ),
    ]
}

/// The host (and port, if any) of a server address. The scheme, userinfo, path,
/// query, and fragment are dropped so a pasted token or password never reaches the screen.
pub(crate) fn server_address_text(url: &str) -> String {
    let trimmed = url.trim();
    let rest = trimmed
        .split_once("://")
        .map_or(trimmed, |(_, after_scheme)| after_scheme);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host)
        .trim();
    if host.is_empty() {
        ui_text(ids::SETTINGS_VALUE_ADDRESS_UNAVAILABLE).to_owned()
    } else {
        host.to_owned()
    }
}

/// Where keyboard focus sits on the settings page, kept between frames in egui memory.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct SettingsPageState {
    /// Category whose page is showing, or `None` for the category list.
    open: Option<SettingsSection>,
    /// Index into `SettingsSection::ALL` of the focused list entry.
    focus: usize,
    /// Ask egui to move focus onto the focused control on the next frame.
    /// Set after a key move, a click, or a page change; taken once the control is drawn.
    reveal_focus: bool,
    /// The remembered series languages sub-page under Playback (M2-10) is showing.
    series_memory: bool,
}

/// One Up or Down step over `len` entries. Movement clamps at both ends and does not wrap,
/// matching the rail and card focus in `ui/mod.rs`. Any other action keeps the focus.
pub(crate) fn next_focus_index(current: usize, len: usize, action: NavAction) -> usize {
    let Some(last) = len.checked_sub(1) else {
        return 0;
    };
    let current = current.min(last);
    match action {
        NavAction::Up => current.saturating_sub(1),
        NavAction::Down => (current + 1).min(last),
        _ => current,
    }
}

/// Applies one action on the category list. Back is left alone here so the app shell
/// can leave the Settings page.
pub(crate) fn list_action(state: SettingsPageState, action: NavAction) -> SettingsPageState {
    match action {
        NavAction::Up | NavAction::Down => SettingsPageState {
            focus: next_focus_index(state.focus, SettingsSection::ALL.len(), action),
            reveal_focus: true,
            ..state
        },
        NavAction::Select => match SettingsSection::ALL.get(state.focus) {
            Some(section) => SettingsPageState {
                open: Some(*section),
                reveal_focus: true,
                ..state
            },
            None => state,
        },
        _ => state,
    }
}

/// Back leaves the innermost view first: a sub-page returns to its category, and a category
/// returns to the list. The same entry stays focused.
pub(crate) fn page_back(state: SettingsPageState) -> SettingsPageState {
    if state.series_memory {
        return SettingsPageState {
            series_memory: false,
            reveal_focus: true,
            ..state
        };
    }
    SettingsPageState {
        open: None,
        reveal_focus: true,
        ..state
    }
}

/// The egui focus move for a directional navigation action.
fn focus_direction(action: NavAction) -> Option<egui::FocusDirection> {
    match action {
        NavAction::Up => Some(egui::FocusDirection::Up),
        NavAction::Down => Some(egui::FocusDirection::Down),
        NavAction::Left => Some(egui::FocusDirection::Left),
        NavAction::Right => Some(egui::FocusDirection::Right),
        _ => None,
    }
}

fn take_key(
    ctx: &egui::Context,
    accepted: &[NavAction],
    allow_key_repeat: bool,
) -> Option<NavAction> {
    (!ctx.egui_wants_keyboard_input())
        .then(|| crate::input::take_action(ctx, accepted, allow_key_repeat))
        .flatten()
}

fn take_action(
    ctx: &egui::Context,
    accepted: &[NavAction],
    gamepad_actions: &mut Vec<NavAction>,
    allow_key_repeat: bool,
) -> Option<NavAction> {
    take_key(ctx, accepted, allow_key_repeat)
        .or_else(|| super::take_gamepad_action(gamepad_actions, accepted))
}

fn move_gamepad_focus(ctx: &egui::Context, actions: &mut Vec<NavAction>) {
    let action = super::take_gamepad_action(actions, &ARROW_KEYS);
    if let Some(direction) = action.and_then(focus_direction) {
        ctx.memory_mut(|memory| memory.move_focus(direction));
    }
}

/// Arrow keys move focus on an open page, as they do on the category list. A combo box
/// popup keeps its own arrow keys.
fn move_keyboard_focus(ctx: &egui::Context, allow_key_repeat: bool) {
    if ctx.any_popup_open() {
        return;
    }
    if let Some(direction) = take_key(ctx, &ARROW_KEYS, allow_key_repeat).and_then(focus_direction)
    {
        ctx.memory_mut(|memory| memory.move_focus(direction));
    }
}

fn activate_gamepad_focus(ctx: &egui::Context, actions: &mut Vec<NavAction>) {
    if super::take_gamepad_action(actions, &[NavAction::Select]).is_some() {
        ctx.input_mut(|input| {
            input.events.push(egui::Event::Key {
                key: egui::Key::Enter,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::default(),
            });
        });
    }
}

fn load_state(ctx: &egui::Context) -> SettingsPageState {
    ctx.data(|data| data.get_temp::<SettingsPageState>(egui::Id::unique(STATE_ID)))
        .unwrap_or_default()
}

fn save_state(ctx: &egui::Context, state: SettingsPageState) {
    ctx.data_mut(|data| data.insert_temp(egui::Id::unique(STATE_ID), state));
}

pub(crate) fn show(
    ui: &mut Ui,
    app: &mut KattriApp,
    colors: Palette,
    gamepad_actions: &mut Vec<NavAction>,
) {
    let ctx = ui.ctx().clone();
    let mut state = load_state(&ctx);
    let was_open = state.open;
    let allow_key_repeat = app.settings.settings().input.key_repeat;
    // Runs before navigation so that Escape cancels a key capture instead of leaving the page.
    let capturing = settings_input::capture_frame(&ctx, gamepad_actions);
    if state.open.is_some() {
        if !capturing {
            move_keyboard_focus(&ctx, allow_key_repeat);
            move_gamepad_focus(&ctx, gamepad_actions);
            activate_gamepad_focus(&ctx, gamepad_actions);
            if take_action(&ctx, &[NavAction::Back], gamepad_actions, allow_key_repeat).is_some() {
                state = page_back(state);
            }
        }
    } else if let Some(action) = take_action(&ctx, &LIST_KEYS, gamepad_actions, allow_key_repeat) {
        state = list_action(state, action);
    }
    if state.open.is_some() && state.open != was_open {
        // Read the saved values each time a category opens, so an edit made elsewhere shows.
        app.settings.reload();
        settings_input::reset(&ctx);
    }
    match state.open {
        None => section_list(ui, app, &mut state, colors),
        Some(section) => section_page(ui, app, &mut state, colors, section),
    }
    save_state(&ctx, state);
}

fn section_list(ui: &mut Ui, app: &mut KattriApp, state: &mut SettingsPageState, colors: Palette) {
    ui.set_max_width(ui.available_width().min(760.0));
    heading(ui, ui_text(ids::SETTINGS_HEADING_TITLE), colors);
    let running = app.settings.running_name().map(str::to_owned);
    if let Some(name) = running {
        ui.label(
            RichText::new(ui_text_args(
                ids::SETTINGS_LABEL_ACTIVE_PROFILE,
                &[("name", name.as_str())],
            ))
            .color(colors.muted),
        );
        // The picker opens in its own window; a restart-based switch is handled there.
        if ui
            .button(ui_text(ids::SETTINGS_BUTTON_SWITCH_PROFILE))
            .clicked()
        {
            app.open_profile_picker();
        }
    }
    ui.label(
        RichText::new(ui_text(ids::SETTINGS_HELP_CHOOSE_CATEGORY))
            .size(14.0)
            .color(colors.muted),
    );
    ui.add_space(18.0);
    let reveal = std::mem::take(&mut state.reveal_focus);
    let focus = state.focus;
    let width = ui.available_width().min(560.0);
    // One grouped list: rows share a rounded card and are split by hairlines.
    egui::Frame::new()
        .fill(colors.panel)
        .corner_radius(egui::CornerRadius::same(14))
        .show(ui, |ui| {
            ui.set_width(width);
            ui.spacing_mut().item_spacing.y = 0.0;
            let count = SettingsSection::ALL.len();
            for (index, section) in SettingsSection::ALL.into_iter().enumerate() {
                let response = section_row(ui, section, width, colors);
                if reveal && index == focus {
                    response.request_focus();
                }
                if response.clicked() {
                    *state = SettingsPageState {
                        open: Some(section),
                        focus: index,
                        reveal_focus: true,
                        series_memory: false,
                    };
                }
                if index + 1 < count {
                    let y = response.rect.bottom();
                    ui.painter().hline(
                        (response.rect.left() + 18.0)..=response.rect.right(),
                        y,
                        egui::Stroke::new(1.0, colors.divider),
                    );
                }
            }
        });
}

/// A settings list row: the section name, a line describing it, and a chevron. The whole row is
/// one button for the mouse, keyboard, screen readers and the gamepad.
fn section_row(
    ui: &mut Ui,
    section: SettingsSection,
    width: f32,
    colors: Palette,
) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(width, 62.0), egui::Sense::click());
    let painter = ui.painter_at(rect);
    let first = section == SettingsSection::ALL[0];
    let last = section == SettingsSection::ALL[SettingsSection::ALL.len() - 1];
    let corners = egui::CornerRadius {
        nw: if first { 14 } else { 0 },
        ne: if first { 14 } else { 0 },
        sw: if last { 14 } else { 0 },
        se: if last { 14 } else { 0 },
    };
    if response.is_pointer_button_down_on() {
        painter.rect_filled(rect, corners, egui::Color32::from_white_alpha(22));
    } else if response.hovered() {
        painter.rect_filled(rect, corners, egui::Color32::from_white_alpha(12));
    }
    if response.has_focus() {
        painter.rect_stroke(
            rect.shrink(1.5),
            corners,
            egui::Stroke::new(2.5, colors.focus),
            egui::StrokeKind::Inside,
        );
    }
    let left = rect.left() + 18.0;
    painter.text(
        egui::pos2(left, rect.top() + 13.0),
        egui::Align2::LEFT_TOP,
        section.label(),
        egui::FontId::proportional(16.0),
        colors.text,
    );
    painter.text(
        egui::pos2(left, rect.top() + 35.0),
        egui::Align2::LEFT_TOP,
        section.description(),
        egui::FontId::proportional(12.5),
        colors.muted,
    );
    let chevron = egui::pos2(rect.right() - 22.0, rect.center().y);
    let stroke = egui::Stroke::new(1.8, colors.muted);
    painter.line_segment(
        [
            chevron + egui::vec2(-3.0, -6.0),
            chevron + egui::vec2(3.0, 0.0),
        ],
        stroke,
    );
    painter.line_segment(
        [
            chevron + egui::vec2(3.0, 0.0),
            chevron + egui::vec2(-3.0, 6.0),
        ],
        stroke,
    );
    let label = section.label();
    response.widget_info(move || egui::WidgetInfo::labeled(egui::Role::Button, true, label));
    response
}

fn section_page(
    ui: &mut Ui,
    app: &mut KattriApp,
    state: &mut SettingsPageState,
    colors: Palette,
    section: SettingsSection,
) {
    ui.set_max_width(ui.available_width().min(760.0));
    let reveal = std::mem::take(&mut state.reveal_focus);
    let back_label = if state.series_memory {
        ui_text(ids::SETTINGS_BUTTON_BACK_TO_PLAYBACK)
    } else {
        ui_text(ids::SETTINGS_BUTTON_BACK_TO_SETTINGS)
    };
    let back = ui.button(back_label);
    if reveal {
        back.request_focus();
    }
    if back.clicked() {
        *state = page_back(*state);
    }
    ui.add_space(14.0);
    if state.series_memory {
        heading(ui, ui_text(ids::SETTINGS_MEMORY_HEADING), colors);
        ui.add_space(8.0);
        series_memory::series_memory_page(ui, app, colors);
        return;
    }
    heading(ui, section.label(), colors);
    ui.add_space(8.0);
    match section {
        SettingsSection::General => general_page(ui, app, colors),
        SettingsSection::Server => server_page(ui, app, colors),
        SettingsSection::Playback => {
            settings_media::playback_page(ui, app, colors);
            ui.add_space(18.0);
            if ui.button(ui_text(ids::SETTINGS_MEMORY_HEADING)).clicked() {
                state.series_memory = true;
                state.reveal_focus = true;
            }
        }
        SettingsSection::Audio => settings_media::audio_page(ui, app, colors),
        SettingsSection::Subtitles => {
            settings_media::subtitles_page(ui, app, colors);
            subtitle_style_page(ui, app, colors);
        }
        SettingsSection::Video => {
            settings_media::video_page(ui, app, colors);
            video_preferences_page(ui, app, colors);
        }
        SettingsSection::Input => settings_input::input_page(ui, app, colors),
        SettingsSection::About => about_page(ui, colors),
    }
}

/// Saves the page's changes and shows a notice when the save failed.
pub(super) fn finish_page(ui: &mut Ui, app: &mut KattriApp, colors: Palette, before: &Settings) {
    app.commit_settings(before);
    if let Some(notice) = app.settings.notice() {
        ui.add_space(12.0);
        ui.label(RichText::new(notice).size(12.0).color(colors.text));
    }
}

/// A filled, rounded card that groups a page's controls, like the settings list.
pub(super) fn settings_card(ui: &mut Ui, add_contents: impl FnOnce(&mut Ui)) {
    egui::Frame::new()
        .fill(crate::theme::palette(false).panel)
        .corner_radius(egui::CornerRadius::same(14))
        .inner_margin(egui::Margin::symmetric(20, 16))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add_contents(ui);
        });
}

pub(super) fn subsection_heading(ui: &mut Ui, text: &str, colors: Palette) {
    ui.label(RichText::new(text).size(16.0).strong().color(colors.text));
    ui.add_space(6.0);
}

/// One labelled choice from a fixed list. The list order is the display order.
pub(super) fn choice<T: PartialEq + Copy>(
    ui: &mut Ui,
    label: &str,
    id_salt: &'static str,
    value: &mut T,
    options: &[(T, &str)],
) {
    let current = options
        .iter()
        .find(|(option, _)| *option == *value)
        .map_or(ui_text(ids::SETTINGS_VALUE_UNKNOWN), |(_, text)| *text);
    ui.horizontal(|ui| {
        let caption = ui.label(label);
        egui::ComboBox::from_id_salt(id_salt)
            .selected_text(current)
            .show_ui(ui, |ui| {
                for (option, text) in options {
                    ui.selectable_value(value, *option, *text);
                }
            })
            .response
            .labelled_by(caption.id);
    });
}

fn general_page(ui: &mut Ui, app: &mut KattriApp, colors: Palette) {
    let before = app.settings.settings().clone();
    let mut switch_profile = false;
    settings_card(ui, |ui| {
        subsection_heading(ui, ui_text(ids::SETTINGS_HEADING_DISPLAY), colors);
        let general = &mut app.settings.settings_mut().general;
        ui.checkbox(
            &mut general.high_contrast,
            ui_text(ids::SETTINGS_GENERAL_HIGH_CONTRAST),
        );
        ui.checkbox(
            &mut general.reduce_motion,
            ui_text(ids::SETTINGS_GENERAL_REDUCE_MOTION),
        );
        choice(
            ui,
            ui_text(ids::SETTINGS_GENERAL_BACKDROP),
            "general_backdrop",
            &mut general.backdrop,
            &[
                (
                    kattri_settings::BackdropPreference::Server,
                    ui_text(ids::SETTINGS_GENERAL_BACKDROP_SERVER),
                ),
                (
                    kattri_settings::BackdropPreference::Custom,
                    ui_text(ids::SETTINGS_GENERAL_BACKDROP_CUSTOM),
                ),
                (
                    kattri_settings::BackdropPreference::Off,
                    ui_text(ids::SETTINGS_GENERAL_BACKDROP_OFF),
                ),
            ],
        );
        if general.backdrop == kattri_settings::BackdropPreference::Custom {
            let mut path = general
                .custom_backdrop_path
                .as_ref()
                .map(|value| value.to_string_lossy().into_owned())
                .unwrap_or_default();
            ui.label(ui_text(ids::SETTINGS_GENERAL_BACKDROP_PATH));
            if ui
                .text_edit_singleline(&mut path)
                .on_hover_text(ui_text(ids::SETTINGS_GENERAL_BACKDROP_PATH))
                .changed()
            {
                general.custom_backdrop_path =
                    (!path.trim().is_empty()).then(|| std::path::PathBuf::from(path.trim()));
            }
            if ui
                .button(ui_text(ids::SETTINGS_GENERAL_BACKDROP_CLEAR))
                .clicked()
            {
                general.custom_backdrop_path = None;
            }
            muted(
                ui,
                colors,
                ui_text(ids::SETTINGS_GENERAL_BACKDROP_PATH_HELP),
                12.0,
            );
        }
        app.high_contrast = general.high_contrast;
        app.reduce_motion = general.reduce_motion;
        muted(ui, colors, ui_text(ids::SETTINGS_GENERAL_MOTION_HELP), 12.0);
        ui.add_space(8.0);
        choice(
            ui,
            ui_text(ids::SETTINGS_GENERAL_INTERFACE_SIZE),
            "general_interface_size",
            &mut general.interface_size,
            &[
                (
                    InterfaceSize::Standard,
                    ui_text(ids::SETTINGS_GENERAL_INTERFACE_SIZE_STANDARD),
                ),
                (
                    InterfaceSize::Large,
                    ui_text(ids::SETTINGS_GENERAL_INTERFACE_SIZE_LARGE),
                ),
                (
                    InterfaceSize::TenFoot,
                    ui_text(ids::SETTINGS_GENERAL_INTERFACE_SIZE_TEN_FOOT),
                ),
            ],
        );
        app.interface_size = general.interface_size;
        muted(
            ui,
            colors,
            ui_text(ids::SETTINGS_GENERAL_INTERFACE_SIZE_HELP),
            12.0,
        );
        ui.add_space(8.0);
        ui.label(ui_text(ids::SETTINGS_GENERAL_HOME_SECTIONS));
        for (value, label) in [
            (
                &mut general.home_continue_watching,
                ids::HOME_ROW_CONTINUE_WATCHING,
            ),
            (&mut general.home_next_up, ids::HOME_ROW_NEXT_UP),
            (&mut general.home_latest, ids::HOME_ROW_LATEST),
            (&mut general.home_my_media, ids::HOME_HEADING_MY_MEDIA),
            (&mut general.home_collections, ids::HOME_HEADING_COLLECTIONS),
        ] {
            ui.checkbox(value, ui_text(label));
        }
    });
    ui.add_space(12.0);
    settings_card(ui, |ui| {
        subsection_heading(ui, "Profiles", colors);
        if let Some(name) = app.settings.running_name() {
            muted(
                ui,
                colors,
                &ui_text_args(ids::SETTINGS_LABEL_ACTIVE_PROFILE, &[("name", name)]),
                12.0,
            );
        }
        switch_profile = ui
            .button(ui_text(ids::SETTINGS_BUTTON_SWITCH_PROFILE))
            .clicked();
    });
    ui.add_space(12.0);
    settings_card(ui, |ui| {
        subsection_heading(ui, "Window", colors);
        let window = &mut app.settings.settings_mut().window;
        ui.checkbox(&mut window.fullscreen, "Start in fullscreen");
        ui.add_enabled(
            false,
            egui::Checkbox::new(&mut window.always_on_top, "Keep window on top"),
        );
        ui.add_enabled(
            false,
            egui::Checkbox::new(
                &mut window.minimize_on_defocus,
                "Minimize when the window loses focus",
            ),
        );
        muted(
            ui,
            colors,
            "Unavailable: on-top and defocus behaviors are not connected yet.",
            12.0,
        );
        muted(
            ui,
            colors,
            "Window placement is saved automatically when supported.",
            12.0,
        );
    });
    ui.add_space(12.0);
    settings_card(ui, |ui| {
        subsection_heading(ui, "Updates", colors);
        ui.add_enabled(
            false,
            egui::Checkbox::new(
                &mut app.settings.settings_mut().updates.check_for_updates,
                "Check for updates",
            ),
        );
        muted(
            ui,
            colors,
            "Unavailable: automatic update checks are not connected yet.",
            12.0,
        );
    });
    ui.add_space(12.0);
    diagnostics_section(ui, app, colors);
    ui.add_space(12.0);
    super::update::show_install(ui, app, colors);
    ui.add_space(12.0);
    admin_section(ui, app, colors);
    ui.add_space(12.0);
    super::settings_parental::show(ui, app, colors);
    finish_page(ui, app, colors, &before);
    if switch_profile {
        app.open_profile_picker();
    }
}

/// Local crash reports (M4-21). Off by default. The hook reads this switch at startup only.
fn diagnostics_section(ui: &mut Ui, app: &mut KattriApp, colors: Palette) {
    settings_card(ui, |ui| {
        subsection_heading(ui, ui_text(ids::SETTINGS_HEADING_DIAGNOSTICS), colors);
        ui.checkbox(
            &mut app.settings.settings_mut().diagnostics.crash_reporting,
            ui_text(ids::SETTINGS_DIAGNOSTICS_CRASH_REPORTS),
        );
        muted(ui, colors, ui_text(ids::SETTINGS_DIAGNOSTICS_HELP), 12.0);
        choice(
            ui,
            ui_text(ids::SETTINGS_DIAGNOSTICS_LOG_LEVEL),
            "settings_log_level",
            &mut app.settings.settings_mut().diagnostics.log_level,
            &[
                (
                    LogDetail::Error,
                    ui_text(ids::SETTINGS_DIAGNOSTICS_LOG_ERROR),
                ),
                (LogDetail::Warn, ui_text(ids::SETTINGS_DIAGNOSTICS_LOG_WARN)),
                (LogDetail::Info, ui_text(ids::SETTINGS_DIAGNOSTICS_LOG_INFO)),
                (
                    LogDetail::Debug,
                    ui_text(ids::SETTINGS_DIAGNOSTICS_LOG_DEBUG),
                ),
            ],
        );
        muted(
            ui,
            colors,
            ui_text(ids::SETTINGS_DIAGNOSTICS_LOG_HELP),
            12.0,
        );
    });
}

/// The admin-lite dashboard gate (M4-22). Off by default. The navigation reads the switch on
/// every frame, so a change shows at once. The server still decides who may read the data.
fn admin_section(ui: &mut Ui, app: &mut KattriApp, colors: Palette) {
    settings_card(ui, |ui| {
        subsection_heading(ui, ui_text(ids::SETTINGS_HEADING_ADMIN), colors);
        ui.checkbox(
            &mut app.settings.settings_mut().admin.dashboard,
            ui_text(ids::SETTINGS_ADMIN_SHOW_DASHBOARD),
        );
        muted(ui, colors, ui_text(ids::SETTINGS_ADMIN_HELP), 12.0);
    });
}

fn server_page(ui: &mut Ui, app: &mut KattriApp, colors: Palette) {
    settings_card(ui, |ui| {
        if let Some(server) = &app.server {
            ui.label(RichText::new(&server.name).size(16.0).color(colors.text));
            muted(
                ui,
                colors,
                &ui_text_args(
                    ids::SETTINGS_LABEL_ADDRESS,
                    &[("address", &server_address_text(&app.server_url))],
                ),
                12.0,
            );
            ui.add_space(12.0);
            if ui.button(ui_text(ids::SETTINGS_BUTTON_SIGN_OUT)).clicked() {
                app.disconnect();
            }
        } else {
            muted(ui, colors, ui_text(ids::SETTINGS_VALUE_NOT_SIGNED_IN), 14.0);
            ui.add_space(12.0);
            if ui
                .button(ui_text(ids::SETTINGS_BUTTON_RETURN_TO_SIGN_IN))
                .clicked()
            {
                app.navigate_to(Page::Home);
            }
        }
        ui.add_space(18.0);
        subsection_heading(ui, ui_text(ids::SETTINGS_SERVER_TLS_HEADING), colors);
        let general = &mut app.settings.settings_mut().general;
        path_field(
            ui,
            ui_text(ids::SETTINGS_SERVER_CUSTOM_CA),
            &mut general.custom_ca_path,
        );
        path_field(
            ui,
            ui_text(ids::SETTINGS_SERVER_CLIENT_CERT),
            &mut general.client_cert_path,
        );
        path_field(
            ui,
            ui_text(ids::SETTINGS_SERVER_CLIENT_KEY),
            &mut general.client_key_path,
        );
        muted(ui, colors, ui_text(ids::SETTINGS_SERVER_TLS_HELP), 12.0);
    });
}

/// A single-line path field. An empty value stores `None`, so the default trust store applies.
fn path_field(ui: &mut Ui, label: &str, value: &mut Option<PathBuf>) {
    let mut text = value
        .as_ref()
        .map(|path| path.display().to_string())
        .unwrap_or_default();
    ui.horizontal(|ui| {
        ui.label(label);
        if ui.text_edit_singleline(&mut text).changed() {
            let trimmed = text.trim();
            *value = (!trimmed.is_empty()).then(|| PathBuf::from(trimmed));
        }
    });
}

fn about_page(ui: &mut Ui, colors: Palette) {
    settings_card(ui, |ui| {
        for (label, value) in about_rows(&AboutInfo::current()) {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(label).strong().color(colors.text));
                ui.label(RichText::new(value).color(colors.muted));
            });
        }
        ui.add_space(12.0);
        muted(ui, colors, ui_text(ids::SETTINGS_ABOUT_READ_ONLY), 12.0);
        ui.add_space(18.0);
        ui.label(
            RichText::new(ui_text(ids::SETTINGS_ABOUT_CURRENT_SCOPE))
                .size(16.0)
                .strong()
                .color(colors.text),
        );
        muted(ui, colors, scope_text(), 12.0);
    });
}

fn subtitle_style_page(ui: &mut Ui, app: &mut KattriApp, colors: Palette) {
    let before = app.settings.settings().clone();
    settings_card(ui, |ui| {
        subsection_heading(ui, "Subtitle style", colors);
        let subtitles = &mut app.settings.settings_mut().subtitles;
        optional_choice(
            ui,
            "Text color",
            "settings_subtitle_color",
            &mut subtitles.color,
            &[
                ("", "Player default"),
                ("#FFFFFF", "White"),
                ("#FFFF00", "Yellow"),
                ("#00FFFF", "Cyan"),
                ("#00FF00", "Green"),
            ],
        );
        optional_choice(
            ui,
            "Outline color",
            "settings_subtitle_outline_color",
            &mut subtitles.border_color,
            &[
                ("", "Player default"),
                ("#000000", "Black"),
                ("#FFFFFF", "White"),
                ("#202020", "Charcoal"),
            ],
        );
        optional_choice(
            ui,
            "Background color",
            "settings_subtitle_background_color",
            &mut subtitles.background_color,
            &[
                ("", "Player default"),
                ("#000000", "Black"),
                ("#202020", "Charcoal"),
                ("#FFFFFF", "White"),
            ],
        );
        ui.add_enabled(
            false,
            egui::Checkbox::new(
                &mut subtitles.resync_on_start,
                "Resync subtitles when playback starts",
            ),
        );
        muted(
            ui,
            colors,
            "Unavailable: subtitle resynchronization on start is not connected yet.",
            12.0,
        );
        muted(
            ui,
            colors,
            "Subtitle offset can be adjusted from the player controls.",
            12.0,
        );
        let mut background_transparency = subtitles
            .background_transparency
            .clone()
            .unwrap_or_default();
        ui.horizontal(|ui| {
            let caption = ui.label("Background transparency");
            ui.add_enabled(
                false,
                egui::TextEdit::singleline(&mut background_transparency).desired_width(180.0),
            )
            .labelled_by(caption.id);
        });
        muted(
            ui,
            colors,
            "Unavailable: subtitle background transparency is not connected to mpv.",
            12.0,
        );
        optional_choice(
            ui,
            "Placement",
            "settings_subtitle_placement",
            &mut subtitles.placement,
            &[
                ("", "Player default"),
                ("left,bottom", "Bottom left"),
                ("center,bottom", "Bottom center"),
                ("right,bottom", "Bottom right"),
                ("left,top", "Top left"),
                ("center,top", "Top center"),
                ("right,top", "Top right"),
            ],
        );
    });
    finish_page(ui, app, colors, &before);
}

fn optional_choice(
    ui: &mut Ui,
    label: &str,
    id: &str,
    value: &mut Option<String>,
    options: &[(&str, &str)],
) {
    let mut selected = value.clone().unwrap_or_default();
    ui.horizontal(|ui| {
        let caption = ui.label(label);
        let selected_text = options
            .iter()
            .find(|(option, _)| *option == selected.as_str())
            .map_or("Custom value", |(_, name)| *name);
        egui::ComboBox::from_id_salt(id)
            .selected_text(selected_text)
            .show_ui(ui, |ui| {
                for (option, name) in options {
                    ui.selectable_value(&mut selected, (*option).to_owned(), *name);
                }
            })
            .response
            .labelled_by(caption.id);
    });
    *value = (!selected.is_empty()).then_some(selected);
}

fn video_preferences_page(ui: &mut Ui, app: &mut KattriApp, colors: Palette) {
    let before = app.settings.settings().clone();
    settings_card(ui, |ui| {
        subsection_heading(ui, "Video preferences", colors);
        let video = &mut app.settings.settings_mut().video;
        ui.add_enabled_ui(false, |ui| {
            choice(
                ui,
                "HDR preference",
                "settings_hdr_preference",
                &mut video.hdr_preference,
                &[
                    (HdrPreference::Auto, "Automatic"),
                    (HdrPreference::Dovi, "Dolby Vision"),
                    (HdrPreference::Hdr10, "HDR10"),
                ],
            );
        });
        muted(
            ui,
            colors,
            "Unavailable: HDR preferences are not connected to playback negotiation.",
            12.0,
        );
        ui.add_enabled(
            false,
            egui::Checkbox::new(&mut video.hdr_passthrough, "Allow HDR passthrough"),
        );
        muted(
            ui,
            colors,
            "Unavailable: HDR output control is not connected to playback yet.",
            12.0,
        );
        // Display switching has a backend on Windows only (M6-04).
        let switching_available = cfg!(windows);
        ui.add_enabled(
            switching_available,
            egui::Checkbox::new(
                &mut video.refresh_rate_auto_switch,
                "Switch display refresh rate automatically",
            ),
        );
        ui.add_enabled(
            switching_available && video.refresh_rate_auto_switch,
            egui::Checkbox::new(
                &mut video.refresh_rate_avoid_25_30_hz,
                "Avoid 25/30 Hz refresh rates",
            ),
        );
        muted(
            ui,
            colors,
            if switching_available {
                "Matches the display to the video's frame rate while it plays and puts the original mode back afterwards. Takes effect for the next video."
            } else {
                "Unavailable: display refresh-rate switching is only available on Windows."
            },
            12.0,
        );
        ui.add_enabled_ui(false, |ui| {
            ui.checkbox(&mut video.transcode.prefer_h265, "Prefer H.265 transcoding");
            ui.checkbox(&mut video.transcode.allow_hevc, "Allow HEVC transcoding");
            ui.checkbox(&mut video.transcode.force_hevc, "Request HEVC transcoding");
            ui.checkbox(&mut video.transcode.force_4k, "Request 4K transcoding");
            ui.checkbox(&mut video.transcode.force_av1, "Request AV1 transcoding");
            ui.checkbox(&mut video.transcode.force_hdr, "Request HDR transcoding");
            ui.checkbox(
                &mut video.transcode.force_hi10p,
                "Request Hi10P transcoding",
            );
            ui.checkbox(
                &mut video.transcode.force_dovi,
                "Request Dolby Vision transcoding",
            );
        });
        muted(
            ui,
            colors,
            "Unavailable: only the global Always transcode setting is currently applied.",
            12.0,
        );
        let screenshot = &mut video.screenshot;
        ui.horizontal(|ui| {
            let caption = ui.label("Screenshot folder (blank uses Pictures)");
            ui.text_edit_singleline(&mut screenshot.directory)
                .labelled_by(caption.id);
        });
        ui.horizontal(|ui| {
            let caption = ui.label("Screenshot filename template");
            ui.text_edit_singleline(&mut screenshot.template)
                .labelled_by(caption.id);
        });
        choice(
            ui,
            "Screenshot format",
            "settings_screenshot_format",
            &mut screenshot.format,
            &[
                (ImageFormat::Png, "PNG"),
                (ImageFormat::Jpg, "JPG"),
                (ImageFormat::Webp, "WebP"),
            ],
        );
    });
    finish_page(ui, app, colors, &before);
}

fn heading(ui: &mut Ui, text: &str, colors: Palette) {
    ui.label(RichText::new(text).size(20.0).strong().color(colors.text));
}

pub(super) fn muted(ui: &mut Ui, colors: Palette, text: &str, size: f32) {
    ui.label(RichText::new(text).size(size).color(colors.muted));
}

#[cfg(test)]
mod tests {
    use super::{
        AboutInfo, SettingsPageState, SettingsSection, about_rows, activate_gamepad_focus,
        focus_direction, list_action, move_gamepad_focus, next_focus_index, page_back,
        server_address_text,
    };
    use kattri_client_core::focus::NavAction;

    const LEN: usize = SettingsSection::ALL.len();

    #[test]
    fn gamepad_navigation_moves_egui_focus_and_select_reaches_focused_control() {
        let ctx = egui::Context::default();
        let mut actions = vec![NavAction::Right, NavAction::Select];
        move_gamepad_focus(&ctx, &mut actions);
        assert_eq!(actions, vec![NavAction::Select]);
        activate_gamepad_focus(&ctx, &mut actions);
        assert!(actions.is_empty());
        assert!(ctx.input(|input| input.events.iter().any(|event| matches!(
            event,
            egui::Event::Key {
                key: egui::Key::Enter,
                pressed: true,
                ..
            }
        ))));
    }

    fn list_at(focus: usize) -> SettingsPageState {
        SettingsPageState {
            focus,
            ..SettingsPageState::default()
        }
    }

    #[test]
    fn down_visits_every_category_in_order_then_stops_at_the_last() {
        let mut focus = 0;
        let mut visited = vec![focus];
        for _ in 0..LEN + 2 {
            focus = next_focus_index(focus, LEN, NavAction::Down);
            visited.push(focus);
        }
        assert_eq!(visited, [0, 1, 2, 3, 4, 5, 6, 7, 7, 7, 7]);
    }

    #[test]
    fn up_stops_at_the_first_category() {
        assert_eq!(next_focus_index(0, LEN, NavAction::Up), 0);
        assert_eq!(next_focus_index(3, LEN, NavAction::Up), 2);
    }

    #[test]
    fn focus_outside_the_list_is_clamped_before_moving() {
        assert_eq!(next_focus_index(42, LEN, NavAction::Up), LEN - 2);
        assert_eq!(next_focus_index(42, LEN, NavAction::Down), LEN - 1);
    }

    #[test]
    fn empty_list_keeps_focus_at_zero() {
        assert_eq!(next_focus_index(3, 0, NavAction::Down), 0);
        assert_eq!(next_focus_index(3, 0, NavAction::Up), 0);
    }

    #[test]
    fn non_movement_actions_keep_the_focus() {
        for action in [
            NavAction::Select,
            NavAction::Back,
            NavAction::Left,
            NavAction::Right,
        ] {
            assert_eq!(next_focus_index(5, LEN, action), 5);
        }
    }

    #[test]
    fn select_opens_the_focused_category() {
        let state = list_action(list_at(1), NavAction::Select);
        assert_eq!(state.open, Some(SettingsSection::Server));
        assert!(state.reveal_focus);
    }

    #[test]
    fn moving_down_seven_times_then_select_opens_about() {
        let mut state = list_at(0);
        for _ in 0..LEN - 1 {
            state = list_action(state, NavAction::Down);
        }
        let opened = list_action(state, NavAction::Select);
        assert_eq!(opened.open, Some(SettingsSection::About));
    }

    #[test]
    fn list_leaves_back_to_the_app_shell() {
        let state = list_at(4);
        assert_eq!(list_action(state, NavAction::Back), state);
        assert_eq!(list_action(state, NavAction::Menu), state);
    }

    #[test]
    fn back_from_a_category_returns_to_the_list_with_the_same_entry_focused() {
        let opened = list_action(list_at(7), NavAction::Select);
        let back = page_back(opened);
        assert_eq!(back.open, None);
        assert_eq!(back.focus, 7);
        assert!(back.reveal_focus);
    }

    #[test]
    fn back_on_the_series_sub_page_returns_to_playback_before_the_list() {
        let sub_page = SettingsPageState {
            open: Some(SettingsSection::Playback),
            focus: 2,
            reveal_focus: false,
            series_memory: true,
        };
        let back = page_back(sub_page);
        assert_eq!(back.open, Some(SettingsSection::Playback));
        assert!(!back.series_memory);
        assert_eq!(page_back(back).open, None);
    }

    #[test]
    fn each_arrow_key_moves_focus_in_its_own_direction_and_other_actions_do_not() {
        use egui::FocusDirection;
        assert_eq!(focus_direction(NavAction::Up), Some(FocusDirection::Up));
        assert_eq!(focus_direction(NavAction::Down), Some(FocusDirection::Down));
        assert_eq!(focus_direction(NavAction::Left), Some(FocusDirection::Left));
        assert_eq!(
            focus_direction(NavAction::Right),
            Some(FocusDirection::Right)
        );
        assert_eq!(focus_direction(NavAction::Select), None);
        assert_eq!(focus_direction(NavAction::Back), None);
    }

    fn about_fixture(libmpv_version: Option<&str>) -> AboutInfo {
        AboutInfo {
            version: "0.1.0".to_owned(),
            target: "x86_64-windows".to_owned(),
            libmpv_version: libmpv_version.map(str::to_owned),
            gpu_name: None,
            device_id_path: Some("C:/config/Kattri/device-id".to_owned()),
            log_folder: Some("C:/data/Kattri/logs".to_owned()),
        }
    }

    fn value(rows: &[(&'static str, String)], label: &str) -> String {
        rows.iter()
            .find(|(name, _)| *name == label)
            .map(|(_, value)| value.clone())
            .unwrap_or_default()
    }

    #[test]
    fn categories_appear_in_spec_order() {
        let labels: Vec<&str> = SettingsSection::ALL
            .iter()
            .map(|section| section.label())
            .collect();
        assert_eq!(
            labels,
            [
                "General",
                "Server",
                "Playback",
                "Audio",
                "Subtitles",
                "Video",
                "Input",
                "About",
            ]
        );
    }

    #[test]
    fn about_reports_app_version_from_the_package() {
        let rows = about_rows(&about_fixture(None));
        assert_eq!(value(&rows, "App version"), "0.1.0");
        assert_eq!(env!("CARGO_PKG_VERSION"), "0.1.0");
    }

    #[test]
    fn about_reports_missing_libmpv_without_failing() {
        let rows = about_rows(&about_fixture(None));
        assert_eq!(value(&rows, "libmpv"), "libmpv not loaded");
    }

    #[test]
    fn about_reports_loaded_libmpv_version() {
        let rows = about_rows(&about_fixture(Some("mpv 0.40.0")));
        assert_eq!(value(&rows, "libmpv"), "mpv 0.40.0");
    }

    #[test]
    fn about_rows_keep_a_fixed_order_and_show_the_device_id_path() {
        let rows = about_rows(&about_fixture(None));
        let labels: Vec<&str> = rows.iter().map(|(label, _)| *label).collect();
        assert_eq!(
            labels,
            [
                "App version",
                "Build target",
                "libmpv",
                "GPU",
                "Device ID file",
                "Log folder"
            ]
        );
        assert_eq!(value(&rows, "Device ID file"), "C:/config/Kattri/device-id");
        assert_eq!(value(&rows, "Log folder"), "C:/data/Kattri/logs");
    }

    #[test]
    fn server_page_shows_host_and_port_only() {
        assert_eq!(
            server_address_text("http://10.0.0.10:8096/"),
            "10.0.0.10:8096"
        );
    }

    #[test]
    fn server_page_never_shows_query_token_path_or_userinfo() {
        let shown = server_address_text(
            "https://user:hunter2@jf.example:8920/jellyfin/web?api_key=SECRET-TOKEN#frag",
        );
        assert_eq!(shown, "jf.example:8920");
        assert!(!shown.contains("SECRET-TOKEN"));
        assert!(!shown.contains("hunter2"));
        assert!(!shown.contains('?'));
        assert!(!shown.contains('/'));
    }

    #[test]
    fn server_page_falls_back_when_no_host_is_present() {
        assert_eq!(server_address_text("   "), "Address not available");
        assert_eq!(
            server_address_text("https:///path"),
            "Address not available"
        );
    }

    #[test]
    fn the_dashboard_switch_bound_by_the_checkbox_is_off_by_default() {
        assert!(!kattri_settings::Settings::default().admin.dashboard);
    }
}
