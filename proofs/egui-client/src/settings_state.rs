//! Typed settings for the client, bound to the active profile's settings file.
//!
//! Profiles live under the user's config directory. The settings load when the app starts,
//! and the settings page reloads them each time a category opens. Each change is written
//! back through the profile store. The settings crate refuses credential-like keys on load
//! and save, so no token or password reaches this module.

use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex,
        mpsc::{self, Receiver},
    },
    thread::{self, JoinHandle},
};

use kattri_client_core::{
    queue::Queue,
    queue_store::{QueueStoreError, StoredLoadOutcome},
};
use kattri_input::input_store::input_maps_dir;
use kattri_settings::{LoadWarning, ProfileStore, Settings};

use crate::ui::profile_picker::plan_launch;

const PROFILES_DIR_NAME: &str = "profiles";
const QUEUE_FILE_NAME: &str = "queue.json";

/// Folder that holds the profile index and each profile's settings file, or `None` when
/// no per-user config folder is known.
pub(crate) fn default_profiles_dir() -> Option<PathBuf> {
    directories::ProjectDirs::from("org", "Jellyfin", "Kattri")
        .map(|dirs| dirs.config_dir().join(PROFILES_DIR_NAME))
}

pub(crate) struct SettingsState {
    store: Option<ProfileStore>,
    /// The profile this run reads and writes. It is the index's active profile unless
    /// `--profile` chose another one for this run only.
    profile: Option<String>,
    offers_launch_picker: bool,
    launch_notice: Option<String>,
    writer: Option<SettingsWriter>,
    settings: Settings,
    notice: Option<String>,
    save_failure_logged: bool,
}

struct SaveRequest {
    revision: u64,
    settings: Settings,
}

struct SettingsWriter {
    queue: Arc<(Mutex<WriterQueue>, Condvar)>,
    results: Receiver<(u64, Result<(), String>)>,
    thread: Option<JoinHandle<()>>,
    next_revision: u64,
    /// The most recent save result received, so a switch can tell whether the latest
    /// snapshot reached disk.
    latest: Option<(u64, Result<(), String>)>,
}

#[derive(Default)]
struct WriterQueue {
    pending: Option<SaveRequest>,
    shutting_down: bool,
}

impl SettingsWriter {
    fn spawn(store: &ProfileStore, profile: &str) -> std::io::Result<Self> {
        let worker_store = store.clone();
        let profile = profile.to_owned();
        let queue = Arc::new((Mutex::new(WriterQueue::default()), Condvar::new()));
        let worker_queue = Arc::clone(&queue);
        let (result_tx, results) = mpsc::channel();
        let worker = thread::Builder::new()
            .name("kattri-settings-writer".to_owned())
            .spawn(move || {
                loop {
                    let request = {
                        let (queue_lock, changed) = &*worker_queue;
                        let mut state = queue_lock
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        while state.pending.is_none() && !state.shutting_down {
                            state = changed
                                .wait(state)
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                        }
                        match state.pending.take() {
                            Some(request) => Some(request),
                            None if state.shutting_down => None,
                            None => continue,
                        }
                    };
                    let Some(request) = request else {
                        break;
                    };
                    let result = worker_store
                        .save_settings(&profile, &request.settings)
                        .map_err(|error| error.to_string());
                    let _ = result_tx.send((request.revision, result));
                }
            })?;

        Ok(Self {
            queue,
            results,
            thread: Some(worker),
            next_revision: 0,
            latest: None,
        })
    }

    fn enqueue(&mut self, settings: Settings) -> Result<u64, &'static str> {
        let revision = self
            .next_revision
            .checked_add(1)
            .ok_or("Settings save revision limit reached.")?;
        self.next_revision = revision;
        if self.thread.as_ref().is_some_and(JoinHandle::is_finished) {
            return Err("Settings writer is unavailable.");
        }
        let (queue_lock, changed) = &*self.queue;
        let mut queue = queue_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        queue.pending = Some(SaveRequest { revision, settings });
        changed.notify_one();
        Ok(revision)
    }

    fn drain_results(&mut self) -> Vec<(u64, Result<(), String>)> {
        let mut results = Vec::new();
        while let Ok(result) = self.results.try_recv() {
            results.push(result);
        }
        if let Some((revision, result)) = results.last() {
            self.latest = Some((*revision, result.clone()));
        }
        results
    }

    /// Stop the writer after the queued snapshot is saved. Returns the latest save's outcome,
    /// so a switch that restarts the app does not drop settings that failed to reach disk.
    fn finish(mut self) -> Result<(), String> {
        {
            let (queue_lock, changed) = &*self.queue;
            queue_lock
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .shutting_down = true;
            changed.notify_one();
        }
        if let Some(worker) = self.thread.take()
            && worker.join().is_err()
        {
            return Err("the settings writer stopped unexpectedly".to_owned());
        }
        self.drain_results();
        match &self.latest {
            Some((revision, Err(error))) if *revision == self.next_revision => Err(error.clone()),
            _ => Ok(()),
        }
    }

    #[cfg(test)]
    fn receive_result(&self) -> Result<(u64, Result<(), String>), mpsc::RecvTimeoutError> {
        self.results.recv_timeout(std::time::Duration::from_secs(2))
    }

    /// True until the newest queued snapshot has an acknowledged result.
    fn in_flight(&self) -> bool {
        let acknowledged = self.latest.as_ref().map_or(0, |(revision, _)| *revision);
        self.next_revision > acknowledged
    }
}

impl Drop for SettingsWriter {
    fn drop(&mut self) {
        let (queue_lock, changed) = &*self.queue;
        queue_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .shutting_down = true;
        changed.notify_one();
        if let Some(worker) = self.thread.take()
            && worker.join().is_err()
        {
            tracing::warn!("settings writer thread panicked during shutdown");
        }
    }
}

impl SettingsState {
    /// Open the profiles in `dir` and load the profile this run uses: `requested` when it is
    /// given, otherwise the index's active profile (see `plan_launch`). When the folder is
    /// unknown or cannot be opened, the defaults are used in memory and `notice` says so.
    pub(crate) fn open(dir: Option<&Path>, requested: Option<&str>) -> Self {
        let Some(dir) = dir else {
            return Self::unavailable("No settings folder is available; settings are not saved.");
        };
        match ProfileStore::open(dir) {
            Ok(store) => {
                let plan = plan_launch(
                    store.names(),
                    store.active_profile(),
                    store.default_profile(),
                    requested,
                    |name| store.profile_dir(name).is_ok_and(|path| path.is_dir()),
                );
                let mut state = Self {
                    writer: None,
                    store: Some(store),
                    profile: Some(plan.profile),
                    offers_launch_picker: plan.show_picker,
                    launch_notice: plan.notice,
                    settings: Settings::default(),
                    notice: None,
                    save_failure_logged: false,
                };
                state.writer = state.spawn_writer();
                let writer_unavailable = state.writer.is_none();
                state.reload();
                if writer_unavailable && state.notice.is_none() {
                    state.notice = Some(
                        "Settings could not be saved. Check the settings folder and try again."
                            .to_owned(),
                    );
                }
                state
            }
            Err(error) => {
                tracing::warn!(%error, "profile store could not be opened");
                Self::unavailable("Settings could not be opened; defaults are in use.")
            }
        }
    }

    fn unavailable(notice: &str) -> Self {
        Self {
            store: None,
            profile: None,
            offers_launch_picker: false,
            launch_notice: None,
            writer: None,
            settings: Settings::default(),
            notice: Some(notice.to_owned()),
            save_failure_logged: false,
        }
    }

    /// Start the writer thread for this run's profile. `None` when the thread cannot start.
    fn spawn_writer(&self) -> Option<SettingsWriter> {
        let store = self.store.as_ref()?;
        let profile = self.profile.as_deref()?;
        match SettingsWriter::spawn(store, profile) {
            Ok(writer) => Some(writer),
            Err(error) => {
                tracing::warn!(%error, "settings writer could not be started");
                None
            }
        }
    }

    pub(crate) fn settings(&self) -> &Settings {
        &self.settings
    }

    pub(crate) fn settings_mut(&mut self) -> &mut Settings {
        &mut self.settings
    }

    /// Notice for the settings page, such as a failed save. `None` when all is well.
    pub(crate) fn notice(&self) -> Option<&str> {
        self.notice.as_deref()
    }

    /// Notice from launch, such as a missing profile folder. The app shows it once.
    pub(crate) fn launch_notice(&self) -> Option<&str> {
        self.launch_notice.as_deref()
    }

    /// Whether the launch picker should open for this run (several profiles, no `--profile`).
    pub(crate) fn offers_launch_picker(&self) -> bool {
        self.offers_launch_picker
    }

    /// Name of the profile this run uses, or `None` when no profile store is open.
    pub(crate) fn running_name(&self) -> Option<&str> {
        self.profile.as_deref()
    }

    /// The profile store, for the picker. `None` when no profile store is open.
    pub(crate) fn profile_store_mut(&mut self) -> Option<&mut ProfileStore> {
        self.store.as_mut()
    }

    /// The running profile's `inputmaps` folder (M2-17), or `None` when no profile store is open.
    pub(crate) fn keymap_dir(&self) -> Option<PathBuf> {
        let store = self.store.as_ref()?;
        let name = self.profile.as_deref()?;
        store.profile_dir(name).ok().map(|dir| input_maps_dir(&dir))
    }

    /// Path to the running profile's persisted queue, or `None` when no profile store is open.
    pub(crate) fn queue_path(&self) -> Option<PathBuf> {
        let store = self.store.as_ref()?;
        let name = self.profile.as_deref()?;
        store
            .profile_dir(name)
            .ok()
            .map(|dir| dir.join(QUEUE_FILE_NAME))
    }

    /// Wait until the queued settings snapshot is on disk, ahead of a switch that restarts the
    /// app. The writer is started again afterwards, so later saves still work if the switch
    /// is refused.
    pub(crate) fn finish_pending_saves(&mut self) -> Result<(), String> {
        let Some(writer) = self.writer.take() else {
            return Err("settings are not being saved in this session".to_owned());
        };
        let outcome = writer.finish();
        self.writer = self.spawn_writer();
        outcome
    }

    /// Load the active profile's queue snapshot. `None` means there is no profile store;
    /// file and codec outcomes remain explicit in the returned queue-store result.
    pub(crate) fn load_queue(&self) -> Option<Result<StoredLoadOutcome, QueueStoreError>> {
        let path = self.queue_path()?;
        Some(kattri_client_core::queue_store::load_queue_with_position(
            &path,
        ))
    }

    /// Save only the queue-store contract for the active profile. `None` means there is no
    /// profile store, while I/O and encoding failures are returned without being hidden.
    pub(crate) fn save_queue(
        &self,
        queue: &Queue,
        position_ms: u64,
    ) -> Option<Result<(), QueueStoreError>> {
        let path = self.queue_path()?;
        Some(kattri_client_core::queue_store::save_queue_with_position(
            &path,
            queue,
            position_ms,
        ))
    }

    /// Read the active profile from disk. A file that cannot be read keeps the current
    /// values and sets a notice, so a bad file never blanks the page.
    pub(crate) fn reload(&mut self) {
        let (Some(store), Some(name)) = (self.store.as_ref(), self.profile.as_deref()) else {
            return;
        };
        // A queued save is newer than the file. Reading the file now would drop it, such as a
        // series language pick made during playback (M2-10), so keep the values until it lands.
        if self.writer.as_ref().is_some_and(SettingsWriter::in_flight) {
            return;
        }
        match store.load_settings(name) {
            Ok((settings, warnings)) => {
                let mut future_schema = false;
                for warning in warnings {
                    match warning {
                        LoadWarning::CorruptFile(file) => {
                            tracing::warn!(file = %file, "malformed settings file moved aside; defaults loaded");
                        }
                        LoadWarning::ClampedField(field) => {
                            tracing::warn!(field, "out-of-range settings value was clamped");
                        }
                        LoadWarning::SecretLikeValue(field) => {
                            tracing::warn!(field = %field, "credential-like settings value was dropped");
                        }
                        LoadWarning::FutureSchema(version) => {
                            future_schema = true;
                            tracing::warn!(
                                version,
                                "settings schema is newer; using read-only defaults"
                            );
                        }
                    }
                }
                self.settings = settings;
                self.notice = future_schema.then(|| {
                    "Settings were written by a newer version and are read-only until Kattri is updated."
                        .to_owned()
                });
            }
            Err(error) => {
                tracing::warn!(%error, "active profile settings could not be read");
                self.notice = Some(
                    "The saved settings could not be read; current values are kept.".to_owned(),
                );
            }
        }
    }

    /// Save only when the values differ from `before`, the copy taken before the controls
    /// were drawn. Disk I/O is queued to the dedicated writer thread.
    pub(crate) fn save_if_changed(&mut self, before: &Settings) {
        if self.settings == *before {
            return;
        }
        let Some(writer) = self.writer.as_mut() else {
            self.notice = Some(
                "Settings could not be saved. Check the settings folder and try again.".to_owned(),
            );
            return;
        };
        match writer.enqueue(self.settings.clone()) {
            Ok(_) => self.notice = None,
            Err(_) => {
                self.notice = Some(
                    "Settings could not be saved. Check the settings folder and try again."
                        .to_owned(),
                )
            }
        }
    }

    /// Block until the newest queued snapshot is acknowledged by the writer thread.
    #[cfg(test)]
    pub(crate) fn wait_for_pending_save(&mut self) {
        let Some(writer) = self.writer.as_mut() else {
            return;
        };
        while writer.in_flight() {
            let Ok((revision, result)) = writer.receive_result() else {
                break;
            };
            writer.latest = Some((revision, result));
        }
    }

    /// Apply asynchronous save results to the settings-page notice. A stale result is ignored
    /// when a newer snapshot has already been queued.
    pub(crate) fn poll_save_results(&mut self) {
        let Some(writer) = self.writer.as_mut() else {
            return;
        };
        let latest_revision = writer.next_revision;
        for (revision, result) in writer.drain_results() {
            self.apply_save_result(latest_revision, revision, result);
        }
    }

    fn apply_save_result(
        &mut self,
        latest_revision: u64,
        revision: u64,
        result: Result<(), String>,
    ) {
        if revision != latest_revision {
            return;
        }
        match result {
            Ok(()) => self.notice = None,
            Err(error) => {
                if !self.save_failure_logged {
                    tracing::warn!(%error, "settings could not be saved");
                    self.save_failure_logged = true;
                }
                self.notice = Some(
                    "Settings could not be saved. Check the settings folder and try again."
                        .to_owned(),
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::SettingsState;
    use jellyfin_api::session_reporting::{QueueItem, RepeatMode};
    use kattri_client_core::{queue::Queue, queue_store::StoredLoadOutcome};
    use kattri_settings::{ProfileStore, Settings};

    fn temp_profile_store(label: &str) -> (std::path::PathBuf, SettingsState) {
        let dir = std::env::temp_dir().join(format!(
            "kattri-settings-state-{label}-{}",
            uuid::Uuid::new_v4()
        ));
        let state = SettingsState::open(Some(&dir), None);
        (dir, state)
    }

    fn queue_with_state() -> Queue {
        let mut queue = Queue::with_seed(17);
        for id in ["a1", "b2", "c3"] {
            queue.add(QueueItem::new(id)).expect("queue item adds");
        }
        queue.set_repeat(RepeatMode::All);
        queue.set_shuffle(true);
        queue.next_track();
        queue
    }

    #[test]
    fn queue_path_uses_the_default_active_profile() {
        let (dir, state) = temp_profile_store("queue-path");
        assert_eq!(
            state.queue_path(),
            Some(dir.join("default").join("queue.json"))
        );
        drop(state);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn queue_snapshot_round_trips_queue_state_and_position() {
        let (dir, state) = temp_profile_store("queue-round-trip");
        let queue = queue_with_state();
        let expected_queue = queue.encode().expect("queue encodes");

        assert!(
            state
                .save_queue(&queue, 123_456)
                .expect("profile path exists")
                .is_ok()
        );
        let stored = match state
            .load_queue()
            .expect("profile path exists")
            .expect("queue snapshot loads")
        {
            StoredLoadOutcome::Loaded(stored) => stored,
            other => panic!("expected loaded queue snapshot, got {other:?}"),
        };

        assert_eq!(stored.position_ms, 123_456);
        assert_eq!(
            stored.queue.encode().expect("restored queue encodes"),
            expected_queue
        );
        drop(state);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn missing_profile_store_has_no_queue_path_or_operations() {
        let state = SettingsState::open(None, None);
        assert!(state.queue_path().is_none());
        assert!(state.load_queue().is_none());
        assert!(state.save_queue(&Queue::new(), 0).is_none());
    }

    #[test]
    fn missing_folder_falls_back_to_defaults_with_a_notice() {
        let state = SettingsState::open(None, None);
        assert_eq!(state.settings(), &Settings::default());
        assert!(state.notice().is_some());
        assert!(state.running_name().is_none());
    }

    #[test]
    fn unsaved_state_surfaces_failure_when_a_setting_changes() {
        let mut state = SettingsState::open(None, None);
        let before = state.settings().clone();
        state.settings_mut().audio.normalize = !before.audio.normalize;
        state.save_if_changed(&before);
        assert!(
            state
                .notice()
                .is_some_and(|notice| notice.contains("could not be saved"))
        );
    }

    #[test]
    fn unchanged_values_do_not_produce_a_notice() {
        let mut state = SettingsState::open(None, None);
        let before = state.settings().clone();
        state.notice = None;
        state.save_if_changed(&before);
        assert!(state.notice().is_none());
    }

    #[test]
    fn a_change_is_written_to_the_active_profile_and_read_back() {
        let dir =
            std::env::temp_dir().join(format!("kattri-settings-state-{}", uuid::Uuid::new_v4()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut state = SettingsState::open(Some(&dir), None);
        assert!(state.running_name().is_some());
        let before = state.settings().clone();
        state.settings_mut().audio.normalize = !before.audio.normalize;
        state.save_if_changed(&before);
        let (revision, result) = state
            .writer
            .as_ref()
            .expect("profile opens writer")
            .receive_result()
            .expect("save result arrives");
        assert_eq!(revision, 1);
        assert!(result.is_ok());
        state.poll_save_results();
        assert!(state.notice().is_none());

        let mut reopened = SettingsState::open(Some(&dir), None);
        assert_eq!(reopened.settings().audio.normalize, !before.audio.normalize);
        reopened.settings_mut().audio.normalize = before.audio.normalize;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rapid_changes_coalesce_to_the_latest_settings_snapshot() {
        let dir =
            std::env::temp_dir().join(format!("kattri-settings-coalesce-{}", uuid::Uuid::new_v4()));
        let mut state = SettingsState::open(Some(&dir), None);
        let initial = state.settings().clone();
        state.settings_mut().audio.normalize = !initial.audio.normalize;
        state.save_if_changed(&initial);
        let before_latest = state.settings().clone();
        state.settings_mut().audio.normalize = initial.audio.normalize;
        state.settings_mut().audio.gapless = !initial.audio.gapless;
        assert_ne!(state.settings(), &before_latest);
        state.save_if_changed(&before_latest);

        let writer = state.writer.as_ref().expect("profile opens writer");
        let mut latest_result = None;
        for _ in 0..2 {
            let (revision, result) = writer.receive_result().expect("save result arrives");
            if revision == 2 {
                latest_result = Some(result);
                break;
            }
        }
        assert!(latest_result.expect("latest save completes").is_ok());
        let (saved, warnings) = state
            .store
            .as_ref()
            .expect("profile store remains available")
            .load_settings("default")
            .expect("saved settings load");
        assert!(warnings.is_empty());
        assert_eq!(saved.audio.normalize, initial.audio.normalize);
        assert_eq!(saved.audio.gapless, !initial.audio.gapless);
        drop(state);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn shutdown_flushes_the_latest_queued_settings_before_returning() {
        let dir =
            std::env::temp_dir().join(format!("kattri-settings-shutdown-{}", uuid::Uuid::new_v4()));
        {
            let mut state = SettingsState::open(Some(&dir), None);
            let before = state.settings().clone();
            state.settings_mut().audio.gapless = !before.audio.gapless;
            state.save_if_changed(&before);
        }

        let reopened = SettingsState::open(Some(&dir), None);
        assert_ne!(
            reopened.settings().audio.gapless,
            Settings::default().audio.gapless
        );
        drop(reopened);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn launch_flag_opens_that_profile_without_changing_the_index() {
        let dir = std::env::temp_dir().join(format!(
            "kattri-settings-launch-flag-{}",
            uuid::Uuid::new_v4()
        ));
        let mut store = ProfileStore::open(&dir).expect("profile store opens");
        store.create("kids").expect("kids profile creates");
        let mut kids = Settings::default();
        kids.audio.gapless = !Settings::default().audio.gapless;
        store
            .save_settings("kids", &kids)
            .expect("kids settings save");
        drop(store);

        let state = SettingsState::open(Some(&dir), Some("kids"));
        assert_eq!(state.running_name(), Some("kids"));
        assert_eq!(state.settings().audio.gapless, kids.audio.gapless);
        assert!(state.launch_notice().is_none());
        assert_eq!(
            ProfileStore::open(&dir)
                .expect("profile store reopens")
                .active_profile(),
            "default"
        );
        drop(state);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_active_profile_folder_falls_back_to_default_with_a_notice() {
        let dir = std::env::temp_dir().join(format!(
            "kattri-settings-missing-folder-{}",
            uuid::Uuid::new_v4()
        ));
        let mut store = ProfileStore::open(&dir).expect("profile store opens");
        store.create("kids").expect("kids profile creates");
        store.set_active("kids").expect("kids becomes active");
        let kids_dir = store.profile_dir("kids").expect("kids is listed");
        drop(store);
        std::fs::remove_dir_all(&kids_dir).expect("kids folder removes");

        let state = SettingsState::open(Some(&dir), None);
        assert_eq!(state.running_name(), Some("default"));
        assert!(
            state
                .launch_notice()
                .is_some_and(|notice| notice.contains("`kids`"))
        );
        drop(state);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn finish_pending_saves_writes_the_latest_change_and_keeps_saving() {
        let dir =
            std::env::temp_dir().join(format!("kattri-settings-finish-{}", uuid::Uuid::new_v4()));
        let mut state = SettingsState::open(Some(&dir), None);
        let before = state.settings().clone();
        state.settings_mut().audio.gapless = !before.audio.gapless;
        state.save_if_changed(&before);
        assert!(state.finish_pending_saves().is_ok());
        let (saved, _) = ProfileStore::open(&dir)
            .expect("profile store reopens")
            .load_settings("default")
            .expect("saved settings load");
        assert_eq!(saved.audio.gapless, !before.audio.gapless);

        // The writer starts again after a finish, so a later change still reaches disk.
        assert!(state.writer.is_some());
        let before = state.settings().clone();
        state.settings_mut().audio.gapless = !before.audio.gapless;
        state.save_if_changed(&before);
        assert!(state.finish_pending_saves().is_ok());
        let (saved, _) = ProfileStore::open(&dir)
            .expect("profile store reopens")
            .load_settings("default")
            .expect("saved settings load");
        assert_eq!(saved.audio.gapless, !before.audio.gapless);
        drop(state);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn failed_save_is_reported_without_rewriting_a_future_schema_file() {
        let dir = std::env::temp_dir().join(format!(
            "kattri-settings-future-save-{}",
            uuid::Uuid::new_v4()
        ));
        let store = ProfileStore::open(&dir).expect("profile store opens");
        let file = store
            .profile_dir("default")
            .expect("default profile exists")
            .join("settings.toml");
        std::fs::write(&file, "schema_version = 99\n").expect("future schema fixture writes");
        let original = std::fs::read(&file).expect("fixture bytes read");
        drop(store);

        let mut state = SettingsState::open(Some(&dir), None);
        let before = state.settings().clone();
        state.settings_mut().audio.gapless = !before.audio.gapless;
        state.save_if_changed(&before);
        let (revision, result) = state
            .writer
            .as_ref()
            .expect("profile opens writer")
            .receive_result()
            .expect("failure result arrives");
        assert!(result.is_err());
        state.apply_save_result(1, revision, result);
        assert!(
            state
                .notice()
                .is_some_and(|notice| notice.contains("could not be saved"))
        );
        assert_eq!(
            std::fs::read(&file).expect("fixture remains readable"),
            original
        );
        drop(state);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
