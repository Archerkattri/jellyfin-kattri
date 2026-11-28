//! Reusable, presentation-only Live TV channel list and 24-hour EPG grid.
//!
//! Data is supplied by the caller. Returned intents let the application decide
//! when to select a channel or begin the separately-owned tune flow.

use super::{ui_text, ui_text_args};
use egui::{self, CornerRadius, FontId, RichText, Role, Stroke, TextureHandle};
use jellyfin_api::LiveTvChannel;
use kattri_client_core::epg::{EpgProgram, EpgWindow, build_row};
use kattri_client_core::live_tv::{ChannelListState, channel_list_state, sort_channels};
use kattri_i18n::ids;
use std::collections::HashSet;

const CHANNEL_COLUMN_WIDTH: f32 = 236.0;
const SLOT_WIDTH: f32 = 104.0;
const ROW_HEIGHT: f32 = 52.0;
const LOGO_SIZE: f32 = 40.0;
const SLOT_SECONDS: i64 = 30 * 60;

/// External state needed to draw the Live TV presentation.
#[derive(Clone, Copy)]
pub struct LiveTvInput<'a> {
    pub channels: &'a [LiveTvChannel],
    /// Successfully loaded logos keyed by channel item ID. Missing entries,
    /// including failed or 404 requests, render an accessible placeholder.
    pub channel_logos: &'a std::collections::HashMap<String, TextureHandle>,
    pub favorite_channel_ids: &'a HashSet<String>,
    /// False for unavailable endpoint/setup states; those are presented as a
    /// normal setup message rather than an error.
    pub endpoint_available: bool,
    /// False until the caller has loaded program data for the current window.
    pub guide_available: bool,
    pub programs: &'a [EpgProgram],
    pub window: EpgWindow,
    pub now_unix: i64,
    pub selected_channel_id: Option<&'a str>,
    /// Optional sorted channel row to reveal before constructing virtualized rows.
    pub scroll_to_row: Option<usize>,
    /// Logical destination requested by the parent, if any.
    pub focus_target: Option<&'a LiveTvFocusTarget>,
}

/// Intent emitted by a user interaction. No network or playback work occurs here.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LiveTvAction {
    SelectChannel(String),
    TuneChannel(String),
    /// Record the program now on a channel.
    RecordProgram(String),
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum LiveTvFocusControl {
    Channel,
    Tune,
    Record,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LiveTvFocusTarget {
    /// Index in the sorted channel list rendered by `show`.
    pub row: usize,
    pub control: LiveTvFocusControl,
    /// The same intent emitted by clicking this control.
    pub action: LiveTvAction,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LiveTvFocusDirection {
    Left,
    Right,
    Up,
    Down,
}

#[derive(Clone, Debug, Default)]
pub struct LiveTvResponse {
    pub actions: Vec<LiveTvAction>,
    /// Focusable channel controls in visual order, including virtualized rows.
    pub focus_targets: Vec<LiveTvFocusTarget>,
    /// Responses for controls constructed in this frame, keyed by sorted row and control.
    pub focus_responses: Vec<(usize, LiveTvFocusControl, egui::Response)>,
    /// Logical destination requested by the parent for keyboard/gamepad focus.
    pub focused_target: Option<LiveTvFocusTarget>,
}

/// Move through focus targets without depending on egui's currently visible widgets.
/// Horizontal movement follows controls in a row; vertical movement keeps the
/// current control where possible and clamps to controls available on that row.
pub fn navigate_live_tv_focus(
    targets: &[LiveTvFocusTarget],
    current: Option<&LiveTvFocusTarget>,
    direction: LiveTvFocusDirection,
) -> Option<LiveTvFocusTarget> {
    if targets.is_empty() {
        return None;
    }
    let Some(current_index) = current.and_then(|current| {
        targets
            .iter()
            .position(|target| target.control == current.control && target.action == current.action)
    }) else {
        return match direction {
            LiveTvFocusDirection::Left | LiveTvFocusDirection::Up => targets.last().cloned(),
            LiveTvFocusDirection::Right | LiveTvFocusDirection::Down => targets.first().cloned(),
        };
    };

    let current = &targets[current_index];
    match direction {
        LiveTvFocusDirection::Left => current_index
            .checked_sub(1)
            .and_then(|index| targets.get(index))
            .cloned()
            .or_else(|| Some(current.clone())),
        LiveTvFocusDirection::Right => Some(
            targets
                .get(current_index + 1)
                .cloned()
                .unwrap_or_else(|| current.clone()),
        ),
        LiveTvFocusDirection::Up | LiveTvFocusDirection::Down => {
            let Some(destination_row) = (match direction {
                LiveTvFocusDirection::Up => current.row.checked_sub(1),
                LiveTvFocusDirection::Down => Some(current.row.saturating_add(1)),
                _ => None,
            }) else {
                return Some(current.clone());
            };
            let destination = targets
                .iter()
                .filter(|target| target.row == destination_row)
                .collect::<Vec<_>>();
            if destination.is_empty() {
                return Some(current.clone());
            }
            let desired = control_order(current.control);
            destination
                .iter()
                .min_by_key(|target| control_order(target.control).abs_diff(desired))
                .map(|target| (*target).clone())
                .or_else(|| Some(current.clone()))
        }
    }
}

/// Top-of-row scroll position for a focus target. The caller should apply this
/// to the channel-list ScrollArea before asking egui to focus its widget; this
/// makes logical traversal work even when `show_rows` has not built that row.
pub fn live_tv_focus_scroll_offset(target: &LiveTvFocusTarget, item_spacing_y: f32) -> f32 {
    target.row as f32 * (ROW_HEIGHT + item_spacing_y)
}

fn control_order(control: LiveTvFocusControl) -> usize {
    match control {
        LiveTvFocusControl::Channel => 0,
        LiveTvFocusControl::Tune => 1,
        LiveTvFocusControl::Record => 2,
    }
}

pub(crate) fn live_tv_focus_targets(
    channels: &[LiveTvChannel],
    favorite_channel_ids: &HashSet<String>,
) -> Vec<LiveTvFocusTarget> {
    let mut sorted = channels.to_vec();
    sort_channels(&mut sorted, favorite_channel_ids);
    build_focus_targets(&sorted)
}

fn build_focus_targets(channels: &[LiveTvChannel]) -> Vec<LiveTvFocusTarget> {
    let mut targets = Vec::with_capacity(channels.len() * 3);
    for (row, channel) in channels.iter().enumerate() {
        let id = channel.item.id.clone();
        targets.push(LiveTvFocusTarget {
            row,
            control: LiveTvFocusControl::Channel,
            action: LiveTvAction::SelectChannel(id.clone()),
        });
        targets.push(LiveTvFocusTarget {
            row,
            control: LiveTvFocusControl::Tune,
            action: LiveTvAction::TuneChannel(id),
        });
        if let Some(program) = channel.current_program.as_ref() {
            targets.push(LiveTvFocusTarget {
                row,
                control: LiveTvFocusControl::Record,
                action: LiveTvAction::RecordProgram(program.item.id.clone()),
            });
        }
    }
    targets
}

/// Draw channels and a fixed 24-hour guide using the caller-provided UTC model.
/// UTC remains the source for comparisons and slot boundaries; labels use OS local time.
pub fn show(ui: &mut egui::Ui, input: LiveTvInput<'_>) -> LiveTvResponse {
    let mut output = LiveTvResponse::default();
    ui.heading(ui_text(ids::LIVE_TV_HEADING_TITLE));

    let state = channel_list_state(input.channels, input.endpoint_available);
    if state == ChannelListState::NotSetUp {
        ui.add_space(8.0);
        ui.label(
            RichText::new(ui_text(ids::LIVE_TV_EMPTY_NOT_SET_UP))
                .size(18.0)
                .strong(),
        );
        ui.label(RichText::new(ui_text(ids::LIVE_TV_EMPTY_NOT_SET_UP_HINT)).weak());
        return output;
    }

    let mut channels = input.channels.to_vec();
    sort_channels(&mut channels, input.favorite_channel_ids);
    output.focus_targets = build_focus_targets(&channels);
    let resolved_focus_target = input.focus_target.and_then(|requested| {
        output
            .focus_targets
            .iter()
            .find(|target| target.control == requested.control && target.action == requested.action)
            .cloned()
    });
    // The requested row may be stale after sorting; the resolved identity wins.
    let scroll_to_row = resolved_focus_target
        .as_ref()
        .map(|target| target.row)
        .or(input.scroll_to_row);
    let row_pitch = ROW_HEIGHT + ui.spacing().item_spacing.y;
    ui.label(
        RichText::new(ui_text(ids::LIVE_TV_HEADING_CHANNELS))
            .strong()
            .size(18.0),
    );
    let mut channel_scroll = egui::ScrollArea::vertical().id_salt("live_tv_channel_list");
    if let Some(row) = scroll_to_row.filter(|row| *row < channels.len()) {
        channel_scroll = channel_scroll.vertical_scroll_offset(row as f32 * row_pitch);
    }
    channel_scroll.show_rows(ui, ROW_HEIGHT, channels.len(), |ui, row_range| {
        for row in row_range {
            if let Some(channel) = channels.get(row) {
                channel_row(
                    ui,
                    row,
                    channel,
                    input,
                    resolved_focus_target.as_ref(),
                    &mut output,
                );
            }
        }
    });

    ui.add_space(16.0);
    if !input.guide_available {
        ui.label(ui_text(ids::LIVE_TV_EMPTY_GUIDE_NOT_LOADED));
        return output;
    }
    ui.label(
        RichText::new(ui_text(ids::LIVE_TV_HEADING_GUIDE))
            .strong()
            .size(18.0),
    );
    ui.label(RichText::new(ui_text(ids::LIVE_TV_HELP_GUIDE_CELLS)).weak());
    guide_grid(ui, &channels, input);
    output
}

fn channel_row(
    ui: &mut egui::Ui,
    row: usize,
    channel: &LiveTvChannel,
    input: LiveTvInput<'_>,
    focus_target: Option<&LiveTvFocusTarget>,
    output: &mut LiveTvResponse,
) {
    let id = channel.item.id.clone();
    let selected = input.selected_channel_id == Some(id.as_str());
    let number = channel
        .number
        .as_deref()
        .or(channel.channel_number.as_deref())
        .filter(|number| !number.trim().is_empty())
        .unwrap_or("—");
    let current_title = channel
        .current_program
        .as_ref()
        .map(|program| program.item.name.as_str())
        .filter(|title| !title.trim().is_empty())
        .unwrap_or(ui_text(ids::LIVE_TV_PROGRAM_UNAVAILABLE));

    // Virtualized channel rows have a fixed height; wrapping would overlap the next row.
    ui.horizontal(|ui| {
        let logo_label = ui_text_args(
            ids::LIVE_TV_LABEL_LOGO,
            &[("name", channel.item.name.as_str())],
        );
        if let Some(texture) = input.channel_logos.get(&id) {
            let image = ui.add_sized(
                [LOGO_SIZE, LOGO_SIZE],
                egui::Image::new(texture).fit_to_exact_size(egui::vec2(LOGO_SIZE, LOGO_SIZE)),
            );
            image.widget_info(|| egui::WidgetInfo::labeled(Role::Image, true, logo_label.clone()));
        } else {
            let (rect, response) =
                ui.allocate_exact_size(egui::vec2(LOGO_SIZE, LOGO_SIZE), egui::Sense::hover());
            let painter = ui.painter_at(rect);
            painter.rect_filled(
                rect,
                CornerRadius::same(5),
                ui.visuals().widgets.inactive.bg_fill,
            );
            painter.rect_stroke(
                rect,
                CornerRadius::same(5),
                Stroke::new(1.0, ui.visuals().widgets.noninteractive.bg_stroke.color),
                egui::StrokeKind::Inside,
            );
            painter.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                ui_text(ids::LIVE_TV_LOGO_PLACEHOLDER),
                FontId::proportional(9.0),
                ui.visuals().weak_text_color(),
            );
            response.widget_info(|| {
                egui::WidgetInfo::labeled(
                    Role::Label,
                    true,
                    ui_text_args(
                        ids::LIVE_TV_LABEL_NO_LOGO,
                        &[("label", logo_label.as_str())],
                    ),
                )
            });
        }

        let label = ui_text_args(
            ids::LIVE_TV_LABEL_CHANNEL_NOW,
            &[
                ("number", number),
                ("name", channel.item.name.as_str()),
                ("current_title", current_title),
            ],
        );
        let response = ui
            .add_sized(
                [CHANNEL_COLUMN_WIDTH, ROW_HEIGHT],
                egui::Button::new(format!("{number}  {}\n{current_title}", channel.item.name))
                    .selected(selected),
            )
            .on_hover_text(&label);
        response.widget_info(|| {
            egui::WidgetInfo::selected(Role::Button, true, selected, label.clone())
        });
        output
            .focus_responses
            .push((row, LiveTvFocusControl::Channel, response.clone()));
        if focus_target.is_some_and(|target| {
            target.row == row && target.control == LiveTvFocusControl::Channel
        }) {
            output.focused_target = focus_target.cloned();
            response.request_focus();
        }
        if response.clicked() {
            output.actions.push(LiveTvAction::SelectChannel(id.clone()));
        }

        let tune_label = ui_text_args(
            ids::LIVE_TV_BUTTON_TUNE_TO,
            &[("name", channel.item.name.as_str())],
        );
        let tune = ui
            .add_sized(
                [76.0, ROW_HEIGHT],
                egui::Button::new(ui_text(ids::LIVE_TV_BUTTON_TUNE)),
            )
            .on_hover_text(&tune_label);
        tune.widget_info(|| egui::WidgetInfo::labeled(Role::Button, true, tune_label.clone()));
        output
            .focus_responses
            .push((row, LiveTvFocusControl::Tune, tune.clone()));
        if focus_target
            .is_some_and(|target| target.row == row && target.control == LiveTvFocusControl::Tune)
        {
            output.focused_target = focus_target.cloned();
            tune.request_focus();
        }
        if tune.clicked() {
            output.actions.push(LiveTvAction::TuneChannel(id));
        }
        if let Some(program) = channel.current_program.as_ref() {
            let record_label = ui_text_args(
                ids::LIVE_TV_BUTTON_RECORD_PROGRAM,
                &[("title", program.item.name.as_str())],
            );
            let record = ui
                .add_sized(
                    [76.0, ROW_HEIGHT],
                    egui::Button::new(ui_text(ids::LIVE_TV_BUTTON_RECORD)),
                )
                .on_hover_text(&record_label);
            record.widget_info(|| {
                egui::WidgetInfo::labeled(Role::Button, true, record_label.clone())
            });
            output
                .focus_responses
                .push((row, LiveTvFocusControl::Record, record.clone()));
            if focus_target.is_some_and(|target| {
                target.row == row && target.control == LiveTvFocusControl::Record
            }) {
                output.focused_target = focus_target.cloned();
                record.request_focus();
            }
            if record.clicked() {
                output
                    .actions
                    .push(LiveTvAction::RecordProgram(program.item.id.clone()));
            }
        }
    });
}

fn guide_grid(ui: &mut egui::Ui, channels: &[LiveTvChannel], input: LiveTvInput<'_>) {
    let Some(window_seconds) = input.window.end_unix.checked_sub(input.window.start_unix) else {
        ui.label(ui_text(ids::LIVE_TV_STATUS_GUIDE_UNAVAILABLE));
        return;
    };
    if window_seconds != 24 * 60 * 60 {
        ui.label(ui_text(ids::LIVE_TV_STATUS_GUIDE_UNAVAILABLE));
        return;
    }
    let slot_count = input.window.slot_count().min(48);
    egui::ScrollArea::horizontal()
        .id_salt("live_tv_guide_horizontal")
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.add_sized(
                    [CHANNEL_COLUMN_WIDTH, 30.0],
                    egui::Label::new(ui_text(ids::LIVE_TV_HEADING_CHANNEL_TIME)),
                )
                .widget_info(|| {
                    egui::WidgetInfo::labeled(
                        Role::Label,
                        true,
                        ui_text(ids::LIVE_TV_HELP_GUIDE_CHANNEL_COLUMN).to_owned(),
                    )
                });
                for slot in 0..slot_count {
                    let slot_time = input
                        .window
                        .start_unix
                        .saturating_add(i64::try_from(slot).unwrap_or(i64::MAX) * SLOT_SECONDS);
                    let mut label = format_time(slot_time);
                    if slot_time <= input.now_unix && input.now_unix < slot_time + SLOT_SECONDS {
                        label.push(' ');
                        label.push_str(ui_text(ids::LIVE_TV_LABEL_NOW));
                    }
                    let response = ui.add_sized(
                        [SLOT_WIDTH, 30.0],
                        egui::Label::new(RichText::new(label.clone()).strong()),
                    );
                    response.widget_info(|| {
                        egui::WidgetInfo::labeled(
                            Role::Label,
                            true,
                            ui_text_args(
                                ids::LIVE_TV_LABEL_UTC_COLUMN,
                                &[("label", label.as_str())],
                            ),
                        )
                    });
                }
            });

            for channel in channels {
                let row = build_row(
                    &channel.item.id,
                    input.programs,
                    input.window,
                    input.now_unix,
                );
                ui.horizontal(|ui| {
                    let number = channel
                        .number
                        .as_deref()
                        .or(channel.channel_number.as_deref())
                        .filter(|number| !number.trim().is_empty())
                        .unwrap_or(ui_text(ids::LIVE_TV_CHANNEL_UNNUMBERED));
                    ui.add_sized(
                        [CHANNEL_COLUMN_WIDTH, ROW_HEIGHT],
                        egui::Label::new(format!("{number}  {}", channel.item.name)),
                    )
                    .widget_info(|| {
                        egui::WidgetInfo::labeled(
                            Role::Label,
                            true,
                            ui_text_args(
                                ids::LIVE_TV_LABEL_GUIDE_ROW,
                                &[("name", channel.item.name.as_str()), ("number", number)],
                            ),
                        )
                    });

                    for cell in &row.cells {
                        let start = cell.slot_start_unix;
                        let end = cell.slot_end_unix;
                        let title = cell
                            .title
                            .as_deref()
                            .unwrap_or(ui_text(ids::LIVE_TV_PROGRAM_NO_INFORMATION));
                        let current = cell.current;
                        let (rect, _) = ui.allocate_exact_size(
                            egui::vec2(SLOT_WIDTH, ROW_HEIGHT),
                            egui::Sense::hover(),
                        );
                        let painter = ui.painter_at(rect);
                        let background = if current {
                            ui.visuals().selection.bg_fill
                        } else if cell.title.is_some() {
                            ui.visuals().widgets.inactive.bg_fill
                        } else {
                            ui.visuals().panel_fill
                        };
                        painter.rect_filled(rect.shrink(2.0), CornerRadius::same(5), background);
                        painter.rect_stroke(
                            rect.shrink(2.0),
                            CornerRadius::same(5),
                            Stroke::new(
                                1.0,
                                if current {
                                    ui.visuals().selection.stroke.color
                                } else {
                                    ui.visuals().widgets.noninteractive.bg_stroke.color
                                },
                            ),
                            egui::StrokeKind::Inside,
                        );
                        let mut visible_title = title.to_owned();
                        if current {
                            visible_title = ui_text_args(
                                ids::LIVE_TV_LABEL_NOW_TITLE,
                                &[("title", visible_title.as_str())],
                            );
                            if let Some(program_end) = current_program_end(
                                &channel.item.id,
                                input.programs,
                                input.now_unix,
                            ) {
                                visible_title.push_str(&format!(
                                    "\n{}",
                                    ui_text_args(
                                        ids::LIVE_TV_LABEL_ENDS,
                                        &[("time", &format_time(program_end))]
                                    )
                                ));
                            }
                        }
                        painter.text(
                            rect.shrink2(egui::vec2(6.0, 4.0)).left_top(),
                            egui::Align2::LEFT_TOP,
                            visible_title,
                            FontId::proportional(12.0),
                            ui.visuals().text_color(),
                        );
                        let end_text = if current {
                            current_program_end(&channel.item.id, input.programs, input.now_unix)
                                .map(|program_end| {
                                    ui_text_args(
                                        ids::LIVE_TV_LABEL_ENDS_LOCAL,
                                        &[("time", &format_time(program_end))],
                                    )
                                })
                                .unwrap_or_default()
                        } else {
                            String::new()
                        };
                        let accessible = ui_text_args(
                            ids::LIVE_TV_LABEL_ACCESSIBLE_TIME,
                            &[
                                ("title", title),
                                ("start", &format_time(start)),
                                ("end", &format_time(end)),
                                (
                                    "current",
                                    if current {
                                        ui_text(ids::LIVE_TV_LABEL_CURRENT)
                                    } else {
                                        ""
                                    },
                                ),
                            ],
                        );
                        let accessible = format!("{accessible}{end_text}");
                        ui.interact(
                            rect,
                            ui.make_persistent_id(("live_tv_epg_cell", &channel.item.id, start)),
                            egui::Sense::hover(),
                        )
                        .widget_info(|| {
                            egui::WidgetInfo::labeled(Role::Label, true, accessible.clone())
                        });

                        if input.now_unix >= start && input.now_unix < end {
                            let fraction = (input.now_unix - start) as f32 / SLOT_SECONDS as f32;
                            let x = rect.left() + rect.width() * fraction.clamp(0.0, 1.0);
                            painter.line_segment(
                                [egui::pos2(x, rect.top()), egui::pos2(x, rect.bottom())],
                                Stroke::new(2.0, ui.visuals().selection.stroke.color),
                            );
                        }
                    }
                });
            }
        });
}

fn current_program_end(channel_id: &str, programs: &[EpgProgram], now: i64) -> Option<i64> {
    programs.iter().find_map(|program| {
        let end = program
            .end_unix
            .unwrap_or_else(|| program.start_unix.saturating_add(SLOT_SECONDS));
        (program.channel_id == channel_id && program.start_unix <= now && now < end).then_some(end)
    })
}

fn format_time(unix_seconds: i64) -> String {
    os_local_time_label(unix_seconds)
        .unwrap_or_else(|| ui_text(ids::LIVE_TV_TIME_UNAVAILABLE).to_owned())
}

#[cfg(unix)]
fn os_local_time_label(unix_seconds: i64) -> Option<String> {
    use std::ffi::CStr;

    let timestamp = libc::time_t::try_from(unix_seconds).ok()?;
    // SAFETY: `tm` is a real `struct tm` owned here; `localtime_r` writes into it and returns
    // null on failure. The output buffer is a NUL-terminated C string on success.
    let mut broken_down: libc::tm = unsafe { std::mem::zeroed() };
    let mut output = [0 as libc::c_char; 32];
    let format = c"%H:%M %z";
    unsafe {
        if libc::localtime_r(&timestamp, &mut broken_down).is_null() {
            return None;
        }
        if libc::strftime(
            output.as_mut_ptr(),
            output.len(),
            format.as_ptr(),
            &broken_down,
        ) == 0
        {
            return None;
        }
        let formatted = CStr::from_ptr(output.as_ptr()).to_str().ok()?;
        Some(add_offset_colon(formatted))
    }
}

#[cfg(windows)]
fn os_local_time_label(unix_seconds: i64) -> Option<String> {
    use std::ffi::{CStr, c_char};

    // libc's Windows bindings omit strftime, so declare it against the real `struct tm` type.
    unsafe extern "C" {
        fn strftime(
            output: *mut c_char,
            capacity: usize,
            format: *const c_char,
            time: *const libc::tm,
        ) -> usize;
    }

    let timestamp: libc::time_t = unix_seconds;
    // SAFETY: `tm` is a real `struct tm` owned here; `localtime_s` writes into it and returns
    // non-zero on failure. The output buffer is a NUL-terminated C string on success.
    let mut broken_down: libc::tm = unsafe { std::mem::zeroed() };
    let mut output = [0 as libc::c_char; 32];
    let format = c"%H:%M %z";
    unsafe {
        if libc::localtime_s(&mut broken_down, &timestamp) != 0 {
            return None;
        }
        if strftime(
            output.as_mut_ptr(),
            output.len(),
            format.as_ptr(),
            &broken_down,
        ) == 0
        {
            return None;
        }
        let formatted = CStr::from_ptr(output.as_ptr()).to_str().ok()?;
        Some(add_offset_colon(formatted))
    }
}

#[cfg(not(any(unix, windows)))]
fn os_local_time_label(_unix_seconds: i64) -> Option<String> {
    None
}

fn add_offset_colon(formatted: &str) -> String {
    let Some((clock, offset)) = formatted.split_once(' ') else {
        return formatted.to_owned();
    };
    if offset.len() == 5
        && (offset.starts_with('+') || offset.starts_with('-'))
        && offset[1..].bytes().all(|digit| digit.is_ascii_digit())
    {
        format!("{clock} {}:{}", &offset[..3], &offset[3..])
    } else {
        formatted.to_owned()
    }
}

#[cfg(test)]
fn fixture_local_label(unix_seconds: i64, offset_seconds: i32) -> String {
    let local_seconds = unix_seconds.saturating_add(i64::from(offset_seconds));
    let seconds_in_day = local_seconds.rem_euclid(24 * 60 * 60);
    let sign = if offset_seconds < 0 { '-' } else { '+' };
    let absolute_offset = offset_seconds.unsigned_abs();
    format!(
        "{:02}:{:02} {sign}{:02}:{:02}",
        seconds_in_day / 3600,
        seconds_in_day % 3600 / 60,
        absolute_offset / 3600,
        absolute_offset % 3600 / 60,
    )
}

#[cfg(test)]
mod tests {
    use super::{
        LiveTvAction, LiveTvFocusControl, LiveTvFocusDirection, LiveTvFocusTarget, LiveTvInput,
        fixture_local_label, format_time, live_tv_focus_scroll_offset, navigate_live_tv_focus,
        show,
    };
    use egui::{FullOutput, Pos2, RawInput, Rect, Shape, TextureOptions, Vec2};
    use jellyfin_api::LiveTvChannel;
    use kattri_client_core::epg::{EPG_SLOT_SECONDS, EpgProgram, EpgWindow};
    use std::collections::HashSet;

    fn channel(id: &str, name: &str, number: Option<&str>) -> LiveTvChannel {
        LiveTvChannel {
            item: serde_json::from_value(serde_json::json!({"Id":id,"Name":name}))
                .expect("valid channel fixture"),
            number: None,
            channel_number: number.map(str::to_owned),
            channel_type: None,
            current_program: None,
        }
    }

    fn window() -> EpgWindow {
        EpgWindow::new(0, 24 * 60 * 60).expect("valid 24-hour window")
    }

    fn focus_target(row: usize, control: LiveTvFocusControl) -> LiveTvFocusTarget {
        let action = match control {
            LiveTvFocusControl::Channel => LiveTvAction::SelectChannel(format!("channel-{row}")),
            LiveTvFocusControl::Tune => LiveTvAction::TuneChannel(format!("channel-{row}")),
            LiveTvFocusControl::Record => LiveTvAction::RecordProgram(format!("program-{row}")),
        };
        LiveTvFocusTarget {
            row,
            control,
            action,
        }
    }

    #[test]
    fn focus_navigation_traverses_channel_tune_and_record_controls() {
        let targets = [
            focus_target(0, LiveTvFocusControl::Channel),
            focus_target(0, LiveTvFocusControl::Tune),
            focus_target(0, LiveTvFocusControl::Record),
            focus_target(1, LiveTvFocusControl::Channel),
            focus_target(1, LiveTvFocusControl::Tune),
        ];
        assert_eq!(
            targets[2].action,
            LiveTvAction::RecordProgram("program-0".into())
        );
        let focus = |target, direction| navigate_live_tv_focus(&targets, Some(target), direction);
        assert_eq!(
            focus(&targets[0], LiveTvFocusDirection::Right),
            Some(targets[1].clone())
        );
        assert_eq!(
            focus(&targets[1], LiveTvFocusDirection::Right),
            Some(targets[2].clone())
        );
        assert_eq!(
            focus(&targets[2], LiveTvFocusDirection::Right),
            Some(targets[3].clone())
        );
        assert_eq!(
            focus(&targets[3], LiveTvFocusDirection::Down),
            Some(targets[3].clone())
        );
        assert_eq!(
            focus(&targets[4], LiveTvFocusDirection::Up),
            Some(targets[1].clone())
        );
        assert_eq!(
            focus(&targets[2], LiveTvFocusDirection::Left),
            Some(targets[1].clone())
        );
    }

    #[test]
    fn focus_navigation_clamps_edges_and_initializes_directionally() {
        let targets = [
            focus_target(0, LiveTvFocusControl::Channel),
            focus_target(0, LiveTvFocusControl::Tune),
            focus_target(1, LiveTvFocusControl::Channel),
        ];
        assert_eq!(
            navigate_live_tv_focus(&targets, Some(&targets[0]), LiveTvFocusDirection::Left),
            Some(targets[0].clone())
        );
        assert_eq!(
            navigate_live_tv_focus(&targets, Some(&targets[2]), LiveTvFocusDirection::Down),
            Some(targets[2].clone())
        );
        assert_eq!(
            navigate_live_tv_focus(&targets, None, LiveTvFocusDirection::Right),
            Some(targets[0].clone())
        );
        assert_eq!(
            navigate_live_tv_focus(&targets, None, LiveTvFocusDirection::Left),
            Some(targets[2].clone())
        );
        assert_eq!(
            navigate_live_tv_focus(&[], None, LiveTvFocusDirection::Down),
            None
        );
    }

    #[test]
    fn focus_target_for_offscreen_row_has_scroll_offset_without_widget_construction() {
        let target = focus_target(99, LiveTvFocusControl::Tune);
        assert_eq!(live_tv_focus_scroll_offset(&target, 8.0), 99.0 * 60.0);
    }

    #[test]
    fn focus_navigation_resolves_reordered_channel_by_identity_not_old_row() {
        let targets = [
            LiveTvFocusTarget {
                row: 0,
                control: LiveTvFocusControl::Channel,
                action: LiveTvAction::SelectChannel("channel-a".into()),
            },
            LiveTvFocusTarget {
                row: 0,
                control: LiveTvFocusControl::Tune,
                action: LiveTvAction::TuneChannel("channel-a".into()),
            },
            LiveTvFocusTarget {
                row: 1,
                control: LiveTvFocusControl::Channel,
                action: LiveTvAction::SelectChannel("channel-b".into()),
            },
            LiveTvFocusTarget {
                row: 1,
                control: LiveTvFocusControl::Tune,
                action: LiveTvAction::TuneChannel("channel-b".into()),
            },
        ];
        let old_focus = LiveTvFocusTarget {
            row: 0,
            control: LiveTvFocusControl::Channel,
            action: LiveTvAction::SelectChannel("channel-b".into()),
        };

        assert_eq!(
            navigate_live_tv_focus(&targets, Some(&old_focus), LiveTvFocusDirection::Right),
            Some(targets[3].clone())
        );
    }

    #[test]
    fn stale_record_focus_does_not_alias_a_different_rows_record_control() {
        let targets = [
            focus_target(0, LiveTvFocusControl::Channel),
            focus_target(0, LiveTvFocusControl::Tune),
            LiveTvFocusTarget {
                row: 1,
                control: LiveTvFocusControl::Channel,
                action: LiveTvAction::SelectChannel("other-channel".into()),
            },
            LiveTvFocusTarget {
                row: 1,
                control: LiveTvFocusControl::Tune,
                action: LiveTvAction::TuneChannel("other-channel".into()),
            },
            LiveTvFocusTarget {
                row: 1,
                control: LiveTvFocusControl::Record,
                action: LiveTvAction::RecordProgram("other-program".into()),
            },
        ];
        let stale_record = LiveTvFocusTarget {
            row: 0,
            control: LiveTvFocusControl::Record,
            action: LiveTvAction::RecordProgram("removed-program".into()),
        };

        assert_eq!(
            navigate_live_tv_focus(&targets, Some(&stale_record), LiveTvFocusDirection::Right),
            Some(targets[0].clone())
        );
    }

    fn frame(
        ctx: &egui::Context,
        input: LiveTvInput<'_>,
        click: Option<Pos2>,
    ) -> (FullOutput, super::LiveTvResponse) {
        let mut response = super::LiveTvResponse::default();
        let events = click.map_or_else(Vec::new, |position| {
            vec![
                egui::Event::PointerMoved(position),
                egui::Event::PointerButton {
                    pos: position,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: Default::default(),
                },
                egui::Event::PointerButton {
                    pos: position,
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: Default::default(),
                },
            ]
        });
        let mut output = ctx.run_ui(
            RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(6000.0, 1200.0))),
                events,
                ..Default::default()
            },
            |ui| response = show(ui, input),
        );
        output.textures_delta.clear();
        (output, response)
    }

    fn has_text(output: &FullOutput, expected: &str) -> bool {
        fn visit(shape: &Shape, expected: &str) -> bool {
            match shape {
                Shape::Text(text) => text.galley.job.text.contains(expected),
                Shape::Vec(shapes) => shapes.iter().any(|shape| visit(shape, expected)),
                _ => false,
            }
        }
        output
            .shapes
            .iter()
            .any(|clipped| visit(&clipped.shape, expected))
    }

    fn text_count(output: &FullOutput, expected: &str) -> usize {
        fn visit(shape: &Shape, expected: &str) -> usize {
            match shape {
                Shape::Text(text) => usize::from(text.galley.job.text.contains(expected)),
                Shape::Vec(shapes) => shapes.iter().map(|shape| visit(shape, expected)).sum(),
                _ => 0,
            }
        }
        output
            .shapes
            .iter()
            .map(|clipped| visit(&clipped.shape, expected))
            .sum()
    }

    fn text_center(output: &FullOutput, expected: &str) -> Pos2 {
        output
            .shapes
            .iter()
            .find_map(|clipped| match &clipped.shape {
                Shape::Text(text) if text.galley.job.text.contains(expected) => {
                    Some(text.pos + text.galley.size() * 0.5)
                }
                _ => None,
            })
            .expect("expected UI text is laid out")
    }

    #[test]
    fn show_resolves_focus_by_channel_identity_and_requests_actual_widget_focus() {
        let channels = [
            channel("channel-a", "Alpha", Some("1")),
            channel("channel-b", "Beta", Some("2")),
        ];
        let favorites = HashSet::new();
        let programs = [];
        let context = egui::Context::default();
        let stale_row_focus = LiveTvFocusTarget {
            row: 0,
            control: LiveTvFocusControl::Tune,
            action: LiveTvAction::TuneChannel("channel-b".into()),
        };
        let (_, response) = frame(
            &context,
            LiveTvInput {
                channels: &channels,
                channel_logos: &std::collections::HashMap::new(),
                favorite_channel_ids: &favorites,
                endpoint_available: true,
                guide_available: false,
                programs: &programs,
                window: window(),
                now_unix: 0,
                selected_channel_id: None,
                scroll_to_row: Some(0),
                focus_target: Some(&stale_row_focus),
            },
            None,
        );

        assert_eq!(
            response.focused_target.as_ref().map(|target| target.row),
            Some(1)
        );
        assert_eq!(
            response
                .focused_target
                .as_ref()
                .map(|target| &target.action),
            Some(&LiveTvAction::TuneChannel("channel-b".into()))
        );
        assert!(
            response
                .focus_responses
                .iter()
                .find(|(row, control, _)| *row == 1 && *control == LiveTvFocusControl::Tune)
                .is_some_and(|(_, _, response)| response.has_focus())
        );
    }

    #[test]
    fn show_scrolls_offscreen_focus_target_into_constructed_rows() {
        let channels = (0..12)
            .map(|index| {
                channel(
                    &format!("channel-{index}"),
                    "Channel",
                    Some(&index.to_string()),
                )
            })
            .collect::<Vec<_>>();
        let favorites = HashSet::new();
        let programs = [];
        let context = egui::Context::default();
        let target = LiveTvFocusTarget {
            row: 10,
            control: LiveTvFocusControl::Tune,
            action: LiveTvAction::TuneChannel("channel-10".into()),
        };
        let mut response = super::LiveTvResponse::default();
        let mut output = context.run_ui(
            RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(640.0, 240.0))),
                ..Default::default()
            },
            |ui| {
                response = show(
                    ui,
                    LiveTvInput {
                        channels: &channels,
                        channel_logos: &std::collections::HashMap::new(),
                        favorite_channel_ids: &favorites,
                        endpoint_available: true,
                        guide_available: false,
                        programs: &programs,
                        window: window(),
                        now_unix: 0,
                        selected_channel_id: None,
                        scroll_to_row: Some(target.row),
                        focus_target: Some(&target),
                    },
                );
            },
        );
        output.textures_delta.clear();

        assert_eq!(response.focused_target, Some(target));
        assert!(
            response
                .focus_responses
                .iter()
                .any(|(row, control, widget)| *row == 10
                    && *control == LiveTvFocusControl::Tune
                    && widget.has_focus())
        );
    }

    #[test]
    fn channel_buttons_return_select_and_tune_intents_without_side_effects() {
        let channels = [channel("abc", "News", Some("7"))];
        let favorites = HashSet::new();
        let programs = [];
        let context = egui::Context::default();
        // The first channel row follows the heading and the Channels section label.
        let (mut initial, focus_response) = frame(
            &context,
            LiveTvInput {
                channels: &channels,
                channel_logos: &std::collections::HashMap::new(),
                favorite_channel_ids: &favorites,
                endpoint_available: true,
                guide_available: false,
                programs: &programs,
                window: window(),
                now_unix: 0,
                selected_channel_id: None,
                scroll_to_row: None,
                focus_target: None,
            },
            None,
        );
        assert_eq!(focus_response.focus_targets.len(), 2);
        assert_eq!(focus_response.focus_responses.len(), 2);
        assert_eq!(focus_response.focus_responses[0].0, 0);
        assert_eq!(
            focus_response.focus_responses[0].1,
            LiveTvFocusControl::Channel
        );
        assert_eq!(
            focus_response.focus_responses[1].1,
            LiveTvFocusControl::Tune
        );
        assert_eq!(focus_response.focused_target, None);
        assert_eq!(focus_response.focus_targets[0].row, 0);
        assert_eq!(
            focus_response.focus_targets[0].control,
            LiveTvFocusControl::Channel
        );
        assert_eq!(
            focus_response.focus_targets[1].control,
            LiveTvFocusControl::Tune
        );
        let select_position = text_center(&initial, "7  News");
        initial.textures_delta.clear();
        let (_, selected) = frame(
            &context,
            LiveTvInput {
                channels: &channels,
                channel_logos: &std::collections::HashMap::new(),
                favorite_channel_ids: &favorites,
                endpoint_available: true,
                guide_available: false,
                programs: &programs,
                window: window(),
                now_unix: 0,
                selected_channel_id: None,
                scroll_to_row: None,
                focus_target: None,
            },
            Some(select_position),
        );
        assert!(
            selected
                .actions
                .contains(&LiveTvAction::SelectChannel("abc".into()))
        );

        let context = egui::Context::default();
        let (mut initial, _) = frame(
            &context,
            LiveTvInput {
                channels: &channels,
                channel_logos: &std::collections::HashMap::new(),
                favorite_channel_ids: &favorites,
                endpoint_available: true,
                guide_available: false,
                programs: &programs,
                window: window(),
                now_unix: 0,
                selected_channel_id: None,
                scroll_to_row: None,
                focus_target: None,
            },
            None,
        );
        let tune_position = text_center(&initial, "Tune");
        initial.textures_delta.clear();
        let (_, tuned) = frame(
            &context,
            LiveTvInput {
                channels: &channels,
                channel_logos: &std::collections::HashMap::new(),
                favorite_channel_ids: &favorites,
                endpoint_available: true,
                guide_available: false,
                programs: &programs,
                window: window(),
                now_unix: 0,
                selected_channel_id: None,
                scroll_to_row: None,
                focus_target: None,
            },
            Some(tune_position),
        );
        assert!(
            tuned
                .actions
                .contains(&LiveTvAction::TuneChannel("abc".into()))
        );
    }

    #[test]
    fn not_set_up_and_empty_states_are_clear_and_have_no_channel_actions() {
        for (channels, endpoint_available) in [(&[][..], true), (&[][..], false)] {
            let context = egui::Context::default();
            let (output, response) = frame(
                &context,
                LiveTvInput {
                    channels,
                    channel_logos: &std::collections::HashMap::new(),
                    favorite_channel_ids: &HashSet::new(),
                    endpoint_available,
                    guide_available: false,
                    programs: &[],
                    window: window(),
                    now_unix: 0,
                    selected_channel_id: None,
                    scroll_to_row: None,
                    focus_target: None,
                },
                None,
            );
            assert!(has_text(&output, "Live TV is not set up on this server"));
            assert!(response.actions.is_empty());
        }
    }

    #[test]
    fn guide_renders_48_half_hour_slots_with_accessible_current_and_gap_labels() {
        let channels = [channel("abc", "News", None)];
        let current = EpgProgram {
            channel_id: "abc".into(),
            title: "Morning bulletin".into(),
            start_unix: 0,
            end_unix: Some(EPG_SLOT_SECONDS),
        };
        let programs = [current];
        let context = egui::Context::default();
        context.enable_accesskit();
        let (output, response) = frame(
            &context,
            LiveTvInput {
                channels: &channels,
                channel_logos: &std::collections::HashMap::new(),
                favorite_channel_ids: &HashSet::new(),
                endpoint_available: true,
                guide_available: true,
                programs: &programs,
                window: window(),
                now_unix: 60,
                selected_channel_id: None,
                scroll_to_row: None,
                focus_target: None,
            },
            None,
        );
        assert!(response.actions.is_empty());
        assert!(has_text(&output, "Guide — next 24 hours (local time)"));
        assert!(has_text(&output, &format!("{} NOW", format_time(0))));
        assert!(has_text(&output, "NOW · Morning bulletin"));
        assert!(has_text(
            &output,
            &format!("Ends {}", format_time(EPG_SLOT_SECONDS))
        ));
        assert!(has_text(&output, "No information"));
        assert!(has_text(&output, &format_time(23 * 60 * 60 + 30 * 60)));
        assert_eq!(text_count(&output, "No information"), 47);
        assert_eq!(window().slot_count(), 48);
        let accessibility = output
            .platform_output
            .accesskit_update
            .expect("headless frame should produce an AccessKit tree update");
        let accessibility_debug = format!("{accessibility:?}");
        assert!(accessibility_debug.contains("Morning bulletin"));
        assert!(accessibility_debug.contains("No information"));
    }

    #[test]
    fn os_local_time_format_includes_a_colonized_utc_offset() {
        let label = format_time(0);
        let (clock, offset) = label.split_once(' ').expect("clock and offset are shown");
        assert_eq!(clock.len(), 5);
        assert_eq!(&clock[2..3], ":");
        assert_eq!(offset.len(), 6);
        assert!(offset.starts_with('+') || offset.starts_with('-'));
        assert_eq!(&offset[3..4], ":");
        assert!(offset[1..3].bytes().all(|digit| digit.is_ascii_digit()));
        assert!(offset[4..].bytes().all(|digit| digit.is_ascii_digit()));
    }

    #[test]
    fn explicit_new_york_dst_fixture_disambiguates_repeated_wall_clock_times() {
        // America/New_York transitions in 2024: EDT -> EST at 06:00 UTC.
        const FALL_BACK_UTC: i64 = 1_730_613_600;
        let offsets = [(FALL_BACK_UTC, -5 * 60 * 60)];
        let offset_at = |instant: i64| {
            offsets
                .iter()
                .rev()
                .find(|(transition, _)| instant >= *transition)
                .map_or(-4 * 60 * 60, |(_, offset)| *offset)
        };

        let first_0130_utc = 1_730_611_800;
        let second_0130_utc = 1_730_615_400;
        assert_eq!(
            fixture_local_label(first_0130_utc, offset_at(first_0130_utc)),
            "01:30 -04:00"
        );
        assert_eq!(
            fixture_local_label(second_0130_utc, offset_at(second_0130_utc)),
            "01:30 -05:00"
        );
    }

    #[test]
    fn explicit_new_york_spring_fixture_skips_the_nonexistent_hour() {
        // America/New_York transitions in 2024: EST -> EDT at 07:00 UTC.
        const SPRING_FORWARD_UTC: i64 = 1_710_054_000;
        let offsets = [(SPRING_FORWARD_UTC, -4 * 60 * 60)];
        let offset_at = |instant: i64| {
            offsets
                .iter()
                .rev()
                .find(|(transition, _)| instant >= *transition)
                .map_or(-5 * 60 * 60, |(_, offset)| *offset)
        };

        let before_transition_utc = 1_710_053_820;
        let after_transition_utc = 1_710_054_180;
        assert_eq!(
            fixture_local_label(before_transition_utc, offset_at(before_transition_utc)),
            "01:57 -05:00"
        );
        assert_eq!(
            fixture_local_label(after_transition_utc, offset_at(after_transition_utc)),
            "03:03 -04:00"
        );
    }

    #[test]
    fn channel_logo_texture_uses_bounded_row_thumbnail_layout() {
        let channels = [channel("abc", "News", Some("7"))];
        let context = egui::Context::default();
        let texture = context.load_texture(
            "test-channel-logo",
            egui::ColorImage::filled([2, 2], egui::Color32::WHITE),
            TextureOptions::LINEAR,
        );
        let logos = std::collections::HashMap::from([(String::from("abc"), texture)]);
        context.enable_accesskit();
        let (output, _) = frame(
            &context,
            LiveTvInput {
                channels: &channels,
                channel_logos: &logos,
                favorite_channel_ids: &HashSet::new(),
                endpoint_available: true,
                guide_available: false,
                programs: &[],
                window: window(),
                now_unix: 0,
                selected_channel_id: None,
                scroll_to_row: None,
                focus_target: None,
            },
            None,
        );

        assert!(!has_text(&output, "NO\nLOGO"));
        let accessibility = output
            .platform_output
            .accesskit_update
            .expect("headless frame should produce an AccessKit tree update");
        assert!(format!("{accessibility:?}").contains("Channel logo for News"));
    }

    #[test]
    fn missing_logo_has_visible_accessible_placeholder() {
        let channels = [channel("abc", "News", Some("7"))];
        let context = egui::Context::default();
        context.enable_accesskit();
        let logos = std::collections::HashMap::new();
        let (output, _) = frame(
            &context,
            LiveTvInput {
                channels: &channels,
                channel_logos: &logos,
                favorite_channel_ids: &HashSet::new(),
                endpoint_available: true,
                guide_available: false,
                programs: &[],
                window: window(),
                now_unix: 0,
                selected_channel_id: None,
                scroll_to_row: None,
                focus_target: None,
            },
            None,
        );
        assert!(has_text(&output, "NO\nLOGO"));
        let accessibility = output
            .platform_output
            .accesskit_update
            .expect("headless frame should produce an AccessKit tree update");
        let accessibility_debug = format!("{accessibility:?}");
        assert!(accessibility_debug.contains("Channel logo for News"));
        assert!(accessibility_debug.contains("no logo available"));
    }
}
