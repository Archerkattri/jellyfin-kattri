//! Downloads page: one row per download job with its state, an optional notice,
//! and pause and resume controls.
//!
//! The page is a view. It performs no disk or network I/O. A click becomes a
//! [`DownloadIntent`] for the caller to send to the worker. The button rules
//! ([`controls`]) and the label ([`row_label`]) are pure functions, so the page
//! and its tests read the same values.

use super::ui_text;
use egui::{RichText, Ui};
#[cfg(test)]
use kattri_client_core::downloads::DownloadTransitionError;
use kattri_client_core::downloads::{DownloadJob, DownloadState};
use kattri_downloads::messages::Notice;
use kattri_i18n::ids;

use crate::theme::Palette;

/// Rows laid out by one call to [`show`]. The job list comes from the caller, so it is bounded.
pub(crate) const MAX_ROWS: usize = 200;
/// Titles come from the server. Long names are cut so one row cannot take the page.
const MAX_TITLE_CHARS: usize = 256;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DownloadAction {
    Pause,
    Resume,
}

/// A control the user pressed. The caller forwards it to the worker. Public because the app's
/// public download method takes it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DownloadIntent {
    pub id: String,
    pub action: DownloadAction,
}

/// One job as the page shows it. Borrowed from the caller; no URL is carried here.
pub(crate) struct DownloadRow<'a> {
    pub id: &'a str,
    pub title: &'a str,
    pub job: &'a DownloadJob,
    /// True while a transfer thread owns the job.
    pub running: bool,
    pub notice: Option<Notice>,
}

/// The text shown for one job, computed without any UI state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct StatusLines {
    pub label: &'static str,
    pub notice: Option<&'static str>,
}

/// Which buttons a row offers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Controls {
    pub pause: bool,
    pub resume: bool,
}

pub(crate) fn state_label(state: DownloadState) -> &'static str {
    match state {
        DownloadState::Queued => ui_text(ids::DOWNLOADS_STATE_QUEUED),
        DownloadState::Negotiating => ui_text(ids::DOWNLOADS_STATE_NEGOTIATING),
        DownloadState::Downloading => ui_text(ids::DOWNLOADS_STATE_DOWNLOADING),
        DownloadState::Verifying => ui_text(ids::DOWNLOADS_STATE_VERIFYING),
        DownloadState::Committed => ui_text(ids::DOWNLOADS_STATE_COMMITTED),
        DownloadState::Paused => ui_text(ids::DOWNLOADS_STATE_PAUSED),
        DownloadState::Cancelled => ui_text(ids::DOWNLOADS_STATE_CANCELLED),
        DownloadState::Failed => ui_text(ids::DOWNLOADS_STATE_FAILED),
        DownloadState::NeedsAuth => ui_text(ids::DOWNLOADS_STATE_NEEDS_AUTH),
    }
}

/// A running job shows as downloading whatever its saved state is.
pub(crate) fn row_label(state: DownloadState, running: bool) -> &'static str {
    if running {
        state_label(DownloadState::Downloading)
    } else {
        state_label(state)
    }
}

pub(crate) fn status_lines(state: DownloadState, notice: Option<Notice>) -> StatusLines {
    StatusLines {
        label: state_label(state),
        notice: notice.map(Notice::text),
    }
}

/// Pause is offered while a transfer owns the job, or where the state machine allows it.
/// Resume is offered only for a stopped job that the model can resume. The probes work on
/// clones and leave the job unchanged.
pub(crate) fn controls(job: &DownloadJob, running: bool) -> Controls {
    Controls {
        pause: running || can_pause(job),
        resume: !running && can_resume(job),
    }
}

fn can_pause(job: &DownloadJob) -> bool {
    job.clone().pause().is_ok()
}

fn can_resume(job: &DownloadJob) -> bool {
    job.clone().resume().is_ok()
}

/// Applies a pause or resume through the existing `DownloadJob` transitions. The page only
/// builds intents, so the tests use this to check the button rules against the model.
#[cfg(test)]
pub(crate) fn apply_action(
    job: &mut DownloadJob,
    action: DownloadAction,
) -> Result<(), DownloadTransitionError> {
    match action {
        DownloadAction::Pause => job.pause(),
        DownloadAction::Resume => job.resume(),
    }
}

fn display_title(title: &str) -> String {
    title.chars().take(MAX_TITLE_CHARS).collect()
}

pub(crate) fn show(ui: &mut Ui, rows: &[DownloadRow<'_>], colors: Palette) -> Vec<DownloadIntent> {
    let mut intents = Vec::new();
    ui.label(
        RichText::new(ui_text(ids::DOWNLOADS_HEADING))
            .color(colors.text)
            .strong()
            .size(28.0),
    );
    ui.add_space(6.0);
    if rows.is_empty() {
        ui.label(RichText::new(ui_text(ids::DOWNLOADS_EMPTY)).color(colors.muted));
        return intents;
    }
    for row in rows.iter().take(MAX_ROWS) {
        let state = row.job.snapshot().state;
        let status = status_lines(state, row.notice);
        let controls = controls(row.job, row.running);
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    RichText::new(display_title(row.title))
                        .color(colors.text)
                        .strong()
                        .size(16.0),
                );
                ui.label(
                    RichText::new(row_label(state, row.running))
                        .color(colors.muted)
                        .strong(),
                );
                if ui
                    .add_enabled(
                        controls.pause,
                        egui::Button::new(ui_text(ids::DOWNLOADS_BUTTON_PAUSE)),
                    )
                    .clicked()
                {
                    intents.push(DownloadIntent {
                        id: row.id.to_owned(),
                        action: DownloadAction::Pause,
                    });
                }
                if ui
                    .add_enabled(
                        controls.resume,
                        egui::Button::new(ui_text(ids::DOWNLOADS_BUTTON_RESUME)),
                    )
                    .clicked()
                {
                    intents.push(DownloadIntent {
                        id: row.id.to_owned(),
                        action: DownloadAction::Resume,
                    });
                }
            });
            if let Some(notice) = status.notice {
                ui.label(RichText::new(notice).color(colors.muted));
            }
        });
    }
    intents
}

#[cfg(test)]
mod tests {
    use super::*;
    use kattri_client_core::downloads::{RangeResponse, Validators};

    const SPEC_NOTICE: &str = "This server does not support resuming downloads. The download started again from the beginning.";

    const ALL_STATES: [DownloadState; 9] = [
        DownloadState::Queued,
        DownloadState::Negotiating,
        DownloadState::Downloading,
        DownloadState::Verifying,
        DownloadState::Committed,
        DownloadState::Paused,
        DownloadState::Cancelled,
        DownloadState::Failed,
        DownloadState::NeedsAuth,
    ];

    /// A job in `Downloading` with `offset` bytes flushed out of 10.
    fn downloading_at(offset: u64) -> DownloadJob {
        let mut job = DownloadJob::new();
        job.begin_negotiation().expect("negotiate");
        job.handle_response(RangeResponse::Ok {
            content_length: Some(10),
            validators: Validators::default(),
        })
        .expect("start");
        job.record_durable_bytes(offset).expect("flush");
        job
    }

    #[test]
    fn every_state_has_its_own_label() {
        let expected = [
            "Queued",
            "Checking the server",
            "Downloading",
            "Verifying",
            "Downloaded",
            "Paused",
            "Cancelled",
            "Failed",
            "Sign in needed",
        ];
        for (state, label) in ALL_STATES.into_iter().zip(expected) {
            assert_eq!(state_label(state), label, "{state:?}");
        }
    }

    #[test]
    fn notice_shows_for_each_state_only_when_one_is_set() {
        for state in ALL_STATES {
            assert_eq!(
                status_lines(state, None),
                StatusLines {
                    label: state_label(state),
                    notice: None,
                },
                "{state:?} without notice"
            );
            assert_eq!(
                status_lines(state, Some(Notice::RangesIgnored)),
                StatusLines {
                    label: state_label(state),
                    notice: Some(SPEC_NOTICE),
                },
                "{state:?} with RangesIgnored"
            );
        }
    }

    #[test]
    fn ranges_ignored_notice_is_the_spec_sentence_without_links_or_tokens() {
        let text = Notice::RangesIgnored.text();
        assert_eq!(text, SPEC_NOTICE);
        assert!(!text.contains("://"));
        assert!(!text.contains("api_key"));
        assert!(!text.contains("token"));
    }

    #[test]
    fn running_job_is_labelled_downloading_and_can_pause() {
        let job = downloading_at(4);
        assert_eq!(row_label(DownloadState::Queued, true), "Downloading");
        assert_eq!(row_label(DownloadState::Paused, false), "Paused");
        assert_eq!(
            controls(&job, true),
            Controls {
                pause: true,
                resume: false,
            }
        );
    }

    #[test]
    fn paused_job_offers_resume_only() {
        let mut job = downloading_at(4);
        apply_action(&mut job, DownloadAction::Pause).expect("pause");
        assert_eq!(
            controls(&job, false),
            Controls {
                pause: false,
                resume: true,
            }
        );
    }

    #[test]
    fn pause_then_resume_follows_the_state_machine() {
        let mut job = downloading_at(4);
        assert!(can_pause(&job));
        assert!(!can_resume(&job));

        apply_action(&mut job, DownloadAction::Pause).expect("pause");
        assert_eq!(job.snapshot().state, DownloadState::Paused);
        assert_eq!(job.snapshot().durable_offset, 4);
        assert!(!can_pause(&job));
        assert!(can_resume(&job));

        apply_action(&mut job, DownloadAction::Resume).expect("resume");
        assert_eq!(job.snapshot().state, DownloadState::Queued);
        assert_eq!(job.snapshot().durable_offset, 4);
    }

    #[test]
    fn resume_is_refused_while_downloading() {
        let mut job = downloading_at(4);
        assert_eq!(
            apply_action(&mut job, DownloadAction::Resume),
            Err(DownloadTransitionError::InvalidTransition {
                from: DownloadState::Downloading,
                to: DownloadState::Paused,
            })
        );
        assert_eq!(job.snapshot().state, DownloadState::Downloading);
    }

    #[test]
    fn pause_is_refused_after_commit() {
        let mut job = downloading_at(10);
        job.finish_download().expect("finish");
        job.commit_verified().expect("commit");
        assert!(!can_pause(&job));
        assert_eq!(
            apply_action(&mut job, DownloadAction::Pause),
            Err(DownloadTransitionError::InvalidTransition {
                from: DownloadState::Committed,
                to: DownloadState::Paused,
            })
        );
    }

    #[test]
    fn button_probes_do_not_change_the_job() {
        let job = downloading_at(4);
        let before = job.clone();
        let _ = can_pause(&job);
        let _ = can_resume(&job);
        assert_eq!(job, before);
    }

    #[test]
    fn long_titles_are_cut_to_the_limit() {
        let long = "a".repeat(MAX_TITLE_CHARS + 44);
        assert_eq!(display_title(&long).chars().count(), MAX_TITLE_CHARS);
        assert_eq!(display_title("Short"), "Short");
    }
}
