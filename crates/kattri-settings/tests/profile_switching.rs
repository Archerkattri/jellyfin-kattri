//! M2-13 switching rules: a corrupt index is refused and left as it is, a switch is persisted
//! before it returns and leaves the old pointer in place on failure, and delete never removes
//! the active or default profile. Each test passes a temp directory as the profiles path.

use std::fs;
use std::path::{Path, PathBuf};

use kattri_settings::profile::PROFILE_INDEX_FILE;
use kattri_settings::{ProfileError, ProfileStore, parse_index};

fn profiles_dir(root: &Path) -> PathBuf {
    root.join("profiles")
}

fn index_path(root: &Path) -> PathBuf {
    profiles_dir(root).join(PROFILE_INDEX_FILE)
}

fn open(root: &Path) -> ProfileStore {
    ProfileStore::open(profiles_dir(root)).unwrap()
}

fn entries_on_disk(root: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(profiles_dir(root))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// Write `bytes` as the only file in the profiles directory, as a damaged index would be.
/// A refused open must leave the directory exactly like this.
fn corrupt_index_fixture(root: &Path, bytes: &[u8]) {
    fs::create_dir_all(profiles_dir(root)).unwrap();
    fs::write(index_path(root), bytes).unwrap();
}

#[test]
fn malformed_toml_index_is_refused_and_not_overwritten() {
    let root = tempfile::tempdir().unwrap();
    let bytes = b"schema_version = 1\ndefault = \"default\"\nactive = \n";
    corrupt_index_fixture(root.path(), bytes);

    let err = ProfileStore::open(profiles_dir(root.path())).unwrap_err();
    assert!(matches!(err, ProfileError::CorruptIndex(_)), "{err:?}");
    assert_eq!(fs::read(index_path(root.path())).unwrap(), bytes);
    // No default profile is created next to a corrupt index.
    assert_eq!(entries_on_disk(root.path()), ["index.toml"]);
}

#[test]
fn non_utf8_index_is_refused_as_corrupt() {
    let root = tempfile::tempdir().unwrap();
    let bytes: &[u8] = &[0xff, 0xfe, 0x00, b's', b'c'];
    corrupt_index_fixture(root.path(), bytes);

    let err = ProfileStore::open(profiles_dir(root.path())).unwrap_err();
    assert!(matches!(err, ProfileError::CorruptIndex(_)), "{err:?}");
    assert_eq!(fs::read(index_path(root.path())).unwrap(), bytes);
}

#[test]
fn index_with_wrong_field_types_is_refused_as_corrupt() {
    let root = tempfile::tempdir().unwrap();
    let text =
        "schema_version = 1\ndefault = \"default\"\nactive = \"default\"\nprofiles = \"default\"\n";
    corrupt_index_fixture(root.path(), text.as_bytes());

    let err = ProfileStore::open(profiles_dir(root.path())).unwrap_err();
    assert!(matches!(err, ProfileError::CorruptIndex(_)), "{err:?}");
    assert_eq!(fs::read_to_string(index_path(root.path())).unwrap(), text);
}

#[test]
fn parse_index_reports_corrupt_text_as_typed_error() {
    assert!(matches!(
        parse_index("not = [toml").unwrap_err(),
        ProfileError::CorruptIndex(_)
    ));
}

#[test]
fn switch_is_on_disk_when_set_active_returns() {
    let root = tempfile::tempdir().unwrap();
    let mut store = open(root.path());
    store.create("kids").unwrap();
    store.set_active("kids").unwrap();

    let index = parse_index(&fs::read_to_string(index_path(root.path())).unwrap()).unwrap();
    assert_eq!(index.active, "kids");
    assert_eq!(index.default, "default");
    // The atomic write leaves no temporary file behind.
    assert_eq!(
        entries_on_disk(root.path()),
        ["default", "index.toml", "kids"]
    );
}

#[test]
fn failed_index_write_keeps_the_previous_active_profile() {
    let root = tempfile::tempdir().unwrap();
    let mut store = open(root.path());
    store.create("kids").unwrap();
    let before = fs::read(index_path(root.path())).unwrap();

    // A directory at the index path makes the final rename fail, which is a write failure
    // after the temporary file is written.
    fs::remove_file(index_path(root.path())).unwrap();
    fs::create_dir(index_path(root.path())).unwrap();
    let err = store.set_active("kids").unwrap_err();
    assert!(matches!(err, ProfileError::Io(_)), "{err:?}");
    assert_eq!(store.active_profile(), "default");
    assert!(
        entries_on_disk(root.path())
            .iter()
            .all(|name| !name.starts_with("index.tmp")),
        "temporary index file was left behind: {:?}",
        entries_on_disk(root.path())
    );

    // Restore the original index: a fresh open still sees the old pointer.
    fs::remove_dir(index_path(root.path())).unwrap();
    fs::write(index_path(root.path()), &before).unwrap();
    assert_eq!(open(root.path()).active_profile(), "default");
}

#[test]
fn switch_refuses_names_that_are_not_listed() {
    let root = tempfile::tempdir().unwrap();
    let mut store = open(root.path());
    store.create("kids").unwrap();
    store.set_active("kids").unwrap();
    let before = fs::read(index_path(root.path())).unwrap();

    for name in ["../default", "Kids", "a/b", "", "deleted"] {
        assert!(store.set_active(name).is_err(), "{name:?} was accepted");
    }
    assert_eq!(store.active_profile(), "kids");
    assert_eq!(fs::read(index_path(root.path())).unwrap(), before);
}

#[test]
fn switch_then_delete_the_old_profile_keeps_the_new_one_active() {
    let root = tempfile::tempdir().unwrap();
    let mut store = open(root.path());
    store.create("kids").unwrap();
    store.create("work").unwrap();
    store.set_active("work").unwrap();

    // The old active profile is no longer active, so it can be removed.
    store.delete("kids").unwrap();
    assert_eq!(store.names(), ["default", "work"]);
    assert_eq!(store.active_profile(), "work");

    // The active profile and the default profile cannot be removed.
    assert!(matches!(
        store.delete("work").unwrap_err(),
        ProfileError::ActiveProfile { .. }
    ));
    assert!(matches!(
        store.delete("default").unwrap_err(),
        ProfileError::DefaultProfile { .. }
    ));

    // The reopened store matches the in-memory one, and the removed folder was moved aside.
    let reopened = open(root.path());
    assert_eq!(reopened.names(), ["default", "work"]);
    assert_eq!(reopened.active_profile(), "work");
    let deleted: Vec<String> = fs::read_dir(profiles_dir(root.path()).join("deleted"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(deleted.len(), 1, "{deleted:?}");
    assert!(deleted[0].starts_with("kids-"), "{deleted:?}");
}
