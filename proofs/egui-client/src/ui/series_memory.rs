//! Remembered series languages (M2-10). The Playback page links here. The list shows each
//! series that has a remembered audio or subtitle language, with a delete button per series
//! and one clear-all.
//!
//! The memory stores no series name, so a series is shown by its Jellyfin ID. Rows are built
//! by a pure function so their text and order are tested without egui.

use super::{ui_text, ui_text_args};
use egui::{RichText, Ui};
use kattri_client_core::language_memory::{SeriesLanguageMemory, SeriesLanguageRecord};
use kattri_i18n::ids;

use crate::app::KattriApp;
use crate::theme::Palette;
use crate::ui::settings::{finish_page, muted};

/// One line of the list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SeriesRow {
    pub(crate) series_id: String,
    pub(crate) summary: String,
}

/// Rows for the list, most recently recorded first. `records` is oldest first, as
/// `SeriesLanguageMemory::to_records` returns it.
pub(crate) fn series_rows(records: &[SeriesLanguageRecord]) -> Vec<SeriesRow> {
    records
        .iter()
        .rev()
        .map(|record| SeriesRow {
            series_id: record.series_id.clone(),
            summary: summary(record),
        })
        .collect()
}

fn summary(record: &SeriesLanguageRecord) -> String {
    let audio = record
        .audio_language
        .as_deref()
        .unwrap_or(ui_text(ids::MEMORY_LANGUAGE_DEFAULT));
    let subtitles = if record.subtitle_disabled {
        ui_text(ids::MEMORY_LANGUAGE_OFF)
    } else {
        record
            .subtitle_language
            .as_deref()
            .unwrap_or(ui_text(ids::MEMORY_LANGUAGE_DEFAULT))
    };
    ui_text_args(
        ids::MEMORY_SUMMARY,
        &[("audio", audio), ("subtitles", subtitles)],
    )
}

pub(crate) fn series_memory_page(ui: &mut Ui, app: &mut KattriApp, colors: Palette) {
    let before = app.settings.settings().clone();
    let mut memory = SeriesLanguageMemory::from_settings_records(
        app.settings.settings().series_languages.clone(),
    );
    let rows = series_rows(&memory.to_records());
    if rows.is_empty() {
        muted(ui, colors, ui_text(ids::MEMORY_EMPTY), 14.0);
    } else {
        muted(ui, colors, ui_text(ids::MEMORY_HELP), 12.0);
        ui.add_space(10.0);
        for row in &rows {
            ui.push_id(&row.series_id, |ui| {
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.label(RichText::new(&row.summary).color(colors.text));
                        muted(
                            ui,
                            colors,
                            &ui_text_args(
                                ids::MEMORY_SERIES,
                                &[("series_id", &row.series_id.to_string())],
                            ),
                            12.0,
                        );
                    });
                    if ui.button(ui_text(ids::MEMORY_BUTTON_DELETE)).clicked() {
                        memory.remove(&row.series_id);
                    }
                });
            });
            ui.add_space(8.0);
        }
        ui.add_space(4.0);
        if ui.button(ui_text(ids::MEMORY_BUTTON_CLEAR_ALL)).clicked() {
            memory.clear();
        }
    }

    let updated = memory.to_settings_records();
    if updated != app.settings.settings().series_languages {
        app.settings.settings_mut().series_languages = updated;
    }
    finish_page(ui, app, colors, &before);
}

#[cfg(test)]
mod tests {
    use super::{SeriesRow, series_rows, summary};
    use crate::settings_state::SettingsState;
    use kattri_client_core::language_memory::{
        AudioChoice, RecordOutcome, SeriesLanguageMemory, SeriesLanguageRecord, TrackLanguage,
    };
    use kattri_settings::SeriesLanguageSetting;

    const SERIES_A: &str = "0123456789abcdef0123456789abcdef";
    const SERIES_B: &str = "fedcba9876543210fedcba9876543210";

    fn record(
        series_id: &str,
        audio: Option<&str>,
        subtitle: Option<&str>,
        off: bool,
    ) -> SeriesLanguageRecord {
        SeriesLanguageRecord {
            series_id: series_id.to_owned(),
            audio_language: audio.map(str::to_owned),
            audio_title: None,
            subtitle_language: subtitle.map(str::to_owned),
            subtitle_title: None,
            subtitle_disabled: off,
        }
    }

    fn memory_with_two_series() -> SeriesLanguageMemory {
        let mut memory = SeriesLanguageMemory::default();
        let japanese = TrackLanguage::from_raw(Some("jpn"), None);
        memory.record_audio(Some(SERIES_A), &japanese);
        memory.record_subtitles_off(Some(SERIES_B));
        memory
    }

    #[test]
    fn rows_list_the_most_recent_series_first() {
        let memory = memory_with_two_series();
        let rows = series_rows(&memory.to_records());
        let ids: Vec<&str> = rows.iter().map(|row| row.series_id.as_str()).collect();
        assert_eq!(ids, [SERIES_B, SERIES_A]);
    }

    #[test]
    fn summary_names_each_choice_and_its_defaults() {
        let off = record(SERIES_A, Some("ja"), None, true);
        assert_eq!(summary(&off), "Audio: ja   Subtitles: off");

        let subtitles = record(SERIES_A, None, Some("en"), false);
        assert_eq!(summary(&subtitles), "Audio: default   Subtitles: en");

        let nothing_set = record(SERIES_A, None, None, false);
        assert_eq!(summary(&nothing_set), "Audio: default   Subtitles: default");
    }

    #[test]
    fn deleting_one_series_keeps_the_others_in_settings() {
        let stored = memory_with_two_series().to_settings_records();
        let mut restored = SeriesLanguageMemory::from_settings_records(stored);
        assert!(restored.remove(SERIES_A));
        assert!(!restored.remove(SERIES_A), "a second delete finds nothing");
        let remaining: Vec<SeriesLanguageSetting> = restored.to_settings_records();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].series_id, SERIES_B);
    }

    #[test]
    fn clear_all_empties_the_settings_records() {
        let mut restored = SeriesLanguageMemory::from_settings_records(
            memory_with_two_series().to_settings_records(),
        );
        restored.clear();
        assert!(restored.to_settings_records().is_empty());
    }

    /// Record one Japanese pick the way the player does, through the settings store.
    fn record_japanese_pick(state: &mut SettingsState) {
        let before = state.settings().clone();
        let mut memory =
            SeriesLanguageMemory::from_settings_records(state.settings().series_languages.clone());
        let japanese = TrackLanguage::from_raw(Some("jpn"), Some("Japanese"));
        assert_eq!(
            memory.record_audio(Some(SERIES_A), &japanese),
            RecordOutcome::Stored
        );
        state.settings_mut().series_languages = memory.to_settings_records();
        state.save_if_changed(&before);
    }

    fn listed_rows(state: &SettingsState) -> Vec<SeriesRow> {
        let restored =
            SeriesLanguageMemory::from_settings_records(state.settings().series_languages.clone());
        series_rows(&restored.to_records())
    }

    fn assert_japanese_listed(state: &SettingsState) {
        let rows = listed_rows(state);
        assert_eq!(rows.len(), 1, "the recorded series is listed once");
        assert_eq!(rows[0].series_id, SERIES_A);
        assert_eq!(rows[0].summary, "Audio: jpn   Subtitles: default");
    }

    #[test]
    fn a_recorded_choice_is_listed_after_a_settings_reload() {
        let dir =
            std::env::temp_dir().join(format!("kattri-series-memory-{}", uuid::Uuid::new_v4()));
        let mut state = SettingsState::open(Some(&dir), None);
        record_japanese_pick(&mut state);
        state.wait_for_pending_save();

        state.reload();
        assert_japanese_listed(&state);

        // The next episode of the series gets the same language, not the first track.
        let restored =
            SeriesLanguageMemory::from_settings_records(state.settings().series_languages.clone());
        let episode = [
            TrackLanguage::from_raw(Some("eng"), Some("English")),
            TrackLanguage::from_raw(Some("jpn"), Some("Japanese")),
        ];
        assert_eq!(
            restored.choose_audio(SERIES_A, &episode),
            AudioChoice::Track(1)
        );
        drop(state);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_reload_before_the_save_lands_keeps_the_recorded_choice() {
        let dir = std::env::temp_dir().join(format!(
            "kattri-series-memory-race-{}",
            uuid::Uuid::new_v4()
        ));
        let mut state = SettingsState::open(Some(&dir), None);
        record_japanese_pick(&mut state);

        // The settings page reloads when a category opens, which can happen before the writer
        // thread has flushed the pick. The pick must survive that reload.
        state.reload();
        assert_japanese_listed(&state);

        state.wait_for_pending_save();
        state.reload();
        assert_japanese_listed(&state);
        drop(state);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
