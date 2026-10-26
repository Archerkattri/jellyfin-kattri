//! Build authorized, same-origin requests for negotiated Jellyfin media sources.

use std::{collections::HashSet, fmt};

use reqwest::header::{AUTHORIZATION, HeaderName, HeaderValue};
use url::Url;
use zeroize::Zeroizing;

use crate::{JellyfinClient, JellyfinTicks, MediaSourceInfo, PlayMethod, Session};

const MAX_PLAYBACK_URL_BYTES: usize = 64 * 1024;
const MAX_REQUIRED_HEADER_COUNT: usize = 64;
const MAX_REQUIRED_HEADER_BYTES: usize = 64 * 1024;

/// A media source request whose URL and authorization-bearing headers are
/// intentionally hidden when formatted.
pub struct PlaybackSourceRequest {
    url: Zeroizing<String>,
    authorization: Zeroizing<String>,
    required_headers: Vec<RequiredHeader>,
}

impl fmt::Debug for PlaybackSourceRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PlaybackSourceRequest")
            .field("url", &"[REDACTED]")
            .field("authorization", &"[REDACTED]")
            .field("required_headers", &"[REDACTED]")
            .finish()
    }
}

impl PlaybackSourceRequest {
    /// Consume the prepared request into a URL and iterable header collection
    /// for a media engine. The URL and every header value use zeroizing storage;
    /// the URL contains no recognized authentication query parameters.
    pub fn into_parts(self) -> (PlaybackSourceUrl, Vec<PlaybackSourceHeader>) {
        let mut headers = Vec::with_capacity(self.required_headers.len() + 1);
        headers.push(PlaybackSourceHeader {
            name: "Authorization".to_owned(),
            value: self.authorization,
        });
        headers.extend(
            self.required_headers
                .into_iter()
                .map(|header| PlaybackSourceHeader {
                    name: header.name.as_str().to_owned(),
                    value: header.value,
                }),
        );
        (PlaybackSourceUrl(self.url), headers)
    }
}

/// A one-shot media URL held in zeroizing storage and redacted from `Debug`.
pub struct PlaybackSourceUrl(Zeroizing<String>);

impl PlaybackSourceUrl {
    /// Borrow the URL string while handing it to the media engine.
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl fmt::Debug for PlaybackSourceUrl {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("PlaybackSourceUrl([REDACTED])")
    }
}

/// One outgoing media request header. Its value is zeroized on drop and hidden
/// by `Debug`; use [`PlaybackSourceHeader::as_pair`] to hand it to the engine.
pub struct PlaybackSourceHeader {
    name: String,
    value: Zeroizing<String>,
}

impl PlaybackSourceHeader {
    /// Return this header as a borrowed `(name, value)` pair.
    pub fn as_pair(&self) -> (&str, &str) {
        (&self.name, self.value.as_str())
    }
}

impl fmt::Debug for PlaybackSourceHeader {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PlaybackSourceHeader")
            .field("name", &self.name)
            .field("value", &"[REDACTED]")
            .finish()
    }
}

/// Inputs that bind a negotiated source to the item's selected tracks and
/// starting position.
#[derive(Clone, Copy)]
pub struct PlaybackSourceRequestOptions<'a> {
    pub item_id: &'a str,
    pub media_source: &'a MediaSourceInfo,
    pub play_method: PlayMethod,
    pub play_session_id: Option<&'a str>,
    pub audio_stream_index: Option<i32>,
    pub subtitle_stream_index: Option<i32>,
    pub start_time_ticks: Option<JellyfinTicks>,
}

struct RequiredHeader {
    name: HeaderName,
    value: Zeroizing<String>,
}

/// Errors while constructing a safe playback-source request.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PlaybackSourceError {
    #[error("item ID is empty or invalid")]
    InvalidItemId,
    #[error("negotiated media source has no usable container")]
    InvalidContainer,
    #[error("transcoding playback requires the server-provided transcoding URL")]
    MissingTranscodingUrl,
    #[error("negotiated media source does not support the requested playback method")]
    UnsupportedPlayMethod,
    #[error("server-provided transcoding URL is invalid")]
    InvalidTranscodingUrl,
    #[error("server-provided transcoding URL is not on the configured server origin")]
    CrossOriginTranscodingUrl,
    #[error("server-provided transcoding URL contains credentials or a fragment")]
    UnsafeTranscodingUrl,
    #[error("required HTTP header name is invalid")]
    InvalidRequiredHeaderName,
    #[error("required HTTP header name is duplicated")]
    DuplicateRequiredHeader,
    #[error("required HTTP headers cannot replace Jellyfin Authorization")]
    AuthorizationHeaderConflict,
    #[error("required HTTP header is unsafe for a same-origin authenticated request")]
    UnsafeRequiredHeader,
    #[error("required HTTP header value is invalid")]
    InvalidRequiredHeaderValue,
    #[error("playback source URL or required headers exceed supported limits")]
    InputTooLarge,
}

/// Return whether an item ID can be placed in a URL path as one segment.
///
/// Empty values, the relative segments `.` and `..`, the separators `/`, `?` and
/// `#`, and control characters are refused. The URL builder drops `.` and `..`
/// segments, which would otherwise redirect the request to another endpoint.
pub(crate) fn is_safe_item_id(item_id: &str) -> bool {
    !item_id.is_empty()
        && item_id != "."
        && item_id != ".."
        && !item_id
            .chars()
            .any(|character| matches!(character, '/' | '?' | '#') || character.is_control())
}

impl JellyfinClient {
    /// Build an authenticated request for a media source selected from
    /// Jellyfin's playback-info response.
    ///
    /// Direct play and direct stream use the negotiated source container and
    /// set `static=true` or `static=false`, respectively. Transcoding uses the
    /// server-provided `TranscodingUrl` without rewriting its existing query
    /// options, adding only missing request values. Jellyfin credentials are
    /// placed only in the MediaBrowser `Authorization` header, never in the
    /// URL query.
    pub fn build_playback_source_request(
        &self,
        session: &Session,
        options: PlaybackSourceRequestOptions<'_>,
    ) -> Result<PlaybackSourceRequest, PlaybackSourceError> {
        self.build_playback_source_request_for_route(session, options, "Videos")
    }

    /// Build an authenticated request for a negotiated Jellyfin audio source.
    ///
    /// The negotiated source and required headers are validated identically to
    /// video playback, but direct play and direct stream use `/Audio/...`.
    pub fn build_audio_playback_source_request(
        &self,
        session: &Session,
        options: PlaybackSourceRequestOptions<'_>,
    ) -> Result<PlaybackSourceRequest, PlaybackSourceError> {
        self.build_playback_source_request_for_route(session, options, "Audio")
    }

    fn build_playback_source_request_for_route(
        &self,
        session: &Session,
        options: PlaybackSourceRequestOptions<'_>,
        direct_stream_route: &'static str,
    ) -> Result<PlaybackSourceRequest, PlaybackSourceError> {
        let PlaybackSourceRequestOptions {
            item_id,
            media_source,
            play_method,
            play_session_id,
            audio_stream_index,
            subtitle_stream_index,
            start_time_ticks,
        } = options;
        if !is_safe_item_id(item_id) {
            return Err(PlaybackSourceError::InvalidItemId);
        }
        let supported = match play_method {
            PlayMethod::DirectPlay => media_source.supports_direct_play,
            PlayMethod::DirectStream => media_source.supports_direct_stream,
            PlayMethod::Transcode => media_source.supports_transcoding,
        };
        if !supported {
            return Err(PlaybackSourceError::UnsupportedPlayMethod);
        }

        let mut url = match play_method {
            PlayMethod::Transcode => {
                let transcoding_url = media_source
                    .transcoding_url
                    .as_deref()
                    .filter(|url| !url.trim().is_empty())
                    .ok_or(PlaybackSourceError::MissingTranscodingUrl)?;
                resolve_transcoding_url(&self.config.base_url, transcoding_url)?
            }
            PlayMethod::DirectPlay | PlayMethod::DirectStream => {
                let container = media_source
                    .container
                    .as_deref()
                    .filter(|container| is_safe_container(container))
                    .ok_or(PlaybackSourceError::InvalidContainer)?;
                let stream_route = format!("stream.{container}");
                self.endpoint(&[direct_stream_route, item_id, &stream_route])
            }
        };

        remove_auth_query_parameters(&mut url);
        let transcode = play_method == PlayMethod::Transcode;
        if !transcode {
            set_query_parameter(
                &mut url,
                "static",
                Some((play_method == PlayMethod::DirectPlay).to_string()),
            );
        }
        add_query_parameter_if_missing(
            &mut url,
            "mediaSourceId",
            media_source
                .id
                .as_deref()
                .filter(|id| !id.is_empty())
                .map(str::to_owned),
            transcode,
        );
        add_query_parameter_if_missing(
            &mut url,
            "liveStreamId",
            media_source
                .live_stream_id
                .as_deref()
                .filter(|id| !id.is_empty())
                .map(str::to_owned),
            transcode,
        );
        add_query_parameter_if_missing(
            &mut url,
            "deviceId",
            Some(self.config.device_id.clone()),
            transcode,
        );
        add_query_parameter_if_missing(
            &mut url,
            "playSessionId",
            play_session_id.map(str::to_owned),
            transcode,
        );
        add_query_parameter_if_missing(
            &mut url,
            "audioStreamIndex",
            audio_stream_index.map(|index| index.to_string()),
            transcode,
        );
        add_query_parameter_if_missing(
            &mut url,
            "subtitleStreamIndex",
            subtitle_stream_index.map(|index| index.to_string()),
            transcode,
        );
        add_query_parameter_if_missing(
            &mut url,
            "startTimeTicks",
            start_time_ticks.map(|ticks| ticks.as_ticks().to_string()),
            transcode,
        );
        if url.as_str().len() > MAX_PLAYBACK_URL_BYTES {
            return Err(PlaybackSourceError::InputTooLarge);
        }

        let required_headers = validate_required_headers(media_source)?;

        let authorization = media_browser_authorization(self, &session.access_token);

        Ok(PlaybackSourceRequest {
            url: Zeroizing::new(url.into()),
            authorization,
            required_headers,
        })
    }
}

fn resolve_transcoding_url(base_url: &Url, value: &str) -> Result<Url, PlaybackSourceError> {
    if value.len() > MAX_PLAYBACK_URL_BYTES {
        return Err(PlaybackSourceError::InputTooLarge);
    }
    let mut base_directory = base_url.clone();
    if !base_directory.path().ends_with('/') {
        let mut path = base_directory.path().to_owned();
        path.push('/');
        base_directory.set_path(&path);
    }
    let value = strip_media_pseudo_scheme(value).unwrap_or(value);
    let mut url = match Url::parse(value) {
        Ok(url) => url,
        Err(_) => {
            if value.starts_with("//") {
                return Err(PlaybackSourceError::InvalidTranscodingUrl);
            }
            let normalized_path = value.trim_start_matches('/');
            let path_only = normalized_path
                .split(['?', '#'])
                .next()
                .unwrap_or(normalized_path);
            let configured_prefix = base_url.path().trim_matches('/');
            let includes_prefix = !configured_prefix.is_empty()
                && (path_only == configured_prefix
                    || path_only.starts_with(&format!("{configured_prefix}/")));
            let base = if includes_prefix {
                let mut origin_root = base_url.clone();
                origin_root.set_path("/");
                origin_root.set_query(None);
                origin_root.set_fragment(None);
                origin_root
            } else {
                base_directory
            };
            base.join(normalized_path)
                .map_err(|_| PlaybackSourceError::InvalidTranscodingUrl)?
        }
    };
    if !matches!(url.scheme(), "http" | "https") {
        return Err(PlaybackSourceError::InvalidTranscodingUrl);
    }
    if url.origin() != base_url.origin() {
        return Err(PlaybackSourceError::CrossOriginTranscodingUrl);
    }
    if !url.username().is_empty() || url.password().is_some() || url.fragment().is_some() {
        return Err(PlaybackSourceError::UnsafeTranscodingUrl);
    }
    let configured_prefix = base_url.path().trim_end_matches('/');
    if !configured_prefix.is_empty()
        && url.path() != configured_prefix
        && !url.path().starts_with(&format!("{configured_prefix}/"))
    {
        let prefixed_path = format!("{configured_prefix}/{}", url.path().trim_start_matches('/'));
        url.set_path(&prefixed_path);
    }
    Ok(url)
}

fn strip_media_pseudo_scheme(value: &str) -> Option<&str> {
    let (scheme, rest) = value.split_once(':')?;
    scheme.eq_ignore_ascii_case("media").then_some(rest)
}

fn is_safe_container(container: &str) -> bool {
    !container.is_empty()
        && container
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn remove_auth_query_parameters(url: &mut Url) {
    const AUTH_QUERY_KEYS: [&str; 13] = [
        "api_key",
        "apikey",
        "api-key",
        "access_token",
        "accesstoken",
        "token",
        "x-emby-token",
        "x-mediabrowser-token",
        "x-jellyfin-token",
        "authorization",
        "auth",
        "auth_token",
        "jwt",
    ];
    let retained_pairs: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(name, _)| {
            !AUTH_QUERY_KEYS
                .iter()
                .any(|auth_key| name.eq_ignore_ascii_case(auth_key))
        })
        .map(|(name, value)| (name.into_owned(), value.into_owned()))
        .collect();
    if retained_pairs.len() == url.query_pairs().count() {
        return;
    }
    let mut query = url.query_pairs_mut();
    query.clear();
    for (name, value) in retained_pairs {
        query.append_pair(&name, &value);
    }
}

fn set_query_parameter(url: &mut Url, name: &str, value: Option<String>) {
    let Some(value) = value else {
        return;
    };
    let retained_pairs: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(existing_name, _)| !existing_name.eq_ignore_ascii_case(name))
        .map(|(existing_name, existing_value)| {
            (existing_name.into_owned(), existing_value.into_owned())
        })
        .collect();
    let mut query = url.query_pairs_mut();
    query.clear();
    for (existing_name, existing_value) in retained_pairs {
        query.append_pair(&existing_name, &existing_value);
    }
    query.append_pair(name, &value);
}

fn add_query_parameter_if_missing(
    url: &mut Url,
    name: &str,
    value: Option<String>,
    preserve_existing: bool,
) {
    if preserve_existing
        && url
            .query_pairs()
            .any(|(existing_name, _)| existing_name.eq_ignore_ascii_case(name))
    {
        return;
    }
    set_query_parameter(url, name, value);
}

fn media_browser_authorization(client: &JellyfinClient, token: &str) -> Zeroizing<String> {
    let mut metadata = Zeroizing::new(String::with_capacity(256));
    metadata.push_str("MediaBrowser Client=\"");
    push_encoded_auth_value(&mut metadata, &client.config.client_name);
    metadata.push_str("\", Device=\"");
    push_encoded_auth_value(&mut metadata, &client.config.device_name);
    metadata.push_str("\", DeviceId=\"");
    push_encoded_auth_value(&mut metadata, &client.config.device_id);
    metadata.push_str("\", Version=\"");
    push_encoded_auth_value(&mut metadata, &client.config.version);
    metadata.push_str("\", Token=\"");
    push_encoded_auth_value(&mut metadata, token);
    metadata.push('\"');
    metadata
}

fn push_encoded_auth_value(output: &mut String, value: &str) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.~".contains(&byte) {
            output.push(byte as char);
        } else {
            output.push('%');
            output.push(HEX[(byte >> 4) as usize] as char);
            output.push(HEX[(byte & 0x0f) as usize] as char);
        }
    }
}

fn validate_required_headers(
    media_source: &MediaSourceInfo,
) -> Result<Vec<RequiredHeader>, PlaybackSourceError> {
    let mut seen_names = HashSet::new();
    let mut headers = Vec::new();
    let mut header_count = 0usize;
    let mut total_header_bytes = 0usize;
    for (raw_name, raw_value) in media_source
        .required_http_headers
        .iter()
        .flat_map(|headers| headers.iter())
    {
        header_count = header_count.saturating_add(1);
        total_header_bytes = total_header_bytes
            .saturating_add(raw_name.len())
            .saturating_add(raw_value.as_deref().map_or(0, str::len));
        if header_count > MAX_REQUIRED_HEADER_COUNT
            || total_header_bytes > MAX_REQUIRED_HEADER_BYTES
        {
            return Err(PlaybackSourceError::InputTooLarge);
        }
        let name = HeaderName::from_bytes(raw_name.as_bytes())
            .map_err(|_| PlaybackSourceError::InvalidRequiredHeaderName)?;
        if name == AUTHORIZATION {
            return Err(PlaybackSourceError::AuthorizationHeaderConflict);
        }
        if name.as_str().eq_ignore_ascii_case("host")
            || name.as_str().eq_ignore_ascii_case("proxy-authorization")
        {
            return Err(PlaybackSourceError::UnsafeRequiredHeader);
        }
        if !seen_names.insert(name.as_str().to_ascii_lowercase()) {
            return Err(PlaybackSourceError::DuplicateRequiredHeader);
        }
        let Some(value) = raw_value.as_deref() else {
            continue;
        };
        if value.contains('\r') || value.contains('\n') {
            return Err(PlaybackSourceError::InvalidRequiredHeaderValue);
        }
        HeaderValue::from_str(value)
            .map_err(|_| PlaybackSourceError::InvalidRequiredHeaderValue)?;
        headers.push(RequiredHeader {
            name,
            value: Zeroizing::new(value.to_owned()),
        });
    }
    Ok(headers)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::{ClientConfig, UserDto};

    fn client() -> JellyfinClient {
        JellyfinClient::new(
            ClientConfig::new(
                "https://jellyfin.example.test/jellyfin/",
                "Kattri test",
                "Test device",
                "device-id",
                "1.0-test",
            )
            .expect("valid client config"),
        )
        .expect("valid client")
    }

    fn session() -> Session {
        Session {
            access_token: "session-token-secret".to_owned(),
            user: UserDto {
                id: "user-id".to_owned(),
                name: "test-user".to_owned(),
                server_id: None,
            },
            server_id: None,
            session_info: None,
        }
    }

    fn media_source(extra: serde_json::Value) -> MediaSourceInfo {
        serde_json::from_value(extra).expect("valid media source fixture")
    }

    fn options<'a>(
        media_source: &'a MediaSourceInfo,
        play_method: PlayMethod,
    ) -> PlaybackSourceRequestOptions<'a> {
        PlaybackSourceRequestOptions {
            item_id: "item-id",
            media_source,
            play_method,
            play_session_id: Some("play-session-id"),
            audio_stream_index: Some(2),
            subtitle_stream_index: Some(5),
            start_time_ticks: Some(JellyfinTicks::from_ticks(90_000_000)),
        }
    }

    fn query(url: &Url) -> HashMap<String, String> {
        url.query_pairs()
            .map(|(name, value)| (name.into_owned(), value.into_owned()))
            .collect()
    }

    fn assert_no_auth_query(url: &Url) {
        for (name, value) in url.query_pairs() {
            assert!(!matches!(
                name.to_ascii_lowercase().as_str(),
                "api_key"
                    | "apikey"
                    | "api-key"
                    | "access_token"
                    | "accesstoken"
                    | "token"
                    | "x-emby-token"
                    | "x-mediabrowser-token"
                    | "x-jellyfin-token"
                    | "authorization"
                    | "auth"
                    | "auth_token"
                    | "jwt"
            ));
            assert!(!value.contains("session-token-secret"));
            assert!(!value.contains("header-secret"));
        }
    }

    #[test]
    fn opened_live_stream_id_is_forwarded_to_direct_and_transcode_routes() {
        let client = client();
        let session = session();
        let direct = media_source(serde_json::json!({
            "Id": "live-source",
            "Container": "ts",
            "LiveStreamId": "opaque-live-id",
            "SupportsDirectPlay": true
        }));
        let direct_url = client
            .build_playback_source_request(&session, options(&direct, PlayMethod::DirectPlay))
            .expect("build direct live source request")
            .into_parts()
            .0;
        let direct_url = Url::parse(direct_url.as_str()).expect("parse protected URL");
        assert_eq!(
            query(&direct_url).get("liveStreamId").map(String::as_str),
            Some("opaque-live-id")
        );

        let transcode = media_source(serde_json::json!({
            "Id": "live-source",
            "Container": "ts",
            "LiveStreamId": "opaque-live-id",
            "TranscodingUrl": "/Videos/item-id/master.m3u8",
            "SupportsTranscoding": true
        }));
        let transcode_url = client
            .build_playback_source_request(&session, options(&transcode, PlayMethod::Transcode))
            .expect("build transcoded live source request")
            .into_parts()
            .0;
        let transcode_url = Url::parse(transcode_url.as_str()).expect("parse protected URL");
        assert_eq!(
            query(&transcode_url)
                .get("liveStreamId")
                .map(String::as_str),
            Some("opaque-live-id")
        );
    }

    #[test]
    fn item_id_validation_refuses_relative_segments_and_path_delimiters() {
        for item_id in ["", ".", "..", "a/b", "a?b", "a#b", "a\nb", "a\u{7f}b"] {
            assert!(!is_safe_item_id(item_id), "{item_id:?} must be refused");
        }
        assert!(is_safe_item_id("0f8e2c1a9b7d4e3f8a6b5c4d3e2f1a0b"));
        assert!(is_safe_item_id("item-id"));
        assert!(is_safe_item_id("..."));
    }

    #[test]
    fn unsafe_item_ids_are_refused_before_a_url_is_built() {
        let client = client();
        let session = session();
        let source = media_source(serde_json::json!({
            "Id": "source-id",
            "Container": "mkv",
            "SupportsDirectPlay": true
        }));

        for item_id in [".", "..", "", "a/b", "a?b", "a#b"] {
            let mut options = options(&source, PlayMethod::DirectPlay);
            options.item_id = item_id;
            assert_eq!(
                client
                    .build_playback_source_request(&session, options)
                    .err(),
                Some(PlaybackSourceError::InvalidItemId),
                "{item_id:?} must be refused"
            );
        }
    }

    #[test]
    fn direct_play_and_direct_stream_use_video_route_and_distinct_static_values() {
        let client = client();
        let session = session();
        let source = media_source(serde_json::json!({
            "Id": "source-id",
            "Container": "mkv",
            "SupportsDirectPlay": true,
            "SupportsDirectStream": true
        }));

        for (method, expected_static) in [
            (PlayMethod::DirectPlay, "true"),
            (PlayMethod::DirectStream, "false"),
        ] {
            let request = client
                .build_playback_source_request(&session, options(&source, method))
                .expect("supported direct method");
            let (url, _) = request.into_parts();
            let url = Url::parse(url.as_str()).expect("valid generated URL");
            assert_eq!(url.path(), "/jellyfin/Videos/item-id/stream.mkv");

            let query = query(&url);
            assert_eq!(
                query.get("static").map(String::as_str),
                Some(expected_static)
            );
            assert_eq!(
                query.get("mediaSourceId").map(String::as_str),
                Some("source-id")
            );
            assert_eq!(query.get("deviceId").map(String::as_str), Some("device-id"));
            assert_eq!(
                query.get("playSessionId").map(String::as_str),
                Some("play-session-id")
            );
            assert_eq!(query.get("audioStreamIndex").map(String::as_str), Some("2"));
            assert_eq!(
                query.get("subtitleStreamIndex").map(String::as_str),
                Some("5")
            );
            assert_eq!(
                query.get("startTimeTicks").map(String::as_str),
                Some("90000000")
            );
        }
    }

    #[test]
    fn direct_audio_play_and_stream_use_audio_route_without_query_credentials() {
        let client = client();
        let session = session();
        let source = media_source(serde_json::json!({
            "Id": "audio-source-id",
            "Container": "flac",
            "SupportsDirectPlay": true,
            "SupportsDirectStream": true
        }));

        for (method, expected_static) in [
            (PlayMethod::DirectPlay, "true"),
            (PlayMethod::DirectStream, "false"),
        ] {
            let request = client
                .build_audio_playback_source_request(&session, options(&source, method))
                .expect("supported audio method");
            let (url, headers) = request.into_parts();
            let url = Url::parse(url.as_str()).expect("valid generated audio URL");
            assert_eq!(url.path(), "/jellyfin/Audio/item-id/stream.flac");
            assert_eq!(
                query(&url).get("static").map(String::as_str),
                Some(expected_static)
            );
            assert_eq!(
                query(&url).get("mediaSourceId").map(String::as_str),
                Some("audio-source-id")
            );
            assert_no_auth_query(&url);
            let (header_name, header_value) = headers[0].as_pair();
            assert_eq!(header_name, "Authorization");
            assert!(header_value.contains("Token=\"session-token-secret\""));
        }
    }

    #[test]
    fn credentials_are_only_in_authorization_header_not_url_or_debug() {
        let client = client();
        let session = session();
        let source = media_source(serde_json::json!({
            "Container": "mkv",
            "SupportsDirectPlay": true,
            "RequiredHttpHeaders": { "X-Playback-Key": "header-secret" }
        }));
        let request = client
            .build_playback_source_request(&session, options(&source, PlayMethod::DirectPlay))
            .expect("supported direct play");
        let debug = format!("{request:?}");
        assert!(!debug.contains("session-token-secret"));
        assert!(!debug.contains("header-secret"));

        let (url, headers) = request.into_parts();
        let url = Url::parse(url.as_str()).expect("valid generated URL");
        assert_no_auth_query(&url);
        assert!(!url.as_str().contains("header-secret"));

        let header_values: HashMap<_, _> = headers
            .iter()
            .map(|header| {
                let (name, value) = header.as_pair();
                (name.to_ascii_lowercase(), value)
            })
            .collect();
        let authorization = header_values
            .get("authorization")
            .expect("authorization header");
        assert!(authorization.contains("Token=\"session-token-secret\""));
        assert_eq!(header_values.get("x-playback-key"), Some(&"header-secret"));
    }

    #[test]
    fn transcode_keeps_same_origin_options_adds_missing_values_and_omits_static() {
        let client = client();
        let session = session();
        let source = media_source(serde_json::json!({
            "Id": "source-id",
            "SupportsTranscoding": true,
            "RequiredHttpHeaders": { "X-Playback-Key": "header-secret" },
            "TranscodingUrl": "https://jellyfin.example.test/Videos/item-id/master.m3u8?api_key=leaked&token=also-leaked&MediaSourceId=server-source&VideoCodec=h264&PlaySessionId=server-session"
        }));
        let request = client
            .build_playback_source_request(&session, options(&source, PlayMethod::Transcode))
            .expect("valid same-origin transcode URL");
        let (url, headers) = request.into_parts();
        let url = Url::parse(url.as_str()).expect("valid generated URL");
        assert_eq!(url.origin(), client.server_origin().origin());
        assert_eq!(url.path(), "/jellyfin/Videos/item-id/master.m3u8");

        let query = query(&url);
        assert!(!query.contains_key("static"));
        assert_eq!(
            query.get("MediaSourceId").map(String::as_str),
            Some("server-source")
        );
        assert_eq!(
            query.get("PlaySessionId").map(String::as_str),
            Some("server-session")
        );
        assert_eq!(query.get("VideoCodec").map(String::as_str), Some("h264"));
        assert_eq!(query.get("deviceId").map(String::as_str), Some("device-id"));
        assert_eq!(query.get("audioStreamIndex").map(String::as_str), Some("2"));
        assert_eq!(
            query.get("subtitleStreamIndex").map(String::as_str),
            Some("5")
        );
        assert_eq!(
            query.get("startTimeTicks").map(String::as_str),
            Some("90000000")
        );
        assert_no_auth_query(&url);
        assert!(!url.as_str().contains("header-secret"));
        assert_eq!(headers[0].as_pair().0, "Authorization");
        assert!(headers[0].as_pair().1.contains("session-token-secret"));
        assert_eq!(headers[1].as_pair(), ("x-playback-key", "header-secret"));
    }

    #[test]
    fn transcode_rejects_foreign_origin_and_resolves_relative_url_under_server_path() {
        let client = client();
        let session = session();
        let foreign = media_source(serde_json::json!({
            "SupportsTranscoding": true,
            "TranscodingUrl": "https://attacker.example.test/Videos/item/master.m3u8?token=secret"
        }));
        assert_eq!(
            client
                .build_playback_source_request(&session, options(&foreign, PlayMethod::Transcode))
                .err(),
            Some(PlaybackSourceError::CrossOriginTranscodingUrl)
        );

        let relative = media_source(serde_json::json!({
            "SupportsTranscoding": true,
            "TranscodingUrl": "Videos/item/master.m3u8?VideoCodec=h264"
        }));
        let request = client
            .build_playback_source_request(&session, options(&relative, PlayMethod::Transcode))
            .expect("relative URL resolves on configured origin");
        let (url, _) = request.into_parts();
        let url = Url::parse(url.as_str()).expect("valid generated URL");
        assert_eq!(url.origin(), client.server_origin().origin());
        assert_eq!(url.path(), "/jellyfin/Videos/item/master.m3u8");
        assert_eq!(
            query(&url).get("VideoCodec").map(String::as_str),
            Some("h264")
        );
    }

    #[test]
    fn oversized_transcoding_url_is_refused_before_url_construction() {
        let client = client();
        let session = session();
        let source = media_source(serde_json::json!({
            "SupportsTranscoding": true,
            "TranscodingUrl": format!("/{}", "x".repeat(MAX_PLAYBACK_URL_BYTES))
        }));
        assert_eq!(
            client
                .build_playback_source_request(&session, options(&source, PlayMethod::Transcode))
                .err(),
            Some(PlaybackSourceError::InputTooLarge)
        );
    }

    #[test]
    fn required_headers_are_bounded_by_count_and_total_bytes() {
        let client = client();
        let session = session();

        let mut too_many = serde_json::Map::new();
        for index in 0..=MAX_REQUIRED_HEADER_COUNT {
            too_many.insert(format!("X-Test-{index}"), serde_json::json!("x"));
        }
        let source = media_source(serde_json::json!({
            "Container": "mkv",
            "SupportsDirectPlay": true,
            "RequiredHttpHeaders": too_many
        }));
        assert_eq!(
            client
                .build_playback_source_request(&session, options(&source, PlayMethod::DirectPlay))
                .err(),
            Some(PlaybackSourceError::InputTooLarge)
        );

        let source = media_source(serde_json::json!({
            "Container": "mkv",
            "SupportsDirectPlay": true,
            "RequiredHttpHeaders": { "X-Test": "x".repeat(MAX_REQUIRED_HEADER_BYTES) }
        }));
        assert_eq!(
            client
                .build_playback_source_request(&session, options(&source, PlayMethod::DirectPlay))
                .err(),
            Some(PlaybackSourceError::InputTooLarge)
        );
    }
}
