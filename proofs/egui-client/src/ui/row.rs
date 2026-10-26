//! Netflix / Apple-TV style horizontal poster row.
//!
//! Network-free: callers supply already-decoded textures (or none, for the gradient
//! placeholder). Only visible cards are painted; `RowResponse::visible` tells the caller
//! which cards to lazy-load artwork for.

use super::{ui_text, ui_text_args};
use kattri_i18n::ids;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::ops::Range;
use std::sync::Arc;

use egui::text::LayoutJob;
use egui::{
    Align2, Color32, CornerRadius, FontId, Galley, Mesh, Painter, Pos2, Rect, RichText, Role,
    Sense, Shape, Stroke, Vec2, WidgetInfo, load::SizedTexture,
};

use super::anim::{self, EDGE_PADDING};
use crate::theme::{self, Palette};

/// Poster height / width.
const POSTER_ASPECT: f32 = 1.5;
/// Corner radius of a poster.
const POSTER_RADIUS: CornerRadius = CornerRadius::same(14);
/// Vertical space reserved for the title and subtitle under each poster.
const TEXT_BLOCK_H: f32 = 44.0;
const TEXT_GAP: f32 = 8.0;
/// Half-life (seconds) of the kinetic scroll.
const SCROLL_HALF_LIFE: f32 = 0.08;
/// Duration (seconds) of the focus scale/brighten transition.
const FOCUS_ANIM_SECS: f32 = 0.16;
/// Largest frame step used for smoothing, so stalls do not teleport the row.
const MAX_DT: f32 = 0.05;

/// One poster in a row. Artwork is optional; without it a gradient placeholder is drawn.
pub struct RowCard {
    /// Stable identifier, used for animation state and placeholder colours.
    pub id: String,
    pub title: String,
    pub subtitle: Option<String>,
    /// Watched fraction in `0..=1`; `None` hides the progress bar.
    pub progress: Option<f32>,
    /// Decoded poster texture and its pixel size (used for cover cropping).
    pub image: Option<SizedTexture>,
}

#[derive(Clone, Copy)]
pub struct RowOptions {
    pub card_width: f32,
    /// Image height divided by width; use 2/3 for landscape episode shelves.
    pub image_aspect: f32,
    pub gap: f32,
    pub palette: Palette,
}

impl Default for RowOptions {
    fn default() -> Self {
        Self {
            card_width: 190.0,
            image_aspect: POSTER_ASPECT,
            gap: 22.0,
            palette: theme::palette(false),
        }
    }
}

pub struct RowResponse {
    /// Card activated this frame by a pointer click.
    pub activated: Option<usize>,
    /// Index holding keyboard focus within the row (`None` when the row is empty).
    pub focused: Option<usize>,
    /// Indices of cards that were painted this frame.
    pub visible: Range<usize>,
    /// Row-level egui focus target; the focus graph chooses a card within the row.
    pub focus_response: egui::Response,
    /// Horizontal center of each card, in the UI's logical coordinate space.
    pub card_centers: Vec<f32>,
}

/// Persistent state for one row: focus index plus displayed and target scroll offsets.
#[derive(Debug, Default)]
pub struct PosterRow {
    focus: usize,
    offset: f32,
    target: f32,
    pending_focus: Option<usize>,
}

impl PosterRow {
    /// Draws the row title and cards. Keyboard input is handled while the row has focus
    /// (Left/Right move, Enter/Space activate); the mouse wheel and drag pan the row.
    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        title: &str,
        cards: &[RowCard],
        opts: &RowOptions,
    ) -> RowResponse {
        let ctx = ui.ctx().clone();
        let palette = opts.palette;
        ui.label(RichText::new(title).size(22.0).strong().color(palette.text));
        ui.add_space(8.0);

        let card_w = opts.card_width.max(1.0);
        let gap = opts.gap.max(0.0);
        let step = card_w + gap;
        let poster_h = card_w * opts.image_aspect.clamp(0.5, 2.0);
        // Room above and below the poster for the focus scale to grow into.
        let pad_y = poster_h * anim::FOCUS_SCALE_MAX * 0.5;
        let row_h = poster_h + 2.0 * pad_y + TEXT_GAP + TEXT_BLOCK_H;
        // The row reaches EDGE_PADDING into the page gutters on both sides, so the first poster
        // lines up with the row title and the page content while a focused poster still has room
        // to grow.
        let viewport_w = (ui.available_width() + 2.0 * EDGE_PADDING).max(0.0);
        let row_origin = ui.cursor().min - Vec2::new(EDGE_PADDING, 0.0);
        let row_rect = Rect::from_min_size(row_origin, Vec2::new(viewport_w, row_h));
        let count = cards.len();

        if count == 0 {
            let response = ui.allocate_rect(row_rect, Sense::hover());
            let rect = response.rect;
            ui.painter().text(
                rect.left_center() + Vec2::new(EDGE_PADDING, 0.0),
                Align2::LEFT_CENTER,
                ui_text(ids::ROW_EMPTY_NO_ITEMS),
                FontId::proportional(16.0),
                palette.muted,
            );
            let label = ui_text_args(ids::ROW_LABEL_NO_ITEMS, &[("title", title)]);
            response.widget_info(move || WidgetInfo::labeled(Role::Label, true, label.clone()));
            *self = Self::default();
            return RowResponse {
                activated: None,
                focused: None,
                visible: 0..0,
                focus_response: response,
                card_centers: Vec::new(),
            };
        }

        if let Some(index) = self.pending_focus.take() {
            self.set_focus(index.min(count - 1), card_w, gap, viewport_w);
        }
        self.focus = self.focus.min(count - 1);
        let response = ui.allocate_rect(row_rect, Sense::click_and_drag());
        let rect = response.rect;
        let max_offset = anim::max_scroll(count, card_w, gap, viewport_w);
        let row_active = response.has_focus();
        let mut activated = None;

        // Pointer: a click focuses and activates the poster under it; dragging pans.
        if response.clicked() {
            response.request_focus();
            if let Some(pos) = response.interact_pointer_pos()
                && let Some(index) = card_at(pos.x - rect.left(), self.offset, count, card_w, step)
            {
                self.set_focus(index, card_w, gap, viewport_w);
                activated = Some(index);
            }
        }
        if response.dragged() {
            self.offset = (self.offset - response.drag_delta().x).clamp(0.0, max_offset);
            self.target = self.offset;
        }
        if response.hovered() {
            let wheel = ctx.input(|i| i.smooth_scroll_delta);
            let delta = if wheel.x.abs() > wheel.y.abs() {
                wheel.x
            } else {
                wheel.y
            };
            self.target -= delta;
        }

        // Kinetic scroll: ease the displayed offset toward the target.
        let dt = ctx.input(|i| i.stable_dt).clamp(0.0, MAX_DT);
        self.target = self.target.clamp(0.0, max_offset);
        let reduce_motion = ui.style().animation_time <= 0.0;
        self.offset = if reduce_motion {
            self.target
        } else {
            anim::smooth_toward(self.offset, self.target, dt, SCROLL_HALF_LIFE)
        }
        .clamp(0.0, max_offset);
        let mut animating = (self.offset - self.target).abs() > 0.5;

        let visible = anim::visible_range(count, card_w, gap, viewport_w, self.offset);
        let painter = ui.painter_at(rect);
        let base_y = rect.top() + pad_y;
        let text_y = base_y + poster_h + pad_y + TEXT_GAP;
        for index in visible.clone() {
            let card = &cards[index];
            let goal = if row_active && index == self.focus {
                1.0
            } else {
                0.0
            };
            let t = ctx.animate_value_with_time(
                ui.scope_id().with(("poster_focus", card.id.as_str())),
                goal,
                FOCUS_ANIM_SECS,
            );
            animating |= (t - goal).abs() > 1e-3;

            let x = rect.left() + EDGE_PADDING + index as f32 * step - self.offset;
            let scale = anim::focus_scale(t);
            let poster = Rect::from_center_size(
                Pos2::new(x + card_w / 2.0, base_y + poster_h / 2.0),
                Vec2::new(card_w, poster_h) * scale,
            );
            paint_poster(&painter, poster, card, t, palette);

            let text_left = Pos2::new(x, text_y);
            let title_galley = layout_text(
                &painter,
                &card.title,
                FontId::proportional(15.0),
                palette.text,
                card_w,
                1,
            );
            painter.galley(text_left, title_galley, palette.text);
            if let Some(subtitle) = card.subtitle.as_deref() {
                let galley = layout_text(
                    &painter,
                    subtitle,
                    FontId::proportional(12.5),
                    palette.muted,
                    card_w,
                    1,
                );
                painter.galley(text_left + Vec2::new(0.0, 20.0), galley, palette.muted);
            }
        }

        if animating {
            ctx.request_repaint();
        }
        let focused_title = &cards[self.focus].title;
        let label = ui_text_args(
            ids::ROW_LABEL_COUNT_FOCUSED,
            &[
                ("title", title),
                ("count", &count.to_string()),
                ("focused_title", focused_title),
            ],
        );
        response.widget_info(move || WidgetInfo::labeled(Role::Label, true, label.clone()));

        let card_centers = (0..count)
            .map(|index| {
                rect.left() + EDGE_PADDING + index as f32 * step - self.offset + card_w / 2.0
            })
            .collect();
        RowResponse {
            activated,
            focused: Some(self.focus),
            visible,
            focus_response: response,
            card_centers,
        }
    }

    pub fn queue_focus(&mut self, index: usize) {
        self.pending_focus = Some(index);
    }

    fn set_focus(&mut self, index: usize, card_w: f32, gap: f32, viewport_w: f32) {
        self.focus = index;
        self.target = anim::scroll_offset_to_reveal(index, card_w, gap, viewport_w, self.target);
    }
}

/// Index of the card under `x` (relative to the row's left edge), given the displayed offset.
fn card_at(x: f32, offset: f32, count: usize, card_w: f32, step: f32) -> Option<usize> {
    let content = x + offset - EDGE_PADDING;
    if content < 0.0 {
        return None;
    }
    let index = (content / step).floor() as usize;
    let within = content - index as f32 * step;
    (index < count && within <= card_w).then_some(index)
}

fn paint_poster(painter: &Painter, rect: Rect, card: &RowCard, t: f32, palette: Palette) {
    match card.image {
        Some(texture) => {
            let uv = cover_uv(texture.size, rect.size());
            let shade = (204.0 + 51.0 * t.clamp(0.0, 1.0)) as u8;
            painter.add(
                egui::epaint::RectShape::filled(rect, POSTER_RADIUS, Color32::from_gray(shade))
                    .with_texture(texture.id, uv),
            );
        }
        None => {
            let (top, bottom) = seed_colors(&card.id);
            let lift = 0.25 * t.clamp(0.0, 1.0);
            // Rounded corners need a filled rectangle, so the placeholder uses the mid colour.
            let mid = Color32::from_rgb(
                ((u16::from(top.r()) + u16::from(bottom.r())) / 2) as u8,
                ((u16::from(top.g()) + u16::from(bottom.g())) / 2) as u8,
                ((u16::from(top.b()) + u16::from(bottom.b())) / 2) as u8,
            );
            painter.rect_filled(rect, POSTER_RADIUS, brighten(mid, lift));
        }
    }
    if let Some(progress) = card.progress {
        let progress = if progress.is_finite() {
            progress.clamp(0.0, 1.0)
        } else {
            0.0
        };
        // Inset and fully rounded so the bar sits inside the poster's rounded corners.
        let bar_h = 4.0;
        let inset = 12.0;
        let track = Rect::from_min_size(
            Pos2::new(rect.left() + inset, rect.bottom() - inset - bar_h),
            Vec2::new((rect.width() - 2.0 * inset).max(0.0), bar_h),
        );
        let round = CornerRadius::same(2);
        painter.rect_filled(track, round, Color32::from_black_alpha(150));
        let fill = Rect::from_min_size(track.min, Vec2::new(track.width() * progress, bar_h));
        painter.rect_filled(fill, round, palette.accent);
    }
    if t > 0.05 {
        painter.rect_stroke(
            rect.expand(3.0 * t),
            CornerRadius::same(POSTER_RADIUS.nw + 3),
            Stroke::new(2.0 * t, palette.focus),
            egui::StrokeKind::Outside,
        );
    }
}

/// Two-stop placeholder colours derived deterministically from `seed`.
pub(super) fn seed_colors(seed: &str) -> (Color32, Color32) {
    let mut hasher = DefaultHasher::new();
    seed.hash(&mut hasher);
    let hue = (hasher.finish() % 360) as f32 / 360.0;
    (hsv(hue, 0.55, 0.45), hsv((hue + 0.12) % 1.0, 0.7, 0.85))
}

pub(super) fn hsv(h: f32, s: f32, v: f32) -> Color32 {
    let i = (h * 6.0).floor();
    let f = h * 6.0 - i;
    let p = v * (1.0 - s);
    let q = v * (1.0 - f * s);
    let t = v * (1.0 - (1.0 - f) * s);
    let (r, g, b) = match (i as i32).rem_euclid(6) {
        0 => (v, t, p),
        1 => (q, v, p),
        2 => (p, v, t),
        3 => (p, q, v),
        4 => (t, p, v),
        _ => (v, p, q),
    };
    Color32::from_rgb((r * 255.0) as u8, (g * 255.0) as u8, (b * 255.0) as u8)
}

/// Mixes `c` toward white by `t` (clamped to 0..=1).
pub(super) fn brighten(c: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let mix = |x: u8| (f32::from(x) + (255.0 - f32::from(x)) * t) as u8;
    Color32::from_rgb(mix(c.r()), mix(c.g()), mix(c.b()))
}

/// Fills `rect` with a quad whose corners are coloured `[tl, tr, br, bl]`.
pub(super) fn gradient_quad(painter: &Painter, rect: Rect, corners: [Color32; 4]) {
    let mut mesh = Mesh::default();
    mesh.colored_vertex(rect.left_top(), corners[0]);
    mesh.colored_vertex(rect.right_top(), corners[1]);
    mesh.colored_vertex(rect.right_bottom(), corners[2]);
    mesh.colored_vertex(rect.left_bottom(), corners[3]);
    mesh.add_triangle(0, 1, 2);
    mesh.add_triangle(0, 2, 3);
    painter.add(Shape::mesh(mesh));
}

/// Lays out `text` wrapped to `width` and capped at `max_rows` (elided with an ellipsis).
pub(super) fn layout_text(
    painter: &Painter,
    text: &str,
    font: FontId,
    color: Color32,
    width: f32,
    max_rows: usize,
) -> Arc<Galley> {
    let mut job = LayoutJob::simple(text.to_owned(), font, color, width.max(1.0));
    job.wrap.max_rows = max_rows.max(1);
    job.wrap.break_anywhere = max_rows <= 1;
    painter.layout_job(job)
}

/// UV rectangle that fills `target` with an image of `image` size without distortion,
/// cropping the overflowing axis equally on both sides.
pub(super) fn cover_uv(image: Vec2, target: Vec2) -> Rect {
    if image.x <= 0.0 || image.y <= 0.0 || target.x <= 0.0 || target.y <= 0.0 {
        return Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0));
    }
    let image_aspect = image.x / image.y;
    let target_aspect = target.x / target.y;
    if image_aspect > target_aspect {
        let crop = (1.0 - target_aspect / image_aspect) * 0.5;
        Rect::from_min_max(Pos2::new(crop, 0.0), Pos2::new(1.0 - crop, 1.0))
    } else {
        let crop = (1.0 - image_aspect / target_aspect) * 0.5;
        Rect::from_min_max(Pos2::new(0.0, crop), Pos2::new(1.0, 1.0 - crop))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn card_at_maps_pointer_to_slots_and_gaps() {
        // card_w 200, gap 24 -> step 224; content starts at EDGE_PADDING.
        assert_eq!(card_at(EDGE_PADDING + 10.0, 0.0, 5, 200.0, 224.0), Some(0));
        assert_eq!(card_at(EDGE_PADDING + 199.0, 0.0, 5, 200.0, 224.0), Some(0));
        // In the gap between card 0 (ends at 200) and card 1 (starts at 224).
        assert_eq!(card_at(EDGE_PADDING + 210.0, 0.0, 5, 200.0, 224.0), None);
        assert_eq!(card_at(EDGE_PADDING + 215.0, 0.0, 5, 200.0, 224.0), None);
        assert_eq!(
            card_at(EDGE_PADDING + 224.0 + 5.0, 0.0, 5, 200.0, 224.0),
            Some(1)
        );
        // Offset shifts the hit test.
        assert_eq!(
            card_at(EDGE_PADDING + 10.0, 224.0, 5, 200.0, 224.0),
            Some(1)
        );
        // Left of the first card and past the last card.
        assert_eq!(card_at(0.0, 0.0, 5, 200.0, 224.0), None);
        assert_eq!(
            card_at(EDGE_PADDING + 4.0 * 224.0 + 10.0, 0.0, 5, 200.0, 224.0),
            Some(4)
        );
        assert_eq!(
            card_at(EDGE_PADDING + 5.0 * 224.0 + 10.0, 0.0, 5, 200.0, 224.0),
            None
        );
    }

    #[test]
    fn brighten_moves_toward_white_and_clamps() {
        let c = Color32::from_rgb(0, 100, 200);
        assert_eq!(brighten(c, 0.0), c);
        assert_eq!(brighten(c, 1.0), Color32::WHITE);
        assert_eq!(brighten(c, 9.0), Color32::WHITE);
        let half = brighten(c, 0.5);
        assert!(half.r() >= c.r() && half.g() > c.g() && half.b() > c.b());
    }

    #[test]
    fn seed_colors_are_deterministic_and_distinct_for_different_ids() {
        assert_eq!(seed_colors("item-a"), seed_colors("item-a"));
        assert_ne!(seed_colors("item-a"), seed_colors("item-b"));
    }

    #[test]
    fn cover_uv_crops_like_the_artwork_module() {
        let landscape = cover_uv(Vec2::new(2.0, 1.0), Vec2::new(1.0, 1.0));
        assert!((landscape.left() - 0.25).abs() < 1e-6);
        assert!((landscape.right() - 0.75).abs() < 1e-6);
        let portrait = cover_uv(Vec2::new(1.0, 2.0), Vec2::new(2.0, 1.0));
        assert!((portrait.top() - 0.375).abs() < 1e-6);
        assert!((portrait.bottom() - 0.625).abs() < 1e-6);
        let degenerate = cover_uv(Vec2::ZERO, Vec2::new(1.0, 1.0));
        assert_eq!(
            degenerate,
            Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0))
        );
    }
}
