//! Refresh-rate matching for the host (M6-04). The choice of mode and the restore bookkeeping are
//! in `kattri_client_core::display_match`; this file connects them to the operating system and to
//! playback: switch when a file with a known frame rate starts, put the original mode back when
//! playback has been over for a moment, on exit, and at the next launch after a crash.
//!
//! Windows and macOS have display backends. Wayland and other unsupported platforms report that
//! switching is unavailable once and change nothing.

use std::time::{Duration, Instant};

#[cfg(any(target_os = "macos", test))]
use kattri_client_core::display_match::DisplayMode;
use kattri_client_core::display_match::{FileRecordStore, MatchError, Matcher, Outcome};

#[cfg(target_os = "macos")]
mod macos_backend;
#[cfg(windows)]
mod windows_backend;

#[cfg(target_os = "macos")]
use macos_backend::PrimaryDisplay as Backend;
#[cfg(all(not(windows), not(target_os = "macos")))]
use unsupported::Unsupported as Backend;
#[cfg(windows)]
use windows_backend::PrimaryDisplay as Backend;

/// How long playback must be over before the original mode is restored. Moving between two files
/// ends one and starts the next within this time, and must not flash the display back.
const RESTORE_GRACE: Duration = Duration::from_millis(1500);

#[cfg(all(not(windows), not(target_os = "macos")))]
mod unsupported {
    use kattri_client_core::display_match::{DisplayBackend, DisplayMode};

    pub(super) struct Unsupported;

    pub(super) const MESSAGE: &str =
        "Display refresh-rate switching is unavailable on this platform.";

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

#[cfg(any(target_os = "macos", test))]
#[derive(Clone, Copy)]
struct ModeProperties {
    display_mode: DisplayMode,
    pixel_width: usize,
    pixel_height: usize,
    io_flags: u32,
    usable_for_desktop: bool,
}

#[cfg(any(target_os = "macos", test))]
fn display_mode_from_values(width: usize, height: usize, refresh_hz: f64) -> Option<DisplayMode> {
    let width = u32::try_from(width).ok()?;
    let height = u32::try_from(height).ok()?;
    if width == 0 || height == 0 || !refresh_hz.is_finite() || refresh_hz <= 0.0 {
        return None;
    }
    let refresh_millihz = (refresh_hz * 1000.0).round();
    if !refresh_millihz.is_finite() || !(1.0..=f64::from(u32::MAX)).contains(&refresh_millihz) {
        return None;
    }
    Some(DisplayMode::new(width, height, refresh_millihz as u32))
}

#[cfg(any(target_os = "macos", test))]
fn is_safe_target_mode(
    candidate: &ModeProperties,
    wanted: DisplayMode,
    current: &ModeProperties,
) -> bool {
    candidate.display_mode == wanted
        && candidate.pixel_width == current.pixel_width
        && candidate.pixel_height == current.pixel_height
        && candidate.io_flags == current.io_flags
        && candidate.usable_for_desktop
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

    #[cfg(all(test, not(windows)))]
    fn is_switched(&self) -> bool {
        self.matcher.is_switched()
    }
}

#[cfg(all(test, not(any(windows, target_os = "macos"))))]
mod tests {
    use super::*;

    #[test]
    fn without_a_backend_nothing_changes_and_the_reason_is_shown_once() {
        let mut switch = DisplaySwitch::new();
        let first = switch.apply(24.0, true);
        assert!(first.is_some_and(|text| text.contains("unavailable on this platform")));
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

#[cfg(test)]
mod mode_tests {
    use super::*;

    #[test]
    fn fractional_refresh_rates_are_kept_to_millihertz() {
        assert_eq!(
            display_mode_from_values(1920, 1080, 23.976),
            Some(DisplayMode::new(1920, 1080, 23_976))
        );
        assert_eq!(
            display_mode_from_values(3840, 2160, 59.94),
            Some(DisplayMode::new(3840, 2160, 59_940))
        );
    }

    #[test]
    fn invalid_display_mode_values_are_refused() {
        for (width, height, refresh) in [
            (0, 1080, 60.0),
            (1920, 0, 60.0),
            (1920, 1080, 0.0),
            (1920, 1080, -1.0),
            (1920, 1080, f64::NAN),
            (1920, 1080, f64::INFINITY),
            (1920, 1080, f64::MAX),
        ] {
            assert_eq!(display_mode_from_values(width, height, refresh), None);
        }
    }

    #[test]
    fn target_mode_preserves_pixel_dimensions_and_display_flags() {
        let wanted = DisplayMode::new(1920, 1080, 24_000);
        let current = ModeProperties {
            display_mode: DisplayMode::new(1920, 1080, 60_000),
            pixel_width: 3840,
            pixel_height: 2160,
            io_flags: 4,
            usable_for_desktop: true,
        };
        let compatible = ModeProperties {
            display_mode: wanted,
            pixel_width: 3840,
            pixel_height: 2160,
            io_flags: 4,
            usable_for_desktop: true,
        };
        assert!(is_safe_target_mode(&compatible, wanted, &current));

        let mut scaled = compatible;
        scaled.pixel_width = 1920;
        assert!(!is_safe_target_mode(&scaled, wanted, &current));

        let mut different_output_flags = compatible;
        different_output_flags.io_flags = 8;
        assert!(!is_safe_target_mode(
            &different_output_flags,
            wanted,
            &current
        ));

        let mut unusable = compatible;
        unusable.usable_for_desktop = false;
        assert!(!is_safe_target_mode(&unusable, wanted, &current));
    }
}
