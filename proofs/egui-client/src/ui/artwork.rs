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
    media_card_scaled(ui, item, colors, selected, 1.0)
}

/// Poster card scaled for the library's image-size preference.
pub fn media_card_scaled(
    ui: &mut egui::Ui,
    item: &MediaItem,
    colors: Palette,
    selected: bool,
    scale: f32,
) -> egui::Response {
    let scale = scale.clamp(0.65, 1.55);
    let width = CARD_WIDTH * scale;
    // Room for a two-line title and one muted meta line, so long titles wrap instead of losing text.
    let height = 266.0 * scale;
    let poster_height = 188.0 * scale;
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
        let title = super::row::layout_text(
            &painter,
            &item.title,
            FontId::proportional(15.0 * scale),
            colors.text,
            width - 2.0,
            2,
        );
        let title_height = title.size().y;
        painter.galley(
            egui::pos2(rect.left() + 1.0, art_rect.bottom() + 10.0),
            title,
            colors.text,
        );
        // A year, runtime and rating say more than repeating the item type under every poster.
        let meta = item.meta_line();
        painter.galley(
            egui::pos2(rect.left() + 1.0, art_rect.bottom() + 12.0 + title_height),
            super::row::layout_text(
                &painter,
                if meta.is_empty() {
                    item.subtitle()
                } else {
                    &meta
                },
                FontId::proportional(11.5 * scale),
                colors.muted,
                width - 2.0,
                1,
            ),
            colors.muted,
        );

        if response.has_focus() {
            painter.rect_stroke(
                art_rect,
                CornerRadius::same(10),
                Stroke::new(2.5, colors.focus),
                egui::StrokeKind::Inside,
            );
        } else if selected || response.hovered() {
            painter.rect_stroke(
                art_rect,
                CornerRadius::same(10),
                Stroke::new(if selected { 2.0 } else { 1.5 }, colors.accent),
                egui::StrokeKind::Inside,
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
    // Clip every motif to the tile so a vector placeholder can never spill into
    // neighbouring cells or the title strip below the poster.
    let clipped = painter.with_clip_rect(rect);
    paint_vertical_gradient(&clipped, rect, art.top, art.bottom);
    match art.motif {
        ArtworkMotif::Horizon => horizon(&clipped, rect, art),
        ArtworkMotif::Orbit => orbit(&clipped, rect, art),
        ArtworkMotif::Peaks => peaks(&clipped, rect, art),
        ArtworkMotif::Arches => arches(&clipped, rect, art),
        ArtworkMotif::Sound => sound(&clipped, rect, art),
    }
    clipped.rect_filled(
        rect,
        CornerRadius::same(10),
        Color32::from_rgba_unmultiplied(0, 0, 0, 7),
    );
}

fn rounded_rect_points(rect: Rect, radius: f32) -> Vec<egui::Pos2> {
    let r = radius.clamp(0.0, (rect.width() * 0.5).min(rect.height() * 0.5));
    if r <= 0.0 {
        return vec![
            rect.left_top(),
            rect.right_top(),
            rect.right_bottom(),
            rect.left_bottom(),
        ];
    }
    const STEPS: usize = 6;
    let corners = [
        (
            egui::pos2(rect.left() + r, rect.top() + r),
            std::f32::consts::PI,
            std::f32::consts::FRAC_PI_2 * 3.0,
        ),
        (
            egui::pos2(rect.right() - r, rect.top() + r),
            std::f32::consts::FRAC_PI_2 * 3.0,
            std::f32::consts::TAU,
        ),
        (
            egui::pos2(rect.right() - r, rect.bottom() - r),
            0.0,
            std::f32::consts::FRAC_PI_2,
        ),
        (
            egui::pos2(rect.left() + r, rect.bottom() - r),
            std::f32::consts::FRAC_PI_2,
            std::f32::consts::PI,
        ),
    ];
    let mut points = Vec::with_capacity(corners.len() * (STEPS + 1));
    for (center, start_angle, end_angle) in corners {
        for step in 0..=STEPS {
            let t = step as f32 / STEPS as f32;
            let angle = start_angle + (end_angle - start_angle) * t;
            let (sin, cos) = angle.sin_cos();
            points.push(egui::pos2(center.x + cos * r, center.y + sin * r));
        }
    }
    points
}

fn paint_vertical_gradient(painter: &egui::Painter, rect: Rect, top: [u8; 3], bottom: [u8; 3]) {
    let points = rounded_rect_points(rect, 10.0);
    if points.len() < 3 || rect.height() <= 0.0 {
        return;
    }
    let color_at = |y: f32| {
        let t = ((y - rect.top()) / rect.height()).clamp(0.0, 1.0);
        Color32::from_rgb(
            lerp(top[0], bottom[0], t),
            lerp(top[1], bottom[1], t),
            lerp(top[2], bottom[2], t),
        )
    };
    let mut mesh = egui::epaint::Mesh::default();
    let center = rect.center();
    mesh.colored_vertex(center, color_at(center.y));
    for pos in &points {
        mesh.colored_vertex(*pos, color_at(pos.y));
    }
    let count = points.len() as u32;
    for index in 0..count {
        let next = if index + 1 == count { 1 } else { index + 2 };
        mesh.add_triangle(0, index + 1, next);
    }
    painter.add(egui::Shape::mesh(mesh));
}

pub(crate) fn cover_uv(image_size: egui::Vec2, target_size: egui::Vec2) -> Rect {
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
        CornerRadius {
            nw: 0,
            ne: 0,
            sw: 10,
            se: 10,
        },
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

fn append_bottom_rounded_corners(points: &mut Vec<egui::Pos2>, rect: Rect, radius: f32) {
    let r = radius.clamp(0.0, (rect.width() * 0.5).min(rect.height() * 0.11));
    if r <= 0.0 {
        points.push(rect.right_bottom());
        points.push(rect.left_bottom());
        return;
    }
    const STEPS: usize = 6;
    let br_center = egui::pos2(rect.right() - r, rect.bottom() - r);
    for step in 0..=STEPS {
        let angle = (step as f32 / STEPS as f32) * std::f32::consts::FRAC_PI_2;
        let (sin, cos) = angle.sin_cos();
        points.push(egui::pos2(br_center.x + cos * r, br_center.y + sin * r));
    }
    let bl_center = egui::pos2(rect.left() + r, rect.bottom() - r);
    for step in 0..=STEPS {
        let angle = std::f32::consts::FRAC_PI_2
            + (step as f32 / STEPS as f32) * std::f32::consts::FRAC_PI_2;
        let (sin, cos) = angle.sin_cos();
        points.push(egui::pos2(bl_center.x + cos * r, bl_center.y + sin * r));
    }
}

fn peaks(painter: &egui::Painter, rect: Rect, art: ArtworkPalette) {
    let left = rect.left();
    let right = rect.right();
    let base = rect.bottom();
    let mut back = vec![
        egui::pos2(left, base - rect.height() * 0.38),
        egui::pos2(
            left + rect.width() * 0.28,
            rect.top() + rect.height() * 0.20,
        ),
        egui::pos2(left + rect.width() * 0.55, base - rect.height() * 0.22),
        egui::pos2(right, rect.top() + rect.height() * 0.39),
    ];
    append_bottom_rounded_corners(&mut back, rect, 10.0);
    painter.add(egui::Shape::convex_polygon(
        back,
        theme::color(art.shape),
        Stroke::NONE,
    ));
    let mut foreground = vec![
        egui::pos2(left, base - rect.height() * 0.2),
        egui::pos2(left + rect.width() * 0.43, base - rect.height() * 0.58),
        egui::pos2(right, base - rect.height() * 0.12),
    ];
    append_bottom_rounded_corners(&mut foreground, rect, 10.0);
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
    let base = rect.bottom();
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
mod paint_artwork_clip_tests {
    use super::{ArtworkMotif, ArtworkPalette, paint_artwork};
    use egui::{Pos2, RawInput, Rect, Vec2};

    #[test]
    fn paint_artwork_clips_every_motif_to_tile() {
        let context = egui::Context::default();
        let rect = Rect::from_min_size(Pos2::ZERO, Vec2::new(150.0, 188.0));
        let motifs = [
            ArtworkMotif::Horizon,
            ArtworkMotif::Orbit,
            ArtworkMotif::Peaks,
            ArtworkMotif::Arches,
            ArtworkMotif::Sound,
        ];
        for motif in motifs {
            let art = ArtworkPalette {
                top: [10, 20, 30],
                bottom: [30, 20, 10],
                shape: [50, 60, 70],
                detail: [70, 60, 50],
                motif,
            };
            let mut output = context.run_ui(
                RawInput {
                    screen_rect: Some(rect),
                    ..Default::default()
                },
                |ui| {
                    let painter = ui.painter_at(rect);
                    paint_artwork(&painter, rect, art);
                },
            );
            output.textures_delta.clear();
            assert!(
                !output.shapes.is_empty(),
                "motif {motif:?} produced no shapes"
            );
            for clipped in &output.shapes {
                assert_eq!(
                    clipped.clip_rect, rect,
                    "motif {motif:?} left a non-tile clip rect"
                );
            }
        }
    }
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
