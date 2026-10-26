//! Local crash reporting. No network, no telemetry, nothing uploaded.
//!
//! - Off by default. [`plan_panic_write`] returns nothing while the setting is off, and
//!   [`write_panic_report`] then touches no file at all.
//! - A crash summary is plain text built from the app version, the panic message after
//!   [`redact`], and crate names only (no paths, no memory). It is safe to share.
//! - Crash files are named by timestamp and app version. [`files_to_prune`] keeps the
//!   newest [`KEEP_LAST`] in the crash directory and removes the rest.
//! - [`install_panic_hook`] chains onto the existing hook, so the usual panic output still
//!   appears. Writing a file never panics; a write error is ignored.
//!
//! The credential and URL rules come from `kattri-guard`, the one shared denylist. Thresholds
//! marked [UNCONFIRMED] are proposals.

use std::any::Any;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Number of crash files kept on disk.
pub const KEEP_LAST: usize = 5;

/// Crash reporting setting. Off unless the user turns it on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CrashReportingSettings {
    pub enabled: bool,
}

impl CrashReportingSettings {
    /// Reads the saved `diagnostics.crash_reporting` switch. A missing switch is off.
    pub fn from_diagnostics(diagnostics: &kattri_settings::schema::DiagnosticsSettings) -> Self {
        Self {
            enabled: diagnostics.crash_reporting,
        }
    }
}

/// Plain-text crash summary. Build it with [`CrashSummary::new`], which redacts and limits
/// every field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrashSummary {
    app_version: String,
    timestamp_secs: u64,
    message: String,
    crates: Vec<String>,
}

impl CrashSummary {
    /// Build a summary. `panic_message` is redacted. `crate_names` keeps only names made of
    /// `[A-Za-z0-9_-]` up to 64 characters; anything that looks like a path is dropped.
    pub fn new(
        app_version: &str,
        timestamp_secs: u64,
        panic_message: &str,
        crate_names: &[&str],
    ) -> Self {
        let crates = crate_names
            .iter()
            .filter(|name| is_crate_name(name))
            .map(|name| (*name).to_owned())
            .collect();
        Self {
            app_version: sanitize_version(app_version),
            timestamp_secs,
            message: redact(panic_message),
            crates,
        }
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    /// The text written to the summary file.
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str("Kattri crash summary\n");
        out.push_str(&format!("version: {}\n", self.app_version));
        out.push_str(&format!("unix_time: {}\n", self.timestamp_secs));
        out.push_str(&format!("crates: {}\n", self.crates.join(", ")));
        out.push_str(&format!("message: {}\n", self.message));
        out
    }

    /// File name for this crash, for example `crash-1700000000-1.2.3.txt`.
    pub fn file_name(&self) -> String {
        format!("crash-{}-{}.txt", self.timestamp_secs, self.app_version)
    }
}

/// The summary to write for a panic, or `None` when crash reporting is off.
/// With the setting off, no file is written at all.
pub fn plan_panic_write(
    settings: CrashReportingSettings,
    summary: &CrashSummary,
) -> Option<String> {
    settings.enabled.then(|| summary.render())
}

/// Write the crash summary for one panic into `dir` when reporting is enabled.
///
/// Returns `Ok(None)` without touching the file system when reporting is off. When it is on,
/// the summary is written to a temporary name and renamed into place, and then the crash
/// directory is pruned to [`KEEP_LAST`] files. Returns the path of the new file.
pub fn write_panic_report(
    dir: &Path,
    settings: CrashReportingSettings,
    app_version: &str,
    timestamp_secs: u64,
    panic_message: &str,
    crate_names: &[&str],
) -> io::Result<Option<PathBuf>> {
    let summary = CrashSummary::new(app_version, timestamp_secs, panic_message, crate_names);
    let Some(text) = plan_panic_write(settings, &summary) else {
        return Ok(None);
    };
    fs::create_dir_all(dir)?;
    let file_name = summary.file_name();
    let final_path = dir.join(&file_name);
    let temp_path = dir.join(format!("{file_name}.tmp"));
    fs::write(&temp_path, text)?;
    // On Windows the rename replaces an existing file of the same name.
    fs::rename(&temp_path, &final_path)?;
    prune_crash_directory(dir)?;
    Ok(Some(final_path))
}

/// Record an error message from the running app through the same redaction and retention as
/// a panic. Ordinary errors do not stop the app, so callers ignore a failed write.
///
/// Off by default: with reporting off this returns `Ok(None)` and touches no file.
pub fn record_error_report(
    dir: &Path,
    settings: CrashReportingSettings,
    app_version: &str,
    error_text: &str,
) -> io::Result<Option<PathBuf>> {
    write_panic_report(dir, settings, app_version, unix_now(), error_text, &[])
}

/// Install a panic hook that records crashes under `dir` when `settings` enables reporting.
///
/// The previous hook runs first, so panic output still reaches stderr. With reporting off the
/// hook only delegates and writes nothing.
pub fn install_panic_hook(
    dir: PathBuf,
    settings: CrashReportingSettings,
    app_version: &'static str,
    crate_names: &'static [&'static str],
) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        previous(info);
        let message = payload_text(info.payload());
        // A failed write must not panic inside the hook, so the error is discarded.
        let _ = write_panic_report(
            &dir,
            settings,
            app_version,
            unix_now(),
            &message,
            crate_names,
        );
    }));
}

/// The text of a panic payload. Only `&str` and `String` payloads carry text.
fn payload_text(payload: &(dyn Any + Send)) -> String {
    if let Some(text) = payload.downcast_ref::<&str>() {
        return (*text).to_owned();
    }
    if let Some(text) = payload.downcast_ref::<String>() {
        return text.clone();
    }
    "non-text panic payload".to_owned()
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

/// Remove crash files beyond the newest [`KEEP_LAST`]. Only names that match the crash file
/// pattern are considered, so other files in the directory are never touched.
fn prune_crash_directory(dir: &Path) -> io::Result<()> {
    let mut files = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if !(name.starts_with("crash-") && name.ends_with(".txt")) {
            continue;
        }
        let stamp = name
            .strip_prefix("crash-")
            .and_then(|rest| rest.split('-').next())
            .and_then(|digits| digits.parse::<u64>().ok())
            .unwrap_or(0);
        files.push((name, stamp));
    }
    for name in files_to_prune(files) {
        fs::remove_file(dir.join(name))?;
    }
    Ok(())
}

/// Given `(file name, unix seconds)` pairs, return the names to delete so only the newest
/// [`KEEP_LAST`] remain. Ties are broken by name so the result is deterministic.
pub fn files_to_prune(mut files: Vec<(String, u64)>) -> Vec<String> {
    files.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    files
        .into_iter()
        .skip(KEEP_LAST)
        .map(|(name, _)| name)
        .collect()
}

pub use kattri_guard::redact;

fn is_crate_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Keep only `[A-Za-z0-9.+-]`, at most 32 characters. Anything else becomes `unknown`.
fn sanitize_version(version: &str) -> String {
    let ok = !version.is_empty()
        && version.len() <= 32
        && version
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '+' | '-'));
    if ok {
        version.to_owned()
    } else {
        "unknown".to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FAKE_TOKEN: &str = "tok_FAKE_0123456789abcdef";

    #[test]
    fn crash_reporting_is_off_by_default() {
        assert!(!CrashReportingSettings::default().enabled);
        let summary = CrashSummary::new("1.2.3", 1_700_000_000, "boom", &["kattri_client_core"]);
        assert_eq!(
            plan_panic_write(CrashReportingSettings::default(), &summary),
            None
        );
    }

    #[test]
    fn saved_switch_defaults_to_off_and_maps_on_when_set() {
        let defaults = kattri_settings::Settings::default();
        assert_eq!(
            CrashReportingSettings::from_diagnostics(&defaults.diagnostics),
            CrashReportingSettings { enabled: false }
        );
        let mut on = kattri_settings::Settings::default();
        on.diagnostics.crash_reporting = true;
        assert!(CrashReportingSettings::from_diagnostics(&on.diagnostics).enabled);
    }

    #[test]
    fn report_text_drops_bearer_values_query_tokens_and_hosts() {
        let message = "GET https://jf.example.org/Items?api_key=KEY123 \
                       Authorization: Bearer BEARER456 at 10.0.0.5:8096";
        let summary = CrashSummary::new("1.2.3", 9, message, &[]);
        let text = summary.render();
        for secret in ["KEY123", "BEARER456", "jf.example.org", "10.0.0.5"] {
            assert!(!text.contains(secret), "{secret} leaked: {text}");
        }
    }

    #[test]
    fn enabled_setting_plans_exactly_one_summary_text() {
        let summary = CrashSummary::new("1.2.3", 1_700_000_000, "boom", &["kattri_client_core"]);
        let text = plan_panic_write(CrashReportingSettings { enabled: true }, &summary)
            .expect("enabled writes a summary");
        assert!(text.contains("version: 1.2.3"));
        assert!(text.contains("message: boom"));
        assert!(text.contains("crates: kattri_client_core"));
        assert_eq!(summary.file_name(), "crash-1700000000-1.2.3.txt");
    }

    #[test]
    fn panic_message_with_a_fake_token_is_redacted_in_the_summary() {
        let message =
            format!("failed to sync with token={FAKE_TOKEN} at https://media.example.org/x");
        let summary = CrashSummary::new("1.2.3", 1, &message, &[]);
        let text = summary.render();
        assert!(!text.contains(FAKE_TOKEN), "token leaked: {text}");
        assert!(!text.contains("media.example.org"), "URL leaked: {text}");
        assert!(text.contains("[redacted"));
    }

    #[test]
    fn bearer_authorization_text_loses_the_rest_of_the_line() {
        let out = redact("request failed Authorization: Bearer abc123 retry");
        assert!(!out.contains("abc123"), "{out}");
        assert_eq!(
            out,
            "request failed [redacted] [redacted] [redacted] [redacted]"
        );
    }

    #[test]
    fn key_value_secrets_and_urls_are_redacted() {
        assert_eq!(redact("api_key=abc"), "[redacted]");
        assert_eq!(redact("server=https://h.example.org"), "[redacted-url]");
        assert_eq!(
            redact("see http://10.0.0.5:8096/Items"),
            "see [redacted-url]"
        );
    }

    #[test]
    fn opaque_long_tokens_are_redacted_but_plain_words_are_kept() {
        let opaque = "a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6";
        assert_eq!(redact(&format!("id {opaque}")), "id [redacted]");
        assert_eq!(redact("plain words stay here"), "plain words stay here");
        assert_eq!(
            redact("abcdefghijklmnopqrstuvwxyzabcdef"),
            "abcdefghijklmnopqrstuvwxyzabcdef",
            "letters only is not treated as a token"
        );
    }

    #[test]
    fn redaction_keeps_line_breaks() {
        assert_eq!(redact("one\ntoken=x\nthree"), "one\n[redacted]\nthree");
    }

    #[test]
    fn crate_names_that_look_like_paths_are_dropped() {
        let summary = CrashSummary::new(
            "1.2.3",
            1,
            "x",
            &[
                "kattri_client_core",
                "C:\\Users\\krishi\\secret",
                "/home/u/lib",
                "ok-name",
            ],
        );
        let text = summary.render();
        assert!(
            text.contains("crates: kattri_client_core, ok-name"),
            "{text}"
        );
        assert!(!text.contains("krishi"));
        assert!(!text.contains("home"));
    }

    #[test]
    fn version_is_sanitized_into_the_file_name() {
        let summary = CrashSummary::new("1.0.0/../../x", 5, "x", &[]);
        assert_eq!(summary.file_name(), "crash-5-unknown.txt");
        let ok = CrashSummary::new("1.0.0-beta.1+build", 5, "x", &[]);
        assert_eq!(ok.file_name(), "crash-5-1.0.0-beta.1+build.txt");
    }

    #[test]
    fn retention_keeps_the_newest_five() {
        let files: Vec<(String, u64)> = (1..=7).map(|i| (format!("crash-{i}.txt"), i)).collect();
        assert_eq!(
            files_to_prune(files),
            vec!["crash-2.txt".to_owned(), "crash-1.txt".to_owned()]
        );
    }

    #[test]
    fn retention_does_nothing_at_or_below_the_limit() {
        let files: Vec<(String, u64)> = (1..=5).map(|i| (format!("crash-{i}.txt"), i)).collect();
        assert!(files_to_prune(files).is_empty());
        assert!(files_to_prune(Vec::new()).is_empty());
    }

    #[test]
    fn retention_is_deterministic_for_equal_timestamps() {
        let files = vec![
            ("b.txt".to_owned(), 1),
            ("a.txt".to_owned(), 1),
            ("c.txt".to_owned(), 9),
            ("d.txt".to_owned(), 9),
            ("e.txt".to_owned(), 9),
            ("f.txt".to_owned(), 9),
        ];
        assert_eq!(files_to_prune(files), vec!["b.txt".to_owned()]);
    }

    fn scratch_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!("kattri-crash-{tag}-{}-{nanos}", std::process::id()))
    }

    #[test]
    fn hook_writes_nothing_when_disabled() {
        let dir = scratch_dir("disabled");
        let written = write_panic_report(
            &dir,
            CrashReportingSettings::default(),
            "1.2.3",
            1_700_000_000,
            &format!("failed with token={FAKE_TOKEN} at https://media.example.org/x"),
            &["kattri_client_core"],
        )
        .expect("a disabled hook does not fail");

        assert_eq!(written, None);
        assert!(
            !dir.exists(),
            "no directory or file is created while reporting is off"
        );
    }

    #[test]
    fn hook_writes_a_redacted_file_when_enabled() {
        let dir = scratch_dir("enabled");
        let path = write_panic_report(
            &dir,
            CrashReportingSettings { enabled: true },
            "1.2.3",
            1_700_000_000,
            &format!("failed with token={FAKE_TOKEN} at https://media.example.org/x"),
            &["kattri_client_core"],
        )
        .expect("write succeeds")
        .expect("enabled reporting writes a file");

        assert_eq!(
            path.file_name().and_then(|name| name.to_str()),
            Some("crash-1700000000-1.2.3.txt")
        );
        let text = fs::read_to_string(&path).expect("crash file is readable");
        assert!(text.contains("version: 1.2.3"), "{text}");
        assert!(text.contains("crates: kattri_client_core"), "{text}");
        assert!(!text.contains(FAKE_TOKEN), "token leaked: {text}");
        assert!(!text.contains("media.example.org"), "URL leaked: {text}");
        assert!(text.contains("[redacted"), "{text}");
        let temp_left = fs::read_dir(&dir)
            .expect("directory is readable")
            .filter_map(Result::ok)
            .any(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"));
        assert!(!temp_left, "the temporary file is renamed away");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn error_report_writes_nothing_when_disabled() {
        let dir = scratch_dir("error-disabled");
        let written = record_error_report(
            &dir,
            CrashReportingSettings::default(),
            "1.2.3",
            &format!("load failed token={FAKE_TOKEN}"),
        )
        .expect("a disabled report does not fail");
        assert_eq!(written, None);
        assert!(
            !dir.exists(),
            "no directory is created while reporting is off"
        );
    }

    #[test]
    fn error_report_redacts_the_error_text_when_enabled() {
        let dir = scratch_dir("error-enabled");
        let path = record_error_report(
            &dir,
            CrashReportingSettings { enabled: true },
            "1.2.3",
            &format!("load failed token={FAKE_TOKEN} from https://media.example.org/x"),
        )
        .expect("write succeeds")
        .expect("enabled reporting writes a file");
        let text = fs::read_to_string(&path).expect("report is readable");
        assert!(!text.contains(FAKE_TOKEN), "token leaked: {text}");
        assert!(!text.contains("media.example.org"), "URL leaked: {text}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn enabled_writes_keep_only_the_newest_crash_files() {
        let dir = scratch_dir("retention");
        for stamp in 1..=7_u64 {
            write_panic_report(
                &dir,
                CrashReportingSettings { enabled: true },
                "1.2.3",
                stamp,
                "boom",
                &[],
            )
            .expect("write succeeds");
        }
        let mut names: Vec<String> = fs::read_dir(&dir)
            .expect("directory is readable")
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names.len(), KEEP_LAST);
        assert_eq!(names.first().map(String::as_str), Some("crash-3-1.2.3.txt"));
        assert_eq!(names.last().map(String::as_str), Some("crash-7-1.2.3.txt"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn payload_text_reads_str_and_string_payloads_only() {
        let as_str: Box<dyn Any + Send> = Box::new("static text");
        let as_string: Box<dyn Any + Send> = Box::new(String::from("owned text"));
        let other: Box<dyn Any + Send> = Box::new(42_u32);
        assert_eq!(payload_text(as_str.as_ref()), "static text");
        assert_eq!(payload_text(as_string.as_ref()), "owned text");
        assert_eq!(payload_text(other.as_ref()), "non-text panic payload");
    }
}
