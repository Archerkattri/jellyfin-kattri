//! A loopback-only, same-origin relay for authenticated Jellyfin media.
//!
//! The relay accepts only a source request that is already bound to the
//! caller-supplied server origin, disables HTTP proxies and redirects, and
//! exposes upstream resources only through a per-session capability and
//! opaque random aliases.

pub mod cast_listener;

use std::{
    collections::{HashMap, HashSet},
    fmt,
    future::IntoFuture,
    hash::{Hash, Hasher},
    io,
    net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4, TcpListener as StdTcpListener},
    ops::Range,
    sync::{Arc, Mutex, mpsc},
    thread::{self, JoinHandle as ThreadJoinHandle},
    time::Duration,
};

use axum::{
    Router,
    body::Body,
    extract::{ConnectInfo, Path, State},
    http::{
        HeaderMap, Method, Request, Response, StatusCode,
        header::{self, HeaderName, HeaderValue},
    },
    routing::any,
};
use futures_util::{Stream, StreamExt, stream};
use jellyfin_api::{PlaybackSourceHeader, PlaybackSourceRequest};
use reqwest::{Client as HttpClient, redirect::Policy};
use thiserror::Error;
use tokio::{
    runtime::Builder as RuntimeBuilder,
    sync::oneshot,
    time::{Instant, timeout},
};
use url::Url;
use zeroize::Zeroizing;

const CAPABILITY_BYTES: usize = 32;
const RESOURCE_ID_BYTES: usize = 16;
const MAX_MANIFEST_BYTES: usize = 2 * 1024 * 1024;
const MAX_ALIAS_COUNT: usize = 4096;
const MAX_URI_REFERENCES: usize = 8192;
const MAX_UPSTREAM_URL_BYTES: usize = 64 * 1024;
const MAX_SOURCE_HEADER_COUNT: usize = 64;
const MAX_SOURCE_HEADER_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy)]
struct TimeoutPolicy {
    response_start: Duration,
    read_idle: Duration,
}

impl TimeoutPolicy {
    const DEFAULT: Self = Self {
        response_start: Duration::from_secs(30),
        read_idle: Duration::from_secs(30),
    };
}

/// Start one isolated relay for a negotiated source.
///
/// `allowed_server_origin` must be an origin only, such as
/// `https://media.example:8920/`. The source URL and every URI in every HLS
/// manifest must resolve to that exact HTTP(S) origin. Plain HTTP origins are
/// accepted only for loopback, private, and link-local hosts. The returned URL is
/// intentionally usable by a local media engine; its `Debug` representation
/// is redacted and its storage is zeroized on drop.
pub fn start_relay(
    allowed_server_origin: &str,
    source_request: PlaybackSourceRequest,
) -> Result<(RelayPlaybackUrl, RelayHandle), RelayError> {
    let allowed_origin = parse_allowed_origin(allowed_server_origin)?;

    let (source_url, source_headers) = source_request.into_parts();
    let parsed_source_url =
        Url::parse(source_url.as_str()).map_err(|_| RelayError::InvalidSource)?;
    validate_upstream_url(&parsed_source_url, &allowed_origin)
        .map_err(|_| RelayError::InvalidSource)?;
    let secret_source_url = SecretUrl::from_url(parsed_source_url);
    let secret_headers = validate_source_headers(source_headers)?;

    let mut aliases = AliasTable::default();
    let initial_resource_id = aliases.insert_initial(secret_source_url)?;
    let capability = random_hex(CAPABILITY_BYTES)?;

    let listener = StdTcpListener::bind(SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)))
        .map_err(|_| RelayError::BindFailed)?;
    let port = listener
        .local_addr()
        .map_err(|_| RelayError::BindFailed)?
        .port();

    let server_capability = capability.clone();
    let router_factory = move |client| {
        let state = Arc::new(RelayState {
            allowed_origin,
            capability: server_capability,
            aliases: Mutex::new(aliases),
            source_headers: secret_headers,
            client,
            timeout_policy: TimeoutPolicy::DEFAULT,
        });
        Router::new()
            .route("/r/{capability}/{resource_id}", any(relay_handler))
            .with_state(state)
    };
    let handle = spawn_server_thread_with_policy(listener, TimeoutPolicy::DEFAULT, router_factory)?;

    let url = Zeroizing::new(format!(
        "http://127.0.0.1:{port}/r/{}/{initial_resource_id}",
        capability.as_str()
    ));
    Ok((RelayPlaybackUrl(url), handle))
}

fn spawn_server_thread_with_policy<F>(
    listener: StdTcpListener,
    timeout_policy: TimeoutPolicy,
    router_factory: F,
) -> Result<RelayHandle, RelayError>
where
    F: FnOnce(HttpClient) -> Router + Send + 'static,
{
    listener
        .set_nonblocking(true)
        .map_err(|_| RelayError::BindFailed)?;
    let (shutdown_sender, shutdown_receiver) = oneshot::channel();
    let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
    let thread = thread::Builder::new()
        .name("jellyfin-playback-relay".to_owned())
        .spawn(move || {
            let runtime = match RuntimeBuilder::new_current_thread().enable_all().build() {
                Ok(runtime) => runtime,
                Err(_) => {
                    let _ = ready_sender.send(Err(RelayError::RuntimeInitialization));
                    return Err(RelayError::RuntimeInitialization);
                }
            };

            runtime.block_on(async move {
                let listener = match tokio::net::TcpListener::from_std(listener) {
                    Ok(listener) => listener,
                    Err(_) => {
                        let _ = ready_sender.send(Err(RelayError::BindFailed));
                        return Err(RelayError::BindFailed);
                    }
                };
                let client = match HttpClient::builder()
                    .redirect(Policy::none())
                    .no_proxy()
                    .connect_timeout(std::time::Duration::from_secs(10))
                    .read_timeout(timeout_policy.read_idle)
                    .build()
                {
                    Ok(client) => client,
                    Err(_) => {
                        let _ = ready_sender.send(Err(RelayError::ClientInitialization));
                        return Err(RelayError::ClientInitialization);
                    }
                };
                let router = router_factory(client);
                let server = axum::serve(
                    listener,
                    router.into_make_service_with_connect_info::<SocketAddr>(),
                )
                .into_future();
                if ready_sender.send(Ok(())).is_err() {
                    return Ok(());
                }
                tokio::select! {
                    result = server => result.map_err(|_| RelayError::ServerFailed),
                    _ = shutdown_receiver => Ok(()),
                }
            })
        })
        .map_err(|_| RelayError::ThreadStartFailed)?;

    match ready_receiver.recv() {
        Ok(Ok(())) => Ok(RelayHandle {
            shutdown_sender: Some(shutdown_sender),
            thread: Some(thread),
        }),
        Ok(Err(error)) => {
            let _ = thread.join();
            Err(error)
        }
        Err(_) => {
            let _ = thread.join();
            Err(RelayError::ServerFailed)
        }
    }
}

/// An authenticated loopback playback URL. Treat the value returned by
/// [`RelayPlaybackUrl::as_str`] as a bearer capability and do not log it.
pub struct RelayPlaybackUrl(Zeroizing<String>);

impl RelayPlaybackUrl {
    /// Borrow the URL for handoff to the local media engine.
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl fmt::Debug for RelayPlaybackUrl {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("RelayPlaybackUrl([REDACTED])")
    }
}

/// Owns the relay thread and runtime. Dropping the handle requests shutdown;
/// call [`RelayHandle::shutdown`] to stop it and join the serving thread.
pub struct RelayHandle {
    shutdown_sender: Option<oneshot::Sender<()>>,
    thread: Option<ThreadJoinHandle<Result<(), RelayError>>>,
}

impl RelayHandle {
    /// Stop the local listener and cancel outstanding relay requests.
    pub fn shutdown(mut self) -> Result<(), RelayError> {
        if let Some(sender) = self.shutdown_sender.take() {
            let _ = sender.send(());
        }
        let Some(thread) = self.thread.take() else {
            return Ok(());
        };
        match thread.join() {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(error),
            _ => Err(RelayError::ShutdownFailed),
        }
    }
}

impl Drop for RelayHandle {
    fn drop(&mut self) {
        if let Some(sender) = self.shutdown_sender.take() {
            let _ = sender.send(());
        }
    }
}

impl fmt::Debug for RelayHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("RelayHandle([REDACTED])")
    }
}

/// Errors intentionally contain no source URL, response body, header, or
/// transport-library error text.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum RelayError {
    #[error("invalid allowed server origin")]
    InvalidOrigin,
    #[error("source request is not safe for this relay")]
    InvalidSource,
    #[error("source request contains an unsupported header")]
    InvalidSourceHeader,
    #[error("secure random data is unavailable")]
    RandomUnavailable,
    #[error("could not bind the loopback relay")]
    BindFailed,
    #[error("could not initialize the relay HTTP client")]
    ClientInitialization,
    #[error("could not initialize the relay runtime")]
    RuntimeInitialization,
    #[error("could not start the relay thread")]
    ThreadStartFailed,
    #[error("the relay server stopped unexpectedly")]
    ServerFailed,
    #[error("the upstream server could not be reached")]
    UpstreamUnavailable,
    #[error("the upstream response timed out")]
    UpstreamTimeout,
    #[error("the upstream server returned a redirect")]
    RedirectRejected,
    #[error("the upstream manifest is invalid or unsafe")]
    InvalidManifest,
    #[error("the upstream manifest uses an unsupported URL-bearing attribute")]
    UnsupportedManifestReference,
    #[error("the upstream manifest exceeds the relay limit")]
    ManifestTooLarge,
    #[error("the manifest references another origin")]
    CrossOriginResource,
    #[error("the manifest contains too many URI references")]
    TooManyReferences,
    #[error("the relay resource alias limit was reached")]
    AliasLimitReached,
    #[error("the relay could not shut down cleanly")]
    ShutdownFailed,
}

struct RelayState {
    allowed_origin: Url,
    capability: Zeroizing<String>,
    aliases: Mutex<AliasTable>,
    source_headers: Vec<SecretHeader>,
    client: HttpClient,
    timeout_policy: TimeoutPolicy,
}

struct SecretHeader {
    name: HeaderName,
    value: Zeroizing<String>,
}

impl fmt::Debug for SecretHeader {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SecretHeader([REDACTED])")
    }
}

#[derive(Clone)]
struct SecretUrl(Zeroizing<String>);

impl SecretUrl {
    fn from_url(url: Url) -> Self {
        Self(Zeroizing::new(url.into()))
    }

    fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl PartialEq for SecretUrl {
    fn eq(&self, other: &Self) -> bool {
        self.as_str() == other.as_str()
    }
}

impl Eq for SecretUrl {}

impl Hash for SecretUrl {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.as_str().hash(state);
    }
}

impl fmt::Debug for SecretUrl {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SecretUrl([REDACTED])")
    }
}

#[derive(Default)]
struct AliasTable {
    by_id: HashMap<String, SecretUrl>,
    by_url: HashMap<SecretUrl, String>,
}

impl AliasTable {
    fn insert_initial(&mut self, url: SecretUrl) -> Result<String, RelayError> {
        let id = unique_resource_id(&self.by_id, &HashSet::new())?;
        self.by_url.insert(url.clone(), id.clone());
        self.by_id.insert(id.clone(), url);
        Ok(id)
    }

    fn get(&self, id: &str) -> Option<SecretUrl> {
        self.by_id.get(id).cloned()
    }

    fn register_batch(&mut self, urls: &[SecretUrl]) -> Result<Vec<String>, RelayError> {
        let mut new_ids: HashMap<SecretUrl, String> = HashMap::new();
        let mut additions = Vec::new();
        let mut ids = Vec::with_capacity(urls.len());
        let mut reserved_ids = HashSet::new();

        for url in urls {
            if let Some(id) = self.by_url.get(url) {
                ids.push(id.clone());
                continue;
            }
            if let Some(id) = new_ids.get(url) {
                ids.push(id.clone());
                continue;
            }
            let id = unique_resource_id(&self.by_id, &reserved_ids)?;
            reserved_ids.insert(id.clone());
            new_ids.insert(url.clone(), id.clone());
            additions.push((url.clone(), id.clone()));
            ids.push(id);
        }

        if self.by_id.len().saturating_add(additions.len()) > MAX_ALIAS_COUNT {
            return Err(RelayError::AliasLimitReached);
        }

        for (url, id) in additions {
            self.by_url.insert(url.clone(), id.clone());
            self.by_id.insert(id, url);
        }
        Ok(ids)
    }
}

async fn relay_handler(
    State(state): State<Arc<RelayState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Path((capability, resource_id)): Path<(String, String)>,
    request: Request<Body>,
) -> Response<Body> {
    let capability = Zeroizing::new(capability);
    if !peer.ip().is_loopback()
        || !constant_time_equal(capability.as_str(), state.capability.as_str())
    {
        return empty_response(StatusCode::NOT_FOUND);
    }
    let method = request.method().clone();
    if method != Method::GET && method != Method::HEAD {
        let mut response = empty_response(StatusCode::METHOD_NOT_ALLOWED);
        response
            .headers_mut()
            .insert(header::ALLOW, HeaderValue::from_static("GET, HEAD"));
        return response;
    }

    let Some(resource_url) = state
        .aliases
        .lock()
        .ok()
        .and_then(|aliases| aliases.get(&resource_id))
    else {
        return empty_response(StatusCode::NOT_FOUND);
    };
    let Ok(upstream_url) = Url::parse(resource_url.as_str()) else {
        return empty_response(StatusCode::BAD_GATEWAY);
    };
    if validate_upstream_url(&upstream_url, &state.allowed_origin).is_err() {
        return empty_response(StatusCode::BAD_GATEWAY);
    }
    let Ok(headers) = upstream_request_headers(request.headers(), &state.source_headers) else {
        return empty_response(StatusCode::BAD_GATEWAY);
    };

    let response_deadline = Instant::now() + state.timeout_policy.response_start;
    let upstream = match timeout(
        state.timeout_policy.response_start,
        state
            .client
            .request(method.clone(), upstream_url.clone())
            .headers(headers)
            .send(),
    )
    .await
    {
        Ok(Ok(response)) => response,
        Ok(Err(error)) if error.is_timeout() => {
            return response_for_error(RelayError::UpstreamTimeout);
        }
        Ok(Err(_)) => return response_for_error(RelayError::UpstreamUnavailable),
        Err(_) => return response_for_error(RelayError::UpstreamTimeout),
    };
    let status = upstream.status();
    if status.is_redirection() {
        return response_for_error(RelayError::RedirectRejected);
    }
    if upstream
        .headers()
        .get(header::CONTENT_ENCODING)
        .is_some_and(|value| !value.as_bytes().eq_ignore_ascii_case(b"identity"))
    {
        return empty_response(StatusCode::BAD_GATEWAY);
    }

    let is_manifest = status == StatusCode::OK && is_hls_manifest(&upstream_url, &upstream);
    if is_manifest && method == Method::HEAD {
        let selected_headers = selected_response_headers(upstream.headers());
        let mut response = empty_response(status);
        copy_selected_headers(response.headers_mut(), &selected_headers, true);
        if !response.headers().contains_key(header::CONTENT_TYPE) {
            response.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/vnd.apple.mpegurl"),
            );
        }
        return response;
    }
    if method == Method::HEAD {
        let selected_headers = selected_response_headers(upstream.headers());
        let mut response = empty_response(status);
        copy_selected_headers(response.headers_mut(), &selected_headers, false);
        return response;
    }
    let selected_headers = selected_response_headers(upstream.headers());
    let mut upstream_stream = upstream.bytes_stream();
    let first_chunk_timeout = response_deadline.saturating_duration_since(Instant::now());
    let first_chunk = match timeout(first_chunk_timeout, upstream_stream.next()).await {
        Ok(Some(Ok(chunk))) => Some(Ok(chunk)),
        Ok(Some(Err(error))) if error.is_timeout() => {
            return response_for_error(RelayError::UpstreamTimeout);
        }
        Ok(Some(Err(_))) => return response_for_error(RelayError::UpstreamUnavailable),
        Ok(None) => None,
        Err(_) => return response_for_error(RelayError::UpstreamTimeout),
    };
    #[cfg(debug_assertions)]
    if status.as_u16() >= 400 {
        // Neither a URL path nor an upstream error body is safe diagnostic input: both may
        // contain credentials or private media identifiers, even after query redaction.
        debug_note(&format!("{method} upstream status={status}"));
    }
    let upstream_stream = stream::iter(first_chunk).chain(upstream_stream);
    if is_manifest {
        let content_type = selected_headers.get(header::CONTENT_TYPE).cloned();
        let body = match read_limited_manifest(upstream_stream).await {
            Ok(body) => body,
            Err(error) => return response_for_error(error),
        };
        let rewritten = {
            let Ok(mut aliases) = state.aliases.lock() else {
                return empty_response(StatusCode::BAD_GATEWAY);
            };
            match rewrite_manifest(&body, &resource_url, &state.allowed_origin, &mut aliases) {
                Ok(rewritten) => rewritten,
                Err(error) => return response_for_error(error),
            }
        };
        drop(body);

        let mut response = empty_response(status);
        copy_selected_headers(response.headers_mut(), &selected_headers, true);
        if let Some(value) = content_type {
            response.headers_mut().insert(header::CONTENT_TYPE, value);
        } else {
            response.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/vnd.apple.mpegurl"),
            );
        }
        if let Ok(length) = HeaderValue::from_str(&rewritten.len().to_string()) {
            response
                .headers_mut()
                .insert(header::CONTENT_LENGTH, length);
        }
        *response.body_mut() = Body::from(rewritten.to_vec());
        return response;
    }

    let mut response = empty_response(status);
    copy_selected_headers(response.headers_mut(), &selected_headers, false);
    let stream = upstream_stream
        .map(|chunk| chunk.map_err(|_| io::Error::other("upstream media stream failed")));
    *response.body_mut() = Body::from_stream(stream);
    response
}

/// Debug builds only: appends a line to `relay.log` in `KATTRI_DEBUG_DIR`, when that is set.
#[cfg(debug_assertions)]
fn debug_note(line: &str) {
    use std::io::Write;
    let Some(dir) = std::env::var_os("KATTRI_DEBUG_DIR") else {
        return;
    };
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(std::path::Path::new(&dir).join("relay.log"))
    {
        let _ = writeln!(file, "{line}");
    }
}

fn selected_response_headers(source: &HeaderMap) -> HeaderMap {
    let mut selected = HeaderMap::new();
    for name in [
        header::ACCEPT_RANGES,
        header::CONTENT_RANGE,
        header::CONTENT_TYPE,
        header::CONTENT_LENGTH,
    ] {
        if let Some(value) = source.get(&name) {
            selected.insert(name, value.clone());
        }
    }
    selected
}

fn response_for_error(_error: RelayError) -> Response<Body> {
    empty_response(StatusCode::BAD_GATEWAY)
}

fn empty_response(status: StatusCode) -> Response<Body> {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = status;
    response
}

fn copy_selected_headers(
    destination: &mut HeaderMap,
    source: &HeaderMap,
    rewritten_manifest: bool,
) {
    for name in [
        header::ACCEPT_RANGES,
        header::CONTENT_RANGE,
        header::CONTENT_TYPE,
    ] {
        if let Some(value) = source.get(&name) {
            destination.insert(name, value.clone());
        }
    }
    if !rewritten_manifest && let Some(value) = source.get(header::CONTENT_LENGTH) {
        destination.insert(header::CONTENT_LENGTH, value.clone());
    }
}

fn upstream_request_headers(
    incoming: &HeaderMap,
    source_headers: &[SecretHeader],
) -> Result<HeaderMap, RelayError> {
    let mut headers = HeaderMap::new();
    for name in ["range", "if-range", "accept", "user-agent"] {
        let name =
            HeaderName::from_bytes(name.as_bytes()).map_err(|_| RelayError::InvalidSourceHeader)?;
        for value in incoming.get_all(&name).iter() {
            let mut value = value.clone();
            value.set_sensitive(true);
            headers.append(name.clone(), value);
        }
    }
    for source_header in source_headers {
        let mut value = HeaderValue::from_str(source_header.value.as_str())
            .map_err(|_| RelayError::InvalidSourceHeader)?;
        value.set_sensitive(true);
        headers.insert(source_header.name.clone(), value);
    }
    // Keep wire representation stable even if a reqwest feature enables
    // transparent content decoding. This value is generated by the relay, not
    // forwarded from the media engine.
    headers.insert(
        header::ACCEPT_ENCODING,
        HeaderValue::from_static("identity"),
    );
    Ok(headers)
}

fn validate_source_headers(
    headers: Vec<PlaybackSourceHeader>,
) -> Result<Vec<SecretHeader>, RelayError> {
    if headers.len() > MAX_SOURCE_HEADER_COUNT {
        return Err(RelayError::InvalidSourceHeader);
    }
    let mut result = Vec::with_capacity(headers.len());
    let mut has_authorization = false;
    let mut names = HashSet::new();
    let mut total_header_bytes = 0_usize;
    for header in headers {
        let (raw_name, raw_value) = header.as_pair();
        total_header_bytes = total_header_bytes
            .checked_add(raw_name.len())
            .and_then(|total| total.checked_add(raw_value.len()))
            .ok_or(RelayError::InvalidSourceHeader)?;
        if total_header_bytes > MAX_SOURCE_HEADER_BYTES {
            return Err(RelayError::InvalidSourceHeader);
        }
        let name = HeaderName::from_bytes(raw_name.as_bytes())
            .map_err(|_| RelayError::InvalidSourceHeader)?;
        if forbidden_source_header(&name) || !names.insert(name.clone()) {
            return Err(RelayError::InvalidSourceHeader);
        }
        if name == header::AUTHORIZATION {
            if has_authorization {
                return Err(RelayError::InvalidSourceHeader);
            }
            has_authorization = true;
        }
        HeaderValue::from_str(raw_value).map_err(|_| RelayError::InvalidSourceHeader)?;
        result.push(SecretHeader {
            name,
            value: Zeroizing::new(raw_value.to_owned()),
        });
    }
    if !has_authorization {
        return Err(RelayError::InvalidSourceHeader);
    }
    Ok(result)
}

fn forbidden_source_header(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "host"
            | "proxy-authorization"
            | "proxy-connection"
            | "connection"
            | "keep-alive"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
            | "content-length"
            | "expect"
            | "accept-encoding"
    )
}

async fn read_limited_manifest<S, B>(mut stream: S) -> Result<Zeroizing<Vec<u8>>, RelayError>
where
    S: Stream<Item = Result<B, reqwest::Error>> + Unpin,
    B: AsRef<[u8]>,
{
    let mut body = Zeroizing::new(Vec::new());
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| {
            if error.is_timeout() {
                RelayError::UpstreamTimeout
            } else {
                RelayError::UpstreamUnavailable
            }
        })?;
        let chunk = chunk.as_ref();
        if body.len().saturating_add(chunk.len()) > MAX_MANIFEST_BYTES {
            return Err(RelayError::ManifestTooLarge);
        }
        body.extend_from_slice(chunk);
    }
    Ok(body)
}

fn is_hls_manifest(url: &Url, response: &reqwest::Response) -> bool {
    if let Some(value) = response.headers().get(header::CONTENT_TYPE) {
        let value = value.to_str().unwrap_or_default();
        let media_type = value
            .split(';')
            .next()
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        if matches!(
            media_type.as_str(),
            "application/vnd.apple.mpegurl"
                | "application/x-mpegurl"
                | "audio/mpegurl"
                | "audio/x-mpegurl"
        ) {
            return true;
        }
    }
    url.path().to_ascii_lowercase().ends_with(".m3u8")
}

fn parse_allowed_origin(value: &str) -> Result<Url, RelayError> {
    let origin = Url::parse(value).map_err(|_| RelayError::InvalidOrigin)?;
    if !matches!(origin.scheme(), "http" | "https")
        || origin.host_str().is_none()
        || !origin.username().is_empty()
        || origin.password().is_some()
        || origin.path() != "/"
        || origin.query().is_some()
        || origin.fragment().is_some()
    {
        return Err(RelayError::InvalidOrigin);
    }
    // Plain HTTP would send the access token in cleartext, so it is accepted
    // only for hosts that cannot leave the local network.
    if origin.scheme() == "http" && !is_local_network_host(origin.host_str().unwrap_or_default()) {
        return Err(RelayError::InvalidOrigin);
    }
    Ok(origin)
}

/// Return whether a host names the loopback interface, a local network name,
/// or a private or link-local address.
fn is_local_network_host(host: &str) -> bool {
    let host = host
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
        .unwrap_or(host);
    let lowercase = host.to_ascii_lowercase();
    if lowercase == "localhost"
        || lowercase.ends_with(".localhost")
        || lowercase.ends_with(".local")
    {
        return true;
    }
    match host.parse::<IpAddr>() {
        Ok(IpAddr::V4(address)) => {
            address.is_loopback() || address.is_private() || address.is_link_local()
        }
        Ok(IpAddr::V6(address)) => {
            if address.is_loopback() || address.is_unicast_link_local() {
                return true;
            }
            address.segments()[0] & 0xfe00 == 0xfc00
        }
        Err(_) => false,
    }
}

fn validate_upstream_url(url: &Url, allowed_origin: &Url) -> Result<(), RelayError> {
    if url.as_str().len() > MAX_UPSTREAM_URL_BYTES
        || !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(RelayError::InvalidSource);
    }
    if url.origin() != allowed_origin.origin() {
        return Err(RelayError::CrossOriginResource);
    }
    Ok(())
}

fn rewrite_manifest(
    bytes: &[u8],
    playlist_url: &SecretUrl,
    allowed_origin: &Url,
    aliases: &mut AliasTable,
) -> Result<Zeroizing<Vec<u8>>, RelayError> {
    if bytes.len() > MAX_MANIFEST_BYTES {
        return Err(RelayError::ManifestTooLarge);
    }
    let text = std::str::from_utf8(bytes).map_err(|_| RelayError::InvalidManifest)?;
    let base_url = Url::parse(playlist_url.as_str()).map_err(|_| RelayError::InvalidManifest)?;
    validate_upstream_url(&base_url, allowed_origin).map_err(|_| RelayError::InvalidManifest)?;

    let mut lines = split_playlist_lines(text)?;
    let first_line = lines.first().ok_or(RelayError::InvalidManifest)?;
    if first_line
        .content
        .strip_prefix('\u{feff}')
        .unwrap_or(first_line.content.as_str())
        != "#EXTM3U"
    {
        return Err(RelayError::InvalidManifest);
    }

    let mut resolved = Vec::new();
    for line in &mut lines {
        line.uri_ranges = if line.content.starts_with('#') {
            uri_attribute_ranges(&line.content)?
        } else if line.content.is_empty() {
            Vec::new()
        } else {
            if line.content.trim() != line.content.as_str() {
                return Err(RelayError::InvalidManifest);
            }
            line.whole_line_uri = true;
            Vec::new()
        };
        if !line.whole_line_uri
            && has_absolute_url_outside_uri_ranges(&line.content, &line.uri_ranges)
        {
            return Err(RelayError::UnsupportedManifestReference);
        }
        let reference_count = line.uri_ranges.len() + usize::from(line.whole_line_uri);
        if resolved.len().saturating_add(reference_count) > MAX_URI_REFERENCES {
            return Err(RelayError::TooManyReferences);
        }
        if line.whole_line_uri {
            let url = resolve_hls_reference(&line.content, &base_url, allowed_origin)?;
            resolved.push(url);
        }
        for range in &line.uri_ranges {
            let reference = &line.content[range.clone()];
            let url = resolve_hls_reference(reference, &base_url, allowed_origin)?;
            resolved.push(url);
        }
    }

    let ids = aliases.register_batch(&resolved)?;
    let mut result = Zeroizing::new(Vec::with_capacity(bytes.len().min(MAX_MANIFEST_BYTES)));
    let mut id_index = 0;
    for line in lines {
        let mut cursor = 0;
        if line.whole_line_uri {
            result.extend_from_slice(b"./");
            result.extend_from_slice(ids[id_index].as_bytes());
            id_index += 1;
            cursor = line.content.len();
        }
        for range in line.uri_ranges {
            result.extend_from_slice(line.content[cursor..range.start].as_bytes());
            result.extend_from_slice(b"./");
            result.extend_from_slice(ids[id_index].as_bytes());
            id_index += 1;
            cursor = range.end;
        }
        result.extend_from_slice(line.content[cursor..].as_bytes());
        result.extend_from_slice(line.ending.as_bytes());
    }
    debug_assert_eq!(id_index, ids.len());
    Ok(result)
}

struct PlaylistLine {
    content: Zeroizing<String>,
    ending: String,
    uri_ranges: Vec<Range<usize>>,
    whole_line_uri: bool,
}

fn split_playlist_lines(text: &str) -> Result<Vec<PlaylistLine>, RelayError> {
    let mut lines = Vec::new();
    for line in text.split_inclusive('\n') {
        let (without_lf, ending) = match line.strip_suffix('\n') {
            Some(without_lf) => (without_lf, "\n"),
            None => (line, ""),
        };
        let (content, ending) = match without_lf.strip_suffix('\r') {
            Some(content) if ending == "\n" => (content, "\r\n"),
            Some(_) => return Err(RelayError::InvalidManifest),
            None => (without_lf, ending),
        };
        lines.push(PlaylistLine {
            content: Zeroizing::new(content.to_owned()),
            ending: ending.to_owned(),
            uri_ranges: Vec::new(),
            whole_line_uri: false,
        });
    }
    if lines.is_empty() {
        return Err(RelayError::InvalidManifest);
    }
    Ok(lines)
}

fn uri_attribute_ranges(line: &str) -> Result<Vec<Range<usize>>, RelayError> {
    let tag = line.split(':').next().unwrap_or(line);
    if tag.eq_ignore_ascii_case("#EXT-X-CONTENT-STEERING") {
        return Err(RelayError::UnsupportedManifestReference);
    }
    let Some(colon) = line.find(':') else {
        return Ok(Vec::new());
    };
    let bytes = line.as_bytes();
    let start = colon + 1;
    let mut segment_start = start;
    let mut in_quotes = false;
    let mut ranges = Vec::new();

    for index in start..=bytes.len() {
        if index < bytes.len() && bytes[index] == b'"' {
            in_quotes = !in_quotes;
        }
        if index == bytes.len() || (bytes[index] == b',' && !in_quotes) {
            let segment = &line[segment_start..index];
            if let Some(equal) = segment.find('=') {
                let key = segment[..equal].trim();
                let value_start = segment_start + equal + 1;
                let value = segment[equal + 1..].trim();
                let leading = segment[equal + 1..].len() - segment[equal + 1..].trim_start().len();
                let value_start = value_start + leading;
                let normalized_key = key.to_ascii_uppercase();
                let url_named = normalized_key.contains("URI") || normalized_key.contains("URL");
                if normalized_key == "URI" {
                    if !tag_supports_uri_attribute(tag) {
                        return Err(RelayError::UnsupportedManifestReference);
                    }
                    if value.len() < 2 || !value.starts_with('"') || !value.ends_with('"') {
                        return Err(RelayError::InvalidManifest);
                    }
                    let inner = &value[1..value.len() - 1];
                    if inner.contains('"') {
                        return Err(RelayError::InvalidManifest);
                    }
                    ranges.push(value_start + 1..value_start + value.len() - 1);
                } else if url_named || looks_like_url_attribute_value(value, tag, key) {
                    return Err(RelayError::UnsupportedManifestReference);
                }
            }
            segment_start = index + 1;
        }
    }
    if in_quotes {
        return Err(RelayError::InvalidManifest);
    }
    Ok(ranges)
}

fn has_absolute_url_outside_uri_ranges(line: &str, uri_ranges: &[Range<usize>]) -> bool {
    let mut cursor = 0;
    for range in uri_ranges {
        if line[cursor..range.start].contains("://") {
            return true;
        }
        cursor = range.end;
    }
    line[cursor..].contains("://")
}

fn tag_supports_uri_attribute(tag: &str) -> bool {
    [
        "#EXT-X-KEY",
        "#EXT-X-MAP",
        "#EXT-X-MEDIA",
        "#EXT-X-I-FRAME-STREAM-INF",
        "#EXT-X-SESSION-DATA",
        "#EXT-X-SESSION-KEY",
        "#EXT-X-PRELOAD-HINT",
        "#EXT-X-RENDITION-REPORT",
        "#EXT-X-IMAGE-STREAM-INF",
        "#EXT-X-PART",
    ]
    .iter()
    .any(|supported| tag.eq_ignore_ascii_case(supported))
}

fn tag_supports_attribute(tag: &str, key: &str) -> bool {
    let tag = tag.to_ascii_uppercase();
    let key = key.to_ascii_uppercase();
    let allowed: &[&str] = match tag.as_str() {
        "#EXT-X-KEY" => &["METHOD", "URI", "IV", "KEYFORMAT", "KEYFORMATVERSIONS"],
        "#EXT-X-MAP" => &["URI", "BYTERANGE"],
        "#EXT-X-MEDIA" => &[
            "TYPE",
            "URI",
            "GROUP-ID",
            "LANGUAGE",
            "ASSOC-LANGUAGE",
            "NAME",
            "DEFAULT",
            "AUTOSELECT",
            "FORCED",
            "INSTREAM-ID",
            "CHARACTERISTICS",
            "CHANNELS",
            "STABLE-RENDITION-ID",
        ],
        "#EXT-X-I-FRAME-STREAM-INF" => &[
            "BANDWIDTH",
            "URI",
            "AVERAGE-BANDWIDTH",
            "CODECS",
            "RESOLUTION",
            "HDCP-LEVEL",
            "VIDEO",
            "SCORE",
            "STABLE-VARIANT-ID",
            "VIDEO-RANGE",
            "REQ-VIDEO-LAYOUT",
        ],
        "#EXT-X-SESSION-DATA" => &["DATA-ID", "VALUE", "URI", "LANGUAGE"],
        "#EXT-X-SESSION-KEY" => &["METHOD", "URI", "IV", "KEYFORMAT", "KEYFORMATVERSIONS"],
        "#EXT-X-PRELOAD-HINT" => &["TYPE", "URI", "BYTERANGE-START", "BYTERANGE-LENGTH"],
        "#EXT-X-RENDITION-REPORT" => &["URI", "LAST-MSN", "LAST-PART"],
        "#EXT-X-IMAGE-STREAM-INF" => &[
            "BANDWIDTH",
            "URI",
            "CODECS",
            "RESOLUTION",
            "VIDEO",
            "SCORE",
            "STABLE-VARIANT-ID",
            "VIDEO-RANGE",
            "REQ-VIDEO-LAYOUT",
            "PATHWAY-ID",
        ],
        "#EXT-X-PART" => &["DURATION", "URI", "INDEPENDENT", "BYTERANGE", "GAP"],
        "#EXT-X-STREAM-INF" => &[
            "BANDWIDTH",
            "AVERAGE-BANDWIDTH",
            "CODECS",
            "RESOLUTION",
            "FRAME-RATE",
            "HDCP-LEVEL",
            "AUDIO",
            "VIDEO",
            "SUBTITLES",
            "CLOSED-CAPTIONS",
            "SCORE",
            "STABLE-VARIANT-ID",
            "VIDEO-RANGE",
            "REQ-VIDEO-LAYOUT",
            "PATHWAY-ID",
        ],
        "#EXT-X-DATERANGE" => &[
            "ID",
            "CLASS",
            "START-DATE",
            "CUE",
            "END-DATE",
            "DURATION",
            "PLANNED-DURATION",
            "END-ON-NEXT",
            "SCTE35-CMD",
            "SCTE35-OUT",
            "SCTE35-IN",
            "SCTE35-TYPE",
        ],
        "#EXT-X-DEFINE" => &["NAME", "VALUE", "IMPORT", "QUERYPARAM"],
        "#EXT-X-SERVER-CONTROL" => &[
            "CAN-BLOCK-RELOAD",
            "CAN-SKIP-UNTIL",
            "HOLD-BACK",
            "PART-HOLD-BACK",
            "CAN-SKIP-DATERANGES",
        ],
        "#EXT-X-PART-INF" => &["PART-TARGET"],
        "#EXT-X-SKIP" => &["SKIPPED-SEGMENTS", "RECENTLY-REMOVED-DATERANGES"],
        "#EXT-X-START" => &["TIME-OFFSET", "PRECISE"],
        _ => return false,
    };
    allowed.contains(&key.as_str())
}

fn looks_like_url_attribute_value(value: &str, tag: &str, key: &str) -> bool {
    let value = value.trim();
    let known_non_url_slash_value = (tag.eq_ignore_ascii_case("#EXT-X-MEDIA")
        && key.eq_ignore_ascii_case("CHANNELS"))
        || (tag.eq_ignore_ascii_case("#EXT-X-KEY")
            && key.eq_ignore_ascii_case("KEYFORMATVERSIONS"))
        || (tag_supports_attribute(tag, key)
            && !key.eq_ignore_ascii_case("URI")
            && !key.to_ascii_uppercase().contains("URL"));
    value.contains("://")
        || value.starts_with('/')
        || value.starts_with("./")
        || value.starts_with("../")
        || value.contains('?')
        || value.contains('#')
        || value.contains('\\')
        || (value.contains('/') && !known_non_url_slash_value)
        || value.split_once(':').is_some_and(|(scheme, _)| {
            !scheme.is_empty() && scheme.bytes().all(|byte| byte.is_ascii_alphabetic())
        })
}

fn resolve_hls_reference(
    reference: &str,
    playlist_url: &Url,
    allowed_origin: &Url,
) -> Result<SecretUrl, RelayError> {
    if reference.is_empty()
        || reference.chars().any(char::is_control)
        || reference.trim() != reference
        || reference.contains("{$")
    {
        return Err(RelayError::InvalidManifest);
    }
    let resolved = playlist_url
        .join(reference)
        .map_err(|_| RelayError::InvalidManifest)?;
    validate_upstream_url(&resolved, allowed_origin).map_err(|error| match error {
        RelayError::CrossOriginResource => RelayError::CrossOriginResource,
        _ => RelayError::InvalidManifest,
    })?;
    Ok(SecretUrl::from_url(resolved))
}

fn unique_resource_id(
    existing: &HashMap<String, SecretUrl>,
    reserved: &HashSet<String>,
) -> Result<String, RelayError> {
    loop {
        let token = random_hex(RESOURCE_ID_BYTES)?;
        let id = token.as_str().to_owned();
        if !existing.contains_key(&id) && !reserved.contains(&id) {
            return Ok(id);
        }
    }
}

fn random_hex(byte_count: usize) -> Result<Zeroizing<String>, RelayError> {
    let mut random = Zeroizing::new(vec![0_u8; byte_count]);
    getrandom::fill(&mut random).map_err(|_| RelayError::RandomUnavailable)?;
    let mut output = Zeroizing::new(String::with_capacity(byte_count * 2));
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for byte in random.iter().copied() {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    Ok(output)
}

fn constant_time_equal(left: &str, right: &str) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.bytes()
        .zip(right.bytes())
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use tokio::time::sleep;

    struct DropSignal(Option<oneshot::Sender<()>>);

    impl Drop for DropSignal {
        fn drop(&mut self) {
            if let Some(sender) = self.0.take() {
                let _ = sender.send(());
            }
        }
    }

    fn origin() -> Url {
        Url::parse("https://jellyfin.example:8920/").expect("valid test origin")
    }

    fn initial_aliases(source: &str) -> (SecretUrl, AliasTable) {
        let url = Url::parse(source).expect("valid test source");
        let mut aliases = AliasTable::default();
        aliases
            .insert_initial(SecretUrl::from_url(url.clone()))
            .expect("random alias");
        (SecretUrl::from_url(url), aliases)
    }

    async fn start_timeout_fixture(
        upstream_router: Router,
        path: &str,
        timeout_policy: TimeoutPolicy,
    ) -> (String, RelayHandle, tokio::task::JoinHandle<()>) {
        let upstream_listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind upstream fixture");
        let upstream_port = upstream_listener
            .local_addr()
            .expect("upstream fixture address")
            .port();
        let upstream_task = tokio::spawn(async move {
            axum::serve(upstream_listener, upstream_router)
                .await
                .expect("upstream fixture should serve");
        });

        let allowed_origin = Url::parse(&format!("http://127.0.0.1:{upstream_port}/"))
            .expect("valid upstream origin");
        let upstream_url = allowed_origin.join(path).expect("valid upstream URL");
        let mut aliases = AliasTable::default();
        let resource_id = aliases
            .insert_initial(SecretUrl::from_url(upstream_url))
            .expect("create media alias");
        let capability = random_hex(CAPABILITY_BYTES).expect("random test capability");
        let route_capability = capability.clone();
        let relay_listener = StdTcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))
            .expect("bind relay fixture");
        let relay_port = relay_listener
            .local_addr()
            .expect("relay fixture address")
            .port();
        let relay_policy = timeout_policy;
        let relay_handle =
            spawn_server_thread_with_policy(relay_listener, timeout_policy, move |client| {
                let state = Arc::new(RelayState {
                    allowed_origin,
                    capability: route_capability,
                    aliases: Mutex::new(aliases),
                    source_headers: Vec::new(),
                    client,
                    timeout_policy: relay_policy,
                });
                Router::new()
                    .route("/r/{capability}/{resource_id}", any(relay_handler))
                    .with_state(state)
            })
            .expect("relay fixture should start");
        let relay_url = format!(
            "http://127.0.0.1:{relay_port}/r/{}/{resource_id}",
            capability.as_str()
        );
        (relay_url, relay_handle, upstream_task)
    }

    fn fixture_client() -> HttpClient {
        HttpClient::builder()
            .no_proxy()
            .build()
            .expect("fixture HTTP client")
    }

    #[tokio::test]
    async fn upstream_redirect_is_rejected_without_following_it() {
        let router = Router::new()
            .route(
                "/redirect",
                axum::routing::get(|| async {
                    let mut response = Response::new(Body::from("redirect"));
                    *response.status_mut() = StatusCode::FOUND;
                    response
                        .headers_mut()
                        .insert(header::LOCATION, HeaderValue::from_static("/target"));
                    response
                }),
            )
            .route("/target", axum::routing::get(|| async { "target" }));
        let (url, relay, upstream) =
            start_timeout_fixture(router, "redirect", TimeoutPolicy::DEFAULT).await;

        let response = fixture_client()
            .get(url)
            .send()
            .await
            .expect("relay should return a gateway response");
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);

        relay.shutdown().expect("relay fixture should stop");
        upstream.abort();
    }

    #[tokio::test]
    async fn upstream_client_and_server_errors_pass_through_with_their_status() {
        for status in [
            StatusCode::BAD_REQUEST,
            StatusCode::UNAUTHORIZED,
            StatusCode::FORBIDDEN,
            StatusCode::NOT_FOUND,
            StatusCode::RANGE_NOT_SATISFIABLE,
            StatusCode::INTERNAL_SERVER_ERROR,
            StatusCode::SERVICE_UNAVAILABLE,
        ] {
            let router = Router::new().route(
                "/media.mkv",
                axum::routing::get(move || async move {
                    let mut response = Response::new(Body::from("upstream said no"));
                    *response.status_mut() = status;
                    response
                }),
            );
            let (url, relay, upstream) =
                start_timeout_fixture(router, "media.mkv", TimeoutPolicy::DEFAULT).await;

            let response = fixture_client()
                .get(url)
                .send()
                .await
                .expect("relay should answer");
            assert_eq!(response.status(), status, "status must pass through");

            relay.shutdown().expect("relay fixture should stop");
            upstream.abort();
        }
    }

    #[tokio::test]
    async fn oversized_hls_manifest_from_upstream_is_a_bad_gateway() {
        let router = Router::new().route(
            "/huge.m3u8",
            axum::routing::get(|| async {
                let mut body = String::from("#EXTM3U\n");
                while body.len() <= MAX_MANIFEST_BYTES {
                    body.push_str("#EXT-X-VERSION:3\n");
                }
                let mut response = Response::new(Body::from(body));
                response.headers_mut().insert(
                    header::CONTENT_TYPE,
                    HeaderValue::from_static("application/vnd.apple.mpegurl"),
                );
                response
            }),
        );
        let (url, relay, upstream) =
            start_timeout_fixture(router, "huge.m3u8", TimeoutPolicy::DEFAULT).await;

        let response = fixture_client()
            .get(url)
            .send()
            .await
            .expect("relay should answer");
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);

        relay.shutdown().expect("relay fixture should stop");
        upstream.abort();
    }

    #[tokio::test]
    async fn manifest_served_through_the_relay_hides_every_upstream_reference() {
        let router = Router::new().route(
            "/hls/main.m3u8",
            axum::routing::get(|| async {
                let mut response = Response::new(Body::from(
                    "#EXTM3U\n#EXT-X-MAP:URI=\"init.mp4?api_key=map-secret\"\n#EXTINF:4,\nseg-1.m4s?api_key=seg-secret\n",
                ));
                response.headers_mut().insert(
                    header::CONTENT_TYPE,
                    HeaderValue::from_static("application/vnd.apple.mpegurl"),
                );
                response
            }),
        );
        let (url, relay, upstream) =
            start_timeout_fixture(router, "hls/main.m3u8", TimeoutPolicy::DEFAULT).await;

        let response = fixture_client()
            .get(url)
            .send()
            .await
            .expect("relay should answer");
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.text().await.expect("manifest body");
        for secret in [
            "map-secret",
            "seg-secret",
            "init.mp4",
            "seg-1.m4s",
            "127.0.0.1",
        ] {
            assert!(!body.contains(secret), "{secret} leaked: {body}");
        }
        assert!(body.starts_with("#EXTM3U"));

        relay.shutdown().expect("relay fixture should stop");
        upstream.abort();
    }

    #[tokio::test]
    async fn redirect_on_a_manifest_url_is_rejected_too() {
        let router = Router::new().route(
            "/main.m3u8",
            axum::routing::get(|| async {
                let mut response = Response::new(Body::empty());
                *response.status_mut() = StatusCode::TEMPORARY_REDIRECT;
                response.headers_mut().insert(
                    header::LOCATION,
                    HeaderValue::from_static("http://127.0.0.1:1/other.m3u8"),
                );
                response
            }),
        );
        let (url, relay, upstream) =
            start_timeout_fixture(router, "main.m3u8", TimeoutPolicy::DEFAULT).await;

        let response = fixture_client()
            .get(url)
            .send()
            .await
            .expect("relay should answer");
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        assert!(response.headers().get(header::LOCATION).is_none());

        relay.shutdown().expect("relay fixture should stop");
        upstream.abort();
    }

    #[tokio::test]
    async fn stalled_upstream_headers_fail_within_response_start_deadline() {
        let router = Router::new().route(
            "/headers",
            axum::routing::get(|| async {
                futures_util::future::pending::<Response<Body>>().await
            }),
        );
        let policy = TimeoutPolicy {
            response_start: Duration::from_millis(150),
            read_idle: Duration::from_millis(500),
        };
        let (url, relay, upstream) = start_timeout_fixture(router, "headers", policy).await;

        let response = timeout(Duration::from_secs(2), fixture_client().get(url).send())
            .await
            .expect("relay should bound the stalled headers")
            .expect("relay should return a gateway response");
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);

        relay.shutdown().expect("relay fixture should stop");
        upstream.abort();
    }

    #[tokio::test]
    async fn stalled_first_body_chunk_fails_within_response_start_deadline() {
        let router = Router::new().route(
            "/body",
            axum::routing::get(|| async {
                Body::from_stream(stream::once(async {
                    sleep(Duration::from_secs(2)).await;
                    Ok::<_, io::Error>(b"late".to_vec())
                }))
            }),
        );
        let policy = TimeoutPolicy {
            response_start: Duration::from_millis(150),
            read_idle: Duration::from_millis(500),
        };
        let (url, relay, upstream) = start_timeout_fixture(router, "body", policy).await;

        let response = timeout(Duration::from_secs(2), fixture_client().get(url).send())
            .await
            .expect("relay should bound the stalled first body chunk")
            .expect("relay should return a gateway response");
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);

        relay.shutdown().expect("relay fixture should stop");
        upstream.abort();
    }

    #[tokio::test]
    async fn stalled_head_headers_fail_within_response_start_deadline() {
        let (started_sender, started_receiver) = oneshot::channel();
        let started_sender = Arc::new(Mutex::new(Some(started_sender)));
        let router = Router::new().route(
            "/head",
            axum::routing::any(move || {
                if let Ok(mut sender) = started_sender.lock()
                    && let Some(sender) = sender.take()
                {
                    let _ = sender.send(());
                }
                async { futures_util::future::pending::<Response<Body>>().await }
            }),
        );
        let policy = TimeoutPolicy {
            response_start: Duration::from_millis(150),
            read_idle: Duration::from_millis(500),
        };
        let (url, relay, upstream) = start_timeout_fixture(router, "head", policy).await;
        let request = tokio::spawn(async move { fixture_client().head(url).send().await });
        timeout(Duration::from_secs(2), started_receiver)
            .await
            .expect("upstream HEAD handler should start")
            .expect("upstream HEAD start signal should arrive");
        let response = timeout(Duration::from_secs(2), request)
            .await
            .expect("HEAD response-start timeout should complete")
            .expect("HEAD request task should complete")
            .expect("relay should return a gateway response");
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);

        relay.shutdown().expect("relay fixture should stop");
        upstream.abort();
    }

    #[tokio::test]
    async fn stalled_hls_manifest_body_fails_with_existing_idle_timeout() {
        let (stalled_sender, stalled_receiver) = oneshot::channel();
        let stalled_sender = Arc::new(Mutex::new(Some(stalled_sender)));
        let router = Router::new().route(
            "/playlist.m3u8",
            axum::routing::get({
                let stalled_sender = stalled_sender.clone();
                move || {
                    let stalled_sender = stalled_sender.clone();
                    async move {
                        let chunks = stream::iter([Ok::<_, io::Error>(b"#EXTM3U\n".to_vec())])
                            .chain(stream::once(async move {
                                if let Ok(mut sender) = stalled_sender.lock()
                                    && let Some(sender) = sender.take()
                                {
                                    let _ = sender.send(());
                                }
                                futures_util::future::pending::<Result<Vec<u8>, io::Error>>().await
                            }));
                        let mut response = Response::new(Body::from_stream(chunks));
                        response.headers_mut().insert(
                            header::CONTENT_TYPE,
                            HeaderValue::from_static("application/vnd.apple.mpegurl"),
                        );
                        response
                    }
                }
            }),
        );
        let policy = TimeoutPolicy {
            response_start: Duration::from_millis(500),
            read_idle: Duration::from_millis(150),
        };
        let (url, relay, upstream) = start_timeout_fixture(router, "playlist.m3u8", policy).await;
        let request = tokio::spawn(async move { fixture_client().get(url).send().await });
        timeout(Duration::from_secs(2), stalled_receiver)
            .await
            .expect("manifest stream should reach its stalled chunk")
            .expect("manifest stall signal should arrive");
        let response = timeout(Duration::from_secs(2), request)
            .await
            .expect("manifest idle timeout should complete")
            .expect("manifest request task should complete")
            .expect("relay should return a gateway response");
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);

        relay.shutdown().expect("relay fixture should stop");
        upstream.abort();
    }

    #[tokio::test]
    async fn concurrent_fast_request_completes_while_another_body_is_stalled() {
        let (stalled_sender, stalled_receiver) = oneshot::channel();
        let stalled_sender = Arc::new(Mutex::new(Some(stalled_sender)));
        let router = Router::new().route(
            "/body",
            axum::routing::get(move |request: Request<Body>| {
                let stalled_sender = stalled_sender.clone();
                async move {
                    if request.headers().contains_key(header::RANGE) {
                        let chunks = stream::iter([Ok::<_, io::Error>(b"first".to_vec())]).chain(
                            stream::once(async move {
                                if let Ok(mut sender) = stalled_sender.lock()
                                    && let Some(sender) = sender.take()
                                {
                                    let _ = sender.send(());
                                }
                                futures_util::future::pending::<Result<Vec<u8>, io::Error>>().await
                            }),
                        );
                        Body::from_stream(chunks)
                    } else {
                        Body::from("fast")
                    }
                }
            }),
        );
        let policy = TimeoutPolicy {
            response_start: Duration::from_millis(500),
            read_idle: Duration::from_millis(500),
        };
        let (url, relay, upstream) = start_timeout_fixture(router, "body", policy).await;
        let stalled_url = url.clone();
        let stalled_request = tokio::spawn(async move {
            fixture_client()
                .get(stalled_url)
                .header(header::RANGE, "bytes=0-0")
                .send()
                .await
        });
        timeout(Duration::from_secs(2), stalled_receiver)
            .await
            .expect("stalled request should reach its pending chunk")
            .expect("stalled request signal should arrive");
        let fast_body = timeout(Duration::from_secs(2), async {
            fixture_client().get(url).send().await
        })
        .await
        .expect("concurrent fast request should not wait for the stalled stream")
        .expect("fast request should succeed")
        .bytes()
        .await
        .expect("fast response body should be readable");
        assert_eq!(&fast_body[..], b"fast");

        stalled_request.abort();
        relay.shutdown().expect("relay fixture should stop");
        upstream.abort();
    }

    #[tokio::test]
    async fn cancelling_downstream_body_read_releases_upstream_stream() {
        let (started_sender, started_receiver) = oneshot::channel();
        let (dropped_sender, dropped_receiver) = oneshot::channel();
        let started_sender = Arc::new(Mutex::new(Some(started_sender)));
        let dropped_sender = Arc::new(Mutex::new(Some(dropped_sender)));
        let router = Router::new().route(
            "/body",
            axum::routing::get({
                let started_sender = started_sender.clone();
                let dropped_sender = dropped_sender.clone();
                move || {
                    let started_sender = started_sender.clone();
                    let dropped_sender = dropped_sender.clone();
                    async move {
                        let drop_signal = DropSignal(
                            dropped_sender
                                .lock()
                                .ok()
                                .and_then(|mut sender| sender.take()),
                        );
                        let chunks = stream::iter([Ok::<_, io::Error>(b"first".to_vec())]).chain(
                            stream::once(async move {
                                let _drop_signal = drop_signal;
                                if let Ok(mut sender) = started_sender.lock()
                                    && let Some(sender) = sender.take()
                                {
                                    let _ = sender.send(());
                                }
                                futures_util::future::pending::<Result<Vec<u8>, io::Error>>().await
                            }),
                        );
                        Body::from_stream(chunks)
                    }
                }
            }),
        );
        let policy = TimeoutPolicy {
            response_start: Duration::from_millis(500),
            read_idle: Duration::from_secs(5),
        };
        let (url, relay, upstream) = start_timeout_fixture(router, "body", policy).await;
        let response = fixture_client()
            .get(url)
            .send()
            .await
            .expect("relay should return the first media chunk");
        let mut body = response.bytes_stream();
        assert_eq!(
            body.next()
                .await
                .expect("first body chunk should exist")
                .expect("first body chunk should be successful"),
            &b"first"[..]
        );
        let pending_read = tokio::spawn(async move { body.next().await });
        timeout(Duration::from_secs(2), started_receiver)
            .await
            .expect("upstream stream should enter its pending read")
            .expect("upstream read-start signal should arrive");
        pending_read.abort();
        timeout(Duration::from_secs(2), dropped_receiver)
            .await
            .expect("cancelling the client should release the upstream stream")
            .expect("upstream drop signal should arrive");

        relay.shutdown().expect("relay fixture should stop");
        upstream.abort();
    }

    #[tokio::test]
    async fn idle_read_timeout_rejects_a_body_that_stalls_after_its_first_chunk() {
        let router = Router::new().route(
            "/body",
            axum::routing::get(|| async {
                let chunks = stream::iter([Ok::<_, io::Error>(b"first".to_vec())]).chain(
                    stream::once(async {
                        sleep(Duration::from_secs(2)).await;
                        Ok(b"late".to_vec())
                    }),
                );
                Body::from_stream(chunks)
            }),
        );
        let policy = TimeoutPolicy {
            response_start: Duration::from_millis(500),
            read_idle: Duration::from_millis(150),
        };
        let (url, relay, upstream) = start_timeout_fixture(router, "body", policy).await;

        let response = timeout(Duration::from_secs(2), fixture_client().get(url).send())
            .await
            .expect("first body chunk should arrive")
            .expect("relay should return a response");
        assert_eq!(response.status(), StatusCode::OK);
        let body_result = timeout(Duration::from_secs(2), response.bytes())
            .await
            .expect("idle timeout should bound the body read");
        assert!(body_result.is_err(), "stalled upstream body should fail");

        relay.shutdown().expect("relay fixture should stop");
        upstream.abort();
    }

    #[tokio::test]
    async fn long_stream_completes_when_each_read_arrives_within_idle_window() {
        let router = Router::new().route(
            "/body",
            axum::routing::get(|| async {
                let chunks = stream::unfold(0_u8, |index| async move {
                    if index == 5 {
                        return None;
                    }
                    if index > 0 {
                        sleep(Duration::from_millis(75)).await;
                    }
                    Some((Ok::<_, io::Error>(vec![b'a' + index]), index + 1))
                });
                Body::from_stream(chunks)
            }),
        );
        let policy = TimeoutPolicy {
            response_start: Duration::from_millis(500),
            read_idle: Duration::from_millis(200),
        };
        let (url, relay, upstream) = start_timeout_fixture(router, "body", policy).await;

        let response = timeout(Duration::from_secs(3), async {
            fixture_client()
                .get(url)
                .send()
                .await
                .expect("relay request should complete")
                .bytes()
                .await
                .expect("long stream should complete")
        })
        .await
        .expect("total stream duration may exceed the idle window");
        assert_eq!(&response[..], b"abcde");

        relay.shutdown().expect("relay fixture should stop");
        upstream.abort();
    }

    #[test]
    fn rewrites_relative_and_absolute_hls_references_to_opaque_ids() {
        let (playlist_url, mut aliases) =
            initial_aliases("https://jellyfin.example:8920/base/hls/master.m3u8?ticket=top-secret");
        let manifest = concat!(
            "#EXTM3U\r\n",
            "#EXT-X-KEY:METHOD=AES-128,URI=\"../keys/key.bin?sig=key-secret\"\r\n",
            "video/segment-1.ts?sig=segment-secret\r\n",
            "https://jellyfin.example:8920/base/hls/segment-2.ts?sig=absolute-secret\r\n",
        );

        let rewritten =
            rewrite_manifest(manifest.as_bytes(), &playlist_url, &origin(), &mut aliases)
                .expect("same-origin HLS should be rewritten");
        let rewritten = std::str::from_utf8(&rewritten).expect("rewritten playlist is UTF-8");

        assert!(rewritten.starts_with("#EXTM3U\r\n"));
        assert!(rewritten.contains("URI=\"./"));
        assert_eq!(rewritten.matches("./").count(), 3);
        assert!(!rewritten.contains("jellyfin.example"));
        assert!(!rewritten.contains("top-secret"));
        assert!(!rewritten.contains("key-secret"));
        assert!(!rewritten.contains("segment-secret"));
        assert!(!rewritten.contains("absolute-secret"));
        assert_eq!(aliases.by_id.len(), 4);
    }

    #[test]
    fn preserves_new_non_url_hls_attributes_and_rewrites_variant_uri() {
        let (playlist_url, mut aliases) =
            initial_aliases("https://jellyfin.example:8920/base/hls/master.m3u8");
        let manifest = concat!(
            "#EXTM3U\n",
            "#EXT-X-STREAM-INF:BANDWIDTH=1000000,VIDEO-RANGE=SDR/HLG\n",
            "variants/main.m3u8\n",
        );

        let rewritten =
            rewrite_manifest(manifest.as_bytes(), &playlist_url, &origin(), &mut aliases)
                .expect("a modern non-URL variant attribute should be preserved");
        let rewritten = std::str::from_utf8(&rewritten).expect("rewritten manifest is UTF-8");

        assert!(rewritten.contains("VIDEO-RANGE=SDR/HLG"));
        assert!(!rewritten.contains("variants/main.m3u8"));
        assert_eq!(aliases.by_id.len(), 2);
    }

    #[test]
    fn accepts_live_hls_manifest_without_endlist_and_rewrites_segments() {
        let (playlist_url, mut aliases) =
            initial_aliases("https://jellyfin.example:8920/base/live/master.m3u8");
        let manifest = concat!(
            "#EXTM3U\n",
            "#EXT-X-TARGETDURATION:6\n",
            "#EXT-X-MEDIA-SEQUENCE:42\n",
            "#EXTINF:6.000,\n",
            "segment-42.ts?ticket=segment-secret\n",
        );

        let rewritten =
            rewrite_manifest(manifest.as_bytes(), &playlist_url, &origin(), &mut aliases)
                .expect("a rolling live manifest without ENDLIST should be accepted");
        let rewritten = std::str::from_utf8(&rewritten).expect("rewritten manifest is UTF-8");

        assert!(rewritten.contains("#EXT-X-TARGETDURATION:6"));
        assert!(rewritten.contains("#EXT-X-MEDIA-SEQUENCE:42"));
        assert!(!rewritten.contains("#EXT-X-ENDLIST"));
        assert!(!rewritten.contains("segment-42.ts"));
        assert!(!rewritten.contains("segment-secret"));
        assert_eq!(aliases.by_id.len(), 2);
    }

    fn rewrite(manifest: &str) -> Result<String, RelayError> {
        let (playlist_url, mut aliases) =
            initial_aliases("https://jellyfin.example:8920/base/hls/main.m3u8?ticket=top-secret");
        rewrite_manifest(manifest.as_bytes(), &playlist_url, &origin(), &mut aliases)
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
    }

    #[test]
    fn fmp4_init_segment_map_uri_is_rewritten_and_its_byte_range_is_kept() {
        let rewritten = rewrite(concat!(
            "#EXTM3U\n",
            "#EXT-X-VERSION:7\n",
            "#EXT-X-MAP:URI=\"init.mp4?sig=map-secret\",BYTERANGE=\"720@0\"\n",
            "#EXTINF:4.0,\n",
            "seg-1.m4s?sig=seg-secret\n",
        ))
        .expect("an fMP4 playlist is valid");
        assert!(rewritten.contains("#EXT-X-MAP:URI=\"./"), "{rewritten}");
        assert!(rewritten.contains("BYTERANGE=\"720@0\""), "{rewritten}");
        for secret in [
            "init.mp4",
            "map-secret",
            "seg-1.m4s",
            "seg-secret",
            "top-secret",
        ] {
            assert!(!rewritten.contains(secret), "{secret} leaked: {rewritten}");
        }
    }

    #[test]
    fn byte_range_segments_keep_their_range_lines_and_alias_the_uri() {
        let rewritten = rewrite(concat!(
            "#EXTM3U\n",
            "#EXT-X-VERSION:4\n",
            "#EXTINF:4.0,\n",
            "#EXT-X-BYTERANGE:1000@0\n",
            "media.ts?sig=range-secret\n",
            "#EXTINF:4.0,\n",
            "#EXT-X-BYTERANGE:1000\n",
            "media.ts?sig=range-secret\n",
        ))
        .expect("byte-range playlist is valid");
        assert!(
            rewritten.contains("#EXT-X-BYTERANGE:1000@0\n"),
            "{rewritten}"
        );
        assert!(rewritten.contains("#EXT-X-BYTERANGE:1000\n"), "{rewritten}");
        assert!(!rewritten.contains("range-secret"), "{rewritten}");
        assert!(!rewritten.contains("media.ts"), "{rewritten}");
    }

    #[test]
    fn query_strings_on_segments_are_not_forwarded_to_the_player() {
        let rewritten =
            rewrite("#EXTM3U\n#EXTINF:6,\nseg.ts?a=1&b=2&api_key=abc\n").expect("valid playlist");
        assert!(
            !rewritten.contains('?') && !rewritten.contains("api_key"),
            "{rewritten}"
        );
    }

    #[test]
    fn encrypted_key_uri_is_aliased_and_the_other_key_attributes_survive() {
        let rewritten = rewrite(concat!(
            "#EXTM3U\n",
            "#EXT-X-KEY:METHOD=AES-128,URI=\"key.bin?sig=key-secret\",IV=0x00112233445566778899aabbccddeeff\n",
            "#EXTINF:4,\n",
            "seg.ts\n",
        ))
        .expect("an AES-128 playlist is valid");
        assert!(rewritten.contains("METHOD=AES-128"), "{rewritten}");
        assert!(
            rewritten.contains("IV=0x00112233445566778899aabbccddeeff"),
            "{rewritten}"
        );
        assert!(!rewritten.contains("key.bin") && !rewritten.contains("key-secret"));
    }

    #[test]
    fn key_method_none_without_uri_is_accepted() {
        let rewritten = rewrite("#EXTM3U\n#EXT-X-KEY:METHOD=NONE\n#EXTINF:4,\nseg.ts\n")
            .expect("METHOD=NONE needs no URI");
        assert!(rewritten.contains("#EXT-X-KEY:METHOD=NONE"), "{rewritten}");
    }

    #[test]
    fn a_manifest_over_the_size_cap_is_refused_and_one_at_the_cap_is_not() {
        let (playlist_url, mut aliases) =
            initial_aliases("https://jellyfin.example:8920/base/hls/main.m3u8");
        let at_cap = {
            let mut text = String::from("#EXTM3U\n");
            while text.len() < MAX_MANIFEST_BYTES {
                text.push_str("#EXT-X-VERSION:3\n");
            }
            text.truncate(MAX_MANIFEST_BYTES);
            text
        };
        assert_eq!(at_cap.len(), MAX_MANIFEST_BYTES);
        let over = format!("{at_cap}#");
        let refused = rewrite_manifest(over.as_bytes(), &playlist_url, &origin(), &mut aliases);
        assert!(matches!(refused, Err(RelayError::ManifestTooLarge)));
        let at = rewrite_manifest(at_cap.as_bytes(), &playlist_url, &origin(), &mut aliases);
        assert!(
            !matches!(at, Err(RelayError::ManifestTooLarge)),
            "a manifest exactly at the cap must not be reported as too large"
        );
    }

    #[test]
    fn non_utf8_and_headerless_manifests_are_invalid() {
        let (playlist_url, mut aliases) =
            initial_aliases("https://jellyfin.example:8920/base/hls/main.m3u8");
        for bytes in [
            &b"#EXTM3U\n\xff\xfe\n"[..],
            b"",
            b"not a playlist\nseg.ts\n",
        ] {
            let result = rewrite_manifest(bytes, &playlist_url, &origin(), &mut aliases);
            assert!(
                matches!(result, Err(RelayError::InvalidManifest)),
                "{bytes:?}"
            );
        }
    }

    #[test]
    fn a_segment_on_another_origin_is_refused() {
        for url in [
            "https://other.example:8920/seg.ts",
            "https://jellyfin.example:9999/seg.ts",
            "http://jellyfin.example:8920/seg.ts",
        ] {
            assert!(
                rewrite(&format!("#EXTM3U\n#EXTINF:4,\n{url}\n")).is_err(),
                "{url} must be refused"
            );
        }
    }

    #[test]
    fn rejects_external_uri_attribute_without_mutating_aliases() {
        let (playlist_url, mut aliases) =
            initial_aliases("https://jellyfin.example:8920/base/hls/master.m3u8");
        let original_alias_count = aliases.by_id.len();
        let manifest = concat!(
            "#EXTM3U\n",
            "#EXT-X-KEY:METHOD=AES-128,URI=\"https://attacker.example/key?secret=value\"\n",
        );

        let error = rewrite_manifest(manifest.as_bytes(), &playlist_url, &origin(), &mut aliases)
            .expect_err("cross-origin key URI must be rejected");

        assert_eq!(error, RelayError::CrossOriginResource);
        assert_eq!(aliases.by_id.len(), original_alias_count);
        let diagnostic = format!("{error:?}");
        assert!(!diagnostic.contains("attacker.example"));
        assert!(!diagnostic.contains("secret=value"));
    }

    #[test]
    fn rejects_content_steering_and_unknown_url_bearing_attributes() {
        let (playlist_url, mut aliases) =
            initial_aliases("https://jellyfin.example:8920/base/hls/master.m3u8");
        let original_alias_count = aliases.by_id.len();
        let unsupported = [
            "#EXT-X-CONTENT-STEERING:SERVER-URI=\"https://jellyfin.example/steering?ticket=secret\"",
            "#EXT-X-VENDOR:PLAYLIST-URI=\"https://jellyfin.example/playlist?ticket=secret\"",
            "#EXT-X-VENDOR:SOURCE=\"../hls/playlist.m3u8?ticket=secret\"",
            "#EXT-X-KEY:METHOD=AES-128,SERVER-URI=\"segment.ts\"",
        ];

        for tag in unsupported {
            let manifest = format!("#EXTM3U\n{tag}\nsegment.ts\n");
            let error =
                rewrite_manifest(manifest.as_bytes(), &playlist_url, &origin(), &mut aliases)
                    .expect_err("unsupported URL-bearing manifest data must be rejected");
            let diagnostic = format!("{error:?}");
            assert!(!diagnostic.contains("jellyfin.example"));
            assert!(!diagnostic.contains("ticket=secret"));
        }

        assert_eq!(aliases.by_id.len(), original_alias_count);
        assert!(uri_attribute_ranges("#EXT-X-MEDIA:CHANNELS=\"6/JOC\"").is_ok());
        assert!(uri_attribute_ranges("#EXT-X-KEY:KEYFORMATVERSIONS=\"1/2\"").is_ok());
    }

    #[test]
    fn rejects_absolute_url_text_outside_rewritten_uri_fields() {
        let (playlist_url, mut aliases) =
            initial_aliases("https://jellyfin.example:8920/base/hls/master.m3u8");
        let manifest = "#EXTM3U\n#EXT-X-VENDOR:NOTE=\"https://host/key?token=private\"\n";

        let error = rewrite_manifest(manifest.as_bytes(), &playlist_url, &origin(), &mut aliases)
            .expect_err("unrewritten absolute URL text must not reach the player");

        assert_eq!(error, RelayError::UnsupportedManifestReference);
    }

    #[test]
    fn rejects_userinfo_fragments_and_non_http_resource_references() {
        let base = Url::parse("https://jellyfin.example:8920/base/master.m3u8")
            .expect("valid playlist URL");
        for reference in [
            "https://user@jellyfin.example:8920/segment.ts",
            "https://jellyfin.example:8920/segment.ts#fragment",
            "data:video/mp2t;base64,AAAA",
        ] {
            assert!(resolve_hls_reference(reference, &base, &origin()).is_err());
        }
    }

    #[test]
    fn debug_output_redacts_loopback_capability_and_upstream_url() {
        let playback_url = RelayPlaybackUrl(Zeroizing::new(
            "http://127.0.0.1:4567/r/secret-capability/secret-resource".to_owned(),
        ));
        let upstream_url = SecretUrl(Zeroizing::new(
            "https://jellyfin.example:8920/stream.m3u8?api_key=private-token".to_owned(),
        ));

        let playback_debug = format!("{playback_url:?}");
        let upstream_debug = format!("{upstream_url:?}");
        assert!(!playback_debug.contains("secret-capability"));
        assert!(!playback_debug.contains("secret-resource"));
        assert!(!upstream_debug.contains("jellyfin.example"));
        assert!(!upstream_debug.contains("private-token"));
    }

    #[test]
    fn source_headers_are_redacted_and_only_engine_allowlist_is_forwarded() {
        let authorization = "MediaBrowser Token=private-token";
        let required_header = "signed-header-value";
        let source_headers = vec![
            SecretHeader {
                name: HeaderName::from_static("authorization"),
                value: Zeroizing::new(authorization.to_owned()),
            },
            SecretHeader {
                name: HeaderName::from_static("x-jellyfin-required"),
                value: Zeroizing::new(required_header.to_owned()),
            },
        ];
        let debug = format!("{:?}", source_headers[0]);
        assert!(!debug.contains(authorization));

        let mut incoming = HeaderMap::new();
        incoming.insert(header::RANGE, HeaderValue::from_static("bytes=0-99"));
        incoming.insert(header::ACCEPT, HeaderValue::from_static("video/mp4"));
        incoming.insert(header::COOKIE, HeaderValue::from_static("engine-cookie"));
        incoming.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("engine-authorization"),
        );
        let forwarded = upstream_request_headers(&incoming, &source_headers)
            .expect("the allowlisted engine headers and source credentials are valid");

        assert_eq!(forwarded.get(header::RANGE).unwrap(), "bytes=0-99");
        assert_eq!(forwarded.get(header::ACCEPT).unwrap(), "video/mp4");
        assert_eq!(forwarded.get(header::AUTHORIZATION).unwrap(), authorization);
        assert_eq!(
            forwarded.get("x-jellyfin-required").unwrap(),
            required_header
        );
        assert!(!forwarded.contains_key(header::COOKIE));
        assert_eq!(forwarded.get(header::ACCEPT_ENCODING).unwrap(), "identity");
        let forwarded_debug = format!("{forwarded:?}");
        assert!(!forwarded_debug.contains(authorization));
        assert!(!forwarded_debug.contains(required_header));
        assert!(!forwarded_debug.contains("engine-cookie"));
        assert!(!forwarded_debug.contains("engine-authorization"));
    }

    #[test]
    fn accepts_only_an_origin_without_path_or_credentials() {
        assert!(parse_allowed_origin("https://jellyfin.example:8920/").is_ok());
        assert!(parse_allowed_origin("https://jellyfin.example:8920/base/").is_err());
        assert!(parse_allowed_origin("https://user@jellyfin.example:8920/").is_err());
        assert!(parse_allowed_origin("https://jellyfin.example:8920/?token=secret").is_err());
    }

    #[test]
    fn refuses_plain_http_for_public_hosts() {
        assert!(parse_allowed_origin("http://jellyfin.example:8096/").is_err());
        assert!(parse_allowed_origin("http://8.8.8.8:8096/").is_err());
        assert!(parse_allowed_origin("http://[2001:4860:4860::8888]:8096/").is_err());
        assert!(parse_allowed_origin("https://jellyfin.example:8920/").is_ok());
    }

    #[test]
    fn accepts_plain_http_for_loopback_private_and_link_local_hosts() {
        assert!(parse_allowed_origin("http://127.0.0.1:8096/").is_ok());
        assert!(parse_allowed_origin("http://localhost:8096/").is_ok());
        assert!(parse_allowed_origin("http://jellyfin.localhost:8096/").is_ok());
        assert!(parse_allowed_origin("http://[::1]:8096/").is_ok());
        assert!(parse_allowed_origin("http://192.168.1.20:8096/").is_ok());
        assert!(parse_allowed_origin("http://10.0.0.5:8096/").is_ok());
        assert!(parse_allowed_origin("http://169.254.10.1:8096/").is_ok());
        assert!(parse_allowed_origin("http://[fd00::20]:8096/").is_ok());
        assert!(parse_allowed_origin("http://[fe80::20]:8096/").is_ok());
    }

    #[test]
    fn owned_runtime_serves_after_caller_without_tokio_runtime_returns() {
        use std::io::{Read, Write};

        let listener = StdTcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind test listener");
        let port = listener.local_addr().expect("test listener address").port();
        let handle = spawn_server_thread_with_policy(listener, TimeoutPolicy::DEFAULT, |_| {
            Router::new().route("/ready", axum::routing::get(|| async { "relay-ready" }))
        })
        .expect("owned Tokio runtime should start");

        let mut stream = std::net::TcpStream::connect((Ipv4Addr::LOCALHOST, port))
            .expect("connect to loopback listener");
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .expect("set test timeout");
        stream
            .write_all(b"GET /ready HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .expect("write test request");
        let mut response = String::new();
        stream
            .read_to_string(&mut response)
            .expect("read loopback response");
        assert!(response.contains("relay-ready"));

        handle
            .shutdown()
            .expect("owned runtime should stop and join");
    }

    #[tokio::test]
    async fn relay_preserves_authenticated_range_requests_and_partial_content() {
        let observed_range = Arc::new(Mutex::new(None::<String>));
        let observed_authorization = Arc::new(Mutex::new(None::<String>));
        let range_slot = Arc::clone(&observed_range);
        let authorization_slot = Arc::clone(&observed_authorization);
        let upstream_listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind upstream fixture");
        let upstream_port = upstream_listener
            .local_addr()
            .expect("upstream fixture address")
            .port();
        let upstream_router = Router::new().route(
            "/media.mkv",
            any(move |headers: HeaderMap| {
                let range_slot = Arc::clone(&range_slot);
                let authorization_slot = Arc::clone(&authorization_slot);
                async move {
                    let range = headers
                        .get(header::RANGE)
                        .and_then(|value| value.to_str().ok())
                        .map(str::to_owned);
                    let authorization = headers
                        .get(header::AUTHORIZATION)
                        .and_then(|value| value.to_str().ok())
                        .map(str::to_owned);
                    if let Ok(mut observed) = range_slot.lock() {
                        *observed = range.clone();
                    }
                    if let Ok(mut observed) = authorization_slot.lock() {
                        *observed = authorization;
                    }

                    let mut response = Response::new(Body::from("0123"));
                    *response.status_mut() = if range.as_deref() == Some("bytes=0-3") {
                        StatusCode::PARTIAL_CONTENT
                    } else {
                        StatusCode::BAD_REQUEST
                    };
                    response
                        .headers_mut()
                        .insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
                    response.headers_mut().insert(
                        header::CONTENT_RANGE,
                        HeaderValue::from_static("bytes 0-3/10"),
                    );
                    response
                }
            }),
        );
        let upstream_task = tokio::spawn(async move {
            axum::serve(upstream_listener, upstream_router)
                .await
                .expect("upstream fixture should serve");
        });

        let allowed_origin = Url::parse(&format!("http://127.0.0.1:{upstream_port}/"))
            .expect("valid upstream origin");
        let upstream_url = allowed_origin
            .join("media.mkv")
            .expect("valid upstream media URL");
        let mut aliases = AliasTable::default();
        let resource_id = aliases
            .insert_initial(SecretUrl::from_url(upstream_url))
            .expect("create media alias");
        let capability = random_hex(CAPABILITY_BYTES).expect("random test capability");
        let route_capability = capability.clone();
        let source_headers = vec![SecretHeader {
            name: HeaderName::from_static("authorization"),
            value: Zeroizing::new("MediaBrowser Token=relay-test-secret".to_owned()),
        }];

        let relay_listener = StdTcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))
            .expect("bind relay listener");
        let relay_port = relay_listener
            .local_addr()
            .expect("relay listener address")
            .port();
        let relay_handle = spawn_server_thread_with_policy(
            relay_listener,
            TimeoutPolicy::DEFAULT,
            move |client| {
                let state = Arc::new(RelayState {
                    allowed_origin,
                    capability: route_capability,
                    aliases: Mutex::new(aliases),
                    source_headers,
                    client,
                    timeout_policy: TimeoutPolicy::DEFAULT,
                });
                Router::new()
                    .route("/r/{capability}/{resource_id}", any(relay_handler))
                    .with_state(state)
            },
        )
        .expect("relay server should start");

        let relay_url = format!(
            "http://127.0.0.1:{relay_port}/r/{}/{resource_id}",
            capability.as_str()
        );
        let response = HttpClient::builder()
            .no_proxy()
            .build()
            .expect("test HTTP client")
            .get(relay_url)
            .header(header::RANGE, "bytes=0-3")
            .send()
            .await
            .expect("request through relay");
        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(
            response.headers().get(header::ACCEPT_RANGES).unwrap(),
            "bytes"
        );
        assert_eq!(
            response.headers().get(header::CONTENT_RANGE).unwrap(),
            "bytes 0-3/10"
        );
        assert_eq!(&response.bytes().await.unwrap()[..], b"0123");
        assert_eq!(observed_range.lock().unwrap().as_deref(), Some("bytes=0-3"));
        assert_eq!(
            observed_authorization.lock().unwrap().as_deref(),
            Some("MediaBrowser Token=relay-test-secret")
        );

        relay_handle.shutdown().expect("relay should stop cleanly");
        upstream_task.abort();
    }

    #[test]
    fn capability_comparison_checks_equal_length_without_early_byte_exit() {
        assert!(constant_time_equal("0123456789abcdef", "0123456789abcdef"));
        assert!(!constant_time_equal("0123456789abcdef", "0123456789abcdeg"));
        assert!(!constant_time_equal("short", "longer"));
    }
}
