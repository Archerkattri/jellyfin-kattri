//! M2-13 profile model and storage. Every test passes a temp directory as the profiles path.

use std::fs;
use std::path::Path;

use kattri_settings::{
    DEFAULT_PROFILE_NAME, PROFILE_INDEX_VERSION, ProfileError, ProfileIndex, ProfileStore,
    Settings, SettingsError, parse_index, to_index_string, validate_profile_name,
};

fn open_in(root: &Path) -> ProfileStore {
    ProfileStore::open(root.join("profiles")).unwrap()
}

fn names_on_disk(profiles: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(profiles)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[test]
fn first_run_creates_default_profile_index_and_settings() {
    let root = tempfile::tempdir().unwrap();
    let store = open_in(root.path());
    assert_eq!(store.names(), [DEFAULT_PROFILE_NAME.to_owned()]);
    assert_eq!(store.default_profile(), "default");
    assert_eq!(store.active_profile(), "default");
    let profiles = root.path().join("profiles");
    assert_eq!(
        names_on_disk(&profiles),
        vec!["default".to_owned(), "index.toml".to_owned()]
    );
    assert!(profiles.join("default").join("settings.toml").is_file());
}

#[test]
fn create_adds_profile_with_default_settings() {
    let root = tempfile::tempdir().unwrap();
    let mut store = open_in(root.path());
    store.create("kids").unwrap();
    assert_eq!(store.names(), ["default", "kids"]);
    let (settings, clamped) = store.load_settings("kids").unwrap();
    assert_eq!(settings, Settings::default());
    assert!(clamped.is_empty());
    assert!(store.settings_path("kids").unwrap().is_file());
}

#[test]
fn create_refuses_duplicate_and_existing_folder() {
    let root = tempfile::tempdir().unwrap();
    let mut store = open_in(root.path());
    store.create("kids").unwrap();
    assert!(matches!(
        store.create("kids").unwrap_err(),
        ProfileError::Duplicate { .. }
    ));
    // A folder with no index entry is not overwritten.
    fs::create_dir_all(root.path().join("profiles").join("work")).unwrap();
    assert!(matches!(
        store.create("work").unwrap_err(),
        ProfileError::FolderExists { .. }
    ));
    assert_eq!(store.names(), ["default", "kids"]);
}

#[test]
fn name_rules_table() {
    let long = "a".repeat(33);
    let max = "a".repeat(32);
    let rejected = [
        "",
        long.as_str(),
        "a/b",
        "a\\b",
        "..",
        "../x",
        "Kids",
        "my kid",
        "kid_1",
        "kid.one",
        "con",
        "NUL",
        "prn",
        "aux",
        "com1",
        "lpt9",
        "com0",
        "deleted",
    ];
    for name in rejected {
        assert!(
            validate_profile_name(name).is_err(),
            "{name:?} should be rejected"
        );
    }
    let accepted = [
        "default",
        "kids",
        "a",
        "kid-2",
        "com10",
        "conx",
        "lpt",
        max.as_str(),
    ];
    for name in accepted {
        assert!(
            validate_profile_name(name).is_ok(),
            "{name:?} should be accepted"
        );
    }
}

#[test]
fn path_separators_are_reported_as_such() {
    let err = validate_profile_name("a/b").unwrap_err();
    assert!(matches!(
        err,
        ProfileError::InvalidName {
            reason: "must not contain a path separator",
            ..
        }
    ));
}

#[test]
fn create_refuses_invalid_name_and_writes_nothing() {
    let root = tempfile::tempdir().unwrap();
    let mut store = open_in(root.path());
    for name in ["con", "../escape", "a\\b", "Work"] {
        assert!(matches!(
            store.create(name).unwrap_err(),
            ProfileError::InvalidName { .. }
        ));
    }
    assert_eq!(store.names(), ["default"]);
    assert_eq!(
        names_on_disk(&root.path().join("profiles")),
        vec!["default".to_owned(), "index.toml".to_owned()]
    );
}

#[test]
fn profile_paths_refuse_unlisted_and_traversal_names() {
    let root = tempfile::tempdir().unwrap();
    let store = open_in(root.path());
    assert!(matches!(
        store.settings_path("../default").unwrap_err(),
        ProfileError::InvalidName { .. }
    ));
    assert!(matches!(
        store.profile_dir("kids").unwrap_err(),
        ProfileError::NotFound { .. }
    ));
}

#[test]
fn rename_moves_folder_and_keeps_settings() {
    let root = tempfile::tempdir().unwrap();
    let mut store = open_in(root.path());
    store.create("kids").unwrap();
    store.rename("kids", "family").unwrap();
    assert_eq!(store.names(), ["default", "family"]);
    let profiles = root.path().join("profiles");
    assert!(profiles.join("family").join("settings.toml").is_file());
    assert!(!profiles.join("kids").exists());
}

#[test]
fn rename_default_moves_the_default_pointer() {
    let root = tempfile::tempdir().unwrap();
    let mut store = open_in(root.path());
    store.rename("default", "main").unwrap();
    assert_eq!(store.default_profile(), "main");
    assert_eq!(store.active_profile(), "main");
    assert_eq!(store.names(), ["main"]);
}

#[test]
fn rename_refuses_duplicate_reserved_and_missing_names() {
    let root = tempfile::tempdir().unwrap();
    let mut store = open_in(root.path());
    store.create("kids").unwrap();
    store.create("work").unwrap();
    assert!(matches!(
        store.rename("kids", "work").unwrap_err(),
        ProfileError::Duplicate { .. }
    ));
    assert!(matches!(
        store.rename("kids", "nul").unwrap_err(),
        ProfileError::InvalidName { .. }
    ));
    assert!(matches!(
        store.rename("missing", "other").unwrap_err(),
        ProfileError::NotFound { .. }
    ));
    assert_eq!(store.names(), ["default", "kids", "work"]);
    // Renaming to the same name is a no-op.
    store.rename("kids", "kids").unwrap();
    assert_eq!(store.names(), ["default", "kids", "work"]);
}

#[test]
fn delete_last_profile_is_refused() {
    let root = tempfile::tempdir().unwrap();
    let mut store = open_in(root.path());
    assert!(matches!(
        store.delete("default").unwrap_err(),
        ProfileError::LastProfile
    ));
    assert_eq!(store.names(), ["default"]);
}

#[test]
fn delete_default_profile_is_refused_even_when_not_active() {
    let root = tempfile::tempdir().unwrap();
    let mut store = open_in(root.path());
    store.create("kids").unwrap();
    assert!(matches!(
        store.delete("default").unwrap_err(),
        ProfileError::DefaultProfile { .. }
    ));
    assert_eq!(store.names(), ["default", "kids"]);
}

#[test]
fn delete_active_profile_is_refused_until_switched() {
    let root = tempfile::tempdir().unwrap();
    let mut store = open_in(root.path());
    store.create("kids").unwrap();
    store.set_active("kids").unwrap();
    assert!(matches!(
        store.delete("kids").unwrap_err(),
        ProfileError::ActiveProfile { .. }
    ));
    store.set_active("default").unwrap();
    store.delete("kids").unwrap();
    assert_eq!(store.names(), ["default"]);
}

#[test]
fn delete_moves_folder_into_deleted_and_never_hard_deletes() {
    let root = tempfile::tempdir().unwrap();
    let mut store = open_in(root.path());
    store.create("kids").unwrap();
    store.delete("kids").unwrap();
    let profiles = root.path().join("profiles");
    assert!(!profiles.join("kids").exists());
    let kept: Vec<String> = fs::read_dir(profiles.join("deleted"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(kept.len(), 1);
    assert!(kept[0].starts_with("kids-"), "got {kept:?}");
    assert!(
        profiles
            .join("deleted")
            .join(&kept[0])
            .join("settings.toml")
            .is_file()
    );
}

#[test]
fn deleting_a_profile_whose_folder_is_missing_only_updates_the_index() {
    let root = tempfile::tempdir().unwrap();
    let mut store = open_in(root.path());
    store.create("kids").unwrap();
    fs::remove_dir_all(root.path().join("profiles").join("kids")).unwrap();
    store.delete("kids").unwrap();
    assert_eq!(store.names(), ["default"]);
}

#[test]
fn set_active_persists_across_reopen() {
    let root = tempfile::tempdir().unwrap();
    {
        let mut store = open_in(root.path());
        store.create("kids").unwrap();
        store.set_active("kids").unwrap();
        assert_eq!(store.active_profile(), "kids");
    }
    let reopened = open_in(root.path());
    assert_eq!(reopened.active_profile(), "kids");
    assert_eq!(reopened.default_profile(), "default");
    assert_eq!(reopened.names(), ["default", "kids"]);
}

#[test]
fn set_active_refuses_unknown_profile() {
    let root = tempfile::tempdir().unwrap();
    let mut store = open_in(root.path());
    assert!(matches!(
        store.set_active("ghost").unwrap_err(),
        ProfileError::NotFound { .. }
    ));
    assert_eq!(store.active_profile(), "default");
}

#[test]
fn each_profile_keeps_its_own_settings_after_reopen() {
    let root = tempfile::tempdir().unwrap();
    {
        let mut store = open_in(root.path());
        store.create("kids").unwrap();
        let mut kids = Settings::default();
        kids.audio.max_volume_percent = 150;
        store.save_settings("kids", &kids).unwrap();
        let mut main = Settings::default();
        main.playback.default_speed = 1.25;
        store.save_settings("default", &main).unwrap();
    }
    let store = open_in(root.path());
    assert_eq!(
        store
            .load_settings("kids")
            .unwrap()
            .0
            .audio
            .max_volume_percent,
        150
    );
    assert_eq!(
        store
            .load_settings("default")
            .unwrap()
            .0
            .playback
            .default_speed,
        1.25
    );
}

#[test]
fn index_round_trips_through_toml() {
    let index = ProfileIndex {
        schema_version: PROFILE_INDEX_VERSION,
        default: "default".to_owned(),
        active: "kids".to_owned(),
        profiles: vec!["default".to_owned(), "kids".to_owned(), "work-2".to_owned()],
    };
    let text = to_index_string(&index).unwrap();
    assert_eq!(parse_index(&text).unwrap(), index);
}

#[test]
fn index_file_on_disk_is_the_versioned_format() {
    let root = tempfile::tempdir().unwrap();
    let mut store = open_in(root.path());
    store.create("kids").unwrap();
    store.set_active("kids").unwrap();
    let text = fs::read_to_string(root.path().join("profiles").join("index.toml")).unwrap();
    let index = parse_index(&text).unwrap();
    assert_eq!(index.schema_version, 1);
    assert_eq!(index.active, "kids");
    assert_eq!(index.profiles, ["default", "kids"]);
}

#[test]
fn atomic_index_write_leaves_no_temp_file() {
    let root = tempfile::tempdir().unwrap();
    let mut store = open_in(root.path());
    store.create("kids").unwrap();
    store.set_active("kids").unwrap();
    let names = names_on_disk(&root.path().join("profiles"));
    assert!(
        names.iter().all(|n| !n.contains("tmp")),
        "temp file left behind: {names:?}"
    );
}

#[test]
fn unknown_index_version_is_refused_and_file_is_not_rewritten() {
    for version in ["2", "0", "99"] {
        let root = tempfile::tempdir().unwrap();
        let profiles = root.path().join("profiles");
        fs::create_dir_all(&profiles).unwrap();
        let text = format!(
            "schema_version = {version}\ndefault = \"default\"\nactive = \"default\"\nprofiles = [\"default\"]\n"
        );
        fs::write(profiles.join("index.toml"), &text).unwrap();
        let err = ProfileStore::open(&profiles).unwrap_err();
        assert!(
            matches!(
                err,
                ProfileError::UnsupportedIndexVersion { supported: 1, .. }
            ),
            "version {version}: {err:?}"
        );
        assert_eq!(
            fs::read_to_string(profiles.join("index.toml")).unwrap(),
            text
        );
    }
}

#[test]
fn index_without_version_is_refused() {
    let text = "default = \"default\"\nactive = \"default\"\nprofiles = [\"default\"]\n";
    assert!(matches!(
        parse_index(text).unwrap_err(),
        ProfileError::InvalidIndex { .. }
    ));
}

#[test]
fn structurally_wrong_index_is_refused() {
    let cases = [
        // default not listed
        "schema_version = 1\ndefault = \"x\"\nactive = \"a\"\nprofiles = [\"a\"]\n",
        // active not listed
        "schema_version = 1\ndefault = \"a\"\nactive = \"x\"\nprofiles = [\"a\"]\n",
        // empty list
        "schema_version = 1\ndefault = \"a\"\nactive = \"a\"\nprofiles = []\n",
        // duplicate name
        "schema_version = 1\ndefault = \"a\"\nactive = \"a\"\nprofiles = [\"a\", \"a\"]\n",
        // reserved name in the index
        "schema_version = 1\ndefault = \"nul\"\nactive = \"nul\"\nprofiles = [\"nul\"]\n",
        // path separator in the index
        "schema_version = 1\ndefault = \"a\"\nactive = \"a\"\nprofiles = [\"a\", \"../b\"]\n",
    ];
    for text in cases {
        assert!(parse_index(text).is_err(), "accepted: {text}");
    }
}

#[test]
fn credential_like_key_in_profile_settings_is_refused() {
    let root = tempfile::tempdir().unwrap();
    let mut store = open_in(root.path());
    store.create("kids").unwrap();
    let path = store.settings_path("kids").unwrap();
    fs::write(&path, "[playback]\naccess_token = \"not-a-real-value\"\n").unwrap();

    assert!(matches!(
        store.load_settings("kids").unwrap_err(),
        ProfileError::Settings(SettingsError::SecretLikeKey { .. })
    ));
    // Switching into the profile is refused, and the pointer does not move.
    assert!(matches!(
        store.set_active("kids").unwrap_err(),
        ProfileError::Settings(SettingsError::SecretLikeKey { .. })
    ));
    assert_eq!(store.active_profile(), "default");
    // The refused file is left exactly as it was.
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        "[playback]\naccess_token = \"not-a-real-value\"\n"
    );
}

#[test]
fn credential_like_key_in_default_profile_settings_is_refused() {
    let root = tempfile::tempdir().unwrap();
    let store = open_in(root.path());
    fs::write(
        store.settings_path("default").unwrap(),
        "password = \"not-a-real-value\"\n",
    )
    .unwrap();
    assert!(matches!(
        store.load_settings("default").unwrap_err(),
        ProfileError::Settings(SettingsError::SecretLikeKey { .. })
    ));
}

#[test]
fn saving_profile_settings_that_fail_validation_writes_nothing() {
    let root = tempfile::tempdir().unwrap();
    let store = open_in(root.path());
    let path = store.settings_path("default").unwrap();
    let before = fs::read_to_string(&path).unwrap();
    let mut bad = Settings::default();
    bad.audio.max_volume_percent = 301;
    assert!(store.save_settings("default", &bad).is_err());
    assert_eq!(fs::read_to_string(&path).unwrap(), before);
}
