//! The operating system's credential store behind [`CredentialStore`].
//!
//! Windows uses Credential Manager, macOS the Keychain, and Linux the Secret Service over
//! D-Bus, all through `keyring-core` (ADR 0003). Where no store can be opened (for example
//! Linux without a running Secret Service) [`OsCredentialStore::open`] returns an error and
//! the caller keeps the token in memory only. Nothing here ever falls back to a file.
//!
//! Error values are built from fixed text or the platform's own message, never from the stored
//! bytes, so a failure cannot put a secret into a log.

use std::sync::{Mutex, OnceLock};

use crate::{CredentialKey, CredentialStore, PlatformError, Secret};

/// Handle to the process-wide OS credential store. Cheap to create after the first `open`.
#[derive(Debug)]
pub struct OsCredentialStore {
    _private: (),
}

/// Set once the platform store has been installed successfully. A failure is deliberately not
/// cached: a locked keyring or a Secret Service that starts after the app can succeed later.
static INSTALLED: OnceLock<()> = OnceLock::new();
static INSTALL_LOCK: Mutex<()> = Mutex::new(());

impl OsCredentialStore {
    /// Opens the OS store, installing the platform backend on first success. A failed install
    /// is retried on the next call so a transient startup failure does not disable credential
    /// persistence for the whole process. Errors with [`PlatformError::Unsupported`] on a
    /// platform without a backend.
    pub fn open() -> Result<Self, PlatformError> {
        if INSTALLED.get().is_none() {
            let _guard = INSTALL_LOCK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if INSTALLED.get().is_none() {
                install_platform_store()?;
                let _ = INSTALLED.set(());
            }
        }
        Ok(Self { _private: () })
    }

    /// For tests: a handle on whatever default `keyring-core` store the test installed.
    #[cfg(test)]
    fn on_installed_default() -> Result<Self, PlatformError> {
        keyring_core::get_default_store()
            .map(|_| Self { _private: () })
            .ok_or(PlatformError::Unsupported("credential store"))
    }

    fn entry(key: &CredentialKey) -> Result<keyring_core::Entry, PlatformError> {
        keyring_core::Entry::new(&key.service, &key.account).map_err(map_error)
    }
}

impl CredentialStore for OsCredentialStore {
    fn get(&self, key: &CredentialKey) -> Result<Option<Secret>, PlatformError> {
        match Self::entry(key)?.get_secret() {
            Ok(bytes) => Ok(Some(Secret::new(bytes))),
            Err(keyring_core::Error::NoEntry) => Ok(None),
            Err(error) => Err(map_error(error)),
        }
    }

    fn set(&mut self, key: &CredentialKey, secret: Secret) -> Result<(), PlatformError> {
        Self::entry(key)?
            .set_secret(secret.expose())
            .map_err(map_error)
    }

    fn delete(&mut self, key: &CredentialKey) -> Result<bool, PlatformError> {
        match Self::entry(key)?.delete_credential() {
            Ok(()) => Ok(true),
            Err(keyring_core::Error::NoEntry) => Ok(false),
            Err(error) => Err(map_error(error)),
        }
    }
}

/// Maps a `keyring-core` error. Variants that carry raw secret bytes are reduced to fixed text.
fn map_error(error: keyring_core::Error) -> PlatformError {
    use keyring_core::Error;
    match error {
        Error::NoEntry => PlatformError::NotFound,
        Error::NoStorageAccess(cause) => PlatformError::Denied(cause.to_string()),
        Error::PlatformFailure(cause) => PlatformError::Failed(cause.to_string()),
        Error::NoDefaultStore => PlatformError::Unsupported("credential store"),
        Error::TooLong(name, limit) => {
            PlatformError::Failed(format!("{name} is longer than the limit of {limit}"))
        }
        Error::Invalid(name, reason) => {
            PlatformError::Failed(format!("{name} is invalid: {reason}"))
        }
        Error::BadEncoding(_) | Error::BadDataFormat(..) => {
            PlatformError::Failed("stored value is malformed".to_owned())
        }
        Error::BadStoreFormat(_) => {
            PlatformError::Failed("credential store is malformed".to_owned())
        }
        Error::Ambiguous(_) => PlatformError::Failed("more than one matching entry".to_owned()),
        Error::NotSupportedByStore(_) => PlatformError::Unsupported("credential store operation"),
        // `Error` is non-exhaustive across keyring-core releases.
        #[allow(unreachable_patterns)]
        _ => PlatformError::Failed("credential store failed".to_owned()),
    }
}

#[cfg(windows)]
fn install_platform_store() -> Result<(), PlatformError> {
    let store = windows_native_keyring_store::Store::new().map_err(map_error)?;
    keyring_core::set_default_store(store);
    Ok(())
}

#[cfg(target_os = "macos")]
fn install_platform_store() -> Result<(), PlatformError> {
    let store = apple_native_keyring_store::keychain::Store::new().map_err(map_error)?;
    keyring_core::set_default_store(store);
    Ok(())
}

#[cfg(target_os = "linux")]
fn install_platform_store() -> Result<(), PlatformError> {
    let store = zbus_secret_service_keyring_store::Store::new().map_err(map_error)?;
    keyring_core::set_default_store(store);
    Ok(())
}

#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
fn install_platform_store() -> Result<(), PlatformError> {
    Err(PlatformError::Unsupported("credential store"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Installs the in-memory `keyring-core` mock once. It stands in for the OS store; these
    /// tests prove the adapter, not Credential Manager, Keychain or Secret Service.
    fn mock_store() -> OsCredentialStore {
        static MOCK: std::sync::Once = std::sync::Once::new();
        MOCK.call_once(|| {
            let store = keyring_core::mock::Store::new().expect("mock store");
            keyring_core::set_default_store(store);
        });
        OsCredentialStore::on_installed_default().expect("mock installed")
    }

    fn key(account: &str) -> CredentialKey {
        CredentialKey {
            service: "kattri-test".to_owned(),
            account: account.to_owned(),
        }
    }

    #[test]
    fn a_stored_secret_reads_back_and_replaces_and_deletes() {
        let mut store = mock_store();
        let key = key("round-trip");

        assert_eq!(store.get(&key), Ok(None));
        store
            .set(&key, Secret::new(b"first".to_vec()))
            .expect("set");
        assert_eq!(
            store.get(&key).expect("get").expect("present").expose(),
            b"first"
        );
        store
            .set(&key, Secret::new(b"second".to_vec()))
            .expect("replace");
        assert_eq!(
            store.get(&key).expect("get").expect("present").expose(),
            b"second"
        );
        assert_eq!(store.delete(&key), Ok(true));
        assert_eq!(store.get(&key), Ok(None));
        assert_eq!(
            store.delete(&key),
            Ok(false),
            "deleting twice reports absence"
        );
    }

    #[test]
    fn entries_are_separate_per_account_and_per_service() {
        let mut store = mock_store();
        let one = key("server-one");
        let two = key("server-two");
        let other_service = CredentialKey {
            service: "kattri-test-other".to_owned(),
            account: "server-one".to_owned(),
        };
        store
            .set(&one, Secret::new(b"one".to_vec()))
            .expect("set one");
        store
            .set(&two, Secret::new(b"two".to_vec()))
            .expect("set two");
        assert_eq!(store.get(&other_service), Ok(None));
        assert_eq!(
            store.get(&one).expect("get").expect("some").expose(),
            b"one"
        );
        assert_eq!(
            store.get(&two).expect("get").expect("some").expose(),
            b"two"
        );
        store.delete(&one).expect("delete one");
        assert_eq!(
            store.get(&two).expect("get").expect("some").expose(),
            b"two"
        );
        store.delete(&two).expect("delete two");
    }

    #[test]
    fn errors_never_carry_the_secret_bytes() {
        let secret = b"super-secret-bytes".to_vec();
        for error in [
            keyring_core::Error::BadEncoding(secret.clone()),
            keyring_core::Error::BadDataFormat(secret.clone(), "x".into()),
        ] {
            let text = format!("{:?}", map_error(error));
            assert!(!text.contains("super-secret"), "{text}");
        }
    }

    #[test]
    fn error_mapping_distinguishes_missing_denied_and_failed() {
        assert_eq!(
            map_error(keyring_core::Error::NoEntry),
            PlatformError::NotFound
        );
        assert!(matches!(
            map_error(keyring_core::Error::NoStorageAccess("locked".into())),
            PlatformError::Denied(_)
        ));
        assert!(matches!(
            map_error(keyring_core::Error::PlatformFailure("boom".into())),
            PlatformError::Failed(_)
        ));
        assert_eq!(
            map_error(keyring_core::Error::NoDefaultStore),
            PlatformError::Unsupported("credential store")
        );
    }
}
