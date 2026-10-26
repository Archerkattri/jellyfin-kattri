//! Discord Rich Presence: the IPC frame encoder, the pipe path resolver and the activity
//! payload builder.
//!
//! Frame layout, little endian: a `u32` opcode, a `u32` payload length, then the JSON
//! payload. The plugin never opens a socket. The host builds the frame with this module and
//! writes it through [`write_frame`] to the connection returned by [`connect`].
//!
//! Presence text never carries a URL or a credential. Every text field passes
//! [`check_presence_text`] before it reaches a frame, and the large image key must be a bare
//! asset name.
//!
//! Nothing in this module touches the operating system. The real endpoint is in [`ipc`],
//! built only with the `discord-ipc` feature. [`bus`] binds the plugin to the playback event
//! bus.

use std::io::{self, Write};
use std::path::PathBuf;

use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::error::PluginError;

pub mod bus;
#[cfg(feature = "discord-ipc")]
pub mod ipc;
pub mod plugin;
pub mod presence;
pub mod transport;

pub use bus::PresenceBinding;
#[cfg(feature = "discord-ipc")]
pub use ipc::IpcTransport;
pub use transport::{MockTransport, PresenceTransport, TransportError};

/// Bytes in a frame header: opcode and payload length.
pub const HEADER_LEN: usize = 8;
/// Largest JSON payload accepted in either direction.
pub const MAX_PAYLOAD_BYTES: usize = 64 * 1024;
/// Highest pipe or socket index probed. Discord listens on `discord-ipc-0` to `-9`.
pub const MAX_IPC_INDEX: u32 = 9;
/// Longest state or details line, in characters.
pub const MAX_PRESENCE_CHARS: usize = 128;
/// Longest large image key, in characters.
pub const MAX_IMAGE_KEY_CHARS: usize = 32;
/// Longest nonce a caller may pass with a command.
const MAX_NONCE_CHARS: usize = 64;

/// Frame opcodes defined by the Discord IPC protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Opcode {
    Handshake,
    Frame,
    Close,
    Ping,
    Pong,
}

impl Opcode {
    /// The wire value of the opcode.
    pub const fn code(self) -> u32 {
        match self {
            Self::Handshake => 0,
            Self::Frame => 1,
            Self::Close => 2,
            Self::Ping => 3,
            Self::Pong => 4,
        }
    }

    /// The opcode for a wire value, or `None` for an unknown value.
    pub const fn from_code(code: u32) -> Option<Self> {
        match code {
            0 => Some(Self::Handshake),
            1 => Some(Self::Frame),
            2 => Some(Self::Close),
            3 => Some(Self::Ping),
            4 => Some(Self::Pong),
            _ => None,
        }
    }
}

/// Build one frame: header followed by the payload bytes.
pub fn encode_frame(opcode: Opcode, payload: &[u8]) -> Result<Vec<u8>, PluginError> {
    let length = check_payload_len(payload.len())?;
    let mut frame = Vec::with_capacity(HEADER_LEN + payload.len());
    frame.extend_from_slice(&opcode.code().to_le_bytes());
    frame.extend_from_slice(&length.to_le_bytes());
    frame.extend_from_slice(payload);
    Ok(frame)
}

/// Read the opcode and payload length from a frame header.
pub fn decode_header(header: [u8; HEADER_LEN]) -> Result<(Opcode, usize), PluginError> {
    let code = u32::from_le_bytes([header[0], header[1], header[2], header[3]]);
    let length = u32::from_le_bytes([header[4], header[5], header[6], header[7]]);
    let opcode = Opcode::from_code(code).ok_or_else(|| PluginError::Invalid {
        field: "discord.opcode",
        reason: format!("unknown opcode {code}"),
    })?;
    let length = usize::try_from(length).unwrap_or(usize::MAX);
    if length > MAX_PAYLOAD_BYTES {
        return Err(PluginError::Invalid {
            field: "discord.payload",
            reason: format!("must be at most {MAX_PAYLOAD_BYTES} bytes"),
        });
    }
    Ok((opcode, length))
}

/// Write one frame to `writer`.
pub fn write_frame<W: Write>(writer: &mut W, opcode: Opcode, payload: &[u8]) -> io::Result<()> {
    let frame = encode_frame(opcode, payload)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err.to_string()))?;
    writer.write_all(&frame)?;
    writer.flush()
}

/// The handshake payload for an application id. The id must be all ASCII digits.
pub fn handshake_payload(client_id: &str) -> Result<Vec<u8>, PluginError> {
    let valid = !client_id.is_empty()
        && client_id.len() <= 32
        && client_id.bytes().all(|b| b.is_ascii_digit());
    if !valid {
        return Err(PluginError::Invalid {
            field: "discord.client_id",
            reason: "must be 1 to 32 ASCII digits".to_owned(),
        });
    }
    Ok(format!(r#"{{"v":1,"client_id":"{client_id}"}}"#).into_bytes())
}

/// Candidate pipe paths on Windows, in probe order: `\\?\pipe\discord-ipc-0` to `-9`.
pub fn windows_pipe_paths() -> Vec<String> {
    (0..=MAX_IPC_INDEX)
        .map(|index| format!(r"\\?\pipe\discord-ipc-{index}"))
        .collect()
}

/// The socket path for one index under `runtime_dir`, for example `/run/user/1000/discord-ipc-0`.
pub fn unix_socket_path(runtime_dir: &str, index: u32) -> PathBuf {
    PathBuf::from(runtime_dir).join(format!("discord-ipc-{index}"))
}

/// The directory that holds the Unix sockets: `$XDG_RUNTIME_DIR` when it is set and not empty,
/// otherwise `/tmp`.
pub fn unix_runtime_dir(xdg_runtime_dir: Option<&str>) -> String {
    match xdg_runtime_dir {
        Some(dir) if !dir.is_empty() => dir.to_owned(),
        _ => "/tmp".to_owned(),
    }
}

/// Presence shown to other users. Every field is optional. An empty activity clears it.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PresenceActivity {
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default)]
    pub details: Option<String>,
    #[serde(default)]
    pub large_image_key: Option<String>,
}

impl PresenceActivity {
    /// Check every field against the presence rules.
    pub fn validate(&self) -> Result<(), PluginError> {
        if let Some(state) = &self.state {
            check_presence_text("discord.state", state)?;
        }
        if let Some(details) = &self.details {
            check_presence_text("discord.details", details)?;
        }
        if let Some(key) = &self.large_image_key {
            check_image_key(key)?;
        }
        Ok(())
    }

    fn is_empty(&self) -> bool {
        self.state.is_none() && self.details.is_none() && self.large_image_key.is_none()
    }
}

/// Parse and validate the activity JSON a plugin passes to `presence-write`.
pub fn parse_activity(text: &str) -> Result<PresenceActivity, PluginError> {
    check_payload_len(text.len())?;
    let activity: PresenceActivity =
        serde_json::from_str(text).map_err(|err| PluginError::Json(err.to_string()))?;
    activity.validate()?;
    Ok(activity)
}

/// The `SET_ACTIVITY` command frame payload for `activity`. An empty activity clears the
/// presence, which Discord expects as a null `activity`.
pub fn set_activity_payload(
    activity: &PresenceActivity,
    process_id: u32,
    nonce: &str,
) -> Result<Vec<u8>, PluginError> {
    activity.validate()?;
    let nonce_ok = !nonce.is_empty()
        && nonce.chars().count() <= MAX_NONCE_CHARS
        && nonce.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
    if !nonce_ok {
        return Err(PluginError::Invalid {
            field: "discord.nonce",
            reason: "must be 1 to 64 ASCII letters, digits or dashes".to_owned(),
        });
    }

    let activity_value = if activity.is_empty() {
        Value::Null
    } else {
        let mut object = Map::new();
        if let Some(state) = &activity.state {
            object.insert("state".to_owned(), Value::String(state.clone()));
        }
        if let Some(details) = &activity.details {
            object.insert("details".to_owned(), Value::String(details.clone()));
        }
        if let Some(key) = &activity.large_image_key {
            object.insert("assets".to_owned(), json!({ "large_image": key }));
        }
        Value::Object(object)
    };

    let command = json!({
        "cmd": "SET_ACTIVITY",
        "args": { "pid": process_id, "activity": activity_value },
        "nonce": nonce,
    });
    let bytes = serde_json::to_vec(&command).map_err(|err| PluginError::Json(err.to_string()))?;
    check_payload_len(bytes.len())?;
    Ok(bytes)
}

/// Check one line of presence text: 1 to [`MAX_PRESENCE_CHARS`] characters, no control
/// characters, no URL or credential marker, and no token-like run.
pub fn check_presence_text(field: &'static str, text: &str) -> Result<(), PluginError> {
    let length = text.chars().count();
    if length == 0 || length > MAX_PRESENCE_CHARS {
        return Err(PluginError::Invalid {
            field,
            reason: format!("must be 1 to {MAX_PRESENCE_CHARS} characters"),
        });
    }
    if text.chars().any(char::is_control) {
        return Err(PluginError::Invalid {
            field,
            reason: "contains control characters".to_owned(),
        });
    }
    if kattri_guard::has_scheme_separator(text) || kattri_guard::is_secret_like_value(text) {
        return Err(PluginError::Invalid {
            field,
            reason: "looks like a URL or a credential".to_owned(),
        });
    }
    if text.split_whitespace().any(is_token_like) {
        return Err(PluginError::Invalid {
            field,
            reason: "contains a token-like run".to_owned(),
        });
    }
    Ok(())
}

/// Check a large image key: 1 to [`MAX_IMAGE_KEY_CHARS`] lowercase ASCII letters, digits,
/// `_` or `-`. A key that is a URL, a path or a mixed-case token is refused.
pub fn check_image_key(key: &str) -> Result<(), PluginError> {
    let valid = !key.is_empty()
        && key.chars().count() <= MAX_IMAGE_KEY_CHARS
        && key
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-');
    if valid {
        Ok(())
    } else {
        Err(PluginError::Invalid {
            field: "discord.large_image_key",
            reason: "must be 1 to 32 lowercase letters, digits, '_' or '-'".to_owned(),
        })
    }
}

/// A whitespace-free run of 32 or more letters and digits that mixes both kinds, which is how
/// access tokens and ids look.
fn is_token_like(token: &str) -> bool {
    token.len() >= 32
        && token
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        && token.chars().any(|c| c.is_ascii_digit())
        && token.chars().any(|c| c.is_ascii_alphabetic())
}

fn check_payload_len(length: usize) -> Result<u32, PluginError> {
    if length > MAX_PAYLOAD_BYTES {
        return Err(PluginError::Invalid {
            field: "discord.payload",
            reason: format!("must be at most {MAX_PAYLOAD_BYTES} bytes"),
        });
    }
    u32::try_from(length).map_err(|_| PluginError::Invalid {
        field: "discord.payload",
        reason: "length does not fit a frame header".to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_header_is_opcode_then_length_little_endian() {
        let frame = encode_frame(Opcode::Frame, b"{}").expect("small frame");
        assert_eq!(frame, [1, 0, 0, 0, 2, 0, 0, 0, b'{', b'}']);
    }

    #[test]
    fn handshake_frame_bytes_match_the_protocol_layout() {
        let payload = handshake_payload("123456789012345678").expect("valid id");
        assert_eq!(
            payload,
            br#"{"v":1,"client_id":"123456789012345678"}"#.to_vec()
        );
        let frame = encode_frame(Opcode::Handshake, &payload).expect("frame");
        assert_eq!(&frame[..4], &[0, 0, 0, 0]);
        assert_eq!(&frame[4..8], &(payload.len() as u32).to_le_bytes());
        assert_eq!(&frame[8..], payload.as_slice());
    }

    #[test]
    fn header_round_trips_through_decode() {
        for opcode in [
            Opcode::Handshake,
            Opcode::Frame,
            Opcode::Close,
            Opcode::Ping,
            Opcode::Pong,
        ] {
            let frame = encode_frame(opcode, b"abc").expect("frame");
            let mut header = [0_u8; HEADER_LEN];
            header.copy_from_slice(&frame[..HEADER_LEN]);
            assert_eq!(decode_header(header), Ok((opcode, 3)));
        }
    }

    #[test]
    fn unknown_opcode_and_oversize_length_are_refused() {
        let unknown = [9, 0, 0, 0, 0, 0, 0, 0];
        assert!(decode_header(unknown).is_err());

        let mut oversize = [1, 0, 0, 0, 0, 0, 0, 0];
        let too_big = (MAX_PAYLOAD_BYTES as u32 + 1).to_le_bytes();
        oversize[4..8].copy_from_slice(&too_big);
        assert!(decode_header(oversize).is_err());

        let big = vec![0_u8; MAX_PAYLOAD_BYTES + 1];
        assert!(encode_frame(Opcode::Frame, &big).is_err());
    }

    #[test]
    fn write_frame_writes_exactly_the_frame_bytes() {
        let mut sink: Vec<u8> = Vec::new();
        write_frame(&mut sink, Opcode::Ping, b"x").expect("write to memory");
        assert_eq!(sink, [3, 0, 0, 0, 1, 0, 0, 0, b'x']);
    }

    #[test]
    fn handshake_refuses_non_numeric_ids() {
        assert!(handshake_payload("").is_err());
        assert!(handshake_payload("12a").is_err());
        assert!(handshake_payload("123\"").is_err());
    }

    #[test]
    fn windows_pipe_paths_use_the_extended_pipe_prefix() {
        let paths = windows_pipe_paths();
        assert_eq!(paths.len(), 10);
        assert_eq!(paths[0], r"\\?\pipe\discord-ipc-0");
        assert_eq!(paths[9], r"\\?\pipe\discord-ipc-9");
    }

    #[test]
    fn unix_paths_use_the_runtime_dir_or_tmp() {
        assert_eq!(
            unix_socket_path("/run/user/1000", 3),
            PathBuf::from("/run/user/1000/discord-ipc-3")
        );
        assert_eq!(unix_runtime_dir(Some("/run/user/1000")), "/run/user/1000");
        assert_eq!(unix_runtime_dir(Some("")), "/tmp");
        assert_eq!(unix_runtime_dir(None), "/tmp");
    }

    #[test]
    fn activity_payload_carries_state_details_and_large_image() {
        let activity = parse_activity(
            r#"{"state":"Watching","details":"Kitchen sink, season 2","large_image_key":"poster_art-1"}"#,
        )
        .expect("valid activity");
        let bytes = set_activity_payload(&activity, 4242, "n-1").expect("payload");
        let value: Value = serde_json::from_slice(&bytes).expect("payload is JSON");
        assert_eq!(value["cmd"], "SET_ACTIVITY");
        assert_eq!(value["nonce"], "n-1");
        assert_eq!(value["args"]["pid"], 4242);
        assert_eq!(value["args"]["activity"]["state"], "Watching");
        assert_eq!(
            value["args"]["activity"]["details"],
            "Kitchen sink, season 2"
        );
        assert_eq!(
            value["args"]["activity"]["assets"]["large_image"],
            "poster_art-1"
        );
    }

    #[test]
    fn empty_activity_clears_the_presence() {
        let activity = PresenceActivity::default();
        let bytes = set_activity_payload(&activity, 1, "clear-1").expect("payload");
        let value: Value = serde_json::from_slice(&bytes).expect("payload is JSON");
        assert_eq!(value["args"]["activity"], Value::Null);
    }

    #[test]
    fn unknown_activity_fields_are_refused() {
        assert!(parse_activity(r#"{"state":"ok","party":"x"}"#).is_err());
    }

    #[test]
    fn presence_text_never_carries_a_url() {
        for text in [
            r#"{"details":"http://10.0.0.5:8096/Items/1"}"#,
            r#"{"state":"see https://media.example.org"}"#,
            r#"{"details":"Authorization: Bearer abc"}"#,
            r#"{"state":"api_key=abc"}"#,
        ] {
            assert!(parse_activity(text).is_err(), "accepted: {text}");
        }
    }

    #[test]
    fn presence_text_never_carries_a_token() {
        let token = "a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6"; // gitleaks:allow -- synthetic presence-redaction fixture.
        let text = format!(r#"{{"details":"session {token}"}}"#);
        assert!(parse_activity(&text).is_err());
    }

    #[test]
    fn presence_text_length_and_control_characters_are_bounded() {
        let long = "x".repeat(MAX_PRESENCE_CHARS + 1);
        assert!(check_presence_text("discord.state", &long).is_err());
        let exact = "x".repeat(MAX_PRESENCE_CHARS);
        assert!(check_presence_text("discord.state", &exact).is_ok());
        assert!(check_presence_text("discord.state", "bad\u{7}bell").is_err());
        assert!(check_presence_text("discord.state", "").is_err());
    }

    #[test]
    fn large_image_key_must_be_a_bare_asset_name() {
        assert!(check_image_key("poster_art-1").is_ok());
        assert!(check_image_key("https://cdn.example.org/a.png").is_err());
        assert!(check_image_key("Poster").is_err());
        assert!(check_image_key("a/b").is_err());
        assert!(check_image_key(&"k".repeat(MAX_IMAGE_KEY_CHARS + 1)).is_err());
    }

    #[test]
    fn nonce_must_be_a_plain_identifier() {
        let activity = PresenceActivity::default();
        assert!(set_activity_payload(&activity, 1, "").is_err());
        assert!(set_activity_payload(&activity, 1, "a b").is_err());
        assert!(set_activity_payload(&activity, 1, "ok-1").is_ok());
    }
}
