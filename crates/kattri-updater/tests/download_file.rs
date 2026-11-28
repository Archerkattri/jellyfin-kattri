use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use ed25519_dalek::{Signer, SigningKey};
use kattri_updater::download_file::{
    ArtifactFileDownloader, ArtifactFileTransport, ChunkSink, FileResponse,
};
use kattri_updater::{Channel, InstallState, KeyState, Keyring, UpdateError, open_envelope};
use reqwest::Url;
use serde_json::json;
use sha2::{Digest, Sha256};

const ORIGIN: &str = "https://updates.example.org";
const ARTIFACT_URL: &str = "https://updates.example.org/releases/kattri.bin";
const BODY: &[u8] = b"streamed artifact bytes";

fn test_key() -> SigningKey {
    let seed: [u8; 32] = Sha256::digest(b"kattri download-file TEST KEY only").into();
    SigningKey::from_bytes(&seed)
}

fn verified(sha: &str, size: usize) -> kattri_updater::VerifiedManifest {
    let key = test_key();
    let payload = json!({
        "schema_version": 1,
        "channel": "stable",
        "version": "2.1.0",
        "sequence": 1,
        "published": "2026-11-21",
        "artifacts": [{
            "platform": "windows", "arch": "x64", "format": "msi",
            "url": ARTIFACT_URL, "sha256": sha, "size_bytes": size
        }]
    })
    .to_string();
    let envelope = json!({
        "envelope_version": 1,
        "alg": "ed25519",
        "kid": "file-test",
        "payload": STANDARD.encode(payload.as_bytes()),
        "sig": STANDARD.encode(key.sign(payload.as_bytes()).to_bytes())
    })
    .to_string();
    let ring = Keyring::new(vec![(
        "file-test".to_owned(),
        key.verifying_key().to_bytes(),
        KeyState::Active,
    )])
    .expect("keyring");
    open_envelope(
        &ring,
        &envelope,
        Channel::Stable,
        &InstallState::new("2.0.0").expect("state"),
    )
    .expect("verified manifest")
}

struct Fake {
    chunks: Vec<Vec<u8>>,
    status: u16,
    redirected: bool,
}

impl ArtifactFileTransport for Fake {
    fn get<'a>(
        &'a self,
        url: &'a Url,
        _limit: u64,
        sink: &'a mut dyn ChunkSink,
    ) -> Pin<Box<dyn Future<Output = Result<FileResponse, UpdateError>> + Send + 'a>> {
        Box::pin(async move {
            for chunk in &self.chunks {
                sink.push(chunk).await?;
            }
            Ok(FileResponse {
                status: self.status,
                final_url: url.clone(),
                redirected: self.redirected,
            })
        })
    }
}

fn downloader(chunks: Vec<Vec<u8>>) -> ArtifactFileDownloader {
    ArtifactFileDownloader::with_transport(
        Arc::new(Fake {
            chunks,
            status: 200,
            redirected: false,
        }),
        Url::parse(ORIGIN).expect("origin"),
    )
    .expect("downloader")
}

fn block_on_ready<F: Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("test runtime")
        .block_on(async {
            tokio::time::timeout(Duration::from_secs(5), future)
                .await
                .expect("writer completed")
        })
}

fn temp_dir() -> std::path::PathBuf {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "kattri-download-file-test-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::create_dir(&path).expect("temp directory");
    path
}

fn sha(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[test]
fn streams_chunks_and_guard_removes_staged_file_on_drop() {
    let dir = temp_dir();
    let verified = verified(&sha(BODY), BODY.len());
    let staged = block_on_ready(
        downloader(vec![BODY[..5].to_vec(), BODY[5..].to_vec()])
            .download_to_file(&verified, "windows", "x64", &dir),
    )
    .expect("download");
    assert_eq!(std::fs::read(staged.path()).expect("read stage"), BODY);
    let path = staged.path().to_owned();
    drop(staged);
    assert!(!path.exists(), "unpersisted staged file must be removed");
    std::fs::remove_dir_all(dir).expect("remove directory");
}

#[test]
fn dropping_inflight_download_eventually_cleans_staging() {
    struct Stalls;
    impl ArtifactFileTransport for Stalls {
        fn get<'a>(
            &'a self,
            _url: &'a Url,
            _limit: u64,
            sink: &'a mut dyn ChunkSink,
        ) -> Pin<Box<dyn Future<Output = Result<FileResponse, UpdateError>> + Send + 'a>> {
            Box::pin(async move {
                sink.push(BODY).await?;
                std::future::pending().await
            })
        }
    }
    let dir = temp_dir();
    let manifest = verified(&sha(BODY), BODY.len());
    let downloader = ArtifactFileDownloader::with_transport(
        Arc::new(Stalls),
        Url::parse(ORIGIN).expect("origin"),
    )
    .expect("downloader");
    block_on_ready(async {
        let mut download =
            std::pin::pin!(downloader.download_to_file(&manifest, "windows", "x64", &dir));
        loop {
            tokio::select! {
                result = &mut download => panic!("stalled download completed: {result:?}"),
                () = tokio::time::sleep(Duration::from_millis(5)) => {
                    if std::fs::read_dir(&dir).expect("staging directory").next().is_some() { break; }
                }
            }
        }
    });
    block_on_ready(async {
        while std::fs::read_dir(&dir)
            .expect("staging directory")
            .next()
            .is_some()
        {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    });
    std::fs::remove_dir(dir).expect("staging cleared");
}

#[test]
fn rejects_short_body_and_cleans_temporary_file() {
    let dir = temp_dir();
    let verified = verified(&sha(BODY), BODY.len());
    let error = block_on_ready(
        downloader(vec![BODY[..4].to_vec()]).download_to_file(&verified, "windows", "x64", &dir),
    )
    .expect_err("short body");
    assert!(matches!(error, UpdateError::SizeMismatch { .. }));
    assert_eq!(std::fs::read_dir(&dir).expect("read directory").count(), 0);
    std::fs::remove_dir_all(dir).expect("remove directory");
}

#[test]
fn rejects_overrun_before_queueing_it_and_cleans_temporary_file() {
    let dir = temp_dir();
    let verified = verified(&sha(BODY), BODY.len() - 1);
    let error = block_on_ready(
        downloader(vec![BODY.to_vec()]).download_to_file(&verified, "windows", "x64", &dir),
    )
    .expect_err("overrun");
    assert_eq!(
        error,
        UpdateError::ArtifactTooLarge {
            limit: (BODY.len() - 1) as u64
        }
    );
    assert_eq!(std::fs::read_dir(&dir).expect("read directory").count(), 0);
    std::fs::remove_dir_all(dir).expect("remove directory");
}

#[test]
fn rejects_digest_mismatch_and_cleans_temporary_file() {
    let dir = temp_dir();
    let verified = verified(&sha(b"different bytes"), BODY.len());
    let error = block_on_ready(
        downloader(vec![BODY.to_vec()]).download_to_file(&verified, "windows", "x64", &dir),
    )
    .expect_err("digest mismatch");
    assert!(matches!(error, UpdateError::DigestMismatch { .. }));
    assert_eq!(std::fs::read_dir(&dir).expect("read directory").count(), 0);
    std::fs::remove_dir_all(dir).expect("remove directory");
}
