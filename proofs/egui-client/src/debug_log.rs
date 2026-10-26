//! Debug-build QA log. This module is compiled only with `debug_assertions`; release builds
//! (MSI, zip, DMG and the rest) do not contain it.
//!
//! The log goes to `<KATTRI_DEBUG_DIR>/kattri-debug.log` and only when that variable is set
//! (scripts/run_debug_signin.ps1 sets it). Messages pass through [`redact`] first. Callers log
//! state names and counts, not credentials, and the redaction is a second guard.

use std::io::Write;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Instant;

static START: OnceLock<Instant> = OnceLock::new();

fn log_path() -> Option<PathBuf> {
    let dir = std::env::var_os("KATTRI_DEBUG_DIR")?;
    Some(PathBuf::from(dir).join("kattri-debug.log"))
}

/// Replaces secret-looking values. Keys are matched case-insensitively and the value runs to
/// the next quote, ampersand, whitespace, comma or bracket.
pub fn redact(input: &str) -> String {
    const KEYS: [&str; 6] = [
        "api_key=",
        "apikey=",
        "token=",
        "token\":\"",
        "authorization:",
        "password=",
    ];
    let mut text = input.to_owned();
    if let Some(password) = std::env::var_os("KATTRI_DEBUG_PASSWORD")
        .and_then(|value| value.into_string().ok())
        .filter(|value| value.len() >= 4)
    {
        text = text.replace(&password, "[redacted]");
    }
    for key in KEYS {
        let mut out = String::with_capacity(text.len());
        let mut rest = text.as_str();
        loop {
            let lower = rest.to_ascii_lowercase();
            let Some(at) = lower.find(key) else {
                out.push_str(rest);
                break;
            };
            let value_start = at + key.len();
            out.push_str(&rest[..value_start]);
            let mut tail = &rest[value_start..];
            if let Some(unquoted) = tail.strip_prefix('"') {
                out.push('"');
                tail = unquoted;
            }
            out.push_str("[redacted]");
            let end = tail
                .find(|c: char| {
                    c == '"' || c == '&' || c == ',' || c == ')' || c == '}' || c.is_whitespace()
                })
                .unwrap_or(tail.len());
            rest = &tail[end..];
        }
        text = out;
    }
    text
}

/// Appends one redacted line. Does nothing when `KATTRI_DEBUG_DIR` is not set.
pub fn log(message: &str) {
    let Some(path) = log_path() else { return };
    let elapsed = START.get_or_init(Instant::now).elapsed().as_secs_f64();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(file, "[{elapsed:>9.3}s] {}", redact(message));
    }
}

/// Logs panics (message and location) before the default hook runs.
pub fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        log(&format!("PANIC: {info}"));
        previous(info);
    }));
}

#[cfg(test)]
mod tests {
    use super::redact;

    #[test]
    fn redacts_keys_and_leaves_other_text() {
        assert_eq!(
            redact("GET /Items?api_key=abc123&x=1 ok"),
            "GET /Items?api_key=[redacted]&x=1 ok"
        );
        assert_eq!(
            redact("Token=\"secret\", user=bob"),
            "Token=\"[redacted]\", user=bob"
        );
        assert_eq!(redact("plain text"), "plain text");
    }
}
