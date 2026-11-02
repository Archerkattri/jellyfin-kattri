//! Validated, per-client extra HTTP headers for the configured Jellyfin server.

use std::fmt;

use reqwest::header::{HeaderName, HeaderValue};
use zeroize::Zeroizing;

/// Maximum extra headers accepted for one Jellyfin client.
pub const MAX_EXTRA_REQUEST_HEADER_COUNT: usize = 32;
/// Maximum combined name and value bytes accepted for one Jellyfin client.
pub const MAX_EXTRA_REQUEST_HEADER_BYTES: usize = 16 * 1024;

/// A validated HTTP header value held in zeroizing memory.
#[derive(Clone)]
pub struct ExtraRequestHeader {
    pub(crate) name: HeaderName,
    pub(crate) value: Zeroizing<String>,
}

impl PartialEq for ExtraRequestHeader {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name && self.value.as_str() == other.value.as_str()
    }
}

impl Eq for ExtraRequestHeader {}

impl ExtraRequestHeader {
    /// Validate a custom header. The value is moved into zeroizing storage before validation.
    pub fn new(
        name: impl AsRef<str>,
        value: impl Into<String>,
    ) -> Result<Self, RequestHeadersError> {
        let value = Zeroizing::new(value.into());
        let name = HeaderName::from_bytes(name.as_ref().as_bytes())
            .map_err(|_| RequestHeadersError::InvalidName)?;
        if is_forbidden_name(&name) {
            return Err(RequestHeadersError::ForbiddenName);
        }
        if value.chars().any(char::is_control) || HeaderValue::from_str(value.as_str()).is_err() {
            return Err(RequestHeadersError::InvalidValue);
        }
        Ok(Self { name, value })
    }
}

impl fmt::Debug for ExtraRequestHeader {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ExtraRequestHeader")
            .field("name", &self.name)
            .field("value", &"[REDACTED]")
            .finish()
    }
}

/// Errors returned while validating per-client extra request headers.
#[derive(Clone, Copy, Debug, thiserror::Error, PartialEq, Eq)]
pub enum RequestHeadersError {
    #[error("custom HTTP header name is invalid")]
    InvalidName,
    #[error("custom HTTP header name is reserved")]
    ForbiddenName,
    #[error("custom HTTP header value is invalid")]
    InvalidValue,
    #[error("custom HTTP header name is duplicated")]
    DuplicateName,
    #[error("too many custom HTTP headers")]
    TooManyHeaders,
    #[error("custom HTTP headers exceed the size limit")]
    TooLarge,
}

/// Names that could replace transport, framing, routing, or Jellyfin auth behavior.
pub(crate) fn is_forbidden_name(name: &HeaderName) -> bool {
    let name = name.as_str();
    matches!(
        name,
        "accept"
            | "accept-encoding"
            | "accept-ranges"
            | "alt-used"
            | "api-key"
            | "api_key"
            | "apikey"
            | "authorization"
            | "auth"
            | "auth_token"
            | "access_token"
            | "connection"
            | "content-encoding"
            | "content-length"
            | "content-range"
            | "content-type"
            | "cookie"
            | "cookie2"
            | "destination"
            | "expect"
            | "forwarded"
            | "host"
            | "if-range"
            | "keep-alive"
            | "origin"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "proxy-connection"
            | "range"
            | "referer"
            | "set-cookie"
            | "te"
            | "token"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
            | "user-agent"
            | "via"
            | "www-authenticate"
            | "x-access-token"
            | "x-api-key"
            | "x-api-token"
            | "x-auth-token"
            | "x-authentication-token"
            | "x-emby-auth"
            | "x-emby-auth-token"
            | "x-emby-authorization"
            | "x-emby-token"
            | "x-http-method"
            | "x-http-method-override"
            | "x-jellyfin-authorization"
            | "x-jellyfin-token"
            | "x-jellyfin-api-key"
            | "x-media-browser-authorization"
            | "x-media-browser-token"
            | "x-mediabrowser-authorization"
            | "x-mediabrowser-token"
            | "x-method-override"
            | "x-original-host"
            | "x-original-url"
            | "x-rewrite-url"
            | "x-session-token"
            | "x-token"
            | "jwt"
    ) || name.starts_with("proxy-")
        || name.starts_with("x-forwarded-")
        || name.starts_with("x-proxy-")
}

/// Validate uniqueness and aggregate bounds after individual header validation.
pub(crate) fn validate_header_set<I>(
    headers: I,
) -> Result<Vec<ExtraRequestHeader>, RequestHeadersError>
where
    I: IntoIterator<Item = ExtraRequestHeader>,
{
    let mut result = Vec::new();
    let mut names = std::collections::HashSet::new();
    let mut total_bytes = 0usize;
    for header in headers {
        if result.len() >= MAX_EXTRA_REQUEST_HEADER_COUNT {
            return Err(RequestHeadersError::TooManyHeaders);
        }
        total_bytes = total_bytes
            .saturating_add(header.name.as_str().len())
            .saturating_add(header.value.len());
        if total_bytes > MAX_EXTRA_REQUEST_HEADER_BYTES {
            return Err(RequestHeadersError::TooLarge);
        }
        if !names.insert(header.name.clone()) {
            return Err(RequestHeadersError::DuplicateName);
        }
        result.push(header);
    }
    Ok(result)
}
