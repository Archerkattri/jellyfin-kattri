//! LAN-only media listener for one cast session.
//!
//! A cast receiver cannot reach the loopback relay, so this listener serves one
//! media item on a private LAN address. The capability URL stays valid for every
//! request until the session ends, because a receiver seeks and resumes with new
//! Range requests on the same URL. The bytes are fetched through the loopback
//! relay, so the upstream URL and its credentials never leave Rust. Plain HTTP is
//! accepted here only because the peer must be on the local network; the relay's
//! refusal of plain HTTP to other hosts is unchanged.

use std::{
    fmt, io,
    net::{IpAddr, SocketAddr, TcpListener as StdTcpListener},
    sync::{Arc, Mutex},
};

use axum::{
    Router,
    body::Body,
    extract::{ConnectInfo, Path, State},
    http::{
        HeaderMap, Method, Request, Response, StatusCode,
        header::{self, HeaderValue},
    },
    routing::any,
};
use futures_util::StreamExt;
use jellyfin_api::PlaybackSourceRequest;
use thiserror::Error;
use tokio::time::timeout;
use zeroize::Zeroizing;

use crate::{
    CAPABILITY_BYTES, RelayError, RelayHandle, RelayPlaybackUrl, TimeoutPolicy,
    constant_time_equal, copy_selected_headers, empty_response, random_hex,
    selected_response_headers, spawn_server_thread_with_policy, start_relay,
};

/// Refusal reasons kept for the owner of a listener. The log is bounded so a
/// hostile peer cannot grow it without limit.
const MAX_RECORDED_REFUSALS: usize = 32;

/// Reasons the listener refused a request. None of them contains a URL, a
/// token, or a peer address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CastRefusal {
    PeerNotOnLan,
    CapabilityRejected,
    UnsupportedMedia,
    UpstreamUnavailable,
}

impl fmt::Display for CastRefusal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::PeerNotOnLan => "peer address is not on a private LAN",
            Self::CapabilityRejected => "capability is unknown or its session has ended",
            Self::UnsupportedMedia => "adaptive manifests are not served by the cast listener",
            Self::UpstreamUnavailable => "upstream media could not be fetched",
        })
    }
}

/// Errors from starting or stopping the cast listener. Relay errors carry no
/// source URL, token, or transport text.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum CastListenerError {
    #[error("cast listener address is not a private LAN address")]
    NotLanAddress,
    #[error("could not bind the cast listener to its LAN address")]
    BindFailed,
    #[error(transparent)]
    Relay(#[from] RelayError),
}

/// Session-scoped capability URL handed to the cast receiver. It stays valid
/// until the session ends. Treat it as a bearer credential: `Debug` is redacted
/// and the storage is zeroized on drop.
pub struct CastMediaUrl(Zeroizing<String>);

impl CastMediaUrl {
    /// Borrow the URL while handing it to the cast session.
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl fmt::Debug for CastMediaUrl {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("CastMediaUrl([REDACTED])")
    }
}

/// A running cast media session. Dropping it requests shutdown of the listener
/// and the loopback relay; [`CastMediaListener::end`] also waits for both to stop.
pub struct CastMediaListener {
    front: RelayHandle,
    relay: RelayHandle,
    token_state: Arc<Mutex<SessionState>>,
    refusals: Arc<Mutex<Vec<CastRefusal>>>,
}

impl CastMediaListener {
    /// Stop serving and wait until the LAN listener and the loopback relay have
    /// stopped. After this returns, the capability URL no longer connects.
    pub fn end(self) -> Result<(), CastListenerError> {
        // Mark the session ended before the sockets close, so a request that
        // races the shutdown is refused by the handler, not served.
        if let Ok(mut state) = self.token_state.lock() {
            *state = SessionState::Ended;
        }
        let front = self.front.shutdown();
        let relay = self.relay.shutdown();
        front.map_err(CastListenerError::Relay)?;
        relay.map_err(CastListenerError::Relay)?;
        Ok(())
    }

    /// Return whether the session still accepts the capability URL.
    pub fn is_active(&self) -> bool {
        matches!(
            self.token_state.lock().map(|state| *state),
            Ok(SessionState::Active)
        )
    }

    /// Return the refusal reasons recorded so far, oldest first.
    pub fn refusals(&self) -> Vec<CastRefusal> {
        self.refusals
            .lock()
            .map(|log| log.clone())
            .unwrap_or_default()
    }
}

/// Return whether `address` is a private or link-local address that a cast
/// receiver on the local network can use: RFC 1918, IPv4 link-local, RFC 4193
/// unique-local, or IPv6 link-local. Loopback, public, unspecified, multicast,
/// and shared (100.64/10) addresses are not.
pub fn is_private_lan_address(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => address.is_private() || address.is_link_local(),
        IpAddr::V6(address) => {
            if let Some(mapped) = address.to_ipv4_mapped() {
                return mapped.is_private() || mapped.is_link_local();
            }
            address.is_unicast_link_local() || (address.segments()[0] & 0xfe00) == 0xfc00
        }
    }
}

/// Refuse a listener address that is not on the private LAN. Checked before any
/// socket is opened.
pub fn check_cast_bind_address(address: IpAddr) -> Result<(), CastListenerError> {
    if is_private_lan_address(address) {
        Ok(())
    } else {
        Err(CastListenerError::NotLanAddress)
    }
}

/// Start the cast media listener for one item.
///
/// `lan_address` must be a private or link-local address on the host's LAN
/// interface. The source is served through a loopback relay, so the upstream
/// URL and its credentials stay in Rust. The returned capability URL is the
/// only value that leaves this function, and it stops working when the returned
/// listener ends.
pub fn start_cast_media_listener(
    lan_address: IpAddr,
    allowed_server_origin: &str,
    source_request: PlaybackSourceRequest,
) -> Result<(CastMediaUrl, CastMediaListener), CastListenerError> {
    check_cast_bind_address(lan_address)?;
    let (relay_url, relay) = start_relay(allowed_server_origin, source_request)?;
    start_front(lan_address, None, relay_url, relay)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionState {
    Active,
    Ended,
}

struct CastFrontState {
    token: Zeroizing<String>,
    relay_url: RelayPlaybackUrl,
    /// Set only by unit tests, which serve from loopback but must exercise LAN
    /// peers. Production always passes `None`, so the socket's peer is used.
    peer_override: Option<IpAddr>,
    client: reqwest::Client,
    token_state: Arc<Mutex<SessionState>>,
    refusals: Arc<Mutex<Vec<CastRefusal>>>,
}

impl CastFrontState {
    fn session_accepts(&self, presented: &str) -> bool {
        let Ok(state) = self.token_state.lock() else {
            return false;
        };
        *state == SessionState::Active && constant_time_equal(presented, self.token.as_str())
    }
}

fn record_refusal(log: &Mutex<Vec<CastRefusal>>, reason: CastRefusal) {
    if let Ok(mut log) = log.lock() {
        if log.len() >= MAX_RECORDED_REFUSALS {
            log.remove(0);
        }
        log.push(reason);
    }
}

fn is_hls_manifest_response(headers: &HeaderMap) -> bool {
    // The relay rewrites manifest references to loopback aliases that a LAN
    // receiver cannot resolve, so this listener serves direct media only.
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.to_ascii_lowercase().contains("mpegurl"))
}

async fn cast_handler(
    State(state): State<Arc<CastFrontState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Path(presented): Path<String>,
    request: Request<Body>,
) -> Response<Body> {
    let presented = Zeroizing::new(presented);
    let peer_address = state.peer_override.unwrap_or_else(|| peer.ip());
    if !is_private_lan_address(peer_address) {
        record_refusal(&state.refusals, CastRefusal::PeerNotOnLan);
        return empty_response(StatusCode::NOT_FOUND);
    }
    if !state.session_accepts(presented.as_str()) {
        record_refusal(&state.refusals, CastRefusal::CapabilityRejected);
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

    let mut upstream_request = state
        .client
        .request(method.clone(), state.relay_url.as_str());
    for name in [header::RANGE, header::IF_RANGE] {
        if let Some(value) = request.headers().get(&name) {
            upstream_request = upstream_request.header(name, value.clone());
        }
    }
    let upstream = match timeout(
        TimeoutPolicy::DEFAULT.response_start,
        upstream_request.send(),
    )
    .await
    {
        Ok(Ok(response)) => response,
        _ => {
            record_refusal(&state.refusals, CastRefusal::UpstreamUnavailable);
            return empty_response(StatusCode::BAD_GATEWAY);
        }
    };
    if is_hls_manifest_response(upstream.headers()) {
        record_refusal(&state.refusals, CastRefusal::UnsupportedMedia);
        return empty_response(StatusCode::BAD_GATEWAY);
    }

    let status = upstream.status();
    let selected = selected_response_headers(upstream.headers());
    let mut response = empty_response(status);
    copy_selected_headers(response.headers_mut(), &selected, false);
    // HEAD and non-media statuses carry no body to forward. Returning here drops
    // the upstream response, which closes that connection.
    if method == Method::HEAD || !matches!(status, StatusCode::OK | StatusCode::PARTIAL_CONTENT) {
        return response;
    }

    let body = upstream
        .bytes_stream()
        .map(|chunk| chunk.map_err(|_| io::Error::other("upstream media stream failed")));
    *response.body_mut() = Body::from_stream(body);
    response
}

/// Bind the LAN listener on `bind_ip` and attach it to an already-started relay.
/// The listener takes ownership of the relay, so the relay stops with it.
fn start_front(
    bind_ip: IpAddr,
    peer_override: Option<IpAddr>,
    relay_url: RelayPlaybackUrl,
    relay: RelayHandle,
) -> Result<(CastMediaUrl, CastMediaListener), CastListenerError> {
    let token = random_hex(CAPABILITY_BYTES)?;
    let listener = StdTcpListener::bind(SocketAddr::new(bind_ip, 0))
        .map_err(|_| CastListenerError::BindFailed)?;
    let local = listener
        .local_addr()
        .map_err(|_| CastListenerError::BindFailed)?;
    let media_url = Zeroizing::new(format!("http://{local}/c/{}", token.as_str()));

    let token_state = Arc::new(Mutex::new(SessionState::Active));
    let refusals = Arc::new(Mutex::new(Vec::new()));
    let route_state = Arc::clone(&token_state);
    let route_refusals = Arc::clone(&refusals);
    let front = spawn_server_thread_with_policy(listener, TimeoutPolicy::DEFAULT, move |client| {
        let state = Arc::new(CastFrontState {
            token,
            relay_url,
            peer_override,
            client,
            token_state: route_state,
            refusals: route_refusals,
        });
        Router::new()
            .route("/c/{token}", any(cast_handler))
            .with_state(state)
    })?;

    Ok((
        CastMediaUrl(media_url),
        CastMediaListener {
            front,
            relay,
            token_state,
            refusals,
        },
    ))
}

#[cfg(test)]
mod tests {
    use std::{
        net::{Ipv4Addr, SocketAddr, TcpListener as StdTcpListener},
        sync::{Arc, Mutex},
    };

    use axum::{
        Router,
        body::Body,
        http::{HeaderMap, HeaderValue, Response, StatusCode, header},
        routing::any,
    };
    use reqwest::redirect::Policy;
    use url::Url;
    use zeroize::Zeroizing;

    use super::*;
    use crate::{AliasTable, RelayState, SecretHeader, SecretUrl, relay_handler};

    const PRIVATE_PEER: IpAddr = IpAddr::V4(Ipv4Addr::new(192, 168, 10, 20));
    const PUBLIC_PEER: IpAddr = IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8));
    const MEDIA_ROUTE: &str = "/Videos/item-1/stream.mkv";
    const MEDIA_SOURCE: &str =
        "/Videos/item-1/stream.mkv?static=true&api_key=UPSTREAM-SECRET-7f3a9c";
    const MANIFEST_ROUTE: &str = "/Videos/item-1/master.m3u8";
    const MEDIA: &[u8] = b"KATTRI-MEDIA-BYTES-0123456789";
    const UPSTREAM_HEADER_SECRET: &str = "MediaBrowser Token=UPSTREAM-HEADER-SECRET";

    fn media_response(headers: &HeaderMap, media: &[u8]) -> Response<Body> {
        let ranged = headers
            .get(header::RANGE)
            .and_then(|value| value.to_str().ok())
            == Some("bytes=0-3");
        let (status, bytes) = if ranged {
            (StatusCode::PARTIAL_CONTENT, media[..4].to_vec())
        } else {
            (StatusCode::OK, media.to_vec())
        };
        let mut response = Response::new(Body::from(bytes));
        *response.status_mut() = status;
        response
            .headers_mut()
            .insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("video/x-matroska"),
        );
        if ranged {
            response.headers_mut().insert(
                header::CONTENT_RANGE,
                HeaderValue::from_str(&format!("bytes 0-3/{}", media.len()))
                    .expect("valid range header"),
            );
        }
        response
    }

    fn manifest_response() -> Response<Body> {
        let mut response = Response::new(Body::from(
            "#EXTM3U\n#EXT-X-VERSION:3\n#EXTINF:1.0,\nsegment-0.ts\n#EXT-X-ENDLIST\n",
        ));
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/vnd.apple.mpegurl"),
        );
        response
    }

    /// Serve a media item and a manifest on loopback. Returns the origin, which
    /// is the only origin the relay is allowed to fetch from.
    async fn start_upstream(media: Vec<u8>) -> (Url, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind upstream fixture");
        let port = listener
            .local_addr()
            .expect("upstream fixture address")
            .port();
        let media = Arc::new(media);
        let router = Router::new()
            .route(
                MEDIA_ROUTE,
                any(move |headers: HeaderMap| {
                    let media = Arc::clone(&media);
                    async move { media_response(&headers, &media) }
                }),
            )
            .route(MANIFEST_ROUTE, any(|| async { manifest_response() }));
        let task = tokio::spawn(async move {
            axum::serve(listener, router)
                .await
                .expect("upstream fixture serves");
        });
        let origin = Url::parse(&format!("http://127.0.0.1:{port}/")).expect("upstream origin");
        (origin, task)
    }

    /// Start the loopback relay for one source, using the relay's own handler
    /// and the same upstream credentials a real session would carry.
    fn start_relay_fixture(origin: &Url, source: &str) -> (RelayPlaybackUrl, RelayHandle) {
        let upstream_url = origin.join(source).expect("valid upstream source");
        let mut aliases = AliasTable::default();
        let resource_id = aliases
            .insert_initial(SecretUrl::from_url(upstream_url))
            .expect("create media alias");
        let capability = random_hex(CAPABILITY_BYTES).expect("random relay capability");
        let relay_listener =
            StdTcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind relay fixture");
        let port = relay_listener
            .local_addr()
            .expect("relay fixture address")
            .port();
        let allowed_origin = origin.clone();
        let route_capability = capability.clone();
        let handle = spawn_server_thread_with_policy(
            relay_listener,
            TimeoutPolicy::DEFAULT,
            move |client| {
                let state = Arc::new(RelayState {
                    allowed_origin,
                    capability: route_capability,
                    aliases: Mutex::new(aliases),
                    source_headers: vec![SecretHeader {
                        name: header::AUTHORIZATION,
                        value: Zeroizing::new(UPSTREAM_HEADER_SECRET.to_owned()),
                    }],
                    client,
                    timeout_policy: TimeoutPolicy::DEFAULT,
                });
                Router::new()
                    .route("/r/{capability}/{resource_id}", any(relay_handler))
                    .with_state(state)
            },
        )
        .expect("start relay fixture");
        let url = Zeroizing::new(format!(
            "http://127.0.0.1:{port}/r/{}/{resource_id}",
            capability.as_str()
        ));
        (RelayPlaybackUrl(url), handle)
    }

    /// Start the LAN listener on loopback. `peer_override` injects the peer
    /// address that the admission check sees.
    fn start_front_fixture(
        peer_override: Option<IpAddr>,
        relay: (RelayPlaybackUrl, RelayHandle),
    ) -> (CastMediaUrl, CastMediaListener) {
        start_front(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            peer_override,
            relay.0,
            relay.1,
        )
        .expect("start cast listener fixture")
    }

    fn test_client() -> reqwest::Client {
        reqwest::Client::builder()
            .no_proxy()
            .redirect(Policy::none())
            .build()
            .expect("test HTTP client")
    }

    fn socket_address(url: &CastMediaUrl) -> SocketAddr {
        url.as_str()
            .split('/')
            .nth(2)
            .expect("authority in cast URL")
            .parse()
            .expect("socket address in cast URL")
    }

    #[tokio::test]
    async fn private_peer_is_served_repeatedly_for_the_whole_session() {
        let (origin, _upstream) = start_upstream(MEDIA.to_vec()).await;
        let relay = start_relay_fixture(&origin, MEDIA_SOURCE);
        let (url, listener) = start_front_fixture(Some(PRIVATE_PEER), relay);
        let client = test_client();

        for attempt in 0..2 {
            let full = client
                .get(url.as_str())
                .send()
                .await
                .expect("full request completes");
            assert_eq!(full.status(), StatusCode::OK, "full GET attempt {attempt}");
            assert_eq!(full.bytes().await.expect("full body").as_ref(), MEDIA);
            assert!(
                listener.is_active(),
                "session open after full GET {attempt}"
            );
        }

        for attempt in 0..2 {
            let seek = client
                .get(url.as_str())
                .header(header::RANGE, "bytes=0-3")
                .send()
                .await
                .expect("seek request completes");
            assert_eq!(
                seek.status(),
                StatusCode::PARTIAL_CONTENT,
                "Range attempt {attempt}"
            );
            assert_eq!(
                seek.bytes().await.expect("range body").as_ref(),
                &MEDIA[..4]
            );
        }

        assert!(listener.is_active());
        assert!(listener.refusals().is_empty());
        listener.end().expect("listener ends");
    }

    #[tokio::test]
    async fn public_and_loopback_peers_are_refused_and_the_session_stays_open() {
        let (origin, _upstream) = start_upstream(MEDIA.to_vec()).await;
        let client = test_client();

        let relay = start_relay_fixture(&origin, MEDIA_SOURCE);
        let (url, listener) = start_front_fixture(Some(PUBLIC_PEER), relay);
        let refused = client
            .get(url.as_str())
            .send()
            .await
            .expect("refused request completes");
        assert_eq!(refused.status(), StatusCode::NOT_FOUND);
        assert!(listener.is_active());
        assert_eq!(listener.refusals(), vec![CastRefusal::PeerNotOnLan]);
        listener.end().expect("listener ends");

        // No injection: the real peer is loopback, which is not a LAN address.
        let relay = start_relay_fixture(&origin, MEDIA_SOURCE);
        let (url, listener) = start_front_fixture(None, relay);
        let refused = client
            .get(url.as_str())
            .send()
            .await
            .expect("loopback request completes");
        assert_eq!(refused.status(), StatusCode::NOT_FOUND);
        assert!(listener.is_active());
        assert_eq!(listener.refusals(), vec![CastRefusal::PeerNotOnLan]);
        listener.end().expect("listener ends");
    }

    #[tokio::test]
    async fn wrong_token_is_refused_and_the_real_token_still_works() {
        let (origin, _upstream) = start_upstream(MEDIA.to_vec()).await;
        let relay = start_relay_fixture(&origin, MEDIA_SOURCE);
        let (url, listener) = start_front_fixture(Some(PRIVATE_PEER), relay);
        let client = test_client();

        let (base, _token) = url.as_str().rsplit_once('/').expect("token segment");
        let wrong = format!("{base}/{}", "0".repeat(64));
        let refused = client
            .get(wrong)
            .send()
            .await
            .expect("wrong-token request completes");
        assert_eq!(refused.status(), StatusCode::NOT_FOUND);
        assert!(listener.is_active());

        let served = client
            .get(url.as_str())
            .send()
            .await
            .expect("correct request completes");
        assert_eq!(served.status(), StatusCode::OK);
        assert_eq!(served.bytes().await.expect("full body").as_ref(), MEDIA);
        listener.end().expect("listener ends");
    }

    #[tokio::test]
    async fn response_carries_no_upstream_url_token_or_relay_capability() {
        let (origin, _upstream) = start_upstream(MEDIA.to_vec()).await;
        let relay = start_relay_fixture(&origin, MEDIA_SOURCE);
        let relay_capability = relay
            .0
            .as_str()
            .split('/')
            .nth(4)
            .expect("relay capability segment")
            .to_owned();
        let (url, listener) = start_front_fixture(Some(PRIVATE_PEER), relay);
        let listener_token = url
            .as_str()
            .rsplit('/')
            .next()
            .expect("listener token segment")
            .to_owned();
        let client = test_client();

        let response = client
            .get(url.as_str())
            .send()
            .await
            .expect("request completes");
        assert_eq!(response.status(), StatusCode::OK);
        let header_lines: Vec<String> = response
            .headers()
            .iter()
            .map(|(name, value)| format!("{name}: {}", value.to_str().unwrap_or("")))
            .collect();
        let body = response.bytes().await.expect("full body");
        assert_eq!(body.as_ref(), MEDIA);
        let body_text = String::from_utf8_lossy(&body).into_owned();

        let forbidden = [
            "UPSTREAM-SECRET-7f3a9c".to_owned(),
            "UPSTREAM-HEADER-SECRET".to_owned(),
            "Videos".to_owned(),
            "http://".to_owned(),
            "/r/".to_owned(),
            origin.as_str().trim_end_matches('/').to_owned(),
            relay_capability,
            listener_token,
        ];
        for needle in &forbidden {
            assert!(!body_text.contains(needle.as_str()), "body leaks {needle}");
            for line in &header_lines {
                assert!(!line.contains(needle.as_str()), "header leaks {needle}");
            }
        }
        listener.end().expect("listener ends");
    }

    #[tokio::test]
    async fn head_probe_leaves_the_session_open() {
        let (origin, _upstream) = start_upstream(MEDIA.to_vec()).await;
        let relay = start_relay_fixture(&origin, MEDIA_SOURCE);
        let (url, listener) = start_front_fixture(Some(PRIVATE_PEER), relay);
        let client = test_client();

        let probe = client
            .head(url.as_str())
            .send()
            .await
            .expect("HEAD completes");
        assert_eq!(probe.status(), StatusCode::OK);
        assert_eq!(
            probe.headers().get(header::CONTENT_LENGTH),
            Some(&HeaderValue::from_str(&MEDIA.len().to_string()).expect("length header"))
        );
        assert!(listener.is_active());

        let served = client
            .get(url.as_str())
            .send()
            .await
            .expect("GET completes");
        assert_eq!(served.bytes().await.expect("full body").as_ref(), MEDIA);
        assert!(listener.is_active());
        listener.end().expect("listener ends");
    }

    #[tokio::test]
    async fn interrupted_transfer_keeps_the_session_open_for_retry() {
        // Larger than loopback socket buffers, so the relay cannot finish
        // writing the body before the client drops the connection.
        let media = vec![7_u8; 32 * 1024 * 1024];
        let (origin, _upstream) = start_upstream(media.clone()).await;
        let relay = start_relay_fixture(&origin, MEDIA_SOURCE);
        let (url, listener) = start_front_fixture(Some(PRIVATE_PEER), relay);
        let client = test_client();

        let mut partial = client
            .get(url.as_str())
            .send()
            .await
            .expect("partial request completes");
        assert_eq!(partial.status(), StatusCode::OK);
        let first = partial.chunk().await.expect("first chunk");
        assert!(first.is_some());
        drop(partial);
        assert!(listener.is_active());

        let full = client
            .get(url.as_str())
            .send()
            .await
            .expect("retry completes")
            .bytes()
            .await
            .expect("retry body");
        assert_eq!(full.len(), media.len());
        assert!(listener.is_active());
        listener.end().expect("listener ends");
    }

    #[tokio::test]
    async fn range_request_is_forwarded_and_session_stays_open_for_a_seek_back() {
        let (origin, _upstream) = start_upstream(MEDIA.to_vec()).await;
        let relay = start_relay_fixture(&origin, MEDIA_SOURCE);
        let (url, listener) = start_front_fixture(Some(PRIVATE_PEER), relay);
        let client = test_client();

        let partial = client
            .get(url.as_str())
            .header(header::RANGE, "bytes=0-3")
            .send()
            .await
            .expect("range request completes");
        assert_eq!(partial.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(
            partial.headers().get(header::CONTENT_RANGE),
            Some(&HeaderValue::from_str(&format!("bytes 0-3/{}", MEDIA.len())).expect("range"))
        );
        assert_eq!(
            partial.bytes().await.expect("range body").as_ref(),
            &MEDIA[..4]
        );
        assert!(listener.is_active());

        let full = client
            .get(url.as_str())
            .send()
            .await
            .expect("full request after seek completes");
        assert_eq!(full.status(), StatusCode::OK);
        assert_eq!(full.bytes().await.expect("full body").as_ref(), MEDIA);
        assert!(listener.is_active());
        listener.end().expect("listener ends");
    }

    #[tokio::test]
    async fn adaptive_manifest_is_refused_and_recorded() {
        let (origin, _upstream) = start_upstream(MEDIA.to_vec()).await;
        let relay = start_relay_fixture(&origin, MANIFEST_ROUTE);
        let (url, listener) = start_front_fixture(Some(PRIVATE_PEER), relay);
        let client = test_client();

        let response = client
            .get(url.as_str())
            .send()
            .await
            .expect("manifest request completes");
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        assert!(listener.is_active());
        assert_eq!(listener.refusals(), vec![CastRefusal::UnsupportedMedia]);
        listener.end().expect("listener ends");
    }

    #[tokio::test]
    async fn ending_the_session_stops_the_listener_and_expires_the_capability() {
        let (origin, _upstream) = start_upstream(MEDIA.to_vec()).await;
        let relay = start_relay_fixture(&origin, MEDIA_SOURCE);
        let (url, listener) = start_front_fixture(Some(PRIVATE_PEER), relay);
        let client = test_client();
        let address = socket_address(&url);
        assert!(
            std::net::TcpStream::connect(address).is_ok(),
            "listener accepts before the session ends"
        );

        listener.end().expect("listener ends");

        assert!(
            client.get(url.as_str()).send().await.is_err(),
            "capability must not connect after the session ends"
        );
        assert!(
            std::net::TcpStream::connect_timeout(&address, std::time::Duration::from_secs(2))
                .is_err(),
            "listener socket must be closed after the session ends"
        );
    }
}
