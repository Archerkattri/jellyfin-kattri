//! Download manager against a loopback mock: start, pause, resume and list, the job-list file
//! contents, and recovery from a corrupt job list. No real server and no network beyond
//! 127.0.0.1.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use jellyfin_api::{ClientConfig, JellyfinClient, SessionAuthorization};
use kattri_client_core::downloads::DownloadState;
use kattri_downloads::driver::SYNC_CHUNK_BYTES;
use kattri_downloads::manager::{DownloadManager, JOB_LIST_FILE, JobView, ManagerError};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const TOKEN: &str = "SENTINEL-TOKEN-7f3a";
const SERVER_ID: &str = "server-1";
const ITEM: &str = "item-1";
const BODY_LEN: usize = 1024 * 1024;
/// The mock stops sending here on the first request, after one flush has happened.
const STALL_AFTER: usize = 300 * 1024;

fn body() -> Vec<u8> {
    (0..BODY_LEN).map(|index| (index % 251) as u8).collect()
}

#[derive(Clone, Debug)]
struct Seen {
    range: Option<String>,
    authorization: Option<String>,
}

struct Server {
    base: String,
    seen: Arc<Mutex<Vec<Seen>>>,
}

/// Serves sign-in and the original-file route. The first download request streams part of the
/// body and then stalls. A request with `Range` gets the rest as a 206.
async fn start_server() -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let base = format!("http://{}", listener.local_addr().expect("addr"));
    let seen: Arc<Mutex<Vec<Seen>>> = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    tokio::spawn(async move {
        loop {
            let Ok((socket, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(serve(socket, Arc::clone(&log)));
        }
    });
    Server { base, seen }
}

async fn serve(mut socket: TcpStream, log: Arc<Mutex<Vec<Seen>>>) {
    let Some(head) = read_request_head(&mut socket).await else {
        return;
    };
    let path = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("")
        .to_owned();
    if path == "/Users/AuthenticateByName" {
        let body = format!(
            r#"{{"AccessToken":"{TOKEN}","User":{{"Id":"user-1","Name":"tester"}},"ServerId":"{SERVER_ID}"}}"#
        );
        let reply = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = socket.write_all(reply.as_bytes()).await;
        let _ = socket.shutdown().await;
        return;
    }
    if path == format!("/Items/{ITEM}/Download") {
        let range = header_value(&head, "range");
        log.lock().expect("log").push(Seen {
            range: range.clone(),
            authorization: header_value(&head, "authorization"),
        });
        let data = body();
        let (start, status, content_range) = match range.as_deref().and_then(range_start) {
            Some(start) => (
                start,
                "206 Partial Content",
                format!(
                    "Content-Range: bytes {start}-{}/{BODY_LEN}\r\n",
                    BODY_LEN - 1
                ),
            ),
            None => (0, "200 OK", String::new()),
        };
        let reply = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nETag: \"v1\"\r\n{content_range}Connection: close\r\n\r\n",
            BODY_LEN - start
        );
        if socket.write_all(reply.as_bytes()).await.is_err() {
            return;
        }
        if start > 0 {
            let _ = socket.write_all(&data[start..]).await;
            let _ = socket.shutdown().await;
        } else {
            let _ = socket.write_all(&data[..STALL_AFTER]).await;
            tokio::time::sleep(Duration::from_secs(60)).await;
        }
        return;
    }
    let _ = socket
        .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
        .await;
}

/// Reads up to the end of the header block and any declared body, so the client does not see
/// a reset before it reads the reply.
async fn read_request_head(socket: &mut TcpStream) -> Option<String> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        if let Some(position) = buf.windows(4).position(|window| window == b"\r\n\r\n") {
            break position + 4;
        }
        let read = socket.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..read]);
    };
    let head = String::from_utf8_lossy(&buf[..header_end]).into_owned();
    let declared = header_value(&head, "content-length")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);
    while buf.len() < header_end + declared {
        let read = socket.read(&mut chunk).await.ok()?;
        if read == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..read]);
    }
    Some(head)
}

fn header_value(head: &str, name: &str) -> Option<String> {
    head.lines().skip(1).find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.trim()
            .eq_ignore_ascii_case(name)
            .then(|| value.trim().to_owned())
    })
}

/// The first byte of a `bytes=N-` range header.
fn range_start(value: &str) -> Option<usize> {
    value
        .strip_prefix("bytes=")?
        .strip_suffix('-')?
        .parse()
        .ok()
}

async fn authorization_for(base: &str) -> SessionAuthorization {
    let config = ClientConfig::new(
        base,
        "Kattri test",
        "test device",
        "device-id",
        "test-version",
    )
    .expect("valid config");
    let client = JellyfinClient::new(config).expect("loopback HTTP is allowed");
    let session = client
        .authenticate("tester", "test-password")
        .await
        .expect("sign-in against loopback");
    client.session_authorization(&session)
}

/// Polls the manager until `condition` holds, or fails the test after 15 seconds.
async fn wait_for<F>(manager: &DownloadManager, what: &str, condition: F) -> Vec<JobView>
where
    F: Fn(&[JobView]) -> bool,
{
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let jobs = manager.list();
            if condition(&jobs) {
                return jobs;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
}

/// Waits until the temp folder holds a file with at least `minimum` bytes, so a durable
/// flush has happened before the test acts.
async fn wait_for_flushed_bytes(tmp_dir: &Path, minimum: u64) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let flushed = fs::read_dir(tmp_dir)
                .ok()
                .into_iter()
                .flatten()
                .filter_map(Result::ok)
                .filter_map(|entry| entry.metadata().ok())
                .any(|metadata| metadata.len() >= minimum);
            if flushed {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("download flushed the requested bytes");
}

fn flush_bytes() -> u64 {
    u64::try_from(SYNC_CHUNK_BYTES).expect("flush size fits in u64")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn start_pause_resume_and_list_commit_the_whole_body() {
    let server = start_server().await;
    let authorization = authorization_for(&server.base).await;
    let dir = tempfile::tempdir().expect("tempdir");
    let downloads = dir.path().join("downloads");
    let manager = DownloadManager::open(&downloads).expect("open manager");
    assert!(manager.list().is_empty());
    assert!(manager.recovered_list().is_none());

    manager
        .start(SERVER_ID, &server.base, ITEM, &authorization)
        .expect("start");
    let started = manager.list();
    assert_eq!(started.len(), 1);
    assert_eq!(started[0].item_id, ITEM);
    assert!(started[0].running, "transfer owns the job after start");

    assert!(matches!(
        manager.start(SERVER_ID, &server.base, ITEM, &authorization),
        Err(ManagerError::AlreadyTracked)
    ));

    wait_for_flushed_bytes(&downloads.join("tmp"), flush_bytes()).await;
    manager.pause(ITEM).expect("pause a running job");
    let paused = wait_for(&manager, "pause to stop the transfer", |jobs| {
        !jobs[0].running
    })
    .await;
    assert_eq!(paused[0].state, DownloadState::Paused);
    let offset = paused[0].durable_offset;
    assert!(
        offset >= flush_bytes() && offset <= STALL_AFTER as u64,
        "paused at a flushed offset, got {offset}"
    );

    manager
        .pause(ITEM)
        .expect("pausing an idle paused job is a no-op");
    assert!(matches!(
        manager.resume("server-2", &server.base, ITEM, &authorization),
        Err(ManagerError::ServerMismatch)
    ));

    manager
        .resume(SERVER_ID, &server.base, ITEM, &authorization)
        .expect("resume");
    let committed = wait_for(&manager, "commit after resume", |jobs| {
        !jobs[0].running && jobs[0].state == DownloadState::Committed
    })
    .await;
    assert_eq!(committed[0].durable_offset, BODY_LEN as u64);

    let seen = server.seen.lock().expect("log").clone();
    assert_eq!(seen.len(), 2, "one request per transfer");
    assert_eq!(seen[0].range, None, "first request is a full GET");
    assert_eq!(
        seen[1].range.as_deref(),
        Some(format!("bytes={offset}-").as_str()),
        "resume starts at the flushed offset"
    );
    assert!(
        seen.iter().all(|request| request
            .authorization
            .as_deref()
            .is_some_and(|value| value.contains(TOKEN))),
        "the session authorization travels in the header"
    );

    let temp_files: Vec<PathBuf> = fs::read_dir(downloads.join("tmp"))
        .expect("temp folder")
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .collect();
    assert_eq!(temp_files.len(), 1, "one temp file for the item");
    let committed_bytes = fs::read(&temp_files[0]).expect("temp bytes");
    assert!(
        committed_bytes == body(),
        "committed bytes match the server body"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn job_list_file_holds_no_token_no_url_and_reloads_as_paused() {
    let server = start_server().await;
    let authorization = authorization_for(&server.base).await;
    let dir = tempfile::tempdir().expect("tempdir");
    let downloads = dir.path().join("downloads");
    let manager = DownloadManager::open(&downloads).expect("open manager");
    manager
        .start(SERVER_ID, &server.base, ITEM, &authorization)
        .expect("start");
    wait_for_flushed_bytes(&downloads.join("tmp"), flush_bytes()).await;
    manager.pause(ITEM).expect("pause");
    let paused = wait_for(&manager, "pause", |jobs| !jobs[0].running).await;
    drop(manager);

    let list = fs::read_to_string(downloads.join(JOB_LIST_FILE)).expect("job list written");
    assert!(list.contains(ITEM), "item ID is saved");
    assert!(list.contains(SERVER_ID), "server ID is saved");
    for forbidden in [
        TOKEN,
        "Token",
        "Authorization",
        "api_key",
        "http",
        "127.0.0.1",
    ] {
        assert!(
            !list.contains(forbidden),
            "job list must not contain the forbidden text {forbidden:?}"
        );
    }

    let reopened = DownloadManager::open(&downloads).expect("reopen manager");
    let reloaded = reopened.list();
    assert_eq!(reloaded.len(), 1);
    assert_eq!(reloaded[0].state, DownloadState::Paused);
    assert!(!reloaded[0].running);
    assert_eq!(reloaded[0].durable_offset, paused[0].durable_offset);
}

#[test]
fn corrupt_job_list_is_moved_aside_and_the_manager_starts_empty() {
    let dir = tempfile::tempdir().expect("tempdir");
    let downloads = dir.path().join("downloads");
    fs::create_dir_all(&downloads).expect("downloads folder");
    let list_path = downloads.join(JOB_LIST_FILE);
    fs::write(&list_path, b"{ not json").expect("write corrupt list");

    let manager = DownloadManager::open(&downloads).expect("open with a corrupt list");
    assert!(manager.list().is_empty(), "manager starts empty");
    let first_moved = downloads.join(format!("{JOB_LIST_FILE}.corrupt"));
    assert_eq!(manager.recovered_list(), Some(first_moved.as_path()));
    assert_eq!(
        fs::read(&first_moved).expect("moved copy"),
        b"{ not json",
        "the corrupt bytes are kept"
    );
    assert!(!list_path.exists(), "the original name is free");
    drop(manager);

    fs::write(&list_path, br#"{"schemaVersion":99,"jobs":[]}"#).expect("write future list");
    let manager = DownloadManager::open(&downloads).expect("open with a future-version list");
    assert!(manager.list().is_empty());
    let second_moved = downloads.join(format!("{JOB_LIST_FILE}.corrupt.1"));
    assert_eq!(manager.recovered_list(), Some(second_moved.as_path()));
    assert_eq!(
        fs::read(&first_moved).expect("first copy"),
        b"{ not json",
        "an earlier moved-aside copy is not overwritten"
    );
}
