//! Refresh-rate matching for the host (M6-04). The choice of mode and the restore bookkeeping are
//! in `kattri_client_core::display_match`; this file connects them to the operating system and to
//! playback: switch when a file with a known frame rate starts, put the original mode back when
//! playback has been over for a moment, on exit, and at the next launch after a crash.
//!
//! Only Windows has a display backend. Linux and macOS report that switching is unavailable once
//! and change nothing.

use std::time::{Duration, Instant};

use kattri_client_core::display_match::{
    DisplayBackend, DisplayMode, FileRecordStore, MatchError, Matcher, Outcome,
};

#[cfg(windows)]
mod windows_backend;

#[cfg(not(windows))]
use unsupported::Unsupported as Backend;
#[cfg(windows)]
use windows_backend::PrimaryDisplay as Backend;

/// How long playback must be over before the original mode is restored. Moving between two files
/// ends one and starts the next within this time, and must not flash the display back.
const RESTORE_GRACE: Duration = Duration::from_millis(1500);

#[cfg(not(windows))]
mod unsupported {
    use super::{DisplayBackend, DisplayMode};

    pub(super) struct Unsupported;

    pub(super) const MESSAGE: &str = "Display refresh-rate switching is only available on Windows.";

    impl DisplayBackend for Unsupported {
        type Error = String;
        fn current_mode(&mut self) -> Result<DisplayMode, String> {
            Err(MESSAGE.to_owned())
        }
        fn available_modes(&mut self) -> Result<Vec<DisplayMode>, String> {
            Err(MESSAGE.to_owned())
        }
        fn set_mode(&mut self, _mode: DisplayMode) -> Result<(), String> {
            Err(MESSAGE.to_owned())
        }
    }
}

pub(crate) struct DisplaySwitch {
    matcher: Matcher<Backend, FileRecordStore>,
    /// When playback was first seen to be over, while a switch is active.
    idle_since: Option<Instant>,
    /// A failure is shown once per kind of failure, not on every file.
    last_error: Option<String>,
    /// The start-up recovery ran.
    recovered: bool,
}

impl DisplaySwitch {
    pub(crate) fn new() -> Self {
        let path = directories::ProjectDirs::from("org", "Jellyfin", "Kattri")
            .map(|dirs| dirs.data_local_dir().join("display-mode-record.txt"))
            .unwrap_or_else(|| std::env::temp_dir().join("kattri-display-mode-record.txt"));
        Self {
            matcher: Matcher::new(Backend, FileRecordStore::new(path)),
            idle_since: None,
            last_error: None,
            recovered: false,
        }
    }

    /// Put the display back after a crash. [`Self::tick`] runs it on its first call, whatever the
    /// setting says, so a mode left behind by an earlier run is never kept.
    fn recover(&mut self) -> Option<String> {
        match self.matcher.recover() {
            Ok(Outcome::Restored(mode)) => {
                tracing::info!(%mode, "restored the display mode left by an earlier run");
                None
            }
            Ok(_) => None,
            Err(error) => self.report(error),
        }
    }

    /// Switch to the mode that suits `content_fps`.
    pub(crate) fn apply(&mut self, content_fps: f64, avoid_25_30_hz: bool) -> Option<String> {
        self.idle_since = None;
        match self.matcher.apply(content_fps, avoid_25_30_hz) {
            Ok(Outcome::Switched(mode)) => {
                tracing::info!(%mode, content_fps, "display mode matched to the content");
                None
            }
            Ok(_) => None,
            Err(error) => self.report(error),
        }
    }

    /// Call every frame. Restores the original mode once playback has been over for
    /// [`RESTORE_GRACE`].
    pub(crate) fn tick(&mut self, has_media: bool, now: Instant) -> Option<String> {
        if !self.recovered {
            self.recovered = true;
            if let Some(notice) = self.recover() {
                return Some(notice);
            }
        }
        if has_media || !self.matcher.is_switched() {
            self.idle_since = None;
            return None;
        }
        let since = *self.idle_since.get_or_insert(now);
        if now.saturating_duration_since(since) < RESTORE_GRACE {
            return None;
        }
        self.idle_since = None;
        self.restore_now()
    }

    /// Restore at once, for exit.
    pub(crate) fn restore_now(&mut self) -> Option<String> {
        match self.matcher.restore() {
            Ok(Outcome::Restored(mode)) => {
                tracing::info!(%mode, "display mode restored");
                None
            }
            Ok(_) => None,
            Err(error) => self.report(error),
        }
    }

    fn report(&mut self, error: MatchError) -> Option<String> {
        let text = error.to_string();
        tracing::warn!(error = %text, "display refresh-rate switching");
        if self.last_error.as_deref() == Some(text.as_str()) {
            return None;
        }
        self.last_error = Some(text.clone());
        Some(text)
    }

    #[cfg(test)]
    fn is_switched(&self) -> bool {
        self.matcher.is_switched()
    }
}

#[cfg(all(test, not(windows)))]
mod tests {
    use super::*;

    #[test]
    fn without_a_backend_nothing_changes_and_the_reason_is_shown_once() {
        let mut switch = DisplaySwitch::new();
        let first = switch.apply(24.0, true);
        assert!(first.is_some_and(|text| text.contains("only available on Windows")));
        assert_eq!(switch.apply(24.0, true), None);
        assert!(!switch.is_switched());
        let now = Instant::now();
        assert_eq!(switch.tick(false, now + Duration::from_secs(10)), None);
        assert_eq!(switch.restore_now(), None);
    }

    #[test]
    fn recovering_with_no_record_does_nothing() {
        let mut switch = DisplaySwitch::new();
        assert_eq!(switch.recover(), None);
    }
}
