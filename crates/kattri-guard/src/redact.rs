//! Text redaction shared by crash reports and the production log.
//!
//! Redaction is fail-closed and token based. It is a second guard: callers log state names and
//! counts, never credentials, and the typed `Debug` impls of secret-carrying types are redacted
//! at the source.

/// Opaque runs at least this long are treated as tokens. Jellyfin access tokens and item ids
/// are both 32 hex characters and cannot be told apart, so both are redacted.
const OPAQUE_MIN_LEN: usize = 32;

/// Extra lowercase markers that only matter in free text (headers, cookies, JSON bodies). They
/// are not part of the settings-key denylist because they would refuse legitimate keys.
const EXTRA_KEY_MARKERS: &[&str] = &["cookie", "passwd", "credential", "bearer", "secret"];

use crate::{has_scheme_separator, is_secret_like_key};

/// Replace token-like and URL-like text with markers, line by line.
///
/// Rules, applied to whitespace-separated tokens:
/// - a token containing a URL marker becomes `[redacted-url]`;
/// - a token containing a credential marker and `=` or `:` becomes `[redacted]`, and every
///   later token on the same line is also redacted (fail closed: `Authorization: Bearer x`
///   loses the bearer value even though it has no marker of its own);
/// - an opaque run of at least 32 letters, digits, `-` or `_` that mixes letters and digits
///   becomes `[redacted]`.
///
/// Line breaks and the spacing between kept tokens are preserved. If a credential assignment
/// or auth scheme has no inline value, the next line is redacted as its continuation.
pub fn redact(text: &str) -> String {
    Redactor::default().redact(text)
}

/// Stateful line redactor for writers that receive one event in several physical lines.
///
/// Keeping this state with an event writer prevents a credential value on a later line from
/// escaping when each line is persisted separately.
#[derive(Default)]
pub struct Redactor {
    redact_continuation: bool,
}

impl Redactor {
    /// Redacts text while carrying an empty credential assignment across line boundaries.
    pub fn redact(&mut self, text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        for (index, line) in text.split('\n').enumerate() {
            if index != 0 {
                out.push('\n');
            }
            let (redacted, continues) = redact_line(line, self.redact_continuation);
            out.push_str(&redacted);
            self.redact_continuation = continues;
        }
        out
    }
}

fn redact_line(line: &str, redact_all: bool) -> (String, bool) {
    if redact_all {
        let line_tokens = tokens(line);
        // Empty lines and a scheme-only line (for example Authorization:, then Bearer, then
        // the value) do not consume the credential continuation. A non-empty value does.
        let continues = line_tokens.is_empty()
            || (line_tokens.len() == 1 && is_bare_scheme_word(line_tokens[0].1));
        let mut out = String::with_capacity(line.len());
        let mut last_end = 0;
        for (start, token) in line_tokens {
            out.push_str(&line[last_end..start]);
            out.push_str("[redacted]");
            last_end = start + token.len();
        }
        out.push_str(&line[last_end..]);
        return (out, continues);
    }

    let mut out = String::with_capacity(line.len());
    let mut rest_redacted = false;
    let mut redact_continuation = false;
    let mut last_end = 0;
    for (start, token) in tokens(line) {
        out.push_str(&line[last_end..start]);
        last_end = start + token.len();
        let replacement = if rest_redacted {
            "[redacted]".to_owned()
        } else if has_scheme_separator(token) {
            "[redacted-url]".to_owned()
        } else if is_bare_scheme_word(token) {
            // "Bearer abc" and "Basic abc": the word is kept, everything after it is not.
            rest_redacted = true;
            redact_continuation = line[last_end..].trim().is_empty();
            token.to_owned()
        } else if is_assignment(token) && is_secret_assignment(token) {
            rest_redacted = true;
            redact_continuation = has_empty_secret_assignment(token)
                && has_only_optional_scheme_word(&line[last_end..]);
            "[redacted]".to_owned()
        } else if is_opaque_token(token) {
            "[redacted]".to_owned()
        } else {
            token.to_owned()
        };
        out.push_str(&replacement);
    }
    out.push_str(&line[last_end..]);
    (out, redact_continuation)
}

/// Whether a credential assignment ends at the line boundary, with its value on the next line.
fn has_empty_secret_assignment(token: &str) -> bool {
    token
        .find(['=', ':'])
        .is_some_and(|separator| token[separator + 1..].is_empty())
}

/// An empty credential field followed by only `Bearer` or `Basic` still has its value ahead.
fn has_only_optional_scheme_word(rest: &str) -> bool {
    let mut words = rest.split_whitespace();
    match words.next() {
        None => true,
        Some(word) => is_bare_scheme_word(word) && words.next().is_none(),
    }
}

/// Whitespace-separated tokens with their byte offsets.
fn tokens(line: &str) -> Vec<(usize, &str)> {
    let mut found = Vec::new();
    let mut start: Option<usize> = None;
    for (i, c) in line.char_indices() {
        match (c.is_whitespace(), start) {
            (false, None) => start = Some(i),
            (true, Some(s)) => {
                found.push((s, &line[s..i]));
                start = None;
            }
            _ => {}
        }
    }
    if let Some(s) = start {
        found.push((s, &line[s..]));
    }
    found
}

fn is_opaque_token(token: &str) -> bool {
    token.len() >= OPAQUE_MIN_LEN
        && token
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        && token.chars().any(|c| c.is_ascii_digit())
        && token.chars().any(|c| c.is_ascii_alphabetic())
}

fn is_assignment(token: &str) -> bool {
    token.contains('=') || token.contains(':')
}

/// True when the key side of `key=value` or `"key":value` looks like a credential. The key is
/// percent-decoded first so `api%5Fkey=...` is caught.
fn is_secret_assignment(token: &str) -> bool {
    let decoded = percent_decode(token);
    if is_secret_like_key(&decoded) {
        return true;
    }
    let lower = decoded.to_ascii_lowercase();
    if EXTRA_KEY_MARKERS
        .iter()
        .any(|marker| lower.contains(marker))
    {
        return true;
    }
    // Jellyfin's AuthenticateByName body carries the password in a field named "Pw".
    let trimmed = lower.trim_start_matches(['{', '[', ',', '"', '\'']);
    trimmed
        .strip_prefix("pw")
        .is_some_and(|after| after.starts_with(['"', '\'', '=', ':']))
}

fn is_bare_scheme_word(token: &str) -> bool {
    token.eq_ignore_ascii_case("bearer") || token.eq_ignore_ascii_case("basic")
}

/// Decodes `%XX` escapes for matching only. Invalid escapes are kept as written.
fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(hi), Some(lo)) = (hex(bytes[i + 1]), hex(bytes[i + 2]))
        {
            out.push(hi * 16 + lo);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::redact;

    /// Input, then text that must not survive in the output.
    const LEAKS: &[(&str, &str)] = &[
        ("GET /Items?api_key=abc123def&x=1", "abc123def"),
        ("GET /Items?ApiKey=abc123def", "abc123def"),
        ("GET /Items?api%5Fkey=abc123def", "abc123def"),
        (
            "Authorization: MediaBrowser Client=\"Kattri\", Token=\"abc123def\"",
            "abc123def",
        ),
        (
            "X-Emby-Authorization: MediaBrowser Token=\"abc123def\"",
            "abc123def",
        ),
        ("X-Emby-Token: abc123def", "abc123def"),
        ("X-MediaBrowser-Token: abc123def", "abc123def"),
        ("Authorization: Bearer abc123def", "abc123def"),
        ("sent Bearer abc123def to server", "abc123def"),
        ("sent Basic dXNlcjpwYXNz now", "dXNlcjpwYXNz"),
        ("Cookie: session=abc123def", "abc123def"),
        ("Set-Cookie: sid=abc123def; Path=/", "abc123def"),
        ("{\"AccessToken\": \"abc123def\"}", "abc123def"),
        ("{\"AccessToken\":\"abc123def\"}", "abc123def"),
        ("{\"Username\":\"bob\",\"Pw\":\"hunter22\"}", "hunter22"),
        ("{\"Username\": \"bob\", \"Pw\": \"hunter22\"}", "hunter22"),
        ("body {\"Pw\": \"hunter22\"}", "hunter22"),
        ("password=hunter22", "hunter22"),
        ("passwd: hunter22", "hunter22"),
        ("login failed for bob password: hunter22", "hunter22"),
        ("connect http://bob:hunter22@jellyfin.lan:8096/", "hunter22"),
        (
            "connect https://jellyfin.lan:8096/Videos/1/stream?api_key=abc123def",
            "jellyfin.lan",
        ),
        ("upstream http://192.168.1.50:8096/Items", "192.168.1.50"),
        (
            "session token 0123456789abcdef0123456789abcdef issued",
            "0123456789abcdef0123456789abcdef",
        ),
        ("secret=abc123def", "abc123def"),
        ("client_secret: abc123def", "abc123def"),
    ];

    #[test]
    fn no_secret_survives_the_table() {
        for (input, leak) in LEAKS {
            let out = redact(input);
            assert!(!out.contains(leak), "{input:?} leaked {leak:?}: {out:?}");
        }
    }

    #[test]
    fn plain_text_is_unchanged() {
        for text in [
            "",
            "playback started",
            "item count 42 in 3 ms",
            "state Idle -> Loading",
            "window 1280x720 scale 1.5",
            "mpv profile loaded from embedded baseline",
        ] {
            assert_eq!(redact(text), text);
        }
    }

    #[test]
    fn spacing_and_line_breaks_are_kept() {
        assert_eq!(redact("a  b\n\nc"), "a  b\n\nc");
        assert_eq!(redact("one\npassword=x\nthree"), "one\n[redacted]\nthree");
    }

    #[test]
    fn redaction_is_per_line() {
        let out = redact("Authorization: Bearer abc\nnext line is fine");
        assert!(out.ends_with("next line is fine"), "{out}");
    }

    #[test]
    fn credential_values_on_the_next_line_are_redacted() {
        for (input, secret) in [
            ("Authorization:\nshort-secret", "short-secret"),
            ("Authorization: Bearer\nshort-secret", "short-secret"),
            (
                "{\n  \"AccessToken\":\n  \"short-secret\"\n}",
                "short-secret",
            ),
            ("bEaReR\nshort-secret", "short-secret"),
        ] {
            let output = redact(input);
            assert!(!output.contains(secret), "{input:?} leaked: {output:?}");
        }
    }

    #[test]
    fn redaction_is_idempotent() {
        for (input, _) in LEAKS {
            let once = redact(input);
            assert_eq!(redact(&once), once, "{input:?}");
        }
    }

    #[test]
    fn invalid_percent_escapes_do_not_panic() {
        for text in [
            "%",
            "%2",
            "%zz=1",
            "a%=b",
            "%E2%82",
            "api_key%=x",
            "%ff%fe=1",
        ] {
            let _ = redact(text);
        }
    }

    #[test]
    fn non_ascii_text_is_kept() {
        assert_eq!(
            redact("Sprache gewählt: Deutsch"),
            "Sprache gewählt: Deutsch"
        );
    }

    #[test]
    fn short_words_that_start_with_pw_are_kept() {
        assert_eq!(redact("pwned count 3"), "pwned count 3");
        assert_eq!(redact("power: on"), "power: on");
    }
}
