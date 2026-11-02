//! Bounded, origin-pinned fetching of signed update manifests.
//!
//! The caller supplies the configured manifest URL and allowed HTTPS origin. This crate does not
//! choose or publish a release endpoint. Only a successfully verified envelope is returned.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use reqwest::Url;
use reqwest::redirect::Policy;

use crate::verify::MAX_ENVELOPE_BYTES;
use crate::{Channel, InstallState, Keyring, UpdateError, VerifiedManifest, open_envelope};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const READ_TIMEOUT: Duration = Duration::from_secs(5);
const TOTAL_TIMEOUT: Duration = Duration::from_secs(15);

/// A single HTTP response collected under the transport's size and timeout limits.
///
/// Implementations must not follow redirects. `redirected` reports any redirect attempt or URL
/// change observed by the transport. The fetcher independently validates the final status, URL,
/// and body size.
#[derive(Clone, PartialEq, Eq)]
struct ManifestResponse {
    status: u16,
    final_url: Url,
    redirected: bool,
    body: Vec<u8>,
}

/// Async transport seam for deterministic local tests and future integrations.
///
/// Implementations must use HTTPS, disable redirects, cap streaming reads at
/// [`MAX_ENVELOPE_BYTES`], and enforce connect, idle-read, and total timeouts.
trait ManifestTransport: Send + Sync {
    fn get<'a>(
        &'a self,
        url: &'a Url,
    ) -> Pin<Box<dyn Future<Output = Result<ManifestResponse, UpdateError>> + Send + 'a>>;
}

/// Reqwest transport with redirects disabled and bounded connection and body reads.
struct ReqwestManifestTransport {
    client: reqwest::Client,
}

impl ReqwestManifestTransport {
    fn new() -> Result<Self, UpdateError> {
        let client = reqwest::Client::builder()
            .redirect(Policy::none())
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(READ_TIMEOUT)
            .timeout(TOTAL_TIMEOUT)
            .build()
            .map_err(|_| UpdateError::ManifestTransport)?;
        Ok(Self { client })
    }
}

impl ManifestTransport for ReqwestManifestTransport {
    fn get<'a>(
        &'a self,
        url: &'a Url,
    ) -> Pin<Box<dyn Future<Output = Result<ManifestResponse, UpdateError>> + Send + 'a>> {
        Box::pin(async move {
            let mut response = self
                .client
                .get(url.clone())
                .send()
                .await
                .map_err(map_reqwest_error)?;
            let status = response.status();
            let final_url = response.url().clone();
            let redirected = status.is_redirection() || final_url.as_str() != url.as_str();

            if redirected || status.as_u16() != reqwest::StatusCode::OK.as_u16() {
                return Ok(ManifestResponse {
                    status: status.as_u16(),
                    final_url,
                    redirected,
                    body: Vec::new(),
                });
            }

            if response
                .content_length()
                .is_some_and(|length| length > MAX_ENVELOPE_BYTES as u64)
            {
                return Err(envelope_too_large());
            }

            let capacity = response
                .content_length()
                .unwrap_or(0)
                .min(MAX_ENVELOPE_BYTES as u64) as usize;
            let mut body = Vec::with_capacity(capacity);
            while let Some(chunk) = response.chunk().await.map_err(map_reqwest_error)? {
                append_bounded_chunk(&mut body, &chunk)?;
            }

            Ok(ManifestResponse {
                status: status.as_u16(),
                final_url,
                redirected,
                body,
            })
        })
    }
}

/// Fetches and verifies manifests against one caller-configured HTTPS origin.
pub struct ManifestFetcher {
    transport: Box<dyn ManifestTransport>,
    allowed_origin: Url,
}

impl ManifestFetcher {
    /// Build the production transport for a configured origin such as `https://updates.example`.
    pub fn reqwest(allowed_origin: &str) -> Result<Self, UpdateError> {
        let allowed_origin = parse_endpoint(allowed_origin)?;
        Self::with_transport(Box::new(ReqwestManifestTransport::new()?), allowed_origin)
    }

    fn with_transport(
        transport: Box<dyn ManifestTransport>,
        allowed_origin: Url,
    ) -> Result<Self, UpdateError> {
        if !is_https_url(&allowed_origin)
            || allowed_origin.path() != "/"
            || allowed_origin.query().is_some()
        {
            return Err(UpdateError::ManifestOrigin);
        }
        Ok(Self {
            transport,
            allowed_origin,
        })
    }

    /// Fetch one manifest and return it only after signature, channel, downgrade, and replay checks.
    /// This method does not update [`InstallState`]; the caller records only after its own acceptance.
    pub async fn fetch_verified(
        &self,
        manifest_url: &str,
        channel: Channel,
        keyring: &Keyring,
        state: &InstallState,
    ) -> Result<VerifiedManifest, UpdateError> {
        if keyring.is_empty() {
            return Err(UpdateError::NoTrustedKeys);
        }
        let manifest_url = parse_endpoint(manifest_url)?;
        if !is_https_url(&manifest_url) || manifest_url.origin() != self.allowed_origin.origin() {
            return Err(UpdateError::ManifestOrigin);
        }

        let response = self.transport.get(&manifest_url).await?;
        if response.final_url.origin() != self.allowed_origin.origin() {
            return Err(UpdateError::ManifestOrigin);
        }
        if response.redirected || response.final_url.as_str() != manifest_url.as_str() {
            return Err(UpdateError::ManifestRedirect);
        }
        if (300..400).contains(&response.status) {
            return Err(UpdateError::ManifestRedirect);
        }
        if response.status != reqwest::StatusCode::OK.as_u16() {
            return Err(UpdateError::ManifestHttpStatus {
                status: response.status,
            });
        }
        if response.body.len() > MAX_ENVELOPE_BYTES {
            return Err(envelope_too_large());
        }

        let envelope = std::str::from_utf8(&response.body)
            .map_err(|_| UpdateError::Envelope("response is not UTF-8".to_owned()))?;
        open_envelope(keyring, envelope, channel, state)
    }
}

fn parse_endpoint(input: &str) -> Result<Url, UpdateError> {
    // URL parsers repair empty authorities and strip empty userinfo. Refuse the raw form first.
    if input.len() > 2_048
        || input
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || c == '\\')
    {
        return Err(UpdateError::ManifestOrigin);
    }
    let (_, rest) = input.split_once("://").ok_or(UpdateError::ManifestOrigin)?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if authority.is_empty() || authority.contains('@') {
        return Err(UpdateError::ManifestOrigin);
    }
    let url = Url::parse(input).map_err(|_| UpdateError::ManifestOrigin)?;
    if !is_https_url(&url) || url.query().is_some() {
        return Err(UpdateError::ManifestOrigin);
    }
    Ok(url)
}

fn is_https_url(url: &Url) -> bool {
    url.scheme() == "https"
        && url.host_str().is_some()
        && url.username().is_empty()
        && url.password().is_none()
        && !has_userinfo(url)
        && url.fragment().is_none()
}

fn has_userinfo(url: &Url) -> bool {
    let Some((_, authority_and_path)) = url.as_str().split_once("://") else {
        return true;
    };
    let authority = authority_and_path
        .split('/')
        .next()
        .unwrap_or_default()
        .split('?')
        .next()
        .unwrap_or_default();
    authority.contains('@')
}

fn envelope_too_large() -> UpdateError {
    UpdateError::TooLarge {
        what: "update envelope",
        limit: MAX_ENVELOPE_BYTES as u64,
    }
}

fn append_bounded_chunk(body: &mut Vec<u8>, chunk: &[u8]) -> Result<(), UpdateError> {
    if chunk.len() > MAX_ENVELOPE_BYTES.saturating_sub(body.len()) {
        return Err(envelope_too_large());
    }
    body.extend_from_slice(chunk);
    Ok(())
}

fn map_reqwest_error(error: reqwest::Error) -> UpdateError {
    if error.is_timeout() {
        UpdateError::ManifestTimeout
    } else {
        // Do not include reqwest's diagnostic: it may contain the configured URL.
        UpdateError::ManifestTransport
    }
}

#[cfg(test)]
mod tests {
    use std::future::{Future, ready};
    use std::pin::pin;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::{Context, Poll, Waker};

    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD;
    use ed25519_dalek::{Signer, SigningKey};
    use serde_json::json;
    use sha2::{Digest, Sha256};

    use super::*;

    const ALLOWED_ORIGIN: &str = "https://updates.example.org/";

    struct FakeTransport {
        response: ManifestResponse,
        calls: Arc<AtomicUsize>,
    }

    impl ManifestTransport for FakeTransport {
        fn get<'a>(
            &'a self,
            _url: &'a Url,
        ) -> Pin<Box<dyn Future<Output = Result<ManifestResponse, UpdateError>> + Send + 'a>>
        {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(ready(Ok(self.response.clone())))
        }
    }

    fn block_on_ready<F: Future>(future: F) -> F::Output {
        let mut future = pin!(future);
        let mut context = Context::from_waker(Waker::noop());
        match future.as_mut().poll(&mut context) {
            Poll::Ready(output) => output,
            Poll::Pending => panic!("fake transport unexpectedly returned a pending future"),
        }
    }

    fn test_key(label: &str) -> SigningKey {
        let seed: [u8; 32] = Sha256::digest(
            format!("kattri updater FETCH TEST KEY; never a release key: {label}").as_bytes(),
        )
        .into();
        SigningKey::from_bytes(&seed)
    }

    fn signed_envelope(key: &SigningKey) -> Vec<u8> {
        let payload = serde_json::to_vec(&json!({
            "schema_version": 1,
            "channel": "stable",
            "version": "2.1.0",
            "sequence": 3,
            "published": "2026-10-15",
            "artifacts": [{
                "platform": "windows",
                "arch": "x64",
                "format": "msi",
                "url": "https://cdn.example.org/kattri.msi",
                "sha256": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
                "size_bytes": 128
            }]
        }))
        .expect("test payload JSON");
        let signature = key.sign(&payload).to_bytes();
        serde_json::to_vec(&json!({
            "envelope_version": 1,
            "alg": "ed25519",
            "kid": "test-fetch-key",
            "payload": STANDARD.encode(payload),
            "sig": STANDARD.encode(signature)
        }))
        .expect("test envelope JSON")
    }

    fn active_test_keyring(key: &SigningKey) -> Keyring {
        Keyring::new(vec![(
            "test-fetch-key".to_owned(),
            key.verifying_key().to_bytes(),
            crate::KeyState::Active,
        )])
        .expect("valid test keyring")
    }

    fn fetcher(response: ManifestResponse) -> (ManifestFetcher, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let fetcher = ManifestFetcher::with_transport(
            Box::new(FakeTransport {
                response,
                calls: Arc::clone(&calls),
            }),
            Url::parse(ALLOWED_ORIGIN).expect("origin URL"),
        )
        .expect("fetcher");
        (fetcher, calls)
    }

    fn response(final_url: &str, status: u16, redirected: bool, body: Vec<u8>) -> ManifestResponse {
        ManifestResponse {
            status,
            final_url: Url::parse(final_url).expect("response URL"),
            redirected,
            body,
        }
    }

    #[test]
    fn a_signed_same_origin_manifest_is_verified_without_recording_state() {
        let key = test_key("signer");
        let (fetcher, calls) = fetcher(response(
            "https://updates.example.org/stable.json",
            200,
            false,
            signed_envelope(&key),
        ));
        let keyring = active_test_keyring(&key);
        let state = InstallState::new("2.0.0").expect("install state");

        let verified = block_on_ready(fetcher.fetch_verified(
            "https://updates.example.org/stable.json",
            Channel::Stable,
            &keyring,
            &state,
        ))
        .expect("verified manifest");

        assert_eq!(verified.manifest().version, "2.1.0");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(state.accepted(Channel::Stable).is_none());
    }

    #[test]
    fn an_empty_release_keyring_fails_before_transport_access() {
        let (fetcher, calls) = fetcher(response(
            "https://updates.example.org/stable.json",
            200,
            false,
            b"not trusted".to_vec(),
        ));
        let state = InstallState::new("2.0.0").expect("install state");

        assert!(matches!(
            block_on_ready(fetcher.fetch_verified(
                "https://updates.example.org/stable.json",
                Channel::Stable,
                &Keyring::release().expect("empty release keyring"),
                &state,
            )),
            Err(UpdateError::NoTrustedKeys)
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn cross_origin_and_redirect_responses_are_refused() {
        let key = test_key("signer");
        let keyring = active_test_keyring(&key);
        let state = InstallState::new("2.0.0").expect("install state");
        let manifest_url = "https://updates.example.org/stable.json";

        let (cross_origin, calls) = fetcher(response(
            "https://attacker.example/stable.json",
            200,
            false,
            signed_envelope(&key),
        ));
        assert!(matches!(
            block_on_ready(cross_origin.fetch_verified(
                manifest_url,
                Channel::Stable,
                &keyring,
                &state,
            )),
            Err(UpdateError::ManifestOrigin)
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        let (redirect, _) = fetcher(response(
            "https://updates.example.org/other.json",
            302,
            true,
            Vec::new(),
        ));
        assert!(matches!(
            block_on_ready(redirect.fetch_verified(
                manifest_url,
                Channel::Stable,
                &keyring,
                &state,
            )),
            Err(UpdateError::ManifestRedirect)
        ));
    }

    #[test]
    fn non_https_and_cross_origin_requests_are_rejected_before_transport() {
        let key = test_key("signer");
        let (fetcher, calls) = fetcher(response(
            "https://updates.example.org/stable.json",
            200,
            false,
            signed_envelope(&key),
        ));
        let keyring = active_test_keyring(&key);
        let state = InstallState::new("2.0.0").expect("install state");

        for url in [
            "http://updates.example.org/stable.json",
            "https://attacker.example/stable.json",
            "https://user:secret@updates.example.org/stable.json",
            "https://@updates.example.org/stable.json",
            "https:///updates.example.org/stable.json",
            " https://updates.example.org/stable.json",
            "https://updates.example.org/stable.json?credential=secret",
            "https://updates.example.org/\\attacker.example/stable.json",
        ] {
            assert!(matches!(
                block_on_ready(fetcher.fetch_verified(url, Channel::Stable, &keyring, &state,)),
                Err(UpdateError::ManifestOrigin)
            ));
        }
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn oversized_envelope_is_refused_before_signature_parsing() {
        let key = test_key("signer");
        let (fetcher, _) = fetcher(response(
            "https://updates.example.org/stable.json",
            200,
            false,
            vec![b' '; MAX_ENVELOPE_BYTES + 1],
        ));
        let keyring = active_test_keyring(&key);
        let state = InstallState::new("2.0.0").expect("install state");

        assert!(matches!(
            block_on_ready(fetcher.fetch_verified(
                "https://updates.example.org/stable.json",
                Channel::Stable,
                &keyring,
                &state,
            )),
            Err(UpdateError::TooLarge { .. })
        ));
    }

    #[test]
    fn streaming_body_limit_accepts_exact_cap_and_rejects_next_byte() {
        let mut body = Vec::new();
        append_bounded_chunk(&mut body, &vec![b'x'; MAX_ENVELOPE_BYTES])
            .expect("exact cap accepted");
        assert_eq!(body.len(), MAX_ENVELOPE_BYTES);
        assert!(matches!(
            append_bounded_chunk(&mut body, b"x"),
            Err(UpdateError::TooLarge { .. })
        ));
    }

    #[test]
    fn a_bad_signature_does_not_change_install_state() {
        let signing_key = test_key("untrusted signer");
        let trusted_key = test_key("trusted signer");
        let (fetcher, _) = fetcher(response(
            "https://updates.example.org/stable.json",
            200,
            false,
            signed_envelope(&signing_key),
        ));
        let keyring = active_test_keyring(&trusted_key);
        let state = InstallState::new("2.0.0").expect("install state");
        let before = state.clone();

        assert!(matches!(
            block_on_ready(fetcher.fetch_verified(
                "https://updates.example.org/stable.json",
                Channel::Stable,
                &keyring,
                &state,
            )),
            Err(UpdateError::VerificationFailed(_))
        ));
        assert_eq!(state, before);
    }
}
