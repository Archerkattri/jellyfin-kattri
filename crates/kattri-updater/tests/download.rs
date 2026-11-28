//! Acceptance tests for the bounded artifact download.
//!
//! Every key here is derived at runtime from a label. They are TEST KEYS, never release keys, and
//! no private key is committed. The HTTP transport is a deterministic double, so these tests run
//! offline and assert the rules the downloader enforces before any byte reaches `apply`.

use std::future::Future;
use std::pin::{Pin, pin};
use std::sync::Arc;
use std::task::{Context, Poll, Waker};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use ed25519_dalek::{Signer, SigningKey};
use kattri_updater::download::{
    ArtifactDownloader, ArtifactResponse, ArtifactTransport, MAX_IN_MEMORY_BYTES,
};
use kattri_updater::{Channel, InstallState, KeyState, Keyring, UpdateError, open_envelope};
use reqwest::Url;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const ORIGIN: &str = "https://updates.example.org";
const ARTIFACT_URL: &str = "https://updates.example.org/dl/kattri-x64.msi";
const ARTIFACT: &[u8] = b"installer bytes for the download acceptance tests";
const INSTALLED: &str = "2.0.0";

fn test_key(label: &str) -> SigningKey {
    let seed: [u8; 32] =
        Sha256::digest(format!("kattri-updater download TEST KEY: {label}").as_bytes()).into();
    SigningKey::from_bytes(&seed)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn artifact_sha() -> String {
    hex(&Sha256::digest(ARTIFACT))
}

fn manifest_value(url: &str, sha: &str, size: u64) -> Value {
    json!({
        "schema_version": 1,
        "channel": "stable",
        "version": "2.1.0",
        "sequence": 7,
        "published": "2026-11-21",
        "minimum_version": "1.0.0",
        "artifacts": [{
            "platform": "windows",
            "arch": "x64",
            "format": "msi",
            "url": url,
            "sha256": sha,
            "size_bytes": size
        }]
    })
}

fn ring_with(kid: &str, key: &SigningKey) -> Keyring {
    Keyring::new(vec![(
        kid.to_owned(),
        key.verifying_key().to_bytes(),
        KeyState::Active,
    )])
    .expect("test keyring")
}

fn verified(manifest: &Value) -> kattri_updater::VerifiedManifest {
    let key = test_key("release");
    let kid = "test-2026";
    let payload = manifest.to_string();
    let envelope = json!({
        "envelope_version": 1,
        "alg": "ed25519",
        "kid": kid,
        "payload": STANDARD.encode(payload.as_bytes()),
        "sig": STANDARD.encode(key.sign(payload.as_bytes()).to_bytes()),
    })
    .to_string();
    let state = InstallState::new(INSTALLED).expect("state");
    open_envelope(&ring_with(kid, &key), &envelope, Channel::Stable, &state).expect("verified")
}

/// Deterministic transport returning one canned response and recording the URL and limit it saw.
struct Fake {
    response: ArtifactResponse,
    seen_url: std::sync::Mutex<Option<String>>,
    seen_limit: std::sync::Mutex<Option<u64>>,
}

impl Fake {
    fn new(body: Vec<u8>, status: u16) -> Arc<Self> {
        Arc::new(Self {
            response: ArtifactResponse {
                status,
                final_url: Url::parse(ARTIFACT_URL).expect("url"),
                redirected: false,
                body,
            },
            seen_url: std::sync::Mutex::new(None),
            seen_limit: std::sync::Mutex::new(None),
        })
    }

    fn redirected() -> Arc<Self> {
        Arc::new(Self {
            response: ArtifactResponse {
                status: 200,
                final_url: Url::parse(ARTIFACT_URL).expect("url"),
                redirected: true,
                body: ARTIFACT.to_vec(),
            },
            seen_url: std::sync::Mutex::new(None),
            seen_limit: std::sync::Mutex::new(None),
        })
    }
}

impl ArtifactTransport for Fake {
    fn get<'a>(
        &'a self,
        url: &'a Url,
        limit: u64,
    ) -> Pin<Box<dyn Future<Output = Result<ArtifactResponse, UpdateError>> + Send + 'a>> {
        *self.seen_url.lock().expect("lock") = Some(url.to_string());
        *self.seen_limit.lock().expect("lock") = Some(limit);
        let response = self.response.clone();
        Box::pin(async move { Ok(response) })
    }
}

fn downloader(fake: Arc<Fake>) -> ArtifactDownloader {
    ArtifactDownloader::with_transport(fake, Url::parse(ORIGIN).expect("origin"))
        .expect("downloader")
}

/// The doubles below answer immediately, so a no-op waker is enough and the crate stays free of an
/// async runtime dependency.
fn block_on_ready<F: Future>(future: F) -> F::Output {
    let mut future = pin!(future);
    let mut context = Context::from_waker(Waker::noop());
    match future.as_mut().poll(&mut context) {
        Poll::Ready(output) => output,
        Poll::Pending => panic!("the fake transport unexpectedly returned a pending future"),
    }
}

fn fetch(downloader: &ArtifactDownloader, manifest: &Value) -> Result<Vec<u8>, UpdateError> {
    let verified = verified(manifest);
    block_on_ready(downloader.download(&verified, "windows", "x64"))
}

#[test]
fn a_matching_artifact_is_returned_and_the_declared_size_bounds_the_read() {
    let fake = Fake::new(ARTIFACT.to_vec(), 200);
    let created = downloader(Arc::clone(&fake));
    let bytes = fetch(
        &created,
        &manifest_value(ARTIFACT_URL, &artifact_sha(), ARTIFACT.len() as u64),
    )
    .expect("download");
    assert_eq!(bytes, ARTIFACT);
    assert_eq!(
        *fake.seen_limit.lock().expect("lock"),
        Some(ARTIFACT.len() as u64),
        "the transport must be told the manifest's size, not an unbounded read"
    );
    assert_eq!(
        fake.seen_url.lock().expect("lock").as_deref(),
        Some(ARTIFACT_URL)
    );
}

#[test]
fn a_digest_mismatch_is_refused() {
    let wrong = hex(&Sha256::digest(b"different bytes"));
    let fake = Fake::new(ARTIFACT.to_vec(), 200);
    let created = downloader(fake);
    let error = fetch(
        &created,
        &manifest_value(ARTIFACT_URL, &wrong, ARTIFACT.len() as u64),
    )
    .expect_err("digest mismatch");
    assert!(
        matches!(error, UpdateError::DigestMismatch { .. }),
        "unexpected {error:?}"
    );
}

#[test]
fn a_size_mismatch_is_refused() {
    let fake = Fake::new(ARTIFACT.to_vec(), 200);
    let created = downloader(fake);
    let declared = ARTIFACT.len() as u64 + 10;
    let error = fetch(
        &created,
        &manifest_value(ARTIFACT_URL, &artifact_sha(), declared),
    )
    .expect_err("size mismatch");
    assert!(
        matches!(error, UpdateError::SizeMismatch { .. }),
        "unexpected {error:?}"
    );
}

#[test]
fn an_artifact_on_another_origin_is_refused_before_any_request() {
    let fake = Fake::new(ARTIFACT.to_vec(), 200);
    let created = downloader(Arc::clone(&fake));
    let elsewhere = "https://evil.example.net/dl/kattri-x64.msi";
    let error = fetch(
        &created,
        &manifest_value(elsewhere, &artifact_sha(), ARTIFACT.len() as u64),
    )
    .expect_err("foreign origin");
    assert_eq!(error, UpdateError::ArtifactOrigin);
    assert!(
        fake.seen_url.lock().expect("lock").is_none(),
        "the request must not be attempted"
    );
}

#[test]
fn a_redirect_is_refused() {
    let created = downloader(Fake::redirected());
    let error = fetch(
        &created,
        &manifest_value(ARTIFACT_URL, &artifact_sha(), ARTIFACT.len() as u64),
    )
    .expect_err("redirect");
    assert_eq!(error, UpdateError::ArtifactRedirect);
}

#[test]
fn a_non_success_status_is_refused() {
    let created = downloader(Fake::new(Vec::new(), 404));
    let error = fetch(
        &created,
        &manifest_value(ARTIFACT_URL, &artifact_sha(), ARTIFACT.len() as u64),
    )
    .expect_err("status");
    assert_eq!(error, UpdateError::ArtifactHttpStatus { status: 404 });
}

#[test]
fn a_missing_platform_or_arch_is_refused() {
    let created = downloader(Fake::new(ARTIFACT.to_vec(), 200));
    let verified = verified(&manifest_value(
        ARTIFACT_URL,
        &artifact_sha(),
        ARTIFACT.len() as u64,
    ));
    let error =
        block_on_ready(created.download(&verified, "linux", "arm64")).expect_err("no artifact");
    assert!(
        matches!(error, UpdateError::NoArtifactForPlatform { .. }),
        "unexpected {error:?}"
    );
}

#[test]
fn an_artifact_above_the_in_memory_floor_asks_for_a_manual_install() {
    let fake = Fake::new(Vec::new(), 200);
    let created = downloader(Arc::clone(&fake));
    let huge = MAX_IN_MEMORY_BYTES + 1;
    let error = fetch(
        &created,
        &manifest_value(ARTIFACT_URL, &artifact_sha(), huge),
    )
    .expect_err("too large");
    assert_eq!(
        error,
        UpdateError::ArtifactTooLargeToBuffer {
            declared: huge,
            limit: MAX_IN_MEMORY_BYTES
        }
    );
    assert!(
        fake.seen_url.lock().expect("lock").is_none(),
        "nothing may be downloaded for a refused size"
    );
}
