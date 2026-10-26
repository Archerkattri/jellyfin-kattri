//! Remote control commands for this client: parsing the commands the server
//! sends, and the request form the server expects for the same commands.
//!
//! Commands reach a client over the server's WebSocket, not over HTTP. The
//! OpenAPI describes the payloads as `GeneralCommandMessage` (its `Data` is a
//! `GeneralCommand` with `Name` and `Arguments`) and `PlaystateMessage` (its
//! `Data` is a `PlaystateRequest` with `Command`, `SeekPositionTicks`, and
//! `ControllingUserId`). The HTTP routes that send the same commands to a
//! session are `POST /Sessions/{sessionId}/Command` (`SendFullGeneralCommand`,
//! body `GeneralCommand`) and `POST /Sessions/{sessionId}/Playing/{command}`
//! (`SendPlaystateCommand`, `command` is a `PlaystateCommand`, optional
//! `seekPositionTicks` query).
//!
//! Nothing here executes a command or performs I/O. The client core decides
//! what to do with a parsed [`RemoteCommand`].

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::capabilities::GeneralCommandType;

/// Highest volume a `SetVolume` command may request.
pub const MAX_VOLUME: u8 = 100;

/// A playstate command as the `PlaystateCommand` enum names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PlaystateCommand {
    Stop,
    Pause,
    Unpause,
    NextTrack,
    PreviousTrack,
    Seek,
    Rewind,
    FastForward,
    PlayPause,
}

impl PlaystateCommand {
    /// The path segment used by `POST /Sessions/{sessionId}/Playing/{command}`.
    pub const fn as_api_value(self) -> &'static str {
        match self {
            Self::Stop => "Stop",
            Self::Pause => "Pause",
            Self::Unpause => "Unpause",
            Self::NextTrack => "NextTrack",
            Self::PreviousTrack => "PreviousTrack",
            Self::Seek => "Seek",
            Self::Rewind => "Rewind",
            Self::FastForward => "FastForward",
            Self::PlayPause => "PlayPause",
        }
    }
}

/// The `GeneralCommand` body: a command name and string arguments.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct GeneralCommand {
    pub name: GeneralCommandType,
    pub arguments: BTreeMap<String, String>,
}

/// A remote command this client understands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemoteCommand {
    Play,
    Pause,
    Stop,
    Next,
    Previous,
    /// Seek to an absolute position in Jellyfin ticks (100-nanosecond units).
    Seek {
        position_ticks: i64,
    },
    /// Set the volume to `0..=MAX_VOLUME` percent.
    SetVolume {
        volume: u8,
    },
}

/// The request form the server expects for a [`RemoteCommand`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RemoteRoute {
    /// `POST /Sessions/{sessionId}/Playing/{command}` with an optional
    /// `seekPositionTicks` query parameter and no body.
    Playstate {
        command: PlaystateCommand,
        seek_position_ticks: Option<i64>,
    },
    /// `POST /Sessions/{sessionId}/Command` with this `GeneralCommand` body.
    General(GeneralCommand),
}

/// Why a remote command payload was not accepted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RemoteCommandError {
    #[error("the remote command payload is not valid JSON for a command")]
    InvalidPayload,
    #[error("this client does not support the remote command")]
    UnsupportedCommand,
    #[error("the remote command argument is missing or out of range")]
    InvalidArgument,
}

/// Inbound `GeneralCommand` payload, read from the server's message data.
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct InboundGeneralCommand {
    name: String,
    #[serde(default)]
    arguments: BTreeMap<String, String>,
}

/// Inbound `PlaystateRequest` payload, read from the server's message data.
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct InboundPlaystateRequest {
    command: String,
    #[serde(default)]
    seek_position_ticks: Option<i64>,
}

impl RemoteCommand {
    /// Parse the `Data` of a `GeneralCommandMessage`.
    ///
    /// Only `SetVolume` is accepted here; every other general command name is
    /// refused as unsupported.
    pub fn from_general_command_json(json: &str) -> Result<Self, RemoteCommandError> {
        let inbound: InboundGeneralCommand =
            serde_json::from_str(json).map_err(|_| RemoteCommandError::InvalidPayload)?;
        match inbound.name.as_str() {
            "SetVolume" => {
                let raw = inbound
                    .arguments
                    .get("Volume")
                    .ok_or(RemoteCommandError::InvalidArgument)?;
                let volume: u8 = raw
                    .parse()
                    .map_err(|_| RemoteCommandError::InvalidArgument)?;
                if volume > MAX_VOLUME {
                    return Err(RemoteCommandError::InvalidArgument);
                }
                Ok(Self::SetVolume { volume })
            }
            _ => Err(RemoteCommandError::UnsupportedCommand),
        }
    }

    /// Parse the `Data` of a `PlaystateMessage`.
    ///
    /// `Unpause` maps to [`RemoteCommand::Play`], because the playstate enum
    /// has no separate play command. `PlayPause`, `Rewind`, and `FastForward`
    /// are refused as unsupported.
    pub fn from_playstate_json(json: &str) -> Result<Self, RemoteCommandError> {
        let inbound: InboundPlaystateRequest =
            serde_json::from_str(json).map_err(|_| RemoteCommandError::InvalidPayload)?;
        match inbound.command.as_str() {
            "Unpause" => Ok(Self::Play),
            "Pause" => Ok(Self::Pause),
            "Stop" => Ok(Self::Stop),
            "NextTrack" => Ok(Self::Next),
            "PreviousTrack" => Ok(Self::Previous),
            "Seek" => {
                let position_ticks = inbound
                    .seek_position_ticks
                    .filter(|ticks| *ticks >= 0)
                    .ok_or(RemoteCommandError::InvalidArgument)?;
                Ok(Self::Seek { position_ticks })
            }
            _ => Err(RemoteCommandError::UnsupportedCommand),
        }
    }

    /// The request form the server expects for this command.
    pub fn route(&self) -> RemoteRoute {
        match *self {
            Self::Play => playstate(PlaystateCommand::Unpause, None),
            Self::Pause => playstate(PlaystateCommand::Pause, None),
            Self::Stop => playstate(PlaystateCommand::Stop, None),
            Self::Next => playstate(PlaystateCommand::NextTrack, None),
            Self::Previous => playstate(PlaystateCommand::PreviousTrack, None),
            Self::Seek { position_ticks } => {
                playstate(PlaystateCommand::Seek, Some(position_ticks))
            }
            Self::SetVolume { volume } => RemoteRoute::General(GeneralCommand {
                name: GeneralCommandType::SetVolume,
                arguments: BTreeMap::from([("Volume".to_owned(), volume.to_string())]),
            }),
        }
    }
}

fn playstate(command: PlaystateCommand, seek_position_ticks: Option<i64>) -> RemoteRoute {
    RemoteRoute::Playstate {
        command,
        seek_position_ticks,
    }
}

#[cfg(test)]
mod tests {
    use super::{MAX_VOLUME, PlaystateCommand, RemoteCommand, RemoteCommandError, RemoteRoute};

    /// Shape of a `GeneralCommandMessage` `Data` payload as the OpenAPI describes it.
    const SET_VOLUME_RECORDED: &str = r#"{
        "Name": "SetVolume",
        "Arguments": { "Volume": "40" },
        "ControllingUserId": "00000000-0000-0000-0000-000000000d01"
    }"#;

    /// Shape of a `PlaystateMessage` `Data` payload as the OpenAPI describes it.
    const SEEK_RECORDED: &str = r#"{
        "Command": "Seek",
        "SeekPositionTicks": 600000000,
        "ControllingUserId": "00000000-0000-0000-0000-000000000d01"
    }"#;

    #[test]
    fn parses_a_set_volume_general_command() {
        assert_eq!(
            RemoteCommand::from_general_command_json(SET_VOLUME_RECORDED),
            Ok(RemoteCommand::SetVolume { volume: 40 })
        );
    }

    #[test]
    fn parses_playstate_requests_into_each_transport_command() {
        for (json, expected) in [
            (r#"{"Command":"Unpause"}"#, RemoteCommand::Play),
            (r#"{"Command":"Pause"}"#, RemoteCommand::Pause),
            (r#"{"Command":"Stop"}"#, RemoteCommand::Stop),
            (r#"{"Command":"NextTrack"}"#, RemoteCommand::Next),
            (r#"{"Command":"PreviousTrack"}"#, RemoteCommand::Previous),
        ] {
            assert_eq!(
                RemoteCommand::from_playstate_json(json),
                Ok(expected),
                "{json}"
            );
        }
    }

    #[test]
    fn parses_the_recorded_seek_example() {
        assert_eq!(
            RemoteCommand::from_playstate_json(SEEK_RECORDED),
            Ok(RemoteCommand::Seek {
                position_ticks: 600_000_000
            })
        );
    }

    #[test]
    fn unknown_general_commands_are_refused() {
        for name in ["ToggleFullscreen", "Mute", "VolumeUp", "Bogus", ""] {
            let json = format!(r#"{{"Name":"{name}","Arguments":{{}}}}"#);
            assert_eq!(
                RemoteCommand::from_general_command_json(&json),
                Err(RemoteCommandError::UnsupportedCommand),
                "{name:?}"
            );
        }
    }

    #[test]
    fn unknown_playstate_commands_are_refused() {
        for command in ["PlayPause", "Rewind", "FastForward", "Bogus", ""] {
            let json = format!(r#"{{"Command":"{command}"}}"#);
            assert_eq!(
                RemoteCommand::from_playstate_json(&json),
                Err(RemoteCommandError::UnsupportedCommand),
                "{command:?}"
            );
        }
    }

    #[test]
    fn malformed_payloads_are_invalid_not_unsupported() {
        assert_eq!(
            RemoteCommand::from_general_command_json("not json"),
            Err(RemoteCommandError::InvalidPayload)
        );
        assert_eq!(
            RemoteCommand::from_playstate_json(r#"{"Command": 7}"#),
            Err(RemoteCommandError::InvalidPayload)
        );
    }

    #[test]
    fn volume_must_be_present_numeric_and_within_range() {
        for body in [
            r#"{"Name":"SetVolume","Arguments":{}}"#,
            r#"{"Name":"SetVolume","Arguments":{"Volume":"loud"}}"#,
            r#"{"Name":"SetVolume","Arguments":{"Volume":"-5"}}"#,
            r#"{"Name":"SetVolume","Arguments":{"Volume":"101"}}"#,
        ] {
            assert_eq!(
                RemoteCommand::from_general_command_json(body),
                Err(RemoteCommandError::InvalidArgument),
                "{body}"
            );
        }
        let edge = format!(r#"{{"Name":"SetVolume","Arguments":{{"Volume":"{MAX_VOLUME}"}}}}"#);
        assert_eq!(
            RemoteCommand::from_general_command_json(&edge),
            Ok(RemoteCommand::SetVolume { volume: MAX_VOLUME })
        );
    }

    #[test]
    fn seek_requires_a_non_negative_position() {
        assert_eq!(
            RemoteCommand::from_playstate_json(r#"{"Command":"Seek"}"#),
            Err(RemoteCommandError::InvalidArgument)
        );
        assert_eq!(
            RemoteCommand::from_playstate_json(r#"{"Command":"Seek","SeekPositionTicks":-1}"#),
            Err(RemoteCommandError::InvalidArgument)
        );
    }

    #[test]
    fn transport_commands_map_to_playstate_routes() {
        assert_eq!(
            RemoteCommand::Play.route(),
            RemoteRoute::Playstate {
                command: PlaystateCommand::Unpause,
                seek_position_ticks: None
            }
        );
        assert_eq!(
            RemoteCommand::Seek { position_ticks: 42 }.route(),
            RemoteRoute::Playstate {
                command: PlaystateCommand::Seek,
                seek_position_ticks: Some(42)
            }
        );
        for (command, expected) in [
            (RemoteCommand::Pause, PlaystateCommand::Pause),
            (RemoteCommand::Stop, PlaystateCommand::Stop),
            (RemoteCommand::Next, PlaystateCommand::NextTrack),
            (RemoteCommand::Previous, PlaystateCommand::PreviousTrack),
        ] {
            assert_eq!(
                command.route(),
                RemoteRoute::Playstate {
                    command: expected,
                    seek_position_ticks: None
                }
            );
        }
    }

    #[test]
    fn volume_maps_to_a_general_command_body() {
        let RemoteRoute::General(body) = (RemoteCommand::SetVolume { volume: 40 }).route() else {
            panic!("volume must use the general command route");
        };
        let encoded = serde_json::to_value(&body).expect("serialize");
        assert_eq!(
            encoded,
            serde_json::json!({"Name": "SetVolume", "Arguments": {"Volume": "40"}})
        );
    }

    #[test]
    fn playstate_api_values_match_the_openapi_enum() {
        for (command, expected) in [
            (PlaystateCommand::Stop, "Stop"),
            (PlaystateCommand::Pause, "Pause"),
            (PlaystateCommand::Unpause, "Unpause"),
            (PlaystateCommand::NextTrack, "NextTrack"),
            (PlaystateCommand::PreviousTrack, "PreviousTrack"),
            (PlaystateCommand::Seek, "Seek"),
            (PlaystateCommand::Rewind, "Rewind"),
            (PlaystateCommand::FastForward, "FastForward"),
            (PlaystateCommand::PlayPause, "PlayPause"),
        ] {
            assert_eq!(command.as_api_value(), expected);
        }
    }
}
