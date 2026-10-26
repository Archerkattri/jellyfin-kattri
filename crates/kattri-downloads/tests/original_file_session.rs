//! M3-08: the session's `Authorization` value reaches the original-file request only as a
//! header. Runs against a loopback server on 127.0.0.1 that answers sign-in and the
//! download route. No real Jellyfin server and no network beyond loopback.

use std::fs;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use jellyfin_api::{ClientConfig, JellyfinClient, SessionAuthorization};
use kattri_downloads::driver::{DriverConfig, DriverError, OriginalFileRequest, Outcome, Transfer};
use kattri_downloads::fs::TempStore;
use kattri_downloads::manifest::{Manifest, ManifestInput, write_atomic};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const TOKEN: &str = "SENTINEL-TOKEN-7f3a";
const BODY_LEN: usize = 1024 * 1024;
/// Enough bytes for at least one 256 KiB durable flush before the stall.
const STALL_AFTER: usize = 300 * 1024;

#[derive(Clone, Debug)]
struct Seen {
    path: String,
    authorization: Option<String>,
}

struct Server {
    base: String,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl Server {
    fn download_requests(&self) -> Vec<Seen> {
        self.seen.lock().expect("log").clone()
    }
}

/// `stall_download` sends part of the download body and then holds the socket open, so a
/// test can pause a transfer mid-body.
async fn start(stall_download: bool) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let base = format!("http://{}", listener.local_addr().expect("addr"));
    let seen: Arc<Mutex<Vec<Seen>>> = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    tokio::spawn(async move {
        loop {
            let Ok((socket, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(serve(socket, Arc::clone(&log), stall_download));
        }
    });
    Server { base, seen }
}

async fn serve(mut socket: TcpStream, log: Arc<Mutex<Vec<Seen>>>, stall_download: bool) {
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
        let body = r#"{"AccessToken":"SENTINEL-TOKEN-7f3a","User":{"Id":"user-1","Name":"tester"},"ServerId":"server-1"}"#;
        let reply = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = socket.write_all(reply.as_bytes()).await;
        let _ = socket.shutdown().await;
        return;
    }
    if path.starts_with("/Items/") && path.ends_with("/Download") {
        let authorization = header_value(&head, "authorization");
        log.lock().expect("log").push(Seen {
            path,
            authorization,
        });
        let reply = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {BODY_LEN}\r\nETag: \"v1\"\r\nConnection: close\r\n\r\n"
        );
        if socket.write_all(reply.as_bytes()).await.is_err() {
            return;
        }
        let body = vec![7u8; BODY_LEN];
        if stall_download {
            let _ = socket.write_all(&body[..STALL_AFTER]).await;
            tokio::time::sleep(Duration::from_secs(60)).await;
        } else {
            let _ = socket.write_all(&body).await;
            let _ = socket.shutdown().await;
        }
        return;
    }
    let _ = socket
        .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
        .await;
}

/// Reads up to the end of the header block and consumes any declared request body, so the
/// client does not see a reset before it reads the reply.
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

/// Signs in against the loopback server and returns only the authorization value. The
/// session itself is dropped here, so the test holds the value alone.
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

fn driver_config(dir: &Path) -> DriverConfig {
    let mut cfg = DriverConfig::new(TempStore::new(dir.join("tmp"))).expect("driver config");
    cfg.retry_backoff = Duration::from_millis(5);
    cfg
}

async fn wait_for_temp_bytes(tmp_dir: &Path, minimum: u64) {
    tokio::time::timeout(Duration::from_secs(3), async {
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

#[tokio::test]
async fn request_sends_the_session_token_only_in_the_authorization_header() {
    let server = start(false).await;
    let authorization = authorization_for(&server.base).await;
    let request =
        OriginalFileRequest::new(&server.base, "item-1", &authorization).expect("request");
    let dir = tempfile::tempdir().expect("tempdir");
    let cfg = driver_config(dir.path());
    let mut transfer = Transfer::from_source(&request.source);

    let outcome = transfer.run(&cfg, Some(&request.auth)).await;

    assert_eq!(outcome, Outcome::Committed);
    let seen = server.download_requests();
    assert_eq!(seen.len(), 1, "exactly one download request");
    assert_eq!(seen[0].path, "/Items/item-1/Download", "no query string");
    let header = seen[0]
        .authorization
        .as_deref()
        .expect("Authorization header");
    assert!(header.starts_with("MediaBrowser "), "{header}");
    assert!(header.contains(&format!("Token=\"{TOKEN}\"")), "{header}");
}

#[tokio::test]
async fn token_stays_out_of_debug_output_the_url_and_the_persisted_records() {
    let server = start(true).await;
    let authorization = authorization_for(&server.base).await;
    let request =
        OriginalFileRequest::new(&server.base, "item-1", &authorization).expect("request");

    let authorization_debug = format!("{authorization:?}");
    let request_debug = format!("{request:?}");
    let source_debug = format!("{:?}", request.source);
    assert!(
        !authorization_debug.contains(TOKEN),
        "{authorization_debug}"
    );
    assert!(!request_debug.contains(TOKEN), "{request_debug}");
    assert!(!source_debug.contains(TOKEN), "{source_debug}");
    assert!(
        !request.source.url().contains(TOKEN),
        "URL carries no token"
    );
    assert!(
        !request.source.url().contains("Token"),
        "URL carries no auth field"
    );

    let dir = tempfile::tempdir().expect("tempdir");
    let snapshot_path = dir.path().join("snapshot.json");
    let mut cfg = driver_config(dir.path());
    cfg.snapshot_path = Some(snapshot_path.clone());
    let tmp_dir = dir.path().join("tmp");
    let OriginalFileRequest { source, auth } = request;
    let mut transfer = Transfer::from_source(&source);
    let pause = transfer.pause_handle();
    let running = tokio::spawn(async move {
        let outcome = transfer.run(&cfg, Some(&auth)).await;
        (outcome, transfer)
    });
    wait_for_temp_bytes(&tmp_dir, 256 * 1024).await;
    pause.store(true, Ordering::SeqCst);
    let (outcome, transfer) = tokio::time::timeout(Duration::from_secs(3), running)
        .await
        .expect("pause is observed during the stalled body")
        .expect("download task joins");
    assert_eq!(outcome, Outcome::Paused);

    let snapshot_text = fs::read_to_string(&snapshot_path).expect("snapshot saved on flush");
    assert!(
        !snapshot_text.contains(TOKEN),
        "snapshot record has no token"
    );
    assert!(
        !snapshot_text.contains(&server.base),
        "snapshot record has no upstream URL"
    );

    let snapshot = transfer.job().snapshot();
    let manifest = Manifest::build(&ManifestInput {
        server_id: "server-1",
        item_id: "item-1",
        raw_title: Some("Feature film"),
        snapshot,
        temp_name: transfer.temp_name(),
        final_name: None,
        verification: None,
        failed: false,
        added_at: 1,
    });
    let manifest_path = dir.path().join("manifest.json");
    write_atomic(&manifest_path, &manifest).expect("manifest written");
    let manifest_text = fs::read_to_string(&manifest_path).expect("manifest read");
    assert!(!manifest_text.contains(TOKEN), "manifest has no token");
    assert!(
        !manifest_text.contains(&server.base),
        "manifest has no upstream URL"
    );
    assert!(
        !manifest_text.contains("Authorization"),
        "manifest has no auth field"
    );
}

#[tokio::test]
async fn item_id_with_path_characters_is_refused() {
    let server = start(false).await;
    let authorization = authorization_for(&server.base).await;

    for item_id in [
        "item/1", "item?1", "item#1", "..", ".", "", "item%2F1", "item 1", "item\n1", "a/../b",
    ] {
        let result = OriginalFileRequest::new(&server.base, item_id, &authorization);
        assert!(
            matches!(result, Err(DriverError::InvalidItemId)),
            "{item_id:?} must be refused"
        );
    }

    let accepted = OriginalFileRequest::new(
        &server.base,
        "0f8e2c1a9b7d4e3f8a6b5c4d3e2f1a0b",
        &authorization,
    );
    assert!(accepted.is_ok(), "a plain 32-character item ID is accepted");
}
