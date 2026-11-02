use egui::{self, Color32, Stroke};
use kattri_settings::ThemePreset;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

static CURRENT_PRESET: AtomicU8 = AtomicU8::new(0);
static CURRENT_HIGH_CONTRAST: AtomicBool = AtomicBool::new(false);

fn current_preset() -> ThemePreset {
    match CURRENT_PRESET.load(Ordering::Relaxed) {
        1 => ThemePreset::WarmDark,
        2 => ThemePreset::Light,
        _ => ThemePreset::JellyfinDark,
    }
}

/// Set the preset used by the existing shared palette and apply entry points.
pub fn set_current_preset(preset: ThemePreset) {
    let value = match preset {
        ThemePreset::JellyfinDark => 0,
        ThemePreset::WarmDark => 1,
        ThemePreset::Light => 2,
        // apply_with_preset resolves the OS preference before publishing the palette.
        ThemePreset::System => return,
    };
    CURRENT_PRESET.store(value, Ordering::Relaxed);
}

#[cfg(test)]
fn apply(ctx: &egui::Context, high_contrast: bool, reduce_motion: bool) {
    apply_with_preset(ctx, ThemePreset::JellyfinDark, high_contrast, reduce_motion);
}

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
    /// Warning copy that stays legible on this page.
    pub warning: Color32,
    /// Failure copy that stays legible on this page.
    pub danger: Color32,
}

pub const NIGHT_SLATE: Color32 = Color32::from_rgb(0x0A, 0x0A, 0x0C);
pub const PANEL: Color32 = Color32::from_rgb(0x15, 0x15, 0x17);
pub const RAISED: Color32 = Color32::from_rgb(0x2A, 0x2A, 0x2D);
pub const PRIMARY_TEXT: Color32 = Color32::from_rgb(0xF5, 0xF5, 0xF7);
pub const MUTED_TEXT: Color32 = Color32::from_rgb(0xA1, 0xA1, 0xA6);
pub const JELLYFIN_BLUE: Color32 = Color32::from_rgb(0x00, 0xA4, 0xDC);
/// Warning and failure copy on the interface pages.
pub const WARNING_TEXT: Color32 = Color32::from_rgb(0xE8, 0xB9, 0x77);
pub const DANGER_TEXT: Color32 = Color32::from_rgb(0xF0, 0xA0, 0x99);
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
    palette_for(current_preset(), high_contrast)
}

/// The palette the app last applied, for chrome drawn outside the client's own draw pass (the
/// host's status bar and notices), which never sees the app's settings directly.
pub fn active_palette() -> Palette {
    palette_for(
        current_preset(),
        CURRENT_HIGH_CONTRAST.load(Ordering::Relaxed),
    )
}

/// Resolve a preset to the shared semantic palette. High contrast keeps its own foreground,
/// surface and focus tokens regardless of the selected color preset.
pub fn palette_for(preset: ThemePreset, high_contrast: bool) -> Palette {
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
            warning: WARNING_TEXT,
            danger: DANGER_TEXT,
        }
    } else {
        match preset {
            ThemePreset::JellyfinDark | ThemePreset::System => Palette {
                background: NIGHT_SLATE,
                panel: PANEL,
                raised: RAISED,
                text: PRIMARY_TEXT,
                muted: MUTED_TEXT,
                accent: Color32::from_rgb(0xF5, 0xF5, 0xF7),
                focus: Color32::WHITE,
                divider: Color32::from_rgb(0x2C, 0x2C, 0x2F),
                warning: WARNING_TEXT,
                danger: DANGER_TEXT,
            },
            ThemePreset::WarmDark => Palette {
                background: Color32::from_rgb(0x18, 0x12, 0x10),
                panel: Color32::from_rgb(0x27, 0x1B, 0x16),
                raised: Color32::from_rgb(0x3B, 0x29, 0x20),
                text: Color32::from_rgb(0xFF, 0xF3, 0xE5),
                muted: Color32::from_rgb(0xD2, 0xB9, 0xA5),
                accent: Color32::from_rgb(0xFF, 0xB4, 0x5C),
                focus: Color32::from_rgb(0xFF, 0xD1, 0x7A),
                divider: Color32::from_rgb(0x55, 0x3B, 0x2E),
                warning: WARNING_TEXT,
                danger: DANGER_TEXT,
            },
            ThemePreset::Light => Palette {
                background: Color32::from_rgb(0xF4, 0xF5, 0xF7),
                panel: Color32::WHITE,
                raised: Color32::from_rgb(0xE5, 0xE8, 0xED),
                text: Color32::from_rgb(0x18, 0x20, 0x2A),
                muted: Color32::from_rgb(0x4B, 0x57, 0x66),
                accent: Color32::from_rgb(0x00, 0x60, 0x86),
                focus: Color32::from_rgb(0x00, 0x55, 0x80),
                divider: Color32::from_rgb(0xB5, 0xBE, 0xC9),
                warning: Color32::from_rgb(0x85, 0x48, 0x00),
                danger: Color32::from_rgb(0xAB, 0x20, 0x20),
            },
        }
    }
}

/// Open-licensed interface font.
const UI_FONT: &[u8] = include_bytes!("../../../assets/fonts/Inter.ttf");

/// Install Inter, egui's proportional font, and the monospace fallback used for arrows and
/// geometric shapes (`◌`, `▾`).
pub fn install_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        "inter".to_owned(),
        std::sync::Arc::new(egui::FontData::from_static(UI_FONT)),
    );
    let monospace = fonts
        .families
        .get(&egui::FontFamily::Monospace)
        .and_then(|family| family.first().cloned());
    if let Some(proportional) = fonts.families.get_mut(&egui::FontFamily::Proportional) {
        proportional.insert(0, "inter".to_owned());
        if let Some(fallback) = monospace
            && !proportional.contains(&fallback)
        {
            proportional.push(fallback);
        }
    }
    ctx.set_fonts(fonts);
}

/// Apply the selected preset together with the persistent accessibility overrides.
pub fn apply_with_preset(
    ctx: &egui::Context,
    preset: ThemePreset,
    high_contrast: bool,
    reduce_motion: bool,
) {
    let requested = preset;
    let preset = resolve_preset(preset, ctx.system_theme());
    set_current_preset(preset);
    // The whole style is rebuilt only when the flags change. Reassigning every Visuals field on
    // every frame is pure waste: the draw loop calls this before each pass.
    let applied = egui::Id::unique("kattri-theme-applied");
    let wanted = (requested, preset, high_contrast, reduce_motion);
    if ctx.data(|data| data.get_temp::<(ThemePreset, ThemePreset, bool, bool)>(applied))
        == Some(wanted)
    {
        return;
    }
    ctx.data_mut(|data| data.insert_temp(applied, wanted));
    CURRENT_HIGH_CONTRAST.store(high_contrast, Ordering::Relaxed);
    let colors = palette_for(preset, high_contrast);
    ctx.set_theme(if high_contrast {
        egui::ThemePreference::Dark
    } else if requested == ThemePreset::System {
        egui::ThemePreference::System
    } else if preset == ThemePreset::Light && !high_contrast {
        egui::ThemePreference::Light
    } else {
        egui::ThemePreference::Dark
    });
    ctx.global_style_mut(|style| {
        style.visuals = if preset == ThemePreset::Light && !high_contrast {
            egui::Visuals::light()
        } else {
            egui::Visuals::dark()
        };
        style.visuals.panel_fill = colors.background;
        style.visuals.window_fill = colors.panel;
        style.visuals.extreme_bg_color = colors.background;
        style.visuals.text_edit_bg_color = Some(if high_contrast {
            FIELD
        } else if preset == ThemePreset::Light {
            Color32::WHITE
        } else if preset == ThemePreset::WarmDark {
            Color32::from_rgb(0x30, 0x22, 0x1B)
        } else {
            FIELD
        });
        style.visuals.faint_bg_color = colors.panel;
        // A global text override would mask the dark foreground on blue active controls.
        style.visuals.override_text_color = None;
        style.visuals.selection.bg_fill = match (high_contrast, preset) {
            (true, _) => ACTIVE_FILL_HIGH_CONTRAST,
            (false, ThemePreset::JellyfinDark) => SELECTION_BLUE,
            (false, ThemePreset::WarmDark) => Color32::from_rgb(0x4A, 0x35, 0x28),
            (false, ThemePreset::Light) => Color32::from_rgb(0xD8, 0xE5, 0xEC),
            (false, ThemePreset::System) => SELECTION_BLUE,
        };
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
        style.visuals.widgets.hovered.bg_fill = if high_contrast {
            HOVER_FILL
        } else if preset == ThemePreset::Light {
            Color32::from_rgb(0xD8, 0xE5, 0xEC)
        } else if preset == ThemePreset::WarmDark {
            Color32::from_rgb(0x4A, 0x35, 0x28)
        } else {
            HOVER_FILL
        };
        style.visuals.widgets.hovered.fg_stroke.color = colors.text;
        // egui draws `strong()` text, and the text of a pressed widget, in this foreground, so it
        // must be light. Pressed widgets get a mid-grey fill (a deep blue in high contrast).
        style.visuals.widgets.active.bg_fill = match (high_contrast, preset) {
            (true, _) => ACTIVE_FILL_HIGH_CONTRAST,
            (false, ThemePreset::JellyfinDark) => SELECTION_BLUE,
            (false, ThemePreset::WarmDark) => Color32::from_rgb(0x4A, 0x35, 0x28),
            (false, ThemePreset::Light) => Color32::from_rgb(0xD8, 0xE5, 0xEC),
            (false, ThemePreset::System) => SELECTION_BLUE,
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

fn resolve_preset(preset: ThemePreset, system: Option<egui::Theme>) -> ThemePreset {
    match (preset, system) {
        (ThemePreset::System, Some(egui::Theme::Light)) => ThemePreset::Light,
        (ThemePreset::System, _) => ThemePreset::JellyfinDark,
        (chosen, _) => chosen,
    }
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
    let colors = palette_for(ThemePreset::JellyfinDark, false);
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
            let colors = palette_for(ThemePreset::JellyfinDark, high_contrast);
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
            let colors = palette_for(ThemePreset::JellyfinDark, high_contrast);
            assert_eq!(colors.background, Color32::from_rgb(0x0A, 0x0A, 0x0C));
            assert_eq!(colors.panel, Color32::from_rgb(0x15, 0x15, 0x17));
            assert_eq!(colors.focus, Color32::WHITE);
        }

        // The standard theme selects with a near-white accent; high contrast keeps the blue.
        assert_eq!(
            palette_for(ThemePreset::JellyfinDark, false).accent,
            Color32::from_rgb(0xF5, 0xF5, 0xF7)
        );
        assert_eq!(
            palette_for(ThemePreset::JellyfinDark, true).accent,
            JELLYFIN_BLUE
        );
    }

    #[test]
    fn apply_uses_consistent_widget_geometry_and_motion_setting() {
        let ctx = egui::Context::default();
        apply(&ctx, false, true);
        let style = ctx.style_of(egui::Theme::Dark);

        assert_eq!(style.visuals.override_text_color, None);
        assert_eq!(
            style.visuals.text_color(),
            palette_for(ThemePreset::JellyfinDark, false).text
        );
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
    fn every_preset_keeps_text_and_focus_contrast_on_its_surfaces() {
        for preset in [
            ThemePreset::JellyfinDark,
            ThemePreset::WarmDark,
            ThemePreset::Light,
            ThemePreset::System,
        ] {
            let colors = palette_for(preset, false);
            for surface in [colors.background, colors.panel, colors.raised] {
                assert!(
                    contrast_ratio(colors.text, surface) >= 7.0,
                    "{preset:?} text is below 7:1 on {surface:?}"
                );
                assert!(
                    contrast_ratio(colors.muted, surface) >= AA_TEXT_MIN,
                    "{preset:?} muted text is below AA on {surface:?}"
                );
                assert!(
                    contrast_ratio(colors.focus, surface) >= 3.0,
                    "{preset:?} focus ring is below 3:1 on {surface:?}"
                );
                assert!(
                    contrast_ratio(colors.danger, surface) >= AA_TEXT_MIN,
                    "{preset:?} failure copy is below AA on {surface:?}"
                );
                assert!(
                    contrast_ratio(colors.warning, surface) >= AA_TEXT_MIN,
                    "{preset:?} warning copy is below AA on {surface:?}"
                );
            }
        }
    }

    #[test]
    fn presets_have_distinct_surfaces_and_high_contrast_overrides_them() {
        let dark = palette_for(ThemePreset::JellyfinDark, false);
        let warm = palette_for(ThemePreset::WarmDark, false);
        assert_ne!(dark.background, warm.background);
        assert_ne!(dark.accent, warm.accent);

        // High contrast keeps one palette whatever preset is selected.
        let forced = palette_for(ThemePreset::WarmDark, true);
        assert_eq!(forced.background, dark.background);
        assert_eq!(forced.accent, JELLYFIN_BLUE);
        assert_eq!(forced.focus, Color32::WHITE);
    }

    #[test]
    fn system_theme_follows_os_changes_and_high_contrast_takes_precedence() {
        let ctx = egui::Context::default();
        for (system, high_contrast, expected_dark) in [
            (egui::Theme::Light, false, false),
            (egui::Theme::Dark, false, true),
            (egui::Theme::Light, true, true),
            (egui::Theme::Light, false, false),
        ] {
            let mut output = ctx.run_ui(
                egui::RawInput {
                    system_theme: Some(system),
                    ..egui::RawInput::default()
                },
                |ui| {
                    apply_with_preset(ui.ctx(), ThemePreset::System, high_contrast, true);
                    assert_eq!(ui.ctx().global_style().visuals.dark_mode, expected_dark);
                },
            );
            output.textures_delta.clear();
        }
    }

    #[test]
    fn text_fields_stand_out_from_the_page_and_keep_text_contrast() {
        let colors = palette_for(ThemePreset::JellyfinDark, false);
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
