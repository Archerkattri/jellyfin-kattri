//! Stream URL input validation (M6-15). Pure: checks text the user typed. No network, no player
//! calls, and no I/O.
//!
//! Rules:
//! - At most [`MAX_URL_BYTES`] bytes. Surrounding whitespace is trimmed. Whitespace, control
//!   characters, and backslashes inside the URL are refused.
//! - Scheme allowlist: `https`. `http` is allowed only when the host is local or private.
//! - Userinfo (`user:pass@host`) is refused, not stripped.
//! - The path, query, and fragment must not carry a credential-like query key (checked with
//!   `kattri_guard::is_secret_like_key`) or a credential-like or URL-like value
//!   (`kattri_guard::is_secret_like_value`).
//! - [`StreamUrl`] never prints its full text. Its `Debug` and [`StreamUrl::redacted`] show the
//!   scheme, host, and port only. Errors never echo the input.
//!
//! The local-host rule mirrors `servers.rs` and `jellyfin-api`'s `ClientConfig::new`. Those
//! helpers are private, so the rule is repeated here. Keep them in step.

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use kattri_guard::{has_scheme_separator, is_secret_like_key, is_secret_like_value};

/// Longest accepted input, in bytes, after trimming.
pub const MAX_URL_BYTES: usize = 2048;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamUrlError {
    /// Nothing but whitespace was entered.
    Empty,
    /// Longer than [`MAX_URL_BYTES`].
    TooLong,
    /// Whitespace, a control character, or a backslash inside the URL.
    InvalidCharacter,
    /// No `scheme://` prefix, or the authority is malformed.
    InvalidUrl,
    /// A scheme other than `https` or `http`, such as `file`, `rtsp`, or `javascript`.
    UnsupportedScheme,
    /// The URL carries `user:pass@` or `user@`.
    Userinfo,
    /// The host is missing or not a valid hostname or IP address.
    InvalidHost,
    /// The port is missing digits, out of range, or zero.
    InvalidPort,
    /// Plain `http` to a public host.
    InsecureRemoteHttp,
    /// A query key or a path or query value is credential-like.
    CredentialParameter,
    /// A query or path contains another URL.
    UrlInParameter,
}

impl fmt::Display for StreamUrlError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::Empty => "Enter a stream address",
            Self::TooLong => "The stream address is too long",
            Self::InvalidCharacter => "The stream address contains a space or control character",
            Self::InvalidUrl => "The stream address is not a valid URL",
            Self::UnsupportedScheme => "Only https and http stream addresses are supported",
            Self::Userinfo => "Stream addresses cannot contain a user name or password",
            Self::InvalidHost => "The stream address has no valid host",
            Self::InvalidPort => "The stream address has an invalid port",
            Self::InsecureRemoteHttp => "Plain http is allowed only for private or local addresses",
            Self::CredentialParameter => "The stream address contains a credential parameter",
            Self::UrlInParameter => "The stream address contains another URL",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for StreamUrlError {}

/// A stream address that passed every rule above. The text is normalized: the scheme and host are
/// lowercase, and surrounding whitespace is gone.
#[derive(Clone, PartialEq, Eq)]
pub struct StreamUrl {
    scheme: &'static str,
    host: String,
    port: Option<u16>,
    canonical: String,
}

impl StreamUrl {
    pub fn parse(input: &str) -> Result<Self, StreamUrlError> {
        if input.len() > MAX_URL_BYTES {
            return Err(StreamUrlError::TooLong);
        }
        let text = input.trim();
        if text.is_empty() {
            return Err(StreamUrlError::Empty);
        }
        if text
            .chars()
            .any(|ch| ch.is_control() || ch.is_whitespace() || ch == '\\')
        {
            return Err(StreamUrlError::InvalidCharacter);
        }

        let (scheme_text, rest) = text.split_once("://").ok_or(StreamUrlError::InvalidUrl)?;
        let (scheme, local_only) = match scheme_text.to_ascii_lowercase().as_str() {
            "https" => ("https", false),
            "http" => ("http", true),
            _ => return Err(StreamUrlError::UnsupportedScheme),
        };

        let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        let (authority, remainder) = rest.split_at(authority_end);
        if authority.contains('@') {
            return Err(StreamUrlError::Userinfo);
        }
        let (host, port) = parse_authority(authority)?;
        if local_only && !is_local_host(&host) {
            return Err(StreamUrlError::InsecureRemoteHttp);
        }
        check_remainder(remainder)?;

        let port_text = port.map_or_else(String::new, |port| format!(":{port}"));
        let canonical = format!("{scheme}://{}{port_text}{remainder}", host.text);
        Ok(Self {
            scheme,
            host: host.text,
            port,
            canonical,
        })
    }

    /// The normalized URL, for the player only. Never log it. Use [`StreamUrl::redacted`].
    pub fn as_str(&self) -> &str {
        &self.canonical
    }

    pub fn scheme(&self) -> &'static str {
        self.scheme
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn port(&self) -> Option<u16> {
        self.port
    }

    /// `scheme://host[:port]/...`, with the path, query, and fragment removed. Safe for logs.
    pub fn redacted(&self) -> String {
        let port_text = self
            .port
            .map_or_else(String::new, |port| format!(":{port}"));
        format!("{}://{}{port_text}/…", self.scheme, self.host)
    }
}

impl fmt::Debug for StreamUrl {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "StreamUrl({})", self.redacted())
    }
}

struct Host {
    text: String,
    ip: Option<IpAddr>,
}

fn parse_authority(authority: &str) -> Result<(Host, Option<u16>), StreamUrlError> {
    use StreamUrlError as E;
    if authority.is_empty() {
        return Err(E::InvalidHost);
    }
    if let Some(after_bracket) = authority.strip_prefix('[') {
        let (inside, after) = after_bracket.split_once(']').ok_or(E::InvalidHost)?;
        let ip: Ipv6Addr = inside.parse().map_err(|_| E::InvalidHost)?;
        let port = if after.is_empty() {
            None
        } else {
            let digits = after.strip_prefix(':').ok_or(E::InvalidPort)?;
            Some(parse_port(digits)?)
        };
        return Ok((
            Host {
                text: format!("[{ip}]"),
                ip: Some(IpAddr::V6(ip)),
            },
            port,
        ));
    }

    let (host, port) = match authority.split_once(':') {
        Some((host, port)) => (host, Some(parse_port(port)?)),
        None => (authority, None),
    };
    if host.is_empty() {
        return Err(E::InvalidHost);
    }
    if let Ok(ip) = host.parse::<Ipv4Addr>() {
        return Ok((
            Host {
                text: ip.to_string(),
                ip: Some(IpAddr::V4(ip)),
            },
            port,
        ));
    }
    let lower = host.to_ascii_lowercase();
    if !valid_hostname(&lower) {
        return Err(E::InvalidHost);
    }
    Ok((
        Host {
            text: lower,
            ip: None,
        },
        port,
    ))
}

fn parse_port(text: &str) -> Result<u16, StreamUrlError> {
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(StreamUrlError::InvalidPort);
    }
    text.parse::<u16>()
        .ok()
        .filter(|port| *port != 0)
        .ok_or(StreamUrlError::InvalidPort)
}

fn valid_hostname(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= 253
        && host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
}

/// Same rule as `servers.rs`: localhost names, `.localhost` and `.local` names, and loopback,
/// private, or link-local addresses.
fn is_local_host(host: &Host) -> bool {
    if host.text == "localhost"
        || host.text.ends_with(".localhost")
        || host.text.ends_with(".local")
    {
        return true;
    }
    match host.ip {
        Some(IpAddr::V4(address)) => {
            address.is_loopback() || address.is_private() || address.is_link_local()
        }
        Some(IpAddr::V6(address)) => {
            address.is_loopback()
                || address.is_unicast_link_local()
                || (address.segments()[0] & 0xfe00) == 0xfc00
        }
        None => false,
    }
}

/// Check everything after the authority: path, query, and fragment.
fn check_remainder(remainder: &str) -> Result<(), StreamUrlError> {
    if has_scheme_separator(remainder) {
        return Err(StreamUrlError::UrlInParameter);
    }
    if is_secret_like_value(remainder) {
        return Err(StreamUrlError::CredentialParameter);
    }
    let query = remainder
        .split_once('?')
        .map(|(_, after)| after.split('#').next().unwrap_or_default())
        .unwrap_or_default();
    for pair in query.split('&').filter(|pair| !pair.is_empty()) {
        let key = pair.split_once('=').map_or(pair, |(key, _)| key);
        if is_secret_like_key(key) {
            return Err(StreamUrlError::CredentialParameter);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{MAX_URL_BYTES, StreamUrl, StreamUrlError};

    fn parse(text: &str) -> Result<StreamUrl, StreamUrlError> {
        StreamUrl::parse(text)
    }

    #[test]
    fn https_urls_are_accepted_and_normalized() {
        let url = parse("  HTTPS://Media.Example:8920/Videos/1/stream?static=true  ").expect("ok");
        assert_eq!(url.scheme(), "https");
        assert_eq!(url.host(), "media.example");
        assert_eq!(url.port(), Some(8920));
        assert_eq!(
            url.as_str(),
            "https://media.example:8920/Videos/1/stream?static=true"
        );
    }

    #[test]
    fn plain_http_is_accepted_only_for_local_and_private_hosts() {
        for ok in [
            "http://localhost:8096/",
            "http://127.0.0.1:8096/",
            "http://10.0.0.5:8096/",
            "http://172.20.1.9/",
            "http://192.168.1.20:8096/",
            "http://169.254.10.10/",
            "http://jellyfin.local/",
            "http://app.localhost/",
            "http://[::1]:8096/",
            "http://[fd00::5]/",
            "http://[fe80::1]/",
        ] {
            assert!(parse(ok).is_ok(), "{ok} should be accepted");
        }
        for refused in [
            "http://media.example/",
            "http://8.8.8.8/",
            "http://172.32.0.1/",
            "http://[2001:db8::1]/",
            "http://jellyfin/",
        ] {
            assert_eq!(
                parse(refused),
                Err(StreamUrlError::InsecureRemoteHttp),
                "{refused}"
            );
        }
    }

    #[test]
    fn only_http_and_https_are_in_the_allowlist() {
        for refused in [
            "file:///etc/passwd",
            "rtsp://192.168.1.5/live",
            "rtmp://192.168.1.5/live",
            "ftp://192.168.1.5/",
            "javascript:alert(1)",
            "mailto:someone",
        ] {
            assert!(parse(refused).is_err(), "{refused} should be refused");
        }
        assert_eq!(
            parse("file:///etc/passwd"),
            Err(StreamUrlError::UnsupportedScheme)
        );
        assert_eq!(parse("mailto:someone"), Err(StreamUrlError::InvalidUrl));
    }

    #[test]
    fn userinfo_is_refused_not_stripped() {
        assert_eq!(
            parse("https://user:pass@media.example/"),
            Err(StreamUrlError::Userinfo)
        );
        assert_eq!(
            parse("http://user@192.168.1.5/"),
            Err(StreamUrlError::Userinfo)
        );
    }

    #[test]
    fn length_is_capped() {
        let prefix = "https://media.example/";
        let at_cap = format!("{prefix}{}", "a".repeat(MAX_URL_BYTES - prefix.len()));
        assert!(parse(&at_cap).is_ok());
        let over = format!("{at_cap}a");
        assert_eq!(parse(&over), Err(StreamUrlError::TooLong));
    }

    #[test]
    fn empty_and_whitespace_inside_are_refused() {
        assert_eq!(parse("   "), Err(StreamUrlError::Empty));
        assert_eq!(
            parse("https://media.example/a b"),
            Err(StreamUrlError::InvalidCharacter)
        );
        assert_eq!(
            parse("https://media.example/a\tb"),
            Err(StreamUrlError::InvalidCharacter)
        );
        assert_eq!(
            parse("https://media.example\\x"),
            Err(StreamUrlError::InvalidCharacter)
        );
    }

    #[test]
    fn hosts_and_ports_are_validated() {
        assert_eq!(parse("https://"), Err(StreamUrlError::InvalidHost));
        assert_eq!(parse("https://:443/"), Err(StreamUrlError::InvalidHost));
        assert_eq!(
            parse("https://-bad.example/"),
            Err(StreamUrlError::InvalidHost)
        );
        assert_eq!(
            parse("https://media.example:0/"),
            Err(StreamUrlError::InvalidPort)
        );
        assert_eq!(
            parse("https://media.example:70000/"),
            Err(StreamUrlError::InvalidPort)
        );
        assert_eq!(
            parse("https://media.example:abc/"),
            Err(StreamUrlError::InvalidPort)
        );
        assert_eq!(
            parse("https://media.example:/"),
            Err(StreamUrlError::InvalidPort)
        );
        assert!(parse("https://media.example:443/").is_ok());
    }

    #[test]
    fn credential_query_keys_are_refused() {
        for refused in [
            "https://media.example/stream?api_key=abc",
            "https://media.example/stream?ApiKey=abc",
            "https://media.example/stream?access_token=abc",
            "https://media.example/stream?X-Emby-Token=abc",
            "https://media.example/stream?password=abc",
            "https://media.example/stream?volume=1&auth=abc",
        ] {
            assert_eq!(
                parse(refused),
                Err(StreamUrlError::CredentialParameter),
                "{refused}"
            );
        }
        assert!(parse("https://media.example/stream?static=true&start=10").is_ok());
    }

    #[test]
    fn credentials_and_urls_in_the_path_or_query_values_are_refused() {
        assert_eq!(
            parse("https://media.example/Videos/1?token=abc"),
            Err(StreamUrlError::CredentialParameter)
        );
        assert_eq!(
            parse("https://media.example/redirect?to=https://other.example/x"),
            Err(StreamUrlError::UrlInParameter)
        );
    }

    #[test]
    fn redacted_form_and_debug_hide_path_and_query() {
        let url = parse("https://media.example:8920/Videos/1/stream?static=true").expect("ok");
        let redacted = url.redacted();
        assert_eq!(redacted, "https://media.example:8920/…");
        let debug = format!("{url:?}");
        assert!(!debug.contains("Videos"));
        assert!(!debug.contains("static"));
    }

    #[test]
    fn errors_never_echo_the_input() {
        let secret_input = "https://user:hunter2@media.example/Videos?api_key=zz";
        let error = parse(secret_input).expect_err("refused");
        let message = error.to_string();
        assert!(!message.contains("hunter2"));
        assert!(!message.contains("zz"));
        assert!(!message.contains("media.example"));
    }
}
