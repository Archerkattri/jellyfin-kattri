//! Pure chapter model.
//!
//! A [`ChapterList`] holds chapters in playback order with distinct start positions. Positions
//! and durations are Jellyfin ticks, 10,000,000 per second, the same unit used by
//! `jellyfin_ticks_to_duration` in the crate root. The list performs no I/O. The caller supplies
//! the playback position and acts on the index it gets back.
//!
//! Rules:
//! - Titles have control characters removed, then are truncated to [`MAX_TITLE_CHARS`]
//!   characters, then have surrounding whitespace trimmed. An empty title is allowed.
//! - Start positions must be zero or more.
//! - [`ChapterList::new`] sorts by start. When two chapters share a start, the one given first
//!   is kept and the later one is dropped.
//! - When a duration is supplied, every start must lie within `0..=duration`.
//! - Next and previous lookups clamp at the ends of the list instead of wrapping.

use std::fmt;

/// Longest chapter title, in characters, after control characters are removed.
pub const MAX_TITLE_CHARS: usize = 120;

/// Why a chapter or chapter list was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChapterError {
    NegativeStart,
    NegativeDuration,
    StartBeyondDuration,
}

impl fmt::Display for ChapterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NegativeStart => f.write_str("chapter start is negative"),
            Self::NegativeDuration => f.write_str("duration is negative"),
            Self::StartBeyondDuration => f.write_str("chapter start is past the duration"),
        }
    }
}

impl std::error::Error for ChapterError {}

/// One chapter: a start position in ticks and a sanitized title.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Chapter {
    start_ticks: i64,
    title: String,
}

impl Chapter {
    /// Build a chapter. The title is sanitized as described in the module notes.
    pub fn new(start_ticks: i64, title: &str) -> Result<Self, ChapterError> {
        if start_ticks < 0 {
            return Err(ChapterError::NegativeStart);
        }
        Ok(Self {
            start_ticks,
            title: sanitize_title(title),
        })
    }

    pub const fn start_ticks(&self) -> i64 {
        self.start_ticks
    }

    pub fn title(&self) -> &str {
        &self.title
    }
}

/// Remove control characters, truncate to [`MAX_TITLE_CHARS`] characters, and trim whitespace.
/// Control characters are removed rather than replaced, so text on either side of a tab or line
/// break is joined directly.
pub fn sanitize_title(raw: &str) -> String {
    let kept: String = raw
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_TITLE_CHARS)
        .collect();
    kept.trim().to_owned()
}

/// Chapters in ascending start order with distinct starts.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChapterList {
    chapters: Vec<Chapter>,
}

impl ChapterList {
    /// Build a list from chapters given in any order.
    ///
    /// The sort is stable. A chapter whose start equals the start of the chapter kept before it
    /// is dropped, so the first of two chapters with the same start wins. If `duration_ticks` is
    /// `Some`, it must be zero or more, and every chapter start must be at most that duration.
    pub fn new(
        mut chapters: Vec<Chapter>,
        duration_ticks: Option<i64>,
    ) -> Result<Self, ChapterError> {
        if let Some(duration) = duration_ticks {
            if duration < 0 {
                return Err(ChapterError::NegativeDuration);
            }
            if chapters
                .iter()
                .any(|chapter| chapter.start_ticks > duration)
            {
                return Err(ChapterError::StartBeyondDuration);
            }
        }

        chapters.sort_by_key(|chapter| chapter.start_ticks);

        let mut kept: Vec<Chapter> = Vec::with_capacity(chapters.len());
        for chapter in chapters {
            let repeats_previous_start = kept
                .last()
                .is_some_and(|previous| previous.start_ticks == chapter.start_ticks);
            if !repeats_previous_start {
                kept.push(chapter);
            }
        }
        Ok(Self { chapters: kept })
    }

    pub fn chapters(&self) -> &[Chapter] {
        &self.chapters
    }

    pub fn get(&self, index: usize) -> Option<&Chapter> {
        self.chapters.get(index)
    }

    pub fn len(&self) -> usize {
        self.chapters.len()
    }

    pub fn is_empty(&self) -> bool {
        self.chapters.is_empty()
    }

    /// Index of the chapter that contains `position_ticks`: the last chapter whose start is at or
    /// before the position. Returns `None` for an empty list or a position before the first
    /// chapter. A position past the last start stays on the last chapter.
    pub fn current_chapter(&self, position_ticks: i64) -> Option<usize> {
        let started = self
            .chapters
            .partition_point(|chapter| chapter.start_ticks <= position_ticks);
        started.checked_sub(1)
    }

    /// Index of the chapter after `current`. `None`, meaning before the first chapter, moves to
    /// the first chapter. The last chapter stays on itself. Returns `None` for an empty list.
    pub fn next_index(&self, current: Option<usize>) -> Option<usize> {
        let last = self.chapters.len().checked_sub(1)?;
        Some(match current {
            None => 0,
            Some(index) => index.min(last).saturating_add(1).min(last),
        })
    }

    /// Index of the chapter before `current`. `None` and the first chapter both return the
    /// first chapter. Returns `None` for an empty list.
    pub fn previous_index(&self, current: Option<usize>) -> Option<usize> {
        let last = self.chapters.len().checked_sub(1)?;
        Some(match current {
            None => 0,
            Some(index) => index.min(last).saturating_sub(1),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECOND: i64 = 10_000_000;

    fn list(entries: &[(i64, &str)], duration: Option<i64>) -> ChapterList {
        let chapters: Vec<Chapter> = entries
            .iter()
            .filter_map(|(start, title)| Chapter::new(*start, title).ok())
            .collect();
        match ChapterList::new(chapters, duration) {
            Ok(list) => list,
            Err(error) => panic!("test list refused: {error}"),
        }
    }

    fn starts(list: &ChapterList) -> Vec<i64> {
        list.chapters().iter().map(Chapter::start_ticks).collect()
    }

    fn three() -> ChapterList {
        list(
            &[
                (0, "Opening"),
                (100 * SECOND, "Middle"),
                (200 * SECOND, "End"),
            ],
            None,
        )
    }

    #[test]
    fn lookup_changes_exactly_at_each_chapter_start() {
        let chapters = three();
        assert_eq!(chapters.current_chapter(0), Some(0));
        assert_eq!(chapters.current_chapter(100 * SECOND - 1), Some(0));
        assert_eq!(chapters.current_chapter(100 * SECOND), Some(1));
        assert_eq!(chapters.current_chapter(200 * SECOND - 1), Some(1));
        assert_eq!(chapters.current_chapter(200 * SECOND), Some(2));
    }

    #[test]
    fn lookup_past_the_last_start_stays_on_the_last_chapter() {
        assert_eq!(three().current_chapter(i64::MAX), Some(2));
    }

    #[test]
    fn lookup_before_the_first_chapter_is_none() {
        let late_first = list(&[(5 * SECOND, "Late")], None);
        assert_eq!(late_first.current_chapter(0), None);
        assert_eq!(late_first.current_chapter(-1), None);
        assert_eq!(late_first.current_chapter(5 * SECOND), Some(0));
    }

    #[test]
    fn empty_list_has_no_lookups() {
        let empty = ChapterList::default();
        assert!(empty.is_empty());
        assert_eq!(empty.len(), 0);
        assert_eq!(empty.get(0), None);
        assert_eq!(empty.current_chapter(0), None);
        assert_eq!(empty.next_index(None), None);
        assert_eq!(empty.next_index(Some(0)), None);
        assert_eq!(empty.previous_index(None), None);
        assert_eq!(empty.previous_index(Some(0)), None);
    }

    #[test]
    fn next_clamps_at_the_last_chapter() {
        let chapters = three();
        assert_eq!(chapters.next_index(None), Some(0));
        assert_eq!(chapters.next_index(Some(0)), Some(1));
        assert_eq!(chapters.next_index(Some(1)), Some(2));
        assert_eq!(chapters.next_index(Some(2)), Some(2));
        assert_eq!(chapters.next_index(Some(9)), Some(2));
    }

    #[test]
    fn previous_clamps_at_the_first_chapter() {
        let chapters = three();
        assert_eq!(chapters.previous_index(None), Some(0));
        assert_eq!(chapters.previous_index(Some(2)), Some(1));
        assert_eq!(chapters.previous_index(Some(1)), Some(0));
        assert_eq!(chapters.previous_index(Some(0)), Some(0));
        assert_eq!(chapters.previous_index(Some(9)), Some(1));
    }

    #[test]
    fn single_chapter_list_stays_put_in_both_directions() {
        let single = list(&[(0, "Only")], None);
        assert_eq!(single.next_index(Some(0)), Some(0));
        assert_eq!(single.previous_index(Some(0)), Some(0));
    }

    #[test]
    fn unsorted_input_is_sorted_by_start() {
        let chapters = list(&[(300, "C"), (100, "A"), (200, "B")], None);
        assert_eq!(starts(&chapters), vec![100, 200, 300]);
        assert_eq!(chapters.get(0).map(Chapter::title), Some("A"));
    }

    #[test]
    fn duplicate_start_keeps_the_first_given_chapter() {
        let chapters = list(&[(100, "First"), (0, "Zero"), (100, "Second")], None);
        assert_eq!(starts(&chapters), vec![0, 100]);
        assert_eq!(chapters.get(1).map(Chapter::title), Some("First"));
    }

    #[test]
    fn run_of_equal_starts_collapses_to_one_chapter() {
        let chapters = list(&[(50, "a"), (50, "b"), (50, "c")], None);
        assert_eq!(starts(&chapters), vec![50]);
        assert_eq!(chapters.get(0).map(Chapter::title), Some("a"));
    }

    #[test]
    fn starts_past_the_duration_are_refused() {
        let chapters = [Chapter::new(500, "Late").ok()]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
        assert_eq!(
            ChapterList::new(chapters.clone(), Some(400)),
            Err(ChapterError::StartBeyondDuration)
        );
        assert!(ChapterList::new(chapters.clone(), Some(500)).is_ok());
        assert!(ChapterList::new(chapters, None).is_ok());
    }

    #[test]
    fn negative_duration_is_refused() {
        assert_eq!(
            ChapterList::new(Vec::new(), Some(-1)),
            Err(ChapterError::NegativeDuration)
        );
    }

    #[test]
    fn negative_start_is_refused() {
        assert_eq!(Chapter::new(-1, "Bad"), Err(ChapterError::NegativeStart));
        assert!(Chapter::new(0, "Zero").is_ok());
    }

    #[test]
    fn title_control_characters_are_removed_and_text_is_trimmed() {
        assert_eq!(sanitize_title("  Chapter One \n"), "Chapter One");
        assert_eq!(sanitize_title("Intro\u{7}\u{0}duction"), "Introduction");
        assert_eq!(sanitize_title("Tab\there"), "Tabhere");
        assert_eq!(sanitize_title("\u{1b}[31mRed\u{1b}[0m"), "[31mRed[0m");
    }

    #[test]
    fn title_whitespace_only_becomes_empty() {
        assert_eq!(sanitize_title("   \t  "), "");
        assert_eq!(
            Chapter::new(0, " \r\n ")
                .map(|c| c.title().to_owned())
                .ok()
                .as_deref(),
            Some("")
        );
    }

    #[test]
    fn title_is_truncated_by_characters_not_bytes() {
        let long_ascii = "a".repeat(MAX_TITLE_CHARS + 50);
        assert_eq!(sanitize_title(&long_ascii).chars().count(), MAX_TITLE_CHARS);

        let long_multibyte = "\u{e9}".repeat(MAX_TITLE_CHARS + 50);
        let cleaned = sanitize_title(&long_multibyte);
        assert_eq!(cleaned.chars().count(), MAX_TITLE_CHARS);
        assert!(cleaned.chars().all(|c| c == '\u{e9}'));
    }

    #[test]
    fn title_is_trimmed_after_truncation() {
        let padded = format!("{} {}", "a".repeat(MAX_TITLE_CHARS - 1), "bbbb");
        let cleaned = sanitize_title(&padded);
        assert_eq!(cleaned.chars().count(), MAX_TITLE_CHARS - 1);
        assert!(!cleaned.ends_with(' '));
    }
}
