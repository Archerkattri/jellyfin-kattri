//! Presentational playback controls for the native egui host.
//!
//! The chrome follows the patterns shared by current streaming players rather than a floating
//! settings panel. Researched patterns and sources:
//!
//! - Full-bleed video with a bottom scrim and auto-hiding chrome, kept visible while paused or
//!   while a menu is open: the Video.js 10 controls design
//!   <https://github.com/videojs/v10/blob/main/internal/design/ui/controls-design.md>.
//! - A single top overlay with a back affordance, title and secondary line; a large center
//!   play affordance while paused: the Netflix iOS video player and web player captures on
//!   Mobbin <https://mobbin.com/explore/screens/b1f2670c-5b85-416c-aec3-368bbc58c2fb>.
//! - A slim seek bar with a hover time/thumbnail bubble and drag scrubbing: Netflix's timeline
//!   preview behaviour
//!   <https://exabyting.com/blog/how-youtube-and-netflix-show-video-timeline-preview-images/>.
//! - Playback options grouped by the viewer's intent instead of several dense combo boxes:
//!   speed, audio, subtitles and video each open their own compact surface, following Infuse's
//!   Apple TV playback options
//!   <https://support.firecore.com/hc/en-us/articles/215090987-Playback-Controls>.
//! - Chapter ticks and a compact transport row: Jellyfin Web and Plex player reviews
//!   <https://forums.plex.tv/t/make-playback-speed-control-easier-to-access/877939>.
//! - Picture-in-picture from the control row and a settings gear: Disney+ player captures on
//!   Mobbin <https://mobbin.com/explore/screens/34a6b7ef-4ed3-4b60-9073-8f46ae0ec7e3>.
//!
//! Controls stay reachable at every window width: secondary transport controls move into an
//! overflow popover, and the four playback groups use a separate compact row on narrow windows.

use std::time::Duration;

use egui::accesskit::{Action, ActionData};
use egui::{Color32, RichText, Role, Sense, WidgetInfo};
use kattri_client_core::controls::PlaybackSpeed;
use kattri_client_core::segments::{
    SegmentError, SegmentKind, SegmentSkipper, SkipDecision, SkipSettings,
};
use kattri_settings::{AspectMode, PlaybackSettings};
use mpv_render_proof::{CalibrationMarker, TrackSelection};

use crate::blur::{BackdropBlur, show_frosted_glass_frame};
use crate::player_icons::{self, OsdIcon};
use jellyfin_egui_client::SeekThumbnail;

/// Width of the seek preview thumbnail in points.
const SEEK_PREVIEW_WIDTH: f32 = 240.0;

/// Size of one icon button in points. Generous enough for a remote or a touch pad, small enough
/// that the row never dwarfs the video.
const ICON_BUTTON: f32 = 32.0;
/// The play/pause button, the one control a viewer looks for first.
const PRIMARY_BUTTON: f32 = 44.0;
/// Height of the interactive strip around the slim seek track. The visible track is much thinner;
/// the strip is what the pointer and the focus ring use.
const SEEK_HIT_HEIGHT: f32 = 26.0;
/// Resting and hovered heights of the seek track, in points.
const SEEK_TRACK_REST: f32 = 3.0;
const SEEK_TRACK_HOVER: f32 = 6.0;

/// Fade duration for the chrome and small hover transitions. Reduced motion turns it off.
pub const CHROME_FADE: f32 = 0.24;
/// Height of the bottom gradient scrim. Tall enough to cover the seek bar and single control row.
pub const BOTTOM_SCRIM_HEIGHT: f32 = 150.0;
/// Height of the top gradient scrim under the back/title/Player details row.
pub const TOP_SCRIM_HEIGHT: f32 = 116.0;

/// Maximum number of chapter entries exposed in the popover.
pub const MAX_CHAPTER_CHOICES: usize = 128;

/// Width of a playback-group popover, clamped to the window.
const POPOVER_WIDTH: f32 = 320.0;

/// A chapter choice, using mpv's zero-based chapter index.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChapterChoice {
    /// Zero-based chapter index to send back to the player.
    pub index: usize,
    /// Human-readable chapter title.
    pub title: String,
    /// Chapter start time.
    pub time: Duration,
}

/// Read-only playback state needed to render the controls.
#[derive(Clone, Copy, Debug)]
pub struct PlayerControlsView<'a> {
    /// Current playback position.
    pub position: Duration,
    /// Known media duration, or `None` for live/unknown-length media.
    pub duration: Option<Duration>,
    /// Whether playback is paused; `None` means the state is not known yet.
    pub paused: Option<bool>,
    /// Current playback speed; `None` means libmpv has not reported it yet.
    pub speed_multiplier: Option<f64>,
    /// Whether local playback speed can be changed in the current session.
    pub speed_enabled: bool,
    /// Short label of the current video aspect mode, for the settings cycle row.
    pub aspect_label: &'a str,
    /// Whether the current source permits seeking.
    pub seek_enabled: bool,
    /// Whether there is a loaded item the Stop control can stop.
    pub stop_enabled: bool,
    /// Whether Previous has somewhere to go: the music queue, or an earlier episode.
    pub previous_track_enabled: bool,
    /// Whether Next has somewhere to go: the music queue, or a later episode.
    pub next_track_enabled: bool,
    /// Bounded-by-render chapter list. Only the first [`MAX_CHAPTER_CHOICES`] entries are shown.
    pub chapters: &'a [ChapterChoice],
    /// Secondary subtitle choices from `secondary_candidates`. Empty hides the selector.
    pub secondary_subtitles: &'a [SecondarySubtitleChoice],
    /// The mpv track id now set as secondary, or `None` when the secondary is off.
    pub secondary_selected: Option<i64>,
    /// Whether the window is in picture-in-picture now (M2-12). The button then leaves it.
    pub pip_active: bool,
    /// Whether picture-in-picture can be entered: a video is loaded and its renderer is ready.
    pub pip_available: bool,
    /// A-B loop point A for the current item, when one is set (M6-12). Session-only.
    /// A-B loop point B for the current item, when one is set (M6-12). Session-only.
    /// Observed mpv volume in percent. `None` hides the slider until mpv reports it.
    pub volume_percent: Option<f64>,
    /// Highest volume the settings allow, so the slider cannot exceed the boost cap.
    pub volume_max_percent: u32,
    /// Whether the session is muted (M2-16, session-only).
    pub muted: Option<bool>,
    /// Whether the window is fullscreen now.
    pub fullscreen: bool,
    /// End of the demuxer's loaded range, when the backend reports one. The seek bar paints it
    /// behind the progress; `None` leaves only the played range visible.
    pub buffered: Option<Duration>,
    /// Primary audio choices for the Audio popover. `None` while the inventory is loading.
    pub audio_tracks: Option<&'a [TrackChoice<TrackSelection>]>,
    /// Primary subtitle choices for the Subtitles popover. `None` while the inventory is loading.
    pub subtitle_tracks: Option<&'a [TrackChoice<TrackSelection>]>,
    /// Jellyfin source subtitles for an active server-burned transcode.
    pub burn_in_subtitle_tracks: Option<&'a [TrackChoice<Option<i32>>]>,
    /// Opacity of the whole chrome, from the host's OSD fade. Tests draw at full opacity.
    pub opacity: f32,
}

/// One secondary subtitle the selector offers.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SecondarySubtitleChoice {
    /// mpv track id, as written to `secondary-sid`.
    pub track: i64,
    /// Human-readable track label.
    pub title: String,
}

/// One entry in an audio or subtitle track combo. `value` is what the host applies when it is
/// chosen.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TrackChoice<T> {
    /// The value returned when this entry is picked.
    pub value: T,
    /// Human-readable entry text.
    pub label: String,
    /// Whether this entry is the one mpv has now.
    pub selected: bool,
}

/// What one gamepad action does while a video fills the window (M6-18).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GamepadPlayerAction {
    /// Move egui focus in the matching direction without forwarding input to libmpv.
    Navigate(egui::Key),
    /// Activate the focused player widget through egui's normal keyboard path.
    ActivateFocused,
    /// A transport control, the same as pressing it on screen.
    Control(PlayerControlAction),
    /// Leave the video (B / Back), as the on-screen Back button does.
    Leave,
    /// Show or hide the media info panel (Menu).
    ToggleMediaInfo,
}

/// Map a gamepad action to the player. Directional input follows the focused egui controls;
/// South activates the focused widget, and Play/Pause remains a direct transport action.
pub fn gamepad_player_action(action: kattri_client_core::focus::NavAction) -> GamepadPlayerAction {
    use kattri_client_core::focus::NavAction;
    match action {
        NavAction::Up => GamepadPlayerAction::Navigate(egui::Key::ArrowUp),
        NavAction::Down => GamepadPlayerAction::Navigate(egui::Key::ArrowDown),
        NavAction::Left => GamepadPlayerAction::Navigate(egui::Key::ArrowLeft),
        NavAction::Right => GamepadPlayerAction::Navigate(egui::Key::ArrowRight),
        NavAction::Select => GamepadPlayerAction::ActivateFocused,
        NavAction::PlayPause => GamepadPlayerAction::Control(PlayerControlAction::TogglePause),
        NavAction::Back => GamepadPlayerAction::Leave,
        NavAction::Menu => GamepadPlayerAction::ToggleMediaInfo,
    }
}

/// Semantic intent emitted by the controls; the caller performs player operations.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlayerControlAction {
    /// Seek by a signed number of seconds.
    SeekRelative(i64),
    /// Seek to an absolute position.
    SeekAbsolute(Duration),
    /// Toggle paused/playing state.
    TogglePause,
    /// Move to the previous queued track, or restart the current track.
    PreviousTrack,
    /// Move to the next queued track.
    NextTrack,
    /// Stop the active media and close its report lifecycle.
    Stop,
    /// Select one of the supported playback rates.
    SetSpeed(PlaybackSpeed),
    /// Select a zero-based chapter.
    /// Set the secondary subtitle track by mpv track id, or `None` to turn it off.
    SelectSecondarySubtitle(Option<i64>),
    /// Set the primary audio track from the Audio popover.
    SelectAudioTrack(TrackSelection),
    /// Set the primary subtitle track from the Subtitles popover.
    SelectSubtitleTrack(TrackSelection),
    /// Re-negotiate the active transcode with this Jellyfin subtitle stream index, or turn it off.
    SelectBurnInSubtitle(Option<i32>),
    /// Show or hide the existing media-info panel.
    ToggleMediaInfo,
    /// Enter or leave picture-in-picture (M2-12).
    TogglePip,
    /// Step exactly one frame, pausing first when playing (M6-12).
    StepFrame {
        /// True steps to the previous frame.
        backward: bool,
    },
    /// One press of the A-B control: set A, then set B, then clear both (M6-12).
    /// Set the playback volume in percent, before the boost cap is applied (M2-16).
    SetVolume(u32),
    /// Mute or unmute the session (M2-16, session-only).
    ToggleMute,
    /// Move one chapter forward or back through the same path as PageUp/PageDown.
    StepChapter {
        /// True moves to the next chapter.
        forward: bool,
    },
    /// Enter or leave fullscreen.
    ToggleFullscreen,
    /// Resize the window client area so one video pixel is one physical screen pixel.
    FitWindowToVideo,
    /// Switch between normal and double playback speed.
    ToggleDoubleSpeed,
    /// Step the playback rate one ladder notch (Ctrl+Up / Ctrl+Down).
    StepSpeed {
        /// True steps faster, false slower.
        up: bool,
    },
    /// Return the playback rate to normal (Ctrl+0).
    ResetSpeed,
    /// Cycle the video aspect ratio one step (Ctrl+A or the A key).
    CycleAspect,
    /// Select a specific video aspect ratio from the Video popover.
    SetAspect(AspectMode),
}

/// Segment skip for the current item. The caller keeps `skipper` loaded with the item's
/// segments and builds `settings` with [`skip_settings`].
pub struct SkipControls<'a> {
    pub skipper: &'a mut SegmentSkipper,
    pub settings: SkipSettings,
}

/// Build the skip model from the persisted playback settings.
pub fn skip_settings(playback: &PlaybackSettings) -> Result<SkipSettings, SegmentError> {
    SkipSettings::new(
        playback.skip_intro,
        playback.skip_outro,
        playback.auto_skip_intro,
        playback.auto_skip_outro,
        playback.outro_delay_ms,
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ControlId {
    SeekBar,
    PlayPause,
    PreviousTrack,
    NextTrack,
    Stop,
    SeekBackward,
    SeekForward,
    SkipSegment,
    SpeedMenu,
    AudioMenu,
    SubtitlesMenu,
    VideoMenu,
    PlaybackMenu,
    PipToggle,
    MuteToggle,
    VolumeSlider,
    Fullscreen,
}

/// Video chrome stays neutral white-on-black whatever the app theme is, because it sits on
/// arbitrary film frames. The accent is a soft white rather than the old bright blue.
const OSD_ACCENT: Color32 = Color32::from_rgb(0xF2, 0xF4, 0xF5);
const OSD_TRACK: Color32 = Color32::from_rgba_premultiplied(54, 54, 54, 54);
const OSD_BUFFERED: Color32 = Color32::from_rgba_premultiplied(92, 92, 92, 92);
const OSD_FOCUS: Color32 = Color32::from_rgb(0x7D, 0xD8, 0xF4);
const OSD_TEXT: Color32 = Color32::from_rgb(242, 244, 245);
pub const OSD_MUTED: Color32 = Color32::from_rgb(176, 182, 188);
const OSD_POPOVER: Color32 = Color32::from_rgb(20, 21, 23);
const SYNCED_SPEED_TOOLTIP: &str = "Playback speed is synchronized by Watch Together.";

/// Multiply `color`'s opacity. `gamma_multiply` scales the premultiplied channels, which is the
/// correct result for a translucent overlay on video.
fn faded(color: Color32, opacity: f32) -> Color32 {
    color.gamma_multiply(opacity.clamp(0.0, 1.0))
}

/// The chrome fade duration for an egui style. The app sets `animation_time` to zero when the
/// viewer asked for reduced motion, and then every transition snaps.
pub fn fade_seconds(animation_time: f32) -> f32 {
    if animation_time > 0.0 {
        CHROME_FADE
    } else {
        0.0
    }
}

/// Move an opacity value toward `target` by the fraction of `over` that `elapsed` represents.
pub fn advance_fade(current: f32, target: f32, elapsed: Duration, over: Duration) -> f32 {
    let current = current.clamp(0.0, 1.0);
    let target = target.clamp(0.0, 1.0);
    if over.is_zero() {
        return target;
    }
    let step = (elapsed.as_secs_f32() / over.as_secs_f32()).clamp(0.0, 1.0);
    if target >= current {
        (current + step).min(target)
    } else {
        (current - step).max(target)
    }
}

/// Keep the chrome visible while the existing OSD timer says so, while a control has keyboard
/// focus, or while any popup is open. This is a decision on top of `OsdVisibility`, not a second
/// inactivity timer.
pub fn player_controls_visible(timed: bool, focused: bool, popup_open: bool) -> bool {
    timed || focused || popup_open
}

/// Whether the centered play affordance should be offered.
pub fn center_play_visible(paused: Option<bool>, media_active: bool) -> bool {
    media_active && paused == Some(true)
}

/// Draw compact, keyboard-accessible playback controls and return user actions. The automatic
/// skip seeks without a press. The button seeks on press and appears only while a segment is
/// skippable now. Both return [`PlayerControlAction::SeekAbsolute`] for the caller to perform.
///
/// The host calls [`draw_with_skip_and_extra`]; this entry point is what the widget tests use.
#[cfg(test)]
pub fn draw_with_skip(
    ui: &mut egui::Ui,
    view: PlayerControlsView<'_>,
    skip: Option<SkipControls<'_>>,
) -> Vec<PlayerControlAction> {
    draw_inner_with_skip(ui, view, skip, |_, _| {}, |_| {})
}

/// Draw the controls, then let the caller place one widget inside the Subtitles popover. The host
/// puts its subtitle-sync controls there, so they move and hide with the bar instead of floating
/// below the panel, and remain reachable by pointer and keyboard from the first control.
///
/// `preview` gives the trickplay thumbnail for a position; while the pointer is over the seek
/// bar (or drags it) the time there is shown above the bar, with the thumbnail when there is one.
pub(crate) fn draw_with_skip_and_extra(
    ui: &mut egui::Ui,
    view: PlayerControlsView<'_>,
    skip: Option<SkipControls<'_>>,
    extra: impl FnOnce(&mut egui::Ui),
    preview: impl FnMut(Duration) -> Option<SeekThumbnail>,
    backdrop: Option<BackdropBlur>,
) -> Vec<PlayerControlAction> {
    draw_controls(ui, view, skip, |_, _| {}, Some(extra), preview, backdrop)
}

// The host always goes through `draw_with_skip`. The plain draw and the observer hook exist for
// the widget tests, which need to find control rectangles.
#[cfg(test)]
fn draw(ui: &mut egui::Ui, view: PlayerControlsView<'_>) -> Vec<PlayerControlAction> {
    draw_inner(ui, view, |_, _| {})
}

#[cfg(test)]
fn draw_inner(
    ui: &mut egui::Ui,
    view: PlayerControlsView<'_>,
    observe: impl FnMut(ControlId, egui::Rect),
) -> Vec<PlayerControlAction> {
    draw_inner_with_skip(ui, view, None, observe, |_| {})
}

/// The automatic skip for a frame when the controls are not drawn. `draw_with_skip` runs the same
/// check while the OSD is visible. Without this, auto-skip would stop when the OSD fades out.
pub fn auto_skip_action(
    skip: SkipControls<'_>,
    position: Duration,
    duration: Option<Duration>,
) -> Option<PlayerControlAction> {
    let SkipControls { skipper, settings } = skip;
    let decision = skipper.pick(
        duration_to_ticks(position),
        duration.map(duration_to_ticks),
        &settings,
    )?;
    Some(PlayerControlAction::SeekAbsolute(ticks_to_duration(
        decision.seek_to_ticks,
    )))
}

#[cfg(test)]
fn draw_inner_with_skip(
    ui: &mut egui::Ui,
    view: PlayerControlsView<'_>,
    skip: Option<SkipControls<'_>>,
    observe: impl FnMut(ControlId, egui::Rect),
    extra: impl FnOnce(&mut egui::Ui),
) -> Vec<PlayerControlAction> {
    draw_controls(ui, view, skip, observe, Some(extra), |_| None, None)
}

/// The seek position under `x` on a bar spanning `rect`, for a bar ending at `seek_end`.
fn seek_position_at(rect: egui::Rect, x: f32, seek_end: Duration) -> Option<Duration> {
    let width = rect.width();
    (width > 0.0).then(|| {
        let fraction = ((x - rect.left()) / width).clamp(0.0, 1.0);
        position_at_fraction(fraction, seek_end)
    })
}

/// Fraction of the seekable timeline that `time` occupies.
fn timeline_fraction(time: Duration, seek_end: Duration) -> f32 {
    if seek_end.is_zero() {
        return 0.0;
    }
    (time.as_secs_f64() / seek_end.as_secs_f64()).clamp(0.0, 1.0) as f32
}

/// Progress of the played position on the bar, where the seekable end is one second before the
/// reported duration. That keeps the thumb reachable at the end of the item.
#[cfg(test)]
fn seek_fraction(position: Duration, duration: Option<Duration>) -> f32 {
    let Some(duration) = duration.filter(|duration| !duration.is_zero()) else {
        return 0.0;
    };
    timeline_fraction(position, duration.saturating_sub(Duration::from_secs(1)))
}

/// The position at `fraction` of a bar ending at `seek_end`.
fn position_at_fraction(fraction: f32, seek_end: Duration) -> Duration {
    Duration::from_secs_f64(f64::from(fraction.clamp(0.0, 1.0)) * seek_end.as_secs_f64())
}

/// Layout tiers for the single-row control bar. A narrow window folds secondary controls into
/// the More overflow popover so the primary transport and menus fit on one row.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ChromeLayout {
    roomy: bool,
    medium: bool,
    compact: bool,
    /// Below this tier only Play/Pause and the compact menu icons fit on the row.
    tiny: bool,
}

fn chrome_layout(width: f32) -> ChromeLayout {
    ChromeLayout {
        roomy: width >= 720.0,
        medium: width >= 470.0,
        compact: width >= 350.0,
        tiny: width < 330.0,
    }
}

/// The independent playback surfaces opened from the control row.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PlaybackPopover {
    Speed,
    Audio,
    Subtitles,
    Video,
    Playback,
}

impl PlaybackPopover {
    fn id(self) -> egui::Id {
        egui::Id::unique(match self {
            Self::Speed => "kattri-player-speed",
            Self::Audio => "kattri-player-audio",
            Self::Subtitles => "kattri-player-subtitles",
            Self::Video => "kattri-player-video",
            Self::Playback => "kattri-player-playback",
        })
    }

    fn control(self) -> ControlId {
        match self {
            Self::Speed => ControlId::SpeedMenu,
            Self::Audio => ControlId::AudioMenu,
            Self::Subtitles => ControlId::SubtitlesMenu,
            Self::Video => ControlId::VideoMenu,
            Self::Playback => ControlId::PlaybackMenu,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Speed => "Speed",
            Self::Audio => "Audio",
            Self::Subtitles => "Subtitles",
            Self::Video => "Video",
            Self::Playback => "More",
        }
    }
}

fn subtitles_available(view: &PlayerControlsView<'_>) -> bool {
    view.burn_in_subtitle_tracks
        .is_some_and(|tracks| !tracks.is_empty())
        || view
            .subtitle_tracks
            .is_some_and(|tracks| !tracks.is_empty())
        || !view.secondary_subtitles.is_empty()
}

fn playback_overflow_available(layout: ChromeLayout) -> bool {
    !layout.roomy
}

/// Paints the time at the pointer above the seek bar, and the trickplay thumbnail above it when
/// there is one. Drawn on the tooltip layer so the video and other controls never cover it.
fn paint_seek_preview(
    ui: &egui::Ui,
    bar: egui::Rect,
    x: f32,
    time: Duration,
    thumbnail: Option<SeekThumbnail>,
    opacity: f32,
) {
    let painter = ui.ctx().layer_painter(egui::LayerId::new(
        egui::Order::Tooltip,
        egui::Id::unique("kattri-seek-preview"),
    ));
    let label = painter.layout_no_wrap(
        format_time(time),
        egui::FontId::monospace(13.0),
        faded(OSD_TEXT, opacity),
    );
    let image_size = thumbnail.map(|thumbnail| {
        egui::vec2(
            SEEK_PREVIEW_WIDTH,
            SEEK_PREVIEW_WIDTH / thumbnail.aspect.clamp(0.5, 4.0),
        )
    });
    let padding = 4.0;
    let content_width = image_size.map_or(label.size().x, |size| size.x.max(label.size().x));
    let content_height = image_size.map_or(0.0, |size| size.y + padding) + label.size().y;
    let size = egui::vec2(
        content_width + 2.0 * padding,
        content_height + 2.0 * padding,
    );
    let screen = ui.ctx().content_rect();
    let left = (x - size.x / 2.0)
        .min(screen.right() - size.x - 4.0)
        .max(screen.left() + 4.0);
    let top = (bar.top() - 10.0 - size.y).max(screen.top() + 4.0);
    let frame = egui::Rect::from_min_size(egui::pos2(left, top), size);
    painter.rect_filled(
        frame,
        egui::CornerRadius::same(8),
        faded(OSD_POPOVER, opacity),
    );
    painter.rect_stroke(
        frame,
        egui::CornerRadius::same(8),
        egui::Stroke::new(1.0, faded(Color32::from_white_alpha(40), opacity)),
        egui::StrokeKind::Inside,
    );
    let mut cursor = frame.min + egui::vec2(padding, padding);
    if let (Some(thumbnail), Some(image_size)) = (thumbnail, image_size) {
        let image = egui::Rect::from_min_size(cursor, image_size);
        painter.image(
            thumbnail.texture,
            image,
            thumbnail.uv,
            faded(Color32::WHITE, opacity),
        );
        cursor.y += image_size.y + padding;
    }
    let label_left = frame.center().x - label.size().x / 2.0;
    painter.galley(
        egui::pos2(label_left, cursor.y),
        label,
        faded(OSD_TEXT, opacity),
    );
}

/// Paints the bottom-on-top gradient that replaces the old floating card. `screen` is the full
/// video area, so the scrim runs edge to edge while the row keeps its own margins.
pub fn paint_bottom_scrim(painter: &egui::Painter, screen: egui::Rect, opacity: f32) {
    let top = (screen.bottom() - BOTTOM_SCRIM_HEIGHT).max(screen.top());
    let rect = egui::Rect::from_min_max(
        egui::pos2(screen.left(), top),
        egui::pos2(screen.right(), screen.bottom()),
    );
    paint_vertical_scrim(painter, rect, 0, scale_alpha(236, opacity));
}

/// Paints the top scrim under the back chevron, title and Player details actions.
pub fn paint_top_scrim(painter: &egui::Painter, screen: egui::Rect, opacity: f32) {
    let bottom = (screen.top() + TOP_SCRIM_HEIGHT).min(screen.bottom());
    let rect = egui::Rect::from_min_max(screen.left_top(), egui::pos2(screen.right(), bottom));
    paint_vertical_scrim(painter, rect, scale_alpha(214, opacity), 0);
}

fn scale_alpha(alpha: u8, opacity: f32) -> u8 {
    (f32::from(alpha) * opacity.clamp(0.0, 1.0)).round() as u8
}

fn paint_vertical_scrim(
    painter: &egui::Painter,
    rect: egui::Rect,
    top_alpha: u8,
    bottom_alpha: u8,
) {
    if rect.height() <= 0.0 || (top_alpha == 0 && bottom_alpha == 0) {
        return;
    }
    let mut mesh = egui::Mesh::default();
    for (pos, alpha) in [
        (rect.left_top(), top_alpha),
        (rect.right_top(), top_alpha),
        (rect.left_bottom(), bottom_alpha),
        (rect.right_bottom(), bottom_alpha),
    ] {
        mesh.vertices.push(egui::epaint::Vertex {
            pos,
            uv: egui::epaint::WHITE_UV,
            color: Color32::from_black_alpha(alpha),
        });
    }
    mesh.indices.extend_from_slice(&[0, 1, 2, 2, 1, 3]);
    painter.add(egui::Shape::mesh(mesh));
}

/// What the top overlay asked the host to do this frame.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TopChromeAction {
    /// No action.
    None,
    /// Leave the video, as the old Back pill did.
    Back,
    /// Open or toggle the compact media-info overlay.
    MediaInfo,
}

/// Everything the top overlay draws.
#[derive(Clone, Copy)]
pub struct TopChromeView<'a> {
    /// Item title.
    pub title: Option<&'a str>,
    /// Secondary line, normally the current chapter.
    pub subtitle: Option<&'a str>,
    /// Opacity from the shared OSD fade.
    pub opacity: f32,
    /// Whether Media info is currently open; closing it latches hover/focus until re-entry.
    pub media_info_open: bool,
    /// Whether the Media info trigger is shown (live playback vs. loading skeleton).
    pub show_media_info: bool,
}

/// What the top overlay produced this frame: any explicit button click plus whether the
/// "Media info" trigger is currently hovered or keyboard-focused.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TopChromeOutcome {
    pub action: TopChromeAction,
    pub media_info_hot: bool,
    pub media_info_rect: egui::Rect,
}

/// Actions emitted by the compact Picture-in-Picture overlay.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PipOverlayAction {
    None,
    ExitPip,
    TogglePaused,
}

/// Draw the compact floating controls for the Picture-in-Picture window: an
/// `"Exit picture-in-picture"` icon button in the top-right corner and a Play/Pause button at the
/// bottom-center.
pub fn draw_pip_overlay(
    ctx: &egui::Context,
    stage: egui::Rect,
    paused: Option<bool>,
    opacity: f32,
) -> PipOverlayAction {
    let opacity = opacity.clamp(0.0, 1.0);
    if opacity <= 0.0 {
        return PipOverlayAction::None;
    }
    let screen = ctx.content_rect();
    let mut action = PipOverlayAction::None;
    egui::Area::new(egui::Id::unique("kattri-pip-exit"))
        .order(egui::Order::Foreground)
        .anchor(
            egui::Align2::RIGHT_TOP,
            egui::vec2(
                stage.right() - screen.right() - 10.0,
                stage.top() - screen.top() + 10.0,
            ),
        )
        .show(ctx, |ui| {
            egui::Frame::new()
                .fill(faded(Color32::from_black_alpha(150), opacity))
                .corner_radius(egui::CornerRadius::same(8))
                .show(ui, |ui| {
                    if icon_button(
                        ui,
                        OsdIcon::Pip,
                        "Exit picture-in-picture",
                        true,
                        opacity,
                        |_| {},
                    )
                    .clicked()
                    {
                        action = PipOverlayAction::ExitPip;
                    }
                });
        });
    let (play_icon, play_label) = transport_icon(paused);
    egui::Area::new(egui::Id::unique("kattri-pip-play-pause"))
        .order(egui::Order::Foreground)
        .anchor(
            egui::Align2::CENTER_BOTTOM,
            egui::vec2(
                stage.center().x - screen.center().x,
                stage.bottom() - screen.bottom() - 12.0,
            ),
        )
        .show(ctx, |ui| {
            egui::Frame::new()
                .fill(faded(Color32::from_black_alpha(150), opacity))
                .corner_radius(egui::CornerRadius::same(22))
                .show(ui, |ui| {
                    if primary_icon_button(
                        ui,
                        play_icon,
                        play_label,
                        true,
                        PRIMARY_BUTTON,
                        opacity,
                        |_| {},
                    )
                    .clicked()
                    {
                        action = PipOverlayAction::TogglePaused;
                    }
                });
        });
    action
}

/// Draw the top overlay: a back chevron, the title block, and the compact Media info action. The
/// host paints [`paint_top_scrim`] behind this, and the whole row hides with the bottom controls.
pub fn draw_top_chrome_outcome(ui: &mut egui::Ui, view: TopChromeView<'_>) -> TopChromeOutcome {
    let _ = view.media_info_open;
    let opacity = view.opacity.clamp(0.0, 1.0);
    let mut action = TopChromeAction::None;
    let mut media_info_hot = false;
    let mut media_info_rect = egui::Rect::NOTHING;
    egui::Frame::new()
        .inner_margin(egui::Margin {
            left: 18,
            right: 18,
            top: 10,
            bottom: 14,
        })
        .show(ui, |ui| {
            ui.allocate_ui_with_layout(
                egui::vec2(ui.available_width().max(1.0), 34.0),
                egui::Layout::left_to_right(egui::Align::Center),
                |ui| {
                    if chrome_icon_text_button(ui, OsdIcon::Back, "Back", opacity) {
                        action = TopChromeAction::Back;
                    }
                    ui.add_space(10.0);
                    let right_reserve = if view.show_media_info {
                        ICON_BUTTON + 10.0
                    } else {
                        0.0
                    };
                    let title_width = (ui.available_width() - right_reserve).max(1.0);
                    ui.allocate_ui_with_layout(
                        egui::vec2(title_width, 34.0),
                        egui::Layout::left_to_right(egui::Align::Center),
                        |ui| {
                            ui.set_max_width(title_width);
                            let title = view.title.map(str::trim).filter(|t| !t.is_empty());
                            let subtitle = view.subtitle.map(str::trim).filter(|t| !t.is_empty());
                            match (title, subtitle) {
                                (Some(title), None) => {
                                    ui.add(
                                        egui::Label::new(
                                            RichText::new(title)
                                                .size(17.0)
                                                .strong()
                                                .color(faded(OSD_TEXT, opacity)),
                                        )
                                        .truncate(),
                                    );
                                }
                                (title, subtitle) => {
                                    ui.vertical(|ui| {
                                        if let Some(title) = title {
                                            ui.add(
                                                egui::Label::new(
                                                    RichText::new(title)
                                                        .size(17.0)
                                                        .strong()
                                                        .color(faded(OSD_TEXT, opacity)),
                                                )
                                                .truncate(),
                                            );
                                        }
                                        if let Some(subtitle) = subtitle {
                                            ui.add(
                                                egui::Label::new(
                                                    RichText::new(subtitle)
                                                        .size(12.0)
                                                        .color(faded(OSD_MUTED, opacity)),
                                                )
                                                .truncate(),
                                            );
                                        }
                                    });
                                }
                            }
                        },
                    );
                    if view.show_media_info {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let media_info =
                                icon_button(ui, OsdIcon::Info, "Media info", true, opacity, |_| {});
                            media_info_hot = media_info.hovered() || media_info.has_focus();
                            media_info_rect = media_info.rect;
                            if media_info.clicked() {
                                action = TopChromeAction::MediaInfo;
                            }
                        });
                    }
                },
            );
        });
    TopChromeOutcome {
        action,
        media_info_hot,
        media_info_rect,
    }
}

/// The large centered play affordance while paused. Returns true when it was clicked.
pub fn draw_center_play(
    ctx: &egui::Context,
    stage: egui::Rect,
    paused: Option<bool>,
    media_active: bool,
    opacity: f32,
) -> bool {
    if !center_play_visible(paused, media_active) || opacity <= 0.0 {
        return false;
    }
    let opacity = opacity.clamp(0.0, 1.0);
    let mut clicked = false;
    egui::Area::new(egui::Id::unique("kattri-center-play"))
        .order(egui::Order::Foreground)
        .anchor(
            egui::Align2::CENTER_CENTER,
            stage.center() - ctx.content_rect().center(),
        )
        .show(ctx, |ui| {
            let (rect, response) = ui.allocate_exact_size(egui::vec2(84.0, 84.0), Sense::click());
            let fill = if response.hovered() {
                Color32::from_white_alpha(48)
            } else {
                Color32::from_white_alpha(28)
            };
            ui.painter()
                .circle_filled(rect.center(), rect.width() / 2.0, faded(fill, opacity));
            ui.painter().circle_stroke(
                rect.center(),
                rect.width() / 2.0,
                egui::Stroke::new(1.0, faded(Color32::from_white_alpha(70), opacity)),
            );
            player_icons::paint(
                ui.painter(),
                icon_area(rect, 0.42),
                OsdIcon::Play,
                faded(Color32::WHITE, opacity),
            );
            focus_ring(ui, &response, egui::CornerRadius::same(42), opacity);
            response.widget_info(|| WidgetInfo::labeled(Role::Button, true, "Play"));
            clicked = response.on_hover_text("Play").clicked();
        });
    clicked
}

/// The current seek bar frame: the seek the host should perform, and the position the bar drew.
struct SeekBar {
    action: Option<PlayerControlAction>,
    displayed: Duration,
}

fn seek_bar(
    ui: &mut egui::Ui,
    view: &PlayerControlsView<'_>,
    opacity: f32,
    observe: &mut impl FnMut(ControlId, egui::Rect),
    preview: &mut impl FnMut(Duration) -> Option<SeekThumbnail>,
) -> SeekBar {
    let duration = view.duration.unwrap_or_default();
    let seek_end = duration.saturating_sub(Duration::from_secs(1));
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(ui.available_width().max(1.0), SEEK_HIT_HEIGHT),
        Sense::click_and_drag(),
    );
    observe(ControlId::SeekBar, rect);

    let active = response.hovered() || response.dragged() || response.has_focus();
    let grow = ui.ctx().animate_bool_with_time(
        response.id.with("grow"),
        active,
        fade_seconds(ui.style().animation_time),
    );
    let height = SEEK_TRACK_REST + (SEEK_TRACK_HOVER - SEEK_TRACK_REST) * grow;
    let track = egui::Rect::from_center_size(rect.center(), egui::vec2(rect.width(), height));

    let pointer_x = response
        .interact_pointer_pos()
        .or_else(|| response.hover_pos())
        .map(|pointer| pointer.x);
    let pointer_time = pointer_x.and_then(|x| seek_position_at(rect, x, seek_end));
    let pressed = response.clicked() || response.dragged() || response.is_pointer_button_down_on();
    let displayed = pointer_time
        .filter(|_| pressed)
        .unwrap_or_else(|| view.position.min(seek_end));

    let mut action = None;
    if pressed && let Some(time) = pointer_time {
        action = Some(PlayerControlAction::SeekAbsolute(time));
        response.request_focus();
    }
    if response.has_focus() && action.is_none() {
        let mut seek = None;
        ui.input_mut(|input| {
            if input.consume_key(egui::Modifiers::NONE, egui::Key::ArrowLeft) {
                seek = Some(-10);
            } else if input.consume_key(egui::Modifiers::NONE, egui::Key::ArrowRight) {
                seek = Some(10);
            }
        });
        if let Some(delta) = seek {
            action = Some(PlayerControlAction::SeekRelative(delta));
        }
    }
    if action.is_none() {
        action = ui.input(|input| {
            if let Some(seconds) = input
                .accesskit_action_requests(response.id, Action::SetValue)
                .filter_map(|request| match request.data {
                    Some(ActionData::NumericValue(seconds)) if seconds.is_finite() => Some(seconds),
                    _ => None,
                })
                .last()
            {
                return Some(PlayerControlAction::SeekAbsolute(Duration::from_secs_f64(
                    seconds.clamp(0.0, seek_end.as_secs_f64()),
                )));
            }
            if input.has_accesskit_action_request(response.id, Action::Increment) {
                return Some(PlayerControlAction::SeekRelative(10));
            }
            if input.has_accesskit_action_request(response.id, Action::Decrement) {
                return Some(PlayerControlAction::SeekRelative(-10));
            }
            None
        });
    }

    paint_seek_track(ui, view, track, displayed, opacity, grow);
    if let (Some(x), Some(time)) = (pointer_x, pointer_time)
        && (response.hovered() || response.dragged())
    {
        paint_seek_preview(ui, rect, x, time, preview(time), opacity);
    }
    if response.has_focus() {
        ui.painter().rect_stroke(
            rect.expand(3.0),
            egui::CornerRadius::same(8),
            egui::Stroke::new(2.0, faded(OSD_FOCUS, opacity)),
            egui::StrokeKind::Outside,
        );
    }
    response.widget_info(|| WidgetInfo::slider(true, displayed.as_secs_f64(), "Playback position"));
    ui.ctx().accesskit_node_builder(response.id, |node| {
        node.set_min_numeric_value(0.0);
        node.set_max_numeric_value(seek_end.as_secs_f64());
        node.set_numeric_value_step(10.0);
        node.add_action(Action::SetValue);
        node.add_action(Action::Increment);
        node.add_action(Action::Decrement);
    });
    response.on_hover_text("Playback position");

    SeekBar { action, displayed }
}

fn paint_seek_track(
    ui: &egui::Ui,
    view: &PlayerControlsView<'_>,
    track: egui::Rect,
    displayed: Duration,
    opacity: f32,
    grow: f32,
) {
    let painter = ui.painter();
    let rounding = egui::CornerRadius::same((track.height() / 2.0).round() as u8);
    let width = track.width();
    let duration = view.duration.unwrap_or_default();
    let seek_end = duration.saturating_sub(Duration::from_secs(1));
    painter.rect_filled(track, rounding, faded(OSD_TRACK, opacity));

    if let Some(buffered) = view.buffered {
        let loaded = timeline_fraction(buffered.min(seek_end), seek_end);
        if loaded > 0.0 {
            let rect = egui::Rect::from_min_max(
                track.left_top(),
                egui::pos2(track.left() + width * loaded, track.bottom()),
            );
            painter.rect_filled(rect, rounding, faded(OSD_BUFFERED, opacity));
        }
    }

    let played = timeline_fraction(displayed, seek_end);
    if played > 0.0 {
        let rect = egui::Rect::from_min_max(
            track.left_top(),
            egui::pos2(track.left() + width * played, track.bottom()),
        );
        painter.rect_filled(rect, rounding, faded(OSD_ACCENT, opacity));
    }

    if !seek_end.is_zero() {
        for chapter in view.chapters.iter().take(MAX_CHAPTER_CHOICES) {
            let fraction = timeline_fraction(chapter.time, seek_end);
            if fraction <= 0.0 || fraction >= 1.0 {
                continue;
            }
            let x = track.left() + width * fraction;
            let tick = egui::Rect::from_center_size(
                egui::pos2(x, track.center().y),
                egui::vec2(1.5, track.height() + 4.0),
            );
            painter.rect_filled(
                tick,
                egui::CornerRadius::same(1),
                faded(Color32::from_white_alpha(150), opacity),
            );
        }
    }

    let x = track.left() + width * played;
    painter.circle_filled(
        egui::pos2(x, track.center().y),
        4.5 + 2.5 * grow,
        faded(OSD_ACCENT, opacity),
    );
}

fn draw_controls(
    ui: &mut egui::Ui,
    view: PlayerControlsView<'_>,
    mut skip: Option<SkipControls<'_>>,
    mut observe: impl FnMut(ControlId, egui::Rect),
    extra: Option<impl FnOnce(&mut egui::Ui)>,
    mut preview: impl FnMut(Duration) -> Option<SeekThumbnail>,
    backdrop: Option<BackdropBlur>,
) -> Vec<PlayerControlAction> {
    let opacity = view.opacity.clamp(0.0, 1.0);
    let seek_available = view.seek_enabled && view.duration.is_some_and(|d| !d.is_zero());
    let mut actions = keyboard_seek_actions(ui.ctx(), seek_available);

    // The automatic switch is checked first inside `pick`. It records the skip, so the same
    // segment is not seeked to again. Otherwise the button is offered for this frame.
    let mut offered: Option<SkipDecision> = None;
    if let Some(controls) = skip.as_mut()
        && view.seek_enabled
    {
        let position = duration_to_ticks(view.position);
        let duration = view.duration.map(duration_to_ticks);
        if let Some(decision) = controls
            .skipper
            .pick(position, duration, &controls.settings)
        {
            actions.push(PlayerControlAction::SeekAbsolute(ticks_to_duration(
                decision.seek_to_ticks,
            )));
        } else {
            offered = controls
                .skipper
                .button(position, duration, &controls.settings);
        }
    }

    let mut timeline: Option<(Duration, Duration)> = None;
    let available = ui.available_width();
    let horizontal_margin = if available < 260.0 {
        4
    } else if available < 360.0 {
        10
    } else {
        18
    };

    egui::Frame::new()
        .inner_margin(egui::Margin {
            left: horizontal_margin,
            right: horizontal_margin,
            top: 6,
            bottom: 14,
        })
        .show(ui, |ui| {
            ui.set_min_width(0.0);
            ui.visuals_mut().selection.bg_fill = faded(OSD_ACCENT, opacity);
            ui.visuals_mut().selection.stroke.color = faded(OSD_TEXT, opacity);
            ui.visuals_mut().widgets.inactive.bg_fill = Color32::TRANSPARENT;
            ui.visuals_mut().widgets.inactive.bg_stroke = egui::Stroke::NONE;
            ui.visuals_mut().widgets.inactive.fg_stroke.color = faded(OSD_TEXT, opacity);
            ui.visuals_mut().widgets.hovered.bg_fill = Color32::from_white_alpha(34);
            ui.visuals_mut().widgets.hovered.bg_stroke = egui::Stroke::NONE;
            ui.visuals_mut().widgets.hovered.fg_stroke.color = faded(OSD_TEXT, opacity);
            ui.visuals_mut().widgets.active.bg_fill = Color32::from_white_alpha(54);
            ui.visuals_mut().widgets.active.fg_stroke.color = faded(OSD_TEXT, opacity);
            ui.spacing_mut().item_spacing = egui::vec2(4.0, 4.0);

            if seek_available {
                let seek = seek_bar(ui, &view, opacity, &mut observe, &mut preview);
                if let Some(action) = seek.action {
                    actions.push(action);
                }
                timeline = Some((seek.displayed, view.duration.unwrap_or_default()));
            }

            ui.add_space(4.0);
            let row_width = ui.available_width();
            let layout = chrome_layout(row_width);
            let button_size = if row_width < 220.0 { 26.0 } else { ICON_BUTTON };
            let primary_size = if layout.tiny {
                button_size
            } else {
                PRIMARY_BUTTON
            };
            let spacing = if layout.medium {
                4.0
            } else if layout.tiny {
                1.0
            } else {
                2.0
            };

            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing = egui::vec2(spacing, 4.0);

                let (play_icon, play_accessible) = transport_icon(view.paused);
                if primary_icon_button(
                    ui,
                    play_icon,
                    play_accessible,
                    view.paused.is_some(),
                    primary_size,
                    opacity,
                    |rect| observe(ControlId::PlayPause, rect),
                )
                .clicked()
                {
                    actions.push(PlayerControlAction::TogglePause);
                }

                if !layout.tiny
                    && icon_button(
                        ui,
                        OsdIcon::Rewind,
                        "Seek backward 10 seconds",
                        seek_available,
                        opacity,
                        |rect| observe(ControlId::SeekBackward, rect),
                    )
                    .clicked()
                {
                    actions.push(PlayerControlAction::SeekRelative(-10));
                }

                if !layout.tiny
                    && icon_button(
                        ui,
                        OsdIcon::Forward,
                        "Seek forward 10 seconds",
                        seek_available,
                        opacity,
                        |rect| observe(ControlId::SeekForward, rect),
                    )
                    .clicked()
                {
                    actions.push(PlayerControlAction::SeekRelative(10));
                }

                if !layout.tiny
                    && view.previous_track_enabled
                    && icon_button(
                        ui,
                        OsdIcon::PreviousItem,
                        "Previous (a track restarts after 3 seconds)",
                        true,
                        opacity,
                        |rect| observe(ControlId::PreviousTrack, rect),
                    )
                    .clicked()
                {
                    actions.push(PlayerControlAction::PreviousTrack);
                }

                if !layout.tiny
                    && view.next_track_enabled
                    && icon_button(ui, OsdIcon::NextItem, "Next", true, opacity, |rect| {
                        observe(ControlId::NextTrack, rect)
                    })
                    .clicked()
                {
                    actions.push(PlayerControlAction::NextTrack);
                }

                if layout.medium
                    && icon_button(
                        ui,
                        OsdIcon::Stop,
                        "Stop playback",
                        view.stop_enabled,
                        opacity,
                        |rect| observe(ControlId::Stop, rect),
                    )
                    .clicked()
                {
                    actions.push(PlayerControlAction::Stop);
                }

                if layout.roomy {
                    let (mute_icon, mute_accessible) = OsdIcon::for_mute(view.muted);
                    if icon_button(ui, mute_icon, mute_accessible, true, opacity, |rect| {
                        observe(ControlId::MuteToggle, rect);
                    })
                    .clicked()
                    {
                        actions.push(PlayerControlAction::ToggleMute);
                    }

                    if let Some(percent) = view.volume_percent {
                        let max = f64::from(view.volume_max_percent.max(1));
                        let mut value = percent.clamp(0.0, max);
                        ui.visuals_mut().widgets.inactive.bg_fill = faded(OSD_TRACK, opacity);
                        ui.visuals_mut().selection.bg_fill = faded(OSD_ACCENT, opacity);
                        ui.spacing_mut().slider_width = 64.0;
                        ui.spacing_mut().slider_rail_height = 3.0;
                        let response = ui.add(
                            egui::Slider::new(&mut value, 0.0..=max)
                                .show_value(false)
                                .clamping(egui::SliderClamping::Always),
                        );
                        response.widget_info(|| WidgetInfo::labeled(Role::Slider, true, "Volume"));
                        observe(ControlId::VolumeSlider, response.rect);
                        if response.changed() {
                            actions.push(PlayerControlAction::SetVolume(value.round() as u32));
                        }
                    }
                }

                if let Some((elapsed, total)) = timeline
                    && layout.roomy
                {
                    ui.add_space(4.0);
                    draw_time_label(ui, &format_time(elapsed), opacity, true);
                    draw_time_label(ui, &format!("/ {}", format_time(total)), opacity, false);
                }

                if let Some(decision) = offered.as_ref() {
                    let label = skip_label(decision.kind);
                    if labeled_button(ui, label, label, true, opacity, |rect| {
                        observe(ControlId::SkipSegment, rect);
                    }) {
                        if let Some(controls) = skip.as_mut() {
                            controls.skipper.mark_skipped(&decision.segment_id);
                        }
                        actions.push(PlayerControlAction::SeekAbsolute(ticks_to_duration(
                            decision.seek_to_ticks,
                        )));
                    }
                }

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.spacing_mut().item_spacing = egui::vec2(spacing, 4.0);

                    if layout.compact {
                        let (fullscreen_icon, fullscreen_accessible) =
                            fullscreen_icon_label(view.fullscreen);
                        if icon_button(
                            ui,
                            fullscreen_icon,
                            fullscreen_accessible,
                            true,
                            opacity,
                            |rect| observe(ControlId::Fullscreen, rect),
                        )
                        .clicked()
                        {
                            actions.push(PlayerControlAction::ToggleFullscreen);
                        }

                        let (pip_icon, pip_accessible) = if view.pip_active {
                            (OsdIcon::Pip, "Exit picture-in-picture")
                        } else {
                            (OsdIcon::Pip, "Picture-in-picture")
                        };
                        if icon_button(
                            ui,
                            pip_icon,
                            pip_accessible,
                            view.pip_active || view.pip_available,
                            opacity,
                            |rect| observe(ControlId::PipToggle, rect),
                        )
                        .clicked()
                        {
                            actions.push(PlayerControlAction::TogglePip);
                        }
                    }

                    let video = popover_icon_button(
                        ui,
                        PlaybackPopover::Video,
                        OsdIcon::Video,
                        button_size,
                        opacity,
                        |rect| observe(PlaybackPopover::Video.control(), rect),
                    );
                    if let Some(action) = playback_popover(
                        ui,
                        &video,
                        PlaybackPopover::Video,
                        layout,
                        opacity,
                        backdrop,
                        |ui, opening| video_popover_contents(ui, &view, opening, opacity),
                    ) {
                        actions.push(action);
                    }

                    let subtitles = popover_icon_button(
                        ui,
                        PlaybackPopover::Subtitles,
                        OsdIcon::Subtitles,
                        button_size,
                        opacity,
                        |rect| observe(PlaybackPopover::Subtitles.control(), rect),
                    );
                    if let Some(action) = playback_popover(
                        ui,
                        &subtitles,
                        PlaybackPopover::Subtitles,
                        layout,
                        opacity,
                        backdrop,
                        |ui, opening| {
                            subtitles_popover_contents(ui, &view, opening, opacity, extra)
                        },
                    ) {
                        actions.push(action);
                    }

                    let audio = popover_icon_button(
                        ui,
                        PlaybackPopover::Audio,
                        OsdIcon::Audio,
                        button_size,
                        opacity,
                        |rect| observe(PlaybackPopover::Audio.control(), rect),
                    );
                    if let Some(action) = playback_popover(
                        ui,
                        &audio,
                        PlaybackPopover::Audio,
                        layout,
                        opacity,
                        backdrop,
                        |ui, opening| audio_popover_contents(ui, &view, opening, opacity),
                    ) {
                        actions.push(action);
                    }

                    let speed_text = speed_button_label(
                        view.speed_multiplier
                            .filter(|s| s.is_finite() && *s > 0.0)
                            .unwrap_or(1.0),
                    );
                    let speed = speed_pill_button(ui, &speed_text, button_size, opacity, |rect| {
                        observe(PlaybackPopover::Speed.control(), rect)
                    });
                    if let Some(action) = playback_popover(
                        ui,
                        &speed,
                        PlaybackPopover::Speed,
                        layout,
                        opacity,
                        backdrop,
                        |ui, opening| speed_popover_contents(ui, &view, opening, opacity),
                    ) {
                        actions.push(action);
                    }

                    if playback_overflow_available(layout) {
                        let more = popover_icon_button(
                            ui,
                            PlaybackPopover::Playback,
                            OsdIcon::Settings,
                            button_size,
                            opacity,
                            |rect| observe(PlaybackPopover::Playback.control(), rect),
                        );
                        if let Some(action) = playback_popover(
                            ui,
                            &more,
                            PlaybackPopover::Playback,
                            layout,
                            opacity,
                            backdrop,
                            |ui, opening| {
                                settings_heading(ui, "Playback", opacity);
                                let mut first_row = true;
                                let mut chosen = None;
                                settings_playback_rows(
                                    ui,
                                    &view,
                                    layout,
                                    opacity,
                                    opening,
                                    &mut first_row,
                                    &mut chosen,
                                );
                                chosen
                            },
                        ) {
                            actions.push(action);
                        }
                    }
                });
            });
        });

    actions
}

fn draw_time_label(ui: &mut egui::Ui, text: &str, opacity: f32, primary: bool) {
    let color = if primary { OSD_TEXT } else { OSD_MUTED };
    ui.label(
        RichText::new(text)
            .monospace()
            .size(12.0)
            .color(faded(color, opacity)),
    );
}

/// An icon trigger on the right side of the control bar for one playback popover.
fn popover_icon_button(
    ui: &mut egui::Ui,
    popover: PlaybackPopover,
    icon: OsdIcon,
    size: f32,
    opacity: f32,
    observe: impl FnOnce(egui::Rect),
) -> egui::Response {
    let rounding = egui::CornerRadius::same(8);
    let (rect, response) = ui.allocate_exact_size(egui::vec2(size, size), Sense::click());
    let open = egui::Popup::is_id_open(ui.ctx(), popover.id());
    let fill = if open {
        Color32::from_white_alpha(48)
    } else if response.hovered() {
        Color32::from_white_alpha(38)
    } else {
        Color32::TRANSPARENT
    };
    ui.painter()
        .rect_filled(rect, rounding, faded(fill, opacity));
    let color = if open || response.hovered() {
        Color32::WHITE
    } else {
        OSD_TEXT
    };
    player_icons::paint(
        ui.painter(),
        icon_area(rect, 0.56),
        icon,
        faded(color, opacity),
    );
    focus_ring(ui, &response, rounding, opacity);
    observe(rect);
    response.widget_info(|| WidgetInfo::labeled(Role::Button, true, popover.label()));
    response.on_hover_text(popover.label())
}

/// Compact pill button for playback speed, always showing the numeric multiplier (e.g. `1×`).
fn speed_pill_button(
    ui: &mut egui::Ui,
    label: &str,
    height: f32,
    opacity: f32,
    observe: impl FnOnce(egui::Rect),
) -> egui::Response {
    let open = egui::Popup::is_id_open(ui.ctx(), PlaybackPopover::Speed.id());
    let compact = height < ICON_BUTTON;
    let font_size = if compact { 11.0 } else { 12.0 };
    let horizontal_padding = if compact { 5.0 } else { 8.0 };
    let color = if open { Color32::WHITE } else { OSD_TEXT };
    let galley = ui.painter().layout_no_wrap(
        label.to_owned(),
        egui::FontId::proportional(font_size),
        faded(color, opacity),
    );
    let width = (galley.size().x + 2.0 * horizontal_padding).max(height);
    let (rect, response) = ui.allocate_exact_size(egui::vec2(width, height), Sense::click());
    let fill = if open {
        Color32::from_white_alpha(50)
    } else if response.hovered() {
        Color32::from_white_alpha(38)
    } else {
        Color32::from_white_alpha(20)
    };
    let rounding = egui::CornerRadius::same((height / 2.0).round() as u8);
    ui.painter()
        .rect_filled(rect, rounding, faded(fill, opacity));
    ui.painter().rect_stroke(
        rect,
        rounding,
        egui::Stroke::new(
            1.0,
            faded(
                Color32::from_white_alpha(if open { 58 } else { 30 }),
                opacity,
            ),
        ),
        egui::StrokeKind::Inside,
    );
    ui.painter().galley(
        egui::pos2(
            rect.center().x - galley.size().x / 2.0,
            rect.center().y - galley.size().y / 2.0,
        ),
        galley,
        faded(color, opacity),
    );
    focus_ring(ui, &response, rounding, opacity);
    observe(rect);
    response.widget_info(|| WidgetInfo::labeled(Role::Button, true, "Speed"));
    response.on_hover_text("Speed")
}

/// One anchored playback surface. Each group owns its popup id, so opening one closes the others.
fn playback_popover(
    ui: &mut egui::Ui,
    anchor: &egui::Response,
    popover: PlaybackPopover,
    layout: ChromeLayout,
    opacity: f32,
    backdrop: Option<BackdropBlur>,
    content: impl FnOnce(&mut egui::Ui, bool) -> Option<PlayerControlAction>,
) -> Option<PlayerControlAction> {
    let popup_id = popover.id();
    let was_open_id = popup_id.with("was-open");
    let was_open = ui
        .ctx()
        .data(|data| data.get_temp::<bool>(was_open_id))
        .unwrap_or(false);
    let opening = anchor.clicked() && !was_open;
    let max_width = if layout.tiny { 260.0 } else { POPOVER_WIDTH };
    let width = (ui.ctx().content_rect().width() - 24.0)
        .max(80.0)
        .min(max_width);
    let screen_height = ui.ctx().content_rect().height();
    let max_height = (screen_height * 0.62)
        .max(120.0)
        .min((screen_height - 24.0).max(80.0));
    let mut chosen = None;
    egui::Popup::menu(anchor)
        .id(popup_id)
        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
        .align(egui::RectAlign::TOP_END)
        .align_alternatives(&[egui::RectAlign::TOP_START, egui::RectAlign::TOP])
        .gap(8.0)
        .width(width)
        .frame(egui::Frame::NONE)
        .show(|ui| {
            show_frosted_glass_frame(
                ui,
                backdrop,
                opacity.max(0.94),
                egui::Margin::same(8),
                |ui| {
                    ui.set_max_width(width - 16.0);
                    egui::ScrollArea::vertical()
                        .max_height(max_height)
                        .auto_shrink([false, true])
                        .show(ui, |ui| {
                            chosen = content(ui, opening);
                        });
                },
            );
        });
    let open_now = egui::Popup::is_id_open(ui.ctx(), popup_id);
    ui.ctx()
        .data_mut(|data| data.insert_temp(was_open_id, open_now));
    chosen
}

fn speed_popover_contents(
    ui: &mut egui::Ui,
    view: &PlayerControlsView<'_>,
    opening: bool,
    opacity: f32,
) -> Option<PlayerControlAction> {
    settings_heading(ui, "Speed", opacity);
    if !view.speed_enabled {
        settings_disabled_note(ui, "Playback speed", SYNCED_SPEED_TOOLTIP, opacity, opening);
        return None;
    }

    let mut chosen = None;
    let mut first_row = true;
    for speed in PlaybackSpeed::ALL {
        let selected = view
            .speed_multiplier
            .is_some_and(|value| (value - speed.multiplier()).abs() < 0.001);
        if settings_row(
            ui,
            &speed_label(Some(speed.multiplier())),
            selected,
            true,
            opacity,
            opening && first_row,
        ) {
            chosen = Some(PlayerControlAction::SetSpeed(speed));
        }
        first_row = false;
    }
    chosen
}

fn audio_popover_contents(
    ui: &mut egui::Ui,
    view: &PlayerControlsView<'_>,
    opening: bool,
    opacity: f32,
) -> Option<PlayerControlAction> {
    settings_heading(ui, "Audio", opacity);
    let Some(tracks) = view.audio_tracks else {
        settings_disabled_note(ui, "Audio track", "Loading audio tracks…", opacity, opening);
        return None;
    };
    if tracks.is_empty() {
        settings_disabled_note(
            ui,
            "Audio track",
            "No audio tracks available",
            opacity,
            opening,
        );
        return None;
    }

    let mut chosen = None;
    let mut first_row = true;
    for choice in tracks {
        if settings_row(
            ui,
            &choice.label,
            choice.selected,
            true,
            opacity,
            opening && first_row,
        ) {
            chosen = Some(PlayerControlAction::SelectAudioTrack(choice.value));
        }
        first_row = false;
    }
    chosen
}

fn subtitles_popover_contents(
    ui: &mut egui::Ui,
    view: &PlayerControlsView<'_>,
    opening: bool,
    opacity: f32,
    extra: Option<impl FnOnce(&mut egui::Ui)>,
) -> Option<PlayerControlAction> {
    settings_heading(ui, "Subtitle track", opacity);
    let mut chosen = None;
    let mut first_row = true;
    if let Some(tracks) = view.burn_in_subtitle_tracks {
        for choice in tracks {
            if settings_row(
                ui,
                &choice.label,
                choice.selected,
                true,
                opacity,
                opening && first_row,
            ) {
                chosen = Some(PlayerControlAction::SelectBurnInSubtitle(choice.value));
            }
            first_row = false;
        }
    } else if let Some(tracks) = view.subtitle_tracks {
        for choice in tracks {
            if settings_row(
                ui,
                &choice.label,
                choice.selected,
                true,
                opacity,
                opening && first_row,
            ) {
                chosen = Some(PlayerControlAction::SelectSubtitleTrack(choice.value));
            }
            first_row = false;
        }
    } else {
        settings_disabled_note(
            ui,
            "Subtitle track",
            "Loading subtitle tracks…",
            opacity,
            opening && first_row,
        );
        first_row = false;
    }
    if first_row
        && !subtitles_available(view)
        && (view.burn_in_subtitle_tracks.is_some() || view.subtitle_tracks.is_some())
    {
        settings_disabled_note(
            ui,
            "Subtitle track",
            "No subtitle tracks available",
            opacity,
            opening,
        );
    }

    if !view.secondary_subtitles.is_empty() {
        settings_heading(ui, "Secondary subtitles", opacity);
        if settings_row(
            ui,
            "Off",
            view.secondary_selected.is_none(),
            true,
            opacity,
            opening && first_row,
        ) {
            chosen = Some(PlayerControlAction::SelectSecondarySubtitle(None));
        }
        first_row = false;
        for choice in view.secondary_subtitles {
            let selected = Some(choice.track) == view.secondary_selected;
            if settings_row(
                ui,
                &choice.title,
                selected,
                true,
                opacity,
                opening && first_row,
            ) && !selected
            {
                chosen = Some(PlayerControlAction::SelectSecondarySubtitle(Some(
                    choice.track,
                )));
            }
            first_row = false;
        }
    }

    if let Some(extra) = extra {
        ui.add_space(4.0);
        ui.separator();
        extra(ui);
    }
    chosen
}

fn video_popover_contents(
    ui: &mut egui::Ui,
    view: &PlayerControlsView<'_>,
    opening: bool,
    opacity: f32,
) -> Option<PlayerControlAction> {
    settings_heading(ui, "Aspect ratio", opacity);
    let mut chosen = None;
    let mut first_row = true;
    for mode in [
        AspectMode::Normal,
        AspectMode::Force16x9,
        AspectMode::Force4x3,
        AspectMode::Zoom,
        AspectMode::Stretch,
    ] {
        let selected = view.aspect_label == mode.label();
        if settings_row(
            ui,
            mode.label(),
            selected,
            true,
            opacity,
            opening && first_row,
        ) {
            chosen = Some(PlayerControlAction::SetAspect(mode));
        }
        first_row = false;
    }
    ui.add_space(4.0);
    ui.separator();
    if settings_row(ui, "Media info", false, true, opacity, false) {
        chosen = Some(PlayerControlAction::ToggleMediaInfo);
    }
    chosen
}

fn settings_playback_rows(
    ui: &mut egui::Ui,
    view: &PlayerControlsView<'_>,
    layout: ChromeLayout,
    opacity: f32,
    opening: bool,
    first_row: &mut bool,
    chosen: &mut Option<PlayerControlAction>,
) {
    let seek_available = view.seek_enabled && view.duration.is_some_and(|d| !d.is_zero());
    if layout.tiny {
        if seek_available {
            if settings_row(
                ui,
                "Seek backward 10 seconds",
                false,
                true,
                opacity,
                opening && *first_row,
            ) {
                *chosen = Some(PlayerControlAction::SeekRelative(-10));
            }
            *first_row = false;
            if settings_row(
                ui,
                "Seek forward 10 seconds",
                false,
                true,
                opacity,
                opening && *first_row,
            ) {
                *chosen = Some(PlayerControlAction::SeekRelative(10));
            }
            *first_row = false;
        }
        if view.previous_track_enabled {
            if settings_row(ui, "Previous", false, true, opacity, opening && *first_row) {
                *chosen = Some(PlayerControlAction::PreviousTrack);
            }
            *first_row = false;
        }
        if view.next_track_enabled {
            if settings_row(ui, "Next", false, true, opacity, opening && *first_row) {
                *chosen = Some(PlayerControlAction::NextTrack);
            }
            *first_row = false;
        }
    }
    if !layout.medium {
        if settings_row(
            ui,
            "Stop playback",
            false,
            view.stop_enabled,
            opacity,
            opening && *first_row,
        ) {
            *chosen = Some(PlayerControlAction::Stop);
        }
        *first_row = false;
    }
    if !layout.roomy {
        let (_, mute_accessible) = OsdIcon::for_mute(view.muted);
        if settings_row(
            ui,
            mute_accessible,
            false,
            true,
            opacity,
            opening && *first_row,
        ) {
            *chosen = Some(PlayerControlAction::ToggleMute);
        }
        *first_row = false;
    }
    if let Some(percent) = view.volume_percent {
        let max = f64::from(view.volume_max_percent.max(1));
        let bounded = percent.clamp(0.0, max);
        if settings_row(
            ui,
            &format!("Volume up · {:.0}%", bounded),
            false,
            bounded < max,
            opacity,
            opening && *first_row,
        ) {
            *chosen = Some(PlayerControlAction::SetVolume(
                (bounded + 5.0).min(max).round() as u32,
            ));
        }
        *first_row = false;
        if settings_row(
            ui,
            &format!("Volume down · {:.0}%", bounded),
            false,
            bounded > 0.0,
            opacity,
            opening && *first_row,
        ) {
            *chosen = Some(PlayerControlAction::SetVolume(
                (bounded - 5.0).max(0.0).round() as u32,
            ));
        }
        *first_row = false;
    }
    if seek_available {
        if settings_row(
            ui,
            "Step back one frame",
            false,
            true,
            opacity,
            opening && *first_row,
        ) {
            *chosen = Some(PlayerControlAction::StepFrame { backward: true });
        }
        *first_row = false;
        if settings_row(
            ui,
            "Step forward one frame",
            false,
            true,
            opacity,
            opening && *first_row,
        ) {
            *chosen = Some(PlayerControlAction::StepFrame { backward: false });
        }
        *first_row = false;
    }
    if !view.chapters.is_empty() && seek_available {
        if settings_row(
            ui,
            "Previous chapter",
            false,
            true,
            opacity,
            opening && *first_row,
        ) {
            *chosen = Some(PlayerControlAction::StepChapter { forward: false });
        }
        *first_row = false;
        if settings_row(
            ui,
            "Next chapter",
            false,
            true,
            opacity,
            opening && *first_row,
        ) {
            *chosen = Some(PlayerControlAction::StepChapter { forward: true });
        }
        *first_row = false;
    }
    if !layout.compact {
        let (_, pip_accessible) = if view.pip_active {
            (OsdIcon::Pip, "Exit picture-in-picture")
        } else {
            (OsdIcon::Pip, "Picture-in-picture")
        };
        if settings_row(
            ui,
            pip_accessible,
            false,
            view.pip_active || view.pip_available,
            opacity,
            opening && *first_row,
        ) {
            *chosen = Some(PlayerControlAction::TogglePip);
        }
        *first_row = false;
        let (_, fullscreen_accessible) = fullscreen_icon_label(view.fullscreen);
        if settings_row(
            ui,
            fullscreen_accessible,
            false,
            true,
            opacity,
            opening && *first_row,
        ) {
            *chosen = Some(PlayerControlAction::ToggleFullscreen);
        }
        *first_row = false;
    }
}

fn settings_heading(ui: &mut egui::Ui, label: &str, opacity: f32) {
    ui.add_space(6.0);
    ui.label(
        RichText::new(label)
            .size(11.0)
            .strong()
            .color(faded(OSD_MUTED, opacity)),
    );
    ui.add_space(2.0);
}

fn settings_row(
    ui: &mut egui::Ui,
    label: &str,
    selected: bool,
    enabled: bool,
    opacity: f32,
    focus: bool,
) -> bool {
    let width = ui.available_width().max(1.0);
    let (rect, response) = ui
        .add_enabled_ui(enabled, |ui| {
            ui.allocate_exact_size(egui::vec2(width, 30.0), Sense::click())
        })
        .inner;
    if focus {
        response.request_focus();
    }
    let hover = response.hovered() && enabled;
    let fill = if selected {
        Color32::from_white_alpha(24)
    } else if hover {
        Color32::from_white_alpha(14)
    } else {
        Color32::TRANSPARENT
    };
    ui.painter()
        .rect_filled(rect, egui::CornerRadius::same(8), faded(fill, opacity));
    if selected {
        player_icons::paint(
            ui.painter(),
            egui::Rect::from_center_size(
                egui::pos2(rect.left() + 14.0, rect.center().y),
                egui::vec2(14.0, 14.0),
            ),
            OsdIcon::Check,
            faded(OSD_ACCENT, opacity),
        );
    }
    let color = if enabled { OSD_TEXT } else { OSD_MUTED };
    let text_left = rect.left() + 28.0;
    let text_width = (rect.right() - 8.0 - text_left).max(1.0);
    let display_label = ellipsized_label(
        ui,
        label,
        text_width,
        egui::FontId::proportional(13.0),
        faded(color, opacity),
    );
    ui.painter().text(
        egui::pos2(text_left, rect.center().y),
        egui::Align2::LEFT_CENTER,
        display_label,
        egui::FontId::proportional(13.0),
        faded(color, opacity),
    );
    if response.has_focus() {
        ui.painter().rect_stroke(
            rect,
            egui::CornerRadius::same(8),
            egui::Stroke::new(2.0, faded(OSD_FOCUS, opacity)),
            egui::StrokeKind::Inside,
        );
    }
    response.widget_info(|| WidgetInfo::selected(Role::RadioButton, enabled, selected, label));
    response.clicked()
}

fn settings_disabled_note(ui: &mut egui::Ui, label: &str, note: &str, opacity: f32, focus: bool) {
    let width = ui.available_width().max(1.0);
    // A disabled clickable response still registers an accessibility node, so the note is
    // announced and explains why speed cannot change.
    let (rect, response) = ui
        .add_enabled_ui(false, |ui| {
            ui.allocate_exact_size(egui::vec2(width, 42.0), Sense::click())
        })
        .inner;
    if focus {
        response.request_focus();
    }
    let text_width = (width - 16.0).max(1.0);
    let display_label = ellipsized_label(
        ui,
        label,
        text_width,
        egui::FontId::proportional(13.0),
        faded(OSD_MUTED, opacity),
    );
    let display_note = ellipsized_label(
        ui,
        note,
        text_width,
        egui::FontId::proportional(11.0),
        faded(Color32::from_white_alpha(150), opacity),
    );
    ui.painter().text(
        egui::pos2(rect.left() + 8.0, rect.center().y - 8.0),
        egui::Align2::LEFT_CENTER,
        display_label,
        egui::FontId::proportional(13.0),
        faded(OSD_MUTED, opacity),
    );
    ui.painter().text(
        egui::pos2(rect.left() + 8.0, rect.center().y + 9.0),
        egui::Align2::LEFT_CENTER,
        display_note,
        egui::FontId::proportional(11.0),
        faded(Color32::from_white_alpha(150), opacity),
    );
    response.widget_info(|| WidgetInfo::labeled(Role::Button, false, label));
    let _ = response
        .ctx
        .accesskit_node_builder(response.id, |node| node.set_description(note));
}

/// A widget in the audio and subtitle section (M2-18). Entries are observed only while their
/// combo's popup is open.
#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TrackControl {
    Header,
    Audio,
    Subtitle,
    AudioEntry(usize),
    SubtitleEntry(usize),
}

/// Draw audio and subtitle track menus without a collapsible header.
///
/// The returned values are selections made in this frame. Full track names remain in each menu;
/// the selected value is compacted to keep the labeled controls within narrow panels. The player
/// overlay uses separate group popovers; this helper remains under test for the shared combo path.
#[cfg(test)]
pub fn track_menu_contents<A: Copy, S: Copy>(
    ui: &mut egui::Ui,
    audio: Option<&[TrackChoice<A>]>,
    subtitle: Option<&[TrackChoice<S>]>,
    feedback: Option<&str>,
) -> (Option<A>, Option<S>) {
    track_menu_contents_inner(ui, audio, subtitle, feedback, |_, _| {})
}

#[cfg(test)]
fn track_section_inner<A: Copy, S: Copy>(
    ui: &mut egui::Ui,
    audio: Option<&[TrackChoice<A>]>,
    subtitle: Option<&[TrackChoice<S>]>,
    feedback: Option<&str>,
    mut observe: impl FnMut(TrackControl, egui::Id),
) -> (Option<A>, Option<S>) {
    let section = egui::CollapsingHeader::new("Audio & subtitle tracks")
        .default_open(false)
        .show(ui, |ui| {
            track_menu_contents_inner(ui, audio, subtitle, feedback, &mut observe)
        });
    observe(TrackControl::Header, section.header_response.id);
    section.body_returned.unwrap_or((None, None))
}

#[cfg(test)]
fn track_menu_contents_inner<A: Copy, S: Copy>(
    ui: &mut egui::Ui,
    audio: Option<&[TrackChoice<A>]>,
    subtitle: Option<&[TrackChoice<S>]>,
    feedback: Option<&str>,
    mut observe: impl FnMut(TrackControl, egui::Id),
) -> (Option<A>, Option<S>) {
    let mut chosen: (Option<A>, Option<S>) = (None, None);
    if let Some(choices) = audio {
        chosen.0 = track_combo(
            ui,
            "Audio track",
            choices,
            (TrackControl::Audio, TrackControl::AudioEntry),
            &mut observe,
        );
    }
    if let Some(choices) = subtitle {
        chosen.1 = track_combo(
            ui,
            "Subtitle track",
            choices,
            (TrackControl::Subtitle, TrackControl::SubtitleEntry),
            &mut observe,
        );
    }
    if let Some(feedback) = feedback {
        ui.label(feedback);
    }
    if audio.is_none() && subtitle.is_none() && feedback.is_none() {
        ui.label("Loading audio and subtitle tracks…");
    }
    chosen
}

/// One track combo. `controls` names the combo and makes the id of each entry from its index.
/// The combo is a `ComboBox`, as the speed and chapter menus are, so it takes the same keys.
#[cfg(test)]
fn track_combo<T: Copy>(
    ui: &mut egui::Ui,
    label: &str,
    choices: &[TrackChoice<T>],
    controls: (TrackControl, fn(usize) -> TrackControl),
    observe: &mut impl FnMut(TrackControl, egui::Id),
) -> Option<T> {
    let selected = choices.iter().find(|choice| choice.selected).map_or_else(
        || "Unknown selection".to_owned(),
        |choice| compact_label(&choice.label, 24),
    );
    let (combo, entry_of) = controls;
    let mut picked = None;
    let label_width = ui
        .painter()
        .layout_no_wrap(
            label.to_owned(),
            egui::TextStyle::Body.resolve(ui.style()),
            ui.visuals().text_color(),
        )
        .size()
        .x;
    let combo_width =
        (ui.available_width() - label_width - ui.spacing().item_spacing.x - 8.0).clamp(1.0, 280.0);
    let response = egui::ComboBox::from_label(label)
        .width(combo_width)
        .selected_text(selected)
        .show_ui(ui, |ui| {
            for (index, choice) in choices.iter().enumerate() {
                let response = ui.selectable_label(choice.selected, &choice.label);
                observe(entry_of(index), response.id);
                if response.clicked() {
                    picked = Some(choice.value);
                }
            }
        });
    observe(combo, response.response.id);
    picked
}

/// True when the frame has Ctrl+S pressed, the default `host:screenshot` binding in kattri-input.
/// Repeats are ignored so a held key takes one picture. The host does not depend on kattri-input
/// yet, so the default is repeated here; keep it equal to that crate's legacy map.
pub fn screenshot_key_pressed(events: &[egui::Event]) -> bool {
    events.iter().any(|event| {
        matches!(
            event,
            egui::Event::Key {
                key: egui::Key::S,
                pressed: true,
                repeat: false,
                modifiers,
                ..
            } if modifiers.ctrl && !modifiers.shift && !modifiers.alt && !modifiers.mac_cmd
        )
    })
}

/// The player action for a Ctrl shortcut in one frame of input, if one was pressed.
///
/// Ctrl+1 asks for a 1:1 window, Ctrl+2 toggles normal and double speed, Ctrl+Up / Ctrl+Down step
/// the playback rate, Ctrl+0 returns it to normal, and Ctrl+A cycles the aspect ratio. Repeats are
/// ignored so a held key fires once. Like [`screenshot_key_pressed`], the caller must skip this
/// while a text field has focus. The host does not depend on kattri-input yet, so these defaults
/// are repeated here; keep them equal to that crate's player map (SpeedUp/SpeedDown are Ctrl+Up /
/// Ctrl+Down there).
pub fn player_shortcut_action(events: &[egui::Event]) -> Option<PlayerControlAction> {
    events.iter().find_map(|event| {
        let egui::Event::Key {
            key,
            pressed: true,
            repeat: false,
            modifiers,
            ..
        } = event
        else {
            return None;
        };
        if !modifiers.ctrl || modifiers.shift || modifiers.alt || modifiers.mac_cmd {
            return None;
        }
        match key {
            egui::Key::Num1 => Some(PlayerControlAction::FitWindowToVideo),
            egui::Key::Num2 => Some(PlayerControlAction::ToggleDoubleSpeed),
            egui::Key::Num0 => Some(PlayerControlAction::ResetSpeed),
            egui::Key::ArrowUp => Some(PlayerControlAction::StepSpeed { up: true }),
            egui::Key::ArrowDown => Some(PlayerControlAction::StepSpeed { up: false }),
            egui::Key::A => Some(PlayerControlAction::CycleAspect),
            _ => None,
        }
    })
}

/// Handle one unmodified arrow press only when egui has no focused widget or popup.
/// This leaves text editing, focused controls, and menu navigation to egui.
fn keyboard_seek_actions(ctx: &egui::Context, seek_available: bool) -> Vec<PlayerControlAction> {
    if !seek_available || ctx.egui_wants_keyboard_input() || egui::Popup::is_any_open(ctx) {
        return Vec::new();
    }

    let mut actions = Vec::with_capacity(1);
    ctx.input_mut(|input| {
        let unmodified_press = |key| {
            input.events.iter().any(|event| {
                matches!(
                    event,
                    egui::Event::Key {
                        key: event_key,
                        pressed: true,
                        modifiers,
                        ..
                    } if *event_key == key && *modifiers == egui::Modifiers::NONE
                )
            })
        };
        let left_pressed = unmodified_press(egui::Key::ArrowLeft);
        let right_pressed = unmodified_press(egui::Key::ArrowRight);
        if left_pressed && input.consume_key(egui::Modifiers::NONE, egui::Key::ArrowLeft) {
            actions.push(PlayerControlAction::SeekRelative(-10));
        } else if right_pressed && input.consume_key(egui::Modifiers::NONE, egui::Key::ArrowRight) {
            actions.push(PlayerControlAction::SeekRelative(10));
        }
    });
    actions
}

/// A white pill for the skip action. It is the only filled label on the row, and it is the one
/// action that must never be missed.
fn labeled_button(
    ui: &mut egui::Ui,
    label: &str,
    accessible_name: &str,
    enabled: bool,
    opacity: f32,
    observe: impl FnOnce(egui::Rect),
) -> bool {
    let text_color = faded(Color32::from_rgb(14, 15, 16), opacity);
    let galley = ui.painter().layout_no_wrap(
        label.to_owned(),
        egui::FontId::proportional(13.0),
        text_color,
    );
    let size = egui::vec2((galley.size().x + 28.0).max(70.0), 32.0);
    let (rect, response) = ui
        .add_enabled_ui(enabled, |ui| ui.allocate_exact_size(size, Sense::click()))
        .inner;
    let fill = if response.hovered() {
        Color32::from_white_alpha(250)
    } else {
        Color32::from_white_alpha(226)
    };
    ui.painter()
        .rect_filled(rect, egui::CornerRadius::same(9), faded(fill, opacity));
    ui.painter().galley(
        egui::pos2(
            rect.center().x - galley.size().x / 2.0,
            rect.center().y - galley.size().y / 2.0,
        ),
        galley,
        text_color,
    );
    focus_ring(ui, &response, egui::CornerRadius::same(9), opacity);
    observe(rect);
    response.widget_info(|| WidgetInfo::labeled(Role::Button, enabled, accessible_name));
    response.clicked()
}

/// An icon-only button painted by the chrome rather than by egui's button visuals, so the whole
/// row can fade as one surface. A hover wash is the only resting decoration; the accessible name
/// carries the meaning for screen readers and the tooltip.
fn icon_button(
    ui: &mut egui::Ui,
    icon: OsdIcon,
    accessible_name: &str,
    enabled: bool,
    opacity: f32,
    observe: impl FnOnce(egui::Rect),
) -> egui::Response {
    let rounding = egui::CornerRadius::same(8);
    let (rect, response) = ui
        .add_enabled_ui(enabled, |ui| {
            ui.allocate_exact_size(egui::vec2(ICON_BUTTON, ICON_BUTTON), Sense::click())
        })
        .inner;
    let hover = response.hovered() && enabled;
    let fill = if hover {
        Color32::from_white_alpha(38)
    } else {
        Color32::TRANSPARENT
    };
    ui.painter()
        .rect_filled(rect, rounding, faded(fill, opacity));
    let color = if !enabled {
        OSD_MUTED
    } else if hover {
        Color32::WHITE
    } else {
        OSD_TEXT
    };
    player_icons::paint(
        ui.painter(),
        icon_area(rect, 0.54),
        icon,
        faded(color, opacity),
    );
    focus_ring(ui, &response, rounding, opacity);
    observe(rect);
    response.widget_info(|| WidgetInfo::labeled(Role::Button, enabled, accessible_name));
    response.on_hover_text(accessible_name)
}

/// The play/pause button. It is the one control with a filled circle, so the eye lands on it.
fn primary_icon_button(
    ui: &mut egui::Ui,
    icon: OsdIcon,
    accessible_name: &str,
    enabled: bool,
    size: f32,
    opacity: f32,
    observe: impl FnOnce(egui::Rect),
) -> egui::Response {
    let rounding = egui::CornerRadius::same((size / 2.0).round() as u8);
    let (rect, response) = ui
        .add_enabled_ui(enabled, |ui| {
            ui.allocate_exact_size(egui::vec2(size, size), Sense::click())
        })
        .inner;
    let fill = if response.hovered() {
        Color32::from_white_alpha(58)
    } else {
        Color32::from_white_alpha(30)
    };
    ui.painter()
        .rect_filled(rect, rounding, faded(fill, opacity));
    let color = if enabled { Color32::WHITE } else { OSD_MUTED };
    player_icons::paint(
        ui.painter(),
        icon_area(rect, 0.48),
        icon,
        faded(color, opacity),
    );
    focus_ring(ui, &response, rounding, opacity);
    observe(rect);
    response.widget_info(|| WidgetInfo::labeled(Role::Button, enabled, accessible_name));
    response.on_hover_text(accessible_name)
}

pub fn sync_marker_button(
    ui: &mut egui::Ui,
    icon: OsdIcon,
    label: &str,
    enabled: bool,
    opacity: f32,
    width: f32,
) -> bool {
    let rounding = egui::CornerRadius::same(8);
    let (rect, response) = ui
        .add_enabled_ui(enabled, |ui| {
            ui.allocate_exact_size(egui::vec2(width, 38.0), Sense::click())
        })
        .inner;
    let hover = response.hovered() && enabled;
    let fill = if hover {
        Color32::from_white_alpha(42)
    } else {
        Color32::from_white_alpha(18)
    };
    ui.painter()
        .rect_filled(rect, rounding, faded(fill, opacity));
    ui.painter().rect_stroke(
        rect,
        rounding,
        egui::Stroke::new(1.0, faded(Color32::from_white_alpha(44), opacity)),
        egui::StrokeKind::Inside,
    );
    let color = if !enabled {
        OSD_MUTED
    } else if hover {
        Color32::WHITE
    } else {
        OSD_TEXT
    };
    let icon_rect =
        egui::Rect::from_min_size(rect.min + egui::vec2(10.0, 7.0), egui::vec2(24.0, 24.0));
    player_icons::paint(ui.painter(), icon_rect, icon, faded(color, opacity));
    let galley = ui.painter().layout_no_wrap(
        label.to_owned(),
        egui::FontId::proportional(13.0),
        faded(color, opacity),
    );
    // The compact form is icon-only; its name stays in the hover text and accessible label.
    if width >= 120.0 {
        ui.painter().galley(
            egui::pos2(rect.min.x + 44.0, rect.center().y - galley.size().y / 2.0),
            galley,
            faded(color, opacity),
        );
    }
    focus_ring(ui, &response, rounding, opacity);
    response.widget_info(|| WidgetInfo::labeled(Role::Button, enabled, label));
    response.on_hover_text(label).clicked()
}

/// The sync marker button for one calibration marker, choosing the icon that symbolizes it:
/// an ear and sound waves for the spoken line, a caption box for the text seen.
pub fn sync_marker_button_for(
    ui: &mut egui::Ui,
    marker: CalibrationMarker,
    label: &str,
    enabled: bool,
    opacity: f32,
    width: f32,
) -> bool {
    let icon = match marker {
        CalibrationMarker::VoiceHeard => OsdIcon::SyncVoice,
        CalibrationMarker::TextSeen => OsdIcon::SyncText,
    };
    sync_marker_button(ui, icon, label, enabled, opacity, width)
}

/// A back-chevron plus label for the top overlay.
fn chrome_icon_text_button(ui: &mut egui::Ui, icon: OsdIcon, label: &str, opacity: f32) -> bool {
    let text_color = faded(OSD_TEXT, opacity);
    let galley = ui.painter().layout_no_wrap(
        label.to_owned(),
        egui::FontId::proportional(13.5),
        text_color,
    );
    let size = egui::vec2(galley.size().x + 52.0, 34.0);
    let (rect, response) = ui.allocate_exact_size(size, Sense::click());
    let fill = if response.hovered() {
        Color32::from_white_alpha(30)
    } else {
        Color32::TRANSPARENT
    };
    ui.painter()
        .rect_filled(rect, egui::CornerRadius::same(17), faded(fill, opacity));
    player_icons::paint(
        ui.painter(),
        egui::Rect::from_center_size(
            egui::pos2(rect.left() + 18.0, rect.center().y),
            egui::vec2(20.0, 20.0),
        ),
        icon,
        text_color,
    );
    ui.painter().galley(
        egui::pos2(rect.left() + 32.0, rect.center().y - galley.size().y / 2.0),
        galley,
        text_color,
    );
    focus_ring(ui, &response, egui::CornerRadius::same(17), opacity);
    response.widget_info(|| WidgetInfo::labeled(Role::Button, true, label));
    response.on_hover_text(label).clicked()
}

/// The square an icon is drawn in, centred in a button. `fraction` sets how much of the button
/// the icon covers.
fn icon_area(button: egui::Rect, fraction: f32) -> egui::Rect {
    let side = (button.width().min(button.height()) * fraction).max(8.0);
    egui::Rect::from_center_size(button.center(), egui::vec2(side, side))
}

fn focus_ring(
    ui: &egui::Ui,
    response: &egui::Response,
    rounding: egui::CornerRadius,
    opacity: f32,
) {
    if response.has_focus() {
        ui.painter().rect_stroke(
            response.rect.expand(3.0),
            rounding,
            egui::Stroke::new(2.0, faded(OSD_FOCUS, opacity)),
            egui::StrokeKind::Outside,
        );
    }
}

fn skip_label(kind: SegmentKind) -> &'static str {
    match kind {
        SegmentKind::Intro => "Skip intro",
        SegmentKind::Outro => "Skip outro",
    }
}

/// Jellyfin ticks are 100 ns. The conversion saturates instead of failing, which real media
/// positions never reach.
fn duration_to_ticks(time: Duration) -> u64 {
    u64::try_from(time.as_nanos() / 100).unwrap_or(u64::MAX)
}

fn ticks_to_duration(ticks: u64) -> Duration {
    Duration::from_nanos(ticks.saturating_mul(100))
}

fn speed_label(speed: Option<f64>) -> String {
    let Some(speed) = speed.filter(|value| value.is_finite() && *value > 0.0) else {
        return "Speed unavailable".into();
    };
    if let Some(choice) = PlaybackSpeed::ALL
        .into_iter()
        .find(|choice| (choice.multiplier() - speed).abs() < 0.001)
    {
        format!("{}×", choice.multiplier())
    } else {
        format!("Custom {speed:.2}×")
    }
}

fn speed_button_label(speed: f64) -> String {
    if let Some(choice) = PlaybackSpeed::ALL
        .into_iter()
        .find(|choice| (choice.multiplier() - speed).abs() < 0.001)
    {
        format!("{}×", choice.multiplier())
    } else {
        format!("{speed:.2}×")
    }
}

#[cfg(test)]
fn compact_label(label: &str, max_chars: usize) -> String {
    let mut chars = label.chars();
    let compact: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{compact}…")
    } else {
        compact
    }
}

/// Fit a menu label to its actual pixel width while preserving the full accessible name.
fn ellipsized_label(
    ui: &egui::Ui,
    label: &str,
    max_width: f32,
    font: egui::FontId,
    color: Color32,
) -> String {
    let full = ui
        .painter()
        .layout_no_wrap(label.to_owned(), font.clone(), color);
    if full.size().x <= max_width {
        return label.to_owned();
    }
    let ellipsis = ui
        .painter()
        .layout_no_wrap("…".to_owned(), font.clone(), color);
    if ellipsis.size().x > max_width {
        return String::new();
    }
    let available = max_width - ellipsis.size().x;
    let char_count = label.chars().count();
    let mut low = 0;
    let mut high = char_count;
    while low < high {
        let middle = low + (high - low).div_ceil(2);
        let candidate = label.chars().take(middle).collect::<String>();
        let width = ui
            .painter()
            .layout_no_wrap(candidate, font.clone(), color)
            .size()
            .x;
        if width <= available {
            low = middle;
        } else {
            high = middle - 1;
        }
    }
    format!("{}…", label.chars().take(low).collect::<String>())
}

fn format_time(time: Duration) -> String {
    let total_seconds = time.as_secs();
    let hours = total_seconds / 3600;
    let minutes = (total_seconds % 3600) / 60;
    let seconds = total_seconds % 60;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

fn transport_icon(paused: Option<bool>) -> (OsdIcon, &'static str) {
    match paused {
        Some(true) => (OsdIcon::Play, "Play"),
        Some(false) => (OsdIcon::Pause, "Pause"),
        None => (OsdIcon::Play, "Playback state unavailable"),
    }
}

/// Fullscreen control icon and accessible name. The control offers the state it switches to.
fn fullscreen_icon_label(fullscreen: bool) -> (OsdIcon, &'static str) {
    if fullscreen {
        (OsdIcon::ExitFullscreen, "Leave fullscreen")
    } else {
        (OsdIcon::EnterFullscreen, "Enter fullscreen")
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ChapterChoice, ControlId, GamepadPlayerAction, MAX_CHAPTER_CHOICES, PlaybackPopover,
        PlayerControlAction, PlayerControlsView, SecondarySubtitleChoice, SkipControls,
        TopChromeAction, TopChromeOutcome, TopChromeView, TrackChoice, TrackControl, advance_fade,
        auto_skip_action, center_play_visible, chrome_layout, compact_label, draw, draw_inner,
        draw_inner_with_skip, draw_top_chrome_outcome, draw_with_skip, duration_to_ticks,
        fade_seconds, format_time, gamepad_player_action, keyboard_seek_actions,
        playback_overflow_available, player_controls_visible, player_shortcut_action,
        position_at_fraction, screenshot_key_pressed, seek_fraction, skip_settings, speed_label,
        subtitles_available, ticks_to_duration, timeline_fraction, track_menu_contents,
        track_section_inner,
    };
    use crate::player_icons::OsdIcon;

    #[test]
    fn a_gamepad_drives_the_player_while_a_video_fills_the_window() {
        use kattri_client_core::focus::NavAction;
        for (pressed, expected) in [
            (NavAction::Select, GamepadPlayerAction::ActivateFocused),
            (
                NavAction::Up,
                GamepadPlayerAction::Navigate(egui::Key::ArrowUp),
            ),
            (
                NavAction::Down,
                GamepadPlayerAction::Navigate(egui::Key::ArrowDown),
            ),
            (
                NavAction::Left,
                GamepadPlayerAction::Navigate(egui::Key::ArrowLeft),
            ),
            (
                NavAction::Right,
                GamepadPlayerAction::Navigate(egui::Key::ArrowRight),
            ),
            (
                NavAction::PlayPause,
                GamepadPlayerAction::Control(PlayerControlAction::TogglePause),
            ),
            (NavAction::Back, GamepadPlayerAction::Leave),
            (NavAction::Menu, GamepadPlayerAction::ToggleMediaInfo),
        ] {
            assert_eq!(gamepad_player_action(pressed), expected, "{pressed:?}");
        }
    }
    use egui::{CentralPanel, Color32, Context, Id, RawInput, Stroke};
    use kattri_client_core::segments::{MediaSegment, SegmentKind, SegmentSkipper, SkipSettings};
    use kattri_settings::PlaybackSettings;
    use std::time::Duration;

    /// One second in Jellyfin ticks.
    const SECOND: u64 = 10_000_000;

    fn view<'a>(chapters: &'a [ChapterChoice]) -> PlayerControlsView<'a> {
        PlayerControlsView {
            position: Duration::from_secs(20),
            duration: Some(Duration::from_secs(120)),
            paused: Some(true),
            speed_multiplier: Some(1.0),
            speed_enabled: true,
            aspect_label: "Normal",
            seek_enabled: true,
            stop_enabled: true,
            previous_track_enabled: true,
            next_track_enabled: true,
            chapters,
            secondary_subtitles: &[],
            secondary_selected: None,
            pip_active: false,
            pip_available: true,
            volume_percent: Some(80.0),
            volume_max_percent: 100,
            muted: Some(false),
            fullscreen: false,
            buffered: None,
            audio_tracks: None,
            subtitle_tracks: None,
            burn_in_subtitle_tracks: None,
            opacity: 1.0,
        }
    }

    fn run_ui(context: &Context, input: RawInput, run: impl FnMut(&mut egui::Ui)) {
        let mut output = context.run_ui(input, run);
        output.textures_delta.clear();
    }

    #[test]
    fn top_chrome_opens_media_info_by_click_hover_and_focus() {
        let context = Context::default();
        context.enable_accesskit();
        let view = TopChromeView {
            title: Some("Sample film"),
            subtitle: None,
            opacity: 1.0,
            media_info_open: false,
            show_media_info: true,
        };
        let mut media_info_rect = None;
        let mut output = context.run_ui(RawInput::default(), |ui| {
            CentralPanel::default().show(ui, |ui| {
                let initial = draw_top_chrome_outcome(ui, view);
                assert_eq!(initial.action, TopChromeAction::None);
                assert!(!initial.media_info_hot);
                media_info_rect = Some(initial.media_info_rect);
            });
        });
        output.textures_delta.clear();
        let tree = output
            .platform_output
            .accesskit_update
            .expect("accessibility tree is emitted");
        assert!(
            tree.nodes.iter().any(|(_, node)| {
                node.role() == egui::Role::Button && node.label() == Some("Media info")
            }),
            "the compact media-info action is accessible"
        );
        assert!(
            tree.nodes
                .iter()
                .all(|(_, node)| node.label() != Some("Player details")),
            "the removed details action is not exposed"
        );

        // Hovering the trigger marks media_info_hot across frames without re-toggling action.
        let trigger_center = media_info_rect.expect("Media info trigger").center();
        let mut outcome = TopChromeOutcome {
            action: TopChromeAction::None,
            media_info_hot: false,
            media_info_rect: egui::Rect::NOTHING,
        };
        for frame in 0..3 {
            let mut open_view = view;
            open_view.media_info_open = frame > 0;
            run_ui(
                &context,
                RawInput {
                    events: vec![egui::Event::PointerMoved(trigger_center)],
                    ..RawInput::default()
                },
                |ui| {
                    CentralPanel::default().show(ui, |ui| {
                        outcome = draw_top_chrome_outcome(ui, open_view);
                    });
                },
            );
            assert!(
                outcome.media_info_hot,
                "hover keeps details hot on frame {frame}"
            );
            assert_eq!(outcome.action, TopChromeAction::None);
        }

        // Back is first in the row; focusing Media info is the next keyboard stop.
        let context = Context::default();
        run_ui(&context, key_press(egui::Key::Tab), |ui| {
            CentralPanel::default().show(ui, |ui| {
                assert_eq!(
                    draw_top_chrome_outcome(ui, view).action,
                    TopChromeAction::None
                );
            });
        });
        run_ui(&context, key_press(egui::Key::Tab), |ui| {
            CentralPanel::default().show(ui, |ui| {
                outcome = draw_top_chrome_outcome(ui, view);
            });
        });
        assert!(outcome.media_info_hot, "focus opens details");

        // Clicking Media info emits TopChromeAction::MediaInfo once.
        let context = Context::default();
        run_ui(&context, RawInput::default(), |ui| {
            CentralPanel::default().show(ui, |ui| {
                assert_eq!(
                    draw_top_chrome_outcome(ui, view).action,
                    TopChromeAction::None
                );
            });
        });
        let mut click_rect = None;
        run_ui(&context, RawInput::default(), |ui| {
            CentralPanel::default().show(ui, |ui| {
                click_rect = Some(draw_top_chrome_outcome(ui, view).media_info_rect);
            });
        });
        let position = click_rect.expect("Media info trigger").center();
        run_ui(
            &context,
            RawInput {
                events: vec![
                    egui::Event::PointerMoved(position),
                    egui::Event::PointerButton {
                        pos: position,
                        button: egui::PointerButton::Primary,
                        pressed: true,
                        modifiers: egui::Modifiers::NONE,
                    },
                    egui::Event::PointerButton {
                        pos: position,
                        button: egui::PointerButton::Primary,
                        pressed: false,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
                ..RawInput::default()
            },
            |ui| {
                CentralPanel::default().show(ui, |ui| {
                    outcome = draw_top_chrome_outcome(ui, view);
                });
            },
        );
        assert_eq!(
            outcome.action,
            TopChromeAction::MediaInfo,
            "click pins/toggles details"
        );
    }

    #[test]
    fn top_chrome_hides_media_info_on_skeleton_and_aligns_title_without_collision() {
        let context = Context::default();
        context.enable_accesskit();
        let skeleton_view = TopChromeView {
            title: Some("Loading film"),
            subtitle: None,
            opacity: 1.0,
            media_info_open: false,
            show_media_info: false,
        };
        let mut skeleton_outcome = TopChromeOutcome {
            action: TopChromeAction::None,
            media_info_hot: false,
            media_info_rect: egui::Rect::NOTHING,
        };
        let mut output = context.run_ui(RawInput::default(), |ui| {
            CentralPanel::default().show(ui, |ui| {
                skeleton_outcome = draw_top_chrome_outcome(ui, skeleton_view);
            });
        });
        output.textures_delta.clear();
        assert_eq!(skeleton_outcome.media_info_rect, egui::Rect::NOTHING);
        assert!(!skeleton_outcome.media_info_hot);
        let tree = output
            .platform_output
            .accesskit_update
            .expect("accessibility tree is emitted");
        assert!(
            tree.nodes
                .iter()
                .all(|(_, node)| node.label() != Some("Media info")),
            "skeleton top chrome must not expose Media info"
        );

        let long_title = "A Very Long Movie Title That Would Otherwise Collide With Right Controls";
        let live_view = TopChromeView {
            title: Some(long_title),
            subtitle: None,
            opacity: 1.0,
            media_info_open: false,
            show_media_info: true,
        };
        let mut live_outcome = TopChromeOutcome {
            action: TopChromeAction::None,
            media_info_hot: false,
            media_info_rect: egui::Rect::NOTHING,
        };
        let narrow_input = RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(420.0, 240.0),
            )),
            ..RawInput::default()
        };
        let mut live_output = context.run_ui(narrow_input, |ui| {
            CentralPanel::default().show(ui, |ui| {
                live_outcome = draw_top_chrome_outcome(ui, live_view);
            });
        });
        live_output.textures_delta.clear();
        let live_tree = live_output
            .platform_output
            .accesskit_update
            .expect("live accessibility tree");
        let back_bounds = live_tree
            .nodes
            .iter()
            .find_map(|(_, node)| {
                (node.role() == egui::Role::Button && node.label() == Some("Back"))
                    .then(|| node.bounds())
                    .flatten()
            })
            .expect("Back button bounds");
        let back_center_y = ((back_bounds.y0 + back_bounds.y1) * 0.5) as f32;
        assert!(
            (back_center_y - live_outcome.media_info_rect.center().y).abs() <= 1.0,
            "Back ({back_center_y}) and Media info ({}) share the 34px centerline",
            live_outcome.media_info_rect.center().y
        );
        for clipped in &live_output.shapes {
            if let egui::Shape::Text(text) = &clipped.shape {
                let text_rect = text.visual_bounding_rect();
                if text.galley.text().starts_with("A Very Long") {
                    assert!(
                        text_rect.right() <= live_outcome.media_info_rect.left(),
                        "title ({:?}) must truncate before Media info ({:?})",
                        text_rect,
                        live_outcome.media_info_rect
                    );
                    assert!(
                        (text_rect.center().y - back_center_y).abs() <= 2.0,
                        "single-line title center ({}) shares centerline ({back_center_y})",
                        text_rect.center().y
                    );
                }
            }
        }
    }

    #[test]
    fn pip_overlay_exposes_exit_and_play_pause_actions() {
        use super::{PipOverlayAction, draw_pip_overlay};

        let context = Context::default();
        context.enable_accesskit();
        let stage = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(480.0, 270.0));
        let mut warmup = context.run_ui(
            RawInput {
                screen_rect: Some(stage),
                ..RawInput::default()
            },
            |ui| {
                assert_eq!(
                    draw_pip_overlay(ui.ctx(), stage, Some(false), 1.0),
                    PipOverlayAction::None
                );
            },
        );
        warmup.textures_delta.clear();
        let input = RawInput {
            screen_rect: Some(stage),
            ..RawInput::default()
        };
        let mut output = context.run_ui(input, |ui| {
            assert_eq!(
                draw_pip_overlay(ui.ctx(), stage, Some(false), 1.0),
                PipOverlayAction::None
            );
        });
        output.textures_delta.clear();
        let tree = output
            .platform_output
            .accesskit_update
            .expect("PiP accessibility tree");
        let exit_bounds = tree
            .nodes
            .iter()
            .find_map(|(_, node)| {
                (node.role() == egui::Role::Button
                    && node.label() == Some("Exit picture-in-picture"))
                .then(|| node.bounds())
                .flatten()
            })
            .expect("Exit picture-in-picture button");
        let pause_bounds = tree
            .nodes
            .iter()
            .find_map(|(_, node)| {
                (node.role() == egui::Role::Button && node.label() == Some("Pause"))
                    .then(|| node.bounds())
                    .flatten()
            })
            .expect("Pause button in PiP");

        let exit_center = egui::pos2(
            ((exit_bounds.x0 + exit_bounds.x1) * 0.5) as f32,
            ((exit_bounds.y0 + exit_bounds.y1) * 0.5) as f32,
        );
        let pause_center = egui::pos2(
            ((pause_bounds.x0 + pause_bounds.x1) * 0.5) as f32,
            ((pause_bounds.y0 + pause_bounds.y1) * 0.5) as f32,
        );

        let click_pip = |pos: egui::Pos2| -> PipOverlayAction {
            let mut action = PipOverlayAction::None;
            let click = RawInput {
                screen_rect: Some(stage),
                events: vec![
                    egui::Event::PointerMoved(pos),
                    egui::Event::PointerButton {
                        pos,
                        button: egui::PointerButton::Primary,
                        pressed: true,
                        modifiers: egui::Modifiers::NONE,
                    },
                    egui::Event::PointerButton {
                        pos,
                        button: egui::PointerButton::Primary,
                        pressed: false,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
                ..RawInput::default()
            };
            run_ui(&context, click, |ui| {
                action = draw_pip_overlay(ui.ctx(), stage, Some(false), 1.0);
            });
            action
        };

        assert_eq!(click_pip(exit_center), PipOverlayAction::ExitPip);
        assert_eq!(click_pip(pause_center), PipOverlayAction::TogglePaused);
    }

    #[test]
    fn chrome_fade_snaps_for_reduced_motion_and_steps_to_its_target() {
        assert_eq!(fade_seconds(0.0), 0.0, "reduced motion has no fade");
        assert_eq!(fade_seconds(0.12), super::CHROME_FADE);

        let half = advance_fade(
            0.0,
            1.0,
            Duration::from_millis(120),
            Duration::from_millis(240),
        );
        assert!(
            (half - 0.5).abs() < 0.001,
            "half of the fade is half opacity"
        );
        assert_eq!(
            advance_fade(0.5, 1.0, Duration::from_secs(1), Duration::from_millis(240)),
            1.0,
            "the fade clamps at its target"
        );
        assert_eq!(
            advance_fade(
                1.0,
                0.0,
                Duration::from_millis(120),
                Duration::from_millis(240)
            ),
            0.5
        );
        assert_eq!(
            advance_fade(0.4, 1.0, Duration::from_secs(1), Duration::ZERO),
            1.0,
            "a zero duration snaps"
        );
    }

    #[test]
    fn seek_fractions_and_chapter_positions_follow_the_timeline() {
        let duration = Some(Duration::from_secs(120));
        // The seekable end is one second before the duration, matching the bar and the host.
        assert!((seek_fraction(Duration::from_secs(30), duration) - 30.0 / 119.0).abs() < 0.001);
        assert_eq!(seek_fraction(Duration::from_secs(500), duration), 1.0);
        assert_eq!(seek_fraction(Duration::from_secs(10), None), 0.0);

        let seek_end = Duration::from_secs(119);
        assert!((position_at_fraction(0.5, seek_end).as_secs_f64() - 59.5).abs() < 0.01);
        assert_eq!(position_at_fraction(2.0, seek_end), seek_end);
        assert!(
            (timeline_fraction(Duration::from_secs(30), seek_end) - 30.0 / 119.0).abs() < 0.001
        );
        assert_eq!(
            timeline_fraction(Duration::from_secs(30), Duration::ZERO),
            0.0
        );
    }

    #[test]
    fn playback_groups_have_distinct_ids_and_controls() {
        let groups = [
            (PlaybackPopover::Speed, ControlId::SpeedMenu, "Speed"),
            (PlaybackPopover::Audio, ControlId::AudioMenu, "Audio"),
            (
                PlaybackPopover::Subtitles,
                ControlId::SubtitlesMenu,
                "Subtitles",
            ),
            (PlaybackPopover::Video, ControlId::VideoMenu, "Video"),
            (PlaybackPopover::Playback, ControlId::PlaybackMenu, "More"),
        ];
        for (index, (popover, control, label)) in groups.into_iter().enumerate() {
            assert_eq!(popover.control(), control);
            assert_eq!(popover.label(), label);
            for (other, _, _) in groups.iter().skip(index + 1) {
                assert_ne!(popover.id(), other.id(), "each group owns its popup");
            }
        }
        assert!(playback_overflow_available(chrome_layout(400.0)));
        assert!(!playback_overflow_available(chrome_layout(1000.0)));
    }

    #[test]
    fn subtitle_availability_includes_primary_burned_in_and_secondary_tracks() {
        use mpv_render_proof::TrackSelection;
        let subtitles = [TrackChoice {
            value: TrackSelection::Disabled,
            label: "Off".into(),
            selected: true,
        }];
        let burned_subtitles = [TrackChoice {
            value: Some(9),
            label: "English".into(),
            selected: true,
        }];
        let secondary = [SecondarySubtitleChoice {
            track: 3,
            title: "English".into(),
        }];
        assert!(!subtitles_available(&view(&[])));
        assert!(subtitles_available(&PlayerControlsView {
            subtitle_tracks: Some(&subtitles),
            ..view(&[])
        }));
        assert!(subtitles_available(&PlayerControlsView {
            burn_in_subtitle_tracks: Some(&burned_subtitles),
            ..view(&[])
        }));
        assert!(subtitles_available(&PlayerControlsView {
            secondary_subtitles: &secondary,
            ..view(&[])
        }));
    }

    #[test]
    fn the_chrome_auto_hides_only_when_the_timer_focus_and_menus_are_clear() {
        assert!(!player_controls_visible(false, false, false));
        assert!(player_controls_visible(true, false, false));
        assert!(player_controls_visible(false, true, false));
        assert!(player_controls_visible(false, false, true));
        assert!(player_controls_visible(false, true, true));

        assert!(center_play_visible(Some(true), true));
        assert!(!center_play_visible(Some(false), true));
        assert!(!center_play_visible(Some(true), false));
        assert!(!center_play_visible(None, true));
    }

    #[test]
    fn unknown_duration_hides_seek_bar_and_seek_buttons_are_disabled() {
        let context = Context::default();
        let mut seek_bar = None;
        run_ui(&context, RawInput::default(), |ui| {
            CentralPanel::default().show(ui, |ui| {
                let mut state = view(&[]);
                state.duration = None;
                let _ = draw_inner(ui, state, |id, rect| {
                    if id == ControlId::SeekBar {
                        seek_bar = Some(rect);
                    }
                });
            });
        });
        assert!(seek_bar.is_none());
    }

    #[test]
    fn the_transport_mute_and_fullscreen_controls_offer_the_state_they_switch_to() {
        assert_eq!(super::transport_icon(Some(true)), (OsdIcon::Play, "Play"));
        assert_eq!(
            super::transport_icon(Some(false)),
            (OsdIcon::Pause, "Pause")
        );
        assert_eq!(
            super::transport_icon(None),
            (OsdIcon::Play, "Playback state unavailable")
        );
        assert_eq!(OsdIcon::for_mute(Some(false)), (OsdIcon::Volume, "Mute"));
        assert_eq!(OsdIcon::for_mute(None), (OsdIcon::Volume, "Mute"));
        assert_eq!(OsdIcon::for_mute(Some(true)), (OsdIcon::Muted, "Unmute"));
        assert_eq!(
            super::fullscreen_icon_label(false),
            (OsdIcon::EnterFullscreen, "Enter fullscreen")
        );
        assert_eq!(
            super::fullscreen_icon_label(true),
            (OsdIcon::ExitFullscreen, "Leave fullscreen")
        );
    }

    #[test]
    fn the_volume_slider_is_bounded_by_the_boost_cap() {
        let context = Context::default();
        let state = PlayerControlsView {
            volume_percent: Some(250.0),
            volume_max_percent: 100,
            ..view(&[])
        };
        let frame = overlay_frame(&context, RawInput::default(), state);
        let slider = frame
            .rects
            .iter()
            .find(|(id, _)| *id == ControlId::VolumeSlider)
            .map(|(_, rect)| *rect)
            .expect("volume slider is drawn");
        assert!(slider.width() > 0.0);
    }

    #[test]
    fn labels_match_playback_speed_and_clock_conventions() {
        assert_eq!(speed_label(Some(0.5)), "0.5×");
        assert_eq!(speed_label(Some(1.0)), "1×");
        assert_eq!(speed_label(Some(2.0)), "2×");
        assert_eq!(speed_label(None), "Speed unavailable");
        assert_eq!(super::speed_button_label(1.0), "1×");
        assert_eq!(super::speed_button_label(1.25), "1.25×");
        assert_eq!(super::speed_button_label(1.5), "1.5×");
        assert_eq!(format_time(Duration::from_secs(65)), "1:05");
        assert_eq!(format_time(Duration::from_secs(3661)), "1:01:01");
    }

    #[test]
    fn single_row_bar_shows_numeric_speed_pill_and_right_aligned_icons() {
        let context = Context::default();
        context.enable_accesskit();
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1024.0, 420.0));

        for (multiplier, expected_text) in [(None, "1×"), (Some(1.0), "1×"), (Some(1.25), "1.25×")]
        {
            let mut rects = Vec::new();
            let mut output = context.run_ui(
                RawInput {
                    screen_rect: Some(screen),
                    ..RawInput::default()
                },
                |ui| {
                    CentralPanel::default().show(ui, |ui| {
                        let _ = draw_inner(
                            ui,
                            PlayerControlsView {
                                speed_multiplier: multiplier,
                                ..view(&[])
                            },
                            |id, rect| rects.push((id, rect)),
                        );
                    });
                },
            );
            let painted_texts: Vec<String> = output
                .shapes
                .iter()
                .filter_map(|clipped| match &clipped.shape {
                    egui::Shape::Text(text) => Some(text.galley.text().to_owned()),
                    _ => None,
                })
                .collect();
            output.textures_delta.clear();

            assert!(
                painted_texts.iter().any(|text| text == expected_text),
                "expected speed pill {expected_text:?} in {painted_texts:?}"
            );
            assert!(
                !painted_texts.iter().any(|text| text == "Speed"),
                "speed button never paints the word Speed: {painted_texts:?}"
            );

            let rect_of = |target| {
                rects
                    .iter()
                    .find(|(id, _)| *id == target)
                    .map(|(_, rect)| *rect)
                    .unwrap_or_else(|| panic!("{target:?} is drawn"))
            };
            let play = rect_of(ControlId::PlayPause);
            let speed = rect_of(ControlId::SpeedMenu);
            let audio = rect_of(ControlId::AudioMenu);
            let subtitles = rect_of(ControlId::SubtitlesMenu);
            let video = rect_of(ControlId::VideoMenu);
            let pip = rect_of(ControlId::PipToggle);
            let fullscreen = rect_of(ControlId::Fullscreen);

            for rect in [speed, audio, subtitles, video, pip, fullscreen] {
                assert!(
                    (rect.center().y - play.center().y).abs() < 8.0,
                    "all controls sit on one row: play={play:?}, control={rect:?}"
                );
            }
            assert!(speed.right() <= audio.left());
            assert!(audio.right() <= subtitles.left());
            assert!(subtitles.right() <= video.left());
            assert!(video.right() <= pip.left());
            assert!(pip.right() <= fullscreen.left());

            let tree = output
                .platform_output
                .accesskit_update
                .expect("accessibility tree is emitted");
            for name in ["Speed", "Audio", "Subtitles", "Video"] {
                assert!(
                    tree.nodes.iter().any(|(_, node)| {
                        node.role() == egui::Role::Button && node.label() == Some(name)
                    }),
                    "{name:?} keeps its accessible name"
                );
            }
        }
    }

    #[test]
    fn compact_track_labels_keep_full_short_names_and_trim_long_names() {
        assert_eq!(compact_label("English", 8), "English");
        assert_eq!(compact_label("English commentary track", 8), "English …");
    }

    #[test]
    fn narrow_osd_keeps_controls_inside_the_viewport() {
        let context = Context::default();
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(240.0, 420.0));
        let chapters = [ChapterChoice {
            index: 0,
            title: "A very long chapter title that must remain reachable".into(),
            time: Duration::from_secs(20),
        }];
        let secondary = [SecondarySubtitleChoice {
            track: 3,
            title: "An unusually long subtitle track name".into(),
        }];
        let mut rects = Vec::new();
        let mut output = context.run_ui(
            RawInput {
                screen_rect: Some(screen),
                ..RawInput::default()
            },
            |ui| {
                CentralPanel::default().show(ui, |ui| {
                    let _ = draw_inner(
                        ui,
                        PlayerControlsView {
                            chapters: &chapters,
                            secondary_subtitles: &secondary,
                            ..view(&[])
                        },
                        |id, rect| rects.push((id, rect)),
                    );
                });
            },
        );
        output.textures_delta.clear();
        assert!(rects.iter().any(|(id, _)| *id == ControlId::SeekBar));
        // The narrow tier keeps the transport essentials, folds playback actions into More, and
        // gives the four named groups their own compact row.
        for id in [
            ControlId::SpeedMenu,
            ControlId::AudioMenu,
            ControlId::SubtitlesMenu,
            ControlId::VideoMenu,
            ControlId::PlaybackMenu,
        ] {
            assert!(rects.iter().any(|(candidate, _)| *candidate == id));
        }
        assert!(
            rects.iter().all(|(id, _)| !matches!(
                id,
                ControlId::Stop
                    | ControlId::MuteToggle
                    | ControlId::PipToggle
                    | ControlId::Fullscreen
            )),
            "secondary controls are folded into More at narrow widths"
        );
        for (id, rect) in rects {
            assert!(
                rect.min.x >= screen.min.x,
                "{id:?} starts inside viewport: {rect:?}"
            );
            assert!(
                rect.max.x <= screen.max.x,
                "{id:?} ends inside viewport: {rect:?}"
            );
        }
    }

    #[test]
    fn icon_only_playback_groups_fit_a_200px_viewport() {
        let context = Context::default();
        context.enable_accesskit();
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(200.0, 420.0));
        let mut rects = Vec::new();
        let mut output = context.run_ui(
            RawInput {
                screen_rect: Some(screen),
                ..RawInput::default()
            },
            |ui| {
                CentralPanel::default().show(ui, |ui| {
                    let _ = draw_inner(ui, view(&[]), |id, rect| rects.push((id, rect)));
                });
            },
        );
        output.textures_delta.clear();
        let mut row_rects = Vec::new();
        for id in [
            ControlId::PlayPause,
            ControlId::PlaybackMenu,
            ControlId::SpeedMenu,
            ControlId::AudioMenu,
            ControlId::SubtitlesMenu,
            ControlId::VideoMenu,
        ] {
            let rect = rects
                .iter()
                .find(|(candidate, _)| *candidate == id)
                .map(|(_, rect)| *rect)
                .unwrap_or_else(|| panic!("{id:?} is drawn at 200px"));
            assert!(rect.min.x >= screen.min.x && rect.max.x <= screen.max.x);
            row_rects.push((id, rect));
        }
        row_rects.sort_by(|a, b| a.1.min.x.total_cmp(&b.1.min.x));
        for pair in row_rects.windows(2) {
            assert!(
                pair[0].1.max.x <= pair[1].1.min.x + 0.5,
                "controls must not overlap at 200px: {:?} vs {:?}",
                pair[0],
                pair[1]
            );
        }
        let tree = output
            .platform_output
            .accesskit_update
            .expect("accessibility tree is emitted");
        for name in ["Speed", "Audio", "Subtitles", "Video", "More"] {
            assert!(
                tree.nodes.iter().any(
                    |(_, node)| node.role() == egui::Role::Button && node.label() == Some(name)
                ),
                "icon-only {name:?} trigger keeps its accessible name"
            );
        }
    }

    #[test]
    fn hovering_the_timeline_asks_for_the_preview_at_the_pointer() {
        let context = Context::default();
        let mut seek_bar = None;
        run_ui(&context, RawInput::default(), |ui| {
            CentralPanel::default().show(ui, |ui| {
                let _ = draw_inner(ui, view(&[]), |id, rect| {
                    if id == ControlId::SeekBar {
                        seek_bar = Some(rect);
                    }
                });
            });
        });
        let bar = seek_bar.expect("seek bar is rendered");
        let mut asked = Vec::new();
        for _ in 0..2 {
            run_ui(
                &context,
                RawInput {
                    events: vec![egui::Event::PointerMoved(bar.center())],
                    ..RawInput::default()
                },
                |ui| {
                    CentralPanel::default().show(ui, |ui| {
                        let _ = super::draw_controls(
                            ui,
                            view(&[]),
                            None,
                            |_, _| {},
                            None::<fn(&mut egui::Ui)>,
                            |time| {
                                asked.push(time);
                                None
                            },
                            None,
                        );
                    });
                },
            );
        }
        let time = asked.last().copied().expect("preview asked while hovering");
        assert!(
            time.abs_diff(Duration::from_secs(59)) < Duration::from_secs(2),
            "{time:?}"
        );
        // Without a thumbnail the bubble still shows the time; nothing is asked once the
        // pointer leaves the bar.
        asked.clear();
        run_ui(
            &context,
            RawInput {
                events: vec![egui::Event::PointerGone],
                ..RawInput::default()
            },
            |ui| {
                CentralPanel::default().show(ui, |ui| {
                    let _ = super::draw_controls(
                        ui,
                        view(&[]),
                        None,
                        |_, _| {},
                        None::<fn(&mut egui::Ui)>,
                        |time| {
                            asked.push(time);
                            None
                        },
                        None,
                    );
                });
            },
        );
        assert!(asked.is_empty());
    }

    #[test]
    fn clicking_timeline_emits_seek_preview_position() {
        let context = Context::default();
        let mut seek_bar = None;
        run_ui(&context, RawInput::default(), |ui| {
            CentralPanel::default().show(ui, |ui| {
                let _ = draw_inner(ui, view(&[]), |id, rect| {
                    if id == ControlId::SeekBar {
                        seek_bar = Some(rect);
                    }
                });
            });
        });
        let center = seek_bar.expect("seek bar is rendered").center();
        let mut actions = Vec::new();
        run_ui(
            &context,
            RawInput {
                events: vec![
                    egui::Event::PointerMoved(center),
                    egui::Event::PointerButton {
                        pos: center,
                        button: egui::PointerButton::Primary,
                        pressed: true,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
                ..RawInput::default()
            },
            |ui| {
                CentralPanel::default().show(ui, |ui| {
                    actions = draw_inner(ui, view(&[]), |_, _| {});
                });
            },
        );
        if actions.is_empty() {
            run_ui(
                &context,
                RawInput {
                    events: vec![egui::Event::PointerButton {
                        pos: center,
                        button: egui::PointerButton::Primary,
                        pressed: false,
                        modifiers: egui::Modifiers::NONE,
                    }],
                    ..RawInput::default()
                },
                |ui| {
                    CentralPanel::default().show(ui, |ui| {
                        actions = draw_inner(ui, view(&[]), |_, _| {});
                    });
                },
            );
        }
        assert!(
            actions.iter().any(|action| matches!(
                action,
                PlayerControlAction::SeekAbsolute(position)
                    if position.abs_diff(Duration::from_secs(59)) < Duration::from_secs(2)
            )),
            "timeline click reports its preview position: {actions:?}"
        );
    }

    #[test]
    fn chapter_section_is_conditional_and_entries_are_bounded() {
        assert_eq!(MAX_CHAPTER_CHOICES, 128);

        let many: Vec<_> = (0..MAX_CHAPTER_CHOICES + 8)
            .map(|index| ChapterChoice {
                index,
                title: format!("Chapter {index}"),
                time: Duration::from_secs(index as u64 * 30),
            })
            .collect();
        assert_eq!(
            many.iter().take(MAX_CHAPTER_CHOICES).count(),
            MAX_CHAPTER_CHOICES,
            "the popover iterates at most MAX_CHAPTER_CHOICES entries"
        );
    }

    #[test]
    fn stop_button_emits_a_stop_action() {
        let context = Context::default();
        let mut stop_rect = None;
        let mut actions = Vec::new();
        run_ui(&context, RawInput::default(), |ui| {
            CentralPanel::default().show(ui, |ui| {
                actions = draw_inner(ui, view(&[]), |id, rect| {
                    if id == ControlId::Stop {
                        stop_rect = Some(rect);
                    }
                });
            });
        });
        assert!(actions.is_empty());
        let center = stop_rect.expect("stop button is rendered").center();
        run_ui(
            &context,
            RawInput {
                events: vec![
                    egui::Event::PointerMoved(center),
                    egui::Event::PointerButton {
                        pos: center,
                        button: egui::PointerButton::Primary,
                        pressed: true,
                        modifiers: egui::Modifiers::NONE,
                    },
                    egui::Event::PointerButton {
                        pos: center,
                        button: egui::PointerButton::Primary,
                        pressed: false,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
                ..RawInput::default()
            },
            |ui| {
                CentralPanel::default().show(ui, |ui| {
                    actions = draw(ui, view(&[]));
                });
            },
        );
        assert!(actions.contains(&PlayerControlAction::Stop));
    }

    #[test]
    fn stop_button_is_disabled_without_loaded_media() {
        let context = Context::default();
        let mut state = view(&[]);
        state.stop_enabled = false;
        let mut stop_rect = None;
        let mut actions = Vec::new();
        run_ui(&context, RawInput::default(), |ui| {
            CentralPanel::default().show(ui, |ui| {
                actions = draw_inner(ui, state, |id, rect| {
                    if id == ControlId::Stop {
                        stop_rect = Some(rect);
                    }
                });
            });
        });
        let center = stop_rect.expect("stop button is rendered").center();
        run_ui(
            &context,
            RawInput {
                events: vec![
                    egui::Event::PointerMoved(center),
                    egui::Event::PointerButton {
                        pos: center,
                        button: egui::PointerButton::Primary,
                        pressed: true,
                        modifiers: egui::Modifiers::NONE,
                    },
                    egui::Event::PointerButton {
                        pos: center,
                        button: egui::PointerButton::Primary,
                        pressed: false,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
                ..RawInput::default()
            },
            |ui| {
                CentralPanel::default().show(ui, |ui| {
                    actions = draw(ui, state);
                });
            },
        );
        assert!(!actions.contains(&PlayerControlAction::Stop));
    }

    #[test]
    fn pip_button_emits_toggle_and_is_disabled_without_video() {
        // One fresh context per case, laid out once and then clicked, as the play-button test does.
        let click_pip = |available: bool| -> Vec<PlayerControlAction> {
            let context = Context::default();
            let pip_view = || PlayerControlsView {
                pip_available: available,
                ..view(&[])
            };
            let mut pip_rect = None;
            run_ui(&context, RawInput::default(), |ui| {
                CentralPanel::default().show(ui, |ui| {
                    draw_inner(ui, pip_view(), |id, rect| {
                        if id == ControlId::PipToggle {
                            pip_rect = Some(rect);
                        }
                    });
                });
            });
            let center = pip_rect.expect("PiP button is rendered").center();
            let mut action = Vec::new();
            run_ui(
                &context,
                RawInput {
                    events: vec![
                        egui::Event::PointerMoved(center),
                        egui::Event::PointerButton {
                            pos: center,
                            button: egui::PointerButton::Primary,
                            pressed: true,
                            modifiers: egui::Modifiers::NONE,
                        },
                        egui::Event::PointerButton {
                            pos: center,
                            button: egui::PointerButton::Primary,
                            pressed: false,
                            modifiers: egui::Modifiers::NONE,
                        },
                    ],
                    ..RawInput::default()
                },
                |ui| {
                    CentralPanel::default().show(ui, |ui| {
                        action = draw(ui, pip_view());
                    });
                },
            );
            action
        };
        assert!(
            !click_pip(false).contains(&PlayerControlAction::TogglePip),
            "a disabled PiP button must not emit an action"
        );
        assert!(click_pip(true).contains(&PlayerControlAction::TogglePip));
    }

    #[test]
    fn ctrl_s_press_is_the_screenshot_key_and_repeats_or_other_chords_are_not() {
        let key = |pressed, repeat, modifiers| egui::Event::Key {
            key: egui::Key::S,
            physical_key: None,
            pressed,
            repeat,
            modifiers,
        };
        let ctrl = egui::Modifiers::CTRL;
        assert!(screenshot_key_pressed(&[key(true, false, ctrl)]));
        assert!(
            !screenshot_key_pressed(&[key(false, false, ctrl)]),
            "release"
        );
        assert!(!screenshot_key_pressed(&[key(true, true, ctrl)]), "repeat");
        assert!(!screenshot_key_pressed(&[key(
            true,
            false,
            egui::Modifiers::NONE
        )]));
        assert!(!screenshot_key_pressed(&[key(
            true,
            false,
            ctrl | egui::Modifiers::SHIFT
        )]));
        assert!(!screenshot_key_pressed(&[key(
            true,
            false,
            ctrl | egui::Modifiers::ALT
        )]));
    }

    #[test]
    fn ctrl_number_shortcuts_select_the_player_actions() {
        let key = |key, pressed, repeat, modifiers| egui::Event::Key {
            key,
            physical_key: Some(key),
            pressed,
            repeat,
            modifiers,
        };
        let ctrl = egui::Modifiers::CTRL;
        assert_eq!(
            player_shortcut_action(&[key(egui::Key::Num1, true, false, ctrl)]),
            Some(PlayerControlAction::FitWindowToVideo)
        );
        assert_eq!(
            player_shortcut_action(&[key(egui::Key::Num2, true, false, ctrl)]),
            Some(PlayerControlAction::ToggleDoubleSpeed)
        );
        assert_eq!(
            player_shortcut_action(&[key(egui::Key::Num1, false, false, ctrl)]),
            None,
            "release"
        );
        assert_eq!(
            player_shortcut_action(&[key(egui::Key::Num2, true, true, ctrl)]),
            None,
            "repeat"
        );
        assert_eq!(
            player_shortcut_action(&[key(egui::Key::Num1, true, false, egui::Modifiers::NONE)]),
            None,
            "Ctrl is required"
        );
        assert_eq!(
            player_shortcut_action(&[key(
                egui::Key::Num2,
                true,
                false,
                ctrl | egui::Modifiers::SHIFT
            )]),
            None,
            "other chords do not toggle speed"
        );
    }

    #[test]
    fn ctrl_speed_and_aspect_shortcuts_select_the_player_actions() {
        let key = |key, pressed, repeat, modifiers| egui::Event::Key {
            key,
            physical_key: Some(key),
            pressed,
            repeat,
            modifiers,
        };
        let ctrl = egui::Modifiers::CTRL;
        assert_eq!(
            player_shortcut_action(&[key(egui::Key::ArrowUp, true, false, ctrl)]),
            Some(PlayerControlAction::StepSpeed { up: true })
        );
        assert_eq!(
            player_shortcut_action(&[key(egui::Key::ArrowDown, true, false, ctrl)]),
            Some(PlayerControlAction::StepSpeed { up: false })
        );
        assert_eq!(
            player_shortcut_action(&[key(egui::Key::Num0, true, false, ctrl)]),
            Some(PlayerControlAction::ResetSpeed)
        );
        assert_eq!(
            player_shortcut_action(&[key(egui::Key::A, true, false, ctrl)]),
            Some(PlayerControlAction::CycleAspect)
        );
        assert_eq!(
            player_shortcut_action(&[key(egui::Key::ArrowUp, true, false, egui::Modifiers::NONE)]),
            None,
            "bare arrows belong to seeking, not speed"
        );
        assert_eq!(
            player_shortcut_action(&[key(egui::Key::A, true, true, ctrl)]),
            None,
            "repeat"
        );
    }

    #[test]
    fn clicking_play_emits_toggle_action() {
        let context = Context::default();
        let mut action = Vec::new();
        let mut play_rect = None;
        run_ui(&context, RawInput::default(), |ui| {
            CentralPanel::default().show(ui, |ui| {
                action = draw_inner(ui, view(&[]), |id, rect| {
                    if id == ControlId::PlayPause {
                        play_rect = Some(rect);
                    }
                });
            });
        });
        assert!(action.is_empty());
        let center = play_rect.expect("play/pause button is rendered").center();
        run_ui(
            &context,
            RawInput {
                events: vec![
                    egui::Event::PointerMoved(center),
                    egui::Event::PointerButton {
                        pos: center,
                        button: egui::PointerButton::Primary,
                        pressed: true,
                        modifiers: egui::Modifiers::NONE,
                    },
                    egui::Event::PointerButton {
                        pos: center,
                        button: egui::PointerButton::Primary,
                        pressed: false,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
                ..RawInput::default()
            },
            |ui| {
                CentralPanel::default().show(ui, |ui| {
                    action = draw(ui, view(&[]));
                });
            },
        );
        assert!(action.contains(&PlayerControlAction::TogglePause));
    }

    #[test]
    fn previous_track_button_emits_action_and_is_disabled_without_queue_context() {
        let context = Context::default();
        let mut previous_rect = None;
        let mut actions = Vec::new();
        run_ui(&context, RawInput::default(), |ui| {
            CentralPanel::default().show(ui, |ui| {
                actions = draw_inner(ui, view(&[]), |id, rect| {
                    if id == ControlId::PreviousTrack {
                        previous_rect = Some(rect);
                    }
                });
            });
        });
        let center = previous_rect
            .expect("previous track button is rendered")
            .center();
        run_ui(
            &context,
            RawInput {
                events: vec![
                    egui::Event::PointerMoved(center),
                    egui::Event::PointerButton {
                        pos: center,
                        button: egui::PointerButton::Primary,
                        pressed: true,
                        modifiers: egui::Modifiers::NONE,
                    },
                    egui::Event::PointerButton {
                        pos: center,
                        button: egui::PointerButton::Primary,
                        pressed: false,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
                ..RawInput::default()
            },
            |ui| {
                CentralPanel::default().show(ui, |ui| {
                    actions = draw(ui, view(&[]));
                });
            },
        );
        assert!(actions.contains(&PlayerControlAction::PreviousTrack));

        let context = Context::default();
        let mut disabled = view(&[]);
        disabled.previous_track_enabled = false;
        let mut previous_rect = None;
        run_ui(&context, RawInput::default(), |ui| {
            CentralPanel::default().show(ui, |ui| {
                let _ = draw_inner(ui, disabled, |id, rect| {
                    if id == ControlId::PreviousTrack {
                        previous_rect = Some(rect);
                    }
                });
            });
        });
        assert!(
            previous_rect.is_none(),
            "previous track button is hidden when disabled"
        );
    }

    #[test]
    fn next_track_button_emits_action() {
        let context = Context::default();
        let mut next_rect = None;
        let mut actions = Vec::new();
        run_ui(&context, RawInput::default(), |ui| {
            CentralPanel::default().show(ui, |ui| {
                actions = draw_inner(ui, view(&[]), |id, rect| {
                    if id == ControlId::NextTrack {
                        next_rect = Some(rect);
                    }
                });
            });
        });
        let center = next_rect.expect("next track button is rendered").center();
        run_ui(
            &context,
            RawInput {
                events: vec![
                    egui::Event::PointerMoved(center),
                    egui::Event::PointerButton {
                        pos: center,
                        button: egui::PointerButton::Primary,
                        pressed: true,
                        modifiers: egui::Modifiers::NONE,
                    },
                    egui::Event::PointerButton {
                        pos: center,
                        button: egui::PointerButton::Primary,
                        pressed: false,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
                ..RawInput::default()
            },
            |ui| {
                CentralPanel::default().show(ui, |ui| {
                    actions = draw(ui, view(&[]));
                });
            },
        );
        assert!(actions.contains(&PlayerControlAction::NextTrack));
    }

    #[test]
    fn unmodified_left_and_right_seek_exactly_ten_seconds() {
        for (key, expected) in [
            (egui::Key::ArrowLeft, PlayerControlAction::SeekRelative(-10)),
            (egui::Key::ArrowRight, PlayerControlAction::SeekRelative(10)),
        ] {
            let context = Context::default();
            let mut actions = Vec::new();
            run_ui(
                &context,
                RawInput {
                    events: vec![egui::Event::Key {
                        key,
                        physical_key: Some(key),
                        pressed: true,
                        repeat: false,
                        modifiers: egui::Modifiers::NONE,
                    }],
                    ..RawInput::default()
                },
                |ui| {
                    CentralPanel::default().show(ui, |ui| {
                        actions = draw(ui, view(&[]));
                    });
                },
            );
            assert_eq!(actions, [expected]);
        }
    }

    #[test]
    fn arrow_seek_does_not_run_while_a_text_editor_is_focused() {
        let context = Context::default();
        let mut text = String::new();
        run_ui(&context, RawInput::default(), |ui| {
            CentralPanel::default().show(ui, |ui| {
                let response = ui.add(egui::TextEdit::singleline(&mut text));
                response.request_focus();
            });
        });
        assert!(context.egui_wants_keyboard_input());

        let actions = keyboard_seek_actions(&context, true);
        assert!(actions.is_empty());
    }

    #[test]
    fn arrow_seek_does_not_run_while_a_popup_is_open() {
        let context = Context::default();
        egui::Popup::open_id(&context, Id::unique("player-control-test-popup"));
        assert!(egui::Popup::is_any_open(&context));

        let mut actions = Vec::new();
        run_ui(
            &context,
            RawInput {
                events: vec![egui::Event::Key {
                    key: egui::Key::ArrowRight,
                    physical_key: Some(egui::Key::ArrowRight),
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::NONE,
                }],
                ..RawInput::default()
            },
            |ui| {
                CentralPanel::default().show(ui, |ui| {
                    actions = draw(ui, view(&[]));
                });
            },
        );
        assert!(actions.is_empty());
    }

    #[test]
    fn modified_arrows_and_unseekable_media_do_not_seek() {
        let context = Context::default();
        let mut actions = Vec::new();
        run_ui(
            &context,
            RawInput {
                events: vec![egui::Event::Key {
                    key: egui::Key::ArrowRight,
                    physical_key: Some(egui::Key::ArrowRight),
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::CTRL,
                }],
                ..RawInput::default()
            },
            |ui| {
                actions = keyboard_seek_actions(ui.ctx(), true);
            },
        );
        assert!(actions.is_empty());
        assert!(keyboard_seek_actions(&context, false).is_empty());
    }

    /// Playback at 12 s in a 600 s item, with an intro from 10 s to 30 s loaded.
    fn skip_view() -> PlayerControlsView<'static> {
        PlayerControlsView {
            position: Duration::from_secs(12),
            duration: Some(Duration::from_secs(600)),
            ..view(&[])
        }
    }

    fn intro_skipper() -> SegmentSkipper {
        let mut skipper = SegmentSkipper::new();
        let intro = MediaSegment::new("intro-1", SegmentKind::Intro, 10 * SECOND, 30 * SECOND)
            .expect("intro segment");
        skipper.load("episode-1", vec![intro]);
        skipper
    }

    fn click_events(center: egui::Pos2) -> Vec<egui::Event> {
        vec![
            egui::Event::PointerMoved(center),
            egui::Event::PointerButton {
                pos: center,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::NONE,
            },
            egui::Event::PointerButton {
                pos: center,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            },
        ]
    }

    #[test]
    fn skip_button_shows_for_an_intro_and_seeks_once_when_pressed() {
        let context = Context::default();
        let mut skipper = intro_skipper();
        let settings = SkipSettings::new(true, false, false, false, 0).expect("settings");
        let mut skip_rect = None;
        let mut actions = Vec::new();
        run_ui(&context, RawInput::default(), |ui| {
            CentralPanel::default().show(ui, |ui| {
                actions = draw_inner_with_skip(
                    ui,
                    skip_view(),
                    Some(SkipControls {
                        skipper: &mut skipper,
                        settings,
                    }),
                    |id, rect| {
                        if id == ControlId::SkipSegment {
                            skip_rect = Some(rect);
                        }
                    },
                    |_| {},
                );
            });
        });
        assert!(actions.is_empty(), "the button alone must not seek");
        let center = skip_rect.expect("skip button is rendered").center();

        run_ui(
            &context,
            RawInput {
                events: click_events(center),
                ..RawInput::default()
            },
            |ui| {
                CentralPanel::default().show(ui, |ui| {
                    actions = draw_with_skip(
                        ui,
                        skip_view(),
                        Some(SkipControls {
                            skipper: &mut skipper,
                            settings,
                        }),
                    );
                });
            },
        );
        assert_eq!(
            actions,
            [PlayerControlAction::SeekAbsolute(Duration::from_secs(30))]
        );
        assert!(
            skipper
                .button(12 * SECOND, Some(600 * SECOND), &settings)
                .is_none(),
            "a pressed segment is not offered again"
        );
    }

    #[test]
    fn automatic_skip_seeks_without_a_press_and_only_once() {
        let context = Context::default();
        let mut skipper = intro_skipper();
        let settings = SkipSettings::new(true, false, true, false, 0).expect("settings");
        let mut skip_rect = None;
        let mut actions = Vec::new();
        for frame in 0..2 {
            run_ui(&context, RawInput::default(), |ui| {
                CentralPanel::default().show(ui, |ui| {
                    actions = draw_inner_with_skip(
                        ui,
                        skip_view(),
                        Some(SkipControls {
                            skipper: &mut skipper,
                            settings,
                        }),
                        |id, rect| {
                            if id == ControlId::SkipSegment {
                                skip_rect = Some(rect);
                            }
                        },
                        |_| {},
                    );
                });
            });
            if frame == 0 {
                assert_eq!(
                    actions,
                    [PlayerControlAction::SeekAbsolute(Duration::from_secs(30))]
                );
            } else {
                assert!(actions.is_empty(), "the segment is not skipped twice");
            }
        }
        assert!(
            skip_rect.is_none(),
            "an automatic skip does not also show the button"
        );
    }

    #[test]
    fn automatic_switch_without_its_button_switch_never_seeks() {
        let segments_off = [
            SkipSettings::default(),
            SkipSettings::new(false, false, true, true, 0).expect("settings"),
        ];
        for settings in segments_off {
            let context = Context::default();
            let mut skipper = intro_skipper();
            let mut skip_rect = None;
            let mut actions = Vec::new();
            run_ui(&context, RawInput::default(), |ui| {
                CentralPanel::default().show(ui, |ui| {
                    actions = draw_inner_with_skip(
                        ui,
                        skip_view(),
                        Some(SkipControls {
                            skipper: &mut skipper,
                            settings,
                        }),
                        |id, rect| {
                            if id == ControlId::SkipSegment {
                                skip_rect = Some(rect);
                            }
                        },
                        |_| {},
                    );
                });
            });
            assert!(actions.is_empty());
            assert!(skip_rect.is_none());
        }
    }

    #[test]
    fn hidden_controls_still_run_the_automatic_skip_once() {
        let mut skipper = intro_skipper();
        let settings = SkipSettings::new(true, false, true, false, 0).expect("settings");
        let duration = Some(Duration::from_secs(600));

        let first = auto_skip_action(
            SkipControls {
                skipper: &mut skipper,
                settings,
            },
            Duration::from_secs(12),
            duration,
        );
        assert_eq!(
            first,
            Some(PlayerControlAction::SeekAbsolute(Duration::from_secs(30)))
        );

        let second = auto_skip_action(
            SkipControls {
                skipper: &mut skipper,
                settings,
            },
            Duration::from_secs(13),
            duration,
        );
        assert_eq!(second, None, "the same intro must not be skipped twice");
    }

    #[test]
    fn playback_settings_map_to_the_skip_model() {
        let playback = PlaybackSettings {
            skip_outro: true,
            auto_skip_outro: true,
            outro_delay_ms: 5_000,
            ..PlaybackSettings::default()
        };
        let settings = skip_settings(&playback).expect("valid settings");
        assert!(settings.shows_button(SegmentKind::Outro));
        assert!(settings.auto_skips(SegmentKind::Outro));
        assert!(!settings.shows_button(SegmentKind::Intro));
        assert_eq!(settings.outro_delay_ms(), 5_000);
        assert!(
            !skip_settings(&PlaybackSettings::default())
                .expect("defaults are valid")
                .fetch_needed()
        );

        let too_late = PlaybackSettings {
            outro_delay_ms: 60_001,
            ..PlaybackSettings::default()
        };
        assert!(skip_settings(&too_late).is_err());
    }

    #[test]
    fn tick_conversion_is_100_ns_per_tick() {
        assert_eq!(duration_to_ticks(Duration::from_millis(1_500)), 15_000_000);
        assert_eq!(
            ticks_to_duration(duration_to_ticks(Duration::from_millis(1_500))),
            Duration::from_millis(1_500)
        );
    }

    /// Tab presses allowed for one walk through the overlay.
    const TAB_WALK_LIMIT: usize = 32;

    /// Every control in single-row widget allocation order, with the skip button offered (position
    /// 20 s is inside the intro) and right-aligned menus added right-to-left.
    const DRAW_ORDER: [ControlId; 16] = [
        ControlId::SeekBar,
        ControlId::PlayPause,
        ControlId::SeekBackward,
        ControlId::SeekForward,
        ControlId::PreviousTrack,
        ControlId::NextTrack,
        ControlId::Stop,
        ControlId::MuteToggle,
        ControlId::VolumeSlider,
        ControlId::SkipSegment,
        ControlId::Fullscreen,
        ControlId::PipToggle,
        ControlId::VideoMenu,
        ControlId::SubtitlesMenu,
        ControlId::AudioMenu,
        ControlId::SpeedMenu,
    ];

    /// The position in `DRAW_ORDER`. Matching every variant means a new control fails to compile
    /// here until it is given a place, and the walk test then has to include it.
    fn draw_index(id: ControlId) -> usize {
        match id {
            ControlId::SeekBar => 0,
            ControlId::PlayPause => 1,
            ControlId::SeekBackward => 2,
            ControlId::SeekForward => 3,
            ControlId::PreviousTrack => 4,
            ControlId::NextTrack => 5,
            ControlId::Stop => 6,
            ControlId::MuteToggle => 7,
            ControlId::VolumeSlider => 8,
            ControlId::SkipSegment => 9,
            ControlId::Fullscreen => 10,
            ControlId::PipToggle => 11,
            ControlId::VideoMenu => 12,
            ControlId::SubtitlesMenu => 13,
            ControlId::AudioMenu => 14,
            ControlId::SpeedMenu => 15,
            // More follows Speed when a narrow row needs playback overflow.
            ControlId::PlaybackMenu => 16,
        }
    }

    /// Every control enabled and visible, so the walk reaches all of them.
    fn keyboard_view<'a>(
        chapters: &'a [ChapterChoice],
        secondary: &'a [SecondarySubtitleChoice],
    ) -> PlayerControlsView<'a> {
        PlayerControlsView {
            chapters,
            secondary_subtitles: secondary,
            ..view(&[])
        }
    }

    fn key_press(key: egui::Key) -> RawInput {
        RawInput {
            events: vec![egui::Event::Key {
                key,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }],
            ..RawInput::default()
        }
    }

    /// One frame of the overlay: the actions, each control's rectangle, and the painted shapes.
    struct OverlayFrame {
        actions: Vec<PlayerControlAction>,
        rects: Vec<(ControlId, egui::Rect)>,
        shapes: Vec<egui::epaint::ClippedShape>,
    }

    fn overlay_frame(
        context: &Context,
        input: RawInput,
        view: PlayerControlsView<'_>,
    ) -> OverlayFrame {
        overlay_frame_with_extra(context, input, view, |_| {})
    }

    fn overlay_frame_with_extra(
        context: &Context,
        input: RawInput,
        view: PlayerControlsView<'_>,
        extra: impl FnOnce(&mut egui::Ui),
    ) -> OverlayFrame {
        let mut skipper = intro_skipper();
        let settings = SkipSettings::new(true, false, false, false, 0).expect("settings");
        let mut actions = Vec::new();
        let mut rects = Vec::new();
        let mut extra = Some(extra);
        let mut output = context.run_ui(input, |ui| {
            CentralPanel::default().show(ui, |ui| {
                actions = draw_inner_with_skip(
                    ui,
                    view,
                    Some(SkipControls {
                        skipper: &mut skipper,
                        settings,
                    }),
                    |id, rect| rects.push((id, rect)),
                    extra
                        .take()
                        .unwrap_or_else(|| panic!("extra is consumed once")),
                );
            });
        });
        output.textures_delta.clear();
        OverlayFrame {
            actions,
            rects,
            shapes: output.shapes,
        }
    }

    /// The control that has keyboard focus after a frame. The focused widget's own rectangle can be
    /// smaller than the control (the slider's grab point), so it is matched by its centre.
    fn focused_control(context: &Context, rects: &[(ControlId, egui::Rect)]) -> Option<ControlId> {
        let focused = context.memory(|memory| memory.focused())?;
        let center = context.read_response(focused)?.rect.center();
        rects
            .iter()
            .find(|(_, rect)| rect.contains(center))
            .map(|(id, _)| *id)
    }

    /// Press Tab until `target` has focus. False if it is not reached within the walk limit.
    fn tab_to(context: &Context, view: PlayerControlsView<'_>, target: ControlId) -> bool {
        (0..TAB_WALK_LIMIT).any(|_| {
            let frame = overlay_frame(context, key_press(egui::Key::Tab), view);
            focused_control(context, &frame.rects) == Some(target)
        })
    }

    #[test]
    fn every_control_is_reached_by_tab_in_visual_row_order() {
        for (index, id) in DRAW_ORDER.iter().enumerate() {
            assert_eq!(draw_index(*id), index, "{id:?} is listed at its draw index");
        }

        let chapters = [ChapterChoice {
            index: 0,
            title: "Opening".into(),
            time: Duration::ZERO,
        }];
        let secondary = [SecondarySubtitleChoice {
            track: 3,
            title: "English".into(),
        }];
        let context = Context::default();
        let state = keyboard_view(&chapters, &secondary);
        let mut reached = Vec::new();
        for _ in 0..TAB_WALK_LIMIT {
            let frame = overlay_frame(&context, key_press(egui::Key::Tab), state);
            let Some(id) = focused_control(&context, &frame.rects) else {
                break;
            };
            if reached.contains(&id) {
                break;
            }
            reached.push(id);
        }
        assert_eq!(reached, DRAW_ORDER);
    }

    #[test]
    fn accessible_controls_are_named_and_disabled_buttons_report_their_state() {
        let context = Context::default();
        context.enable_accesskit();
        let state = PlayerControlsView {
            duration: None,
            paused: None,
            seek_enabled: false,
            stop_enabled: false,
            previous_track_enabled: false,
            next_track_enabled: false,
            pip_available: false,
            ..view(&[])
        };
        let mut output = context.run_ui(RawInput::default(), |ui| {
            CentralPanel::default().show(ui, |ui| {
                let _ = draw_inner(ui, state, |_, _| {});
            });
        });
        output.textures_delta.clear();
        let tree = output
            .platform_output
            .accesskit_update
            .expect("accessibility tree is emitted");

        let interactive_roles = [
            egui::Role::Button,
            egui::Role::DefaultButton,
            egui::Role::ComboBox,
            egui::Role::Slider,
            egui::Role::RadioButton,
            egui::Role::MenuItem,
        ];
        for (_, node) in tree
            .nodes
            .iter()
            .filter(|(_, node)| interactive_roles.contains(&node.role()))
        {
            assert!(
                node.label().is_some_and(|label| !label.trim().is_empty())
                    || !node.labelled_by().is_empty(),
                "interactive {:?} node has an accessible name",
                node.role()
            );
        }

        for name in [
            "Playback state unavailable",
            "Seek backward 10 seconds",
            "Seek forward 10 seconds",
            "Stop playback",
            "Picture-in-picture",
        ] {
            let node = tree
                .nodes
                .iter()
                .find(|(_, node)| node.label() == Some(name))
                .map(|(_, node)| node)
                .unwrap_or_else(|| panic!("accessibility node {name:?} exists"));
            assert!(node.is_disabled(), "{name:?} is announced as disabled");
        }
    }

    #[test]
    fn synchronized_speed_is_disabled_and_has_an_accessible_explanation() {
        let context = Context::default();
        context.enable_accesskit();
        let state = PlayerControlsView {
            speed_enabled: false,
            ..view(&[])
        };
        // Open the popover from the keyboard path, exactly as a viewer would, so its anchor is
        // known before the accessibility tree is read.
        let layout = overlay_frame(&context, RawInput::default(), state);
        let center = layout
            .rects
            .iter()
            .find(|(id, _)| *id == ControlId::SpeedMenu)
            .map(|(_, rect)| rect.center())
            .expect("speed group is drawn");
        overlay_frame(
            &context,
            RawInput {
                events: click_events(center),
                ..RawInput::default()
            },
            state,
        );
        let mut output = context.run_ui(RawInput::default(), |ui| {
            CentralPanel::default().show(ui, |ui| {
                let _ = draw_inner(ui, state, |_, _| {});
            });
        });
        output.textures_delta.clear();
        let tree = output
            .platform_output
            .accesskit_update
            .expect("accessibility tree is emitted");
        let speed = tree
            .nodes
            .iter()
            .find(|(_, node)| {
                node.role() == egui::Role::Button && node.label() == Some("Playback speed")
            })
            .map(|(_, node)| node)
            .expect("the disabled playback speed row is exposed");
        assert!(speed.is_disabled());
        assert_eq!(speed.description(), Some(super::SYNCED_SPEED_TOOLTIP));
    }

    #[test]
    fn disabled_speed_menu_cannot_emit_a_speed_change() {
        let chapters = [];
        let secondary = [];
        let state = PlayerControlsView {
            speed_enabled: false,
            ..keyboard_view(&chapters, &secondary)
        };
        let keyboard_context = Context::default();
        assert!(
            tab_to(&keyboard_context, state, ControlId::SpeedMenu),
            "the speed group stays reachable"
        );
        // Open the speed surface; its disabled note offers no enabled speed row.
        for _ in 0..TAB_WALK_LIMIT {
            let frame = overlay_frame(&keyboard_context, key_press(egui::Key::Tab), state);
            if !frame.actions.is_empty() {
                break;
            }
        }
        let pointer_context = Context::default();
        egui::Popup::open_id(&pointer_context, PlaybackPopover::Speed.id());
        let mut actions = Vec::new();
        let mut output = pointer_context.run_ui(RawInput::default(), |ui| {
            CentralPanel::default().show(ui, |ui| {
                actions = draw_inner(ui, state, |_, _| {});
            });
        });
        output.textures_delta.clear();
        assert!(
            !actions
                .iter()
                .any(|action| matches!(action, PlayerControlAction::SetSpeed(_))),
            "a synchronized session never offers a local speed change"
        );
    }

    #[test]
    fn live_playback_value_changes_keep_keyboard_focus_on_the_control() {
        let chapters = [];
        let secondary = [];
        for target in [ControlId::PlayPause, ControlId::SeekBar] {
            let context = Context::default();
            let state = keyboard_view(&chapters, &secondary);
            assert!(tab_to(&context, state, target), "Tab reaches {target:?}");

            let updated = match target {
                ControlId::PlayPause => PlayerControlsView {
                    paused: Some(false),
                    ..state
                },
                ControlId::SeekBar => PlayerControlsView {
                    position: Duration::from_secs(30),
                    ..state
                },
                _ => unreachable!("only controls with changing playback values are covered"),
            };
            let frame = overlay_frame(&context, RawInput::default(), updated);
            assert_eq!(
                focused_control(&context, &frame.rects),
                Some(target),
                "a live playback update keeps focus on {target:?}"
            );
        }
    }

    #[test]
    fn static_controls_do_not_schedule_an_immediate_repaint() {
        let context = Context::default();
        let state = keyboard_view(&[], &[]);
        // egui schedules a follow-up pass for each immediate repaint. Warm the
        // same layout (without a changing skip button), then require idle frames.
        for frame in 0..7 {
            let mut output = context.run_ui(
                RawInput {
                    time: Some(f64::from(frame) / 60.0),
                    ..RawInput::default()
                },
                |ui| {
                    CentralPanel::default().show(ui, |ui| {
                        let _ = draw_inner(ui, state, |_, _| {});
                    });
                },
            );
            let repaint_delay = output
                .viewport_output
                .get(&egui::ViewportId::ROOT)
                .map_or(Duration::MAX, |viewport| viewport.repaint_delay);
            output.textures_delta.clear();
            if frame >= 3 {
                assert_ne!(
                    repaint_delay,
                    Duration::ZERO,
                    "idle control frame {frame} requested an immediate repaint: {:?}",
                    context.repaint_causes()
                );
            }
        }
    }

    #[test]
    fn enter_activates_each_button_once_tab_has_focused_it() {
        let chapters = [];
        let secondary = [];
        let expected = [
            (ControlId::PlayPause, PlayerControlAction::TogglePause),
            (ControlId::PreviousTrack, PlayerControlAction::PreviousTrack),
            (ControlId::NextTrack, PlayerControlAction::NextTrack),
            (
                ControlId::SeekBackward,
                PlayerControlAction::SeekRelative(-10),
            ),
            (
                ControlId::SeekForward,
                PlayerControlAction::SeekRelative(10),
            ),
            (ControlId::Stop, PlayerControlAction::Stop),
            (ControlId::PipToggle, PlayerControlAction::TogglePip),
            (ControlId::MuteToggle, PlayerControlAction::ToggleMute),
            (ControlId::Fullscreen, PlayerControlAction::ToggleFullscreen),
        ];
        for (id, action) in expected {
            let context = Context::default();
            let state = keyboard_view(&chapters, &secondary);
            assert!(tab_to(&context, state, id), "Tab reaches {id:?}");
            let frame = overlay_frame(&context, key_press(egui::Key::Enter), state);
            assert!(
                frame.actions.contains(&action),
                "Enter on {id:?} emits {action:?}, got {:?}",
                frame.actions
            );
        }
    }

    #[test]
    fn dpad_navigation_moves_focus_and_south_activates_the_focused_control() {
        let state = keyboard_view(&[], &[]);
        let context = Context::default();
        assert!(tab_to(&context, state, ControlId::PlayPause));
        let before = overlay_frame(&context, RawInput::default(), state);
        assert_eq!(
            focused_control(&context, &before.rects),
            Some(ControlId::PlayPause)
        );

        let moved = overlay_frame(&context, key_press(egui::Key::ArrowRight), state);
        assert_eq!(
            focused_control(&context, &moved.rects),
            Some(ControlId::SeekBackward)
        );

        let selected = overlay_frame(&context, key_press(egui::Key::Enter), state);
        assert!(
            selected
                .actions
                .contains(&PlayerControlAction::SeekRelative(-10))
        );
    }

    #[test]
    fn arrows_on_the_focused_volume_slider_adjust_volume_without_seeking() {
        for key in [egui::Key::ArrowLeft, egui::Key::ArrowRight] {
            let context = Context::default();
            let state = keyboard_view(&[], &[]);
            assert!(tab_to(&context, state, ControlId::VolumeSlider));
            let frame = overlay_frame(&context, key_press(key), state);
            assert!(
                frame
                    .actions
                    .iter()
                    .any(|action| matches!(action, PlayerControlAction::SetVolume(_))),
                "{key:?} on the focused volume slider must update its value"
            );
            assert!(
                frame.actions.iter().all(|action| !matches!(
                    action,
                    PlayerControlAction::SeekRelative(_) | PlayerControlAction::SeekAbsolute(_)
                )),
                "{key:?} on the volume slider must not seek"
            );
        }
    }

    #[test]
    fn one_arrow_press_on_the_focused_seek_bar_seeks_once() {
        let chapters = [];
        let secondary = [];
        let context = Context::default();
        let state = keyboard_view(&chapters, &secondary);
        assert!(tab_to(&context, state, ControlId::SeekBar));
        let frame = overlay_frame(&context, key_press(egui::Key::ArrowRight), state);
        let seeks = frame
            .actions
            .iter()
            .filter(|action| {
                matches!(
                    action,
                    PlayerControlAction::SeekRelative(_) | PlayerControlAction::SeekAbsolute(_)
                )
            })
            .count();
        assert_eq!(
            seeks, 1,
            "one arrow press seeks once, got {:?}",
            frame.actions
        );
    }

    #[test]
    fn assistive_technology_can_set_the_playback_position() {
        use egui::accesskit::{Action, ActionData, ActionRequest, TreeId};

        let context = Context::default();
        context.enable_accesskit();
        let state = keyboard_view(&[], &[]);
        let mut output = context.run_ui(RawInput::default(), |ui| {
            CentralPanel::default().show(ui, |ui| {
                let _ = draw_inner(ui, state, |_, _| {});
            });
        });
        output.textures_delta.clear();
        let tree = output
            .platform_output
            .accesskit_update
            .expect("accessibility tree");
        let (target, node) = tree
            .nodes
            .iter()
            .find(|(_, node)| node.label() == Some("Playback position"))
            .expect("seek slider");
        assert_eq!(node.min_numeric_value(), Some(0.0));
        assert!(node.max_numeric_value().is_some_and(|end| end > 25.0));
        assert!(node.supports_action(Action::SetValue));
        let input = RawInput {
            events: vec![egui::Event::AccessKitActionRequest(ActionRequest {
                action: Action::SetValue,
                target_tree: TreeId::ROOT,
                target_node: *target,
                data: Some(ActionData::NumericValue(25.0)),
            })],
            ..RawInput::default()
        };
        let frame = overlay_frame(&context, input, state);
        assert!(
            frame
                .actions
                .contains(&PlayerControlAction::SeekAbsolute(Duration::from_secs(25)))
        );
    }

    #[test]
    fn enter_on_the_skip_button_seeks_once_it_has_focus() {
        let chapters = [];
        let secondary = [];
        let context = Context::default();
        let state = keyboard_view(&chapters, &secondary);
        assert!(tab_to(&context, state, ControlId::SkipSegment));
        let frame = overlay_frame(&context, key_press(egui::Key::Enter), state);
        assert!(
            frame
                .actions
                .iter()
                .any(|action| matches!(action, PlayerControlAction::SeekAbsolute(_))),
            "Enter on the skip button seeks, got {:?}",
            frame.actions
        );
    }

    #[test]
    fn each_playback_group_takes_an_item_from_the_keyboard() {
        use mpv_render_proof::TrackSelection;
        let chapters = [ChapterChoice {
            index: 0,
            title: "Opening".into(),
            time: Duration::ZERO,
        }];
        let secondary = [SecondarySubtitleChoice {
            track: 3,
            title: "English".into(),
        }];
        let audio = [TrackChoice {
            value: TrackSelection::Automatic,
            label: "Automatic".into(),
            selected: true,
        }];
        let subtitles = [TrackChoice {
            value: TrackSelection::Disabled,
            label: "Off".into(),
            selected: true,
        }];
        let state = PlayerControlsView {
            audio_tracks: Some(&audio),
            subtitle_tracks: Some(&subtitles),
            ..keyboard_view(&chapters, &secondary)
        };
        type IsChoice = fn(&PlayerControlAction) -> bool;
        let choices: [(ControlId, IsChoice); 4] = [
            (ControlId::SpeedMenu, |action| {
                matches!(action, PlayerControlAction::SetSpeed(_))
            }),
            (ControlId::AudioMenu, |action| {
                matches!(action, PlayerControlAction::SelectAudioTrack(_))
            }),
            (ControlId::SubtitlesMenu, |action| {
                matches!(action, PlayerControlAction::SelectSubtitleTrack(_))
            }),
            (ControlId::VideoMenu, |action| {
                matches!(action, PlayerControlAction::SetAspect(_))
            }),
        ];
        for (target, is_choice) in choices {
            let context = Context::default();
            assert!(tab_to(&context, state, target));
            let opened = overlay_frame(&context, key_press(egui::Key::Enter), state);
            assert!(opened.actions.is_empty(), "Enter opens {target:?}");
            overlay_frame(&context, RawInput::default(), state);
            let mut chosen = Vec::new();
            for _ in 0..TAB_WALK_LIMIT {
                chosen.extend(overlay_frame(&context, key_press(egui::Key::Enter), state).actions);
                if chosen.iter().any(is_choice) {
                    break;
                }
                overlay_frame(&context, key_press(egui::Key::Tab), state);
            }
            assert!(
                chosen.iter().any(is_choice),
                "{target:?} takes an item with Tab and Enter, got {chosen:?}"
            );
        }
    }

    #[test]
    fn host_subtitle_extra_is_drawn_only_in_the_subtitles_popover() {
        let context = Context::default();
        let state = view(&[]);
        let layout = overlay_frame(&context, RawInput::default(), state);
        let center = |id| {
            layout
                .rects
                .iter()
                .find(|(candidate, _)| *candidate == id)
                .map(|(_, rect)| rect.center())
                .unwrap_or_else(|| panic!("{id:?} is drawn"))
        };
        let mut extra_calls = 0;
        for group in [
            PlaybackPopover::Speed,
            PlaybackPopover::Audio,
            PlaybackPopover::Video,
        ] {
            overlay_frame_with_extra(
                &context,
                RawInput {
                    events: click_events(center(group.control())),
                    ..RawInput::default()
                },
                state,
                |ui| {
                    extra_calls += 1;
                    ui.label("Host subtitle sync");
                },
            );
            assert!(egui::Popup::is_id_open(&context, group.id()));
            for other in [
                PlaybackPopover::Speed,
                PlaybackPopover::Audio,
                PlaybackPopover::Subtitles,
                PlaybackPopover::Video,
            ] {
                assert_eq!(
                    egui::Popup::is_id_open(&context, other.id()),
                    other == group,
                    "opening {group:?} leaves only that group open"
                );
            }
        }
        assert_eq!(extra_calls, 0, "non-subtitle groups never draw host extras");

        overlay_frame_with_extra(
            &context,
            RawInput {
                events: click_events(center(ControlId::SubtitlesMenu)),
                ..RawInput::default()
            },
            state,
            |ui| {
                extra_calls += 1;
                ui.label("Host subtitle sync");
            },
        );
        if extra_calls == 0 {
            overlay_frame_with_extra(&context, RawInput::default(), state, |ui| {
                extra_calls += 1;
                ui.label("Host subtitle sync");
            });
        }
        assert!(
            extra_calls > 0,
            "the subtitle group draws the host sync controls"
        );
        assert!(egui::Popup::is_id_open(
            &context,
            PlaybackPopover::Subtitles.id()
        ));
        assert!(!egui::Popup::is_id_open(
            &context,
            PlaybackPopover::Video.id()
        ));
    }

    /// The fill and stroke egui paints for the button at `id` in `frame`.
    fn button_paint(frame: &OverlayFrame, id: ControlId) -> Option<(Color32, Stroke)> {
        let center = frame
            .rects
            .iter()
            .find(|(candidate, _)| *candidate == id)?
            .1
            .center();
        frame
            .shapes
            .iter()
            .find_map(|clipped| match &clipped.shape {
                egui::Shape::Rect(shape) if (shape.rect.center() - center).length() < 1.0 => {
                    Some((shape.fill, shape.stroke))
                }
                _ => None,
            })
    }

    #[test]
    fn the_focused_button_is_painted_differently_from_its_neighbours() {
        let chapters = [];
        let secondary = [];
        let context = Context::default();
        let state = keyboard_view(&chapters, &secondary);
        assert!(
            tab_to(&context, state, ControlId::PlayPause),
            "Tab reaches play"
        );
        // egui reads a button's style from the previous frame, so one idle frame shows the focus.
        let frame = overlay_frame(&context, RawInput::default(), state);
        assert_eq!(
            focused_control(&context, &frame.rects),
            Some(ControlId::PlayPause),
            "focus stays on play while the frame is idle"
        );
        let focused = button_paint(&frame, ControlId::PlayPause).expect("play is painted");
        let neighbour = button_paint(&frame, ControlId::NextTrack).expect("next is painted");
        assert_ne!(
            focused, neighbour,
            "the focused button shows a different fill or outline"
        );
    }

    /// Tab presses allowed for one walk through the track section (header, audio, subtitle).
    const TRACK_TAB_LIMIT: usize = 8;

    /// Idle frames allowed for the header's open animation before the combos must be drawn.
    const TRACK_SETTLE_LIMIT: usize = 240;

    fn audio_choices() -> Vec<TrackChoice<&'static str>> {
        vec![
            TrackChoice {
                value: "automatic",
                label: "Automatic".into(),
                selected: true,
            },
            TrackChoice {
                value: "english-5.1",
                label: "English 5.1".into(),
                selected: false,
            },
            TrackChoice {
                value: "commentary",
                label: "Commentary".into(),
                selected: false,
            },
        ]
    }

    fn subtitle_choices() -> Vec<TrackChoice<&'static str>> {
        vec![
            TrackChoice {
                value: "off",
                label: "Off".into(),
                selected: false,
            },
            TrackChoice {
                value: "english",
                label: "English".into(),
                selected: true,
            },
            TrackChoice {
                value: "french",
                label: "French".into(),
                selected: false,
            },
        ]
    }

    /// One frame of the track section: the picks, and each widget with its egui id.
    struct TrackFrame {
        audio: Option<&'static str>,
        subtitle: Option<&'static str>,
        observed: Vec<(TrackControl, egui::Id)>,
    }

    fn track_frame(
        context: &Context,
        input: RawInput,
        audio: &[TrackChoice<&'static str>],
        subtitle: &[TrackChoice<&'static str>],
    ) -> TrackFrame {
        let mut observed = Vec::new();
        let mut picked: (Option<&'static str>, Option<&'static str>) = (None, None);
        let mut output = context.run_ui(input, |ui| {
            CentralPanel::default().show(ui, |ui| {
                picked =
                    track_section_inner(ui, Some(audio), Some(subtitle), None, |control, id| {
                        observed.push((control, id));
                    });
            });
        });
        output.textures_delta.clear();
        TrackFrame {
            audio: picked.0,
            subtitle: picked.1,
            observed,
        }
    }

    #[test]
    fn narrow_track_menus_fit_200px_viewport_with_long_track_names() {
        let audio = [TrackChoice {
            value: 7,
            label: "English Dolby TrueHD 7.1 immersive commentary track".to_owned(),
            selected: true,
        }];
        let subtitle = [TrackChoice {
            value: 12,
            label: "English SDH forced descriptive subtitles for director commentary".to_owned(),
            selected: true,
        }];
        let context = Context::default();
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(200.0, 320.0));
        let mut contents_rect = None;
        let mut output = context.run_ui(
            RawInput {
                screen_rect: Some(screen),
                ..RawInput::default()
            },
            |ui| {
                CentralPanel::default().show(ui, |ui| {
                    let picked = track_menu_contents(ui, Some(&audio), Some(&subtitle), None);
                    assert_eq!(picked, (None, None));
                    contents_rect = Some(ui.min_rect());
                });
            },
        );
        output.textures_delta.clear();
        let contents_rect = contents_rect.expect("track menus were rendered");
        assert!(
            contents_rect.min.x >= screen.min.x,
            "track labels start inside viewport: {contents_rect:?}"
        );
        assert!(
            contents_rect.max.x <= screen.max.x,
            "track labels and menus fit the viewport: {contents_rect:?}"
        );
    }

    #[test]
    fn audio_and_subtitle_track_combos_have_accessible_names() {
        let context = Context::default();
        context.enable_accesskit();
        let audio = audio_choices();
        let subtitle = subtitle_choices();
        let mut output = context.run_ui(RawInput::default(), |ui| {
            CentralPanel::default().show(ui, |ui| {
                let picked = track_menu_contents(ui, Some(&audio), Some(&subtitle), None);
                assert_eq!(picked, (None, None));
            });
        });
        output.textures_delta.clear();
        let tree = output
            .platform_output
            .accesskit_update
            .expect("accessibility tree is emitted");
        let combos: Vec<_> = tree
            .nodes
            .iter()
            .filter(|(_, node)| node.role() == egui::Role::ComboBox)
            .collect();
        assert_eq!(combos.len(), 2, "audio and subtitle combos are exposed");
        for (_, node) in combos {
            assert!(
                node.label().is_some_and(|label| !label.trim().is_empty())
                    || !node.labelled_by().is_empty(),
                "track combo has an accessible name"
            );
        }
    }

    /// The section widget that has keyboard focus, matched by egui's id for the widget.
    fn focused_track_control(
        context: &Context,
        observed: &[(TrackControl, egui::Id)],
    ) -> Option<TrackControl> {
        let focused = context.memory(|memory| memory.focused())?;
        observed
            .iter()
            .find(|(_, id)| *id == focused)
            .map(|(control, _)| *control)
    }

    fn track_tab_to(
        context: &Context,
        audio: &[TrackChoice<&'static str>],
        subtitle: &[TrackChoice<&'static str>],
        target: TrackControl,
    ) -> bool {
        (0..TRACK_TAB_LIMIT).any(|_| {
            let frame = track_frame(context, key_press(egui::Key::Tab), audio, subtitle);
            focused_track_control(context, &frame.observed) == Some(target)
        })
    }

    /// Open the header with Enter, then idle until the audio combo is drawn. The header opens
    /// with an animation, so the combo is waited for rather than assumed on the next frame.
    fn open_track_header(
        context: &Context,
        audio: &[TrackChoice<&'static str>],
        subtitle: &[TrackChoice<&'static str>],
    ) {
        track_frame(context, key_press(egui::Key::Enter), audio, subtitle);
        for _ in 0..TRACK_SETTLE_LIMIT {
            let frame = track_frame(context, RawInput::default(), audio, subtitle);
            if frame
                .observed
                .iter()
                .any(|(control, _)| *control == TrackControl::Audio)
            {
                return;
            }
        }
    }

    /// Tab to the header, Enter to open it, Tab to the combo, Enter to open it, Tab to the entry,
    /// and Enter to choose it. Returns what the section picked on that last Enter.
    fn pick_by_keyboard(
        context: &Context,
        audio: &[TrackChoice<&'static str>],
        subtitle: &[TrackChoice<&'static str>],
        combo: TrackControl,
        entry: TrackControl,
    ) -> Option<&'static str> {
        assert!(
            track_tab_to(context, audio, subtitle, TrackControl::Header),
            "Tab reaches the tracks header"
        );
        open_track_header(context, audio, subtitle);
        assert!(
            track_tab_to(context, audio, subtitle, combo),
            "Tab reaches {combo:?}"
        );
        track_frame(context, key_press(egui::Key::Enter), audio, subtitle);
        assert!(
            track_tab_to(context, audio, subtitle, entry),
            "Tab reaches {entry:?} in the open popup"
        );
        let frame = track_frame(context, key_press(egui::Key::Enter), audio, subtitle);
        match combo {
            TrackControl::Subtitle => frame.subtitle,
            _ => frame.audio,
        }
    }

    #[test]
    fn track_combos_are_hidden_until_the_header_is_opened_from_the_keyboard() {
        let (audio, subtitle) = (audio_choices(), subtitle_choices());
        let context = Context::default();
        assert!(
            track_tab_to(&context, &audio, &subtitle, TrackControl::Header),
            "Tab reaches the tracks header"
        );
        let closed = track_frame(&context, RawInput::default(), &audio, &subtitle);
        assert!(
            !closed
                .observed
                .iter()
                .any(|(control, _)| *control == TrackControl::Audio),
            "the combos are not drawn while the header is closed"
        );
        open_track_header(&context, &audio, &subtitle);
        let open = track_frame(&context, RawInput::default(), &audio, &subtitle);
        assert!(
            open.observed
                .iter()
                .any(|(control, _)| *control == TrackControl::Audio),
            "Enter on the header draws the audio combo"
        );
        assert!(
            open.observed
                .iter()
                .any(|(control, _)| *control == TrackControl::Subtitle),
            "Enter on the header draws the subtitle combo"
        );
    }

    #[test]
    fn changing_the_selected_track_keeps_keyboard_focus_on_its_combo() {
        let mut audio = audio_choices();
        let subtitle = subtitle_choices();
        let context = Context::default();
        assert!(track_tab_to(
            &context,
            &audio,
            &subtitle,
            TrackControl::Header
        ));
        open_track_header(&context, &audio, &subtitle);
        assert!(track_tab_to(
            &context,
            &audio,
            &subtitle,
            TrackControl::Audio
        ));

        audio[0].selected = false;
        audio[1].selected = true;
        let frame = track_frame(&context, RawInput::default(), &audio, &subtitle);
        assert_eq!(
            focused_track_control(&context, &frame.observed),
            Some(TrackControl::Audio),
            "the combo retains focus when playback reports a new track"
        );
    }

    #[test]
    fn every_track_entry_is_chosen_from_the_keyboard() {
        let (audio, subtitle) = (audio_choices(), subtitle_choices());
        // A fresh context per entry, so focus and popup state from one pick do not leak.
        for (index, choice) in audio.iter().enumerate() {
            let context = Context::default();
            let picked = pick_by_keyboard(
                &context,
                &audio,
                &subtitle,
                TrackControl::Audio,
                TrackControl::AudioEntry(index),
            );
            assert_eq!(
                picked,
                Some(choice.value),
                "audio entry {index} ({}) is chosen from the keyboard",
                choice.label
            );
        }
        for (index, choice) in subtitle.iter().enumerate() {
            let context = Context::default();
            let picked = pick_by_keyboard(
                &context,
                &audio,
                &subtitle,
                TrackControl::Subtitle,
                TrackControl::SubtitleEntry(index),
            );
            assert_eq!(
                picked,
                Some(choice.value),
                "subtitle entry {index} ({}) is chosen from the keyboard",
                choice.label
            );
        }
    }
}
