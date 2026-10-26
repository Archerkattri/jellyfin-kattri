//! Credential guard: the single denylist that keeps secrets out of the settings file.
//!
//! Credentials (passwords, access tokens, Quick Connect secrets, API keys) belong in the
//! OS credential store (see `kattri-platform::CredentialStore`), never in `settings.toml`.
//! The typed schema has no credential fields. This module backs that rule in two places:
//!
//! - [`crate::store::load_from_str`] rejects a file that contains a credential-like key,
//!   so a hand-edited or foreign key is never silently carried around.
//! - `tests/no_secrets.rs` checks the serialized output of the default and fully populated
//!   settings against the same constants.
//!
//! The denylist constants and the string matchers live in the `kattri-guard` crate and are
//! re-exported here, so `kattri_settings::guard` keeps its public API. The TOML walkers below
//! stay in this crate because they need `toml::Table`, and `kattri-guard` has no dependencies.
//!
//! Matching is substring based and case-insensitive. A future legitimate key that contains
//! one of these words (for example `authentication_mode`) is refused on purpose; rename the
//! key rather than weakening this list.

use toml::{Table, Value};

pub use kattri_guard::{
    SECRET_KEY_MARKERS, SECRET_VALUE_MARKERS, is_secret_like_key, is_secret_like_value,
};

/// Dotted path of the first credential-like key in `table`, depth first, or `None`.
pub fn find_secret_like_key(table: &Table) -> Option<String> {
    first_match(table, "", &|key, _| is_secret_like_key(key))
}

/// Dotted path of the first key whose string value looks like a URL or credential, or `None`.
pub fn find_secret_like_value(table: &Table) -> Option<String> {
    first_match(table, "", &|_, value| value_has_secret_text(value))
}

/// Removes every string value that looks like a URL or credential, at any depth, and returns
/// the dotted paths of the removed values. Array items that match are removed from their
/// array, and the array path is reported once. Values are never returned.
pub fn remove_secret_like_values(table: &mut Table) -> Vec<String> {
    let mut removed = Vec::new();
    strip_values(table, "", &mut removed);
    removed
}

fn is_secret_like_string(value: &Value) -> bool {
    matches!(value, Value::String(text) if is_secret_like_value(text))
}

fn strip_values(table: &mut Table, prefix: &str, removed: &mut Vec<String>) {
    let doomed: Vec<String> = table
        .iter()
        .filter(|(_, value)| is_secret_like_string(value))
        .map(|(key, _)| key.clone())
        .collect();
    for key in doomed {
        table.remove(key.as_str());
        removed.push(join(prefix, &key));
    }

    for (key, value) in table.iter_mut() {
        let path = join(prefix, key);
        match value {
            Value::Table(inner) => strip_values(inner, &path, removed),
            Value::Array(items) => {
                let before = items.len();
                items.retain(|item| !is_secret_like_string(item));
                if items.len() != before {
                    removed.push(path.clone());
                }
                for item in items.iter_mut() {
                    if let Value::Table(inner) = item {
                        strip_values(inner, &path, removed);
                    }
                }
            }
            _ => {}
        }
    }
}

fn value_has_secret_text(value: &Value) -> bool {
    match value {
        Value::String(text) => is_secret_like_value(text),
        Value::Array(items) => items.iter().any(value_has_secret_text),
        _ => false,
    }
}

fn join(prefix: &str, key: &str) -> String {
    if prefix.is_empty() {
        key.to_owned()
    } else {
        format!("{prefix}.{key}")
    }
}

fn first_match(table: &Table, prefix: &str, hit: &dyn Fn(&str, &Value) -> bool) -> Option<String> {
    for (key, value) in table {
        let path = join(prefix, key);
        if hit(key, value) {
            return Some(path);
        }
        if let Some(found) = descend(value, &path, hit) {
            return Some(found);
        }
    }
    None
}

fn descend(value: &Value, path: &str, hit: &dyn Fn(&str, &Value) -> bool) -> Option<String> {
    match value {
        Value::Table(inner) => first_match(inner, path, hit),
        Value::Array(items) => items.iter().find_map(|item| descend(item, path, hit)),
        _ => None,
    }
}
