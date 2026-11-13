use crate::app::{ActivePlayerPosition, TICKS_PER_SECOND};
use crate::theme::Palette;
use egui::{RichText, Ui};
use jellyfin_api::LyricLine;
use std::time::Duration;

pub(super) fn show_track_lyrics(
    ui: &mut Ui,
    lines: &[LyricLine],
    active_position: Option<ActivePlayerPosition>,
    colors: Palette,
) {
    let active_line =
        active_position.and_then(|position| active_lyric_index(lines, position.position_ticks));
    let repaint_after = active_position.and_then(|position| next_lyric_repaint(lines, position));

    egui::CollapsingHeader::new(crate::ui::ui_text(kattri_i18n::ids::DETAIL_LYRICS))
        .id_salt("detail_lyrics")
        .default_open(true)
        .show(ui, |ui| {
            if let Some(delay) = repaint_after {
                ui.ctx().request_repaint_after(delay);
            }
            egui::ScrollArea::vertical()
                .id_salt("detail_lyrics_scroll")
                .max_height(320.0)
                .show(ui, |ui| {
                    for (index, line) in lines.iter().enumerate() {
                        let text = RichText::new(&line.text);
                        let text = if active_line == Some(index) {
                            text.strong().color(colors.accent)
                        } else {
                            text.color(colors.text)
                        };
                        ui.label(text);
                    }
                });
        });
}

fn active_lyric_index(lines: &[LyricLine], position_ticks: i64) -> Option<usize> {
    if position_ticks < 0 {
        return None;
    }
    lines
        .iter()
        .enumerate()
        .filter_map(|(index, line)| {
            let start_ticks = line
                .start_ticks
                .filter(|start| *start >= 0 && *start <= position_ticks)?;
            Some((start_ticks, index))
        })
        .max_by_key(|(start_ticks, index)| (*start_ticks, *index))
        .map(|(_, index)| index)
}

fn next_lyric_repaint(lines: &[LyricLine], position: ActivePlayerPosition) -> Option<Duration> {
    if position.paused != Some(false) {
        return None;
    }
    let speed = position
        .speed_multiplier
        .filter(|speed| speed.is_finite() && *speed > 0.0)?;
    let next_start = lines
        .iter()
        .filter_map(|line| line.start_ticks)
        .filter(|start| *start >= 0 && *start > position.position_ticks)
        .min()?;
    let seconds = (next_start - position.position_ticks) as f64 / TICKS_PER_SECOND as f64 / speed;
    if !seconds.is_finite() || seconds <= 0.0 {
        return None;
    }
    Duration::try_from_secs_f64(seconds)
        .ok()
        .map(|delay| delay.max(Duration::from_millis(1)))
}

#[cfg(test)]
mod tests {
    use super::{active_lyric_index, next_lyric_repaint};
    use crate::app::{ActivePlayerPosition, TICKS_PER_SECOND};
    use jellyfin_api::LyricLine;
    use std::time::Duration;

    fn line(start_ticks: Option<i64>) -> LyricLine {
        LyricLine {
            text: "line".into(),
            start_ticks,
        }
    }

    #[test]
    fn selects_latest_valid_start_at_or_before_the_player_position() {
        let lines = [
            line(Some(10)),
            line(Some(-1)),
            line(Some(5)),
            line(None),
            line(Some(15)),
        ];
        assert_eq!(active_lyric_index(&lines, 4), None);
        assert_eq!(active_lyric_index(&lines, 5), Some(2));
        assert_eq!(active_lyric_index(&lines, 12), Some(0));
        assert_eq!(active_lyric_index(&lines, 15), Some(4));
    }

    #[test]
    fn unsynced_lyrics_have_no_active_line_or_timed_repaint() {
        let lines = [line(None), line(None)];
        assert_eq!(active_lyric_index(&lines, TICKS_PER_SECOND), None);
        assert_eq!(
            next_lyric_repaint(
                &lines,
                ActivePlayerPosition {
                    position_ticks: TICKS_PER_SECOND,
                    paused: Some(false),
                    speed_multiplier: Some(1.0),
                }
            ),
            None
        );
    }

    #[test]
    fn schedules_only_the_next_boundary_while_playing() {
        let lines = [
            line(Some(2 * TICKS_PER_SECOND)),
            line(Some(6 * TICKS_PER_SECOND)),
        ];
        let position = ActivePlayerPosition {
            position_ticks: 2 * TICKS_PER_SECOND,
            paused: Some(false),
            speed_multiplier: Some(2.0),
        };
        assert_eq!(
            next_lyric_repaint(&lines, position),
            Some(Duration::from_secs(2))
        );

        assert_eq!(
            next_lyric_repaint(
                &lines,
                ActivePlayerPosition {
                    paused: Some(true),
                    ..position
                }
            ),
            None
        );
        assert_eq!(
            next_lyric_repaint(
                &lines,
                ActivePlayerPosition {
                    speed_multiplier: None,
                    ..position
                }
            ),
            None
        );
    }
}
