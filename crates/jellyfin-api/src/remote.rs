//! Remote control for this client: the capability value it advertises (M4-12) and the
//! parser for inbound session messages (M4-13).
//!
//! Advertising uses `POST /Sessions/Capabilities/Full` with a `ClientCapabilitiesDto` body
//! (`PostFullCapabilities`). The OpenAPI document at api.jellyfin.org (version 12.2.0,
//! checked 2026-10-21) confirms the route and the PascalCase field names. No live server
//! has been checked.
//!
//! Inbound commands arrive as `{"MessageType": ..., "Data": ...}` envelopes. The OpenAPI
//! confirms the `GeneralCommand` and `Playstate` message types and their `Data` shapes.
//! The WebSocket that carries them is not in the OpenAPI, so the transport stays
//! unconfirmed. [`parse_session_message`] only reads the envelope.

use serde::Deserialize;
use serde_json::Value;

use crate::capabilities::{ClientCapabilities, GeneralCommandType, PlayableMediaType};
use crate::remote_command::{PlaystateCommand, RemoteCommand, RemoteCommandError};

/// Media types this client plays. Photos and books are not remote-playable here.
pub const PLAYABLE_MEDIA_TYPES: [PlayableMediaType; 2] =
    [PlayableMediaType::Video, PlayableMediaType::Audio];

/// General commands this client executes. Each must have a branch in
/// [`RemoteCommand::from_general_command_json`]; advertising anything else is a bug.
pub const SUPPORTED_GENERAL_COMMANDS: [GeneralCommandType; 1] = [GeneralCommandType::SetVolume];

/// Playstate commands this client executes. They are not listed in `SupportedCommands`
/// because the server sends them as `Playstate` messages once media control is on.
pub const SUPPORTED_PLAYSTATE_COMMANDS: [PlaystateCommand; 4] = [
    PlaystateCommand::Unpause,
    PlaystateCommand::Pause,
    PlaystateCommand::Stop,
    PlaystateCommand::Seek,
];

/// The capability body this client advertises for its session. Media control is on, and
/// the command list is exactly [`SUPPORTED_GENERAL_COMMANDS`].
pub fn remote_control_capabilities() -> ClientCapabilities {
    ClientCapabilities::remote_controllable(
        PLAYABLE_MEDIA_TYPES.to_vec(),
        SUPPORTED_GENERAL_COMMANDS.to_vec(),
    )
}

/// A session message envelope as the server sends it.
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct SessionMessage {
    message_type: String,
    data: Value,
}

/// Parse one inbound session message into a [`RemoteCommand`].
///
/// `GeneralCommand` and `Playstate` are the only message types read. Any other type, and
/// any command the client does not implement, returns
/// [`RemoteCommandError::UnsupportedCommand`]. A command is never guessed from a
/// different one.
pub fn parse_session_message(message: &str) -> Result<RemoteCommand, RemoteCommandError> {
    let envelope: SessionMessage =
        serde_json::from_str(message).map_err(|_| RemoteCommandError::InvalidPayload)?;
    let data =
        serde_json::to_string(&envelope.data).map_err(|_| RemoteCommandError::InvalidPayload)?;
    match envelope.message_type.as_str() {
        "GeneralCommand" => RemoteCommand::from_general_command_json(&data),
        "Playstate" => RemoteCommand::from_playstate_json(&data),
        _ => Err(RemoteCommandError::UnsupportedCommand),
    }
}

