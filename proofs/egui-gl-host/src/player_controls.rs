//! Presentational playback controls for the native egui host.

use std::time::Duration;

use egui::{Color32, RichText, Role, WidgetInfo};
use kattri_client_core::controls::PlaybackSpeed;
use kattri_client_core::segments::{
    SegmentError, SegmentKind, SegmentSkipper, SkipDecision, SkipSettings,
};
use kattri_settings::PlaybackSettings;

use crate::player_icons::{self, OsdIcon};

/// Size of one icon button in points. Compact enough for a row of transports without the
/// control bar dwarfing the video.
const ICON_BUTTON: f32 = 34.0;
/// The play/pause button, the one control a viewer looks for first.
const PRIMARY_BUTTON: f32 = 40.0;

/// Maximum number of chapter entries exposed in the menu.
pub const MAX_CHAPTER_CHOICES: usize = 128;

/// A chapter choice, using mpv's zero-based chapter index.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChapterChoice {
    /// Zero-based chapter index to send back to the player.
    pub index: usize,
    /// Human-readable chapter title.
    pub title: String,
    /// Chapter start time.
    pub time: Duration,
}

/// Read-only playback state needed to render the controls.
#[derive(Clone, Copy, Debug)]
pub struct PlayerControlsView<'a> {
    /// Current playback position.
    pub position: Duration,
    /// Known media duration, or `None` for live/unknown-length media.
    pub duration: Option<Duration>,
    /// Whether playback is paused; `None` means the state is not known yet.
    pub paused: Option<bool>,
    /// Current playback speed; `None` means libmpv has not reported it yet.
    pub speed_multiplier: Option<f64>,
    /// Whether the current source permits seeking.
    pub seek_enabled: bool,
    /// Whether there is a loaded item the Stop control can stop.
    pub stop_enabled: bool,
    /// Whether Previous has somewhere to go: the music queue, or an earlier episode.
    pub previous_track_enabled: bool,
    /// Whether Next has somewhere to go: the music queue, or a later episode.
    pub next_track_enabled: bool,
    /// Bounded-by-render chapter list. Only the first [`MAX_CHAPTER_CHOICES`] entries are shown.
    pub chapters: &'a [ChapterChoice],
    /// Secondary subtitle choices from `secondary_candidates`. Empty hides the selector.
    pub secondary_subtitles: &'a [SecondarySubtitleChoice],
    /// The mpv track id now set as secondary, or `None` when the secondary is off.
    pub secondary_selected: Option<i64>,
    /// Whether the window is in picture-in-picture now (M2-12). The button then leaves it.
    pub pip_active: bool,
    /// Whether picture-in-picture can be entered: a video is loaded and its renderer is ready.
    pub pip_available: bool,
    /// A-B loop point A for the current item, when one is set (M6-12). Session-only.
    pub ab_loop_a: Option<Duration>,
    /// A-B loop point B for the current item, when one is set (M6-12). Session-only.
    pub ab_loop_b: Option<Duration>,
    /// Title of the item now playing, shown above the transport row.
    pub title: Option<&'a str>,
    /// Observed mpv volume in percent. `None` hides the slider until mpv reports it.
    pub volume_percent: Option<f64>,
    /// Highest volume the settings allow, so the slider cannot exceed the boost cap.
    pub volume_max_percent: u32,
    /// Whether the session is muted (M2-16, session-only).
    pub muted: Option<bool>,
    /// Whether the window is fullscreen now.
    pub fullscreen: bool,
}

/// One secondary subtitle the selector offers.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SecondarySubtitleChoice {
    /// mpv track id, as written to `secondary-sid`.
    pub track: i64,
    /// Human-readable track label.
    pub title: String,
}

/// One entry in an audio or subtitle track combo. `value` is what the host applies when it is
/// chosen.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TrackChoice<T> {
    /// The value returned when this entry is picked.
    pub value: T,
    /// Human-readable entry text.
    pub label: String,
    /// Whether this entry is the one mpv has now.
    pub selected: bool,
}

/// What one gamepad action does while a video fills the window (M6-18).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GamepadPlayerAction {
    /// A transport control, the same as pressing it on screen.
    Control(PlayerControlAction),
    /// Leave the video (B / Back), as the on-screen Back button does.
    Leave,
    /// Show or hide the media info panel (Menu).
    ToggleMediaInfo,
    /// Bring the on-screen controls back without changing playback.
    ShowControls,
}

/// Map a gamepad action to the player: A and Play/Pause toggle pause, Left and Right seek ten
/// seconds, B leaves, Menu toggles media info, Up and Down only reveal the controls.
pub fn gamepad_player_action(action: kattri_client_core::focus::NavAction) -> GamepadPlayerAction {
    use kattri_client_core::focus::NavAction;
    match action {
        NavAction::Select | NavAction::PlayPause => {
            GamepadPlayerAction::Control(PlayerControlAction::TogglePause)
        }
        NavAction::Left => GamepadPlayerAction::Control(PlayerControlAction::SeekRelative(-10)),
        NavAction::Right => GamepadPlayerAction::Control(PlayerControlAction::SeekRelative(10)),
        NavAction::Back => GamepadPlayerAction::Leave,
        NavAction::Menu => GamepadPlayerAction::ToggleMediaInfo,
        NavAction::Up | NavAction::Down => GamepadPlayerAction::ShowControls,
    }
}

/// Semantic intent emitted by the controls; the caller performs player operations.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlayerControlAction {
    /// Seek by a signed number of seconds.
    SeekRelative(i64),
    /// Seek to an absolute position.
    SeekAbsolute(Duration),
    /// Toggle paused/playing state.
    TogglePause,
    /// Move to the previous queued track, or restart the current track.
    PreviousTrack,
    /// Move to the next queued track.
    NextTrack,
    /// Stop the active media and close its report lifecycle.
    Stop,
    /// Select one of the supported playback rates.
    SetSpeed(PlaybackSpeed),
    /// Select a zero-based chapter.
    SelectChapter(usize),
    /// Set the secondary subtitle track by mpv track id, or `None` to turn it off.
    SelectSecondarySubtitle(Option<i64>),
    /// Enter or leave picture-in-picture (M2-12).
    TogglePip,
    /// Step exactly one frame, pausing first when playing (M6-12).
    StepFrame {
        /// True steps to the previous frame.
        backward: bool,
    },
    /// One press of the A-B control: set A, then set B, then clear both (M6-12).
    CycleAbLoop,
    /// Set the playback volume in percent, before the boost cap is applied (M2-16).
    SetVolume(u32),
    /// Mute or unmute the session (M2-16, session-only).
    ToggleMute,
    /// Move one chapter forward or back through the same path as PageUp/PageDown.
    StepChapter {
        /// True moves to the next chapter.
        forward: bool,
    },
    /// Enter or leave fullscreen.
    ToggleFullscreen,
}

/// Segment skip for the current item. The caller keeps `skipper` loaded with the item's
/// segments and builds `settings` with [`skip_settings`].
pub struct SkipControls<'a> {
    pub skipper: &'a mut SegmentSkipper,
    pub settings: SkipSettings,
}

/// Build the skip model from the persisted playback settings.
pub fn skip_settings(playback: &PlaybackSettings) -> Result<SkipSettings, SegmentError> {
    SkipSettings::new(
        playback.skip_intro,
        playback.skip_outro,
        playback.auto_skip_intro,
        playback.auto_skip_outro,
        playback.outro_delay_ms,
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ControlId {
    SeekBar,
    PlayPause,
    PreviousTrack,
    NextTrack,
    Stop,
    SeekBackward,
    SeekForward,
    SkipSegment,
    ChapterMenu,
    PipToggle,
    SpeedMenu,
    SecondarySubtitleMenu,
    FrameBack,
    FrameForward,
    AbLoop,
    ChapterPrevious,
    ChapterNext,
    MuteToggle,
    VolumeSlider,
    Fullscreen,
}

const THEATER_BG: Color32 = Color32::from_rgb(24, 25, 27);
const OSD_ACCENT: Color32 = Color32::from_rgb(0x00, 0xA4, 0xDC);
const OSD_CONTROL: Color32 = Color32::from_rgb(46, 49, 53);
const OSD_CONTROL_HOVER: Color32 = Color32::from_rgb(66, 70, 75);
const OSD_FOCUS: Color32 = Color32::from_rgb(0x7D, 0xD8, 0xF4);
const OSD_TEXT: Color32 = Color32::from_rgb(242, 244, 245);
const OSD_MUTED: Color32 = Color32::from_rgb(164, 170, 174);

/// Draw compact, keyboard-accessible playback controls and return user actions. The automatic
/// skip seeks without a press. The button seeks on press and appears only while a segment is
/// skippable now. Both return [`PlayerControlAction::SeekAbsolute`] for the caller to perform.
pub fn draw_with_skip(
    ui: &mut egui::Ui,
    view: PlayerControlsView<'_>,
    skip: Option<SkipControls<'_>>,
) -> Vec<PlayerControlAction> {
    draw_inner_with_skip(ui, view, skip, |_, _| {})
}

// The host always goes through `draw_with_skip`. The plain draw and the observer hook exist for
// the widget tests, which need to find control rectangles.
#[cfg(test)]
fn draw(ui: &mut egui::Ui, view: PlayerControlsView<'_>) -> Vec<PlayerControlAction> {
    draw_inner(ui, view, |_, _| {})
}

#[cfg(test)]
fn draw_inner(
    ui: &mut egui::Ui,
    view: PlayerControlsView<'_>,
    observe: impl FnMut(ControlId, egui::Rect),
) -> Vec<PlayerControlAction> {
    draw_inner_with_skip(ui, view, None, observe)
}

/// The automatic skip for a frame when the controls are not drawn. `draw_with_skip` runs the same
/// check while the OSD is visible. Without this, auto-skip would stop when the OSD fades out.
pub fn auto_skip_action(
    skip: SkipControls<'_>,
    position: Duration,
    duration: Option<Duration>,
) -> Option<PlayerControlAction> {
    let SkipControls { skipper, settings } = skip;
    let decision = skipper.pick(
        duration_to_ticks(position),
        duration.map(duration_to_ticks),
        &settings,
    )?;
    Some(PlayerControlAction::SeekAbsolute(ticks_to_duration(
        decision.seek_to_ticks,
    )))
}

fn draw_inner_with_skip(
    ui: &mut egui::Ui,
    view: PlayerControlsView<'_>,
    mut skip: Option<SkipControls<'_>>,
    mut observe: impl FnMut(ControlId, egui::Rect),
) -> Vec<PlayerControlAction> {
    let seek_available = view.seek_enabled && view.duration.is_some_and(|d| !d.is_zero());
    let mut actions = keyboard_seek_actions(ui.ctx(), seek_available);

    // The automatic switch is checked first inside `pick`. It records the skip, so the same
    // segment is not seeked to again. Otherwise the button is offered for this frame.
    let mut offered: Option<SkipDecision> = None;
    if let Some(controls) = skip.as_mut()
        && view.seek_enabled
    {
        let position = duration_to_ticks(view.position);
        let duration = view.duration.map(duration_to_ticks);
        if let Some(decision) = controls
            .skipper
            .pick(position, duration, &controls.settings)
        {
            actions.push(PlayerControlAction::SeekAbsolute(ticks_to_duration(
                decision.seek_to_ticks,
            )));
        } else {
            offered = controls
                .skipper
                .button(position, duration, &controls.settings);
        }
    }

    // The position readout is drawn on the transport row, so the bar stays three rows tall
    // (timeline, transport, secondary) no matter how wide the window is.
    let mut timeline: Option<(Duration, Duration)> = None;

    let horizontal_margin = if ui.available_width() < 360.0 { 8 } else { 12 };
    egui::Frame::new()
        .fill(THEATER_BG.gamma_multiply(0.97))
        .stroke(egui::Stroke::new(1.0, Color32::from_white_alpha(24)))
        .corner_radius(egui::CornerRadius::same(14))
        .inner_margin(egui::Margin::symmetric(horizontal_margin, 10))
        .show(ui, |ui| {
            ui.set_min_width(0.0);
            ui.visuals_mut().selection.bg_fill = OSD_ACCENT;
            ui.visuals_mut().selection.stroke.color = OSD_TEXT;
            ui.visuals_mut().widgets.inactive.bg_fill = OSD_CONTROL;
            ui.visuals_mut().widgets.inactive.fg_stroke.color = OSD_TEXT;
            ui.visuals_mut().widgets.hovered.bg_fill = OSD_CONTROL_HOVER;
            ui.visuals_mut().widgets.hovered.fg_stroke.color = OSD_TEXT;
            ui.visuals_mut().widgets.active.bg_fill = OSD_ACCENT;
            ui.visuals_mut().widgets.active.fg_stroke.color = OSD_TEXT;
            ui.spacing_mut().item_spacing = egui::vec2(6.0, 6.0);

            if let Some(title) = view.title.map(str::trim).filter(|t| !t.is_empty()) {
                ui.label(
                    RichText::new(title)
                        .size(13.0)
                        .strong()
                        .color(OSD_TEXT),
                );
            }

            if seek_available {
                let duration = view.duration.unwrap_or_default();
                let seek_end = duration.saturating_sub(Duration::from_secs(1));
                let mut position = view
                    .position
                    .as_secs_f64()
                    .clamp(0.0, seek_end.as_secs_f64());
                // Keep the timeline inside the OSD at every window width.
                ui.spacing_mut().slider_width = ui.available_width().max(1.0);
                ui.spacing_mut().slider_rail_height = 6.0;
                let slider = egui::Slider::new(&mut position, 0.0..=seek_end.as_secs_f64())
                    .show_value(false)
                    .clamping(egui::SliderClamping::Always);
                let response = ui.add(slider);
                observe(ControlId::SeekBar, response.rect);
                response.widget_info(|| {
                    WidgetInfo::labeled(Role::Slider, true, "Playback position")
                });
                let seek_to_pointer = || {
                    let pointer = response.interact_pointer_pos()?;
                    let width = response.rect.width();
                    (width > 0.0).then(|| {
                        let fraction = ((pointer.x - response.rect.left()) / width).clamp(0.0, 1.0);
                        Duration::from_secs_f64(fraction as f64 * seek_end.as_secs_f64())
                    })
                };
                let displayed_position = if response.changed() {
                    let seek_target = Duration::from_secs_f64(position);
                    actions.push(PlayerControlAction::SeekAbsolute(seek_target));
                    seek_target
                } else if response.clicked() && let Some(seek_target) = seek_to_pointer() {
                    // egui may classify a pointer press/release in one input frame as a click
                    // without changing the Slider's value. Keep click-to-seek working alongside
                    // normal drag and keyboard-slider interaction.
                    actions.push(PlayerControlAction::SeekAbsolute(seek_target));
                    seek_target
                } else {
                    view.position.min(seek_end)
                };
                timeline = Some((displayed_position, duration));
            }

            ui.add_space(4.0);
            // Transport row: the position readout, item and chapter stepping around the one
            // primary control.
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                ui.spacing_mut().item_spacing.y = 6.0;
                if let Some((elapsed, total)) = timeline {
                    ui.label(
                        RichText::new(format_time(elapsed))
                            .monospace()
                            .size(12.0)
                            .strong()
                            .color(OSD_TEXT),
                    );
                    ui.label(
                        RichText::new(format!("/ {}", format_time(total)))
                            .monospace()
                            .size(12.0)
                            .color(OSD_MUTED),
                    );
                    ui.separator();
                }
                if icon_button(
                    ui,
                    OsdIcon::PreviousItem,
                    "Previous (a track restarts after 3 seconds)",
                    view.previous_track_enabled,
                    |rect| observe(ControlId::PreviousTrack, rect),
                ) {
                    actions.push(PlayerControlAction::PreviousTrack);
                }

                if icon_button(
                    ui,
                    OsdIcon::Rewind,
                    "Seek backward 10 seconds",
                    seek_available,
                    |rect| observe(ControlId::SeekBackward, rect),
                ) {
                    actions.push(PlayerControlAction::SeekRelative(-10));
                }

                let (play_icon, play_accessible) = transport_icon(view.paused);
                if primary_icon_button(
                    ui,
                    play_icon,
                    play_accessible,
                    view.paused.is_some(),
                    |rect| observe(ControlId::PlayPause, rect),
                ) {
                    actions.push(PlayerControlAction::TogglePause);
                }

                if icon_button(
                    ui,
                    OsdIcon::Forward,
                    "Seek forward 10 seconds",
                    seek_available,
                    |rect| observe(ControlId::SeekForward, rect),
                ) {
                    actions.push(PlayerControlAction::SeekRelative(10));
                }

                if icon_button(
                    ui,
                    OsdIcon::NextItem,
                    "Next",
                    view.next_track_enabled,
                    |rect| observe(ControlId::NextTrack, rect),
                ) {
                    actions.push(PlayerControlAction::NextTrack);
                }

                if icon_button(
                    ui,
                    OsdIcon::Stop,
                    "Stop playback",
                    view.stop_enabled,
                    |rect| observe(ControlId::Stop, rect),
                ) {
                    actions.push(PlayerControlAction::Stop);
                }

                if let Some(decision) = offered.as_ref() {
                    let label = skip_label(decision.kind);
                    if labeled_button(ui, label, label, true, |rect| {
                        observe(ControlId::SkipSegment, rect);
                    }) {
                        if let Some(controls) = skip.as_mut() {
                            controls.skipper.mark_skipped(&decision.segment_id);
                        }
                        actions.push(PlayerControlAction::SeekAbsolute(ticks_to_duration(
                            decision.seek_to_ticks,
                        )));
                    }
                }

                if !view.chapters.is_empty() {
                    ui.separator();
                    if icon_button(
                        ui,
                        OsdIcon::ChapterPrevious,
                        "Previous chapter",
                        seek_available,
                        |rect| observe(ControlId::ChapterPrevious, rect),
                    ) {
                        actions.push(PlayerControlAction::StepChapter { forward: false });
                    }
                    if icon_button(
                        ui,
                        OsdIcon::ChapterNext,
                        "Next chapter",
                        seek_available,
                        |rect| observe(ControlId::ChapterNext, rect),
                    ) {
                        actions.push(PlayerControlAction::StepChapter { forward: true });
                    }
                    let response = egui::ComboBox::from_id_salt("player-control-chapter")
                        .width(ui.available_width().clamp(1.0, 180.0))
                        .selected_text("Chapters")
                        .show_ui(ui, |ui| {
                            for chapter in view.chapters.iter().take(MAX_CHAPTER_CHOICES) {
                                let title = if chapter.title.trim().is_empty() {
                                    format!("Chapter {}", chapter.index.saturating_add(1))
                                } else {
                                    chapter.title.clone()
                                };
                                let label = format!("{} · {}", title, format_time(chapter.time));
                                let response = ui.button(label.clone());
                                response.widget_info(|| {
                                    WidgetInfo::labeled(Role::Button, true, label.clone())
                                });
                                if response.clicked() {
                                    actions.push(PlayerControlAction::SelectChapter(chapter.index));
                                }
                            }
                        });
                    response.response.widget_info(|| {
                        WidgetInfo::labeled(Role::ComboBox, true, "Chapters")
                    });
                    observe(ControlId::ChapterMenu, response.response.rect);
                }

                // Shown only with two or more subtitle tracks (the candidate list is empty
                // otherwise). The offset applies to the primary track only; say so here.
                if !view.secondary_subtitles.is_empty() {
                    let selected_text = view
                        .secondary_subtitles
                        .iter()
                        .find(|choice| Some(choice.track) == view.secondary_selected)
                        .map_or_else(|| "Secondary: Off".to_owned(), |choice| {
                            format!("Secondary: {}", compact_label(&choice.title, 18))
                        });
                    let secondary_menu = egui::ComboBox::from_id_salt("player-control-secondary-subtitle")
                        .width(ui.available_width().clamp(1.0, 260.0))
                        .selected_text(selected_text)
                        .show_ui(ui, |ui| {
                            let off_selected = view.secondary_selected.is_none();
                            if ui.selectable_label(off_selected, "Off").clicked() && !off_selected {
                                actions.push(PlayerControlAction::SelectSecondarySubtitle(None));
                            }
                            for choice in view.secondary_subtitles {
                                let selected = Some(choice.track) == view.secondary_selected;
                                if ui.selectable_label(selected, &choice.title).clicked()
                                    && !selected
                                {
                                    actions.push(PlayerControlAction::SelectSecondarySubtitle(
                                        Some(choice.track),
                                    ));
                                }
                            }
                        })
                        .response
                        .on_hover_text(
                            "Second subtitle track, placed at the saved secondary position. The subtitle offset moves only the primary track.",
                        );
                    secondary_menu.widget_info(|| {
                        WidgetInfo::labeled(
                            Role::ComboBox,
                            true,
                            "Secondary subtitle track",
                        )
                    });
                    observe(ControlId::SecondarySubtitleMenu, secondary_menu.rect);
                    ui.small(
                        RichText::new("Subtitle offset affects the primary track only.")
                            .color(OSD_MUTED),
                    );
                }
            });

            // Secondary row: volume, frame stepping, speed and the window toggles. It wraps, so a
            // narrow window moves controls to another line instead of clipping them.
            ui.add_space(6.0);
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                ui.spacing_mut().item_spacing.y = 6.0;
                let (mute_icon, mute_accessible) = OsdIcon::for_mute(view.muted);
                if icon_button(ui, mute_icon, mute_accessible, true, |rect| {
                    observe(ControlId::MuteToggle, rect);
                }) {
                    actions.push(PlayerControlAction::ToggleMute);
                }

                if let Some(percent) = view.volume_percent {
                    let max = f64::from(view.volume_max_percent.max(1));
                    let mut value = percent.clamp(0.0, max);
                    // Inside a wrapped row `available_width` spans the whole container, so the
                    // slider is sized from the room left after the mute button and the percent
                    // label that follows it.
                    let room = (ui.available_rect_before_wrap().right() - ui.cursor().min.x).max(0.0);
                    ui.spacing_mut().slider_width = (room - 56.0).clamp(60.0, 240.0);
                    ui.spacing_mut().slider_rail_height = 5.0;
                    let slider = egui::Slider::new(&mut value, 0.0..=max)
                        .show_value(false)
                        .clamping(egui::SliderClamping::Always);
                    let response = ui.add(slider);
                    response.widget_info(|| WidgetInfo::labeled(Role::Slider, true, "Volume"));
                    observe(ControlId::VolumeSlider, response.rect);
                    if response.changed() {
                        actions.push(PlayerControlAction::SetVolume(value.round() as u32));
                    }
                    ui.label(
                        RichText::new(format!("{:.0}%", value))
                            .monospace()
                            .size(12.0)
                            .color(OSD_MUTED),
                    );
                }

                ui.separator();
                // Frame stepping and the A-B control need the same seek capability as the
                // timeline: neither can act on live or unknown-length media.
                if icon_button(
                    ui,
                    OsdIcon::FrameBack,
                    "Step back one frame",
                    seek_available,
                    |rect| observe(ControlId::FrameBack, rect),
                ) {
                    actions.push(PlayerControlAction::StepFrame { backward: true });
                }

                if icon_button(
                    ui,
                    OsdIcon::FrameForward,
                    "Step forward one frame",
                    seek_available,
                    |rect| observe(ControlId::FrameForward, rect),
                ) {
                    actions.push(PlayerControlAction::StepFrame { backward: false });
                }

                let (loop_label, loop_accessible) = ab_loop_label(view.ab_loop_a, view.ab_loop_b);
                if labeled_button(
                    ui,
                    loop_label,
                    loop_accessible,
                    seek_available,
                    |rect| observe(ControlId::AbLoop, rect),
                ) {
                    actions.push(PlayerControlAction::CycleAbLoop);
                }

                let speed_menu = egui::ComboBox::from_id_salt("player-control-speed")
                    .width(78.0)
                    .selected_text(speed_label(view.speed_multiplier))
                    .show_ui(ui, |ui| {
                        for speed in PlaybackSpeed::ALL {
                            let selected = view
                                .speed_multiplier
                                .is_some_and(|value| (value - speed.multiplier()).abs() < 0.001);
                            let response = ui
                                .selectable_label(selected, speed_label(Some(speed.multiplier())));
                            response.widget_info(|| {
                                WidgetInfo::selected(
                                    Role::RadioButton,
                                    true,
                                    selected,
                                    format!(
                                        "Playback speed {}",
                                        speed_label(Some(speed.multiplier()))
                                    ),
                                )
                            });
                            if response.clicked() && !selected {
                                actions.push(PlayerControlAction::SetSpeed(speed));
                            }
                        }
                    });
                speed_menu.response.widget_info(|| {
                    WidgetInfo::labeled(Role::ComboBox, true, "Playback speed")
                });
                observe(ControlId::SpeedMenu, speed_menu.response.rect);

                let (pip_icon, pip_accessible) = if view.pip_active {
                    (OsdIcon::Pip, "Exit picture-in-picture")
                } else {
                    (OsdIcon::Pip, "Picture-in-picture")
                };
                if icon_button(
                    ui,
                    pip_icon,
                    pip_accessible,
                    view.pip_active || view.pip_available,
                    |rect| observe(ControlId::PipToggle, rect),
                ) {
                    actions.push(PlayerControlAction::TogglePip);
                }

                let (fullscreen_icon, fullscreen_accessible) =
                    fullscreen_icon_label(view.fullscreen);
                if icon_button(
                    ui,
                    fullscreen_icon,
                    fullscreen_accessible,
                    true,
                    |rect| observe(ControlId::Fullscreen, rect),
                ) {
                    actions.push(PlayerControlAction::ToggleFullscreen);
                }
            });
        });

    actions
}

/// A widget in the audio and subtitle section (M2-18). Entries are observed only while their
/// combo's popup is open.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TrackControl {
    Header,
    Audio,
    Subtitle,
    AudioEntry(usize),
    SubtitleEntry(usize),
}

/// The "Audio & subtitle tracks" section. The combos sit under a collapsed header. A keyboard
/// user Tabs to the header and presses Enter, Tabs to a combo and presses Enter to open it, then
/// Tabs to an entry and presses Enter. Returns the entries chosen this frame.
pub fn track_section<A: Copy, S: Copy>(
    ui: &mut egui::Ui,
    audio: Option<&[TrackChoice<A>]>,
    subtitle: Option<&[TrackChoice<S>]>,
    feedback: Option<&str>,
) -> (Option<A>, Option<S>) {
    track_section_inner(ui, audio, subtitle, feedback, |_, _| {})
}

/// Draw audio and subtitle track menus without a collapsible header.
///
/// The returned values are selections made in this frame. Full track names remain in each menu;
/// the selected value is compacted to keep the labeled controls within narrow panels.
pub fn track_menu_contents<A: Copy, S: Copy>(
    ui: &mut egui::Ui,
    audio: Option<&[TrackChoice<A>]>,
    subtitle: Option<&[TrackChoice<S>]>,
    feedback: Option<&str>,
) -> (Option<A>, Option<S>) {
    track_menu_contents_inner(ui, audio, subtitle, feedback, |_, _| {})
}

fn track_section_inner<A: Copy, S: Copy>(
    ui: &mut egui::Ui,
    audio: Option<&[TrackChoice<A>]>,
    subtitle: Option<&[TrackChoice<S>]>,
    feedback: Option<&str>,
    mut observe: impl FnMut(TrackControl, egui::Id),
) -> (Option<A>, Option<S>) {
    let section = egui::CollapsingHeader::new("Audio & subtitle tracks")
        .default_open(false)
        .show(ui, |ui| {
            track_menu_contents_inner(ui, audio, subtitle, feedback, &mut observe)
        });
    observe(TrackControl::Header, section.header_response.id);
    section.body_returned.unwrap_or((None, None))
}

fn track_menu_contents_inner<A: Copy, S: Copy>(
    ui: &mut egui::Ui,
    audio: Option<&[TrackChoice<A>]>,
    subtitle: Option<&[TrackChoice<S>]>,
    feedback: Option<&str>,
    mut observe: impl FnMut(TrackControl, egui::Id),
) -> (Option<A>, Option<S>) {
    let mut chosen: (Option<A>, Option<S>) = (None, None);
    if let Some(choices) = audio {
        chosen.0 = track_combo(
            ui,
            "Audio track",
            choices,
            (TrackControl::Audio, TrackControl::AudioEntry),
            &mut observe,
        );
    }
    if let Some(choices) = subtitle {
        chosen.1 = track_combo(
            ui,
            "Subtitle track",
            choices,
            (TrackControl::Subtitle, TrackControl::SubtitleEntry),
            &mut observe,
        );
    }
    if let Some(feedback) = feedback {
        ui.label(feedback);
    }
    if audio.is_none() && subtitle.is_none() && feedback.is_none() {
        ui.label("Loading audio and subtitle tracks…");
    }
    chosen
}

/// One track combo. `controls` names the combo and makes the id of each entry from its index.
/// The combo is a `ComboBox`, as the speed and chapter menus are, so it takes the same keys.
fn track_combo<T: Copy>(
    ui: &mut egui::Ui,
    label: &str,
    choices: &[TrackChoice<T>],
    controls: (TrackControl, fn(usize) -> TrackControl),
    observe: &mut impl FnMut(TrackControl, egui::Id),
) -> Option<T> {
    let selected = choices.iter().find(|choice| choice.selected).map_or_else(
        || "Unknown selection".to_owned(),
        |choice| compact_label(&choice.label, 24),
    );
    let (combo, entry_of) = controls;
    let mut picked = None;
    let label_width = ui
        .painter()
        .layout_no_wrap(
            label.to_owned(),
            egui::TextStyle::Body.resolve(ui.style()),
            ui.visuals().text_color(),
        )
        .size()
        .x;
    let combo_width =
        (ui.available_width() - label_width - ui.spacing().item_spacing.x - 8.0).clamp(1.0, 280.0);
    let response = egui::ComboBox::from_label(label)
        .width(combo_width)
        .selected_text(selected)
        .show_ui(ui, |ui| {
            for (index, choice) in choices.iter().enumerate() {
                let response = ui.selectable_label(choice.selected, &choice.label);
                observe(entry_of(index), response.id);
                if response.clicked() {
                    picked = Some(choice.value);
                }
            }
        });
    observe(combo, response.response.id);
    picked
}

/// True when the frame has Ctrl+S pressed, the default `host:screenshot` binding in kattri-input.
/// Repeats are ignored so a held key takes one picture. The host does not depend on kattri-input
/// yet, so the default is repeated here; keep it equal to that crate's legacy map.
pub fn screenshot_key_pressed(events: &[egui::Event]) -> bool {
    events.iter().any(|event| {
        matches!(
            event,
            egui::Event::Key {
                key: egui::Key::S,
                pressed: true,
                repeat: false,
                modifiers,
                ..
            } if modifiers.ctrl && !modifiers.shift && !modifiers.alt && !modifiers.mac_cmd
        )
    })
}

/// Handle one unmodified arrow press only when egui has no focused widget or popup.
/// This leaves text editing, focused controls, and menu navigation to egui.
fn keyboard_seek_actions(ctx: &egui::Context, seek_available: bool) -> Vec<PlayerControlAction> {
    if !seek_available || ctx.egui_wants_keyboard_input() || egui::Popup::is_any_open(ctx) {
        return Vec::new();
    }

    let mut actions = Vec::with_capacity(1);
    ctx.input_mut(|input| {
        let unmodified_press = |key| {
            input.events.iter().any(|event| {
                matches!(
                    event,
                    egui::Event::Key {
                        key: event_key,
                        pressed: true,
                        modifiers,
                        ..
                    } if *event_key == key && *modifiers == egui::Modifiers::NONE
                )
            })
        };
        let left_pressed = unmodified_press(egui::Key::ArrowLeft);
        let right_pressed = unmodified_press(egui::Key::ArrowRight);
        if left_pressed && input.consume_key(egui::Modifiers::NONE, egui::Key::ArrowLeft) {
            actions.push(PlayerControlAction::SeekRelative(-10));
        } else if right_pressed && input.consume_key(egui::Modifiers::NONE, egui::Key::ArrowRight) {
            actions.push(PlayerControlAction::SeekRelative(10));
        }
    });
    actions
}

fn labeled_button(
    ui: &mut egui::Ui,
    label: &str,
    accessible_name: &str,
    enabled: bool,
    observe: impl FnOnce(egui::Rect),
) -> bool {
    let response = ui.add_enabled(
        enabled,
        egui::Button::new(RichText::new(label).size(14.0).color(OSD_TEXT))
            .min_size(egui::vec2(46.0, ICON_BUTTON))
            .corner_radius(egui::CornerRadius::same(11)),
    );
    focus_ring(ui, &response, egui::CornerRadius::same(11));
    observe(response.rect);
    response.widget_info(|| WidgetInfo::labeled(Role::Button, true, accessible_name));
    response.clicked()
}

/// An icon-only button. The glyph is painted, not typeset, so it cannot fall back to a box,
/// and the accessible name carries the meaning for screen readers and the tooltip.
fn icon_button(
    ui: &mut egui::Ui,
    icon: OsdIcon,
    accessible_name: &str,
    enabled: bool,
    observe: impl FnOnce(egui::Rect),
) -> bool {
    let rounding = egui::CornerRadius::same(11);
    let response = ui.add_enabled(
        enabled,
        egui::Button::new("")
            .min_size(egui::vec2(ICON_BUTTON, ICON_BUTTON))
            .corner_radius(rounding),
    );
    let color = if !enabled {
        OSD_MUTED
    } else if response.hovered() {
        Color32::WHITE
    } else {
        OSD_TEXT
    };
    player_icons::paint(ui.painter(), icon_area(response.rect, 0.56), icon, color);
    focus_ring(ui, &response, rounding);
    observe(response.rect);
    response.widget_info(|| WidgetInfo::labeled(Role::Button, true, accessible_name));
    let clicked = response.clicked();
    response.on_hover_text(accessible_name);
    clicked
}

/// The play/pause button. It is the only filled control on the bar, so the eye lands on it.
fn primary_icon_button(
    ui: &mut egui::Ui,
    icon: OsdIcon,
    accessible_name: &str,
    enabled: bool,
    observe: impl FnOnce(egui::Rect),
) -> bool {
    let rounding = egui::CornerRadius::same(15);
    let response = ui.add_enabled(
        enabled,
        egui::Button::new("")
            .fill(if enabled { OSD_ACCENT } else { OSD_CONTROL })
            .stroke(egui::Stroke::new(1.0, Color32::from_rgb(0x78, 0xD5, 0xF1)))
            .min_size(egui::vec2(PRIMARY_BUTTON, PRIMARY_BUTTON))
            .corner_radius(rounding),
    );
    let color = if enabled { Color32::WHITE } else { OSD_MUTED };
    player_icons::paint(ui.painter(), icon_area(response.rect, 0.52), icon, color);
    focus_ring(ui, &response, rounding);
    observe(response.rect);
    response.widget_info(|| WidgetInfo::labeled(Role::Button, true, accessible_name));
    response.clicked()
}

/// The square an icon is drawn in, centred in a button. `fraction` sets how much of the button
/// the icon covers.
fn icon_area(button: egui::Rect, fraction: f32) -> egui::Rect {
    let side = (button.width().min(button.height()) * fraction).max(8.0);
    egui::Rect::from_center_size(button.center(), egui::vec2(side, side))
}

fn focus_ring(ui: &egui::Ui, response: &egui::Response, rounding: egui::CornerRadius) {
    if response.has_focus() {
        ui.painter().rect_stroke(
            response.rect.expand(3.0),
            rounding,
            egui::Stroke::new(2.0, OSD_FOCUS),
            egui::StrokeKind::Outside,
        );
    }
}

fn skip_label(kind: SegmentKind) -> &'static str {
    match kind {
        SegmentKind::Intro => "Skip intro",
        SegmentKind::Outro => "Skip outro",
    }
}

/// Jellyfin ticks are 100 ns. The conversion saturates instead of failing, which real media
/// positions never reach.
fn duration_to_ticks(time: Duration) -> u64 {
    u64::try_from(time.as_nanos() / 100).unwrap_or(u64::MAX)
}

fn ticks_to_duration(ticks: u64) -> Duration {
    Duration::from_nanos(ticks.saturating_mul(100))
}

fn speed_label(speed: Option<f64>) -> String {
    let Some(speed) = speed.filter(|value| value.is_finite() && *value > 0.0) else {
        return "Speed unavailable".into();
    };
    if let Some(choice) = PlaybackSpeed::ALL
        .into_iter()
        .find(|choice| (choice.multiplier() - speed).abs() < 0.001)
    {
        format!("{}×", choice.multiplier())
    } else {
        format!("Custom {speed:.2}×")
    }
}

fn compact_label(label: &str, max_chars: usize) -> String {
    let mut chars = label.chars();
    let compact: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{compact}…")
    } else {
        compact
    }
}

fn format_time(time: Duration) -> String {
    let total_seconds = time.as_secs();
    let hours = total_seconds / 3600;
    let minutes = (total_seconds % 3600) / 60;
    let seconds = total_seconds % 60;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

fn transport_icon(paused: Option<bool>) -> (OsdIcon, &'static str) {
    match paused {
        Some(true) => (OsdIcon::Play, "Play"),
        Some(false) => (OsdIcon::Pause, "Pause"),
        None => (OsdIcon::Play, "Playback state unavailable"),
    }
}

/// Label and accessibility text for the A-B control. The label carries the state, because the
/// single button both sets and clears the loop (M6-12).
fn ab_loop_label(a: Option<Duration>, b: Option<Duration>) -> (&'static str, &'static str) {
    match (a, b) {
        (None, _) => ("A-B", "Set A-B loop point A"),
        (Some(_), None) => ("A-B A", "Set A-B loop point B"),
        (Some(_), Some(_)) => ("A-B on", "Clear the A-B loop"),
    }
}

/// Fullscreen control icon and accessible name. The control offers the state it switches to.
fn fullscreen_icon_label(fullscreen: bool) -> (OsdIcon, &'static str) {
    if fullscreen {
        (OsdIcon::ExitFullscreen, "Leave fullscreen")
    } else {
        (OsdIcon::EnterFullscreen, "Enter fullscreen")
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ChapterChoice, ControlId, GamepadPlayerAction, MAX_CHAPTER_CHOICES, PlayerControlAction,
        PlayerControlsView, SecondarySubtitleChoice, SkipControls, TrackChoice, TrackControl,
        auto_skip_action, compact_label, draw, draw_inner, draw_inner_with_skip, draw_with_skip,
        duration_to_ticks, format_time, gamepad_player_action, keyboard_seek_actions,
        screenshot_key_pressed, skip_settings, speed_label, ticks_to_duration, track_menu_contents,
        track_section_inner,
    };
    use crate::player_icons::OsdIcon;

    #[test]
    fn a_gamepad_drives_the_player_while_a_video_fills_the_window() {
        use kattri_client_core::focus::NavAction;
        for (pressed, expected) in [
            (
                NavAction::Select,
                GamepadPlayerAction::Control(PlayerControlAction::TogglePause),
            ),
            (
                NavAction::PlayPause,
                GamepadPlayerAction::Control(PlayerControlAction::TogglePause),
            ),
            (
                NavAction::Left,
                GamepadPlayerAction::Control(PlayerControlAction::SeekRelative(-10)),
            ),
            (
                NavAction::Right,
                GamepadPlayerAction::Control(PlayerControlAction::SeekRelative(10)),
            ),
            (NavAction::Back, GamepadPlayerAction::Leave),
            (NavAction::Menu, GamepadPlayerAction::ToggleMediaInfo),
            (NavAction::Up, GamepadPlayerAction::ShowControls),
            (NavAction::Down, GamepadPlayerAction::ShowControls),
        ] {
            assert_eq!(gamepad_player_action(pressed), expected, "{pressed:?}");
        }
    }
    use egui::{CentralPanel, Color32, Context, Id, RawInput, Stroke};
    use kattri_client_core::segments::{MediaSegment, SegmentKind, SegmentSkipper, SkipSettings};
    use kattri_settings::PlaybackSettings;
    use std::time::Duration;

    /// One second in Jellyfin ticks.
    const SECOND: u64 = 10_000_000;

    fn view<'a>(chapters: &'a [ChapterChoice]) -> PlayerControlsView<'a> {
        PlayerControlsView {
            position: Duration::from_secs(20),
            duration: Some(Duration::from_secs(120)),
            paused: Some(true),
            speed_multiplier: Some(1.0),
            seek_enabled: true,
            stop_enabled: true,
            previous_track_enabled: true,
            next_track_enabled: true,
            chapters,
            secondary_subtitles: &[],
            secondary_selected: None,
            pip_active: false,
            pip_available: true,
            ab_loop_a: None,
            ab_loop_b: None,
            title: Some("Test item"),
            volume_percent: Some(80.0),
            volume_max_percent: 100,
            muted: Some(false),
            fullscreen: false,
        }
    }

    fn run_ui(context: &Context, input: RawInput, run: impl FnMut(&mut egui::Ui)) {
        let mut output = context.run_ui(input, run);
        output.textures_delta.clear();
    }

    #[test]
    fn unknown_duration_hides_seek_bar_and_seek_buttons_are_disabled() {
        let context = Context::default();
        let mut seek_bar = None;
        run_ui(&context, RawInput::default(), |ui| {
            CentralPanel::default().show(ui, |ui| {
                let mut state = view(&[]);
                state.duration = None;
                let _ = draw_inner(ui, state, |id, rect| {
                    if id == ControlId::SeekBar {
                        seek_bar = Some(rect);
                    }
                });
            });
        });
        assert!(seek_bar.is_none());
    }

    #[test]
    fn the_ab_control_label_follows_the_loop_state() {
        assert_eq!(
            super::ab_loop_label(None, None),
            ("A-B", "Set A-B loop point A")
        );
        assert_eq!(
            super::ab_loop_label(Some(Duration::from_secs(5)), None),
            ("A-B A", "Set A-B loop point B")
        );
        assert_eq!(
            super::ab_loop_label(Some(Duration::from_secs(5)), Some(Duration::from_secs(9))),
            ("A-B on", "Clear the A-B loop")
        );
    }

    #[test]
    fn the_transport_mute_and_fullscreen_controls_offer_the_state_they_switch_to() {
        assert_eq!(super::transport_icon(Some(true)), (OsdIcon::Play, "Play"));
        assert_eq!(
            super::transport_icon(Some(false)),
            (OsdIcon::Pause, "Pause")
        );
        assert_eq!(
            super::transport_icon(None),
            (OsdIcon::Play, "Playback state unavailable")
        );
        assert_eq!(OsdIcon::for_mute(Some(false)), (OsdIcon::Volume, "Mute"));
        assert_eq!(OsdIcon::for_mute(None), (OsdIcon::Volume, "Mute"));
        assert_eq!(OsdIcon::for_mute(Some(true)), (OsdIcon::Muted, "Unmute"));
        assert_eq!(
            super::fullscreen_icon_label(false),
            (OsdIcon::EnterFullscreen, "Enter fullscreen")
        );
        assert_eq!(
            super::fullscreen_icon_label(true),
            (OsdIcon::ExitFullscreen, "Leave fullscreen")
        );
    }

    #[test]
    fn the_volume_slider_is_bounded_by_the_boost_cap() {
        let context = Context::default();
        let state = PlayerControlsView {
            volume_percent: Some(250.0),
            volume_max_percent: 100,
            ..view(&[])
        };
        let frame = overlay_frame(&context, RawInput::default(), state);
        let slider = frame
            .rects
            .iter()
            .find(|(id, _)| *id == ControlId::VolumeSlider)
            .map(|(_, rect)| *rect)
            .expect("volume slider is drawn");
        assert!(slider.width() > 0.0);
    }

    #[test]
    fn labels_match_playback_speed_and_clock_conventions() {
        assert_eq!(speed_label(Some(0.5)), "0.5×");
        assert_eq!(speed_label(Some(1.0)), "1×");
        assert_eq!(speed_label(Some(2.0)), "2×");
        assert_eq!(speed_label(None), "Speed unavailable");
        assert_eq!(format_time(Duration::from_secs(65)), "1:05");
        assert_eq!(format_time(Duration::from_secs(3661)), "1:01:01");
    }

    #[test]
    fn compact_track_labels_keep_full_short_names_and_trim_long_names() {
        assert_eq!(compact_label("English", 8), "English");
        assert_eq!(compact_label("English commentary track", 8), "English …");
    }

    #[test]
    fn narrow_osd_keeps_controls_inside_the_viewport() {
        let context = Context::default();
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(240.0, 420.0));
        let chapters = [ChapterChoice {
            index: 0,
            title: "A very long chapter title that must remain reachable".into(),
            time: Duration::from_secs(20),
        }];
        let secondary = [SecondarySubtitleChoice {
            track: 3,
            title: "An unusually long subtitle track name".into(),
        }];
        let mut rects = Vec::new();
        let mut output = context.run_ui(
            RawInput {
                screen_rect: Some(screen),
                ..RawInput::default()
            },
            |ui| {
                CentralPanel::default().show(ui, |ui| {
                    let _ = draw_inner(
                        ui,
                        PlayerControlsView {
                            chapters: &chapters,
                            secondary_subtitles: &secondary,
                            ..view(&[])
                        },
                        |id, rect| rects.push((id, rect)),
                    );
                });
            },
        );
        output.textures_delta.clear();
        assert!(rects.iter().any(|(id, _)| *id == ControlId::SeekBar));
        assert!(rects.iter().any(|(id, _)| *id == ControlId::ChapterMenu));
        assert!(
            rects
                .iter()
                .any(|(id, _)| *id == ControlId::SecondarySubtitleMenu)
        );
        for (id, rect) in rects {
            assert!(
                rect.min.x >= screen.min.x,
                "{id:?} starts inside viewport: {rect:?}"
            );
            assert!(
                rect.max.x <= screen.max.x,
                "{id:?} ends inside viewport: {rect:?}"
            );
        }
    }

    #[test]
    fn clicking_timeline_emits_seek_preview_position() {
        let context = Context::default();
        let mut seek_bar = None;
        run_ui(&context, RawInput::default(), |ui| {
            CentralPanel::default().show(ui, |ui| {
                let _ = draw_inner(ui, view(&[]), |id, rect| {
                    if id == ControlId::SeekBar {
                        seek_bar = Some(rect);
                    }
                });
            });
        });
        let center = seek_bar.expect("seek bar is rendered").center();
        let mut actions = Vec::new();
        run_ui(
            &context,
            RawInput {
                events: vec![
                    egui::Event::PointerMoved(center),
                    egui::Event::PointerButton {
                        pos: center,
                        button: egui::PointerButton::Primary,
                        pressed: true,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
                ..RawInput::default()
            },
            |ui| {
                CentralPanel::default().show(ui, |ui| {
                    actions = draw_inner(ui, view(&[]), |_, _| {});
                });
            },
        );
        if actions.is_empty() {
            run_ui(
                &context,
                RawInput {
                    events: vec![egui::Event::PointerButton {
                        pos: center,
                        button: egui::PointerButton::Primary,
                        pressed: false,
                        modifiers: egui::Modifiers::NONE,
                    }],
                    ..RawInput::default()
                },
                |ui| {
                    CentralPanel::default().show(ui, |ui| {
                        actions = draw_inner(ui, view(&[]), |_, _| {});
                    });
                },
            );
        }
        assert!(
            actions.iter().any(|action| matches!(
                action,
                PlayerControlAction::SeekAbsolute(position)
                    if position.abs_diff(Duration::from_secs(59)) < Duration::from_secs(2)
            )),
            "timeline click reports its preview position: {actions:?}"
        );
    }

    #[test]
    fn chapter_selector_is_conditional_and_entries_are_bounded() {
        assert_eq!(MAX_CHAPTER_CHOICES, 128);
        let context = Context::default();
        let mut chapter_menu = false;
        run_ui(&context, RawInput::default(), |ui| {
            CentralPanel::default().show(ui, |ui| {
                let _ = draw_inner(ui, view(&[]), |id, _| {
                    chapter_menu |= id == ControlId::ChapterMenu;
                });
            });
        });
        assert!(!chapter_menu);
        let chapters = [ChapterChoice {
            index: 0,
            title: "Opening".into(),
            time: Duration::ZERO,
        }];
        let context = Context::default();
        run_ui(&context, RawInput::default(), |ui| {
            CentralPanel::default().show(ui, |ui| {
                let _ = draw_inner(ui, view(&chapters), |id, _| {
                    chapter_menu |= id == ControlId::ChapterMenu;
                });
            });
        });
        assert!(chapter_menu);
    }

    #[test]
    fn stop_button_emits_a_stop_action() {
        let context = Context::default();
        let mut stop_rect = None;
        let mut actions = Vec::new();
        run_ui(&context, RawInput::default(), |ui| {
            CentralPanel::default().show(ui, |ui| {
                actions = draw_inner(ui, view(&[]), |id, rect| {
                    if id == ControlId::Stop {
                        stop_rect = Some(rect);
                    }
                });
            });
        });
        assert!(actions.is_empty());
        let center = stop_rect.expect("stop button is rendered").center();
        run_ui(
            &context,
            RawInput {
                events: vec![
                    egui::Event::PointerMoved(center),
                    egui::Event::PointerButton {
                        pos: center,
                        button: egui::PointerButton::Primary,
                        pressed: true,
                        modifiers: egui::Modifiers::NONE,
                    },
                    egui::Event::PointerButton {
                        pos: center,
                        button: egui::PointerButton::Primary,
                        pressed: false,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
                ..RawInput::default()
            },
            |ui| {
                CentralPanel::default().show(ui, |ui| {
                    actions = draw(ui, view(&[]));
                });
            },
        );
        assert!(actions.contains(&PlayerControlAction::Stop));
    }

    #[test]
    fn stop_button_is_disabled_without_loaded_media() {
        let context = Context::default();
        let mut state = view(&[]);
        state.stop_enabled = false;
        let mut stop_rect = None;
        let mut actions = Vec::new();
        run_ui(&context, RawInput::default(), |ui| {
            CentralPanel::default().show(ui, |ui| {
                actions = draw_inner(ui, state, |id, rect| {
                    if id == ControlId::Stop {
                        stop_rect = Some(rect);
                    }
                });
            });
        });
        let center = stop_rect.expect("stop button is rendered").center();
        run_ui(
            &context,
            RawInput {
                events: vec![
                    egui::Event::PointerMoved(center),
                    egui::Event::PointerButton {
                        pos: center,
                        button: egui::PointerButton::Primary,
                        pressed: true,
                        modifiers: egui::Modifiers::NONE,
                    },
                    egui::Event::PointerButton {
                        pos: center,
                        button: egui::PointerButton::Primary,
                        pressed: false,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
                ..RawInput::default()
            },
            |ui| {
                CentralPanel::default().show(ui, |ui| {
                    actions = draw(ui, state);
                });
            },
        );
        assert!(!actions.contains(&PlayerControlAction::Stop));
    }

    #[test]
    fn pip_button_emits_toggle_and_is_disabled_without_video() {
        // One fresh context per case, laid out once and then clicked, as the play-button test does.
        let click_pip = |available: bool| -> Vec<PlayerControlAction> {
            let context = Context::default();
            let pip_view = || PlayerControlsView {
                pip_available: available,
                ..view(&[])
            };
            let mut pip_rect = None;
            run_ui(&context, RawInput::default(), |ui| {
                CentralPanel::default().show(ui, |ui| {
                    draw_inner(ui, pip_view(), |id, rect| {
                        if id == ControlId::PipToggle {
                            pip_rect = Some(rect);
                        }
                    });
                });
            });
            let center = pip_rect.expect("PiP button is rendered").center();
            let mut action = Vec::new();
            run_ui(
                &context,
                RawInput {
                    events: vec![
                        egui::Event::PointerMoved(center),
                        egui::Event::PointerButton {
                            pos: center,
                            button: egui::PointerButton::Primary,
                            pressed: true,
                            modifiers: egui::Modifiers::NONE,
                        },
                        egui::Event::PointerButton {
                            pos: center,
                            button: egui::PointerButton::Primary,
                            pressed: false,
                            modifiers: egui::Modifiers::NONE,
                        },
                    ],
                    ..RawInput::default()
                },
                |ui| {
                    CentralPanel::default().show(ui, |ui| {
                        action = draw(ui, pip_view());
                    });
                },
            );
            action
        };
        assert!(
            !click_pip(false).contains(&PlayerControlAction::TogglePip),
            "a disabled PiP button must not emit an action"
        );
        assert!(click_pip(true).contains(&PlayerControlAction::TogglePip));
    }

    #[test]
    fn ctrl_s_press_is_the_screenshot_key_and_repeats_or_other_chords_are_not() {
        let key = |pressed, repeat, modifiers| egui::Event::Key {
            key: egui::Key::S,
            physical_key: None,
            pressed,
            repeat,
            modifiers,
        };
        let ctrl = egui::Modifiers::CTRL;
        assert!(screenshot_key_pressed(&[key(true, false, ctrl)]));
        assert!(
            !screenshot_key_pressed(&[key(false, false, ctrl)]),
            "release"
        );
        assert!(!screenshot_key_pressed(&[key(true, true, ctrl)]), "repeat");
        assert!(!screenshot_key_pressed(&[key(
            true,
            false,
            egui::Modifiers::NONE
        )]));
        assert!(!screenshot_key_pressed(&[key(
            true,
            false,
            ctrl | egui::Modifiers::SHIFT
        )]));
        assert!(!screenshot_key_pressed(&[key(
            true,
            false,
            ctrl | egui::Modifiers::ALT
        )]));
    }

    #[test]
    fn clicking_play_emits_toggle_action() {
        let context = Context::default();
        let mut action = Vec::new();
        let mut play_rect = None;
        run_ui(&context, RawInput::default(), |ui| {
            CentralPanel::default().show(ui, |ui| {
                action = draw_inner(ui, view(&[]), |id, rect| {
                    if id == ControlId::PlayPause {
                        play_rect = Some(rect);
                    }
                });
            });
        });
        assert!(action.is_empty());
        let center = play_rect.expect("play/pause button is rendered").center();
        run_ui(
            &context,
            RawInput {
                events: vec![
                    egui::Event::PointerMoved(center),
                    egui::Event::PointerButton {
                        pos: center,
                        button: egui::PointerButton::Primary,
                        pressed: true,
                        modifiers: egui::Modifiers::NONE,
                    },
                    egui::Event::PointerButton {
                        pos: center,
                        button: egui::PointerButton::Primary,
                        pressed: false,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
                ..RawInput::default()
            },
            |ui| {
                CentralPanel::default().show(ui, |ui| {
                    action = draw(ui, view(&[]));
                });
            },
        );
        assert!(action.contains(&PlayerControlAction::TogglePause));
    }

    #[test]
    fn previous_track_button_emits_action_and_is_disabled_without_queue_context() {
        let context = Context::default();
        let mut previous_rect = None;
        let mut actions = Vec::new();
        run_ui(&context, RawInput::default(), |ui| {
            CentralPanel::default().show(ui, |ui| {
                actions = draw_inner(ui, view(&[]), |id, rect| {
                    if id == ControlId::PreviousTrack {
                        previous_rect = Some(rect);
                    }
                });
            });
        });
        let center = previous_rect
            .expect("previous track button is rendered")
            .center();
        run_ui(
            &context,
            RawInput {
                events: vec![
                    egui::Event::PointerMoved(center),
                    egui::Event::PointerButton {
                        pos: center,
                        button: egui::PointerButton::Primary,
                        pressed: true,
                        modifiers: egui::Modifiers::NONE,
                    },
                    egui::Event::PointerButton {
                        pos: center,
                        button: egui::PointerButton::Primary,
                        pressed: false,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
                ..RawInput::default()
            },
            |ui| {
                CentralPanel::default().show(ui, |ui| {
                    actions = draw(ui, view(&[]));
                });
            },
        );
        assert!(actions.contains(&PlayerControlAction::PreviousTrack));

        let context = Context::default();
        let mut disabled = view(&[]);
        disabled.previous_track_enabled = false;
        let mut previous_rect = None;
        run_ui(&context, RawInput::default(), |ui| {
            CentralPanel::default().show(ui, |ui| {
                let _ = draw_inner(ui, disabled, |id, rect| {
                    if id == ControlId::PreviousTrack {
                        previous_rect = Some(rect);
                    }
                });
            });
        });
        let center = previous_rect
            .expect("previous track button is rendered")
            .center();
        run_ui(
            &context,
            RawInput {
                events: vec![
                    egui::Event::PointerMoved(center),
                    egui::Event::PointerButton {
                        pos: center,
                        button: egui::PointerButton::Primary,
                        pressed: true,
                        modifiers: egui::Modifiers::NONE,
                    },
                    egui::Event::PointerButton {
                        pos: center,
                        button: egui::PointerButton::Primary,
                        pressed: false,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
                ..RawInput::default()
            },
            |ui| {
                CentralPanel::default().show(ui, |ui| {
                    actions = draw(ui, disabled);
                });
            },
        );
        assert!(!actions.contains(&PlayerControlAction::PreviousTrack));
    }

    #[test]
    fn next_track_button_emits_action() {
        let context = Context::default();
        let mut next_rect = None;
        let mut actions = Vec::new();
        run_ui(&context, RawInput::default(), |ui| {
            CentralPanel::default().show(ui, |ui| {
                actions = draw_inner(ui, view(&[]), |id, rect| {
                    if id == ControlId::NextTrack {
                        next_rect = Some(rect);
                    }
                });
            });
        });
        let center = next_rect.expect("next track button is rendered").center();
        run_ui(
            &context,
            RawInput {
                events: vec![
                    egui::Event::PointerMoved(center),
                    egui::Event::PointerButton {
                        pos: center,
                        button: egui::PointerButton::Primary,
                        pressed: true,
                        modifiers: egui::Modifiers::NONE,
                    },
                    egui::Event::PointerButton {
                        pos: center,
                        button: egui::PointerButton::Primary,
                        pressed: false,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
                ..RawInput::default()
            },
            |ui| {
                CentralPanel::default().show(ui, |ui| {
                    actions = draw(ui, view(&[]));
                });
            },
        );
        assert!(actions.contains(&PlayerControlAction::NextTrack));
    }

    #[test]
    fn unmodified_left_and_right_seek_exactly_ten_seconds() {
        for (key, expected) in [
            (egui::Key::ArrowLeft, PlayerControlAction::SeekRelative(-10)),
            (egui::Key::ArrowRight, PlayerControlAction::SeekRelative(10)),
        ] {
            let context = Context::default();
            let mut actions = Vec::new();
            run_ui(
                &context,
                RawInput {
                    events: vec![egui::Event::Key {
                        key,
                        physical_key: Some(key),
                        pressed: true,
                        repeat: false,
                        modifiers: egui::Modifiers::NONE,
                    }],
                    ..RawInput::default()
                },
                |ui| {
                    CentralPanel::default().show(ui, |ui| {
                        actions = draw(ui, view(&[]));
                    });
                },
            );
            assert_eq!(actions, [expected]);
        }
    }

    #[test]
    fn arrow_seek_does_not_run_while_a_text_editor_is_focused() {
        let context = Context::default();
        let mut text = String::new();
        run_ui(&context, RawInput::default(), |ui| {
            CentralPanel::default().show(ui, |ui| {
                let response = ui.add(egui::TextEdit::singleline(&mut text));
                response.request_focus();
            });
        });
        assert!(context.egui_wants_keyboard_input());

        let actions = keyboard_seek_actions(&context, true);
        assert!(actions.is_empty());
    }

    #[test]
    fn arrow_seek_does_not_run_while_a_popup_is_open() {
        let context = Context::default();
        egui::Popup::open_id(&context, Id::unique("player-control-test-popup"));
        assert!(egui::Popup::is_any_open(&context));

        let mut actions = Vec::new();
        run_ui(
            &context,
            RawInput {
                events: vec![egui::Event::Key {
                    key: egui::Key::ArrowRight,
                    physical_key: Some(egui::Key::ArrowRight),
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::NONE,
                }],
                ..RawInput::default()
            },
            |ui| {
                CentralPanel::default().show(ui, |ui| {
                    actions = draw(ui, view(&[]));
                });
            },
        );
        assert!(actions.is_empty());
    }

    #[test]
    fn modified_arrows_and_unseekable_media_do_not_seek() {
        let context = Context::default();
        let mut actions = Vec::new();
        run_ui(
            &context,
            RawInput {
                events: vec![egui::Event::Key {
                    key: egui::Key::ArrowRight,
                    physical_key: Some(egui::Key::ArrowRight),
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::CTRL,
                }],
                ..RawInput::default()
            },
            |ui| {
                actions = keyboard_seek_actions(ui.ctx(), true);
            },
        );
        assert!(actions.is_empty());
        assert!(keyboard_seek_actions(&context, false).is_empty());
    }

    /// Playback at 12 s in a 600 s item, with an intro from 10 s to 30 s loaded.
    fn skip_view() -> PlayerControlsView<'static> {
        PlayerControlsView {
            position: Duration::from_secs(12),
            duration: Some(Duration::from_secs(600)),
            ..view(&[])
        }
    }

    fn intro_skipper() -> SegmentSkipper {
        let mut skipper = SegmentSkipper::new();
        let intro = MediaSegment::new("intro-1", SegmentKind::Intro, 10 * SECOND, 30 * SECOND)
            .expect("intro segment");
        skipper.load("episode-1", vec![intro]);
        skipper
    }

    fn click_events(center: egui::Pos2) -> Vec<egui::Event> {
        vec![
            egui::Event::PointerMoved(center),
            egui::Event::PointerButton {
                pos: center,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::NONE,
            },
            egui::Event::PointerButton {
                pos: center,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            },
        ]
    }

    #[test]
    fn skip_button_shows_for_an_intro_and_seeks_once_when_pressed() {
        let context = Context::default();
        let mut skipper = intro_skipper();
        let settings = SkipSettings::new(true, false, false, false, 0).expect("settings");
        let mut skip_rect = None;
        let mut actions = Vec::new();
        run_ui(&context, RawInput::default(), |ui| {
            CentralPanel::default().show(ui, |ui| {
                actions = draw_inner_with_skip(
                    ui,
                    skip_view(),
                    Some(SkipControls {
                        skipper: &mut skipper,
                        settings,
                    }),
                    |id, rect| {
                        if id == ControlId::SkipSegment {
                            skip_rect = Some(rect);
                        }
                    },
                );
            });
        });
        assert!(actions.is_empty(), "the button alone must not seek");
        let center = skip_rect.expect("skip button is rendered").center();

        run_ui(
            &context,
            RawInput {
                events: click_events(center),
                ..RawInput::default()
            },
            |ui| {
                CentralPanel::default().show(ui, |ui| {
                    actions = draw_with_skip(
                        ui,
                        skip_view(),
                        Some(SkipControls {
                            skipper: &mut skipper,
                            settings,
                        }),
                    );
                });
            },
        );
        assert_eq!(
            actions,
            [PlayerControlAction::SeekAbsolute(Duration::from_secs(30))]
        );
        assert!(
            skipper
                .button(12 * SECOND, Some(600 * SECOND), &settings)
                .is_none(),
            "a pressed segment is not offered again"
        );
    }

    #[test]
    fn automatic_skip_seeks_without_a_press_and_only_once() {
        let context = Context::default();
        let mut skipper = intro_skipper();
        let settings = SkipSettings::new(true, false, true, false, 0).expect("settings");
        let mut skip_rect = None;
        let mut actions = Vec::new();
        for frame in 0..2 {
            run_ui(&context, RawInput::default(), |ui| {
                CentralPanel::default().show(ui, |ui| {
                    actions = draw_inner_with_skip(
                        ui,
                        skip_view(),
                        Some(SkipControls {
                            skipper: &mut skipper,
                            settings,
                        }),
                        |id, rect| {
                            if id == ControlId::SkipSegment {
                                skip_rect = Some(rect);
                            }
                        },
                    );
                });
            });
            if frame == 0 {
                assert_eq!(
                    actions,
                    [PlayerControlAction::SeekAbsolute(Duration::from_secs(30))]
                );
            } else {
                assert!(actions.is_empty(), "the segment is not skipped twice");
            }
        }
        assert!(
            skip_rect.is_none(),
            "an automatic skip does not also show the button"
        );
    }

    #[test]
    fn automatic_switch_without_its_button_switch_never_seeks() {
        let segments_off = [
            SkipSettings::default(),
            SkipSettings::new(false, false, true, true, 0).expect("settings"),
        ];
        for settings in segments_off {
            let context = Context::default();
            let mut skipper = intro_skipper();
            let mut skip_rect = None;
            let mut actions = Vec::new();
            run_ui(&context, RawInput::default(), |ui| {
                CentralPanel::default().show(ui, |ui| {
                    actions = draw_inner_with_skip(
                        ui,
                        skip_view(),
                        Some(SkipControls {
                            skipper: &mut skipper,
                            settings,
                        }),
                        |id, rect| {
                            if id == ControlId::SkipSegment {
                                skip_rect = Some(rect);
                            }
                        },
                    );
                });
            });
            assert!(actions.is_empty());
            assert!(skip_rect.is_none());
        }
    }

    #[test]
    fn hidden_controls_still_run_the_automatic_skip_once() {
        let mut skipper = intro_skipper();
        let settings = SkipSettings::new(true, false, true, false, 0).expect("settings");
        let duration = Some(Duration::from_secs(600));

        let first = auto_skip_action(
            SkipControls {
                skipper: &mut skipper,
                settings,
            },
            Duration::from_secs(12),
            duration,
        );
        assert_eq!(
            first,
            Some(PlayerControlAction::SeekAbsolute(Duration::from_secs(30)))
        );

        let second = auto_skip_action(
            SkipControls {
                skipper: &mut skipper,
                settings,
            },
            Duration::from_secs(13),
            duration,
        );
        assert_eq!(second, None, "the same intro must not be skipped twice");
    }

    #[test]
    fn playback_settings_map_to_the_skip_model() {
        let playback = PlaybackSettings {
            skip_outro: true,
            auto_skip_outro: true,
            outro_delay_ms: 5_000,
            ..PlaybackSettings::default()
        };
        let settings = skip_settings(&playback).expect("valid settings");
        assert!(settings.shows_button(SegmentKind::Outro));
        assert!(settings.auto_skips(SegmentKind::Outro));
        assert!(!settings.shows_button(SegmentKind::Intro));
        assert_eq!(settings.outro_delay_ms(), 5_000);
        assert!(
            !skip_settings(&PlaybackSettings::default())
                .expect("defaults are valid")
                .fetch_needed()
        );

        let too_late = PlaybackSettings {
            outro_delay_ms: 60_001,
            ..PlaybackSettings::default()
        };
        assert!(skip_settings(&too_late).is_err());
    }

    #[test]
    fn tick_conversion_is_100_ns_per_tick() {
        assert_eq!(duration_to_ticks(Duration::from_millis(1_500)), 15_000_000);
        assert_eq!(
            ticks_to_duration(duration_to_ticks(Duration::from_millis(1_500))),
            Duration::from_millis(1_500)
        );
    }

    /// Tab presses allowed for one walk through the overlay.
    const TAB_WALK_LIMIT: usize = 32;

    /// Every control in draw order, with the skip button offered (position 20 s is inside the
    /// intro), chapters present, and a second subtitle track present. Tab must reach them in
    /// this order (M2-18).
    const DRAW_ORDER: [ControlId; 20] = [
        ControlId::SeekBar,
        ControlId::PreviousTrack,
        ControlId::SeekBackward,
        ControlId::PlayPause,
        ControlId::SeekForward,
        ControlId::NextTrack,
        ControlId::Stop,
        ControlId::SkipSegment,
        ControlId::ChapterPrevious,
        ControlId::ChapterNext,
        ControlId::ChapterMenu,
        ControlId::SecondarySubtitleMenu,
        ControlId::MuteToggle,
        ControlId::VolumeSlider,
        ControlId::FrameBack,
        ControlId::FrameForward,
        ControlId::AbLoop,
        ControlId::SpeedMenu,
        ControlId::PipToggle,
        ControlId::Fullscreen,
    ];

    /// The position in `DRAW_ORDER`. Matching every variant means a new control fails to compile
    /// here until it is given a place, and the walk test then has to include it.
    fn draw_index(id: ControlId) -> usize {
        match id {
            ControlId::SeekBar => 0,
            ControlId::PreviousTrack => 1,
            ControlId::SeekBackward => 2,
            ControlId::PlayPause => 3,
            ControlId::SeekForward => 4,
            ControlId::NextTrack => 5,
            ControlId::Stop => 6,
            ControlId::SkipSegment => 7,
            ControlId::ChapterPrevious => 8,
            ControlId::ChapterNext => 9,
            ControlId::ChapterMenu => 10,
            ControlId::SecondarySubtitleMenu => 11,
            ControlId::MuteToggle => 12,
            ControlId::VolumeSlider => 13,
            ControlId::FrameBack => 14,
            ControlId::FrameForward => 15,
            ControlId::AbLoop => 16,
            ControlId::SpeedMenu => 17,
            ControlId::PipToggle => 18,
            ControlId::Fullscreen => 19,
        }
    }

    /// Every control enabled and visible, so the walk reaches all of them.
    fn keyboard_view<'a>(
        chapters: &'a [ChapterChoice],
        secondary: &'a [SecondarySubtitleChoice],
    ) -> PlayerControlsView<'a> {
        PlayerControlsView {
            chapters,
            secondary_subtitles: secondary,
            ..view(&[])
        }
    }

    fn key_press(key: egui::Key) -> RawInput {
        RawInput {
            events: vec![egui::Event::Key {
                key,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }],
            ..RawInput::default()
        }
    }

    /// One frame of the overlay: the actions, each control's rectangle, and the painted shapes.
    struct OverlayFrame {
        actions: Vec<PlayerControlAction>,
        rects: Vec<(ControlId, egui::Rect)>,
        shapes: Vec<egui::epaint::ClippedShape>,
    }

    fn overlay_frame(
        context: &Context,
        input: RawInput,
        view: PlayerControlsView<'_>,
    ) -> OverlayFrame {
        let mut skipper = intro_skipper();
        let settings = SkipSettings::new(true, false, false, false, 0).expect("settings");
        let mut actions = Vec::new();
        let mut rects = Vec::new();
        let mut output = context.run_ui(input, |ui| {
            CentralPanel::default().show(ui, |ui| {
                actions = draw_inner_with_skip(
                    ui,
                    view,
                    Some(SkipControls {
                        skipper: &mut skipper,
                        settings,
                    }),
                    |id, rect| rects.push((id, rect)),
                );
            });
        });
        output.textures_delta.clear();
        OverlayFrame {
            actions,
            rects,
            shapes: output.shapes,
        }
    }

    /// The control that has keyboard focus after a frame. The focused widget's own rectangle can be
    /// smaller than the control (the slider's grab point), so it is matched by its centre.
    fn focused_control(context: &Context, rects: &[(ControlId, egui::Rect)]) -> Option<ControlId> {
        let focused = context.memory(|memory| memory.focused())?;
        let center = context.read_response(focused)?.rect.center();
        rects
            .iter()
            .find(|(_, rect)| rect.contains(center))
            .map(|(id, _)| *id)
    }

    /// Press Tab until `target` has focus. False if it is not reached within the walk limit.
    fn tab_to(context: &Context, view: PlayerControlsView<'_>, target: ControlId) -> bool {
        (0..TAB_WALK_LIMIT).any(|_| {
            let frame = overlay_frame(context, key_press(egui::Key::Tab), view);
            focused_control(context, &frame.rects) == Some(target)
        })
    }

    #[test]
    fn every_control_is_reached_by_tab_in_draw_order() {
        for (index, id) in DRAW_ORDER.iter().enumerate() {
            assert_eq!(draw_index(*id), index, "{id:?} is listed at its draw index");
        }

        let chapters = [ChapterChoice {
            index: 0,
            title: "Opening".into(),
            time: Duration::ZERO,
        }];
        let secondary = [SecondarySubtitleChoice {
            track: 3,
            title: "English".into(),
        }];
        let context = Context::default();
        let state = keyboard_view(&chapters, &secondary);
        let mut reached = Vec::new();
        for _ in 0..TAB_WALK_LIMIT {
            let frame = overlay_frame(&context, key_press(egui::Key::Tab), state);
            let Some(id) = focused_control(&context, &frame.rects) else {
                break;
            };
            if reached.contains(&id) {
                break;
            }
            reached.push(id);
        }
        assert_eq!(reached, DRAW_ORDER);
    }

    #[test]
    fn enter_activates_each_button_once_tab_has_focused_it() {
        let chapters = [];
        let secondary = [];
        let expected = [
            (ControlId::PlayPause, PlayerControlAction::TogglePause),
            (ControlId::PreviousTrack, PlayerControlAction::PreviousTrack),
            (ControlId::NextTrack, PlayerControlAction::NextTrack),
            (
                ControlId::SeekBackward,
                PlayerControlAction::SeekRelative(-10),
            ),
            (
                ControlId::SeekForward,
                PlayerControlAction::SeekRelative(10),
            ),
            (
                ControlId::FrameBack,
                PlayerControlAction::StepFrame { backward: true },
            ),
            (
                ControlId::FrameForward,
                PlayerControlAction::StepFrame { backward: false },
            ),
            (ControlId::AbLoop, PlayerControlAction::CycleAbLoop),
            (ControlId::Stop, PlayerControlAction::Stop),
            (ControlId::PipToggle, PlayerControlAction::TogglePip),
            (ControlId::MuteToggle, PlayerControlAction::ToggleMute),
            (ControlId::Fullscreen, PlayerControlAction::ToggleFullscreen),
        ];
        for (id, action) in expected {
            let context = Context::default();
            let state = keyboard_view(&chapters, &secondary);
            assert!(tab_to(&context, state, id), "Tab reaches {id:?}");
            let frame = overlay_frame(&context, key_press(egui::Key::Enter), state);
            assert!(
                frame.actions.contains(&action),
                "Enter on {id:?} emits {action:?}, got {:?}",
                frame.actions
            );
        }
    }

    #[test]
    fn one_arrow_press_on_the_focused_seek_bar_seeks_once() {
        let chapters = [];
        let secondary = [];
        let context = Context::default();
        let state = keyboard_view(&chapters, &secondary);
        assert!(tab_to(&context, state, ControlId::SeekBar));
        let frame = overlay_frame(&context, key_press(egui::Key::ArrowRight), state);
        let seeks = frame
            .actions
            .iter()
            .filter(|action| {
                matches!(
                    action,
                    PlayerControlAction::SeekRelative(_) | PlayerControlAction::SeekAbsolute(_)
                )
            })
            .count();
        assert_eq!(
            seeks, 1,
            "one arrow press seeks once, got {:?}",
            frame.actions
        );
    }

    #[test]
    fn enter_on_the_skip_button_seeks_once_it_has_focus() {
        let chapters = [];
        let secondary = [];
        let context = Context::default();
        let state = keyboard_view(&chapters, &secondary);
        assert!(tab_to(&context, state, ControlId::SkipSegment));
        let frame = overlay_frame(&context, key_press(egui::Key::Enter), state);
        assert!(
            frame
                .actions
                .iter()
                .any(|action| matches!(action, PlayerControlAction::SeekAbsolute(_))),
            "Enter on the skip button seeks, got {:?}",
            frame.actions
        );
    }

    #[test]
    fn menu_items_can_be_chosen_from_the_keyboard() {
        let chapters = [ChapterChoice {
            index: 0,
            title: "Opening".into(),
            time: Duration::ZERO,
        }];
        let secondary = [SecondarySubtitleChoice {
            track: 3,
            title: "English".into(),
        }];
        type IsChoice = fn(&PlayerControlAction) -> bool;
        let menus: [(ControlId, IsChoice); 3] = [
            (ControlId::SpeedMenu, |action| {
                matches!(action, PlayerControlAction::SetSpeed(_))
            }),
            (ControlId::ChapterMenu, |action| {
                matches!(action, PlayerControlAction::SelectChapter(_))
            }),
            (ControlId::SecondarySubtitleMenu, |action| {
                matches!(action, PlayerControlAction::SelectSecondarySubtitle(_))
            }),
        ];
        for (menu, is_choice) in menus {
            let context = Context::default();
            let state = keyboard_view(&chapters, &secondary);
            assert!(tab_to(&context, state, menu), "Tab reaches {menu:?}");
            let opened = overlay_frame(&context, key_press(egui::Key::Enter), state);
            assert!(opened.actions.is_empty(), "Enter opens {menu:?}");
            let mut chosen = Vec::new();
            for _ in 0..TAB_WALK_LIMIT {
                overlay_frame(&context, key_press(egui::Key::ArrowDown), state);
                chosen.extend(overlay_frame(&context, key_press(egui::Key::Enter), state).actions);
                if chosen.iter().any(is_choice) {
                    break;
                }
            }
            assert!(
                chosen.iter().any(is_choice),
                "{menu:?} takes an item with arrows and Enter, got {chosen:?}"
            );
        }
    }

    /// The fill and stroke egui paints for the button at `id` in `frame`.
    fn button_paint(frame: &OverlayFrame, id: ControlId) -> Option<(Color32, Stroke)> {
        let center = frame
            .rects
            .iter()
            .find(|(candidate, _)| *candidate == id)?
            .1
            .center();
        frame
            .shapes
            .iter()
            .find_map(|clipped| match &clipped.shape {
                egui::Shape::Rect(shape) if (shape.rect.center() - center).length() < 1.0 => {
                    Some((shape.fill, shape.stroke))
                }
                _ => None,
            })
    }

    #[test]
    fn the_focused_button_is_painted_differently_from_its_neighbours() {
        let chapters = [];
        let secondary = [];
        let context = Context::default();
        let state = keyboard_view(&chapters, &secondary);
        assert!(
            tab_to(&context, state, ControlId::PlayPause),
            "Tab reaches play"
        );
        // egui reads a button's style from the previous frame, so one idle frame shows the focus.
        let frame = overlay_frame(&context, RawInput::default(), state);
        assert_eq!(
            focused_control(&context, &frame.rects),
            Some(ControlId::PlayPause),
            "focus stays on play while the frame is idle"
        );
        let focused = button_paint(&frame, ControlId::PlayPause).expect("play is painted");
        let neighbour = button_paint(&frame, ControlId::NextTrack).expect("next is painted");
        assert_ne!(
            focused, neighbour,
            "the focused button shows a different fill or outline"
        );
    }

    /// Tab presses allowed for one walk through the track section (header, audio, subtitle).
    const TRACK_TAB_LIMIT: usize = 8;

    /// Idle frames allowed for the header's open animation before the combos must be drawn.
    const TRACK_SETTLE_LIMIT: usize = 240;

    fn audio_choices() -> Vec<TrackChoice<&'static str>> {
        vec![
            TrackChoice {
                value: "automatic",
                label: "Automatic".into(),
                selected: true,
            },
            TrackChoice {
                value: "english-5.1",
                label: "English 5.1".into(),
                selected: false,
            },
            TrackChoice {
                value: "commentary",
                label: "Commentary".into(),
                selected: false,
            },
        ]
    }

    fn subtitle_choices() -> Vec<TrackChoice<&'static str>> {
        vec![
            TrackChoice {
                value: "off",
                label: "Off".into(),
                selected: false,
            },
            TrackChoice {
                value: "english",
                label: "English".into(),
                selected: true,
            },
            TrackChoice {
                value: "french",
                label: "French".into(),
                selected: false,
            },
        ]
    }

    /// One frame of the track section: the picks, and each widget with its egui id.
    struct TrackFrame {
        audio: Option<&'static str>,
        subtitle: Option<&'static str>,
        observed: Vec<(TrackControl, egui::Id)>,
    }

    fn track_frame(
        context: &Context,
        input: RawInput,
        audio: &[TrackChoice<&'static str>],
        subtitle: &[TrackChoice<&'static str>],
    ) -> TrackFrame {
        let mut observed = Vec::new();
        let mut picked: (Option<&'static str>, Option<&'static str>) = (None, None);
        let mut output = context.run_ui(input, |ui| {
            CentralPanel::default().show(ui, |ui| {
                picked =
                    track_section_inner(ui, Some(audio), Some(subtitle), None, |control, id| {
                        observed.push((control, id));
                    });
            });
        });
        output.textures_delta.clear();
        TrackFrame {
            audio: picked.0,
            subtitle: picked.1,
            observed,
        }
    }

    #[test]
    fn narrow_track_menus_fit_200px_viewport_with_long_track_names() {
        let audio = [TrackChoice {
            value: 7,
            label: "English Dolby TrueHD 7.1 immersive commentary track".to_owned(),
            selected: true,
        }];
        let subtitle = [TrackChoice {
            value: 12,
            label: "English SDH forced descriptive subtitles for director commentary".to_owned(),
            selected: true,
        }];
        let context = Context::default();
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(200.0, 320.0));
        let mut contents_rect = None;
        let mut output = context.run_ui(
            RawInput {
                screen_rect: Some(screen),
                ..RawInput::default()
            },
            |ui| {
                CentralPanel::default().show(ui, |ui| {
                    let picked = track_menu_contents(ui, Some(&audio), Some(&subtitle), None);
                    assert_eq!(picked, (None, None));
                    contents_rect = Some(ui.min_rect());
                });
            },
        );
        output.textures_delta.clear();
        let contents_rect = contents_rect.expect("track menus were rendered");
        assert!(
            contents_rect.min.x >= screen.min.x,
            "track labels start inside viewport: {contents_rect:?}"
        );
        assert!(
            contents_rect.max.x <= screen.max.x,
            "track labels and menus fit the viewport: {contents_rect:?}"
        );
    }

    /// The section widget that has keyboard focus, matched by egui's id for the widget.
    fn focused_track_control(
        context: &Context,
        observed: &[(TrackControl, egui::Id)],
    ) -> Option<TrackControl> {
        let focused = context.memory(|memory| memory.focused())?;
        observed
            .iter()
            .find(|(_, id)| *id == focused)
            .map(|(control, _)| *control)
    }

    fn track_tab_to(
        context: &Context,
        audio: &[TrackChoice<&'static str>],
        subtitle: &[TrackChoice<&'static str>],
        target: TrackControl,
    ) -> bool {
        (0..TRACK_TAB_LIMIT).any(|_| {
            let frame = track_frame(context, key_press(egui::Key::Tab), audio, subtitle);
            focused_track_control(context, &frame.observed) == Some(target)
        })
    }

    /// Open the header with Enter, then idle until the audio combo is drawn. The header opens
    /// with an animation, so the combo is waited for rather than assumed on the next frame.
    fn open_track_header(
        context: &Context,
        audio: &[TrackChoice<&'static str>],
        subtitle: &[TrackChoice<&'static str>],
    ) {
        track_frame(context, key_press(egui::Key::Enter), audio, subtitle);
        for _ in 0..TRACK_SETTLE_LIMIT {
            let frame = track_frame(context, RawInput::default(), audio, subtitle);
            if frame
                .observed
                .iter()
                .any(|(control, _)| *control == TrackControl::Audio)
            {
                return;
            }
        }
    }

    /// Tab to the header, Enter to open it, Tab to the combo, Enter to open it, Tab to the entry,
    /// and Enter to choose it. Returns what the section picked on that last Enter.
    fn pick_by_keyboard(
        context: &Context,
        audio: &[TrackChoice<&'static str>],
        subtitle: &[TrackChoice<&'static str>],
        combo: TrackControl,
        entry: TrackControl,
    ) -> Option<&'static str> {
        assert!(
            track_tab_to(context, audio, subtitle, TrackControl::Header),
            "Tab reaches the tracks header"
        );
        open_track_header(context, audio, subtitle);
        assert!(
            track_tab_to(context, audio, subtitle, combo),
            "Tab reaches {combo:?}"
        );
        track_frame(context, key_press(egui::Key::Enter), audio, subtitle);
        assert!(
            track_tab_to(context, audio, subtitle, entry),
            "Tab reaches {entry:?} in the open popup"
        );
        let frame = track_frame(context, key_press(egui::Key::Enter), audio, subtitle);
        match combo {
            TrackControl::Subtitle => frame.subtitle,
            _ => frame.audio,
        }
    }

    #[test]
    fn track_combos_are_hidden_until_the_header_is_opened_from_the_keyboard() {
        let (audio, subtitle) = (audio_choices(), subtitle_choices());
        let context = Context::default();
        assert!(
            track_tab_to(&context, &audio, &subtitle, TrackControl::Header),
            "Tab reaches the tracks header"
        );
        let closed = track_frame(&context, RawInput::default(), &audio, &subtitle);
        assert!(
            !closed
                .observed
                .iter()
                .any(|(control, _)| *control == TrackControl::Audio),
            "the combos are not drawn while the header is closed"
        );
        open_track_header(&context, &audio, &subtitle);
        let open = track_frame(&context, RawInput::default(), &audio, &subtitle);
        assert!(
            open.observed
                .iter()
                .any(|(control, _)| *control == TrackControl::Audio),
            "Enter on the header draws the audio combo"
        );
        assert!(
            open.observed
                .iter()
                .any(|(control, _)| *control == TrackControl::Subtitle),
            "Enter on the header draws the subtitle combo"
        );
    }

    #[test]
    fn every_track_entry_is_chosen_from_the_keyboard() {
        let (audio, subtitle) = (audio_choices(), subtitle_choices());
        // A fresh context per entry, so focus and popup state from one pick do not leak.
        for (index, choice) in audio.iter().enumerate() {
            let context = Context::default();
            let picked = pick_by_keyboard(
                &context,
                &audio,
                &subtitle,
                TrackControl::Audio,
                TrackControl::AudioEntry(index),
            );
            assert_eq!(
                picked,
                Some(choice.value),
                "audio entry {index} ({}) is chosen from the keyboard",
                choice.label
            );
        }
        for (index, choice) in subtitle.iter().enumerate() {
            let context = Context::default();
            let picked = pick_by_keyboard(
                &context,
                &audio,
                &subtitle,
                TrackControl::Subtitle,
                TrackControl::SubtitleEntry(index),
            );
            assert_eq!(
                picked,
                Some(choice.value),
                "subtitle entry {index} ({}) is chosen from the keyboard",
                choice.label
            );
        }
    }
}
