//! Idle screensaver: a full-screen overlay shown after the configured number of seconds without
//! input. The state machine is pure so the idle/dismissal rules are unit-tested without a display.

use std::time::Duration;

use egui::{Align2, Color32, FontId, Rect, Vec2, load::SizedTexture};
use kattri_i18n::ids;

use super::row::cover_uv;
use super::ui_text;

/// Longest delay the settings schema accepts. Zero turns the screensaver off.
pub(crate) const MAX_IDLE_SECONDS: u32 = 3600;
/// Cadence of the slow pan/zoom while the overlay is visible.
const ANIMATION_INTERVAL: Duration = Duration::from_millis(50);

/// Why the screensaver is or is not due this frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum IdleDecision {
    /// The setting is zero.
    Off,
    /// Configured, but either a media item is playing or the idle delay has not elapsed.
    NotDue,
    Due,
}

/// Pure idle decision: off wins over everything, and a playing item never gets covered.
pub(crate) fn idle_decision(
    configured_seconds: u32,
    idle_for: Duration,
    media_playing: bool,
) -> IdleDecision {
    if configured_seconds == 0 {
        return IdleDecision::Off;
    }
    if media_playing || idle_for < Duration::from_secs(u64::from(configured_seconds)) {
        return IdleDecision::NotDue;
    }
    IdleDecision::Due
}

/// True for every egui event that counts as the viewer doing something.
pub(crate) fn event_resets_idle(event: &egui::Event) -> bool {
    matches!(
        event,
        egui::Event::Key { .. }
            | egui::Event::PointerMoved(_)
            | egui::Event::PointerButton { .. }
            | egui::Event::MouseWheel { .. }
            | egui::Event::Zoom(_)
            | egui::Event::WindowFocused(_)
            | egui::Event::Touch { .. }
    )
}

/// True when this frame carried keyboard, pointer or focus activity.
pub(crate) fn input_seen(ctx: &egui::Context) -> bool {
    ctx.input(|input| input.pointer.any_click() || input.events.iter().any(event_resets_idle))
}

/// The idle clock and whether the overlay is visible.
#[derive(Clone, Debug, Default)]
pub(crate) struct Screensaver {
    last_input: Option<Duration>,
    active: bool,
    activated_at: Duration,
}

impl Screensaver {
    /// Feed one frame and return whether the overlay should be painted. Input always dismisses
    /// and restarts the timer, and media playback suppresses the overlay without moving it.
    pub(crate) fn update(
        &mut self,
        now: Duration,
        input_seen: bool,
        configured_seconds: u32,
        media_playing: bool,
    ) -> bool {
        // The first frame has no earlier activity to measure from, so it becomes the baseline.
        let last = *self.last_input.get_or_insert(now);
        if input_seen {
            self.last_input = Some(now);
            self.active = false;
            return false;
        }
        let idle_for = now.saturating_sub(last);
        match idle_decision(configured_seconds, idle_for, media_playing) {
            IdleDecision::Due => {
                if !self.active {
                    self.active = true;
                    self.activated_at = now;
                }
                true
            }
            IdleDecision::Off | IdleDecision::NotDue => {
                self.active = false;
                false
            }
        }
    }

    /// Dismiss on gamepad activity, which is collected outside the egui input stream.
    pub(crate) fn reset(&mut self, now: Duration) {
        self.last_input = Some(now);
        self.active = false;
    }

    pub(crate) fn active(&self) -> bool {
        self.active
    }

    /// Seconds since the overlay appeared, used for the pan/zoom.
    pub(crate) fn animation_seconds(&self, now: Duration) -> f32 {
        now.saturating_sub(self.activated_at).as_secs_f32()
    }

    /// How long until the overlay is due, or `None` when it is off, already visible or
    /// suppressed by playback. The app uses this to schedule the frame that shows it.
    pub(crate) fn time_until_due(
        &self,
        now: Duration,
        configured_seconds: u32,
        media_playing: bool,
    ) -> Option<Duration> {
        if configured_seconds == 0 || media_playing || self.active {
            return None;
        }
        let last = self.last_input?;
        let delay = Duration::from_secs(u64::from(configured_seconds));
        Some(delay.saturating_sub(now.saturating_sub(last)))
    }

    /// Repaint cadence while the overlay animates. Reduced motion only needs one frame.
    pub(crate) fn repaint_interval(&self, reduced_motion: bool) -> Option<Duration> {
        (self.active && !reduced_motion).then_some(ANIMATION_INTERVAL)
    }
}

/// Paint the overlay on the topmost layer. It draws only; it never allocates an interactive
/// widget, so input continues to reach the screen underneath and dismissal is never trapped.
pub(crate) fn paint(
    ctx: &egui::Context,
    texture: Option<SizedTexture>,
    elapsed_seconds: f32,
    reduced_motion: bool,
) {
    let rect = ctx.content_rect();
    if rect.width() <= 0.0 || rect.height() <= 0.0 {
        return;
    }
    let painter = ctx.layer_painter(egui::LayerId::new(
        egui::Order::Foreground,
        egui::Id::unique("kattri_screensaver"),
    ));
    painter.rect_filled(rect, 0.0, Color32::from_black_alpha(215));
    if let Some(texture) = texture {
        let (target, uv) = pan_zoom(texture.size, rect, elapsed_seconds, reduced_motion);
        painter.image(texture.id, target, uv, Color32::from_white_alpha(120));
        // Darken the artwork again so the wordmark keeps its contrast.
        painter.rect_filled(rect, 0.0, Color32::from_black_alpha(120));
    }
    let center = rect.center();
    painter.text(
        center - Vec2::new(0.0, 16.0),
        Align2::CENTER_CENTER,
        ui_text(ids::SCREENSAVER_WORDMARK),
        FontId::proportional(48.0),
        Color32::from_white_alpha(240),
    );
    painter.text(
        center + Vec2::new(0.0, 44.0),
        Align2::CENTER_CENTER,
        ui_text(ids::SCREENSAVER_HINT),
        FontId::proportional(18.0),
        Color32::from_white_alpha(175),
    );
}

/// Cover the screen with the texture while drifting and zooming a little. Reduced motion keeps
/// the texture still and unscaled.
fn pan_zoom(texture: Vec2, rect: Rect, elapsed_seconds: f32, reduced_motion: bool) -> (Rect, Rect) {
    let base = cover_uv(texture, rect.size());
    if reduced_motion || texture.x <= 0.0 || texture.y <= 0.0 {
        return (rect, base);
    }
    // The zoom reaches its limit after 24 seconds and then holds. The drift stays inside the
    // margin the zoom creates, so the texture never shows an edge.
    let progress = (elapsed_seconds.max(0.0) / 24.0).clamp(0.0, 1.0);
    let zoom = 1.0 + 0.10 * progress;
    let half = base.size() * (0.5 / zoom);
    let margin = base.size() * (0.5 * (1.0 - 1.0 / zoom));
    let drift = Vec2::new(
        (elapsed_seconds.max(0.0) * 0.20).sin() * margin.x,
        (elapsed_seconds.max(0.0) * 0.14).cos() * margin.y,
    );
    let uv = Rect::from_center_size(base.center() + drift, half * 2.0);
    (rect, uv)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIGURED: u32 = 10;

    fn settled() -> Screensaver {
        let mut screensaver = Screensaver::default();
        assert!(!screensaver.update(Duration::ZERO, false, CONFIGURED, false));
        screensaver
    }

    #[test]
    fn decision_covers_off_not_yet_due_and_due() {
        assert_eq!(
            idle_decision(0, Duration::from_secs(3_600), false),
            IdleDecision::Off
        );
        assert_eq!(
            idle_decision(CONFIGURED, Duration::from_secs(9), false),
            IdleDecision::NotDue
        );
        assert_eq!(
            idle_decision(CONFIGURED, Duration::from_secs(10), false),
            IdleDecision::Due
        );
        assert_eq!(
            idle_decision(CONFIGURED, Duration::from_secs(11), false),
            IdleDecision::Due
        );
    }

    #[test]
    fn off_never_becomes_visible() {
        let mut screensaver = settled();
        assert!(!screensaver.update(Duration::from_secs(3_600), false, 0, false));
        assert!(!screensaver.active());
    }

    #[test]
    fn due_after_the_configured_delay_and_input_resets_the_timer() {
        let mut screensaver = settled();
        assert!(!screensaver.update(Duration::from_secs(9), false, CONFIGURED, false));
        assert!(screensaver.update(Duration::from_secs(10), false, CONFIGURED, false));
        assert!(screensaver.active());

        // Input at 12 seconds restarts the clock. The old deadline no longer applies.
        assert!(!screensaver.update(Duration::from_secs(12), true, CONFIGURED, false));
        assert!(!screensaver.update(Duration::from_secs(21), false, CONFIGURED, false));
        assert!(!screensaver.active());
        assert!(screensaver.update(Duration::from_secs(22), false, CONFIGURED, false));
    }

    #[test]
    fn any_input_dismisses_and_never_leaves_the_overlay_stuck() {
        let mut screensaver = settled();
        assert!(screensaver.update(Duration::from_secs(10), false, CONFIGURED, false));
        assert!(screensaver.update(Duration::from_secs(30), false, CONFIGURED, false));
        assert!(!screensaver.update(Duration::from_secs(30), true, CONFIGURED, false));
        assert!(!screensaver.active());
        // The very next painted frame is normally visible again; no input is required to leave.
        assert!(!screensaver.update(Duration::from_secs(39), false, CONFIGURED, false));
    }

    #[test]
    fn media_playback_suppresses_the_overlay() {
        let mut screensaver = settled();
        assert!(!screensaver.update(Duration::from_secs(60), false, CONFIGURED, true));
        assert!(!screensaver.active());
        // When playback ends, a still-idle viewer sees the overlay again.
        assert!(screensaver.update(Duration::from_secs(60), false, CONFIGURED, false));
    }

    #[test]
    fn events_that_reset_the_idle_clock() {
        use egui::{Event, Key, Modifiers, pos2};
        assert!(event_resets_idle(&Event::PointerMoved(pos2(1.0, 2.0))));
        assert!(event_resets_idle(&Event::PointerButton {
            pos: pos2(1.0, 2.0),
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: Modifiers::NONE,
        }));
        assert!(event_resets_idle(&Event::Key {
            key: Key::A,
            physical_key: None,
            pressed: false,
            repeat: false,
            modifiers: Modifiers::NONE,
        }));
        assert!(event_resets_idle(&Event::MouseWheel {
            unit: egui::MouseWheelUnit::Line,
            delta: Vec2::new(0.0, 1.0),
            phase: egui::TouchPhase::Move,
            source: egui::MouseWheelSource::Unknown,
            modifiers: Modifiers::NONE,
        }));
        assert!(event_resets_idle(&Event::WindowFocused(true)));
        assert!(!event_resets_idle(&Event::PointerGone));
    }

    #[test]
    fn repaint_interval_is_skipped_for_reduced_motion() {
        let mut screensaver = settled();
        assert_eq!(screensaver.repaint_interval(false), None);
        assert!(screensaver.update(Duration::from_secs(10), false, CONFIGURED, false));
        assert_eq!(
            screensaver.repaint_interval(false),
            Some(ANIMATION_INTERVAL)
        );
        assert_eq!(screensaver.repaint_interval(true), None);
    }
}
