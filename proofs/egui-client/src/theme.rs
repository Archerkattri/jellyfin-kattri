use egui::{self, Color32, Stroke};

#[derive(Clone, Copy)]
pub struct Palette {
    pub background: Color32,
    pub panel: Color32,
    pub raised: Color32,
    pub text: Color32,
    pub muted: Color32,
    pub accent: Color32,
    pub focus: Color32,
    pub divider: Color32,
}

pub const NIGHT_SLATE: Color32 = Color32::from_rgb(0x0A, 0x0A, 0x0C);
pub const PANEL: Color32 = Color32::from_rgb(0x15, 0x15, 0x17);
pub const RAISED: Color32 = Color32::from_rgb(0x2A, 0x2A, 0x2D);
pub const PRIMARY_TEXT: Color32 = Color32::from_rgb(0xF5, 0xF5, 0xF7);
pub const MUTED_TEXT: Color32 = Color32::from_rgb(0xA1, 0xA1, 0xA6);
pub const JELLYFIN_BLUE: Color32 = Color32::from_rgb(0x00, 0xA4, 0xDC);
const SELECTION_BLUE: Color32 = Color32::from_rgb(0x48, 0x48, 0x4C);
const ACTIVE_FILL_HIGH_CONTRAST: Color32 = Color32::from_rgb(0x00, 0x68, 0x90);
const HOVER_FILL: Color32 = Color32::from_rgb(0x3A, 0x3A, 0x3E);
/// Fill of text fields, a step above the page so an empty field is still visible.
pub const FIELD: Color32 = Color32::from_rgb(0x1E, 0x1E, 0x22);
/// Hairline border of text fields and quiet controls.
const HAIRLINE: Color32 = Color32::from_rgb(0x33, 0x33, 0x38);

/// Type scale, in points. Body text is 14 pt so long settings pages stay readable from a
/// laptop's viewing distance.
const SMALL_SIZE: f32 = 11.5;
const BODY_SIZE: f32 = 14.0;
const HEADING_SIZE: f32 = 22.0;

pub fn palette(high_contrast: bool) -> Palette {
    if high_contrast {
        Palette {
            background: NIGHT_SLATE,
            panel: PANEL,
            raised: Color32::from_rgb(0x2D, 0x2D, 0x2D),
            text: Color32::WHITE,
            muted: Color32::from_rgb(0xE0, 0xE0, 0xE0),
            accent: JELLYFIN_BLUE,
            focus: Color32::WHITE,
            divider: Color32::from_rgb(0x8A, 0x8A, 0x8A),
        }
    } else {
        Palette {
            background: NIGHT_SLATE,
            panel: PANEL,
            raised: RAISED,
            text: PRIMARY_TEXT,
            muted: MUTED_TEXT,
            accent: Color32::from_rgb(0xF5, 0xF5, 0xF7),
            focus: Color32::WHITE,
            divider: Color32::from_rgb(0x2C, 0x2C, 0x2F),
        }
    }
}

/// The Apple TV typeface: SF Pro Display. Bundled for this personal build at the owner's
/// request; Apple's font licence does not permit redistribution, so a public build should swap
/// this for the Inter fallback below.
const UI_FONT: &[u8] = include_bytes!("../../../assets/fonts/SF-Pro-Display-Regular.otf");
/// Open-licensed fallback for glyphs SF Pro does not carry and for public redistribution.
const FALLBACK_UI_FONT: &[u8] = include_bytes!("../../../assets/fonts/Inter.ttf");

/// Install the fonts once at start-up: SF Pro Display, then Inter, then egui's own proportional
/// font, then the monospace family for arrows and geometric shapes (`◌`, `▾`) that neither
/// proportional face carries. Without the last fallback those characters render as boxes.
pub fn install_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        "sf-pro".to_owned(),
        std::sync::Arc::new(egui::FontData::from_static(UI_FONT)),
    );
    fonts.font_data.insert(
        "inter".to_owned(),
        std::sync::Arc::new(egui::FontData::from_static(FALLBACK_UI_FONT)),
    );
    let monospace = fonts
        .families
        .get(&egui::FontFamily::Monospace)
        .and_then(|family| family.first().cloned());
    if let Some(proportional) = fonts.families.get_mut(&egui::FontFamily::Proportional) {
        proportional.insert(0, "inter".to_owned());
        proportional.insert(0, "sf-pro".to_owned());
        if let Some(fallback) = monospace
            && !proportional.contains(&fallback)
        {
            proportional.push(fallback);
        }
    }
    ctx.set_fonts(fonts);
}

pub fn apply(ctx: &egui::Context, high_contrast: bool, reduce_motion: bool) {
    // The whole style is rebuilt only when the flags change. Reassigning every Visuals field on
    // every frame is pure waste: the draw loop calls this before each pass.
    let applied = egui::Id::unique("kattri-theme-applied");
    let wanted = (high_contrast, reduce_motion);
    if ctx.data(|data| data.get_temp::<(bool, bool)>(applied)) == Some(wanted) {
        return;
    }
    ctx.data_mut(|data| data.insert_temp(applied, wanted));
    let colors = palette(high_contrast);
    ctx.global_style_mut(|style| {
        style.visuals = egui::Visuals::dark();
        style.visuals.panel_fill = colors.background;
        style.visuals.window_fill = colors.panel;
        style.visuals.extreme_bg_color = colors.background;
        style.visuals.text_edit_bg_color = Some(FIELD);
        style.visuals.faint_bg_color = colors.panel;
        // A global text override would mask the dark foreground on blue active controls.
        style.visuals.override_text_color = None;
        style.visuals.selection.bg_fill = SELECTION_BLUE;
        style.visuals.selection.stroke = Stroke::new(1.0, colors.text);
        style.visuals.widgets.noninteractive.bg_fill = colors.panel;
        style.visuals.widgets.noninteractive.fg_stroke.color = colors.text;
        style.visuals.widgets.inactive.bg_fill = colors.raised;
        style.visuals.widgets.inactive.fg_stroke.color = colors.text;
        // A hairline outlines text fields and checkboxes, which otherwise vanish on the page.
        style.visuals.widgets.inactive.bg_stroke = Stroke::new(
            1.0,
            if high_contrast {
                colors.divider
            } else {
                HAIRLINE
            },
        );
        style.visuals.widgets.hovered.bg_fill = HOVER_FILL;
        style.visuals.widgets.hovered.fg_stroke.color = colors.text;
        // egui draws `strong()` text, and the text of a pressed widget, in this foreground, so it
        // must be light. Pressed widgets get a mid-grey fill (a deep blue in high contrast).
        style.visuals.widgets.active.bg_fill = if high_contrast {
            ACTIVE_FILL_HIGH_CONTRAST
        } else {
            SELECTION_BLUE
        };
        style.visuals.widgets.active.fg_stroke.color = colors.text;
        // egui 0.36 resolves keyboard-focused widgets through the active visual state.
        style.visuals.widgets.active.bg_stroke = Stroke::new(2.5, colors.focus);
        style.visuals.widgets.noninteractive.corner_radius = egui::CornerRadius::same(10);
        style.visuals.widgets.inactive.corner_radius = egui::CornerRadius::same(10);
        style.visuals.widgets.hovered.corner_radius = egui::CornerRadius::same(10);
        style.visuals.widgets.active.corner_radius = egui::CornerRadius::same(10);
        style.visuals.window_corner_radius = egui::CornerRadius::same(18);
        style.visuals.menu_corner_radius = egui::CornerRadius::same(14);
        style.spacing.item_spacing = egui::vec2(14.0, 10.0);
        style.spacing.button_padding = egui::vec2(14.0, 7.0);
        style.spacing.interact_size.y = 30.0;
        // Sliders fill the track up to the handle, as system controls do.
        style.visuals.slider_trailing_fill = true;
        style.spacing.slider_width = 200.0;
        for (text_style, size) in [
            (egui::TextStyle::Small, SMALL_SIZE),
            (egui::TextStyle::Body, BODY_SIZE),
            (egui::TextStyle::Button, BODY_SIZE),
            (egui::TextStyle::Heading, HEADING_SIZE),
        ] {
            if let Some(font) = style.text_styles.get_mut(&text_style) {
                font.size = size;
            }
        }
        style.animation_time = if reduce_motion { 0.0 } else { 0.12 };
    });
}

/// Scale the whole interface to the chosen size. egui repaints only when the factor changes.
pub fn apply_interface_size(ctx: &egui::Context, size: kattri_settings::InterfaceSize) {
    let factor = size.zoom_factor();
    if (ctx.zoom_factor() - factor).abs() > f32::EPSILON {
        ctx.set_zoom_factor(factor);
    }
}

pub fn color(rgb: [u8; 3]) -> Color32 {
    Color32::from_rgb(rgb[0], rgb[1], rgb[2])
}

/// WCAG 2.1 contrast ratio, from 1.0 to 21.0. It is test-only: the palette tokens are the input,
/// and the check runs in `cargo test` rather than on each frame.
#[cfg(test)]
fn contrast_ratio(foreground: Color32, background: Color32) -> f32 {
    fn luminance(color: Color32) -> f32 {
        let linear = |channel: u8| {
            let channel = f32::from(channel) / 255.0;
            if channel <= 0.04045 {
                channel / 12.92
            } else {
                ((channel + 0.055) / 1.055).powf(2.4)
            }
        };

        0.2126 * linear(color.r()) + 0.7152 * linear(color.g()) + 0.0722 * linear(color.b())
    }

    let foreground = luminance(foreground);
    let background = luminance(background);
    (foreground.max(background) + 0.05) / (foreground.min(background) + 0.05)
}

/// WCAG 2.1 AA minimum for normal text.
#[cfg(test)]
const AA_TEXT_MIN: f32 = 4.5;

/// WCAG 2.1 AA minimum for a non-text focus indicator.
#[cfg(test)]
const AA_NON_TEXT_MIN: f32 = 3.0;

/// The text and focus pairs of the default (not high-contrast) theme, each with its AA minimum.
#[cfg(test)]
fn default_theme_pairs() -> Vec<(&'static str, Color32, Color32, f32)> {
    let colors = palette(false);
    vec![
        (
            "text on background",
            colors.text,
            colors.background,
            AA_TEXT_MIN,
        ),
        ("text on panel", colors.text, colors.panel, AA_TEXT_MIN),
        ("text on raised", colors.text, colors.raised, AA_TEXT_MIN),
        (
            "muted text on background",
            colors.muted,
            colors.background,
            AA_TEXT_MIN,
        ),
        (
            "muted text on panel",
            colors.muted,
            colors.panel,
            AA_TEXT_MIN,
        ),
        (
            "muted text on raised",
            colors.muted,
            colors.raised,
            AA_TEXT_MIN,
        ),
        (
            "text on hovered control",
            colors.text,
            HOVER_FILL,
            AA_TEXT_MIN,
        ),
        (
            "background on accent (active control)",
            colors.background,
            colors.accent,
            AA_TEXT_MIN,
        ),
        (
            "focus ring on background",
            colors.focus,
            colors.background,
            AA_NON_TEXT_MIN,
        ),
        (
            "focus ring on panel",
            colors.focus,
            colors.panel,
            AA_NON_TEXT_MIN,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contrast_ratio_passes_and_fails_known_pairs() {
        // Black on white is the 21:1 maximum.
        let maximum = contrast_ratio(Color32::BLACK, Color32::WHITE);
        assert!(
            (maximum - 21.0).abs() < 0.01,
            "black on white was {maximum}"
        );

        // #767676 on white is 4.54:1, the lightest grey that passes AA text.
        let passing = contrast_ratio(Color32::from_rgb(0x76, 0x76, 0x76), Color32::WHITE);
        assert!(passing >= AA_TEXT_MIN, "#767676 on white was {passing}");

        // #777777 on white is 4.48:1, just under the AA text minimum.
        let failing = contrast_ratio(Color32::from_rgb(0x77, 0x77, 0x77), Color32::WHITE);
        assert!(failing < AA_TEXT_MIN, "#777777 on white was {failing}");
    }

    #[test]
    fn default_theme_text_and_focus_pairs_meet_wcag_aa() {
        for (name, foreground, background, minimum) in default_theme_pairs() {
            let ratio = contrast_ratio(foreground, background);
            assert!(
                ratio >= minimum,
                "{name}: {ratio:.2}:1 is below the {minimum}:1 minimum"
            );
        }
    }

    #[test]
    fn palette_text_and_focus_keep_contrast_on_each_surface() {
        for high_contrast in [false, true] {
            let colors = palette(high_contrast);
            for surface in [colors.background, colors.panel, colors.raised] {
                assert!(contrast_ratio(colors.text, surface) >= 7.0);
                assert!(contrast_ratio(colors.muted, surface) >= 4.5);
                assert!(contrast_ratio(colors.focus, surface) >= 3.0);
            }
            assert!(contrast_ratio(colors.background, colors.accent) >= 4.5);
            assert!(contrast_ratio(colors.text, SELECTION_BLUE) >= 4.5);
        }
    }

    #[test]
    fn palette_uses_apple_tv_surfaces_and_white_focus() {
        for high_contrast in [false, true] {
            let colors = palette(high_contrast);
            assert_eq!(colors.background, Color32::from_rgb(0x0A, 0x0A, 0x0C));
            assert_eq!(colors.panel, Color32::from_rgb(0x15, 0x15, 0x17));
            assert_eq!(colors.focus, Color32::WHITE);
        }

        // The standard theme selects with a near-white accent; high contrast keeps the blue.
        assert_eq!(palette(false).accent, Color32::from_rgb(0xF5, 0xF5, 0xF7));
        assert_eq!(palette(true).accent, JELLYFIN_BLUE);
    }

    #[test]
    fn apply_uses_consistent_widget_geometry_and_motion_setting() {
        let ctx = egui::Context::default();
        apply(&ctx, false, true);
        let style = ctx.style_of(egui::Theme::Dark);

        assert_eq!(style.visuals.override_text_color, None);
        assert_eq!(style.visuals.text_color(), palette(false).text);
        assert!(
            contrast_ratio(
                style.visuals.selection.stroke.color,
                style.visuals.selection.bg_fill
            ) >= AA_TEXT_MIN
        );
        assert!(
            contrast_ratio(
                style.visuals.widgets.active.fg_stroke.color,
                style.visuals.widgets.active.bg_fill,
            ) >= 4.5
        );

        assert_eq!(style.spacing.item_spacing, egui::vec2(14.0, 10.0));
        assert_eq!(style.spacing.button_padding, egui::vec2(14.0, 7.0));
        assert_eq!(
            style.visuals.widgets.inactive.corner_radius,
            egui::CornerRadius::same(10)
        );
        assert_eq!(
            style.visuals.widgets.hovered.corner_radius,
            egui::CornerRadius::same(10)
        );
        assert_eq!(
            style.visuals.widgets.active.corner_radius,
            egui::CornerRadius::same(10)
        );
        assert_eq!(style.animation_time, 0.0);
        assert_eq!(style.visuals.text_edit_bg_color(), FIELD);
        assert_eq!(
            style
                .text_styles
                .get(&egui::TextStyle::Body)
                .map(|f| f.size),
            Some(BODY_SIZE)
        );
    }

    #[test]
    fn the_style_is_rebuilt_only_when_the_flags_change() {
        let ctx = egui::Context::default();
        apply(&ctx, false, false);

        // A marker the theme would overwrite: the second call with the same flags must leave it
        // alone, which is how the per-frame rebuild is skipped.
        ctx.global_style_mut(|style| style.spacing.item_spacing = egui::vec2(1.0, 1.0));
        apply(&ctx, false, false);
        assert_eq!(
            ctx.style_of(egui::Theme::Dark).spacing.item_spacing,
            egui::vec2(1.0, 1.0),
            "unchanged flags skip the rebuild"
        );

        // A flag change rebuilds the whole style, marker included.
        apply(&ctx, true, false);
        assert_eq!(
            ctx.style_of(egui::Theme::Dark).spacing.item_spacing,
            egui::vec2(14.0, 10.0)
        );
    }

    #[test]
    fn the_interface_size_sets_the_zoom_factor() {
        use kattri_settings::InterfaceSize;
        let ctx = egui::Context::default();
        for size in InterfaceSize::ALL {
            apply_interface_size(&ctx, size);
            // egui applies a new zoom factor at the start of the next frame.
            let mut output = ctx.run_ui(egui::RawInput::default(), |_| {});
            output.textures_delta.clear();
            assert_eq!(ctx.zoom_factor(), size.zoom_factor());
        }
    }

    #[test]
    fn text_fields_stand_out_from_the_page_and_keep_text_contrast() {
        let colors = palette(false);
        assert_ne!(FIELD, colors.background);
        assert!(contrast_ratio(colors.text, FIELD) >= 7.0);
        assert!(contrast_ratio(colors.muted, FIELD) >= AA_TEXT_MIN);
    }

    #[test]
    fn proportional_text_falls_back_to_a_font_with_arrows() {
        let ctx = egui::Context::default();
        install_fonts(&ctx);
        let mut output = ctx.run_ui(egui::RawInput::default(), |_| {});
        output.textures_delta.clear();
        for ch in ['←', '→', '↑', '↓', '◌', '▾'] {
            let has_glyph =
                ctx.fonts_mut(|fonts| fonts.has_glyph(&egui::FontId::proportional(14.0), ch));
            assert!(has_glyph, "no glyph for {ch}");
        }
    }
}
