//! Pure animation and layout math for the home-screen rows and banners.
//!
//! Nothing in this module touches egui types, so every function is unit-tested directly.

use std::ops::Range;

/// Edge padding (in points) kept between the viewport and the first/last card.
pub const EDGE_PADDING: f32 = 24.0;

/// Maximum extra scale applied to a focused card (1.0 + this at full focus).
pub const FOCUS_SCALE_MAX: f32 = 0.08;

/// Cubic ease-out: fast start, gentle landing. Input is clamped to 0..=1.
pub fn ease_out_cubic(t: f32) -> f32 {
    let t = clamp01(t);
    1.0 - (1.0 - t).powi(3)
}

/// Scale factor for a focused card. `t` is the focus animation progress (0 = unfocused,
/// 1 = fully focused). Result is always within `1.0..=1.0 + FOCUS_SCALE_MAX` and never
/// decreases as `t` increases.
pub fn focus_scale(t: f32) -> f32 {
    1.0 + FOCUS_SCALE_MAX * ease_out_cubic(t)
}

/// Frame-rate independent exponential smoothing of `current` toward `target`.
///
/// After `half_life` seconds the remaining distance to `target` is halved. The blend factor
/// is always in `0..=1`, so the result never overshoots the target. A non-positive
/// `half_life` snaps straight to `target`; a non-positive or non-finite `dt` leaves
/// `current` unchanged.
pub fn smooth_toward(current: f32, target: f32, dt: f32, half_life: f32) -> f32 {
    if !dt.is_finite() || dt <= 0.0 {
        return current;
    }
    if half_life <= 0.0 || !half_life.is_finite() {
        return target;
    }
    let factor = 1.0 - 0.5_f32.powf(dt / half_life);
    current + (target - current) * factor
}

/// Width of one card slot including the gap that follows it.
fn step(card_w: f32, gap: f32) -> f32 {
    card_w + gap
}

/// Total scrollable content width for `count` cards, including edge padding on both ends.
pub fn content_width(count: usize, card_w: f32, gap: f32) -> f32 {
    if count == 0 {
        return 2.0 * EDGE_PADDING;
    }
    let count = count as f32;
    2.0 * EDGE_PADDING + count * card_w + (count - 1.0) * gap
}

/// Largest valid scroll offset for the given row geometry (0 when everything fits).
pub fn max_scroll(count: usize, card_w: f32, gap: f32, viewport_w: f32) -> f32 {
    (content_width(count, card_w, gap) - viewport_w).max(0.0)
}

/// Scroll offset that makes card `focus_index` fully visible with `EDGE_PADDING` on each
/// side, moving as little as possible from `current`.
///
/// If the card is already fully visible (with padding) the current offset is returned.
/// When the viewport is narrower than the padded card, the card's left edge is aligned.
/// The result is never negative.
pub fn scroll_offset_to_reveal(
    focus_index: usize,
    card_w: f32,
    gap: f32,
    viewport_w: f32,
    current: f32,
) -> f32 {
    let current = current.max(0.0);
    if !(card_w > 0.0 && viewport_w > 0.0) {
        return current;
    }
    let left = EDGE_PADDING + focus_index as f32 * step(card_w, gap);
    let right = left + card_w;

    let mut offset = current;
    // Right edge first so that, if the padded card cannot fit, the left edge wins.
    if right > offset + viewport_w - EDGE_PADDING {
        offset = right - viewport_w + EDGE_PADDING;
    }
    if left < offset + EDGE_PADDING {
        offset = left - EDGE_PADDING;
    }
    offset.max(0.0)
}

/// Indices of cards whose slot intersects the viewport (plus `EDGE_PADDING` slack so
/// scaled cards at the edges are still drawn). Returns an empty range for zero cards.
pub fn visible_range(
    count: usize,
    card_w: f32,
    gap: f32,
    viewport_w: f32,
    offset: f32,
) -> Range<usize> {
    if count == 0 || card_w <= 0.0 {
        return 0..0;
    }
    let slot = step(card_w, gap).max(f32::EPSILON);
    let offset = offset.max(0.0);
    let first = ((offset - EDGE_PADDING) / slot).floor().max(0.0);
    let end = ((offset + viewport_w + EDGE_PADDING) / slot)
        .ceil()
        .max(0.0);
    let start = (first as usize).min(count);
    let end = (end as usize).min(count);
    start..end.max(start)
}

/// Fade-in alpha for `elapsed` seconds into a `duration`-second fade (eased, 0..=1).
/// A non-positive duration is fully opaque immediately.
pub fn fade_alpha(elapsed: f64, duration: f32) -> f32 {
    if duration <= 0.0 {
        return 1.0;
    }
    ease_out_cubic((elapsed / f64::from(duration)).clamp(0.0, 1.0) as f32)
}

fn clamp01(t: f32) -> f32 {
    if t.is_nan() { 0.0 } else { t.clamp(0.0, 1.0) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ease_out_cubic_hits_endpoints_and_clamps() {
        assert_eq!(ease_out_cubic(0.0), 0.0);
        assert_eq!(ease_out_cubic(1.0), 1.0);
        assert_eq!(ease_out_cubic(-3.0), 0.0);
        assert_eq!(ease_out_cubic(7.0), 1.0);
        assert_eq!(ease_out_cubic(f32::NAN), 0.0);
    }

    #[test]
    fn ease_out_cubic_is_monotonic_and_front_loaded() {
        let mut prev = -1.0;
        for i in 0..=100 {
            let v = ease_out_cubic(i as f32 / 100.0);
            assert!(v >= prev, "not monotonic at {i}");
            prev = v;
        }
        assert!(ease_out_cubic(0.5) > 0.5);
    }

    #[test]
    fn focus_scale_is_bounded_and_monotonic() {
        assert_eq!(focus_scale(0.0), 1.0);
        assert!((focus_scale(1.0) - (1.0 + FOCUS_SCALE_MAX)).abs() < 1e-6);
        let mut prev = 0.0;
        for i in 0..=100 {
            let s = focus_scale(i as f32 / 100.0);
            assert!((1.0..=1.0 + FOCUS_SCALE_MAX + 1e-6).contains(&s));
            assert!(s >= prev);
            prev = s;
        }
        assert_eq!(focus_scale(-1.0), 1.0);
        assert!(focus_scale(5.0) <= 1.0 + FOCUS_SCALE_MAX + 1e-6);
    }

    #[test]
    fn smooth_toward_moves_monotonically_without_overshoot() {
        let target = 100.0;
        let mut value = 0.0;
        let mut prev = value;
        for _ in 0..600 {
            value = smooth_toward(value, target, 1.0 / 60.0, 0.1);
            assert!(value >= prev, "moved backwards");
            assert!(value <= target, "overshot: {value}");
            prev = value;
        }
    }

    #[test]
    fn smooth_toward_converges_and_halves_after_one_half_life() {
        let half = smooth_toward(0.0, 10.0, 0.25, 0.25);
        assert!((half - 5.0).abs() < 1e-4);
        let mut value = 0.0;
        for _ in 0..1000 {
            value = smooth_toward(value, 42.0, 1.0 / 60.0, 0.1);
        }
        assert!((value - 42.0).abs() < 1e-3);
    }

    #[test]
    fn smooth_toward_handles_degenerate_inputs() {
        assert_eq!(smooth_toward(3.0, 9.0, 0.0, 0.1), 3.0);
        assert_eq!(smooth_toward(3.0, 9.0, -1.0, 0.1), 3.0);
        assert_eq!(smooth_toward(3.0, 9.0, f32::NAN, 0.1), 3.0);
        assert_eq!(smooth_toward(3.0, 9.0, 0.016, 0.0), 9.0);
        assert_eq!(smooth_toward(3.0, 9.0, 0.016, -2.0), 9.0);
        assert_eq!(smooth_toward(9.0, 9.0, 0.5, 0.2), 9.0);
    }

    #[test]
    fn content_width_and_max_scroll() {
        assert_eq!(content_width(0, 200.0, 24.0), 2.0 * EDGE_PADDING);
        assert_eq!(content_width(1, 200.0, 24.0), 2.0 * EDGE_PADDING + 200.0);
        assert_eq!(
            content_width(3, 200.0, 24.0),
            2.0 * EDGE_PADDING + 3.0 * 200.0 + 2.0 * 24.0
        );
        assert_eq!(max_scroll(0, 200.0, 24.0, 800.0), 0.0);
        assert_eq!(max_scroll(2, 200.0, 24.0, 2000.0), 0.0);
        let max = max_scroll(10, 200.0, 24.0, 800.0);
        assert!((max - (content_width(10, 200.0, 24.0) - 800.0)).abs() < 1e-4);
    }

    #[test]
    fn reveal_keeps_offset_when_card_already_visible() {
        // Card 1 spans 248..448 in content space; viewport 0..800 easily contains it.
        assert_eq!(scroll_offset_to_reveal(1, 200.0, 24.0, 800.0, 0.0), 0.0);
        assert_eq!(scroll_offset_to_reveal(1, 200.0, 24.0, 800.0, 10.0), 10.0);
    }

    #[test]
    fn reveal_scrolls_minimally_to_the_right_with_padding() {
        // Card 5 right edge: 24 + 5*224 + 200 = 1344. Viewport 800 -> offset 1344-800+24 = 568.
        let offset = scroll_offset_to_reveal(5, 200.0, 24.0, 800.0, 0.0);
        assert!((offset - 568.0).abs() < 1e-4);
        // Its right edge sits exactly EDGE_PADDING inside the viewport.
        assert!((offset + 800.0 - (1344.0 + EDGE_PADDING)).abs() < 1e-4);
    }

    #[test]
    fn reveal_scrolls_minimally_to_the_left_with_padding() {
        // Moving back to card 0 from a far offset aligns its left edge at the padding.
        let offset = scroll_offset_to_reveal(0, 200.0, 24.0, 800.0, 500.0);
        assert_eq!(offset, 0.0);
        // Card 2 left edge = 24 + 2*224 = 472; minimal left scroll = 472 - 24 = 448.
        let offset = scroll_offset_to_reveal(2, 200.0, 24.0, 800.0, 900.0);
        assert!((offset - 448.0).abs() < 1e-4);
    }

    #[test]
    fn reveal_handles_viewport_smaller_than_card_by_aligning_left_edge() {
        let offset = scroll_offset_to_reveal(3, 200.0, 24.0, 100.0, 0.0);
        let left = EDGE_PADDING + 3.0 * 224.0;
        assert!((offset - (left - EDGE_PADDING)).abs() < 1e-4);
    }

    #[test]
    fn reveal_edge_cases_are_safe() {
        assert_eq!(scroll_offset_to_reveal(0, 0.0, 24.0, 800.0, 5.0), 5.0);
        assert_eq!(scroll_offset_to_reveal(0, 200.0, 24.0, 0.0, 5.0), 5.0);
        assert_eq!(scroll_offset_to_reveal(0, 200.0, 24.0, 800.0, -50.0), 0.0);
        // Index 0 never produces a negative offset.
        assert!(scroll_offset_to_reveal(0, 200.0, 24.0, 800.0, 0.0) >= 0.0);
    }

    #[test]
    fn visible_range_virtualizes_and_handles_empty() {
        assert_eq!(visible_range(0, 200.0, 24.0, 800.0, 0.0), 0..0);
        assert_eq!(visible_range(5, 0.0, 24.0, 800.0, 0.0), 0..0);
        // At offset 0 a 800 wide viewport shows the first few cards only.
        let range = visible_range(100, 200.0, 24.0, 800.0, 0.0);
        assert_eq!(range.start, 0);
        assert!(range.end >= 3 && range.end < 10);
        // Far scrolled: range is near the offset and never exceeds count.
        let range = visible_range(100, 200.0, 24.0, 800.0, 2240.0);
        assert!(range.start <= 10 && range.start >= 8);
        assert!(range.end <= 100);
        // Offset past the end clamps to an empty tail rather than panicking.
        let range = visible_range(3, 200.0, 24.0, 800.0, 10_000.0);
        assert_eq!(range, 3..3);
    }

    #[test]
    fn visible_range_is_contiguous_and_within_bounds() {
        for offset in (0..4000).step_by(37) {
            let range = visible_range(20, 200.0, 24.0, 800.0, offset as f32);
            assert!(range.start <= range.end);
            assert!(range.end <= 20);
        }
    }

    #[test]
    fn fade_alpha_is_bounded_monotonic_and_handles_zero_duration() {
        assert_eq!(fade_alpha(0.0, 0.4), 0.0);
        assert_eq!(fade_alpha(10.0, 0.4), 1.0);
        assert_eq!(fade_alpha(0.1, 0.0), 1.0);
        assert_eq!(fade_alpha(0.1, -1.0), 1.0);
        let mut prev = 0.0;
        for i in 0..=40 {
            let a = fade_alpha(f64::from(i) * 0.01, 0.4);
            assert!((0.0..=1.0).contains(&a));
            assert!(a >= prev);
            prev = a;
        }
    }
}
