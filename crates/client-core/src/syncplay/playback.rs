//! M4-11 mapping from server SyncPlay commands to local playback actions. Pure: the caller
//! owns the player and applies the result through its existing command path. Nothing here
//! sends a command to a server.
//!
//! Times are milliseconds. `offset_ms` follows [`ClockEstimate`]: `server_ms = local_ms +
//! offset_ms`.

use jellyfin_api::websocket::{SyncPlayMessage, SyncPlayWireCommand};

use super::{
    ClockEstimate, FUTURE_TOLERANCE_MS, PlayAnchor, SyncPlayError, TimestampError, advance,
    parse_server_timestamp_ms,
};

/// Drift at or below this is ignored. [UNCONFIRMED] proposed value from the M4-11 spec.
pub const DRIFT_THRESHOLD_MS: i64 = 100;
/// Corrective seeks are at least this far apart on the local clock. [UNCONFIRMED]
pub const MIN_CORRECTION_INTERVAL_MS: i64 = 2_000;
/// Jellyfin ticks are 100 ns, so one millisecond is 10 000 ticks.
const TICKS_PER_MS: i64 = 10_000;

/// A playback command from the server. `server_time_ms` is when the group applies it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupCommand {
    Pause {
        position_ms: i64,
        server_time_ms: i64,
    },
    Unpause {
        position_ms: i64,
        server_time_ms: i64,
    },
    Seek {
        position_ms: i64,
        server_time_ms: i64,
    },
}

/// The local player action a command maps to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalAction {
    /// Stop advancing and hold at the position.
    Pause,
    /// Advance from the position.
    Unpause,
    /// Jump to the position and keep the current play state.
    Seek,
}

/// What the local player should do for one command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalPlayback {
    pub action: LocalAction,
    /// Local monotonic time at which the command takes effect (server time minus offset).
    /// It is later than the current time when the command is stamped slightly ahead.
    pub apply_at_local_ms: i64,
    /// Position to show at the local time passed to [`map_command`].
    pub position_ms: i64,
}

/// Map one server command to a local action.
///
/// A command stamped more than [`FUTURE_TOLERANCE_MS`] ahead of server time is refused, as
/// in [`super::apply_server_event`]. `playing` is the local play state. It matters only for
/// `Seek`: a seek while playing keeps advancing from the new position.
pub fn map_command(
    command: GroupCommand,
    clock: ClockEstimate,
    local_now_ms: i64,
    playing: bool,
) -> Result<LocalPlayback, SyncPlayError> {
    let (action, position_ms, server_time_ms, advancing) = match command {
        GroupCommand::Pause {
            position_ms,
            server_time_ms,
        } => (LocalAction::Pause, position_ms, server_time_ms, false),
        GroupCommand::Unpause {
            position_ms,
            server_time_ms,
        } => (LocalAction::Unpause, position_ms, server_time_ms, true),
        GroupCommand::Seek {
            position_ms,
            server_time_ms,
        } => (LocalAction::Seek, position_ms, server_time_ms, playing),
    };

    let now_server_ms = local_now_ms.saturating_add(clock.offset_ms);
    if server_time_ms.saturating_sub(now_server_ms) > FUTURE_TOLERANCE_MS {
        return Err(SyncPlayError::FutureCommand {
            server_time_ms,
            now_server_ms,
        });
    }

    // Both clients run the same formula on the same command, so their positions differ
    // only by the difference of their clock estimates.
    let position_ms = if advancing {
        advance(
            &PlayAnchor {
                position_ms,
                server_time_ms,
            },
            now_server_ms,
        )
    } else {
        position_ms.max(0)
    };

    Ok(LocalPlayback {
        action,
        apply_at_local_ms: server_time_ms.saturating_sub(clock.offset_ms),
        position_ms,
    })
}

/// Turn an inbound SyncPlay command from the socket into the [`GroupCommand`] that
/// [`map_command`] takes. `When` is read as the server's UTC time, and the position is
/// converted from ticks. A `When` that is not in UTC form is refused.
pub fn group_command_from_wire(message: &SyncPlayMessage) -> Result<GroupCommand, TimestampError> {
    let server_time_ms = parse_server_timestamp_ms(&message.when)?;
    let position_ms = message.position_ticks / TICKS_PER_MS;
    Ok(match message.command {
        SyncPlayWireCommand::Pause => GroupCommand::Pause {
            position_ms,
            server_time_ms,
        },
        SyncPlayWireCommand::Unpause => GroupCommand::Unpause {
            position_ms,
            server_time_ms,
        },
        SyncPlayWireCommand::Seek => GroupCommand::Seek {
            position_ms,
            server_time_ms,
        },
    })
}

/// Decides when a measured playback position needs one corrective seek. Pure: the caller
/// measures the position and applies the seek. Call it only while the group is playing.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct DriftController {
    last_correction_local_ms: Option<i64>,
}

impl DriftController {
    pub fn new() -> Self {
        Self::default()
    }

    /// `expected_ms` is the target from [`super::target_position_ms`] and `measured_ms` is the
    /// local position, both at `local_now_ms`. Returns the position to seek to when the drift
    /// is above [`DRIFT_THRESHOLD_MS`] and the last correction is at least
    /// [`MIN_CORRECTION_INTERVAL_MS`] old.
    pub fn check(&mut self, expected_ms: i64, measured_ms: i64, local_now_ms: i64) -> Option<i64> {
        let drift = measured_ms.saturating_sub(expected_ms);
        if drift.unsigned_abs() <= DRIFT_THRESHOLD_MS.unsigned_abs() {
            return None;
        }
        let too_soon = self
            .last_correction_local_ms
            .is_some_and(|last| local_now_ms.saturating_sub(last) < MIN_CORRECTION_INTERVAL_MS);
        if too_soon {
            return None;
        }
        self.last_correction_local_ms = Some(local_now_ms);
        Some(expected_ms.max(0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Local clock reads 0 when the server reads 1_000_000.
    const CLOCK: ClockEstimate = ClockEstimate {
        offset_ms: 1_000_000,
        error_bound_ms: 20,
    };

    fn pause(position_ms: i64, server_time_ms: i64) -> GroupCommand {
        GroupCommand::Pause {
            position_ms,
            server_time_ms,
        }
    }

    fn unpause(position_ms: i64, server_time_ms: i64) -> GroupCommand {
        GroupCommand::Unpause {
            position_ms,
            server_time_ms,
        }
    }

    fn seek(position_ms: i64, server_time_ms: i64) -> GroupCommand {
        GroupCommand::Seek {
            position_ms,
            server_time_ms,
        }
    }

    #[test]
    fn pause_holds_the_command_position() {
        let at_command = map_command(pause(60_000, 1_000_000), CLOCK, 0, true).expect("valid");
        assert_eq!(at_command.action, LocalAction::Pause);
        assert_eq!(at_command.apply_at_local_ms, 0);
        assert_eq!(at_command.position_ms, 60_000);

        let later = map_command(pause(60_000, 1_000_000), CLOCK, 5_000, true).expect("valid");
        assert_eq!(
            later.position_ms, 60_000,
            "paused position does not advance"
        );
    }

    #[test]
    fn unpause_advances_from_the_command_position() {
        let mapped = map_command(unpause(60_000, 1_000_000), CLOCK, 2_500, false).expect("valid");
        assert_eq!(mapped.action, LocalAction::Unpause);
        assert_eq!(mapped.apply_at_local_ms, 0);
        assert_eq!(mapped.position_ms, 62_500);
    }

    #[test]
    fn seek_keeps_the_local_play_state() {
        let playing = map_command(seek(30_000, 1_000_000), CLOCK, 2_500, true).expect("valid");
        assert_eq!(playing.action, LocalAction::Seek);
        assert_eq!(playing.position_ms, 32_500);

        let paused = map_command(seek(30_000, 1_000_000), CLOCK, 2_500, false).expect("valid");
        assert_eq!(paused.position_ms, 30_000);
    }

    #[test]
    fn command_stamped_slightly_ahead_is_applied_later() {
        // Server time is 300 ms ahead of now: hold at the command position until then.
        let mapped =
            map_command(unpause(60_000, 1_000_300), CLOCK, 0, false).expect("within tolerance");
        assert_eq!(mapped.apply_at_local_ms, 300);
        assert_eq!(mapped.position_ms, 60_000);
    }

    #[test]
    fn command_beyond_the_future_tolerance_is_refused() {
        let too_far = 1_000_000 + FUTURE_TOLERANCE_MS + 1;
        assert_eq!(
            map_command(unpause(60_000, too_far), CLOCK, 0, false),
            Err(SyncPlayError::FutureCommand {
                server_time_ms: too_far,
                now_server_ms: 1_000_000,
            })
        );
    }

    #[test]
    fn negative_positions_are_clamped_to_zero() {
        let mapped = map_command(pause(-5, 1_000_000), CLOCK, 0, false).expect("valid");
        assert_eq!(mapped.position_ms, 0);
    }

    #[test]
    fn drift_at_the_threshold_is_ignored() {
        let mut drift = DriftController::new();
        assert_eq!(drift.check(60_000, 60_100, 0), None);
        assert_eq!(drift.check(60_000, 59_900, 0), None);
    }

    #[test]
    fn drift_above_the_threshold_seeks_to_the_expected_position() {
        let mut drift = DriftController::new();
        assert_eq!(drift.check(60_000, 60_101, 0), Some(60_000));
        let mut drift = DriftController::new();
        assert_eq!(
            drift.check(60_000, 59_899, 0),
            Some(60_000),
            "behind is corrected too"
        );
    }

    #[test]
    fn corrective_seeks_are_at_least_two_seconds_apart() {
        let mut drift = DriftController::new();
        assert_eq!(drift.check(60_000, 65_000, 0), Some(60_000));
        assert_eq!(
            drift.check(60_000, 65_000, 1_999),
            None,
            "one millisecond too soon"
        );
        assert_eq!(drift.check(60_000, 65_000, 2_000), Some(60_000));
    }

    fn wire(command: SyncPlayWireCommand, when: &str) -> SyncPlayMessage {
        SyncPlayMessage {
            command,
            position_ticks: 600_000_000,
            when: when.into(),
        }
    }

    #[test]
    fn an_inbound_pause_becomes_a_pause_action_at_its_position() {
        let command = group_command_from_wire(&wire(
            SyncPlayWireCommand::Pause,
            "1970-01-01T00:16:40.0000000Z",
        ))
        .expect("UTC timestamp");
        assert_eq!(
            command,
            GroupCommand::Pause {
                position_ms: 60_000,
                server_time_ms: 1_000_000,
            }
        );
        let mapped = map_command(command, CLOCK, 0, true).expect("valid");
        assert_eq!(mapped.action, LocalAction::Pause);
        assert_eq!(mapped.position_ms, 60_000);
    }

    #[test]
    fn inbound_unpause_and_seek_keep_their_kind() {
        let when = "1970-01-01T00:16:40.0000000Z";
        assert_eq!(
            group_command_from_wire(&wire(SyncPlayWireCommand::Unpause, when)).expect("valid"),
            unpause(60_000, 1_000_000)
        );
        assert_eq!(
            group_command_from_wire(&wire(SyncPlayWireCommand::Seek, when)).expect("valid"),
            seek(60_000, 1_000_000)
        );
    }

    #[test]
    fn inbound_timestamp_outside_utc_form_is_refused() {
        assert!(
            group_command_from_wire(&wire(
                SyncPlayWireCommand::Pause,
                "1970-01-01T02:16:40+02:00",
            ))
            .is_err()
        );
    }

    #[test]
    fn drift_check_handles_extreme_values_without_panicking() {
        let mut drift = DriftController::new();
        assert_eq!(drift.check(i64::MIN, i64::MAX, 0), Some(0));
    }
}
