use super::ui_text;
use crate::catalog::{ArtworkMotif, ArtworkPalette, MediaItem};
use crate::theme::{self, Palette};
use egui::{self, Color32, CornerRadius, FontId, Rect, Sense, Stroke};
use kattri_i18n::ids;

/// Width of a poster card in a shelf or grid, in points.
pub const CARD_WIDTH: f32 = 150.0;

pub fn media_card(
    ui: &mut egui::Ui,
    item: &MediaItem,
    colors: Palette,
    selected: bool,
) -> egui::Response {
    let width = CARD_WIDTH;
    let height = 246.0;
    let poster_height = 188.0;
    let (rect, response) = ui.allocate_exact_size(egui::vec2(width, height), Sense::click());
    let art_rect = Rect::from_min_size(rect.min, egui::vec2(width, poster_height));
    // A library page can hold a thousand cards; only the ones on screen are painted.
    if ui.is_rect_visible(rect) {
        let painter = ui.painter_at(rect);

        if let Some(texture) = item.artwork_texture.as_ref() {
            painter.add(
                egui::epaint::RectShape::filled(art_rect, CornerRadius::same(10), Color32::WHITE)
                    .with_texture(texture.id(), cover_uv(texture.size_vec2(), art_rect.size())),
            );
        } else {
            paint_artwork(&painter, art_rect, item.artwork);
        }
        painter.galley(
            egui::pos2(rect.left() + 1.0, art_rect.bottom() + 10.0),
            super::row::layout_text(
                &painter,
                &item.title,
                FontId::proportional(15.0),
                colors.text,
                width - 2.0,
                1,
            ),
            colors.text,
        );
        painter.galley(
            egui::pos2(rect.left() + 1.0, art_rect.bottom() + 31.0),
            super::row::layout_text(
                &painter,
                item.subtitle(),
                FontId::proportional(11.5),
                colors.muted,
                width - 2.0,
                1,
            ),
            colors.muted,
        );

        if response.has_focus() {
            painter.rect_stroke(
                art_rect.expand(2.0),
                CornerRadius::same(9),
                Stroke::new(2.5, colors.focus),
                egui::StrokeKind::Outside,
            );
        } else if selected || response.hovered() {
            painter.rect_stroke(
                art_rect.expand(1.0),
                CornerRadius::same(8),
                Stroke::new(if selected { 2.0 } else { 1.5 }, colors.accent),
                egui::StrokeKind::Outside,
            );
        }
    }

    response.widget_info(|| {
        egui::WidgetInfo::selected(egui::Role::Button, true, selected, card_label(item))
    });
    response.on_hover_ui(|ui| {
        ui.label(card_label(item));
    })
}

/// The spoken and tooltip description of a card. Built only when accessibility or a tooltip
/// asks for it, not for every card on every frame.
fn card_label(item: &MediaItem) -> String {
    let artwork_description = if item.artwork_texture.is_some() {
        ui_text(ids::ARTWORK_DESCRIPTION_PRIMARY)
    } else {
        ui_text(ids::ARTWORK_DESCRIPTION_PLACEHOLDER)
    };
    let mut label = format!(
        "{}, {}. {}. {artwork_description} ",
        item.title,
        item.kind.label(),
        item.subtitle()
    );
    if let Some(overview) = item
        .overview
        .as_deref()
        .map(str::trim)
        .filter(|overview| !overview.is_empty())
    {
        let mut overview_chars = overview.chars();
        let preview = overview_chars.by_ref().take(220).collect::<String>();
        label.push_str(ui_text(ids::ARTWORK_LABEL_SUMMARY));
        label.push(' ');
        label.push_str(&preview);
        if overview_chars.next().is_some() {
            label.push('…');
        }
        label.push_str(". ");
    }
    label.push_str(ui_text(ids::ARTWORK_HINT_LOAD_DETAILS));
    label
}

pub fn paint_artwork(painter: &egui::Painter, rect: Rect, art: ArtworkPalette) {
    paint_vertical_gradient(painter, rect, art.top, art.bottom);
    match art.motif {
        ArtworkMotif::Horizon => horizon(painter, rect, art),
        ArtworkMotif::Orbit => orbit(painter, rect, art),
        ArtworkMotif::Peaks => peaks(painter, rect, art),
        ArtworkMotif::Arches => arches(painter, rect, art),
        ArtworkMotif::Sound => sound(painter, rect, art),
    }
    painter.rect_filled(
        rect,
        CornerRadius::same(8),
        Color32::from_rgba_unmultiplied(0, 0, 0, 7),
    );
}

fn paint_vertical_gradient(painter: &egui::Painter, rect: Rect, top: [u8; 3], bottom: [u8; 3]) {
    const STEPS: usize = 32;
    for index in 0..STEPS {
        let t = index as f32 / (STEPS - 1) as f32;
        let color = Color32::from_rgb(
            lerp(top[0], bottom[0], t),
            lerp(top[1], bottom[1], t),
            lerp(top[2], bottom[2], t),
        );
        let y0 = rect.top() + rect.height() * index as f32 / STEPS as f32;
        let y1 = rect.top() + rect.height() * (index + 1) as f32 / STEPS as f32 + 0.6;
        painter.rect_filled(
            Rect::from_min_max(egui::pos2(rect.left(), y0), egui::pos2(rect.right(), y1)),
            CornerRadius::ZERO,
            color,
        );
    }
}

fn cover_uv(image_size: egui::Vec2, target_size: egui::Vec2) -> Rect {
    if image_size.x <= 0.0 || image_size.y <= 0.0 || target_size.x <= 0.0 || target_size.y <= 0.0 {
        return Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0));
    }
    let image_aspect = image_size.x / image_size.y;
    let target_aspect = target_size.x / target_size.y;
    if image_aspect > target_aspect {
        let visible_width = target_aspect / image_aspect;
        let crop = (1.0 - visible_width) * 0.5;
        Rect::from_min_max(egui::pos2(crop, 0.0), egui::pos2(1.0 - crop, 1.0))
    } else {
        let visible_height = image_aspect / target_aspect;
        let crop = (1.0 - visible_height) * 0.5;
        Rect::from_min_max(egui::pos2(0.0, crop), egui::pos2(1.0, 1.0 - crop))
    }
}

fn horizon(painter: &egui::Painter, rect: Rect, art: ArtworkPalette) {
    let center = egui::pos2(
        rect.left() + rect.width() * 0.72,
        rect.top() + rect.height() * 0.39,
    );
    let radius = rect.width().min(rect.height()) * 0.24;
    painter.circle_filled(center, radius, theme::color(art.detail));
    painter.rect_filled(
        Rect::from_min_max(
            egui::pos2(rect.left(), rect.top() + rect.height() * 0.62),
            rect.right_bottom(),
        ),
        CornerRadius::ZERO,
        Color32::from_rgba_unmultiplied(10, 22, 31, 150),
    );
    for line in 0..5 {
        let y = rect.top() + rect.height() * (0.71 + line as f32 * 0.055);
        painter.line_segment(
            [
                egui::pos2(rect.left() + rect.width() * 0.1, y),
                egui::pos2(rect.right(), y),
            ],
            Stroke::new(1.0, Color32::from_rgba_unmultiplied(220, 229, 206, 48)),
        );
    }
    painter.circle_filled(center, radius * 0.68, theme::color(art.shape));
}

fn orbit(painter: &egui::Painter, rect: Rect, art: ArtworkPalette) {
    let center = egui::pos2(
        rect.center().x + rect.width() * 0.08,
        rect.center().y - rect.height() * 0.04,
    );
    let radius = rect.height().min(rect.width()) * 0.24;
    painter.circle_filled(center, radius, theme::color(art.shape));
    painter.circle_stroke(
        center,
        radius * 1.45,
        Stroke::new(2.0, Color32::from_rgba_unmultiplied(228, 237, 224, 160)),
    );
    painter.circle_stroke(
        center,
        radius * 1.84,
        Stroke::new(1.0, Color32::from_rgba_unmultiplied(228, 237, 224, 86)),
    );
    painter.circle_filled(
        egui::pos2(center.x + radius * 1.15, center.y - radius * 0.84),
        radius * 0.12,
        theme::color(art.detail),
    );
}

fn peaks(painter: &egui::Painter, rect: Rect, art: ArtworkPalette) {
    let left = rect.left();
    let right = rect.right();
    let base = rect.bottom();
    let back = vec![
        egui::pos2(left, base - rect.height() * 0.38),
        egui::pos2(
            left + rect.width() * 0.28,
            rect.top() + rect.height() * 0.20,
        ),
        egui::pos2(left + rect.width() * 0.55, base - rect.height() * 0.22),
        egui::pos2(right, rect.top() + rect.height() * 0.39),
        egui::pos2(right, base),
        egui::pos2(left, base),
    ];
    painter.add(egui::Shape::convex_polygon(
        back,
        theme::color(art.shape),
        Stroke::NONE,
    ));
    let foreground = vec![
        egui::pos2(left, base - rect.height() * 0.2),
        egui::pos2(left + rect.width() * 0.43, base - rect.height() * 0.58),
        egui::pos2(right, base - rect.height() * 0.12),
        egui::pos2(right, base),
        egui::pos2(left, base),
    ];
    painter.add(egui::Shape::convex_polygon(
        foreground,
        Color32::from_rgba_unmultiplied(8, 18, 25, 180),
        Stroke::NONE,
    ));
    painter.line_segment(
        [
            egui::pos2(left + rect.width() * 0.28, rect.top() + rect.height() * 0.2),
            egui::pos2(
                left + rect.width() * 0.38,
                rect.top() + rect.height() * 0.38,
            ),
        ],
        Stroke::new(2.0, theme::color(art.detail)),
    );
}

fn arches(painter: &egui::Painter, rect: Rect, art: ArtworkPalette) {
    let base = rect.bottom() + rect.height() * 0.08;
    for index in 0..3 {
        let inset = rect.width() * (0.12 + index as f32 * 0.16);
        let arch = Rect::from_min_max(
            egui::pos2(
                rect.left() + inset,
                rect.top() + rect.height() * (0.14 + index as f32 * 0.08),
            ),
            egui::pos2(rect.right() - inset * 0.55, base),
        );
        painter.rect_stroke(
            arch,
            CornerRadius::same(48),
            Stroke::new(
                2.0,
                if index == 0 {
                    theme::color(art.detail)
                } else {
                    theme::color(art.shape)
                },
            ),
            egui::StrokeKind::Inside,
        );
    }
    painter.circle_filled(
        egui::pos2(
            rect.left() + rect.width() * 0.25,
            rect.top() + rect.height() * 0.72,
        ),
        rect.width() * 0.06,
        theme::color(art.detail),
    );
}

fn sound(painter: &egui::Painter, rect: Rect, art: ArtworkPalette) {
    let center = rect.center();
    let columns = 11;
    for index in 0..columns {
        let phase = index as f32 / columns as f32;
        let height = rect.height() * (0.16 + (phase * std::f32::consts::PI).sin().abs() * 0.43);
        let x = rect.left() + rect.width() * (0.12 + phase * 0.76);
        painter.line_segment(
            [
                egui::pos2(x, center.y - height * 0.5),
                egui::pos2(x, center.y + height * 0.5),
            ],
            Stroke::new(
                if index % 3 == 0 { 3.0 } else { 1.6 },
                if index % 2 == 0 {
                    theme::color(art.detail)
                } else {
                    theme::color(art.shape)
                },
            ),
        );
    }
    painter.circle_stroke(
        center,
        rect.width().min(rect.height()) * 0.34,
        Stroke::new(1.0, Color32::from_rgba_unmultiplied(240, 241, 226, 80)),
    );
}

fn lerp(start: u8, end: u8, t: f32) -> u8 {
    (f32::from(start) + (f32::from(end) - f32::from(start)) * t).round() as u8
}

#[cfg(test)]
mod cover_crop_tests {
    use super::cover_uv;
    use egui::vec2;

    #[test]
    fn crops_landscape_images_horizontally_to_fill_a_poster() {
        let uv = cover_uv(vec2(2.0, 1.0), vec2(1.0, 1.0));
        assert!((uv.left() - 0.25).abs() < f32::EPSILON);
        assert!((uv.right() - 0.75).abs() < f32::EPSILON);
        assert_eq!(uv.top(), 0.0);
        assert_eq!(uv.bottom(), 1.0);
    }

    #[test]
    fn crops_portrait_images_vertically_to_fill_a_wide_card() {
        let uv = cover_uv(vec2(1.0, 2.0), vec2(2.0, 1.0));
        assert_eq!(uv.left(), 0.0);
        assert_eq!(uv.right(), 1.0);
        assert!((uv.top() - 0.375).abs() < f32::EPSILON);
        assert!((uv.bottom() - 0.625).abs() < f32::EPSILON);
    }
}
