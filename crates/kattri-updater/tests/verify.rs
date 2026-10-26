//! M4-15 acceptance tests for signed-manifest verification.
//!
//! All keys are derived at runtime from labels in this file. They are TEST KEYS, never
//! release keys, and no private key is committed.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use ed25519_dalek::{Signer, SigningKey};
use kattri_updater::manifest::MAX_ARTIFACT_BYTES;
use kattri_updater::{
    Channel, InstallState, KeyState, Keyring, UpdateError, open_envelope, verify_artifact_bytes,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const INSTALLED: &str = "2.0.0";
const ARTIFACT: &[u8] = b"fake installer bytes for the M4-15 tests";

/// A test-only signing key derived from a label. Never a release key.
fn test_key(label: &str) -> SigningKey {
    let seed: [u8; 32] =
        Sha256::digest(format!("kattri-updater integration TEST KEY: {label}").as_bytes()).into();
    SigningKey::from_bytes(&seed)
}

fn public_bytes(key: &SigningKey) -> [u8; 32] {
    key.verifying_key().to_bytes()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn artifact_sha() -> String {
    hex(&Sha256::digest(ARTIFACT))
}

fn manifest_value(version: &str, sequence: u64, channel: &str) -> Value {
    json!({
        "schema_version": 1,
        "channel": channel,
        "version": version,
        "sequence": sequence,
        "published": "2026-10-15",
        "minimum_version": "1.0.0",
        "artifacts": [{
            "platform": "windows",
            "arch": "x64",
            "format": "msi",
            "url": "https://updates.example.org/dl/kattri-x64.msi",
            "sha256": artifact_sha(),
            "size_bytes": ARTIFACT.len()
        }]
    })
}

/// Build an envelope signed by `signer` over exactly `payload`.
fn envelope_over(signer: &SigningKey, kid: &str, payload: &[u8]) -> String {
    let sig = signer.sign(payload).to_bytes();
    json!({
        "envelope_version": 1,
        "alg": "ed25519",
        "kid": kid,
        "payload": STANDARD.encode(payload),
        "sig": STANDARD.encode(sig),
    })
    .to_string()
}

fn signed_envelope(signer: &SigningKey, kid: &str, manifest: &Value) -> String {
    envelope_over(signer, kid, manifest.to_string().as_bytes())
}

/// Keyring with one active test key under `kid`.
fn ring_with(kid: &str, key: &SigningKey) -> Keyring {
    Keyring::new(vec![(kid.to_owned(), public_bytes(key), KeyState::Active)]).unwrap()
}

fn fresh_state() -> InstallState {
    InstallState::new(INSTALLED).unwrap()
}

/// Replace the envelope's payload with `new_payload`, keeping the old signature.
fn with_payload(envelope: &str, new_payload: &[u8]) -> String {
    let mut v: Value = serde_json::from_str(envelope).unwrap();
    v["payload"] = Value::String(STANDARD.encode(new_payload));
    v.to_string()
}

fn with_sig(envelope: &str, sig: &[u8]) -> String {
    let mut v: Value = serde_json::from_str(envelope).unwrap();
    v["sig"] = Value::String(STANDARD.encode(sig));
    v.to_string()
}

fn decoded_payload(envelope: &str) -> Vec<u8> {
    let v: Value = serde_json::from_str(envelope).unwrap();
    STANDARD.decode(v["payload"].as_str().unwrap()).unwrap()
}

fn decoded_sig(envelope: &str) -> Vec<u8> {
    let v: Value = serde_json::from_str(envelope).unwrap();
    STANDARD.decode(v["sig"].as_str().unwrap()).unwrap()
}

#[test]
fn valid_signed_manifest_is_accepted() {
    let key = test_key("release-like");
    let ring = ring_with("test-2026a", &key);
    let env = signed_envelope(&key, "test-2026a", &manifest_value("2.1.0", 5, "stable"));
    let verified = open_envelope(&ring, &env, Channel::Stable, &fresh_state()).unwrap();
    assert_eq!(verified.manifest().version, "2.1.0");
    assert_eq!(verified.manifest().sequence, 5);
    assert_eq!(verified.kid(), "test-2026a");
}

#[test]
fn roadmap_bad_signature_leaves_installed_version_unchanged() {
    // The ROADMAP criterion: a bad-signature manifest is refused and the installed
    // version is unchanged.
    let key = test_key("signer");
    let attacker = test_key("attacker");
    let ring = ring_with("test-2026a", &key);
    let state = fresh_state();
    let before = state.clone();

    // Valid-looking manifest, signed by a key that is not in the ring.
    let env = signed_envelope(
        &attacker,
        "test-2026a",
        &manifest_value("9.9.9", 1, "stable"),
    );
    let result = open_envelope(&ring, &env, Channel::Stable, &state);

    assert!(matches!(result, Err(UpdateError::VerificationFailed(_))));
    assert_eq!(state, before);
    assert_eq!(state.installed_version().to_string(), INSTALLED);
}

#[test]
fn single_flipped_bit_in_payload_is_rejected() {
    let key = test_key("signer");
    let ring = ring_with("test-2026a", &key);
    let env = signed_envelope(&key, "test-2026a", &manifest_value("2.1.0", 5, "stable"));
    let mut payload = decoded_payload(&env);
    payload[10] ^= 0x01;
    let tampered = with_payload(&env, &payload);
    let state = fresh_state();
    let before = state.clone();
    let result = open_envelope(&ring, &tampered, Channel::Stable, &state);
    assert!(matches!(result, Err(UpdateError::VerificationFailed(_))));
    assert_eq!(state, before);
}

#[test]
fn single_flipped_bit_in_signature_is_rejected() {
    let key = test_key("signer");
    let ring = ring_with("test-2026a", &key);
    let env = signed_envelope(&key, "test-2026a", &manifest_value("2.1.0", 5, "stable"));
    let mut sig = decoded_sig(&env);
    sig[0] ^= 0x80;
    let tampered = with_sig(&env, &sig);
    assert!(matches!(
        open_envelope(&ring, &tampered, Channel::Stable, &fresh_state()),
        Err(UpdateError::VerificationFailed(_))
    ));
}

#[test]
fn wrong_key_with_known_kid_is_rejected() {
    let trusted = test_key("trusted");
    let other = test_key("other");
    let ring = ring_with("test-2026a", &trusted);
    let env = signed_envelope(&other, "test-2026a", &manifest_value("2.1.0", 5, "stable"));
    assert!(matches!(
        open_envelope(&ring, &env, Channel::Stable, &fresh_state()),
        Err(UpdateError::VerificationFailed(_))
    ));
}

#[test]
fn unknown_kid_is_rejected_without_trying_other_keys() {
    let key = test_key("signer");
    let ring = ring_with("test-2026a", &key);
    let env = signed_envelope(&key, "test-2099z", &manifest_value("2.1.0", 5, "stable"));
    assert!(matches!(
        open_envelope(&ring, &env, Channel::Stable, &fresh_state()),
        Err(UpdateError::UnknownKey { kid }) if kid == "test-2099z"
    ));
}

#[test]
fn retired_key_is_rejected_with_specific_error() {
    let key = test_key("old-signer");
    let ring = Keyring::new(vec![(
        "test-2025a".to_owned(),
        public_bytes(&key),
        KeyState::Retired,
    )])
    .unwrap();
    let env = signed_envelope(&key, "test-2025a", &manifest_value("2.1.0", 5, "stable"));
    let err = open_envelope(&ring, &env, Channel::Stable, &fresh_state()).unwrap_err();
    assert!(matches!(err, UpdateError::RetiredKey { .. }));
    assert!(
        err.to_string()
            .contains("install the current release manually")
    );
}

#[test]
fn revoked_key_is_rejected_with_a_manual_download_error() {
    let key = test_key("compromised-signer");
    let ring = Keyring::new(vec![(
        "test-2024a".to_owned(),
        public_bytes(&key),
        KeyState::Revoked,
    )])
    .unwrap();
    let env = signed_envelope(&key, "test-2024a", &manifest_value("2.1.0", 5, "stable"));
    let err = open_envelope(&ring, &env, Channel::Stable, &fresh_state()).unwrap_err();
    assert!(matches!(err, UpdateError::RevokedKey { .. }));
    let text = err.to_string();
    assert!(text.contains("test-2024a"));
    assert!(text.contains("download the current release manually"));
    // The message carries the key id and the instruction, and nothing about the payload.
    assert!(!text.contains("2.1.0"));
}

#[test]
fn pending_key_is_accepted_for_verification() {
    let key = test_key("next-signer");
    let ring = Keyring::new(vec![(
        "test-2026b".to_owned(),
        public_bytes(&key),
        KeyState::Pending,
    )])
    .unwrap();
    let env = signed_envelope(&key, "test-2026b", &manifest_value("2.1.0", 5, "stable"));
    assert!(open_envelope(&ring, &env, Channel::Stable, &fresh_state()).is_ok());
}

#[test]
fn signature_of_wrong_length_is_rejected() {
    let key = test_key("signer");
    let ring = ring_with("test-2026a", &key);
    let env = signed_envelope(&key, "test-2026a", &manifest_value("2.1.0", 5, "stable"));
    let sig = decoded_sig(&env);
    for bad_len in [63usize, 65] {
        let mut bad = sig.clone();
        bad.resize(bad_len, 0);
        let tampered = with_sig(&env, &bad);
        assert!(
            matches!(
                open_envelope(&ring, &tampered, Channel::Stable, &fresh_state()),
                Err(UpdateError::Envelope(_))
            ),
            "length {bad_len}"
        );
    }
}

#[test]
fn malformed_or_non_canonical_base64_is_rejected() {
    let key = test_key("signer");
    let ring = ring_with("test-2026a", &key);
    let env = signed_envelope(&key, "test-2026a", &manifest_value("2.1.0", 5, "stable"));

    // Invalid character.
    let mut v: Value = serde_json::from_str(&env).unwrap();
    v["sig"] = Value::String("!!not base64!!".to_owned());
    assert!(matches!(
        open_envelope(&ring, &v.to_string(), Channel::Stable, &fresh_state()),
        Err(UpdateError::Envelope(_))
    ));

    // Canonical padding removed from a 64-byte signature (88 chars ending in "==").
    let good_sig = STANDARD.encode(decoded_sig(&env));
    assert!(good_sig.ends_with("=="));
    let mut v2: Value = serde_json::from_str(&env).unwrap();
    v2["sig"] = Value::String(good_sig.trim_end_matches('=').to_owned());
    assert!(matches!(
        open_envelope(&ring, &v2.to_string(), Channel::Stable, &fresh_state()),
        Err(UpdateError::Envelope(_))
    ));
}

#[test]
fn envelope_over_64_kib_is_rejected_before_parsing() {
    let key = test_key("signer");
    let ring = ring_with("test-2026a", &key);
    let huge = " ".repeat(64 * 1024 + 1);
    assert!(matches!(
        open_envelope(&ring, &huge, Channel::Stable, &fresh_state()),
        Err(UpdateError::TooLarge { .. })
    ));
}

#[test]
fn unsupported_algorithm_and_envelope_version_are_rejected() {
    let key = test_key("signer");
    let ring = ring_with("test-2026a", &key);
    let env = signed_envelope(&key, "test-2026a", &manifest_value("2.1.0", 5, "stable"));
    for (field, value) in [("alg", json!("rsa")), ("envelope_version", json!(2))] {
        let mut v: Value = serde_json::from_str(&env).unwrap();
        v[field] = value;
        assert!(matches!(
            open_envelope(&ring, &v.to_string(), Channel::Stable, &fresh_state()),
            Err(UpdateError::Envelope(_))
        ));
    }
}

#[test]
fn downgrade_is_rejected_even_with_a_valid_signature() {
    let key = test_key("signer");
    let ring = ring_with("test-2026a", &key);
    let env = signed_envelope(&key, "test-2026a", &manifest_value("1.9.0", 99, "stable"));
    assert!(matches!(
        open_envelope(&ring, &env, Channel::Stable, &fresh_state()),
        Err(UpdateError::Downgrade { .. })
    ));
}

#[test]
fn replayed_lower_sequence_is_rejected() {
    let key = test_key("signer");
    let ring = ring_with("test-2026a", &key);
    let mut state = fresh_state();

    let newer = signed_envelope(&key, "test-2026a", &manifest_value("2.1.0", 10, "stable"));
    let accepted = open_envelope(&ring, &newer, Channel::Stable, &state).unwrap();
    state.record(&accepted);

    let older = signed_envelope(&key, "test-2026a", &manifest_value("2.2.0", 9, "stable"));
    let before = state.clone();
    assert!(matches!(
        open_envelope(&ring, &older, Channel::Stable, &state),
        Err(UpdateError::Replay {
            sequence: 9,
            last_accepted: 10
        })
    ));
    assert_eq!(state, before);
}

#[test]
fn identical_recheck_is_accepted_and_different_bytes_at_same_sequence_are_not() {
    let key = test_key("signer");
    let ring = ring_with("test-2026a", &key);
    let mut state = fresh_state();

    let env = signed_envelope(&key, "test-2026a", &manifest_value("2.1.0", 10, "stable"));
    let first = open_envelope(&ring, &env, Channel::Stable, &state).unwrap();
    state.record(&first);

    let again = open_envelope(&ring, &env, Channel::Stable, &state).unwrap();
    assert_eq!(again, first);

    let other = signed_envelope(&key, "test-2026a", &manifest_value("2.1.1", 10, "stable"));
    assert!(matches!(
        open_envelope(&ring, &other, Channel::Stable, &state),
        Err(UpdateError::SequenceConflict { sequence: 10 })
    ));
}

#[test]
fn replay_defense_is_per_channel() {
    let key = test_key("signer");
    let ring = ring_with("test-2026a", &key);
    let mut state = fresh_state();
    let stable = signed_envelope(&key, "test-2026a", &manifest_value("2.1.0", 10, "stable"));
    let accepted = open_envelope(&ring, &stable, Channel::Stable, &state).unwrap();
    state.record(&accepted);

    // A nightly manifest with a lower sequence is not a replay of the stable counter.
    let nightly = signed_envelope(
        &key,
        "test-2026a",
        &manifest_value("2.1.0-beta.1", 3, "nightly"),
    );
    assert!(open_envelope(&ring, &nightly, Channel::Nightly, &state).is_ok());
}

#[test]
fn channel_mismatch_is_rejected() {
    let key = test_key("signer");
    let ring = ring_with("test-2026a", &key);
    let env = signed_envelope(&key, "test-2026a", &manifest_value("2.1.0", 5, "nightly"));
    assert!(matches!(
        open_envelope(&ring, &env, Channel::Stable, &fresh_state()),
        Err(UpdateError::ChannelMismatch { .. })
    ));
}

#[test]
fn signed_manifest_with_http_artifact_url_is_rejected() {
    let key = test_key("signer");
    let ring = ring_with("test-2026a", &key);
    let mut m = manifest_value("2.1.0", 5, "stable");
    m["artifacts"][0]["url"] = json!("http://updates.example.org/dl/kattri-x64.msi");
    let env = signed_envelope(&key, "test-2026a", &m);
    assert!(matches!(
        open_envelope(&ring, &env, Channel::Stable, &fresh_state()),
        Err(UpdateError::Invalid {
            field: "artifacts.url",
            ..
        })
    ));
}

#[test]
fn signed_manifest_with_size_over_cap_is_rejected() {
    let key = test_key("signer");
    let ring = ring_with("test-2026a", &key);
    let mut m = manifest_value("2.1.0", 5, "stable");
    m["artifacts"][0]["size_bytes"] = json!(MAX_ARTIFACT_BYTES + 1);
    let env = signed_envelope(&key, "test-2026a", &m);
    assert!(matches!(
        open_envelope(&ring, &env, Channel::Stable, &fresh_state()),
        Err(UpdateError::Invalid {
            field: "artifacts.size_bytes",
            ..
        })
    ));
}

#[test]
fn unsupported_manifest_schema_is_rejected() {
    let key = test_key("signer");
    let ring = ring_with("test-2026a", &key);
    let mut m = manifest_value("2.1.0", 5, "stable");
    m["schema_version"] = json!(2);
    let env = signed_envelope(&key, "test-2026a", &m);
    assert!(matches!(
        open_envelope(&ring, &env, Channel::Stable, &fresh_state()),
        Err(UpdateError::Invalid {
            field: "schema_version",
            ..
        })
    ));
}

#[test]
fn artifact_with_matching_digest_and_size_is_accepted() {
    let key = test_key("signer");
    let ring = ring_with("test-2026a", &key);
    let env = signed_envelope(&key, "test-2026a", &manifest_value("2.1.0", 5, "stable"));
    let verified = open_envelope(&ring, &env, Channel::Stable, &fresh_state()).unwrap();
    let artifact = verified.manifest().artifact_for("windows", "x64").unwrap();
    assert!(verify_artifact_bytes(artifact, ARTIFACT).is_ok());
}

#[test]
fn artifact_digest_mismatch_is_rejected() {
    let key = test_key("signer");
    let ring = ring_with("test-2026a", &key);
    let env = signed_envelope(&key, "test-2026a", &manifest_value("2.1.0", 5, "stable"));
    let verified = open_envelope(&ring, &env, Channel::Stable, &fresh_state()).unwrap();
    let artifact = verified.manifest().artifact_for("windows", "x64").unwrap();

    // Same length, one byte different.
    let mut tampered = ARTIFACT.to_vec();
    tampered[0] ^= 0x20;
    assert!(matches!(
        verify_artifact_bytes(artifact, &tampered),
        Err(UpdateError::DigestMismatch { .. })
    ));
}

#[test]
fn artifact_size_mismatch_is_rejected() {
    let key = test_key("signer");
    let ring = ring_with("test-2026a", &key);
    let env = signed_envelope(&key, "test-2026a", &manifest_value("2.1.0", 5, "stable"));
    let verified = open_envelope(&ring, &env, Channel::Stable, &fresh_state()).unwrap();
    let artifact = verified.manifest().artifact_for("windows", "x64").unwrap();
    let mut longer = ARTIFACT.to_vec();
    longer.push(0);
    assert!(matches!(
        verify_artifact_bytes(artifact, &longer),
        Err(UpdateError::SizeMismatch { .. })
    ));
}

#[test]
fn empty_release_keyring_refuses_every_manifest() {
    let key = test_key("signer");
    let ring = Keyring::release().unwrap();
    assert!(ring.is_empty());
    let env = signed_envelope(
        &key,
        "kattri-update-2026a",
        &manifest_value("2.1.0", 5, "stable"),
    );
    assert!(matches!(
        open_envelope(&ring, &env, Channel::Stable, &fresh_state()),
        Err(UpdateError::UnknownKey { .. })
    ));
}

#[test]
fn accepted_manifest_advances_state_only_through_record() {
    let key = test_key("signer");
    let ring = ring_with("test-2026a", &key);
    let mut state = fresh_state();
    let env = signed_envelope(&key, "test-2026a", &manifest_value("2.1.0", 7, "stable"));
    let verified = open_envelope(&ring, &env, Channel::Stable, &state).unwrap();

    // Opening alone does not change state.
    assert!(state.accepted(Channel::Stable).is_none());
    state.record(&verified);
    assert_eq!(state.accepted(Channel::Stable).map(|a| a.sequence), Some(7));
    // The installed version is not changed by verification.
    assert_eq!(state.installed_version().to_string(), INSTALLED);
}
