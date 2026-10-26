//! M3-11: a download cut at 50 percent resumes from its flushed offset with a `Range`
//! request, and a 206 whose ETag changed restarts from zero.
//!
//! The server is an in-test HTTP/1.1 listener bound to 127.0.0.1. No Jellyfin server and
//! no non-loopback network access. Run with `--nocapture` to see the resume offset and
//! the final SHA-256 printed by each test.

use std::fs;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use kattri_client_core::downloads::DownloadState;
use kattri_downloads::driver::{DriverConfig, Outcome, Transfer};
use kattri_downloads::fs::TempStore;
use kattri_downloads::messages::Notice;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const TOTAL: usize = 1024 * 1024;
const HALF: usize = TOTAL / 2;
/// The production flush is 256 KiB. A 64 KiB flush lets the cut below leave bytes
/// that were received but not yet durable, which the resume must not count.
const TEST_SYNC_CHUNK: usize = 64 * 1024;
/// The connection is cut 32 KiB past the midpoint. Durable bytes stop at 512 KiB.
/// A driver that resumed from received bytes would send `bytes=557056-` instead.
const CUT_AFTER: usize = HALF + 32 * 1024;

#[derive(Clone, Debug)]
struct Recorded {
    range: Option<String>,
}

struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    cut_after: Option<usize>,
}

/// Gets the request index and the `Range` start (if any), returns the scripted reply.
type Handler = Arc<dyn Fn(usize, Option<usize>) -> Reply + Send + Sync>;

struct Server {
    base: String,
    log: Arc<Mutex<Vec<Recorded>>>,
}

impl Server {
    fn requests(&self) -> Vec<Recorded> {
        self.log.lock().expect("request log").clone()
    }
}

async fn start(handler: Handler) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    let log: Arc<Mutex<Vec<Recorded>>> = Arc::new(Mutex::new(Vec::new()));
    let log_task = Arc::clone(&log);
    tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            tokio::spawn(serve(socket, Arc::clone(&handler), Arc::clone(&log_task)));
        }
    });
    Server {
        base: format!("http://{addr}"),
        log,
    }
}

async fn serve(mut socket: TcpStream, handler: Handler, log: Arc<Mutex<Vec<Recorded>>>) {
    let head = read_head(&mut socket).await;
    let range = head.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.trim()
            .eq_ignore_ascii_case("range")
            .then(|| value.trim().to_owned())
    });
    let range_start = range.as_deref().and_then(parse_range_start);
    let index = {
        let mut entries = log.lock().expect("request log");
        entries.push(Recorded {
            range: range.clone(),
        });
        entries.len() - 1
    };
    let reply = handler(index, range_start);
    let mut out = format!(
        "HTTP/1.1 {} Scripted\r\nContent-Length: {}\r\nConnection: close\r\n",
        reply.status,
        reply.body.len()
    );
    for (name, value) in &reply.headers {
        out.push_str(&format!("{name}: {value}\r\n"));
    }
    out.push_str("\r\n");
    let end = reply
        .cut_after
        .unwrap_or(reply.body.len())
        .min(reply.body.len());
    let _ = socket.write_all(out.as_bytes()).await;
    let _ = socket.write_all(&reply.body[..end]).await;
    // Closing before Content-Length bytes is the simulated network cut.
    let _ = socket.shutdown().await;
}

async fn read_head(socket: &mut TcpStream) -> String {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
        match socket.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
    String::from_utf8_lossy(&buf).into_owned()
}

fn parse_range_start(value: &str) -> Option<usize> {
    value
        .strip_prefix("bytes=")?
        .strip_suffix('-')?
        .parse()
        .ok()
}

fn content(total: usize, salt: u8) -> Vec<u8> {
    (0..total)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 16) as u8 ^ salt)
        .collect()
}

fn full(body: &[u8], etag: &str) -> Reply {
    Reply {
        status: 200,
        headers: vec![("ETag".into(), etag.into())],
        body: body.to_vec(),
        cut_after: None,
    }
}

fn cut(body: &[u8], etag: &str, after: usize) -> Reply {
    Reply {
        cut_after: Some(after),
        ..full(body, etag)
    }
}

fn partial(body: &[u8], start: usize, etag: &str) -> Reply {
    let total = body.len();
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
    }
}

struct Harness {
    _dir: tempfile::TempDir,
    cfg: DriverConfig,
    store: TempStore,
}

fn harness() -> Harness {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = TempStore::new(dir.path().join("tmp"));
    let mut cfg = DriverConfig::new(store.clone()).expect("driver config");
    cfg.retry_backoff = Duration::from_millis(5);
    cfg.sync_chunk_bytes = TEST_SYNC_CHUNK;
    Harness {
        _dir: dir,
        cfg,
        store,
    }
}

fn final_bytes(h: &Harness, transfer: &Transfer) -> Vec<u8> {
    let name = transfer.temp_name().expect("temp name");
    fs::read(h.store.dir().join(name)).expect("read temp file")
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[tokio::test]
async fn cut_at_half_resumes_from_flushed_offset() {
    let body = content(TOTAL, 0);
    let served = body.clone();
    let server = start(Arc::new(move |index, range_start| {
        match (index, range_start) {
            (0, _) => cut(&served, "\"v1\"", CUT_AFTER),
            (_, Some(start)) => partial(&served, start, "\"v1\""),
            (_, None) => full(&served, "\"v1\""),
        }
    }))
    .await;
    let h = harness();
    let mut transfer = Transfer::new(format!("{}/Items/resume-half/Download", server.base));

    let outcome = transfer.run(&h.cfg, None).await;

    assert_eq!(outcome, Outcome::Committed);
    assert_eq!(transfer.job().snapshot().state, DownloadState::Committed);
    let requests = server.requests();
    assert_eq!(requests.len(), 2, "one cut, one resumed request");
    assert_eq!(requests[0].range, None);
    let resume_range = requests[1].range.as_deref();
    assert_eq!(resume_range, Some("bytes=524288-"));
    assert_eq!(
        requests[1].range.as_deref().and_then(parse_range_start),
        Some(HALF)
    );
    assert_eq!(transfer.notice(), None, "no notice on a clean resume");

    let file = final_bytes(&h, &transfer);
    assert_eq!(
        file, body,
        "no byte before the offset is re-downloaded or altered"
    );
    let sha = sha256_hex(&file);
    assert_eq!(sha, sha256_hex(&body));
    println!(
        "M3-11 resume: cut after {CUT_AFTER} bytes received, resume Range {}, final sha256 {sha}",
        resume_range.unwrap_or("none")
    );
}

#[tokio::test]
async fn changed_etag_on_206_restarts_from_zero() {
    let v1 = content(TOTAL, 0);
    let v2 = content(TOTAL, 0x5a);
    let v2_served = v2.clone();
    let server = start(Arc::new(move |index, range_start| {
        match (index, range_start) {
            (0, _) => cut(&v1, "\"v1\"", CUT_AFTER),
            (1, Some(start)) => partial(&v2_served, start, "\"v2\""),
            _ => full(&v2_served, "\"v2\""),
        }
    }))
    .await;
    let h = harness();
    let mut transfer = Transfer::new(format!("{}/Items/resume-half/Download", server.base));

    let outcome = transfer.run(&h.cfg, None).await;

    assert_eq!(outcome, Outcome::Committed);
    let requests = server.requests();
    assert_eq!(requests.len(), 3, "cut, rejected 206, full restart");
    assert_eq!(requests[1].range.as_deref(), Some("bytes=524288-"));
    assert_eq!(requests[2].range, None, "restart from zero sends no Range");
    assert_eq!(transfer.notice(), Some(Notice::ResourceChanged));

    let file = final_bytes(&h, &transfer);
    assert_eq!(
        file, v2,
        "the file is the new resource, not old head plus new tail"
    );
    let sha = sha256_hex(&file);
    assert_eq!(sha, sha256_hex(&v2));
    println!(
        "M3-11 changed ETag: 206 rejected at offset {HALF}, restarted from zero, final sha256 {sha}"
    );
}
