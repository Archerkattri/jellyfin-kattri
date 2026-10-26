//! M4-10 SyncPlay core: clock offset estimation, group state transitions and target-position
//! math. Pure logic only: no network, no UI, no server. All times are milliseconds.
//!
//! - [`ClockEstimator`] keeps the lowest round-trip samples and estimates the server clock
//!   offset from the best one. Its error bound is half the round trip, which holds for any
//!   split of the delay between the two directions.
//! - [`SyncPlayState`] changes only on server events ([`ServerEvent`]). Local buffering
//!   produces reports (see [`should_report_buffering`]) and never changes group state.
//! - [`target_position_ms`] gives the position a client should be at for a play anchor.
//! - [`GroupModel`] pairs the state machine with the member list from the server.
//! - [`playback`] maps a server command to a local action (M4-11). Pure, no player.
//!
//! The tolerances below are proposals from the M4-10 spec and are [UNCONFIRMED] until
//! checked against a live server.

pub mod playback;

pub use playback::{
    DRIFT_THRESHOLD_MS, DriftController, GroupCommand, LocalAction, LocalPlayback,
    MIN_CORRECTION_INTERVAL_MS, map_command,
};

use std::fmt;

/// Samples with a round trip above this are discarded (spec, proposed value).
pub const MAX_ROUND_TRIP_MS: i64 = 300;
/// How many of the lowest-round-trip samples are kept.
pub const MAX_KEPT_SAMPLES: usize = 8;
/// Samples sent more than this long before the newest one are discarded. Without this
/// window a stale best sample would keep an old offset after the server clock changes.
/// [UNCONFIRMED] value.
pub const MAX_SAMPLE_AGE_MS: i64 = 30_000;
/// A server command stamped further in the future than this is rejected. [UNCONFIRMED].
pub const FUTURE_TOLERANCE_MS: i64 = 500;
/// A change in wall-minus-monotonic time bigger than this means the system clock stepped.
/// [UNCONFIRMED]
pub const CLOCK_STEP_TOLERANCE_MS: i64 = 100;
/// Report buffering while less than this much media is buffered ahead. [UNCONFIRMED]
pub const BUFFER_THRESHOLD_MS: i64 = 500;

/// One ping-style exchange: local send and receive times (monotonic) and the server time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClockSample {
    pub sent_local_ms: i64,
    pub received_local_ms: i64,
    pub server_ms: i64,
}

/// What [`ClockEstimator::add_sample`] did with a sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleOutcome {
    Kept,
    /// Round trip above the threshold, or not among the best kept samples.
    Dropped,
    /// Receive time before send time. Not a valid exchange.
    Rejected,
}

/// Estimated offset with its error bound: `server_ms = local_ms + offset_ms`, and the true
/// offset lies within `error_bound_ms` of `offset_ms`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClockEstimate {
    pub offset_ms: i64,
    pub error_bound_ms: i64,
}

/// Clock offset estimator. Keeps the [`MAX_KEPT_SAMPLES`] lowest-round-trip samples.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ClockEstimator {
    /// Sorted by round trip, lowest first.
    samples: Vec<ClockSample>,
    /// Last (monotonic, wall) pair seen, for step detection.
    anchor: Option<(i64, i64)>,
    /// Latest send time among accepted samples, for the age window.
    newest_sent_ms: Option<i64>,
}

impl ClockEstimator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add one exchange. Samples above [`MAX_ROUND_TRIP_MS`], samples older than
    /// [`MAX_SAMPLE_AGE_MS`] before the newest one, and samples slower than every kept
    /// sample once the set is full, are dropped. Kept samples older than the window are
    /// removed, so the estimate follows a server clock change within one window.
    pub fn add_sample(&mut self, sample: ClockSample) -> SampleOutcome {
        let Some(rtt) = round_trip(&sample) else {
            return SampleOutcome::Rejected;
        };
        if rtt > MAX_ROUND_TRIP_MS {
            return SampleOutcome::Dropped;
        }
        let newest = self
            .newest_sent_ms
            .map_or(sample.sent_local_ms, |prev| prev.max(sample.sent_local_ms));
        let cutoff = newest.saturating_sub(MAX_SAMPLE_AGE_MS);
        if sample.sent_local_ms < cutoff {
            return SampleOutcome::Dropped;
        }
        self.newest_sent_ms = Some(newest);
        self.samples.retain(|kept| kept.sent_local_ms >= cutoff);
        if self.samples.len() == MAX_KEPT_SAMPLES {
            let worst = self.samples.last().and_then(round_trip).unwrap_or(i64::MAX);
            if rtt >= worst {
                return SampleOutcome::Dropped;
            }
            self.samples.pop();
        }
        let position = self
            .samples
            .iter()
            .position(|kept| round_trip(kept).is_some_and(|k| k > rtt))
            .unwrap_or(self.samples.len());
        self.samples.insert(position, sample);
        SampleOutcome::Kept
    }

    /// Estimate the offset from the lowest-round-trip sample. `None` with no samples.
    pub fn estimate(&self) -> Option<ClockEstimate> {
        let best = self.samples.first()?;
        let rtt = round_trip(best)?;
        let half = rtt / 2;
        // Midpoint estimate: the server stamped its reply about halfway through the round trip.
        let offset_ms = best
            .server_ms
            .checked_sub(best.sent_local_ms)?
            .checked_sub(half)?;
        Some(ClockEstimate {
            offset_ms,
            error_bound_ms: half + rtt % 2,
        })
    }

    /// Record a (monotonic, wall) clock reading. If the wall clock moved relative to the
    /// monotonic clock by more than [`CLOCK_STEP_TOLERANCE_MS`] since the last reading, the
    /// system time was changed: every sample is discarded and `true` is returned.
    pub fn observe_clocks(&mut self, monotonic_ms: i64, wall_ms: i64) -> bool {
        let stepped = self.anchor.is_some_and(|(prev_mono, prev_wall)| {
            let offsets = wall_ms
                .checked_sub(monotonic_ms)
                .zip(prev_wall.checked_sub(prev_mono));
            offsets
                .is_some_and(|(now, before)| now.abs_diff(before) > CLOCK_STEP_TOLERANCE_MS as u64)
        });
        self.anchor = Some((monotonic_ms, wall_ms));
        if stepped {
            self.samples.clear();
            self.newest_sent_ms = None;
        }
        stepped
    }

    /// Number of kept samples.
    pub fn sample_count(&self) -> usize {
        self.samples.len()
    }
}

fn round_trip(sample: &ClockSample) -> Option<i64> {
    let rtt = sample.received_local_ms.checked_sub(sample.sent_local_ms)?;
    (rtt >= 0).then_some(rtt)
}

/// Longest request round trip that still becomes a sample. The estimator drops anything over
/// [`MAX_ROUND_TRIP_MS`] anyway. This bound stops a stalled request from being timed at all.
/// [UNCONFIRMED] value.
pub const MAX_EXCHANGE_ROUND_TRIP_MS: i64 = 2_000;

/// Why a timed request could not become a [`ClockSample`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExchangeError {
    /// The receive time is before the send time, so the two are not one valid exchange.
    ReceivedBeforeSent,
    /// The round trip is longer than [`MAX_EXCHANGE_ROUND_TRIP_MS`].
    RoundTripTooLong,
}

impl fmt::Display for ExchangeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ReceivedBeforeSent => formatter.write_str("clock exchange ends before it starts"),
            Self::RoundTripTooLong => formatter.write_str("clock exchange round trip is too long"),
        }
    }
}

/// Turn one timed server clock request into a [`ClockSample`]. `sent_local_ms` and
/// `received_local_ms` are monotonic milliseconds taken around the request, and `server_ms`
/// is the server's reply time in Unix milliseconds. Pure: no clock is read and no request is
/// made.
pub fn clock_sample_from_exchange(
    sent_local_ms: i64,
    server_ms: i64,
    received_local_ms: i64,
) -> Result<ClockSample, ExchangeError> {
    let round_trip = received_local_ms
        .checked_sub(sent_local_ms)
        .filter(|rtt| *rtt >= 0)
        .ok_or(ExchangeError::ReceivedBeforeSent)?;
    if round_trip > MAX_EXCHANGE_ROUND_TRIP_MS {
        return Err(ExchangeError::RoundTripTooLong);
    }
    Ok(ClockSample {
        sent_local_ms,
        received_local_ms,
        server_ms,
    })
}

/// A playback anchor from the server: at `server_time_ms`, the group was at `position_ms`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlayAnchor {
    pub position_ms: i64,
    pub server_time_ms: i64,
}

/// Commands that can wait for the group to be ready.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingCommand {
    Play(PlayAnchor),
    Pause { position_ms: i64 },
}

/// Group state from the client's point of view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncPlayState {
    NotJoined,
    Joined {
        group_id: String,
    },
    Waiting {
        group_id: String,
        then: PendingCommand,
    },
    Playing {
        group_id: String,
        anchor: PlayAnchor,
    },
    Paused {
        group_id: String,
        position_ms: i64,
    },
}

impl SyncPlayState {
    pub const fn name(&self) -> &'static str {
        match self {
            Self::NotJoined => "NotJoined",
            Self::Joined { .. } => "Joined",
            Self::Waiting { .. } => "Waiting",
            Self::Playing { .. } => "Playing",
            Self::Paused { .. } => "Paused",
        }
    }

    fn group_id(&self) -> Option<&str> {
        match self {
            Self::NotJoined => None,
            Self::Joined { group_id }
            | Self::Waiting { group_id, .. }
            | Self::Playing { group_id, .. }
            | Self::Paused { group_id, .. } => Some(group_id),
        }
    }
}

/// Events from the server. Local playback never produces these.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerEvent {
    GroupJoined {
        group_id: String,
    },
    /// The group was deleted, or the server removed this client from it.
    GroupGone,
    Play {
        position_ms: i64,
        server_time_ms: i64,
    },
    Pause {
        position_ms: i64,
    },
    /// Every member is ready; releases a waiting command.
    Ready,
    /// The member list of the joined group changed. Playback state is unchanged.
    MembersChanged {
        members: Vec<String>,
    },
}

impl ServerEvent {
    const fn name(&self) -> &'static str {
        match self {
            Self::GroupJoined { .. } => "GroupJoined",
            Self::GroupGone => "GroupGone",
            Self::Play { .. } => "Play",
            Self::Pause { .. } => "Pause",
            Self::Ready => "Ready",
            Self::MembersChanged { .. } => "MembersChanged",
        }
    }
}

/// Why a server event was refused. The state is unchanged in every case.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncPlayError {
    InvalidTransition {
        from: &'static str,
        event: &'static str,
    },
    /// A play command is stamped beyond [`FUTURE_TOLERANCE_MS`] ahead of server time.
    FutureCommand {
        server_time_ms: i64,
        now_server_ms: i64,
    },
}

/// Apply one server event. `now_server_ms` is the client's current estimate of server time,
/// used only to reject play commands stamped too far in the future.
pub fn apply_server_event(
    state: &SyncPlayState,
    event: &ServerEvent,
    now_server_ms: i64,
) -> Result<SyncPlayState, SyncPlayError> {
    let invalid = || SyncPlayError::InvalidTransition {
        from: state.name(),
        event: event.name(),
    };

    if let ServerEvent::Play { server_time_ms, .. } = event {
        let ahead = server_time_ms.saturating_sub(now_server_ms);
        if ahead > FUTURE_TOLERANCE_MS {
            return Err(SyncPlayError::FutureCommand {
                server_time_ms: *server_time_ms,
                now_server_ms,
            });
        }
    }

    use ServerEvent as E;
    use SyncPlayState as S;
    let next = match (state, event) {
        (S::NotJoined, E::GroupJoined { group_id }) => S::Joined {
            group_id: group_id.clone(),
        },
        (_, E::GroupGone) if state.group_id().is_some() => S::NotJoined,
        (
            S::Joined { group_id } | S::Playing { group_id, .. } | S::Paused { group_id, .. },
            E::Play {
                position_ms,
                server_time_ms,
            },
        )
        | (
            S::Waiting { group_id, .. },
            E::Play {
                position_ms,
                server_time_ms,
            },
        ) => S::Waiting {
            group_id: group_id.clone(),
            then: PendingCommand::Play(PlayAnchor {
                position_ms: *position_ms,
                server_time_ms: *server_time_ms,
            }),
        },
        (
            S::Joined { group_id }
            | S::Playing { group_id, .. }
            | S::Waiting { group_id, .. }
            | S::Paused { group_id, .. },
            E::Pause { position_ms },
        ) => S::Paused {
            group_id: group_id.clone(),
            position_ms: *position_ms,
        },
        (
            S::Waiting {
                group_id,
                then: PendingCommand::Play(anchor),
            },
            E::Ready,
        ) => S::Playing {
            group_id: group_id.clone(),
            anchor: *anchor,
        },
        (
            S::Waiting {
                group_id,
                then: PendingCommand::Pause { position_ms },
            },
            E::Ready,
        ) => S::Paused {
            group_id: group_id.clone(),
            position_ms: *position_ms,
        },
        (s, E::MembersChanged { .. }) if s.group_id().is_some() => s.clone(),
        _ => return Err(invalid()),
    };
    Ok(next)
}

/// The joined group as the client sees it: playback state plus member names. Both change
/// only through [`GroupModel::apply`], which delegates the state rules to
/// [`apply_server_event`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupModel {
    pub state: SyncPlayState,
    pub members: Vec<String>,
}

impl GroupModel {
    pub fn new() -> Self {
        Self {
            state: SyncPlayState::NotJoined,
            members: Vec::new(),
        }
    }

    /// Apply one server event. On error the model is unchanged, because the caller keeps
    /// the old value. Leaving the group (`GroupGone`) clears the member list.
    pub fn apply(&self, event: &ServerEvent, now_server_ms: i64) -> Result<Self, SyncPlayError> {
        let state = apply_server_event(&self.state, event, now_server_ms)?;
        let members = match event {
            ServerEvent::MembersChanged { members } => members.clone(),
            ServerEvent::GroupGone => Vec::new(),
            _ => self.members.clone(),
        };
        Ok(Self { state, members })
    }
}

impl Default for GroupModel {
    fn default() -> Self {
        Self::new()
    }
}

/// Position a client should show now for `state`: the anchor advanced by the server time
/// elapsed when playing, the held position when paused, and nothing otherwise.
pub fn target_position_ms(state: &SyncPlayState, now_server_ms: i64) -> Option<i64> {
    match state {
        SyncPlayState::Playing { anchor, .. } => Some(advance(anchor, now_server_ms)),
        SyncPlayState::Paused { position_ms, .. } => Some((*position_ms).max(0)),
        _ => None,
    }
}

/// `position + (now - command_time)`, never negative. A command stamped slightly in the
/// future (within tolerance) holds its position rather than moving backwards.
fn advance(anchor: &PlayAnchor, now_server_ms: i64) -> i64 {
    let elapsed = now_server_ms.saturating_sub(anchor.server_time_ms).max(0);
    anchor.position_ms.saturating_add(elapsed).max(0)
}

/// Whether to report buffering to the server: true while less than
/// [`BUFFER_THRESHOLD_MS`] of media is buffered ahead of the playhead.
pub const fn should_report_buffering(buffered_ahead_ms: i64) -> bool {
    buffered_ahead_ms < BUFFER_THRESHOLD_MS
}

/// Why a server timestamp could not be read as a time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimestampError {
    /// Not `YYYY-MM-DDTHH:MM:SS`, optional `.fraction`, then `Z`.
    Malformed,
    /// A field is outside its calendar range, or the value does not fit in an `i64`.
    OutOfRange,
}

impl fmt::Display for TimestampError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed => formatter.write_str("server timestamp is not in UTC ISO form"),
            Self::OutOfRange => formatter.write_str("server timestamp field is out of range"),
        }
    }
}

/// Parse a server timestamp in Jellyfin's UTC form, such as `2026-10-15T20:00:00.0000000Z`,
/// into Unix milliseconds. Fractions are cut to whole milliseconds. Any offset other than
/// `Z` is refused, so no local time zone is ever assumed.
pub fn parse_server_timestamp_ms(when: &str) -> Result<i64, TimestampError> {
    let bytes = when.as_bytes();
    let fixed = bytes.get(..19).ok_or(TimestampError::Malformed)?;
    let shape_ok = fixed[4] == b'-'
        && fixed[7] == b'-'
        && fixed[10] == b'T'
        && fixed[13] == b':'
        && fixed[16] == b':';
    if !shape_ok {
        return Err(TimestampError::Malformed);
    }
    let year = digits(&fixed[0..4])?;
    let month = digits(&fixed[5..7])?;
    let day = digits(&fixed[8..10])?;
    let hour = digits(&fixed[11..13])?;
    let minute = digits(&fixed[14..16])?;
    let second = digits(&fixed[17..19])?;

    let rest = &bytes[19..];
    let (fraction_ms, tail) = match rest.split_first() {
        Some((b'.', after_dot)) => {
            let count = after_dot
                .iter()
                .take_while(|byte| byte.is_ascii_digit())
                .count();
            if count == 0 || count > 9 {
                return Err(TimestampError::Malformed);
            }
            let ms = fraction_to_ms(&after_dot[..count]);
            (ms, &after_dot[count..])
        }
        _ => (0, rest),
    };
    if tail != b"Z" {
        return Err(TimestampError::Malformed);
    }

    if !(1..=12).contains(&month) || !(1..=days_in_month(year, month)).contains(&day) {
        return Err(TimestampError::OutOfRange);
    }
    if hour > 23 || minute > 59 || second > 59 {
        return Err(TimestampError::OutOfRange);
    }

    let days = days_from_civil(year, month, day);
    let seconds = days
        .checked_mul(86_400)
        .and_then(|value| value.checked_add(hour * 3_600 + minute * 60 + second))
        .ok_or(TimestampError::OutOfRange)?;
    seconds
        .checked_mul(1_000)
        .and_then(|value| value.checked_add(fraction_ms))
        .ok_or(TimestampError::OutOfRange)
}

/// Read a server timestamp as a time on the local clock the estimator's samples use.
/// The estimate maps server time to local time as `local_ms = server_ms - offset_ms`,
/// which is the inverse of [`ClockEstimate`]'s definition. Pure: no clock is read.
pub fn when_to_local_ms(when: &str, estimate: ClockEstimate) -> Result<i64, TimestampError> {
    let server_ms = parse_server_timestamp_ms(when)?;
    server_ms
        .checked_sub(estimate.offset_ms)
        .ok_or(TimestampError::OutOfRange)
}

/// Parse a fixed-width run of ASCII digits. Non-digits are refused as malformed.
fn digits(bytes: &[u8]) -> Result<i64, TimestampError> {
    bytes.iter().try_fold(0_i64, |value, byte| {
        if byte.is_ascii_digit() {
            Ok(value * 10 + i64::from(byte - b'0'))
        } else {
            Err(TimestampError::Malformed)
        }
    })
}

/// The first three fraction digits as milliseconds, padded on the right when shorter.
fn fraction_to_ms(digits_run: &[u8]) -> i64 {
    let mut ms = 0_i64;
    for slot in 0..3 {
        let value = digits_run
            .get(slot)
            .map_or(0, |byte| i64::from(byte - b'0'));
        ms = ms * 10 + value;
    }
    ms
}

fn is_leap_year(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        2 if is_leap_year(year) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Days since 1970-01-01 in the proleptic Gregorian calendar. The date must already be
/// validated, so the arithmetic cannot overflow for four-digit years.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let year_of_era = y - era * 400;
    let shifted_month = (month + 9) % 12;
    let day_of_year = (153 * shifted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[cfg(test)]
mod timestamp_tests {
    use super::{
        ClockEstimate, ClockEstimator, ClockSample, TimestampError, parse_server_timestamp_ms,
        when_to_local_ms,
    };

    /// 2026-10-15T20:00:00Z in Unix milliseconds, checked against the calendar by hand:
    /// 2026-01-01 is 1 767 225 600 s, plus 287 days and 20 hours.
    const JELLYFIN_EXAMPLE_MS: i64 = 1_792_094_400_000;

    #[test]
    fn parses_the_jellyfin_utc_form_with_seven_fraction_digits() {
        assert_eq!(
            parse_server_timestamp_ms("2026-10-15T20:00:00.0000000Z"),
            Ok(JELLYFIN_EXAMPLE_MS)
        );
        assert_eq!(
            parse_server_timestamp_ms("2026-10-15T20:00:00.1234567Z"),
            Ok(JELLYFIN_EXAMPLE_MS + 123)
        );
        assert_eq!(
            parse_server_timestamp_ms("2026-10-15T20:00:00Z"),
            Ok(JELLYFIN_EXAMPLE_MS)
        );
    }

    #[test]
    fn fraction_is_cut_not_rounded_and_short_fractions_pad() {
        assert_eq!(
            parse_server_timestamp_ms("2026-10-15T20:00:00.9999Z"),
            Ok(JELLYFIN_EXAMPLE_MS + 999)
        );
        assert_eq!(
            parse_server_timestamp_ms("2026-10-15T20:00:00.5Z"),
            Ok(JELLYFIN_EXAMPLE_MS + 500)
        );
    }

    #[test]
    fn epoch_and_leap_days_are_exact() {
        assert_eq!(parse_server_timestamp_ms("1970-01-01T00:00:00Z"), Ok(0));
        // 2000-02-29 exists; 2000-03-01 is 951 868 800 s after the epoch.
        assert_eq!(
            parse_server_timestamp_ms("2000-03-01T00:00:00Z"),
            Ok(951_868_800_000)
        );
        assert_eq!(
            parse_server_timestamp_ms("2000-02-29T00:00:00Z"),
            Ok(951_782_400_000)
        );
        // 2100 is not a leap year.
        assert_eq!(
            parse_server_timestamp_ms("2100-02-29T00:00:00Z"),
            Err(TimestampError::OutOfRange)
        );
    }

    #[test]
    fn refuses_offsets_and_other_shapes_as_malformed() {
        for value in [
            "",
            "2026-10-15",
            "2026-10-15T20:00:00",
            "2026-10-15T20:00:00+02:00",
            "2026-10-15T20:00:00.Z",
            "2026-10-15T20:00:00.0000000000Z",
            "2026-10-15 20:00:00Z",
            "2026-10-15t20:00:00Z",
            "2026/10/15T20:00:00Z",
            "2026-1a-15T20:00:00Z",
            "2026-10-15T20:00:00Zjunk",
        ] {
            assert_eq!(
                parse_server_timestamp_ms(value),
                Err(TimestampError::Malformed),
                "{value:?} must be refused as malformed"
            );
        }
    }

    #[test]
    fn refuses_out_of_range_fields() {
        for value in [
            "2026-13-01T00:00:00Z",
            "2026-00-01T00:00:00Z",
            "2026-02-29T00:00:00Z",
            "2026-04-31T00:00:00Z",
            "2026-10-15T24:00:00Z",
            "2026-10-15T20:60:00Z",
            "2026-10-15T20:00:60Z",
        ] {
            assert_eq!(
                parse_server_timestamp_ms(value),
                Err(TimestampError::OutOfRange),
                "{value:?} must be refused as out of range"
            );
        }
    }

    #[test]
    fn local_time_is_server_time_minus_the_offset() {
        // server_ms = local_ms + offset_ms, so a server time of 1 000 000 ms ahead of the
        // estimate's base maps back to the local clock.
        let estimate = ClockEstimate {
            offset_ms: JELLYFIN_EXAMPLE_MS - 1_000,
            error_bound_ms: 5,
        };
        assert_eq!(
            when_to_local_ms("2026-10-15T20:00:00Z", estimate),
            Ok(1_000)
        );
        let behind = ClockEstimate {
            offset_ms: -250,
            error_bound_ms: 0,
        };
        assert_eq!(when_to_local_ms("1970-01-01T00:00:00Z", behind), Ok(250));
    }

    #[test]
    fn local_time_uses_the_estimator_offset_for_a_symmetric_exchange() {
        // Local send 1000, server stamps at local 1010 with its clock 1 790 000 000 000 ms
        // ahead, reply at local 1020. The round trip is symmetric, so the estimate is exact.
        let mut estimator = ClockEstimator::new();
        estimator.add_sample(ClockSample {
            sent_local_ms: 1_000,
            received_local_ms: 1_020,
            server_ms: 1_010 + 1_790_000_000_000,
        });
        let estimate = estimator.estimate().expect("one kept sample");
        assert_eq!(
            when_to_local_ms("2026-10-15T20:00:00Z", estimate),
            Ok(JELLYFIN_EXAMPLE_MS - 1_790_000_000_000)
        );
    }

    #[test]
    fn a_timestamp_before_the_offset_base_is_still_exact() {
        let estimate = ClockEstimate {
            offset_ms: 0,
            error_bound_ms: 0,
        };
        assert_eq!(
            when_to_local_ms("1969-12-31T23:59:59.5Z", estimate),
            Ok(-500)
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(sent: i64, rtt_out: i64, rtt_back: i64, true_offset: i64) -> ClockSample {
        // Local send at `sent`; the server stamps after `rtt_out`; reply arrives after `rtt_back`.
        let server_local = sent + rtt_out;
        ClockSample {
            sent_local_ms: sent,
            received_local_ms: server_local + rtt_back,
            server_ms: server_local + true_offset,
        }
    }

    #[test]
    fn symmetric_delay_recovers_the_offset_exactly() {
        let mut est = ClockEstimator::new();
        est.add_sample(sample(1_000, 40, 40, 5_000));
        let e = est.estimate().expect("one sample");
        assert_eq!(e.offset_ms, 5_000);
        assert_eq!(e.error_bound_ms, 40);
    }

    #[test]
    fn asymmetric_delay_error_is_within_the_reported_bound() {
        // Outbound 10 ms, return 90 ms: the midpoint estimate is off by 40 ms.
        let true_offset = -2_000;
        let mut est = ClockEstimator::new();
        est.add_sample(sample(1_000, 10, 90, true_offset));
        let e = est.estimate().expect("one sample");
        let error = (e.offset_ms - true_offset).abs();
        assert_eq!(error, 40);
        assert!(
            error <= e.error_bound_ms,
            "error {error} exceeds bound {}",
            e.error_bound_ms
        );
        assert_eq!(e.error_bound_ms, 50);
    }

    #[test]
    fn outlier_samples_do_not_win_over_the_lowest_round_trip() {
        let true_offset = 750;
        let mut est = ClockEstimator::new();
        est.add_sample(sample(0, 200, 5, true_offset)); // rtt 205, asymmetric, bad
        est.add_sample(sample(1_000, 20, 20, true_offset)); // rtt 40, clean
        est.add_sample(sample(2_000, 150, 150, true_offset + 999)); // rtt 300, high
        let e = est.estimate().expect("samples");
        assert_eq!(e.offset_ms, true_offset);
        assert_eq!(
            est.sample_count(),
            3,
            "all three are within the limit and the set is not full"
        );
    }

    #[test]
    fn round_trips_above_the_threshold_are_dropped() {
        let mut est = ClockEstimator::new();
        assert_eq!(
            est.add_sample(sample(0, 151, 150, 0)),
            SampleOutcome::Dropped,
            "rtt 301 is above 300"
        );
        assert_eq!(
            est.add_sample(sample(0, 150, 150, 0)),
            SampleOutcome::Kept,
            "rtt 300 is allowed"
        );
        assert_eq!(est.sample_count(), 1);
    }

    #[test]
    fn receive_before_send_is_rejected() {
        let mut est = ClockEstimator::new();
        let bad = ClockSample {
            sent_local_ms: 100,
            received_local_ms: 99,
            server_ms: 0,
        };
        assert_eq!(est.add_sample(bad), SampleOutcome::Rejected);
        assert!(est.estimate().is_none());
    }

    #[test]
    fn keeps_at_most_the_best_eight_samples() {
        let mut est = ClockEstimator::new();
        for rtt in (10..=200).step_by(10) {
            est.add_sample(sample(0, rtt / 2, rtt - rtt / 2, 0));
        }
        assert_eq!(est.sample_count(), MAX_KEPT_SAMPLES);
        assert_eq!(est.estimate().map(|e| e.error_bound_ms), Some(5));
        assert_eq!(
            est.add_sample(sample(0, 100, 100, 0)),
            SampleOutcome::Dropped
        );
    }

    #[test]
    fn clock_step_resets_the_estimator() {
        let mut est = ClockEstimator::new();
        est.add_sample(sample(0, 10, 10, 0));
        assert!(
            !est.observe_clocks(1_000, 50_000),
            "first reading is not a step"
        );
        assert!(
            !est.observe_clocks(2_000, 51_000),
            "steady clocks are not a step"
        );
        assert!(
            est.observe_clocks(3_000, 90_000),
            "wall clock jumped by about 40 s"
        );
        assert_eq!(est.sample_count(), 0);
        assert!(est.estimate().is_none());
    }

    #[test]
    fn small_wall_clock_jitter_is_not_a_step() {
        let mut est = ClockEstimator::new();
        est.observe_clocks(0, 0);
        est.add_sample(sample(0, 10, 10, 0));
        assert!(!est.observe_clocks(1_000, 1_050));
        assert_eq!(est.sample_count(), 1);
    }

    #[test]
    fn state_machine_full_table() {
        use ServerEvent as E;
        use SyncPlayState as S;
        let g = || "group-1".to_owned();
        let anchor = PlayAnchor {
            position_ms: 10_000,
            server_time_ms: 5_000,
        };
        let joined = S::Joined { group_id: g() };
        let waiting_play = S::Waiting {
            group_id: g(),
            then: PendingCommand::Play(anchor),
        };
        let playing = S::Playing {
            group_id: g(),
            anchor,
        };
        let paused = S::Paused {
            group_id: g(),
            position_ms: 7_000,
        };
        let now = 5_000;
        let play_cmd = E::Play {
            position_ms: 10_000,
            server_time_ms: 5_000,
        };

        // NotJoined only accepts a join.
        assert_eq!(
            apply_server_event(&S::NotJoined, &E::GroupJoined { group_id: g() }, now),
            Ok(joined.clone())
        );
        for event in [
            &play_cmd,
            &E::Ready,
            &E::Pause { position_ms: 1 },
            &E::GroupGone,
        ] {
            assert!(
                apply_server_event(&S::NotJoined, event, now).is_err(),
                "{event:?}"
            );
        }

        // Joined: play waits; pause holds.
        assert_eq!(
            apply_server_event(&joined, &play_cmd, now),
            Ok(waiting_play.clone())
        );
        assert_eq!(
            apply_server_event(&joined, &E::Pause { position_ms: 7_000 }, now),
            Ok(paused.clone())
        );
        assert!(apply_server_event(&joined, &E::Ready, now).is_err());

        // Waiting: ready releases the pending command.
        assert_eq!(
            apply_server_event(&waiting_play, &E::Ready, now),
            Ok(playing.clone())
        );
        assert_eq!(
            apply_server_event(&waiting_play, &E::Pause { position_ms: 7_000 }, now),
            Ok(paused.clone())
        );
        let waiting_pause = S::Waiting {
            group_id: g(),
            then: PendingCommand::Pause { position_ms: 7_000 },
        };
        assert_eq!(
            apply_server_event(&waiting_pause, &E::Ready, now),
            Ok(paused.clone())
        );
        assert_eq!(
            apply_server_event(&waiting_play, &play_cmd, now),
            Ok(waiting_play.clone())
        );

        // Playing and Paused: a new play re-enters waiting, a pause holds.
        assert_eq!(
            apply_server_event(&playing, &E::Pause { position_ms: 7_000 }, now),
            Ok(paused.clone())
        );
        assert_eq!(
            apply_server_event(&playing, &play_cmd, now),
            Ok(waiting_play.clone())
        );
        assert_eq!(
            apply_server_event(&paused, &play_cmd, now),
            Ok(waiting_play.clone())
        );
        assert_eq!(
            apply_server_event(&paused, &E::Pause { position_ms: 8_000 }, now),
            Ok(S::Paused {
                group_id: g(),
                position_ms: 8_000
            }),
            "a repeated pause moves the held position"
        );
        assert!(apply_server_event(&paused, &E::Ready, now).is_err());

        // Group deleted or client removed: back to NotJoined from every joined state.
        for state in [&joined, &waiting_play, &playing, &paused] {
            assert_eq!(
                apply_server_event(state, &E::GroupGone, now),
                Ok(S::NotJoined),
                "{state:?}"
            );
        }
    }

    #[test]
    fn invalid_transition_reports_names_and_keeps_state() {
        let state = SyncPlayState::NotJoined;
        assert_eq!(
            apply_server_event(&state, &ServerEvent::Ready, 0),
            Err(SyncPlayError::InvalidTransition {
                from: "NotJoined",
                event: "Ready",
            })
        );
    }

    #[test]
    fn play_stamped_beyond_tolerance_is_rejected() {
        let joined = SyncPlayState::Joined {
            group_id: "g".to_owned(),
        };
        let future = ServerEvent::Play {
            position_ms: 0,
            server_time_ms: 10_000 + FUTURE_TOLERANCE_MS + 1,
        };
        assert_eq!(
            apply_server_event(&joined, &future, 10_000),
            Err(SyncPlayError::FutureCommand {
                server_time_ms: 10_000 + FUTURE_TOLERANCE_MS + 1,
                now_server_ms: 10_000,
            })
        );
        let within = ServerEvent::Play {
            position_ms: 0,
            server_time_ms: 10_000 + FUTURE_TOLERANCE_MS,
        };
        assert!(apply_server_event(&joined, &within, 10_000).is_ok());
    }

    #[test]
    fn group_gone_while_not_joined_is_refused() {
        assert!(apply_server_event(&SyncPlayState::NotJoined, &ServerEvent::GroupGone, 0).is_err());
    }

    #[test]
    fn target_position_advances_when_playing_and_holds_when_paused() {
        let playing = SyncPlayState::Playing {
            group_id: "g".to_owned(),
            anchor: PlayAnchor {
                position_ms: 60_000,
                server_time_ms: 1_000_000,
            },
        };
        assert_eq!(target_position_ms(&playing, 1_000_000), Some(60_000));
        assert_eq!(target_position_ms(&playing, 1_002_500), Some(62_500));
        // A command stamped slightly ahead holds its position rather than going backwards.
        assert_eq!(target_position_ms(&playing, 999_800), Some(60_000));

        let paused = SyncPlayState::Paused {
            group_id: "g".to_owned(),
            position_ms: 61_000,
        };
        assert_eq!(target_position_ms(&paused, 1_000_000), Some(61_000));
        assert_eq!(target_position_ms(&paused, 9_999_999), Some(61_000));

        assert_eq!(target_position_ms(&SyncPlayState::NotJoined, 0), None);
    }

    #[test]
    fn target_position_math_never_goes_negative() {
        let playing = SyncPlayState::Playing {
            group_id: "g".to_owned(),
            anchor: PlayAnchor {
                position_ms: -5,
                server_time_ms: 0,
            },
        };
        assert_eq!(target_position_ms(&playing, 0), Some(0));
    }

    #[test]
    fn buffering_is_reported_below_the_threshold_only() {
        assert!(should_report_buffering(BUFFER_THRESHOLD_MS - 1));
        assert!(!should_report_buffering(BUFFER_THRESHOLD_MS));
    }

    #[test]
    fn server_clock_change_is_followed_after_the_sample_window() {
        // Offset 5_000 for the first ten exchanges (one per second, symmetric 20 ms legs).
        let mut est = ClockEstimator::new();
        for i in 0..10 {
            est.add_sample(sample(i * 1_000, 20, 20, 5_000));
        }
        assert_eq!(est.estimate().map(|e| e.offset_ms), Some(5_000));

        // The server clock moves to 5_200. The old best samples are still in the window.
        for i in 10..15 {
            est.add_sample(sample(i * 1_000, 20, 20, 5_200));
        }
        assert_eq!(
            est.estimate().map(|e| e.offset_ms),
            Some(5_000),
            "old samples are inside the window"
        );

        for i in 15..50 {
            est.add_sample(sample(i * 1_000, 20, 20, 5_200));
        }
        let e = est.estimate().expect("samples");
        assert_eq!(e.offset_ms, 5_200, "estimate follows the new offset");
        assert_eq!(e.error_bound_ms, 20);
        assert_eq!(
            est.add_sample(sample(0, 20, 20, 5_000)),
            SampleOutcome::Dropped,
            "a sample older than the window is refused"
        );
    }

    #[test]
    fn lost_and_late_pings_leave_the_estimate_from_the_rest() {
        let true_offset = 3_000;
        let mut est = ClockEstimator::new();
        est.add_sample(sample(0, 30, 30, true_offset));
        est.add_sample(sample(1_000, 25, 35, true_offset));
        // The pings at 2_000 and 3_000 never get a reply: nothing is recorded for them.
        assert_eq!(
            est.add_sample(sample(4_000, 200, 200, true_offset)),
            SampleOutcome::Dropped,
            "a reply after 400 ms is above the round-trip limit"
        );
        assert_eq!(est.sample_count(), 2);
        let e = est.estimate().expect("two samples remain");
        assert!((e.offset_ms - true_offset).abs() <= e.error_bound_ms);
        assert_eq!(
            e.offset_ms, true_offset,
            "lowest round trip is the first sample"
        );
    }

    #[test]
    fn group_model_tracks_members_and_clears_them_on_leave() {
        use ServerEvent as E;
        use SyncPlayState as S;

        let model = GroupModel::new();
        assert_eq!(model.state, S::NotJoined);
        assert!(
            model
                .apply(
                    &E::MembersChanged {
                        members: vec!["alice".to_owned()]
                    },
                    0
                )
                .is_err(),
            "members cannot arrive before a join"
        );

        let joined = model
            .apply(
                &E::GroupJoined {
                    group_id: "g".to_owned(),
                },
                0,
            )
            .expect("join");
        assert!(joined.members.is_empty());

        let with_members = joined
            .apply(
                &E::MembersChanged {
                    members: vec!["alice".to_owned(), "bob".to_owned()],
                },
                0,
            )
            .expect("members while joined");
        assert_eq!(with_members.state, joined.state);
        assert_eq!(with_members.members, ["alice", "bob"]);

        let waiting = with_members
            .apply(
                &E::Play {
                    position_ms: 1_000,
                    server_time_ms: 0,
                },
                0,
            )
            .expect("play waits for the group");
        assert_eq!(waiting.state.name(), "Waiting");
        let still_waiting = waiting
            .apply(
                &E::MembersChanged {
                    members: vec!["alice".to_owned()],
                },
                0,
            )
            .expect("membership while waiting");
        assert_eq!(still_waiting.state, waiting.state);
        assert_eq!(still_waiting.members, ["alice"]);

        let gone = still_waiting.apply(&E::GroupGone, 0).expect("removed");
        assert_eq!(gone, GroupModel::new(), "leaving clears state and members");
        assert!(gone.apply(&E::Ready, 0).is_err());
    }

    #[test]
    fn exchange_keeps_its_three_times_as_a_sample() {
        let sample = clock_sample_from_exchange(1_000, 5_120, 1_080).expect("valid exchange");
        assert_eq!(
            sample,
            ClockSample {
                sent_local_ms: 1_000,
                received_local_ms: 1_080,
                server_ms: 5_120,
            }
        );
    }

    #[test]
    fn round_trip_at_the_bound_is_kept_and_one_millisecond_over_is_refused() {
        assert!(clock_sample_from_exchange(0, 0, MAX_EXCHANGE_ROUND_TRIP_MS).is_ok());
        assert_eq!(
            clock_sample_from_exchange(0, 0, MAX_EXCHANGE_ROUND_TRIP_MS + 1),
            Err(ExchangeError::RoundTripTooLong)
        );
    }

    #[test]
    fn receive_before_send_is_refused() {
        assert_eq!(
            clock_sample_from_exchange(100, 0, 99),
            Err(ExchangeError::ReceivedBeforeSent)
        );
        assert_eq!(
            clock_sample_from_exchange(i64::MIN, 0, 1),
            Err(ExchangeError::ReceivedBeforeSent),
            "a difference that does not fit is not a valid exchange"
        );
    }

    #[test]
    fn exchange_sample_gives_the_offset_the_estimator_would_compute() {
        // Server clock 5 s ahead. The server stamps at the midpoint of a 40 ms round trip.
        let mut estimator = ClockEstimator::new();
        let sample =
            clock_sample_from_exchange(1_000, 1_000 + 5_000 + 20, 1_040).expect("valid exchange");
        assert_eq!(estimator.add_sample(sample), SampleOutcome::Kept);
        let estimate = estimator.estimate().expect("one kept sample");
        assert_eq!(estimate.offset_ms, 5_000);
        assert_eq!(estimate.error_bound_ms, 20);
    }

    #[test]
    fn a_sample_between_the_two_bounds_is_refused_by_the_estimator() {
        // 1 s is accepted here but over MAX_ROUND_TRIP_MS, so the estimator drops it.
        let sample = clock_sample_from_exchange(0, 500, 1_000).expect("under the exchange bound");
        let mut estimator = ClockEstimator::new();
        assert_eq!(estimator.add_sample(sample), SampleOutcome::Dropped);
        assert!(estimator.estimate().is_none());
    }
}
