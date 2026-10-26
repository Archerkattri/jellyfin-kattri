//! Per-series audio and subtitle language memory (M2-10).
//!
//! Pure data and rules, no I/O. The memory stores ISO 639 language codes, the track title as a
//! tie-breaker, and the subtitle-off flag, keyed by the Jellyfin series GUID. It never stores track
//! indexes, paths, URLs, or tokens.
//!
//! Persistence: [`SeriesLanguageMemory::to_records`] produces plain records, oldest first, and
//! [`SeriesLanguageMemory::from_records`] rebuilds the memory from records that may come from an
//! edited file. Invalid records are dropped. `to_settings_records` and
//! `from_settings_records` map through the validated TOML schema owned by `kattri-settings`.
//!
//! [UNCONFIRMED: the stream `Language` and `Title` field names; check
//! `crates/jellyfin-api/src/playback.rs`.]

use std::collections::BTreeMap;

use kattri_settings::SeriesLanguageSetting;

/// Most series entries kept. The least recently recorded entry is evicted first.
pub const MAX_SERIES_ENTRIES: usize = 500;

/// Longest track title kept, in characters. Longer titles are truncated.
pub const MAX_TITLE_CHARS: usize = 128;

/// A Jellyfin series GUID in canonical lowercase 32-digit form.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SeriesId(String);

impl SeriesId {
    /// Accept 32 hex digits or the hyphenated 8-4-4-4-12 form, in any letter case.
    /// The all-zero GUID is rejected because it does not identify a series.
    pub fn parse(raw: &str) -> Option<Self> {
        let simple: String = match raw.len() {
            32 => raw.to_owned(),
            36 => {
                let bytes = raw.as_bytes();
                let hyphens_in_place = [8, 13, 18, 23]
                    .iter()
                    .all(|&index| bytes.get(index) == Some(&b'-'));
                if !hyphens_in_place {
                    return None;
                }
                raw.chars().filter(|&c| c != '-').collect()
            }
            _ => return None,
        };
        let well_formed = simple.len() == 32
            && simple.chars().all(|c| c.is_ascii_hexdigit())
            && simple.bytes().any(|b| b != b'0');
        well_formed.then(|| Self(simple.to_ascii_lowercase()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// An ISO 639 language code: two or three ASCII letters, stored in lowercase.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct LanguageCode(String);

impl LanguageCode {
    /// Accept a two- or three-letter code. ISO 639-2 `und` (undetermined) and `zxx` (no linguistic
    /// content) name no language, so remembering them would match unrelated tracks; they are refused.
    pub fn parse(raw: &str) -> Option<Self> {
        let trimmed = raw.trim();
        let lowered = trimmed.to_ascii_lowercase();
        let well_formed =
            (2..=3).contains(&trimmed.len()) && trimmed.bytes().all(|b| b.is_ascii_alphabetic());
        let names_a_language = !matches!(lowered.as_str(), "und" | "zxx");
        (well_formed && names_a_language).then_some(Self(lowered))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Keep a title only if it has text and no control characters. Long titles are truncated.
fn sanitize_title(raw: Option<&str>) -> Option<String> {
    let text = raw?.trim();
    if text.is_empty() || text.chars().any(char::is_control) {
        return None;
    }
    Some(text.chars().take(MAX_TITLE_CHARS).collect())
}

/// One track from the media item, in server order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TrackLanguage {
    language: Option<LanguageCode>,
    title: Option<String>,
}

impl TrackLanguage {
    /// Build a track description from raw server strings. An unusable language becomes `None`,
    /// and an unusable title is dropped.
    pub fn from_raw(language: Option<&str>, title: Option<&str>) -> Self {
        Self {
            language: language.and_then(LanguageCode::parse),
            title: sanitize_title(title),
        }
    }

    pub fn language(&self) -> Option<&LanguageCode> {
        self.language.as_ref()
    }

    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct StoredTrack {
    language: LanguageCode,
    title: Option<String>,
}

/// What is remembered for one series.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SeriesLanguageEntry {
    audio: Option<StoredTrack>,
    subtitle: Option<StoredTrack>,
    subtitle_disabled: bool,
}

impl SeriesLanguageEntry {
    pub fn audio_language(&self) -> Option<&LanguageCode> {
        self.audio.as_ref().map(|track| &track.language)
    }

    pub fn subtitle_language(&self) -> Option<&LanguageCode> {
        self.subtitle.as_ref().map(|track| &track.language)
    }

    pub const fn subtitle_disabled(&self) -> bool {
        self.subtitle_disabled
    }

    fn is_empty(&self) -> bool {
        self.audio.is_none() && self.subtitle.is_none() && !self.subtitle_disabled
    }
}

/// The audio track to use for a new item of a remembered series.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AudioChoice {
    /// Use the track at this index in the item's server-ordered track list.
    Track(usize),
    /// No remembered match: use the server default, and keep the entry unchanged.
    KeepDefault,
}

/// The subtitle track to use for a new item of a remembered series.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubtitleChoice {
    Track(usize),
    /// The viewer last turned subtitles off for this series.
    Off,
    /// No remembered match: use the server default, and keep the entry unchanged.
    KeepDefault,
}

/// Why a write was skipped.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SkipReason {
    /// The item is not an episode of a series (for example, a movie).
    NoSeries,
    /// The series ID is not a valid GUID.
    InvalidSeriesId,
    /// The chosen track has no usable language code, so there is nothing to remember.
    NoLanguage,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecordOutcome {
    Stored,
    Skipped(SkipReason),
}

/// Plain persisted form of one entry. Strings are untrusted until [`SeriesLanguageMemory::from_records`]
/// validates them.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SeriesLanguageRecord {
    pub series_id: String,
    pub audio_language: Option<String>,
    pub audio_title: Option<String>,
    pub subtitle_language: Option<String>,
    pub subtitle_title: Option<String>,
    pub subtitle_disabled: bool,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct Slot {
    entry: SeriesLanguageEntry,
    last_recorded: u64,
}

/// Per-series language choices, bounded to [`MAX_SERIES_ENTRIES`].
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SeriesLanguageMemory {
    entries: BTreeMap<SeriesId, Slot>,
    clock: u64,
}

/// Pick the first track in server order with the stored language. A track whose title equals the
/// stored title wins over an earlier track with the same language.
fn pick_index(stored: &StoredTrack, tracks: &[TrackLanguage]) -> Option<usize> {
    let mut first_match = None;
    for (index, track) in tracks.iter().enumerate() {
        if track.language.as_ref() != Some(&stored.language) {
            continue;
        }
        if stored.title.is_some() && track.title.as_deref() == stored.title.as_deref() {
            return Some(index);
        }
        first_match.get_or_insert(index);
    }
    first_match
}

impl SeriesLanguageMemory {
    /// Record the audio language the viewer chose for an episode.
    pub fn record_audio(&mut self, series: Option<&str>, track: &TrackLanguage) -> RecordOutcome {
        let id = match self.writable_series(series) {
            Ok(id) => id,
            Err(reason) => return RecordOutcome::Skipped(reason),
        };
        let Some(language) = track.language.clone() else {
            return RecordOutcome::Skipped(SkipReason::NoLanguage);
        };
        let stored = StoredTrack {
            language,
            title: track.title.clone(),
        };
        self.upsert(id, |entry| entry.audio = Some(stored));
        RecordOutcome::Stored
    }

    /// Record a subtitle track the viewer chose for an episode.
    pub fn record_subtitle(
        &mut self,
        series: Option<&str>,
        track: &TrackLanguage,
    ) -> RecordOutcome {
        let id = match self.writable_series(series) {
            Ok(id) => id,
            Err(reason) => return RecordOutcome::Skipped(reason),
        };
        let Some(language) = track.language.clone() else {
            return RecordOutcome::Skipped(SkipReason::NoLanguage);
        };
        let stored = StoredTrack {
            language,
            title: track.title.clone(),
        };
        self.upsert(id, |entry| {
            entry.subtitle = Some(stored);
            entry.subtitle_disabled = false;
        });
        RecordOutcome::Stored
    }

    /// Record that the viewer turned subtitles off for an episode.
    pub fn record_subtitles_off(&mut self, series: Option<&str>) -> RecordOutcome {
        let id = match self.writable_series(series) {
            Ok(id) => id,
            Err(reason) => return RecordOutcome::Skipped(reason),
        };
        self.upsert(id, |entry| {
            entry.subtitle = None;
            entry.subtitle_disabled = true;
        });
        RecordOutcome::Stored
    }

    /// Choose the audio track for a new item of `series`. Does not change the memory.
    pub fn choose_audio(&self, series: &str, tracks: &[TrackLanguage]) -> AudioChoice {
        self.entry_for(series)
            .and_then(|entry| entry.audio.as_ref())
            .and_then(|stored| pick_index(stored, tracks))
            .map_or(AudioChoice::KeepDefault, AudioChoice::Track)
    }

    /// Choose the subtitle track for a new item of `series`. Does not change the memory.
    pub fn choose_subtitle(&self, series: &str, tracks: &[TrackLanguage]) -> SubtitleChoice {
        let Some(entry) = self.entry_for(series) else {
            return SubtitleChoice::KeepDefault;
        };
        if entry.subtitle_disabled {
            return SubtitleChoice::Off;
        }
        entry
            .subtitle
            .as_ref()
            .and_then(|stored| pick_index(stored, tracks))
            .map_or(SubtitleChoice::KeepDefault, SubtitleChoice::Track)
    }

    pub fn entry_for(&self, series: &str) -> Option<&SeriesLanguageEntry> {
        let id = SeriesId::parse(series)?;
        self.entries.get(&id).map(|slot| &slot.entry)
    }

    /// Remove one series. Returns true if an entry was removed.
    pub fn remove(&mut self, series: &str) -> bool {
        SeriesId::parse(series).is_some_and(|id| self.entries.remove(&id).is_some())
    }

    /// Remove every entry (the settings action "clear all").
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Persisted form, oldest recording first, so [`Self::from_records`] keeps the same eviction order.
    pub fn to_records(&self) -> Vec<SeriesLanguageRecord> {
        let mut slots: Vec<(&SeriesId, &Slot)> = self.entries.iter().collect();
        slots.sort_by_key(|(_, slot)| slot.last_recorded);
        slots
            .into_iter()
            .map(|(id, slot)| SeriesLanguageRecord {
                series_id: id.as_str().to_owned(),
                audio_language: slot.entry.audio_language().map(|c| c.as_str().to_owned()),
                audio_title: slot.entry.audio.as_ref().and_then(|t| t.title.clone()),
                subtitle_language: slot
                    .entry
                    .subtitle_language()
                    .map(|c| c.as_str().to_owned()),
                subtitle_title: slot.entry.subtitle.as_ref().and_then(|t| t.title.clone()),
                subtitle_disabled: slot.entry.subtitle_disabled,
            })
            .collect()
    }

    /// Convert to the Rust-owned settings model for profile persistence.
    pub fn to_settings_records(&self) -> Vec<SeriesLanguageSetting> {
        self.to_records()
            .into_iter()
            .map(|record| SeriesLanguageSetting {
                series_id: record.series_id,
                audio_language: record.audio_language,
                audio_title: record.audio_title,
                subtitle_language: record.subtitle_language,
                subtitle_title: record.subtitle_title,
                subtitle_disabled: record.subtitle_disabled,
            })
            .collect()
    }

    /// Restore from settings records, dropping invalid values through the same validation path
    /// used for imported records.
    pub fn from_settings_records<I>(records: I) -> Self
    where
        I: IntoIterator<Item = SeriesLanguageSetting>,
    {
        Self::from_records(records.into_iter().map(|record| SeriesLanguageRecord {
            series_id: record.series_id,
            audio_language: record.audio_language,
            audio_title: record.audio_title,
            subtitle_language: record.subtitle_language,
            subtitle_title: record.subtitle_title,
            subtitle_disabled: record.subtitle_disabled,
        }))
    }

    /// Rebuild the memory from records. Invalid records are dropped: bad series IDs, bad
    /// language codes, and entries with nothing to remember. If there are more than
    /// [`MAX_SERIES_ENTRIES`] valid records, the most recent ones are kept.
    pub fn from_records<I>(records: I) -> Self
    where
        I: IntoIterator<Item = SeriesLanguageRecord>,
    {
        let valid: Vec<(SeriesId, SeriesLanguageEntry)> = records
            .into_iter()
            .filter_map(|record| {
                let id = SeriesId::parse(&record.series_id)?;
                let audio = record
                    .audio_language
                    .as_deref()
                    .and_then(LanguageCode::parse)
                    .map(|language| StoredTrack {
                        language,
                        title: sanitize_title(record.audio_title.as_deref()),
                    });
                let subtitle_disabled = record.subtitle_disabled;
                let subtitle = if subtitle_disabled {
                    None
                } else {
                    record
                        .subtitle_language
                        .as_deref()
                        .and_then(LanguageCode::parse)
                        .map(|language| StoredTrack {
                            language,
                            title: sanitize_title(record.subtitle_title.as_deref()),
                        })
                };
                let entry = SeriesLanguageEntry {
                    audio,
                    subtitle,
                    subtitle_disabled,
                };
                (!entry.is_empty()).then_some((id, entry))
            })
            .collect();

        let skip = valid.len().saturating_sub(MAX_SERIES_ENTRIES);
        let mut memory = Self::default();
        for (id, entry) in valid.into_iter().skip(skip) {
            memory.clock = memory.clock.saturating_add(1);
            let last_recorded = memory.clock;
            memory.entries.insert(
                id,
                Slot {
                    entry,
                    last_recorded,
                },
            );
        }
        memory
    }

    fn writable_series(&self, series: Option<&str>) -> Result<SeriesId, SkipReason> {
        let raw = series.ok_or(SkipReason::NoSeries)?;
        SeriesId::parse(raw).ok_or(SkipReason::InvalidSeriesId)
    }

    fn upsert(&mut self, id: SeriesId, update: impl FnOnce(&mut SeriesLanguageEntry)) {
        self.clock = self.clock.saturating_add(1);
        let clock = self.clock;
        if !self.entries.contains_key(&id) && self.entries.len() >= MAX_SERIES_ENTRIES {
            let oldest = self
                .entries
                .iter()
                .min_by_key(|(_, slot)| slot.last_recorded)
                .map(|(key, _)| key.clone());
            if let Some(oldest) = oldest {
                self.entries.remove(&oldest);
            }
        }
        let slot = self.entries.entry(id).or_default();
        update(&mut slot.entry);
        slot.last_recorded = clock;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EPISODE_1: &str = "0123456789abcdef0123456789abcdef";
    const EPISODE_2_HYPHENATED: &str = "01234567-89AB-CDEF-0123-456789ABCDEF";

    fn track(language: &str) -> TrackLanguage {
        TrackLanguage::from_raw(Some(language), None)
    }

    fn titled(language: &str, title: &str) -> TrackLanguage {
        TrackLanguage::from_raw(Some(language), Some(title))
    }

    #[test]
    fn series_id_accepts_plain_and_hyphenated_guids_in_any_case() {
        let plain = SeriesId::parse(EPISODE_1).expect("test entry must parse");
        let hyphen = SeriesId::parse(EPISODE_2_HYPHENATED).expect("test entry must parse");
        assert_eq!(plain, hyphen);
        assert_eq!(hyphen.as_str(), "0123456789abcdef0123456789abcdef");
    }

    #[test]
    fn series_id_rejects_names_paths_and_zero_guid() {
        assert!(SeriesId::parse("").is_none());
        assert!(SeriesId::parse("Breaking Bad").is_none());
        assert!(SeriesId::parse("../../etc/passwd").is_none());
        assert!(SeriesId::parse("0123456789abcdef0123456789abcdeg").is_none());
        assert!(SeriesId::parse("0123456789abcdef0123456789abcdef0").is_none());
        assert!(SeriesId::parse("00000000000000000000000000000000").is_none());
        assert!(SeriesId::parse("01234567_89ab-cdef-0123-456789abcdef").is_none());
    }

    #[test]
    fn language_codes_are_two_or_three_letters_in_lowercase() {
        assert_eq!(
            LanguageCode::parse("JPN").map(|c| c.as_str().to_owned()),
            Some("jpn".to_owned())
        );
        assert_eq!(
            LanguageCode::parse("en").map(|c| c.as_str().to_owned()),
            Some("en".to_owned())
        );
        assert!(LanguageCode::parse("japanese").is_none());
        assert!(LanguageCode::parse("j1").is_none());
        assert!(LanguageCode::parse("").is_none());
        assert!(LanguageCode::parse("  ").is_none());
    }

    #[test]
    fn titles_are_dropped_when_empty_or_control_characters_and_truncated() {
        assert_eq!(
            sanitize_title(Some("  Commentary ")),
            Some("Commentary".to_owned())
        );
        assert_eq!(sanitize_title(Some("   ")), None);
        assert_eq!(sanitize_title(Some("bad\u{0007}title")), None);
        assert_eq!(sanitize_title(None), None);
        let long = "x".repeat(MAX_TITLE_CHARS + 50);
        assert_eq!(
            sanitize_title(Some(&long)).map(|t| t.chars().count()),
            Some(MAX_TITLE_CHARS)
        );
    }

    #[test]
    fn episode_one_japanese_audio_is_used_for_episode_two() {
        let mut memory = SeriesLanguageMemory::default();
        let episode_one = [track("eng"), track("jpn")];
        assert_eq!(
            memory.record_audio(Some(EPISODE_1), &episode_one[1]),
            RecordOutcome::Stored
        );

        let episode_two = [track("eng"), track("jpn")];
        assert_eq!(
            memory.choose_audio(EPISODE_2_HYPHENATED, &episode_two),
            AudioChoice::Track(1)
        );
    }

    #[test]
    fn no_match_keeps_the_default_and_leaves_the_entry_unchanged() {
        let mut memory = SeriesLanguageMemory::default();
        memory.record_audio(Some(EPISODE_1), &track("jpn"));
        let before = memory.clone();

        let tracks = [track("eng"), track("fre")];
        assert_eq!(
            memory.choose_audio(EPISODE_1, &tracks),
            AudioChoice::KeepDefault
        );
        assert_eq!(
            memory.choose_subtitle(EPISODE_1, &tracks),
            SubtitleChoice::KeepDefault
        );
        assert_eq!(memory, before);
    }

    #[test]
    fn subtitles_off_is_remembered_and_applied() {
        let mut memory = SeriesLanguageMemory::default();
        assert_eq!(
            memory.record_subtitles_off(Some(EPISODE_1)),
            RecordOutcome::Stored
        );
        let entry = memory.entry_for(EPISODE_1).expect("test entry must parse");
        assert!(entry.subtitle_disabled());
        assert!(entry.subtitle_language().is_none());

        let tracks = [track("eng")];
        assert_eq!(
            memory.choose_subtitle(EPISODE_2_HYPHENATED, &tracks),
            SubtitleChoice::Off
        );
    }

    #[test]
    fn choosing_a_subtitle_again_re_enables_subtitles() {
        let mut memory = SeriesLanguageMemory::default();
        memory.record_subtitles_off(Some(EPISODE_1));
        memory.record_subtitle(Some(EPISODE_1), &track("eng"));

        let tracks = [track("eng")];
        assert_eq!(
            memory.choose_subtitle(EPISODE_1, &tracks),
            SubtitleChoice::Track(0)
        );
        assert!(
            !memory
                .entry_for(EPISODE_1)
                .expect("test entry must parse")
                .subtitle_disabled()
        );
    }

    #[test]
    fn same_language_tie_breaks_on_the_stored_title() {
        let mut memory = SeriesLanguageMemory::default();
        let commentary = titled("eng", "Director Commentary");
        memory.record_audio(Some(EPISODE_1), &commentary);

        let tracks = [
            titled("eng", "Full Cast"),
            titled("eng", "Director Commentary"),
        ];
        assert_eq!(
            memory.choose_audio(EPISODE_1, &tracks),
            AudioChoice::Track(1)
        );
    }

    #[test]
    fn same_language_without_a_title_match_uses_server_order() {
        let mut memory = SeriesLanguageMemory::default();
        memory.record_audio(Some(EPISODE_1), &titled("eng", "Old Title"));

        let tracks = [
            track("jpn"),
            titled("eng", "New Title"),
            titled("eng", "Other"),
        ];
        assert_eq!(
            memory.choose_audio(EPISODE_1, &tracks),
            AudioChoice::Track(1)
        );
    }

    #[test]
    fn movies_and_invalid_series_ids_do_not_write() {
        let mut memory = SeriesLanguageMemory::default();
        assert_eq!(
            memory.record_audio(None, &track("jpn")),
            RecordOutcome::Skipped(SkipReason::NoSeries)
        );
        assert_eq!(
            memory.record_subtitles_off(None),
            RecordOutcome::Skipped(SkipReason::NoSeries)
        );
        assert_eq!(
            memory.record_subtitle(Some("not-a-guid"), &track("eng")),
            RecordOutcome::Skipped(SkipReason::InvalidSeriesId)
        );
        assert!(memory.is_empty());
    }

    #[test]
    fn missing_language_writes_nothing_for_that_track_type() {
        let mut memory = SeriesLanguageMemory::default();
        let unknown = TrackLanguage::from_raw(None, Some("Unlabelled"));
        assert_eq!(
            memory.record_audio(Some(EPISODE_1), &unknown),
            RecordOutcome::Skipped(SkipReason::NoLanguage)
        );
        assert_eq!(
            memory.record_subtitle(Some(EPISODE_1), &unknown),
            RecordOutcome::Skipped(SkipReason::NoLanguage)
        );
        assert!(memory.is_empty());
    }

    #[test]
    fn audio_and_subtitle_memories_are_independent() {
        let mut memory = SeriesLanguageMemory::default();
        memory.record_audio(Some(EPISODE_1), &track("jpn"));
        let entry = memory.entry_for(EPISODE_1).expect("test entry must parse");
        assert_eq!(
            entry.audio_language().map(LanguageCode::as_str),
            Some("jpn")
        );
        assert!(entry.subtitle_language().is_none());
        assert!(!entry.subtitle_disabled());
        assert_eq!(
            memory.choose_subtitle(EPISODE_1, &[track("eng")]),
            SubtitleChoice::KeepDefault
        );
    }

    #[test]
    fn remove_and_clear_work() {
        let mut memory = SeriesLanguageMemory::default();
        memory.record_audio(Some(EPISODE_1), &track("jpn"));
        memory.record_audio(Some(EPISODE_2_HYPHENATED), &track("eng"));
        assert_eq!(memory.len(), 1);
        assert!(memory.remove(EPISODE_1));
        assert!(!memory.remove(EPISODE_1));
        memory.record_audio(Some(EPISODE_1), &track("jpn"));
        memory.clear();
        assert!(memory.is_empty());
    }

    #[test]
    fn size_is_bounded_and_the_least_recent_series_is_evicted() {
        let mut memory = SeriesLanguageMemory::default();
        let ids: Vec<String> = (0..MAX_SERIES_ENTRIES + 10)
            .map(|index| format!("{:032x}", index + 1))
            .collect();
        for id in &ids {
            memory.record_audio(Some(id), &track("eng"));
        }
        assert_eq!(memory.len(), MAX_SERIES_ENTRIES);
        assert!(memory.entry_for(&ids[0]).is_none());
        assert!(memory.entry_for(&ids[9]).is_none());
        assert!(memory.entry_for(&ids[10]).is_some());
        assert!(
            memory
                .entry_for(ids.last().map_or("", String::as_str))
                .is_some()
        );
    }

    #[test]
    fn records_round_trip_exactly() {
        let mut memory = SeriesLanguageMemory::default();
        memory.record_audio(Some(EPISODE_1), &titled("jpn", "Original"));
        memory.record_subtitle(Some(EPISODE_1), &track("eng"));
        memory.record_subtitles_off(Some(EPISODE_2_HYPHENATED));

        let records = memory.to_records();
        let restored = SeriesLanguageMemory::from_records(records.clone());
        assert_eq!(restored.to_records(), records);
        assert_eq!(
            restored.choose_audio(EPISODE_1, &[track("eng"), titled("jpn", "Original")]),
            AudioChoice::Track(1)
        );
        assert_eq!(
            restored.choose_subtitle(EPISODE_2_HYPHENATED, &[track("eng")]),
            SubtitleChoice::Off
        );
    }

    #[test]
    fn settings_records_round_trip_audio_subtitle_and_disabled_choice() {
        let mut memory = SeriesLanguageMemory::default();
        memory.record_audio(Some(EPISODE_1), &titled("jpn", "Original"));
        memory.record_subtitle(Some(EPISODE_1), &titled("eng", "English"));
        memory.record_subtitles_off(Some(EPISODE_2_HYPHENATED));

        let settings = kattri_settings::Settings {
            series_languages: memory.to_settings_records(),
            ..kattri_settings::Settings::default()
        };
        let text = kattri_settings::to_toml_string(&settings).expect("settings memory serializes");
        let loaded = kattri_settings::load_from_str(&text).expect("settings memory loads");
        let restored = SeriesLanguageMemory::from_settings_records(loaded.series_languages);

        assert_eq!(restored.to_records(), memory.to_records());
    }

    #[test]
    fn invalid_persisted_records_are_dropped() {
        let records = vec![
            SeriesLanguageRecord {
                series_id: "not-a-guid".to_owned(),
                audio_language: Some("jpn".to_owned()),
                ..Default::default()
            },
            SeriesLanguageRecord {
                series_id: EPISODE_1.to_owned(),
                audio_language: Some("japanese".to_owned()),
                ..Default::default()
            },
            SeriesLanguageRecord {
                series_id: EPISODE_2_HYPHENATED.to_owned(),
                audio_language: Some("eng".to_owned()),
                audio_title: Some("bad\u{0001}".to_owned()),
                ..Default::default()
            },
        ];
        let memory = SeriesLanguageMemory::from_records(records);
        assert_eq!(memory.len(), 1);
        let entry = memory
            .entry_for(EPISODE_2_HYPHENATED)
            .expect("test entry must parse");
        assert_eq!(
            entry.audio_language().map(LanguageCode::as_str),
            Some("eng")
        );
    }

    #[test]
    fn loading_more_records_than_the_bound_keeps_the_most_recent() {
        let records: Vec<SeriesLanguageRecord> = (0..MAX_SERIES_ENTRIES + 5)
            .map(|index| SeriesLanguageRecord {
                series_id: format!("{:032x}", index + 1),
                audio_language: Some("eng".to_owned()),
                ..Default::default()
            })
            .collect();
        let memory = SeriesLanguageMemory::from_records(records);
        assert_eq!(memory.len(), MAX_SERIES_ENTRIES);
        assert!(memory.entry_for(&format!("{:032x}", 1)).is_none());
        assert!(
            memory
                .entry_for(&format!("{:032x}", MAX_SERIES_ENTRIES + 5))
                .is_some()
        );
    }

    #[test]
    fn disabled_record_ignores_a_stored_subtitle_language() {
        let memory = SeriesLanguageMemory::from_records(vec![SeriesLanguageRecord {
            series_id: EPISODE_1.to_owned(),
            subtitle_language: Some("eng".to_owned()),
            subtitle_disabled: true,
            ..Default::default()
        }]);
        let entry = memory.entry_for(EPISODE_1).expect("test entry must parse");
        assert!(entry.subtitle_disabled());
        assert!(entry.subtitle_language().is_none());
    }
}
