//! Bounded, origin-pinned download of a verified update artifact.
//!
//! This is the step between a [`VerifiedManifest`] and [`crate::apply::apply_verified`]: the URL
//! comes from the signed manifest, so the caller never chooses it, and the bytes are only returned
//! after the manifest's own size and digest match them.
//!
//! Streaming rules match the manifest fetcher: HTTPS only, the configured origin only, redirects
//! refused, and every read bounded. The declared size is the ceiling, so a server cannot stream
//! past what the signed manifest promised.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use reqwest::Url;
use reqwest::redirect::Policy;
use sha2::{Digest, Sha256};

use crate::{UpdateError, VerifiedManifest, verify::verify_artifact_bytes};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const READ_TIMEOUT: Duration = Duration::from_secs(30);
const TOTAL_TIMEOUT: Duration = Duration::from_secs(600);

/// Largest artifact this crate will hold in memory. The manifest schema allows more, but applying
/// an update needs the bytes in one buffer, so anything above this floor asks for a manual install.
pub const MAX_IN_MEMORY_BYTES: u64 = 512 * 1024 * 1024;

/// A single HTTP response collected under the transport's size and timeout limits.
#[derive(Clone, PartialEq, Eq)]
pub struct ArtifactResponse {
    pub status: u16,
    pub final_url: Url,
    pub redirected: bool,
    pub body: Vec<u8>,
}

/// Async transport seam for deterministic local tests and future integrations.
///
/// Implementations must use HTTPS, disable redirects, stop reading past `limit` bytes, and enforce
/// connect, idle-read and total timeouts.
pub trait ArtifactTransport: Send + Sync {
    fn get<'a>(
        &'a self,
        url: &'a Url,
        limit: u64,
    ) -> Pin<Box<dyn Future<Output = Result<ArtifactResponse, UpdateError>> + Send + 'a>>;
}

/// Reqwest transport with redirects disabled and bounded connection, body reads and total time.
struct ReqwestArtifactTransport {
    client: reqwest::Client,
}

impl ReqwestArtifactTransport {
    fn new() -> Result<Self, UpdateError> {
        let client = reqwest::Client::builder()
            .redirect(Policy::none())
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(READ_TIMEOUT)
            .timeout(TOTAL_TIMEOUT)
            .build()
            .map_err(|_| UpdateError::ArtifactTransport)?;
        Ok(Self { client })
    }
}

impl ArtifactTransport for ReqwestArtifactTransport {
    fn get<'a>(
        &'a self,
        url: &'a Url,
        limit: u64,
    ) -> Pin<Box<dyn Future<Output = Result<ArtifactResponse, UpdateError>> + Send + 'a>> {
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
                return Ok(ArtifactResponse {
                    status: status.as_u16(),
                    final_url,
                    redirected,
                    body: Vec::new(),
                });
            }

            let mut body = Vec::new();
            while let Some(chunk) = response.chunk().await.map_err(map_reqwest_error)? {
                append_bounded(&mut body, &chunk, limit)?;
            }
            Ok(ArtifactResponse {
                status: status.as_u16(),
                final_url,
                redirected,
                body,
            })
        })
    }
}

fn map_reqwest_error(error: reqwest::Error) -> UpdateError {
    if error.is_timeout() {
        UpdateError::ArtifactTimeout
    } else {
        UpdateError::ArtifactTransport
    }
}

fn append_bounded(body: &mut Vec<u8>, chunk: &[u8], limit: u64) -> Result<(), UpdateError> {
    let current = body.len() as u64;
    let next = current.saturating_add(chunk.len() as u64);
    if next > limit {
        return Err(UpdateError::ArtifactTooLarge { limit });
    }
    body.extend_from_slice(chunk);
    Ok(())
}

/// Downloads the artifact a verified manifest names for one platform and architecture.
pub struct ArtifactDownloader {
    transport: Arc<dyn ArtifactTransport>,
    allowed_origin: Url,
}

impl ArtifactDownloader {
    /// Build the production transport for a configured origin such as `https://updates.example`.
    pub fn reqwest(allowed_origin: &str) -> Result<Self, UpdateError> {
        let origin = parse_origin(allowed_origin)?;
        Self::with_transport(Arc::new(ReqwestArtifactTransport::new()?), origin)
    }

    /// Build with an injected transport. Used by tests to run without a network.
    pub fn with_transport(
        transport: Arc<dyn ArtifactTransport>,
        allowed_origin: Url,
    ) -> Result<Self, UpdateError> {
        if !is_https_url(&allowed_origin)
            || allowed_origin.path() != "/"
            || allowed_origin.query().is_some()
        {
            return Err(UpdateError::ArtifactOrigin);
        }
        Ok(Self {
            transport,
            allowed_origin,
        })
    }

    /// Download the artifact for `platform`/`arch` and return it only if the manifest's size and
    /// digest match the bytes received.
    pub async fn download(
        &self,
        verified: &VerifiedManifest,
        platform: &str,
        arch: &str,
    ) -> Result<Vec<u8>, UpdateError> {
        let artifact = verified
            .manifest()
            .artifact_for(platform, arch)
            .ok_or_else(|| UpdateError::NoArtifactForPlatform {
                platform: platform.to_owned(),
                arch: arch.to_owned(),
            })?;

        if artifact.size_bytes > MAX_IN_MEMORY_BYTES {
            return Err(UpdateError::ArtifactTooLargeToBuffer {
                declared: artifact.size_bytes,
                limit: MAX_IN_MEMORY_BYTES,
            });
        }

        let url = parse_origin(&artifact.url)?;
        if !is_https_url(&url) || url.origin() != self.allowed_origin.origin() {
            return Err(UpdateError::ArtifactOrigin);
        }

        let response = self.transport.get(&url, artifact.size_bytes).await?;
        if response.final_url.origin() != self.allowed_origin.origin() {
            return Err(UpdateError::ArtifactOrigin);
        }
        if response.redirected || response.final_url.as_str() != url.as_str() {
            return Err(UpdateError::ArtifactRedirect);
        }
        if (300..400).contains(&response.status) {
            return Err(UpdateError::ArtifactRedirect);
        }
        if response.status != reqwest::StatusCode::OK.as_u16() {
            return Err(UpdateError::ArtifactHttpStatus {
                status: response.status,
            });
        }

        verify_artifact_bytes(artifact, &response.body)?;
        Ok(response.body)
    }
}

/// SHA-256 of downloaded bytes, exposed so a caller can log a digest without a second dependency.
pub fn digest_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn parse_origin(input: &str) -> Result<Url, UpdateError> {
    Url::parse(input.trim()).map_err(|_| UpdateError::ArtifactOrigin)
}

fn is_https_url(url: &Url) -> bool {
    url.scheme() == "https" && url.host_str().is_some()
}

#[cfg(test)]
mod tests {
    use super::{append_bounded, digest_hex};

    #[test]
    fn digest_is_lowercase_hex_of_known_value() {
        assert_eq!(
            digest_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn reading_past_the_limit_is_refused() {
        let mut body = vec![0u8; 4];
        let error = append_bounded(&mut body, &[0u8; 4], 4).expect_err("over limit");
        assert_eq!(error, crate::UpdateError::ArtifactTooLarge { limit: 4 });
        assert_eq!(body.len(), 4);
    }
}
