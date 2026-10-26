//! Saved server list model (M1-16). Pure: no filesystem, clock, or network.
//!
//! An entry holds only what M1-16 allows on disk: a local id, a display name,
//! a base URL without userinfo or query, and the insecure-HTTP opt-in. It never
//! holds a username, password, token, or secret. The caller owns the config
//! path, atomic writes, and moving a corrupt file aside; this module encodes and
//! decodes the versioned record text only.
//!
//! The URL rule mirrors `jellyfin-api`'s `ClientConfig::new` and its private
//! `is_local_host`. That crate does not export them, so the rule is repeated
//! here. Keep the two in step.

use std::fmt::{self, Write as _};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use kattri_guard::{has_scheme_separator, is_secret_like_value};

/// Version written to and required from the persisted record.
pub const SERVER_LIST_VERSION: u32 = 1;
/// Maximum number of saved servers.
pub const MAX_SERVERS: usize = 32;
/// Maximum display name length, counted in characters after trimming.
pub const MAX_DISPLAY_NAME_CHARS: usize = 64;
/// Maximum base URL length in bytes, before canonicalization.
pub const MAX_BASE_URL_BYTES: usize = 2048;
/// Maximum persisted record size in bytes.
pub const MAX_RECORD_BYTES: usize = 128 * 1024;
const MAX_JSON_DEPTH: usize = 8;

/// Why a server list operation or record decode was refused. Variants carry no
/// user data, so an error can never echo a URL that contains credentials.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServerListError {
    InvalidServerId,
    InvalidDisplayName,
    InvalidBaseUrl,
    BaseUrlHasCredentials,
    BaseUrlHasQueryOrFragment,
    InsecureRemoteHttp,
    DuplicateServerId,
    DuplicateBaseUrl,
    ListFull,
    NotFound,
    UnsupportedVersion,
    MalformedRecord,
    RecordTooLarge,
}

impl fmt::Display for ServerListError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::InvalidServerId => "Server id is not a valid GUID",
            Self::InvalidDisplayName => {
                "Server name must be 1 to 64 characters, with no control characters or URLs"
            }
            Self::InvalidBaseUrl => "Server address is not a valid http or https URL",
            Self::BaseUrlHasCredentials => "Server address must not contain a username or password",
            Self::BaseUrlHasQueryOrFragment => {
                "Server address must not contain a query or fragment"
            }
            Self::InsecureRemoteHttp => "Plain http is allowed only for private or local addresses",
            Self::DuplicateServerId => "Server id is already in the list",
            Self::DuplicateBaseUrl => "This server is already saved",
            Self::ListFull => "The server list is full",
            Self::NotFound => "Server is not in the list",
            Self::UnsupportedVersion => "Saved server list has an unsupported version",
            Self::MalformedRecord => "Saved server list is malformed",
            Self::RecordTooLarge => "Saved server list is too large",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for ServerListError {}

/// A server identifier in canonical lowercase 8-4-4-4-12 form.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ServerId(String);

impl ServerId {
    /// Parses a GUID in 8-4-4-4-12 hexadecimal form, in any letter case.
    pub fn parse(text: &str) -> Result<Self, ServerListError> {
        const GROUPS: [usize; 5] = [8, 4, 4, 4, 12];
        let bytes = text.as_bytes();
        if bytes.len() != 36 {
            return Err(ServerListError::InvalidServerId);
        }
        let mut canonical = String::with_capacity(36);
        let mut offset = 0;
        for (index, width) in GROUPS.iter().enumerate() {
            if index > 0 {
                if bytes.get(offset) != Some(&b'-') {
                    return Err(ServerListError::InvalidServerId);
                }
                canonical.push('-');
                offset += 1;
            }
            let group = bytes
                .get(offset..offset + *width)
                .ok_or(ServerListError::InvalidServerId)?;
            if !group.iter().all(u8::is_ascii_hexdigit) {
                return Err(ServerListError::InvalidServerId);
            }
            canonical.extend(
                group
                    .iter()
                    .map(|byte| char::from(byte.to_ascii_lowercase())),
            );
            offset += *width;
        }
        Ok(Self(canonical))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// One saved server. Fields are private so every entry passes [`ServerEntry::new`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerEntry {
    id: ServerId,
    display_name: String,
    base_url: String,
    allow_insecure_remote_http: bool,
}

impl ServerEntry {
    /// Validates the display name and base URL. Plain `http` is refused for a
    /// public host unless `allow_insecure_remote_http` is set.
    pub fn new(
        id: ServerId,
        display_name: &str,
        base_url: &str,
        allow_insecure_remote_http: bool,
    ) -> Result<Self, ServerListError> {
        Ok(Self {
            id,
            display_name: canonical_display_name(display_name)?,
            base_url: canonical_base_url(base_url, allow_insecure_remote_http)?,
            allow_insecure_remote_http,
        })
    }

    pub fn id(&self) -> &ServerId {
        &self.id
    }

    pub fn display_name(&self) -> &str {
        &self.display_name
    }

    /// Canonical base URL: lowercase scheme and host, default port removed, no
    /// trailing slash.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub fn allow_insecure_remote_http(&self) -> bool {
        self.allow_insecure_remote_http
    }
}

/// Ordered list of saved servers. Order is insertion order; removing or renaming
/// an entry keeps the relative order of the others.
///
/// Default rule: when the list is non-empty it always has a default. The first
/// added server becomes the default. Removing the default makes the first
/// remaining server the default. The list is empty only when it has no default.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ServerList {
    entries: Vec<ServerEntry>,
    default_id: Option<ServerId>,
}

impl ServerList {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn entries(&self) -> &[ServerEntry] {
        &self.entries
    }

    pub fn get(&self, id: &ServerId) -> Option<&ServerEntry> {
        self.entries.iter().find(|entry| &entry.id == id)
    }

    pub fn default_server(&self) -> Option<&ServerEntry> {
        self.default_id.as_ref().and_then(|id| self.get(id))
    }

    /// Appends an entry. Refuses a full list, a duplicate id, and a duplicate
    /// canonical base URL.
    pub fn add(&mut self, entry: ServerEntry) -> Result<(), ServerListError> {
        if self.entries.len() >= MAX_SERVERS {
            return Err(ServerListError::ListFull);
        }
        if self.entries.iter().any(|saved| saved.id == entry.id) {
            return Err(ServerListError::DuplicateServerId);
        }
        if self
            .entries
            .iter()
            .any(|saved| saved.base_url == entry.base_url)
        {
            return Err(ServerListError::DuplicateBaseUrl);
        }
        if self.default_id.is_none() {
            self.default_id = Some(entry.id.clone());
        }
        self.entries.push(entry);
        Ok(())
    }

    /// Removes an entry and returns it. If it was the default, the first
    /// remaining entry becomes the default.
    pub fn remove(&mut self, id: &ServerId) -> Result<ServerEntry, ServerListError> {
        let index = self.position(id)?;
        let removed = self.entries.remove(index);
        if self.default_id.as_ref() == Some(&removed.id) {
            self.default_id = self.entries.first().map(|entry| entry.id.clone());
        }
        Ok(removed)
    }

    pub fn rename(&mut self, id: &ServerId, display_name: &str) -> Result<(), ServerListError> {
        let index = self.position(id)?;
        let name = canonical_display_name(display_name)?;
        let entry = self
            .entries
            .get_mut(index)
            .ok_or(ServerListError::NotFound)?;
        entry.display_name = name;
        Ok(())
    }

    pub fn set_default(&mut self, id: &ServerId) -> Result<(), ServerListError> {
        self.position(id)?;
        self.default_id = Some(id.clone());
        Ok(())
    }

    fn position(&self, id: &ServerId) -> Result<usize, ServerListError> {
        self.entries
            .iter()
            .position(|entry| &entry.id == id)
            .ok_or(ServerListError::NotFound)
    }

    /// Encodes the list as the versioned JSON record. The output has only the
    /// keys in [`RECORD_KEYS`].
    pub fn encode(&self) -> String {
        let mut out = format!("{{\"version\":{SERVER_LIST_VERSION},\"default_id\":");
        match &self.default_id {
            Some(id) => write_json_string(&mut out, id.as_str()),
            None => out.push_str("null"),
        }
        out.push_str(",\"servers\":[");
        for (index, entry) in self.entries.iter().enumerate() {
            if index > 0 {
                out.push(',');
            }
            out.push_str("{\"id\":");
            write_json_string(&mut out, entry.id.as_str());
            out.push_str(",\"display_name\":");
            write_json_string(&mut out, &entry.display_name);
            out.push_str(",\"base_url\":");
            write_json_string(&mut out, &entry.base_url);
            out.push_str(",\"allow_insecure_remote_http\":");
            out.push_str(if entry.allow_insecure_remote_http {
                "true"
            } else {
                "false"
            });
            out.push('}');
        }
        out.push_str("]}");
        out
    }

    /// Decodes a record. Any unknown or missing key, duplicate key, bad URL,
    /// duplicate entry, or broken default makes the whole record invalid. The
    /// caller then moves the file aside and uses an empty list.
    pub fn decode(text: &str) -> Result<Self, ServerListError> {
        use ServerListError as E;
        if text.len() > MAX_RECORD_BYTES {
            return Err(E::RecordTooLarge);
        }
        let root = parse_json(text)?;
        let Json::Object(root) = root else {
            return Err(E::MalformedRecord);
        };
        match find_field(&root, "version")? {
            Json::Number(version) if *version == u64::from(SERVER_LIST_VERSION) => {}
            Json::Number(_) => return Err(E::UnsupportedVersion),
            _ => return Err(E::MalformedRecord),
        }
        check_keys(&root, &["version", "default_id", "servers"])?;

        let default_id = match find_field(&root, "default_id")? {
            Json::Null => None,
            Json::String(id) => Some(ServerId::parse(id)?),
            _ => return Err(E::MalformedRecord),
        };

        let Json::Array(items) = find_field(&root, "servers")? else {
            return Err(E::MalformedRecord);
        };
        let mut list = Self::new();
        for item in items {
            let Json::Object(members) = item else {
                return Err(E::MalformedRecord);
            };
            check_keys(
                members,
                &[
                    "id",
                    "display_name",
                    "base_url",
                    "allow_insecure_remote_http",
                ],
            )?;
            let id = ServerId::parse(json_str(members, "id")?)?;
            let allow = match find_field(members, "allow_insecure_remote_http")? {
                Json::Bool(value) => *value,
                _ => return Err(E::MalformedRecord),
            };
            let entry = ServerEntry::new(
                id,
                json_str(members, "display_name")?,
                json_str(members, "base_url")?,
                allow,
            )?;
            list.add(entry)?;
        }

        match (list.is_empty(), default_id) {
            (true, None) => {}
            (false, Some(id)) => list.set_default(&id).map_err(|_| E::MalformedRecord)?,
            _ => return Err(E::MalformedRecord),
        }
        Ok(list)
    }
}

fn canonical_display_name(input: &str) -> Result<String, ServerListError> {
    let trimmed = input.trim();
    if trimmed.is_empty()
        || trimmed.chars().count() > MAX_DISPLAY_NAME_CHARS
        || trimmed.chars().any(char::is_control)
        || is_secret_like_value(trimmed)
        || has_scheme_separator(trimmed)
    {
        return Err(ServerListError::InvalidDisplayName);
    }
    Ok(trimmed.to_owned())
}

fn canonical_base_url(
    input: &str,
    allow_insecure_remote_http: bool,
) -> Result<String, ServerListError> {
    use ServerListError as E;
    if input.len() > MAX_BASE_URL_BYTES {
        return Err(E::InvalidBaseUrl);
    }
    let text = input.trim();
    if text
        .chars()
        .any(|ch| ch.is_control() || ch.is_whitespace() || ch == '\\')
    {
        return Err(E::InvalidBaseUrl);
    }
    if text.contains('?') || text.contains('#') {
        return Err(E::BaseUrlHasQueryOrFragment);
    }
    let (scheme, rest) = text.split_once("://").ok_or(E::InvalidBaseUrl)?;
    let scheme = scheme.to_ascii_lowercase();
    let secure = match scheme.as_str() {
        "https" => true,
        "http" => false,
        _ => return Err(E::InvalidBaseUrl),
    };
    let (authority, path) = match rest.split_once('/') {
        Some((authority, path)) => (authority, path),
        None => (rest, ""),
    };
    if authority.contains('@') {
        return Err(E::BaseUrlHasCredentials);
    }
    let (host, port) = parse_authority(authority)?;
    if !secure && !allow_insecure_remote_http && !is_local_host(&host) {
        return Err(E::InsecureRemoteHttp);
    }

    let default_port = if secure { 443 } else { 80 };
    let port_text = match port {
        Some(port) if port != default_port => format!(":{port}"),
        _ => String::new(),
    };
    let path = path.trim_end_matches('/');
    let path_text = if path.is_empty() {
        String::new()
    } else {
        format!("/{path}")
    };
    Ok(format!("{scheme}://{}{port_text}{path_text}", host.text))
}

struct Host {
    text: String,
    ip: Option<IpAddr>,
}

fn parse_authority(authority: &str) -> Result<(Host, Option<u16>), ServerListError> {
    use ServerListError as E;
    if let Some(after_bracket) = authority.strip_prefix('[') {
        let (inside, after) = after_bracket.split_once(']').ok_or(E::InvalidBaseUrl)?;
        let ip: Ipv6Addr = inside.parse().map_err(|_| E::InvalidBaseUrl)?;
        let port = if after.is_empty() {
            None
        } else {
            let digits = after.strip_prefix(':').ok_or(E::InvalidBaseUrl)?;
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
        return Err(E::InvalidBaseUrl);
    }
    Ok((
        Host {
            text: lower,
            ip: None,
        },
        port,
    ))
}

fn parse_port(text: &str) -> Result<u16, ServerListError> {
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(ServerListError::InvalidBaseUrl);
    }
    text.parse::<u16>()
        .ok()
        .filter(|port| *port != 0)
        .ok_or(ServerListError::InvalidBaseUrl)
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

/// Same rule as `jellyfin-api`'s `is_local_host`: localhost names, `.localhost`
/// and `.local` names, and loopback, private, or link-local addresses.
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

fn find_field<'a>(members: &'a [(String, Json)], key: &str) -> Result<&'a Json, ServerListError> {
    members
        .iter()
        .find(|(name, _)| name == key)
        .map(|(_, value)| value)
        .ok_or(ServerListError::MalformedRecord)
}

fn json_str<'a>(members: &'a [(String, Json)], key: &str) -> Result<&'a str, ServerListError> {
    match find_field(members, key)? {
        Json::String(value) => Ok(value),
        _ => Err(ServerListError::MalformedRecord),
    }
}

/// Requires exactly the expected keys, each present once. Unknown keys are
/// refused, so a token or password written into the file makes it corrupt.
fn check_keys(members: &[(String, Json)], expected: &[&str]) -> Result<(), ServerListError> {
    let exact = members.len() == expected.len()
        && expected
            .iter()
            .all(|key| members.iter().filter(|(name, _)| name == key).count() == 1);
    if exact {
        Ok(())
    } else {
        Err(ServerListError::MalformedRecord)
    }
}

/// Keys written to the record. Used by the guard test.
pub const RECORD_KEYS: &[&str] = &[
    "version",
    "default_id",
    "servers",
    "id",
    "display_name",
    "base_url",
    "allow_insecure_remote_http",
];

fn write_json_string(out: &mut String, value: &str) {
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            ch if (ch as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", ch as u32);
            }
            ch => out.push(ch),
        }
    }
    out.push('"');
}

#[derive(Debug, PartialEq, Eq)]
enum Json {
    Null,
    Bool(bool),
    Number(u64),
    String(String),
    Array(Vec<Json>),
    Object(Vec<(String, Json)>),
}

fn parse_json(text: &str) -> Result<Json, ServerListError> {
    let mut parser = JsonParser {
        bytes: text.as_bytes(),
        pos: 0,
    };
    let value = parser.value(0)?;
    parser.skip_ws();
    if parser.pos != parser.bytes.len() {
        return Err(ServerListError::MalformedRecord);
    }
    Ok(value)
}

/// Minimal strict JSON reader for the record shape. It accepts only
/// non-negative integers and no fractions or exponents, and it bounds nesting.
struct JsonParser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl JsonParser<'_> {
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn bump(&mut self) -> Option<u8> {
        let byte = self.peek()?;
        self.pos += 1;
        Some(byte)
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.pos += 1;
        }
    }

    fn eat(&mut self, byte: u8) -> Result<(), ServerListError> {
        if self.bump() == Some(byte) {
            Ok(())
        } else {
            Err(ServerListError::MalformedRecord)
        }
    }

    fn literal(&mut self, word: &[u8]) -> Result<(), ServerListError> {
        let end = self.pos + word.len();
        if self.bytes.get(self.pos..end) == Some(word) {
            self.pos = end;
            Ok(())
        } else {
            Err(ServerListError::MalformedRecord)
        }
    }

    fn value(&mut self, depth: usize) -> Result<Json, ServerListError> {
        if depth > MAX_JSON_DEPTH {
            return Err(ServerListError::MalformedRecord);
        }
        self.skip_ws();
        match self.peek().ok_or(ServerListError::MalformedRecord)? {
            b'{' => self.object(depth + 1),
            b'[' => self.array(depth + 1),
            b'"' => self.string().map(Json::String),
            b't' => {
                self.literal(b"true")?;
                Ok(Json::Bool(true))
            }
            b'f' => {
                self.literal(b"false")?;
                Ok(Json::Bool(false))
            }
            b'n' => {
                self.literal(b"null")?;
                Ok(Json::Null)
            }
            b'0'..=b'9' => self.number(),
            _ => Err(ServerListError::MalformedRecord),
        }
    }

    fn object(&mut self, depth: usize) -> Result<Json, ServerListError> {
        self.eat(b'{')?;
        let mut members = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(Json::Object(members));
        }
        loop {
            self.skip_ws();
            let key = self.string()?;
            self.skip_ws();
            self.eat(b':')?;
            let value = self.value(depth)?;
            members.push((key, value));
            self.skip_ws();
            match self.bump() {
                Some(b',') => {}
                Some(b'}') => return Ok(Json::Object(members)),
                _ => return Err(ServerListError::MalformedRecord),
            }
        }
    }

    fn array(&mut self, depth: usize) -> Result<Json, ServerListError> {
        self.eat(b'[')?;
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(Json::Array(items));
        }
        loop {
            items.push(self.value(depth)?);
            self.skip_ws();
            match self.bump() {
                Some(b',') => {}
                Some(b']') => return Ok(Json::Array(items)),
                _ => return Err(ServerListError::MalformedRecord),
            }
        }
    }

    fn string(&mut self) -> Result<String, ServerListError> {
        self.eat(b'"')?;
        let mut out: Vec<u8> = Vec::new();
        loop {
            let byte = self.bump().ok_or(ServerListError::MalformedRecord)?;
            match byte {
                b'"' => break,
                b'\\' => {
                    let escaped = self.bump().ok_or(ServerListError::MalformedRecord)?;
                    match escaped {
                        b'"' | b'\\' | b'/' => out.push(escaped),
                        b'b' => out.push(0x08),
                        b'f' => out.push(0x0c),
                        b'n' => out.push(b'\n'),
                        b'r' => out.push(b'\r'),
                        b't' => out.push(b'\t'),
                        b'u' => {
                            let code = self.hex4()?;
                            let ch =
                                char::from_u32(code).ok_or(ServerListError::MalformedRecord)?;
                            let mut buf = [0u8; 4];
                            out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                        }
                        _ => return Err(ServerListError::MalformedRecord),
                    }
                }
                0x00..=0x1f => return Err(ServerListError::MalformedRecord),
                _ => out.push(byte),
            }
        }
        String::from_utf8(out).map_err(|_| ServerListError::MalformedRecord)
    }

    fn hex4(&mut self) -> Result<u32, ServerListError> {
        let mut value = 0u32;
        for _ in 0..4 {
            let byte = self.bump().ok_or(ServerListError::MalformedRecord)?;
            let digit = char::from(byte)
                .to_digit(16)
                .ok_or(ServerListError::MalformedRecord)?;
            value = value * 16 + digit;
        }
        Ok(value)
    }

    fn number(&mut self) -> Result<Json, ServerListError> {
        let start = self.pos;
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.pos += 1;
        }
        let digits = self
            .bytes
            .get(start..self.pos)
            .ok_or(ServerListError::MalformedRecord)?;
        let text = std::str::from_utf8(digits).map_err(|_| ServerListError::MalformedRecord)?;
        text.parse::<u64>()
            .map(Json::Number)
            .map_err(|_| ServerListError::MalformedRecord)
    }
}

#[cfg(test)]
mod tests {
    use kattri_guard::is_secret_like_key;

    use super::*;

    const GUID_A: &str = "11111111-2222-4333-8444-555555555555";
    const GUID_B: &str = "AAAAAAAA-BBBB-4CCC-8DDD-EEEEEEEEEEEE";
    const GUID_C: &str = "01234567-89ab-4def-8123-456789abcdef";

    fn entry(id: &str, name: &str, url: &str) -> Result<ServerEntry, ServerListError> {
        ServerEntry::new(ServerId::parse(id)?, name, url, false)
    }

    #[test]
    fn server_id_accepts_guid_in_any_case_and_stores_lowercase() -> Result<(), ServerListError> {
        let id = ServerId::parse(GUID_B)?;
        assert_eq!(id.as_str(), "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee");
        Ok(())
    }

    #[test]
    fn server_id_rejects_malformed_values() {
        for bad in [
            "",
            "11111111222243338444555555555555",
            "11111111-2222-4333-8444-55555555555",
            "11111111-2222-4333-8444-5555555555555",
            "1111111g-2222-4333-8444-555555555555",
            "11111111_2222-4333-8444-555555555555",
            "{11111111-2222-4333-8444-555555555555}",
        ] {
            assert_eq!(
                ServerId::parse(bad),
                Err(ServerListError::InvalidServerId),
                "{bad}"
            );
        }
    }

    #[test]
    fn url_accepts_https_and_private_or_local_http() -> Result<(), ServerListError> {
        for url in [
            "https://jelly.example.com",
            "https://jelly.example.com:8443/jellyfin/",
            "http://10.0.0.10:8096/",
            "http://192.168.1.20:8096",
            "http://172.16.0.4",
            "http://127.0.0.1:8096",
            "http://localhost:8096",
            "http://media.local:8096",
            "http://[::1]:8096",
            "http://[fd00::20]:8096",
        ] {
            let result = ServerEntry::new(ServerId::parse(GUID_A)?, "Home", url, false);
            assert!(result.is_ok(), "{url} refused: {:?}", result.err());
        }
        Ok(())
    }

    #[test]
    fn url_is_canonicalized() -> Result<(), ServerListError> {
        let e = entry(GUID_A, "Home", "  HTTPS://Jelly.Example.COM:443/jf/// ")?;
        assert_eq!(e.base_url(), "https://jelly.example.com/jf");
        let e = entry(GUID_A, "Home", "http://10.0.0.5:80")?;
        assert_eq!(e.base_url(), "http://10.0.0.5");
        Ok(())
    }

    #[test]
    fn url_refuses_userinfo() {
        for url in [
            "https://user:pass@jelly.example.com",
            "https://user@jelly.example.com",
            "http://user:pass@10.0.0.5:8096",
            "https://:pass@jelly.example.com",
        ] {
            assert_eq!(
                entry(GUID_A, "Home", url),
                Err(ServerListError::BaseUrlHasCredentials),
                "{url}"
            );
        }
    }

    #[test]
    fn url_refuses_query_and_fragment() {
        for url in [
            "https://jelly.example.com/?api_key=abc",
            "https://jelly.example.com/#frag",
            "http://10.0.0.5:8096/?x=1",
        ] {
            assert_eq!(
                entry(GUID_A, "Home", url),
                Err(ServerListError::BaseUrlHasQueryOrFragment),
                "{url}"
            );
        }
    }

    #[test]
    fn url_refuses_plaintext_to_public_host_without_opt_in() -> Result<(), ServerListError> {
        for url in [
            "http://example.com",
            "http://8.8.8.8:8096",
            "http://172.32.0.1",
            "http://[2001:db8::1]:8096",
        ] {
            assert_eq!(
                entry(GUID_A, "Home", url),
                Err(ServerListError::InsecureRemoteHttp),
                "{url}"
            );
        }
        let opted_in =
            ServerEntry::new(ServerId::parse(GUID_A)?, "Home", "http://example.com", true)?;
        assert!(opted_in.allow_insecure_remote_http());
        Ok(())
    }

    #[test]
    fn url_refuses_other_schemes_and_malformed_text() {
        for url in [
            "ftp://jelly.example.com",
            "file:///etc/passwd",
            "javascript://x",
            "jelly.example.com",
            "https://",
            "https:///path",
            "https://bad host",
            "https://jelly..example.com",
            "https://jelly.example.com:0",
            "https://jelly.example.com:65536",
            "https://jelly.example.com:",
            "https://[::1",
            "https://host\\path",
            "https://a://b",
        ] {
            assert!(
                entry(GUID_A, "Home", url).is_err(),
                "{url} should be refused"
            );
        }
    }

    #[test]
    fn url_length_is_bounded() {
        let long = format!(
            "https://jelly.example.com/{}",
            "a".repeat(MAX_BASE_URL_BYTES)
        );
        assert_eq!(
            entry(GUID_A, "Home", &long),
            Err(ServerListError::InvalidBaseUrl)
        );
    }

    #[test]
    fn display_name_is_trimmed_and_bounded() -> Result<(), ServerListError> {
        assert_eq!(
            entry(GUID_A, "  Living room  ", "https://a.example")?.display_name(),
            "Living room"
        );
        let max = "x".repeat(MAX_DISPLAY_NAME_CHARS);
        entry(GUID_A, &max, "https://a.example")?;
        let over = "x".repeat(MAX_DISPLAY_NAME_CHARS + 1);
        for name in [
            "",
            "   ",
            over.as_str(),
            "bad\tname",
            "see https://x",
            "token=abc",
        ] {
            assert_eq!(
                entry(GUID_A, name, "https://a.example"),
                Err(ServerListError::InvalidDisplayName),
                "{name:?}"
            );
        }
        Ok(())
    }

    #[test]
    fn list_first_added_is_default_and_removal_falls_back() -> Result<(), ServerListError> {
        let mut list = ServerList::new();
        list.add(entry(GUID_A, "A", "https://a.example")?)?;
        list.add(entry(GUID_B, "B", "https://b.example")?)?;
        let a = ServerId::parse(GUID_A)?;
        let b = ServerId::parse(GUID_B)?;
        assert_eq!(list.default_server().map(ServerEntry::id), Some(&a));

        list.set_default(&b)?;
        assert_eq!(list.default_server().map(ServerEntry::id), Some(&b));
        list.remove(&b)?;
        assert_eq!(list.default_server().map(ServerEntry::id), Some(&a));

        list.remove(&a)?;
        assert!(list.default_server().is_none());
        assert!(list.is_empty());
        Ok(())
    }

    #[test]
    fn list_add_refuses_duplicate_id_and_duplicate_url() -> Result<(), ServerListError> {
        let mut list = ServerList::new();
        list.add(entry(GUID_A, "A", "https://a.example")?)?;
        assert_eq!(
            list.add(entry(GUID_A, "Other", "https://other.example")?),
            Err(ServerListError::DuplicateServerId)
        );
        assert_eq!(
            list.add(entry(GUID_B, "Same", "HTTPS://A.EXAMPLE/")?),
            Err(ServerListError::DuplicateBaseUrl)
        );
        assert_eq!(list.len(), 1);
        Ok(())
    }

    #[test]
    fn list_length_is_bounded() -> Result<(), ServerListError> {
        let mut list = ServerList::new();
        for index in 0..MAX_SERVERS {
            let url = format!("https://server{index}.example");
            let id = ServerId::parse(&format!("00000000-0000-4000-8000-{index:012x}"))?;
            list.add(ServerEntry::new(id, "Server", &url, false)?)?;
        }
        assert_eq!(list.len(), MAX_SERVERS);
        let extra = ServerEntry::new(
            ServerId::parse(GUID_A)?,
            "Extra",
            "https://extra.example",
            false,
        )?;
        assert_eq!(list.add(extra), Err(ServerListError::ListFull));
        Ok(())
    }

    #[test]
    fn list_order_is_stable_across_remove_and_rename() -> Result<(), ServerListError> {
        let mut list = ServerList::new();
        list.add(entry(GUID_A, "A", "https://a.example")?)?;
        list.add(entry(GUID_B, "B", "https://b.example")?)?;
        list.add(entry(GUID_C, "C", "https://c.example")?)?;
        list.rename(&ServerId::parse(GUID_B)?, "Bee")?;
        list.remove(&ServerId::parse(GUID_A)?)?;
        let names: Vec<&str> = list
            .entries()
            .iter()
            .map(ServerEntry::display_name)
            .collect();
        assert_eq!(names, ["Bee", "C"]);
        Ok(())
    }

    #[test]
    fn list_missing_id_operations_report_not_found() -> Result<(), ServerListError> {
        let mut list = ServerList::new();
        let missing = ServerId::parse(GUID_A)?;
        assert_eq!(list.remove(&missing), Err(ServerListError::NotFound));
        assert_eq!(list.rename(&missing, "x"), Err(ServerListError::NotFound));
        assert_eq!(list.set_default(&missing), Err(ServerListError::NotFound));
        Ok(())
    }

    #[test]
    fn record_round_trip_preserves_entries_and_default() -> Result<(), ServerListError> {
        let mut list = ServerList::new();
        list.add(ServerEntry::new(
            ServerId::parse(GUID_A)?,
            "Home \"main\"",
            "http://10.0.0.10:8096/",
            false,
        )?)?;
        list.add(entry(GUID_B, "Remote", "https://jelly.example.com/jf")?)?;
        list.add(ServerEntry::new(
            ServerId::parse(GUID_C)?,
            "Opt-in",
            "http://example.org",
            true,
        )?)?;
        list.set_default(&ServerId::parse(GUID_B)?)?;

        let text = list.encode();
        let decoded = ServerList::decode(&text)?;
        assert_eq!(decoded, list);
        assert_eq!(decoded.encode(), text);
        Ok(())
    }

    #[test]
    fn empty_list_round_trips() -> Result<(), ServerListError> {
        let text = ServerList::new().encode();
        assert_eq!(text, "{\"version\":1,\"default_id\":null,\"servers\":[]}");
        assert_eq!(ServerList::decode(&text)?, ServerList::new());
        Ok(())
    }

    #[test]
    fn decode_refuses_unknown_or_credential_keys() -> Result<(), ServerListError> {
        let mut list = ServerList::new();
        list.add(entry(GUID_A, "Home", "https://a.example")?)?;
        let valid = list.encode();
        let with_token = valid.replacen("\"base_url\"", "\"token\":\"abc\",\"base_url\"", 1);
        let with_password =
            valid.replacen("\"display_name\"", "\"password\":\"x\",\"display_name\"", 1);
        let with_username =
            valid.replacen("\"display_name\"", "\"username\":\"x\",\"display_name\"", 1);
        let with_top_extra = valid.replacen("\"servers\"", "\"access\":1,\"servers\"", 1);
        for bad in [with_token, with_password, with_username, with_top_extra] {
            assert_eq!(
                ServerList::decode(&bad),
                Err(ServerListError::MalformedRecord),
                "{bad}"
            );
        }
        Ok(())
    }

    #[test]
    fn decode_refuses_stored_url_with_credentials_or_plaintext() {
        let userinfo = "{\"version\":1,\"default_id\":\"11111111-2222-4333-8444-555555555555\",\"servers\":[{\"id\":\"11111111-2222-4333-8444-555555555555\",\"display_name\":\"Home\",\"base_url\":\"https://u:p@a.example\",\"allow_insecure_remote_http\":false}]}";
        assert_eq!(
            ServerList::decode(userinfo),
            Err(ServerListError::BaseUrlHasCredentials)
        );
        let plain = "{\"version\":1,\"default_id\":\"11111111-2222-4333-8444-555555555555\",\"servers\":[{\"id\":\"11111111-2222-4333-8444-555555555555\",\"display_name\":\"Home\",\"base_url\":\"http://a.example\",\"allow_insecure_remote_http\":false}]}";
        assert_eq!(
            ServerList::decode(plain),
            Err(ServerListError::InsecureRemoteHttp)
        );
    }

    #[test]
    fn decode_refuses_broken_default_and_versions() {
        let no_default = "{\"version\":1,\"default_id\":null,\"servers\":[{\"id\":\"11111111-2222-4333-8444-555555555555\",\"display_name\":\"Home\",\"base_url\":\"https://a.example\",\"allow_insecure_remote_http\":false}]}";
        assert_eq!(
            ServerList::decode(no_default),
            Err(ServerListError::MalformedRecord)
        );
        let dangling = "{\"version\":1,\"default_id\":\"99999999-2222-4333-8444-555555555555\",\"servers\":[]}";
        assert_eq!(
            ServerList::decode(dangling),
            Err(ServerListError::MalformedRecord)
        );
        let future = "{\"version\":2,\"default_id\":null,\"servers\":[]}";
        assert_eq!(
            ServerList::decode(future),
            Err(ServerListError::UnsupportedVersion)
        );
    }

    #[test]
    fn decode_refuses_corrupt_and_oversized_input() {
        for bad in [
            "",
            "not json",
            "{\"version\":1",
            "[]",
            "{\"version\":1,\"default_id\":null,\"servers\":[]} x",
            "{\"version\":1.5}",
        ] {
            assert!(
                ServerList::decode(bad).is_err(),
                "{bad:?} should be refused"
            );
        }
        let big = " ".repeat(MAX_RECORD_BYTES + 1);
        assert_eq!(
            ServerList::decode(&big),
            Err(ServerListError::RecordTooLarge)
        );
    }

    #[test]
    fn guard_no_record_key_names_a_credential() -> Result<(), ServerListError> {
        for key in RECORD_KEYS {
            assert!(
                !is_secret_like_key(key),
                "record key {key} matches the credential guard"
            );
        }
        let mut list = ServerList::new();
        list.add(entry(GUID_A, "Home", "https://a.example")?)?;
        let parsed = parse_json(&list.encode())?;
        let mut keys = Vec::new();
        collect_keys(&parsed, &mut keys);
        assert!(!keys.is_empty());
        for key in keys {
            assert!(
                !is_secret_like_key(&key),
                "encoded key {key} matches the credential guard"
            );
            assert!(RECORD_KEYS.contains(&key.as_str()), "unexpected key {key}");
        }
        Ok(())
    }

    fn collect_keys(value: &Json, keys: &mut Vec<String>) {
        match value {
            Json::Object(members) => {
                for (key, child) in members {
                    keys.push(key.clone());
                    collect_keys(child, keys);
                }
            }
            Json::Array(items) => items.iter().for_each(|item| collect_keys(item, keys)),
            _ => {}
        }
    }
}
