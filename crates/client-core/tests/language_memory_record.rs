//! M2-10 per-series language memory, exercised through the public API.
//!
//! The memory is pure: it never touches the settings file. These tests cover the record-then-recall
//! path the player uses, per-series isolation, clearing, and refusal of unusable choices.

use kattri_client_core::language_memory::{
    AudioChoice, RecordOutcome, SeriesLanguageMemory, SkipReason, SubtitleChoice, TrackLanguage,
};

const SERIES_A: &str = "0123456789abcdef0123456789abcdef";
const SERIES_A_EPISODE_2: &str = "01234567-89AB-CDEF-0123-456789ABCDEF";
const SERIES_B: &str = "fedcba9876543210fedcba9876543210";

fn track(language: &str) -> TrackLanguage {
    TrackLanguage::from_raw(Some(language), None)
}

fn server_order() -> [TrackLanguage; 3] {
    [track("eng"), track("jpn"), track("fre")]
}

#[test]
fn recorded_audio_choice_is_recalled_for_the_next_episode() {
    let mut memory = SeriesLanguageMemory::default();
    assert_eq!(
        memory.record_audio(Some(SERIES_A), &track("jpn")),
        RecordOutcome::Stored
    );

    assert_eq!(
        memory.choose_audio(SERIES_A_EPISODE_2, &server_order()),
        AudioChoice::Track(1)
    );
}

#[test]
fn recorded_subtitle_choice_is_recalled_for_the_next_episode() {
    let mut memory = SeriesLanguageMemory::default();
    memory.record_subtitle(Some(SERIES_A), &track("fre"));

    assert_eq!(
        memory.choose_subtitle(SERIES_A_EPISODE_2, &server_order()),
        SubtitleChoice::Track(2)
    );
}

#[test]
fn subtitles_off_is_recalled_as_off_for_the_next_episode() {
    let mut memory = SeriesLanguageMemory::default();
    assert_eq!(
        memory.record_subtitles_off(Some(SERIES_A)),
        RecordOutcome::Stored
    );

    assert_eq!(
        memory.choose_subtitle(SERIES_A_EPISODE_2, &server_order()),
        SubtitleChoice::Off
    );
}

#[test]
fn a_remembered_series_does_not_affect_other_series() {
    let mut memory = SeriesLanguageMemory::default();
    memory.record_audio(Some(SERIES_A), &track("jpn"));
    memory.record_subtitles_off(Some(SERIES_A));

    assert_eq!(
        memory.choose_audio(SERIES_B, &server_order()),
        AudioChoice::KeepDefault
    );
    assert_eq!(
        memory.choose_subtitle(SERIES_B, &server_order()),
        SubtitleChoice::KeepDefault
    );
    assert!(memory.entry_for(SERIES_B).is_none());

    memory.record_audio(Some(SERIES_B), &track("fre"));
    assert_eq!(
        memory.choose_audio(SERIES_A, &server_order()),
        AudioChoice::Track(1)
    );
    assert_eq!(
        memory.choose_audio(SERIES_B, &server_order()),
        AudioChoice::Track(2)
    );
}

#[test]
fn removing_one_series_keeps_the_others() {
    let mut memory = SeriesLanguageMemory::default();
    memory.record_audio(Some(SERIES_A), &track("jpn"));
    memory.record_audio(Some(SERIES_B), &track("fre"));

    assert!(memory.remove(SERIES_A));
    assert_eq!(memory.len(), 1);
    assert_eq!(
        memory.choose_audio(SERIES_A, &server_order()),
        AudioChoice::KeepDefault
    );
    assert_eq!(
        memory.choose_audio(SERIES_B, &server_order()),
        AudioChoice::Track(2)
    );
}

#[test]
fn clear_removes_every_series() {
    let mut memory = SeriesLanguageMemory::default();
    memory.record_audio(Some(SERIES_A), &track("jpn"));
    memory.record_subtitle(Some(SERIES_B), &track("eng"));
    assert_eq!(memory.len(), 2);

    memory.clear();

    assert!(memory.is_empty());
    assert_eq!(
        memory.choose_audio(SERIES_A, &server_order()),
        AudioChoice::KeepDefault
    );
    assert_eq!(
        memory.choose_subtitle(SERIES_B, &server_order()),
        SubtitleChoice::KeepDefault
    );
}

#[test]
fn undetermined_or_empty_language_is_refused_and_writes_nothing() {
    let mut memory = SeriesLanguageMemory::default();
    let refused = [
        TrackLanguage::from_raw(Some(""), None),
        TrackLanguage::from_raw(Some("   "), None),
        TrackLanguage::from_raw(Some("und"), None),
        TrackLanguage::from_raw(Some("zxx"), None),
        TrackLanguage::from_raw(Some("japanese"), None),
        TrackLanguage::from_raw(None, Some("Unlabelled")),
    ];

    for choice in &refused {
        assert_eq!(
            memory.record_audio(Some(SERIES_A), choice),
            RecordOutcome::Skipped(SkipReason::NoLanguage),
            "audio {choice:?}"
        );
        assert_eq!(
            memory.record_subtitle(Some(SERIES_A), choice),
            RecordOutcome::Skipped(SkipReason::NoLanguage),
            "subtitle {choice:?}"
        );
    }
    assert!(memory.is_empty());
}

#[test]
fn a_refused_choice_leaves_the_stored_entry_unchanged() {
    let mut memory = SeriesLanguageMemory::default();
    memory.record_audio(Some(SERIES_A), &track("jpn"));
    let before = memory.clone();

    assert_eq!(
        memory.record_audio(Some(SERIES_A), &TrackLanguage::from_raw(Some("und"), None)),
        RecordOutcome::Skipped(SkipReason::NoLanguage)
    );
    assert_eq!(memory, before);
}

#[test]
fn movies_without_a_series_and_malformed_series_ids_are_not_written() {
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
        memory.record_audio(Some("Breaking Bad"), &track("jpn")),
        RecordOutcome::Skipped(SkipReason::InvalidSeriesId)
    );
    assert!(memory.is_empty());
}
