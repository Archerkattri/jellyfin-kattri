//! Debug-build per-client SyncPlay position trace. Compiled only with `debug_assertions`, so
//! release builds do not contain it, and inert unless `KATTRI_DEBUG_DIR` is set together with the
//! acceptance opt-ins accepted by [`crate::debug_validation::stream_only_syncplay_opt_in`].
//!
//! While media is loaded, one row is appended to `<KATTRI_DEBUG_DIR>/position-trace-<pid>.csv`
//! roughly every 250 ms. `scripts/acceptance/syncplay-real-analyze.ps1` reads that file as CSV
//! with the header in [`HEADER`].
//!
//! The file is bounded by row count and wall time, a failed write stops the trace after one log
//! line instead of retrying, and the rows carry only numbers and lowercase booleans, so a server
//! URL or token cannot reach the trace.

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// The analyzer's exact header columns, in order.
pub(crate) const HEADER: &str =
    "utc_unix_ms,monotonic_ms,position_ticks,paused,buffering,player_error";

/// One row per interval while media is loaded. The analyzer interpolates samples up to 1000 ms
/// apart, so 250 ms leaves room for a missed render without failing their coverage check.
pub(crate) const SAMPLE_INTERVAL: Duration = Duration::from_millis(250);

/// A SyncPlay acceptance window is minutes long; these caps only stop a runaway file.
const MAX_ROWS: u64 = 500_000;
const MAX_TRACE_DURATION: Duration = Duration::from_secs(3 * 60 * 60);

/// Jellyfin position ticks are 100 ns units, so one mpv second is ten million ticks.
const TICKS_PER_SECOND: f64 = 10_000_000.0;

const DEBUG_DIR_ENV: &str = "KATTRI_DEBUG_DIR";

struct TraceConfig {
    enabled: bool,
    process_started: Instant,
    directory: Option<PathBuf>,
}

static CONFIG: OnceLock<TraceConfig> = OnceLock::new();

struct TraceState {
    writer: Option<BufWriter<File>>,
    next_due: Option<Instant>,
    started: Option<Instant>,
    last_utc_ms: Option<u64>,
    rows: u64,
    player_error: bool,
    stopped: bool,
}

static STATE: Mutex<TraceState> = Mutex::new(TraceState {
    writer: None,
    next_due: None,
    started: None,
    last_utc_ms: None,
    rows: 0,
    player_error: false,
    stopped: false,
});

/// One row before formatting. Pure so the CSV mapping can be unit tested without a clock or a
/// file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TraceSample {
    pub(crate) utc_unix_ms: u64,
    pub(crate) monotonic_ms: u64,
    pub(crate) position_ticks: i64,
    pub(crate) paused: bool,
    pub(crate) buffering: bool,
    pub(crate) player_error: bool,
}

/// Formats one CSV row, newline included. Only digits, `true`/`false`, commas and minus signs
/// can appear here.
pub(crate) fn format_row(sample: &TraceSample) -> String {
    format!(
        "{},{},{},{},{},{}\n",
        sample.utc_unix_ms,
        sample.monotonic_ms,
        sample.position_ticks,
        sample.paused,
        sample.buffering,
        sample.player_error,
    )
}

/// The current UTC time in Unix milliseconds. `None` only for a clock before the epoch.
pub(crate) fn utc_unix_ms(now: SystemTime) -> Option<u64> {
    let elapsed = now.duration_since(UNIX_EPOCH).ok()?;
    Some(u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
}

/// Milliseconds from the recorded process start, saturating instead of panicking.
pub(crate) fn monotonic_ms(process_started: Instant, now: Instant) -> u64 {
    u64::try_from(now.saturating_duration_since(process_started).as_millis()).unwrap_or(u64::MAX)
}

/// mpv seconds as Jellyfin 100 ns ticks, clamped to `0..=i64::MAX`. A non-finite or negative
/// reading is not a position, so it maps to zero instead of a fabricated value.
pub(crate) fn position_ticks(position_seconds: Option<f64>) -> i64 {
    let Some(seconds) = position_seconds else {
        return 0;
    };
    let ticks = seconds * TICKS_PER_SECOND;
    if !ticks.is_finite() || ticks <= 0.0 {
        return 0;
    }
    // `f64 as i64` saturates at i64::MAX, so a huge position cannot wrap or panic.
    ticks.min(i64::MAX as f64) as i64
}

/// True when a row is due. The first sample is always due.
fn sample_due(next_due: Option<Instant>, now: Instant) -> bool {
    next_due.is_none_or(|due| now >= due)
}

/// The analyzer rejects duplicate or regressed UTC samples, so a clock step backwards drops that
/// sample instead of persisting a row their import would refuse.
fn utc_is_strictly_newer(previous: Option<u64>, current: u64) -> bool {
    previous.is_none_or(|previous| current > previous)
}

/// Stops a runaway trace while leaving a normal acceptance window (minutes) well inside the cap.
fn within_bounds(rows: u64, elapsed: Duration) -> bool {
    rows < MAX_ROWS && elapsed < MAX_TRACE_DURATION
}

/// Resolves the debug directory and the acceptance opt-in once, at host startup. Without both,
/// every later call is a cheap no-op.
pub(crate) fn configure(process_started: Instant, acceptance_opt_in: bool) {
    let directory = std::env::var_os(DEBUG_DIR_ENV).map(PathBuf::from);
    let _ = CONFIG.set(TraceConfig {
        enabled: acceptance_opt_in && directory.is_some(),
        process_started,
        directory,
    });
}

/// Records a libmpv playback error for the rest of the process. The flag is sticky so a later
/// healthy sample cannot hide that the acceptance run's player failed.
pub(crate) fn note_player_error() {
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    state.player_error = true;
}

/// The host's next wake-up for the trace, while it is enabled. Media can be paused with no other
/// repaint source, and the analyzer needs samples no more than a second apart.
pub(crate) fn poll_deadline(now: Instant) -> Option<Instant> {
    let config = CONFIG.get()?;
    if !config.enabled {
        return None;
    }
    now.checked_add(SAMPLE_INTERVAL)
}

/// Appends one sample when the trace is configured, media is loaded, and the interval since the
/// previous row has elapsed. Cheap when disabled: one branch and one `OnceLock` read.
pub(crate) fn observe(
    media_loaded: bool,
    position_seconds: Option<f64>,
    paused: Option<bool>,
    buffering: Option<bool>,
    now: Instant,
) {
    if !media_loaded {
        return;
    }
    let Some(config) = CONFIG.get() else {
        return;
    };
    if !config.enabled {
        return;
    }
    let Some(directory) = config.directory.as_deref() else {
        return;
    };
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    if state.stopped || !sample_due(state.next_due, now) {
        return;
    }
    let started = *state.started.get_or_insert(now);
    if !within_bounds(state.rows, now.saturating_duration_since(started)) {
        state.stopped = true;
        log("position trace: row or duration bound reached; stopped");
        return;
    }
    let Some(utc_unix_ms) = utc_unix_ms(SystemTime::now()) else {
        return;
    };
    let sample = TraceSample {
        utc_unix_ms,
        monotonic_ms: monotonic_ms(config.process_started, now),
        position_ticks: position_ticks(position_seconds),
        paused: paused.unwrap_or(false),
        buffering: buffering.unwrap_or(false),
        player_error: state.player_error,
    };
    if !utc_is_strictly_newer(state.last_utc_ms, sample.utc_unix_ms) {
        set_next_due(&mut state, now);
        return;
    }
    if state.writer.is_none() {
        match open_trace(directory) {
            Ok(writer) => {
                state.writer = Some(writer);
                log("position trace: started");
            }
            Err(error) => {
                state.stopped = true;
                log(&format!(
                    "position trace: cannot create the trace file: {error}"
                ));
                return;
            }
        }
    }
    let outcome = match state.writer.as_mut() {
        Some(writer) => append_row(writer, &sample),
        None => return,
    };
    if let Err(error) = outcome {
        state.stopped = true;
        log(&format!("position trace: write failed; stopped: {error}"));
        return;
    }
    state.rows = state.rows.saturating_add(1);
    state.last_utc_ms = Some(sample.utc_unix_ms);
    set_next_due(&mut state, now);
}

fn set_next_due(state: &mut TraceState, now: Instant) {
    match now.checked_add(SAMPLE_INTERVAL) {
        Some(due) => state.next_due = Some(due),
        None => state.stopped = true,
    }
}

fn append_row(writer: &mut BufWriter<File>, sample: &TraceSample) -> std::io::Result<()> {
    writer.write_all(format_row(sample).as_bytes())?;
    writer.flush()
}

/// Creates a fresh per-process trace. The pid keeps two acceptance clients from sharing a file,
/// and truncation makes a leftover file from an earlier process unreadable to the analyzer only
/// until this process writes its own header and rows.
fn open_trace(directory: &Path) -> std::io::Result<BufWriter<File>> {
    std::fs::create_dir_all(directory)?;
    let path = directory.join(format!("position-trace-{}.csv", std::process::id()));
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)?;
    let mut writer = BufWriter::with_capacity(8 * 1024, file);
    writer.write_all(HEADER.as_bytes())?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    Ok(writer)
}

/// Logs through the existing debug harness log, which is inert without `KATTRI_DEBUG_DIR`.
fn log(message: &str) {
    jellyfin_egui_client::debug_log::log(message);
}

#[cfg(test)]
mod tests {
    use super::{
        HEADER, MAX_ROWS, MAX_TRACE_DURATION, SAMPLE_INTERVAL, TraceSample, append_row, format_row,
        monotonic_ms, open_trace, position_ticks, sample_due, utc_is_strictly_newer, utc_unix_ms,
        within_bounds,
    };
    use std::time::{Duration, Instant, UNIX_EPOCH};

    fn sample() -> TraceSample {
        TraceSample {
            utc_unix_ms: 1_760_000_000_123,
            monotonic_ms: 4_000,
            position_ticks: 601_234_567,
            paused: false,
            buffering: false,
            player_error: false,
        }
    }

    fn fields(row: &str) -> Vec<&str> {
        row.trim_end_matches('\n').split(',').collect()
    }

    #[test]
    fn header_is_the_analyzer_schema_in_order() {
        assert_eq!(
            HEADER,
            "utc_unix_ms,monotonic_ms,position_ticks,paused,buffering,player_error"
        );
        assert_eq!(
            fields(HEADER),
            [
                "utc_unix_ms",
                "monotonic_ms",
                "position_ticks",
                "paused",
                "buffering",
                "player_error",
            ]
        );
    }

    #[test]
    fn row_has_six_fields_in_header_order() {
        let row = format_row(&sample());
        assert_eq!(row, "1760000000123,4000,601234567,false,false,false\n");
        assert_eq!(fields(&row).len(), fields(HEADER).len());
    }

    #[test]
    fn trace_file_starts_with_one_header_and_one_row_per_sample() {
        let directory = std::env::temp_dir().join(format!(
            "kattri-position-trace-{}-{}",
            std::process::id(),
            line!()
        ));
        let _ = std::fs::remove_dir_all(&directory);
        let mut writer = open_trace(&directory).expect("create the trace file");
        append_row(&mut writer, &sample()).expect("append one row");
        drop(writer);
        let path = directory.join(format!("position-trace-{}.csv", std::process::id()));
        let text = std::fs::read_to_string(&path).expect("read the trace file");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "header plus one row");
        assert_eq!(lines[0], HEADER);
        assert_eq!(lines[1], format_row(&sample()).trim_end_matches('\n'));
        assert!(text.ends_with('\n'));
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn utc_and_monotonic_fields_stay_ordered_across_samples() {
        let origin = Instant::now();
        let first = TraceSample {
            utc_unix_ms: 1_760_000_000_123,
            monotonic_ms: monotonic_ms(origin, origin + SAMPLE_INTERVAL),
            ..sample()
        };
        let second = TraceSample {
            utc_unix_ms: 1_760_000_000_373,
            monotonic_ms: monotonic_ms(origin, origin + SAMPLE_INTERVAL * 2),
            ..sample()
        };
        assert_eq!(first.monotonic_ms, 250);
        assert_eq!(second.monotonic_ms, 500);
        let first_row = format_row(&first);
        let second_row = format_row(&second);
        let first_fields = fields(&first_row);
        let second_fields = fields(&second_row);
        let utc_pair = (
            first_fields[0].parse::<u64>().ok(),
            second_fields[0].parse::<u64>().ok(),
        );
        let monotonic_pair = (
            first_fields[1].parse::<u64>().ok(),
            second_fields[1].parse::<u64>().ok(),
        );
        assert!(utc_pair.0 < utc_pair.1);
        assert!(monotonic_pair.0 < monotonic_pair.1);
        assert_eq!(utc_unix_ms(UNIX_EPOCH), Some(0));
        assert_eq!(
            utc_unix_ms(UNIX_EPOCH + Duration::from_millis(1234)),
            Some(1234)
        );
    }

    #[test]
    fn paused_buffering_and_player_error_render_as_lowercase_booleans() {
        let row = format_row(&TraceSample {
            paused: true,
            buffering: true,
            player_error: true,
            ..sample()
        });
        assert_eq!(row, "1760000000123,4000,601234567,true,true,true\n");
        for value in fields(&row).into_iter().skip(3) {
            assert!(value == "true" || value == "false", "{value}");
        }
    }

    #[test]
    fn position_ticks_clamps_to_the_non_negative_i64_range() {
        assert_eq!(position_ticks(None), 0);
        assert_eq!(position_ticks(Some(0.0)), 0);
        assert_eq!(position_ticks(Some(1.5)), 15_000_000);
        assert_eq!(position_ticks(Some(3_600.0)), 36_000_000_000);
        assert_eq!(position_ticks(Some(-4.0)), 0);
        assert_eq!(position_ticks(Some(f64::NAN)), 0);
        assert_eq!(position_ticks(Some(f64::INFINITY)), 0);
        assert_eq!(position_ticks(Some(1.0e30)), i64::MAX);
    }

    #[test]
    fn rows_contain_only_numbers_booleans_and_separators() {
        for enabled in [false, true] {
            let row = format_row(&TraceSample {
                paused: enabled,
                buffering: enabled,
                player_error: enabled,
                ..sample()
            });
            assert!(row.is_ascii(), "trace rows are ASCII");
            for (index, field) in fields(&row).into_iter().enumerate() {
                if index < 3 {
                    let value = field.parse::<i64>().expect("numeric field parses");
                    assert!(value >= 0, "non-negative numeric field: {field}");
                } else {
                    assert!(field == "true" || field == "false", "boolean field");
                }
            }
            let lowered = row.to_ascii_lowercase();
            assert!(!lowered.contains("token"));
            assert!(!lowered.contains("api_key"));
            assert!(!lowered.contains("://"));
        }
    }

    #[test]
    fn duplicate_or_regressed_utc_samples_are_rejected() {
        assert!(utc_is_strictly_newer(None, 1));
        assert!(utc_is_strictly_newer(Some(1), 2));
        assert!(!utc_is_strictly_newer(Some(2), 2));
        assert!(!utc_is_strictly_newer(Some(2), 1));
    }

    #[test]
    fn samples_are_due_at_the_acceptance_cadence() {
        assert_eq!(SAMPLE_INTERVAL, Duration::from_millis(250));
        let origin = Instant::now();
        let due = origin + SAMPLE_INTERVAL;
        assert!(sample_due(None, origin));
        assert!(!sample_due(Some(due), origin + Duration::from_millis(249)));
        assert!(sample_due(Some(due), due));
        assert!(sample_due(Some(due), origin + SAMPLE_INTERVAL * 2));
    }

    #[test]
    fn trace_growth_is_bounded_by_rows_and_wall_time() {
        assert!(within_bounds(0, Duration::ZERO));
        assert!(within_bounds(
            MAX_ROWS - 1,
            MAX_TRACE_DURATION - Duration::from_millis(1)
        ));
        assert!(!within_bounds(MAX_ROWS, Duration::ZERO));
        assert!(!within_bounds(0, MAX_TRACE_DURATION));
    }
}
