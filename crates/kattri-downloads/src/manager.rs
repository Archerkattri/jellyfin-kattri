//! Download manager for one profile: the set of download jobs, keyed by item ID.
//!
//! The manager starts a job from a session's authorization, and offers `list`, `pause` and
//! `resume`. Each transfer runs on its own thread with a current-thread runtime, so no public
//! method waits for network I/O.
//!
//! The job list is one JSON file in the downloads folder that the caller passes to
//! [`DownloadManager::open`] (see [`JOB_LIST_FILE`]). An entry holds the item ID, the server ID
//! and the job's [`PersistedDownload`] record. It never holds the token, the `Authorization`
//! header or the upstream URL. The URL is rebuilt from the base URL the caller passes to
//! `start` and `resume`.
//!
//! A transfer owns its job while it runs, so [`JobView::running`] is true and the other fields
//! show what the transfer last returned. Live byte progress is not reported here.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;

use jellyfin_api::SessionAuthorization;
use kattri_client_core::downloads::{DownloadJob, DownloadState};
#[cfg(windows)]
use kattri_platform::WindowsDiskSpace;
use serde::{Deserialize, Serialize};

use crate::driver::{
    DriverConfig, DriverError, FailureReason, OriginalFileRequest, Outcome, RequestAuth, Transfer,
};
use crate::fs::{FsError, TempStore};
use crate::messages::Notice;
use crate::persist::{PersistError, PersistedDownload};
use crate::verify::Verification;

/// Name of the job list file inside the downloads folder.
pub const JOB_LIST_FILE: &str = "download-jobs.json";
/// The only job list format this build reads or writes.
pub const JOB_LIST_VERSION: u32 = 1;
/// A larger job list is treated as corrupt. One record is a few hundred bytes.
const MAX_LIST_BYTES: u64 = 1024 * 1024;
/// Longest accepted item or server ID. Jellyfin IDs are 32 hex digits, sometimes with dashes.
const MAX_ID_CHARS: usize = 64;
/// Temp files live in this folder under the downloads folder (the M2-13 layout).
const TEMP_SUBDIR: &str = "tmp";
/// How many numbered `.corrupt` names are tried before giving up.
const MAX_CORRUPT_NAMES: u32 = 100;

#[derive(Debug, thiserror::Error)]
pub enum ManagerError {
    #[error("download folder or job list could not be read or written: {0}")]
    Io(#[from] io::Error),
    #[error(transparent)]
    Fs(#[from] FsError),
    #[error(transparent)]
    Driver(#[from] DriverError),
    #[error(transparent)]
    Persist(#[from] PersistError),
    #[error("job list could not be encoded")]
    Encode,
    #[error("job list is too large to write")]
    ListTooLarge,
    #[error("server ID must be 1 to 64 ASCII letters, digits or '-'")]
    InvalidServerId,
    #[error("this item already has an active or completed download")]
    AlreadyTracked,
    #[error("no download exists for this item")]
    UnknownItem,
    #[error("this download is already running")]
    AlreadyRunning,
    #[error("this download is not running and cannot be paused")]
    NotPausable,
    #[error("this download cannot be resumed from its current state")]
    NotResumable,
    #[error("this download belongs to a different server")]
    ServerMismatch,
    #[error("the transfer thread could not be started")]
    ThreadStart,
}

/// What the UI shows for one job. Copied out of the manager, so it holds no handles.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JobView {
    pub item_id: String,
    /// The state the transfer last returned. While `running`, the live state is not visible.
    pub state: DownloadState,
    pub running: bool,
    pub durable_offset: u64,
    pub expected_total_length: Option<u64>,
    pub notice: Option<Notice>,
    pub verification: Option<Verification>,
    pub failure: Option<FailureReason>,
}

/// One job as the manager keeps it in memory.
struct Entry {
    server_id: String,
    job: DownloadJob,
    temp_name: Option<String>,
    notice: Option<Notice>,
    verification: Option<Verification>,
    failure: Option<FailureReason>,
    /// Set while a transfer thread owns the job. Requesting a pause sets this flag.
    pause: Option<Arc<AtomicBool>>,
}

struct Shared {
    list_path: PathBuf,
    jobs: BTreeMap<String, Entry>,
    /// The last error a transfer thread hit that the caller could not see at the time.
    background_error: Option<ManagerError>,
}

/// Owns the download jobs of one profile. Methods take `&self`, so the UI can call them
/// from its own thread while transfers run in the background.
pub struct DownloadManager {
    shared: Arc<Mutex<Shared>>,
    driver: DriverConfig,
    recovered: Option<PathBuf>,
    dir: PathBuf,
}

impl DownloadManager {
    /// Opens the downloads folder `dir`, creating it and its `tmp` folder if needed, and loads
    /// the job list. A job list that cannot be parsed or fails validation is renamed to
    /// `download-jobs.json.corrupt` (or a numbered name), and the manager starts empty. The
    /// renamed path is available from [`Self::recovered_list`].
    pub fn open(dir: impl Into<PathBuf>) -> Result<Self, ManagerError> {
        let dir = dir.into();
        fs::create_dir_all(&dir)?;
        let store = TempStore::new(dir.join(TEMP_SUBDIR));
        store.ensure_dir()?;
        let driver = with_volume_check(DriverConfig::new(store)?);
        let list_path = dir.join(JOB_LIST_FILE);
        let (mut jobs, recovered) = load_list(&list_path)?;
        // A crash between job-list writes leaves a newer per-job snapshot on disk. Apply it
        // so a restart resumes from the last durable offset instead of re-downloading.
        restore_snapshots(&dir, &mut jobs);
        Ok(Self {
            shared: Arc::new(Mutex::new(Shared {
                list_path,
                jobs,
                background_error: None,
            })),
            driver,
            recovered,
            dir,
        })
    }

    /// The renamed copy of a job list that could not be used, if `open` found one.
    pub fn recovered_list(&self) -> Option<&Path> {
        self.recovered.as_deref()
    }

    /// Starts a new download of `item_id` from `base` (the server root URL). Returns once the
    /// job is saved as `Queued` and its thread is started, without waiting for network I/O.
    ///
    /// An item with a running, paused or completed job is refused. A failed job is replaced.
    pub fn start(
        &self,
        server_id: &str,
        base: &str,
        item_id: &str,
        auth: &SessionAuthorization,
    ) -> Result<(), ManagerError> {
        if !is_valid_id(server_id) {
            return Err(ManagerError::InvalidServerId);
        }
        let OriginalFileRequest {
            source,
            auth: request_auth,
        } = OriginalFileRequest::new(base, item_id, auth)?;
        let transfer = Transfer::from_source(&source);
        let shared = lock(&self.shared);
        if shared
            .jobs
            .get(item_id)
            .is_some_and(|entry| entry.job.snapshot().state != DownloadState::Failed)
        {
            return Err(ManagerError::AlreadyTracked);
        }
        let entry = Entry {
            server_id: server_id.to_owned(),
            job: DownloadJob::new(),
            temp_name: None,
            notice: None,
            verification: None,
            failure: None,
            pause: Some(transfer.pause_handle()),
        };
        self.launch(shared, item_id, entry, transfer, request_auth)
    }

    /// Asks a running download to pause. The transfer flushes and stops at its next check,
    /// and the state becomes `Paused` at that point; until then [`JobView::running`] stays
    /// true. Pausing a job that is already paused and idle succeeds without effect.
    pub fn pause(&self, item_id: &str) -> Result<(), ManagerError> {
        let shared = lock(&self.shared);
        let entry = shared.jobs.get(item_id).ok_or(ManagerError::UnknownItem)?;
        if let Some(flag) = &entry.pause {
            flag.store(true, Ordering::SeqCst);
            return Ok(());
        }
        if entry.job.snapshot().state == DownloadState::Paused {
            return Ok(());
        }
        Err(ManagerError::NotPausable)
    }

    /// Resumes a paused, sign-in-blocked or restored-queued download from its durable offset.
    ///
    /// `base` and `auth` come from the caller because neither is saved. `server_id` must match
    /// the one the job was started with, so a download is never resumed against another server.
    pub fn resume(
        &self,
        server_id: &str,
        base: &str,
        item_id: &str,
        auth: &SessionAuthorization,
    ) -> Result<(), ManagerError> {
        let OriginalFileRequest {
            source,
            auth: request_auth,
        } = OriginalFileRequest::new(base, item_id, auth)?;
        let shared = lock(&self.shared);
        let entry = shared.jobs.get(item_id).ok_or(ManagerError::UnknownItem)?;
        if entry.pause.is_some() {
            return Err(ManagerError::AlreadyRunning);
        }
        if entry.server_id != server_id {
            return Err(ManagerError::ServerMismatch);
        }
        if !matches!(
            entry.job.snapshot().state,
            DownloadState::Queued | DownloadState::Paused | DownloadState::NeedsAuth
        ) {
            return Err(ManagerError::NotResumable);
        }
        let transfer = Transfer::restored(
            source.url().to_owned(),
            entry.job.clone(),
            entry.temp_name.clone(),
        );
        let resumed = Entry {
            server_id: entry.server_id.clone(),
            job: entry.job.clone(),
            temp_name: entry.temp_name.clone(),
            notice: None,
            verification: None,
            failure: None,
            pause: Some(transfer.pause_handle()),
        };
        self.launch(shared, item_id, resumed, transfer, request_auth)
    }

    /// Every job, sorted by item ID.
    pub fn list(&self) -> Vec<JobView> {
        let shared = lock(&self.shared);
        shared
            .jobs
            .iter()
            .map(|(item_id, entry)| {
                let snapshot = entry.job.snapshot();
                JobView {
                    item_id: item_id.clone(),
                    state: snapshot.state,
                    running: entry.pause.is_some(),
                    durable_offset: snapshot.durable_offset,
                    expected_total_length: snapshot.expected_total_length,
                    notice: entry.notice,
                    verification: entry.verification,
                    failure: entry.failure,
                }
            })
            .collect()
    }

    /// Returns and clears the error from a transfer thread that could not be reported when it
    /// happened, such as a failed job-list save after a transfer finished.
    pub fn take_background_error(&self) -> Option<ManagerError> {
        lock(&self.shared).background_error.take()
    }

    /// Saves the new entry and starts its thread. The caller holds the lock, which is released
    /// when this returns. A thread cannot reach its completion update before then.
    fn launch(
        &self,
        mut shared: MutexGuard<'_, Shared>,
        item_id: &str,
        entry: Entry,
        transfer: Transfer,
        auth: RequestAuth,
    ) -> Result<(), ManagerError> {
        let previous = shared.jobs.insert(item_id.to_owned(), entry);
        if let Err(error) = save_list(&shared) {
            restore(&mut shared, item_id, previous);
            return Err(error);
        }
        let owner = Arc::clone(&self.shared);
        let mut cfg = self.driver.clone();
        // The driver writes the durable offset here after every synced chunk, which is what
        // survives a crash between job-list saves.
        cfg.snapshot_path = Some(snapshot_path(&self.dir, item_id));
        let thread_item = item_id.to_owned();
        let spawned = thread::Builder::new()
            .name("kattri-download".to_owned())
            .spawn(move || run_transfer(owner, thread_item, transfer, cfg, auth));
        if spawned.is_err() {
            restore(&mut shared, item_id, previous);
            save_list(&shared)?;
            return Err(ManagerError::ThreadStart);
        }
        Ok(())
    }
}

/// Runs one transfer to a stop and writes what it returned back into the job map.
fn run_transfer(
    owner: Arc<Mutex<Shared>>,
    item_id: String,
    mut transfer: Transfer,
    cfg: DriverConfig,
    auth: RequestAuth,
) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(_) => {
            // Nothing ran, so the job keeps its last state and can be resumed again.
            let mut shared = lock(&owner);
            if let Some(entry) = shared.jobs.get_mut(&item_id) {
                entry.pause = None;
            }
            shared.background_error = Some(ManagerError::ThreadStart);
            return;
        }
    };
    let outcome = runtime.block_on(transfer.run(&cfg, Some(&auth)));

    let mut shared = lock(&owner);
    if let Some(entry) = shared.jobs.get_mut(&item_id) {
        entry.job = transfer.job().clone();
        entry.temp_name = transfer.temp_name().map(str::to_owned);
        entry.notice = transfer.notice();
        entry.verification = transfer.verification();
        entry.failure = match outcome {
            Outcome::Failed(reason) => Some(reason),
            Outcome::Committed | Outcome::Paused | Outcome::NeedsAuth => None,
        };
        entry.pause = None;
    }
    if let Err(error) = save_list(&shared) {
        shared.background_error = Some(error);
    }
}

/// Refuses a fresh temp file when the volume cannot hold the file (M3-09). Only the Windows
/// volume check exists, so other platforms run without it until one is added.
#[cfg(windows)]
fn with_volume_check(mut driver: DriverConfig) -> DriverConfig {
    driver.disk_space = Some(Arc::new(WindowsDiskSpace));
    driver
}

#[cfg(not(windows))]
fn with_volume_check(driver: DriverConfig) -> DriverConfig {
    driver
}

/// Per-job crash snapshot beside the job list. The driver writes it after every synced chunk.
fn snapshot_path(dir: &Path, item_id: &str) -> PathBuf {
    dir.join(format!("{item_id}.download.json"))
}

/// Overlay a newer per-job snapshot onto the loaded job list so an interrupted transfer resumes
/// from its last durable offset instead of from zero.
fn restore_snapshots(dir: &Path, jobs: &mut BTreeMap<String, Entry>) {
    for (item_id, entry) in jobs.iter_mut() {
        let Ok(record) = crate::persist::load(&snapshot_path(dir, item_id)) else {
            continue;
        };
        let Ok((job, temp_name)) = record.into_job() else {
            continue;
        };
        let restored_state = job.snapshot().state;
        let restored_offset = job.snapshot().durable_offset;
        if matches!(
            restored_state,
            DownloadState::Committed | DownloadState::Failed | DownloadState::Cancelled
        ) {
            continue;
        }
        if restored_offset < entry.job.snapshot().durable_offset {
            continue;
        }
        entry.job = job;
        if temp_name.is_some() {
            entry.temp_name = temp_name;
        }
    }
}

fn lock(shared: &Mutex<Shared>) -> MutexGuard<'_, Shared> {
    // Every update leaves the map consistent, so a lock poisoned by a panic is still usable.
    shared.lock().unwrap_or_else(PoisonError::into_inner)
}

fn restore(shared: &mut Shared, item_id: &str, previous: Option<Entry>) {
    match previous {
        Some(entry) => {
            shared.jobs.insert(item_id.to_owned(), entry);
        }
        None => {
            shared.jobs.remove(item_id);
        }
    }
}

fn is_valid_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_ID_CHARS
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct JobListFile {
    schema_version: u32,
    jobs: Vec<JobListEntry>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct JobListEntry {
    item_id: String,
    server_id: String,
    record: PersistedDownload,
}

enum Loaded {
    Missing,
    Valid(BTreeMap<String, Entry>),
    Corrupt,
}

fn load_list(path: &Path) -> Result<(BTreeMap<String, Entry>, Option<PathBuf>), ManagerError> {
    match read_list(path)? {
        Loaded::Missing => Ok((BTreeMap::new(), None)),
        Loaded::Valid(jobs) => Ok((jobs, None)),
        Loaded::Corrupt => {
            let moved = move_aside(path)?;
            Ok((BTreeMap::new(), Some(moved)))
        }
    }
}

fn read_list(path: &Path) -> Result<Loaded, ManagerError> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Loaded::Missing),
        Err(error) => return Err(ManagerError::Io(error)),
    };
    let mut bytes = Vec::new();
    file.take(MAX_LIST_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_LIST_BYTES {
        return Ok(Loaded::Corrupt);
    }
    Ok(match parse_list(&bytes) {
        Some(jobs) => Loaded::Valid(jobs),
        None => Loaded::Corrupt,
    })
}

/// Parses and validates a job list. Any failure, including an unknown format version, a bad
/// ID, a record that fails validation, or a duplicate item, makes the whole file corrupt.
fn parse_list(bytes: &[u8]) -> Option<BTreeMap<String, Entry>> {
    let file: JobListFile = serde_json::from_slice(bytes).ok()?;
    if file.schema_version != JOB_LIST_VERSION {
        return None;
    }
    let mut jobs = BTreeMap::new();
    for entry in file.jobs {
        if !is_valid_id(&entry.item_id) || !is_valid_id(&entry.server_id) {
            return None;
        }
        let (job, temp_name) = entry.record.into_job().ok()?;
        let restored = Entry {
            server_id: entry.server_id,
            job,
            temp_name,
            notice: None,
            verification: None,
            failure: None,
            pause: None,
        };
        if jobs.insert(entry.item_id, restored).is_some() {
            return None;
        }
    }
    Some(jobs)
}

/// Writes the whole job list. Each record is validated before encoding, so a value that
/// carries a URL or credential text is refused rather than written.
fn save_list(shared: &Shared) -> Result<(), ManagerError> {
    let file = JobListFile {
        schema_version: JOB_LIST_VERSION,
        jobs: shared
            .jobs
            .iter()
            .map(|(item_id, entry)| JobListEntry {
                item_id: item_id.clone(),
                server_id: entry.server_id.clone(),
                record: PersistedDownload::capture(
                    entry.job.snapshot(),
                    entry.temp_name.as_deref(),
                ),
            })
            .collect(),
    };
    for entry in &file.jobs {
        entry.record.validate()?;
    }
    let bytes = serde_json::to_vec(&file).map_err(|_| ManagerError::Encode)?;
    if bytes.len() as u64 > MAX_LIST_BYTES {
        return Err(ManagerError::ListTooLarge);
    }
    write_atomic(&shared.list_path, &bytes)?;
    Ok(())
}

/// Writes `bytes` to a sibling temp file, flushes and syncs it, and renames it over `path`.
/// A reader sees either the old list or the new one.
fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut tmp_name = path.as_os_str().to_owned();
    tmp_name.push(".tmp");
    let tmp = PathBuf::from(tmp_name);
    {
        let mut file = File::create(&tmp)?;
        file.write_all(bytes)?;
        file.flush()?;
        file.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    sync_parent(path)
}

/// Unix can sync the directory entry after a rename. std has no equivalent on Windows, so
/// there the rename is atomic but not separately synced.
#[cfg(unix)]
fn sync_parent(path: &Path) -> io::Result<()> {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => File::open(parent)?.sync_all(),
        _ => Ok(()),
    }
}

#[cfg(not(unix))]
fn sync_parent(_path: &Path) -> io::Result<()> {
    Ok(())
}

/// Renames an unusable job list to `<name>.corrupt`, or `<name>.corrupt.<n>` when that name is
/// taken, so an earlier copy is never overwritten.
fn move_aside(path: &Path) -> Result<PathBuf, ManagerError> {
    let base = path.as_os_str().to_owned();
    for attempt in 0..MAX_CORRUPT_NAMES {
        let mut name = base.clone();
        name.push(".corrupt");
        if attempt > 0 {
            name.push(format!(".{attempt}"));
        }
        let destination = PathBuf::from(name);
        if !destination.exists() {
            fs::rename(path, &destination)?;
            return Ok(destination);
        }
    }
    Err(ManagerError::Io(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "no free name for the corrupt job list",
    )))
}
