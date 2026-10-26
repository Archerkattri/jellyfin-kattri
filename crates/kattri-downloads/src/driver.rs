//! HTTP driver for one download, including range handling for resumed transfers.
//!
//! The driver never sets a job state directly. Every change goes through
//! `DownloadJob` methods, and every response is turned into a `RangeResponse` and
//! handed to `handle_response`. Bytes are written and persisted through
//! `fs::persist_chunk`, so the durable offset moves only after fsync.
//!
//! Credentials: the auth header is passed in per call as [`RequestAuth`]. It is never
//! stored on the job, the transfer or the file system. The URL is kept in memory only;
//! the manifest never contains it.
//!
//! The HTTP client is built by [`DriverConfig::new`], not by the caller. It follows only
//! same-origin redirects, so the credential is never sent to another host.
//!
//! Download source: [`OriginalFileSource`] is the only way to name the item to download.
//! It holds the validated `/Items/{itemId}/Download` URL and no credential. The auth
//! header travels separately as [`RequestAuth`], so a source or a record cannot carry it.
//!
//! Not in this module: playback negotiation (downloads never use PlaybackInfo, see
//! decision 0009).

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use jellyfin_api::SessionAuthorization;
use kattri_client_core::downloads::{
    DownloadJob, DownloadState, DownloadTransitionError, RangeDecision, RangeResponse, Validators,
};
use kattri_platform::DiskSpace;
use kattri_settings::guard::is_secret_like_value;
use reqwest::Url;
use reqwest::header::{self, HeaderName, HeaderValue};
use reqwest::redirect::Policy;

use crate::fs::{ChunkError, FsError, TempFile, TempStore, check_free_space, persist_chunk};
use crate::messages::Notice;
use crate::persist::{self, PersistedDownload};
use crate::verify::{ServerChecksum, Verification, VerifyError, verify_file};

/// Flush at least every 256 KiB written.
pub const SYNC_CHUNK_BYTES: usize = 256 * 1024;
/// Flush at least every 2 seconds while bytes are arriving.
pub const SYNC_INTERVAL: Duration = Duration::from_secs(2);
/// Network-level attempts per call before the download fails.
pub const MAX_NETWORK_ATTEMPTS: u32 = 3;
/// Base backoff between network attempts; multiplied by the attempt number.
pub const RETRY_BACKOFF: Duration = Duration::from_millis(200);
/// Most redirects followed for one download request.
pub const MAX_REDIRECTS: usize = 10;
/// Upper bound for waiting on response headers or body progress from one request.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Returns whether `target` has the same scheme, host and port as `original`.
pub fn redirect_stays_on_origin(original: &Url, target: &Url) -> bool {
    original.scheme() == target.scheme()
        && original.host_str() == target.host_str()
        && original.port_or_known_default() == target.port_or_known_default()
}

/// Redirect policy for the download client.
///
/// A redirect is followed only when it keeps the scheme, host and port of the
/// original request, and at most [`MAX_REDIRECTS`] are followed. Any other redirect
/// stops at the 3xx response, so the credential is never sent to another origin.
fn download_redirect_policy() -> Policy {
    Policy::custom(|attempt| {
        let previous = attempt.previous();
        let stays = previous
            .first()
            .is_some_and(|original| redirect_stays_on_origin(original, attempt.url()));
        if stays && previous.len() < MAX_REDIRECTS {
            attempt.follow()
        } else {
            attempt.stop()
        }
    })
}

/// Why a request produced no usable response.
enum SendError {
    /// The connection failed or the request could not complete. Retried.
    Network,
    /// A user-requested pause canceled the request before response headers arrived.
    Paused,
    /// The final URL left the original origin while a credential header was sent.
    RedirectRefused,
}

#[derive(Debug, thiserror::Error)]
pub enum DriverError {
    #[error("auth header name or value is not valid")]
    InvalidAuthHeader,
    #[error("item ID must be non-empty ASCII letters, digits or '-'")]
    InvalidItemId,
    #[error("base URL must start with http:// or https://")]
    InvalidBaseUrl,
    #[error("base URL must not contain a query, fragment, user info or credential text")]
    BaseUrlNotPlain,
    #[error("HTTP client could not be built")]
    ClientBuild,
}

/// Tunables for the driver. Defaults are the production values; tests shorten the backoff.
///
/// The HTTP client is private and built by [`DriverConfig::new`], so every download
/// uses the same-origin redirect policy.
#[derive(Clone)]
pub struct DriverConfig {
    client: reqwest::Client,
    pub store: TempStore,
    pub max_network_attempts: u32,
    pub retry_backoff: Duration,
    pub sync_chunk_bytes: usize,
    pub sync_interval: Duration,
    /// When set, the job snapshot is saved here after every durable advance and the
    /// record is removed on commit.
    pub snapshot_path: Option<PathBuf>,
    /// When set, a fresh temp file is refused if the volume cannot hold the full body.
    pub disk_space: Option<Arc<dyn DiskSpace + Send + Sync>>,
    /// Manifest file the app writes for this download. The driver only deletes it, and
    /// only when the download fails (see [`remove_failed_record`]).
    pub manifest_path: Option<PathBuf>,
}

impl DriverConfig {
    /// Builds the download client with the same-origin redirect policy and the
    /// default tunables.
    pub fn new(store: TempStore) -> Result<Self, DriverError> {
        let client = reqwest::Client::builder()
            .redirect(download_redirect_policy())
            .connect_timeout(REQUEST_TIMEOUT)
            .read_timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|_| DriverError::ClientBuild)?;
        Ok(Self {
            client,
            store,
            max_network_attempts: MAX_NETWORK_ATTEMPTS,
            retry_backoff: RETRY_BACKOFF,
            sync_chunk_bytes: SYNC_CHUNK_BYTES,
            sync_interval: SYNC_INTERVAL,
            snapshot_path: None,
            disk_space: None,
            manifest_path: None,
        })
    }
}

/// Credential header for one call. Not `Debug`, and marked sensitive so it is not logged.
pub struct RequestAuth {
    name: HeaderName,
    value: HeaderValue,
}

impl RequestAuth {
    pub fn new(name: &str, value: &str) -> Result<Self, DriverError> {
        let name =
            HeaderName::from_bytes(name.as_bytes()).map_err(|_| DriverError::InvalidAuthHeader)?;
        let mut value = HeaderValue::from_str(value).map_err(|_| DriverError::InvalidAuthHeader)?;
        value.set_sensitive(true);
        Ok(Self { name, value })
    }
}

/// Builds the original-file URL for an item.
///
/// Confirmed 2026-10-20 against the official generated SDK `LibraryApi.getDownload`
/// and Jellyfin's `LibraryController.GetDownload`. The server returns the original
/// item file; this route has no playback-profile or transcoding parameters.
pub fn original_file_url(base: &str, item_id: &str) -> Result<String, DriverError> {
    if item_id.is_empty()
        || !item_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
    {
        return Err(DriverError::InvalidItemId);
    }
    let base = base.trim_end_matches('/');
    if !(base.starts_with("http://") || base.starts_with("https://")) {
        return Err(DriverError::InvalidBaseUrl);
    }
    // A base with a query, fragment or user info would put a credential-like value into
    // the URL that the record and logs must never hold. Refuse it rather than strip it.
    // The guard's value list includes the scheme itself, so only the text after `://`
    // is checked for credential markers.
    let after_scheme = base.split_once("://").map_or("", |(_, rest)| rest);
    if base.contains(['?', '#']) || after_scheme.contains('@') || is_secret_like_value(after_scheme)
    {
        return Err(DriverError::BaseUrlNotPlain);
    }
    Ok(format!("{base}/Items/{item_id}/Download"))
}

/// The validated download source for one item.
///
/// Holds the original-file URL and nothing else. The credential is a [`RequestAuth`]
/// passed to [`Transfer::run`] per call, so this type has no path to a record or a log.
/// The URL is redacted from `Debug` because it names the upstream server.
#[derive(Clone, PartialEq, Eq)]
pub struct OriginalFileSource {
    url: String,
}

impl OriginalFileSource {
    /// Validates the base URL and item ID once. See [`original_file_url`].
    pub fn new(base: &str, item_id: &str) -> Result<Self, DriverError> {
        Ok(Self {
            url: original_file_url(base, item_id)?,
        })
    }

    /// The URL to request. Used by the driver only; never persist or log it.
    pub fn url(&self) -> &str {
        &self.url
    }
}

impl fmt::Debug for OriginalFileSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("OriginalFileSource([REDACTED])")
    }
}

/// One original-file download request: the validated source and the auth header that
/// goes with it. The token exists only inside `auth`, as a sensitive header value; the
/// source holds the URL alone, so neither field can reach a record or a URL.
pub struct OriginalFileRequest {
    pub source: OriginalFileSource,
    pub auth: RequestAuth,
}

impl OriginalFileRequest {
    /// Builds the source for `base` and `item_id`, and the `Authorization` header from the
    /// session's value. The item ID and base are validated before the header is built, so
    /// a refused item sends nothing.
    pub fn new(
        base: &str,
        item_id: &str,
        authorization: &SessionAuthorization,
    ) -> Result<Self, DriverError> {
        let source = OriginalFileSource::new(base, item_id)?;
        let auth = RequestAuth::new("Authorization", authorization.header_value())?;
        Ok(Self { source, auth })
    }
}

impl fmt::Debug for OriginalFileRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("OriginalFileRequest([REDACTED])")
    }
}

/// Removes everything a failed download left behind: its temp file, its snapshot record
/// and its manifest file. A missing item is not an error.
///
/// The temp file goes first. If it cannot be removed, the records are kept so a later
/// cleanup can retry; deleting the records first would orphan the bytes.
pub fn remove_failed_record(
    store: &TempStore,
    temp_name: Option<&str>,
    snapshot_path: Option<&Path>,
    manifest_path: Option<&Path>,
) -> Result<(), FsError> {
    if let Some(name) = temp_name {
        store.discard_named(name)?;
    }
    if let Some(path) = snapshot_path {
        persist::clear(path)?;
    }
    if let Some(path) = manifest_path {
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(FsError::Io(error)),
        }
    }
    Ok(())
}

/// Why a download ended in `Failed`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FailureReason {
    Model(DownloadTransitionError),
    Http(u16),
    DiskFull,
    Storage,
    NetworkExhausted,
    InvalidState,
    ChecksumMismatch,
    RedirectRefused,
}

/// Result of one `run` call. The job state matches the outcome.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Outcome {
    /// The file is complete and committed in the temp location (see `Transfer::temp_name`).
    Committed,
    /// Paused by request; the temp file and durable offset are kept.
    Paused,
    /// The server returned 401; the temp file is kept until the user signs in.
    NeedsAuth,
    Failed(FailureReason),
}

/// One download: the job, its temp file name and the in-memory URL.
pub struct Transfer {
    job: DownloadJob,
    url: String,
    temp_name: Option<String>,
    notice: Option<Notice>,
    stop: Arc<AtomicBool>,
    checksum: Option<ServerChecksum>,
    verification: Option<Verification>,
}

enum Step {
    Done(Outcome),
    Interrupted,
}

impl Transfer {
    pub fn new(url: String) -> Self {
        Self {
            job: DownloadJob::new(),
            url,
            temp_name: None,
            notice: None,
            stop: Arc::new(AtomicBool::new(false)),
            checksum: None,
            verification: None,
        }
    }

    /// A transfer for a validated item source. Callers that have an
    /// [`OriginalFileSource`] use this instead of [`Transfer::new`].
    pub fn from_source(source: &OriginalFileSource) -> Self {
        Self::new(source.url().to_owned())
    }

    /// A transfer rebuilt from a snapshot record after a restart. The caller
    /// supplies the URL again; it is never part of the record.
    pub fn restored(url: String, job: DownloadJob, temp_name: Option<String>) -> Self {
        Self {
            job,
            url,
            temp_name,
            notice: None,
            stop: Arc::new(AtomicBool::new(false)),
            checksum: None,
            verification: None,
        }
    }

    pub fn job(&self) -> &DownloadJob {
        &self.job
    }

    /// Random temp name (no title or ID). `None` before the first body byte.
    pub fn temp_name(&self) -> Option<&str> {
        self.temp_name.as_deref()
    }

    /// Server-provided checksum to verify against. Without one the download commits as
    /// `Unverified`.
    pub fn with_server_checksum(mut self, checksum: ServerChecksum) -> Self {
        self.checksum = Some(checksum);
        self
    }

    /// Verification status after `Committed` (`None` before commit).
    pub fn verification(&self) -> Option<Verification> {
        self.verification
    }

    /// The notice for the current attempt, if any. At most one per attempt.
    pub fn notice(&self) -> Option<Notice> {
        self.notice
    }

    /// Set to request a pause. The driver flushes, syncs and stops at the next chunk.
    pub fn pause_handle(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.stop)
    }

    /// Run one download call. Retries network failures up to `max_network_attempts`,
    /// resuming from the durable offset each time. A `Failed` outcome removes the temp
    /// file, the snapshot record and the manifest file (see [`remove_failed_record`]).
    pub async fn run(&mut self, cfg: &DriverConfig, auth: Option<&RequestAuth>) -> Outcome {
        let outcome = self.run_attempts(cfg, auth).await;
        // Only a job that reached `Failed` is cleaned up. A call rejected on a committed or
        // in-flight job leaves its state, and its bytes, untouched.
        if matches!(outcome, Outcome::Failed(_))
            && self.job.snapshot().state == DownloadState::Failed
            && remove_failed_record(
                &cfg.store,
                self.temp_name.as_deref(),
                cfg.snapshot_path.as_deref(),
                cfg.manifest_path.as_deref(),
            )
            .is_ok()
        {
            // Keep the name when cleanup failed, so a retry can still find the bytes.
            self.temp_name = None;
        }
        outcome
    }

    async fn run_attempts(&mut self, cfg: &DriverConfig, auth: Option<&RequestAuth>) -> Outcome {
        self.notice = None;
        self.stop.store(false, Ordering::SeqCst);
        match self.job.snapshot().state {
            DownloadState::Queued => {}
            DownloadState::Paused => {
                if self.job.resume().is_err() {
                    return self.fail(FailureReason::InvalidState);
                }
            }
            DownloadState::NeedsAuth => {
                if self.job.credentials_restored().is_err() {
                    return self.fail(FailureReason::InvalidState);
                }
            }
            _ => return self.fail(FailureReason::InvalidState),
        }

        let mut network_failures = 0u32;
        loop {
            match self.attempt(cfg, auth).await {
                Step::Done(outcome) => return outcome,
                Step::Interrupted => {
                    network_failures += 1;
                    if network_failures >= cfg.max_network_attempts {
                        return self.fail(FailureReason::NetworkExhausted);
                    }
                    if self.job.resume().is_err() {
                        return self.fail(FailureReason::InvalidState);
                    }
                    tokio::time::sleep(cfg.retry_backoff * network_failures).await;
                }
            }
        }
    }

    async fn attempt(&mut self, cfg: &DriverConfig, auth: Option<&RequestAuth>) -> Step {
        if let Err(error) = self.job.begin_negotiation() {
            return Step::Done(self.fail(FailureReason::Model(error)));
        }

        let mut restarted = false;
        loop {
            // Range only when the local file proves the durable bytes are present.
            let offset = self.job.snapshot().durable_offset;
            let mut temp: Option<TempFile> = None;
            let mut range_from = None;
            // After a restart decision the next request is a full GET (no Range).
            if offset > 0 && !restarted {
                let Some(name) = self.temp_name.clone() else {
                    return Step::Done(self.fail(FailureReason::Storage));
                };
                match cfg.store.open_resume(&name, offset) {
                    Ok(file) => {
                        temp = Some(file);
                        range_from = Some(offset);
                    }
                    Err(FsError::Missing | FsError::LengthMismatch { .. }) => {
                        // Partial file lost or damaged: send a full GET.
                        self.set_notice(Notice::LocalBytesLost);
                    }
                    Err(_) => return Step::Done(self.fail(FailureReason::Storage)),
                }
            }

            let response = match self.send(cfg, auth, range_from).await {
                Ok(response) => response,
                Err(SendError::Network) => return self.interrupted(),
                Err(SendError::Paused) => return self.pause(cfg),
                Err(SendError::RedirectRefused) => {
                    return Step::Done(self.fail(FailureReason::RedirectRefused));
                }
            };
            let status = response.status().as_u16();
            match status {
                401 => {
                    return match self.job.require_auth() {
                        Ok(()) => Step::Done(Outcome::NeedsAuth),
                        Err(error) => Step::Done(self.fail(FailureReason::Model(error))),
                    };
                }
                429 | 500..=599 => return self.interrupted(),
                200 => {
                    let decision = self.job.handle_response(RangeResponse::Ok {
                        content_length: response.content_length(),
                        validators: validators_from(&response),
                    });
                    if let Err(error) = decision {
                        return Step::Done(self.fail(FailureReason::Model(error)));
                    }
                    // A 200 to a range request means the range was ignored.
                    if range_from.is_some() {
                        self.set_notice(Notice::RangesIgnored);
                    }
                    drop(temp);
                    let fresh = match self.start_fresh(cfg) {
                        Ok(file) => file,
                        Err(outcome) => return Step::Done(outcome),
                    };
                    return self.stream_body(cfg, response, fresh).await;
                }
                206 => {
                    let Some((start, end_inclusive, total)) = response
                        .headers()
                        .get(header::CONTENT_RANGE)
                        .and_then(|v| v.to_str().ok())
                        .and_then(parse_content_range)
                    else {
                        return Step::Done(self.fail(FailureReason::Model(
                            DownloadTransitionError::InvalidContentRange,
                        )));
                    };
                    let decision = self.job.handle_response(RangeResponse::Partial {
                        start,
                        end_inclusive,
                        total,
                        validators: validators_from(&response),
                    });
                    match decision {
                        Ok(RangeDecision::Append { .. }) => {
                            let file = match temp.take() {
                                Some(file) => file,
                                None => match self.start_fresh(cfg) {
                                    Ok(file) => file,
                                    Err(outcome) => return Step::Done(outcome),
                                },
                            };
                            return self.stream_body(cfg, response, file).await;
                        }
                        Err(DownloadTransitionError::ValidatorMismatch) if !restarted => {
                            drop(temp);
                            restarted = true;
                            self.set_notice(Notice::ResourceChanged);
                            // Still Negotiating: send a full GET; its 200 restarts the file.
                            continue;
                        }
                        Err(DownloadTransitionError::MissingTrustedValidator) if !restarted => {
                            drop(temp);
                            restarted = true;
                            self.set_notice(Notice::RangeRejected);
                            continue;
                        }
                        Ok(_) => {
                            return Step::Done(self.fail(FailureReason::InvalidState));
                        }
                        Err(error) => {
                            return Step::Done(self.fail(FailureReason::Model(error)));
                        }
                    }
                }
                416 => {
                    let Some(total) = response
                        .headers()
                        .get(header::CONTENT_RANGE)
                        .and_then(|v| v.to_str().ok())
                        .and_then(parse_unsatisfied_total)
                    else {
                        return Step::Done(self.fail(FailureReason::Model(
                            DownloadTransitionError::InvalidContentRange,
                        )));
                    };
                    let decision = self.job.handle_response(RangeResponse::Unsatisfied {
                        total,
                        validators: validators_from(&response),
                    });
                    match decision {
                        Ok(RangeDecision::VerifyLocalBytes) => {
                            drop(temp);
                            return self.finish_and_commit(cfg);
                        }
                        Ok(RangeDecision::RestartFromZero) if !restarted => {
                            drop(temp);
                            restarted = true;
                            self.set_notice(Notice::RangeRejected);
                            // The model moved to Queued; negotiate again without a range.
                            if let Err(error) = self.job.begin_negotiation() {
                                return Step::Done(self.fail(FailureReason::Model(error)));
                            }
                            continue;
                        }
                        Ok(_) => return Step::Done(self.fail(FailureReason::InvalidState)),
                        Err(error) => return Step::Done(self.fail(FailureReason::Model(error))),
                    }
                }
                other => return Step::Done(self.fail(FailureReason::Http(other))),
            }
        }
    }

    async fn send(
        &self,
        cfg: &DriverConfig,
        auth: Option<&RequestAuth>,
        range_from: Option<u64>,
    ) -> Result<reqwest::Response, SendError> {
        let original = Url::parse(&self.url).map_err(|_| SendError::Network)?;
        let mut request = cfg.client.get(&self.url);
        if let Some(range_start) = range_from {
            request = request.header(header::RANGE, format!("bytes={range_start}-"));
        }
        if let Some(auth) = auth {
            request = request.header(auth.name.clone(), auth.value.clone());
        }
        let mut send = Box::pin(request.send());
        let response = loop {
            tokio::select! {
                result = &mut send => break result.map_err(|_| SendError::Network)?,
                _ = tokio::time::sleep(Duration::from_millis(50)) => {
                    if self.stop.load(Ordering::SeqCst) {
                        return Err(SendError::Paused);
                    }
                }
            }
        };
        // The redirect policy stops cross-origin redirects before they are followed.
        // This check is a second line of defense: a response whose final URL is on
        // another origin is refused when a credential was sent with the request.
        if auth.is_some() && !redirect_stays_on_origin(&original, response.url()) {
            return Err(SendError::RedirectRefused);
        }
        Ok(response)
    }

    async fn stream_body(
        &mut self,
        cfg: &DriverConfig,
        mut response: reqwest::Response,
        mut temp: TempFile,
    ) -> Step {
        let mut pending: Vec<u8> = Vec::new();
        let mut last_sync = Instant::now();
        loop {
            if self.stop.load(Ordering::SeqCst) {
                if let Err(outcome) = self.persist_pending(&mut temp, &mut pending, cfg, true) {
                    return Step::Done(outcome);
                }
                return match self.job.pause() {
                    Ok(()) => match self.checkpoint(cfg) {
                        Ok(()) => Step::Done(Outcome::Paused),
                        Err(outcome) => Step::Done(outcome),
                    },
                    Err(error) => Step::Done(self.fail(FailureReason::Model(error))),
                };
            }
            let next_chunk = tokio::select! {
                result = response.chunk() => Some(result),
                _ = tokio::time::sleep(Duration::from_millis(50)) => None,
            };
            let Some(next_chunk) = next_chunk else {
                // A pause request must be observed even when the server has stalled
                // without closing its response body.
                continue;
            };
            match next_chunk {
                Ok(Some(bytes)) => {
                    pending.extend_from_slice(&bytes);
                    if pending.len() >= cfg.sync_chunk_bytes
                        || last_sync.elapsed() >= cfg.sync_interval
                    {
                        if let Err(outcome) =
                            self.persist_pending(&mut temp, &mut pending, cfg, false)
                        {
                            return Step::Done(outcome);
                        }
                        last_sync = Instant::now();
                    }
                }
                Ok(None) => break,
                // Connection cut: unflushed bytes are dropped, the durable offset stays.
                Err(_) => return self.interrupted(),
            }
        }
        if let Err(outcome) = self.persist_pending(&mut temp, &mut pending, cfg, true) {
            return Step::Done(outcome);
        }
        drop(temp);
        let snapshot = self.job.snapshot();
        if let Some(expected) = snapshot.expected_total_length
            && snapshot.durable_offset < expected
        {
            // The body ended early: a network-level interruption, resume later.
            return self.interrupted();
        }
        self.finish_and_commit(cfg)
    }

    /// Verifying, then checksum, then Committed. A mismatch fails the job; the failure
    /// cleanup in [`Transfer::run`] then removes the bytes.
    fn finish_and_commit(&mut self, cfg: &DriverConfig) -> Step {
        if self.job.snapshot().state == DownloadState::Downloading
            && let Err(error) = self.job.finish_download()
        {
            return Step::Done(self.fail(FailureReason::Model(error)));
        }
        let Some(name) = self.temp_name.as_deref() else {
            return Step::Done(self.fail(FailureReason::Storage));
        };
        let path = cfg.store.dir().join(name);
        match verify_file(&path, self.checksum.as_ref()) {
            Ok(verification) => self.verification = Some(verification),
            Err(VerifyError::Mismatch) => {
                return Step::Done(self.fail(FailureReason::ChecksumMismatch));
            }
            Err(VerifyError::Io(_)) => return Step::Done(self.fail(FailureReason::Storage)),
        }
        match self.job.commit_verified() {
            Ok(()) => {
                // A stale record is harmless (it resumes from an offset whose file is gone
                // or complete), so a failed removal does not fail a committed download.
                if let Some(path) = cfg.snapshot_path.as_deref() {
                    let _ = persist::clear(path);
                }
                Step::Done(Outcome::Committed)
            }
            Err(error) => Step::Done(self.fail(FailureReason::Model(error))),
        }
    }

    /// Writes and syncs pending bytes in 256 KiB pieces. `all` also writes a short tail.
    fn persist_pending(
        &mut self,
        temp: &mut TempFile,
        pending: &mut Vec<u8>,
        cfg: &DriverConfig,
        all: bool,
    ) -> Result<(), Outcome> {
        let chunk = cfg.sync_chunk_bytes.max(1);
        let mut written = 0usize;
        while written < pending.len() && (all || pending.len() - written >= chunk) {
            let end = (written + chunk).min(pending.len());
            match persist_chunk(temp, &mut self.job, &pending[written..end]) {
                Ok(()) => {
                    written = end;
                    if let Err(outcome) = self.checkpoint(cfg) {
                        pending.drain(..written);
                        return Err(outcome);
                    }
                }
                Err(error) => {
                    pending.drain(..written);
                    return Err(self.map_chunk_error(error));
                }
            }
        }
        pending.drain(..written);
        Ok(())
    }

    fn map_chunk_error(&mut self, error: ChunkError) -> Outcome {
        let reason = match error {
            ChunkError::Io(error) if error.kind() == std::io::ErrorKind::StorageFull => {
                FailureReason::DiskFull
            }
            ChunkError::Io(_) => FailureReason::Storage,
            ChunkError::Job(error) => FailureReason::Model(error),
        };
        self.fail(reason)
    }

    /// Start a new empty temp file, discarding the previous one by name.
    fn start_fresh(&mut self, cfg: &DriverConfig) -> Result<TempFile, Outcome> {
        if let Some(old) = self.temp_name.take()
            && cfg.store.discard_named(&old).is_err()
        {
            return Err(self.fail(FailureReason::Storage));
        }
        if cfg.store.ensure_dir().is_err() {
            return Err(self.fail(FailureReason::Storage));
        }
        self.check_free_space(cfg)?;
        let file = match cfg.store.create_fresh() {
            Ok(file) => file,
            Err(_) => return Err(self.fail(FailureReason::Storage)),
        };
        self.temp_name = Some(file.name().to_owned());
        // Record the new (empty) file before any body byte is written, so a kill during
        // the first chunk cannot leave the record pointing at a discarded file.
        self.checkpoint(cfg)?;
        Ok(file)
    }

    /// Refuses a fresh file that cannot hold the whole body plus the spec margin. A failed
    /// space query does not block the download; the write-time `StorageFull` mapping still
    /// applies.
    fn check_free_space(&mut self, cfg: &DriverConfig) -> Result<(), Outcome> {
        let (Some(space), Some(total)) = (
            cfg.disk_space.as_deref(),
            self.job.snapshot().expected_total_length,
        ) else {
            return Ok(());
        };
        match space.available_bytes(cfg.store.dir()) {
            Ok(available) if check_free_space(available, total).is_err() => {
                Err(self.fail(FailureReason::DiskFull))
            }
            _ => Ok(()),
        }
    }

    /// Saves the job snapshot when a snapshot path is configured.
    fn checkpoint(&mut self, cfg: &DriverConfig) -> Result<(), Outcome> {
        let Some(path) = cfg.snapshot_path.as_deref() else {
            return Ok(());
        };
        let record = PersistedDownload::capture(self.job.snapshot(), self.temp_name.as_deref());
        match persist::save(path, &record) {
            Ok(()) => Ok(()),
            Err(_) => Err(self.fail(FailureReason::Storage)),
        }
    }

    fn interrupted(&mut self) -> Step {
        match self.job.pause() {
            Ok(()) => Step::Interrupted,
            Err(error) => Step::Done(self.fail(FailureReason::Model(error))),
        }
    }

    fn pause(&mut self, cfg: &DriverConfig) -> Step {
        match self.job.pause() {
            Ok(()) => match self.checkpoint(cfg) {
                Ok(()) => Step::Done(Outcome::Paused),
                Err(outcome) => Step::Done(outcome),
            },
            Err(error) => Step::Done(self.fail(FailureReason::Model(error))),
        }
    }

    fn set_notice(&mut self, notice: Notice) {
        if self.notice.is_none() {
            self.notice = Some(notice);
        }
    }

    /// Moves the job to Failed (resuming first if paused) and returns the outcome.
    fn fail(&mut self, reason: FailureReason) -> Outcome {
        if self.job.snapshot().state == DownloadState::Paused {
            let _ = self.job.resume();
        }
        let _ = self.job.fail();
        Outcome::Failed(reason)
    }
}

fn validators_from(response: &reqwest::Response) -> Validators {
    let text = |name: HeaderName| {
        response
            .headers()
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
    };
    Validators {
        etag: text(header::ETAG),
        last_modified: text(header::LAST_MODIFIED),
    }
}

/// Parses `bytes START-END/TOTAL`. Returns `None` for `*` or malformed values.
pub fn parse_content_range(value: &str) -> Option<(u64, u64, u64)> {
    let rest = value.trim().strip_prefix("bytes ")?;
    let (range, total) = rest.split_once('/')?;
    let (start, end) = range.split_once('-')?;
    Some((
        start.trim().parse().ok()?,
        end.trim().parse().ok()?,
        total.trim().parse().ok()?,
    ))
}

/// Parses the total from an unsatisfied range `bytes */TOTAL`.
pub fn parse_unsatisfied_total(value: &str) -> Option<u64> {
    let rest = value.trim().strip_prefix("bytes */")?;
    rest.trim().parse().ok()
}

#[cfg(test)]
mod source_and_cleanup_tests {
    use std::fs;

    use super::*;
    use crate::fs::DurableSink;

    #[test]
    fn source_uses_the_original_file_route_without_a_query() {
        let source = OriginalFileSource::new("http://127.0.0.1:8096/", "abc-1").expect("source");
        assert_eq!(source.url(), "http://127.0.0.1:8096/Items/abc-1/Download");
        assert!(!source.url().contains('?'));
    }

    #[test]
    fn source_debug_output_hides_the_upstream_url() {
        let source = OriginalFileSource::new("http://media.example", "abc-1").expect("source");
        let text = format!("{source:?}");
        assert_eq!(text, "OriginalFileSource([REDACTED])");
        assert!(!text.contains("media.example"));
    }

    #[test]
    fn base_with_a_query_fragment_or_user_info_is_refused() {
        for base in [
            "http://h/?api_key=abc",
            "http://h/#frag",
            "http://user:pw@h",
            "http://h/token=abc",
        ] {
            assert!(
                matches!(
                    OriginalFileSource::new(base, "abc-1"),
                    Err(DriverError::BaseUrlNotPlain)
                ),
                "{base} must be refused"
            );
        }
    }

    #[test]
    fn plain_base_with_a_path_prefix_is_accepted() {
        assert!(OriginalFileSource::new("https://media.example/jellyfin", "abc-1").is_ok());
    }

    #[test]
    fn item_ids_with_separators_or_empty_text_are_refused() {
        for item in ["", "a/b", "a?b", "a b"] {
            assert!(
                matches!(
                    OriginalFileSource::new("http://h", item),
                    Err(DriverError::InvalidItemId)
                ),
                "{item:?} must be refused"
            );
        }
    }

    #[test]
    fn remove_failed_record_removes_temp_snapshot_and_manifest() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = TempStore::new(dir.path().join("tmp"));
        let mut temp = store.create_fresh().expect("temp");
        temp.write_all_bytes(b"partial").expect("write");
        let name = temp.name().to_owned();
        let temp_path = temp.path().to_path_buf();
        drop(temp);
        let snapshot = dir.path().join("snapshot.json");
        let manifest = dir.path().join("manifest.json");
        fs::write(&snapshot, b"{}").expect("snapshot");
        fs::write(&manifest, b"{}").expect("manifest");

        remove_failed_record(
            &store,
            Some(name.as_str()),
            Some(snapshot.as_path()),
            Some(manifest.as_path()),
        )
        .expect("cleanup");

        assert!(!temp_path.exists(), "temp file is removed");
        assert!(!snapshot.exists(), "snapshot record is removed");
        assert!(!manifest.exists(), "manifest file is removed");
    }

    #[test]
    fn remove_failed_record_tolerates_files_that_are_already_gone() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = TempStore::new(dir.path().join("tmp"));
        let snapshot = dir.path().join("absent-snapshot.json");
        let manifest = dir.path().join("absent-manifest.json");
        let result = remove_failed_record(
            &store,
            Some("0123456789abcdef.part"),
            Some(snapshot.as_path()),
            Some(manifest.as_path()),
        );
        assert!(result.is_ok());
    }

    #[test]
    fn remove_failed_record_with_no_paths_is_a_no_op() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = TempStore::new(dir.path().join("tmp"));
        assert!(remove_failed_record(&store, None, None, None).is_ok());
    }
}

#[cfg(test)]
mod redirect_tests {
    use reqwest::Url;

    use super::redirect_stays_on_origin;

    fn url(text: &str) -> Url {
        Url::parse(text).expect("test URL")
    }

    #[test]
    fn same_scheme_host_and_port_stays_on_origin() {
        let original = url("https://media.example/Items/a/Download");
        assert!(redirect_stays_on_origin(
            &original,
            &url("https://media.example/other")
        ));
        assert!(redirect_stays_on_origin(
            &original,
            &url("https://media.example:443/other")
        ));
    }

    #[test]
    fn different_host_leaves_origin() {
        let original = url("https://media.example/Items/a/Download");
        assert!(!redirect_stays_on_origin(
            &original,
            &url("https://cdn.example/Items/a/Download")
        ));
        assert!(!redirect_stays_on_origin(
            &original,
            &url("https://media.example.evil/Items/a/Download")
        ));
    }

    #[test]
    fn different_port_leaves_origin() {
        let original = url("https://media.example/Items/a/Download");
        assert!(!redirect_stays_on_origin(
            &original,
            &url("https://media.example:8920/Items/a/Download")
        ));
    }

    #[test]
    fn different_scheme_leaves_origin() {
        let original = url("https://media.example/Items/a/Download");
        assert!(!redirect_stays_on_origin(
            &original,
            &url("http://media.example/Items/a/Download")
        ));
    }
}
