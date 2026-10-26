use std::collections::BTreeMap;

use crate::I18nError;
use crate::locale::LocaleId;
use crate::template;

/// One locale's messages: id to template. Templates are validated when inserted, so every stored
/// value renders without error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Catalog {
    locale: LocaleId,
    entries: BTreeMap<String, String>,
}

fn is_valid_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

/// Resolves the escapes `\\ \n \r \t`. Returns `None` for an unknown or dangling escape.
fn unescape(raw: &str) -> Option<String> {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next()? {
            '\\' => out.push('\\'),
            'n' => out.push('\n'),
            'r' => out.push('\r'),
            't' => out.push('\t'),
            _ => return None,
        }
    }
    Some(out)
}

fn escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            other => out.push(other),
        }
    }
    out
}

impl Catalog {
    pub fn new(locale: LocaleId) -> Self {
        Self {
            locale,
            entries: BTreeMap::new(),
        }
    }

    pub fn locale(&self) -> &LocaleId {
        &self.locale
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The raw template for `id`, if this catalog has it.
    pub fn get(&self, id: &str) -> Option<&str> {
        self.entries.get(id).map(String::as_str)
    }

    /// Message ids in sorted order.
    pub fn ids(&self) -> impl Iterator<Item = &str> {
        self.entries.keys().map(String::as_str)
    }

    /// Inserts or replaces a message. Rejects invalid ids, values with leading or trailing
    /// whitespace (the file format trims them), and malformed templates.
    pub fn insert(&mut self, id: &str, template_text: &str) -> Result<(), I18nError> {
        if !is_valid_id(id) {
            return Err(I18nError::InvalidId(id.to_owned()));
        }
        if template_text.starts_with(char::is_whitespace)
            || template_text.ends_with(char::is_whitespace)
        {
            return Err(I18nError::WhitespaceEdge(id.to_owned()));
        }
        template::validate(template_text).map_err(|reason| I18nError::Template {
            id: id.to_owned(),
            reason,
        })?;
        self.entries.insert(id.to_owned(), template_text.to_owned());
        Ok(())
    }

    /// Parses a `key = value` catalog. The locale is supplied by the caller, not read from the
    /// file.
    pub fn parse(locale: LocaleId, source: &str) -> Result<Self, I18nError> {
        let mut catalog = Self::new(locale);
        for (index, raw) in source.lines().enumerate() {
            let line_no = index + 1;
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                return Err(I18nError::MissingSeparator { line: line_no });
            };
            let key = key.trim();
            if !is_valid_id(key) {
                return Err(I18nError::InvalidKey { line: line_no });
            }
            let value = unescape(value.trim()).ok_or(I18nError::BadEscape { line: line_no })?;
            if catalog.entries.contains_key(key) {
                return Err(I18nError::DuplicateId(key.to_owned()));
            }
            catalog.insert(key, &value)?;
        }
        Ok(catalog)
    }

    /// Serializes the catalog in sorted id order. `Catalog::parse` of the output yields an equal
    /// catalog.
    pub fn to_source(&self) -> String {
        let mut out = String::new();
        for (id, value) in &self.entries {
            out.push_str(id);
            out.push_str(" = ");
            out.push_str(&escape(value));
            out.push('\n');
        }
        out
    }
}
