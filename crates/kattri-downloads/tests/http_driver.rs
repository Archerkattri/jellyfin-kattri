//! Driver tests against an in-test HTTP/1.1 server on 127.0.0.1. No real Jellyfin server.
//!
//! The server records each request (Range and Authorization) and answers from a
//! scripted handler. A `cut_after` reply sends a prefix of the body and then closes the
//! socket, which the client sees as an interrupted transfer.

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, atomic::Ordering};
use std::time::Duration;

use kattri_client_core::downloads::{DownloadState, DownloadTransitionError};
use kattri_downloads::driver::{
    DriverConfig, FailureReason, OriginalFileSource, Outcome, RequestAuth, Transfer,
};
use kattri_downloads::fs::{TempStore, required_free_bytes};
use kattri_downloads::messages::Notice;
use kattri_downloads::persist::{self, PersistedState};
use kattri_platform::memory::MemoryDiskSpace;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const TOTAL: usize = 1024 * 1024;
const HALF: usize = TOTAL / 2;
/// 600 KiB sent before the cut; two full 256 KiB flushes (512 KiB) are durable.
const CUT_AFTER: usize = 600 * 1024;

fn content(total: usize) -> Vec<u8> {
    (0..total).map(|i| (i % 251) as u8).collect()
}

#[derive(Clone, Debug)]
struct Recorded {
    path: String,
    range: Option<String>,
    authorization: Option<String>,
    emby_token: Option<String>,
}

struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    cut_after: Option<usize>,
    /// Keep the socket open after the prefix (simulates a process killed mid-body).
    stall: bool,
}

type Handler = Arc<dyn Fn(usize, &Recorded) -> Reply + Send + Sync>;

struct TestServer {
    base: String,
    log: Arc<Mutex<Vec<Recorded>>>,
}

impl TestServer {
    fn requests(&self) -> Vec<Recorded> {
        self.log.lock().expect("log").clone()
    }
}

async fn start(handler: Handler) -> TestServer {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let log: Arc<Mutex<Vec<Recorded>>> = Arc::new(Mutex::new(Vec::new()));
    let log_for_loop = Arc::clone(&log);
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let handler = Arc::clone(&handler);
            let log = Arc::clone(&log_for_loop);
            tokio::spawn(async move {
                let head = read_head(&mut socket).await;
                let recorded = parse_head(&head);
                let index = {
                    let mut entries = log.lock().expect("log");
                    entries.push(recorded.clone());
                    entries.len() - 1
                };
                let reply = handler(index, &recorded);
                if recorded.path == "/stall-headers" {
                    tokio::time::sleep(Duration::from_secs(60)).await;
                    return;
                }
                let mut out = format!(
                    "HTTP/1.1 {} Scripted\r\nContent-Length: {}\r\nConnection: close\r\n",
                    reply.status,
                    reply.body.len()
                );
                for (name, value) in &reply.headers {
                    out.push_str(&format!("{name}: {value}\r\n"));
                }
                out.push_str("\r\n");
                let _ = socket.write_all(out.as_bytes()).await;
                let end = reply
                    .cut_after
                    .unwrap_or(reply.body.len())
                    .min(reply.body.len());
                let _ = socket.write_all(&reply.body[..end]).await;
                if reply.stall {
                    tokio::time::sleep(Duration::from_secs(60)).await;
                }
                let _ = socket.shutdown().await;
            });
        }
    });
    TestServer {
        base: format!("http://{addr}"),
        log,
    }
}

async fn read_head(socket: &mut tokio::net::TcpStream) -> String {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let n = socket.read(&mut chunk).await.unwrap_or(0);
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }
    String::from_utf8_lossy(&buf).into_owned()
}

fn parse_head(head: &str) -> Recorded {
    let mut range = None;
    let mut authorization = None;
    let mut emby_token = None;
    let path = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("")
        .to_owned();
    for line in head.lines().skip(1) {
        if let Some((name, value)) = line.split_once(':') {
            let value = value.trim().to_owned();
            match name.trim().to_ascii_lowercase().as_str() {
                "range" => range = Some(value),
                "authorization" => authorization = Some(value),
                "x-emby-token" => emby_token = Some(value),
                _ => {}
            }
        }
    }
    Recorded {
        path,
        range,
        authorization,
        emby_token,
    }
}

fn full(body: &[u8], etag: &str) -> Reply {
    Reply {
        status: 200,
        headers: vec![("ETag".into(), etag.into())],
        body: body.to_vec(),
        cut_after: None,
        stall: false,
    }
}

fn cut_full(body: &[u8], etag: &str, cut_after: usize) -> Reply {
    Reply {
        cut_after: Some(cut_after),
        stall: false,
        ..full(body, etag)
    }
}

fn partial(body: &[u8], start: usize, total: usize, etag: &str) -> Reply {
    Reply {
        status: 206,
        headers: vec![
            ("ETag".into(), etag.into()),
            (
                "Content-Range".into(),
                format!("bytes {start}-{}/{total}", total - 1),
            ),
        ],
        body: body[start..].to_vec(),
        cut_after: None,
        stall: false,
    }
}

/// Sends `after` bytes and then stops responding, so the transfer can be dropped mid-body.
fn stalled(body: &[u8], etag: &str, after: usize) -> Reply {
    Reply {
        stall: true,
        ..cut_full(body, etag, after)
    }
}

fn range_start(recorded: &Recorded) -> usize {
    recorded
        .range
        .as_deref()
        .and_then(|r| r.strip_prefix("bytes="))
        .and_then(|r| r.strip_suffix('-'))
        .and_then(|r| r.parse().ok())
        .expect("range header")
}

struct Harness {
    _dir: tempfile::TempDir,
    cfg: DriverConfig,
    store: TempStore,
}

fn harness(backoff: Duration) -> Harness {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = TempStore::new(dir.path().join("tmp"));
    let mut cfg = DriverConfig::new(store.clone()).expect("driver config");
    cfg.retry_backoff = backoff;
    Harness {
        _dir: dir,
        cfg,
        store,
    }
}

/// Config whose snapshot and manifest paths sit beside the temp folder. The manifest is
/// pre-created, as the app would write it, so a removal is observable.
fn cfg_with_records(h: &Harness) -> (DriverConfig, PathBuf, PathBuf) {
    let root = h
        .store
        .dir()
        .parent()
        .expect("temp folder has a parent")
        .to_path_buf();
    let snapshot = root.join("snapshot.json");
    let manifest = root.join("manifest.json");
    fs::write(&manifest, b"{\"schemaVersion\":1}").expect("seed manifest");
    let mut cfg = h.cfg.clone();
    cfg.snapshot_path = Some(snapshot.clone());
    cfg.manifest_path = Some(manifest.clone());
    (cfg, snapshot, manifest)
}

fn final_bytes(h: &Harness, transfer: &Transfer) -> Vec<u8> {
    let name = transfer.temp_name().expect("temp name");
    fs::read(h.store.dir().join(name)).expect("read temp")
}

fn tmp_files(h: &Harness) -> Vec<PathBuf> {
    match fs::read_dir(h.store.dir()) {
        Ok(entries) => entries.map(|e| e.expect("entry").path()).collect(),
        Err(_) => Vec::new(),
    }
}

async fn wait_for_temp_file(store: &TempStore, minimum_length: u64) {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let has_durable_bytes = fs::read_dir(store.dir())
                .ok()
                .into_iter()
                .flatten()
                .filter_map(Result::ok)
                .filter_map(|entry| entry.metadata().ok())
                .any(|metadata| metadata.len() >= minimum_length);
            if has_durable_bytes {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("download flushed the requested bytes");
}

#[tokio::test]
async fn resume_at_half_uses_range_from_flushed_offset() {
    let body = content(TOTAL);
    let scripted = body.clone();
    let server = start(Arc::new(move |index, recorded: &Recorded| {
        if index == 0 {
            cut_full(&scripted, "\"v1\"", CUT_AFTER)
        } else {
            partial(&scripted, range_start(recorded), TOTAL, "\"v1\"")
        }
    }))
    .await;
    let h = harness(Duration::from_millis(5));
    let mut transfer = Transfer::new(format!("{}/Items/abc/Download", server.base));

    let outcome = transfer.run(&h.cfg, None).await;

    assert_eq!(outcome, Outcome::Committed);
    assert_eq!(transfer.job().snapshot().state, DownloadState::Committed);
    let requests = server.requests();
    assert_eq!(requests[0].range, None);
    assert_eq!(requests[1].range.as_deref(), Some("bytes=524288-"));
    assert_eq!(range_start(&requests[1]), HALF);
    assert_eq!(final_bytes(&h, &transfer), body);
    assert_eq!(transfer.notice(), None, "no notice on a clean resume");
}

#[tokio::test]
async fn pause_interrupts_a_stalled_body_flushes_and_resumes_from_durable_offset() {
    let body = content(TOTAL);
    let scripted = body.clone();
    let server = start(Arc::new(move |index, recorded: &Recorded| {
        if index == 0 {
            stalled(&scripted, "\"v1\"", CUT_AFTER)
        } else {
            partial(&scripted, range_start(recorded), TOTAL, "\"v1\"")
        }
    }))
    .await;
    let h = harness(Duration::from_millis(5));
    let mut transfer = Transfer::new(format!("{}/x", server.base));
    let pause = transfer.pause_handle();
    let cfg = h.cfg.clone();
    let running = tokio::spawn(async move {
        let outcome = transfer.run(&cfg, None).await;
        (outcome, transfer)
    });

    wait_for_temp_file(&h.store, 256 * 1024).await;
    pause.store(true, Ordering::SeqCst);
    let (paused, mut transfer) = tokio::time::timeout(Duration::from_secs(3), running)
        .await
        .expect("pause is observed while waiting for the stalled response")
        .expect("download task joins");

    assert_eq!(paused, Outcome::Paused);
    assert_eq!(transfer.job().snapshot().state, DownloadState::Paused);
    let durable_offset = transfer.job().snapshot().durable_offset;
    assert!(durable_offset >= 256 * 1024);
    assert!(durable_offset < TOTAL as u64);
    assert_eq!(
        tmp_files(&h)
            .first()
            .map(|path| fs::metadata(path).unwrap().len()),
        Some(durable_offset)
    );

    let resumed = transfer.run(&h.cfg, None).await;
    assert_eq!(resumed, Outcome::Committed);
    let requests = server.requests();
    assert_eq!(requests[0].range, None);
    let expected_range = format!("bytes={durable_offset}-");
    assert_eq!(requests[1].range.as_deref(), Some(expected_range.as_str()));
    assert_eq!(final_bytes(&h, &transfer), body);
    assert_eq!(transfer.job().snapshot().state, DownloadState::Committed);
}

#[tokio::test]
async fn pause_cancels_request_stalled_before_response_headers() {
    let body = content(TOTAL);
    let server = start(Arc::new(move |_, _| full(&body, "\"v1\""))).await;
    let h = harness(Duration::from_millis(1));
    let mut transfer = Transfer::new(format!("{}/stall-headers", server.base));
    let pause = transfer.pause_handle();
    let running = tokio::spawn(async move {
        let outcome = transfer.run(&h.cfg, None).await;
        (outcome, transfer)
    });

    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if !server.requests().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("request reaches the server before pause");
    pause.store(true, Ordering::SeqCst);

    let (outcome, transfer) = tokio::time::timeout(Duration::from_secs(3), running)
        .await
        .expect("pause cancels the pending header request")
        .expect("download task");
    assert_eq!(outcome, Outcome::Paused);
    assert_eq!(transfer.job().snapshot().state, DownloadState::Paused);
    assert_eq!(transfer.job().snapshot().durable_offset, 0);
    assert_eq!(transfer.temp_name(), None);
}

#[tokio::test]
async fn ranges_ignored_restarts_once_with_notice() {
    let body = content(TOTAL);
    let scripted = body.clone();
    let server = start(Arc::new(move |index, _recorded: &Recorded| {
        if index == 0 {
            cut_full(&scripted, "\"v1\"", CUT_AFTER)
        } else {
            full(&scripted, "\"v1\"")
        }
    }))
    .await;
    let h = harness(Duration::from_millis(5));
    let mut transfer = Transfer::new(format!("{}/x", server.base));

    let outcome = transfer.run(&h.cfg, None).await;

    assert_eq!(outcome, Outcome::Committed);
    let requests = server.requests();
    assert_eq!(requests[1].range.as_deref(), Some("bytes=524288-"));
    assert_eq!(final_bytes(&h, &transfer), body);
    assert_eq!(transfer.notice(), Some(Notice::RangesIgnored));
}

#[tokio::test]
async fn needs_auth_keeps_bytes_and_resumes_with_new_credentials() {
    let body = content(TOTAL);
    let scripted = body.clone();
    let server = start(Arc::new(move |index, recorded: &Recorded| match index {
        0 => cut_full(&scripted, "\"v1\"", CUT_AFTER),
        1 => Reply {
            status: 401,
            headers: vec![],
            body: vec![],
            cut_after: None,
            stall: false,
        },
        _ => partial(&scripted, range_start(recorded), TOTAL, "\"v1\""),
    }))
    .await;
    let h = harness(Duration::from_millis(5));
    let mut transfer = Transfer::new(format!("{}/x", server.base));
    let old_auth = RequestAuth::new("Authorization", "MediaBrowser Token=token-old").expect("auth");

    let first = transfer.run(&h.cfg, Some(&old_auth)).await;

    assert_eq!(first, Outcome::NeedsAuth);
    assert_eq!(transfer.job().snapshot().state, DownloadState::NeedsAuth);
    assert_eq!(transfer.job().snapshot().durable_offset, 512 * 1024);
    let debug = format!("{:?}", transfer.job().snapshot());
    assert!(
        !debug.contains("token"),
        "credentials must not reach the job"
    );

    let new_auth = RequestAuth::new("Authorization", "MediaBrowser Token=token-new").expect("auth");
    let second = transfer.run(&h.cfg, Some(&new_auth)).await;

    assert_eq!(second, Outcome::Committed);
    let requests = server.requests();
    assert_eq!(
        requests[1].authorization.as_deref(),
        Some("MediaBrowser Token=token-old")
    );
    assert_eq!(requests[2].range.as_deref(), Some("bytes=524288-"));
    assert_eq!(
        requests[2].authorization.as_deref(),
        Some("MediaBrowser Token=token-new")
    );
    assert_eq!(final_bytes(&h, &transfer), body);
}

#[tokio::test]
async fn weak_etag_refuses_resume_and_restarts() {
    let body = content(TOTAL);
    let scripted = body.clone();
    let server = start(Arc::new(move |index, recorded: &Recorded| match index {
        0 => cut_full(&scripted, "W/\"v1\"", CUT_AFTER),
        1 => partial(&scripted, range_start(recorded), TOTAL, "W/\"v1\""),
        _ => full(&scripted, "W/\"v1\""),
    }))
    .await;
    let h = harness(Duration::from_millis(5));
    let mut transfer = Transfer::new(format!("{}/x", server.base));

    let outcome = transfer.run(&h.cfg, None).await;

    assert_eq!(outcome, Outcome::Committed);
    let ranges: Vec<Option<String>> = server.requests().into_iter().map(|r| r.range).collect();
    assert_eq!(ranges.len(), 3);
    assert_eq!(ranges[0], None);
    assert!(ranges[1].is_some());
    assert_eq!(
        ranges[2], None,
        "weak ETag: the retry must not send a Range"
    );
    assert_eq!(final_bytes(&h, &transfer), body);
    assert_eq!(transfer.notice(), Some(Notice::RangeRejected));
}

#[tokio::test]
async fn changed_strong_etag_restarts_with_resource_notice() {
    let body = content(TOTAL);
    let scripted = body.clone();
    let server = start(Arc::new(move |index, recorded: &Recorded| match index {
        0 => cut_full(&scripted, "\"v1\"", CUT_AFTER),
        1 => partial(&scripted, range_start(recorded), TOTAL, "\"v2\""),
        _ => full(&scripted, "\"v2\""),
    }))
    .await;
    let h = harness(Duration::from_millis(5));
    let mut transfer = Transfer::new(format!("{}/x", server.base));

    let outcome = transfer.run(&h.cfg, None).await;

    assert_eq!(outcome, Outcome::Committed);
    let requests = server.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[2].range, None);
    assert_eq!(final_bytes(&h, &transfer), body);
    assert_eq!(transfer.notice(), Some(Notice::ResourceChanged));
}

#[tokio::test]
async fn deleted_partial_file_restarts_with_local_bytes_notice() {
    let body = content(TOTAL);
    let scripted = body.clone();
    let server = start(Arc::new(move |index, _recorded: &Recorded| {
        if index == 0 {
            cut_full(&scripted, "\"v1\"", CUT_AFTER)
        } else {
            full(&scripted, "\"v1\"")
        }
    }))
    .await;
    // Long backoff: the partial file is deleted while the driver waits to retry.
    let h = harness(Duration::from_millis(800));
    let mut transfer = Transfer::new(format!("{}/x", server.base));
    let store_dir = h.store.dir().to_path_buf();
    let deleter = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        if let Ok(entries) = fs::read_dir(&store_dir) {
            for entry in entries.flatten() {
                let _ = fs::remove_file(entry.path());
            }
        }
    });

    let outcome = transfer.run(&h.cfg, None).await;
    deleter.await.expect("deleter");

    assert_eq!(outcome, Outcome::Committed);
    let requests = server.requests();
    assert_eq!(requests[1].range, None, "missing partial file: full GET");
    assert_eq!(final_bytes(&h, &transfer), body);
    assert_eq!(transfer.notice(), Some(Notice::LocalBytesLost));
    assert!(
        tmp_files(&h)
            .iter()
            .all(|p| !p.to_string_lossy().ends_with(".discarded"))
    );
}

#[tokio::test]
async fn unsatisfiable_range_restarts_with_notice() {
    let body = content(TOTAL);
    let scripted = body.clone();
    let server = start(Arc::new(move |index, _recorded: &Recorded| match index {
        0 => cut_full(&scripted, "\"v1\"", CUT_AFTER),
        1 => Reply {
            status: 416,
            headers: vec![
                ("ETag".into(), "\"v1\"".into()),
                ("Content-Range".into(), format!("bytes */{TOTAL}")),
            ],
            body: vec![],
            cut_after: None,
            stall: false,
        },
        _ => full(&scripted, "\"v1\""),
    }))
    .await;
    let h = harness(Duration::from_millis(5));
    let mut transfer = Transfer::new(format!("{}/x", server.base));

    let outcome = transfer.run(&h.cfg, None).await;

    assert_eq!(outcome, Outcome::Committed);
    let requests = server.requests();
    assert_eq!(requests[2].range, None);
    assert_eq!(final_bytes(&h, &transfer), body);
    assert_eq!(transfer.notice(), Some(Notice::RangeRejected));
}

#[tokio::test]
async fn total_length_change_fails_the_download() {
    let body = content(TOTAL);
    let scripted = body.clone();
    let server = start(Arc::new(move |index, _recorded: &Recorded| {
        if index == 0 {
            cut_full(&scripted, "\"v1\"", CUT_AFTER)
        } else {
            Reply {
                status: 206,
                headers: vec![
                    ("ETag".into(), "\"v1\"".into()),
                    (
                        "Content-Range".into(),
                        format!("bytes 524288-{}/{}", TOTAL, TOTAL * 2),
                    ),
                ],
                body: scripted[HALF..].to_vec(),
                cut_after: None,
                stall: false,
            }
        }
    }))
    .await;
    let h = harness(Duration::from_millis(5));
    let mut transfer = Transfer::new(format!("{}/x", server.base));

    let outcome = transfer.run(&h.cfg, None).await;

    assert_eq!(
        outcome,
        Outcome::Failed(FailureReason::Model(
            DownloadTransitionError::TotalLengthMismatch {
                expected: TOTAL as u64,
                actual: (TOTAL * 2) as u64,
            }
        ))
    );
    assert_eq!(transfer.job().snapshot().state, DownloadState::Failed);
}

#[tokio::test]
async fn repeated_network_cuts_fail_after_bounded_attempts() {
    let body = content(TOTAL);
    let scripted = body.clone();
    let server = start(Arc::new(move |_index, _recorded: &Recorded| {
        cut_full(&scripted, "\"v1\"", 100 * 1024)
    }))
    .await;
    let h = harness(Duration::from_millis(5));
    let mut transfer = Transfer::new(format!("{}/x", server.base));

    let outcome = transfer.run(&h.cfg, None).await;

    assert_eq!(outcome, Outcome::Failed(FailureReason::NetworkExhausted));
    assert_eq!(server.requests().len(), 3);
    assert_eq!(transfer.job().snapshot().state, DownloadState::Failed);
}

#[tokio::test]
async fn server_error_status_fails_with_http_reason() {
    let server = start(Arc::new(|_index, _recorded: &Recorded| Reply {
        status: 404,
        headers: vec![],
        body: vec![],
        cut_after: None,
        stall: false,
    }))
    .await;
    let h = harness(Duration::from_millis(5));
    let mut transfer = Transfer::new(format!("{}/x", server.base));

    let outcome = transfer.run(&h.cfg, None).await;

    assert_eq!(outcome, Outcome::Failed(FailureReason::Http(404)));
}

fn sha1_hex(bytes: &[u8]) -> String {
    use sha1::{Digest, Sha1};
    Sha1::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[tokio::test]
async fn matching_server_checksum_commits_verified() {
    use kattri_downloads::verify::{ServerChecksum, Verification};
    let body = content(TOTAL);
    let scripted = body.clone();
    let server = start(Arc::new(move |_index, _recorded: &Recorded| {
        full(&scripted, "\"v1\"")
    }))
    .await;
    let h = harness(Duration::from_millis(5));
    let checksum = ServerChecksum::parse("sha1", &sha1_hex(&body)).expect("parse");
    let mut transfer = Transfer::new(format!("{}/x", server.base)).with_server_checksum(checksum);

    let outcome = transfer.run(&h.cfg, None).await;

    assert_eq!(outcome, Outcome::Committed);
    assert_eq!(transfer.verification(), Some(Verification::Verified));
}

/// A failed record leaves nothing behind (M3-10): the bytes, the snapshot and the manifest
/// are all removed when the checksum does not match.
#[tokio::test]
async fn mismatching_server_checksum_fails_and_removes_temp_file_and_records() {
    use kattri_downloads::verify::ServerChecksum;
    let body = content(TOTAL);
    let scripted = body.clone();
    let server = start(Arc::new(move |_index, _recorded: &Recorded| {
        full(&scripted, "\"v1\"")
    }))
    .await;
    let h = harness(Duration::from_millis(5));
    let (cfg, snapshot, manifest) = cfg_with_records(&h);
    let wrong = ServerChecksum::parse("sha1", &"0".repeat(40)).expect("parse");
    let mut transfer = Transfer::new(format!("{}/x", server.base)).with_server_checksum(wrong);

    let outcome = transfer.run(&cfg, None).await;

    assert_eq!(outcome, Outcome::Failed(FailureReason::ChecksumMismatch));
    assert_eq!(transfer.job().snapshot().state, DownloadState::Failed);
    assert_eq!(transfer.verification(), None);
    assert!(
        transfer.temp_name().is_none(),
        "failed record forgets its temp file"
    );
    assert!(tmp_files(&h).is_empty(), "failed temp file is removed");
    assert!(!snapshot.exists(), "failed snapshot record is removed");
    assert!(!manifest.exists(), "failed manifest file is removed");
}

#[tokio::test]
async fn no_server_checksum_commits_unverified() {
    use kattri_downloads::verify::Verification;
    let body = content(TOTAL);
    let scripted = body.clone();
    let server = start(Arc::new(move |_index, _recorded: &Recorded| {
        full(&scripted, "\"v1\"")
    }))
    .await;
    let h = harness(Duration::from_millis(5));
    let mut transfer = Transfer::new(format!("{}/x", server.base));

    let outcome = transfer.run(&h.cfg, None).await;

    assert_eq!(outcome, Outcome::Committed);
    assert_eq!(transfer.verification(), Some(Verification::Unverified));
}

/// A video with two external text subtitles, one failing with 500. The video
/// completes, the failing subtitle is marked failed, and the manifest has no URLs.
#[tokio::test]
async fn failing_subtitle_does_not_fail_the_video_and_manifest_has_no_urls() {
    use kattri_downloads::manifest::{Manifest, ManifestInput};
    use kattri_downloads::subtitles::{SubtitleSelection, download_subtitles};
    use kattri_downloads::verify::Verification;

    let body = content(TOTAL);
    let scripted = body.clone();
    let server = start(Arc::new(move |_index, recorded: &Recorded| {
        if recorded.path.ends_with("/Subtitles/2/Stream.srt") {
            full(b"1\n00:00:01,000 --> 00:00:02,000\nHello\n", "\"s2\"")
        } else if recorded.path.contains("/Subtitles/3/") {
            Reply {
                status: 500,
                headers: vec![],
                body: vec![],
                cut_after: None,
                stall: false,
            }
        } else {
            full(&scripted, "\"v1\"")
        }
    }))
    .await;
    let h = harness(Duration::from_millis(5));
    let item = "item1";
    let video_url = format!("{}/Items/{item}/Download", server.base);
    let mut video = Transfer::new(video_url);
    assert_eq!(video.run(&h.cfg, None).await, Outcome::Committed);

    let selections = vec![
        SubtitleSelection {
            index: 2,
            format: "srt".into(),
            language: Some("eng".into()),
        },
        SubtitleSelection {
            index: 3,
            format: "srt".into(),
            language: Some("fre".into()),
        },
    ];
    let downloads_root = h._dir.path().join("downloads");
    let results = download_subtitles(
        &h.cfg,
        &server.base,
        item,
        item,
        &selections,
        None,
        &downloads_root,
    )
    .await;

    // The video is unaffected by the subtitle failure.
    assert_eq!(video.job().snapshot().state, DownloadState::Committed);
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].file_name.as_deref(), Some("2.srt"));
    assert_eq!(results[0].verification, Some(Verification::Unverified));
    assert!(!results[0].failed);
    assert!(results[1].failed, "500 subtitle is marked failed");
    let stored = fs::read(downloads_root.join(item).join("subtitles").join("2.srt")).expect("sub");
    assert!(stored.starts_with(b"1\n00:00:01"));

    let snapshot = video.job().snapshot().clone();
    let subtitles: Vec<_> = results.iter().map(|r| r.to_entry()).collect();
    let mut manifest = Manifest::build(&ManifestInput {
        server_id: "server-1",
        item_id: item,
        raw_title: Some("Episode 1"),
        snapshot: &snapshot,
        temp_name: video.temp_name(),
        final_name: Some("item1.mkv"),
        verification: video.verification(),
        failed: false,
        added_at: 1_700_000_000,
    });
    manifest.subtitles = subtitles;
    let manifest_path = downloads_root.join(item).join("manifest.json");
    kattri_downloads::manifest::write_atomic(&manifest_path, &manifest).expect("write manifest");

    let text = fs::read_to_string(&manifest_path).expect("read manifest");
    for banned in [
        "http",
        "127.0.0.1",
        "api_key",
        "token",
        "Authorization",
        "Subtitles/",
    ] {
        assert!(!text.contains(banned), "manifest contains {banned:?}");
    }
    assert!(text.contains("\"status\": \"failed\""));
    assert!(text.contains("\"fileName\": \"2.srt\""));
}

#[tokio::test]
async fn dropped_transfer_resumes_from_persisted_offset_at_half() {
    let body = content(TOTAL);
    let scripted = body.clone();
    let server = start(Arc::new(move |index, recorded: &Recorded| {
        if index == 0 {
            stalled(&scripted, "\"v1\"", CUT_AFTER)
        } else {
            partial(&scripted, range_start(recorded), TOTAL, "\"v1\"")
        }
    }))
    .await;
    let h = harness(Duration::from_millis(5));
    let record_path = h._dir.path().join("snapshot.json");
    let mut cfg = h.cfg.clone();
    cfg.snapshot_path = Some(record_path.clone());
    let url = format!("{}/Items/abc/Download", server.base);

    let mut first = Transfer::new(url.clone());
    let killed = tokio::time::timeout(Duration::from_secs(5), first.run(&cfg, None)).await;
    assert!(
        killed.is_err(),
        "first attempt must still be streaming when killed"
    );
    drop(first);

    let record = persist::load(&record_path).expect("record survives the kill");
    assert_eq!(record.durable_offset, HALF as u64);
    assert_eq!(record.state, PersistedState::Queued);
    let (job, temp_name) = record.into_job().expect("restore job");
    let mut resumed = Transfer::restored(url, job, temp_name);

    let outcome = resumed.run(&cfg, None).await;

    assert_eq!(outcome, Outcome::Committed);
    let requests = server.requests();
    assert_eq!(requests[1].range.as_deref(), Some("bytes=524288-"));
    assert_eq!(range_start(&requests[1]), HALF);
    assert_eq!(final_bytes(&h, &resumed), body);
    assert_eq!(resumed.notice(), None, "no notice on a clean resume");
    assert!(!record_path.exists(), "the record is removed on commit");
}

#[tokio::test]
async fn dropped_transfer_then_ranges_ignored_restarts_from_zero_with_notice() {
    let body = content(TOTAL);
    let scripted = body.clone();
    let server = start(Arc::new(move |index, _recorded: &Recorded| {
        if index == 0 {
            stalled(&scripted, "\"v1\"", CUT_AFTER)
        } else {
            full(&scripted, "\"v1\"")
        }
    }))
    .await;
    let h = harness(Duration::from_millis(5));
    let record_path = h._dir.path().join("snapshot.json");
    let mut cfg = h.cfg.clone();
    cfg.snapshot_path = Some(record_path.clone());
    let url = format!("{}/x", server.base);

    let mut first = Transfer::new(url.clone());
    let killed = tokio::time::timeout(Duration::from_secs(5), first.run(&cfg, None)).await;
    assert!(
        killed.is_err(),
        "first attempt must still be streaming when killed"
    );
    drop(first);

    let (job, temp_name) = persist::load(&record_path)
        .expect("record survives the kill")
        .into_job()
        .expect("restore job");
    let mut resumed = Transfer::restored(url, job, temp_name);

    let outcome = resumed.run(&cfg, None).await;

    assert_eq!(outcome, Outcome::Committed);
    let requests = server.requests();
    assert_eq!(requests[1].range.as_deref(), Some("bytes=524288-"));
    assert_eq!(final_bytes(&h, &resumed), body);
    assert_eq!(resumed.notice(), Some(Notice::RangesIgnored));
}

const CHILD_MODE_ENV: &str = "KATTRI_DOWNLOAD_SUBPROCESS_CHILD";
const CHILD_URL_ENV: &str = "KATTRI_DOWNLOAD_SUBPROCESS_URL";
const CHILD_STORE_ENV: &str = "KATTRI_DOWNLOAD_SUBPROCESS_STORE";
const CHILD_SNAPSHOT_ENV: &str = "KATTRI_DOWNLOAD_SUBPROCESS_SNAPSHOT";

/// Child mode runs the real transfer in a separate test-process. The parent-owned HTTP
/// server stalls after sending 600 KiB, leaving the atomically persisted offset at 512 KiB.
async fn run_subprocess_resume_case(ignore_range_after_restart: bool) {
    if std::env::var(CHILD_MODE_ENV).is_ok() {
        let expected_mode = if ignore_range_after_restart {
            "ignore"
        } else {
            "resume"
        };
        assert_eq!(std::env::var(CHILD_MODE_ENV).as_deref(), Ok(expected_mode));
        let url = std::env::var(CHILD_URL_ENV).expect("child URL");
        let store_dir = PathBuf::from(std::env::var_os(CHILD_STORE_ENV).expect("child store"));
        let snapshot_path =
            PathBuf::from(std::env::var_os(CHILD_SNAPSHOT_ENV).expect("child snapshot"));
        let store = TempStore::new(store_dir);
        let mut cfg = DriverConfig::new(store).expect("child driver config");
        cfg.snapshot_path = Some(snapshot_path);
        cfg.retry_backoff = Duration::from_millis(5);
        let mut transfer = Transfer::new(url);

        // The parent must terminate this process while the body remains stalled.
        let outcome = transfer.run(&cfg, None).await;
        panic!("stalled child transfer unexpectedly returned: {outcome:?}");
    }

    let body = content(TOTAL);
    let scripted = body.clone();
    let server = start(Arc::new(move |index, recorded: &Recorded| {
        if index == 0 {
            stalled(&scripted, "\"v1\"", CUT_AFTER)
        } else if ignore_range_after_restart {
            full(&scripted, "\"v1\"")
        } else {
            partial(&scripted, range_start(recorded), TOTAL, "\"v1\"")
        }
    }))
    .await;
    let h = harness(Duration::from_millis(5));
    let snapshot_path = h._dir.path().join("process-snapshot.json");
    let url = format!("{}/Items/test/Download", server.base);
    let mode = if ignore_range_after_restart {
        "ignore"
    } else {
        "resume"
    };
    let child_test = if ignore_range_after_restart {
        "subprocess_kill_then_ignored_range_restarts_with_notice"
    } else {
        "subprocess_kill_resumes_from_durable_half_offset"
    };
    let executable = std::env::current_exe().expect("test executable path");
    let mut child = Command::new(executable)
        .arg("--exact")
        .arg(child_test)
        .arg("--nocapture")
        .env(CHILD_MODE_ENV, mode)
        .env(CHILD_URL_ENV, &url)
        .env(CHILD_STORE_ENV, h.store.dir())
        .env(CHILD_SNAPSHOT_ENV, &snapshot_path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn subprocess test child");

    let reached_half = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(status) = child.try_wait().expect("poll child process") {
                return Err(format!("child exited before durable half offset: {status}"));
            }
            if let Ok(record) = persist::load(&snapshot_path)
                && record.durable_offset == HALF as u64
            {
                return Ok(record);
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;

    let record = match reached_half {
        Ok(Ok(record)) => record,
        Ok(Err(error)) => {
            let _ = child.kill();
            let _ = child.wait();
            panic!("{error}");
        }
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            panic!("child did not durably reach the half offset within 10 seconds");
        }
    };

    child
        .kill()
        .expect("terminate child operating-system process");
    let child_status = child.wait().expect("reap terminated child process");
    assert!(
        !child_status.success(),
        "terminated child must not exit normally"
    );

    assert_eq!(record.state, PersistedState::Queued);
    assert_eq!(record.expected_total_length, Some(TOTAL as u64));
    let snapshot_text = fs::read_to_string(&snapshot_path).expect("read child snapshot");
    for forbidden in [
        &url,
        "http://",
        "127.0.0.1",
        "Authorization",
        "token",
        "Items/",
    ] {
        assert!(
            !snapshot_text.contains(forbidden),
            "persisted snapshot contains forbidden value {forbidden:?}"
        );
    }
    let temp_name = record.temp_name.as_deref().expect("partial file name");
    assert_eq!(
        fs::metadata(h.store.dir().join(temp_name))
            .expect("child's durable partial file")
            .len(),
        HALF as u64
    );

    // Recreate the driver from the on-disk snapshot only after the child is dead.
    let restored = persist::load(&snapshot_path).expect("reload snapshot after process kill");
    assert_eq!(restored.durable_offset, HALF as u64);
    let (job, temp_name) = restored.into_job().expect("restore snapshot job");
    let mut cfg = h.cfg.clone();
    cfg.snapshot_path = Some(snapshot_path.clone());
    let mut resumed = Transfer::restored(url, job, temp_name);
    assert_eq!(resumed.run(&cfg, None).await, Outcome::Committed);

    let requests = server.requests();
    assert_eq!(
        requests.len(),
        2,
        "one child request and one resumed request"
    );
    assert_eq!(requests[0].range, None);
    assert_eq!(requests[1].range.as_deref(), Some("bytes=524288-"));
    assert_eq!(range_start(&requests[1]), HALF);
    let final_data = final_bytes(&h, &resumed);
    assert_eq!(final_data, body);
    assert_eq!(sha256_hex(&final_data), sha256_hex(&body));
    if ignore_range_after_restart {
        assert_eq!(resumed.notice(), Some(Notice::RangesIgnored));
    } else {
        assert_eq!(resumed.notice(), None);
    }
    assert!(!snapshot_path.exists(), "successful commit clears snapshot");
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Verifies a real OS process termination, fresh snapshot reload, and byte-perfect resume.
#[tokio::test]
async fn subprocess_kill_resumes_from_durable_half_offset() {
    run_subprocess_resume_case(false).await;
}

/// Verifies the real-process restart path when the server ignores the resumed Range.
#[tokio::test]
async fn subprocess_kill_then_ignored_range_restarts_with_notice() {
    run_subprocess_resume_case(true).await;
}

/// Pre-check: a body that cannot fit on the volume is refused before any byte is
/// written, and no temp file is left behind.
#[tokio::test]
async fn insufficient_disk_space_fails_before_writing_bytes() {
    let body = content(TOTAL);
    let scripted = body.clone();
    let server = start(Arc::new(move |_index, _recorded: &Recorded| {
        full(&scripted, "\"v1\"")
    }))
    .await;
    let h = harness(Duration::from_millis(5));
    let mut cfg = h.cfg.clone();
    cfg.disk_space = Some(Arc::new(MemoryDiskSpace::new(1024)));
    let mut transfer = Transfer::new(format!("{}/x", server.base));

    let outcome = transfer.run(&cfg, None).await;

    assert_eq!(outcome, Outcome::Failed(FailureReason::DiskFull));
    assert!(
        tmp_files(&h).is_empty(),
        "no temp file for a body that cannot fit"
    );
}

/// Near-full volume (M3-09): free space equal to the file size is below the spec's
/// size-plus-5-percent requirement, so the fresh file is refused before any byte is written.
#[tokio::test]
async fn near_full_volume_at_exact_file_size_is_refused() {
    let body = content(TOTAL);
    let scripted = body.clone();
    let server = start(Arc::new(move |_index, _recorded: &Recorded| {
        full(&scripted, "\"v1\"")
    }))
    .await;
    let h = harness(Duration::from_millis(5));
    let mut cfg = h.cfg.clone();
    cfg.disk_space = Some(Arc::new(MemoryDiskSpace::new(TOTAL as u64)));
    let mut transfer = Transfer::new(format!("{}/x", server.base));

    let outcome = transfer.run(&cfg, None).await;

    assert_eq!(outcome, Outcome::Failed(FailureReason::DiskFull));
    assert!(
        tmp_files(&h).is_empty(),
        "no temp file on a near-full volume"
    );
}

/// Pre-check passes when the volume holds the file size plus the 5 percent margin.
#[tokio::test]
async fn sufficient_disk_space_lets_the_download_commit() {
    let body = content(TOTAL);
    let scripted = body.clone();
    let server = start(Arc::new(move |_index, _recorded: &Recorded| {
        full(&scripted, "\"v1\"")
    }))
    .await;
    let h = harness(Duration::from_millis(5));
    let mut cfg = h.cfg.clone();
    let required = required_free_bytes(TOTAL as u64).expect("requirement fits in u64");
    cfg.disk_space = Some(Arc::new(MemoryDiskSpace::new(required)));
    let mut transfer = Transfer::new(format!("{}/x", server.base));

    let outcome = transfer.run(&cfg, None).await;

    assert_eq!(outcome, Outcome::Committed);
    assert_eq!(final_bytes(&h, &transfer), body);
}

fn redirect_to(location: &str) -> Reply {
    Reply {
        status: 302,
        headers: vec![("Location".into(), location.into())],
        body: Vec::new(),
        cut_after: None,
        stall: false,
    }
}

/// Same host as `base` but spelled `localhost`, so reqwest sees a different host name.
fn other_host(base: &str) -> String {
    base.replace("127.0.0.1", "localhost")
}

#[tokio::test]
async fn same_origin_redirect_is_followed_with_credential() {
    let body = content(TOTAL);
    let scripted = body.clone();
    let server = start(Arc::new(move |index, _recorded: &Recorded| {
        if index == 0 {
            redirect_to("/Items/abc/Final")
        } else {
            full(&scripted, "\"v1\"")
        }
    }))
    .await;
    let h = harness(Duration::from_millis(5));
    let mut transfer = Transfer::new(format!("{}/Items/abc/Download", server.base));
    let auth = RequestAuth::new("X-Emby-Token", "token-a").expect("auth");

    let outcome = transfer.run(&h.cfg, Some(&auth)).await;

    assert_eq!(outcome, Outcome::Committed);
    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1].path, "/Items/abc/Final");
    assert_eq!(requests[1].emby_token.as_deref(), Some("token-a"));
    assert_eq!(final_bytes(&h, &transfer), body);
}

#[tokio::test]
async fn cross_host_redirect_does_not_carry_credential() {
    let body = content(TOTAL);
    let target_body = body.clone();
    let target = start(Arc::new(move |_index, _recorded: &Recorded| {
        full(&target_body, "\"v1\"")
    }))
    .await;
    let target_url = format!("{}/Items/abc/Download", other_host(&target.base));
    let origin = start(Arc::new(move |_index, _recorded: &Recorded| {
        redirect_to(&target_url)
    }))
    .await;
    let h = harness(Duration::from_millis(5));
    let mut transfer = Transfer::new(format!("{}/Items/abc/Download", origin.base));
    let auth = RequestAuth::new("X-Emby-Token", "token-a").expect("auth");

    let outcome = transfer.run(&h.cfg, Some(&auth)).await;

    assert_eq!(outcome, Outcome::Failed(FailureReason::Http(302)));
    assert_eq!(origin.requests()[0].emby_token.as_deref(), Some("token-a"));
    assert!(
        target.requests().is_empty(),
        "the cross-host target must not be contacted"
    );
    assert!(tmp_files(&h).is_empty());
}

#[tokio::test]
async fn cross_port_redirect_does_not_carry_credential() {
    let body = content(TOTAL);
    let target_body = body.clone();
    let target = start(Arc::new(move |_index, _recorded: &Recorded| {
        full(&target_body, "\"v1\"")
    }))
    .await;
    let target_url = format!("{}/Items/abc/Download", target.base);
    let origin = start(Arc::new(move |_index, _recorded: &Recorded| {
        redirect_to(&target_url)
    }))
    .await;
    let h = harness(Duration::from_millis(5));
    let mut transfer = Transfer::new(format!("{}/Items/abc/Download", origin.base));
    let auth = RequestAuth::new("X-Emby-Token", "token-a").expect("auth");

    let outcome = transfer.run(&h.cfg, Some(&auth)).await;

    assert_eq!(outcome, Outcome::Failed(FailureReason::Http(302)));
    assert_eq!(origin.requests()[0].emby_token.as_deref(), Some("token-a"));
    assert!(
        target.requests().is_empty(),
        "the same host on another port is another origin and must not be contacted"
    );
    assert!(tmp_files(&h).is_empty());
}

/// M3-08: the typed source builds the `/Items/{itemId}/Download` request, and the
/// credential travels only in the auth header.
#[tokio::test]
async fn typed_source_requests_original_file_route_with_header_auth_only() {
    let body = content(TOTAL);
    let scripted = body.clone();
    let server = start(Arc::new(move |_index, _recorded: &Recorded| {
        full(&scripted, "\"v1\"")
    }))
    .await;
    let h = harness(Duration::from_millis(5));
    let source = OriginalFileSource::new(&server.base, "item-1").expect("source");
    let mut transfer = Transfer::from_source(&source);
    let auth = RequestAuth::new("X-Emby-Token", "token-a").expect("auth");

    let outcome = transfer.run(&h.cfg, Some(&auth)).await;

    assert_eq!(outcome, Outcome::Committed);
    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].path, "/Items/item-1/Download");
    assert_eq!(requests[0].range, None);
    assert_eq!(requests[0].emby_token.as_deref(), Some("token-a"));
    assert_eq!(requests[0].authorization, None);
    assert_eq!(final_bytes(&h, &transfer), body);
}

/// M3-10: a download that fails after partial bytes (network retries exhausted) leaves
/// no temp file, snapshot record or manifest file.
#[tokio::test]
async fn network_exhausted_after_partial_bytes_removes_temp_file_and_records() {
    let body = content(TOTAL);
    let scripted = body.clone();
    let server = start(Arc::new(move |index, _recorded: &Recorded| {
        if index == 0 {
            cut_full(&scripted, "\"v1\"", CUT_AFTER)
        } else {
            Reply {
                status: 500,
                headers: vec![],
                body: vec![],
                cut_after: None,
                stall: false,
            }
        }
    }))
    .await;
    let h = harness(Duration::from_millis(5));
    let (cfg, snapshot, manifest) = cfg_with_records(&h);
    let mut transfer = Transfer::new(format!("{}/x", server.base));

    let outcome = transfer.run(&cfg, None).await;

    assert_eq!(outcome, Outcome::Failed(FailureReason::NetworkExhausted));
    // The cut response is the first network attempt; the rest are 500s.
    assert_eq!(server.requests().len(), cfg.max_network_attempts as usize);
    assert!(tmp_files(&h).is_empty(), "partial temp file is removed");
    assert!(!snapshot.exists(), "snapshot record is removed");
    assert!(!manifest.exists(), "manifest file is removed");
    assert!(transfer.temp_name().is_none());
}

/// M3-10: a subtitle child job never removes the video's records, on success or failure.
#[tokio::test]
async fn subtitle_jobs_leave_the_video_record_in_place() {
    use kattri_downloads::subtitles::{SubtitleSelection, download_subtitles};

    let body = content(TOTAL);
    let scripted = body.clone();
    let server = start(Arc::new(move |_index, recorded: &Recorded| {
        if recorded.path.contains("/Subtitles/3/") {
            Reply {
                status: 500,
                headers: vec![],
                body: vec![],
                cut_after: None,
                stall: false,
            }
        } else if recorded.path.contains("/Subtitles/2/") {
            full(b"1\n00:00:01,000 --> 00:00:02,000\nHi\n", "\"s2\"")
        } else {
            full(&scripted, "\"v1\"")
        }
    }))
    .await;
    let h = harness(Duration::from_millis(5));
    let (cfg, snapshot, manifest) = cfg_with_records(&h);
    let item = "item1";
    let mut video = Transfer::new(format!("{}/Items/{item}/Download", server.base));
    assert_eq!(video.run(&h.cfg, None).await, Outcome::Committed);
    fs::write(&snapshot, b"{}").expect("seed video snapshot");
    let selections = vec![
        SubtitleSelection {
            index: 2,
            format: "srt".into(),
            language: None,
        },
        SubtitleSelection {
            index: 3,
            format: "srt".into(),
            language: None,
        },
    ];
    let downloads_root = h._dir.path().join("downloads");

    let results = download_subtitles(
        &cfg,
        &server.base,
        item,
        item,
        &selections,
        None,
        &downloads_root,
    )
    .await;

    assert!(!results[0].failed);
    assert!(results[1].failed);
    assert!(
        snapshot.exists(),
        "subtitle commit and failure keep the video record"
    );
    assert!(
        manifest.exists(),
        "subtitle failure keeps the video manifest"
    );
}

/// Cleanup runs only for a job that reached `Failed`. A rejected second `run` on a
/// committed download reports `InvalidState` and keeps its bytes.
#[tokio::test]
async fn rejected_run_on_committed_job_keeps_its_temp_file() {
    let body = content(TOTAL);
    let scripted = body.clone();
    let server = start(Arc::new(move |_index, _recorded: &Recorded| {
        full(&scripted, "\"v1\"")
    }))
    .await;
    let h = harness(Duration::from_millis(5));
    let mut transfer = Transfer::new(format!("{}/x", server.base));
    assert_eq!(transfer.run(&h.cfg, None).await, Outcome::Committed);

    let again = transfer.run(&h.cfg, None).await;

    assert_eq!(again, Outcome::Failed(FailureReason::InvalidState));
    assert_eq!(transfer.job().snapshot().state, DownloadState::Committed);
    assert_eq!(final_bytes(&h, &transfer), body);
}
