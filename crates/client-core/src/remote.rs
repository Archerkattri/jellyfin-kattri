//! M4-13 mapping from inbound session commands to local player actions. Pure: the caller
//! owns the player and applies the returned [`LocalAction`] through its existing command
//! path. Nothing here performs I/O.
//!
//! Commands the client does not implement come back as
//! [`RemoteCommandError::UnsupportedCommand`]. They are never mapped to a guessed action.

use jellyfin_api::remote::parse_session_message;
use jellyfin_api::remote_command::{RemoteCommand, RemoteCommandError};

use crate::volume::clamp_volume;

/// Jellyfin ticks are 100-nanosecond units, so one millisecond is 10 000 ticks.
pub const TICKS_PER_MS: u64 = 10_000;

/// The local player action for one remote command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalAction {
    Play,
    Pause,
    Stop,
    /// Jump to an absolute position. The caller clamps to the item duration, which this
    /// module does not know.
    Seek {
        position_ms: u64,
    },
    /// Set the volume in percent, already clamped to the caller's maximum.
    SetVolume {
        percent: u32,
    },
}

/// Map a parsed command to a local action. `max_volume_percent` is the M2 maximum
/// (`audio.max_volume_percent`), so a volume never exceeds what the user allowed locally.
pub fn local_action(
    command: RemoteCommand,
    max_volume_percent: u32,
) -> Result<LocalAction, RemoteCommandError> {
    match command {
        RemoteCommand::Play => Ok(LocalAction::Play),
        RemoteCommand::Pause => Ok(LocalAction::Pause),
        RemoteCommand::Stop => Ok(LocalAction::Stop),
        RemoteCommand::Seek { position_ticks } => {
            // Check the sign before dividing: integer division would turn -1 tick into 0.
            let ticks =
                u64::try_from(position_ticks).map_err(|_| RemoteCommandError::InvalidArgument)?;
            Ok(LocalAction::Seek {
                position_ms: ticks / TICKS_PER_MS,
            })
        }
        RemoteCommand::SetVolume { volume } => Ok(LocalAction::SetVolume {
            percent: clamp_volume(u32::from(volume), max_volume_percent),
        }),
        // Not advertised by the client (see `jellyfin_api::remote`), so the server
        // should not send them. Refuse rather than guess an action.
        RemoteCommand::Next | RemoteCommand::Previous => {
            Err(RemoteCommandError::UnsupportedCommand)
        }
    }
}

/// Parse one inbound session message and map it to a local action.
pub fn map_session_message(
    message: &str,
    max_volume_percent: u32,
) -> Result<LocalAction, RemoteCommandError> {
    local_action(parse_session_message(message)?, max_volume_percent)
}

#[cfg(test)]
mod tests {
    use jellyfin_api::remote_command::{RemoteCommand, RemoteCommandError};

    use super::{LocalAction, local_action, map_session_message};
    use crate::volume::VOLUME_BOOST_MAX_PERCENT;

    #[test]
    fn each_implemented_command_maps_to_one_action() {
        let max = 100;
        assert_eq!(
            local_action(RemoteCommand::Play, max),
            Ok(LocalAction::Play)
        );
        assert_eq!(
            local_action(RemoteCommand::Pause, max),
            Ok(LocalAction::Pause)
        );
        assert_eq!(
            local_action(RemoteCommand::Stop, max),
            Ok(LocalAction::Stop)
        );
        assert_eq!(
            local_action(RemoteCommand::SetVolume { volume: 40 }, max),
            Ok(LocalAction::SetVolume { percent: 40 })
        );
    }

    #[test]
    fn seek_converts_ticks_to_milliseconds_and_floors() {
        assert_eq!(
            local_action(
                RemoteCommand::Seek {
                    position_ticks: 300_000_000
                },
                100
            ),
            Ok(LocalAction::Seek {
                position_ms: 30_000
            })
        );
        assert_eq!(
            local_action(RemoteCommand::Seek { position_ticks: 0 }, 100),
            Ok(LocalAction::Seek { position_ms: 0 })
        );
        assert_eq!(
            local_action(
                RemoteCommand::Seek {
                    position_ticks: 9_999
                },
                100
            ),
            Ok(LocalAction::Seek { position_ms: 0 })
        );
    }

    #[test]
    fn negative_seek_is_refused_even_when_it_would_round_to_zero() {
        for position_ticks in [-1, -9_999, -10_000] {
            assert_eq!(
                local_action(RemoteCommand::Seek { position_ticks }, 100),
                Err(RemoteCommandError::InvalidArgument),
                "{position_ticks}"
            );
        }
    }

    #[test]
    fn volume_is_capped_at_the_local_maximum() {
        // Remote volume is 0..=100 (the parser refuses more), so a boost maximum cannot
        // raise it and a 100 maximum leaves it as is.
        assert_eq!(
            local_action(RemoteCommand::SetVolume { volume: 100 }, 100),
            Ok(LocalAction::SetVolume { percent: 100 })
        );
        assert_eq!(
            local_action(
                RemoteCommand::SetVolume { volume: 100 },
                VOLUME_BOOST_MAX_PERCENT
            ),
            Ok(LocalAction::SetVolume { percent: 100 })
        );
        assert_eq!(
            local_action(RemoteCommand::SetVolume { volume: 0 }, 100),
            Ok(LocalAction::SetVolume { percent: 0 })
        );
    }

    #[test]
    fn next_and_previous_are_unsupported_not_guessed() {
        assert_eq!(
            local_action(RemoteCommand::Next, 100),
            Err(RemoteCommandError::UnsupportedCommand)
        );
        assert_eq!(
            local_action(RemoteCommand::Previous, 100),
            Err(RemoteCommandError::UnsupportedCommand)
        );
    }

    #[test]
    fn session_messages_map_end_to_end() {
        assert_eq!(
            map_session_message(
                r#"{"MessageType":"Playstate","Data":{"Command":"Pause"}}"#,
                100
            ),
            Ok(LocalAction::Pause)
        );
        assert_eq!(
            map_session_message(
                r#"{"MessageType":"GeneralCommand","Data":{"Name":"SetVolume","Arguments":{"Volume":"55"}}}"#,
                100
            ),
            Ok(LocalAction::SetVolume { percent: 55 })
        );
        assert_eq!(
            map_session_message(
                r#"{"MessageType":"GeneralCommand","Data":{"Name":"Mute","Arguments":{}}}"#,
                100
            ),
            Err(RemoteCommandError::UnsupportedCommand)
        );
        assert_eq!(
            map_session_message("not json", 100),
            Err(RemoteCommandError::InvalidPayload)
        );
    }
}
