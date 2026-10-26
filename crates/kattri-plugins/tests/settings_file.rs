//! Settings file tests: declared-key persistence, preservation of other content, corrupt file
//! handling and the save lock. Every test works in its own temporary directory.

use std::fs;
use std::path::Path;

use kattri_plugins::settings::PluginSettingsStore;
use kattri_plugins::settings_file::{SETTINGS_FILE_NAME, SettingsFile};
use kattri_plugins::{Permission, PermissionGrants, PluginError, PluginManifest};
use tempfile::TempDir;
use toml::Table;

fn manifest(id: &str, settings: &[&str]) -> PluginManifest {
    let keys = settings
        .iter()
        .map(|key| format!("\"{key}\""))
        .collect::<Vec<_>>()
        .join(",");
    let text = format!(
        r#"{{"id":"{id}","name":"Test","version":"0.1.0","api_version":1,
            "entry":"main.js","permissions":["settings.own"],"settings":[{keys}]}}"#
    );
    PluginManifest::from_json(&text).expect("test manifest is valid")
}

fn granted(manifest: &PluginManifest) -> PermissionGrants {
    let mut grants = PermissionGrants::for_plugin(manifest);
    grants.grant(Permission::SettingsOwn);
    grants
}

fn set(store: &mut PluginSettingsStore, manifest: &PluginManifest, key: &str, value: &str) {
    store
        .set(manifest, &granted(manifest), key, value)
        .expect("declared key with a plain value");
}

fn file_in(dir: &TempDir) -> SettingsFile {
    SettingsFile::new(dir.path().join(SETTINGS_FILE_NAME))
}

fn read_table(path: &Path) -> Table {
    fs::read_to_string(path)
        .expect("settings file is readable")
        .parse::<Table>()
        .expect("settings file is valid TOML")
}

#[test]
fn missing_file_loads_empty_and_creates_nothing() {
    let dir = TempDir::new().expect("temp dir");
    let file = file_in(&dir);
    let player = manifest("org.example.player", &["quality"]);

    let outcome = file.load(std::slice::from_ref(&player)).expect("load");

    assert!(outcome.store.table("org.example.player").is_none());
    assert_eq!(outcome.moved_aside, None);
    assert!(!file.path().exists(), "load never creates the file");
}

#[test]
fn save_then_load_round_trips_declared_values_only() {
    let dir = TempDir::new().expect("temp dir");
    let file = file_in(&dir);
    let player = manifest("org.example.player", &["quality", "theme"]);

    let mut store = PluginSettingsStore::new();
    set(&mut store, &player, "quality", "1080p");
    file.save(&store, std::slice::from_ref(&player))
        .expect("save");

    let on_disk = read_table(file.path());
    let plugin = on_disk["org.example.player"]
        .as_table()
        .expect("plugin table is written");
    assert_eq!(plugin["quality"].as_str(), Some("1080p"));
    assert!(
        !plugin.contains_key("theme"),
        "an unset declared key is not written"
    );

    let loaded = file.load(std::slice::from_ref(&player)).expect("load");
    assert_eq!(
        loaded.store.table("org.example.player"),
        store.table("org.example.player")
    );
}

#[test]
fn other_plugins_undeclared_keys_and_unknown_top_level_entries_survive_save() {
    let dir = TempDir::new().expect("temp dir");
    let file = file_in(&dir);
    fs::write(
        file.path(),
        r#"schema_note = "written by a newer build"

["org.example.retired"]
token_slot = "keep me"

["org.example.player"]
quality = "720p"
legacy_mode = "undeclared but kept"
"#,
    )
    .expect("seed file");

    let player = manifest("org.example.player", &["quality"]);
    let mut store = PluginSettingsStore::new();
    set(&mut store, &player, "quality", "1080p");
    file.save(&store, std::slice::from_ref(&player))
        .expect("save");

    let on_disk = read_table(file.path());
    assert_eq!(
        on_disk["schema_note"].as_str(),
        Some("written by a newer build")
    );
    assert_eq!(
        on_disk["org.example.retired"]["token_slot"].as_str(),
        Some("keep me")
    );
    let plugin = on_disk["org.example.player"]
        .as_table()
        .expect("plugin table");
    assert_eq!(plugin["quality"].as_str(), Some("1080p"));
    assert_eq!(
        plugin["legacy_mode"].as_str(),
        Some("undeclared but kept"),
        "a key the manifest does not declare is never removed"
    );
}

#[test]
fn clearing_a_declared_key_removes_it_from_the_file() {
    let dir = TempDir::new().expect("temp dir");
    let file = file_in(&dir);
    let player = manifest("org.example.player", &["quality"]);

    let mut store = PluginSettingsStore::new();
    set(&mut store, &player, "quality", "1080p");
    file.save(&store, std::slice::from_ref(&player))
        .expect("first save");

    store
        .remove(&player, &granted(&player), "quality")
        .expect("declared key");
    file.save(&store, std::slice::from_ref(&player))
        .expect("second save");

    let on_disk = read_table(file.path());
    assert!(
        !on_disk.contains_key("org.example.player"),
        "an emptied plugin table is removed"
    );
}

#[test]
fn corrupt_file_is_moved_aside_and_loading_continues() {
    let dir = TempDir::new().expect("temp dir");
    let file = file_in(&dir);
    let original = "[org.example.player\nquality = = 1\n";
    fs::write(file.path(), original).expect("seed corrupt file");
    let player = manifest("org.example.player", &["quality"]);

    let outcome = file
        .load(std::slice::from_ref(&player))
        .expect("load continues");

    assert!(outcome.store.table("org.example.player").is_none());
    let aside = outcome.moved_aside.expect("the corrupt file was moved");
    assert!(!file.path().exists(), "the original name is free again");
    assert_eq!(fs::read_to_string(&aside).expect("moved copy"), original);
    let name = aside
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    assert!(name.starts_with("plugins.toml.corrupt-"), "{name}");
}

#[test]
fn non_utf8_file_is_treated_as_corrupt() {
    let dir = TempDir::new().expect("temp dir");
    let file = file_in(&dir);
    fs::write(file.path(), [0xff_u8, 0xfe, 0x00, 0x41]).expect("seed binary file");

    let outcome = file.load(&[]).expect("load continues");

    assert!(outcome.moved_aside.is_some());
}

#[test]
fn stored_values_that_fail_validation_or_are_undeclared_are_not_loaded() {
    let dir = TempDir::new().expect("temp dir");
    let file = file_in(&dir);
    fs::write(
        file.path(),
        r#"[org.example.player]
quality = "https://media.example.org/stream?api_key=abc"
theme = "dark"
"#,
    )
    .expect("seed file");
    let player = manifest("org.example.player", &["quality"]);

    let outcome = file.load(std::slice::from_ref(&player)).expect("load");

    let loaded = outcome.store.table("org.example.player");
    assert!(
        loaded.is_none_or(|values| !values.contains_key("quality")),
        "a URL-looking value is not loaded"
    );
    assert!(
        loaded.is_none_or(|values| !values.contains_key("theme")),
        "an undeclared key is not loaded"
    );
}

#[test]
fn save_leaves_no_temporary_or_lock_file_behind() {
    let dir = TempDir::new().expect("temp dir");
    let file = file_in(&dir);
    let player = manifest("org.example.player", &["quality"]);
    let mut store = PluginSettingsStore::new();
    set(&mut store, &player, "quality", "1080p");

    file.save(&store, std::slice::from_ref(&player))
        .expect("save");

    let names: Vec<String> = fs::read_dir(dir.path())
        .expect("dir readable")
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    assert_eq!(names, vec![SETTINGS_FILE_NAME.to_owned()]);
}

#[test]
fn a_held_lock_refuses_the_save_and_leaves_the_file_unchanged() {
    let dir = TempDir::new().expect("temp dir");
    let file = file_in(&dir);
    let original = "[org.example.player]\nquality = \"480p\"\n";
    fs::write(file.path(), original).expect("seed file");
    let lock_path = dir.path().join("plugins.toml.lock");
    fs::write(&lock_path, "held").expect("simulate another writer");

    let player = manifest("org.example.player", &["quality"]);
    let mut store = PluginSettingsStore::new();
    set(&mut store, &player, "quality", "1080p");

    let err = file
        .save(&store, std::slice::from_ref(&player))
        .expect_err("a held lock must refuse the save");
    assert!(matches!(err, PluginError::Locked { .. }), "{err:?}");
    assert_eq!(fs::read_to_string(file.path()).expect("file"), original);
    assert!(
        lock_path.exists(),
        "a refused save does not remove another writer's lock"
    );

    fs::remove_file(&lock_path).expect("remove the simulated lock");
    file.save(&store, std::slice::from_ref(&player))
        .expect("save after the lock is released");
    assert!(!lock_path.exists(), "the save removes its own lock");
    assert_eq!(
        read_table(file.path())["org.example.player"]["quality"].as_str(),
        Some("1080p")
    );
}

#[test]
fn save_creates_a_missing_settings_directory() {
    let dir = TempDir::new().expect("temp dir");
    let nested = dir.path().join("config").join("kattri");
    let file = SettingsFile::new(nested.join(SETTINGS_FILE_NAME));
    let player = manifest("org.example.player", &["quality"]);
    let mut store = PluginSettingsStore::new();
    set(&mut store, &player, "quality", "1080p");

    file.save(&store, std::slice::from_ref(&player))
        .expect("save");

    assert!(nested.join(SETTINGS_FILE_NAME).exists());
}

#[test]
fn a_failed_write_before_rename_leaves_the_previous_file_intact() {
    let dir = TempDir::new().expect("temp dir");
    let file = file_in(&dir);
    let original = "[org.example.player]\nquality = \"480p\"\n";
    fs::write(file.path(), original).expect("seed file");

    // A directory where the temp file should go makes the write fail before the rename,
    // which is the interruption point the spec asks for.
    let temp_path = dir.path().join("plugins.toml.tmp");
    fs::create_dir(&temp_path).expect("block the temp path");

    let player = manifest("org.example.player", &["quality"]);
    let mut store = PluginSettingsStore::new();
    set(&mut store, &player, "quality", "1080p");

    let err = file
        .save(&store, std::slice::from_ref(&player))
        .expect_err("a blocked temp path must fail the save");
    assert!(matches!(err, PluginError::Storage { .. }), "{err:?}");
    assert_eq!(
        fs::read_to_string(file.path()).expect("file"),
        original,
        "the previous file must survive a failed write"
    );

    fs::remove_dir(&temp_path).expect("unblock the temp path");
    file.save(&store, std::slice::from_ref(&player))
        .expect("save after the temp path is free");
    assert_eq!(
        read_table(file.path())["org.example.player"]["quality"].as_str(),
        Some("1080p")
    );
}
