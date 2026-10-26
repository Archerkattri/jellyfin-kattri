use std::path::Path;

use crate::memory::*;
use crate::*;

fn key(account: &str) -> CredentialKey {
    CredentialKey {
        service: "jellyfin-kattri".to_owned(),
        account: account.to_owned(),
    }
}

#[test]
fn secret_debug_output_is_redacted() {
    let s = Secret::new(b"token-value".to_vec());
    let shown = format!("{s:?}");
    assert_eq!(shown, "Secret(<redacted>)");
    assert!(!shown.contains("token"));
    assert_eq!(s.expose(), b"token-value");
}

#[test]
fn credential_store_set_get_overwrite_delete() {
    let mut store = MemoryCredentialStore::new();
    let k = key("server-1/user-1");
    assert_eq!(store.get(&k).unwrap(), None);

    store.set(&k, Secret::new(b"first".to_vec())).unwrap();
    assert_eq!(store.get(&k).unwrap().unwrap().expose(), b"first");

    store.set(&k, Secret::new(b"second".to_vec())).unwrap();
    assert_eq!(store.get(&k).unwrap().unwrap().expose(), b"second");

    assert!(store.delete(&k).unwrap());
    assert!(!store.delete(&k).unwrap());
    assert_eq!(store.get(&k).unwrap(), None);
}

#[test]
fn credential_keys_are_isolated_per_account() {
    let mut store = MemoryCredentialStore::new();
    store.set(&key("a"), Secret::new(vec![1])).unwrap();
    assert_eq!(store.get(&key("b")).unwrap(), None);
}

#[test]
fn app_dirs_are_distinct_and_under_root() {
    let dirs = MemoryAppDirs::new("/tmp/kattri-test");
    let all = [
        dirs.config_dir(),
        dirs.data_dir(),
        dirs.cache_dir(),
        dirs.log_dir(),
        dirs.pictures_dir().unwrap(),
    ];
    for p in &all {
        assert!(p.starts_with(dirs.root()), "{p:?} is outside root");
    }
    for (i, a) in all.iter().enumerate() {
        for b in &all[i + 1..] {
            assert_ne!(a, b);
        }
    }
    assert_eq!(dirs.root(), Path::new("/tmp/kattri-test"));
}

#[test]
fn media_keys_drain_in_order_and_publish_now_playing() {
    let mut keys = MemoryMediaKeys::new();
    keys.push_key(MediaKey::PlayPause);
    keys.push_key(MediaKey::Next);
    assert_eq!(
        keys.poll().unwrap(),
        vec![MediaKey::PlayPause, MediaKey::Next]
    );
    assert!(keys.poll().unwrap().is_empty());

    let info = NowPlaying {
        title: "Episode 1".to_owned(),
        playing: true,
        duration_secs: 1320,
        ..NowPlaying::default()
    };
    keys.publish_now_playing(Some(info.clone())).unwrap();
    assert_eq!(keys.now_playing(), Some(&info));
    keys.publish_now_playing(None).unwrap();
    assert_eq!(keys.now_playing(), None);
}

#[test]
fn power_guard_releases_on_drop_and_counts_nest() {
    let mut power = MemoryPowerInhibitor::new();
    let first = power.inhibit_sleep("Playing video").unwrap();
    let second = power.inhibit_sleep("Playing audio").unwrap();
    assert_eq!(power.active_count(), 2);

    drop(first);
    assert_eq!(power.active_count(), 1);
    drop(second);
    assert_eq!(power.active_count(), 0);
    assert_eq!(power.reasons(), ["Playing video", "Playing audio"]);
}

#[test]
fn window_apply_and_state_round_trip() {
    let mut window = MemoryWindow::new(WindowState::default());
    let target = WindowState {
        x: 100,
        y: 50,
        width: 1280,
        height: 720,
        monitor: Some("DISPLAY1".to_owned()),
        ..WindowState::default()
    };
    window.apply(&target).unwrap();
    assert_eq!(window.state().unwrap(), target);
}

#[test]
fn picture_in_picture_implies_always_on_top() {
    let mut window = MemoryWindow::new(WindowState::default());
    window.set_picture_in_picture(true).unwrap();
    let s = window.state().unwrap();
    assert!(s.picture_in_picture && s.always_on_top);

    window.set_picture_in_picture(false).unwrap();
    assert!(!window.state().unwrap().picture_in_picture);
    // Leaving PiP does not silently clear always-on-top: that is the user's own setting.
    assert!(window.state().unwrap().always_on_top);
}

#[test]
fn notifier_records_in_order() {
    let mut notifier = RecordingNotifier::new();
    notifier
        .notify(Notification {
            title: "Download complete".to_owned(),
            body: "Movie.mkv".to_owned(),
        })
        .unwrap();
    assert_eq!(notifier.sent().len(), 1);
    assert_eq!(notifier.sent()[0].title, "Download complete");
}

#[test]
fn unsupported_error_names_the_service() {
    let err = PlatformError::Unsupported("keychain");
    assert_eq!(
        err.to_string(),
        "keychain is not supported on this platform"
    );
}

#[test]
fn memory_disk_space_reports_configured_amount() {
    use crate::DiskSpace;
    let space = MemoryDiskSpace::new(4096);
    assert_eq!(
        space.available_bytes(Path::new("anywhere")).expect("space"),
        4096
    );
}
