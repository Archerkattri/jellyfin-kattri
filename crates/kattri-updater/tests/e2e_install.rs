//! End-to-end acceptance test for the update chain, offline.
//!
//! Runs the real sequence a release would take: a signed manifest is verified, the artifact is
//! downloaded through the bounded downloader, and the bytes are applied to a real
//! [`FsInstallStore`] on a temporary directory. Then it drives the failed-launch path to prove
//! rollback restores the previous build.
//!
//! Keys are derived at runtime from a label. They are TEST KEYS, never release keys, and no private
//! key is committed. The HTTP transport is a deterministic double.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use ed25519_dalek::{Signer, SigningKey};
use kattri_updater::apply::{
    InstallStore, LaunchOutcome, apply_verified_reader, on_launch, rollback,
};
use kattri_updater::download_file::{
    ArtifactFileDownloader, ArtifactFileTransport, ChunkSink, FileResponse,
};
use kattri_updater::{
    Channel, FsInstallStore, InstallState, KeyState, Keyring, UpdateError, open_envelope,
};
use reqwest::Url;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const ORIGIN: &str = "https://updates.example.org";
const ARTIFACT_URL: &str = "https://updates.example.org/dl/kattri-2.1.0-x64.msi";
const INSTALLED: &str = "2.0.0";
const OFFERED: &str = "2.1.0";
const NEW_BYTES: &[u8] = b"installer payload for the end-to-end update acceptance test";

fn test_key(label: &str) -> SigningKey {
    let seed: [u8; 32] =
        Sha256::digest(format!("kattri-updater e2e TEST KEY: {label}").as_bytes()).into();
    SigningKey::from_bytes(&seed)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn manifest_value() -> Value {
    json!({
        "schema_version": 1,
        "channel": "stable",
        "version": OFFERED,
        "sequence": 11,
        "published": "2026-11-21",
        "minimum_version": "1.0.0",
        "artifacts": [{
            "platform": "windows",
            "arch": "x64",
            "format": "msi",
            "url": ARTIFACT_URL,
            "sha256": hex(&Sha256::digest(NEW_BYTES)),
            "size_bytes": NEW_BYTES.len()
        }]
    })
}

fn verified_manifest() -> kattri_updater::VerifiedManifest {
    let key = test_key("release");
    let kid = "e2e-2026";
    let payload = manifest_value().to_string();
    let envelope = json!({
        "envelope_version": 1,
        "alg": "ed25519",
        "kid": kid,
        "payload": STANDARD.encode(payload.as_bytes()),
        "sig": STANDARD.encode(key.sign(payload.as_bytes()).to_bytes()),
    })
    .to_string();
    let keyring = Keyring::new(vec![(
        kid.to_owned(),
        key.verifying_key().to_bytes(),
        KeyState::Active,
    )])
    .expect("keyring");
    let state = InstallState::new(INSTALLED).expect("state");
    open_envelope(&keyring, &envelope, Channel::Stable, &state).expect("manifest verifies")
}

struct Fake;

impl ArtifactFileTransport for Fake {
    fn get<'a>(
        &'a self,
        url: &'a Url,
        _limit: u64,
        sink: &'a mut dyn ChunkSink,
    ) -> Pin<Box<dyn Future<Output = Result<FileResponse, UpdateError>> + Send + 'a>> {
        Box::pin(async move {
            for chunk in NEW_BYTES.chunks(7) {
                sink.push(chunk).await?;
            }
            Ok(FileResponse {
                status: 200,
                final_url: url.clone(),
                redirected: false,
            })
        })
    }
}

fn temp_dir(label: &str) -> std::path::PathBuf {
    let unique = format!(
        "kattri-updater-e2e-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    );
    let dir = std::env::temp_dir().join(unique);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

#[test]
fn a_verified_manifest_downloads_and_installs_then_rolls_back() {
    let dir = temp_dir("chain");
    let mut store = FsInstallStore::open(&dir).expect("store");

    // The running release is already installed.
    store
        .stage(kattri_updater::apply::Slot {
            version: INSTALLED.to_owned(),
            bytes: b"the currently installed build".to_vec(),
        })
        .expect("stage current");
    store.commit_staged().expect("commit current");
    assert_eq!(
        store.installed().expect("installed").map(|s| s.version),
        Some(INSTALLED.to_owned())
    );

    // Verify -> download -> verify bytes.
    let verified = verified_manifest();
    let downloader =
        ArtifactFileDownloader::with_transport(Arc::new(Fake), Url::parse(ORIGIN).expect("origin"))
            .expect("downloader");
    let staged = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime")
        .block_on(async {
            tokio::time::timeout(
                std::time::Duration::from_secs(5),
                downloader.download_to_file(&verified, "windows", "x64", &dir),
            )
            .await
            .expect("download deadline")
        })
        .expect("download");
    let mut corrupt = NEW_BYTES.to_vec();
    corrupt[0] ^= 1;
    assert!(matches!(
        apply_verified_reader(&mut store, &verified, "windows", "x64", corrupt.as_slice()),
        Err(kattri_updater::apply::ApplyError::Verify(
            UpdateError::DigestMismatch { .. }
        ))
    ));
    assert!(store.staged().expect("staging cleanup").is_none());
    assert!(store.journal().expect("journal").pending.is_none());
    assert_eq!(
        store.installed().expect("installed").expect("slot").version,
        INSTALLED
    );

    let artifact = std::fs::File::open(staged.path()).expect("open staged file");
    apply_verified_reader(&mut store, &verified, "windows", "x64", artifact).expect("apply");
    drop(staged);
    assert_eq!(
        store.installed().expect("installed").map(|s| s.version),
        Some(OFFERED.to_owned()),
        "the offered build must be committed"
    );
    assert_eq!(
        store.installed().expect("installed").map(|s| s.bytes),
        Some(NEW_BYTES.to_vec())
    );

    // The update is pending until it proves healthy. Two launches without a healthy marker must
    // offer a rollback, and only an explicit rollback restores the previous build.
    assert_eq!(
        on_launch(&mut store, OFFERED).expect("launch 1"),
        LaunchOutcome::HealthCheckPending {
            target: OFFERED.to_owned(),
            failed_launches: 1
        }
    );
    let second = on_launch(&mut store, OFFERED).expect("launch 2");
    assert!(
        matches!(second, LaunchOutcome::RollbackOffered { .. }),
        "unexpected {second:?}"
    );
    assert_eq!(
        store.installed().expect("installed").map(|s| s.version),
        Some(OFFERED.to_owned()),
        "the offered update must stay installed until the user asks to roll back"
    );
    rollback(&mut store).expect("rollback");
    assert_eq!(
        store.installed().expect("installed").map(|s| s.version),
        Some(INSTALLED.to_owned()),
        "rollback must restore the build that was installed before the update"
    );

    std::fs::remove_dir_all(&dir).ok();
}
