use egui::{Color32, Rect, load::SizedTexture};

use super::row::{cover_uv, gradient_quad};
use crate::theme::Palette;

#[derive(Default)]
pub struct AmbientBackdrop {
    current: Option<SizedTexture>,
    previous: Option<SizedTexture>,
    changed_at: f64,
}

impl AmbientBackdrop {
    pub fn paint(
        &mut self,
        ui: &egui::Ui,
        rect: Rect,
        next: Option<SizedTexture>,
        palette: Palette,
    ) {
        if rect.width() <= 0.0 || rect.height() <= 0.0 {
            return;
        }
        if next.is_none() {
            self.current = None;
            self.previous = None;
            return;
        }
        let now = ui.input(|input| input.time);
        let reduced_motion = ui.style().animation_time <= 0.0;
        let next_id = next.map(|texture| texture.id);
        if self.current.map(|texture| texture.id) != next_id {
            self.previous = self.current.take();
            self.current = next;
            self.changed_at = now;
        }
        let alpha = transition_alpha(now, self.changed_at, reduced_motion);
        if alpha < 1.0 {
            ui.ctx().request_repaint();
        }
        let painter = ui.painter_at(rect);
        if let Some(previous) = self.previous.filter(|_| alpha < 1.0) {
            painter.image(
                previous.id,
                rect,
                cover_uv(previous.size, rect.size()),
                Color32::WHITE,
            );
        }
        if let Some(current) = self.current {
            painter.image(
                current.id,
                rect,
                cover_uv(current.size, rect.size()),
                Color32::WHITE.gamma_multiply(alpha),
            );
        }
        paint_scrim(&painter, rect, palette);
    }
}

fn transition_alpha(now: f64, changed_at: f64, reduced_motion: bool) -> f32 {
    if reduced_motion {
        1.0
    } else {
        ((now - changed_at).max(0.0) as f32 / 0.3).clamp(0.0, 1.0)
    }
}

fn paint_scrim(painter: &egui::Painter, rect: Rect, palette: Palette) {
    let dark = Color32::from_black_alpha(205);
    let clear = Color32::TRANSPARENT;
    gradient_quad(
        painter,
        Rect::from_min_max(
            rect.left_top(),
            egui::pos2(rect.left() + rect.width() * 0.72, rect.bottom()),
        ),
        [dark, clear, clear, dark],
    );
    gradient_quad(
        painter,
        Rect::from_min_max(
            egui::pos2(rect.left(), rect.top() + rect.height() * 0.28),
            rect.right_bottom(),
        ),
        [clear, clear, palette.background, palette.background],
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crossfade_finishes_in_300_ms_and_reduced_motion_is_instant() {
        assert_eq!(transition_alpha(10.0, 10.0, false), 0.0);
        assert_eq!(transition_alpha(10.15, 10.0, false), 0.5);
        assert_eq!(transition_alpha(10.3, 10.0, false), 1.0);
        assert_eq!(transition_alpha(10.0, 10.0, true), 1.0);
    }

    #[test]
    fn cover_crop_handles_narrow_and_wide_targets() {
        for size in [egui::vec2(1.0, 900.0), egui::vec2(1600.0, 1.0)] {
            let uv = cover_uv(egui::vec2(1920.0, 1080.0), size);
            assert!(uv.min.is_finite() && uv.max.is_finite());
            assert!(uv.min.x >= 0.0 && uv.min.y >= 0.0 && uv.max.x <= 1.0 && uv.max.y <= 1.0);
        }
    }
}
