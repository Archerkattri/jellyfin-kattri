//! Pure UTC scheduling rules for the Live TV guide.
//!
//! The guide stores intervals as UTC Unix seconds. The native UI is responsible
//! for formatting those instants in the operating system's local timezone; fixed
//! UTC slots therefore remain continuous across spring and fall DST changes.

/// Duration of one guide column.
pub const EPG_SLOT_SECONDS: i64 = 30 * 60;
/// Background refresh cadence while the guide is visible.
pub const EPG_REFRESH_INTERVAL_SECONDS: i64 = 15 * 60;
/// Roll the 24-hour window when less than this much time remains.
pub const EPG_ROLL_WINDOW_THRESHOLD_SECONDS: i64 = 2 * 60 * 60;
const DEFAULT_PROGRAM_SECONDS: i64 = 30 * 60;

/// Maximum accepted timestamp length, including a fractional second and offset.
const MAX_RFC3339_TIMESTAMP_BYTES: usize = 64;

/// Parse an RFC3339 date-time into UTC Unix seconds.
///
/// Fractional seconds are validated but truncated because the guide stores
/// whole-second instants. Input is capped to keep parsing work bounded.
pub fn parse_rfc3339_unix_seconds(value: &str) -> Option<i64> {
    let bytes = value.as_bytes();
    if bytes.len() < 20 || bytes.len() > MAX_RFC3339_TIMESTAMP_BYTES {
        return None;
    }

    let year = parse_digits(bytes, 0, 4)?;
    let month = parse_digits(bytes, 5, 2)?;
    let day = parse_digits(bytes, 8, 2)?;
    let hour = parse_digits(bytes, 11, 2)?;
    let minute = parse_digits(bytes, 14, 2)?;
    let second = parse_digits(bytes, 17, 2)?;
    if bytes.get(4) != Some(&b'-')
        || bytes.get(7) != Some(&b'-')
        || !matches!(bytes.get(10), Some(b'T' | b't'))
        || bytes.get(13) != Some(&b':')
        || bytes.get(16) != Some(&b':')
        || !(1..=12).contains(&month)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return None;
    }

    let days_in_month = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        2 => 28,
        _ => return None,
    };
    if day == 0 || day > days_in_month {
        return None;
    }

    let mut index = 19;
    if bytes.get(index) == Some(&b'.') {
        index += 1;
        let fraction_start = index;
        while bytes.get(index).is_some_and(u8::is_ascii_digit) {
            index += 1;
        }
        if index == fraction_start {
            return None;
        }
    }

    let offset_seconds: i64 = match bytes.get(index).copied()? {
        b'Z' | b'z' if index + 1 == bytes.len() => 0,
        b'+' | b'-' if index + 6 == bytes.len() && bytes.get(index + 3) == Some(&b':') => {
            let offset_hour = parse_digits(bytes, index + 1, 2)?;
            let offset_minute = parse_digits(bytes, index + 4, 2)?;
            if offset_hour > 23 || offset_minute > 59 {
                return None;
            }
            let magnitude = i64::from(offset_hour * 60 + offset_minute) * 60;
            if bytes[index] == b'+' {
                magnitude
            } else {
                -magnitude
            }
        }
        _ => return None,
    };

    // RFC3339's numeric offset is local time minus UTC, so subtract it.
    let days = days_from_civil(year, month, day)?;
    days.checked_mul(86_400)?
        .checked_add(i64::from(hour) * 3_600)?
        .checked_add(i64::from(minute) * 60)?
        .checked_add(i64::from(second))?
        .checked_sub(offset_seconds)
}

fn parse_digits(bytes: &[u8], start: usize, count: usize) -> Option<u32> {
    let end = start.checked_add(count)?;
    let digits = bytes.get(start..end)?;
    digits.iter().try_fold(0_u32, |value, digit| {
        if !digit.is_ascii_digit() {
            return None;
        }
        value.checked_mul(10)?.checked_add(u32::from(*digit - b'0'))
    })
}

fn is_leap_year(year: u32) -> bool {
    year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400))
}

/// Days since 1970-01-01 in the proleptic Gregorian calendar.
fn days_from_civil(year: u32, month: u32, day: u32) -> Option<i64> {
    let year = i64::from(year).checked_sub(i64::from(month <= 2))?;
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let adjusted_month = i64::from(month) + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * adjusted_month + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era.checked_mul(146_097)?
        .checked_add(day_of_era)?
        .checked_sub(719_468)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EpgWindow {
    pub start_unix: i64,
    pub end_unix: i64,
}

impl EpgWindow {
    pub fn new(start_unix: i64, end_unix: i64) -> Option<Self> {
        (end_unix > start_unix).then_some(Self {
            start_unix,
            end_unix,
        })
    }

    pub fn slot_count(self) -> usize {
        let duration = self.end_unix.saturating_sub(self.start_unix);
        let slots = duration / EPG_SLOT_SECONDS + i64::from(duration % EPG_SLOT_SECONDS != 0);
        usize::try_from(slots).unwrap_or(usize::MAX)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EpgProgram {
    pub channel_id: String,
    pub title: String,
    pub start_unix: i64,
    /// A program with no end time is represented as one 30-minute slot.
    pub end_unix: Option<i64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EpgCell {
    pub slot_start_unix: i64,
    pub slot_end_unix: i64,
    pub title: Option<String>,
    pub clipped_start_unix: Option<i64>,
    pub clipped_end_unix: Option<i64>,
    pub current: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EpgRow {
    pub channel_id: String,
    pub cells: Vec<EpgCell>,
}

/// Build a channel row with half-hour UTC columns, clipping programs at the
/// requested edges and leaving gaps explicit as empty cells.
pub fn build_row(
    channel_id: &str,
    programs: &[EpgProgram],
    window: EpgWindow,
    now_unix: i64,
) -> EpgRow {
    let count = window.slot_count();
    let mut cells = Vec::with_capacity(count);
    for index in 0..count {
        let offset = i64::try_from(index)
            .unwrap_or(i64::MAX)
            .saturating_mul(EPG_SLOT_SECONDS);
        let slot_start = window.start_unix.saturating_add(offset);
        let slot_end = slot_start
            .saturating_add(EPG_SLOT_SECONDS)
            .min(window.end_unix);
        let match_program = programs.iter().find_map(|program| {
            if program.channel_id != channel_id {
                return None;
            }
            let program_end = program
                .end_unix
                .unwrap_or_else(|| program.start_unix.saturating_add(DEFAULT_PROGRAM_SECONDS));
            if program_end <= program.start_unix
                || program.start_unix >= slot_end
                || program_end <= slot_start
            {
                return None;
            }
            let clipped_start = program.start_unix.max(slot_start).max(window.start_unix);
            let clipped_end = program_end.min(slot_end).min(window.end_unix);
            Some((program, clipped_start, clipped_end, program_end))
        });
        let (title, clipped_start, clipped_end, current) = match match_program {
            Some((program, start, end, program_end)) => (
                Some(program.title.clone()),
                Some(start),
                Some(end),
                program.start_unix <= now_unix && now_unix < program_end,
            ),
            None => (None, None, None, false),
        };
        cells.push(EpgCell {
            slot_start_unix: slot_start,
            slot_end_unix: slot_end,
            title,
            clipped_start_unix: clipped_start,
            clipped_end_unix: clipped_end,
            current,
        });
    }
    EpgRow {
        channel_id: channel_id.to_owned(),
        cells,
    }
}

/// Whether a guide query should advance its window to keep a full day ahead.
pub fn should_roll_window(window_end_unix: i64, now_unix: i64) -> bool {
    window_end_unix.saturating_sub(now_unix) < EPG_ROLL_WINDOW_THRESHOLD_SECONDS
}

/// The 24-hour guide window for `now_unix`. It starts at the slot boundary at or before now, so
/// the columns read 18:00, 18:30 rather than 18:01, 18:31, and the current program is in the
/// first column.
pub fn guide_window_at(now_unix: i64) -> EpgWindow {
    let start_unix = now_unix.saturating_sub(now_unix.rem_euclid(EPG_SLOT_SECONDS));
    EpgWindow {
        start_unix,
        end_unix: start_unix.saturating_add(24 * 60 * 60),
    }
}

/// True when `window` no longer starts at the current slot, or is about to run out. A guide left
/// open for hours would otherwise open on programs that have already ended.
pub fn guide_window_is_stale(window: EpgWindow, now_unix: i64) -> bool {
    window.start_unix < guide_window_at(now_unix).start_unix
        || should_roll_window(window.end_unix, now_unix)
}

/// Clock skew beyond two minutes is surfaced to the viewer.
pub fn clock_skew_exceeds_threshold(server_unix: i64, client_unix: i64) -> bool {
    server_unix.abs_diff(client_unix) > 120
}

#[cfg(test)]
mod tests {
    use super::{
        EPG_REFRESH_INTERVAL_SECONDS, EPG_SLOT_SECONDS, EpgProgram, EpgWindow, build_row,
        clock_skew_exceeds_threshold, guide_window_at, guide_window_is_stale,
        parse_rfc3339_unix_seconds, should_roll_window,
    };

    fn program(start: i64, end: Option<i64>, title: &str) -> EpgProgram {
        EpgProgram {
            channel_id: "channel".to_owned(),
            title: title.to_owned(),
            start_unix: start,
            end_unix: end,
        }
    }

    #[test]
    fn programs_clip_at_both_window_edges_and_gaps_are_empty() {
        let window = EpgWindow::new(100, 100 + 2 * EPG_SLOT_SECONDS).expect("valid window");
        let row = build_row(
            "channel",
            &[
                program(50, Some(100 + EPG_SLOT_SECONDS / 2), "opening"),
                program(
                    100 + 3 * EPG_SLOT_SECONDS / 2,
                    Some(100 + 3 * EPG_SLOT_SECONDS),
                    "closing",
                ),
            ],
            window,
            100,
        );
        assert_eq!(row.cells.len(), 2);
        assert_eq!(row.cells[0].clipped_start_unix, Some(100));
        assert_eq!(
            row.cells[0].clipped_end_unix,
            Some(100 + EPG_SLOT_SECONDS / 2)
        );
        assert_eq!(row.cells[1].title, Some("closing".to_owned()));
        assert_eq!(row.cells[1].clipped_end_unix, Some(window.end_unix));

        let gap = build_row("channel", &[], window, 100);
        assert!(gap.cells.iter().all(|cell| cell.title.is_none()));
    }

    #[test]
    fn current_program_uses_start_inclusive_end_exclusive_boundaries() {
        let window = EpgWindow::new(0, 2 * EPG_SLOT_SECONDS).expect("valid window");
        let programs = [
            program(0, Some(EPG_SLOT_SECONDS), "first"),
            program(EPG_SLOT_SECONDS, Some(2 * EPG_SLOT_SECONDS), "second"),
        ];
        let at_start = build_row("channel", &programs, window, 0);
        assert!(at_start.cells[0].current);
        let at_boundary = build_row("channel", &programs, window, EPG_SLOT_SECONDS);
        assert!(!at_boundary.cells[0].current);
        assert!(at_boundary.cells[1].current);
    }

    #[test]
    fn missing_end_uses_one_half_hour_slot() {
        let window = EpgWindow::new(0, EPG_SLOT_SECONDS * 2).expect("valid window");
        let row = build_row("channel", &[program(0, None, "unknown end")], window, 0);
        assert_eq!(row.cells[0].title.as_deref(), Some("unknown end"));
        assert!(row.cells[0].current);
        assert_eq!(row.cells[1].title, None);
    }

    #[test]
    fn utc_slot_counts_cover_short_and_long_dst_local_days_without_hour_loss() {
        let start = 1_800_000_000;
        let spring_day = EpgWindow::new(start, start + 23 * 60 * 60).expect("valid local day");
        let fall_day = EpgWindow::new(start, start + 25 * 60 * 60).expect("valid local day");
        assert_eq!(spring_day.slot_count(), 46);
        assert_eq!(fall_day.slot_count(), 50);
        assert_eq!(build_row("channel", &[], spring_day, start).cells.len(), 46);
        assert_eq!(build_row("channel", &[], fall_day, start).cells.len(), 50);
    }

    #[test]
    fn guide_roll_and_clock_skew_thresholds_are_explicit() {
        assert_eq!(EPG_REFRESH_INTERVAL_SECONDS, 900);
        assert!(!should_roll_window(10_000, 10_000 - 7_200));
        assert!(should_roll_window(10_000, 10_000 - 7_199));
        assert!(!clock_skew_exceeds_threshold(1_120, 1_000));
        assert!(clock_skew_exceeds_threshold(1_121, 1_000));
    }

    #[test]
    fn rfc3339_parser_converts_epoch_and_numeric_offsets() {
        assert_eq!(parse_rfc3339_unix_seconds("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            parse_rfc3339_unix_seconds("1970-01-01T01:00:00+01:00"),
            Some(0)
        );
        assert_eq!(
            parse_rfc3339_unix_seconds("1969-12-31T19:00:00-05:00"),
            Some(0)
        );
        assert_eq!(parse_rfc3339_unix_seconds("1970-01-01t00:00:00z"), Some(0));
    }

    #[test]
    fn rfc3339_parser_validates_fraction_and_leap_year_boundaries() {
        assert_eq!(
            parse_rfc3339_unix_seconds("2000-02-29T12:34:56.123456789Z"),
            Some(951_827_696)
        );
        assert_eq!(
            parse_rfc3339_unix_seconds("1900-02-28T00:00:00Z"),
            Some(-2_203_977_600)
        );
        assert_eq!(
            parse_rfc3339_unix_seconds("2000-02-29T00:00:00Z"),
            Some(951_782_400)
        );
    }

    #[test]
    fn rfc3339_parser_rejects_invalid_or_unbounded_values() {
        for invalid in [
            "1970-02-29T00:00:00Z",
            "1900-02-29T00:00:00Z",
            "2000-13-01T00:00:00Z",
            "2000-01-00T00:00:00Z",
            "2000-01-32T00:00:00Z",
            "2000-01-01T24:00:00Z",
            "2000-01-01T00:60:00Z",
            "2000-01-01T00:00:60Z",
            "2000-01-01T00:00:00+24:00",
            "2000-01-01T00:00:00+00:60",
            "2000-01-01T00:00:00+01",
            "2000-01-01T00:00:00.Z",
            "2000-01-01 00:00:00Z",
            "2000-01-01T00:00:00Zsuffix",
        ] {
            assert_eq!(parse_rfc3339_unix_seconds(invalid), None, "{invalid}");
        }
        let too_long = format!("1970-01-01T00:00:00.{}Z", "1".repeat(64));
        assert_eq!(parse_rfc3339_unix_seconds(&too_long), None);
        assert_eq!(
            parse_rfc3339_unix_seconds("0000-01-01T00:00:00Z"),
            Some(-62_167_219_200)
        );
    }

    #[test]
    fn the_guide_window_starts_on_the_slot_boundary_at_or_before_now() {
        let now = 1_700_000_000 + 17 * 60; // 17 minutes past a half-hour boundary
        let window = guide_window_at(now);
        assert_eq!(window.start_unix % EPG_SLOT_SECONDS, 0);
        assert!(window.start_unix <= now && now < window.start_unix + EPG_SLOT_SECONDS);
        assert_eq!(window.end_unix - window.start_unix, 24 * 60 * 60);
        assert_eq!(window.slot_count(), 48);
        assert_eq!(guide_window_at(window.start_unix), window);
        // Negative times keep the boundary below now.
        assert_eq!(guide_window_at(-1).start_unix, -EPG_SLOT_SECONDS);
    }

    #[test]
    fn a_window_is_stale_once_now_leaves_its_first_slot() {
        let start = 1_800_000_000 - 1_800_000_000 % EPG_SLOT_SECONDS;
        let window = guide_window_at(start);
        assert!(!guide_window_is_stale(window, start));
        assert!(!guide_window_is_stale(window, start + EPG_SLOT_SECONDS - 1));
        assert!(guide_window_is_stale(window, start + EPG_SLOT_SECONDS));
        assert!(guide_window_is_stale(window, start + 23 * 60 * 60));
    }
}
