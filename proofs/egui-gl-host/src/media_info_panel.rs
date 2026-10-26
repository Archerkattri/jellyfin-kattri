//! The media info panel (M6-14): a read-only list of what the player reports about the playing
//! stream, with a button that copies the same fields as plain text.
//!
//! The state here decides when the stream properties are read again; the drawing is a pure
//! function of a [`MediaInfo`]. Neither holds a stream URL, token, or server origin: the view
//! model hides any text that looks like one, and the properties read (see
//! `mpv_render_proof::StreamSnapshot`) are not ones that carry them.

use std::time::{Duration, Instant};

use kattri_client_core::media_info::MediaInfo;
use mpv_render_proof::StreamSnapshot;

/// How often the properties are read again while the panel is open. Some of them (the decoder in
/// use, the video color) only settle after the first frames.
pub(crate) const REFRESH_INTERVAL: Duration = Duration::from_secs(1);

const PANEL_WIDTH: f32 = 380.0;

/// Whether the panel is open and the latest properties the player returned.
#[derive(Default)]
pub(crate) struct MediaInfoState {
    open: bool,
    /// The read in flight, if any.
    request: Option<u64>,
    /// When the last read was queued (or failed to queue).
    last_attempt: Option<Instant>,
    snapshot: Option<StreamSnapshot>,
}

impl MediaInfoState {
    pub(crate) fn is_open(&self) -> bool {
        self.open
    }

    /// Open the panel, or close it when it is already open. Opening reads the properties at the
    /// next opportunity.
    pub(crate) fn toggle(&mut self) {
        if self.open {
            self.close();
        } else {
            self.open = true;
            self.last_attempt = None;
        }
    }

    pub(crate) fn close(&mut self) {
        *self = Self::default();
    }

    /// A new file started: the old properties no longer describe the stream.
    pub(crate) fn clear_snapshot(&mut self) {
        self.request = None;
        self.last_attempt = None;
        self.snapshot = None;
    }

    /// True when the panel is open, no read is in flight, and the last read is old enough.
    pub(crate) fn read_due(&self, now: Instant) -> bool {
        self.open
            && self.request.is_none()
            && self
                .last_attempt
                .is_none_or(|at| now.saturating_duration_since(at) >= REFRESH_INTERVAL)
    }

    pub(crate) fn read_queued(&mut self, request_id: u64, now: Instant) {
        self.request = Some(request_id);
        self.last_attempt = Some(now);
    }

    /// The read could not be queued. It is tried again after [`REFRESH_INTERVAL`].
    pub(crate) fn read_failed(&mut self, now: Instant) {
        self.request = None;
        self.last_attempt = Some(now);
    }

    pub(crate) fn is_waiting_for(&self, request_id: u64) -> bool {
        self.request == Some(request_id)
    }

    /// The in-flight read finished. `snapshot` is `None` when the player no longer had it.
    pub(crate) fn read_finished(&mut self, snapshot: Option<StreamSnapshot>) {
        self.request = None;
        if snapshot.is_some() {
            self.snapshot = snapshot;
        }
    }

    pub(crate) fn snapshot(&self) -> Option<&StreamSnapshot> {
        self.snapshot.as_ref()
    }
}

/// What the viewer did in the panel this frame.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct PanelOutcome {
    pub(crate) close: bool,
}

/// Draw the panel over the video, at the top right. "Copy diagnostics" puts the same fields on
/// the clipboard as plain text.
pub(crate) fn show(ctx: &egui::Context, info: &MediaInfo) -> PanelOutcome {
    let mut outcome = PanelOutcome::default();
    let max_height = (ctx.content_rect().height() - 150.0).max(120.0);
    egui::Area::new(egui::Id::unique("media_info_panel"))
        .order(egui::Order::Foreground)
        .anchor(egui::Align2::RIGHT_TOP, egui::vec2(-24.0, 20.0))
        .show(ctx, |ui| {
            egui::Frame::new()
                .fill(egui::Color32::from_black_alpha(215))
                .corner_radius(egui::CornerRadius::same(12))
                .inner_margin(egui::Margin::same(16))
                .show(ui, |ui| {
                    ui.set_width(PANEL_WIDTH);
                    ui.label(
                        egui::RichText::new("Media info")
                            .size(18.0)
                            .strong()
                            .color(egui::Color32::WHITE),
                    );
                    ui.add_space(8.0);
                    egui::ScrollArea::vertical()
                        .max_height(max_height)
                        .auto_shrink([false, true])
                        .show(ui, |ui| {
                            egui::Grid::new("media_info_rows")
                                .num_columns(2)
                                .spacing([14.0, 6.0])
                                .show(ui, |ui| {
                                    for row in info.rows() {
                                        ui.label(
                                            egui::RichText::new(row.label)
                                                .color(egui::Color32::from_gray(170)),
                                        );
                                        ui.add(
                                            egui::Label::new(
                                                egui::RichText::new(row.value)
                                                    .color(egui::Color32::WHITE),
                                            )
                                            .wrap(),
                                        );
                                        ui.end_row();
                                    }
                                });
                        });
                    ui.add_space(10.0);
                    ui.horizontal(|ui| {
                        if ui.button("Copy diagnostics").clicked() {
                            ui.ctx().copy_text(info.diagnostics_text());
                        }
                        if ui.button("Close").clicked() {
                            outcome.close = true;
                        }
                    });
                });
        });
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::{Event, Key, Modifiers, OutputCommand, RawInput};
    use kattri_client_core::media_info::MediaInput;
    use kattri_client_core::media_info_mpv::{
        MpvStreamValues, MpvTrackValues, media_input_from_mpv,
    };

    fn at(offset_ms: u64, base: Instant) -> Instant {
        base + Duration::from_millis(offset_ms)
    }

    #[test]
    fn a_closed_panel_never_reads() {
        let state = MediaInfoState::default();
        assert!(!state.is_open());
        assert!(!state.read_due(Instant::now()));
    }

    #[test]
    fn opening_reads_at_once_then_once_per_interval() {
        let base = Instant::now();
        let mut state = MediaInfoState::default();
        state.toggle();
        assert!(state.is_open());
        assert!(state.read_due(base));

        state.read_queued(7, base);
        // A read is in flight: no second read, however long it takes.
        assert!(!state.read_due(at(60_000, base)));
        assert!(state.is_waiting_for(7));
        assert!(!state.is_waiting_for(8));

        state.read_finished(None);
        assert!(!state.read_due(at(999, base)));
        assert!(state.read_due(at(1_000, base)));
    }

    #[test]
    fn a_failed_queue_is_retried_after_the_interval_not_every_frame() {
        let base = Instant::now();
        let mut state = MediaInfoState::default();
        state.toggle();
        state.read_failed(base);
        assert!(!state.read_due(at(10, base)));
        assert!(state.read_due(at(1_000, base)));
    }

    #[test]
    fn toggling_twice_closes_and_forgets_everything() {
        let base = Instant::now();
        let mut state = MediaInfoState::default();
        state.toggle();
        state.read_queued(3, base);
        state.toggle();
        assert!(!state.is_open());
        assert!(!state.is_waiting_for(3));
        assert!(state.snapshot().is_none());
        // Reopening reads again straight away.
        state.toggle();
        assert!(state.read_due(base));
    }

    #[test]
    fn a_new_file_drops_the_old_properties_but_keeps_the_panel_open() {
        let base = Instant::now();
        let mut state = MediaInfoState::default();
        state.toggle();
        state.read_queued(3, base);
        state.clear_snapshot();
        assert!(state.is_open());
        assert!(!state.is_waiting_for(3));
        assert!(state.read_due(base));
    }

    fn key_press(key: Key) -> RawInput {
        RawInput {
            events: vec![Event::Key {
                key,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: Modifiers::NONE,
            }],
            ..RawInput::default()
        }
    }

    /// The panel for a stream fixture built from values captured from mpv 0.37.0.
    fn fixture_info() -> MediaInfo {
        let stream = MpvStreamValues {
            container: Some("mkv".to_owned()),
            video_codec: Some("hevc".to_owned()),
            width: Some("640".to_owned()),
            height: Some("360".to_owned()),
            frame_rate: Some("23.976025".to_owned()),
            pixel_format: Some("yuv420p10".to_owned()),
            transfer: Some("pq".to_owned()),
            primaries: Some("bt.2020".to_owned()),
            hardware_decoder: Some("no".to_owned()),
            audio_codec: Some("ac3".to_owned()),
            audio_channel_count: Some("6".to_owned()),
            audio_channel_layout: Some("5.1(side)".to_owned()),
            audio_output: Some("null".to_owned()),
            audio_device: Some("auto".to_owned()),
            subtitle_codec: None,
        };
        let audio = [MpvTrackValues {
            id: 1,
            codec: Some("ac3".to_owned()),
            language: Some("eng".to_owned()),
            selected: true,
            ..MpvTrackValues::default()
        }];
        MediaInfo::from_input(&media_input_from_mpv(&stream, &audio, &[]))
    }

    /// Run one frame of the panel and return what it did and the text it copied.
    fn frame(
        context: &egui::Context,
        input: RawInput,
        info: &MediaInfo,
    ) -> (PanelOutcome, Vec<String>) {
        let mut outcome = PanelOutcome::default();
        let mut output = context.run_ui(input, |ui| {
            outcome = show(ui.ctx(), info);
        });
        // egui checks in debug builds that the host took the texture changes.
        output.textures_delta.clear();
        let copied = output
            .platform_output
            .commands
            .into_iter()
            .filter_map(|command| match command {
                OutputCommand::CopyText(text) => Some(text),
                _ => None,
            })
            .collect();
        (outcome, copied)
    }

    #[test]
    fn copy_diagnostics_copies_the_rows_as_plain_text_without_urls_or_tokens() {
        let context = egui::Context::default();
        let info = fixture_info();
        frame(&context, RawInput::default(), &info);

        // The first Tab stop is "Copy diagnostics"; Enter activates it.
        frame(&context, key_press(Key::Tab), &info);
        let (_, copied) = frame(&context, key_press(Key::Enter), &info);

        assert_eq!(copied, vec![info.diagnostics_text()]);
        let text = &copied[0];
        assert!(text.contains("Dynamic range: HDR (PQ)"));
        assert!(text.contains("Hardware decode: Software"));
        for forbidden in ["api_key", "token", "://", "http"] {
            assert!(!text.contains(forbidden), "copied text leaked {forbidden}");
        }
    }

    #[test]
    fn close_is_reachable_from_the_keyboard() {
        let context = egui::Context::default();
        let info = fixture_info();
        frame(&context, RawInput::default(), &info);
        frame(&context, key_press(Key::Tab), &info);
        frame(&context, key_press(Key::Tab), &info);
        let (outcome, copied) = frame(&context, key_press(Key::Enter), &info);
        assert!(outcome.close);
        assert!(copied.is_empty());
    }

    #[test]
    fn an_unloaded_stream_still_draws_with_placeholders() {
        let context = egui::Context::default();
        let info = MediaInfo::from_input(&MediaInput::default());
        let (outcome, copied) = frame(&context, RawInput::default(), &info);
        assert!(!outcome.close);
        assert!(copied.is_empty());
        assert!(info.diagnostics_text().contains("Container: Unavailable"));
    }
}
