//! Parse Kattri's deliberately narrow Jellyfin item-link format.
//!
//! Jellyfin Desktop does not define the cross-client format here. At upstream
//! revision `6ba5b9814f2d655b12b4e26b659817d7546c2cde`, `src/main.cpp` starts
//! the desktop shell and `src/ui/webview.qml` loads its configured
//! `mainWindow.webUrl`. The official cross-client discussion, Jellyfin Meta
//! issue #133, remains a proposal with unresolved format choices. Kattri's v1
//! format below is its own bounded intent format, not an upstream protocol.
//!
//! ```text
//! jellyfin://kattri/v1/item/<GUID>?server=<percent-encoded-http(s)-base-url>
//! ```
//!
//! It carries only one server base URL and one Jellyfin item GUID. It cannot
//! request playback, choose a user, or carry query parameters or credentials.
//! Links are capped at 8 KiB; decoded server URLs are capped at 2 KiB. Server
//! base paths use slash-separated ASCII unreserved segments, without dot
//! segments or percent escapes. GUIDs use 32 hexadecimal digits or canonical
//! 8-4-4-4-12 hexadecimal form. Plain HTTP follows `ServerEntry`'s existing
//! local/private-host policy.
//! Parsing creates no saved-server entry and performs no network or launch
//! action. The caller must retain the normal explicit connection policy.

use std::{
    fmt,
    net::{Ipv4Addr, Ipv6Addr},
};

use crate::servers::{ServerEntry, ServerId, ServerListError};
use url::Url;

/// Maximum full deep-link size, in bytes.
pub const MAX_DEEP_LINK_BYTES: usize = 8 * 1024;
/// Maximum decoded server base URL size, in bytes.
pub const MAX_DEEP_LINK_SERVER_URL_BYTES: usize = 2 * 1024;

const MAX_ENCODED_SERVER_URL_BYTES: usize = MAX_DEEP_LINK_SERVER_URL_BYTES * 3;
const DEEP_LINK_PREFIX: &str = "jellyfin://";
const DEEP_LINK_AUTHORITY: &str = "kattri";
const VALIDATION_SERVER_ID: &str = "00000000-0000-0000-0000-000000000000";

/// A validated request to open one item on a server selected by the link.
///
/// The server URL remains an untrusted candidate. `Debug` redacts both it and
/// the item ID, and parsing does not connect to or save the server.
#[derive(Clone, PartialEq, Eq)]
pub struct DeepLinkIntent {
    server_url: String,
    item_id: String,
}

impl DeepLinkIntent {
    /// Parse the Kattri v1 item format. Errors never contain the input link.
    pub fn parse(input: &str) -> Result<Self, DeepLinkError> {
        if input.is_empty() {
            return Err(DeepLinkError::Empty);
        }
        if input.len() > MAX_DEEP_LINK_BYTES {
            return Err(DeepLinkError::TooLong);
        }
        if !input.is_ascii()
            || input.chars().any(|character| {
                character.is_control() || character.is_whitespace() || character == '\\'
            })
        {
            return Err(DeepLinkError::InvalidCharacter);
        }

        let rest = input
            .strip_prefix(DEEP_LINK_PREFIX)
            .ok_or(DeepLinkError::InvalidScheme)?;
        if rest.contains('#') {
            return Err(DeepLinkError::InvalidQuery);
        }
        let (route, query) = rest.split_once('?').ok_or(DeepLinkError::MissingServer)?;
        if query.contains('?') || query.is_empty() {
            return Err(DeepLinkError::InvalidQuery);
        }

        let (authority, path) = route
            .split_once('/')
            .ok_or(DeepLinkError::InvalidAuthority)?;
        if authority != DEEP_LINK_AUTHORITY {
            return Err(DeepLinkError::InvalidAuthority);
        }

        let mut path_parts = path.split('/');
        match path_parts.next() {
            Some("v1") => {}
            _ => return Err(DeepLinkError::UnsupportedVersion),
        }
        if path_parts.next() != Some("item") {
            return Err(DeepLinkError::UnsupportedAction);
        }
        let item_id = canonical_item_id(path_parts.next().unwrap_or_default())?;
        if path_parts.next().is_some() {
            return Err(DeepLinkError::UnsupportedAction);
        }

        if query.contains('&') {
            return Err(DeepLinkError::UnsupportedParameter);
        }
        let (key, encoded_server_url) = query.split_once('=').ok_or(DeepLinkError::InvalidQuery)?;
        if key != "server" {
            return Err(DeepLinkError::UnsupportedParameter);
        }
        if encoded_server_url.is_empty() {
            return Err(DeepLinkError::MissingServer);
        }

        let decoded_server_url = decode_server_url(encoded_server_url)?;
        let server_url = canonical_server_url(&decoded_server_url)?;
        Ok(Self {
            server_url,
            item_id,
        })
    }

    /// Return the validated canonical server URL for explicit user review.
    /// Do not log it or treat it as trusted solely because parsing succeeded.
    pub fn server_url(&self) -> &str {
        &self.server_url
    }

    /// Return the normalized lowercase item GUID.
    pub fn item_id(&self) -> &str {
        &self.item_id
    }
}

impl fmt::Debug for DeepLinkIntent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DeepLinkIntent")
            .field("server_url", &"[REDACTED]")
            .field("item_id", &"[REDACTED]")
            .finish()
    }
}

/// Why a deep link was refused. Variants never include caller-supplied text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeepLinkError {
    Empty,
    TooLong,
    InvalidCharacter,
    InvalidScheme,
    InvalidAuthority,
    UnsupportedVersion,
    UnsupportedAction,
    InvalidItemId,
    MissingServer,
    InvalidQuery,
    UnsupportedParameter,
    InvalidServerEncoding,
    InvalidServerUrl,
    ServerUrlCredentials,
    ServerUrlHasQueryOrFragment,
    ServerUrlTraversal,
    InsecureRemoteHttp,
}

impl fmt::Display for DeepLinkError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::Empty => "Deep link is empty",
            Self::TooLong => "Deep link exceeds the size limit",
            Self::InvalidCharacter => "Deep link contains unsupported characters",
            Self::InvalidScheme => "Deep link must use the jellyfin scheme",
            Self::InvalidAuthority => "Deep link authority is invalid",
            Self::UnsupportedVersion => "Deep link version is unsupported",
            Self::UnsupportedAction => "Deep link action is unsupported",
            Self::InvalidItemId => "Deep link item ID is not a GUID",
            Self::MissingServer => "Deep link has no server URL",
            Self::InvalidQuery => "Deep link query is malformed",
            Self::UnsupportedParameter => "Deep link contains an unsupported parameter",
            Self::InvalidServerEncoding => "Deep link server URL encoding is invalid",
            Self::InvalidServerUrl => "Deep link server URL is invalid",
            Self::ServerUrlCredentials => "Deep link server URL cannot contain credentials",
            Self::ServerUrlHasQueryOrFragment => {
                "Deep link server URL cannot contain a query or fragment"
            }
            Self::ServerUrlTraversal => "Deep link server URL path contains traversal",
            Self::InsecureRemoteHttp => {
                "Plain HTTP to a remote server requires explicit connection policy"
            }
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for DeepLinkError {}

fn canonical_item_id(input: &str) -> Result<String, DeepLinkError> {
    let bytes = input.as_bytes();
    let compact = bytes.len() == 32 && bytes.iter().all(u8::is_ascii_hexdigit);
    let dashed = bytes.len() == 36
        && bytes.iter().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                *byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        });
    if !compact && !dashed {
        return Err(DeepLinkError::InvalidItemId);
    }
    Ok(input.to_ascii_lowercase())
}

fn decode_server_url(encoded: &str) -> Result<String, DeepLinkError> {
    if encoded.len() > MAX_ENCODED_SERVER_URL_BYTES {
        return Err(DeepLinkError::TooLong);
    }

    let bytes = encoded.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' => {
                let high = bytes
                    .get(index + 1)
                    .and_then(|byte| hex_value(*byte))
                    .ok_or(DeepLinkError::InvalidServerEncoding)?;
                let low = bytes
                    .get(index + 2)
                    .and_then(|byte| hex_value(*byte))
                    .ok_or(DeepLinkError::InvalidServerEncoding)?;
                decoded.push((high << 4) | low);
                index += 3;
            }
            byte if is_unreserved(byte) => {
                decoded.push(byte);
                index += 1;
            }
            _ => return Err(DeepLinkError::InvalidServerEncoding),
        }
    }

    String::from_utf8(decoded).map_err(|_| DeepLinkError::InvalidServerEncoding)
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn is_unreserved(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~')
}

fn canonical_server_url(input: &str) -> Result<String, DeepLinkError> {
    if input.len() > MAX_DEEP_LINK_SERVER_URL_BYTES {
        return Err(DeepLinkError::TooLong);
    }
    if input
        .chars()
        .any(|character| character.is_control() || character.is_whitespace() || character == '\\')
    {
        return Err(DeepLinkError::InvalidServerUrl);
    }
    if input.contains('?') || input.contains('#') {
        return Err(DeepLinkError::ServerUrlHasQueryOrFragment);
    }

    let parsed = Url::parse(input).map_err(|_| DeepLinkError::InvalidServerUrl)?;
    if !matches!(parsed.scheme(), "http" | "https")
        || parsed.host_str().is_none()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
        || parsed.port() == Some(0)
    {
        return Err(DeepLinkError::InvalidServerUrl);
    }

    let (_, rest) = input
        .split_once("://")
        .ok_or(DeepLinkError::InvalidServerUrl)?;
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    // URL parsing normalizes shorthand IPs and dot segments, so check those
    // source components before using its canonical form.
    validate_raw_authority(authority, &parsed)?;
    validate_raw_base_path(&rest[authority_end..])?;
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(DeepLinkError::ServerUrlCredentials);
    }

    // Reuse the existing base-URL and HTTP policy without returning or saving
    // a synthetic server entry.
    let validation_id =
        ServerId::parse(VALIDATION_SERVER_ID).map_err(|_| DeepLinkError::InvalidServerUrl)?;
    let entry = ServerEntry::new(validation_id, "Deep link", parsed.as_str(), false)
        .map_err(map_server_error)?;
    Ok(entry.base_url().to_owned())
}

fn validate_raw_authority(authority: &str, parsed: &Url) -> Result<(), DeepLinkError> {
    if authority.is_empty() {
        return Err(DeepLinkError::InvalidAuthority);
    }
    if authority.contains('@') {
        return Err(DeepLinkError::ServerUrlCredentials);
    }
    if authority.contains('%') {
        return Err(DeepLinkError::InvalidAuthority);
    }

    let raw_host = if authority.starts_with('[') {
        authority
            .strip_prefix('[')
            .and_then(|value| value.split_once(']'))
            .map(|(host, _)| host)
            .ok_or(DeepLinkError::InvalidAuthority)?
    } else {
        authority.split(':').next().unwrap_or_default()
    };
    if raw_host.is_empty() {
        return Err(DeepLinkError::InvalidAuthority);
    }

    let raw_port = if authority.starts_with('[') {
        authority
            .split_once(']')
            .and_then(|(_, suffix)| suffix.strip_prefix(':'))
    } else {
        authority.split_once(':').map(|(_, port)| port)
    };
    if raw_port.is_some_and(|port| port.len() > 1 && port.starts_with('0')) {
        return Err(DeepLinkError::InvalidAuthority);
    }

    if !authority.starts_with('[') {
        let non_trailing_dot_host = raw_host.strip_suffix('.').unwrap_or(raw_host);
        if let Ok(address) = non_trailing_dot_host.parse::<Ipv4Addr>() {
            if address.is_unspecified()
                || address.is_broadcast()
                || address.is_multicast()
                || raw_host != address.to_string()
            {
                return Err(DeepLinkError::InvalidAuthority);
            }
        } else if ambiguous_numeric_host(raw_host) || ambiguous_numeric_host(non_trailing_dot_host)
        {
            return Err(DeepLinkError::InvalidAuthority);
        }
    }

    if let Some(host) = parsed.host_str() {
        let unbracketed_host = host
            .strip_prefix('[')
            .and_then(|value| value.strip_suffix(']'))
            .unwrap_or(host);
        if let Ok(address) = unbracketed_host.parse::<Ipv4Addr>() {
            if address.is_unspecified() || address.is_broadcast() || address.is_multicast() {
                return Err(DeepLinkError::InvalidAuthority);
            }
        } else if let Ok(address) = unbracketed_host.parse::<Ipv6Addr>()
            && (address.is_unspecified() || address.is_multicast())
        {
            return Err(DeepLinkError::InvalidAuthority);
        }
    }
    Ok(())
}

fn ambiguous_numeric_host(host: &str) -> bool {
    let final_label = host.rsplit('.').next().unwrap_or_default();
    if final_label.is_empty() {
        return false;
    }
    if final_label.bytes().all(|byte| byte.is_ascii_digit()) {
        return true;
    }
    if let Some(hex) = final_label
        .strip_prefix("0x")
        .or_else(|| final_label.strip_prefix("0X"))
    {
        return hex.is_empty() || hex.bytes().all(|byte| byte.is_ascii_hexdigit());
    }
    false
}

fn validate_raw_base_path(path: &str) -> Result<(), DeepLinkError> {
    if path.is_empty() {
        return Ok(());
    }
    let path = path
        .strip_prefix('/')
        .ok_or(DeepLinkError::InvalidServerUrl)?;
    if path.is_empty() {
        return Ok(());
    }
    let path = if path.ends_with('/') {
        let without_trailing_slash = path
            .strip_suffix('/')
            .ok_or(DeepLinkError::InvalidServerUrl)?;
        if without_trailing_slash.is_empty() || without_trailing_slash.ends_with('/') {
            return Err(DeepLinkError::InvalidServerUrl);
        }
        without_trailing_slash
    } else {
        path
    };

    for segment in path.split('/') {
        if segment == "." || segment == ".." {
            return Err(DeepLinkError::ServerUrlTraversal);
        }
        if segment.is_empty() || !segment.bytes().all(is_unreserved) {
            return Err(DeepLinkError::InvalidServerUrl);
        }
    }
    Ok(())
}

fn map_server_error(error: ServerListError) -> DeepLinkError {
    match error {
        ServerListError::InsecureRemoteHttp => DeepLinkError::InsecureRemoteHttp,
        ServerListError::BaseUrlHasCredentials => DeepLinkError::ServerUrlCredentials,
        ServerListError::BaseUrlHasQueryOrFragment => DeepLinkError::ServerUrlHasQueryOrFragment,
        _ => DeepLinkError::InvalidServerUrl,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DeepLinkError, DeepLinkIntent, MAX_DEEP_LINK_BYTES, MAX_DEEP_LINK_SERVER_URL_BYTES,
    };

    const ITEM_ID: &str = "0123456789ABCDEF0123456789ABCDEF";

    fn encode_url(url: &str) -> String {
        let mut encoded = String::with_capacity(url.len() * 3);
        for byte in url.bytes() {
            if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
                encoded.push(char::from(byte));
            } else {
                encoded.push('%');
                encoded.push(char::from(b"0123456789ABCDEF"[(byte >> 4) as usize]));
                encoded.push(char::from(b"0123456789ABCDEF"[(byte & 0x0f) as usize]));
            }
        }
        encoded
    }

    fn link(item_id: &str, server_url: &str) -> String {
        format!(
            "jellyfin://kattri/v1/item/{item_id}?server={}",
            encode_url(server_url)
        )
    }

    #[test]
    fn parses_canonical_item_intent_and_redacts_debug() {
        let intent = DeepLinkIntent::parse(&link(ITEM_ID, "https://media.example:443/jellyfin/"))
            .expect("valid item link");

        assert_eq!(intent.item_id(), "0123456789abcdef0123456789abcdef");
        assert_eq!(intent.server_url(), "https://media.example/jellyfin");
        let debug = format!("{intent:?}");
        assert!(!debug.contains("media.example"));
        assert!(!debug.contains(&ITEM_ID.to_ascii_lowercase()));
    }

    #[test]
    fn accepts_a_well_formed_dashed_guid() {
        let intent = DeepLinkIntent::parse(&link(
            "01234567-89AB-CDEF-0123-456789ABCDEF",
            "https://media.example",
        ))
        .expect("valid dashed GUID");
        assert_eq!(intent.item_id(), "01234567-89ab-cdef-0123-456789abcdef");
    }

    #[test]
    fn accepts_a_bracketed_ipv6_server_origin() {
        let intent = DeepLinkIntent::parse(&link(ITEM_ID, "https://[2001:db8::1]:8443/jellyfin"))
            .expect("valid IPv6 URL");
        assert_eq!(intent.server_url(), "https://[2001:db8::1]:8443/jellyfin");
    }

    #[test]
    fn rejects_unsupported_actions_and_parameters() {
        let server = encode_url("https://media.example");
        for input in [
            format!("jellyfin://kattri/v1/play/{ITEM_ID}?server={server}"),
            format!("jellyfin://kattri/v1/server/01234567?server={server}"),
            format!("jellyfin://kattri/v1/item/{ITEM_ID}?server={server}&play=true"),
            format!("jellyfin://kattri/v1/item/{ITEM_ID}?credential=fixture"),
        ] {
            assert!(DeepLinkIntent::parse(&input).is_err());
        }
    }

    #[test]
    fn rejects_ambiguous_outer_authorities_and_item_paths() {
        let server = encode_url("https://media.example");
        for input in [
            format!("jellyfin://kattri:443/v1/item/{ITEM_ID}?server={server}"),
            format!("jellyfin://user@kattri/v1/item/{ITEM_ID}?server={server}"),
            format!("jellyfin:///v1/item/{ITEM_ID}?server={server}"),
            format!("jellyfin://kattri/v1/item/../{ITEM_ID}?server={server}"),
            format!("jellyfin://kattri/v1/item/{ITEM_ID}/extra?server={server}"),
        ] {
            assert!(DeepLinkIntent::parse(&input).is_err());
        }
    }

    #[test]
    fn rejects_invalid_item_ids_and_versions() {
        let server = encode_url("https://media.example");
        for input in [
            format!("jellyfin://kattri/v2/item/{ITEM_ID}?server={server}"),
            format!("jellyfin://kattri/v1/item/not-a-guid?server={server}"),
            format!("jellyfin://kattri/v1/item/%2E%2E?server={server}"),
        ] {
            assert!(DeepLinkIntent::parse(&input).is_err());
        }
    }

    #[test]
    fn rejects_credentials_queries_fragments_and_unsafe_schemes() {
        for (server, expected) in [
            (
                "https://user:password@media.example",
                DeepLinkError::ServerUrlCredentials,
            ),
            (
                "https://@media.example",
                DeepLinkError::ServerUrlCredentials,
            ),
            (
                "https://media.example/?credential=fixture",
                DeepLinkError::ServerUrlHasQueryOrFragment,
            ),
            (
                "https://media.example/#fragment",
                DeepLinkError::ServerUrlHasQueryOrFragment,
            ),
            ("file://media.example/path", DeepLinkError::InvalidServerUrl),
        ] {
            assert_eq!(DeepLinkIntent::parse(&link(ITEM_ID, server)), Err(expected));
        }
    }

    #[test]
    fn rejects_traversal_and_ambiguous_numeric_authorities() {
        for server in [
            "https://media.example/media/../admin",
            "https://media.example/media/%2e%2e/admin",
            "https://127.1",
            "https://0x7f000001",
            "https://192.168.001.1",
            "https://media.example:08096",
            "https://media.example//jellyfin",
            "https://media.example/jellyfin//",
        ] {
            assert!(DeepLinkIntent::parse(&link(ITEM_ID, server)).is_err());
        }
    }

    #[test]
    fn preserves_the_remote_http_connection_policy() {
        assert_eq!(
            DeepLinkIntent::parse(&link(ITEM_ID, "http://media.example")),
            Err(DeepLinkError::InsecureRemoteHttp)
        );
        let local = DeepLinkIntent::parse(&link(ITEM_ID, "http://192.168.1.4:8096/jellyfin"))
            .expect("private HTTP candidate is syntactically valid");
        assert_eq!(local.server_url(), "http://192.168.1.4:8096/jellyfin");
    }

    #[test]
    fn rejects_oversized_inputs_and_malformed_percent_encoding() {
        let oversized = "x".repeat(MAX_DEEP_LINK_BYTES + 1);
        assert_eq!(
            DeepLinkIntent::parse(&oversized),
            Err(DeepLinkError::TooLong)
        );
        let too_long_url = "a".repeat(MAX_DEEP_LINK_SERVER_URL_BYTES + 1);
        assert_eq!(
            DeepLinkIntent::parse(&link(ITEM_ID, &too_long_url)),
            Err(DeepLinkError::TooLong)
        );
        assert_eq!(
            DeepLinkIntent::parse(&format!(
                "jellyfin://kattri/v1/item/{ITEM_ID}?server=https%3A%2F%2Fmedia.example%2"
            )),
            Err(DeepLinkError::InvalidServerEncoding)
        );
    }
}
