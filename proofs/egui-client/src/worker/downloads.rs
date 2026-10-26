//! Download jobs on the network worker.
//!
//! The worker owns the [`DownloadManager`]. Opening the profile folder, starting, pausing and
//! resuming jobs touch disk or the network, so each runs as a worker command and never on the
//! UI thread. The functions here turn manager results into worker events. They hold no state
//! of their own beyond what the manager keeps.

use std::collections::HashMap;
use std::path::Path;
use std::time::Instant;

use jellyfin_api::SessionAuthorization;
use kattri_client_core::downloads::{DownloadJob, DownloadSnapshot, DownloadState, Validators};
use kattri_downloads::manager::{DownloadManager, JobView, ManagerError};
use kattri_downloads::messages::Notice;

use super::{WorkerCommand, WorkerEvent};
use crate::ui::downloads::{DownloadAction, DownloadIntent};

pub(crate) const NOT_OPEN_MESSAGE: &str = "Downloads are not ready yet. Try again in a moment.";
pub(crate) const SIGN_IN_MESSAGE: &str = "Sign in to the server before downloading.";
const UNTITLED: &str = "Downloaded item";

/// One download as the page shows it. The job is rebuilt with the client-core model, so the
/// page's button checks use the same transitions as the rest of the download code.
// The Downloads page reads these fields once ui/mod.rs routes Page::Downloads to it. Until
// that routing exists, only the tests read them.
#[allow(dead_code)]
#[derive(Clone, Debug)]
pub struct DownloadRow {
    pub item_id: String,
    pub job: DownloadJob,
    /// A transfer thread owns the job. The page shows it as downloading and offers pause.
    pub running: bool,
    pub notice: Option<Notice>,
}

/// Download state the app keeps between worker events.
#[derive(Debug, Default)]
pub struct DownloadsState {
    pub rows: Vec<DownloadRow>,
    /// Titles live only in memory, so the job list file never holds them.
    titles: HashMap<String, String>,
    pub opened: bool,
    pub error: Option<String>,
    pub last_refresh: Option<Instant>,
}

impl DownloadsState {
    pub fn apply_rows(&mut self, rows: Vec<DownloadRow>) {
        self.rows = rows;
        self.error = None;
    }

    pub fn fail(&mut self, message: String) {
        self.error = Some(message);
    }

    pub fn remember_title(&mut self, item_id: &str, title: &str) {
        self.titles.insert(item_id.to_owned(), title.to_owned());
    }

    /// The page's title for a row. Read by the page once it is routed.
    #[allow(dead_code)]
    pub fn title(&self, item_id: &str) -> &str {
        self.titles.get(item_id).map_or(UNTITLED, String::as_str)
    }

    pub fn has_running(&self) -> bool {
        self.rows.iter().any(|row| row.running)
    }
}

pub(crate) fn open(dir: &Path) -> Result<DownloadManager, &'static str> {
    DownloadManager::open(dir).map_err(|error| message_for(&error))
}

/// Rows for every saved job, sorted by item ID as the manager returns them.
pub(crate) fn rows(manager: &DownloadManager) -> Vec<DownloadRow> {
    rows_from_views(&manager.list())
}

fn rows_from_views(views: &[JobView]) -> Vec<DownloadRow> {
    views.iter().map(row_from_view).collect()
}

fn row_from_view(view: &JobView) -> DownloadRow {
    // The manager does not expose a live state while a transfer runs. The saved job is
    // Queued in that case, and the row shows Downloading from `running`.
    let state = match view.state {
        DownloadState::Negotiating | DownloadState::Downloading => DownloadState::Queued,
        other => other,
    };
    let snapshot = DownloadSnapshot {
        state,
        validators: Validators::default(),
        expected_total_length: view.expected_total_length,
        durable_offset: view.durable_offset,
    };
    DownloadRow {
        item_id: view.item_id.clone(),
        // The manager saves only snapshots that `from_snapshot` accepts. The fallback keeps the
        // row on the page instead of dropping a saved download if that ever changes.
        job: DownloadJob::from_snapshot(snapshot).unwrap_or_default(),
        running: view.running,
        notice: view.notice,
    }
}

pub(crate) fn list_event(manager: Option<&DownloadManager>) -> WorkerEvent {
    WorkerEvent::DownloadsUpdated {
        rows: manager.map(rows).unwrap_or_default(),
    }
}

pub(crate) fn failure_event(message: &str) -> WorkerEvent {
    WorkerEvent::DownloadFailed {
        message: message.to_owned(),
    }
}

/// After a command that changed a job, the event carries the new list. A refused command
/// carries its message instead.
fn after(manager: Option<&DownloadManager>, result: Result<(), &'static str>) -> WorkerEvent {
    match result {
        Ok(()) => list_event(manager),
        Err(message) => failure_event(message),
    }
}

pub(crate) fn start_event(
    manager: Option<&DownloadManager>,
    server_id: Option<&str>,
    base: &str,
    item_id: &str,
    auth: &SessionAuthorization,
) -> WorkerEvent {
    let result = match (manager, server_id) {
        (None, _) => Err(NOT_OPEN_MESSAGE),
        (Some(_), None) => Err("This server did not report an ID, so downloads are unavailable."),
        (Some(manager), Some(server_id)) => manager
            .start(server_id, base, item_id, auth)
            .map_err(|error| message_for(&error)),
    };
    after(manager, result)
}

pub(crate) fn resume_event(
    manager: Option<&DownloadManager>,
    server_id: Option<&str>,
    base: &str,
    item_id: &str,
    auth: &SessionAuthorization,
) -> WorkerEvent {
    let result = match (manager, server_id) {
        (None, _) => Err(NOT_OPEN_MESSAGE),
        (Some(_), None) => Err("This server did not report an ID, so downloads are unavailable."),
        (Some(manager), Some(server_id)) => manager
            .resume(server_id, base, item_id, auth)
            .map_err(|error| message_for(&error)),
    };
    after(manager, result)
}

pub(crate) fn pause_event(manager: Option<&DownloadManager>, item_id: &str) -> WorkerEvent {
    let result = match manager {
        None => Err(NOT_OPEN_MESSAGE),
        Some(manager) => manager.pause(item_id).map_err(|error| message_for(&error)),
    };
    after(manager, result)
}

/// Asks every running transfer to stop. Called on sign-out, because a transfer keeps the
/// bearer header it started with until it stops.
pub(crate) fn pause_running(manager: &DownloadManager) {
    for view in manager.list().into_iter().filter(|view| view.running) {
        // A job that finished in the meantime has nothing left to pause.
        let _ = manager.pause(&view.item_id);
    }
}

/// Maps a page intent to the worker command that carries it out.
pub(super) fn command_for_intent(intent: &DownloadIntent, base_url: &str) -> WorkerCommand {
    match intent.action {
        DownloadAction::Pause => WorkerCommand::DownloadPause {
            item_id: intent.id.clone(),
        },
        DownloadAction::Resume => WorkerCommand::DownloadResume {
            item_id: intent.id.clone(),
            base_url: base_url.to_owned(),
        },
    }
}

/// User text for a manager error. The match is exhaustive, so a new error variant fails to
/// compile until it has a message. Paths, server addresses and tokens never appear here.
fn message_for(error: &ManagerError) -> &'static str {
    match error {
        ManagerError::Io(_)
        | ManagerError::Fs(_)
        | ManagerError::Persist(_)
        | ManagerError::Encode
        | ManagerError::ListTooLarge => {
            "Downloads could not be saved. Check the profile folder and try again."
        }
        ManagerError::Driver(_) => "The download could not start.",
        ManagerError::InvalidServerId => "This server did not give a usable ID for downloads.",
        ManagerError::AlreadyTracked => "This item already has a download.",
        ManagerError::UnknownItem => "This download is no longer in the list.",
        ManagerError::AlreadyRunning => "This download is already running.",
        ManagerError::NotPausable => "This download cannot be paused right now.",
        ManagerError::NotResumable => "This download cannot be resumed right now.",
        ManagerError::ServerMismatch => "This download belongs to a different server.",
        ManagerError::ThreadStart => "The download could not start. Try again.",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(
        item_id: &str,
        state: DownloadState,
        running: bool,
        durable_offset: u64,
        expected_total_length: Option<u64>,
        notice: Option<Notice>,
    ) -> JobView {
        JobView {
            item_id: item_id.to_owned(),
            state,
            running,
            durable_offset,
            expected_total_length,
            notice,
            verification: None,
            failure: None,
        }
    }

    #[test]
    fn pause_intent_maps_to_the_pause_command() {
        let intent = DownloadIntent {
            id: "item-1".to_owned(),
            action: DownloadAction::Pause,
        };
        let WorkerCommand::DownloadPause { item_id } =
            command_for_intent(&intent, "https://example.test")
        else {
            panic!("a pause intent must send DownloadPause");
        };
        assert_eq!(item_id, "item-1");
    }

    #[test]
    fn resume_intent_maps_to_the_resume_command_with_its_base() {
        let intent = DownloadIntent {
            id: "item-2".to_owned(),
            action: DownloadAction::Resume,
        };
        let WorkerCommand::DownloadResume { item_id, base_url } =
            command_for_intent(&intent, "https://example.test/jf")
        else {
            panic!("a resume intent must send DownloadResume");
        };
        assert_eq!(item_id, "item-2");
        assert_eq!(base_url, "https://example.test/jf");
    }

    #[test]
    fn list_result_updates_the_page_rows() {
        let views = [
            view(
                "paused",
                DownloadState::Paused,
                false,
                512,
                Some(1024),
                Some(Notice::RangesIgnored),
            ),
            view("live", DownloadState::Downloading, true, 0, Some(10), None),
        ];
        let rows = rows_from_views(&views);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].item_id, "paused");
        assert_eq!(rows[0].job.snapshot().state, DownloadState::Paused);
        assert_eq!(rows[0].job.snapshot().durable_offset, 512);
        assert!(!rows[0].running);
        // A live state is not saved, so the running row is rebuilt as Queued.
        assert_eq!(rows[1].job.snapshot().state, DownloadState::Queued);
        assert!(rows[1].running);

        let mut state = DownloadsState::default();
        state.apply_rows(rows);
        assert_eq!(state.rows.len(), 2);
        assert!(state.has_running());
        assert_eq!(state.error, None);
    }

    #[test]
    fn restart_notice_reaches_the_row() {
        let rows = rows_from_views(&[view(
            "a",
            DownloadState::Queued,
            false,
            0,
            None,
            Some(Notice::RangesIgnored),
        )]);
        let text = rows[0].notice.map(Notice::text);
        assert_eq!(
            text,
            Some(
                "This server does not support resuming downloads. The download started again from the beginning."
            )
        );
    }

    #[test]
    fn titles_fall_back_to_a_neutral_label() {
        let mut state = DownloadsState::default();
        state.remember_title("known", "A Film");
        assert_eq!(state.title("known"), "A Film");
        assert_eq!(state.title("unknown"), UNTITLED);
    }
}
