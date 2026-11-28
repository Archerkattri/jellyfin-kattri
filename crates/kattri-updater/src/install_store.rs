//! On-disk [`InstallStore`] for Windows, macOS and Linux (M4-16), using `std::fs` plus a
//! Windows atomic replace-existing call for installed-slot switches.
//!
//! Layout inside the store directory:
//! - `installed.slot`: the build that runs now.
//! - `previous.slot`: the build kept for rollback until the update is marked healthy.
//! - `staging/staged.slot`: the verified bytes waiting for the switch. The staging folder is
//!   created by [`FsInstallStore::open`].
//! - `journal.json`: the update journal (see [`Journal`]), version 1.
//!
//! A slot file is one header line, `kattri-slot v1 <version>`, followed by the exact build
//! bytes. The version and the bytes therefore change together in one file.
//!
//! Every write goes to a uniquely created temp sibling in the same directory and is synced before
//! replacement. Readers see either the old complete file or the new one. The directory is synced
//! after replacement on Unix; Windows requests write-through for the atomic replacement.
//!
//! Commit first keeps the old installed build in `previous.slot`, then atomically replaces
//! `installed.slot` with the staged file. Both folders are inside the store directory, so they
//! share one volume. A crash before replacement leaves the installed build unchanged; a failure
//! after replacement retains the pending journal so the next launch can reconcile it.
//!
//! Rollback atomically writes the kept previous build over `installed.slot`. The previous
//! slot remains intact until the rollback journal is committed, so a journal write failure can
//! safely retry the same rollback.
//!
//! Error messages name the operation and the OS error, never file contents.

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::apply::{InstallStore, Journal, Pending, Slot, StoreError};

const INSTALLED_FILE: &str = "installed.slot";
const PREVIOUS_FILE: &str = "previous.slot";
const STAGING_DIR: &str = "staging";
const STAGED_FILE: &str = "staged.slot";
const JOURNAL_FILE: &str = "journal.json";
const SLOT_HEADER: &str = "kattri-slot v1 ";
const JOURNAL_VERSION: u32 = 1;
const MAX_JOURNAL_BYTES: u64 = 64 * 1024;
const MAX_SLOT_HEADER_BYTES: usize = 256;

/// An [`InstallStore`] that keeps its builds and journal in one directory on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsInstallStore {
    dir: PathBuf,
}

impl FsInstallStore {
    /// Open the store in `dir`, creating the directory and its staging folder if needed.
    pub fn open(dir: impl Into<PathBuf>) -> Result<Self, StoreError> {
        let dir = dir.into();
        fs::create_dir_all(dir.join(STAGING_DIR))
            .map_err(|e| io_error("create the install and staging folders", &e))?;
        Ok(Self { dir })
    }

    /// The directory that holds the store files.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Copy a raw payload into the staging slot without allocating artifact-sized buffers.
    /// The caller must verify the staged slot against the signed manifest before committing.
    pub(crate) fn stage_payload(
        &self,
        version: &str,
        payload: impl Read,
    ) -> Result<(), StoreError> {
        let header = encode_slot_header(&Slot {
            version: version.to_owned(),
            bytes: Vec::new(),
        })?;
        write_slot_payload(&self.staged_file(), &header, payload)
    }

    /// Size of the installed payload without allocating an in-memory slot.
    pub fn installed_payload_len(&self) -> Result<Option<u64>, StoreError> {
        let Some((_, mut reader)) = open_slot(&self.file(INSTALLED_FILE))? else {
            return Ok(None);
        };
        use std::io::Seek as _;
        let offset = reader
            .stream_position()
            .map_err(|e| io_error("inspect the stored build header", &e))?;
        let len = reader
            .get_ref()
            .metadata()
            .map_err(|e| io_error("inspect a stored build", &e))?
            .len();
        let size = len
            .checked_sub(offset)
            .filter(|&size| size <= crate::manifest::MAX_ARTIFACT_BYTES)
            .ok_or_else(|| corrupt("invalid stored build length"))?;
        Ok(Some(size))
    }

    pub(crate) fn staged_payload(&self) -> Result<Option<(String, impl Read)>, StoreError> {
        Ok(open_slot(&self.staged_file())?.map(|(slot, reader)| (slot.version, reader)))
    }

    fn file(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    fn staged_file(&self) -> PathBuf {
        self.dir.join(STAGING_DIR).join(STAGED_FILE)
    }
}

impl InstallStore for FsInstallStore {
    fn installed(&self) -> Result<Option<Slot>, StoreError> {
        read_slot(&self.file(INSTALLED_FILE))
    }

    fn previous(&self) -> Result<Option<Slot>, StoreError> {
        read_slot(&self.file(PREVIOUS_FILE))
    }

    fn installed_identity(&self) -> Result<Option<(String, String)>, StoreError> {
        slot_identity(&self.file(INSTALLED_FILE))
    }

    fn previous_identity(&self) -> Result<Option<(String, String)>, StoreError> {
        slot_identity(&self.file(PREVIOUS_FILE))
    }

    fn staged(&self) -> Result<Option<Slot>, StoreError> {
        read_slot(&self.staged_file())
    }

    fn stage(&mut self, slot: Slot) -> Result<(), StoreError> {
        self.stage_payload(&slot.version, slot.bytes.as_slice())
    }

    fn discard_staged(&mut self) -> Result<(), StoreError> {
        remove(&self.staged_file())
    }

    fn commit_staged(&mut self) -> Result<(), StoreError> {
        let staged_path = self.staged_file();
        if !staged_path.is_file() {
            return Err(StoreError("nothing is staged".to_owned()));
        }
        let installed_path = self.file(INSTALLED_FILE);
        // Keep the old installed build first, so a failed second rename still leaves it intact.
        copy_slot(&installed_path, &self.file(PREVIOUS_FILE))?;
        replace_file(&staged_path, &installed_path)
            .map_err(|e| io_error("switch to the staged build", &e))?;
        sync_dir(&self.dir)?;
        sync_dir(&self.dir.join(STAGING_DIR))
    }

    fn swap_with_previous(&mut self) -> Result<(), StoreError> {
        if !copy_slot(&self.file(PREVIOUS_FILE), &self.file(INSTALLED_FILE))? {
            return Err(StoreError("no previous build is kept".to_owned()));
        }
        Ok(())
    }

    fn drop_previous(&mut self) -> Result<(), StoreError> {
        remove(&self.file(PREVIOUS_FILE))
    }

    fn journal(&self) -> Result<Journal, StoreError> {
        let Some(raw) = read_bounded_file(&self.file(JOURNAL_FILE), MAX_JOURNAL_BYTES)? else {
            return Ok(Journal::default());
        };
        decode_journal(&raw)
    }

    fn set_journal(&mut self, journal: Journal) -> Result<(), StoreError> {
        let encoded = encode_journal(&journal)?;
        write_atomic(&self.file(JOURNAL_FILE), &encoded)
    }
}

fn read_slot(path: &Path) -> Result<Option<Slot>, StoreError> {
    let Some((mut slot, reader)) = open_slot(path)? else {
        return Ok(None);
    };
    reader
        .take(crate::manifest::MAX_ARTIFACT_BYTES + 1)
        .read_to_end(&mut slot.bytes)
        .map_err(|e| io_error("read a stored build", &e))?;
    if slot.bytes.len() as u64 > crate::manifest::MAX_ARTIFACT_BYTES {
        return Err(corrupt("a stored build exceeds the size limit"));
    }
    Ok(Some(slot))
}

fn open_slot(path: &Path) -> Result<Option<(Slot, BufReader<File>)>, StoreError> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(io_error("open a stored build", &e)),
    };
    let max_file_bytes = crate::manifest::MAX_ARTIFACT_BYTES + MAX_SLOT_HEADER_BYTES as u64;
    if file
        .metadata()
        .map_err(|e| io_error("inspect a stored build", &e))?
        .len()
        > max_file_bytes
    {
        return Err(corrupt("a stored build exceeds the size limit"));
    }
    let mut reader = BufReader::new(file);
    let mut header = Vec::new();
    Read::take(&mut reader, MAX_SLOT_HEADER_BYTES as u64 + 1)
        .read_until(b'\n', &mut header)
        .map_err(|e| io_error("read a stored build header", &e))?;
    if header.len() > MAX_SLOT_HEADER_BYTES || header.last() != Some(&b'\n') {
        return Err(corrupt(
            "a stored build header is missing or exceeds the size limit",
        ));
    }
    // Validate the small header before allocating space for any artifact bytes.
    Ok(Some((decode_slot(&header)?, reader)))
}

fn slot_identity(path: &Path) -> Result<Option<(String, String)>, StoreError> {
    use sha2::{Digest, Sha256};
    let Some((slot, reader)) = open_slot(path)? else {
        return Ok(None);
    };
    let cap = crate::manifest::MAX_ARTIFACT_BYTES;
    let mut hash = Sha256::new();
    struct HashSink<'a>(&'a mut Sha256);
    impl Write for HashSink<'_> {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.update(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let size = std::io::copy(&mut reader.take(cap + 1), &mut HashSink(&mut hash))
        .map_err(|e| io_error("hash a stored build", &e))?;
    if size > cap {
        return Err(corrupt("a stored build exceeds the size limit"));
    }
    Ok(Some((
        slot.version,
        crate::verify::to_hex(&hash.finalize()),
    )))
}

fn copy_slot(source: &Path, target: &Path) -> Result<bool, StoreError> {
    let Some((slot, reader)) = open_slot(source)? else {
        return Ok(false);
    };
    let header = encode_slot_header(&slot)?;
    write_slot_payload(target, &header, reader)?;
    Ok(true)
}

fn write_slot_payload(target: &Path, header: &[u8], reader: impl Read) -> Result<(), StoreError> {
    write_atomic_with(target, |file| {
        file.write_all(header)
            .map_err(|e| io_error("write a slot header", &e))?;
        let cap = crate::manifest::MAX_ARTIFACT_BYTES;
        let size = std::io::copy(&mut reader.take(cap + 1), file)
            .map_err(|e| io_error("copy a stored build", &e))?;
        if size > cap {
            return Err(corrupt("a stored build exceeds the size limit"));
        }
        Ok(())
    })
}

fn read_bounded_file(path: &Path, limit: u64) -> Result<Option<Vec<u8>>, StoreError> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(io_error("open the update journal", &e)),
    };
    let mut raw = Vec::new();
    file.take(limit + 1)
        .read_to_end(&mut raw)
        .map_err(|e| io_error("read the update journal", &e))?;
    if raw.len() as u64 > limit {
        return Err(corrupt("the update journal exceeds the size limit"));
    }
    Ok(Some(raw))
}

fn remove(path: &Path) -> Result<(), StoreError> {
    match fs::remove_file(path) {
        Ok(()) => sync_dir(parent_of(path)?),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
        Err(e) => Err(io_error("remove a stored build", &e)),
    }
}

/// Write `contents` to `target` through a synced sibling temp file, then rename it into place.
fn write_atomic(target: &Path, contents: &[u8]) -> Result<(), StoreError> {
    write_atomic_parts(target, &[contents])
}

fn write_atomic_parts(target: &Path, parts: &[&[u8]]) -> Result<(), StoreError> {
    write_atomic_with(target, |file| {
        for part in parts {
            file.write_all(part)
                .map_err(|e| io_error("write a temp file", &e))?;
        }
        Ok(())
    })
}

fn write_atomic_with(
    target: &Path,
    write: impl FnOnce(&mut File) -> Result<(), StoreError>,
) -> Result<(), StoreError> {
    let dir = parent_of(target)?;
    let (tmp, mut file) = create_temp_sibling(target)?;
    let mut renamed = false;
    let result = (|| -> Result<(), StoreError> {
        write(&mut file)?;
        file.sync_all()
            .map_err(|e| io_error("sync a temp file", &e))?;
        drop(file);
        replace_file(&tmp, target).map_err(|e| io_error("rename a temp file into place", &e))?;
        renamed = true;
        sync_dir(dir)
    })();
    if result.is_err() && !renamed {
        // This path was exclusively created by this call; collisions are never cleaned up.
        let _ = fs::remove_file(&tmp);
    }
    result
}

fn parent_of(path: &Path) -> Result<&Path, StoreError> {
    path.parent()
        .ok_or_else(|| StoreError("store path has no folder".to_owned()))
}

const TEMP_CREATE_ATTEMPTS: usize = 32;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Create a unique sibling without following or truncating an existing path.
fn create_temp_sibling(target: &Path) -> Result<(PathBuf, File), StoreError> {
    let name = target
        .file_name()
        .ok_or_else(|| StoreError("store path has no file name".to_owned()))?;
    let temp_name = name.to_os_string();
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    for _ in 0..TEMP_CREATE_ATTEMPTS {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let mut candidate_name = temp_name.clone();
        candidate_name.push(format!(
            ".tmp-{}-{timestamp}-{sequence}",
            std::process::id()
        ));
        let candidate = target.with_file_name(candidate_name);
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(file) => return Ok((candidate, file)),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(io_error("create a temp file", &error)),
        }
    }
    Err(StoreError(format!(
        "could not create a temp file after {TEMP_CREATE_ATTEMPTS} attempts"
    )))
}

#[cfg(windows)]
fn replace_file(source: &Path, target: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };

    let source: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let target: Vec<u16> = target.as_os_str().encode_wide().chain(Some(0)).collect();
    // SAFETY: Both buffers are NUL-terminated UTF-16 strings alive for the call.
    let result = unsafe {
        MoveFileExW(
            source.as_ptr(),
            target.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(windows))]
fn replace_file(source: &Path, target: &Path) -> std::io::Result<()> {
    fs::rename(source, target)
}

#[cfg(unix)]
fn sync_dir(dir: &Path) -> Result<(), StoreError> {
    File::open(dir)
        .and_then(|d| d.sync_all())
        .map_err(|e| io_error("sync the install directory", &e))
}

#[cfg(not(unix))]
fn sync_dir(_dir: &Path) -> Result<(), StoreError> {
    Ok(())
}

fn encode_slot_header(slot: &Slot) -> Result<Vec<u8>, StoreError> {
    if slot.version.is_empty() || slot.version.contains('\n') {
        return Err(StoreError("slot version is not a single line".to_owned()));
    }
    if SLOT_HEADER.len() + slot.version.len() + 1 > MAX_SLOT_HEADER_BYTES
        || slot.bytes.len() as u64 > crate::manifest::MAX_ARTIFACT_BYTES
    {
        return Err(corrupt("a stored build exceeds the size limit"));
    }
    let mut out = Vec::with_capacity(SLOT_HEADER.len() + slot.version.len() + 1);
    out.extend_from_slice(SLOT_HEADER.as_bytes());
    out.extend_from_slice(slot.version.as_bytes());
    out.push(b'\n');
    Ok(out)
}

fn decode_slot(raw: &[u8]) -> Result<Slot, StoreError> {
    let newline = raw
        .iter()
        .position(|b| *b == b'\n')
        .ok_or_else(|| corrupt("a stored build has no header line"))?;
    let header = std::str::from_utf8(&raw[..newline])
        .map_err(|_| corrupt("a stored build header is not UTF-8"))?;
    let version = header
        .strip_prefix(SLOT_HEADER)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| corrupt("a stored build header is not recognized"))?;
    Ok(Slot {
        version: version.to_owned(),
        bytes: raw[newline + 1..].to_vec(),
    })
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct JournalFile {
    version: u32,
    #[serde(default)]
    pending: Option<PendingFile>,
    #[serde(default)]
    healthy: Option<String>,
    #[serde(default)]
    failed_targets: Vec<String>,
    #[serde(default)]
    clean_shutdown: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PendingFile {
    target: String,
    previous: String,
    previous_sha256: String,
    failed_launches: u32,
}

fn encode_journal(journal: &Journal) -> Result<Vec<u8>, StoreError> {
    let file = JournalFile {
        version: JOURNAL_VERSION,
        pending: journal.pending.as_ref().map(|p| PendingFile {
            target: p.target.clone(),
            previous: p.previous.clone(),
            previous_sha256: p.previous_sha256.clone(),
            failed_launches: p.failed_launches,
        }),
        healthy: journal.healthy.clone(),
        failed_targets: journal.failed_targets.clone(),
        clean_shutdown: journal.clean_shutdown,
    };
    let raw = serde_json::to_vec_pretty(&file)
        .map_err(|_| StoreError("could not encode the journal".to_owned()))?;
    if raw.len() as u64 > MAX_JOURNAL_BYTES {
        return Err(corrupt("the update journal exceeds the size limit"));
    }
    Ok(raw)
}

fn decode_journal(raw: &[u8]) -> Result<Journal, StoreError> {
    let file: JournalFile =
        serde_json::from_slice(raw).map_err(|_| corrupt("the update journal is not valid"))?;
    if file.version != JOURNAL_VERSION {
        return Err(corrupt("the update journal version is not supported"));
    }
    Ok(Journal {
        pending: file.pending.map(|p| Pending {
            target: p.target,
            previous: p.previous,
            previous_sha256: p.previous_sha256,
            failed_launches: p.failed_launches,
        }),
        healthy: file.healthy,
        failed_targets: file.failed_targets,
        clean_shutdown: file.clean_shutdown,
    })
}

fn corrupt(reason: &str) -> StoreError {
    StoreError(format!("{reason}; the store file may be damaged"))
}

fn io_error(operation: &str, err: &std::io::Error) -> StoreError {
    StoreError(format!("could not {operation}: {err}"))
}

#[cfg(test)]
mod tests {
    use super::write_atomic;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static TEST_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let sequence = TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let stamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |duration| duration.as_nanos());
            let path = std::env::temp_dir().join(format!(
                "kattri-install-store-{}-{stamp}-{sequence}",
                std::process::id()
            ));
            fs::create_dir(&path).expect("create test directory");
            Self(path)
        }

        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn preexisting_fixed_temp_file_is_not_modified_or_removed() {
        let root = TempDir::new();
        let target = root.path().join("journal.json");
        let fixed_temp = root.path().join("journal.json.tmp");
        let sentinel = b"unowned sentinel";
        fs::write(&fixed_temp, sentinel).expect("create sentinel temp file");

        write_atomic(&target, b"new target").expect("atomic write");

        assert_eq!(fs::read(&fixed_temp).expect("sentinel remains"), sentinel);
        assert_eq!(fs::read(&target).expect("target written"), b"new target");
    }

    #[cfg(unix)]
    #[test]
    fn preexisting_fixed_temp_symlink_is_not_followed_or_removed() {
        use std::os::unix::fs::symlink;

        let root = TempDir::new();
        let target = root.path().join("journal.json");
        let fixed_temp = root.path().join("journal.json.tmp");
        let sentinel = root.path().join("sentinel");
        fs::write(&sentinel, b"symlink target sentinel").expect("create sentinel");
        symlink(&sentinel, &fixed_temp).expect("create fixed temp symlink");

        write_atomic(&target, b"new target").expect("atomic write");

        assert!(
            fs::symlink_metadata(&fixed_temp)
                .expect("symlink remains")
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            fs::read(&sentinel).expect("sentinel unchanged"),
            b"symlink target sentinel"
        );
        assert_eq!(fs::read(&target).expect("target written"), b"new target");
    }

    #[cfg(unix)]
    #[test]
    fn concurrent_atomic_writes_leave_one_complete_value() {
        let root = TempDir::new();
        let target = root.path().join("journal.json");
        let payloads: Vec<Vec<u8>> = (0..8).map(|index| vec![b'a' + index; 128 * 1024]).collect();
        let threads: Vec<_> = payloads
            .iter()
            .map(|payload| {
                let target = target.clone();
                let payload = payload.clone();
                std::thread::spawn(move || write_atomic(&target, &payload))
            })
            .collect();

        for thread in threads {
            thread.join().expect("writer thread").expect("atomic write");
        }

        let result = fs::read(target).expect("read final target");
        assert!(
            payloads
                .iter()
                .any(|payload| payload.as_slice() == result.as_slice())
        );
    }
}
