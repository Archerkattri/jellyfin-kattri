//! Localization framework for Jellyfin Kattri (M4-18).
//!
//! Strings are looked up by message id from a catalog loaded from a `key = value` file. A lookup
//! walks a fallback chain: the requested locale, then its base language, then en-US. A missing
//! id renders as [`MISSING_TEXT`] (never the raw id) and is reported once through
//! [`Translator::take_new_issues`].
//!
//! Format (`*.kv`): `key = value` per line, `#` comments, values trimmed, escapes `\\ \n \r \t`.
//! Placeholders are `{name}`; `{{` and `}}` render as literal braces. Substituted values are
//! inserted literally and never re-scanned.
//!
//! Not in this crate: Fluent (`.ftl`) and plural rules, locale detection from the OS, locale
//! formatting of dates and numbers (English-only for now), and right-to-left layout.

#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::todo,
        clippy::unimplemented
    )
)]

pub mod access;
mod catalog;
pub mod ids;
mod locale;
mod template;
mod translator;

use std::fmt;

pub use access::{AccessibleName, CONTROLS, ControlName, accessible_names};
pub use catalog::Catalog;
pub use locale::LocaleId;
pub use template::{Rendered, TemplateError, render, validate};
pub use translator::{MISSING_TEXT, Translator};

/// The shipped English catalog source.
pub const EN_US_SOURCE: &str = include_str!("../locales/en-US/kattri.kv");

/// The shipped German catalog source. It is a stub with three messages; the rest fall back.
pub const DE_DE_SOURCE: &str = include_str!("../locales/de-DE/kattri.kv");

/// Parses the shipped English catalog.
pub fn english() -> Result<Catalog, I18nError> {
    Catalog::parse(LocaleId::en_us(), EN_US_SOURCE)
}

/// Parses the shipped German stub catalog.
pub fn german_stub() -> Result<Catalog, I18nError> {
    Catalog::parse(LocaleId::parse("de-DE")?, DE_DE_SOURCE)
}

/// Errors from parsing catalogs and building translators. Messages carry ids, line numbers and
/// locale tags only, never translated text or argument values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum I18nError {
    /// The locale tag is not `language` or `language-REGION`.
    InvalidLocale(String),
    /// A line has no `=` separator.
    MissingSeparator { line: usize },
    /// A key is empty or contains characters outside `[A-Za-z0-9_.-]`.
    InvalidKey { line: usize },
    /// A value contains an unknown or dangling escape.
    BadEscape { line: usize },
    /// The same message id appears twice in one catalog.
    DuplicateId(String),
    /// An id passed to [`Catalog::insert`] is not a valid key.
    InvalidId(String),
    /// A value starts or ends with whitespace, which the file format would drop.
    WhitespaceEdge(String),
    /// A value is not a well-formed template.
    Template { id: String, reason: TemplateError },
    /// The translator was built without the en-US catalog, which is the mandatory last fallback.
    MissingFallback,
}

impl fmt::Display for I18nError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidLocale(tag) => write!(f, "invalid locale tag `{tag}`"),
            Self::MissingSeparator { line } => write!(f, "line {line}: expected `key = value`"),
            Self::InvalidKey { line } => write!(f, "line {line}: invalid message id"),
            Self::BadEscape { line } => write!(f, "line {line}: unknown or dangling escape"),
            Self::DuplicateId(id) => write!(f, "duplicate message id `{id}`"),
            Self::InvalidId(id) => write!(f, "invalid message id `{id}`"),
            Self::WhitespaceEdge(id) => write!(f, "message `{id}` starts or ends with whitespace"),
            Self::Template { id, reason } => write!(f, "message `{id}`: {reason}"),
            Self::MissingFallback => write!(f, "the en-US catalog is required as the fallback"),
        }
    }
}

impl std::error::Error for I18nError {}
