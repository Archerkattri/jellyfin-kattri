//! Pure intro and outro skip model for one playback item. Given the server's
//! segments, the playback position, and the item duration, it decides whether a
//! skip button should be offered, and whether to seek past a segment without a
//! press. Performs no I/O. The caller fetches the segments, observes the position,
//! and performs the seek.
//!
//! Server data: `GET /MediaSegments/{itemId}`. Confirmed against the Jellyfin
//! OpenAPI stable spec (read 2026-10-15): the path exists, and `MediaSegmentDto`
//! has `Id`, `ItemId`, `Type` (`Intro` and `Outro` are among its values),
//! `StartTicks`, and `EndTicks` (int64). This module does not call the endpoint.
//!
//! Two switches per segment type (owner decision, 2026-10-20, "Button, then
//! auto"). The button switch shows a skip button. The automatic switch seeks
//! without a press, and applies only when the button switch is on too. With both
//! button switches off, the segments are not requested and nothing is skipped.
//!
//! Timing follows the legacy player (`legacy/qt-neo/native/mpvVideoPlayer.js`,
//! `pickSegmentSkip`), converted to ticks (10,000 ticks per millisecond):
//! - An intro triggers from its start. An outro triggers from its start plus the
//!   outro delay.
//! - Triggering stops 1 s before the segment end.
//! - The seek target is the segment end, or `duration - 500 ms` when that is
//!   earlier. A target not after the current position is never used, so the
//!   player never skips backward.

use std::collections::BTreeSet;

/// Jellyfin ticks per millisecond (one tick is 100 ns).
pub const TICKS_PER_MS: u64 = 10_000;

/// Largest outro delay the settings accept, in milliseconds.
pub const MAX_OUTRO_DELAY_MS: u32 = 60_000;

const TRIGGER_STOP_BEFORE_END_TICKS: u64 = 1_000 * TICKS_PER_MS;
const SEEK_BEFORE_DURATION_TICKS: u64 = 500 * TICKS_PER_MS;

pub const fn ms_to_ticks(ms: u64) -> u64 {
    ms.saturating_mul(TICKS_PER_MS)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SegmentKind {
    Intro,
    Outro,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SegmentError {
    EmptyId,
    EndNotAfterStart,
    OutroDelayOutOfRange,
}

/// One intro or outro segment. Start and end are in ticks, and `end > start`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MediaSegment {
    id: String,
    kind: SegmentKind,
    start_ticks: u64,
    end_ticks: u64,
}

impl MediaSegment {
    pub fn new(
        id: &str,
        kind: SegmentKind,
        start_ticks: u64,
        end_ticks: u64,
    ) -> Result<Self, SegmentError> {
        if id.is_empty() {
            return Err(SegmentError::EmptyId);
        }
        if end_ticks <= start_ticks {
            return Err(SegmentError::EndNotAfterStart);
        }
        Ok(Self {
            id: id.to_owned(),
            kind,
            start_ticks,
            end_ticks,
        })
    }

    /// Map one server segment (`Id`, `Type`, `StartTicks`, `EndTicks`). Returns
    /// `None` for anything this model does not use: a type other than `Intro` or
    /// `Outro`, an empty id, a negative start, or an end not after the start.
    pub fn from_server(
        id: &str,
        segment_type: &str,
        start_ticks: i64,
        end_ticks: i64,
    ) -> Option<Self> {
        let kind = match segment_type {
            "Intro" => SegmentKind::Intro,
            "Outro" => SegmentKind::Outro,
            _ => return None,
        };
        let start = u64::try_from(start_ticks).ok()?;
        let end = u64::try_from(end_ticks).ok()?;
        Self::new(id, kind, start, end).ok()
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn kind(&self) -> SegmentKind {
        self.kind
    }

    pub fn start_ticks(&self) -> u64 {
        self.start_ticks
    }

    pub fn end_ticks(&self) -> u64 {
        self.end_ticks
    }
}

/// User settings for segment skip. Every switch defaults to off.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SkipSettings {
    button_intro: bool,
    button_outro: bool,
    auto_intro: bool,
    auto_outro: bool,
    outro_delay_ms: u32,
}

impl SkipSettings {
    pub fn new(
        button_intro: bool,
        button_outro: bool,
        auto_intro: bool,
        auto_outro: bool,
        outro_delay_ms: u32,
    ) -> Result<Self, SegmentError> {
        if outro_delay_ms > MAX_OUTRO_DELAY_MS {
            return Err(SegmentError::OutroDelayOutOfRange);
        }
        Ok(Self {
            button_intro,
            button_outro,
            auto_intro,
            auto_outro,
            outro_delay_ms,
        })
    }

    /// True when the skip button for this type should be shown.
    pub fn shows_button(&self, kind: SegmentKind) -> bool {
        match kind {
            SegmentKind::Intro => self.button_intro,
            SegmentKind::Outro => self.button_outro,
        }
    }

    /// True when this type is skipped without a press. The automatic switch has
    /// no effect unless the button switch for the same type is on.
    pub fn auto_skips(&self, kind: SegmentKind) -> bool {
        self.shows_button(kind)
            && match kind {
                SegmentKind::Intro => self.auto_intro,
                SegmentKind::Outro => self.auto_outro,
            }
    }

    pub fn outro_delay_ms(&self) -> u32 {
        self.outro_delay_ms
    }

    /// True when the segments endpoint should be requested. With both button
    /// switches off it is not requested at all.
    pub fn fetch_needed(&self) -> bool {
        self.button_intro || self.button_outro
    }
}

/// A seek to perform: skip `segment_id` by seeking to `seek_to_ticks`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkipDecision {
    pub segment_id: String,
    pub kind: SegmentKind,
    pub seek_to_ticks: u64,
}

/// Which switch decides eligibility: the button, or the automatic seek.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Button,
    Automatic,
}

/// The seek target for one segment at this position, or `None` when the segment
/// should not be skipped now.
fn seek_target(
    segment: &MediaSegment,
    skipped: &BTreeSet<String>,
    position_ticks: u64,
    duration_ticks: Option<u64>,
    settings: &SkipSettings,
    mode: Mode,
) -> Option<u64> {
    let enabled = match mode {
        Mode::Button => settings.shows_button(segment.kind),
        Mode::Automatic => settings.auto_skips(segment.kind),
    };
    if !enabled || skipped.contains(&segment.id) {
        return None;
    }
    // A segment that starts after the item ends is ignored.
    if duration_ticks.is_some_and(|duration| segment.start_ticks >= duration) {
        return None;
    }
    let trigger = match segment.kind {
        SegmentKind::Intro => segment.start_ticks,
        SegmentKind::Outro => segment
            .start_ticks
            .saturating_add(ms_to_ticks(u64::from(settings.outro_delay_ms))),
    };
    let trigger_until = segment
        .end_ticks
        .saturating_sub(TRIGGER_STOP_BEFORE_END_TICKS);
    if position_ticks < trigger || position_ticks >= trigger_until {
        return None;
    }
    let mut target = segment.end_ticks;
    if let Some(duration) = duration_ticks {
        target = target.min(duration.saturating_sub(SEEK_BEFORE_DURATION_TICKS));
    }
    // Never skip backward.
    (target > position_ticks).then_some(target)
}

fn find_skip(
    segments: &[MediaSegment],
    skipped: &BTreeSet<String>,
    position_ticks: u64,
    duration_ticks: Option<u64>,
    settings: &SkipSettings,
    mode: Mode,
) -> Option<SkipDecision> {
    segments
        .iter()
        .filter_map(|segment| {
            seek_target(
                segment,
                skipped,
                position_ticks,
                duration_ticks,
                settings,
                mode,
            )
            .map(|target| (segment, target))
        })
        .min_by_key(|(segment, _)| segment.start_ticks)
        .map(|(segment, target)| SkipDecision {
            segment_id: segment.id.clone(),
            kind: segment.kind,
            seek_to_ticks: target,
        })
}

/// The automatic skip to perform now, or `None`. When several are eligible, the
/// one that starts first wins. `skipped` holds the segment ids already skipped in
/// this item. The function does not record a skip; see [`SegmentSkipper::pick`].
pub fn pick_segment_skip(
    segments: &[MediaSegment],
    skipped: &BTreeSet<String>,
    position_ticks: u64,
    duration_ticks: Option<u64>,
    settings: &SkipSettings,
) -> Option<SkipDecision> {
    find_skip(
        segments,
        skipped,
        position_ticks,
        duration_ticks,
        settings,
        Mode::Automatic,
    )
}

/// The skip a button should offer now, or `None`. Same timing as
/// [`pick_segment_skip`], gated by the button switch instead of the automatic one.
pub fn button_skip(
    segments: &[MediaSegment],
    skipped: &BTreeSet<String>,
    position_ticks: u64,
    duration_ticks: Option<u64>,
    settings: &SkipSettings,
) -> Option<SkipDecision> {
    find_skip(
        segments,
        skipped,
        position_ticks,
        duration_ticks,
        settings,
        Mode::Button,
    )
}

/// Segments for the current item, and the ids already skipped in it.
#[derive(Clone, Debug, Default)]
pub struct SegmentSkipper {
    item_id: Option<String>,
    segments: Vec<MediaSegment>,
    skipped: BTreeSet<String>,
}

impl SegmentSkipper {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn item_id(&self) -> Option<&str> {
        self.item_id.as_deref()
    }

    /// Replace the segments for `item_id`. Moving to a different item clears the
    /// skipped ids. The caller drops responses for an item that is no longer
    /// current, before calling this.
    pub fn load(&mut self, item_id: &str, segments: Vec<MediaSegment>) {
        if self.item_id.as_deref() != Some(item_id) {
            self.skipped.clear();
            self.item_id = Some(item_id.to_owned());
        }
        self.segments = segments;
    }

    /// Decide on an automatic skip at this position. A returned decision is
    /// recorded, so the same segment is not skipped again in this item.
    pub fn pick(
        &mut self,
        position_ticks: u64,
        duration_ticks: Option<u64>,
        settings: &SkipSettings,
    ) -> Option<SkipDecision> {
        let decision = pick_segment_skip(
            &self.segments,
            &self.skipped,
            position_ticks,
            duration_ticks,
            settings,
        )?;
        self.skipped.insert(decision.segment_id.clone());
        Some(decision)
    }

    /// The skip the button should offer now. Nothing is recorded until the user
    /// presses it; the caller then calls [`SegmentSkipper::mark_skipped`].
    pub fn button(
        &self,
        position_ticks: u64,
        duration_ticks: Option<u64>,
        settings: &SkipSettings,
    ) -> Option<SkipDecision> {
        button_skip(
            &self.segments,
            &self.skipped,
            position_ticks,
            duration_ticks,
            settings,
        )
    }

    /// Record that the user skipped this segment, so it is not offered again in
    /// this item.
    pub fn mark_skipped(&mut self, segment_id: &str) {
        self.skipped.insert(segment_id.to_owned());
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::{
        MediaSegment, SegmentError, SegmentKind, SegmentSkipper, SkipSettings, button_skip,
        ms_to_ticks, pick_segment_skip,
    };

    /// One second in ticks.
    const SECOND: u64 = 10_000_000;
    const SECOND_I: i64 = 10_000_000;

    fn intro(start_s: u64, end_s: u64) -> MediaSegment {
        MediaSegment::new(
            "intro-1",
            SegmentKind::Intro,
            start_s * SECOND,
            end_s * SECOND,
        )
        .expect("intro")
    }

    fn outro(start_s: u64, end_s: u64) -> MediaSegment {
        MediaSegment::new(
            "outro-1",
            SegmentKind::Outro,
            start_s * SECOND,
            end_s * SECOND,
        )
        .expect("outro")
    }

    /// Intro button shown, no automatic seek.
    fn intro_button() -> SkipSettings {
        SkipSettings::new(true, false, false, false, 0).expect("settings")
    }

    /// Intro button shown and automatic seek on.
    fn intro_auto() -> SkipSettings {
        SkipSettings::new(true, false, true, false, 0).expect("settings")
    }

    /// Outro button shown and automatic seek on, with the given delay.
    fn outro_auto(delay_ms: u32) -> SkipSettings {
        SkipSettings::new(false, true, false, true, delay_ms).expect("settings")
    }

    fn none() -> BTreeSet<String> {
        BTreeSet::new()
    }

    #[test]
    fn position_table_around_an_intro() {
        // Intro from 10 s to 30 s, item 600 s long, so the duration clamp is not
        // what limits the target.
        let segments = vec![intro(10, 30)];
        let settings = intro_auto();
        let duration = Some(600 * SECOND);
        let pick = |position: u64| {
            pick_segment_skip(&segments, &none(), position, duration, &settings)
                .map(|decision| decision.seek_to_ticks)
        };

        assert_eq!(pick(10 * SECOND - 1), None, "before start");
        assert_eq!(pick(10 * SECOND), Some(30 * SECOND), "at start");
        assert_eq!(pick(20 * SECOND), Some(30 * SECOND), "mid");
        assert_eq!(pick(29 * SECOND), None, "at end minus 1 s");
        assert_eq!(
            pick(30 * SECOND - ms_to_ticks(999)),
            None,
            "at end minus 999 ms"
        );
        assert_eq!(
            pick(30 * SECOND - ms_to_ticks(1_001)),
            Some(30 * SECOND),
            "at end minus 1001 ms"
        );
        assert_eq!(pick(31 * SECOND), None, "after end");
    }

    #[test]
    fn outro_triggers_after_the_delay() {
        // Outro from 500 s to 600 s with a 5 s delay: triggers from 505 s.
        let segments = vec![outro(500, 600)];
        let settings = outro_auto(5_000);
        let duration = Some(700 * SECOND);
        let pick = |position: u64| {
            pick_segment_skip(&segments, &none(), position, duration, &settings)
                .map(|decision| decision.seek_to_ticks)
        };
        assert_eq!(pick(500 * SECOND), None, "start, before the delay");
        assert_eq!(pick(505 * SECOND - 1), None, "just before the delay ends");
        assert_eq!(pick(505 * SECOND), Some(600 * SECOND), "delay reached");
        assert_eq!(pick(550 * SECOND), Some(600 * SECOND), "mid after delay");
    }

    #[test]
    fn outro_without_delay_triggers_from_its_start() {
        let segments = vec![outro(500, 600)];
        let settings = outro_auto(0);
        let decision = pick_segment_skip(
            &segments,
            &none(),
            500 * SECOND,
            Some(700 * SECOND),
            &settings,
        )
        .expect("decision");
        assert_eq!(decision.kind, SegmentKind::Outro);
        assert_eq!(decision.seek_to_ticks, 600 * SECOND);
    }

    #[test]
    fn outro_button_uses_the_delay() {
        let segments = vec![outro(500, 600)];
        let settings = SkipSettings::new(false, true, false, false, 5_000).expect("settings");
        let duration = Some(700 * SECOND);
        assert_eq!(
            button_skip(&segments, &none(), 504 * SECOND, duration, &settings),
            None,
            "before the delay"
        );
        let decision = button_skip(&segments, &none(), 505 * SECOND, duration, &settings)
            .expect("button after delay");
        assert_eq!(decision.seek_to_ticks, 600 * SECOND);
    }

    #[test]
    fn skips_a_segment_once_per_item() {
        let mut skipper = SegmentSkipper::new();
        skipper.load("episode-1", vec![intro(10, 30)]);
        let settings = intro_auto();
        let duration = Some(600 * SECOND);

        assert!(skipper.pick(10 * SECOND, duration, &settings).is_some());
        // Seeking back into the intro does not skip it again.
        assert_eq!(skipper.pick(12 * SECOND, duration, &settings), None);
        assert_eq!(skipper.pick(10 * SECOND, duration, &settings), None);
    }

    #[test]
    fn a_new_item_clears_skipped_ids() {
        let mut skipper = SegmentSkipper::new();
        let settings = intro_auto();
        skipper.load("episode-1", vec![intro(10, 30)]);
        assert!(skipper.pick(10 * SECOND, None, &settings).is_some());

        skipper.load("episode-2", vec![intro(10, 30)]);
        assert_eq!(skipper.item_id(), Some("episode-2"));
        assert!(skipper.pick(10 * SECOND, None, &settings).is_some());
    }

    #[test]
    fn button_shows_without_an_automatic_seek() {
        let mut skipper = SegmentSkipper::new();
        skipper.load("episode-1", vec![intro(10, 30)]);
        let settings = intro_button();
        let duration = Some(600 * SECOND);

        assert_eq!(
            skipper.pick(10 * SECOND, duration, &settings),
            None,
            "no automatic seek"
        );
        assert!(skipper.button(10 * SECOND, duration, &settings).is_some());
        // Showing the button records nothing.
        assert!(skipper.button(10 * SECOND, duration, &settings).is_some());
    }

    #[test]
    fn pressing_the_button_skips_once() {
        let mut skipper = SegmentSkipper::new();
        skipper.load("episode-1", vec![intro(10, 30)]);
        let settings = intro_button();
        let duration = Some(600 * SECOND);

        let offered = skipper
            .button(10 * SECOND, duration, &settings)
            .expect("button offered");
        skipper.mark_skipped(&offered.segment_id);
        assert_eq!(skipper.button(12 * SECOND, duration, &settings), None);
        assert_eq!(skipper.pick(12 * SECOND, duration, &settings), None);
    }

    #[test]
    fn automatic_skip_hides_the_button() {
        let mut skipper = SegmentSkipper::new();
        skipper.load("episode-1", vec![intro(10, 30)]);
        let settings = intro_auto();
        let duration = Some(600 * SECOND);

        assert!(skipper.pick(10 * SECOND, duration, &settings).is_some());
        assert_eq!(skipper.button(11 * SECOND, duration, &settings), None);
    }

    #[test]
    fn automatic_switch_needs_the_button_switch() {
        // Auto on, button off: the type is not offered and not skipped.
        let settings = SkipSettings::new(false, false, true, true, 0).expect("settings");
        let segments = vec![intro(10, 30), outro(500, 600)];
        let duration = Some(700 * SECOND);
        assert!(!settings.auto_skips(SegmentKind::Intro));
        assert!(!settings.auto_skips(SegmentKind::Outro));
        assert!(!settings.fetch_needed());
        assert_eq!(
            pick_segment_skip(&segments, &none(), 10 * SECOND, duration, &settings),
            None
        );
        assert_eq!(
            button_skip(&segments, &none(), 10 * SECOND, duration, &settings),
            None
        );
    }

    #[test]
    fn unknown_types_are_ignored_by_the_server_mapping() {
        assert!(MediaSegment::from_server("a", "Recap", 0, SECOND_I).is_none());
        assert!(MediaSegment::from_server("b", "Commercial", 0, SECOND_I).is_none());
        assert!(MediaSegment::from_server("c", "Unknown", 0, SECOND_I).is_none());
        assert!(MediaSegment::from_server("d", "Intro", 0, SECOND_I).is_some());
        assert!(MediaSegment::from_server("e", "Outro", 0, SECOND_I).is_some());
    }

    #[test]
    fn invalid_server_segments_are_dropped() {
        assert!(
            MediaSegment::from_server("", "Intro", 0, SECOND_I).is_none(),
            "empty id"
        );
        assert!(
            MediaSegment::from_server("a", "Intro", -1, SECOND_I).is_none(),
            "negative start"
        );
        assert!(
            MediaSegment::from_server("b", "Intro", SECOND_I, SECOND_I).is_none(),
            "end equals start"
        );
        assert!(
            MediaSegment::from_server("c", "Intro", SECOND_I, 0).is_none(),
            "end before start"
        );
        assert!(
            MediaSegment::from_server("d", "Intro", 0, -5).is_none(),
            "negative end"
        );
    }

    #[test]
    fn end_not_after_start_is_rejected() {
        assert_eq!(
            MediaSegment::new("a", SegmentKind::Intro, SECOND, SECOND).err(),
            Some(SegmentError::EndNotAfterStart)
        );
        assert_eq!(
            MediaSegment::new("a", SegmentKind::Intro, 2 * SECOND, SECOND).err(),
            Some(SegmentError::EndNotAfterStart)
        );
        assert_eq!(
            MediaSegment::new("", SegmentKind::Intro, 0, SECOND).err(),
            Some(SegmentError::EmptyId)
        );
    }

    #[test]
    fn duration_clamps_the_seek_target() {
        // Intro from 10 s to 300 s in a 200 s item: the target is 200 s - 500 ms,
        // not 300 s.
        let segments = vec![intro(10, 300)];
        let settings = intro_auto();
        let duration = Some(200 * SECOND);
        let target = 199 * SECOND + SECOND / 2;
        let decision = pick_segment_skip(&segments, &none(), 100 * SECOND, duration, &settings)
            .expect("decision");
        assert_eq!(decision.seek_to_ticks, target);

        // Past the clamped target there is nothing to skip to, so no backward seek.
        assert_eq!(
            pick_segment_skip(
                &segments,
                &none(),
                199 * SECOND + SECOND * 8 / 10,
                duration,
                &settings
            ),
            None
        );
    }

    #[test]
    fn segment_starting_after_the_end_is_ignored() {
        let segments = vec![intro(250, 280)];
        let settings = intro_auto();
        assert_eq!(
            pick_segment_skip(
                &segments,
                &none(),
                250 * SECOND,
                Some(200 * SECOND),
                &settings
            ),
            None
        );
    }

    #[test]
    fn settings_off_skip_nothing_and_need_no_fetch() {
        let segments = vec![intro(10, 30), outro(500, 600)];
        let off = SkipSettings::default();
        assert!(!off.fetch_needed());
        assert_eq!(
            pick_segment_skip(&segments, &none(), 10 * SECOND, None, &off),
            None
        );
        assert_eq!(
            button_skip(&segments, &none(), 10 * SECOND, None, &off),
            None
        );
        assert!(intro_button().fetch_needed());
        assert!(intro_auto().fetch_needed());
        assert!(outro_auto(0).fetch_needed());
    }

    #[test]
    fn each_setting_only_affects_its_own_type() {
        let segments = vec![intro(10, 30), outro(500, 600)];
        // Intro on, outro off: the outro is never skipped.
        let settings = intro_auto();
        assert_eq!(
            pick_segment_skip(
                &segments,
                &none(),
                550 * SECOND,
                Some(700 * SECOND),
                &settings
            ),
            None
        );
        assert_eq!(
            button_skip(
                &segments,
                &none(),
                550 * SECOND,
                Some(700 * SECOND),
                &settings
            ),
            None
        );
    }

    #[test]
    fn outro_delay_is_bounded() {
        assert!(SkipSettings::new(true, true, true, true, 60_000).is_ok());
        assert_eq!(
            SkipSettings::new(true, true, true, true, 60_001).err(),
            Some(SegmentError::OutroDelayOutOfRange)
        );
    }

    #[test]
    fn earliest_eligible_segment_wins() {
        let segments = vec![
            MediaSegment::new("late", SegmentKind::Intro, 20 * SECOND, 40 * SECOND).expect("late"),
            MediaSegment::new("early", SegmentKind::Intro, 10 * SECOND, 40 * SECOND)
                .expect("early"),
        ];
        let settings = intro_auto();
        let decision =
            pick_segment_skip(&segments, &none(), 25 * SECOND, None, &settings).expect("decision");
        assert_eq!(decision.segment_id, "early");
    }
}
