//! Window memory model (M2-14): validate saved geometry and place the window against monitor
//! rectangles passed in as data. Pure: no windowing calls and no I/O.
//!
//! Saved geometry is in logical (DPI-scaled) pixels. Monitor bounds are in physical pixels, and each
//! monitor reports its scale factor. Restore converts logical to physical with the target monitor's
//! scale, so a window saved on a 2.0 display and restored on a 1.0 display keeps its logical size.
//!
//! Persistence: [`WindowRecord`] is the plain form of the `window` settings section. It is
//! untrusted until [`SavedWindow::from_record`] validates it. The settings layer maps the fields to
//! `x`, `y`, `width`, `height`, `monitorName`, `maximized`, and `fullscreen`.

/// Smallest window size, in logical pixels (M2-14 requirement 4).
pub const MIN_LOGICAL_WIDTH: i64 = 640;
pub const MIN_LOGICAL_HEIGHT: i64 = 360;

/// A restored window must overlap its monitor by at least this many physical pixels on each axis.
pub const MIN_VISIBLE_PX: i64 = 100;

/// Fallback window size is clamped to this percentage of the primary monitor. Product choice.
pub const FALLBACK_PERCENT: i64 = 90;

/// Size used when nothing was saved, in logical pixels. Product choice.
pub const DEFAULT_LOGICAL_WIDTH: i64 = 1280;
pub const DEFAULT_LOGICAL_HEIGHT: i64 = 720;

const SCALE_MIN: f64 = 0.5;
const SCALE_MAX: f64 = 4.0;

/// A rectangle in physical pixels.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PixelRect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

/// One monitor as reported by the host. Bounds are physical pixels in the desktop coordinate space.
#[derive(Clone, Debug, PartialEq)]
pub struct MonitorInfo {
    pub name: String,
    pub bounds: PixelRect,
    pub scale_factor: f64,
    pub is_primary: bool,
}

/// The `window` settings section, as stored. Every field is optional so an older or damaged file
/// still loads.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct WindowRecord {
    pub x: Option<i32>,
    pub y: Option<i32>,
    pub width: Option<i32>,
    pub height: Option<i32>,
    pub monitor_name: Option<String>,
    pub maximized: Option<bool>,
    pub fullscreen: Option<bool>,
}

/// Validated saved geometry in logical pixels.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SavedWindow {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    /// Empty when the monitor was not known at save time.
    pub monitor_name: String,
    pub maximized: bool,
    pub fullscreen: bool,
}

impl SavedWindow {
    /// Returns `None` when a required value is missing or a size is not positive. The caller then
    /// uses defaults.
    pub fn from_record(record: &WindowRecord) -> Option<Self> {
        let width = u32::try_from(record.width?).ok().filter(|&w| w > 0)?;
        let height = u32::try_from(record.height?).ok().filter(|&h| h > 0)?;
        Some(Self {
            x: record.x?,
            y: record.y?,
            width,
            height,
            monitor_name: record.monitor_name.clone().unwrap_or_default(),
            maximized: record.maximized.unwrap_or(false),
            fullscreen: record.fullscreen.unwrap_or(false),
        })
    }

    pub fn to_record(&self) -> WindowRecord {
        WindowRecord {
            x: Some(self.x),
            y: Some(self.y),
            width: Some(to_i32(i64::from(self.width))),
            height: Some(to_i32(i64::from(self.height))),
            monitor_name: Some(self.monitor_name.clone()),
            maximized: Some(self.maximized),
            fullscreen: Some(self.fullscreen),
        }
    }

    /// Capture a normal-state window (not PiP) from physical pixels. The result is in logical
    /// pixels of `monitor`.
    pub fn capture(
        physical: PixelRect,
        monitor: &MonitorInfo,
        maximized: bool,
        fullscreen: bool,
    ) -> Self {
        let scale = sanitize_scale(monitor.scale_factor);
        Self {
            x: to_i32(to_logical(i64::from(physical.x), scale)),
            y: to_i32(to_logical(i64::from(physical.y), scale)),
            width: to_u32(to_logical(i64::from(physical.width), scale)).max(1),
            height: to_u32(to_logical(i64::from(physical.height), scale)).max(1),
            monitor_name: monitor.name.clone(),
            maximized,
            fullscreen,
        }
    }
}

/// Where the host should put the window.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Placement {
    /// Place at `rect` (physical pixels) on the named monitor.
    Window {
        monitor_name: String,
        rect: PixelRect,
        maximized: bool,
        fullscreen: bool,
    },
    /// No monitor is reported. Use the logical size and let the platform choose the position.
    Unplaced {
        logical_width: u32,
        logical_height: u32,
        maximized: bool,
    },
}

/// Keep a scale factor finite and inside `0.5..=4.0`. A non-finite value becomes 1.0.
pub fn sanitize_scale(scale: f64) -> f64 {
    if scale.is_finite() {
        scale.clamp(SCALE_MIN, SCALE_MAX)
    } else {
        1.0
    }
}

/// Convert a logical length to physical pixels at `scale`.
pub fn to_physical(logical: i64, scale: f64) -> i64 {
    let scale = sanitize_scale(scale);
    (logical as f64 * scale).round() as i64
}

/// Convert a physical length to logical pixels at `scale`.
pub fn to_logical(physical: i64, scale: f64) -> i64 {
    let scale = sanitize_scale(scale);
    (physical as f64 / scale).round() as i64
}

/// Choose where to place the window.
///
/// Rules, in order:
/// 1. Saved size is raised to the minimum (640 x 360 logical). With no saved state, the default size
///    is used.
/// 2. If the saved monitor name matches a monitor and the window overlaps that monitor by at least
///    [`MIN_VISIBLE_PX`] on each axis, restore the position and size on that monitor. Fullscreen is
///    restored only in this case. The size is capped to the monitor's bounds.
/// 3. Otherwise, place the window centered on the primary monitor. The size is capped to
///    [`FALLBACK_PERCENT`] of that monitor. The window starts windowed. Maximized is kept.
/// 4. With no monitors reported, return [`Placement::Unplaced`].
pub fn plan_placement(saved: Option<&SavedWindow>, monitors: &[MonitorInfo]) -> Placement {
    let logical_width = saved
        .map_or(DEFAULT_LOGICAL_WIDTH, |s| i64::from(s.width))
        .max(MIN_LOGICAL_WIDTH);
    let logical_height = saved
        .map_or(DEFAULT_LOGICAL_HEIGHT, |s| i64::from(s.height))
        .max(MIN_LOGICAL_HEIGHT);
    let maximized = saved.is_some_and(|s| s.maximized);

    let Some(first) = monitors.first() else {
        return Placement::Unplaced {
            logical_width: to_u32(logical_width),
            logical_height: to_u32(logical_height),
            maximized,
        };
    };

    if let Some(saved) = saved.filter(|s| !s.monitor_name.is_empty()) {
        let named = monitors.iter().find(|m| m.name == saved.monitor_name);
        if let Some(monitor) = named {
            let scale = sanitize_scale(monitor.scale_factor);
            let width = to_physical(logical_width, scale);
            let height = to_physical(logical_height, scale);
            let x = to_physical(i64::from(saved.x), scale);
            let y = to_physical(i64::from(saved.y), scale);
            if overlaps_enough(x, y, width, height, monitor.bounds) {
                if saved.fullscreen {
                    // Fullscreen covers the current size of the saved monitor, not the saved size.
                    return Placement::Window {
                        monitor_name: monitor.name.clone(),
                        rect: monitor.bounds,
                        maximized,
                        fullscreen: true,
                    };
                }
                let bounds = monitor.bounds;
                let width = width.min(i64::from(bounds.width));
                let height = height.min(i64::from(bounds.height));
                return Placement::Window {
                    monitor_name: monitor.name.clone(),
                    rect: PixelRect {
                        x: to_i32(x),
                        y: to_i32(y),
                        width: to_u32(width),
                        height: to_u32(height),
                    },
                    maximized,
                    fullscreen: saved.fullscreen,
                };
            }
        }
    }

    let primary = monitors.iter().find(|m| m.is_primary).unwrap_or(first);
    let scale = sanitize_scale(primary.scale_factor);
    let bounds = primary.bounds;
    let max_width = i64::from(bounds.width) * FALLBACK_PERCENT / 100;
    let max_height = i64::from(bounds.height) * FALLBACK_PERCENT / 100;
    let width = to_physical(logical_width, scale).min(max_width);
    let height = to_physical(logical_height, scale).min(max_height);
    let x = i64::from(bounds.x) + (i64::from(bounds.width) - width) / 2;
    let y = i64::from(bounds.y) + (i64::from(bounds.height) - height) / 2;
    Placement::Window {
        monitor_name: primary.name.clone(),
        rect: PixelRect {
            x: to_i32(x),
            y: to_i32(y),
            width: to_u32(width),
            height: to_u32(height),
        },
        maximized,
        fullscreen: false,
    }
}

/// True when the rectangle overlaps `bounds` by at least [`MIN_VISIBLE_PX`] on both axes.
fn overlaps_enough(x: i64, y: i64, width: i64, height: i64, bounds: PixelRect) -> bool {
    let overlap_x = overlap_len(x, width, i64::from(bounds.x), i64::from(bounds.width));
    let overlap_y = overlap_len(y, height, i64::from(bounds.y), i64::from(bounds.height));
    overlap_x >= MIN_VISIBLE_PX && overlap_y >= MIN_VISIBLE_PX
}

fn overlap_len(start_a: i64, len_a: i64, start_b: i64, len_b: i64) -> i64 {
    let start = start_a.max(start_b);
    let end = (start_a + len_a).min(start_b + len_b);
    (end - start).max(0)
}

fn to_i32(value: i64) -> i32 {
    i32::try_from(value.clamp(i64::from(i32::MIN), i64::from(i32::MAX))).unwrap_or(0)
}

fn to_u32(value: i64) -> u32 {
    u32::try_from(value.clamp(0, i64::from(u32::MAX))).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn monitor(
        name: &str,
        x: i32,
        y: i32,
        width: u32,
        height: u32,
        scale: f64,
        primary: bool,
    ) -> MonitorInfo {
        MonitorInfo {
            name: name.to_owned(),
            bounds: PixelRect {
                x,
                y,
                width,
                height,
            },
            scale_factor: scale,
            is_primary: primary,
        }
    }

    fn primary_1080p() -> MonitorInfo {
        monitor("DISPLAY1", 0, 0, 1920, 1080, 1.0, true)
    }

    fn second_1080p() -> MonitorInfo {
        monitor("DISPLAY2", 1920, 0, 1920, 1080, 1.0, false)
    }

    fn saved_on(name: &str, x: i32, y: i32, width: u32, height: u32) -> SavedWindow {
        SavedWindow {
            x,
            y,
            width,
            height,
            monitor_name: name.to_owned(),
            maximized: false,
            fullscreen: false,
        }
    }

    fn window_rect(placement: &Placement) -> PixelRect {
        match placement {
            Placement::Window { rect, .. } => *rect,
            Placement::Unplaced { .. } => panic!("expected a placed window in this test"),
        }
    }

    #[test]
    fn exactly_100_px_of_overlap_restores_on_the_saved_monitor() {
        // Window starts at x = 1820 and is 800 wide: 100 px on the primary monitor.
        let saved = saved_on("DISPLAY1", 1820, 100, 800, 500);
        let placement = plan_placement(Some(&saved), &[primary_1080p(), second_1080p()]);
        assert_eq!(window_rect(&placement).x, 1820);
        assert!(matches!(
            placement,
            Placement::Window { ref monitor_name, .. } if monitor_name == "DISPLAY1"
        ));
        assert_eq!(window_rect(&placement).width, 800);
    }

    #[test]
    fn ninety_nine_px_of_overlap_falls_back_to_the_primary_monitor() {
        // Window is 800 wide starting at x = 1821 on a 1920-wide monitor: overlap is 99 px.
        let edge = saved_on("DISPLAY1", 1821, 100, 800, 500);
        let placement = plan_placement(Some(&edge), &[primary_1080p()]);
        // Fallback: centered on the primary, size kept (800 is under the 90% cap).
        let rect = window_rect(&placement);
        assert_eq!(rect.width, 800);
        assert_eq!(rect.x, (1920 - 800) / 2);
    }

    #[test]
    fn overlap_must_hold_on_both_axes() {
        // Enough horizontal overlap (400 px) but only 99 px vertical overlap.
        let saved = saved_on("DISPLAY1", 100, 981, 800, 500);
        let placement = plan_placement(Some(&saved), &[primary_1080p()]);
        let rect = window_rect(&placement);
        // Fallback: the window is recentered vertically, not left at y = 981.
        assert_eq!(rect.y, (1080 - 500) / 2);
        assert_eq!(rect.width, 800);
    }

    #[test]
    fn removed_monitor_puts_the_window_on_the_primary_centered_and_capped_to_90_percent() {
        let saved = saved_on("DISPLAY2", 2000, 100, 3000, 2000);
        let placement = plan_placement(Some(&saved), &[primary_1080p()]);
        let rect = window_rect(&placement);
        // 3000 x 2000 logical at scale 1.0 is capped to 90% of 1920 x 1080.
        assert_eq!(rect.width, 1728);
        assert_eq!(rect.height, 972);
        assert_eq!(rect.x, (1920 - 1728) / 2);
        assert_eq!(rect.y, (1080 - 972) / 2);
    }

    #[test]
    fn window_far_off_screen_on_the_named_monitor_falls_back() {
        let saved = saved_on("DISPLAY1", 9000, 9000, 800, 600);
        let placement = plan_placement(Some(&saved), &[primary_1080p()]);
        assert!(matches!(
            placement,
            Placement::Window { ref monitor_name, .. } if monitor_name == "DISPLAY1"
        ));
        assert_eq!(window_rect(&placement).x, (1920 - 800) / 2);
    }

    #[test]
    fn minimum_size_is_raised_to_640_by_360() {
        let saved = saved_on("DISPLAY1", 100, 100, 100, 100);
        let rect = window_rect(&plan_placement(Some(&saved), &[primary_1080p()]));
        assert_eq!(rect.width, 640);
        assert_eq!(rect.height, 360);
    }

    #[test]
    fn minimum_size_is_applied_in_logical_pixels_before_scaling() {
        let hi_dpi = monitor("HIDPI", 0, 0, 3840, 2160, 2.0, true);
        let saved = saved_on("HIDPI", 200, 200, 50, 50);
        let rect = window_rect(&plan_placement(Some(&saved), &[hi_dpi]));
        assert_eq!(rect.width, 1280);
        assert_eq!(rect.height, 720);
    }

    #[test]
    fn logical_to_physical_at_common_scales() {
        assert_eq!(to_physical(800, 1.0), 800);
        assert_eq!(to_physical(800, 1.25), 1000);
        assert_eq!(to_physical(600, 1.25), 750);
        assert_eq!(to_physical(800, 2.0), 1600);
        assert_eq!(to_logical(1000, 1.25), 800);
        assert_eq!(to_logical(1600, 2.0), 800);
    }

    #[test]
    fn restored_position_and_size_use_the_target_monitor_scale() {
        let hi_dpi = monitor("HIDPI", 0, 0, 3840, 2160, 1.25, true);
        let saved = saved_on("HIDPI", 400, 200, 800, 600);
        let rect = window_rect(&plan_placement(Some(&saved), &[hi_dpi]));
        assert_eq!(rect.x, 500);
        assert_eq!(rect.y, 250);
        assert_eq!(rect.width, 1000);
        assert_eq!(rect.height, 750);
    }

    #[test]
    fn scale_change_between_sessions_keeps_the_logical_size() {
        let at_two = monitor("LAPTOP", 0, 0, 2880, 1800, 2.0, true);
        let captured = SavedWindow::capture(
            PixelRect {
                x: 200,
                y: 100,
                width: 1600,
                height: 1000,
            },
            &at_two,
            false,
            false,
        );
        assert_eq!(captured.width, 800);
        assert_eq!(captured.height, 500);

        let at_one = monitor("LAPTOP", 0, 0, 1920, 1080, 1.0, true);
        let rect = window_rect(&plan_placement(Some(&captured), &[at_one]));
        assert_eq!(rect.width, 800);
        assert_eq!(rect.height, 500);
    }

    #[test]
    fn invalid_scale_values_are_sanitized() {
        assert_eq!(sanitize_scale(f64::NAN), 1.0);
        assert_eq!(sanitize_scale(f64::INFINITY), 1.0);
        assert_eq!(sanitize_scale(0.0), SCALE_MIN);
        assert_eq!(sanitize_scale(100.0), SCALE_MAX);
    }

    #[test]
    fn fullscreen_is_restored_only_when_the_saved_monitor_is_used() {
        let mut saved = saved_on("DISPLAY2", 2000, 100, 800, 600);
        saved.fullscreen = true;
        let restored = plan_placement(Some(&saved), &[primary_1080p(), second_1080p()]);
        assert!(matches!(
            restored,
            Placement::Window {
                fullscreen: true,
                ..
            }
        ));

        let gone = plan_placement(Some(&saved), &[primary_1080p()]);
        assert!(matches!(
            gone,
            Placement::Window {
                fullscreen: false,
                ..
            }
        ));
    }

    #[test]
    fn fullscreen_restore_uses_the_current_monitor_size() {
        let mut saved = saved_on("DISPLAY1", 0, 0, 800, 600);
        saved.fullscreen = true;
        let bigger = monitor("DISPLAY1", 0, 0, 2560, 1440, 1.0, true);
        let placement = plan_placement(Some(&saved), std::slice::from_ref(&bigger));
        assert_eq!(window_rect(&placement), bigger.bounds);
        assert!(matches!(
            placement,
            Placement::Window {
                fullscreen: true,
                ..
            }
        ));
    }

    #[test]
    fn windowed_restore_is_capped_to_the_monitor_size() {
        let tiny = monitor("TINY", 0, 0, 1000, 700, 1.0, true);
        let saved = saved_on("TINY", 0, 0, 1800, 1200);
        let rect = window_rect(&plan_placement(Some(&saved), &[tiny]));
        assert_eq!(rect.width, 1000);
        assert_eq!(rect.height, 700);
    }

    #[test]
    fn maximized_is_kept_on_fallback() {
        let mut saved = saved_on("GONE", 0, 0, 800, 600);
        saved.maximized = true;
        let placement = plan_placement(Some(&saved), &[primary_1080p()]);
        assert!(matches!(
            placement,
            Placement::Window {
                maximized: true,
                fullscreen: false,
                ..
            }
        ));
    }

    #[test]
    fn empty_monitor_name_skips_name_match_and_uses_the_primary() {
        let saved = saved_on("", 1000, 100, 800, 600);
        let placement = plan_placement(Some(&saved), &[second_1080p(), primary_1080p()]);
        assert!(matches!(
            placement,
            Placement::Window { ref monitor_name, .. } if monitor_name == "DISPLAY1"
        ));
    }

    #[test]
    fn no_saved_state_centers_the_default_size_on_the_primary() {
        let placement = plan_placement(None, &[second_1080p(), primary_1080p()]);
        let rect = window_rect(&placement);
        assert_eq!(rect.width, 1280);
        assert_eq!(rect.height, 720);
        assert_eq!(rect.x, (1920 - 1280) / 2);
        assert_eq!(rect.y, (1080 - 720) / 2);
    }

    #[test]
    fn default_size_is_capped_to_90_percent_on_a_small_primary() {
        let small = monitor("SMALL", 0, 0, 1024, 768, 1.0, true);
        let rect = window_rect(&plan_placement(None, &[small]));
        assert_eq!(rect.width, 1024 * 90 / 100);
        assert_eq!(rect.height, 768 * 90 / 100);
    }

    #[test]
    fn negative_coordinates_on_a_monitor_left_of_the_primary_are_restored() {
        let left = monitor("LEFT", -1920, 0, 1920, 1080, 1.0, false);
        let saved = saved_on("LEFT", -1500, 100, 800, 600);
        let rect = window_rect(&plan_placement(Some(&saved), &[primary_1080p(), left]));
        assert_eq!(rect.x, -1500);
        assert_eq!(rect.y, 100);
    }

    #[test]
    fn no_monitors_reports_unplaced_with_the_logical_size() {
        let saved = saved_on("DISPLAY1", 0, 0, 100, 100);
        assert_eq!(
            plan_placement(Some(&saved), &[]),
            Placement::Unplaced {
                logical_width: 640,
                logical_height: 360,
                maximized: false,
            }
        );
    }

    #[test]
    fn primary_falls_back_to_the_first_monitor_when_none_is_marked() {
        let first = monitor("A", 0, 0, 1280, 720, 1.0, false);
        let second = monitor("B", 1280, 0, 1280, 720, 1.0, false);
        let placement = plan_placement(None, &[first, second]);
        assert!(matches!(
            placement,
            Placement::Window { ref monitor_name, .. } if monitor_name == "A"
        ));
    }

    #[test]
    fn record_with_missing_or_non_positive_values_gives_defaults() {
        assert_eq!(SavedWindow::from_record(&WindowRecord::default()), None);
        let zero_width = WindowRecord {
            x: Some(0),
            y: Some(0),
            width: Some(0),
            height: Some(600),
            ..Default::default()
        };
        assert_eq!(SavedWindow::from_record(&zero_width), None);
        let negative_height = WindowRecord {
            height: Some(-5),
            ..zero_width.clone()
        };
        assert_eq!(SavedWindow::from_record(&negative_height), None);
        let missing_y = WindowRecord {
            x: Some(0),
            y: None,
            width: Some(800),
            height: Some(600),
            ..Default::default()
        };
        assert_eq!(SavedWindow::from_record(&missing_y), None);
    }

    #[test]
    fn record_optional_flags_default_to_false() {
        let record = WindowRecord {
            x: Some(10),
            y: Some(20),
            width: Some(800),
            height: Some(600),
            ..Default::default()
        };
        let saved = SavedWindow::from_record(&record).expect("valid record");
        assert_eq!(saved.monitor_name, "");
        assert!(!saved.maximized);
        assert!(!saved.fullscreen);
    }

    #[test]
    fn window_section_round_trips() {
        let saved = SavedWindow {
            x: -1500,
            y: 40,
            width: 1024,
            height: 768,
            monitor_name: "LEFT".to_owned(),
            maximized: true,
            fullscreen: false,
        };
        let record = saved.to_record();
        assert_eq!(SavedWindow::from_record(&record), Some(saved));
    }

    #[test]
    fn overlap_helper_matches_the_rule() {
        assert_eq!(overlap_len(0, 100, 50, 100), 50);
        assert_eq!(overlap_len(0, 100, 100, 100), 0);
        assert_eq!(overlap_len(0, 100, 200, 100), 0);
    }
}
