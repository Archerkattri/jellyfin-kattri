//! Cast discovery spike: the SSDP M-SEARCH builder and the response parser for UPnP and DLNA
//! devices, plus the Chromecast discovery service name.
//!
//! This module is code only. Nothing here opens a socket or contacts a device. The byte
//! formats are tested; a live search is a later step that needs a device on the network.
//!
//! Chromecast devices are not found over SSDP. They advertise the mDNS service
//! [`CHROMECAST_MDNS_SERVICE`], which a later discovery step must browse.

use std::collections::BTreeMap;

use crate::error::PluginError;

/// SSDP multicast group and port used for M-SEARCH.
pub const SSDP_MULTICAST_ADDR: &str = "239.255.255.250:1900";
/// Search target for UPnP media renderers (DLNA receivers).
pub const MEDIA_RENDERER_SEARCH_TARGET: &str = "urn:schemas-upnp-org:device:MediaRenderer:1";
/// Search target for UPnP media servers (DLNA sources).
pub const MEDIA_SERVER_SEARCH_TARGET: &str = "urn:schemas-upnp-org:device:MediaServer:1";
/// mDNS service type that Chromecast devices advertise. Browsed over mDNS, not SSDP.
pub const CHROMECAST_MDNS_SERVICE: &str = "_googlecast._tcp";
/// Largest SSDP response accepted, in bytes. Larger input is refused before parsing.
pub const MAX_RESPONSE_BYTES: usize = 4096;
/// Longest search target accepted, in bytes.
const MAX_SEARCH_TARGET_BYTES: usize = 128;
/// Longest LOCATION URL accepted, in bytes.
const MAX_LOCATION_BYTES: usize = 512;

/// The fields of an SSDP search response that a discovery step needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SsdpResponse {
    /// URL of the device description document. Not fetched by this module.
    pub location: String,
    /// The search target the device answered.
    pub search_target: String,
    /// Unique service name, for example `uuid:...::urn:...`.
    pub usn: String,
    /// Server header, when the device sends one.
    pub server: Option<String>,
}

/// Build an M-SEARCH request for `search_target`. `mx_seconds` is the response window, 1 to 5.
pub fn build_m_search(search_target: &str, mx_seconds: u8) -> Result<Vec<u8>, PluginError> {
    if !(1..=5).contains(&mx_seconds) {
        return Err(PluginError::Invalid {
            field: "ssdp.mx",
            reason: "must be between 1 and 5 seconds".to_owned(),
        });
    }
    let target_ok = !search_target.is_empty()
        && search_target.len() <= MAX_SEARCH_TARGET_BYTES
        && search_target
            .bytes()
            .all(|b| b.is_ascii_graphic() || b == b' ');
    if !target_ok {
        return Err(PluginError::Invalid {
            field: "ssdp.search_target",
            reason: "must be 1 to 128 printable ASCII characters without line breaks".to_owned(),
        });
    }
    let request = format!(
        "M-SEARCH * HTTP/1.1\r\n\
         HOST: {SSDP_MULTICAST_ADDR}\r\n\
         MAN: \"ssdp:discover\"\r\n\
         MX: {mx_seconds}\r\n\
         ST: {search_target}\r\n\
         \r\n"
    );
    Ok(request.into_bytes())
}

/// Parse a search response datagram.
///
/// The status line must be `HTTP/1.1 200 OK`. Header names are matched without regard to
/// case. `LOCATION`, `ST` and `USN` are required, and `LOCATION` must be an http or https URL.
/// Parsing stops at the blank line that ends the headers.
pub fn parse_response(bytes: &[u8]) -> Result<SsdpResponse, PluginError> {
    if bytes.len() > MAX_RESPONSE_BYTES {
        return Err(invalid("ssdp.response", "is larger than 4 KiB"));
    }
    let text = std::str::from_utf8(bytes).map_err(|_| invalid("ssdp.response", "is not UTF-8"))?;
    let mut lines = text.split("\r\n");

    let status = lines.next().unwrap_or_default();
    if !status.eq_ignore_ascii_case("HTTP/1.1 200 OK") {
        return Err(invalid(
            "ssdp.response",
            "status line is not HTTP/1.1 200 OK",
        ));
    }

    let mut headers: BTreeMap<String, String> = BTreeMap::new();
    for line in lines {
        if line.is_empty() {
            break;
        }
        let Some((name, value)) = line.split_once(':') else {
            return Err(invalid("ssdp.response", "a header line has no colon"));
        };
        headers
            .entry(name.trim().to_ascii_lowercase())
            .or_insert_with(|| value.trim().to_owned());
    }

    let location = required(&headers, "location")?;
    let search_target = required(&headers, "st")?;
    let usn = required(&headers, "usn")?;
    if location.len() > MAX_LOCATION_BYTES {
        return Err(invalid("ssdp.location", "is longer than 512 bytes"));
    }
    if !(location.starts_with("http://") || location.starts_with("https://")) {
        return Err(invalid("ssdp.location", "must be an http or https URL"));
    }

    Ok(SsdpResponse {
        location,
        search_target,
        usn,
        server: headers.get("server").cloned(),
    })
}

fn required(headers: &BTreeMap<String, String>, name: &str) -> Result<String, PluginError> {
    match headers.get(name) {
        Some(value) if !value.is_empty() => Ok(value.clone()),
        _ => Err(PluginError::Invalid {
            field: "ssdp.response",
            reason: format!("missing required header {}", name.to_ascii_uppercase()),
        }),
    }
}

fn invalid(field: &'static str, reason: &str) -> PluginError {
    PluginError::Invalid {
        field,
        reason: reason.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_RESPONSE: &[u8] = b"HTTP/1.1 200 OK\r\n\
        CACHE-CONTROL: max-age=1800\r\n\
        location: http://192.168.1.20:49152/description.xml\r\n\
        ST: urn:schemas-upnp-org:device:MediaRenderer:1\r\n\
        USN: uuid:11111111-2222-3333-4444-555555555555::urn:schemas-upnp-org:device:MediaRenderer:1\r\n\
        SERVER: Linux/5.4 UPnP/1.0 Renderer/1.0\r\n\
        \r\n";

    #[test]
    fn m_search_bytes_match_the_ssdp_request_layout() {
        let bytes = build_m_search(MEDIA_RENDERER_SEARCH_TARGET, 2).expect("valid request");
        let text = String::from_utf8(bytes).expect("request is ASCII");
        assert_eq!(
            text,
            "M-SEARCH * HTTP/1.1\r\n\
             HOST: 239.255.255.250:1900\r\n\
             MAN: \"ssdp:discover\"\r\n\
             MX: 2\r\n\
             ST: urn:schemas-upnp-org:device:MediaRenderer:1\r\n\
             \r\n"
        );
    }

    #[test]
    fn m_search_refuses_an_out_of_range_window() {
        assert!(build_m_search(MEDIA_SERVER_SEARCH_TARGET, 0).is_err());
        assert!(build_m_search(MEDIA_SERVER_SEARCH_TARGET, 6).is_err());
        assert!(build_m_search(MEDIA_SERVER_SEARCH_TARGET, 5).is_ok());
    }

    #[test]
    fn m_search_refuses_header_injection_in_the_search_target() {
        assert!(build_m_search("ssdp:all\r\nHOST: evil", 2).is_err());
        assert!(build_m_search("ssdp:all\nX: y", 2).is_err());
        assert!(build_m_search("", 2).is_err());
        assert!(build_m_search(&"x".repeat(MAX_SEARCH_TARGET_BYTES + 1), 2).is_err());
    }

    #[test]
    fn parses_a_renderer_response() {
        let parsed = parse_response(SAMPLE_RESPONSE).expect("valid response");
        assert_eq!(parsed.location, "http://192.168.1.20:49152/description.xml");
        assert_eq!(
            parsed.search_target,
            "urn:schemas-upnp-org:device:MediaRenderer:1"
        );
        assert!(
            parsed
                .usn
                .starts_with("uuid:11111111-2222-3333-4444-555555555555::")
        );
        assert_eq!(
            parsed.server.as_deref(),
            Some("Linux/5.4 UPnP/1.0 Renderer/1.0")
        );
    }

    #[test]
    fn missing_location_is_refused() {
        let text = b"HTTP/1.1 200 OK\r\nST: ssdp:all\r\nUSN: uuid:x\r\n\r\n";
        assert!(parse_response(text).is_err());
    }

    #[test]
    fn non_200_status_is_refused() {
        let text = b"HTTP/1.1 404 Not Found\r\nLOCATION: http://h/d.xml\r\nST: a\r\nUSN: b\r\n\r\n";
        assert!(parse_response(text).is_err());
    }

    #[test]
    fn non_http_location_is_refused() {
        let text = b"HTTP/1.1 200 OK\r\nLOCATION: ftp://h/d.xml\r\nST: a\r\nUSN: b\r\n\r\n";
        assert!(parse_response(text).is_err());
    }

    #[test]
    fn oversize_and_non_utf8_responses_are_refused() {
        let big = vec![b'a'; MAX_RESPONSE_BYTES + 1];
        assert!(parse_response(&big).is_err());
        assert!(parse_response(b"HTTP/1.1 200 OK\r\n\xff\xfe\r\n\r\n").is_err());
    }

    #[test]
    fn header_without_colon_is_refused() {
        let text = b"HTTP/1.1 200 OK\r\nbroken line\r\nLOCATION: http://h/d.xml\r\nST: a\r\nUSN: b\r\n\r\n";
        assert!(parse_response(text).is_err());
    }

    #[test]
    fn chromecast_is_named_by_its_mdns_service_not_ssdp() {
        assert_eq!(CHROMECAST_MDNS_SERVICE, "_googlecast._tcp");
    }
}
