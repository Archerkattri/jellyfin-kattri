//! Painter-drawn icons for the player's on-screen controls.
//!
//! The transport row used to be built from text approximations (`||`, `|◀`, `−10 s`, `PiP`),
//! which read as placeholders next to a video. These icons are drawn as shapes instead of
//! glyphs: no font fallback can turn them into boxes, they stay sharp at every scale factor,
//! and a button can be an icon plus an accessible name without shipping an icon font.
//!
//! Every icon is described in a 24x24 unit box and scaled into the square at the centre of the
//! rectangle it is given, so callers only decide where the icon goes, not how big it is.

use egui::{Color32, Painter, Pos2, Rect, Shape, Stroke};

/// One icon in the player chrome.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OsdIcon {
    /// Play: the action the pause button performs while paused.
    Play,
    /// Pause: two bars, the action while playing.
    Pause,
    /// Stop.
    Stop,
    /// Previous item or track: a triangle against a bar.
    PreviousItem,
    /// Next item or track: a bar after a triangle.
    NextItem,
    /// Seek backward: two triangles.
    Rewind,
    /// Seek forward: two triangles.
    Forward,
    /// Step back one frame: one triangle against a bar.
    FrameBack,
    /// Step forward one frame: one triangle against a bar.
    FrameForward,
    /// Previous chapter: two chevrons.
    ChapterPrevious,
    /// Next chapter: two chevrons.
    ChapterNext,
    /// Speaker with sound waves.
    Volume,
    /// Speaker with a cross.
    Muted,
    /// Picture-in-picture: a small inset frame in the corner.
    Pip,
    /// Entry to fullscreen: four corners pointing outward.
    EnterFullscreen,
    /// Exit from fullscreen: four corners pointing inward.
    ExitFullscreen,
}

impl OsdIcon {
    /// The icon and accessible name for the mute control, which shows the action it performs.
    pub(crate) fn for_mute(muted: Option<bool>) -> (Self, &'static str) {
        if muted == Some(true) {
            (Self::Muted, "Unmute")
        } else {
            (Self::Volume, "Mute")
        }
    }
}

/// The shapes that draw `icon` inside `rect`. Exposed for tests, which check that every icon
/// stays inside the box it was given.
pub(crate) fn shapes(rect: Rect, icon: OsdIcon, color: Color32) -> Vec<Shape> {
    let side = rect.width().min(rect.height()).max(1.0);
    let scale = side / 24.0;
    let centre = rect.center();
    let point = move |x: f32, y: f32| {
        Pos2::new(centre.x + (x - 12.0) * scale, centre.y + (y - 12.0) * scale)
    };
    let stroke = Stroke::new((2.0 * scale).max(1.0), color);
    let thin = Stroke::new((1.6 * scale).max(1.0), color);
    let triangle = |a: (f32, f32), b: (f32, f32), c: (f32, f32)| {
        Shape::convex_polygon(
            vec![point(a.0, a.1), point(b.0, b.1), point(c.0, c.1)],
            color,
            Stroke::NONE,
        )
    };
    let bar = |x0: f32, y0: f32, x1: f32, y1: f32| {
        Shape::rect_filled(Rect::from_two_pos(point(x0, y0), point(x1, y1)), 0.0, color)
    };
    let polyline = |points: &[(f32, f32)], stroke: Stroke| {
        Shape::line(
            points
                .iter()
                .map(|(x, y)| point(*x, *y))
                .collect::<Vec<_>>(),
            stroke,
        )
    };
    // The speaker body is shared by the two volume icons.
    let speaker = || {
        Shape::convex_polygon(
            vec![
                point(3.0, 9.5),
                point(8.0, 9.5),
                point(12.5, 5.0),
                point(12.5, 19.0),
                point(8.0, 14.5),
                point(3.0, 14.5),
            ],
            color,
            Stroke::NONE,
        )
    };

    match icon {
        OsdIcon::Play => vec![triangle((8.0, 5.0), (8.0, 19.0), (19.5, 12.0))],
        OsdIcon::Pause => vec![bar(8.0, 5.0, 11.0, 19.0), bar(14.5, 5.0, 17.5, 19.0)],
        OsdIcon::Stop => vec![Shape::rect_filled(
            Rect::from_min_max(point(6.5, 6.5), point(17.5, 17.5)),
            2.0 * scale,
            color,
        )],
        OsdIcon::PreviousItem => vec![
            bar(6.0, 5.0, 8.5, 19.0),
            triangle((19.0, 4.5), (19.0, 19.5), (9.0, 12.0)),
        ],
        OsdIcon::NextItem => vec![
            triangle((5.0, 4.5), (5.0, 19.5), (15.0, 12.0)),
            bar(15.5, 5.0, 18.0, 19.0),
        ],
        OsdIcon::Rewind => vec![
            triangle((12.5, 5.0), (12.5, 19.0), (3.5, 12.0)),
            triangle((21.0, 5.0), (21.0, 19.0), (12.0, 12.0)),
        ],
        OsdIcon::Forward => vec![
            triangle((3.0, 5.0), (3.0, 19.0), (12.0, 12.0)),
            triangle((11.5, 5.0), (11.5, 19.0), (20.5, 12.0)),
        ],
        OsdIcon::FrameBack => vec![
            bar(8.0, 5.0, 10.0, 19.0),
            triangle((20.0, 5.0), (20.0, 19.0), (11.0, 12.0)),
        ],
        OsdIcon::FrameForward => vec![
            triangle((4.0, 5.0), (4.0, 19.0), (13.0, 12.0)),
            bar(14.0, 5.0, 16.0, 19.0),
        ],
        OsdIcon::ChapterPrevious => vec![
            polyline(&[(14.5, 6.0), (8.5, 12.0), (14.5, 18.0)], stroke),
            polyline(&[(21.0, 6.0), (15.0, 12.0), (21.0, 18.0)], stroke),
        ],
        OsdIcon::ChapterNext => vec![
            polyline(&[(9.5, 6.0), (15.5, 12.0), (9.5, 18.0)], stroke),
            polyline(&[(3.0, 6.0), (9.0, 12.0), (3.0, 18.0)], stroke),
        ],
        OsdIcon::Volume => vec![
            speaker(),
            polyline(&[(15.5, 9.0), (17.5, 12.0), (15.5, 15.0)], thin),
            polyline(&[(18.0, 6.5), (21.0, 12.0), (18.0, 17.5)], thin),
        ],
        OsdIcon::Muted => vec![
            speaker(),
            polyline(&[(16.0, 9.0), (21.0, 15.0)], thin),
            polyline(&[(21.0, 9.0), (16.0, 15.0)], thin),
        ],
        OsdIcon::Pip => vec![
            Shape::rect_stroke(
                Rect::from_min_max(point(3.0, 5.5), point(21.0, 18.5)),
                2.0 * scale,
                stroke,
                egui::StrokeKind::Inside,
            ),
            bar(12.0, 11.5, 19.5, 17.0),
        ],
        OsdIcon::EnterFullscreen => vec![
            polyline(&[(4.0, 9.5), (4.0, 4.0), (9.5, 4.0)], stroke),
            polyline(&[(14.5, 4.0), (20.0, 4.0), (20.0, 9.5)], stroke),
            polyline(&[(20.0, 14.5), (20.0, 20.0), (14.5, 20.0)], stroke),
            polyline(&[(9.5, 20.0), (4.0, 20.0), (4.0, 14.5)], stroke),
        ],
        OsdIcon::ExitFullscreen => vec![
            polyline(&[(9.5, 4.0), (9.5, 9.5), (4.0, 9.5)], stroke),
            polyline(&[(14.5, 9.5), (20.0, 9.5), (14.5, 4.0)], stroke),
            polyline(&[(14.5, 20.0), (14.5, 14.5), (20.0, 14.5)], stroke),
            polyline(&[(9.5, 14.5), (4.0, 14.5), (9.5, 20.0)], stroke),
        ],
    }
}

/// Draw `icon` centred in `rect`.
pub(crate) fn paint(painter: &Painter, rect: Rect, icon: OsdIcon, color: Color32) {
    painter.extend(shapes(rect, icon, color));
}

#[cfg(test)]
mod tests {
    use super::{OsdIcon, shapes};
    use egui::Color32;

    const ALL: [OsdIcon; 16] = [
        OsdIcon::Play,
        OsdIcon::Pause,
        OsdIcon::Stop,
        OsdIcon::PreviousItem,
        OsdIcon::NextItem,
        OsdIcon::Rewind,
        OsdIcon::Forward,
        OsdIcon::FrameBack,
        OsdIcon::FrameForward,
        OsdIcon::ChapterPrevious,
        OsdIcon::ChapterNext,
        OsdIcon::Volume,
        OsdIcon::Muted,
        OsdIcon::Pip,
        OsdIcon::EnterFullscreen,
        OsdIcon::ExitFullscreen,
    ];

    #[test]
    fn every_icon_draws_something_and_stays_inside_its_box() {
        let rect = egui::Rect::from_min_size(egui::pos2(40.0, 40.0), egui::vec2(24.0, 24.0));
        for icon in ALL {
            let drawn = shapes(rect, icon, Color32::WHITE);
            assert!(!drawn.is_empty(), "{icon:?} draws nothing");
            for shape in drawn {
                let bounds = shape.visual_bounding_rect();
                assert!(
                    rect.expand(1.0).contains_rect(bounds),
                    "{icon:?} draws outside its box: {bounds:?}"
                );
            }
        }
    }

    #[test]
    fn icons_scale_with_the_box_they_are_given() {
        let small = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(16.0, 16.0));
        let large = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(64.0, 64.0));
        let small_width = shapes(small, OsdIcon::Play, Color32::WHITE)[0]
            .visual_bounding_rect()
            .width();
        let large_width = shapes(large, OsdIcon::Play, Color32::WHITE)[0]
            .visual_bounding_rect()
            .width();
        assert!(large_width > small_width * 3.0);
    }

    #[test]
    fn the_mute_control_shows_the_action_it_performs() {
        assert_eq!(OsdIcon::for_mute(Some(true)), (OsdIcon::Muted, "Unmute"));
        assert_eq!(OsdIcon::for_mute(Some(false)), (OsdIcon::Volume, "Mute"));
        // mpv starts unmuted and reports no value until the first file loads.
        assert_eq!(OsdIcon::for_mute(None), (OsdIcon::Volume, "Mute"));
    }
}
