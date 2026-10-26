//! Ed25519 keyring: the public keys this client accepts for signed manifests.
//!
//! Key states (ADR 0004 section 4.4):
//! - `Active`: accepted for verification and used by the server to sign.
//! - `Pending`: accepted for verification, not yet used to sign (rotation preparation).
//! - `Retired`: refused. A manifest signed with it gets a specific error.
//! - `Revoked`: refused because the key is believed compromised. A manifest signed with it gets
//!   an error that directs the user to a manual download. The client does not try other keys.
//!
//! Only public keys live here. The private key never enters the repository or the client.

use ed25519_dalek::VerifyingKey;

use crate::UpdateError;

/// Lifecycle state of a trusted key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyState {
    Active,
    Pending,
    Retired,
    /// Compromised. Refused with [`UpdateError::RevokedKey`].
    Revoked,
}

/// A public key compiled into the binary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StaticKey {
    pub kid: &'static str,
    pub public: [u8; 32],
    pub state: KeyState,
}

/// Release public keys. Empty until the owner generates the release key (D3, M4-17).
///
/// An empty keyring refuses every manifest. That is the safe default for a build with no
/// release key yet.
const RELEASE_KEYS: &[StaticKey] = &[];

/// A set of verified public keys with unique ids and unique key bytes.
#[derive(Debug, Clone)]
pub struct Keyring {
    keys: Vec<Entry>,
}

#[derive(Debug, Clone)]
struct Entry {
    kid: String,
    key: VerifyingKey,
    state: KeyState,
}

impl Keyring {
    /// The keyring compiled into this build.
    pub fn release() -> Result<Self, UpdateError> {
        Self::from_static(RELEASE_KEYS)
    }

    /// Build a keyring from compile-time key entries.
    pub fn from_static(keys: &[StaticKey]) -> Result<Self, UpdateError> {
        Self::new(
            keys.iter()
                .map(|k| (k.kid.to_owned(), k.public, k.state))
                .collect(),
        )
    }

    /// Build a keyring, rejecting bad key ids, duplicate ids, duplicate key bytes and
    /// bytes that are not a valid Ed25519 public key.
    pub fn new(keys: Vec<(String, [u8; 32], KeyState)>) -> Result<Self, UpdateError> {
        let mut entries: Vec<Entry> = Vec::with_capacity(keys.len());
        for (kid, public, state) in keys {
            if !is_valid_kid(&kid) {
                return Err(keyring_error(
                    "a key id is empty or has disallowed characters",
                ));
            }
            if entries.iter().any(|e| e.kid == kid) {
                return Err(keyring_error("two keys share the same key id"));
            }
            let key = VerifyingKey::from_bytes(&public)
                .map_err(|_| keyring_error("a public key is not a valid Ed25519 point"))?;
            if entries.iter().any(|e| e.key == key) {
                return Err(keyring_error("two keys have the same public key bytes"));
            }
            entries.push(Entry { kid, key, state });
        }
        Ok(Self { keys: entries })
    }

    /// The verifying key for `kid`, if it may be used to verify a manifest now.
    ///
    /// Unknown ids and retired ids are errors. The caller must not try other keys.
    pub fn lookup(&self, kid: &str) -> Result<&VerifyingKey, UpdateError> {
        let entry =
            self.keys
                .iter()
                .find(|e| e.kid == kid)
                .ok_or_else(|| UpdateError::UnknownKey {
                    kid: kid.to_owned(),
                })?;
        match entry.state {
            KeyState::Active | KeyState::Pending => Ok(&entry.key),
            KeyState::Retired => Err(UpdateError::RetiredKey {
                kid: kid.to_owned(),
            }),
            KeyState::Revoked => Err(UpdateError::RevokedKey {
                kid: kid.to_owned(),
            }),
        }
    }

    /// Number of keys in the ring, of any state.
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    /// True when the ring holds no keys. A build like that refuses every manifest.
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// Number of keys in the `Active` state.
    pub fn active_count(&self) -> usize {
        self.keys
            .iter()
            .filter(|e| e.state == KeyState::Active)
            .count()
    }
}

fn is_valid_kid(kid: &str) -> bool {
    !kid.is_empty()
        && kid.len() <= 64
        && kid
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

fn keyring_error(reason: &str) -> UpdateError {
    UpdateError::Keyring(reason.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    use ed25519_dalek::SigningKey;
    use sha2::{Digest, Sha256};

    /// Public key of a test key derived at runtime from a label. Test-only, never a release key.
    fn public_of(label: &str) -> [u8; 32] {
        let seed: [u8; 32] =
            Sha256::digest(format!("kattri-updater unit TEST KEY: {label}").as_bytes()).into();
        SigningKey::from_bytes(&seed).verifying_key().to_bytes()
    }

    #[test]
    fn release_keyring_is_empty_and_refuses_lookups() {
        let ring = Keyring::release().unwrap();
        assert!(ring.is_empty());
        assert!(matches!(
            ring.lookup("kattri-update-2026a"),
            Err(UpdateError::UnknownKey { .. })
        ));
    }

    #[test]
    fn retired_key_is_refused_and_pending_key_is_accepted() {
        let ring = Keyring::new(vec![
            ("old".to_owned(), public_of("a"), KeyState::Retired),
            ("next".to_owned(), public_of("b"), KeyState::Pending),
            ("cur".to_owned(), public_of("c"), KeyState::Active),
        ])
        .unwrap();
        assert!(matches!(
            ring.lookup("old"),
            Err(UpdateError::RetiredKey { .. })
        ));
        assert!(ring.lookup("next").is_ok());
        assert!(ring.lookup("cur").is_ok());
        assert_eq!(ring.active_count(), 1);
    }

    #[test]
    fn revoked_key_is_refused_with_a_manual_download_error() {
        let ring = Keyring::new(vec![
            ("compromised".to_owned(), public_of("a"), KeyState::Revoked),
            ("cur".to_owned(), public_of("c"), KeyState::Active),
        ])
        .unwrap();
        match ring.lookup("compromised") {
            Err(err @ UpdateError::RevokedKey { .. }) => {
                let text = err.to_string();
                assert!(text.contains("compromised"));
                assert!(text.contains("download the current release manually"));
            }
            other => panic!("expected RevokedKey, got {other:?}"),
        }
        assert_eq!(ring.active_count(), 1);
    }

    #[test]
    fn duplicate_kid_is_rejected() {
        let result = Keyring::new(vec![
            ("k1".to_owned(), public_of("a"), KeyState::Active),
            ("k1".to_owned(), public_of("b"), KeyState::Pending),
        ]);
        assert!(matches!(result, Err(UpdateError::Keyring(_))));
    }

    #[test]
    fn duplicate_key_bytes_are_rejected() {
        let result = Keyring::new(vec![
            ("k1".to_owned(), public_of("a"), KeyState::Active),
            ("k2".to_owned(), public_of("a"), KeyState::Pending),
        ]);
        assert!(matches!(result, Err(UpdateError::Keyring(_))));
    }

    #[test]
    fn bad_kid_characters_are_rejected() {
        let key = public_of("a");
        for bad in ["", "has space", "slash/kid"] {
            let result = Keyring::new(vec![(bad.to_owned(), key, KeyState::Active)]);
            assert!(matches!(result, Err(UpdateError::Keyring(_))), "{bad:?}");
        }
    }
}
