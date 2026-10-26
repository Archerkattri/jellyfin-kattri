//! Full-width hero banner: backdrop (or gradient placeholder), a left-to-transparent scrim,
//! title, truncated overview, and Play / More Info buttons. Content fades in when the item
//! changes. Network-free: the caller supplies the decoded backdrop texture, if any.

use super::ui_text;
use egui::{Color32, FontId, Pos2, Rect, RichText, Sense, Stroke, Vec2, load::SizedTexture};
use kattri_i18n::ids;

use super::anim;
use super::row::{cover_uv, gradient_quad, layout_text, seed_colors};
use crate::theme::{self, Palette};

const SIDE_PAD: f32 = 40.0;
const BUTTON_H: f32 = 48.0;
const PLAY_W: f32 = 150.0;
const INFO_W: f32 = 150.0;
const BUTTON_GAP: f32 = 12.0;
const TEXT_MAX_W: f32 = 620.0;

#[derive(Clone, Copy)]
struct HeroLayout {
    left: f32,
    text_width: f32,
    title_size: f32,
    overview_size: f32,
    button_top: f32,
    play_rect: Rect,
    info_rect: Option<Rect>,
}

impl HeroLayout {
    fn new(rect: Rect, show_more_info: bool) -> Self {
        let side_pad = SIDE_PAD.min(rect.width().max(0.0) * 0.06);
        let content_width = (rect.width() - 2.0 * side_pad).max(0.0);
        let gap = if show_more_info { BUTTON_GAP } else { 0.0 };
        let desired_width = PLAY_W + if show_more_info { INFO_W + gap } else { 0.0 };
        let button_scale = (content_width / desired_width).clamp(0.0, 1.0);
        let play_width = PLAY_W * button_scale;
        let button_gap = gap * button_scale;
        let button_height = BUTTON_H.min(rect.height().max(0.0));
        let button_top = rect.bottom() - side_pad - button_height;
        let play_rect = Rect::from_min_size(
            Pos2::new(rect.left() + side_pad, button_top),
            Vec2::new(play_width, button_height),
        );
        let info_rect = show_more_info.then(|| {
            Rect::from_min_size(
                Pos2::new(rect.left() + side_pad + play_width + button_gap, button_top),
                Vec2::new(INFO_W * button_scale, button_height),
            )
        });
        let text_width = content_width.min(TEXT_MAX_W);
        let text_scale = (text_width / TEXT_MAX_W).clamp(0.75, 1.0);

        Self {
            left: rect.left() + side_pad,
            text_width,
            title_size: 54.0 * text_scale,
            overview_size: 18.0 * text_scale,
            button_top,
            play_rect,
            info_rect,
        }
    }
}

/// The item shown by the banner. `id` drives the change detection and placeholder colours.
pub struct HeroItem<'a> {
    pub id: &'a str,
    pub title: &'a str,
    pub overview: Option<&'a str>,
    pub backdrop: Option<SizedTexture>,
}

#[derive(Clone, Copy)]
pub struct HeroOptions {
    pub height: f32,
    /// Maximum overview lines before the text is elided.
    pub overview_lines: usize,
    pub show_more_info: bool,
    /// Duration (seconds) of the fade-in after the item changes.
    pub fade_secs: f32,
    pub palette: Palette,
}

impl Default for HeroOptions {
    fn default() -> Self {
        Self {
            height: 360.0,
            overview_lines: 3,
            show_more_info: true,
            fade_secs: 0.4,
            palette: theme::palette(false),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HeroResponse {
    pub play: bool,
    pub info: bool,
}

/// Persistent banner state: which item is showing and when it started showing.
#[derive(Debug, Default)]
pub struct HeroBanner {
    shown_id: Option<String>,
    changed_at: f64,
}

impl HeroBanner {
    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        item: &HeroItem<'_>,
        opts: &HeroOptions,
    ) -> HeroResponse {
        let ctx = ui.ctx().clone();
        let now = ctx.input(|i| i.time);
        if self.shown_id.as_deref() != Some(item.id) {
            self.shown_id = Some(item.id.to_owned());
            self.changed_at = now;
        }
        let reduce_motion = ui.style().animation_time <= 0.0;
        let alpha = hero_alpha(now, self.changed_at, opts.fade_secs, reduce_motion);
        if alpha < 1.0 {
            ctx.request_repaint();
        }

        let width = ui.available_width().max(0.0);
        let (rect, _) = ui.allocate_exact_size(Vec2::new(width, opts.height), Sense::hover());
        let painter = ui.painter_at(rect);
        let palette = opts.palette;

        // Backdrop: the image when present, otherwise a deterministic gradient.
        match item.backdrop {
            Some(backdrop) => {
                let uv = cover_uv(backdrop.size, rect.size());
                painter.image(backdrop.id, rect, uv, Color32::WHITE.gamma_multiply(alpha));
            }
            None => {
                let (top, bottom) = seed_colors(item.id);
                let (top, bottom) = (top.gamma_multiply(alpha), bottom.gamma_multiply(alpha));
                gradient_quad(&painter, rect, [top, top, bottom, bottom]);
            }
        }
        paint_scrim(&painter, rect, alpha, palette.background);

        // Text block, laid out upward from the buttons.
        let layout = HeroLayout::new(rect, opts.show_more_info);
        let block_bottom = layout.button_top - 18.0;
        let overview = item.overview.map(str::trim).filter(|text| !text.is_empty());

        let mut overview_top = block_bottom;
        if let Some(text) = overview {
            let galley = layout_text(
                &painter,
                text,
                FontId::proportional(layout.overview_size),
                palette.muted.gamma_multiply(alpha),
                layout.text_width,
                opts.overview_lines.max(1),
            );
            overview_top = block_bottom - galley.size().y;
            painter.galley(
                Pos2::new(layout.left, overview_top),
                galley,
                palette.muted.gamma_multiply(alpha),
            );
        }
        let title_galley = layout_text(
            &painter,
            item.title,
            FontId::proportional(layout.title_size),
            palette.text.gamma_multiply(alpha),
            layout.text_width,
            2,
        );
        let title_bottom = if overview.is_some() {
            overview_top - 12.0
        } else {
            block_bottom
        };
        let title_top = title_bottom - title_galley.size().y;
        painter.galley(
            Pos2::new(layout.left, title_top),
            title_galley,
            palette.text.gamma_multiply(alpha),
        );

        let play = ui.put(
            layout.play_rect,
            egui::Button::new(
                RichText::new(ui_text(ids::PLAYBACK_BUTTON_PLAY))
                    .size(18.0)
                    .strong()
                    .color(palette.background),
            )
            .fill(palette.accent)
            .stroke(Stroke::NONE)
            .corner_radius(egui::CornerRadius::same((BUTTON_H / 2.0) as u8))
            .min_size(layout.play_rect.size()),
        );
        let mut info = false;
        if let Some(info_rect) = layout.info_rect {
            info = ui
                .put(
                    info_rect,
                    egui::Button::new(
                        RichText::new(ui_text(ids::HERO_BUTTON_MORE_INFO)).size(18.0),
                    )
                    .fill(Color32::from_white_alpha(46))
                    .stroke(Stroke::NONE)
                    .corner_radius(egui::CornerRadius::same((BUTTON_H / 2.0) as u8))
                    .min_size(info_rect.size()),
                )
                .clicked();
        }

        HeroResponse {
            play: play.clicked(),
            info,
        }
    }
}

fn hero_alpha(now: f64, changed_at: f64, duration: f32, reduce_motion: bool) -> f32 {
    if reduce_motion {
        1.0
    } else {
        anim::fade_alpha((now - changed_at).max(0.0), duration)
    }
}

/// Darkens the left side (behind the text) and the bottom edge, fading to transparent.
fn paint_scrim(painter: &egui::Painter, rect: Rect, alpha: f32, page: Color32) {
    let dark = Color32::from_black_alpha(190).gamma_multiply(alpha);
    let clear = Color32::TRANSPARENT;
    // The bottom fades into the page colour so the hero melts into the shelves below.
    let page_solid = page.gamma_multiply(alpha);

    let left_band = Rect::from_min_max(
        rect.left_top(),
        Pos2::new(rect.left() + rect.width() * 0.6, rect.bottom()),
    );
    gradient_quad(painter, left_band, [dark, clear, clear, dark]);

    let bottom_band = Rect::from_min_max(
        Pos2::new(rect.left(), rect.top() + rect.height() * 0.45),
        rect.right_bottom(),
    );
    gradient_quad(painter, bottom_band, [clear, clear, page_solid, page_solid]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_options_are_sane() {
        let opts = HeroOptions::default();
        assert!(opts.height > 0.0);
        assert!(opts.overview_lines >= 1);
        assert!(opts.fade_secs > 0.0);
    }

    #[test]
    fn hero_response_defaults_to_no_action() {
        assert_eq!(
            HeroResponse::default(),
            HeroResponse {
                play: false,
                info: false
            }
        );
    }

    #[test]
    fn responsive_layout_keeps_text_and_actions_inside_narrow_and_wide_heroes() {
        for width in [320.0, 960.0] {
            let rect = Rect::from_min_size(Pos2::ZERO, Vec2::new(width, 360.0));
            let layout = HeroLayout::new(rect, true);
            let info_rect = layout.info_rect.expect("More Info is enabled");

            assert!(layout.text_width > 0.0 && layout.text_width <= rect.width());
            assert!(layout.left >= rect.left());
            assert!(layout.left + layout.text_width <= rect.right());
            assert!(layout.play_rect.width() > 0.0);
            assert!(info_rect.width() > 0.0);
            assert!(rect.contains_rect(layout.play_rect));
            assert!(rect.contains_rect(info_rect));
            assert!(layout.play_rect.right() <= info_rect.left());
        }
    }

    #[test]
    fn responsive_layout_keeps_play_reachable_without_more_info() {
        let rect = Rect::from_min_size(Pos2::ZERO, Vec2::new(320.0, 360.0));
        let layout = HeroLayout::new(rect, false);

        assert!(layout.info_rect.is_none());
        assert!(layout.play_rect.width() > 0.0);
        assert!(rect.contains_rect(layout.play_rect));
    }

    #[test]
    fn hero_fade_respects_normal_and_reduced_motion() {
        assert_eq!(hero_alpha(10.0, 10.0, 0.4, false), 0.0);
        assert!(hero_alpha(10.2, 10.0, 0.4, false) > 0.0);
        assert_eq!(hero_alpha(10.0, 10.0, 0.4, true), 1.0);
    }
}
