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
}

/// Intent emitted by a user interaction. No network or playback work occurs here.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LiveTvAction {
    SelectChannel(String),
    TuneChannel(String),
    /// Record the program now on a channel.
    RecordProgram(String),
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LiveTvResponse {
    pub actions: Vec<LiveTvAction>,
}

/// Draw channels and a fixed 24-hour guide using the caller-provided UTC model.
/// UTC remains the source for comparisons and slot boundaries; labels use OS local time.
pub fn show(ui: &mut egui::Ui, input: LiveTvInput<'_>) -> LiveTvResponse {
    let mut output = LiveTvResponse::default();
    ui.heading(ui_text(ids::LIVE_TV_HEADING_TITLE));

    let state = channel_list_state(input.channels, input.endpoint_available);
    if state == ChannelListState::NotSetUp {
        ui.add_space(8.0);
        ui.label(ui_text(ids::LIVE_TV_EMPTY_NOT_SET_UP));
        return output;
    }

    let mut channels = input.channels.to_vec();
    sort_channels(&mut channels, input.favorite_channel_ids);
    ui.label(
        RichText::new(ui_text(ids::LIVE_TV_HEADING_CHANNELS))
            .strong()
            .size(18.0),
    );
    egui::ScrollArea::vertical()
        .id_salt("live_tv_channel_list")
        .show_rows(ui, ROW_HEIGHT, channels.len(), |ui, row_range| {
            for row in row_range {
                if let Some(channel) = channels.get(row) {
                    channel_row(ui, channel, input, &mut output);
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
    channel: &LiveTvChannel,
    input: LiveTvInput<'_>,
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
    use std::ffi::{CStr, c_char, c_long};

    #[repr(C, align(8))]
    struct TmStorage([u64; 32]);

    #[link(name = "c")]
    unsafe extern "C" {
        fn localtime_r(time: *const c_long, result: *mut TmStorage) -> *mut TmStorage;
        fn strftime(
            output: *mut c_char,
            capacity: usize,
            format: *const c_char,
            time: *const TmStorage,
        ) -> usize;
    }

    let timestamp = c_long::try_from(unix_seconds).ok()?;
    let mut broken_down = TmStorage([0; 32]);
    let mut output = [0 as c_char; 32];
    let format = c"%H:%M %z";
    // The oversized, aligned storage is used opaquely by the platform C runtime.
    let result = unsafe {
        if localtime_r(&timestamp, &mut broken_down).is_null() {
            return None;
        }
        strftime(
            output.as_mut_ptr(),
            output.len(),
            format.as_ptr(),
            &broken_down,
        )
    };
    if result == 0 {
        return None;
    }
    let formatted = unsafe { CStr::from_ptr(output.as_ptr()) }.to_str().ok()?;
    Some(add_offset_colon(formatted))
}

#[cfg(windows)]
fn os_local_time_label(unix_seconds: i64) -> Option<String> {
    use std::ffi::{CStr, c_char, c_int};

    #[repr(C, align(8))]
    struct TmStorage([u64; 32]);

    #[link(name = "ucrt")]
    unsafe extern "C" {
        #[link_name = "_localtime64_s"]
        fn localtime64_s(result: *mut TmStorage, time: *const i64) -> c_int;
        fn strftime(
            output: *mut c_char,
            capacity: usize,
            format: *const c_char,
            time: *const TmStorage,
        ) -> usize;
    }

    let mut broken_down = TmStorage([0; 32]);
    let mut output = [0_i8; 32];
    let format = c"%H:%M %z";
    // The oversized, aligned storage is used opaquely by the platform C runtime.
    let result = unsafe {
        if localtime64_s(&mut broken_down, &unix_seconds) != 0 {
            return None;
        }
        strftime(
            output.as_mut_ptr(),
            output.len(),
            format.as_ptr(),
            &broken_down,
        )
    };
    if result == 0 {
        return None;
    }
    let formatted = unsafe { CStr::from_ptr(output.as_ptr()) }.to_str().ok()?;
    Some(add_offset_colon(formatted))
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
    use super::{LiveTvAction, LiveTvInput, fixture_local_label, format_time, show};
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
    fn channel_buttons_return_select_and_tune_intents_without_side_effects() {
        let channels = [channel("abc", "News", Some("7"))];
        let favorites = HashSet::new();
        let programs = [];
        let context = egui::Context::default();
        // The first channel row follows the heading and the Channels section label.
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
            },
            None,
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
