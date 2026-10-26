//! Pure subtitle styling model.
//!
//! A [`SubtitleStyle`] holds presentation settings that have already been checked against their
//! ranges: font family, font size in points, text and outline colors, outline width, and
//! background opacity. Construction is the only way to get a value, so every stored field fits
//! its range. [`SubtitleStyle::to_mpv_options`] turns the style into mpv option pairs. Each name
//! it emits is an entry in the `mpv_options` allowlist. The tests pass every emitted pair through
//! `mpv_options::validate`. This module performs no I/O.
//!
//! Field mapping:
//! - The font family is written to `sub-font`.
//! - The font size has no allowlisted property of its own. It is written to `sub-scale` as the
//!   font size divided by [`REFERENCE_FONT_SIZE_PT`], so a reference-sized font is scale 1.0.
//! - The text color is written to `sub-color`, the outline color to `sub-border-color`, and the
//!   outline width to `sub-border-size`.
//! - The background opacity is written to `sub-back-color` as a black background whose alpha
//!   byte carries the opacity.
//! - Vertical placement (`sub-pos`) and alignment (`sub-align-x`) are not part of this model, so
//!   they are never emitted from here.
//!
//! Colors are entered as `#RRGGBBAA`, or as `#RRGGBB` for an opaque color. mpv reads the alpha
//! byte first, so [`RgbaColor::to_mpv_argb`] writes `#AARRGGBB`. That byte order has not been
//! confirmed against the pinned libmpv.

use std::fmt;

use crate::mpv_options::{MpvOptionError, OptionValue, ValidatedOption, validate};

/// Smallest font size, in points, that a style accepts.
pub const FONT_SIZE_PT_MIN: u8 = 8;
/// Largest font size, in points, that a style accepts.
pub const FONT_SIZE_PT_MAX: u8 = 72;
/// The font size that maps to mpv scale 1.0.
pub const REFERENCE_FONT_SIZE_PT: u8 = 32;
/// Largest outline width. Matches the range of `sub-border-size` in the allowlist.
pub const OUTLINE_WIDTH_MAX: u8 = 10;
/// Largest background opacity, in percent.
pub const BACKGROUND_OPACITY_MAX_PERCENT: u8 = 100;
/// Longest font family name, in characters.
pub const FONT_FAMILY_MAX_CHARS: usize = 64;

const DEFAULT_FONT_FAMILY: &str = "sans-serif";
const DEFAULT_FONT_SIZE_PT: u8 = REFERENCE_FONT_SIZE_PT;
const DEFAULT_OUTLINE_WIDTH: u8 = 2;

/// Why a style field or color string was refused. No variant carries the rejected value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubtitleStyleError {
    FontFamilyEmpty,
    FontFamilyTooLong,
    FontFamilyControlCharacter,
    FontFamilyLeadingDash,
    SizeOutOfRange,
    OutlineWidthOutOfRange,
    OpacityOutOfRange,
    InvalidColor,
}

impl fmt::Display for SubtitleStyleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FontFamilyEmpty => f.write_str("font family is empty"),
            Self::FontFamilyTooLong => write!(
                f,
                "font family is longer than {FONT_FAMILY_MAX_CHARS} characters"
            ),
            Self::FontFamilyControlCharacter => {
                f.write_str("font family contains a control character")
            }
            Self::FontFamilyLeadingDash => f.write_str("font family starts with a dash"),
            Self::SizeOutOfRange => write!(
                f,
                "font size must be {FONT_SIZE_PT_MIN} to {FONT_SIZE_PT_MAX} points"
            ),
            Self::OutlineWidthOutOfRange => {
                write!(f, "outline width must be 0 to {OUTLINE_WIDTH_MAX}")
            }
            Self::OpacityOutOfRange => write!(
                f,
                "background opacity must be 0 to {BACKGROUND_OPACITY_MAX_PERCENT} percent"
            ),
            Self::InvalidColor => f.write_str("color must be #RRGGBBAA or #RRGGBB"),
        }
    }
}

impl std::error::Error for SubtitleStyleError {}

/// An 8-bit-per-channel color with alpha.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RgbaColor {
    r: u8,
    g: u8,
    b: u8,
    a: u8,
}

impl RgbaColor {
    pub const fn new(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }

    /// Parse `#RRGGBBAA`, or `#RRGGBB` which is taken as fully opaque. Hex digits may be upper or
    /// lower case. Any other length, a missing `#`, or a non-hex byte is refused.
    pub fn parse(text: &str) -> Result<Self, SubtitleStyleError> {
        let body = text
            .strip_prefix('#')
            .ok_or(SubtitleStyleError::InvalidColor)?
            .as_bytes();
        let r = hex_channel(body, 0)?;
        let g = hex_channel(body, 2)?;
        let b = hex_channel(body, 4)?;
        let a = match body.len() {
            6 => u8::MAX,
            8 => hex_channel(body, 6)?,
            _ => return Err(SubtitleStyleError::InvalidColor),
        };
        Ok(Self { r, g, b, a })
    }

    /// Format as `#RRGGBBAA` with upper-case digits. [`RgbaColor::parse`] reads this back.
    pub fn to_rgba_hex(self) -> String {
        format!("#{:02X}{:02X}{:02X}{:02X}", self.r, self.g, self.b, self.a)
    }

    /// Format as `#AARRGGBB`, the byte order mpv reads for color options.
    pub fn to_mpv_argb(self) -> String {
        format!("#{:02X}{:02X}{:02X}{:02X}", self.a, self.r, self.g, self.b)
    }
}

fn hex_channel(body: &[u8], start: usize) -> Result<u8, SubtitleStyleError> {
    body.get(start..start + 2)
        .and_then(hex_pair)
        .ok_or(SubtitleStyleError::InvalidColor)
}

fn hex_pair(pair: &[u8]) -> Option<u8> {
    let [high, low] = pair else {
        return None;
    };
    Some(hex_digit(*high)? * 16 + hex_digit(*low)?)
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Validated subtitle presentation settings. Fields are private; use [`SubtitleStyle::new`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubtitleStyle {
    font_family: String,
    size_pt: u8,
    text_color: RgbaColor,
    outline_color: RgbaColor,
    outline_width: u8,
    background_opacity_percent: u8,
}

impl Default for SubtitleStyle {
    /// The product default: a 32 pt sans-serif font, opaque white text, an opaque black outline
    /// two pixels wide, and no background. Each value is inside its allowed range.
    fn default() -> Self {
        Self {
            font_family: DEFAULT_FONT_FAMILY.to_owned(),
            size_pt: DEFAULT_FONT_SIZE_PT,
            text_color: RgbaColor::new(0xFF, 0xFF, 0xFF, 0xFF),
            outline_color: RgbaColor::new(0x00, 0x00, 0x00, 0xFF),
            outline_width: DEFAULT_OUTLINE_WIDTH,
            background_opacity_percent: 0,
        }
    }
}

impl SubtitleStyle {
    /// Build a style, checking every field.
    ///
    /// - The font family is trimmed. It must be non-empty, at most
    ///   [`FONT_FAMILY_MAX_CHARS`] characters, free of control characters, and must not start
    ///   with `-`, so it cannot be read as an option.
    /// - The size is `FONT_SIZE_PT_MIN..=FONT_SIZE_PT_MAX` points.
    /// - The outline width is `0..=OUTLINE_WIDTH_MAX`.
    /// - The background opacity is `0..=BACKGROUND_OPACITY_MAX_PERCENT` percent.
    pub fn new(
        font_family: &str,
        size_pt: u8,
        text_color: RgbaColor,
        outline_color: RgbaColor,
        outline_width: u8,
        background_opacity_percent: u8,
    ) -> Result<Self, SubtitleStyleError> {
        if !(FONT_SIZE_PT_MIN..=FONT_SIZE_PT_MAX).contains(&size_pt) {
            return Err(SubtitleStyleError::SizeOutOfRange);
        }
        if outline_width > OUTLINE_WIDTH_MAX {
            return Err(SubtitleStyleError::OutlineWidthOutOfRange);
        }
        if background_opacity_percent > BACKGROUND_OPACITY_MAX_PERCENT {
            return Err(SubtitleStyleError::OpacityOutOfRange);
        }
        Ok(Self {
            font_family: validate_font_family(font_family)?,
            size_pt,
            text_color,
            outline_color,
            outline_width,
            background_opacity_percent,
        })
    }

    pub fn font_family(&self) -> &str {
        &self.font_family
    }

    pub const fn size_pt(&self) -> u8 {
        self.size_pt
    }

    pub const fn text_color(&self) -> RgbaColor {
        self.text_color
    }

    pub const fn outline_color(&self) -> RgbaColor {
        self.outline_color
    }

    pub const fn outline_width(&self) -> u8 {
        self.outline_width
    }

    pub const fn background_opacity_percent(&self) -> u8 {
        self.background_opacity_percent
    }

    /// The mpv option pairs for this style, in a fixed order. Every name is on the `mpv_options`
    /// allowlist and every value is already in the spelling mpv takes.
    pub fn to_mpv_options(&self) -> Vec<(&'static str, String)> {
        vec![
            ("sub-font", self.font_family.clone()),
            ("sub-scale", self.scale().to_string()),
            ("sub-color", self.text_color.to_mpv_argb()),
            ("sub-border-color", self.outline_color.to_mpv_argb()),
            ("sub-border-size", self.outline_width.to_string()),
            (
                "sub-back-color",
                background_argb(self.background_opacity_percent),
            ),
        ]
    }

    /// Build subtitle-setting writes that have passed the shared mpv option allowlist.
    pub fn to_validated_mpv_options(&self) -> Result<Vec<ValidatedOption>, MpvOptionError> {
        Ok(vec![
            validate("sub-font", OptionValue::Text(&self.font_family))?,
            validate("sub-scale", OptionValue::Float(self.scale()))?,
            validate(
                "sub-color",
                OptionValue::Text(&self.text_color.to_mpv_argb()),
            )?,
            validate(
                "sub-border-color",
                OptionValue::Text(&self.outline_color.to_mpv_argb()),
            )?,
            validate(
                "sub-border-size",
                OptionValue::Int(i64::from(self.outline_width)),
            )?,
            validate(
                "sub-back-color",
                OptionValue::Text(&background_argb(self.background_opacity_percent)),
            )?,
        ])
    }

    fn scale(&self) -> f64 {
        scale_for_size_pt(self.size_pt)
    }
}

/// The mpv `sub-scale` for a font size in points. A reference-sized font is 1.0.
fn scale_for_size_pt(size_pt: u8) -> f64 {
    f64::from(size_pt) / f64::from(REFERENCE_FONT_SIZE_PT)
}

/// The `sub-scale` and `sub-color` writes for a size and a text colour (M2-20). Each argument is
/// optional. `None` writes nothing for that field, so a setting left at the player default keeps
/// mpv's own value. The other style fields are not written, because the settings leave them at
/// the player default too.
pub fn size_and_color_options(
    size_pt: Option<u8>,
    text_color: Option<&str>,
) -> Result<Vec<ValidatedOption>, MpvOptionError> {
    let mut options = Vec::with_capacity(2);
    if let Some(size) = size_pt {
        if !(FONT_SIZE_PT_MIN..=FONT_SIZE_PT_MAX).contains(&size) {
            return Err(MpvOptionError::OutOfRange { name: "sub-scale" });
        }
        options.push(validate(
            "sub-scale",
            OptionValue::Float(scale_for_size_pt(size)),
        )?);
    }
    if let Some(text) = text_color {
        // Parsed first so that only the #AARRGGBB form mpv reads reaches the allowlist.
        let color = RgbaColor::parse(text)
            .map_err(|_| MpvOptionError::NotAllowedValue { name: "sub-color" })?;
        options.push(validate(
            "sub-color",
            OptionValue::Text(&color.to_mpv_argb()),
        )?);
    }
    Ok(options)
}

fn validate_font_family(raw: &str) -> Result<String, SubtitleStyleError> {
    if raw.chars().any(char::is_control) {
        return Err(SubtitleStyleError::FontFamilyControlCharacter);
    }
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(SubtitleStyleError::FontFamilyEmpty);
    }
    if trimmed.starts_with('-') {
        return Err(SubtitleStyleError::FontFamilyLeadingDash);
    }
    if trimmed.chars().count() > FONT_FAMILY_MAX_CHARS {
        return Err(SubtitleStyleError::FontFamilyTooLong);
    }
    Ok(trimmed.to_owned())
}

/// Black background with its alpha byte set from a percentage, rounded to the nearest step.
fn background_argb(percent: u8) -> String {
    let alpha = (u16::from(percent) * 255 + 50) / 100;
    format!("#{alpha:02X}000000")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mpv_options::{ALLOWLIST, OptionValue, ValueKind, validate};

    fn kind_accepts_text(kind: ValueKind) -> bool {
        matches!(kind, ValueKind::Text | ValueKind::Enum(_))
    }

    fn default_style() -> SubtitleStyle {
        SubtitleStyle::default()
    }

    fn style_with_size(size: u8) -> Result<SubtitleStyle, SubtitleStyleError> {
        SubtitleStyle::new(
            "sans-serif",
            size,
            RgbaColor::new(255, 255, 255, 255),
            RgbaColor::new(0, 0, 0, 255),
            2,
            0,
        )
    }

    #[test]
    fn font_size_accepts_the_documented_range_and_refuses_outside_it() {
        assert!(style_with_size(FONT_SIZE_PT_MIN - 1).is_err());
        assert_eq!(
            style_with_size(7).err(),
            Some(SubtitleStyleError::SizeOutOfRange)
        );
        assert!(style_with_size(8).is_ok());
        assert!(style_with_size(72).is_ok());
        assert_eq!(
            style_with_size(73).err(),
            Some(SubtitleStyleError::SizeOutOfRange)
        );
    }

    #[test]
    fn outline_width_bounds() {
        let build = |width: u8| {
            SubtitleStyle::new(
                "sans-serif",
                32,
                RgbaColor::new(255, 255, 255, 255),
                RgbaColor::new(0, 0, 0, 255),
                width,
                0,
            )
        };
        assert!(build(0).is_ok());
        assert!(build(10).is_ok());
        assert_eq!(
            build(11).err(),
            Some(SubtitleStyleError::OutlineWidthOutOfRange)
        );
    }

    #[test]
    fn background_opacity_bounds() {
        let build = |opacity: u8| {
            SubtitleStyle::new(
                "sans-serif",
                32,
                RgbaColor::new(255, 255, 255, 255),
                RgbaColor::new(0, 0, 0, 255),
                2,
                opacity,
            )
        };
        assert!(build(0).is_ok());
        assert!(build(100).is_ok());
        assert_eq!(
            build(101).err(),
            Some(SubtitleStyleError::OpacityOutOfRange)
        );
    }

    #[test]
    fn font_family_is_trimmed_and_bounded() {
        let build = |name: &str| {
            SubtitleStyle::new(
                name,
                32,
                RgbaColor::new(255, 255, 255, 255),
                RgbaColor::new(0, 0, 0, 255),
                2,
                0,
            )
        };
        let trimmed = build("  Noto Sans  ").map(|s| s.font_family().to_owned());
        assert_eq!(trimmed.as_deref(), Ok("Noto Sans"));

        assert_eq!(
            build("   ").err(),
            Some(SubtitleStyleError::FontFamilyEmpty)
        );
        assert_eq!(
            build("Noto\nSans").err(),
            Some(SubtitleStyleError::FontFamilyControlCharacter)
        );
        assert_eq!(
            build("-bad").err(),
            Some(SubtitleStyleError::FontFamilyLeadingDash)
        );

        let longest = "a".repeat(FONT_FAMILY_MAX_CHARS);
        assert!(build(&longest).is_ok());
        let too_long = "a".repeat(FONT_FAMILY_MAX_CHARS + 1);
        assert_eq!(
            build(&too_long).err(),
            Some(SubtitleStyleError::FontFamilyTooLong)
        );
    }

    #[test]
    fn color_parses_rgba_hex_and_opaque_rgb() {
        assert_eq!(
            RgbaColor::parse("#FF8000CC"),
            Ok(RgbaColor::new(0xFF, 0x80, 0x00, 0xCC))
        );
        assert_eq!(
            RgbaColor::parse("#ff8000cc"),
            Ok(RgbaColor::new(0xFF, 0x80, 0x00, 0xCC))
        );
        assert_eq!(
            RgbaColor::parse("#102030"),
            Ok(RgbaColor::new(0x10, 0x20, 0x30, 0xFF))
        );
    }

    #[test]
    fn color_refuses_malformed_input() {
        for bad in [
            "FF8000CC",
            "#FF800",
            "#FF8000CC0",
            "#GG8000CC",
            "#+F8000CC",
            "",
            "#",
            "#\u{e9}\u{e9}0000",
        ] {
            assert_eq!(
                RgbaColor::parse(bad),
                Err(SubtitleStyleError::InvalidColor),
                "accepted {bad:?}"
            );
        }
    }

    #[test]
    fn color_round_trips_through_rgba_hex() {
        for color in [
            RgbaColor::new(0, 0, 0, 0),
            RgbaColor::new(0xFF, 0xFF, 0xFF, 0xFF),
            RgbaColor::new(0x12, 0xAB, 0x5E, 0x7F),
        ] {
            assert_eq!(RgbaColor::parse(&color.to_rgba_hex()), Ok(color));
        }
    }

    #[test]
    fn mpv_argb_puts_alpha_first() {
        assert_eq!(
            RgbaColor::new(0x11, 0x22, 0x33, 0x44).to_mpv_argb(),
            "#44112233"
        );
    }

    #[test]
    fn default_style_round_trips_through_its_own_constructor() {
        let style = default_style();
        let rebuilt = SubtitleStyle::new(
            style.font_family(),
            style.size_pt(),
            style.text_color(),
            style.outline_color(),
            style.outline_width(),
            style.background_opacity_percent(),
        );
        assert_eq!(rebuilt, Ok(style.clone()));
        assert_eq!(style.size_pt(), 32);
        assert_eq!(style.font_family(), "sans-serif");
        assert_eq!(style.text_color().to_rgba_hex(), "#FFFFFFFF");
        assert_eq!(style.outline_color().to_rgba_hex(), "#000000FF");
        assert_eq!(style.outline_width(), 2);
        assert_eq!(style.background_opacity_percent(), 0);
    }

    #[test]
    fn default_style_emits_the_allowlist_defaults_for_its_values() {
        let pairs: std::collections::HashMap<_, _> =
            default_style().to_mpv_options().into_iter().collect();
        assert_eq!(pairs.get("sub-scale").map(String::as_str), Some("1"));
        assert_eq!(
            pairs.get("sub-font").map(String::as_str),
            Some("sans-serif")
        );
        assert_eq!(pairs.get("sub-border-size").map(String::as_str), Some("2"));
        assert_eq!(
            pairs.get("sub-back-color").map(String::as_str),
            Some("#00000000")
        );
    }

    #[test]
    fn emitted_pairs_are_allowlisted_names_that_validate() {
        let style = SubtitleStyle::new(
            "Noto Sans",
            8,
            RgbaColor::new(0x10, 0x20, 0x30, 0x40),
            RgbaColor::new(0xAA, 0xBB, 0xCC, 0xDD),
            10,
            100,
        );
        let Ok(style) = style else {
            panic!("the extreme style should be accepted");
        };
        let emitted = style.to_mpv_options();
        assert_eq!(emitted.len(), 6);
        for (name, value) in &emitted {
            let spec = ALLOWLIST.iter().find(|spec| spec.name == *name);
            let Some(spec) = spec else {
                panic!("{name} is not on the allowlist");
            };
            let option = if kind_accepts_text(spec.kind) {
                OptionValue::Text(value)
            } else {
                match spec.kind {
                    ValueKind::Int { .. } => match value.parse::<i64>() {
                        Ok(parsed) => OptionValue::Int(parsed),
                        Err(_) => panic!("{name} emitted a non-integer {value}"),
                    },
                    ValueKind::Float { .. } => match value.parse::<f64>() {
                        Ok(parsed) => OptionValue::Float(parsed),
                        Err(_) => panic!("{name} emitted a non-number {value}"),
                    },
                    _ => panic!("{name} has a kind this style does not emit"),
                }
            };
            assert!(
                validate(name, option).is_ok(),
                "{name} refused by the allowlist"
            );
        }

        let validated = style
            .to_validated_mpv_options()
            .expect("constructed style values fit the mpv allowlist");
        assert_eq!(validated.len(), emitted.len());
        assert!(validated.iter().all(|option| {
            emitted
                .iter()
                .any(|(name, value)| *name == option.name && value == &option.value)
        }));
    }

    #[test]
    fn scale_spans_the_documented_size_range() {
        let scale_for = |size: u8| {
            let style = style_with_size(size);
            let Ok(style) = style else {
                panic!("size {size} should be accepted");
            };
            let pairs = style.to_mpv_options();
            let value = pairs
                .iter()
                .find(|(name, _)| *name == "sub-scale")
                .map(|(_, value)| value.clone());
            value.and_then(|v| v.parse::<f64>().ok())
        };
        assert_eq!(scale_for(FONT_SIZE_PT_MIN), Some(0.25));
        assert_eq!(scale_for(REFERENCE_FONT_SIZE_PT), Some(1.0));
        assert_eq!(scale_for(FONT_SIZE_PT_MAX), Some(2.25));
    }

    #[test]
    fn background_opacity_maps_to_alpha_byte() {
        assert_eq!(background_argb(0), "#00000000");
        assert_eq!(background_argb(50), "#80000000");
        assert_eq!(background_argb(100), "#FF000000");
    }
}
