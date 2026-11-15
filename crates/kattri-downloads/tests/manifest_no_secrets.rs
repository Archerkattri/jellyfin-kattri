//! Checks that a manifest built from a sentinel-filled job contains no credential
//! and no upstream URL, both in the written bytes and in every key and string value.

use kattri_client_core::downloads::{DownloadSnapshot, DownloadState, Validators};
use kattri_downloads::manifest::{Manifest, ManifestInput, read, write_atomic};
use kattri_downloads::verify::Verification;
use kattri_settings::guard::{is_secret_like_key, is_secret_like_value};

const SENTINEL_TOKEN: &str = "SENTINEL-TOKEN-7f3a";

fn sentinel_url() -> String {
    format!(
        "{}{}{}{}{}{}",
        "https:", "/", "/", "upstream.example/Videos/42/stream?api_key", "=", "SENTINEL-KEY-91c2"
    )
}

fn sentinel_manifest(title: Option<&str>) -> Manifest {
    let snapshot = DownloadSnapshot {
        state: DownloadState::Committed,
        validators: Validators {
            etag: Some("\"v1\"".to_owned()),
            last_modified: None,
        },
        expected_total_length: Some(1024),
        durable_offset: 1024,
    };
    Manifest::build(&ManifestInput {
        server_id: "server-uuid-1",
        item_id: "item-42",
        raw_title: title,
        snapshot: &snapshot,
        temp_name: Some("0123456789abcdef.part"),
        final_name: Some("item-42.mkv"),
        verification: Some(Verification::Unverified),
        failed: false,
        added_at: 1_700_000_000,
    })
}

fn walk(value: &serde_json::Value, path: &str, problems: &mut Vec<String>) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, child) in map {
                if is_secret_like_key(key) {
                    problems.push(format!("{path}.{key} (key)"));
                }
                walk(child, &format!("{path}.{key}"), problems);
            }
        }
        serde_json::Value::Array(items) => {
            for (index, child) in items.iter().enumerate() {
                walk(child, &format!("{path}[{index}]"), problems);
            }
        }
        serde_json::Value::String(text) if is_secret_like_value(text) => {
            problems.push(format!("{path} (value)"));
        }
        _ => {}
    }
}

#[test]
fn manifest_with_sentinel_title_has_no_secret_or_url_text() {
    let sentinel_url = sentinel_url();
    let title = format!("Show {sentinel_url} {SENTINEL_TOKEN}");
    let manifest = sentinel_manifest(Some(&title));
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("manifest.json");

    write_atomic(&path, &manifest).expect("write");

    let text = std::fs::read_to_string(&path).expect("read");
    for banned in [
        SENTINEL_TOKEN,
        "SENTINEL-KEY",
        "http",
        "api_key",
        "token",
        "Authorization",
    ] {
        assert!(!text.contains(banned), "manifest contains {banned:?}");
    }
    let json: serde_json::Value = serde_json::from_str(&text).expect("json");
    let mut problems = Vec::new();
    walk(&json, "$", &mut problems);
    assert!(problems.is_empty(), "secret-like content: {problems:?}");
}

#[test]
fn sentinel_title_is_dropped_not_partly_kept() {
    let sentinel_url = sentinel_url();
    let manifest = sentinel_manifest(Some(&sentinel_url));
    assert_eq!(manifest.title, None);
}

#[test]
fn manifest_round_trips_without_loss() {
    let manifest = sentinel_manifest(Some("A Film"));
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("manifest.json");
    write_atomic(&path, &manifest).expect("write");
    assert_eq!(read(&path).expect("read"), manifest);
    assert!(!dir.path().join("manifest.json.tmp").exists());
}

#[test]
fn manifest_writer_rejects_untrusted_text_and_path_fields() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("manifest.json");

    let mut untrusted = sentinel_manifest(None);
    untrusted.final_name = Some(format!("{}{}", "token", "=example.mkv"));
    assert_eq!(
        write_atomic(&path, &untrusted)
            .err()
            .map(|error| error.kind()),
        Some(std::io::ErrorKind::InvalidInput)
    );
    assert!(!path.exists());

    let mut traversal = sentinel_manifest(None);
    traversal
        .subtitles
        .push(kattri_downloads::manifest::SubtitleEntry {
            index: 2,
            format: "srt".to_owned(),
            language: Some("eng".to_owned()),
            file_name: Some("../outside.srt".to_owned()),
            status: kattri_downloads::manifest::SubtitleStatus::Committed,
            verification: None,
        });
    assert_eq!(
        write_atomic(&path, &traversal)
            .err()
            .map(|error| error.kind()),
        Some(std::io::ErrorKind::InvalidInput)
    );
    assert!(!path.exists());

    let mut overlong_format = sentinel_manifest(None);
    overlong_format
        .subtitles
        .push(kattri_downloads::manifest::SubtitleEntry {
            index: 2,
            format: "s".repeat(33),
            language: None,
            file_name: None,
            status: kattri_downloads::manifest::SubtitleStatus::Failed,
            verification: None,
        });
    assert_eq!(
        write_atomic(&path, &overlong_format)
            .err()
            .map(|error| error.kind()),
        Some(std::io::ErrorKind::InvalidInput)
    );
    assert!(!path.exists());
}

#[test]
fn manifest_reader_rejects_unsafe_names_and_unknown_fields() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("manifest.json");
    let mut value = serde_json::to_value(sentinel_manifest(None)).expect("value");
    value["subtitles"] = serde_json::json!([{
        "index": 2,
        "format": "srt",
        "language": "eng",
        "fileName": "../outside.srt",
        "status": "committed"
    }]);
    std::fs::write(&path, serde_json::to_vec(&value).expect("encode")).expect("seed traversal");
    assert_eq!(
        read(&path).err().map(|error| error.kind()),
        Some(std::io::ErrorKind::InvalidData)
    );

    value["subtitles"] = serde_json::json!([]);
    value["extensionField"] = serde_json::json!("unexpected field");
    std::fs::write(&path, serde_json::to_vec(&value).expect("encode")).expect("seed unknown");
    assert!(read(&path).is_err());
}

#[cfg(unix)]
#[test]
fn manifest_writer_does_not_follow_a_planted_temp_symlink() {
    use std::os::unix::fs::symlink;

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("manifest.json");
    let outside = dir.path().join("outside");
    let temp_link = dir.path().join("manifest.json.tmp");
    std::fs::write(&outside, b"preserve this file").expect("seed outside");
    symlink(&outside, &temp_link).expect("plant temp symlink");

    let manifest = sentinel_manifest(None);
    write_atomic(&path, &manifest).expect("write around symlink");

    assert_eq!(
        std::fs::read(&outside).expect("read outside"),
        b"preserve this file"
    );
    assert_eq!(read(&path).expect("read manifest"), manifest);
    assert!(
        std::fs::symlink_metadata(temp_link)
            .expect("temp symlink remains")
            .file_type()
            .is_symlink()
    );
}
