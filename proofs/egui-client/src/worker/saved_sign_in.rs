//! "Stay signed in": the access token of one sign-in, kept in the operating system's credential
//! store and nowhere else (ADR 0003, owner decision 1).
//!
//! The token never leaves the API worker. The UI only says whether the viewer wants to be
//! remembered and for which local profile; the worker saves, reads and deletes the entry. Every
//! failure is quiet and ends in "ask again": a locked or missing store means the token stays in
//! memory, and nothing is written to a file instead.
//!
//! Entries bind the local profile, server ID and configured endpoint. A server claiming another
//! server's public ID must never inherit that server's saved sign-in.

#[cfg(test)]
use std::sync::{Arc, Mutex, PoisonError};

use jellyfin_api::JellyfinClient;
use kattri_platform::{CredentialKey, CredentialStore, OsCredentialStore, PlatformError, Secret};
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

/// Service name of every saved sign-in in the OS store.
const SERVICE: &str = "org.jellyfin.kattri.sign-in";

/// Longest profile or server id kept in an entry name.
const MAX_PART_BYTES: usize = 128;

/// What the viewer asked for on this sign-in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SavedSignInRequest {
    /// The local profile the sign-in belongs to.
    pub(crate) scope: String,
    /// Keep the token for the next start. When false, any older saved token for this profile
    /// and server is deleted.
    pub(crate) remember: bool,
}

impl SavedSignInRequest {
    /// A sign-in that is not remembered, for tests that do not exercise saving.
    #[cfg(test)]
    pub(crate) fn not_remembered() -> Self {
        Self {
            scope: "default".to_owned(),
            remember: false,
        }
    }
}

/// Bind the entry to the configured endpoint as well as its claimed ID and local profile.
/// Public server IDs are untrusted; they must not select another origin's saved token.
pub(crate) fn credential_key(
    scope: &str,
    server_id: &str,
    client: &JellyfinClient,
) -> Option<CredentialKey> {
    let usable = |part: &str| !part.is_empty() && part.len() <= MAX_PART_BYTES;
    (usable(scope) && usable(server_id)).then(|| {
        let base = client.server_base_url();
        // The API appends route segments after removing one empty trailing path segment.
        let base = base.strip_suffix('/').unwrap_or(base);
        let mut digest = Sha256::new();
        for part in [scope, server_id, base] {
            digest.update((part.len() as u64).to_le_bytes());
            digest.update(part.as_bytes());
        }
        CredentialKey {
            service: SERVICE.to_owned(),
            account: format!(
                "v2/{}",
                digest
                    .finalize()
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<String>()
            ),
        }
    })
}

/// Where saved tokens go. Cheap to clone; clones share the same backend.
#[derive(Clone)]
pub(crate) struct SavedSignInStore {
    backend: Backend,
}

#[derive(Clone)]
enum Backend {
    /// The OS credential store, opened on first use inside the worker.
    Os,
    /// Nothing is saved or read. Used by test sinks, so tests never touch the real store.
    #[cfg(test)]
    Off,
    /// An injected store, for tests.
    #[cfg(test)]
    Shared(Arc<Mutex<dyn CredentialStore + Send>>),
}

impl SavedSignInStore {
    pub(crate) fn os() -> Self {
        Self {
            backend: Backend::Os,
        }
    }

    #[cfg(test)]
    pub(crate) fn off() -> Self {
        Self {
            backend: Backend::Off,
        }
    }

    #[cfg(test)]
    pub(crate) fn shared(store: Arc<Mutex<dyn CredentialStore + Send>>) -> Self {
        Self {
            backend: Backend::Shared(store),
        }
    }

    /// Runs one store operation. `None` means there is no store or it failed; the failure is
    /// logged without any secret.
    fn run<T>(
        &self,
        what: &'static str,
        operation: impl FnOnce(&mut dyn CredentialStore) -> Result<T, PlatformError>,
    ) -> Option<T> {
        let result = match &self.backend {
            #[cfg(test)]
            Backend::Off => return None,
            Backend::Os => {
                let mut store = match OsCredentialStore::open() {
                    Ok(store) => store,
                    Err(error) => {
                        tracing::debug!(%error, "no credential store; the sign-in stays in memory");
                        return None;
                    }
                };
                operation(&mut store)
            }
            #[cfg(test)]
            Backend::Shared(store) => {
                let mut guard = store.lock().unwrap_or_else(PoisonError::into_inner);
                operation(&mut *guard)
            }
        };
        match result {
            Ok(value) => Some(value),
            Err(error) => {
                tracing::warn!(%error, operation = what, "credential store call failed");
                None
            }
        }
    }

    /// Whether this backend can keep a token: the OS store opens, or a test store is injected.
    /// Opening the OS store can take a moment (a Secret Service may prompt), so the worker asks.
    pub(crate) fn is_available(&self) -> bool {
        match &self.backend {
            #[cfg(test)]
            Backend::Off => false,
            Backend::Os => OsCredentialStore::open().is_ok(),
            #[cfg(test)]
            Backend::Shared(_) => true,
        }
    }

    /// Saves `token`. Returns whether it reached the store.
    pub(crate) fn save(&self, key: &CredentialKey, token: &str) -> bool {
        self.run("save", |store| {
            store.set(key, Secret::new(token.as_bytes().to_vec()))
        })
        .is_some()
    }

    /// Reads a saved token. A value that is not valid UTF-8 counts as absent.
    pub(crate) fn load(&self, key: &CredentialKey) -> Option<Zeroizing<String>> {
        let secret = self.run("load", |store| store.get(key))??;
        std::str::from_utf8(secret.expose())
            .ok()
            .map(|text| Zeroizing::new(text.to_owned()))
    }

    /// Deletes a saved token. A missing entry is fine.
    pub(crate) fn forget(&self, key: &CredentialKey) {
        let _ = self.run("forget", |store| store.delete(key));
    }
}

/// A token this worker saved for the live session. When the session ends the entry is deleted.
pub(crate) struct SavedEntry {
    store: SavedSignInStore,
    key: CredentialKey,
}

impl SavedEntry {
    pub(crate) fn new(store: SavedSignInStore, key: CredentialKey) -> Self {
        Self { store, key }
    }

    pub(crate) fn forget(self) {
        self.store.forget(&self.key);
    }
}

/// After a successful sign-in: saves the token when asked, or deletes an older one when not.
/// Returns the entry to delete when the session ends, if a token is now stored.
pub(crate) fn apply_after_sign_in(
    store: &SavedSignInStore,
    request: &SavedSignInRequest,
    client: &JellyfinClient,
    server_id: Option<&str>,
    token: &str,
) -> Option<SavedEntry> {
    let key = credential_key(&request.scope, server_id?, client)?;
    if request.remember && store.save(&key, token) {
        Some(SavedEntry::new(store.clone(), key))
    } else {
        if !request.remember {
            store.forget(&key);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kattri_platform::memory::MemoryCredentialStore;

    fn client(url: &str) -> JellyfinClient {
        JellyfinClient::new(
            jellyfin_api::ClientConfig::new(url, "test", "test", "test", "test")
                .expect("valid address"),
        )
        .expect("client")
    }

    fn credential_key(scope: &str, server_id: &str) -> Option<CredentialKey> {
        super::credential_key(
            scope,
            server_id,
            &client("https://example.invalid/jellyfin"),
        )
    }

    fn apply_after_sign_in(
        store: &SavedSignInStore,
        request: &SavedSignInRequest,
        server_id: Option<&str>,
        token: &str,
    ) -> Option<SavedEntry> {
        super::apply_after_sign_in(
            store,
            request,
            &client("https://example.invalid/jellyfin"),
            server_id,
            token,
        )
    }

    #[test]
    fn credential_namespace_binds_origin_deployment_and_framed_profile_identity() {
        let key =
            |scope, id, address| super::credential_key(scope, id, &client(address)).expect("key");
        let trusted = key("default", "srv-1", "https://EXAMPLE.invalid:443/jellyfin/");
        assert_eq!(
            trusted,
            key("default", "srv-1", "https://example.invalid/jellyfin")
        );
        for address in [
            "https://other.invalid/jellyfin",
            "https://example.invalid:8443/jellyfin",
            "http://127.0.0.1/jellyfin",
            "https://example.invalid/other",
            "https://example.invalid/jellyfin//",
        ] {
            assert_ne!(trusted, key("default", "srv-1", address));
        }
        assert_ne!(
            key("a/server/b", "c", "https://example.invalid"),
            key("a", "b/server/c", "https://example.invalid")
        );
        assert_eq!(trusted.account.len(), 67);
        assert!(!trusted.account.contains("example.invalid"));
    }

    fn memory() -> (SavedSignInStore, Arc<Mutex<MemoryCredentialStore>>) {
        let inner = Arc::new(Mutex::new(MemoryCredentialStore::new()));
        let shared: Arc<Mutex<dyn CredentialStore + Send>> = inner.clone();
        (SavedSignInStore::shared(shared), inner)
    }

    fn request(remember: bool) -> SavedSignInRequest {
        SavedSignInRequest {
            scope: "default".to_owned(),
            remember,
        }
    }

    #[test]
    fn entry_names_are_per_profile_and_server_and_reject_empty_or_huge_parts() {
        let a = credential_key("default", "srv-1").expect("key");
        let b = credential_key("kids", "srv-1").expect("key");
        let c = credential_key("default", "srv-2").expect("key");
        assert_eq!(a.service, b.service);
        assert!(a != b && a != c && b != c);
        assert_eq!(credential_key("", "srv-1"), None);
        assert_eq!(credential_key("default", ""), None);
        assert_eq!(
            credential_key(&"x".repeat(MAX_PART_BYTES + 1), "srv-1"),
            None
        );
    }

    #[test]
    fn remembering_saves_the_token_and_the_entry_deletes_it() {
        let (store, backing) = memory();
        let entry =
            apply_after_sign_in(&store, &request(true), Some("srv-1"), "token-abc").expect("saved");
        let key = credential_key("default", "srv-1").expect("key");
        assert_eq!(
            store.load(&key).expect("token").as_str(),
            "token-abc",
            "the token reads back"
        );
        entry.forget();
        assert!(store.load(&key).is_none());
        assert!(
            backing
                .lock()
                .expect("lock")
                .get(&key)
                .expect("get")
                .is_none()
        );
    }

    #[test]
    fn not_remembering_deletes_an_older_saved_token() {
        let (store, _) = memory();
        let key = credential_key("default", "srv-1").expect("key");
        assert!(store.save(&key, "old-token"));
        assert!(apply_after_sign_in(&store, &request(false), Some("srv-1"), "new").is_none());
        assert!(store.load(&key).is_none());
    }

    #[test]
    fn a_missing_server_id_saves_nothing() {
        let (store, backing) = memory();
        assert!(apply_after_sign_in(&store, &request(true), None, "token").is_none());
        assert!(apply_after_sign_in(&store, &request(true), Some(""), "token").is_none());
        let key = credential_key("default", "srv-1").expect("key");
        assert!(
            backing
                .lock()
                .expect("lock")
                .get(&key)
                .expect("get")
                .is_none()
        );
    }

    #[test]
    fn the_off_store_saves_and_reads_nothing() {
        let store = SavedSignInStore::off();
        let key = credential_key("default", "srv-1").expect("key");
        assert!(!store.save(&key, "token"));
        assert!(store.load(&key).is_none());
        store.forget(&key);
        assert!(apply_after_sign_in(&store, &request(true), Some("srv-1"), "token").is_none());
    }

    #[test]
    fn a_failing_store_never_panics_and_reports_not_saved() {
        struct Broken;
        impl CredentialStore for Broken {
            fn get(&self, _: &CredentialKey) -> Result<Option<Secret>, PlatformError> {
                Err(PlatformError::Denied("locked".into()))
            }
            fn set(&mut self, _: &CredentialKey, _: Secret) -> Result<(), PlatformError> {
                Err(PlatformError::Denied("locked".into()))
            }
            fn delete(&mut self, _: &CredentialKey) -> Result<bool, PlatformError> {
                Err(PlatformError::Denied("locked".into()))
            }
        }
        let shared: Arc<Mutex<dyn CredentialStore + Send>> = Arc::new(Mutex::new(Broken));
        let store = SavedSignInStore::shared(shared);
        let key = credential_key("default", "srv-1").expect("key");
        assert!(!store.save(&key, "token"));
        assert!(store.load(&key).is_none());
        store.forget(&key);
        assert!(apply_after_sign_in(&store, &request(true), Some("srv-1"), "token").is_none());
    }

    #[test]
    fn a_non_utf8_value_counts_as_absent() {
        let (store, backing) = memory();
        let key = credential_key("default", "srv-1").expect("key");
        backing
            .lock()
            .expect("lock")
            .set(&key, Secret::new(vec![0xff, 0xfe]))
            .expect("set");
        assert!(store.load(&key).is_none());
    }
}
