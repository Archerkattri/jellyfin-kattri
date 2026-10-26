//! M2-03: Windows Credential Manager round-trip smoke test.
//!
//! Touches the real Windows Credential Manager, so it is `#[ignore]`d by default.
//! Run with:
//!
//! ```text
//! cargo test -p kattri-platform --test windows_credential_smoke -- --ignored --nocapture
//! ```
//!
//! It exercises the production `OsCredentialStore`. It uses a fixed, obviously fake service name and a random dummy secret generated
//! at runtime. It creates one entry, reads it back, compares, deletes it, and asserts
//! the entry is gone. No secret value is ever printed.

#![cfg(windows)]

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};

use kattri_platform::{CredentialKey, CredentialStore, OsCredentialStore, Secret};
use zeroize::Zeroizing;

const SMOKE_SERVICE: &str = "kattri-smoke-test";
const SMOKE_ACCOUNT: &str = "m2-03-round-trip";

/// 64 random hex characters, generated at runtime from std's OS-seeded hashers.
fn random_secret() -> Zeroizing<Vec<u8>> {
    let mut text = String::with_capacity(64);
    for _ in 0..4 {
        let mut hasher = RandomState::new().build_hasher();
        hasher.write_u64(std::process::id() as u64);
        text.push_str(&format!("{:016x}", hasher.finish()));
    }
    Zeroizing::new(text.into_bytes())
}

fn round_trip(store: &mut OsCredentialStore, key: &CredentialKey) -> Result<(), String> {
    let expected = random_secret();

    store
        .set(key, Secret::new(expected.to_vec()))
        .map_err(|e| format!("set failed: {e}"))?;
    println!("step 1: entry written");

    let read_back = store
        .get(key)
        .map_err(|e| format!("get failed: {e}"))?
        .ok_or_else(|| "entry not found right after write".to_string())?;
    // Compare without printing either value.
    if read_back.expose() != expected.as_slice() {
        return Err("read-back value does not match the written value".to_string());
    }
    println!("step 2: read back and compared, values match");
    Ok(())
}

#[test]
#[ignore = "touches Windows Credential Manager; run with --ignored"]
fn windows_credential_round_trip_leaves_no_entry() {
    let mut store = OsCredentialStore::open().expect("open Windows credential store");
    let key = CredentialKey {
        service: SMOKE_SERVICE.to_string(),
        account: SMOKE_ACCOUNT.to_string(),
    };

    // Clear any leftover entry from an interrupted earlier run (fake, test-only name).
    store.delete(&key).expect("pre-clean delete");

    let outcome = round_trip(&mut store, &key);

    // Always attempt cleanup, even when the round trip failed.
    let deleted = store.delete(&key);
    println!("step 3: delete returned {deleted:?}");

    let still_present = store.get(&key).expect("get after delete");
    assert!(
        still_present.is_none(),
        "smoke entry still present after delete"
    );
    println!("step 4: entry confirmed gone");

    outcome.expect("round trip");
    assert_eq!(
        deleted,
        Ok(true),
        "delete did not report the entry as removed"
    );
}
