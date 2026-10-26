//! Per-profile keymap files (M2-17): load and save one input map as `<name>.json` inside
//! a profile's `inputmaps` folder.
//!
//! This module takes the `inputmaps` folder as a parameter (see [`input_maps_dir`]) and does
//! not depend on `kattri-settings`. The caller gets the folder from the profile store, which
//! has already checked the profile name, so no name reaches a path outside the profiles folder.
//!
//! Rules:
//! - A missing file means the default bindings, with no notice.
//! - A file that cannot be read, is larger than [`MAX_MAP_BYTES`], or is not a valid v1 map
//!   falls back to the defaults with one notice. A load never writes to disk, so the bad
//!   file is left as it is.
//! - A save does not replace a file it cannot parse. That file is moved to
//!   `<name>.json.corrupt` (or a numbered name) first, and its new path is returned in
//!   [`SaveReport`] so the caller can tell the user.
//! - A save writes a unique `<name>.json.tmp-<pid>-<sequence>`, flushes it to disk, and renames it over the
//!   target. A failed write removes the temporary file and leaves the old file in place.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

use crate::input_map::{InputMap, InputMapError};

/// Folder under a profile folder that holds the input maps.
pub const INPUT_MAPS_DIR: &str = "inputmaps";

/// Largest map file that is read. Real maps are a few kilobytes; the cap keeps a stray
/// large file from being read whole.
pub const MAX_MAP_BYTES: u64 = 1 << 20;

/// Longest accepted map name, in bytes.
const MAP_NAME_MAX_LEN: usize = 32;

/// How many numbered quarantine names are tried before a save gives up.
const QUARANTINE_ATTEMPTS: u32 = 100;

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// The `inputmaps` folder of a profile folder: `<profile_dir>/inputmaps`.
pub fn input_maps_dir(profile_dir: &Path) -> PathBuf {
    profile_dir.join(INPUT_MAPS_DIR)
}

/// The base name of a map file, such as `keyboard` for `keyboard.json`.
///
/// Accepts 1 to 32 lowercase ASCII letters, digits and hyphens. Windows device names
/// (`con`, `nul`, `com0`-`com9`, `lpt0`-`lpt9` and so on) are refused, because they cannot
/// be file names on Windows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MapName(String);

impl MapName {
    pub fn new(name: &str) -> Result<Self, StoreError> {
        let charset_ok = name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
        if name.is_empty()
            || name.len() > MAP_NAME_MAX_LEN
            || !charset_ok
            || is_windows_device_name(name)
        {
            return Err(StoreError::InvalidName(name.to_owned()));
        }
        Ok(Self(name.to_owned()))
    }

    /// The name as written in the file name, without the `.json` suffix.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn file_name(&self) -> String {
        format!("{}.json", self.0)
    }
}

fn is_windows_device_name(name: &str) -> bool {
    matches!(name, "con" | "prn" | "aux" | "nul")
        || (name.len() == 4
            && (name.starts_with("com") || name.starts_with("lpt"))
            && name.as_bytes()[3].is_ascii_digit())
}

/// Errors from saving a map. Loading never returns an error; it falls back instead.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error(
        "`{0}` is not a valid map name (lowercase letters, digits and hyphens; 1 to 32 characters; not a Windows device name)"
    )]
    InvalidName(String),
    #[error("input map could not be serialized: {0}")]
    Map(#[from] InputMapError),
    #[error("input map file could not be written: {0}")]
    Io(#[from] io::Error),
    #[error("no free name to move the unusable map file to")]
    NoQuarantineName,
}

/// Why a map file was not used.
#[derive(Debug, thiserror::Error)]
pub enum LoadFault {
    #[error("the file could not be read: {0}")]
    Unreadable(#[source] io::Error),
    #[error("the file is larger than {MAX_MAP_BYTES} bytes")]
    TooLarge,
    #[error("the file is not a valid input map: {0}")]
    Malformed(#[source] InputMapError),
}

/// Where the bindings of a [`LoadedMap`] came from.
#[derive(Debug)]
pub enum MapSource {
    /// Read from the file. Loader warnings (duplicate entries, an empty essential action)
    /// are in [`LoadedMap::notices`].
    File,
    /// No file yet. The defaults are in use.
    Missing,
    /// The file could not be used. The defaults are in use and the file is unchanged.
    Fallback(LoadFault),
}

/// A map loaded for a profile, with the notices to show the user.
#[derive(Debug)]
pub struct LoadedMap {
    pub map: InputMap,
    pub source: MapSource,
    pub notices: Vec<String>,
}

/// Result of a successful save.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SaveReport {
    /// Where an unparseable file was moved before the save, if there was one.
    pub quarantined: Option<PathBuf>,
}

/// Load the map `name` from `maps_dir`. Never fails: a missing file gives the defaults, and an
/// unusable file gives the defaults plus one notice.
pub fn load_map(maps_dir: &Path, name: &MapName) -> LoadedMap {
    match read_bounded(&maps_dir.join(name.file_name())) {
        Ok(None) => LoadedMap {
            map: InputMap::legacy_defaults(),
            source: MapSource::Missing,
            notices: Vec::new(),
        },
        Ok(Some(text)) => match InputMap::from_json_str(&text) {
            Ok(loaded) => LoadedMap {
                map: loaded.map,
                source: MapSource::File,
                notices: loaded.warnings,
            },
            Err(err) => fallback(LoadFault::Malformed(err)),
        },
        Err(fault) => fallback(fault),
    }
}

fn fallback(fault: LoadFault) -> LoadedMap {
    let notice = format!(
        "Input map could not be used, so the default bindings are in effect ({fault}). \
         The file is unchanged until the next save."
    );
    LoadedMap {
        map: InputMap::legacy_defaults(),
        source: MapSource::Fallback(fault),
        notices: vec![notice],
    }
}

/// Write `map` as `<name>.json` in `maps_dir`, creating the folder if needed.
pub fn save_map(maps_dir: &Path, name: &MapName, map: &InputMap) -> Result<SaveReport, StoreError> {
    let text = map.to_json_string()?;
    fs::create_dir_all(maps_dir)?;
    let file_name = name.file_name();
    let target = maps_dir.join(&file_name);
    let quarantined = move_unusable_aside(maps_dir, &target, &file_name)?;

    let (tmp, file) = create_temp(maps_dir, &file_name)?;
    if let Err(err) = write_and_sync(file, text.as_bytes()) {
        let _ = fs::remove_file(&tmp);
        return Err(err.into());
    }
    // std::fs::rename replaces an existing destination on Windows and Unix.
    if let Err(err) = fs::rename(&tmp, &target) {
        let _ = fs::remove_file(&tmp);
        return Err(err.into());
    }
    sync_parent(maps_dir)?;
    Ok(SaveReport { quarantined })
}

/// Modification time of a map file. `None` when the file is missing or its metadata cannot
/// be read; both count as "no file" for [`map_file_changed`].
pub fn map_file_mtime(maps_dir: &Path, name: &MapName) -> Option<SystemTime> {
    fs::metadata(maps_dir.join(name.file_name()))
        .and_then(|meta| meta.modified())
        .ok()
}

/// True when a map file's modification time differs from the value recorded at the last
/// check. `last_seen` is what the caller stored at that check (`None` before the first
/// check or when the file was missing). `now` is what [`map_file_mtime`] returns now.
///
/// The caller reads the times and this function only compares them, so tests can pass
/// simulated values. A time that moves backwards (a restored file) counts as a change.
/// A file that appears or disappears counts as a change too.
///
/// Poll order matters. Read the time before loading the file, so an edit made during the
/// load is seen at the next poll. The time has the resolution of the file system, so two
/// edits within one tick can look like one.
pub fn map_file_changed(last_seen: Option<SystemTime>, now: Option<SystemTime>) -> bool {
    last_seen != now
}

/// Read a map file, refusing files over [`MAX_MAP_BYTES`]. `Ok(None)` means no file.
fn read_bounded(path: &Path) -> Result<Option<String>, LoadFault> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(LoadFault::Unreadable(err)),
    };
    let mut text = String::new();
    file.take(MAX_MAP_BYTES + 1)
        .read_to_string(&mut text)
        .map_err(LoadFault::Unreadable)?;
    if text.len() as u64 > MAX_MAP_BYTES {
        return Err(LoadFault::TooLarge);
    }
    Ok(Some(text))
}

/// If `target` exists and is not a usable map, rename it aside so the save does not destroy it.
fn move_unusable_aside(
    maps_dir: &Path,
    target: &Path,
    file_name: &str,
) -> Result<Option<PathBuf>, StoreError> {
    match read_bounded(target) {
        Ok(None) => return Ok(None),
        Ok(Some(text)) if InputMap::from_json_str(&text).is_ok() => return Ok(None),
        Ok(Some(_)) | Err(_) => {}
    }
    let aside = free_quarantine_path(maps_dir, file_name)?;
    fs::rename(target, &aside)?;
    Ok(Some(aside))
}

fn free_quarantine_path(maps_dir: &Path, file_name: &str) -> Result<PathBuf, StoreError> {
    let base = format!("{file_name}.corrupt");
    for attempt in 0..QUARANTINE_ATTEMPTS {
        let name = if attempt == 0 {
            base.clone()
        } else {
            format!("{base}-{attempt}")
        };
        let candidate = maps_dir.join(name);
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    Err(StoreError::NoQuarantineName)
}

fn create_temp(maps_dir: &Path, file_name: &str) -> Result<(PathBuf, File), StoreError> {
    for _ in 0..100 {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = maps_dir.join(format!("{file_name}.tmp-{}-{sequence}", std::process::id()));
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate a unique temporary input map",
    )
    .into())
}

fn write_and_sync(mut file: File, bytes: &[u8]) -> io::Result<()> {
    file.write_all(bytes)?;
    file.flush()?;
    file.sync_all()
}

#[cfg(unix)]
fn sync_parent(path: &Path) -> Result<(), StoreError> {
    File::open(path)?.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
fn sync_parent(_path: &Path) -> Result<(), StoreError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::{Action, HostAction};
    use crate::test_support::Scratch;
    use std::time::{Duration, UNIX_EPOCH};

    fn keyboard() -> MapName {
        MapName::new("keyboard").expect("valid name")
    }

    #[test]
    fn map_names_accept_plain_names_and_refuse_paths_and_device_names() {
        assert!(MapName::new("keyboard").is_ok());
        assert!(MapName::new("gamepad-xbox").is_ok());
        let bad = [
            "",
            "Keyboard",
            "../keyboard",
            "a/b",
            "a\\b",
            "keyboard.json",
            "a b",
            "con",
            "nul",
            "com0",
            "com1",
            "lpt9",
        ];
        for name in bad {
            assert!(
                matches!(MapName::new(name), Err(StoreError::InvalidName(_))),
                "{name} should be refused"
            );
        }
        assert!(MapName::new(&"x".repeat(33)).is_err());
    }

    #[test]
    fn input_maps_folder_sits_under_the_profile_folder() {
        let profile = Path::new("profiles").join("kids");
        assert_eq!(input_maps_dir(&profile), profile.join("inputmaps"));
    }

    #[test]
    fn missing_file_loads_the_defaults_without_a_notice() {
        let dir = Scratch::new("missing");
        let loaded = load_map(&dir.0, &keyboard());
        assert!(matches!(loaded.source, MapSource::Missing));
        assert_eq!(loaded.map, InputMap::legacy_defaults());
        assert!(loaded.notices.is_empty());
    }

    #[test]
    fn save_then_load_round_trips_a_rebound_map() {
        let dir = Scratch::new("roundtrip");
        let mut map = InputMap::legacy_defaults();
        map.set_bindings(Action::Pause, &["key:F9", "pad:South"])
            .expect("valid bindings");
        map.set_bindings(Action::Stop, &[])
            .expect("non-essential action");

        let report = save_map(&dir.0, &keyboard(), &map).expect("save");
        assert_eq!(report.quarantined, None);

        let loaded = load_map(&dir.0, &keyboard());
        assert!(matches!(loaded.source, MapSource::File));
        assert!(loaded.notices.is_empty(), "{:?}", loaded.notices);
        assert_eq!(loaded.map, map);

        let files: Vec<String> = fs::read_dir(&dir.0)
            .expect("read dir")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(files, vec!["keyboard.json".to_owned()], "no temp file left");
    }

    #[test]
    fn plus_key_chords_from_escaped_legacy_patterns_are_reachable() {
        let keymap = crate::keymap::parse_keymap(
            r#"{"name":"Keyboard","idmatcher":"Keyboard.*","mapping":{"(Shift\\+)?\\+":"increase_volume"}}"#,
        )
        .expect("parse keymap");
        let plus = keymap
            .bindings
            .iter()
            .find(|binding| matches!(&binding.trigger, crate::keymap::Trigger::Key(chord) if chord.key == "+" && !chord.shift))
            .expect("unmodified plus binding");
        assert_eq!(plus.source_pattern, r#"(Shift\+)?\+"#);
    }

    #[test]
    fn defaults_saved_and_loaded_keep_every_action_but_pip_reachable_by_keyboard() {
        let dir = Scratch::new("reachable");
        save_map(&dir.0, &keyboard(), &InputMap::legacy_defaults()).expect("save");
        let loaded = load_map(&dir.0, &keyboard());
        assert_eq!(
            loaded.map.actions_without_keyboard_binding(),
            vec![Action::Host(HostAction::TogglePip)]
        );
    }

    #[test]
    fn corrupt_file_falls_back_to_defaults_and_load_leaves_it_untouched() {
        let dir = Scratch::new("corrupt");
        let path = dir.0.join("keyboard.json");
        let original = "{ \"schemaVersion\": 1, \"bindings\": ";
        fs::write(&path, original).expect("write");

        let loaded = load_map(&dir.0, &keyboard());
        assert!(matches!(
            loaded.source,
            MapSource::Fallback(LoadFault::Malformed(_))
        ));
        assert_eq!(loaded.map, InputMap::legacy_defaults());
        assert_eq!(loaded.notices.len(), 1, "{:?}", loaded.notices);
        assert!(loaded.notices[0].contains("default bindings"));
        assert_eq!(fs::read_to_string(&path).expect("read"), original);
    }

    #[test]
    fn save_moves_an_unparseable_file_aside_instead_of_overwriting_it() {
        let dir = Scratch::new("quarantine");
        let path = dir.0.join("keyboard.json");
        let original = "{ not json";
        fs::write(&path, original).expect("write");

        let mut map = InputMap::legacy_defaults();
        map.set_bindings(Action::Home, &["key:F1"]).expect("valid");
        let moved = save_map(&dir.0, &keyboard(), &map)
            .expect("save")
            .quarantined
            .expect("bad file moved aside");
        assert!(moved.ends_with("keyboard.json.corrupt"), "{moved:?}");
        assert_eq!(fs::read_to_string(&moved).expect("read"), original);
        assert_eq!(load_map(&dir.0, &keyboard()).map, map);

        // A second unusable file gets a numbered quarantine name; the first is kept.
        fs::write(&path, "[]").expect("write");
        let second = save_map(&dir.0, &keyboard(), &map)
            .expect("save")
            .quarantined
            .expect("second bad file moved aside");
        assert!(second.ends_with("keyboard.json.corrupt-1"), "{second:?}");
        assert_eq!(fs::read_to_string(&moved).expect("read"), original);
    }

    #[test]
    fn a_folder_named_like_the_map_is_unreadable_and_is_not_deleted() {
        let dir = Scratch::new("unreadable");
        let path = dir.0.join("keyboard.json");
        fs::create_dir(&path).expect("mkdir");

        let loaded = load_map(&dir.0, &keyboard());
        assert!(matches!(
            loaded.source,
            MapSource::Fallback(LoadFault::Unreadable(_))
        ));
        assert_eq!(loaded.map, InputMap::legacy_defaults());
        assert!(path.is_dir());
    }

    #[test]
    fn oversized_file_falls_back_without_being_parsed() {
        let dir = Scratch::new("oversized");
        let path = dir.0.join("keyboard.json");
        let size = usize::try_from(MAX_MAP_BYTES + 1).expect("fits in usize");
        fs::write(&path, vec![b' '; size]).expect("write");

        let loaded = load_map(&dir.0, &keyboard());
        assert!(matches!(
            loaded.source,
            MapSource::Fallback(LoadFault::TooLarge)
        ));
        assert_eq!(loaded.map, InputMap::legacy_defaults());
    }

    fn at(secs: u64) -> Option<SystemTime> {
        Some(UNIX_EPOCH + Duration::from_secs(secs))
    }

    #[test]
    fn an_unchanged_time_is_not_a_change() {
        assert!(!map_file_changed(at(100), at(100)));
    }

    #[test]
    fn a_later_time_is_a_change() {
        assert!(map_file_changed(at(100), at(101)));
    }

    #[test]
    fn an_earlier_time_is_a_change_for_a_restored_file() {
        assert!(map_file_changed(at(100), at(50)));
    }

    #[test]
    fn a_file_that_appears_or_disappears_is_a_change() {
        assert!(map_file_changed(None, at(100)));
        assert!(map_file_changed(at(100), None));
    }

    #[test]
    fn two_missing_files_are_not_a_change() {
        assert!(!map_file_changed(None, None));
    }

    #[test]
    fn the_first_check_reports_a_change_when_the_file_exists() {
        assert!(map_file_changed(None, at(0)));
    }

    #[test]
    fn a_poll_loop_reports_each_edit_once() {
        // Simulated polls of a file that is edited at t=200 and again at t=260.
        let edits = [
            (0, 100),
            (1, 100),
            (2, 100),
            (3, 200),
            (4, 200),
            (5, 260),
            (6, 260),
        ];
        let mut last_seen: Option<SystemTime> = None;
        let mut reloads = Vec::new();
        for (poll, mtime) in edits {
            let now = at(mtime);
            if map_file_changed(last_seen, now) {
                reloads.push(poll);
            }
            last_seen = now;
        }
        // One reload at the first poll (no earlier check), then one per edit.
        assert_eq!(reloads, vec![0, 3, 5]);
    }

    #[test]
    fn map_file_mtime_is_none_for_a_missing_file_and_some_after_a_save() {
        let dir = Scratch::new("mtime");
        assert_eq!(map_file_mtime(&dir.0, &keyboard()), None);
        save_map(&dir.0, &keyboard(), &InputMap::legacy_defaults()).expect("save");
        assert!(map_file_mtime(&dir.0, &keyboard()).is_some());
    }

    #[test]
    fn duplicate_entries_in_a_file_load_with_their_warning() {
        let dir = Scratch::new("duplicates");
        let text =
            r#"{"schemaVersion":1,"bindings":{"home":["key:H","key:H"],"back":["key:Esc"]}}"#;
        fs::write(dir.0.join("keyboard.json"), text).expect("write");

        let loaded = load_map(&dir.0, &keyboard());
        assert!(matches!(loaded.source, MapSource::File));
        assert!(
            loaded
                .notices
                .iter()
                .any(|n| n.contains("duplicate binding")),
            "{:?}",
            loaded.notices
        );
        assert_eq!(loaded.map.bindings(Action::Home), &["key:H".to_owned()]);
    }
}
