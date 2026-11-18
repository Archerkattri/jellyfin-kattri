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

/// Most item ids accepted in one `Play` request. A server-side "play all" of a large playlist is
/// refused rather than silently truncated.
pub const MAX_REMOTE_PLAY_ITEMS: usize = 100;

/// Longest accepted item id. Jellyfin ids are 32-character hex strings.
const MAX_ITEM_ID_LEN: usize = 64;

/// Whether one item id from a remote payload is usable: bounded, non-empty, and made of the
/// characters a server id can contain. A payload cannot smuggle a path or a wildcard.
fn usable_item_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_ITEM_ID_LEN
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

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
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RemoteCommand {
    Play,
    Pause,
    Stop,
    Next,
    Previous,
    /// Play these items now, starting at `start_position_ticks` when the server supplied one.
    /// The server sends this as a `Play` message with a `PlayRequest` payload ("Play on").
    /// Only the first item is started; see [`Self::from_play_json`].
    PlayItems {
        item_ids: Vec<String>,
        start_position_ticks: Option<i64>,
    },
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

/// Inbound `PlayRequest` payload, read from the `Data` of a `Play` message.
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct InboundPlayRequest {
    play_command: String,
    #[serde(default)]
    item_ids: Option<Vec<String>>,
    #[serde(default)]
    start_position_ticks: Option<i64>,
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

    /// Parse the `Data` of a `Play` message (the server's "Play on" / `SendPlayCommand`).
    ///
    /// Only `PlayNow` is accepted: `PlayNext` and `PlayLast` insert into a queue this client does
    /// not expose to remote control, so they are refused rather than turned into a plain play.
    /// Item ids are bounded and character-checked. When the request names several items only the
    /// first is started; the caller starts one item and this module never guesses a queue.
    pub fn from_play_json(json: &str) -> Result<Self, RemoteCommandError> {
        let inbound: InboundPlayRequest =
            serde_json::from_str(json).map_err(|_| RemoteCommandError::InvalidPayload)?;
        if inbound.play_command != "PlayNow" {
            return Err(RemoteCommandError::UnsupportedCommand);
        }
        let item_ids = inbound
            .item_ids
            .filter(|ids| !ids.is_empty() && ids.len() <= MAX_REMOTE_PLAY_ITEMS)
            .ok_or(RemoteCommandError::InvalidArgument)?;
        if !item_ids.iter().all(|id| usable_item_id(id)) {
            return Err(RemoteCommandError::InvalidArgument);
        }
        let start_position_ticks = inbound.start_position_ticks.filter(|ticks| *ticks >= 0);
        Ok(Self::PlayItems {
            item_ids,
            start_position_ticks,
        })
    }

    /// The request form the server expects for this command.
    ///
    /// `None` means the command only travels server to client, so there is no request form.
    pub fn route(&self) -> Option<RemoteRoute> {
        Some(match self {
            Self::Play => playstate(PlaystateCommand::Unpause, None),
            Self::Pause => playstate(PlaystateCommand::Pause, None),
            Self::Stop => playstate(PlaystateCommand::Stop, None),
            Self::Next => playstate(PlaystateCommand::NextTrack, None),
            Self::Previous => playstate(PlaystateCommand::PreviousTrack, None),
            Self::Seek { position_ticks } => {
                playstate(PlaystateCommand::Seek, Some(*position_ticks))
            }
            Self::SetVolume { volume } => RemoteRoute::General(GeneralCommand {
                name: GeneralCommandType::SetVolume,
                arguments: BTreeMap::from([("Volume".to_owned(), volume.to_string())]),
            }),
            Self::PlayItems { .. } => return None,
        })
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
    use super::{
        MAX_ITEM_ID_LEN, MAX_REMOTE_PLAY_ITEMS, MAX_VOLUME, PlaystateCommand, RemoteCommand,
        RemoteCommandError, RemoteRoute,
    };

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
            Some(RemoteRoute::Playstate {
                command: PlaystateCommand::Unpause,
                seek_position_ticks: None
            })
        );
        assert_eq!(
            RemoteCommand::Seek { position_ticks: 42 }.route(),
            Some(RemoteRoute::Playstate {
                command: PlaystateCommand::Seek,
                seek_position_ticks: Some(42)
            })
        );
        for (command, expected) in [
            (RemoteCommand::Pause, PlaystateCommand::Pause),
            (RemoteCommand::Stop, PlaystateCommand::Stop),
            (RemoteCommand::Next, PlaystateCommand::NextTrack),
            (RemoteCommand::Previous, PlaystateCommand::PreviousTrack),
        ] {
            assert_eq!(
                command.route(),
                Some(RemoteRoute::Playstate {
                    command: expected,
                    seek_position_ticks: None
                })
            );
        }
    }

    #[test]
    fn volume_maps_to_a_general_command_body() {
        let Some(RemoteRoute::General(body)) = (RemoteCommand::SetVolume { volume: 40 }).route()
        else {
            panic!("volume must use the general command route");
        };
        let encoded = serde_json::to_value(&body).expect("serialize");
        assert_eq!(
            encoded,
            serde_json::json!({"Name": "SetVolume", "Arguments": {"Volume": "40"}})
        );
    }

    #[test]
    fn parses_a_play_now_request_with_its_item_and_position() {
        let json = r#"{
            "PlayCommand": "PlayNow",
            "ItemIds": ["dc1a5c2bb708012441ef2aa742336f21"],
            "StartPositionTicks": 600000000
        }"#;
        assert_eq!(
            RemoteCommand::from_play_json(json),
            Ok(RemoteCommand::PlayItems {
                item_ids: vec!["dc1a5c2bb708012441ef2aa742336f21".to_owned()],
                start_position_ticks: Some(600_000_000),
            })
        );
        // No position is a start from the beginning, not a rejection.
        let no_position = r#"{"PlayCommand":"PlayNow","ItemIds":["abc"]}"#;
        assert_eq!(
            RemoteCommand::from_play_json(no_position),
            Ok(RemoteCommand::PlayItems {
                item_ids: vec!["abc".to_owned()],
                start_position_ticks: None,
            })
        );
    }

    #[test]
    fn play_items_has_no_server_request_form() {
        let command = RemoteCommand::PlayItems {
            item_ids: vec!["abc".to_owned()],
            start_position_ticks: None,
        };
        assert_eq!(command.route(), None);
    }

    #[test]
    fn non_play_now_play_commands_are_refused() {
        for command in ["PlayNext", "PlayLast", "Bogus", ""] {
            let json = format!(r#"{{"PlayCommand":"{command}","ItemIds":["abc"]}}"#);
            assert_eq!(
                RemoteCommand::from_play_json(&json),
                Err(RemoteCommandError::UnsupportedCommand),
                "{command:?}"
            );
        }
    }

    #[test]
    fn play_items_refuses_missing_empty_or_unsafe_ids() {
        for json in [
            r#"{"PlayCommand":"PlayNow"}"#,
            r#"{"PlayCommand":"PlayNow","ItemIds":[]}"#,
            r#"{"PlayCommand":"PlayNow","ItemIds":[""]}"#,
            r#"{"PlayCommand":"PlayNow","ItemIds":["../escape"]}"#,
            r#"{"PlayCommand":"PlayNow","ItemIds":["has space"]}"#,
        ] {
            assert_eq!(
                RemoteCommand::from_play_json(json),
                Err(RemoteCommandError::InvalidArgument),
                "{json}"
            );
        }
        let too_many: Vec<String> = (0..=MAX_REMOTE_PLAY_ITEMS)
            .map(|i| format!("id{i}"))
            .collect();
        let json = serde_json::json!({"PlayCommand": "PlayNow", "ItemIds": too_many}).to_string();
        assert_eq!(
            RemoteCommand::from_play_json(&json),
            Err(RemoteCommandError::InvalidArgument)
        );
        let too_long = "a".repeat(MAX_ITEM_ID_LEN + 1);
        let json = serde_json::json!({"PlayCommand": "PlayNow", "ItemIds": [too_long]}).to_string();
        assert_eq!(
            RemoteCommand::from_play_json(&json),
            Err(RemoteCommandError::InvalidArgument)
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
