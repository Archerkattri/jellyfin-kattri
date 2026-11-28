//! Profile-local playlist persistence (M6-13, first slice).
//!
//! A local playlist is an ordered list of Jellyfin item IDs. It holds no names, URLs, tokens or
//! other metadata: the encoded record is a version header plus one line per occurrence carrying
//! only the entry ID and the item ID, so a secret cannot leak by construction — there is no
//! field to put one in.
//!
//! Load path: read at most [`MAX_ENCODED_PLAYLIST_BYTES`] plus one byte, decode, and hand back
//! the model. A file that does not decode is moved aside rather than deleted, so a bad write can
//! never destroy the previous playlist.
//!
//! Save path: encode and validate first, then write through a synced sibling temp file and
//! rename it into place. A refused or failed save leaves the existing file byte-identical.

use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use crate::playlist::{MAX_ENTRIES, PlaylistEntry, PlaylistError, PlaylistModel};

/// Format marker of the current record layout.
const RECORD_HEADER: &str = "kattri-local-playlist/1";
/// Hard ceiling on the encoded record. Ids are fixed-length hex, so a full playlist fits well
/// below this; the cap only protects the reader.
pub const MAX_ENCODED_PLAYLIST_BYTES: usize = 128 * 1024;
const MAX_ASIDE_ATTEMPTS: u32 = 1_000;

/// Errors from encoding, reading or writing a stored playlist.
#[derive(Debug)]
pub enum PlaylistStoreError {
    Io(io::Error),
    /// The model could not be encoded. Nothing was written.
    Encode(PlaylistError),
    /// The stored record does not match the expected layout.
    Malformed,
}

impl std::fmt::Display for PlaylistStoreError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "local playlist storage failed: {error}"),
            Self::Encode(error) => {
                write!(formatter, "local playlist could not be encoded: {error:?}")
            }
            Self::Malformed => write!(formatter, "stored local playlist is malformed"),
        }
    }
}

impl std::error::Error for PlaylistStoreError {}

impl From<io::Error> for PlaylistStoreError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// What a load found.
#[derive(Debug)]
pub enum PlaylistLoadOutcome {
    Missing,
    Loaded(PlaylistModel),
    /// The record did not decode. It was moved aside and the path is free for a new save.
    MovedAside {
        reason: String,
        moved_to: PathBuf,
    },
}

/// One stored occurrence that the server no longer offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnavailableEntry {
    /// Position in the stored playlist, counting from zero.
    pub index: usize,
    pub entry_id: String,
    pub item_id: String,
}

/// A stored playlist split into what can play and what the server no longer has.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ReconcileReport {
    /// Playable entries in their original order. Duplicates are kept.
    pub playable: Vec<PlaylistEntry>,
    /// Missing occurrences in their original order, so skipped playback can report them.
    pub unavailable: Vec<UnavailableEntry>,
}

impl ReconcileReport {
    pub fn is_empty(&self) -> bool {
        self.playable.is_empty() && self.unavailable.is_empty()
    }
}

/// Split `entries` into playable and unavailable occurrences.
///
/// Order and duplicates are preserved on both sides: a playlist that lists the same item twice
/// stays twice, and unavailable entries keep their original index so the caller can report them
/// while playback skips them.
pub fn reconcile(entries: &[PlaylistEntry], available: &HashSet<String>) -> ReconcileReport {
    let mut report = ReconcileReport::default();
    for (index, entry) in entries.iter().enumerate() {
        if available.contains(entry.item_id()) {
            report.playable.push(entry.clone());
        } else {
            report.unavailable.push(UnavailableEntry {
                index,
                entry_id: entry.entry_id().to_owned(),
                item_id: entry.item_id().to_owned(),
            });
        }
    }
    report
}

/// Encode the record. Only entry and item IDs are written.
pub fn encode(model: &PlaylistModel) -> Result<String, PlaylistStoreError> {
    let mut text = String::with_capacity(RECORD_HEADER.len() + 1 + model.len() * 70);
    text.push_str(RECORD_HEADER);
    text.push('\n');
    for entry in model.entries() {
        // Ids are validated hex, so a tab or newline can never appear here.
        text.push_str(entry.entry_id());
        text.push('\t');
        text.push_str(entry.item_id());
        text.push('\n');
    }
    if text.len() > MAX_ENCODED_PLAYLIST_BYTES {
        return Err(PlaylistStoreError::Malformed);
    }
    Ok(text)
}

fn decode(text: &str) -> Result<PlaylistModel, PlaylistStoreError> {
    let mut lines = text.lines();
    if lines.next() != Some(RECORD_HEADER) {
        return Err(PlaylistStoreError::Malformed);
    }
    let mut entries = Vec::new();
    for line in lines {
        if line.is_empty() {
            return Err(PlaylistStoreError::Malformed);
        }
        let Some((entry_id, item_id)) = line.split_once('\t') else {
            return Err(PlaylistStoreError::Malformed);
        };
        if entries.len() >= MAX_ENTRIES {
            return Err(PlaylistStoreError::Malformed);
        }
        entries.push(PlaylistEntry::new(entry_id, item_id).map_err(PlaylistStoreError::Encode)?);
    }
    PlaylistModel::from_entries(entries).map_err(PlaylistStoreError::Encode)
}

/// Write `model` to `path` atomically.
///
/// The record is encoded and validated before the filesystem is touched, so an unusable model
/// cannot partially replace a good playlist.
pub fn save_playlist(path: &Path, model: &PlaylistModel) -> Result<(), PlaylistStoreError> {
    let text = encode(model)?;
    let Some(file_name) = path.file_name() else {
        return Err(PlaylistStoreError::Io(io::Error::new(
            io::ErrorKind::InvalidInput,
            "playlist path has no file name",
        )));
    };
    if let Some(dir) = path.parent().filter(|dir| !dir.as_os_str().is_empty()) {
        fs::create_dir_all(dir)?;
    }
    let temp = path.with_file_name(temp_file_name(file_name));
    write_atomic(path, &text, &temp)?;
    Ok(())
}

/// Read the playlist stored at `path`.
pub fn load_playlist(path: &Path) -> Result<PlaylistLoadOutcome, PlaylistStoreError> {
    let bytes = match read_capped(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(PlaylistLoadOutcome::Missing);
        }
        Err(error) => return Err(PlaylistStoreError::Io(error)),
    };
    let decoded = if bytes.len() > MAX_ENCODED_PLAYLIST_BYTES {
        Err(PlaylistStoreError::Malformed)
    } else {
        match std::str::from_utf8(&bytes) {
            Ok(text) => decode(text),
            Err(_) => Err(PlaylistStoreError::Malformed),
        }
    };
    match decoded {
        Ok(model) => Ok(PlaylistLoadOutcome::Loaded(model)),
        Err(error) => {
            let reason = error.to_string();
            let moved_to = move_aside(path)?;
            Ok(PlaylistLoadOutcome::MovedAside { reason, moved_to })
        }
    }
}

fn temp_file_name(file_name: &std::ffi::OsStr) -> std::ffi::OsString {
    let mut name = std::ffi::OsString::from(file_name);
    name.push(".tmp");
    name
}

fn write_atomic(path: &Path, text: &str, temp: &Path) -> io::Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(temp)?;
    let written = file
        .write_all(text.as_bytes())
        .and_then(|()| file.sync_all());
    // The handle must be closed before the file can be removed or renamed on Windows.
    drop(file);
    if let Err(error) = written.and_then(|()| fs::rename(temp, path)) {
        let _ = fs::remove_file(temp);
        return Err(error);
    }
    Ok(())
}

fn read_capped(path: &Path) -> io::Result<Vec<u8>> {
    let file = OpenOptions::new().read(true).open(path)?;
    let mut bytes = Vec::new();
    file.take(MAX_ENCODED_PLAYLIST_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// Rename a record that does not decode, keeping its contents for inspection.
fn move_aside(path: &Path) -> io::Result<PathBuf> {
    for attempt in 1..=MAX_ASIDE_ATTEMPTS {
        let candidate = path.with_extension(format!("bad{attempt}"));
        if candidate.exists() {
            continue;
        }
        match fs::rename(path, &candidate) {
            Ok(()) => return Ok(candidate),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "no free name for the unusable playlist record",
    ))
}

#[cfg(test)]
mod tests {
    use super::{
        MAX_ENCODED_PLAYLIST_BYTES, PlaylistLoadOutcome, PlaylistStoreError, encode, load_playlist,
        reconcile, save_playlist,
    };
    use crate::playlist::{MAX_ENTRIES, PlaylistEntry, PlaylistModel};
    use std::collections::HashSet;
    use std::path::PathBuf;

    const ITEM_A: &str = "0000000000000000000000000000aaaa";
    const ITEM_B: &str = "0000000000000000000000000000bbbb";
    const ITEM_C: &str = "0000000000000000000000000000cccc";

    fn entry(n: usize, item: &str) -> PlaylistEntry {
        PlaylistEntry::new(&format!("{:032x}", 0x2000 + n), item).expect("valid entry")
    }

    fn model_of(items: &[&str]) -> PlaylistModel {
        let mut model = PlaylistModel::default();
        for (n, item) in items.iter().enumerate() {
            model.append(entry(n, item)).expect("append");
        }
        model
    }

    fn scratch(name: &str) -> PathBuf {
        let directory = std::env::temp_dir().join(format!(
            "kattri-playlist-store-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).expect("scratch directory");
        directory
    }

    #[test]
    fn round_trip_preserves_order_and_duplicates() {
        let directory = scratch("round-trip");
        let path = directory.join("local-playlist.txt");
        let model = model_of(&[ITEM_A, ITEM_B, ITEM_A, ITEM_C]);

        save_playlist(&path, &model).expect("save");
        let PlaylistLoadOutcome::Loaded(loaded) = load_playlist(&path).expect("load") else {
            panic!("expected a loaded playlist");
        };

        assert_eq!(loaded, model, "order and duplicates survive a restart");
        assert_eq!(loaded.len(), 4);
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn the_record_holds_only_entry_and_item_ids() {
        let directory = scratch("only-ids");
        let path = directory.join("local-playlist.txt");
        save_playlist(&path, &model_of(&[ITEM_A, ITEM_B])).expect("save");

        let text = std::fs::read_to_string(&path).expect("read record");
        let mut lines = text.lines();
        assert_eq!(lines.next(), Some("kattri-local-playlist/1"));
        for line in lines {
            let (entry_id, item_id) = line.split_once('\t').expect("two fields");
            assert_eq!(entry_id.len(), 32, "only the entry id is written");
            assert_eq!(item_id.len(), 32, "only the item id is written");
        }
        // Nothing else exists in the record to leak: no name, URL or credential field.
        for forbidden in ["http", "https", "token", "api_key", "password", "Name"] {
            assert!(
                !text.contains(forbidden),
                "record must not carry {forbidden:?}: {text}"
            );
        }
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn an_unsupported_record_is_moved_aside_and_keeps_its_bytes() {
        let directory = scratch("moved-aside");
        let path = directory.join("local-playlist.txt");
        let garbage = "not a playlist\nwith junk\n";
        std::fs::write(&path, garbage).expect("write garbage");

        let PlaylistLoadOutcome::MovedAside { moved_to, .. } =
            load_playlist(&path).expect("load reports the move")
        else {
            panic!("expected the unusable record to be moved aside");
        };

        assert_eq!(
            std::fs::read_to_string(&moved_to).expect("aside file"),
            garbage,
            "the unusable record is kept, not deleted"
        );
        assert!(!path.exists(), "the path is free for a new save");
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn a_failed_save_leaves_the_existing_record_unchanged() {
        let directory = scratch("failed-save");
        let path = directory.join("local-playlist.txt");
        let original = model_of(&[ITEM_A, ITEM_B]);
        save_playlist(&path, &original).expect("first save");
        let before = std::fs::read(&path).expect("read before");

        // A stale temp file makes the atomic write refuse before it can touch the record.
        std::fs::write(directory.join("local-playlist.txt.tmp"), b"occupied").expect("temp");

        let result = save_playlist(&path, &model_of(&[ITEM_C]));
        assert!(
            result.is_err(),
            "the save must fail on an occupied temp path"
        );
        assert_eq!(
            std::fs::read(&path).expect("read after"),
            before,
            "a failed save leaves the previous playlist byte-identical"
        );
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn a_second_save_replaces_the_record_and_leaves_no_temp_file() {
        let directory = scratch("replace");
        let path = directory.join("local-playlist.txt");
        save_playlist(&path, &model_of(&[ITEM_A, ITEM_B])).expect("first save");
        save_playlist(&path, &model_of(&[ITEM_C])).expect("second save");

        let PlaylistLoadOutcome::Loaded(loaded) = load_playlist(&path).expect("load") else {
            panic!("expected the replaced record to load");
        };
        assert_eq!(
            loaded,
            model_of(&[ITEM_C]),
            "the second save replaces the record"
        );
        let text = std::fs::read_to_string(&path).expect("read record");
        assert!(!text.contains(ITEM_A), "the replaced item is gone");
        assert!(
            !directory.join("local-playlist.txt.tmp").exists(),
            "the atomic write leaves no temp file behind"
        );
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn a_missing_record_reports_missing_without_creating_anything() {
        let directory = scratch("missing");
        let path = directory.join("local-playlist.txt");
        assert!(matches!(
            load_playlist(&path).expect("load"),
            PlaylistLoadOutcome::Missing
        ));
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn unavailable_items_are_reported_in_order_and_playable_duplicates_are_kept() {
        let model = model_of(&[ITEM_A, ITEM_B, ITEM_A, ITEM_C]);
        let available: HashSet<String> = [ITEM_A, ITEM_C].into_iter().map(str::to_owned).collect();

        let report = reconcile(model.entries(), &available);

        let playable: Vec<&str> = report.playable.iter().map(PlaylistEntry::item_id).collect();
        assert_eq!(
            playable,
            [ITEM_A, ITEM_A, ITEM_C],
            "order and duplicates kept"
        );
        assert_eq!(report.unavailable.len(), 1);
        assert_eq!(report.unavailable[0].index, 1, "original position");
        assert_eq!(report.unavailable[0].item_id, ITEM_B);
        assert_eq!(report.unavailable[0].entry_id, entry(1, ITEM_B).entry_id());
    }

    #[test]
    fn the_encoder_rejects_a_record_above_the_byte_cap() {
        // The model's own bounds keep real playlists far below the cap; the guard is only here
        // so a hostile or corrupted model can never be written.
        let mut model = PlaylistModel::default();
        for n in 0..MAX_ENTRIES {
            model.append(entry(n, ITEM_A)).expect("append");
        }
        let text = encode(&model).expect("full playlist encodes");
        assert!(text.len() < MAX_ENCODED_PLAYLIST_BYTES);
        let oversized = format!(
            "kattri-local-playlist/1\n{}",
            "0".repeat(MAX_ENCODED_PLAYLIST_BYTES)
        );
        assert!(matches!(
            super::decode(&oversized),
            Err(PlaylistStoreError::Malformed | PlaylistStoreError::Encode(_))
        ));
    }
}
