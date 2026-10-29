//! On-disk [`InstallStore`] for Windows, macOS and Linux (M4-16), built on `std::fs` only.
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
//! Every write goes to a `*.tmp` sibling in the same directory, is synced to disk, and is then
//! renamed over its target. Rename within one directory is atomic on the supported platforms,
//! so a reader sees either the old file or the new one. The directory itself is synced after
//! each rename on Unix. Windows has no portable directory sync, so the rename is the only
//! durability step there.
//!
//! Commit is two renames. The old installed build is first copied to `previous.slot`, then the
//! staged file is renamed out of the staging folder over `installed.slot`. Both folders are
//! inside the store directory, so they share one volume and the second rename stays atomic. A
//! crash between the two leaves the installed build unchanged. The journal written before the
//! commit lets the next launch recognize the abandoned update (see [`crate::apply::on_launch`]).
//!
//! Rollback atomically writes the kept previous build over `installed.slot`. The previous
//! slot remains intact until the rollback journal is committed, so a journal write failure can
//! safely retry the same rollback.
//!
//! Error messages name the operation and the OS error, never file contents.

use std::fs::{self, File};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::apply::{InstallStore, Journal, Pending, Slot, StoreError};

const INSTALLED_FILE: &str = "installed.slot";
const PREVIOUS_FILE: &str = "previous.slot";
const STAGING_DIR: &str = "staging";
const STAGED_FILE: &str = "staged.slot";
const JOURNAL_FILE: &str = "journal.json";
const SLOT_HEADER: &str = "kattri-slot v1 ";
const JOURNAL_VERSION: u32 = 1;

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

    fn staged(&self) -> Result<Option<Slot>, StoreError> {
        read_slot(&self.staged_file())
    }

    fn stage(&mut self, slot: Slot) -> Result<(), StoreError> {
        write_slot(&self.staged_file(), &slot)
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
        match fs::read(&installed_path) {
            Ok(raw) => write_atomic(&self.file(PREVIOUS_FILE), &raw)?,
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => return Err(io_error("read the installed build", &e)),
        }
        fs::rename(&staged_path, &installed_path)
            .map_err(|e| io_error("switch to the staged build", &e))?;
        sync_dir(&self.dir)?;
        sync_dir(&self.dir.join(STAGING_DIR))
    }

    fn swap_with_previous(&mut self) -> Result<(), StoreError> {
        let Some(previous) = self.previous()? else {
            return Err(StoreError("no previous build is kept".to_owned()));
        };
        write_slot(&self.file(INSTALLED_FILE), &previous)
    }

    fn drop_previous(&mut self) -> Result<(), StoreError> {
        remove(&self.file(PREVIOUS_FILE))
    }

    fn journal(&self) -> Result<Journal, StoreError> {
        match fs::read(self.file(JOURNAL_FILE)) {
            Ok(raw) => decode_journal(&raw),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(Journal::default()),
            Err(e) => Err(io_error("read the update journal", &e)),
        }
    }

    fn set_journal(&mut self, journal: Journal) -> Result<(), StoreError> {
        let encoded = encode_journal(&journal)?;
        write_atomic(&self.file(JOURNAL_FILE), &encoded)
    }
}

fn read_slot(path: &Path) -> Result<Option<Slot>, StoreError> {
    match fs::read(path) {
        Ok(raw) => decode_slot(&raw).map(Some),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(io_error("read a stored build", &e)),
    }
}

fn write_slot(path: &Path, slot: &Slot) -> Result<(), StoreError> {
    let encoded = encode_slot(slot)?;
    write_atomic(path, &encoded)
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
    let dir = parent_of(target)?;
    let tmp = tmp_sibling(target)?;
    let result = (|| -> Result<(), StoreError> {
        let mut file = File::create(&tmp).map_err(|e| io_error("create a temp file", &e))?;
        file.write_all(contents)
            .map_err(|e| io_error("write a temp file", &e))?;
        file.sync_all()
            .map_err(|e| io_error("sync a temp file", &e))?;
        drop(file);
        fs::rename(&tmp, target).map_err(|e| io_error("rename a temp file into place", &e))?;
        sync_dir(dir)
    })();
    if result.is_err() {
        // Best effort: the target was not replaced, so a leftover temp file is only clutter.
        let _ = fs::remove_file(&tmp);
    }
    result
}

fn parent_of(path: &Path) -> Result<&Path, StoreError> {
    path.parent()
        .ok_or_else(|| StoreError("store path has no folder".to_owned()))
}

/// The temp path next to `target`: the same directory and file name with `.tmp` appended.
fn tmp_sibling(target: &Path) -> Result<PathBuf, StoreError> {
    let name = target
        .file_name()
        .ok_or_else(|| StoreError("store path has no file name".to_owned()))?;
    let mut tmp_name = name.to_os_string();
    tmp_name.push(".tmp");
    Ok(target.with_file_name(tmp_name))
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

fn encode_slot(slot: &Slot) -> Result<Vec<u8>, StoreError> {
    if slot.version.is_empty() || slot.version.contains('\n') {
        return Err(StoreError("slot version is not a single line".to_owned()));
    }
    let mut out = Vec::with_capacity(SLOT_HEADER.len() + slot.version.len() + 1 + slot.bytes.len());
    out.extend_from_slice(SLOT_HEADER.as_bytes());
    out.extend_from_slice(slot.version.as_bytes());
    out.push(b'\n');
    out.extend_from_slice(&slot.bytes);
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
    serde_json::to_vec_pretty(&file)
        .map_err(|_| StoreError("could not encode the journal".to_owned()))
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
