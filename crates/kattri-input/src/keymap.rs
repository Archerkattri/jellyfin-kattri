//! JSON keymap loading, compatible with the legacy Neo inputmap format.
//!
//! A legacy file looks like this (see `tests/fixtures/keyboard.json`):
//!
//! ```text
//! {
//!   "name": "Keyboard Generic",
//!   "idmatcher": "Keyboard.*",
//!   "mapping": {
//!     "(Shift\\+)?Left": "left",
//!     "(Esc|Backspace)": { "short": "back", "long": "exit" },
//!     "Space": ["space", "play_pause"],
//!     "KEY_BUTTON_10": ""
//!   }
//! }
//! ```
//!
//! Compatibility decisions:
//! - `//` and `/* */` comments are allowed (the legacy files use them).
//! - Key patterns are regex-like. Only a small subset is expanded: escapes (`\+`),
//!   optional groups `(...)?`, alternation `(a|b)`, non-capturing `(?:...)`, and
//!   single-character classes `[0-9]`. Anything else is recorded as skipped with a reason.
//! - A single target is a string, a list of strings, or `{ short, long }`.
//! - Unknown action names, text-passthrough targets and empty (unbound) targets are
//!   recorded in [`Keymap::skipped`] and never abort the load.
//! - Idmatcher regexes are stored verbatim. They are matched by the platform backend,
//!   not here.
//! - A pattern that appears twice in `mapping` keeps the later entry and adds a
//!   [`Keymap::warnings`] notice. It is not silently overwritten.
//!
//! Not yet implemented (tracked for M2): merging a user override file on top of the
//! bundled map, and writing rebinding changes back to disk.

use std::collections::BTreeMap;
use std::fmt;

use serde_json::Value;

use crate::action::{Action, ActionParseError};
use crate::json_keys::duplicate_object_keys;

/// Upper bound on how many literal keys one pattern may expand into.
const MAX_VARIANTS: usize = 512;

/// A keyboard key with modifiers, normalized from legacy spelling such as `Ctrl+Shift+F`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct KeyChord {
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    pub meta: bool,
    /// Key name as written in the legacy file, for example `Left`, `Return`, `F11`, `A`.
    pub key: String,
}

impl KeyChord {
    /// Parse `Shift+Left`, `Ctrl+Shift+F`, `Meta+Down` and similar.
    pub fn parse(text: &str) -> Result<Self, PatternError> {
        let mut chord = KeyChord::default();
        let mut key = text;
        while let Some((part, remainder)) = key.split_once('+') {
            let modifier = match part {
                "Shift" => Some(&mut chord.shift),
                "Ctrl" | "Control" => Some(&mut chord.ctrl),
                "Alt" => Some(&mut chord.alt),
                "Meta" => Some(&mut chord.meta),
                _ => None,
            };
            let Some(flag) = modifier else { break };
            if *flag {
                return Err(PatternError::Chord(text.to_owned()));
            }
            *flag = true;
            key = remainder;
        }
        // A trailing plus is the key itself (`Ctrl++` means Ctrl plus plus).
        // Modifiers are consumed only from the front, so the remaining text can
        // contain a literal plus produced by the legacy keymap's `\+` escape.
        if key.is_empty() {
            return Err(PatternError::Chord(text.to_owned()));
        }
        if key.contains('+') && key != "+" {
            return Err(PatternError::Chord(text.to_owned()));
        }
        chord.key = key.to_owned();
        Ok(chord)
    }
}

/// What a binding listens for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Trigger {
    Key(KeyChord),
    /// A controller, remote or other non-keyboard button, by its literal name
    /// (for example `KEY_BUTTON_0`). Device-specific naming is the backend's concern.
    Button(String),
}

/// What a trigger does. `ShortLong` supports press-and-hold behaviour.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Response {
    Press(Vec<Action>),
    ShortLong {
        short: Vec<Action>,
        long: Vec<Action>,
    },
}

/// One resolved binding from a keymap file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    pub trigger: Trigger,
    pub response: Response,
    /// The original legacy pattern, kept for diagnostics and re-saving.
    pub source_pattern: String,
}

/// A pattern or target that was not loaded, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skipped {
    pub pattern: String,
    pub reason: String,
}

/// Whether the keymap's triggers are keys or buttons, decided from the idmatcher.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceKind {
    Keyboard,
    Buttons,
}

/// A parsed keymap file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Keymap {
    pub name: String,
    /// Legacy idmatcher regex, stored verbatim.
    pub device_matcher: String,
    pub kind: DeviceKind,
    pub bindings: Vec<Binding>,
    pub skipped: Vec<Skipped>,
    /// Notices that do not stop the load, such as a pattern written twice in `mapping`.
    /// For a repeated pattern the later entry is used, as in the legacy loader.
    pub warnings: Vec<String>,
}

impl Keymap {
    /// Find the first binding for a key chord.
    pub fn lookup_key(&self, chord: &KeyChord) -> Option<&Binding> {
        self.bindings
            .iter()
            .find(|b| matches!(&b.trigger, Trigger::Key(k) if k == chord))
    }

    /// Find the first binding for a button name.
    pub fn lookup_button(&self, button: &str) -> Option<&Binding> {
        self.bindings
            .iter()
            .find(|b| matches!(&b.trigger, Trigger::Button(name) if name == button))
    }
}

/// Errors that stop a keymap from loading at all.
#[derive(Debug, thiserror::Error)]
pub enum KeymapError {
    #[error("keymap is not valid JSON (after removing comments): {0}")]
    Json(#[from] serde_json::Error),
    #[error("keymap is missing or has a non-string field `{0}`")]
    Field(&'static str),
    #[error("keymap `mapping` must be an object")]
    Mapping,
}

/// Errors from expanding one key pattern.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PatternError {
    /// A regex feature this loader does not expand (for example `*`, `+`, `.`, `^`).
    Unsupported(char),
    /// Unbalanced parentheses or brackets.
    Unbalanced,
    /// A pattern that expands to more than the allowed number of keys.
    TooMany,
    /// A chord that is not `[Mod+]...+Key`.
    Chord(String),
}

impl fmt::Display for PatternError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported(c) => write!(f, "regex feature `{c}` is not supported"),
            Self::Unbalanced => write!(f, "unbalanced group or class"),
            Self::TooMany => write!(f, "pattern expands to more than {MAX_VARIANTS} keys"),
            Self::Chord(text) => write!(f, "`{text}` is not a valid key chord"),
        }
    }
}

/// Parse a legacy keymap JSON document. Comments are stripped first.
pub fn parse_keymap(text: &str) -> Result<Keymap, KeymapError> {
    let stripped = strip_comments(text);
    let value: Value = serde_json::from_str(&stripped)?;
    let warnings: Vec<String> = duplicate_object_keys(&stripped)
        .into_iter()
        .map(|key| format!("`{key}` appears more than once in the keymap; the later entry is used"))
        .collect();
    let name = string_field(&value, "name")?;
    let device_matcher = string_field(&value, "idmatcher")?;
    let kind = if device_matcher.starts_with("Keyboard") {
        DeviceKind::Keyboard
    } else {
        DeviceKind::Buttons
    };
    let mapping = value
        .get("mapping")
        .and_then(Value::as_object)
        .ok_or(KeymapError::Mapping)?;

    // BTreeMap gives a deterministic binding order regardless of JSON key order.
    let ordered: BTreeMap<&String, &Value> = mapping.iter().collect();

    let mut bindings = Vec::new();
    let mut skipped = Vec::new();
    for (pattern, target) in ordered {
        let variants = match expand_pattern(pattern) {
            Ok(v) => v,
            Err(err) => {
                skipped.push(Skipped {
                    pattern: pattern.clone(),
                    reason: err.to_string(),
                });
                continue;
            }
        };
        let response = match parse_target(target, pattern, &mut skipped) {
            Some(r) => r,
            None => continue,
        };
        for variant in variants {
            let trigger = match kind {
                DeviceKind::Keyboard => match KeyChord::parse(&variant) {
                    Ok(chord) => Trigger::Key(chord),
                    Err(err) => {
                        skipped.push(Skipped {
                            pattern: pattern.clone(),
                            reason: err.to_string(),
                        });
                        continue;
                    }
                },
                DeviceKind::Buttons => Trigger::Button(variant.clone()),
            };
            bindings.push(Binding {
                trigger,
                response: response.clone(),
                source_pattern: pattern.clone(),
            });
        }
    }

    Ok(Keymap {
        name,
        device_matcher,
        kind,
        bindings,
        skipped,
        warnings,
    })
}

fn string_field(value: &Value, field: &'static str) -> Result<String, KeymapError> {
    value
        .get(field)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or(KeymapError::Field(field))
}

/// Turn a legacy target value into a [`Response`]. Problems are pushed to `skipped`.
fn parse_target(target: &Value, pattern: &str, skipped: &mut Vec<Skipped>) -> Option<Response> {
    match target {
        Value::Object(map) => {
            let short = map.get("short").map(|v| actions_from(v, pattern, skipped));
            let long = map.get("long").map(|v| actions_from(v, pattern, skipped));
            let short = short.unwrap_or_default();
            let long = long.unwrap_or_default();
            if short.is_empty() && long.is_empty() {
                skipped.push(Skipped {
                    pattern: pattern.to_owned(),
                    reason: "short/long target has no usable actions".to_owned(),
                });
                return None;
            }
            Some(Response::ShortLong { short, long })
        }
        other => {
            let actions = actions_from(other, pattern, skipped);
            if actions.is_empty() {
                // Empty string means "explicitly unbound"; record it without alarm.
                skipped.push(Skipped {
                    pattern: pattern.to_owned(),
                    reason: "unbound or no usable actions".to_owned(),
                });
                return None;
            }
            Some(Response::Press(actions))
        }
    }
}

/// Read a string or array of strings into actions, recording every unusable name.
fn actions_from(value: &Value, pattern: &str, skipped: &mut Vec<Skipped>) -> Vec<Action> {
    let names: Vec<&str> = match value {
        Value::String(s) => vec![s.as_str()],
        Value::Array(items) => items.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    };
    let mut actions = Vec::new();
    for name in names {
        if name.is_empty() {
            continue;
        }
        match Action::from_legacy(name) {
            Ok(action) => actions.push(action),
            Err(ActionParseError::Unknown(n)) => skipped.push(Skipped {
                pattern: pattern.to_owned(),
                reason: format!("unknown action `{n}`"),
            }),
            Err(ActionParseError::Unsupported(n)) => skipped.push(Skipped {
                pattern: pattern.to_owned(),
                reason: format!("action `{n}` is not supported yet"),
            }),
        }
    }
    actions
}

/// Expand a legacy key pattern into literal keys.
///
/// Supported syntax: `\x` escapes, `(a|b)` alternation, `(...)?` and `(?:...)` groups,
/// `X?` optional single atoms, and `[a-z0-9]` classes. Quantifiers `*`, `+`, `{}` and
/// anchors are rejected, as are `.` and `^`/`$`.
pub fn expand_pattern(pattern: &str) -> Result<Vec<String>, PatternError> {
    let chars: Vec<char> = pattern.chars().collect();
    let (variants, pos) = parse_sequence(&chars, 0, false)?;
    if pos != chars.len() {
        return Err(PatternError::Unbalanced);
    }
    Ok(variants)
}

fn parse_sequence(
    chars: &[char],
    mut pos: usize,
    in_group: bool,
) -> Result<(Vec<String>, usize), PatternError> {
    let mut variants = vec![String::new()];
    while pos < chars.len() {
        let c = chars[pos];
        if in_group && (c == '|' || c == ')') {
            break;
        }
        let (mut atom, next) = match c {
            '(' => parse_group(chars, pos + 1)?,
            '[' => parse_class(chars, pos + 1)?,
            '\\' => {
                let escaped = *chars.get(pos + 1).ok_or(PatternError::Unbalanced)?;
                (vec![escaped.to_string()], pos + 2)
            }
            ')' | ']' => return Err(PatternError::Unbalanced),
            '.' | '^' | '$' | '*' | '+' | '{' | '}' | '|' => {
                return Err(PatternError::Unsupported(c));
            }
            literal => (vec![literal.to_string()], pos + 1),
        };
        pos = next;
        match chars.get(pos) {
            Some('?') => {
                atom.push(String::new());
                pos += 1;
            }
            Some(q @ ('*' | '+' | '{')) => return Err(PatternError::Unsupported(*q)),
            _ => {}
        }
        variants = cross(&variants, &atom)?;
    }
    Ok((variants, pos))
}

/// Parse a group body starting just after `(`. Returns the alternatives and the position after `)`.
fn parse_group(chars: &[char], mut pos: usize) -> Result<(Vec<String>, usize), PatternError> {
    if chars.get(pos) == Some(&'?') && chars.get(pos + 1) == Some(&':') {
        pos += 2;
    }
    let mut alternatives = Vec::new();
    loop {
        let (alt, next) = parse_sequence(chars, pos, true)?;
        alternatives.extend(alt);
        pos = next;
        match chars.get(pos) {
            Some('|') => pos += 1,
            Some(')') => return Ok((alternatives, pos + 1)),
            _ => return Err(PatternError::Unbalanced),
        }
    }
}

/// Parse a character class starting just after `[`. Supports `a-z` ranges and single characters.
fn parse_class(chars: &[char], mut pos: usize) -> Result<(Vec<String>, usize), PatternError> {
    let mut out = Vec::new();
    loop {
        let c = *chars.get(pos).ok_or(PatternError::Unbalanced)?;
        match c {
            ']' => return Ok((out, pos + 1)),
            '^' if out.is_empty() => return Err(PatternError::Unsupported('^')),
            '\\' => {
                let escaped = *chars.get(pos + 1).ok_or(PatternError::Unbalanced)?;
                out.push(escaped.to_string());
                pos += 2;
            }
            start => {
                if chars.get(pos + 1) == Some(&'-') && chars.get(pos + 2).is_some_and(|c| *c != ']')
                {
                    let end = chars[pos + 2];
                    if end < start {
                        return Err(PatternError::Unbalanced);
                    }
                    for ch in start..=end {
                        out.push(ch.to_string());
                    }
                    pos += 3;
                } else {
                    out.push(start.to_string());
                    pos += 1;
                }
            }
        }
        if out.len() > MAX_VARIANTS {
            return Err(PatternError::TooMany);
        }
    }
}

fn cross(left: &[String], right: &[String]) -> Result<Vec<String>, PatternError> {
    if left.len().saturating_mul(right.len()) > MAX_VARIANTS {
        return Err(PatternError::TooMany);
    }
    let mut out = Vec::with_capacity(left.len() * right.len());
    for l in left {
        for r in right {
            out.push(format!("{l}{r}"));
        }
    }
    Ok(out)
}

/// Remove `//` line comments and `/* */` block comments that are outside string literals.
pub fn strip_comments(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    let mut in_string = false;
    while let Some(c) = chars.next() {
        if in_string {
            out.push(c);
            if c == '\\' {
                if let Some(next) = chars.next() {
                    out.push(next);
                }
            } else if c == '"' {
                in_string = false;
            }
        } else if c == '"' {
            in_string = true;
            out.push(c);
        } else if c == '/' && chars.peek() == Some(&'/') {
            while let Some(&next) = chars.peek() {
                if next == '\n' {
                    break;
                }
                chars.next();
            }
        } else if c == '/' && chars.peek() == Some(&'*') {
            chars.next();
            let mut prev = '\0';
            for next in chars.by_ref() {
                if prev == '*' && next == '/' {
                    break;
                }
                prev = next;
            }
            out.push(' ');
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEYBOARD: &str = include_str!("../tests/fixtures/keyboard.json");
    const DUALSHOCK: &str = include_str!("../tests/fixtures/dualshock4.json");

    #[test]
    fn expands_optional_shift_group() {
        let mut v = expand_pattern("(Shift\\+)?Left").unwrap();
        v.sort();
        assert_eq!(v, vec!["Left".to_owned(), "Shift+Left".to_owned()]);
    }

    #[test]
    fn expands_alternation_and_classes() {
        assert_eq!(
            expand_pattern("(Esc|Backspace)").unwrap(),
            vec!["Esc", "Backspace"]
        );
        assert_eq!(expand_pattern("[0-2]").unwrap(), vec!["0", "1", "2"]);
        assert_eq!(expand_pattern("(?:Shift\\+)?([0-1])").unwrap().len(), 4);
    }

    #[test]
    fn unescapes_literal_plus_and_dot() {
        assert_eq!(expand_pattern("Ctrl\\+A").unwrap(), vec!["Ctrl+A"]);
        assert_eq!(expand_pattern("\\.").unwrap(), vec!["."]);
    }

    #[test]
    fn rejects_regex_features_outside_the_subset() {
        assert_eq!(
            expand_pattern("Shift\\+.*"),
            Err(PatternError::Unsupported('.'))
        );
        assert_eq!(expand_pattern("A+"), Err(PatternError::Unsupported('+')));
        assert_eq!(expand_pattern("(A"), Err(PatternError::Unbalanced));
        assert_eq!(expand_pattern("A)"), Err(PatternError::Unbalanced));
    }

    #[test]
    fn caps_expansion_size() {
        // 26 * 26 * 26 = 17576 variants, over the cap of 512.
        assert_eq!(
            expand_pattern("[A-Z][A-Z][A-Z]"),
            Err(PatternError::TooMany)
        );
    }

    #[test]
    fn chord_parsing_normalizes_modifiers() {
        let c = KeyChord::parse("Ctrl+Shift+F").unwrap();
        assert!(c.ctrl && c.shift && !c.alt && !c.meta);
        assert_eq!(c.key, "F");
        assert!(KeyChord::parse("Shift+").is_err());
        assert!(KeyChord::parse("A+B").is_err());
    }

    #[test]
    fn chord_parser_accepts_plus_as_a_key_with_optional_modifiers() {
        let plus = KeyChord::parse("+").unwrap();
        assert_eq!(plus.key, "+");
        let ctrl_plus = KeyChord::parse("Ctrl++").unwrap();
        assert!(ctrl_plus.ctrl);
        assert_eq!(ctrl_plus.key, "+");
    }

    #[test]
    fn strips_line_and_block_comments_but_not_inside_strings() {
        let src = "{ \"a\": \"http://x\", // trailing\n /* block */ \"b\": 1 }";
        let stripped = strip_comments(src);
        let v: Value = serde_json::from_str(&stripped).unwrap();
        assert_eq!(v["a"], "http://x");
        assert_eq!(v["b"], 1);
    }

    #[test]
    fn loads_legacy_keyboard_fixture() {
        let km = parse_keymap(KEYBOARD).unwrap();
        assert_eq!(km.name, "Keyboard Generic");
        assert_eq!(km.kind, DeviceKind::Keyboard);

        let left = KeyChord::parse("Left").unwrap();
        let shift_left = KeyChord::parse("Shift+Left").unwrap();
        assert!(matches!(km.lookup_key(&left).unwrap().response,
            Response::Press(ref a) if a == &[Action::Left]));
        assert!(km.lookup_key(&shift_left).is_some());

        let esc = KeyChord::parse("Esc").unwrap();
        assert!(matches!(km.lookup_key(&esc).unwrap().response,
            Response::ShortLong { ref short, ref long }
                if short == &[Action::Back] && long == &[Action::Exit]));

        let ctrl_w = KeyChord::parse("Ctrl+W").unwrap();
        assert_eq!(
            km.lookup_key(&ctrl_w).unwrap().response,
            Response::Press(vec![Action::Host(crate::action::HostAction::Quit)])
        );
    }

    #[test]
    fn array_targets_keep_known_actions_and_report_unknown_ones() {
        let km = parse_keymap(KEYBOARD).unwrap();
        let space = KeyChord::parse("Space").unwrap();
        assert_eq!(
            km.lookup_key(&space).unwrap().response,
            Response::Press(vec![Action::PlayPause])
        );
        assert!(km.skipped.iter().any(|s| s.reason.contains("`space`")));
    }

    #[test]
    fn text_passthrough_entries_are_skipped_not_fatal() {
        let km = parse_keymap(KEYBOARD).unwrap();
        assert!(km.skipped.iter().any(|s| s.reason.contains("%1")));
        assert!(!km.bindings.is_empty());
    }

    #[test]
    fn loads_legacy_gamepad_fixture_with_button_triggers() {
        let km = parse_keymap(DUALSHOCK).unwrap();
        assert_eq!(km.kind, DeviceKind::Buttons);
        let b = km.lookup_button("KEY_BUTTON_1").unwrap();
        assert_eq!(b.response, Response::Press(vec![Action::Enter]));
        // Empty target "" means unbound: must not produce a binding.
        assert!(km.lookup_button("KEY_BUTTON_10").is_none());
    }

    #[test]
    fn loads_legacy_xbox_windows_fixture() {
        const XBOX: &str = include_str!("../tests/fixtures/xbox-controller-windows.json");
        let km = parse_keymap(XBOX).unwrap();
        assert_eq!(km.kind, DeviceKind::Buttons);
        // The file lists KEY_BUTTON_3 twice; serde_json keeps the last entry, so `search` wins.
        let b = km.lookup_button("KEY_BUTTON_3").unwrap();
        assert_eq!(b.response, Response::Press(vec![Action::Search]));
        // The repeat is reported, not silently overwritten.
        assert!(
            km.warnings.iter().any(|w| w.contains("KEY_BUTTON_3")),
            "{:?}",
            km.warnings
        );
        // Empty target "" means unbound.
        assert!(km.lookup_button("KEY_BUTTON_8").is_none());
    }

    #[test]
    fn repeated_pattern_keeps_the_later_target_and_warns() {
        let text = r#"{"name":"x","idmatcher":"Keyboard.*","mapping":{"Space":"play_pause","Space":"pause"}}"#;
        let km = parse_keymap(text).unwrap();
        let space = KeyChord::parse("Space").unwrap();
        assert_eq!(
            km.lookup_key(&space).unwrap().response,
            Response::Press(vec![Action::Pause])
        );
        assert_eq!(km.warnings.len(), 1, "{:?}", km.warnings);
        assert!(km.warnings[0].contains("`Space`"));
    }

    #[test]
    fn clean_keymap_has_no_warnings() {
        assert!(parse_keymap(KEYBOARD).unwrap().warnings.is_empty());
        assert!(parse_keymap(DUALSHOCK).unwrap().warnings.is_empty());
    }

    #[test]
    fn missing_mapping_is_an_error() {
        let err = parse_keymap(r#"{"name":"x","idmatcher":"Keyboard.*"}"#).unwrap_err();
        assert!(matches!(err, KeymapError::Mapping));
    }
}
