//! Screenshot naming and path rules (M2-15). Pure: template expansion, name sanitizing, collision
//! suffixes, and the mpv command arguments. No capture and no file I/O.
//!
//! Placeholders (single pass; inserted values are never expanded again):
//! - `%f`: the file name without its extension
//! - `%P`: the playback position as `HH.MM.SS.mmm` [UNCONFIRMED: check against the mpv manual]
//! - `%n`: a counter supplied by the caller
//! - `%%`: a literal `%`
//!
//! Any other `%` sequence is refused, so a typo does not silently produce a wrong name.

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

/// Template used when the setting is empty (product choice).
pub const DEFAULT_TEMPLATE: &str = "kattri-%f-%P";

/// Longest template accepted, in characters.
pub const MAX_TEMPLATE_CHARS: usize = 200;

/// Longest file stem produced, in characters. Keeps the full path well under legacy limits.
pub const MAX_STEM_CHARS: usize = 180;

/// Fallback stem when sanitizing leaves nothing.
pub const FALLBACK_STEM: &str = "screenshot";

/// Candidate names tried for one stem: the base name, then `-2` through `-MAX_COLLISION_ATTEMPTS`.
pub const MAX_COLLISION_ATTEMPTS: u32 = 9_999;

/// Values a template can use.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ScreenshotContext<'a> {
    /// File name without its extension.
    pub file_stem: &'a str,
    pub position: Duration,
    /// Caller-supplied counter for `%n`.
    pub counter: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TemplateError {
    TooLong,
    /// A `%` was not followed by a known placeholder.
    UnknownPlaceholder(char),
    /// The template ends with a bare `%`.
    TrailingPercent,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NameError {
    /// Every candidate name is taken.
    NoFreeName,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PathError {
    /// The name is not a single plain path component.
    UnsafeFileName,
    /// The path cannot be passed as a command argument safely.
    UnsafeCommandPath,
}

/// Image format for screenshots. Only PNG is supported in M2.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScreenshotFormat {
    Png,
}

impl ScreenshotFormat {
    /// Accepts `png` in any letter case. Other values are refused, so the caller keeps PNG.
    pub fn parse(raw: &str) -> Option<Self> {
        raw.trim().eq_ignore_ascii_case("png").then_some(Self::Png)
    }

    pub const fn extension(self) -> &'static str {
        match self {
            Self::Png => "png",
        }
    }
}

/// What the screenshot covers. `subtitles` keeps subtitles and omits the OSD (legacy parity,
/// PlayerComponent.cpp:1729). `video` drops subtitles too. Decision record: M2-15 requires the
/// owner to confirm `subtitles`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScreenshotMode {
    Subtitles,
    Video,
}

/// Mode used when the caller does not choose one.
pub const DEFAULT_MODE: ScreenshotMode = ScreenshotMode::Subtitles;

impl ScreenshotMode {
    pub const fn mpv_flag(self) -> &'static str {
        match self {
            Self::Subtitles => "subtitles",
            Self::Video => "video",
        }
    }
}

/// An mpv command as separate arguments. Arguments are never joined into one string.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScreenshotCommand {
    pub name: &'static str,
    pub args: Vec<String>,
}

/// Format `position` as `HH.MM.SS.mmm`.
pub fn format_position(position: Duration) -> String {
    let total_millis = position.as_millis();
    let millis = total_millis % 1_000;
    let total_seconds = total_millis / 1_000;
    let seconds = total_seconds % 60;
    let minutes = (total_seconds / 60) % 60;
    let hours = total_seconds / 3_600;
    format!("{hours:02}.{minutes:02}.{seconds:02}.{millis:03}")
}

/// Expand the placeholders in `template`. The result is not sanitized; see [`screenshot_stem`].
pub fn expand_template(
    template: &str,
    context: &ScreenshotContext<'_>,
) -> Result<String, TemplateError> {
    if template.chars().count() > MAX_TEMPLATE_CHARS {
        return Err(TemplateError::TooLong);
    }
    let mut expanded = String::with_capacity(template.len());
    let mut chars = template.chars();
    while let Some(current) = chars.next() {
        if current != '%' {
            expanded.push(current);
            continue;
        }
        match chars.next() {
            None => return Err(TemplateError::TrailingPercent),
            Some('%') => expanded.push('%'),
            Some('f') => expanded.push_str(context.file_stem),
            Some('P') => expanded.push_str(&format_position(context.position)),
            Some('n') => expanded.push_str(&context.counter.to_string()),
            Some(other) => return Err(TemplateError::UnknownPlaceholder(other)),
        }
    }
    Ok(expanded)
}

/// Expand the template and sanitize the result into a file stem (no extension).
///
/// An empty template uses [`DEFAULT_TEMPLATE`]. Path separators are replaced with `_`. Characters
/// that are invalid in Windows file names, and control characters, are removed. Leading and trailing
/// spaces and dots are trimmed. Reserved device names such as `CON` get a leading `_`.
pub fn screenshot_stem(
    template: &str,
    context: &ScreenshotContext<'_>,
) -> Result<String, TemplateError> {
    let effective = if template.trim().is_empty() {
        DEFAULT_TEMPLATE
    } else {
        template
    };
    let expanded = expand_template(effective, context)?;
    Ok(sanitize_stem(&expanded))
}

/// Sanitize a raw stem as described in [`screenshot_stem`].
pub fn sanitize_stem(raw: &str) -> String {
    let mut cleaned = String::with_capacity(raw.len());
    for character in raw.chars() {
        match character {
            '/' | '\\' => cleaned.push('_'),
            '<' | '>' | ':' | '"' | '|' | '?' | '*' => {}
            other if other.is_control() => {}
            other => cleaned.push(other),
        }
    }
    let truncated: String = cleaned.chars().take(MAX_STEM_CHARS).collect();
    let trimmed = truncated.trim().trim_end_matches(['.', ' ']).trim();
    if trimmed.is_empty() {
        return FALLBACK_STEM.to_owned();
    }
    let base = trimmed.split('.').next().unwrap_or("");
    if is_reserved_device_name(base) {
        format!("_{trimmed}")
    } else {
        trimmed.to_owned()
    }
}

fn is_reserved_device_name(base: &str) -> bool {
    let upper = base.to_ascii_uppercase();
    if matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL") {
        return true;
    }
    let number = upper
        .strip_prefix("COM")
        .or_else(|| upper.strip_prefix("LPT"));
    matches!(
        number,
        Some("1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9")
    )
}

/// Choose a file name for `stem` that is not in `existing`.
///
/// The base name is tried first, then `stem-2.ext`, `stem-3.ext`, and so on. Comparison ignores
/// case, which matches the default Windows file system.
pub fn unique_file_name(
    stem: &str,
    format: ScreenshotFormat,
    existing: &[String],
) -> Result<String, NameError> {
    let taken: HashSet<String> = existing.iter().map(|name| name.to_lowercase()).collect();
    let extension = format.extension();
    for attempt in 1..=MAX_COLLISION_ATTEMPTS {
        let candidate = if attempt == 1 {
            format!("{stem}.{extension}")
        } else {
            format!("{stem}-{attempt}.{extension}")
        };
        if !taken.contains(&candidate.to_lowercase()) {
            return Ok(candidate);
        }
    }
    Err(NameError::NoFreeName)
}

/// Join `directory` and one file name. The name must be a single plain component, so a template
/// cannot escape the directory.
pub fn screenshot_path(directory: &Path, file_name: &str) -> Result<PathBuf, PathError> {
    if file_name.is_empty() || file_name.contains(['/', '\\']) {
        return Err(PathError::UnsafeFileName);
    }
    let mut components = Path::new(file_name).components();
    let single_normal = matches!(
        (components.next(), components.next()),
        (Some(Component::Normal(_)), None)
    );
    if !single_normal {
        return Err(PathError::UnsafeFileName);
    }
    Ok(directory.join(file_name))
}

/// Build the mpv `screenshot-to-file` command for `path`. The path is a separate argument. A path
/// that starts with `-` or contains control characters is refused, so it cannot act as an option.
pub fn screenshot_command(
    path: &str,
    mode: ScreenshotMode,
) -> Result<ScreenshotCommand, PathError> {
    if path.is_empty() || path.starts_with('-') || path.chars().any(char::is_control) {
        return Err(PathError::UnsafeCommandPath);
    }
    Ok(ScreenshotCommand {
        name: "screenshot-to-file",
        args: vec![path.to_owned(), mode.mpv_flag().to_owned()],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context(stem: &str, seconds: u64) -> ScreenshotContext<'_> {
        ScreenshotContext {
            file_stem: stem,
            position: Duration::from_secs(seconds),
            counter: 7,
        }
    }

    #[test]
    fn each_placeholder_expands() {
        let ctx = ScreenshotContext {
            file_stem: "episode01",
            position: Duration::from_millis(3_723_456),
            counter: 12,
        };
        assert_eq!(expand_template("%f", &ctx).as_deref(), Ok("episode01"));
        assert_eq!(expand_template("%P", &ctx).as_deref(), Ok("01.02.03.456"));
        assert_eq!(expand_template("%n", &ctx).as_deref(), Ok("12"));
        assert_eq!(expand_template("100%%", &ctx).as_deref(), Ok("100%"));
    }

    #[test]
    fn position_formats_zero_and_long_runs() {
        assert_eq!(format_position(Duration::ZERO), "00.00.00.000");
        assert_eq!(
            format_position(Duration::from_millis(65_500)),
            "00.01.05.500"
        );
        assert_eq!(
            format_position(Duration::from_secs(100 * 3_600)),
            "100.00.00.000"
        );
    }

    #[test]
    fn default_template_produces_the_expected_stem() {
        let stem = screenshot_stem("", &context("Show S01E02", 65)).expect("default template");
        assert_eq!(stem, "kattri-Show S01E02-00.01.05.000");
        let explicit =
            screenshot_stem(DEFAULT_TEMPLATE, &context("Show S01E02", 65)).expect("template");
        assert_eq!(explicit, stem);
    }

    #[test]
    fn unknown_placeholder_and_trailing_percent_are_refused() {
        let ctx = context("x", 0);
        assert_eq!(
            expand_template("%x", &ctx),
            Err(TemplateError::UnknownPlaceholder('x'))
        );
        assert_eq!(
            expand_template("%é", &ctx),
            Err(TemplateError::UnknownPlaceholder('é'))
        );
        assert_eq!(
            expand_template("abc%", &ctx),
            Err(TemplateError::TrailingPercent)
        );
    }

    #[test]
    fn overlong_template_is_refused() {
        let long = "a".repeat(MAX_TEMPLATE_CHARS + 1);
        assert_eq!(
            expand_template(&long, &context("x", 0)),
            Err(TemplateError::TooLong)
        );
        let at_limit = "a".repeat(MAX_TEMPLATE_CHARS);
        assert!(expand_template(&at_limit, &context("x", 0)).is_ok());
    }

    #[test]
    fn separators_are_escaped_to_underscores() {
        assert_eq!(sanitize_stem("a/b\\c"), "a_b_c");
        let stem = screenshot_stem("%f", &context("Season 1/Ep 2", 0)).expect("template");
        assert_eq!(stem, "Season 1_Ep 2");
    }

    #[test]
    fn invalid_characters_are_removed() {
        assert_eq!(sanitize_stem("a<b>c:d\"e|f?g*h"), "abcdefgh");
        assert_eq!(sanitize_stem("a\u{0001}b\u{007f}c"), "abc");
    }

    #[test]
    fn trailing_dots_and_spaces_are_trimmed_and_empty_becomes_fallback() {
        assert_eq!(sanitize_stem("name. "), "name");
        assert_eq!(sanitize_stem("  ... "), FALLBACK_STEM);
        assert_eq!(sanitize_stem(""), FALLBACK_STEM);
    }

    #[test]
    fn reserved_device_names_get_a_prefix() {
        assert_eq!(sanitize_stem("CON"), "_CON");
        assert_eq!(sanitize_stem("lpt9.backup"), "_lpt9.backup");
        assert_eq!(sanitize_stem("COM10"), "COM10");
        assert_eq!(sanitize_stem("console"), "console");
    }

    #[test]
    fn long_stems_are_truncated() {
        let long = "x".repeat(MAX_STEM_CHARS + 40);
        assert_eq!(sanitize_stem(&long).chars().count(), MAX_STEM_CHARS);
    }

    #[test]
    fn expanded_file_name_is_not_expanded_again() {
        let stem = screenshot_stem("%f", &context("x%Py%n", 0)).expect("template");
        assert_eq!(stem, "x%Py%n");
    }

    #[test]
    fn collision_appends_a_counter() {
        let existing = vec!["kattri-x.png".to_owned()];
        assert_eq!(
            unique_file_name("kattri-x", ScreenshotFormat::Png, &existing).as_deref(),
            Ok("kattri-x-2.png")
        );
        let more = vec!["kattri-x.png".to_owned(), "kattri-x-2.png".to_owned()];
        assert_eq!(
            unique_file_name("kattri-x", ScreenshotFormat::Png, &more).as_deref(),
            Ok("kattri-x-3.png")
        );
    }

    #[test]
    fn collision_check_ignores_case() {
        let existing = vec!["KATTRI-X.PNG".to_owned()];
        assert_eq!(
            unique_file_name("kattri-x", ScreenshotFormat::Png, &existing).as_deref(),
            Ok("kattri-x-2.png")
        );
    }

    #[test]
    fn free_name_is_returned_unchanged() {
        assert_eq!(
            unique_file_name("shot", ScreenshotFormat::Png, &[]).as_deref(),
            Ok("shot.png")
        );
    }

    #[test]
    fn exhausted_candidates_return_an_error() {
        let mut existing = vec!["shot.png".to_owned()];
        for attempt in 2..=MAX_COLLISION_ATTEMPTS {
            existing.push(format!("shot-{attempt}.png"));
        }
        assert_eq!(
            unique_file_name("shot", ScreenshotFormat::Png, &existing),
            Err(NameError::NoFreeName)
        );
    }

    #[test]
    fn format_accepts_only_png() {
        assert_eq!(ScreenshotFormat::parse("PNG"), Some(ScreenshotFormat::Png));
        assert_eq!(
            ScreenshotFormat::parse(" png "),
            Some(ScreenshotFormat::Png)
        );
        assert_eq!(ScreenshotFormat::parse("jpg"), None);
        assert_eq!(ScreenshotFormat::Png.extension(), "png");
    }

    #[test]
    fn path_joins_a_single_plain_name() {
        let dir = Path::new("pictures");
        assert_eq!(
            screenshot_path(dir, "shot.png").as_deref(),
            Ok(dir.join("shot.png").as_path())
        );
    }

    #[test]
    fn path_refuses_traversal_and_separators() {
        let dir = Path::new("pictures");
        assert_eq!(screenshot_path(dir, ""), Err(PathError::UnsafeFileName));
        assert_eq!(screenshot_path(dir, ".."), Err(PathError::UnsafeFileName));
        assert_eq!(
            screenshot_path(dir, "../x.png"),
            Err(PathError::UnsafeFileName)
        );
        assert_eq!(
            screenshot_path(dir, "sub/x.png"),
            Err(PathError::UnsafeFileName)
        );
        assert_eq!(
            screenshot_path(dir, "sub\\x.png"),
            Err(PathError::UnsafeFileName)
        );
    }

    #[test]
    fn default_mode_is_subtitles_for_legacy_parity() {
        assert_eq!(DEFAULT_MODE, ScreenshotMode::Subtitles);
    }

    #[test]
    fn command_is_built_for_both_modes() {
        let subtitles =
            screenshot_command("C:\\Pics\\shot.png", ScreenshotMode::Subtitles).expect("safe path");
        assert_eq!(subtitles.name, "screenshot-to-file");
        assert_eq!(subtitles.args, vec!["C:\\Pics\\shot.png", "subtitles"]);

        let video =
            screenshot_command("C:\\Pics\\shot.png", ScreenshotMode::Video).expect("safe path");
        assert_eq!(video.args, vec!["C:\\Pics\\shot.png", "video"]);
    }

    #[test]
    fn command_refuses_option_like_or_control_paths() {
        assert_eq!(
            screenshot_command("--script=evil", ScreenshotMode::Video),
            Err(PathError::UnsafeCommandPath)
        );
        assert_eq!(
            screenshot_command("", ScreenshotMode::Video),
            Err(PathError::UnsafeCommandPath)
        );
        assert_eq!(
            screenshot_command("C:\\a\nb.png", ScreenshotMode::Video),
            Err(PathError::UnsafeCommandPath)
        );
    }
}
