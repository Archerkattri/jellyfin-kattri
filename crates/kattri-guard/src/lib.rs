//! Shared credential and URL denylist for Jellyfin Kattri.
//!
//! This is the one definition of the rules that keep credentials and upstream URLs out of
//! anything the app persists or shares. Matching is substring based and case-insensitive
//! (ASCII lowercasing). A key or value that matches is refused, not rewritten.
//!
//! Rules:
//! - [`SECRET_KEY_MARKERS`]: a key name containing one of these is credential-like.
//! - [`SECRET_VALUE_MARKERS`]: a value containing one of these is a URL or a credential query.
//! - [`has_scheme_separator`]: any `://` counts as a URL. Crash redaction uses this broader
//!   check.
//!
//! A future legitimate key that contains a marker (for example `authentication_mode`) is
//! refused on purpose. Rename the key rather than weakening this list.
//!
//! Users: `kattri-settings` (re-exported as `kattri_settings::guard`, which also adds the TOML
//! walkers because they need the `toml` crate), `kattri-plugins` (declared keys and stored
//! values), `kattri-downloads` (persisted manifest and store strings) and `kattri-client-core`
//! (crash summary redaction).
//!
//! [`redact`] rewrites free text (crash reports and the production log) with the same rules.
//!
//! This crate has no dependencies.

mod redact;

pub use redact::{Redactor, redact};

/// Lowercase substrings that make a key credential-like. Matched against the lowercased key name.
pub const SECRET_KEY_MARKERS: &[&str] = &[
    "password", "token", "secret", "apikey", "api_key", "access", "username", "auth",
];

/// Lowercase substrings that must not appear in any string value: URLs (upstream or otherwise),
/// credential query parameters and credential header or assignment forms. Matching is
/// case-insensitive, so the entries are stored in lowercase.
pub const SECRET_VALUE_MARKERS: &[&str] = &[
    "http://",
    "https://",
    "api_key=",
    "apikey=",
    "token=",
    "password=",
    "passwd=",
    "secret=",
    "auth=",
    "authorization:",
    "bearer ",
];

/// URL scheme separator. Any text containing it is treated as a URL by crash redaction.
pub const SCHEME_SEPARATOR: &str = "://";

/// Returns true if `key` contains a credential-like marker (case-insensitive).
pub fn is_secret_like_key(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    SECRET_KEY_MARKERS
        .iter()
        .any(|marker| lower.contains(marker))
}

/// Returns true if `value` contains a URL or credential query marker (case-insensitive).
pub fn is_secret_like_value(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    SECRET_VALUE_MARKERS
        .iter()
        .any(|marker| lower.contains(marker))
}

/// Returns true if `text` contains a URL scheme separator (`://`). Broader than
/// [`is_secret_like_value`], which only knows `http://` and `https://`.
pub fn has_scheme_separator(text: &str) -> bool {
    text.contains(SCHEME_SEPARATOR)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_assignment_is_refused_as_a_value() {
        assert!(is_secret_like_value("/Videos/1?auth=abc"));
        assert!(is_secret_like_value("Auth=abc.def"));
        assert!(is_secret_like_value("client_auth=abc"));
        assert!(!is_secret_like_value("Authors=Gaiman"));
    }

    #[test]
    fn key_markers_match_case_insensitively_as_substrings() {
        assert!(is_secret_like_key("PlaybackApiKey"));
        assert!(is_secret_like_key("Server_Password"));
        assert!(
            is_secret_like_key("authentication_mode"),
            "refused on purpose"
        );
        assert!(!is_secret_like_key("volume"));
    }

    #[test]
    fn value_markers_catch_urls_and_credential_queries() {
        assert!(is_secret_like_value("https://host.example/x"));
        assert!(is_secret_like_value("/Videos/1?API_KEY=abc"));
        assert!(is_secret_like_value("/Videos/1?token=abc"));
        assert!(!is_secret_like_value("/Videos/1"));
        assert!(
            !is_secret_like_value("api_key"),
            "no '=' means no query marker"
        );
    }

    #[test]
    fn credential_assignment_markers_are_refused_case_insensitively() {
        assert!(is_secret_like_value("/Videos/1?ApiKey=abc"));
        assert!(is_secret_like_value("Password=hunter2"));
        assert!(is_secret_like_value("passwd=hunter2"));
        assert!(is_secret_like_value("client_SECRET=abc"));
        assert!(is_secret_like_value("Authorization: abc"));
        assert!(is_secret_like_value("Bearer abc.def"));
    }

    #[test]
    fn existing_value_markers_are_still_refused() {
        assert!(is_secret_like_value("api_key=abc"));
        assert!(is_secret_like_value("token=abc"));
    }

    #[test]
    fn ordinary_text_is_accepted_as_a_value() {
        assert!(!is_secret_like_value("Hello world"));
        assert!(!is_secret_like_value("Kitchen sink, season 2"));
        assert!(!is_secret_like_value("passwords are managed elsewhere"));
        assert!(!is_secret_like_value("Keep the secret of the recipe"));
        assert!(!is_secret_like_value("Authorized users only"));
        assert!(!is_secret_like_value("bearer"));
        assert!(!is_secret_like_value("Dolby Atmos, 7.1 channels"));
    }

    #[test]
    fn scheme_separator_is_broader_than_value_markers() {
        assert!(has_scheme_separator("ftp://host"));
        assert!(has_scheme_separator("http://10.0.0.5:8096"));
        assert!(!is_secret_like_value("ftp://host"));
        assert!(!has_scheme_separator("plain words"));
    }
}
