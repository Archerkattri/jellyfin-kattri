//! Bounded artifact download to a temporary file with incremental verification.
//!
//! This API is intentionally separate from [`crate::download::ArtifactDownloader`], which keeps
//! returning a `Vec<u8>` for existing callers. A returned [`DownloadedArtifact`] owns the staging
//! file and removes it on drop unless the caller explicitly keeps it.

use std::future::Future;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

use reqwest::Url;
use reqwest::redirect::Policy;
use sha2::{Digest, Sha256};

use crate::{UpdateError, VerifiedManifest};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const READ_TIMEOUT: Duration = Duration::from_secs(30);
const TOTAL_TIMEOUT: Duration = Duration::from_secs(600);
const MAX_STAGED_ARTIFACT_BYTES: u64 = crate::manifest::MAX_ARTIFACT_BYTES;
const WRITER_QUEUE_CHUNKS: usize = 1;
static TEMP_FILE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Response facts returned by an artifact transport after it has consumed the body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileResponse {
    pub status: u16,
    pub final_url: Url,
    pub redirected: bool,
}

/// Async transport seam. Implementations must stream chunks to `sink`, never buffer the full body,
/// and must refuse redirects. The sink enforces the byte limit while the transport is reading.
pub trait ArtifactFileTransport: Send + Sync {
    fn get<'a>(
        &'a self,
        url: &'a Url,
        limit: u64,
        sink: &'a mut dyn ChunkSink,
    ) -> Pin<Box<dyn Future<Output = Result<FileResponse, UpdateError>> + Send + 'a>>;
}

/// Asynchronous bounded writer endpoint used by a transport for each received network chunk.
pub trait ChunkSink: Send {
    fn push<'a>(
        &'a mut self,
        chunk: &'a [u8],
    ) -> Pin<Box<dyn Future<Output = Result<(), UpdateError>> + Send + 'a>>;
}

struct ReqwestFileTransport {
    client: reqwest::Client,
}

impl ReqwestFileTransport {
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

impl ArtifactFileTransport for ReqwestFileTransport {
    fn get<'a>(
        &'a self,
        url: &'a Url,
        limit: u64,
        sink: &'a mut dyn ChunkSink,
    ) -> Pin<Box<dyn Future<Output = Result<FileResponse, UpdateError>> + Send + 'a>> {
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
                return Ok(FileResponse {
                    status: status.as_u16(),
                    final_url,
                    redirected,
                });
            }

            if response.content_length().is_some_and(|n| n > limit) {
                return Err(UpdateError::ArtifactTooLarge { limit });
            }
            while let Some(chunk) = response.chunk().await.map_err(map_reqwest_error)? {
                sink.push(&chunk).await?;
            }
            Ok(FileResponse {
                status: status.as_u16(),
                final_url,
                redirected,
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

/// A staged artifact file that is deleted unless explicitly persisted.
#[derive(Debug)]
pub struct DownloadedArtifact {
    path: PathBuf,
    keep: bool,
}

impl DownloadedArtifact {
    /// Path to the verified staged artifact.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Keep the staged artifact after this guard is dropped and return its path.
    pub fn persist(mut self) -> PathBuf {
        self.keep = true;
        self.path.clone()
    }
}

impl Drop for DownloadedArtifact {
    fn drop(&mut self) {
        if !self.keep {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Streams one signed artifact into a temporary file under `staging_dir`.
pub struct ArtifactFileDownloader {
    transport: Arc<dyn ArtifactFileTransport>,
    allowed_origin: Url,
}

impl ArtifactFileDownloader {
    /// Create the production reqwest transport for a configured HTTPS origin.
    pub fn reqwest(allowed_origin: &str) -> Result<Self, UpdateError> {
        let origin = parse_url(allowed_origin)?;
        Self::with_transport(Arc::new(ReqwestFileTransport::new()?), origin)
    }

    /// Create a downloader with an injected streaming transport (primarily for tests).
    pub fn with_transport(
        transport: Arc<dyn ArtifactFileTransport>,
        allowed_origin: Url,
    ) -> Result<Self, UpdateError> {
        if !valid_https_url(&allowed_origin)
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

    /// Download and verify a manifest artifact into an exclusively-created temporary file.
    ///
    /// The bounded writer runs on a dedicated thread so disk I/O does not execute on the async
    /// caller. Its one-chunk queue is bounded; if disk is slower than the network, `push` can block
    /// the transport task while applying backpressure. Callers should therefore avoid invoking this
    /// future directly on a latency-sensitive UI executor.
    pub async fn download_to_file(
        &self,
        verified: &VerifiedManifest,
        platform: &str,
        arch: &str,
        staging_dir: &Path,
    ) -> Result<DownloadedArtifact, UpdateError> {
        let artifact = verified
            .manifest()
            .artifact_for(platform, arch)
            .ok_or_else(|| UpdateError::NoArtifactForPlatform {
                platform: platform.to_owned(),
                arch: arch.to_owned(),
            })?;
        if artifact.size_bytes > MAX_STAGED_ARTIFACT_BYTES {
            return Err(UpdateError::ArtifactTooLarge {
                limit: MAX_STAGED_ARTIFACT_BYTES,
            });
        }
        let url = parse_url(&artifact.url)?;
        if !valid_https_url(&url) || url.origin() != self.allowed_origin.origin() {
            return Err(UpdateError::ArtifactOrigin);
        }

        let mut writer = BoundedFileWriter::new(staging_dir.to_owned(), artifact.size_bytes)?;
        let response = self
            .transport
            .get(&url, artifact.size_bytes, &mut writer)
            .await;
        // Wait for cleanup on ordinary failure, but report the transport error first.
        let staged_result = writer.finish().await;
        let response = response?;
        let (mut staged, facts) = staged_result?;

        if response.final_url.origin() != self.allowed_origin.origin() {
            return Err(UpdateError::ArtifactOrigin);
        }
        if response.redirected
            || response.final_url.as_str() != url.as_str()
            || (300..400).contains(&response.status)
        {
            return Err(UpdateError::ArtifactRedirect);
        }
        if response.status != reqwest::StatusCode::OK.as_u16() {
            return Err(UpdateError::ArtifactHttpStatus {
                status: response.status,
            });
        }
        if facts.count != artifact.size_bytes {
            return Err(UpdateError::SizeMismatch {
                expected: artifact.size_bytes,
                actual: facts.count,
            });
        }
        let digest = facts
            .digest
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        if digest != artifact.sha256 {
            return Err(UpdateError::DigestMismatch {
                platform: artifact.platform.clone(),
                arch: artifact.arch.clone(),
            });
        }
        staged.keep = true;
        Ok(DownloadedArtifact {
            path: staged.path.clone(),
            keep: false,
        })
    }
}

struct TempFile {
    path: PathBuf,
    file: Option<std::fs::File>,
    keep: bool,
}

impl Drop for TempFile {
    fn drop(&mut self) {
        self.file.take();
        if !self.keep {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

fn create_temp_file(dir: &Path) -> Result<TempFile, UpdateError> {
    for _ in 0..32 {
        let sequence = TEMP_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = dir.join(format!(
            ".kattri-update-{}-{sequence}.part",
            std::process::id()
        ));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => {
                return Ok(TempFile {
                    path,
                    file: Some(file),
                    keep: false,
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) => return Err(UpdateError::ArtifactFileIo),
        }
    }
    Err(UpdateError::ArtifactFileIo)
}

struct BoundedFileWriter {
    sender: mpsc::Sender<Vec<u8>>,
    result: oneshot::Receiver<Result<(TempFile, FileFacts), UpdateError>>,
    limit: u64,
    submitted: u64,
}

struct FileFacts {
    count: u64,
    digest: [u8; 32],
}

impl BoundedFileWriter {
    fn new(dir: PathBuf, limit: u64) -> Result<Self, UpdateError> {
        let (sender, mut receiver) = mpsc::channel::<Vec<u8>>(WRITER_QUEUE_CHUNKS);
        let (complete, result) = oneshot::channel();
        thread::Builder::new()
            .name("kattri-update-writer".to_owned())
            .spawn(move || {
                let outcome = (|| {
                    let mut staged = create_temp_file(&dir)?;
                    let mut file = staged.file.take().ok_or(UpdateError::ArtifactFileIo)?;
                    let mut count = 0u64;
                    let mut hash = Sha256::new();
                    while let Some(chunk) = receiver.blocking_recv() {
                        if complete.is_closed() {
                            return Err(UpdateError::ArtifactFileIo);
                        }
                        file.write_all(&chunk)
                            .map_err(|_| UpdateError::ArtifactFileIo)?;
                        hash.update(&chunk);
                        count = count
                            .checked_add(chunk.len() as u64)
                            .ok_or(UpdateError::ArtifactTooLarge { limit })?;
                    }
                    if complete.is_closed() {
                        return Err(UpdateError::ArtifactFileIo);
                    }
                    file.sync_all().map_err(|_| UpdateError::ArtifactFileIo)?;
                    drop(file);
                    Ok((
                        staged,
                        FileFacts {
                            count,
                            digest: hash.finalize().into(),
                        },
                    ))
                })();
                // On cancellation, either the worker or the undelivered result owns cleanup.
                let _ = complete.send(outcome);
            })
            .map_err(|_| UpdateError::ArtifactFileIo)?;
        Ok(Self {
            sender,
            result,
            limit,
            submitted: 0,
        })
    }

    async fn finish(self) -> Result<(TempFile, FileFacts), UpdateError> {
        drop(self.sender);
        self.result.await.map_err(|_| UpdateError::ArtifactFileIo)?
    }
}

impl ChunkSink for BoundedFileWriter {
    fn push<'a>(
        &'a mut self,
        chunk: &'a [u8],
    ) -> Pin<Box<dyn Future<Output = Result<(), UpdateError>> + Send + 'a>> {
        Box::pin(async move {
            let next = self
                .submitted
                .checked_add(chunk.len() as u64)
                .filter(|next| *next <= self.limit)
                .ok_or(UpdateError::ArtifactTooLarge { limit: self.limit })?;
            self.sender
                .send(chunk.to_vec())
                .await
                .map_err(|_| UpdateError::ArtifactFileIo)?;
            self.submitted = next;
            Ok(())
        })
    }
}

fn parse_url(value: &str) -> Result<Url, UpdateError> {
    Url::parse(value.trim()).map_err(|_| UpdateError::ArtifactOrigin)
}

fn valid_https_url(url: &Url) -> bool {
    url.scheme() == "https"
        && url.host_str().is_some()
        && url.username().is_empty()
        && url.password().is_none()
        && url.fragment().is_none()
}

#[cfg(test)]
mod reqwest_loopback_tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("Tokio runtime")
    }

    fn loopback_server(
        handler: impl FnOnce(TcpStream) + Send + 'static,
    ) -> (Url, thread::JoinHandle<()>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind loopback");
        let address = listener.local_addr().expect("loopback address");
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept loopback client");
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("set server read timeout");
            let mut request = Vec::new();
            let mut byte = [0u8; 1];
            while !request.ends_with(b"\r\n\r\n") {
                if stream.read_exact(&mut byte).is_err() {
                    return;
                }
                request.push(byte[0]);
            }
            handler(stream);
        });
        (
            Url::parse(&format!("http://{address}/artifact")).expect("loopback URL"),
            server,
        )
    }

    struct Collect(Vec<u8>);

    impl ChunkSink for Collect {
        fn push<'a>(
            &'a mut self,
            chunk: &'a [u8],
        ) -> Pin<Box<dyn Future<Output = Result<(), UpdateError>> + Send + 'a>> {
            self.0.extend_from_slice(chunk);
            Box::pin(async { Ok(()) })
        }
    }

    #[test]
    fn reqwest_returns_actual_non_success_status() {
        let (url, server) = loopback_server(|mut stream| {
            stream
                .write_all(
                    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .expect("write response");
        });
        let transport = ReqwestFileTransport::new().expect("transport");
        let mut sink = Collect(Vec::new());
        let result = runtime().block_on(transport.get(&url, 16, &mut sink));
        assert_eq!(result.expect("HTTP status response").status, 404);
        assert!(sink.0.is_empty());
        server.join().expect("server thread");
    }

    #[test]
    fn reqwest_refuses_redirect_without_following_location() {
        let (url, server) = loopback_server(|mut stream| {
            stream
                .write_all(b"HTTP/1.1 302 Found\r\nLocation: /elsewhere\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .expect("write redirect");
        });
        let transport = ReqwestFileTransport::new().expect("transport");
        let mut sink = Collect(Vec::new());
        let response = runtime()
            .block_on(transport.get(&url, 16, &mut sink))
            .expect("redirect response");
        assert_eq!(response.status, 302);
        assert!(response.redirected);
        assert_eq!(response.final_url, url);
        assert!(sink.0.is_empty());
        server.join().expect("server thread");
    }

    #[test]
    fn reqwest_rejects_oversized_content_length_before_streaming() {
        let (url, server) = loopback_server(|mut stream| {
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 9\r\nConnection: close\r\n\r\n")
                .expect("write headers");
        });
        let transport = ReqwestFileTransport::new().expect("transport");
        let mut sink = Collect(Vec::new());
        let error = runtime()
            .block_on(transport.get(&url, 8, &mut sink))
            .expect_err("oversized Content-Length");
        assert_eq!(error, UpdateError::ArtifactTooLarge { limit: 8 });
        assert!(sink.0.is_empty());
        server.join().expect("server thread");
    }

    #[test]
    fn reqwest_streams_before_server_finishes_body() {
        let observed = Arc::new(AtomicBool::new(false));
        let observed_by_server = Arc::clone(&observed);
        let (url, server) = loopback_server(move |mut stream| {
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n3\r\nabc\r\n")
                .expect("write first body chunk");
            let deadline = Instant::now() + Duration::from_secs(2);
            while !observed_by_server.load(Ordering::Acquire) && Instant::now() < deadline {
                thread::yield_now();
            }
            let received_first_chunk = observed_by_server.load(Ordering::Acquire);
            stream
                .write_all(b"3\r\ndef\r\n0\r\n\r\n")
                .expect("write final body chunk");
            assert!(
                received_first_chunk,
                "transport did not deliver the first chunk before body completion"
            );
        });
        let transport = ReqwestFileTransport::new().expect("transport");
        struct SignalSink(Arc<AtomicBool>);
        impl ChunkSink for SignalSink {
            fn push<'a>(
                &'a mut self,
                _chunk: &'a [u8],
            ) -> Pin<Box<dyn Future<Output = Result<(), UpdateError>> + Send + 'a>> {
                self.0.store(true, Ordering::Release);
                Box::pin(async { Ok(()) })
            }
        }
        let mut sink = SignalSink(observed);
        let response = runtime()
            .block_on(transport.get(&url, 16, &mut sink))
            .expect("streaming response");
        assert_eq!(response.status, 200);
        server.join().expect("server thread");
    }

    #[test]
    fn reqwest_reports_truncated_declared_body_as_transport_error() {
        let (url, server) = loopback_server(|mut stream| {
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\nConnection: close\r\n\r\nshort",
                )
                .expect("write truncated body");
        });
        let transport = ReqwestFileTransport::new().expect("transport");
        let mut sink = Collect(Vec::new());
        let error = runtime()
            .block_on(transport.get(&url, 16, &mut sink))
            .expect_err("truncated declared body");
        assert_eq!(error, UpdateError::ArtifactTransport);
        assert_eq!(sink.0, b"short");
        server.join().expect("server thread");
    }
}
