//! Checks that a manifest built from a sentinel-filled job contains no credential
//! and no upstream URL, both in the written bytes and in every key and string value.

use kattri_client_core::downloads::{DownloadSnapshot, DownloadState, Validators};
use kattri_downloads::manifest::{Manifest, ManifestInput, read, write_atomic};
use kattri_downloads::verify::Verification;
use kattri_settings::guard::{is_secret_like_key, is_secret_like_value};

const SENTINEL_TOKEN: &str = "SENTINEL-TOKEN-7f3a";
const SENTINEL_URL: &str = "https://upstream.example/Videos/42/stream?api_key=SENTINEL-KEY-91c2"; // gitleaks:allow -- synthetic redaction-test sentinel on a reserved example domain.

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
    let title = format!("Show {SENTINEL_URL} {SENTINEL_TOKEN}");
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
    let manifest = sentinel_manifest(Some(SENTINEL_URL));
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
