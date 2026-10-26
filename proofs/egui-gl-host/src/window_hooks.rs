//! Native window glue for picture-in-picture (M2-12) and window memory (M2-14). Reads the
//! normal-state window from winit, applies the transitions the pure model in
//! `kattri_client_core::pip` returns, and builds the monitor list the model needs. Decisions stay
//! in the pure models; this file only calls winit.

use std::time::{Duration, Instant};

use kattri_client_core::pip::PipTransition;
use kattri_client_core::window_state::{
    MonitorInfo, PixelRect, Placement, SavedWindow, WindowRecord, plan_placement,
};
use kattri_settings::WindowSettings;
use winit::dpi::{LogicalSize, PhysicalPosition, PhysicalSize};
use winit::event_loop::ActiveEventLoop;
use winit::monitor::MonitorHandle;
use winit::window::{Fullscreen, Window, WindowLevel};

/// Quiet time after the last move or resize before the geometry is written (M2-14).
pub(crate) const SAVE_DEBOUNCE: Duration = Duration::from_millis(500);

/// Trailing debounce for geometry saves (M2-14). Each move or resize pushes the deadline out, so a
/// drag writes once after it settles. The caller passes the time in, so tests use simulated
/// instants and nothing sleeps.
#[derive(Debug)]
pub(crate) struct SaveDebounce {
    delay: Duration,
    deadline: Option<Instant>,
}

impl SaveDebounce {
    pub(crate) fn new(delay: Duration) -> Self {
        Self {
            delay,
            deadline: None,
        }
    }

    pub(crate) fn touch(&mut self, now: Instant) {
        // On overflow the save is due at once, rather than never.
        self.deadline = Some(now.checked_add(self.delay).unwrap_or(now));
    }

    /// When the pending save is due, so the event loop can wake for it. `None` when nothing waits.
    pub(crate) fn deadline(&self) -> Option<Instant> {
        self.deadline
    }

    /// True once the pending save is due. Taking it clears the pending save.
    pub(crate) fn take_due(&mut self, now: Instant) -> bool {
        match self.deadline {
            Some(deadline) if now >= deadline => {
                self.deadline = None;
                true
            }
            _ => false,
        }
    }
}

/// The remembered window from the settings `window` section, or `None` when no valid geometry is
/// saved (first run, or a damaged value). The caller then keeps the default placement.
pub(crate) fn saved_from_settings(window: &WindowSettings) -> Option<SavedWindow> {
    let geometry = window.geometry.as_ref()?;
    SavedWindow::from_record(&WindowRecord {
        x: Some(geometry.x),
        y: Some(geometry.y),
        width: i32::try_from(geometry.width).ok(),
        height: i32::try_from(geometry.height).ok(),
        monitor_name: geometry.monitor.clone(),
        maximized: None,
        fullscreen: Some(window.fullscreen),
    })
}

/// Only a normal-size window is remembered. A maximized window reports the screen size, and the
/// settings schema has no maximized key yet, so the last normal size is kept instead.
pub(crate) fn is_storable(saved: &SavedWindow) -> bool {
    !saved.maximized
}

/// Place the window from the saved geometry before it is shown (M2-14). Without a saved value the
/// window keeps its built-in size and the platform's position, as it did before this feature.
pub(crate) fn restore_at_startup(
    window: &Window,
    event_loop: &ActiveEventLoop,
    saved: Option<&SavedWindow>,
) {
    let Some(saved) = saved else {
        return;
    };
    let monitors = monitor_infos(event_loop);
    apply_placement(window, &plan_placement(Some(saved), &monitors));
}

/// Monitors in the form the window model reads. The primary is the one winit reports as primary.
pub(crate) fn monitor_infos(event_loop: &ActiveEventLoop) -> Vec<MonitorInfo> {
    let primary = event_loop.primary_monitor();
    event_loop
        .available_monitors()
        .map(|handle| {
            let is_primary = primary.as_ref() == Some(&handle);
            monitor_info(&handle, is_primary)
        })
        .collect()
}

fn monitor_info(handle: &MonitorHandle, is_primary: bool) -> MonitorInfo {
    let position = handle.position();
    let size = handle.size();
    MonitorInfo {
        name: handle.name().unwrap_or_default(),
        bounds: PixelRect {
            x: position.x,
            y: position.y,
            width: size.width,
            height: size.height,
        },
        scale_factor: handle.scale_factor(),
        is_primary,
    }
}

/// The window as it is now, in logical pixels of its monitor. `None` when winit cannot report the
/// position or the monitor, so the caller keeps the current state and says so.
pub(crate) fn capture_window(window: &Window, event_loop: &ActiveEventLoop) -> Option<SavedWindow> {
    let position = window.outer_position().ok()?;
    let size = window.inner_size();
    let handle = window.current_monitor()?;
    let is_primary = event_loop.primary_monitor().as_ref() == Some(&handle);
    let monitor = monitor_info(&handle, is_primary);
    let physical = PixelRect {
        x: position.x,
        y: position.y,
        width: size.width,
        height: size.height,
    };
    Some(SavedWindow::capture(
        physical,
        &monitor,
        window.is_maximized(),
        window.fullscreen().is_some(),
    ))
}

/// Apply one transition from the PiP model to the window. Enter sets always-on-top and the PiP
/// size. Leave clears always-on-top and puts the window back where the model placed it.
pub(crate) fn apply_transition(window: &Window, transition: &PipTransition) {
    match transition {
        PipTransition::Enter {
            size,
            leave_fullscreen,
        } => {
            if *leave_fullscreen {
                window.set_fullscreen(None);
            }
            window.set_maximized(false);
            window.set_window_level(WindowLevel::AlwaysOnTop);
            // The resize is a request. The platform may apply it on a later event, so the
            // returned size is not used here.
            let _ = window.request_inner_size(LogicalSize::new(
                f64::from(size.width()),
                f64::from(size.height()),
            ));
        }
        PipTransition::Leave { placement } => {
            window.set_window_level(WindowLevel::Normal);
            apply_placement(window, placement);
        }
    }
}

fn apply_placement(window: &Window, placement: &Placement) {
    match placement {
        Placement::Window {
            rect,
            maximized,
            fullscreen,
            ..
        } => {
            if *fullscreen {
                window.set_fullscreen(Some(Fullscreen::Borderless(None)));
            } else {
                window.set_fullscreen(None);
                window.set_outer_position(PhysicalPosition::new(rect.x, rect.y));
                let _ = window.request_inner_size(PhysicalSize::new(rect.width, rect.height));
            }
            window.set_maximized(*maximized);
        }
        Placement::Unplaced {
            logical_width,
            logical_height,
            maximized,
        } => {
            window.set_fullscreen(None);
            let _ = window.request_inner_size(LogicalSize::new(
                f64::from(*logical_width),
                f64::from(*logical_height),
            ));
            window.set_maximized(*maximized);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kattri_settings::WindowGeometry;

    fn ms(value: u64) -> Duration {
        Duration::from_millis(value)
    }

    #[test]
    fn nothing_is_due_without_a_change() {
        let t0 = Instant::now();
        let mut debounce = SaveDebounce::new(SAVE_DEBOUNCE);
        assert_eq!(debounce.deadline(), None);
        assert!(!debounce.take_due(t0 + ms(10_000)));
    }

    #[test]
    fn a_change_is_due_at_500_ms_and_only_once() {
        let t0 = Instant::now();
        let mut debounce = SaveDebounce::new(SAVE_DEBOUNCE);
        debounce.touch(t0);
        assert_eq!(debounce.deadline(), Some(t0 + ms(500)));
        assert!(!debounce.take_due(t0 + ms(499)));
        assert!(debounce.take_due(t0 + ms(500)));
        assert!(!debounce.take_due(t0 + ms(2_000)));
        assert_eq!(debounce.deadline(), None);
    }

    #[test]
    fn each_move_or_resize_restarts_the_quiet_period() {
        let t0 = Instant::now();
        let mut debounce = SaveDebounce::new(SAVE_DEBOUNCE);
        debounce.touch(t0);
        debounce.touch(t0 + ms(300));
        assert!(
            !debounce.take_due(t0 + ms(500)),
            "the first event's deadline was replaced by the second"
        );
        assert!(!debounce.take_due(t0 + ms(799)));
        assert!(debounce.take_due(t0 + ms(800)));
    }

    #[test]
    fn a_change_after_a_save_starts_a_new_quiet_period() {
        let t0 = Instant::now();
        let mut debounce = SaveDebounce::new(SAVE_DEBOUNCE);
        debounce.touch(t0);
        assert!(debounce.take_due(t0 + ms(500)));
        debounce.touch(t0 + ms(1_000));
        assert!(!debounce.take_due(t0 + ms(1_499)));
        assert!(debounce.take_due(t0 + ms(1_500)));
    }

    #[test]
    fn saved_geometry_is_read_from_the_settings_section() {
        let window = WindowSettings {
            fullscreen: true,
            geometry: Some(WindowGeometry {
                x: -1500,
                y: 40,
                width: 1024,
                height: 768,
                monitor: Some("LEFT".to_owned()),
            }),
            ..WindowSettings::default()
        };
        assert_eq!(
            saved_from_settings(&window),
            Some(SavedWindow {
                x: -1500,
                y: 40,
                width: 1024,
                height: 768,
                monitor_name: "LEFT".to_owned(),
                maximized: false,
                fullscreen: true,
            })
        );
    }

    #[test]
    fn missing_or_damaged_geometry_gives_no_saved_window() {
        assert_eq!(saved_from_settings(&WindowSettings::default()), None);
        let zero_width = WindowSettings {
            geometry: Some(WindowGeometry {
                x: 0,
                y: 0,
                width: 0,
                height: 600,
                monitor: None,
            }),
            ..WindowSettings::default()
        };
        assert_eq!(saved_from_settings(&zero_width), None);
    }

    #[test]
    fn a_missing_monitor_name_is_stored_as_empty() {
        let window = WindowSettings {
            geometry: Some(WindowGeometry {
                x: 100,
                y: 100,
                width: 800,
                height: 600,
                monitor: None,
            }),
            ..WindowSettings::default()
        };
        let saved = saved_from_settings(&window).expect("valid geometry");
        assert_eq!(saved.monitor_name, "");
    }

    #[test]
    fn a_maximized_window_does_not_replace_the_normal_size() {
        let mut saved = SavedWindow {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
            monitor_name: "DISPLAY1".to_owned(),
            maximized: true,
            fullscreen: false,
        };
        assert!(!is_storable(&saved));
        saved.maximized = false;
        assert!(is_storable(&saved));
    }

    #[test]
    fn captured_physical_pixels_become_the_logical_size_used_for_the_save() {
        let monitor = MonitorInfo {
            name: "LAPTOP".to_owned(),
            bounds: PixelRect {
                x: 0,
                y: 0,
                width: 2880,
                height: 1800,
            },
            scale_factor: 2.0,
            is_primary: true,
        };
        let captured = SavedWindow::capture(
            PixelRect {
                x: 200,
                y: 100,
                width: 1600,
                height: 1000,
            },
            &monitor,
            false,
            false,
        );
        let saved = saved_from_settings(&WindowSettings {
            geometry: Some(WindowGeometry {
                x: captured.x,
                y: captured.y,
                width: captured.width,
                height: captured.height,
                monitor: Some(captured.monitor_name.clone()),
            }),
            ..WindowSettings::default()
        });
        assert_eq!(saved.map(|s| (s.width, s.height)), Some((800, 500)));
    }
}
