//! Async, typed access to the small set of Jellyfin endpoints used by the
//! Rust client's initial connection and library flow.

mod admin;
pub mod capabilities;
mod collections;
pub mod connectivity;
mod device_profile;
mod favorites;
mod home;
pub mod item_images;
mod json_response;
mod live_tv;
mod mpv_profile;
mod playback;
mod playback_source;
mod played_state;
mod playlists;
mod quick_connect;
pub mod remote;
pub mod remote_command;
mod search;
pub mod segments;
mod series;
pub mod session_reporting;
mod syncplay;
pub mod system_time;
pub mod websocket;

pub use admin::{ServerInfo, VirtualFolder};
pub use collections::{CollectionRuntime, collection_runtime};
pub use device_profile::{
    CodecProfile, ContainerProfile, DeviceProfile, DirectPlayProfile, ProfileCondition,
    SubtitleProfile, TranscodingProfile,
};
pub use favorites::FavoriteToggle;
pub use home::{MAX_HOME_LIMIT, UserView};
pub use item_images::BackdropImageError;
pub use live_tv::{
    GuideWindow, GuideWindowError, LiveTvChannel, LiveTvChannelPage, LiveTvProgram,
    LiveTvProgramPage, MAX_GUIDE_CHANNEL_IDS, MAX_GUIDE_CHANNELS_PER_WINDOW,
    MAX_GUIDE_PROGRAMS_PER_WINDOW, MAX_GUIDE_WINDOW_SECONDS,
};
pub use mpv_profile::{
    BaselineError, BaselineSubtitleProfile, EffectiveMpvProfile, MpvEntryResult, MpvProfile,
    MpvProfileEntry, MpvProfileError, MpvSoftwareBaseline, ProfileProvenance, RuntimeCapabilities,
    SoftwareCapabilities, baseline_device_profile, device_profile_from_runtime_inventory,
    load_device_profile, load_mpv_profile,
};
pub use playback::{
    MediaSourceInfo, MediaStreamInfo, PlaybackErrorCode, PlaybackInfoRequest, PlaybackInfoResponse,
    PlaybackRequestOverrides, SensitiveString,
};
pub use playback_source::{
    PlaybackSourceError, PlaybackSourceHeader, PlaybackSourceRequest, PlaybackSourceRequestOptions,
    PlaybackSourceUrl,
};
pub use playlists::{
    MAX_PLAYLIST_IDS_PER_REQUEST, PlaylistDto, PlaylistEntry, PlaylistItemsResponse,
    PlaylistUserPermissions,
};
pub use quick_connect::{QuickConnectRequest, QuickConnectSecret, QuickConnectStatus};
pub use search::{SEARCH_ITEM_TYPES, SearchError, SearchGroup, SearchResultGroup, group_by_type};
pub use segments::{MediaSegment, MediaSegmentType, MediaSegmentsResponse};
pub use session_reporting::{
    JellyfinTicks, PlayMethod, PlaybackOrder, PlaybackProgressInfo, PlaybackStartInfo,
    PlaybackStopInfo, QueueItem, RepeatMode, TICKS_PER_SECOND,
};
pub use syncplay::{
    GroupState, MAX_GROUP_NAME_LENGTH, PlaybackCommand, SyncPlayGroup, SyncPlayPlaybackStatus,
    SyncPlayStatusReportKind,
};

use std::{
    collections::HashMap,
    fmt,
    net::IpAddr,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use reqwest::{Client as HttpClient, Method, StatusCode, header, redirect::Policy};
use serde::{Deserialize, Serialize};
use url::Url;
use zeroize::{Zeroize, Zeroizing};

const MAX_PRIMARY_IMAGE_RESPONSE_BYTES: usize = 5 * 1024 * 1024;
/// Maximum dimensions requested for a photo-grid thumbnail.
pub const PHOTO_THUMBNAIL_MAX_WIDTH: u32 = 480;
pub const PHOTO_THUMBNAIL_MAX_HEIGHT: u32 = 360;
/// Largest photo dimensions requested for the full-size viewer.
pub const PHOTO_FULL_SIZE_MAX_DIMENSION: u32 = 8_000;
const MAX_ITEMS_ERROR_RESPONSE_BYTES: usize = 16 * 1024;
/// Upper bound for `/Items?ids=...` requests used when restoring saved queues.
pub const MAX_ITEM_IDS_PER_QUERY: usize = 50;

/// Stable client metadata sent in Jellyfin's `Authorization` header.
#[derive(Clone, PartialEq, Eq)]
pub struct ClientConfig {
    base_url: Url,
    client_name: String,
    device_name: String,
    device_id: String,
    version: String,
    allow_insecure_remote_http: bool,
}

impl ClientConfig {
    /// Create a config from a server URL and stable app/device identifiers.
    /// HTTP is allowed for user-configured private-network, loopback, and
    /// `.local` servers. Public HTTP requires explicit opt-in with
    /// [`ClientConfig::with_insecure_remote_http`].
    pub fn new(
        base_url: &str,
        client_name: impl Into<String>,
        device_name: impl Into<String>,
        device_id: impl Into<String>,
        version: impl Into<String>,
    ) -> Result<Self, ApiError> {
        let base_url = Url::parse(base_url).map_err(|_| ApiError::InvalidConfiguration)?;
        if !matches!(base_url.scheme(), "http" | "https")
            || base_url.host_str().is_none()
            || !base_url.username().is_empty()
            || base_url.password().is_some()
            || base_url.query().is_some()
            || base_url.fragment().is_some()
        {
            return Err(ApiError::InvalidConfiguration);
        }

        let config = Self {
            base_url,
            client_name: client_name.into(),
            device_name: device_name.into(),
            device_id: device_id.into(),
            version: version.into(),
            allow_insecure_remote_http: false,
        };
        for value in [
            &config.client_name,
            &config.device_name,
            &config.device_id,
            &config.version,
        ] {
            if value.is_empty() || value.chars().any(char::is_control) {
                return Err(ApiError::InvalidConfiguration);
            }
        }
        Ok(config)
    }

    /// Explicitly allow sending credentials and access tokens to a public
    /// server over unencrypted HTTP. Prefer HTTPS whenever the server supports it.
    pub fn with_insecure_remote_http(mut self) -> Self {
        self.allow_insecure_remote_http = true;
        self
    }
}

impl fmt::Debug for ClientConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ClientConfig")
            .field("base_url", &self.base_url)
            .field("client_name", &self.client_name)
            .field("device_name", &self.device_name)
            .field("device_id", &self.device_id)
            .field("version", &self.version)
            .field(
                "allow_insecure_remote_http",
                &self.allow_insecure_remote_http,
            )
            .finish()
    }
}

/// A Jellyfin API client. Redirects are disabled to prevent credentials or
/// tokens from being forwarded to another origin.
#[derive(Clone)]
pub struct JellyfinClient {
    http: HttpClient,
    config: ClientConfig,
}

impl fmt::Debug for JellyfinClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("JellyfinClient")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl JellyfinClient {
    pub fn new(config: ClientConfig) -> Result<Self, ApiError> {
        if config.base_url.scheme() == "http"
            && !is_local_host(config.base_url.host_str().unwrap_or_default())
            && !config.allow_insecure_remote_http
        {
            return Err(ApiError::InsecureRemoteTransport);
        }
        let http = HttpClient::builder()
            .redirect(Policy::none())
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(60))
            .build()
            .map_err(|_| ApiError::ClientInitialization)?;
        Ok(Self { http, config })
    }

    /// Fetch public server identity and version information without a token.
    pub async fn public_system_info(&self) -> Result<PublicSystemInfo, ApiError> {
        self.send_json(
            Method::GET,
            self.endpoint(&["System", "Info", "Public"]),
            None,
            None,
        )
        .await
    }

    /// Authenticate with a Jellyfin username and password.
    ///
    /// Jellyfin's stable authentication response does not define a refresh
    /// token. A later 401 requires the user to authenticate again; a 403 is
    /// reported separately as an authorization denial.
    pub async fn authenticate(&self, username: &str, password: &str) -> Result<Session, ApiError> {
        let url = self.endpoint(&["Users", "AuthenticateByName"]);
        let credentials = Credentials { username, password };
        let body = serde_json::to_value(credentials).map_err(|_| ApiError::InvalidResponse)?;
        let result: AuthenticationResult =
            self.send_json(Method::POST, url, None, Some(body)).await?;
        if result.access_token.is_empty() || result.access_token.chars().any(char::is_control) {
            return Err(ApiError::InvalidResponse);
        }
        Ok(Session {
            access_token: result.access_token,
            user: result.user,
            server_id: result.server_id,
            session_info: result.session_info,
        })
    }

    /// Rebuild a session from an access token saved earlier in the OS credential store.
    ///
    /// The token is checked against the server (`GET /Users/Me`), so a revoked or expired token
    /// is found here and not on the first library request. A 401 is
    /// [`ApiError::ReauthenticationRequired`]; the caller deletes the saved token and shows
    /// sign-in. A token that cannot be a header value is refused without a request.
    pub async fn resume_session(&self, token: &str) -> Result<Session, ApiError> {
        if token.is_empty() || token.chars().any(char::is_control) {
            return Err(ApiError::ReauthenticationRequired);
        }
        let url = self.endpoint(&["Users", "Me"]);
        let user: UserDto = self.send_json(Method::GET, url, Some(token), None).await?;
        Ok(Session {
            access_token: token.to_owned(),
            server_id: user.server_id.clone(),
            user,
            session_info: None,
        })
    }

    /// List items in the authenticated user's library.
    pub async fn list_items(
        &self,
        session: &Session,
        query: &ItemQuery,
    ) -> Result<ItemsResponse, ApiError> {
        if let Some(ids) = query.ids.as_deref() {
            if ids.is_empty() {
                return Ok(ItemsResponse::default());
            }
            if ids.len() > MAX_ITEM_IDS_PER_QUERY {
                return Err(ApiError::InvalidResponse);
            }
            for id in ids {
                home::validate_id(id)?;
            }
        }
        let url = self.endpoint(&["Items"]);
        let mut request = self.http.request(Method::GET, url);
        request = self.add_metadata(request, Some(&session.access_token));
        if let Some(parent_id) = query.parent_id.as_deref() {
            home::validate_id(parent_id)?;
        }
        if let Some(person_ids) = query.person_ids.as_deref() {
            for person_id in person_ids {
                home::validate_id(person_id)?;
            }
        }
        request = request.query(&query.to_query_params(&session.user.id));
        let response = request.send().await.map_err(transport_error)?;
        if response.status() == StatusCode::BAD_REQUEST {
            return Err(classify_items_bad_request(response, query.filters.as_deref()).await);
        }
        decode_response(response).await
    }

    /// Fetch saved queue items in bounded batches and return them in the
    /// caller's order. IDs the server no longer has are omitted; duplicates in
    /// the requested queue remain duplicated in the result.
    pub async fn list_items_by_ids(
        &self,
        session: &Session,
        ids: &[String],
    ) -> Result<Vec<BaseItemDto>, ApiError> {
        let mut ordered = Vec::with_capacity(ids.len());
        for batch in ids.chunks(MAX_ITEM_IDS_PER_QUERY) {
            let response = self
                .list_items(
                    session,
                    &ItemQuery {
                        ids: Some(batch.to_vec()),
                        limit: u32::try_from(batch.len()).map_err(|_| ApiError::InvalidResponse)?,
                        enable_total_record_count: Some(false),
                        ..ItemQuery::default()
                    },
                )
                .await?;
            let returned: HashMap<String, BaseItemDto> = response
                .items
                .into_iter()
                .map(|item| (item.id.clone(), item))
                .collect();
            ordered.extend(batch.iter().filter_map(|id| returned.get(id).cloned()));
        }
        Ok(ordered)
    }

    /// List items similar to an item for the authenticated user.
    pub async fn list_similar(
        &self,
        session: &Session,
        item_id: &str,
        limit: u32,
    ) -> Result<ItemsResponse, ApiError> {
        home::validate_id(item_id)?;
        let url = self.endpoint(&["Items", item_id, "Similar"]);
        let limit = limit.clamp(1, home::MAX_HOME_LIMIT).to_string();
        let request = self.http.request(Method::GET, url).query(&[
            ("userId", session.user.id.as_str()),
            ("limit", limit.as_str()),
        ]);
        let request = self.add_metadata(request, Some(&session.access_token));
        let response = request.send().await.map_err(transport_error)?;
        decode_response(response).await
    }

    /// List items associated with a person by filtering the user's `/Items`.
    pub async fn list_person_items(
        &self,
        session: &Session,
        person_id: &str,
        limit: u32,
    ) -> Result<ItemsResponse, ApiError> {
        home::validate_id(person_id)?;
        self.list_items(
            session,
            &ItemQuery {
                person_ids: Some(vec![person_id.to_owned()]),
                limit: limit.clamp(1, home::MAX_HOME_LIMIT),
                ..ItemQuery::default()
            },
        )
        .await
    }

    /// Fetch one item from the authenticated user's library.
    pub async fn get_item(
        &self,
        session: &Session,
        item_id: &str,
    ) -> Result<BaseItemDto, ApiError> {
        // `endpoint` appends each value as a URL path segment, so reserved
        // characters in an ID cannot change the route structure.
        let url = self.endpoint(&["Items", item_id]);
        let request = self
            .http
            .request(Method::GET, url)
            .query(&[("userId", session.user.id.as_str())]);
        let request = self.add_metadata(request, Some(&session.access_token));
        let response = request.send().await.map_err(transport_error)?;
        decode_response(response).await
    }

    /// Fetch one item's primary artwork using the authenticated session.
    ///
    /// The image tag enables Jellyfin's cache validators. The server is asked
    /// for a bounded JPEG suitable for poster-card display, and the response
    /// body is capped even when the server omits `Content-Length`.
    pub async fn get_primary_image(
        &self,
        session: &Session,
        item_id: &str,
        image_tag: Option<&str>,
    ) -> Result<Vec<u8>, ApiError> {
        home::validate_id(item_id)?;
        self.get_primary_image_at(
            session,
            &["Items", item_id, "Images", "Primary"],
            image_tag,
            480,
            720,
        )
        .await
    }

    /// Fetch a bounded JPEG thumbnail for the native photo grid.
    pub async fn get_photo_thumbnail(
        &self,
        session: &Session,
        item_id: &str,
        image_tag: Option<&str>,
    ) -> Result<Vec<u8>, ApiError> {
        home::validate_id(item_id)?;
        self.get_primary_image_at(
            session,
            &["Items", item_id, "Images", "Primary"],
            image_tag,
            PHOTO_THUMBNAIL_MAX_WIDTH,
            PHOTO_THUMBNAIL_MAX_HEIGHT,
        )
        .await
    }

    /// Fetch the largest bounded JPEG rendition accepted by the photo viewer.
    ///
    /// The response still uses the existing 5 MiB wire cap; requested output
    /// dimensions and decoded dimensions are separately bounded.
    pub async fn get_photo_full_size_image(
        &self,
        session: &Session,
        item_id: &str,
        image_tag: Option<&str>,
    ) -> Result<Vec<u8>, ApiError> {
        home::validate_id(item_id)?;
        self.get_primary_image_at(
            session,
            &["Items", item_id, "Images", "Primary"],
            image_tag,
            PHOTO_FULL_SIZE_MAX_DIMENSION,
            PHOTO_FULL_SIZE_MAX_DIMENSION,
        )
        .await
    }

    /// Fetch a person's primary artwork with the authenticated session.
    pub async fn get_person_image(
        &self,
        session: &Session,
        name: &str,
        image_tag: Option<&str>,
    ) -> Result<Vec<u8>, ApiError> {
        home::validate_id(name)?;
        self.get_primary_image_at(
            session,
            &["Persons", name, "Images", "Primary"],
            image_tag,
            480,
            720,
        )
        .await
    }

    pub(crate) async fn get_primary_image_at(
        &self,
        session: &Session,
        path: &[&str],
        image_tag: Option<&str>,
        max_width: u32,
        max_height: u32,
    ) -> Result<Vec<u8>, ApiError> {
        if image_tag.is_some_and(|tag| {
            tag.is_empty() || tag.len() > 128 || tag.chars().any(char::is_control)
        }) {
            return Err(ApiError::InvalidResponse);
        }
        let url = self.endpoint(path);
        let max_width = max_width.to_string();
        let max_height = max_height.to_string();
        let mut query = vec![
            ("maxWidth", max_width.as_str()),
            ("maxHeight", max_height.as_str()),
            ("quality", "82"),
            ("format", "jpg"),
        ];
        if let Some(tag) = image_tag {
            query.push(("tag", tag));
        }
        let request = self
            .http
            .request(Method::GET, url)
            .query(&query)
            .timeout(Duration::from_secs(15));
        let request = self
            .add_metadata(request, Some(&session.access_token))
            .header(header::ACCEPT, "image/jpeg");
        let mut response = request.send().await.map_err(transport_error)?;
        let status = response.status();
        if status == StatusCode::UNAUTHORIZED {
            return Err(ApiError::ReauthenticationRequired);
        }
        if status == StatusCode::FORBIDDEN {
            return Err(ApiError::Forbidden);
        }
        if !status.is_success() {
            return Err(status_error(status, response.headers()));
        }
        if response
            .content_length()
            .is_some_and(|length| length > MAX_PRIMARY_IMAGE_RESPONSE_BYTES as u64)
        {
            return Err(ApiError::ImageTooLarge);
        }

        let mut body = Vec::with_capacity(
            response
                .content_length()
                .unwrap_or(0)
                .min(MAX_PRIMARY_IMAGE_RESPONSE_BYTES as u64) as usize,
        );
        while let Some(chunk) = response.chunk().await.map_err(transport_error)? {
            if body.len().saturating_add(chunk.len()) > MAX_PRIMARY_IMAGE_RESPONSE_BYTES {
                return Err(ApiError::ImageTooLarge);
            }
            body.extend_from_slice(&chunk);
        }
        if body.is_empty() {
            return Err(ApiError::InvalidResponse);
        }
        Ok(body)
    }

    /// Canonical configured address, including the deployment path, for credential scoping.
    /// The validated address has no userinfo, query or fragment. It is not a log field.
    pub fn server_base_url(&self) -> &str {
        self.config.base_url.as_str()
    }

    /// Return the configured server origin without its reverse-proxy path.
    ///
    /// This is intended for same-origin media transport validation; it never
    /// contains credentials, query parameters, or fragments.
    pub fn server_origin(&self) -> Url {
        let mut origin = self.config.base_url.clone();
        origin.set_path("/");
        origin.set_query(None);
        origin.set_fragment(None);
        origin
    }

    /// Builds `base` plus `segments` as percent-encoded path segments.
    fn endpoint(&self, segments: &[&str]) -> Url {
        let mut url = self.config.base_url.clone();
        // `new` accepts only http(s) URLs with a host, and those always take path segments,
        // so the `Err` case is unreachable. It returns the base instead of panicking.
        if let Ok(mut path) = url.path_segments_mut() {
            path.pop_if_empty();
            path.extend(segments.iter().copied());
        }
        url
    }

    async fn send_json<T: for<'de> Deserialize<'de>>(
        &self,
        method: Method,
        url: Url,
        token: Option<&str>,
        body: Option<serde_json::Value>,
    ) -> Result<T, ApiError> {
        let mut request = self.add_metadata(self.http.request(method, url), token);
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.map_err(transport_error)?;
        decode_response(response).await
    }

    /// The MediaBrowser `Authorization` value for `session`, built with this client's
    /// metadata. The download layer sends it as a request header; it is not a string
    /// the caller can print (see [`SessionAuthorization`]).
    pub fn session_authorization(&self, session: &Session) -> SessionAuthorization {
        SessionAuthorization {
            value: self.authorization_value(Some(&session.access_token)),
        }
    }

    fn authorization_value(&self, token: Option<&str>) -> Zeroizing<String> {
        let mut metadata = Zeroizing::new(format!(
            "MediaBrowser Client=\"{}\", Device=\"{}\", DeviceId=\"{}\", Version=\"{}\"",
            encode_auth_value(&self.config.client_name),
            encode_auth_value(&self.config.device_name),
            encode_auth_value(&self.config.device_id),
            encode_auth_value(&self.config.version),
        ));
        if let Some(token) = token {
            metadata.push_str(&format!(", Token=\"{}\"", encode_auth_value(token)));
        }
        metadata
    }

    fn add_metadata(
        &self,
        request: reqwest::RequestBuilder,
        token: Option<&str>,
    ) -> reqwest::RequestBuilder {
        let metadata = self.authorization_value(token);
        request
            .header(header::AUTHORIZATION, metadata.as_str())
            .header(header::ACCEPT, "application/json")
    }
}

fn is_local_host(host: &str) -> bool {
    let host = host
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
        .unwrap_or(host);
    if host.eq_ignore_ascii_case("localhost")
        || host.to_ascii_lowercase().ends_with(".localhost")
        || host.to_ascii_lowercase().ends_with(".local")
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

fn encode_auth_value(value: &str) -> String {
    // Hex digits are pushed directly: the output is infallible, so no `expect` is needed
    // and the bytes match the former `%{byte:02X}` format (uppercase, two digits).
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.~".contains(&byte) {
            encoded.push(byte as char);
        } else {
            encoded.push('%');
            encoded.push(char::from(HEX[usize::from(byte >> 4)]));
            encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
    }
    encoded
}

#[cfg(test)]
mod auth_value_tests {
    use super::encode_auth_value;

    #[test]
    fn encodes_reserved_and_multibyte_bytes_as_uppercase_hex() {
        assert_eq!(encode_auth_value("Kattri-1.0_~"), "Kattri-1.0_~");
        assert_eq!(encode_auth_value("a b/c\"d"), "a%20b%2Fc%22d");
        assert_eq!(encode_auth_value("\u{e9}"), "%C3%A9");
        assert_eq!(encode_auth_value(""), "");
    }
}

async fn decode_response<T: for<'de> Deserialize<'de>>(
    response: reqwest::Response,
) -> Result<T, ApiError> {
    let status = response.status();
    if status == StatusCode::UNAUTHORIZED {
        return Err(ApiError::ReauthenticationRequired);
    }
    if status == StatusCode::FORBIDDEN {
        return Err(ApiError::Forbidden);
    }
    if !status.is_success() {
        return Err(status_error(status, response.headers()));
    }
    json_response::decode(response).await
}

/// Build the error for a non-success status. HTTP 429 and 503 keep the
/// server's `Retry-After` wait; other statuses use [`ApiError::HttpStatus`].
fn status_error(status: StatusCode, headers: &header::HeaderMap) -> ApiError {
    if status == StatusCode::TOO_MANY_REQUESTS || status == StatusCode::SERVICE_UNAVAILABLE {
        let retry_after = headers
            .get(header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| connectivity::parse_retry_after(value, unix_time_secs()));
        return ApiError::Unavailable {
            status,
            retry_after,
        };
    }
    ApiError::HttpStatus(status)
}

/// Current Unix time in whole seconds, or zero if the clock is before the epoch.
fn unix_time_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| i64::try_from(elapsed.as_secs()).ok())
        .unwrap_or(0)
}

async fn classify_items_bad_request(
    mut response: reqwest::Response,
    requested_filters: Option<&[ItemFilter]>,
) -> ApiError {
    let status = response.status();
    if response
        .content_length()
        .is_some_and(|length| length > MAX_ITEMS_ERROR_RESPONSE_BYTES as u64)
    {
        return ApiError::HttpStatus(status);
    }

    let mut body = Vec::with_capacity(
        response
            .content_length()
            .unwrap_or(0)
            .min(MAX_ITEMS_ERROR_RESPONSE_BYTES as u64) as usize,
    );
    loop {
        match response.chunk().await {
            Ok(Some(chunk))
                if body.len().saturating_add(chunk.len()) <= MAX_ITEMS_ERROR_RESPONSE_BYTES =>
            {
                body.extend_from_slice(&chunk);
            }
            Ok(Some(_)) | Err(_) => return ApiError::HttpStatus(status),
            Ok(None) => break,
        }
    }

    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&body) else {
        return ApiError::HttpStatus(status);
    };
    let message = value.to_string().to_ascii_lowercase();
    let explicitly_rejected_filter = requested_filters
        .into_iter()
        .flatten()
        .find(|filter| {
            let name = filter.as_api_value().to_ascii_lowercase();
            message.contains("filter")
                && message.contains(&name)
                && (message.contains("unsupported")
                    || message.contains("not supported")
                    || message.contains("invalid"))
        })
        .copied();

    explicitly_rejected_filter.map_or(ApiError::HttpStatus(status), |filter| {
        ApiError::UnsupportedItemFilter(filter)
    })
}

/// Map a failed send to a typed error. Timeouts, TLS failures, and connection
/// failures get their own variants so the UI can classify them; other failures
/// stay [`ApiError::Transport`].
///
/// reqwest reports TLS handshake failures as connect-phase errors, so the TLS
/// check runs before the connect check. Only the error source text is
/// inspected, and nothing from it is copied into the returned error.
fn transport_error(error: reqwest::Error) -> ApiError {
    if error.is_timeout() {
        ApiError::Timeout
    } else if source_chain_mentions_tls(&error) {
        ApiError::Tls
    } else if error.is_connect() {
        ApiError::Unreachable
    } else {
        ApiError::Transport
    }
}

/// Keywords that identify a TLS-layer failure in the error source chain.
const TLS_SOURCE_MARKERS: [&str; 6] = [
    "tls",
    "ssl",
    "certificate",
    "handshake",
    "rustls",
    "corrupt message",
];

fn source_chain_mentions_tls(error: &reqwest::Error) -> bool {
    let mut source = std::error::Error::source(error);
    while let Some(cause) = source {
        let text = cause.to_string().to_ascii_lowercase();
        if TLS_SOURCE_MARKERS
            .iter()
            .any(|marker| text.contains(marker))
        {
            return true;
        }
        source = cause.source();
    }
    false
}

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("invalid Jellyfin client configuration")]
    InvalidConfiguration,
    #[error("could not initialize the HTTP client")]
    ClientInitialization,
    #[error("Jellyfin request failed")]
    Transport,
    #[error("could not connect to Jellyfin")]
    Unreachable,
    #[error("could not establish a verified TLS connection to Jellyfin")]
    Tls,
    #[error("Jellyfin request timed out")]
    Timeout,
    #[error("unencrypted HTTP to a public server requires explicit opt-in")]
    InsecureRemoteTransport,
    #[error("Jellyfin requires authentication again (HTTP 401)")]
    ReauthenticationRequired,
    #[error("Jellyfin denied authorization (HTTP 403)")]
    Forbidden,
    #[error("Jellyfin returned HTTP {0}")]
    HttpStatus(StatusCode),
    /// HTTP 429 or 503, with the server's `Retry-After` wait when it sent a
    /// parseable one.
    #[error("Jellyfin is busy or unavailable (HTTP {status})")]
    Unavailable {
        status: StatusCode,
        retry_after: Option<Duration>,
    },
    #[error("Jellyfin rejected the requested item filter: {0:?}")]
    UnsupportedItemFilter(ItemFilter),
    #[error("Jellyfin returned an invalid response")]
    InvalidResponse,
    #[error("Jellyfin artwork response exceeded the client size limit")]
    ImageTooLarge,
    /// A successful JSON response exceeded the bounded client input budget.
    #[error("Jellyfin JSON response exceeded the client size limit")]
    JsonResponseTooLarge,
}

/// User-facing state for a failed request. The UI maps each class to a short
/// message. Raw error text belongs in the debug log only, after redaction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorClass {
    /// Connection refused, unreachable host, or name lookup failure.
    Offline,
    /// The server did not answer in time.
    Timeout,
    /// The server answered with a 5xx status or asked the client to slow down
    /// (HTTP 429).
    ServerUnavailable,
    /// HTTP 401: the sign-in is no longer valid.
    AuthExpired,
    /// HTTP 404: the item or endpoint is not on this server.
    NotFound,
    /// HTTP 403: the user may not access this resource.
    Forbidden,
    /// Plaintext HTTP to a non-loopback host was refused before any request.
    RefusedPlaintext,
    /// The TLS handshake or certificate check failed. The server may be
    /// reachable, but the connection cannot be trusted.
    TlsFailure,
    /// Any other failure: invalid response, rejected filter, client setup, or
    /// a transport failure that is not a timeout or connection failure.
    Unexpected,
}

impl ErrorClass {
    /// Whether this failure counts toward the offline state. Two consecutive
    /// such failures set offline; any success clears it.
    pub fn counts_toward_offline(self) -> bool {
        matches!(self, Self::Offline | Self::Timeout)
    }

    /// Whether a retry with backoff may help. Authentication, permission,
    /// not-found, and configuration failures are not retried automatically.
    pub fn is_retryable(self) -> bool {
        matches!(
            self,
            Self::Offline | Self::Timeout | Self::ServerUnavailable
        )
    }
}

impl ApiError {
    /// The server's requested wait from a `Retry-After` header, when this
    /// error carries one. Pass it to the retry policy as the minimum delay.
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::Unavailable { retry_after, .. } => *retry_after,
            _ => None,
        }
    }

    /// Classify this error for user messages and retry decisions.
    pub fn classify(&self) -> ErrorClass {
        match self {
            Self::Unreachable => ErrorClass::Offline,
            Self::Timeout => ErrorClass::Timeout,
            Self::Tls => ErrorClass::TlsFailure,
            Self::InsecureRemoteTransport => ErrorClass::RefusedPlaintext,
            Self::ReauthenticationRequired => ErrorClass::AuthExpired,
            Self::Forbidden => ErrorClass::Forbidden,
            Self::HttpStatus(status) if *status == StatusCode::NOT_FOUND => ErrorClass::NotFound,
            Self::HttpStatus(status) if status.is_server_error() => ErrorClass::ServerUnavailable,
            Self::Unavailable { .. } => ErrorClass::ServerUnavailable,
            Self::InvalidConfiguration
            | Self::ClientInitialization
            | Self::Transport
            | Self::HttpStatus(_)
            | Self::UnsupportedItemFilter(_)
            | Self::InvalidResponse
            | Self::ImageTooLarge
            | Self::JsonResponseTooLarge => ErrorClass::Unexpected,
        }
    }
}

#[cfg(test)]
mod error_classification_tests {
    use std::time::Duration;

    use super::{ApiError, ErrorClass, ItemFilter, StatusCode, transport_error};

    #[test]
    fn every_variant_maps_to_one_user_class() {
        let table = [
            (ApiError::InvalidConfiguration, ErrorClass::Unexpected),
            (ApiError::ClientInitialization, ErrorClass::Unexpected),
            (ApiError::Transport, ErrorClass::Unexpected),
            (ApiError::Unreachable, ErrorClass::Offline),
            (ApiError::Timeout, ErrorClass::Timeout),
            (ApiError::Tls, ErrorClass::TlsFailure),
            (
                ApiError::InsecureRemoteTransport,
                ErrorClass::RefusedPlaintext,
            ),
            (ApiError::ReauthenticationRequired, ErrorClass::AuthExpired),
            (ApiError::Forbidden, ErrorClass::Forbidden),
            (
                ApiError::HttpStatus(StatusCode::NOT_FOUND),
                ErrorClass::NotFound,
            ),
            (
                ApiError::HttpStatus(StatusCode::INTERNAL_SERVER_ERROR),
                ErrorClass::ServerUnavailable,
            ),
            (
                ApiError::HttpStatus(StatusCode::SERVICE_UNAVAILABLE),
                ErrorClass::ServerUnavailable,
            ),
            (
                ApiError::HttpStatus(StatusCode::BAD_GATEWAY),
                ErrorClass::ServerUnavailable,
            ),
            (
                ApiError::HttpStatus(StatusCode::CONFLICT),
                ErrorClass::Unexpected,
            ),
            (
                ApiError::UnsupportedItemFilter(ItemFilter::IsUnplayed),
                ErrorClass::Unexpected,
            ),
            (ApiError::InvalidResponse, ErrorClass::Unexpected),
            (ApiError::ImageTooLarge, ErrorClass::Unexpected),
            (ApiError::JsonResponseTooLarge, ErrorClass::Unexpected),
            (
                ApiError::Unavailable {
                    status: StatusCode::SERVICE_UNAVAILABLE,
                    retry_after: None,
                },
                ErrorClass::ServerUnavailable,
            ),
            (
                ApiError::Unavailable {
                    status: StatusCode::TOO_MANY_REQUESTS,
                    retry_after: Some(Duration::from_secs(3)),
                },
                ErrorClass::ServerUnavailable,
            ),
        ];
        for (error, expected) in table {
            assert_eq!(error.classify(), expected, "{error:?}");
        }
    }

    #[test]
    fn only_offline_and_timeout_count_toward_offline() {
        assert!(ErrorClass::Offline.counts_toward_offline());
        assert!(ErrorClass::Timeout.counts_toward_offline());
        for class in [
            ErrorClass::ServerUnavailable,
            ErrorClass::AuthExpired,
            ErrorClass::NotFound,
            ErrorClass::Forbidden,
            ErrorClass::RefusedPlaintext,
            ErrorClass::TlsFailure,
            ErrorClass::Unexpected,
        ] {
            assert!(!class.counts_toward_offline(), "{class:?}");
        }
    }

    #[test]
    fn only_transient_classes_are_retryable() {
        assert!(ErrorClass::Offline.is_retryable());
        assert!(ErrorClass::Timeout.is_retryable());
        assert!(ErrorClass::ServerUnavailable.is_retryable());
        for class in [
            ErrorClass::AuthExpired,
            ErrorClass::NotFound,
            ErrorClass::Forbidden,
            ErrorClass::RefusedPlaintext,
            ErrorClass::TlsFailure,
            ErrorClass::Unexpected,
        ] {
            assert!(!class.is_retryable(), "{class:?}");
        }
    }

    #[tokio::test]
    async fn refused_connection_maps_to_unreachable() {
        // Bind to get a free port, then drop the listener so connects are refused.
        let port = {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind");
            listener.local_addr().expect("addr").port()
        };
        let error = reqwest::Client::new()
            .get(format!("http://127.0.0.1:{port}/"))
            .send()
            .await
            .expect_err("nothing listens");
        assert!(matches!(transport_error(error), ApiError::Unreachable));
    }

    #[tokio::test]
    async fn stalled_response_maps_to_timeout() {
        // The listener accepts into its backlog but never answers.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let error = reqwest::Client::builder()
            .timeout(Duration::from_millis(100))
            .build()
            .expect("client")
            .get(format!("http://127.0.0.1:{port}/"))
            .send()
            .await
            .expect_err("no response");
        drop(listener);
        assert!(matches!(transport_error(error), ApiError::Timeout));
    }
}

#[cfg(test)]
mod connection_security_tests {
    use std::error::Error as _;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::{ApiError, ClientConfig, JellyfinClient, StatusCode, transport_error};

    fn config(base_url: &str) -> Result<ClientConfig, ApiError> {
        ClientConfig::new(base_url, "Kattri test", "test device", "device-id", "1.0")
    }

    fn client_result(base_url: &str) -> Result<JellyfinClient, ApiError> {
        JellyfinClient::new(config(base_url)?)
    }

    #[test]
    fn plain_http_to_public_hosts_is_refused_before_any_request() {
        for url in [
            "http://jellyfin.example.com",
            "http://jellyfin.example.com:8096/jellyfin",
            "http://203.0.113.10:8096",
            "http://8.8.8.8",
            "http://0.0.0.0:8096",
            "http://[2001:db8::1]:8096",
        ] {
            let result = client_result(url);
            assert!(
                matches!(result, Err(ApiError::InsecureRemoteTransport)),
                "{url} should be refused, got {result:?}"
            );
        }
    }

    #[test]
    fn https_to_any_host_is_accepted() {
        for url in [
            "https://jellyfin.example.com",
            "https://203.0.113.10:8920",
            "https://127.0.0.1:8920",
            "https://[2001:db8::1]:8920/jellyfin",
        ] {
            assert!(client_result(url).is_ok(), "{url} should be accepted");
        }
    }

    #[test]
    fn plain_http_to_loopback_private_and_local_names_is_accepted() {
        for url in [
            "http://127.0.0.1:8096",
            "http://127.1.2.3:8096",
            "http://localhost:8096",
            "http://LOCALHOST:8096",
            "http://media.localhost:8096",
            "http://[::1]:8096",
            "http://10.0.0.10:8096",
            "http://172.16.4.2:8096",
            "http://192.168.1.20:8096",
            "http://169.254.10.1:8096",
            "http://[fd00::5]:8096",
            "http://[fe80::1]:8096",
            "http://jellyfin.local:8096",
        ] {
            assert!(client_result(url).is_ok(), "{url} should be accepted");
        }
    }

    #[test]
    fn explicit_opt_in_allows_plain_http_to_a_public_host() {
        let config = config("http://203.0.113.10:8096")
            .expect("valid config")
            .with_insecure_remote_http();
        assert!(JellyfinClient::new(config).is_ok());
    }

    #[test]
    fn credentials_query_and_fragment_in_the_base_url_are_refused() {
        for url in [
            "http://user:secret@10.0.0.10:8096",
            "https://user@jellyfin.example.com",
            "https://jellyfin.example.com/?api_key=x",
            "https://jellyfin.example.com/#frag",
            "ftp://10.0.0.10/",
        ] {
            assert!(
                matches!(config(url), Err(ApiError::InvalidConfiguration)),
                "{url} should be refused"
            );
        }
    }

    #[tokio::test]
    async fn plain_http_server_behind_an_https_url_is_reported_as_tls() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let server = tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                // Consume the TLS ClientHello, then answer with plain HTTP.
                let mut record_header = [0_u8; 5];
                if socket.read_exact(&mut record_header).await.is_ok() {
                    let length =
                        usize::from(u16::from_be_bytes([record_header[3], record_header[4]]));
                    let mut hello = vec![0_u8; length];
                    let _ = socket.read_exact(&mut hello).await;
                }
                let _ = socket
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                    .await;
                let _ = socket.shutdown().await;
            }
        });
        let client = client_result(&format!("https://127.0.0.1:{port}")).expect("https accepted");
        let result = client
            .http
            .get(client.endpoint(&["System", "Info", "Public"]))
            .send()
            .await;
        server.await.expect("server task");
        let error = result.expect_err("plain HTTP is not a TLS response");
        let mut detail = error.to_string();
        let mut source = error.source();
        while let Some(cause) = source {
            detail.push_str(" <- ");
            detail.push_str(&cause.to_string());
            source = cause.source();
        }
        let classified = transport_error(error);
        assert!(
            matches!(classified, ApiError::Tls),
            "{classified:?}: {detail}"
        );
    }

    #[tokio::test]
    async fn refused_https_connection_stays_unreachable_not_tls() {
        let port = {
            let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
            listener.local_addr().expect("addr").port()
        };
        let client = client_result(&format!("https://127.0.0.1:{port}")).expect("https accepted");
        let result = client.public_system_info().await;
        assert!(matches!(result, Err(ApiError::Unreachable)), "{result:?}");
    }

    #[tokio::test]
    async fn authentication_redirect_is_not_followed_to_another_origin() {
        use std::time::Duration;

        let redirect_listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind redirect");
        let redirect_port = redirect_listener
            .local_addr()
            .expect("redirect addr")
            .port();
        let target_listener = TcpListener::bind("127.0.0.1:0").await.expect("bind target");
        let target_port = target_listener.local_addr().expect("target addr").port();
        let redirect_server = tokio::spawn(async move {
            if let Ok((mut socket, _)) = redirect_listener.accept().await {
                let mut request = [0_u8; 4096];
                let _ = socket.read(&mut request).await;
                let response = format!(
                    "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:{target_port}/collect\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
            }
        });
        let client = client_result(&format!("http://127.0.0.1:{redirect_port}"))
            .expect("loopback URL is permitted");

        let result = client.authenticate("demo", "password").await;

        assert!(matches!(
            result,
            Err(ApiError::HttpStatus(StatusCode::FOUND))
        ));
        assert!(
            tokio::time::timeout(Duration::from_millis(100), target_listener.accept())
                .await
                .is_err()
        );
        redirect_server.await.expect("redirect server task");
    }
}

#[cfg(test)]
mod retry_after_http_tests {
    use std::time::Duration;

    use reqwest::StatusCode;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::{ApiError, ClientConfig, ErrorClass, JellyfinClient};

    /// Serve one canned response to the next request and return the client's result.
    async fn respond_once(status_line: &str, extra_headers: &str) -> Result<(), ApiError> {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let response = format!(
            "HTTP/1.1 {status_line}\r\n{extra_headers}Content-Length: 0\r\nConnection: close\r\n\r\n"
        );
        let server = tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                let mut request = [0_u8; 4096];
                let _ = socket.read(&mut request).await;
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
            }
        });
        let config = ClientConfig::new(
            &format!("http://127.0.0.1:{port}"),
            "Kattri test",
            "test device",
            "device-id",
            "1.0",
        )
        .expect("valid config");
        let client = JellyfinClient::new(config).expect("loopback HTTP is allowed");
        let result = client.public_system_info().await.map(|_| ());
        server.await.expect("server task");
        result
    }

    #[tokio::test]
    async fn service_unavailable_keeps_delta_seconds_retry_after() {
        let error = respond_once("503 Service Unavailable", "Retry-After: 7\r\n")
            .await
            .expect_err("503 is an error");
        assert!(
            matches!(
                error,
                ApiError::Unavailable {
                    status: StatusCode::SERVICE_UNAVAILABLE,
                    retry_after: Some(wait),
                } if wait == Duration::from_secs(7)
            ),
            "{error:?}"
        );
        assert_eq!(error.retry_after(), Some(Duration::from_secs(7)));
        assert_eq!(error.classify(), ErrorClass::ServerUnavailable);
    }

    #[tokio::test]
    async fn too_many_requests_keeps_http_date_retry_after() {
        // A date well before the test runs yields a zero wait, which is still a parsed value.
        let error = respond_once(
            "429 Too Many Requests",
            "Retry-After: Sun, 06 Nov 1994 08:49:37 GMT\r\n",
        )
        .await
        .expect_err("429 is an error");
        assert!(
            matches!(
                error,
                ApiError::Unavailable {
                    status: StatusCode::TOO_MANY_REQUESTS,
                    retry_after: Some(Duration::ZERO),
                }
            ),
            "{error:?}"
        );
        assert!(error.classify().is_retryable());
    }

    #[tokio::test]
    async fn unparseable_retry_after_is_absent_not_an_error() {
        let error = respond_once("503 Service Unavailable", "Retry-After: soon\r\n")
            .await
            .expect_err("503 is an error");
        assert!(
            matches!(
                error,
                ApiError::Unavailable {
                    retry_after: None,
                    ..
                }
            ),
            "{error:?}"
        );
        assert_eq!(error.retry_after(), None);
    }

    #[tokio::test]
    async fn other_server_errors_stay_plain_http_status() {
        let error = respond_once("502 Bad Gateway", "Retry-After: 5\r\n")
            .await
            .expect_err("502 is an error");
        assert!(
            matches!(error, ApiError::HttpStatus(StatusCode::BAD_GATEWAY)),
            "{error:?}"
        );
        assert_eq!(error.retry_after(), None);
        assert_eq!(error.classify(), ErrorClass::ServerUnavailable);
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct AuthenticationResult {
    access_token: String,
    user: UserDto,
    #[serde(default)]
    server_id: Option<String>,
    #[serde(default)]
    session_info: Option<SessionInfoDto>,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct Credentials<'a> {
    username: &'a str,
    #[serde(rename = "Pw")]
    password: &'a str,
}

/// Authenticated state. Its token is intentionally private and omitted from
/// formatting; pass the session back to [`JellyfinClient::list_items`].
pub struct Session {
    access_token: String,
    pub user: UserDto,
    pub server_id: Option<String>,
    pub session_info: Option<SessionInfoDto>,
}

impl Clone for Session {
    fn clone(&self) -> Self {
        Self {
            access_token: self.access_token.clone(),
            user: self.user.clone(),
            server_id: self.server_id.clone(),
            session_info: self.session_info.clone(),
        }
    }
}

impl fmt::Debug for Session {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Session")
            .field("access_token", &"[REDACTED]")
            .field("user", &self.user)
            .field("server_id", &self.server_id)
            .field("session_info", &self.session_info)
            .finish()
    }
}

impl Session {
    /// A zeroizing copy of the access token, for the OS credential store only. Never put it in
    /// a log, a settings file, or a URL.
    pub fn token_for_credential_store(&self) -> Zeroizing<String> {
        Zeroizing::new(self.access_token.clone())
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.access_token.zeroize();
    }
}

/// The MediaBrowser `Authorization` value for one signed-in session. It carries the
/// access token, so it has no `Display`, a redacted `Debug`, and zeroizes on drop.
/// Pass it to the download layer as a request header; do not format or log it.
pub struct SessionAuthorization {
    value: Zeroizing<String>,
}

impl SessionAuthorization {
    /// The header value. Use only as a request header value, never in a URL or a record.
    pub fn header_value(&self) -> &str {
        self.value.as_str()
    }
}

impl fmt::Debug for SessionAuthorization {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SessionAuthorization([REDACTED])")
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "PascalCase")]
pub struct UserDto {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub server_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "PascalCase")]
pub struct SessionInfoDto {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub user_id: Option<String>,
    #[serde(default)]
    pub device_id: Option<String>,
    #[serde(default)]
    pub client: Option<String>,
    #[serde(default)]
    pub device_name: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "PascalCase")]
pub struct PublicSystemInfo {
    #[serde(default)]
    pub server_name: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub product_name: Option<String>,
    #[serde(default)]
    pub id: Option<String>,
}

/// Pagination and optional type filtering for the user's `/Items` endpoint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ItemQuery {
    pub recursive: bool,
    pub start_index: u32,
    pub limit: u32,
    pub parent_id: Option<String>,
    pub include_item_types: Option<Vec<String>>,
    pub search_term: Option<String>,
    pub sort_by: Option<Vec<ItemSortBy>>,
    pub sort_order: Option<Vec<ItemSortOrder>>,
    pub filters: Option<Vec<ItemFilter>>,
    pub genres: Option<Vec<String>>,
    pub years: Option<Vec<i32>>,
    pub person_ids: Option<Vec<String>>,
    /// Specific item IDs. Jellyfin documents this query as a comma-delimited
    /// list; the typed client caps a single request at 50 IDs.
    pub ids: Option<Vec<String>>,
    pub enable_total_record_count: Option<bool>,
}

/// Jellyfin's `/Items` `sortBy` values used by the library browser.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ItemSortBy {
    SortName,
    DateCreated,
    PremiereDate,
    CommunityRating,
}

impl ItemSortBy {
    const fn as_api_value(self) -> &'static str {
        match self {
            Self::SortName => "SortName",
            Self::DateCreated => "DateCreated",
            Self::PremiereDate => "PremiereDate",
            Self::CommunityRating => "CommunityRating",
        }
    }
}

/// Jellyfin's `/Items` `sortOrder` values.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ItemSortOrder {
    Ascending,
    Descending,
}

impl ItemSortOrder {
    const fn as_api_value(self) -> &'static str {
        match self {
            Self::Ascending => "Ascending",
            Self::Descending => "Descending",
        }
    }
}

/// Jellyfin's `/Items` `filters` values confirmed by the stable OpenAPI enum.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ItemFilter {
    IsFolder,
    IsNotFolder,
    IsPlayed,
    IsUnplayed,
    IsResumable,
    Likes,
    Dislikes,
    IsFavorite,
    IsFavoriteOrLikes,
}

impl ItemFilter {
    const fn as_api_value(self) -> &'static str {
        match self {
            Self::IsFolder => "IsFolder",
            Self::IsNotFolder => "IsNotFolder",
            Self::IsPlayed => "IsPlayed",
            Self::IsUnplayed => "IsUnplayed",
            Self::IsResumable => "IsResumable",
            Self::Likes => "Likes",
            Self::Dislikes => "Dislikes",
            Self::IsFavorite => "IsFavorite",
            Self::IsFavoriteOrLikes => "IsFavoriteOrLikes",
        }
    }
}

impl ItemQuery {
    fn to_query_params(&self, user_id: &str) -> Vec<(String, String)> {
        let mut params = vec![
            ("userId".into(), user_id.into()),
            ("recursive".into(), self.recursive.to_string()),
            ("startIndex".into(), self.start_index.to_string()),
            ("limit".into(), self.limit.to_string()),
        ];
        if let Some(parent_id) = &self.parent_id {
            params.push(("parentId".into(), parent_id.clone()));
        }
        if let Some(types) = &self.include_item_types {
            params.push(("includeItemTypes".into(), types.join(",")));
        }
        if let Some(search_term) = &self.search_term {
            params.push(("searchTerm".into(), search_term.clone()));
        }
        if let Some(sort_by) = &self.sort_by {
            params.extend(
                sort_by
                    .iter()
                    .map(|value| ("sortBy".into(), value.as_api_value().into())),
            );
        }
        if let Some(sort_order) = &self.sort_order {
            params.extend(
                sort_order
                    .iter()
                    .map(|value| ("sortOrder".into(), value.as_api_value().into())),
            );
        }
        if let Some(filters) = &self.filters {
            params.extend(
                filters
                    .iter()
                    .map(|value| ("filters".into(), value.as_api_value().into())),
            );
        }
        if let Some(genres) = &self.genres {
            params.extend(genres.iter().map(|value| ("genres".into(), value.clone())));
        }
        if let Some(years) = &self.years {
            params.extend(
                years
                    .iter()
                    .map(|value| ("years".into(), value.to_string())),
            );
        }
        if let Some(person_ids) = &self.person_ids {
            params.extend(
                person_ids
                    .iter()
                    .map(|value| ("personIds".into(), value.clone())),
            );
        }
        if let Some(ids) = &self.ids
            && !ids.is_empty()
        {
            params.push(("ids".into(), ids.join(",")));
        }
        if let Some(enable_total_record_count) = self.enable_total_record_count {
            params.push((
                "enableTotalRecordCount".into(),
                enable_total_record_count.to_string(),
            ));
        }
        params
    }
}

impl Default for ItemQuery {
    fn default() -> Self {
        Self {
            recursive: true,
            start_index: 0,
            limit: 100,
            parent_id: None,
            include_item_types: None,
            search_term: None,
            sort_by: None,
            sort_order: None,
            filters: None,
            genres: None,
            years: None,
            person_ids: None,
            ids: None,
            enable_total_record_count: None,
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "PascalCase")]
pub struct ItemsResponse {
    #[serde(default)]
    pub items: Vec<BaseItemDto>,
    #[serde(default)]
    pub total_record_count: Option<u32>,
    #[serde(default)]
    pub start_index: u32,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "PascalCase")]
pub struct BaseItemDto {
    pub id: String,
    pub name: String,
    #[serde(rename = "Type", default)]
    pub item_type: Option<String>,
    #[serde(default)]
    pub overview: Option<String>,
    #[serde(default)]
    pub production_year: Option<i32>,
    /// Parental/age rating supplied by the server (for example, `PG-13`).
    #[serde(default)]
    pub official_rating: Option<String>,
    #[serde(default)]
    pub run_time_ticks: Option<i64>,
    #[serde(default)]
    pub premiere_date: Option<String>,
    /// Series name for episodes (`SeriesName`); absent for other item types.
    #[serde(default)]
    pub series_name: Option<String>,
    /// Season number for episodes (`ParentIndexNumber`).
    #[serde(default)]
    pub parent_index_number: Option<i32>,
    /// Episode number within its season (`IndexNumber`).
    #[serde(default)]
    pub index_number: Option<i32>,
    #[serde(default)]
    pub series_id: Option<String>,
    #[serde(default)]
    pub genres: Option<Vec<String>>,
    #[serde(default)]
    pub people: Option<Vec<BaseItemPerson>>,
    #[serde(default)]
    pub primary_image_aspect_ratio: Option<f64>,
    #[serde(default)]
    pub image_tags: Option<HashMap<String, Option<String>>>,
    #[serde(default)]
    pub backdrop_image_tags: Option<Vec<String>>,
    #[serde(default)]
    pub user_data: Option<UserItemDataDto>,
}

/// Person credit attached to a Jellyfin item.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "PascalCase")]
pub struct BaseItemPerson {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(rename = "Type", default)]
    pub person_type: Option<String>,
    #[serde(default)]
    pub primary_image_tag: Option<String>,
}

/// Per-user playback and rating state returned with a library item.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "PascalCase")]
pub struct UserItemDataDto {
    #[serde(default)]
    pub playback_position_ticks: Option<i64>,
    /// Watched percentage reported by Jellyfin, in the range `0.0..=100.0`.
    #[serde(default)]
    pub played_percentage: Option<f64>,
    #[serde(default)]
    pub play_count: Option<i32>,
    #[serde(default)]
    pub is_favorite: Option<bool>,
    #[serde(default)]
    pub played: Option<bool>,
    #[serde(default)]
    pub last_played_date: Option<String>,
}

#[cfg(test)]
mod base_item_dto_tests {
    use super::BaseItemDto;

    #[test]
    fn deserializes_official_rating_from_base_item_fixture() {
        let item: BaseItemDto =
            serde_json::from_str(r#"{"Id":"item-1","Name":"Example","OfficialRating":"PG-13"}"#)
                .expect("valid BaseItemDto fixture");

        assert_eq!(item.official_rating.as_deref(), Some("PG-13"));
    }

    #[test]
    fn defaults_missing_official_rating_to_none() {
        let item: BaseItemDto = serde_json::from_str(r#"{"Id":"item-1","Name":"Example"}"#)
            .expect("valid BaseItemDto fixture without optional rating");

        assert_eq!(item.official_rating, None);
    }

    #[test]
    fn parses_people_credits_and_ignores_unknown_credit_fields() {
        let item: BaseItemDto = serde_json::from_str(
            r#"{"Id":"item-1","Name":"Example","People":[{"Id":"person-1","Name":"Ada","Role":"Lead","Type":"Actor","PrimaryImageTag":"tag-1","FutureField":true}]}"#,
        )
        .expect("valid people fixture");
        let people = item.people.expect("people present");
        assert_eq!(people.len(), 1);
        assert_eq!(people[0].id.as_deref(), Some("person-1"));
        assert_eq!(people[0].name.as_deref(), Some("Ada"));
        assert_eq!(people[0].role.as_deref(), Some("Lead"));
        assert_eq!(people[0].person_type.as_deref(), Some("Actor"));
        assert_eq!(people[0].primary_image_tag.as_deref(), Some("tag-1"));
    }

    #[test]
    fn missing_or_null_people_are_none() {
        for json in [
            r#"{"Id":"item-1","Name":"Example"}"#,
            r#"{"Id":"item-1","Name":"Example","People":null}"#,
        ] {
            let item: BaseItemDto = serde_json::from_str(json).expect("valid sparse fixture");
            assert_eq!(item.people, None);
        }
    }
}

#[cfg(test)]
mod primary_image_tests {
    use super::{
        ApiError, ClientConfig, ItemFilter, ItemQuery, JellyfinClient, Session, StatusCode, UserDto,
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    #[derive(Clone, Default)]
    struct CapturedEvents(std::sync::Arc<std::sync::Mutex<Vec<String>>>);

    impl tracing::Subscriber for CapturedEvents {
        fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
            true
        }

        fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }

        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}

        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}

        fn event(&self, event: &tracing::Event<'_>) {
            struct EventVisitor(String);

            impl tracing::field::Visit for EventVisitor {
                fn record_debug(
                    &mut self,
                    field: &tracing::field::Field,
                    value: &dyn std::fmt::Debug,
                ) {
                    use std::fmt::Write as _;
                    let _ = write!(&mut self.0, "{}={value:?};", field.name());
                }
            }

            let mut visitor = EventVisitor(String::new());
            event.record(&mut visitor);
            if let Ok(mut events) = self.0.lock() {
                events.push(visitor.0);
            }
        }

        fn enter(&self, _: &tracing::span::Id) {}

        fn exit(&self, _: &tracing::span::Id) {}
    }

    fn session() -> Session {
        Session {
            access_token: "secret-token".into(),
            user: UserDto {
                id: "user-id".into(),
                name: "Test user".into(),
                server_id: None,
            },
            server_id: None,
            session_info: None,
        }
    }

    async fn read_request(socket: &mut TcpStream) -> String {
        let mut request = Vec::new();
        let mut chunk = [0_u8; 2048];
        loop {
            let count = socket.read(&mut chunk).await.expect("read request");
            assert_ne!(count, 0, "client closed before sending headers");
            request.extend_from_slice(&chunk[..count]);
            if request.windows(4).any(|window| window == b"\r\n\r\n") {
                return String::from_utf8_lossy(&request).into_owned();
            }
        }
    }

    fn client_for(address: std::net::SocketAddr) -> JellyfinClient {
        let config = ClientConfig::new(
            &format!("http://{address}/jellyfin"),
            "Kattri test",
            "test device",
            "device-id",
            "test-version",
        )
        .expect("valid local server config");
        JellyfinClient::new(config).expect("local HTTP server is allowed")
    }

    async fn items_error_response(body: Vec<u8>) -> Result<(), ApiError> {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock server");
        let address = listener.local_addr().expect("mock server address");
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept client");
            let _request = read_request(&mut socket).await;
            let headers = format!(
                "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            socket
                .write_all(headers.as_bytes())
                .await
                .expect("write response headers");
            socket.write_all(&body).await.expect("write response body");
        });

        let query = ItemQuery {
            filters: Some(vec![ItemFilter::IsUnplayed]),
            ..ItemQuery::default()
        };
        let result = client_for(address).list_items(&session(), &query).await;
        server.await.expect("mock server task");
        result.map(|_| ())
    }

    #[tokio::test]
    async fn search_term_is_query_encoded_types_are_preserved_and_term_is_not_logged() {
        let search_term = "A&B #C+100% café 東京";
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock server");
        let address = listener.local_addr().expect("mock server address");
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept client");
            let request = read_request(&mut socket).await;
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 12\r\nConnection: close\r\n\r\n{\"Items\":[]}",
                )
                .await
                .expect("write response");
            request
        });

        let events = CapturedEvents::default();
        let dispatch = tracing::Dispatch::new(events.clone());
        let _subscriber = tracing::dispatcher::set_default(&dispatch);
        let query = ItemQuery {
            search_term: Some(search_term.into()),
            include_item_types: Some(vec!["Movie".into(), "Series".into()]),
            ..ItemQuery::default()
        };
        let response = client_for(address)
            .list_items(&session(), &query)
            .await
            .expect("list items");
        let request = server.await.expect("mock server task");
        let request_line = request.lines().next().expect("HTTP request line");
        let uri = request_line.split_whitespace().nth(1).expect("request URI");
        let (_, encoded_query) = uri.split_once('?').expect("query string");
        let decoded: std::collections::HashMap<_, _> =
            url::form_urlencoded::parse(encoded_query.as_bytes()).collect();

        assert!(response.items.is_empty());
        assert_eq!(
            decoded.get("searchTerm").map(|value| value.as_ref()),
            Some(search_term)
        );
        assert_eq!(
            decoded.get("includeItemTypes").map(|value| value.as_ref()),
            Some("Movie,Series")
        );
        assert!(uri.contains("searchTerm=A%26B+%23C%2B100%25+caf%C3%A9+%E6%9D%B1%E4%BA%AC"));
        assert!(!uri.contains("A&B"));
        assert!(!uri.contains("#C"));
        assert!(!uri.contains("+100%"));
        let captured = events.0.lock().expect("captured events lock");
        assert!(captured.iter().all(|event| !event.contains(search_term)));
    }

    #[tokio::test]
    async fn identifies_only_explicitly_rejected_requested_item_filter() {
        let result = items_error_response(
            br#"{"Message":"The item filter 'IsUnplayed' is not supported."}"#.to_vec(),
        )
        .await;

        assert!(matches!(
            result,
            Err(ApiError::UnsupportedItemFilter(ItemFilter::IsUnplayed))
        ));
    }

    #[tokio::test]
    async fn leaves_generic_or_unrelated_bad_request_errors_unchanged() {
        for body in [
            br#"{"Message":"The request is invalid."}"#.to_vec(),
            br#"{"Message":"Sorting by IsUnplayed is not supported."}"#.to_vec(),
            br#"{"Message":"The item filter 'IsPlayed' is unsupported."}"#.to_vec(),
            b"not json".to_vec(),
        ] {
            assert!(matches!(
                items_error_response(body).await,
                Err(ApiError::HttpStatus(StatusCode::BAD_REQUEST))
            ));
        }
    }

    #[tokio::test]
    async fn does_not_parse_oversized_items_error_bodies() {
        let body = vec![b' '; super::MAX_ITEMS_ERROR_RESPONSE_BYTES + 1];
        assert!(matches!(
            items_error_response(body).await,
            Err(ApiError::HttpStatus(StatusCode::BAD_REQUEST))
        ));
    }

    #[tokio::test]
    async fn requests_tagged_jpeg_with_auth_header_and_preserves_reverse_proxy_path() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock server");
        let address = listener.local_addr().expect("mock server address");
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept client");
            let request = read_request(&mut socket).await;
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: image/jpeg\r\nContent-Length: 4\r\nConnection: close\r\n\r\nJPEG",
                )
                .await
                .expect("write image response");
            request
        });

        let client = client_for(address);
        let image = client
            .get_primary_image(&session(), "item/id", Some("cache/tag&v=2"))
            .await
            .expect("fetch image bytes");
        let request = server.await.expect("mock server task");
        let request_line = request.lines().next().expect("HTTP request line");

        assert_eq!(image, b"JPEG");
        assert!(request_line.starts_with("GET /jellyfin/Items/item%2Fid/Images/Primary?"));
        assert!(request_line.contains("maxWidth=480"));
        assert!(request_line.contains("maxHeight=720"));
        assert!(request_line.contains("format=jpg"));
        assert!(request_line.contains("tag=cache%2Ftag%26v%3D2"));
        assert!(!request_line.contains("secret-token"));
        assert!(request.to_ascii_lowercase().contains("accept: image/jpeg"));
        assert!(request.to_ascii_lowercase().contains(
            "authorization: mediabrowser client=\"kattri%20test\", device=\"test%20device\", deviceid=\"device-id\", version=\"test-version\", token=\"secret-token\""
        ));
    }

    #[tokio::test]
    async fn similar_items_uses_authenticated_route_and_bounded_limit() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock server");
        let address = listener.local_addr().expect("mock server address");
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept client");
            let request = read_request(&mut socket).await;
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 12\r\nConnection: close\r\n\r\n{\"Items\":[]}",
                )
                .await
                .expect("write response");
            request
        });

        let response = client_for(address)
            .list_similar(&session(), "item/id", 1000)
            .await
            .expect("list similar items");
        let request = server.await.expect("mock server task");
        let request_line = request.lines().next().expect("HTTP request line");
        let uri = request_line.split_whitespace().nth(1).expect("request URI");

        assert!(response.items.is_empty());
        assert!(uri.starts_with("/jellyfin/Items/item%2Fid/Similar?"));
        assert!(uri.contains("userId=user-id"));
        assert!(uri.contains("limit=100"));
        assert!(!uri.contains("secret-token"));
    }

    #[tokio::test]
    async fn similar_items_preserves_not_found_and_server_errors() {
        for status in [StatusCode::NOT_FOUND, StatusCode::INTERNAL_SERVER_ERROR] {
            let listener = TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind mock server");
            let address = listener.local_addr().expect("mock server address");
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.expect("accept client");
                let _ = read_request(&mut socket).await;
                let response = format!(
                    "HTTP/1.1 {status} Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                );
                socket
                    .write_all(response.as_bytes())
                    .await
                    .expect("write error response");
            });

            let result = client_for(address)
                .list_similar(&session(), "item-id", 10)
                .await;
            server.await.expect("mock server task");
            assert!(matches!(result, Err(ApiError::HttpStatus(actual)) if actual == status));
        }
    }

    #[tokio::test]
    async fn person_items_uses_items_route_with_person_ids() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock server");
        let address = listener.local_addr().expect("mock server address");
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept client");
            let request = read_request(&mut socket).await;
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 12\r\nConnection: close\r\n\r\n{\"Items\":[]}",
                )
                .await
                .expect("write response");
            request
        });

        let response = client_for(address)
            .list_person_items(&session(), "person-1", 12)
            .await
            .expect("list person items");
        let request = server.await.expect("mock server task");
        let request_line = request.lines().next().expect("HTTP request line");
        let uri = request_line.split_whitespace().nth(1).expect("request URI");

        assert!(response.items.is_empty());
        assert!(uri.starts_with("/jellyfin/Items?"));
        assert!(uri.contains("personIds=person-1"));
        assert!(uri.contains("limit=12"));
        assert!(!uri.contains("secret-token"));
    }

    #[tokio::test]
    async fn requests_primary_image_without_tag_when_server_omits_image_tags() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock server");
        let address = listener.local_addr().expect("mock server address");
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept client");
            let request = read_request(&mut socket).await;
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: image/jpeg\r\nContent-Length: 4\r\nConnection: close\r\n\r\nJPEG",
                )
                .await
                .expect("write image response");
            request
        });

        let image = client_for(address)
            .get_primary_image(&session(), "item-id", None)
            .await
            .expect("fetch image without a cache tag");
        let request = server.await.expect("mock server task");
        let request_line = request.lines().next().expect("HTTP request line");

        assert_eq!(image, b"JPEG");
        assert!(request_line.starts_with("GET /jellyfin/Items/item-id/Images/Primary?"));
        assert!(request_line.contains("format=jpg"));
        assert!(!request_line.contains("tag="));
    }

    #[tokio::test]
    async fn requests_authenticated_person_image_with_encoded_name_and_tag() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock server");
        let address = listener.local_addr().expect("mock server address");
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept client");
            let request = read_request(&mut socket).await;
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: image/jpeg\r\nContent-Length: 4\r\nConnection: close\r\n\r\nJPEG",
                )
                .await
                .expect("write image response");
            request
        });

        let image = client_for(address)
            .get_person_image(&session(), "Ada Lovelace/Byron", Some("cache/tag"))
            .await
            .expect("fetch person image");
        let request = server.await.expect("mock server task");
        let request_line = request.lines().next().expect("HTTP request line");

        assert_eq!(image, b"JPEG");
        assert!(
            request_line
                .starts_with("GET /jellyfin/Persons/Ada%20Lovelace%2FByron/Images/Primary?")
        );
        assert!(request_line.contains("tag=cache%2Ftag"));
        assert!(!request_line.contains("secret-token"));
        assert!(request.to_ascii_lowercase().contains("accept: image/jpeg"));
        assert!(
            request
                .to_ascii_lowercase()
                .contains("token=\"secret-token\"")
        );
    }

    #[tokio::test]
    async fn person_image_keeps_not_found_and_server_errors_typed() {
        for status in [StatusCode::NOT_FOUND, StatusCode::INTERNAL_SERVER_ERROR] {
            let listener = TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind mock server");
            let address = listener.local_addr().expect("mock server address");
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.expect("accept client");
                let _ = read_request(&mut socket).await;
                let response = format!(
                    "HTTP/1.1 {status} Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                );
                socket
                    .write_all(response.as_bytes())
                    .await
                    .expect("write error response");
            });

            let result = client_for(address)
                .get_person_image(&session(), "Ada", None)
                .await;
            server.await.expect("mock server task");
            assert!(matches!(result, Err(ApiError::HttpStatus(actual)) if actual == status));
        }
    }

    #[tokio::test]
    async fn person_and_similar_endpoints_reject_invalid_identifiers_before_request() {
        let client = client_for("127.0.0.1:1".parse().expect("local socket address"));
        let session = session();

        assert!(matches!(
            client.list_similar(&session, "", 10).await,
            Err(ApiError::InvalidResponse)
        ));
        assert!(matches!(
            client.list_person_items(&session, "person\n1", 10).await,
            Err(ApiError::InvalidResponse)
        ));
        assert!(matches!(
            client.get_person_image(&session, "", None).await,
            Err(ApiError::InvalidResponse)
        ));
        assert!(matches!(
            client.get_person_image(&session, "Ada", Some("\n")).await,
            Err(ApiError::InvalidResponse)
        ));
    }

    #[tokio::test]
    async fn rejects_oversized_content_length_before_reading_the_body() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock server");
        let address = listener.local_addr().expect("mock server address");
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept client");
            let _ = read_request(&mut socket).await;
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: image/jpeg\r\nContent-Length: 5242881\r\nConnection: close\r\n\r\n",
                )
                .await
                .expect("write oversized response headers");
        });

        let result = client_for(address)
            .get_primary_image(&session(), "item-id", Some("image-tag"))
            .await;
        server.await.expect("mock server task");
        assert!(matches!(result, Err(ApiError::ImageTooLarge)));
    }
}

#[cfg(test)]
mod item_query_serialization_tests {
    use super::{ItemFilter, ItemQuery, ItemSortBy, ItemSortOrder};
    use std::collections::HashMap;

    fn params(query: &ItemQuery) -> HashMap<String, Vec<String>> {
        let mut result = HashMap::new();
        for (key, value) in query.to_query_params("user-1") {
            result.entry(key).or_insert_with(Vec::new).push(value);
        }
        result
    }

    fn assert_values(params: &HashMap<String, Vec<String>>, key: &str, expected: &[&str]) {
        assert_eq!(
            params
                .get(key)
                .map(|values| values.iter().map(String::as_str).collect::<Vec<_>>()),
            Some(expected.to_vec()),
            "{key}"
        );
    }

    #[test]
    fn default_query_keeps_paging_and_omits_all_optional_parameters() {
        let query = ItemQuery {
            start_index: 50,
            limit: 25,
            ..ItemQuery::default()
        };
        let actual = params(&query);

        assert_values(&actual, "userId", &["user-1"]);
        assert_values(&actual, "recursive", &["true"]);
        assert_values(&actual, "startIndex", &["50"]);
        assert_values(&actual, "limit", &["25"]);
        assert_eq!(actual.len(), 4);
        for optional in [
            "parentId",
            "includeItemTypes",
            "searchTerm",
            "sortBy",
            "sortOrder",
            "filters",
            "genres",
            "years",
            "personIds",
            "ids",
            "enableTotalRecordCount",
        ] {
            assert!(!actual.contains_key(optional), "unexpected {optional}");
        }
    }

    #[test]
    fn serializes_all_library_query_fields_with_jellyfin_names_and_values() {
        let query = ItemQuery {
            parent_id: Some("parent-1".into()),
            include_item_types: Some(vec!["Movie".into(), "Series".into()]),
            search_term: Some("space opera".into()),
            sort_by: Some(vec![ItemSortBy::SortName, ItemSortBy::PremiereDate]),
            sort_order: Some(vec![ItemSortOrder::Ascending, ItemSortOrder::Descending]),
            filters: Some(vec![ItemFilter::IsUnplayed, ItemFilter::IsFavorite]),
            genres: Some(vec!["Science Fiction".into(), "Drama".into()]),
            years: Some(vec![1999, 2024]),
            person_ids: Some(vec!["person-1".into(), "person-2".into()]),
            ids: Some(vec!["item-1".into(), "item-2".into()]),
            enable_total_record_count: Some(false),
            start_index: 100,
            limit: 50,
            ..ItemQuery::default()
        };
        let actual = params(&query);

        for (key, expected) in [
            ("parentId", &["parent-1"] as &[&str]),
            ("includeItemTypes", &["Movie,Series"]),
            ("searchTerm", &["space opera"]),
            ("sortBy", &["SortName", "PremiereDate"]),
            ("sortOrder", &["Ascending", "Descending"]),
            ("filters", &["IsUnplayed", "IsFavorite"]),
            ("genres", &["Science Fiction", "Drama"]),
            ("years", &["1999", "2024"]),
            ("personIds", &["person-1", "person-2"]),
            ("ids", &["item-1,item-2"]),
            ("enableTotalRecordCount", &["false"]),
            ("startIndex", &["100"]),
            ("limit", &["50"]),
        ] {
            assert_values(&actual, key, expected);
        }
    }

    #[test]
    fn serializes_explicit_false_and_empty_lists_but_omits_unset_values() {
        let query = ItemQuery {
            recursive: false,
            sort_by: Some(vec![]),
            filters: Some(vec![]),
            genres: Some(vec![]),
            years: Some(vec![]),
            enable_total_record_count: Some(true),
            ..ItemQuery::default()
        };
        let actual = params(&query);

        assert_values(&actual, "recursive", &["false"]);
        assert_values(&actual, "enableTotalRecordCount", &["true"]);
        for empty_array in ["sortBy", "filters", "genres", "years"] {
            assert!(!actual.contains_key(empty_array));
        }
        assert!(!actual.contains_key("parentId"));
        assert!(!actual.contains_key("sortOrder"));
        assert!(!actual.contains_key("searchTerm"));
    }
}
