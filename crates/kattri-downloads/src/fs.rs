//! Temporary file lifecycle for downloads.
//!
//! Temp file names are random hex and never contain the item title or ID. The final
//! name (item ID plus container extension) is applied only on commit.
//!
//! Rules implemented here:
//! - A restart discards the old temp file (rename to a `.discarded` name, then delete)
//!   and creates a new one with `create_new`; old bytes never survive a restart.
//! - A resume opens the existing temp file and checks its length against the durable
//!   offset. A shorter file is refused with [`FsError::LengthMismatch`] so the caller
//!   restarts from zero. A longer file is truncated to the durable offset.
//! - Paths longer than [`MAX_PATH_CHARS`] are refused.
//! - Opens that hit a lock (permission or sharing violation) are retried a bounded
//!   number of times.
//!
//! Resume opens do not follow a symlink or Windows reparse point at the temp-file
//! name, and validate the opened handle as a regular file before truncating or seeking.
//! This protects the target of a planted leaf link. It is not a sandbox against a local
//! attacker who can replace parent directories or race other pathname operations; std
//! does not provide the handle-relative directory operations needed for that boundary.

use kattri_client_core::downloads::{DownloadJob, DownloadTransitionError};
use std::fs::{self, File, OpenOptions};
use std::hash::{BuildHasher, Hasher};
use std::io::{self, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Upper bound on any path the downloader builds (Windows MAX_PATH guard).
pub const MAX_PATH_CHARS: usize = 240;
/// Number of attempts for an open that fails because another process holds the file.
pub const OPEN_ATTEMPTS: u32 = 3;
/// Delay between lock-retry attempts.
pub const OPEN_RETRY_DELAY: Duration = Duration::from_millis(200);
/// Attempts to find an unused random temp name before giving up.
const NAME_ATTEMPTS: u32 = 8;
/// Suffix used when a discarded temp file is renamed before deletion.
const DISCARD_SUFFIX: &str = ".discarded";
const TEMP_NAME_HEX_LEN: usize = 16;
const TEMP_NAME_SUFFIX: &str = ".part";

#[derive(Debug, thiserror::Error)]
pub enum FsError {
    #[error("temporary file has {actual} bytes but the durable offset is {expected}")]
    LengthMismatch { expected: u64, actual: u64 },
    #[error("temporary file is missing")]
    Missing,
    #[error("temporary file name has an unexpected shape")]
    InvalidName,
    #[error("temporary path does not refer to a regular file")]
    UnsafeFile,
    #[error("temporary path would be longer than {MAX_PATH_CHARS} characters")]
    PathTooLong,
    #[error("could not find an unused temporary file name")]
    NameCollision,
    #[error("filesystem error: {0}")]
    Io(#[from] io::Error),
}

/// Destination for downloaded bytes that must be durable before the offset advances.
pub trait DurableSink {
    fn write_all_bytes(&mut self, bytes: &[u8]) -> io::Result<()>;
    /// Flush buffers and fsync. Returning `Ok` is the only proof the bytes are durable.
    fn sync_durable(&mut self) -> io::Result<()>;
}

/// A temp file whose length is the bytes written so far.
#[derive(Debug)]
pub struct TempFile {
    name: String,
    path: PathBuf,
    file: File,
    len: u64,
}

impl TempFile {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Bytes present in the file (not necessarily durable until `sync_durable`).
    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl DurableSink for TempFile {
    fn write_all_bytes(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.file.write_all(bytes)?;
        self.len = self
            .len
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| io::Error::other("temp file length overflow"))?;
        Ok(())
    }

    fn sync_durable(&mut self) -> io::Result<()> {
        self.file.flush()?;
        self.file.sync_data()
    }
}

/// Owns the temp directory (`<profile>/downloads/tmp/`).
#[derive(Debug, Clone)]
pub struct TempStore {
    dir: PathBuf,
}

impl TempStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn ensure_dir(&self) -> Result<(), FsError> {
        fs::create_dir_all(&self.dir)?;
        Ok(())
    }

    /// Create a new empty temp file with a random name.
    pub fn create_fresh(&self) -> Result<TempFile, FsError> {
        self.create_fresh_with(random_name)
    }

    /// Replace the current temp file (if any) with a new empty one. Used on
    /// `RestartFromZero`: the old bytes are discarded before anything is written.
    pub fn restart(&self, previous: Option<TempFile>) -> Result<TempFile, FsError> {
        if let Some(old) = previous {
            discard(old)?;
        }
        self.create_fresh()
    }

    /// Discard a temp file by name when no handle is held. A missing file is not an error.
    pub fn discard_named(&self, name: &str) -> Result<(), FsError> {
        let path = self.path_for(name)?;
        discard_path(&path, name)
    }

    /// Reopen the temp file `name` for a resume at `durable_offset`.
    ///
    /// - Shorter than the offset: [`FsError::LengthMismatch`] (caller restarts).
    /// - Longer than the offset: truncated to the offset (bytes after it were never
    ///   recorded as durable, e.g. after a crash).
    pub fn open_resume(&self, name: &str, durable_offset: u64) -> Result<TempFile, FsError> {
        let path = self.path_for(name)?;
        let mut file = open_with_retry(|| open_regular_file_nofollow(&path)).map_err(|error| {
            match error.kind() {
                io::ErrorKind::NotFound => FsError::Missing,
                io::ErrorKind::InvalidInput => FsError::UnsafeFile,
                _ => FsError::Io(error),
            }
        })?;
        let actual = file.metadata()?.len();
        if actual < durable_offset {
            return Err(FsError::LengthMismatch {
                expected: durable_offset,
                actual,
            });
        }
        if actual > durable_offset {
            file.set_len(durable_offset)?;
        }
        file.seek(SeekFrom::Start(durable_offset))?;
        Ok(TempFile {
            name: name.to_owned(),
            path,
            file,
            len: durable_offset,
        })
    }

    /// Create a fresh temp file, trying each generated name until one is unused.
    fn create_fresh_with(
        &self,
        mut next_name: impl FnMut() -> String,
    ) -> Result<TempFile, FsError> {
        self.ensure_dir()?;
        for _ in 0..NAME_ATTEMPTS {
            let name = next_name();
            let path = self.path_for(&name)?;
            let opened = open_with_retry(|| {
                OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create_new(true)
                    .open(&path)
            });
            match opened {
                Ok(file) => {
                    return Ok(TempFile {
                        name,
                        path,
                        file,
                        len: 0,
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(FsError::Io(error)),
            }
        }
        Err(FsError::NameCollision)
    }

    fn path_for(&self, name: &str) -> Result<PathBuf, FsError> {
        if !is_valid_temp_name(name) {
            return Err(FsError::InvalidName);
        }
        let path = self.dir.join(name);
        if path.as_os_str().len() > MAX_PATH_CHARS {
            return Err(FsError::PathTooLong);
        }
        Ok(path)
    }
}

fn is_valid_temp_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    bytes.len() == TEMP_NAME_HEX_LEN + TEMP_NAME_SUFFIX.len()
        && name.ends_with(TEMP_NAME_SUFFIX)
        && bytes[..TEMP_NAME_HEX_LEN]
            .iter()
            .copied()
            .all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(unix)]
fn open_regular_file_nofollow(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;

    // O_NOFOLLOW is a platform ABI flag and is not exposed by std. These values
    // cover the supported Unix targets (Linux x64/ARM64 and macOS); keeping it here
    // avoids adding a direct dependency solely for one open flag.
    #[cfg(target_os = "linux")]
    const O_NOFOLLOW: i32 = 0x2_0000;
    #[cfg(target_os = "macos")]
    const O_NOFOLLOW: i32 = 0x0100;

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    compile_error!("no-follow temp-file opens are defined only for Linux and macOS");

    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(O_NOFOLLOW)
        .open(path)?;
    validate_regular_file(&file)?;
    Ok(file)
}

#[cfg(windows)]
fn open_regular_file_nofollow(path: &Path) -> io::Result<File> {
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};

    // FILE_FLAG_OPEN_REPARSE_POINT opens the reparse point itself, not its target.
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;

    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    let metadata = file.metadata()?;
    if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "temporary path is a reparse point",
        ));
    }
    validate_regular_metadata(&metadata)?;
    Ok(file)
}

#[cfg(not(any(unix, windows)))]
fn open_regular_file_nofollow(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new().read(true).write(true).open(path)?;
    validate_regular_file(&file)?;
    Ok(file)
}

#[cfg(not(windows))]
fn validate_regular_file(file: &File) -> io::Result<()> {
    validate_regular_metadata(&file.metadata()?)
}

fn validate_regular_metadata(metadata: &fs::Metadata) -> io::Result<()> {
    if metadata.file_type().is_file() {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "temporary path is not a regular file",
        ))
    }
}

/// Failure while persisting a chunk. The job's durable offset is unchanged in both cases.
#[derive(Debug, thiserror::Error)]
pub enum ChunkError {
    #[error("write or sync failed; bytes are not durable: {0}")]
    Io(#[from] io::Error),
    #[error("job rejected the durable advance: {0:?}")]
    Job(DownloadTransitionError),
}

/// Write `bytes`, fsync them, and only then record them as durable on the job.
///
/// The offset moves only after `sync_durable` succeeds, by exactly `bytes.len()`.
/// If the write or the sync fails, the job is untouched. Bytes that were written
/// but not synced are beyond the durable offset and are truncated on resume.
pub fn persist_chunk<S: DurableSink>(
    sink: &mut S,
    job: &mut DownloadJob,
    bytes: &[u8],
) -> Result<(), ChunkError> {
    sink.write_all_bytes(bytes)?;
    sink.sync_durable()?;
    job.record_durable_bytes(bytes.len() as u64)
        .map_err(ChunkError::Job)
}

/// Moves a committed temp file to its final path, creating the parent folder.
/// The bytes were fsynced while downloading, so only the rename happens here.
pub fn commit_final(temp: &Path, final_path: &Path) -> io::Result<()> {
    if let Some(parent) = final_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::rename(temp, final_path)
}

/// Free space required beyond the file size, as a percentage (spec M3-09 requirement 5).
pub const DISK_MARGIN_PERCENT: u64 = 5;

/// A pre-download check that found too little free space.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SpaceShortfall {
    pub required: u64,
    pub available: u64,
}

/// Bytes that must be free before a download of `expected_total` bytes starts: the
/// size plus [`DISK_MARGIN_PERCENT`], rounded up. `None` if the sum overflows.
pub fn required_free_bytes(expected_total: u64) -> Option<u64> {
    let margin = expected_total
        .checked_mul(DISK_MARGIN_PERCENT)?
        .div_ceil(100);
    expected_total.checked_add(margin)
}

/// Pure pre-check. The free space is a parameter, so a near-full volume can be tested
/// without one. An overflowing requirement cannot be met and is refused.
pub fn check_free_space(available: u64, expected_total: u64) -> Result<(), SpaceShortfall> {
    match required_free_bytes(expected_total) {
        Some(required) if available >= required => Ok(()),
        Some(required) => Err(SpaceShortfall {
            required,
            available,
        }),
        None => Err(SpaceShortfall {
            required: u64::MAX,
            available,
        }),
    }
}

/// Rename the file to a discarded name, then delete it. A file already gone is not an error.
fn discard(temp: TempFile) -> Result<(), FsError> {
    let TempFile {
        name, path, file, ..
    } = temp;
    drop(file);
    discard_path(&path, &name)
}

fn discard_path(path: &Path, name: &str) -> Result<(), FsError> {
    let discarded = path.with_file_name(format!("{name}{DISCARD_SUFFIX}"));
    match fs::rename(path, &discarded) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(FsError::Io(error)),
    }
    match fs::remove_file(&discarded) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(FsError::Io(error)),
    }
}

/// Retry opens that fail on a lock (sharing violation on Windows or permission denied),
/// up to [`OPEN_ATTEMPTS`] times. Other errors, including `AlreadyExists`, return at once.
fn open_with_retry<T>(mut open: impl FnMut() -> io::Result<T>) -> io::Result<T> {
    let mut attempt = 1;
    loop {
        match open() {
            Ok(value) => return Ok(value),
            Err(error) if is_lock_error(&error) && attempt < OPEN_ATTEMPTS => {
                attempt += 1;
                thread::sleep(OPEN_RETRY_DELAY);
            }
            Err(error) => return Err(error),
        }
    }
}

fn is_lock_error(error: &io::Error) -> bool {
    // 32 = ERROR_SHARING_VIOLATION on Windows.
    error.kind() == io::ErrorKind::PermissionDenied || error.raw_os_error() == Some(32)
}

static NAME_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A 16 hex character name. It is not cryptographic; it only needs to avoid collisions
/// and reveal nothing about the item. Collisions are handled by `create_new`.
fn random_name() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let counter = NAME_COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u64(nanos);
    hasher.write_u64(counter);
    format!("{:016x}.part", hasher.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, TempStore) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = TempStore::new(dir.path().join("tmp"));
        (dir, store)
    }

    #[test]
    fn restart_creates_new_file_and_old_bytes_are_gone() {
        let (_dir, store) = store();
        let mut first = store.create_fresh().expect("create");
        first.write_all_bytes(b"old bytes").expect("write");
        let old_path = first.path().to_path_buf();

        let second = store.restart(Some(first)).expect("restart");

        assert!(!old_path.exists(), "old temp file must be deleted");
        assert_eq!(second.len(), 0);
        assert_eq!(fs::read(second.path()).expect("read"), Vec::<u8>::new());
    }

    #[test]
    fn resume_with_shorter_file_is_refused() {
        let (_dir, store) = store();
        let mut temp = store.create_fresh().expect("create");
        temp.write_all_bytes(b"abcd").expect("write");
        let name = temp.name().to_owned();
        drop(temp);

        let result = store.open_resume(&name, 10);
        assert!(matches!(
            result,
            Err(FsError::LengthMismatch {
                expected: 10,
                actual: 4
            })
        ));
    }

    #[test]
    fn resume_with_missing_file_reports_missing() {
        let (_dir, store) = store();
        store.ensure_dir().expect("dir");
        assert!(matches!(
            store.open_resume("0000000000000000.part", 0),
            Err(FsError::Missing)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn resume_refuses_symlink_without_changing_its_target() {
        use std::os::unix::fs::symlink;

        let (dir, store) = store();
        store.ensure_dir().expect("dir");
        let target = dir.path().join("outside.bin");
        fs::write(&target, b"preserve this target").expect("seed target");
        let link = store.dir().join("aaaaaaaaaaaaaaaa.part");
        symlink(&target, &link).expect("create symlink");

        let result = store.open_resume("aaaaaaaaaaaaaaaa.part", 0);

        assert!(result.is_err(), "resume must refuse the symlink");
        assert_eq!(
            fs::read(&target).expect("read target"),
            b"preserve this target"
        );
        store
            .discard_named("aaaaaaaaaaaaaaaa.part")
            .expect("discard symlink entry");
        assert_eq!(
            fs::read(&target).expect("read target after discard"),
            b"preserve this target"
        );
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "Opt in with `cargo test -p kattri-downloads resume_refuses_symlink_without_changing_its_target -- --ignored`; requires Developer Mode or symlink privilege"]
    fn resume_refuses_symlink_without_changing_its_target() {
        use std::os::windows::fs::symlink_file;

        let (dir, store) = store();
        store.ensure_dir().expect("dir");
        let target = dir.path().join("outside.bin");
        fs::write(&target, b"preserve this target").expect("seed target");
        let link = store.dir().join("aaaaaaaaaaaaaaaa.part");
        symlink_file(&target, &link).expect("create symlink (requires symlink privilege)");

        let result = store.open_resume("aaaaaaaaaaaaaaaa.part", 0);

        assert!(result.is_err(), "resume must refuse the symlink");
        assert_eq!(
            fs::read(&target).expect("read target"),
            b"preserve this target"
        );
    }

    #[cfg(windows)]
    #[test]
    fn resume_refuses_directory_without_changing_its_contents() {
        let (_dir, store) = store();
        store.ensure_dir().expect("dir");
        let directory = store.dir().join("aaaaaaaaaaaaaaaa.part");
        fs::create_dir(&directory).expect("seed directory");
        let sentinel = directory.join("sentinel.bin");
        fs::write(&sentinel, b"preserve directory contents").expect("seed sentinel");

        let result = store.open_resume("aaaaaaaaaaaaaaaa.part", 0);

        assert!(result.is_err(), "resume must refuse a directory");
        assert_eq!(
            fs::read(&sentinel).expect("read sentinel"),
            b"preserve directory contents"
        );
    }

    #[test]
    fn caller_supplied_names_cannot_escape_temp_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = TempStore::new(dir.path().join("tmp"));
        let outside = dir.path().join("outside.part");
        fs::write(&outside, b"keep me").expect("seed outside file");

        for name in [
            "../outside.part",
            "..\\outside.part",
            "C:\\outside.part",
            "C:/outside.part",
            "0000000000000000.part:stream",
        ] {
            assert!(
                matches!(store.open_resume(name, 0), Err(FsError::InvalidName)),
                "resume must refuse {name:?}"
            );
            assert!(
                matches!(store.discard_named(name), Err(FsError::InvalidName)),
                "discard must refuse {name:?}"
            );
        }
        assert_eq!(fs::read(&outside).expect("read outside file"), b"keep me");
    }

    #[test]
    fn create_new_collision_is_skipped_for_next_name() {
        let (_dir, store) = store();
        store.ensure_dir().expect("dir");
        fs::write(store.dir().join("aaaaaaaaaaaaaaaa.part"), b"someone else").expect("seed");
        let mut names = vec![
            "aaaaaaaaaaaaaaaa.part".to_owned(),
            "bbbbbbbbbbbbbbbb.part".to_owned(),
        ]
        .into_iter();

        let temp = store
            .create_fresh_with(|| names.next().expect("name"))
            .expect("create");

        assert_eq!(temp.name(), "bbbbbbbbbbbbbbbb.part");
        assert_eq!(
            fs::read(store.dir().join("aaaaaaaaaaaaaaaa.part")).expect("read"),
            b"someone else"
        );
    }

    #[test]
    fn repeated_collisions_fail_with_name_collision() {
        let (_dir, store) = store();
        store.ensure_dir().expect("dir");
        fs::write(store.dir().join("aaaaaaaaaaaaaaaa.part"), b"x").expect("seed");
        let result = store.create_fresh_with(|| "aaaaaaaaaaaaaaaa.part".to_owned());
        assert!(matches!(result, Err(FsError::NameCollision)));
    }

    /// Sink that records writes in memory and can inject a failure at write or sync.
    #[derive(Default)]
    struct FaultSink {
        written: Vec<u8>,
        fail_write: bool,
        fail_sync: bool,
    }

    impl DurableSink for FaultSink {
        fn write_all_bytes(&mut self, bytes: &[u8]) -> io::Result<()> {
            if self.fail_write {
                return Err(io::Error::new(io::ErrorKind::StorageFull, "injected write"));
            }
            self.written.extend_from_slice(bytes);
            Ok(())
        }

        fn sync_durable(&mut self) -> io::Result<()> {
            if self.fail_sync {
                return Err(io::Error::other("injected sync"));
            }
            Ok(())
        }
    }

    fn job_downloading(total: u64) -> DownloadJob {
        let mut job = DownloadJob::new();
        job.begin_negotiation().expect("negotiate");
        job.handle_response(kattri_client_core::downloads::RangeResponse::Ok {
            content_length: Some(total),
            validators: Default::default(),
        })
        .expect("ok response");
        job
    }

    #[test]
    fn write_failure_leaves_offset_unchanged() {
        let mut job = job_downloading(10);
        let mut sink = FaultSink {
            fail_write: true,
            ..Default::default()
        };
        assert!(persist_chunk(&mut sink, &mut job, b"0123").is_err());
        assert_eq!(job.snapshot().durable_offset, 0);
    }

    #[test]
    fn sync_failure_leaves_offset_unchanged() {
        let mut job = job_downloading(10);
        let mut sink = FaultSink {
            fail_sync: true,
            ..Default::default()
        };
        assert!(persist_chunk(&mut sink, &mut job, b"0123").is_err());
        assert_eq!(job.snapshot().durable_offset, 0);
    }

    #[test]
    fn successful_sync_advances_offset_by_exact_chunk_length() {
        let mut job = job_downloading(10);
        let mut sink = FaultSink::default();
        persist_chunk(&mut sink, &mut job, b"0123").expect("persist");
        assert_eq!(job.snapshot().durable_offset, 4);
        persist_chunk(&mut sink, &mut job, b"456").expect("persist");
        assert_eq!(job.snapshot().durable_offset, 7);
        assert_eq!(sink.written, b"0123456");
    }

    #[test]
    fn resume_truncates_bytes_beyond_durable_offset() {
        let (_dir, store) = store();
        let mut temp = store.create_fresh().expect("create");
        temp.write_all_bytes(b"0123456789").expect("write");
        temp.sync_durable().expect("sync");
        let name = temp.name().to_owned();
        drop(temp);

        // Simulated crash: only the first 6 bytes were recorded as durable.
        let resumed = store.open_resume(&name, 6).expect("resume");
        assert_eq!(resumed.len(), 6);
        assert_eq!(fs::read(resumed.path()).expect("read"), b"012345");
    }

    #[test]
    fn temp_names_contain_no_title_or_item_id() {
        let (_dir, store) = store();
        let temp = store.create_fresh().expect("create");
        let name = temp.name();
        assert_eq!(name.len(), 16 + ".part".len());
        assert!(name.ends_with(".part"));
        assert!(name[..16].chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn overlong_temp_path_is_refused() {
        let (_dir, store) = store();
        let long_dir = store.dir().join("x".repeat(MAX_PATH_CHARS));
        let store = TempStore::new(long_dir);
        assert!(matches!(
            store.path_for("0000000000000000.part"),
            Err(FsError::PathTooLong)
        ));
    }
}

#[cfg(test)]
mod disk_space_tests {
    use super::*;

    #[test]
    fn margin_is_five_percent_rounded_up() {
        assert_eq!(required_free_bytes(1000), Some(1050));
        assert_eq!(required_free_bytes(1), Some(2));
        assert_eq!(required_free_bytes(0), Some(0));
        assert_eq!(required_free_bytes(u64::MAX), None);
    }

    #[test]
    fn near_full_volume_at_exact_file_size_is_refused() {
        assert_eq!(
            check_free_space(1000, 1000),
            Err(SpaceShortfall {
                required: 1050,
                available: 1000
            })
        );
    }

    #[test]
    fn one_byte_under_the_margin_is_refused_and_the_margin_is_accepted() {
        assert!(check_free_space(1049, 1000).is_err());
        assert!(check_free_space(1050, 1000).is_ok());
    }

    #[test]
    fn full_volume_refuses_any_nonempty_download() {
        assert!(check_free_space(0, 1).is_err());
        assert!(check_free_space(1, 1).is_err());
        assert!(check_free_space(2, 1).is_ok());
    }

    #[test]
    fn empty_download_fits_even_on_a_full_volume() {
        assert!(check_free_space(0, 0).is_ok());
    }

    #[test]
    fn overflowing_requirement_is_refused() {
        assert_eq!(
            check_free_space(u64::MAX, u64::MAX),
            Err(SpaceShortfall {
                required: u64::MAX,
                available: u64::MAX
            })
        );
    }
}

#[cfg(test)]
mod lock_retry_tests {
    use super::*;

    fn sharing_violation() -> io::Error {
        io::Error::from_raw_os_error(32)
    }

    #[test]
    fn lock_error_is_retried_until_the_open_succeeds() {
        let mut calls = 0u32;
        let result = open_with_retry(|| {
            calls += 1;
            if calls < OPEN_ATTEMPTS {
                Err(sharing_violation())
            } else {
                Ok(calls)
            }
        });
        assert_eq!(result.expect("open after retries"), OPEN_ATTEMPTS);
        assert_eq!(calls, OPEN_ATTEMPTS);
    }

    #[test]
    fn lock_error_gives_up_after_bounded_attempts() {
        let mut calls = 0u32;
        let result: io::Result<()> = open_with_retry(|| {
            calls += 1;
            Err(sharing_violation())
        });
        assert!(result.is_err());
        assert_eq!(calls, OPEN_ATTEMPTS, "retries are bounded");
    }

    #[test]
    fn permission_denied_counts_as_a_lock_and_is_retried() {
        let mut calls = 0u32;
        let result = open_with_retry(|| {
            calls += 1;
            if calls == 1 {
                Err(io::Error::from(io::ErrorKind::PermissionDenied))
            } else {
                Ok(())
            }
        });
        assert!(result.is_ok());
        assert_eq!(calls, 2);
    }

    #[test]
    fn other_errors_are_not_retried() {
        let mut calls = 0u32;
        let result: io::Result<()> = open_with_retry(|| {
            calls += 1;
            Err(io::Error::from(io::ErrorKind::NotFound))
        });
        assert!(result.is_err());
        assert_eq!(calls, 1);
    }
}
